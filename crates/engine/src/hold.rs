//! Hold-to-expiry candidate tracking for the engine.
//!
//! The scanner only creates opportunities above the fee-adjusted entry gate —
//! by design. But a divergence *below* the gate is not worthless: the cheap
//! leg may still be a positive-EV outright long-vol position if the
//! cross-venue consensus IV sits above the ask-implied IV by more than the
//! entry fee. This module tracks those near-misses and publishes them to the
//! Hold-to-Expiry Lab, where the full Monte-Carlo analyzer runs on demand.

use std::collections::VecDeque;

use ivarb_protocol::{HoldCandidate, HoldStats};
use ivarb_quant::{hold_analyze_lite, vega_per_vp_unit};

use crate::key::{iso_from_ymd, t_years, InstKey};
use crate::strategy::LegQuote;

/// Rolling set of near-miss candidates.
pub struct HoldTracker {
    /// inst_id → candidate.
    items: rustc_hash::FxHashMap<String, HoldCandidate>,
    order: VecDeque<String>,
    cap: usize,
    /// GA-evolved EV gate (USD/contract) above which a candidate is kept.
    ev_gate_usd: f64,
}

const DEFAULT_CAP: usize = 60;

impl HoldTracker {
    pub fn new() -> Self {
        Self {
            items: rustc_hash::FxHashMap::default(),
            order: VecDeque::with_capacity(DEFAULT_CAP + 8),
            cap: DEFAULT_CAP,
            ev_gate_usd: 5.0,
        }
    }

    pub fn set_ev_gate(&mut self, usd: f64) {
        self.ev_gate_usd = usd.clamp(0.0, 100.0);
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Snapshot of candidates sorted by hold EV.
    pub fn snapshot(&self) -> Vec<HoldCandidate> {
        let mut v: Vec<HoldCandidate> = self.items.values().cloned().collect();
        v.sort_by(|a, b| {
            b.hold
                .ev_usd
                .partial_cmp(&a.hold.ev_usd)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        v
    }

    /// Insert/refresh one candidate from the cross-venue quote set.
    ///
    /// `legs` are (venue, fee_rate, contract_size, quote) for one instrument,
    /// fresh at `now`. The cheapest ask is the hold target; the consensus IV
    /// is the median of the venues' mark IVs.
    #[allow(clippy::too_many_arguments)]
    pub fn refresh(
        &mut self,
        key: &InstKey,
        und_name: &str,
        legs: &[(&str, f64, f64, &LegQuote)],
        spot: f64,
        expiry_ms: i64,
        now: i64,
        stale_ms: i64,
    ) {
        if legs.len() < 2 || spot <= 0.0 {            return;
        }
        let expiry = iso_from_ymd(key.date);
        let t = t_years(now, expiry_ms);
        if t <= 1e-5 || t > 1.2 {            return;
        }

        // Cheapest fresh ask across venues.
        let mut best: Option<(&str, f64, f64, &LegQuote)> = None;
        for l in legs {
            let (venue, fee, cs, q) = *l;
            if now - q.recv_ms > stale_ms {
                continue;
            }
            if let Some(ask) = q.px_ask_unit.filter(|p| *p > 0.0) {
                if best
                    .map(|(_, _, _, bq)| {
                        let bask = bq.px_ask_unit.unwrap_or(f64::INFINITY);
                        ask < bask
                    })
                    .unwrap_or(true)
                {
                    best = Some((venue, fee, cs, q));
                }
            }
        }
        let Some((venue, fee_rate, contract_size, bq)) = best else {            return;
        };
        let ask_px = bq.px_ask_unit.unwrap_or(0.0);
        if ask_px <= 0.0 {            return;
        }

        // Consensus forecast: median of the venues' mark IVs (vp).
        let mut marks: Vec<f64> = legs
            .iter()
            .filter_map(|(_, _, _, q)| q.iv_mark_vp)
            .filter(|v| (15.0..=250.0).contains(v))
            .collect();
        if marks.len() < 2 {            return;
        }
        marks.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let consensus_vp = marks[marks.len() / 2];
        let consensus_raw = consensus_vp / 100.0;

        // The arb view (best sell bid vs this ask) for contrast.
        let vega_unit = vega_per_vp_unit(spot, key.strike(), t, consensus_raw);
        if vega_unit < 0.5 {            return; // dust vega: fees in vp explode, nothing to discuss
        }
        let mut best_bid: Option<(f64, f64)> = None; // (bid, sell-venue fee rate)
        for (_, fee_v, _, q) in legs {
            if now - q.recv_ms > stale_ms {
                continue;
            }
            if let Some(b) = q.px_bid_unit.filter(|p| *p > 0.0) {
                best_bid = Some(match best_bid {
                    Some((x, f)) if x >= b => (x, f),
                    _ => (b, *fee_v),
                });
            }
        }
        let (arb_spread_vp, arb_fee_vp, arb_net_vp) = match best_bid {
            Some((bid, sell_fee_rate)) => {
                let spread = (bid - ask_px) / vega_unit;
                let fee = ivarb_quant::fee_vp(fee_rate, spot, vega_unit)
                    + ivarb_quant::fee_vp(sell_fee_rate, spot, vega_unit);
                (spread, fee, spread - fee)
            }
            None => (0.0, 0.0, -1.0),
        };

        let hold: HoldStats = to_stats(hold_analyze_lite(
            spot,
            key.strike(),
            t,
            crate::strategy::qright(key.right),
            ask_px,
            fee_rate,
            consensus_raw,
            contract_size,
        ));

        // Keep candidates that are (a) below the arb entry gate and (b) have
        // a meaningful hold EV or IV edge.
        let iv_edge = hold.iv_edge_vp;        if arb_net_vp >= 0.5 {            return; // real arb — the scanner owns it
        }
        if hold.ev_usd < self.ev_gate_usd * 0.2 && iv_edge < 0.5 {            return; // nothing to hold either
        }
        let implied_vp = ivarb_quant::implied_vol(ask_px, spot, key.strike(), t, crate::strategy::qright(key.right))
            .map(|v| v * 100.0)
            .unwrap_or(0.0);

        let id = format!(
            "{}-{}-{}-{}",
            und_name,
            key.date,
            crate::strategy::fmt_key_strike(key.strike()),
            if key.right == crate::key::RIGHT_CALL { "C" } else { "P" }
        );
        let cand = HoldCandidate {
            inst_id: id.clone(),
            underlying: und_name.to_string(),
            expiry,
            strike: key.strike(),
            right: if key.right == crate::key::RIGHT_CALL {
                ivarb_protocol::Right::Call
            } else {
                ivarb_protocol::Right::Put
            },
            venue: venue.to_string(),
            ask_px_unit: ask_px,
            consensus_iv_vp: consensus_vp,
            implied_iv_vp: implied_vp,
            fee_rate,
            spot,
            t_years: t,
            contract_size,
            arb_spread_vp,
            arb_fee_vp,
            arb_net_vp,
            vega_vp_usd: vega_unit * contract_size,
            hold,
            updated_ms: now,
        };
        if !self.items.contains_key(&id) {
            self.order.push_back(id.clone());
            while self.order.len() > self.cap {
                if let Some(old) = self.order.pop_front() {
                    self.items.remove(&old);
                }
            }
        }
        self.items.insert(id, cand);
    }

    /// Drop candidates whose quotes went stale.
    pub fn housekeeping(&mut self, now: i64, stale_ms: i64) {
        let mut kill: Vec<String> = Vec::new();
        for (id, c) in &self.items {
            if now - c.updated_ms > stale_ms * 4 {
                kill.push(id.clone());
            }
        }
        for id in kill {
            self.items.remove(&id);
            self.order.retain(|x| *x != id);
        }
    }
}

/// Hold-to-expiry stats for an *active* arb opportunity's cheap (buy) leg —
/// the "arb is good, but what if I just kept the option?" view. Reconstructed
/// from the opportunity's own wire fields: entry fee is estimated as half the
/// round-trip fee_vp on the leg vega, and the vol forecast is the mid IV of
/// the two venues (the same consensus the scanner uses).
pub fn hold_stats_for_opp(
    spot: f64,
    strike: f64,
    t_years: f64,
    right_call: bool,
    ask_px_unit: f64,
    roundtrip_fee_vp: f64,
    vega_unit: f64,
    consensus_iv_raw: f64,
    contract_size: f64,
) -> HoldStats {
    let fee_rate = if spot > 0.0 && vega_unit > 1e-9 {
        (roundtrip_fee_vp * 0.5 * vega_unit / spot).clamp(0.0, 0.01)
    } else {
        0.0003
    };
    let right = if right_call {
        ivarb_quant::Right::Call
    } else {
        ivarb_quant::Right::Put
    };
    to_stats(ivarb_quant::hold_analyze_lite(
        spot,
        strike,
        t_years,
        right,
        ask_px_unit,
        fee_rate,
        consensus_iv_raw,
        contract_size,
    ))
}

impl Default for HoldTracker {
    fn default() -> Self {
        Self::new()
    }
}

fn to_stats(l: ivarb_quant::HoldStatsLite) -> HoldStats {
    HoldStats {
        ev_usd: l.ev_usd,
        theta_day_usd: l.theta_day_usd,
        extrinsic_usd: l.extrinsic_usd,
        p_profit: l.p_profit,
        iv_edge_vp: l.iv_edge_vp,
        dte: l.dte,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::key::RIGHT_CALL;
    use crate::msg::InstrTick;

    fn q(iv_mark: f64, bid: f64, ask: f64, recv_ms: i64) -> LegQuote {
        let t = InstrTick {
            key: InstKey { und: 0, date: 20261230, strike_ticks: 840_000_000, right: RIGHT_CALL },
            iv_mark_vp: Some(iv_mark),
            iv_bid_vp: None,
            iv_ask_vp: None,
            px_mark_unit: Some((bid + ask) / 2.0),
            px_bid_unit: Some(bid),
            px_ask_unit: Some(ask),
            size_bid: Some(1.0),
            size_ask: Some(1.0),
            spot: Some(84_000.0),
            contract_size: None,
            venue_ts_ms: Some(recv_ms),
        };
        LegQuote::from_tick(&t, recv_ms)
    }

    /// `now` sits 30 days before the fixed expiry so the tenor is stable
    /// regardless of the wall clock the test runs on; ticks are fresh.
    fn test_now() -> i64 {
        crate::key::ymd_to_expiry_ms(20261230).unwrap() - 30 * 86_400_000
    }

// scratch: trace where refresh() bails
#[test]
fn debug_hold_trace() {
    use super::*;
    use crate::key::RIGHT_CALL;
    use crate::msg::InstrTick;
    let now = crate::key::ymd_to_expiry_ms(20261230).unwrap() - 30 * 86_400_000;
    let recv = now - 1_000;
    let mk = |iv: f64, bid: f64, ask: f64| {
        let t = InstrTick {
            key: InstKey { und: 0, date: 20261230, strike_ticks: 840_000_000, right: RIGHT_CALL },
            iv_mark_vp: Some(iv), iv_bid_vp: None, iv_ask_vp: None,
            px_mark_unit: Some((bid + ask) / 2.0), px_bid_unit: Some(bid), px_ask_unit: Some(ask),
            size_bid: Some(1.0), size_ask: Some(1.0), spot: Some(84_000.0),
            contract_size: None, venue_ts_ms: Some(recv),
        };
        LegQuote::from_tick(&t, recv)
    };
    let qa = mk(62.0, 5_900.0, 5_920.0);
    let qb = mk(65.5, 5_960.0, 5_985.0);
    let key = InstKey { und: 0, date: 20261230, strike_ticks: 840_000_000, right: RIGHT_CALL };
    let t_y = crate::key::t_years(now, crate::key::ymd_to_expiry_ms(20261230).unwrap());
    eprintln!("t_years = {t_y}");
    let vega_unit = ivarb_quant::vega_per_vp_unit(84_000.0, 84_000.0, t_y, 0.6375);
    eprintln!("vega_unit = {vega_unit}");
    let exp = ivarb_quant::expected_terminal_payoff(84_000.0, 84_000.0, t_y, 0.6375, 0.0, ivarb_quant::Right::Call);
    eprintln!("expected payoff = {exp}");
    let l = ivarb_quant::hold_analyze_lite(84_000.0, 84_000.0, t_y, ivarb_quant::Right::Call, 5_920.0, 0.0003, 0.6375, 1.0);
    eprintln!("lite ev_usd = {}, iv_edge = {}, theta = {}", l.ev_usd, l.iv_edge_vp, l.theta_day_usd);
    let spread = (5_960.0 - 5_920.0) / vega_unit;
    let fee = ivarb_quant::fee_vp(0.0003, 84_000.0, vega_unit) * 2.0;
    eprintln!("spread_vp = {spread}, fee_vp = {fee}, net = {}", spread - fee);
}

    #[test]
    fn tracks_near_miss_below_arb_gate() {
        let mut tr = HoldTracker::new();
        let now = test_now();
        let recv = now - 1_000;
        let qa = q(62.0, 5_900.0, 5_920.0, recv); // cheap venue
        let qb = q(65.5, 5_960.0, 5_985.0, recv); // rich venue
        let key = InstKey { und: 0, date: 20261230, strike_ticks: 840_000_000, right: RIGHT_CALL };
        let legs: Vec<(&str, f64, f64, &LegQuote)> = vec![
            ("a", 0.0003, 1.0, &qa),
            ("b", 0.0003, 1.0, &qb),
        ];
        tr.refresh(&key, "BTC", &legs, 84_000.0, crate::key::ymd_to_expiry_ms(20261230).unwrap(), now, 45_000);
        assert_eq!(tr.len(), 1, "near-miss should be tracked");
        let snap = tr.snapshot();
        let c = &snap[0];
        // Spread ≈ (5 960 − 5 920)/vega; vega(84k, 30d, 64%) ≈ ~95 → ≈ 0.42 vp;
        // fees ≈ 2 × 0.0003×84000/95 ≈ 0.53 vp → net < 0.5 gate → near-miss.
        assert!(c.arb_net_vp < 0.5, "net {} must be below gate", c.arb_net_vp);
        // Consensus 63.75 % vs ask-implied ≈ 62.2 % → ~1.5 vp edge → EV > 0.
        assert!(c.hold.iv_edge_vp > 0.5, "iv edge {}", c.hold.iv_edge_vp);
        assert!(c.hold.ev_usd > 0.0, "ev {}", c.hold.ev_usd);
        assert!(c.hold.theta_day_usd < 0.0);
    }

    #[test]
    fn ignores_real_arbs() {
        let mut tr = HoldTracker::new();
        let now = test_now();
        let recv = now - 1_000;
        let qa = q(58.0, 5_100.0, 5_120.0, recv);
        let qb = q(66.0, 5_600.0, 5_640.0, recv);
        let key = InstKey { und: 0, date: 20261230, strike_ticks: 840_000_000, right: RIGHT_CALL };
        let legs: Vec<(&str, f64, f64, &LegQuote)> = vec![
            ("a", 0.0003, 1.0, &qa),
            ("b", 0.0003, 1.0, &qb),
        ];
        tr.refresh(&key, "BTC", &legs, 84_000.0, crate::key::ymd_to_expiry_ms(20261230).unwrap(), now, 45_000);
        // Spread ≈ 480/95 ≈ 5 vp ≫ gate → scanner territory, not the hold lab.
        assert_eq!(tr.len(), 0);
    }
}
