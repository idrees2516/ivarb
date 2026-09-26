//! Risk manager: every path from "signal" to "order" goes through these gates.
//!
//! Layers (in evaluation order):
//! 1. **Global interlocks** — armed flag, kill switch, daily loss limit.
//! 2. **Capacity** — concurrent attempts, open positions, aggregate vega.
//! 3. **Signal quality** — tier enabled, edge ≥ effective gate, freshness,
//!    per-instrument dedup + cooldown.
//! 4. **Venue health** — both leg venues must be Live in the engine snapshot.
//!
//! The manager owns mutable budget state; all checks + reservations are
//! single-threaded inside the executor core (no locks needed on this struct).

use rustc_hash::FxHashMap;

use ivarb_protocol::{ExecSettings, Opportunity, Snapshot, VenueStatus};
use ivarb_quant::margin_per_contract;

use crate::now_ms;

#[derive(Debug, Default)]
pub struct RiskState {
    /// opp id → retry-not-before (ms).
    pub cooldown_until: FxHashMap<String, i64>,
    /// canonical instrument ids currently owned or attempted.
    pub busy_instruments: FxHashMap<String, u64>,
    /// sum of |units| × vega_per_unit_usd across open positions.
    pub vega_used_usd: f64,
    pub attempts_active: u32,
    pub positions_open: u32,
    /// realized P&L at the start of the current UTC day (anchor for the
    /// daily-loss circuit breaker).
    pub day_anchor_realized: f64,
    pub day_ymd: u32,
}

impl RiskState {
    pub fn new() -> Self {
        let mut s = Self::default();
        s.day_ymd = utc_ymd(now_ms());
        s
    }

    fn day_pnl(&self, realized_total: f64, unrealized: f64) -> f64 {
        (realized_total - self.day_anchor_realized) + unrealized
    }

    /// Full pre-flight for a candidate. Returns the approved size in
    /// underlying units, or a human-readable rejection reason.
    /// `balances` maps venue id → available USD collateral (paper or live).
    /// `equity_scale` compounds sizing with account growth since boot
    /// (1.0 = neutral; the capital gate below remains the final authority).
    #[allow(clippy::too_many_arguments)]
    pub fn preflight(
        &mut self,
        opp: &Opportunity,
        snap: &Snapshot,
        settings: &ExecSettings,
        armed: bool,
        kill: &Option<String>,
        realized_total: f64,
        unrealized: f64,
        balances: &FxHashMap<String, f64>,
        equity_scale: f64,
        now: i64,
    ) -> Result<f64, String> {
        // 1. global interlocks ---------------------------------------------
        if let Some(reason) = kill {
            return Err(format!("kill switch active: {reason}"));
        }
        if !armed {
            return Err("auto-taker disarmed".into());
        }
        if self.day_pnl(realized_total, unrealized) <= -settings.daily_loss_limit_usd {
            return Err(format!(
                "daily loss limit reached ({:.0} USD)",
                settings.daily_loss_limit_usd
            ));
        }

        // 2. capacity -------------------------------------------------------
        if self.attempts_active >= settings.max_concurrent_attempts {
            return Err("max concurrent attempts".into());
        }
        if self.positions_open >= settings.max_open_positions {
            return Err("max open positions".into());
        }

        // 3. signal quality --------------------------------------------------
        let tier_on = match opp.tier {
            ivarb_protocol::Tier::Sniper => settings.sniper_enabled,
            ivarb_protocol::Tier::Convergence => settings.convergence_enabled,
            ivarb_protocol::Tier::Monitor => false,
        };
        if !tier_on {
            return Err(format!("tier {} disabled", opp.tier.label()));
        }
        if opp.net_vp < opp.gate_vp {
            return Err(format!(
                "edge {:+.2} vp below effective gate {:+.2} vp",
                opp.net_vp, opp.gate_vp
            ));
        }
        if opp.stale {
            return Err("signal stale".into());
        }
        if opp.expiry_ms - now < 3 * 3600 * 1000 {
            return Err("expiry too close (<3h) to manage both legs".into());
        }
        let inst = canonical_inst(opp);
        if self.busy_instruments.contains_key(&inst) {
            return Err("instrument already owned/attempted".into());
        }
        if let Some(&until) = self.cooldown_until.get(&opp.id) {
            if now < until {
                return Err("cooldown".into());
            }
        }

        // 4. venue health -----------------------------------------------------
        let venues: FxHashMap<&str, &VenueStatus> = snap
            .venues
            .iter()
            .map(|v| (v.venue.as_str(), v))
            .collect();
        for leg in [&opp.buy_venue, &opp.sell_venue] {
            match venues.get(leg.as_str()) {
                Some(v) if v.state == ivarb_protocol::ConnState::Live => {}
                Some(_) => return Err(format!("venue {leg} not live")),
                None => return Err(format!("venue {leg} unknown")),
            }
        }

        // 5. size: caps × participation × Kelly -------------------------------
        let vega_per_unit = if opp.contract_size > 0.0 {
            opp.vega_vp_usd / opp.contract_size
        } else {
            opp.vega_vp_usd
        };
        if !(vega_per_unit.is_finite() && vega_per_unit > 5.0) {
            return Err(format!(
                "vega ${:.1}/vp too small — wing noise, not tradable edge",
                vega_per_unit
            ));
        }
        let mut units = settings.max_units_per_trade;
        if let Some(max_units) = opp.max_units {
            units = units.min(max_units * settings.max_participation);
        }
        // vega budget
        let vega_headroom = (settings.max_total_vega_usd - self.vega_used_usd).max(0.0);
        units = units.min(vega_headroom / vega_per_unit);
        // fractional Kelly: f = ev / |net| clamped, scaled by kelly_fraction
        if opp.net_vp.abs() > 1e-6 && opp.ev_vp > 0.0 {
            let kelly = (opp.ev_vp / opp.net_vp.abs()).clamp(0.05, 1.0) * settings.kelly_fraction;
            units = units.min(kelly * settings.max_units_per_trade);
        }
        // equity compounding: grow with the account, de-risk when it shrinks
        // (clamped upstream to [0.25, 3.0]; the capital gate re- approves the
        // final size against live per-venue balances below).
        if equity_scale.is_finite() && equity_scale > 0.0 {
            units *= equity_scale;
            units = units.min(settings.max_units_per_trade * 3.0);
        }
        // round down to 1/100th of a unit (venue lot granularity floor)
        units = (units * 100.0).floor() / 100.0;
        if units < 0.01 {
            return Err("sizing rounds to zero".into());
        }

        // 6. capital: segregated collateral per venue ------------------------
        // BUY venue pays the full premium + fee; SELL venue posts short-option
        // margin (venue formula, buffered) + fee. Size down to what the
        // balances can actually fund — a deposit-gated squeeze, not a hope.
        let (buy_need_per_unit, sell_need_per_unit) = capital_needs(opp, &venues)?;
        let buy_px = opp.buy_px_usd.filter(|x| *x > 0.0).unwrap_or(0.0);
        let sell_px = opp.sell_px_usd.filter(|x| *x > 0.0).unwrap_or(0.0);
        if buy_px <= 0.0 || sell_px <= 0.0 {
            return Err("executable premiums unavailable".into());
        }
        let bal_buy = balances.get(&opp.buy_venue).copied().unwrap_or(0.0);
        let bal_sell = balances.get(&opp.sell_venue).copied().unwrap_or(0.0);
        let afford_buy = bal_buy / buy_need_per_unit;
        let afford_sell = bal_sell / sell_need_per_unit;
        let units_afford = afford_buy.min(afford_sell);
        if units_afford < 0.01 {
            let (need_venue, have, per_unit) = if afford_buy <= afford_sell {
                (opp.buy_venue.as_str(), bal_buy, buy_need_per_unit)
            } else {
                (opp.sell_venue.as_str(), bal_sell, sell_need_per_unit)
            };
            return Err(format!(
                "insufficient capital on {need_venue}: have ${have:.0}, one unit needs ${per_unit:.0} — deposit funds in the Funding & capital panel"
            ));
        }
        units = units.min(units_afford);
        units = (units * 100.0).floor() / 100.0;
        if units < 0.01 {
            return Err("capital-capped sizing rounds to zero".into());
        }
        Ok(units)
    }

    /// Register an attempt (after preflight approval).
    pub fn reserve(&mut self, opp: &Opportunity, units: f64) {
        let inst = canonical_inst(opp);
        self.busy_instruments.insert(inst, opp.expiry_ms as u64);
        self.attempts_active += 1;
        // optimistic vega reservation (released on abort, kept on fill)
        let vega_per_unit = if opp.contract_size > 0.0 {
            opp.vega_vp_usd / opp.contract_size
        } else {
            opp.vega_vp_usd
        };
        self.vega_used_usd += units * vega_per_unit;
    }

    /// Release an aborted attempt.
    pub fn release(&mut self, opp: &Opportunity, units: f64, cooldown_ms: i64, now: i64) {
        let inst = canonical_inst(opp);
        self.busy_instruments.remove(&inst);
        self.attempts_active = self.attempts_active.saturating_sub(1);
        let vega_per_unit = if opp.contract_size > 0.0 {
            opp.vega_vp_usd / opp.contract_size
        } else {
            opp.vega_vp_usd
        };
        self.vega_used_usd = (self.vega_used_usd - units * vega_per_unit).max(0.0);
        self.cooldown_until.insert(opp.id.clone(), now + cooldown_ms);
    }

    /// A position opened: attempt slot freed, instrument stays busy.
    pub fn promote(&mut self) {
        self.attempts_active = self.attempts_active.saturating_sub(1);
        self.positions_open += 1;
    }

    /// A position closed.
    pub fn position_closed(&mut self, inst: &str, units: f64, vega_per_unit: f64) {
        self.busy_instruments.remove(inst);
        self.positions_open = self.positions_open.saturating_sub(1);
        self.vega_used_usd = (self.vega_used_usd - units * vega_per_unit).max(0.0);
    }

    /// UTC day rollover: reset the daily loss anchor.
    pub fn rollover_day(&mut self, realized_total: f64) -> bool {
        let today = utc_ymd(now_ms());
        if today != self.day_ymd {
            self.day_ymd = today;
            self.day_anchor_realized = realized_total;
            true
        } else {
            false
        }
    }
}

/// Canonical instrument id from an opportunity id prefix
/// (`UND-yyyymmdd-strike-R:buy>sell` → `UND-yyyymmdd-strike-R`).
pub fn canonical_inst(opp: &Opportunity) -> String {
    opp.id.split(':').next().unwrap_or(&opp.id).to_string()
}

/// Per-underlying-unit capital needs on each leg venue: the buy leg pays the
/// per-unit premium + entry fee, the sell leg posts per-unit buffered
/// short-option margin + entry fee. Shared by the risk gate (layer 6) and the
/// scanner's capital-efficiency ranking, so the two can never disagree about
/// what a trade costs to put on.
pub fn capital_needs(
    opp: &Opportunity,
    venues: &FxHashMap<&str, &VenueStatus>,
) -> Result<(f64, f64), String> {
    let buy_ct = if opp.contract_size > 0.0 { opp.contract_size } else { 1.0 };
    let sell_ct = venues
        .get(opp.sell_venue.as_str())
        .map(|v| v.contract_size)
        .filter(|c| *c > 0.0)
        .unwrap_or(1.0);
    let buy_px = opp.buy_px_usd.filter(|x| *x > 0.0).unwrap_or(0.0);
    let sell_px = opp.sell_px_usd.filter(|x| *x > 0.0).unwrap_or(0.0);
    if buy_px <= 0.0 || sell_px <= 0.0 {
        return Err("executable premiums unavailable".into());
    }
    let buy_fee_rate = venues
        .get(opp.buy_venue.as_str())
        .map(|v| v.taker_fee_rate)
        .filter(|r| *r > 0.0)
        .unwrap_or(0.0003);
    let sell_fee_rate = venues
        .get(opp.sell_venue.as_str())
        .map(|v| v.taker_fee_rate)
        .filter(|r| *r > 0.0)
        .unwrap_or(0.0003);
    let buy_need = buy_px / buy_ct + buy_fee_rate * opp.spot;
    let margin_per_ct = margin_per_contract(
        &opp.sell_venue,
        sell_px / sell_ct,
        sell_ct,
        opp.spot,
        opp.strike,
        right_proto(&opp.right),
    );
    let sell_need =
        margin_per_ct / sell_ct * (1.0 + ivarb_quant::MARGIN_BUFFER) + sell_fee_rate * opp.spot;
    Ok((buy_need, sell_need))
}

/// Protocol right → quant right (margin formulas live in the quant crate).
fn right_proto(r: &ivarb_protocol::Right) -> ivarb_quant::QuantRight {
    match r {
        ivarb_protocol::Right::Call => ivarb_quant::QuantRight::Call,
        ivarb_protocol::Right::Put => ivarb_quant::QuantRight::Put,
    }
}

/// Millis → UTC yyyymmdd (Howard Hinnant's civil_from_days).
pub fn utc_ymd(ms: i64) -> u32 {
    let days = (ms / 86_400_000).max(0);
    let z = days + 719_468;
    let era = z / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y };
    (y as u32) * 10_000 + m * 100 + d
}

#[cfg(test)]
mod tests {
    use super::*;
    use ivarb_protocol::*;

    fn opp() -> Opportunity {
        Opportunity {
            id: "ETH-20261016-2600-C:okx>binance".into(),
            underlying: "ETH".into(),
            expiry: "2026-10-16".into(),
            expiry_ms: 1_793_891_200_000,
            strike: 2600.0,
            right: Right::Call,
            buy_venue: "okx".into(),
            sell_venue: "binance".into(),
            iv_buy_vp: 55.0,
            iv_sell_vp: 58.0,
            iv_spread_vp: 3.0,
            fee_vp: 0.5,
            net_vp: 2.5,
            edge_mode: EdgeMode::Exec,
            vega_vp_usd: 120.0,
            est_usd: 300.0,
            contract_size: 1.0,
            max_units: Some(5.0),
            total_usd: Some(1500.0),
            buy_px_usd: Some(164.0),
            sell_px_usd: Some(180.0),
            spot: 2600.0,
            t_years: 0.15,
            size_buy: Some(4.0),
            size_sell: Some(4.0),
            first_seen_ms: 0,
            updated_ms: 0,
            lead_buy_ms: 0,
            lead_sell_ms: 0,
            drift_buy_vp_1m: 0.0,
            drift_sell_vp_1m: 0.0,
            stale: false,
            retired: false,
            hist_pt: None,
            history: None,
            tier: Tier::Sniper,
            bucket: "8-30d".into(),
            dte: 20.0,
            gate_vp: 0.5,
            ev_vp: 1.5,
            theta_vp_day: -0.8,
            patience_s: 30.0,
            hold_s: 3600.0,
            conv_hint: false,
            conv_eta_s: None,
            kalman_level_vp: None,
            kalman_sigma_vp: None,
            hold: None,
        }
    }

    fn snap() -> Snapshot {
        let vs = |id: &str| VenueStatus {
            venue: id.into(),
            state: ConnState::Live,
            is_sim: false,
            instruments: 100,
            msg_rate: 1.0,
            last_msg_ms: 0,
            taker_fee_rate: 0.0003,
            coin_margined: false,
            contract_size: 1.0,
            lag_ms_ewma: 0.0,
            note: None,
        };
        Snapshot {
            ts_ms: 0,
            venues: vec![vs("okx"), vs("binance")],
            spots: vec![],
            opportunities: vec![],
            holds: vec![],
            multihop: vec![],
            stats: EngineStats {
                ts_ms: 0,
                uptime_s: 0,
                feed_mode: FeedMode::Live,
                venues_live: 2,
                venues_total: 2,
                instruments_tracked: 0,
                opps_active: 0,
                engine_loop_us_ewma: 0.0,
                msgs_total: 0,
                msg_rate: 0.0,
                pipe_p50_us: 0.0,
                pipe_p99_us: 0.0,
                pipe_p999_us: 0.0,
                wire_p50_us: 0.0,
                wire_p99_us: 0.0,
                broadcast_ms: 0,
                sweep_ms: 0,
                multihop_routes: 0,
                holds_tracked: 0,
                ga_generation: 0,
                min_net_vp: 0.5,
                exit_net_vp: 0.25,
                moneyness_pct: 0.12,
                min_vega_usd: 5.0,
            },
        }
    }

    fn balances(buy: f64, sell: f64) -> FxHashMap<String, f64> {
        let mut m = FxHashMap::default();
        m.insert("okx".to_string(), buy);
        m.insert("binance".to_string(), sell);
        m
    }

    /// Capital gate: a funded account approves and sizes to the balance cap;
    /// a drained venue refuses with a deposit-directed reason.
    #[test]
    fn capital_gate_sizes_and_rejects() {
        let settings = ExecSettings::default();
        let o = opp();
        let s = snap();
        let mut risk = RiskState::new();

        // Well funded: approved.
        let r = risk.preflight(&o, &s, &settings, true, &None, 0.0, 0.0, &balances(1_000_000.0, 1_000_000.0), 1.0, 0);
        assert!(r.is_ok(), "funded take must pass: {:?}", r.err());

        // Sell venue drained below its margin need: refused, reason mentions capital.
        let r = risk.preflight(&o, &s, &settings, true, &None, 0.0, 0.0, &balances(1_000_000.0, 1.0), 1.0, 0);
        let err = r.err().expect("drained sell venue must refuse");
        assert!(err.contains("insufficient capital"), "got: {err}");
        assert!(err.contains("binance"), "reason must name the venue: {err}");

        // Buy venue drained: same, other side.
        let r = risk.preflight(&o, &s, &settings, true, &None, 0.0, 0.0, &balances(1.0, 1_000_000.0), 1.0, 0);
        let err = r.err().expect("drained buy venue must refuse");
        assert!(err.contains("okx"), "reason must name the venue: {err}");

        // Partial funding sizes DOWN to what the balance affords (per-unit
        // need ~ premium+fee on buy, margin+fee on sell).
        let r = risk.preflight(&o, &s, &settings, true, &None, 0.0, 0.0, &balances(1_000.0, 1_000_000.0), 1.0, 0);
        let units = r.expect("partial funding must size, not refuse");
        // Kelly caps first: (1.5/2.5 clamped) × 0.25 × 2.0 = 0.30, far below
        // what $1,000 affords (≈ 6.06 units) — sizing survives untouched.
        assert!((units - 0.30).abs() < 0.01, "units: {units}");
        // And a balance that cannot fund even one hundredth of a unit refuses.
        let r = risk.preflight(&o, &s, &settings, true, &None, 0.0, 0.0, &balances(500.0, 1.0), 1.0, 0);
        assert!(r.as_ref().err().map(|e| e.contains("insufficient capital")).unwrap_or(false), "{:?}", r.err());
    }

    /// Equity compounding: scale > 1 grows the approved clip, scale < 1
    /// shrinks it — and the capital gate still clamps to real balances.
    #[test]
    fn equity_scale_compounds_sizes() {
        let settings = ExecSettings::default();
        let o = opp();
        let s = snap();
        let mut risk = RiskState::new();
        let funded = balances(1_000_000.0, 1_000_000.0);

        let base = risk
            .preflight(&o, &s, &settings, true, &None, 0.0, 0.0, &funded, 1.0, 0)
            .expect("base sizing");
        let grown = risk
            .preflight(&o, &s, &settings, true, &None, 0.0, 0.0, &funded, 2.0, 0)
            .expect("compounded sizing");
        let shrunk = risk
            .preflight(&o, &s, &settings, true, &None, 0.0, 0.0, &funded, 0.5, 0)
            .expect("de-risked sizing");
        // Kelly cap 0.30 at scale 1 → 0.60 at scale 2 → 0.15 at scale 0.5.
        assert!((grown - (base * 2.0)).abs() < 0.011, "base {base} grown {grown}");
        assert!((shrunk - (base * 0.5)).abs() < 0.011, "base {base} shrunk {shrunk}");
        assert!(grown <= settings.max_units_per_trade * 3.0);
    }
}
