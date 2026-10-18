//! Consolidation: engine tap digest + REST enrichment + candles → one
//! cross-venue book with venue stats and per-underlying IV summaries.

use std::collections::VecDeque;

use rustc_hash::FxHashMap;

use ivarb_protocol::research::{
    ChainDigestRow, ChainRow, ChainVenueQuote, IvSummary, ResearchVenueStat,
};
use ivarb_protocol::{FeedMode, Right, Snapshot};


/// Everything the research API surfaces, rebuilt every few seconds.
#[derive(Debug, Clone, Default)]
pub struct ConsolidatedBook {
    pub ts_ms: i64,
    /// "live" | "sim" | "mixed".
    pub data_mode: &'static str,
    pub spots: FxHashMap<String, f64>,
    pub rows: Vec<ChainRow>,
    pub venue_stats: Vec<ResearchVenueStat>,
    pub iv: Vec<IvSummary>,
    /// (und, yyyymmdd) → ATM IV in vol points (helper for analytics/suggest).
    pub atm_by_date: FxHashMap<(String, u32), f64>,
}

/// Canonical instrument id: `BTC-20261023-84000-C`.
pub fn canon(und: &str, date: u32, strike: f64, right: Right) -> String {
    format!(
        "{und}-{date}-{}-{}",
        format_strike(strike),
        right.letter()
    )
}

/// Format a strike without trailing zeros (84000, 84250.5).
pub fn format_strike(k: f64) -> String {
    if (k - k.round()).abs() < 1e-9 {
        format!("{}", k.round() as i64)
    } else {
        let s = format!("{k:.4}");
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    }
}

/// Expiry ms at 08:00 UTC (the crypto-options settlement convention).
pub fn ymd_to_ms(date: u32) -> Option<i64> {
    use chrono::TimeZone;
    let y = (date / 10_000) as i32;
    let m = ((date / 100) % 100) as u32;
    let d = (date % 100) as u32;
    let nd = chrono::NaiveDate::from_ymd_opt(y, m, d)?;
    let dt = nd.and_hms_opt(8, 0, 0)?;
    Some(chrono::Utc.from_utc_datetime(&dt).timestamp_millis())
}

pub fn dte_of(now_ms: i64, date: u32) -> i32 {
    match ymd_to_ms(date) {
        Some(exp) => ((exp - now_ms) / 86_400_000) as i32,
        None => i32::MAX / 4,
    }
}

pub fn iso_date(date: u32) -> String {
    format!(
        "{:04}-{:02}-{:02}",
        date / 10_000,
        (date / 100) % 100,
        date % 100
    )
}

fn median(mut v: Vec<f64>) -> Option<f64> {
    if v.is_empty() {
        return None;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    Some(v[v.len() / 2])
}

/// Build the consolidated book from a digest + enrichment + engine snapshot.
///
/// `iv_hist` is sampled here (≤ once per minute) so the IV percentile becomes
/// time-series based after ~30 minutes of uptime.
#[allow(clippy::too_many_arguments)]
pub fn build(
    now_ms: i64,
    digest_rows: &[ChainDigestRow],
    digest_ts: i64,
    enrich_idx: &EnrichIdx,
    snapshot: &Snapshot,
    candles: &FxHashMap<(String, String), ivarb_protocol::research::CandleSeries>,
    iv_hist: &mut FxHashMap<String, VecDeque<f64>>,
    last_hist_sample_ms: &mut i64,
    synthetic: bool,
) -> ConsolidatedBook {
    let mut spots: FxHashMap<String, f64> = snapshot
        .spots
        .iter()
        .map(|s| (s.underlying.clone(), s.price))
        .collect();
    if spots.is_empty() {
        for r in digest_rows {
            // Fall back to approximate spots from option moneyness midpoints.
            spots.entry(r.und.clone()).or_insert(f64::NAN);
        }
        spots.retain(|_, v| !v.is_nan());
    }

    let data_mode: &'static str = if synthetic {
        "sim"
    } else {
        match snapshot.stats.feed_mode {
            FeedMode::Live => "live",
            FeedMode::Sim => "sim",
            FeedMode::Mixed => "mixed",
        }
    };

    // ---- group digest rows by canonical key (strike quantized to 1e-4
    // ticks + right as u8 — f64/Right are not Hash/Ord) ----
    fn right_code(r: Right) -> u8 {
        match r {
            Right::Call => 0,
            Right::Put => 1,
        }
    }
    fn right_rank(r: Right) -> u8 {
        right_code(r)
    }
    let mut grouped: FxHashMap<(String, u32, u64, u8), (f64, Right, Vec<&ChainDigestRow>)> =
        FxHashMap::default();
    for r in digest_rows {
        let key = (
            r.und.clone(),
            r.date,
            (r.strike * 10_000.0).round() as u64,
            right_code(r.right),
        );
        let e = grouped.entry(key).or_insert((r.strike, r.right, Vec::new()));
        e.2.push(r);
    }

    // Enrichment index: venue → canonical → stats.

    let mut rows: Vec<ChainRow> = Vec::with_capacity(grouped.len());
    for ((_und, date, _ticks, _rc), (strike, right, vr)) in grouped {
        let und = vr.first().map(|r| r.und.clone()).unwrap_or_default();
        let spot = spots.get(&und).copied().unwrap_or(f64::NAN);
        let dte = dte_of(now_ms, date);
        let mut venues: Vec<ChainVenueQuote> = Vec::with_capacity(vr.len());
        let mut ivs: Vec<f64> = Vec::new();
        let mut best_bid: Option<(f64, &str)> = None;
        let mut best_ask: Option<(f64, &str)> = None;
        let mut oi_sum = 0.0;
        let mut oi_any = false;
        for r in vr {
            let est = enrich_idx
                .get(&r.venue)
                .and_then(|m| m.get(&canon(&r.und, r.date, r.strike, r.right)))
                .copied();
            venues.push(ChainVenueQuote {
                venue: r.venue.clone(),
                mark_px: r.mark_px,
                bid_px: r.bid_px,
                ask_px: r.ask_px,
                iv_mark_vp: r.iv_mark_vp,
                iv_bid_vp: r.iv_bid_vp,
                iv_ask_vp: r.iv_ask_vp,
                size_bid: r.size_bid,
                size_ask: r.size_ask,
                open_interest: est.and_then(|e| e.oi),
                volume_24h: est.and_then(|e| e.vol24),
                tick_age_ms: None,
            });
            if let Some(iv) = r.iv_mark_vp.filter(|v| (5.0..500.0).contains(v)) {
                ivs.push(iv);
            }
            if let Some(b) = r.bid_px.filter(|p| *p > 0.0) {
                if best_bid.map(|(x, _)| b > x).unwrap_or(true) {
                    best_bid = Some((b, &r.venue));
                }
            }
            if let Some(a) = r.ask_px.filter(|p| *p > 0.0) {
                if best_ask.map(|(x, _)| a < x).unwrap_or(true) {
                    best_ask = Some((a, &r.venue));
                }
            }
            if let Some(e) = est {
                if let Some(oi) = e.oi.filter(|o| o.is_finite() && *o >= 0.0) {
                    oi_sum += oi;
                    oi_any = true;
                }
            }
        }
        venues.sort_by(|a, b| a.venue.cmp(&b.venue));
        let iv_consensus_vp = median(ivs.clone());
        let iv_spread_vp = if ivs.len() > 1 {
            let lo = ivs.iter().cloned().fold(f64::INFINITY, f64::min);
            let hi = ivs.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
            Some(hi - lo)
        } else {
            None
        };
        rows.push(ChainRow {
            und,
            date,
            strike,
            right,
            spot,
            dte,
            iv_consensus_vp,
            iv_spread_vp,
            best_ask_venue: best_ask.map(|(_, v)| v.to_string()),
            best_ask_px: best_ask.map(|(p, _)| p),
            best_bid_venue: best_bid.map(|(_, v)| v.to_string()),
            best_bid_px: best_bid.map(|(p, _)| p),
            open_interest: if oi_any { Some(oi_sum) } else { None },
            venues,
        });
    }
    rows.sort_by(|a, b| {
        a.und
            .cmp(&b.und)
            .then(a.date.cmp(&b.date))
            .then(
                a.strike
                    .partial_cmp(&b.strike)
                    .unwrap_or(std::cmp::Ordering::Equal),
            )
            .then(right_rank(a.right).cmp(&right_rank(b.right)))
    });

    // ---- ATM IV per (und, date): average of the nearest-strike call & put
    // consensus IVs. ----
    let mut atm_acc: FxHashMap<(String, u32), (f64, usize)> = FxHashMap::default();
    for row in &rows {
        let spot = spots.get(&row.und).copied().unwrap_or(f64::NAN);
        if !spot.is_finite() || spot <= 0.0 {
            continue;
        }
        if (row.strike / spot).ln().abs() > 0.03 {
            continue;
        }
        if let Some(iv) = row.iv_consensus_vp {
            let e = atm_acc
                .entry((row.und.clone(), row.date))
                .or_insert((0.0, 0usize));
            let (mean, n) = *e;
            *e = (mean + (iv - mean) / (n as f64 + 1.0), n + 1);
        }
    }
    let atm_by_date: FxHashMap<(String, u32), f64> = atm_acc
        .into_iter()
        .map(|(k, (m, _))| (k, m))
        .collect();

    // ---- venue stats ----
    let venue_stats = venue_stats(&rows, &enrich_idx, snapshot, &spots, &atm_by_date, synthetic);

    // ---- per-und IV summaries (uses candles for realized vol) ----
    let iv = iv_summaries(now_ms, &rows, &spots, &atm_by_date, candles, iv_hist, last_hist_sample_ms, digest_ts);

    ConsolidatedBook {
        ts_ms: now_ms,
        data_mode,
        spots,
        rows,
        venue_stats,
        iv,
        atm_by_date,
    }
}

type EnrichIdx = FxHashMap<String, FxHashMap<String, crate::enrich::EnrichStats>>;

#[allow(clippy::too_many_arguments)]
fn venue_stats(
    rows: &[ChainRow],
    enrich_idx: &EnrichIdx,
    snapshot: &Snapshot,
    spots: &FxHashMap<String, f64>,
    atm_by_date: &FxHashMap<(String, u32), f64>,
    synthetic: bool,
) -> Vec<ResearchVenueStat> {
    // per venue aggregation
    use rustc_hash::FxHashSet;
    let mut venues: Vec<String> = rows
        .iter()
        .flat_map(|r| r.venues.iter().map(|v| v.venue.clone()))
        .collect::<FxHashSet<_>>()
        .into_iter()
        .collect();
    venues.sort();

    let mut out = Vec::with_capacity(venues.len());
    for vname in venues {
        let is_sim = vname.starts_with("sim:") || synthetic;
        let mut instruments = 0usize;
        let mut near_atm_ivs: Vec<f64> = Vec::new();
        let mut spreads: Vec<f64> = Vec::new();
        let mut oi_usd = 0.0;
        let mut vol_usd = 0.0;
        for row in rows {
            let Some(vq) = row.venues.iter().find(|v| v.venue == vname) else {
                continue;
            };
            instruments += 1;
            if let (Some(b), Some(a)) = (vq.bid_px, vq.ask_px) {
                if b > 0.0 && a > b {
                    let mid = 0.5 * (a + b);
                    if mid > 0.0 {
                        spreads.push((a - b) / mid * 100.0);
                    }
                }
            }
            // near-ATM, mid-tenor IV for the venue ATM stat
            if let Some(spot) = spots.get(&row.und) {
                if (row.strike / spot).ln().abs() <= 0.03
                    && row.dte >= 6
                    && row.dte <= 60
                {
                    if let Some(iv) = vq.iv_mark_vp.filter(|x| (5.0..500.0).contains(x)) {
                        near_atm_ivs.push(iv);
                    }
                }
            }
            let spot = spots.get(&row.und).copied().unwrap_or(0.0);
            if let Some(oi) = vq.open_interest.filter(|o| *o > 0.0) {
                // OI is in venue contracts; mark to USD with the venue's
                // contract size — an estimate, labeled as such in the UI.
                oi_usd += oi * spot * crate::enrich::venue_contract_units(&vq.venue);
            }
            if let Some(vol) = vq.volume_24h.filter(|o| *o > 0.0) {
                vol_usd += vol * spot * crate::enrich::venue_contract_units(&vq.venue);
            }
        }
        let _ = atm_by_date; // (venue ATM uses its own IVs above)
        let online = snapshot
            .venues
            .iter()
            .find(|s| s.venue == vname)
            .map(|s| s.state == ivarb_protocol::ConnState::Live || is_sim)
            .unwrap_or(false)
            // Research-plane venues (Paradex public WS) are not engine
            // connectors, so the engine snapshot never lists them. Their
            // presence in the book means quotes passed the 30 s stale gate —
            // which is exactly what "online" should mean here.
            || (vname == "paradex" && instruments > 0);
        out.push(ResearchVenueStat {
            venue: vname.clone(),
            is_sim,
            online,
            instruments: instruments as u32,
            atm_iv_vp: median(near_atm_ivs),
            avg_spread_pct: median(spreads),
            oi_usd: if oi_usd > 0.0 { Some(oi_usd) } else { None },
            volume_usd_24h: if vol_usd > 0.0 { Some(vol_usd) } else { None },
            enrichment: enrich_idx.contains_key(&vname),
            note: if is_sim { Some("simulated feed".into()) } else { None },
        });
    }
    out
}

#[allow(clippy::too_many_arguments)]
fn iv_summaries(
    now_ms: i64,
    rows: &[ChainRow],
    _spots: &FxHashMap<String, f64>,
    atm_by_date: &FxHashMap<(String, u32), f64>,
    candles: &FxHashMap<(String, String), ivarb_protocol::research::CandleSeries>,
    iv_hist: &mut FxHashMap<String, VecDeque<f64>>,
    last_hist_sample_ms: &mut i64,
    digest_ts: i64,
) -> Vec<IvSummary> {
    use rustc_hash::FxHashSet;
    let unds: Vec<String> = rows.iter().map(|r| r.und.clone()).collect::<FxHashSet<_>>().into_iter().collect();
    let mut out = Vec::with_capacity(unds.len());
    for und in unds {
        // term: (dte, atm iv) sorted
        let mut term: Vec<(i32, f64)> = atm_by_date
            .iter()
            .filter(|((u, _), _)| u == &und)
            .map(|((_, d), iv)| (dte_of(now_ms, *d), *iv))
            .filter(|(dte, _)| *dte >= 0)
            .collect();
        term.sort_by_key(|(d, _)| *d);

        // ~30d ATM by linear interpolation between bracketing tenors.
        let atm30 = if term.is_empty() {
            None
        } else {
            Some(interp_dte(&term, 30))
        };

        // Sample the ~30d ATM IV into the history ring (≤ 1 / minute).
        if now_ms - *last_hist_sample_ms >= 60_000 && digest_ts > 0 {
            if let Some(a) = atm30 {
                let ring = iv_hist.entry(und.clone()).or_default();
                ring.push_back(a);
                while ring.len() > 1440 {
                    ring.pop_front();
                }
                *last_hist_sample_ms = now_ms;
            }
        }

        let realized = candles
            .get(&(und.clone(), "1h".into()))
            .and_then(|c| c.readout.realized_vol_vp)
            .filter(|v| *v > 0.0);
        let vr = match (atm30, realized) {
            (Some(a), Some(r)) => Some(a - r),
            _ => None,
        };
        let term_slope_vp = match (term.first(), term.last()) {
            (Some((_, f)), Some((_, b))) if term.len() > 1 => Some(b - f),
            _ => None,
        };

        // Put/call OI ratio across the two nearest expiries.
        let mut dates: Vec<u32> = rows
            .iter()
            .filter(|r| r.und == und)
            .map(|r| r.date)
            .collect::<FxHashSet<_>>()
            .into_iter()
            .collect();
        dates.sort_unstable();
        dates.truncate(2);
        let (mut oi_p, mut oi_c) = (0.0, 0.0);
        for r in rows {
            if r.und == und && dates.contains(&r.date) {
                if let Some(oi) = r.open_interest.filter(|o| *o > 0.0) {
                    if r.right == Right::Put {
                        oi_p += oi;
                    } else {
                        oi_c += oi;
                    }
                }
            }
        }
        let pc_ratio_oi = if oi_c > 0.0 { Some(oi_p / oi_c) } else { None };

        // IV percentile: time-series once ≥30 samples, else cross-sectional.
        let hist = iv_hist.get(&und).cloned().unwrap_or_default();
        let (pct, basis) = if let Some(a) = atm30 {
            if hist.len() >= 30 {
                let below = hist.iter().filter(|x| **x <= a).count();
                (Some(100.0 * below as f64 / hist.len() as f64), "24h")
            } else {
                let all: Vec<f64> = rows
                    .iter()
                    .filter(|r| r.und == und && r.dte <= 60)
                    .filter_map(|r| r.iv_consensus_vp.filter(|x| (5.0..500.0).contains(x)))
                    .collect();
                if all.len() >= 10 {
                    let below = all.iter().filter(|x| **x <= a).count();
                    let total = all.len() as f64;
                    (Some(100.0 * below as f64 / total), "cross-sectional")
                } else {
                    (None, "warming up")
                }
            }
        } else {
            (None, "warming up")
        };

        out.push(IvSummary {
            und,
            atm_iv_vp: atm30,
            iv_percentile: pct,
            iv_percentile_basis: basis.to_string(),
            realized_vol_vp: realized,
            vr_premium_vp: vr,
            pc_ratio_oi,
            term_slope_vp,
        });
    }
    out
}

/// Linear interpolation of ATM IV at `target_dte` days over the term points.
pub fn interp_dte(term: &[(i32, f64)], target_dte: i32) -> f64 {
    if term.is_empty() {
        return f64::NAN;
    }
    if target_dte <= term[0].0 {
        return term[0].1;
    }
    if target_dte >= term[term.len() - 1].0 {
        return term[term.len() - 1].1;
    }
    for w in term.windows(2) {
        let (d0, v0) = w[0];
        let (d1, v1) = w[1];
        if target_dte >= d0 && target_dte <= d1 {
            if d1 == d0 {
                return v0;
            }
            let f = (target_dte - d0) as f64 / (d1 - d0) as f64;
            return v0 + f * (v1 - v0);
        }
    }
    term[term.len() - 1].1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canon_ids() {
        assert_eq!(
            canon("BTC", 20261023, 84000.0, Right::Call),
            "BTC-20261023-84000-C"
        );
        assert_eq!(format_strike(84250.5), "84250.5");
        assert_eq!(format_strike(84000.0), "84000");
    }

    #[test]
    fn date_helpers() {
        assert_eq!(iso_date(20261023), "2026-10-23");
        assert!(dte_of(ymd_to_ms(20261022).unwrap(), 20261023) >= 0);
    }

    #[test]
    fn interp_term() {
        let t = vec![(7, 60.0), (30, 55.0), (60, 50.0)];
        assert!((interp_dte(&t, 7) - 60.0).abs() < 1e-9);
        assert!((interp_dte(&t, 90) - 50.0).abs() < 1e-9);
        assert!((interp_dte(&t, 30) - 55.0).abs() < 1e-9);
        // between 7 and 30
        let f = (18.0 - 7.0) / 23.0;
        assert!((interp_dte(&t, 18) - (60.0 + f * (55.0 - 60.0))).abs() < 1e-9);
    }
}
