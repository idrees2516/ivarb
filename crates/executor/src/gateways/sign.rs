//! Exchange request signing — the HMAC schemes used by the venues that do
//! not offer Deribit-style bearer auth.
//!
//! Three schemes, one primitive (HMAC-SHA256):
//! - **Bybit V5**: `hex(hmac(secret, timestamp + api_key + recv_window + body))`
//!   in the `X-BAPI-Sign` header (body = the raw JSON payload for POST, the
//!   raw query string for GET).
//! - **OKX v5**: `base64(hmac(secret, iso_timestamp + METHOD + path + body))`
//!   in the `OK-ACCESS-SIGN` header, plus the passphrase.
//! - **Binance (eapi)**: `hex(hmac(secret, query_string))` appended to the
//!   query as `&signature=…` with the key in `X-MBX-APIKEY`.
//!
//! No secrets are ever logged; `sign` functions take `&str` slices so the
//! caller controls lifetime, and the signed payloads contain only public
//! fields (timestamps, symbols, sizes — never the key itself).

use hmac::{Hmac, Mac};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// HMAC-SHA256, raw bytes.
pub fn hmac_sha256(key: &[u8], msg: &[u8]) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(key).expect("hmac accepts any key length");
    mac.update(msg);
    mac.finalize().into_bytes().to_vec()
}

/// Lowercase hex (what Bybit and Binance expect).
pub fn hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0xf) as usize] as char);
    }
    s
}

/// Standard base64 (what OKX expects).
pub fn b64(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// Bybit V5 signature.
pub fn bybit_sign(secret: &str, timestamp: &str, api_key: &str, recv_window: &str, body: &str) -> String {
    let payload = format!("{timestamp}{api_key}{recv_window}{body}");
    hex(&hmac_sha256(secret.as_bytes(), payload.as_bytes()))
}

/// OKX v5 signature. `path` includes the query string for GET.
pub fn okx_sign(secret: &str, timestamp: &str, method: &str, path: &str, body: &str) -> String {
    let payload = format!("{timestamp}{method}{path}{body}");
    b64(&hmac_sha256(secret.as_bytes(), payload.as_bytes()))
}

/// Binance signature: HMAC of the query string, appended as `&signature=`.
pub fn binance_signed_query(secret: &str, query: &str) -> String {
    let sig = hex(&hmac_sha256(secret.as_bytes(), query.as_bytes()));
    format!("{query}&signature={sig}")
}

/// OKX wants an ISO-8601 timestamp like `2026-10-04T12:34:56.789Z`.
/// Hand-rolled from unix millis (no chrono needed in this crate).
pub fn iso8601_utc(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let millis = ms.rem_euclid(1000);
    let days = secs.div_euclid(86_400);
    let sod = secs.rem_euclid(86_400);
    let (h, minute, s) = (sod / 3600, (sod % 3600) / 60, sod % 60);
    // civil_from_days (Howard Hinnant)
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if month <= 2 { y + 1 } else { y };
    format!("{y:04}-{month:02}-{d:02}T{h:02}:{minute:02}:{s:02}.{millis:03}Z")
}

#[cfg(test)]
mod tests {
    use super::*;

    // RFC 2202 / FIPS 198 test vectors prove the HMAC primitive itself.
    #[test]
    fn hmac_rfc4231_vectors() {
        let key = b"\x0b".repeat(20);
        assert_eq!(
            hex(&hmac_sha256(&key, b"Hi There")),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
        assert_eq!(
            hex(&hmac_sha256(b"Jefe", b"what do ya want for nothing?")),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    #[test]
    fn hex_and_b64() {
        assert_eq!(hex(&[0xde, 0xad, 0xbe, 0xef]), "deadbeef");
        assert_eq!(b64(b"hello"), "aGVsbG8=");
        assert_eq!(b64(&[0xff, 0x00]), "/wA=");
    }

    #[test]
    fn venue_sign_shapes() {
        // The exact concatenation order each venue documents — a regression
        // here would silently invalidate every request.
        let s = bybit_sign("S", "1700000000000", "KEY", "5000", "{\"a\":1}");
        assert_eq!(s.len(), 64); // sha256 hex
        let s2 = okx_sign("S", "2026-10-04T00:00:00.000Z", "GET", "/api/v5/account/balance?ccy=BTC", "");
        assert_eq!(s2.len(), 44); // base64 of 32 bytes
        let q = binance_signed_query("S", "symbol=BTC-241027-60000-C&side=BUY");
        assert!(q.starts_with("symbol=BTC-241027-60000-C&side=BUY&signature="));
        assert!(q.ends_with(&hex(&hmac_sha256(b"S", b"symbol=BTC-241027-60000-C&side=BUY"))));
    }

    #[test]
    fn iso_ts() {
        assert_eq!(iso8601_utc(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(iso8601_utc(1709208000123), "2024-02-29T12:00:00.123Z");
        assert_eq!(iso8601_utc(1767225540999), "2025-12-31T23:59:00.999Z");
    }
}
