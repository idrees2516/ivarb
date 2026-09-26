//! Compact instrument keys and cross-venue symbol parsing.
//!
//! A canonical key identifies one economic contract across venues:
//! `(underlying, expiry yyyymmdd, strike, right)`. Strikes are quantized to
//! 1e-4 ticks so that `65000` from Deribit matches `65000` from OKX exactly.

use std::collections::HashMap;

use rustc_hash::FxHashMap;

pub const RIGHT_CALL: u8 = 0;
pub const RIGHT_PUT: u8 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct InstKey {
    /// Interned underlying id.
    pub und: u16,
    /// Expiry as yyyymmdd (UTC 08:00 expiry convention).
    pub date: u32,
    /// strike × 10,000.
    pub strike_ticks: u64,
    /// RIGHT_CALL / RIGHT_PUT.
    pub right: u8,
}

impl InstKey {
    #[inline]
    pub fn strike(&self) -> f64 {
        self.strike_ticks as f64 / 10_000.0
    }
}

/// Interning map for underlying symbols ("BTC" → 0, "ETH" → 1, ...).
#[derive(Default)]
pub struct UnderlyingInterner {
    map: FxHashMap<String, u16>,
    pub names: Vec<String>,
}

impl UnderlyingInterner {
    pub fn intern(&mut self, name: &str) -> u16 {
        if let Some(id) = self.map.get(name) {
            return *id;
        }
        let id = self.names.len() as u16;
        self.names.push(name.to_string());
        self.map.insert(name.to_string(), id);
        id
    }
    pub fn name(&self, id: u16) -> &str {
        self.names
            .get(id as usize)
            .map(|s| s.as_str())
            .unwrap_or("?")
    }
}

const MONTHS: [(&str, u32); 12] = [
    ("JAN", 1),
    ("FEB", 2),
    ("MAR", 3),
    ("APR", 4),
    ("MAY", 5),
    ("JUN", 6),
    ("JUL", 7),
    ("AUG", 8),
    ("SEP", 9),
    ("OCT", 10),
    ("NOV", 11),
    ("DEC", 12),
];

fn month3(s: &str) -> Option<u32> {
    MONTHS
        .iter()
        .find(|(m, _)| *m == s)
        .map(|(_, n)| *n)
}

fn valid_ymd(y: i32, m: u32, d: u32) -> Option<u32> {
    if !(2000..=2100).contains(&y) || !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    // Reject impossible day-of-month for the given month (leap-aware enough).
    let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
    let dim = match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if leap {
                29
            } else {
                28
            }
        }
        _ => return None,
    };
    if d > dim {
        return None;
    }
    Some((y as u32) * 10_000 + m * 100 + d)
}

/// Today as yyyymmdd (UTC), for ambiguous 6-digit date disambiguation.
fn today_ymd() -> u32 {
    use chrono::Datelike;
    let d = chrono::Utc::now().date_naive();
    (d.year() as u32) * 10_000 + d.month() * 100 + d.day()
}

/// Days since 1970-01-01 for a yyyymmdd (Howard Hinnant's days_from_civil)
/// — used to pick the 6-digit reading closest to today.
fn days_index(ymd: u32) -> i64 {
    let y = (ymd / 10_000) as i64;
    let m = ((ymd / 100) % 100) as i64;
    let d = (ymd % 100) as i64;
    let (y_adj, m_adj) = if m <= 2 { (y - 1, m + 9) } else { (y, m - 3) };
    let era = if y_adj >= 0 { y_adj } else { y_adj - 399 } / 400;
    let yoe = y_adj - era * 400;
    let doy = (153 * m_adj + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// 6-digit token parsed as DDMMYY (Delta style) instead of YYMMDD.
fn parse_date_ddmmyy(t: &str) -> Option<u32> {
    let up = t.trim().to_uppercase();
    if up.len() == 6 && up.chars().all(|c| c.is_ascii_digit()) {
        let dd: u32 = up[0..2].parse().ok()?;
        let mm: u32 = up[2..4].parse().ok()?;
        let yy: i32 = up[4..6].parse().ok()?;
        return valid_ymd(2000 + yy, mm, dd);
    }
    None
}

/// Parse a date token in any of the venue formats:
/// `DDMMMYY` (24SEP26), `DDMMMYYYY`, `YYMMDD` (260926), `YYYYMMDD` (20260926).
/// Ambiguous 6-digit tokens (Delta quotes `250926` for 2026-09-25) are
/// disambiguated by picking the reading **closest to today** — option
/// expiries are always near-dated, so the reading a year away is wrong.
pub fn parse_date_token(t: &str) -> Option<u32> {
    let up = t.trim().to_uppercase();
    // Pure-digit forms first so YYMMDD is not swallowed by DDMMMYY.
    if up.len() == 6 && up.chars().all(|c| c.is_ascii_digit()) {
        let yy: i32 = up[0..2].parse().ok()?;
        let mm: u32 = up[2..4].parse().ok()?;
        let dd: u32 = up[4..6].parse().ok()?;
        let ymd = valid_ymd(2000 + yy, mm, dd);
        let alt = parse_date_ddmmyy(t);
        return match (ymd, alt) {
            (Some(a), Some(b)) => {
                let today = days_index(today_ymd());
                let da = (days_index(a) - today).abs();
                let db = (days_index(b) - today).abs();
                if db < da {
                    Some(b)
                } else {
                    Some(a)
                }
            }
            (Some(a), None) => Some(a),
            (None, Some(b)) => Some(b),
            (None, None) => None,
        };
    }
    if up.len() == 8 && up.chars().all(|c| c.is_ascii_digit()) {
        let y: i32 = up[0..4].parse().ok()?;
        let m: u32 = up[4..6].parse().ok()?;
        let d: u32 = up[6..8].parse().ok()?;
        return valid_ymd(y, m, d);
    }
    // Alphanumeric DDMMMYY / DDMMMYYYY.
    if up.len() >= 6 && up.len() <= 9 && up.chars().all(|c| c.is_ascii_alphanumeric()) {
        let dd: u32 = up.get(0..2)?.parse().ok()?;
        let mon = month3(up.get(2..5)?)?;
        let ystr = &up[5..];
        let year: i32 = match ystr.len() {
            2 => 2000 + ystr.parse::<i32>().ok()?,
            4 => ystr.parse::<i32>().ok()?,
            _ => return None,
        };
        return valid_ymd(year, mon, dd);
    }
    None
}

/// Parse an option symbol from any supported venue into
/// `(underlying, yyyymmdd, strike, RIGHT_* )`.
///
/// Handles: `BTC-24SEP26-65000-C` (Deribit), `BTC-USD-260926-65000-C` (OKX),
/// `BTC-260926-65000-C` (Binance), `BTC-20260926-65000-C` (Derive),
/// `BTC-24SEP26-65000-C-USDT` (Bybit trailing margin token), and Delta's
/// right-first `C-BTC-83400-260926`.
pub fn parse_option_symbol(sym: &str) -> Option<(String, u32, f64, u8)> {
    let raw_parts: Vec<&str> = sym.split('-').collect();
    if raw_parts.len() < 4 {
        return None;
    }
    // Strip a trailing settlement-currency token (Bybit: ...-C-USDT).
    let mut parts = raw_parts;
    const MARGIN: [&str; 5] = ["USDT", "USD", "USDC", "USDCE", "PERP"];
    if parts.len() >= 5
        && MARGIN.contains(&parts[parts.len() - 1].trim().to_uppercase().as_str())
    {
        parts.pop();
    }
    if parts.len() < 4 {
        return None;
    }

    // Delta style: {C|P}-{UND}-{STRIKE}-{YYMMDD}
    let first = parts[0].trim().to_uppercase();
    if matches!(first.as_str(), "C" | "P" | "CALL" | "PUT") {
        let right = if matches!(first.as_str(), "C" | "CALL") {
            RIGHT_CALL
        } else {
            RIGHT_PUT
        };
        let und = parts[1].trim().to_uppercase();
        if (2..=6).contains(&und.len()) && und.chars().all(|c| c.is_ascii_alphabetic()) {
            if let Some(strike) = parts[2].trim().parse::<f64>().ok() {
                if strike.is_finite() && strike > 0.0 && strike < 1e9 {
                    if let Some(d) = parse_date_token(parts[parts.len() - 1]) {
                        return Some((und, d, strike, right));
                    }
                }
            }
        }
        return None;
    }

    // Standard style: {UND}-...-{STRIKE}-{C|P}
    let und = parts[0].trim().to_uppercase();
    if und.len() < 2 || und.len() > 6 || !und.chars().all(|c| c.is_ascii_alphabetic()) {
        return None;
    }
    let right = match parts[parts.len() - 1].trim().to_uppercase().as_str() {
        "C" | "CALL" => RIGHT_CALL,
        "P" | "PUT" => RIGHT_PUT,
        _ => return None,
    };
    let strike: f64 = parts[parts.len() - 2].trim().parse().ok()?;
    if !(strike.is_finite() && strike > 0.0 && strike < 1e9) {
        return None;
    }
    for p in &parts[1..parts.len() - 2] {
        if let Some(d) = parse_date_token(p) {
            return Some((und, d, strike, right));
        }
    }
    None
}

/// Expiry timestamp (ms) at 08:00 UTC — the Deribit/OKX/Bybit/Binance
/// crypto-options settlement convention.
pub fn ymd_to_expiry_ms(date: u32) -> Option<i64> {
    let y = (date / 10_000) as i32;
    let m = ((date / 100) % 100) as u32;
    let d = (date % 100) as u32;
    use chrono::NaiveDate;
    let nd = NaiveDate::from_ymd_opt(y, m, d)?;
    let dt = nd.and_hms_opt(8, 0, 0)?;
    use chrono::TimeZone;
    Some(chrono::Utc.from_utc_datetime(&dt).timestamp_millis())
}

/// Whole days from `now` to the expiry date (can be negative when expired).
pub fn dte_days(now_ms: i64, date: u32) -> i64 {
    match ymd_to_expiry_ms(date) {
        Some(exp) => (exp - now_ms) / 86_400_000,
        None => i64::MAX / 4,
    }
}

/// Time to expiry in years (365-day convention, floored to avoid blowups).
#[inline]
pub fn t_years(now_ms: i64, expiry_ms: i64) -> f64 {
    let dt = (expiry_ms - now_ms).max(0) as f64 / 1000.0;
    (dt / (365.0 * 24.0 * 3600.0)).max(1e-5)
}

pub fn iso_from_ymd(date: u32) -> String {
    let y = date / 10_000;
    let m = (date / 100) % 100;
    let d = date % 100;
    format!("{y:04}-{m:02}-{d:02}")
}

/// Pick the nearest `max_n` distinct expiry dates within the DTE window.
pub fn cap_expiries(dates: &mut Vec<u32>, now_ms: i64, min_dte: i32, max_dte: i32, max_n: usize) {
    dates.retain(|d| {
        let dte = dte_days(now_ms, *d);
        dte >= min_dte as i64 && dte <= max_dte as i64
    });
    dates.sort_unstable();
    dates.dedup();
    dates.truncate(max_n);
}

/// Keep only strikes within `atm_pct` (log) of the forward/spot.
pub fn keep_near_atm(
    strikes: &mut Vec<(f64, u32, u8)>,
    spot: f64,
    atm_pct: f64,
    cap: usize,
) {
    if spot > 0.0 && atm_pct > 0.0 {
        let lo = (-atm_pct).exp();
        let hi = atm_pct.exp();
        strikes.retain(|(k, _, _)| {
            let r = k / spot;
            r >= lo && r <= hi
        });
    }
    strikes.truncate(cap);
}

/// Convenience: HashMap alias used across connectors.
pub type SymbolMap = HashMap<String, InstKey>;

#[cfg(test)]
mod tests {
    use super::*;

    fn k(und: &str, date: u32, strike: f64, right: u8) -> (String, u32, f64, u8) {
        (und.to_string(), date, strike, right)
    }

    #[test]
    fn parses_all_venue_formats() {
        // Deribit / Bybit
        assert_eq!(
            parse_option_symbol("BTC-24SEP26-65000-C"),
            Some(k("BTC", 20260924, 65000.0, RIGHT_CALL))
        );
        assert_eq!(
            parse_option_symbol("ETH-26JUN27-3200-P"),
            Some(k("ETH", 20270626, 3200.0, RIGHT_PUT))
        );
        // OKX (USD/USDT family in the middle)
        assert_eq!(
            parse_option_symbol("BTC-USD-260924-65000-C"),
            Some(k("BTC", 20260924, 65000.0, RIGHT_CALL))
        );
        assert_eq!(
            parse_option_symbol("ETH-USDT-260924-3200-P"),
            Some(k("ETH", 20260924, 3200.0, RIGHT_PUT))
        );
        // Binance / Thalex style
        assert_eq!(
            parse_option_symbol("BTC-260924-65000-C"),
            Some(k("BTC", 20260924, 65000.0, RIGHT_CALL))
        );
        // Derive ISO style
        assert_eq!(
            parse_option_symbol("BTC-20260924-65000-C"),
            Some(k("BTC", 20260924, 65000.0, RIGHT_CALL))
        );
        // Delta right-first style (dates are DDMMYY)
        assert_eq!(
            parse_option_symbol("C-BTC-83400-260926"),
            Some(k("BTC", 20260926, 83400.0, RIGHT_CALL))
        );
        assert_eq!(
            parse_option_symbol("P-ETH-2580-250926"),
            Some(k("ETH", 20260925, 2580.0, RIGHT_PUT))
        );
        // Bybit trailing margin token
        assert_eq!(
            parse_option_symbol("BTC-25JUN27-106000-P-USDT"),
            Some(k("BTC", 20270625, 106000.0, RIGHT_PUT))
        );
        // CALL/PUT word suffix (standard style)
        assert_eq!(
            parse_option_symbol("BTC-24SEP26-65000-CALL"),
            Some(k("BTC", 20260924, 65000.0, RIGHT_CALL))
        );
        // fractional strikes
        assert_eq!(
            parse_option_symbol("SOL-260924-220.5-P"),
            Some(k("SOL", 20260924, 220.5, RIGHT_PUT))
        );
    }

    #[test]
    fn rejects_non_options() {
        assert!(parse_option_symbol("BTC-PERPETUAL").is_none());
        assert!(parse_option_symbol("BTC-24SEP26").is_none());
        assert!(parse_option_symbol("BTC-USDT-SWAP").is_none());
        assert!(parse_option_symbol("").is_none());
        assert!(parse_option_symbol("BTC-24SEP26-65000-X").is_none());
        assert!(parse_option_symbol("BTC-99XXX26-65000-C").is_none());
        // invalid day
        assert!(parse_option_symbol("BTC-32SEP26-65000-C").is_none());
    }

    #[test]
    fn expiry_math() {
        let ms = ymd_to_expiry_ms(20260926).unwrap();
        // 2026-09-26 08:00 UTC
        assert_eq!(ms, 1_790_409_600_000);
        let t = t_years(ms - 86_400_000, ms);
        assert!((t - 1.0 / 365.0).abs() < 1e-6, "{t}");
        assert_eq!(iso_from_ymd(20260926), "2026-09-26");
        assert_eq!(dte_days(ymd_to_expiry_ms(20260925).unwrap(), 20260926), 1);
    }
}
