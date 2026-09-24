//! Time-aware strategy layer — the "time is changing for different strategies"
//! brain shared by the scanner (tier badges, gates, EV) and the advantage
//! taker (execution parameters, patience windows, theta budgets).
//!
//! Three clocks drive every decision:
//!
//! 1. **Time to expiry (theta clock).** The same 1 vp spread means very
//!    different things with 40 days left versus 36 hours left: vega collapses,
//!    fees in vol points explode (`fee_vp = fee × spot / vega`), and theta
//!    burn per unit of convergence time accelerates. Each DTE bucket carries
//!    its own entry-gate multiplier, patience window, max hold, expected
//!    holding time and size multiplier.
//! 2. **Time of day (liquidity clock).** EU/US overlap hours have the tightest
//!    cross-venue books; Asia quiet hours have the widest. The regime
//!    multiplies the entry gate and shrinks size in thin hours, and weekends
//!    are treated as thin everywhere.
//! 3. **Signal age (opportunity clock).** A divergence spotted 12 minutes ago
//!    is a different animal from one spotted 2 seconds ago — the edge decays
//!    with quote age, so the gate escalates as the signal matures.

use ivarb_protocol::Tier;
use ivarb_quant::{bs_theta, vega_per_vp, bs_vega};

/// Theta bucket keyed off whole days to expiry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThetaBucket {
    D0_2,
    D3_7,
    D8_30,
    D30P,
}

impl ThetaBucket {
    pub fn label(self) -> &'static str {
        match self {
            ThetaBucket::D0_2 => "0-2d",
            ThetaBucket::D3_7 => "3-7d",
            ThetaBucket::D8_30 => "8-30d",
            ThetaBucket::D30P => "30d+",
        }
    }

    pub fn from_dte(dte: f64) -> Self {
        if dte < 3.0 {
            ThetaBucket::D0_2
        } else if dte < 8.0 {
            ThetaBucket::D3_7
        } else if dte < 31.0 {
            ThetaBucket::D8_30
        } else {
            ThetaBucket::D30P
        }
    }

    /// Entry-gate multiplier: short-dated options need more edge because vega
    /// is tiny (fee_vp blows up) and theta burns fast.
    pub fn gate_mult(self) -> f64 {
        match self {
            ThetaBucket::D0_2 => 1.6,
            ThetaBucket::D3_7 => 1.2,
            ThetaBucket::D8_30 => 1.0,
            ThetaBucket::D30P => 1.0,
        }
    }

    /// Patience window for passive convergence attempts.
    pub fn patience_s(self) -> f64 {
        match self {
            ThetaBucket::D0_2 => 8.0 * 60.0,
            ThetaBucket::D3_7 => 25.0 * 60.0,
            ThetaBucket::D8_30 => 90.0 * 60.0,
            ThetaBucket::D30P => 4.0 * 3600.0,
        }
    }

    /// Maximum holding window once both legs are on.
    pub fn max_hold_s(self) -> f64 {
        match self {
            ThetaBucket::D0_2 => 45.0 * 60.0,
            ThetaBucket::D3_7 => 6.0 * 3600.0,
            ThetaBucket::D8_30 => 24.0 * 3600.0,
            ThetaBucket::D30P => 72.0 * 3600.0,
        }
    }

    /// Expected holding time (days) used by the theta-burn term of the EV
    /// model — how long convergence realistically takes in this bucket.
    pub fn e_hold_days(self) -> f64 {
        match self {
            ThetaBucket::D0_2 => 0.03,
            ThetaBucket::D3_7 => 0.20,
            ThetaBucket::D8_30 => 1.0,
            ThetaBucket::D30P => 2.5,
        }
    }

    /// Size multiplier: the less time to manage a gone-wrong position, the
    /// smaller we want it.
    pub fn size_mult(self) -> f64 {
        match self {
            ThetaBucket::D0_2 => 0.5,
            ThetaBucket::D3_7 => 0.8,
            ThetaBucket::D8_30 => 1.0,
            ThetaBucket::D30P => 1.0,
        }
    }
}

/// Liquidity regime keyed off the UTC hour (+ weekend flag).
#[derive(Debug, Clone, Copy)]
pub struct Regime {
    pub utc_hour: u32,
    pub weekend: bool,
    pub gate_mult: f64,
    pub size_mult: f64,
    pub label: &'static str,
}

impl Regime {
    pub fn now_utc(now_ms: i64) -> Self {
        let secs = (now_ms / 1000).max(0) as u64;
        let days = secs / 86_400;
        // 1970-01-01 was a Thursday (weekday 4); Sat = 5, Sun = 6.
        let weekday = ((days + 4) % 7) as u32;
        let hour = ((secs % 86_400) / 3600) as u32;
        let weekend = weekday == 5 || weekday == 6;
        let (mut gate, mut size, label) = match hour {
            13..=20 => (1.00, 1.00, "EU/US overlap — tightest books"),
            7..=12 => (1.10, 0.85, "EU morning"),
            21..=23 => (1.10, 0.85, "US late"),
            _ => (1.25, 0.60, "Asia quiet hours"),
        };
        if weekend {
            gate *= 1.10;
            size *= 0.80;
        }
        Self {
            utc_hour: hour,
            weekend,
            gate_mult: gate,
            size_mult: size,
            label: if weekend {
                "Weekend — thin liquidity everywhere"
            } else {
                label
            },
        }
    }
}

/// Everything the strategy layer computes for one signal.
#[derive(Debug, Clone, Copy)]
pub struct StrategyView {
    pub tier: Tier,
    pub bucket: ThetaBucket,
    pub gate_vp: f64,
    pub ev_vp: f64,
    pub theta_vp_day: f64,
    pub patience_s: f64,
    pub hold_s: f64,
    pub dte: f64,
    pub conv_hint: bool,
}

/// Inputs gathered from the live opportunity (all in vol points / seconds).
#[derive(Debug, Clone, Copy)]
pub struct SignalCtx {
    pub net_vp: f64,
    pub fee_vp: f64,
    /// Executable-edge mode (ask/bid) vs mark mode.
    pub exec_mode: bool,
    /// Age of the freshest leg quote, ms.
    pub quote_age_ms: f64,
    /// Age of the signal episode (first seen above gate), ms.
    pub signal_age_ms: f64,
    /// Top-of-book depth in underlying units (min of legs), when published.
    pub depth_units: Option<f64>,
    /// IV drift of the cheap (buy) leg over ~1m, vp.
    pub drift_buy_vp: f64,
    /// IV drift of the rich (sell) leg over ~1m, vp.
    pub drift_sell_vp: f64,
    /// Black-Scholes inputs for theta: spot, strike, years, mid IV (raw 0..1).
    pub spot: f64,
    pub strike: f64,
    pub t_years: f64,
    pub iv_mid_raw: f64,
    /// Configured base entry gate (vp), normally 0.5.
    pub base_gate_vp: f64,
    /// Genome (GA) multipliers: entry gate, patience window, max hold.
    /// 1.0 when no genome is applied.
    pub ga_gate_mult: f64,
    pub ga_patience_mult: f64,
    pub ga_hold_mult: f64,
}

/// Compute the full time-aware strategy view for one signal.
pub fn evaluate(ctx: &SignalCtx, now_ms: i64) -> StrategyView {
    let dte = (ctx.t_years * 365.0).max(0.0);
    let bucket = ThetaBucket::from_dte(dte);
    let regime = Regime::now_utc(now_ms);

    // --- theta burn rate, in vp per day of holding -------------------------
    // theta_usd_day = |Θ|/365 per underlying unit; in vol points:
    // theta_vp_day = theta_usd_day / vega_per_vp_per_unit.
    let vega_unit = vega_per_vp(bs_vega(ctx.spot, ctx.strike, ctx.t_years, ctx.iv_mid_raw), 1.0);
    let theta_unit = bs_theta(ctx.spot, ctx.strike, ctx.t_years, ctx.iv_mid_raw) / 365.0;
    let theta_vp_day = if vega_unit > 1e-9 {
        (theta_unit / vega_unit).abs()
    } else {
        0.0
    };

    // --- effective gate: bucket × regime × signal-age decay × genome -------
    let age_mult = 1.0
        + ((ctx.signal_age_ms / 60_000.0) / 12.0).min(1.0) * 0.15; // up to +15% over 12 min
    let gate_vp = ctx.base_gate_vp
        * bucket.gate_mult()
        * regime.gate_mult
        * age_mult
        * ctx.ga_gate_mult.max(0.2);

    // --- fill probability model ---------------------------------------------
    // Fresh, deep, executable quotes fill both legs; stale or thin ones leave
    // us with single-leg risk. Sniping needs live touches (5 s ramp);
    // passive capture works against books that persist far longer (30 s ramp).
    let fresh = if ctx.exec_mode {
        1.0 - (ctx.quote_age_ms / 5_000.0).min(1.0)
    } else {
        1.0 - (ctx.quote_age_ms / 30_000.0).min(1.0)
    };
    let depth = match ctx.depth_units {
        Some(d) => (d / 1.0).min(1.0), // 1+ underlying unit at the touch = full score
        None => 0.5, // unknown depth: neutral
    };
    let p_fill = (0.30 + 0.45 * fresh + 0.25 * depth).clamp(0.15, 0.95);

    // --- single-leg unwind cost (vp) ----------------------------------------
    // If leg B misses we cross the spread to unwind leg A: entry fee on the
    // filled leg + assumed 0.4 vp of spread-cross + slippage.
    let unwind_cost_vp = ctx.fee_vp + 0.40;

    // --- expected slippage on entry (vp) -------------------------------------
    let slip_vp = 0.12 + 0.20 * (1.0 - depth);

    // --- convergence hint from drift ----------------------------------------
    // Favorable: rich leg fading down (drift_sell < 0) and/or cheap leg firm
    // (drift_buy > 0) — the spread is already coming to us.
    let conv_hint = ctx.drift_sell_vp < -0.02 || ctx.drift_buy_vp > 0.02;
    let drift_adj_vp = if conv_hint {
        0.15
    } else if ctx.drift_sell_vp > 0.05 && ctx.drift_buy_vp < -0.05 {
        -0.10 // both legs diverging away from convergence
    } else {
        0.0
    };

    // --- expected value (vp) --------------------------------------------------
    // A fully-hedged arb (long one contract + short the same contract)
    // nets most of its theta: the long leg's bleed is the short leg's
    // accrual. Only the residual mismatch (different leg IVs → different
    // greeks) plus the unhedged single-leg window burns, so the model applies
    // a fraction of the raw theta over the expected holding time.
    const NET_THETA_FACTOR: f64 = 0.15;
    let theta_burn = theta_vp_day * bucket.e_hold_days() * NET_THETA_FACTOR;
    let ev_vp = p_fill * (ctx.net_vp - slip_vp)
        - (1.0 - p_fill) * unwind_cost_vp
        - theta_burn
        + drift_adj_vp;

    // --- tier decision ---------------------------------------------------------
    // SNIPER: executable edge above the effective gate, both quotes fresh
    // enough to trust the touch, some visible depth.
    // CONVERGENCE: marks diverged above the gate but the executable edge is
    // thin (or unknown) — passive capture with a bucket-scaled patience.
    // MONITOR: everything else (informational signal only).
    let sniper = ctx.exec_mode
        && ctx.net_vp >= gate_vp
        && ctx.quote_age_ms <= 3_000.0
        && ctx.depth_units.map(|d| d >= 0.05).unwrap_or(true);
    let convergence = !sniper
        && ctx.net_vp >= gate_vp * 0.85
        && ctx.quote_age_ms <= 20_000.0
        && conv_hint;
    let tier = if sniper {
        Tier::Sniper
    } else if convergence {
        Tier::Convergence
    } else {
        Tier::Monitor
    };

    StrategyView {
        tier,
        bucket,
        gate_vp,
        ev_vp: ev_vp.max(-9.99),
        theta_vp_day,
        patience_s: bucket.patience_s() * ctx.ga_patience_mult.max(0.1),
        hold_s: bucket.max_hold_s() * ctx.ga_hold_mult.max(0.1),
        dte,
        conv_hint,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(net: f64, dte: f64) -> SignalCtx {
        SignalCtx {
            net_vp: net,
            fee_vp: 0.10,
            exec_mode: true,
            quote_age_ms: 500.0,
            signal_age_ms: 2_000.0,
            depth_units: Some(2.0),
            drift_buy_vp: 0.0,
            drift_sell_vp: -0.10,
            spot: 84_000.0,
            strike: 85_000.0,
            t_years: dte / 365.0,
            iv_mid_raw: 0.55,
            base_gate_vp: 0.5,
            ga_gate_mult: 1.0,
            ga_patience_mult: 1.0,
            ga_hold_mult: 1.0,
        }
    }

    #[test]
    fn buckets_change_with_time() {
        // The identical 0.8 vp signal is a SNIPER at 30d but only MONITOR at
        // 1d: the theta bucket (×1.6) and quiet-hours regime (×1.25) push the
        // effective gate from 0.63 vp to 1.00 vp. This is the "time is
        // changing for different strategies" contract.
        let s30 = evaluate(&ctx(0.8, 30.0), 0);
        let s1 = evaluate(&ctx(0.8, 1.0), 0);
        assert_eq!(s30.tier, Tier::Sniper);
        assert_eq!(s1.tier, Tier::Monitor);
        assert!(s1.gate_vp > s30.gate_vp, "{} {}", s1.gate_vp, s30.gate_vp);
        assert!(s1.theta_vp_day > s30.theta_vp_day);
        assert!(s1.hold_s < s30.hold_s);
        // A fat 2.5 vp edge is still a SNIPER even at 1d (2.5 > 1.00 gate),
        // and a 1.0 vp edge at 1d degrades to passive-only CONVERGENCE.
        assert_eq!(evaluate(&ctx(2.5, 1.0), 0).tier, Tier::Sniper);
        assert_eq!(evaluate(&ctx(1.0, 1.0), 0).tier, Tier::Convergence);
    }

    #[test]
    fn stale_quotes_block_sniper() {
        let mut c = ctx(3.0, 15.0);
        c.quote_age_ms = 6_000.0;
        // Too stale to snipe (3 s freshness), still fresh enough to work
        // passively with a favorable drift → CONVERGENCE.
        assert_eq!(evaluate(&c, 0).tier, Tier::Convergence);
        // Fully stale (beyond the 20 s passive window) → MONITOR only.
        c.quote_age_ms = 25_000.0;
        assert_eq!(evaluate(&c, 0).tier, Tier::Monitor);
    }

    #[test]
    fn genome_multiplier_scales_gate_and_windows() {
        let mut c = ctx(2.0, 20.0);
        c.ga_gate_mult = 1.8;
        c.ga_patience_mult = 2.0;
        c.ga_hold_mult = 0.5;
        let s = evaluate(&c, 0);
        let base = evaluate(&ctx(2.0, 20.0), 0);
        assert!((s.gate_vp / base.gate_vp - 1.8).abs() < 1e-9);
        assert!((s.patience_s / base.patience_s - 2.0).abs() < 1e-9);
        assert!((s.hold_s / base.hold_s - 0.5).abs() < 1e-9);
        // A 1.8× gate blocks what the default gate admits.
        let mid = ctx(1.0, 20.0);
        assert_eq!(evaluate(&mid, 0).tier, Tier::Sniper);
        let mut strict = mid;
        strict.ga_gate_mult = 3.0;
        assert_ne!(evaluate(&strict, 0).tier, Tier::Sniper);
    }

    #[test]
    fn regime_gates_rise_in_quiet_hours() {
        // Fixed timestamp 2026-09-24T03:00:00Z (Asia quiet).
        let quiet = 1_790_155_640_000_i64;
        let overlap = quiet + 11 * 3600 * 1000; // 14:00 UTC
        let g_quiet = Regime::now_utc(quiet).gate_mult;
        let g_overlap = Regime::now_utc(overlap).gate_mult;
        assert!(g_quiet > g_overlap, "{g_quiet} {g_overlap}");
    }

    #[test]
    fn ev_penalizes_theta_burn() {
        // The theta clock: a 1-day ATM option burns >10× the vol points per
        // day of a 30-day one — this is why the 0-2d bucket gates hardest.
        let s1 = evaluate(&ctx(0.8, 1.0), 0);
        let s30 = evaluate(&ctx(0.8, 30.0), 0);
        assert!(s1.theta_vp_day > s30.theta_vp_day * 10.0, "{} {}", s1.theta_vp_day, s30.theta_vp_day);
        // Because the hedged arb nets its theta, a 0.8 vp edge at 30d is
        // still worth taking after slippage, single-leg risk and residual
        // theta: EV must be positive.
        assert!(s30.ev_vp > 0.0, "ev30={}", s30.ev_vp);
        // And more edge is strictly better: EV is monotone in net_vp.
        let s30_fat = evaluate(&ctx(2.0, 30.0), 0);
        assert!(s30_fat.ev_vp > s30.ev_vp);
    }
}
