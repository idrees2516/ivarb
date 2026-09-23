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
    pub fn preflight(
        &mut self,
        opp: &Opportunity,
        snap: &Snapshot,
        settings: &ExecSettings,
        armed: bool,
        kill: &Option<String>,
        realized_total: f64,
        unrealized: f64,
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
        // round down to 1/100th of a unit (venue lot granularity floor)
        units = (units * 100.0).floor() / 100.0;
        if units < 0.01 {
            return Err("sizing rounds to zero".into());
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
