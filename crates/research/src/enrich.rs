//! Research REST enrichment: OI / volume statistics per venue, underlying
//! candles, and order-book depth for the liquidity map. All pollers fail
//! soft — a dead endpoint only means fewer numbers, never a broken book.

use rustc_hash::{FxHashMap, FxHashSet};

use ivarb_protocol::research::Candle;
use ivarb_protocol::Right;

use crate::analytics;

#[derive(Debug, Clone, Copy, Default)]
pub struct EnrichStats {
    /// Open interest, venue contracts.
    pub oi: Option<f64>,
    /// 24h volume, venue contracts.
    pub vol24: Option<f64>,
    /// 24h premium volume, USD (venue-reported when available).
    pub vol_usd: Option<f64>,
}

/// Everything the enrichment pollers have collected so far.
#[derive(Default)]
pub struct EnrichStore {
    /// venue → canonical instrument → stats.
    per_venue: FxHashMap<String, FxHashMap<String, EnrichStats>>,
    /// Venues whose last poll succeeded.
    ok: FxHashSet<String>,
    /// Underlyings with live candles (per interval in `klines_ok`).
    pub klines_ok: FxHashSet<String>,
}

impl EnrichStore {
    pub fn snapshot(&self) -> FxHashMap<String, FxHashMap<String, EnrichStats>> {
        self.per_venue.clone()
    }
    pub fn ok_venues(&self) -> Vec<String> {
        let mut v: Vec<String> = self.ok.iter().cloned().collect();
        v.sort();
        v
    }
    pub fn klines_ok_list(&self) -> Vec<String> {
        let mut v: Vec<String> = self.klines_ok.iter().cloned().collect();
        v.sort();
        v
    }
}

/// Underlying units per venue contract / reported volume unit (mirrors the
/// engine's venue specs; binance eapi reports volume directly in underlying
/// units — verified: volume × price = amount on /eapi/v1/ticker).
pub fn venue_contract_units(venue: &str) -> f64 {
    match venue {
        "okx" | "delta" => 0.01,
        _ => 1.0,
    }
}

/// True when the venue quotes premiums in the coin (BTC) rather than USD.
pub fn venue_coin_quoted(venue: &str) -> bool {
    matches!(venue, "deribit" | "okx" | "derive" | "aevo")
}

// ---------------------------------------------------------------------------
// Symbol parsing (independent of the engine crate)
// ---------------------------------------------------------------------------

const MONTHS: [(&str, u32); 12] = [
    ("JAN", 1), ("FEB", 2), ("MAR", 3), ("APR", 4), ("MAY", 5), ("JUN", 6),
    ("JUL", 7), ("AUG", 8), ("SEP", 9), ("OCT", 10), ("NOV", 11), ("DEC", 12),
];

fn parse_date(tok: &str) -> Option<u32> {
    let up = tok.trim().to_uppercase();
    if up.len() == 6 && up.chars().all(|c| c.is_ascii_digit()) {
        // YYMMDD (Binance / OKX); Paradex/Deribit use DDMMMYY (letters).
        let yy: i32 = up[0..2].parse().ok()?;
        let mm: u32 = up[2..4].parse().ok()?;
        let dd: u32 = up[4..6].parse().ok()?;
        return valid(2000 + yy, mm, dd);
    }
    if up.len() == 8 && up.chars().all(|c| c.is_ascii_digit()) {
        let y: i32 = up[0..4].parse().ok()?;
        let m: u32 = up[4..6].parse().ok()?;
        let d: u32 = up[6..8].parse().ok()?;
        return valid(y, m, d);
    }
    if up.len() == 7 && up.chars().all(|c| c.is_ascii_alphanumeric()) {
        // DDMMMYY
        let dd: u32 = up[0..2].parse().ok()?;
        let mon = MONTHS.iter().find(|(m, _)| *m == &up[2..5]).map(|(_, n)| *n)?;
        let yy: i32 = up[5..7].parse().ok()?;
        return valid(2000 + yy, mon, dd);
    }
    if up.len() == 9 && up.chars().all(|c| c.is_ascii_alphanumeric()) {
        // DDMMMYYYY
        let dd: u32 = up[0..2].parse().ok()?;
        let mon = MONTHS.iter().find(|(m, _)| *m == &up[2..5]).map(|(_, n)| *n)?;
        let yy: i32 = up[5..9].parse().ok()?;
        return valid(yy, mon, dd);
    }
    None
}

fn valid(y: i32, m: u32, d: u32) -> Option<u32> {
    if !(2000..=2100).contains(&y) || !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    Some(y as u32 * 10_000 + m * 100 + d)
}

/// Parse an option symbol from any polled venue into
/// `(underlying, yyyymmdd, strike, right)`.
pub fn parse_symbol(sym: &str) -> Option<(String, u32, f64, Right)> {
    let mut parts: Vec<&str> = sym.split('-').collect();
    const MARGIN: [&str; 6] = ["USDT", "USD", "USDC", "USDCE", "PERP", "UM"];
    while parts.len() > 4 && MARGIN.contains(&parts[parts.len() - 1].to_uppercase().as_str()) {
        parts.pop();
    }
    if parts.len() < 4 {
        return None;
    }
    // Right token: last part.
    let right = match parts[parts.len() - 1].to_uppercase().as_str() {
        "C" | "CALL" => Right::Call,
        "P" | "PUT" => Right::Put,
        _ => return None,
    };
    let strike: f64 = parts[parts.len() - 2].trim().parse().ok()?;
    if !(strike.is_finite() && strike > 0.0 && strike < 1e9) {
        return None;
    }
    let date = parse_date(parts[parts.len() - 3])?;
    let und = parts[0].trim().to_uppercase();
    if und.len() < 2 || und.len() > 6 || !und.chars().all(|c| c.is_ascii_alphabetic()) {
        return None;
    }
    Some((und, date, strike, right))
}

// ---------------------------------------------------------------------------
// Venue stat polls
// ---------------------------------------------------------------------------

fn jf(v: &serde_json::Value, names: &[&str]) -> Option<f64> {
    for n in names {
        match v.get(*n) {
            Some(serde_json::Value::Number(x)) => {
                if let Some(f) = x.as_f64() {
                    if f.is_finite() {
                        return Some(f);
                    }
                }
            }
            Some(serde_json::Value::String(s)) => {
                let t = s.trim();
                if !t.is_empty() {
                    if let Ok(f) = t.parse::<f64>() {
                        if f.is_finite() {
                            return Some(f);
                        }
                    }
                }
            }
            _ => {}
        }
    }
    None
}

/// Deribit: one call per currency → OI, volume, volume_usd for the whole chain.
pub async fn poll_deribit(http: &reqwest::Client, unds: &[String]) -> anyhow::Result<FxHashMap<String, EnrichStats>> {
    let mut out = FxHashMap::default();
    for und in unds {
        let url = format!(
            "https://www.deribit.com/api/v2/public/get_book_summary_by_currency?currency={und}&kind=option"
        );
        let v: serde_json::Value = http
            .get(&url)
            .timeout(std::time::Duration::from_secs(15))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        for r in v.get("result").and_then(|x| x.as_array()).unwrap_or(&vec![]) {
            let Some(name) = r.get("instrument_name").and_then(|x| x.as_str()) else { continue };
            let Some((u, d, k, right)) = parse_symbol(name) else { continue };
            if u != *und {
                continue;
            }
            let canon = crate::consolidate::canon(&u, d, k, right);
            out.insert(
                canon,
                EnrichStats {
                    oi: jf(r, &["open_interest"]),
                    vol24: jf(r, &["volume"]),
                    vol_usd: jf(r, &["volume_usd"]),
                },
            );
        }
    }
    if out.is_empty() {
        anyhow::bail!("deribit enrichment empty");
    }
    Ok(out)
}

/// Bybit: option tickers → OI + 24h volume.
pub async fn poll_bybit(http: &reqwest::Client, unds: &[String]) -> anyhow::Result<FxHashMap<String, EnrichStats>> {
    let mut out = FxHashMap::default();
    for und in unds {
        let url = format!(
            "https://api.bybit.com/v5/market/tickers?category=option&baseCoin={und}"
        );
        let v: serde_json::Value = http
            .get(&url)
            .timeout(std::time::Duration::from_secs(15))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let list = v
            .pointer("/result/list")
            .and_then(|x| x.as_array())
            .cloned()
            .unwrap_or_default();
        for r in &list {
            let Some(name) = r.get("symbol").and_then(|x| x.as_str()) else { continue };
            let Some((u, d, k, right)) = parse_symbol(name) else { continue };
            if u != *und {
                continue;
            }
            let canon = crate::consolidate::canon(&u, d, k, right);
            out.insert(
                canon,
                EnrichStats {
                    oi: jf(&r, &["openInterest"]),
                    vol24: jf(&r, &["volume24h"]),
                    vol_usd: jf(&r, &["turnover24h"]),
                },
            );
        }
    }
    if out.is_empty() {
        anyhow::bail!("bybit enrichment empty");
    }
    Ok(out)
}

/// Binance eapi: 24h ticker → volume + premium volume (one call, whole market).
pub async fn poll_binance(http: &reqwest::Client, unds: &[String]) -> anyhow::Result<FxHashMap<String, EnrichStats>> {
    let v: serde_json::Value = http
        .get("https://eapi.binance.com/eapi/v1/ticker")
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let arr = v.as_array().cloned().unwrap_or_default();
    let mut out = FxHashMap::default();
    for r in &arr {
        let Some(name) = r.get("symbol").and_then(|x| x.as_str()) else { continue };
        let Some((u, d, k, right)) = parse_symbol(name) else { continue };
        if !unds.contains(&u) {
            continue;
        }
        let canon = crate::consolidate::canon(&u, d, k, right);
        out.insert(
            canon,
            EnrichStats {
                oi: None,
                // eapi "volume" is in underlying units; "amount" is the
                // USD premium turnover (volume × price, verified live).
                vol24: jf(&r, &["volume"]),
                vol_usd: jf(&r, &["amount"]),
            },
        );
    }
    if out.is_empty() {
        anyhow::bail!("binance enrichment empty");
    }
    Ok(out)
}

/// OKX: option tickers → 24h volume (no OI in the public tickers).
pub async fn poll_okx(http: &reqwest::Client, unds: &[String]) -> anyhow::Result<FxHashMap<String, EnrichStats>> {
    let mut out = FxHashMap::default();
    for und in unds {
        let url = format!(
            "https://www.okx.com/api/v5/market/tickers?instType=OPTION&uly={und}-USD"
        );
        let v: serde_json::Value = http
            .get(&url)
            .timeout(std::time::Duration::from_secs(15))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let arr = v.get("data").and_then(|x| x.as_array()).cloned().unwrap_or_default();
        for r in &arr {
            let Some(name) = r.get("instId").and_then(|x| x.as_str()) else { continue };
            let Some((u, d, k, right)) = parse_symbol(name) else { continue };
            if u != *und {
                continue;
            }
            let canon = crate::consolidate::canon(&u, d, k, right);
            out.insert(
                canon,
                EnrichStats {
                    oi: None,
                    vol24: jf(&r, &["vol24h"]),
                    vol_usd: jf(&r, &["volCcy24h"]),
                },
            );
        }
    }
    if out.is_empty() {
        anyhow::bail!("okx enrichment empty");
    }
    Ok(out)
}

/// One enrichment sweep: poll every venue, replace its map, report status.
pub async fn enrichment_sweep(
    http: &reqwest::Client,
    unds: &[String],
) -> Vec<(&'static str, anyhow::Result<FxHashMap<String, EnrichStats>>)> {
    let (d, b, o, k) = tokio::join!(
        poll_deribit(http, unds),
        poll_bybit(http, unds),
        poll_binance(http, unds),
        poll_okx(http, unds)
    );
    vec![("deribit", d), ("bybit", b), ("binance", o), ("okx", k)]
}

pub fn apply_sweep(store: &mut EnrichStore, sweep: Vec<(&'static str, anyhow::Result<FxHashMap<String, EnrichStats>>)>) {
    for (venue, res) in sweep {
        match res {
            Ok(map) => {
                store.per_venue.insert(venue.to_string(), map);
                store.ok.insert(venue.to_string());
            }
            Err(e) => {
                tracing::debug!(venue, error = %e, "research enrichment poll failed");
                store.ok.remove(venue);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Candles
// ---------------------------------------------------------------------------

/// Binance spot klines for the underlying (public, no key).
pub async fn fetch_klines(
    http: &reqwest::Client,
    und: &str,
    interval: &str,
    limit: u32,
) -> anyhow::Result<Vec<Candle>> {
    let url = format!(
        "https://api.binance.com/api/v3/klines?symbol={und}USDT&interval={interval}&limit={limit}"
    );
    let v: serde_json::Value = http
        .get(&url)
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let rows = v.as_array().cloned().unwrap_or_default();
    let candles = analytics::parse_binance_klines(&rows);
    if candles.len() < 30 {
        anyhow::bail!("too few klines for {und} {interval}");
    }
    Ok(candles)
}

// ---------------------------------------------------------------------------
// Order books (liquidity map)
// ---------------------------------------------------------------------------

/// Rebuild the venue-native symbol from a canonical contract.
pub fn to_venue_symbol(venue: &str, und: &str, date: u32, strike: f64, right: Right) -> Option<String> {
    let y = date / 10_000;
    let m = (date / 100) % 100;
    let d = date % 100;
    let yy = y % 100;
    let ks = crate::consolidate::format_strike(strike);
    let r = right.letter();
    let mon = MONTHS
        .iter()
        .find(|(_, n)| *n == m)
        .map(|(s, _)| *s)
        .unwrap_or("XXX");
    match venue {
        "deribit" => Some(format!("{und}-{d}{mon}{yy:02}-{ks}-{r}")),
        "bybit" => Some(format!("{und}-{d}{mon}{yy:02}-{ks}-{r}-USDT")),
        "binance" => Some(format!("{und}-{yy:02}{m:02}{d:02}-{ks}-{r}")),
        "okx" => Some(format!("{und}-USD-{yy:02}{m:02}{d:02}-{ks}-{r}")),
        _ => None,
    }
}

/// One side of a book, as (price, size) pairs normalized to USD notional.
#[derive(Debug, Clone, Copy)]
pub struct BookLevel {
    pub px_usd_per_unit: f64,
    pub units: f64,
}

/// Fetch an aggregated order book; returns (bids, asks) with USD-per-unit
/// prices and underlying-unit sizes.
pub async fn fetch_book(
    http: &reqwest::Client,
    venue: &str,
    und: &str,
    date: u32,
    strike: f64,
    right: Right,
    spot: f64,
) -> Option<(Vec<BookLevel>, Vec<BookLevel>)> {
    let sym = to_venue_symbol(venue, und, date, strike, right)?;
    let units = venue_contract_units(venue);
    let coin = venue_coin_quoted(venue);
    let norm = |rows: &[serde_json::Value]| -> Vec<BookLevel> {
        rows.iter()
            .filter_map(|r| {
                let a = r.as_array()?;
                let px: f64 = a.first()?.as_str()?.parse().ok()?;
                let sz: f64 = a.get(1)?.as_str().and_then(|s| s.parse().ok()).or_else(|| a.get(1)?.as_f64())?;
                if px <= 0.0 || sz <= 0.0 {
                    return None;
                }
                let px_usd = if coin { px * spot } else { px };
                Some(BookLevel {
                    px_usd_per_unit: px_usd,
                    units: sz * units,
                })
            })
            .collect()
    };
    match venue {
        "deribit" => {
            let v: serde_json::Value = http
                .get(format!(
                    "https://www.deribit.com/api/v2/public/get_order_book?instrument_name={sym}&depth=20"
                ))
                .timeout(std::time::Duration::from_secs(10))
                .send()
                .await
                .ok()?
                .error_for_status()
                .ok()?
                .json()
                .await
                .ok()?;
            let res = v.get("result")?;
            let b = res.get("bids")?.as_array()?.clone();
            let a = res.get("asks")?.as_array()?.clone();
            // Deribit sizes are already in underlying units (BTC).
            let raw = |rows: &[serde_json::Value]| -> Vec<BookLevel> {
                rows.iter()
                    .filter_map(|r| {
                        let a = r.as_array()?;
                        let px: f64 = a.first()?.as_f64()?;
                        let sz: f64 = a.get(1)?.as_f64()?;
                        if px <= 0.0 || sz <= 0.0 {
                            return None;
                        }
                        Some(BookLevel {
                            px_usd_per_unit: px * spot,
                            units: sz,
                        })
                    })
                    .collect()
            };
            Some((raw(&b), raw(&a)))
        }
        "binance" => {
            let v: serde_json::Value = http
                .get(format!(
                    "https://eapi.binance.com/eapi/v1/depth?symbol={sym}&limit=20"
                ))
                .timeout(std::time::Duration::from_secs(10))
                .send()
                .await
                .ok()?
                .error_for_status()
                .ok()?
                .json()
                .await
                .ok()?;
            let b = v.get("bids")?.as_array()?.clone();
            let a = v.get("asks")?.as_array()?.clone();
            Some((norm(&b), norm(&a)))
        }
        "okx" => {
            let v: serde_json::Value = http
                .get(format!(
                    "https://www.okx.com/api/v5/market/books?instId={sym}&sz=20"
                ))
                .timeout(std::time::Duration::from_secs(10))
                .send()
                .await
                .ok()?
                .error_for_status()
                .ok()?
                .json()
                .await
                .ok()?;
            let data = v.get("data")?.as_array()?.first()?.clone();
            let b = data.get("bids")?.as_array()?.clone();
            let a = data.get("asks")?.as_array()?.clone();
            Some((norm(&b), norm(&a)))
        }
        _ => None,
    }
}

/// USD notional resting within `pct` of the best price on one side.
pub fn notional_within(levels: &[BookLevel], pct: f64) -> (f64, f64) {
    let Some(best) = levels.first().map(|l| l.px_usd_per_unit) else {
        return (0.0, 0.0);
    };
    let mut usd = 0.0;
    let mut units = 0.0;
    for l in levels {
        if (l.px_usd_per_unit - best).abs() / best <= pct / 100.0 {
            usd += l.px_usd_per_unit * l.units;
            units += l.units;
        }
    }
    (usd, units)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_all_polled_symbols() {
        // Deribit / Bybit (DDMMMYY)
        assert_eq!(
            parse_symbol("BTC-27NOV26-73000-P"),
            Some(("BTC".into(), 20261127, 73000.0, Right::Put))
        );
        // Binance (YYMMDD)
        assert_eq!(
            parse_symbol("BTC-261225-85000-C"),
            Some(("BTC".into(), 20261225, 85000.0, Right::Call))
        );
        // OKX with USD_UM middle token (skipped as margin)
        assert_eq!(
            parse_symbol("BTC-USD_UM-261127-102000-P"),
            Some(("BTC".into(), 20261127, 102000.0, Right::Put))
        );
        // OKX classic
        assert_eq!(
            parse_symbol("ETH-USD-270924-200000-C"),
            Some(("ETH".into(), 20270924, 200000.0, Right::Call))
        );
        // Paradex (DDMMMYY + USD token)
        assert_eq!(
            parse_symbol("BTC-USD-23OCT26-80000-P"),
            Some(("BTC".into(), 20261023, 80000.0, Right::Put))
        );
        // rejects non-options
        assert!(parse_symbol("BTC-PERPETUAL").is_none());
        assert!(parse_symbol("BTC-27NOV26").is_none());
    }

    #[test]
    fn venue_symbols_roundtrip() {
        let s = to_venue_symbol("deribit", "BTC", 20261127, 73000.0, Right::Put).unwrap();
        assert_eq!(s, "BTC-27NOV26-73000-P");
        let s = to_venue_symbol("binance", "BTC", 20261225, 85000.0, Right::Call).unwrap();
        assert_eq!(s, "BTC-261225-85000-C");
        let s = to_venue_symbol("okx", "ETH", 20270924, 200000.5, Right::Call).unwrap();
        assert_eq!(s, "ETH-USD-270924-200000.5-C");
        // and back
        assert_eq!(
            parse_symbol(&s),
            Some(("ETH".into(), 20270924, 200000.5, Right::Call))
        );
    }

    #[test]
    fn notional_within_bounds() {
        let levels = vec![
            BookLevel { px_usd_per_unit: 100.0, units: 2.0 },
            BookLevel { px_usd_per_unit: 99.0, units: 3.0 },
            BookLevel { px_usd_per_unit: 50.0, units: 10.0 },
        ];
        let (usd, units) = notional_within(&levels, 2.0);
        assert!((usd - (200.0 + 297.0)).abs() < 1e-9);
        assert!((units - 5.0).abs() < 1e-9);
    }
}
