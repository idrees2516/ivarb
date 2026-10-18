//! Technical + vol-surface analytics: candle indicators (EMA / BB / RSI /
//! ATR / realized vol) and the IV analytics response (term structure, skew,
//! cross-venue divergences).


use ivarb_protocol::research::{
    Candle, IndicatorReadout, IndicatorSeries, IvAnalytics, IvDivergence, IvPoint, SkewPoint,
};
use ivarb_protocol::Right;

use crate::consolidate::{dte_of, iso_date, ConsolidatedBook};

// ---------------------------------------------------------------------------
// Candle indicators
// ---------------------------------------------------------------------------

fn ema(values: &[f64], period: usize) -> Vec<Option<f64>> {
    if period == 0 || values.len() < period {
        return vec![None; values.len()];
    }
    let k = 2.0 / (period as f64 + 1.0);
    let mut out = Vec::with_capacity(values.len());
    let mut seed = 0.0;
    for v in values.iter().take(period) {
        seed += v;
    }
    seed /= period as f64;
    for _ in 0..period - 1 {
        out.push(None);
    }
    out.push(Some(seed));
    let mut prev = seed;
    for &v in &values[period..] {
        prev = v * k + prev * (1.0 - k);
        out.push(Some(prev));
    }
    out
}

fn bollinger(values: &[f64], period: usize, mult: f64) -> (Vec<Option<f64>>, Vec<Option<f64>>) {
    let mut up = vec![None; values.len()];
    let mut lo = vec![None; values.len()];
    if period == 0 || values.len() < period {
        return (up, lo);
    }
    for i in period - 1..values.len() {
        let w = &values[i + 1 - period..=i];
        let m = w.iter().sum::<f64>() / period as f64;
        let var = w.iter().map(|x| (x - m) * (x - m)).sum::<f64>() / period as f64;
        let sd = var.sqrt();
        up[i] = Some(m + mult * sd);
        lo[i] = Some(m - mult * sd);
    }
    (up, lo)
}

fn rsi(values: &[f64], period: usize) -> Vec<Option<f64>> {
    let mut out = vec![None; values.len()];
    if values.len() <= period || period == 0 {
        return out;
    }
    let mut gain = 0.0;
    let mut loss = 0.0;
    for i in 1..=period {
        let d = values[i] - values[i - 1];
        if d >= 0.0 {
            gain += d;
        } else {
            loss -= d;
        }
    }
    let mut avg_gain = gain / period as f64;
    let mut avg_loss = loss / period as f64;
    let rsi_of = |g: f64, l: f64| -> f64 {
        if l <= 1e-12 {
            100.0
        } else {
            100.0 - 100.0 / (1.0 + g / l)
        }
    };
    out[period] = Some(rsi_of(avg_gain, avg_loss));
    for i in period + 1..values.len() {
        let d = values[i] - values[i - 1];
        let (g, l) = if d >= 0.0 { (d, 0.0) } else { (0.0, -d) };
        avg_gain = (avg_gain * (period as f64 - 1.0) + g) / period as f64;
        avg_loss = (avg_loss * (period as f64 - 1.0) + l) / period as f64;
        out[i] = Some(rsi_of(avg_gain, avg_loss));
    }
    out
}

fn atr_pcts(candles: &[Candle], period: usize) -> Option<f64> {
    if candles.len() < period + 1 || period == 0 {
        return None;
    }
    let mut trs = Vec::with_capacity(candles.len() - 1);
    for i in 1..candles.len() {
        let c = &candles[i];
        let prev_close = candles[i - 1].c;
        let tr = (c.h - c.l)
            .max((c.h - prev_close).abs())
            .max((c.l - prev_close).abs());
        trs.push(tr);
    }
    let n = trs.len();
    let w = &trs[n - period..];
    Some(w.iter().sum::<f64>() / period as f64)
}

/// Annualized realized vol (vol points) from log returns of closes.
fn realized_vol_vp(candles: &[Candle], bars: usize, bars_per_year: f64) -> Option<f64> {
    if candles.len() < bars + 1 || bars < 2 {
        return None;
    }
    let tail = &candles[candles.len() - bars - 1..];
    let mut rets = Vec::with_capacity(bars);
    for w in tail.windows(2) {
        if w[0].c > 0.0 && w[1].c > 0.0 {
            rets.push((w[1].c / w[0].c).ln());
        }
    }
    if rets.len() < bars / 2 {
        return None;
    }
    let n = rets.len() as f64;
    let m = rets.iter().sum::<f64>() / n;
    let var = rets.iter().map(|r| (r - m) * (r - m)).sum::<f64>() / (n - 1.0);
    Some(var.sqrt() * bars_per_year.sqrt() * 100.0)
}

/// Bars per year by interval id.
pub fn bars_per_year(interval: &str) -> f64 {
    let secs = match interval {
        "15m" => 900.0,
        "1h" => 3600.0,
        "4h" => 14400.0,
        "1d" => 86400.0,
        _ => 3600.0,
    };
    365.0 * 86400.0 / secs
}

/// Compute overlay series + the latest readout for a candle set.
pub fn indicators(und: &str, interval: &str, candles: Vec<Candle>) -> ivarb_protocol::research::CandleSeries {
    let closes: Vec<f64> = candles.iter().map(|c| c.c).collect();
    let e20 = ema(&closes, 20);
    let e50 = ema(&closes, 50);
    let (bu, bl) = bollinger(&closes, 20, 2.0);
    let r = rsi(&closes, 14);

    let last_of = |v: &[Option<f64>]| v.iter().flatten().last().copied();
    let rsi14 = last_of(&r);
    let ema20 = last_of(&e20);
    let ema50 = last_of(&e50);
    let (bbu, bbl) = (last_of(&bu), last_of(&bl));
    let close = closes.last().copied();
    let ema_trend = match (ema20, ema50, close) {
        (Some(a), Some(b), Some(c)) if a > b * 1.001 && c > b => "up".to_string(),
        (Some(a), Some(b), Some(c)) if a < b * 0.999 && c < b => "down".to_string(),
        (Some(_), Some(_), Some(_)) => "flat".to_string(),
        _ => "flat".to_string(),
    };
    let bb_pos = match (close, bbu, bbl) {
        (Some(c), Some(u), Some(l)) if u > l => Some(((c - l) / (u - l) * 2.0 - 1.0).clamp(-1.0, 1.0)),
        _ => None,
    };
    let atr14_pct = atr_pcts(&candles, 14)
        .and_then(|a| close.map(|c| if c > 0.0 { Some(a / c * 100.0) } else { None }))
        .flatten();
    let rv = realized_vol_vp(&candles, 20, bars_per_year(interval));
    let change_24h_pct = if candles.len() >= 24 {
        let back = candles[candles.len() - 24].c;
        if back > 0.0 {
            close.map(|c| (c / back - 1.0) * 100.0)
        } else {
            None
        }
    } else {
        None
    };

    ivarb_protocol::research::CandleSeries {
        und: und.to_string(),
        interval: interval.to_string(),
        candles,
        overlays: IndicatorSeries {
            ema20: e20,
            ema50: e50,
            bb_upper: bu,
            bb_lower: bl,
            rsi14: r,
        },
        readout: IndicatorReadout {
            rsi14,
            atr14_pct,
            realized_vol_vp: rv,
            ema_trend,
            bb_pos,
            change_24h_pct,
        },
        data_mode: "live".to_string(),
    }
}

/// Parse Binance kline rows: [openTime, o, h, l, c, v, ...].
pub fn parse_binance_klines(rows: &[serde_json::Value]) -> Vec<Candle> {
    rows.iter()
        .filter_map(|r| {
            let a = r.as_array()?;
            let g = |i: usize| -> Option<f64> { a.get(i)?.as_str()?.parse::<f64>().ok() };
            Some(Candle {
                t_ms: a.first()?.as_i64()?,
                o: g(1)?,
                h: g(2)?,
                l: g(3)?,
                c: g(4)?,
                v: g(5).unwrap_or(0.0),
            })
        })
        .collect()
}

// ---------------------------------------------------------------------------
// IV analytics (term structure, skew, divergences)
// ---------------------------------------------------------------------------

/// Build the `/api/research/iv` response from the consolidated book.
pub fn iv_analytics(book: &ConsolidatedBook, und: &str) -> IvAnalytics {
    let spot = book.spots.get(und).copied().unwrap_or(f64::NAN);
    let rows: Vec<&ivarb_protocol::research::ChainRow> =
        book.rows.iter().filter(|r| r.und == und).collect();

    // ---- term structure (composite) ----
    let dates: Vec<u32> = rows.iter().map(|r| r.date).collect::<std::collections::BTreeSet<_>>().into_iter().collect();
    let now_ms = book.ts_ms;
    let term: Vec<IvPoint> = dates
        .iter()
        .filter_map(|d| {
            book.atm_by_date
                .get(&(und.to_string(), *d))
                .map(|iv| IvPoint {
                    dte: dte_of(now_ms, *d),
                    iv_vp: *iv,
                })
        })
        .filter(|p| p.dte >= 0)
        .collect();
    let dates_vec: Vec<u32> = dates.clone();

    // ---- term by venue (near-ATM IV per venue per expiry) ----
    let mut venue_names: Vec<String> = rows
        .iter()
        .flat_map(|r| r.venues.iter().map(|v| v.venue.clone()))
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    let mut term_by_venue: Vec<(String, Vec<IvPoint>)> = Vec::new();
    for v in venue_names.drain(..) {
        let mut pts: Vec<IvPoint> = Vec::new();
        for d in &dates_vec {
            if (dte_of(now_ms, *d)) < 0 {
                continue;
            }
            let mut ivs: Vec<f64> = rows
                .iter()
                .filter(|r| r.date == *d)
                .filter(|r| {
                    spot.is_finite()
                        && spot > 0.0
                        && (r.strike / spot).ln().abs() <= 0.03
                })
                .filter_map(|r| {
                    r.venues
                        .iter()
                        .find(|q| q.venue == v)
                        .and_then(|q| q.iv_mark_vp)
                        .filter(|x| (5.0..500.0).contains(x))
                })
                .collect();
            if !ivs.is_empty() {
                ivs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
                pts.push(IvPoint {
                    dte: dte_of(now_ms, *d),
                    iv_vp: ivs[ivs.len() / 2],
                });
            }
        }
        if !pts.is_empty() {
            term_by_venue.push((v, pts));
        }
    }

    // ---- skew on the expiry nearest ~14 days: IV by |delta| ----
    let skew_date = dates_vec
        .iter()
        .filter(|d| dte_of(now_ms, **d) >= 3)
        .min_by_key(|d| (dte_of(now_ms, **d) as i64 - 14).abs())
        .copied();
    let skew = skew_date
        .map(|d| skew_for_expiry(book, und, d, spot, now_ms))
        .unwrap_or_default();

    // ---- top cross-venue divergences (near-ATM, any expiry) ----
    let mut divs: Vec<IvDivergence> = Vec::new();
    for r in &rows {
        if r.dte < 1 || r.dte > 60 || r.venues.len() < 2 {
            continue;
        }
        let mut ivs: Vec<(&str, f64)> = r
            .venues
            .iter()
            .filter_map(|q| q.iv_mark_vp.map(|iv| (q.venue.as_str(), iv)))
            .filter(|(_, iv)| (5.0..500.0).contains(iv))
            .collect();
        if ivs.len() < 2 {
            continue;
        }
        ivs.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
        let (lo_n, lo_v) = ivs[0];
        let (hi_n, hi_v) = ivs[ivs.len() - 1];
        if hi_v - lo_v >= 1.0 {
            divs.push(IvDivergence {
                und: und.to_string(),
                date: r.date,
                strike: r.strike,
                right: r.right,
                dte: r.dte,
                low_venue: lo_n.to_string(),
                low_iv_vp: lo_v,
                high_venue: hi_n.to_string(),
                high_iv_vp: hi_v,
                spread_vp: hi_v - lo_v,
            });
        }
    }
    divs.sort_by(|a, b| b.spread_vp.partial_cmp(&a.spread_vp).unwrap_or(std::cmp::Ordering::Equal));
    divs.truncate(12);

    let summ = book.iv.iter().find(|s| s.und == und);
    let atm_iv_vp = summ.and_then(|s| s.atm_iv_vp);
    let vr_note = summ
        .and_then(|s| s.vr_premium_vp)
        .map(|vr| {
            if vr >= 8.0 {
                "rich"
            } else if vr <= -3.0 {
                "cheap"
            } else {
                "fair"
            }
        });

    IvAnalytics {
        und: und.to_string(),
        spot,
        data_mode: book.data_mode.to_string(),
        term,
        term_by_venue,
        skew,
        atm_iv_vp,
        iv_percentile: summ.and_then(|s| s.iv_percentile),
        vr_note: vr_note.map(|s| s.to_string()),
        pc_ratio_oi: summ.and_then(|s| s.pc_ratio_oi),
        top_divergences: divs,
    }
}

/// Skew smile: IV by absolute delta for puts and calls of one expiry.
fn skew_for_expiry(
    book: &ConsolidatedBook,
    und: &str,
    date: u32,
    spot: f64,
    now_ms: i64,
) -> Vec<SkewPoint> {
    use ivarb_quant::{bs_delta, QuantRight};
    // delta buckets 0.10 .. 0.90
    let mut buckets: rustc_hash::FxHashMap<u32, (f64, f64, u32, u32)> = rustc_hash::FxHashMap::default();
    for r in &book.rows {
        if r.und != und || r.date != date {
            continue;
        }
        let Some(iv) = r.iv_consensus_vp.filter(|x| (5.0..500.0).contains(x)) else {
            continue;
        };
        if !spot.is_finite() || spot <= 0.0 {
            continue;
        }
        let t = ((crate::consolidate::ymd_to_ms(date).unwrap_or(now_ms) - now_ms).max(0) as f64
            / 1000.0
            / (365.0 * 86400.0))
            .max(1e-5);
        let right = match r.right {
            Right::Call => QuantRight::Call,
            Right::Put => QuantRight::Put,
        };
        let d = bs_delta(spot, r.strike, t, iv / 100.0, right).abs();
        if !(0.04..=0.96).contains(&d) {
            continue;
        }
        let b = ((d * 20.0).round() as u32).clamp(1, 19);
        let e = buckets.entry(b).or_insert((0.0, 0.0, 0, 0));
        match r.right {
            Right::Call => {
                e.0 += iv;
                e.2 += 1;
            }
            Right::Put => {
                e.1 += iv;
                e.3 += 1;
            }
        }
    }
    let mut out: Vec<SkewPoint> = buckets
        .into_iter()
        .map(|(b, (cs, ps, cn, pn))| SkewPoint {
            delta: b as f64 / 20.0,
            put_iv_vp: if pn > 0 { Some(ps / pn as f64) } else { None },
            call_iv_vp: if cn > 0 { Some(cs / cn as f64) } else { None },
        })
        .collect();
    out.sort_by(|a, b| a.delta.partial_cmp(&b.delta).unwrap_or(std::cmp::Ordering::Equal));
    out
}

/// Convenience: ISO date string list for the chain UI.
pub fn expiry_list(book: &ConsolidatedBook, und: &str) -> Vec<(u32, String, i32)> {
    let now_ms = book.ts_ms;
    let mut ds: Vec<u32> = book
        .rows
        .iter()
        .filter(|r| r.und == und)
        .map(|r| r.date)
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    ds.reverse(); // nearest first
    ds.into_iter()
        .map(|d| (d, iso_date(d), dte_of(now_ms, d)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk_candles(n: usize, base: f64) -> Vec<Candle> {
        (0..n)
            .map(|i| {
                let c = base * (1.0 + 0.01 * ((i as f64 * 0.7).sin()));
                Candle {
                    t_ms: 1_700_000_000_000 + i as i64 * 3_600_000,
                    o: c * 0.999,
                    h: c * 1.005,
                    l: c * 0.995,
                    c,
                    v: 100.0,
                }
            })
            .collect()
    }

    #[test]
    fn indicators_lengths_and_rsi_bounds() {
        let cs = mk_candles(120, 100.0);
        let out = indicators("BTC", "1h", cs);
        assert_eq!(out.candles.len(), 120);
        assert_eq!(out.overlays.ema20.len(), 120);
        assert!(out.overlays.ema20[18].is_none());
        assert!(out.overlays.ema20[19].is_some());
        let rsi = out.readout.rsi14.unwrap();
        assert!((0.0..=100.0).contains(&rsi));
        assert!(out.readout.realized_vol_vp.unwrap() > 0.0);
        assert_eq!(out.readout.ema_trend, "flat");
    }

    #[test]
    fn realized_vol_positive_for_moving_series() {
        let cs = mk_candles(80, 50.0);
        let rv = realized_vol_vp(&cs, 20, bars_per_year("1h")).unwrap();
        assert!(rv > 1.0 && rv < 500.0, "rv={rv}");
    }

    #[test]
    fn parse_klines_roundtrip() {
        let rows: Vec<serde_json::Value> = serde_json::json!([
            [1791817200000i64, "82592.0", "82800.0", "82592.0", "82730.5", "451.2", 1791820799999i64, "0"],
            [1791820800000i64, "82730.5", "82962.4", "82490.1", "82510.6", "351.7", 1791824399999i64, "0"]
        ])
        .as_array()
        .unwrap()
        .clone();
        let cs = parse_binance_klines(&rows);
        assert_eq!(cs.len(), 2);
        assert!((cs[0].c - 82730.5).abs() < 1e-9);
        assert_eq!(cs[1].t_ms, 1791820800000);
    }

    #[test]
    fn rsi_flat_is_neutral_high() {
        // monotonically rising series → RSI near 100
        let vals: Vec<f64> = (0..40).map(|i| 100.0 + i as f64).collect();
        let r = rsi(&vals, 14);
        let last = r.last().unwrap().unwrap();
        assert!(last > 95.0, "rsi={last}");
    }
}
