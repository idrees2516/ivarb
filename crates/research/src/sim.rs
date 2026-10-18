//! Synthetic research data — used only when no live venue is reachable AND
//! the engine fallback is also dead, so the research UI still demonstrates
//! the full analytics path. Clearly labeled `sim` in every response.

use ivarb_protocol::research::{Candle, ChainDigestRow};
use ivarb_protocol::Right;

use crate::analytics;

/// Deterministic pseudo-random walk (xorshift) so sim data is stable
/// across rebuilds within a session.
struct SimRng(u64);
impl SimRng {
    fn next_f64(&mut self) -> f64 {
        // xorshift64*
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        ((x.wrapping_mul(0x2545F4914F6CDD1D) >> 11) as f64 / (1u64 << 53) as f64) * 2.0 - 1.0
    }
}

const SIM_SPOTS: &[(&str, f64)] = &[("BTC", 82450.0), ("ETH", 1980.0), ("SOL", 132.4)];

fn sim_spot(und: &str) -> f64 {
    SIM_SPOTS
        .iter()
        .find(|(u, _)| *u == und)
        .map(|(_, p)| *p)
        .unwrap_or(100.0)
}

/// Build a coherent synthetic near-ATM digest for the given venues
/// (named `sim:<venue>`), mirroring the engine tap's bounds.
pub fn synthetic_digest(now_ms: i64, venues: &[&str]) -> ivarb_protocol::research::ChainDigest {
    let mut rng = SimRng(0x9E3779B97F4A7C15 ^ now_ms as u64);
    let mut rows = Vec::new();
    for (und, spot) in SIM_SPOTS {
        // 4 expiries: ~7, ~14, ~30, ~60 days out (Fri 08:00 UTC).
        for (ei, dte) in [7i64, 14, 30, 60].iter().enumerate() {
            let date = ymd_plus_days(now_ms, *dte);
            let atm_iv = 52.0 + ei as f64 * 1.4 + rng.next_f64() * 2.0;
            for right in [Right::Call, Right::Put] {
                for i in -10i64..=10 {
                    let strike = round_strike(spot * (1.0 + i as f64 * 0.011));
                    let lm = (strike / spot).ln();
                    // Smile: skew + convexity.
                    let skew = if right == Right::Put { -14.0 * lm } else { 6.0 * lm };
                    let smile = 46.0 * lm * lm;
                    let iv = (atm_iv + skew + smile + rng.next_f64() * 0.6).max(15.0);
                    let t = (*dte as f64 + 1.0) / 365.0;
                    let premium = ivarb_quant::bs_price(
                        *spot,
                        strike,
                        t,
                        iv / 100.0,
                        match right {
                            Right::Call => ivarb_quant::QuantRight::Call,
                            Right::Put => ivarb_quant::QuantRight::Put,
                        },
                    )
                    .unwrap_or(1.0);
                    for (vi, v) in venues.iter().enumerate() {
                        let venue_iv = iv + rng.next_f64() * 1.5;
                        let px = premium * (1.0 + rng.next_f64() * 0.01);
                        let spread = px * (0.012 + rng.next_f64().abs() * 0.02);
                        rows.push(ChainDigestRow {
                            venue: format!("sim:{v}"),
                            und: (*und).to_string(),
                            date,
                            strike,
                            right,
                            mark_px: Some(px),
                            bid_px: Some((px - spread * 0.5).max(0.01)),
                            ask_px: Some(px + spread * 0.5),
                            iv_mark_vp: Some(venue_iv),
                            iv_bid_vp: Some(venue_iv - 0.6),
                            iv_ask_vp: Some(venue_iv + 0.6),
                            size_bid: Some(round2(2.0 + rng.next_f64().abs() * 40.0)),
                            size_ask: Some(round2(2.0 + rng.next_f64().abs() * 40.0)),
                            ts_ms: now_ms - (vi as i64) * 137,
                        });
                    }
                }
            }
        }
    }
    ivarb_protocol::research::ChainDigest {
        ts_ms: now_ms,
        rows,
    }
}

/// Synthetic candles: a trendful random walk ending at the sim spot.
pub fn synthetic_candles(now_ms: i64, und: &str, interval: &str, n: usize) -> Vec<Candle> {
    let step_ms = match interval {
        "15m" => 900_000,
        "4h" => 14_400_000,
        "1d" => 86_400_000,
        _ => 3_600_000,
    };
    let spot = sim_spot(und);
    let mut rng = SimRng(0x1234_5678 ^ (und.len() as u64) ^ now_ms as u64);
    // Walk backwards from spot.
    let mut closes = vec![spot];
    let vol = 0.011;
    let drift = 0.0006;
    for _ in 1..n {
        let prev = *closes.last().unwrap();
        let step = prev / (1.0 + drift + rng.next_f64() * vol);
        closes.push(step);
    }
    closes.reverse();
    let t0 = now_ms - (n as i64 - 1) * step_ms;
    closes
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let o = if i == 0 { *c } else { closes[i - 1] };
            let wick = c * (0.002 + rng.next_f64().abs() * 0.006);
            Candle {
                t_ms: t0 + i as i64 * step_ms,
                o,
                h: o.max(*c) + wick,
                l: o.min(*c) - wick,
                c: *c,
                v: (80.0 + rng.next_f64().abs() * 220.0).round(),
            }
        })
        .collect()
}

/// CandleSeries with indicators for the sim path.
pub fn synthetic_series(now_ms: i64, und: &str, interval: &str) -> ivarb_protocol::research::CandleSeries {
    let mut s = analytics::indicators(und, interval, synthetic_candles(now_ms, und, interval, 240));
    s.data_mode = "sim".to_string();
    s
}

fn ymd_plus_days(now_ms: i64, days: i64) -> u32 {
    use chrono::Datelike;
    let d = chrono::DateTime::from_timestamp_millis(now_ms)
        .map(|t| t.date_naive() + chrono::Duration::days(days))
        .unwrap_or_else(|| chrono::NaiveDate::from_ymd_opt(2026, 10, 30).unwrap());
    (d.year() as u32) * 10_000 + d.month() * 100 + d.day()
}

fn round_strike(k: f64) -> f64 {
    if k >= 10_000.0 {
        (k / 500.0).round() * 500.0
    } else if k >= 1000.0 {
        (k / 50.0).round() * 50.0
    } else if k >= 100.0 {
        (k / 5.0).round() * 5.0
    } else {
        (k / 0.5).round() * 0.5
    }
}

fn round2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn synthetic_digest_shape() {
        let now = 1_790_000_000_000i64;
        let d = synthetic_digest(now, &["deribit", "bybit"]);
        assert!(!d.rows.is_empty());
        assert_eq!(d.ts_ms, now);
        assert!(d.rows.iter().all(|r| r.venue.starts_with("sim:")));
        assert!(d.rows.iter().all(|r| r.iv_mark_vp.unwrap() > 10.0));
        // premium sanity: call spread monotonic-ish, no negatives
        assert!(d.rows.iter().all(|r| r.bid_px.unwrap() > 0.0 && r.ask_px.unwrap() > r.bid_px.unwrap()));
    }

    #[test]
    fn synthetic_candles_end_at_spot() {
        let now = 1_790_000_000_000i64;
        let cs = synthetic_candles(now, "BTC", "1h", 240);
        assert_eq!(cs.len(), 240);
        assert!((cs.last().unwrap().c - 82450.0).abs() < 1e-6);
        assert!(cs.windows(2).all(|w| w[1].t_ms > w[0].t_ms));
    }
}
