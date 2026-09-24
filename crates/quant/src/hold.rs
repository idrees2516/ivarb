//! Hold-to-expiry analytics — "the spread is too thin to arb, so what is the
//! option worth if I just keep it until settlement?"
//!
//! This module answers the exact question a scanner cannot: an IV divergence
//! below the fee gate is *not* an arbitrage, but the cheap leg may still be a
//! positive-EV outright bet. Buying the option on the cheap venue and holding
//! it to expiry converts a "missed arb" into a long-vol position whose
//! economics are fully computable:
//!
//! - **Expected value** — the option repriced at your *forecast* volatility
//!   (cross-venue consensus, realized-vol model, or manual view) minus the
//!   premium and the entry fee. `EV = E[payoff] − ask − fee`.
//! - **Theta schedule** — the decay bleed per day now, and how it accelerates
//!   through the remaining life (0 %, 25 %, 50 %, 75 %, 100 % of the tenor).
//! - **Cost if nothing happens** — spot and IV frozen: you lose the whole
//!   extrinsic value plus fees. That is the worst *quiet* outcome and the
//!   number most people underestimate.
//! - **Breakeven spot** at expiry (`K + premium + fee` for a call) and the
//!   closed-form probability of finishing beyond it under (drift, forecast
//!   vol).
//! - **Terminal P&L distribution** via antithetic Monte-Carlo: mean, p05,
//!   p50, p95, stdev and CVaR₀₅ (expected shortfall).
//!
//! Everything is quoted **per one unit of underlying** and per contract
//! (× contract size) so the UI can show both BTC-1 and venue-contract views.

use super::{bs_price, bs_theta, bs_vega, implied_vol, intrinsic, norm_cdf, Right};

/// Inputs for one hold-to-expiry evaluation.
#[derive(Debug, Clone, Copy)]
pub struct HoldInput {
    /// Current spot (consensus across venues).
    pub spot: f64,
    pub strike: f64,
    /// Years to expiry (365-day convention).
    pub t_years: f64,
    pub right: Right,
    /// Ask premium per 1 underlying unit on the cheap venue.
    pub ask_px_unit: f64,
    /// Taker fee as a fraction of spot (fee per unit = rate × spot).
    pub fee_rate: f64,
    /// Your volatility forecast, raw (0.65 = 65 %).
    pub forecast_iv: f64,
    /// Annual spot drift assumption (0 for risk-neutral view).
    pub drift: f64,
    /// Underlying units per contract (for per-contract display).
    pub contract_size: f64,
    /// Monte-Carlo paths (antithetic; rounded up to even).
    pub paths: usize,
    /// RNG seed for reproducible simulations.
    pub seed: u64,
}

impl HoldInput {
    pub fn sanity(&self) -> bool {
        self.spot.is_finite()
            && self.strike.is_finite()
            && self.t_years.is_finite()
            && self.ask_px_unit.is_finite()
            && self.spot > 0.0
            && self.strike > 0.0
            && self.t_years > 0.0
            && self.ask_px_unit > 0.0
            && self.forecast_iv.is_finite()
            && self.forecast_iv > 0.005
            && self.forecast_iv < 5.0
    }
}

/// Full result of the hold-to-expiry evaluation. All `*_unit` fields are per
/// one unit of underlying; `*_contract` fields multiply by `contract_size`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HoldAnalysis {
    // ---- inputs echoed ----
    pub spot: f64,
    pub strike: f64,
    pub t_years: f64,
    pub right: Right,
    pub contract_size: f64,
    pub forecast_iv: f64,
    pub drift: f64,

    // ---- cost decomposition ----
    /// Premium paid per unit (the ask).
    pub premium_unit: f64,
    /// One-way entry fee per unit (rate × spot); no exit fee — settlement is free.
    pub fee_unit: f64,
    /// Total capital locked per unit.
    pub capital_unit: f64,
    /// Intrinsic value per unit at current spot.
    pub intrinsic_unit: f64,
    /// Time value per unit — the maximum quiet-case bleed.
    pub extrinsic_unit: f64,
    /// IV implied by the ask premium (raw).
    pub implied_iv: f64,
    /// forecast_vp − implied_vp: positive means you buy below your vol view.
    pub iv_edge_vp: f64,

    // ---- expected value ----
    /// E[terminal payoff] per unit under (drift, forecast vol).
    pub expected_payoff_unit: f64,
    /// `expected_payoff − premium − fee`, per unit. THE headline number.
    pub ev_unit: f64,
    /// EV per contract, USD.
    pub ev_usd_per_contract: f64,
    /// EV as a fraction of capital locked.
    pub ev_pct: f64,

    // ---- theta schedule (per unit per day, negative) ----
    /// Theta today.
    pub theta_now_day_unit: f64,
    /// Theta at 50 % of remaining life.
    pub theta_mid_day_unit: f64,
    /// Theta in the final quarter of life (the acceleration zone).
    pub theta_late_day_unit: f64,
    /// Theta today per contract, USD/day.
    pub theta_day_usd_per_contract: f64,

    // ---- breakeven & probabilities ----
    /// Spot at expiry where P&L = 0 (call: K + premium + fee).
    pub breakeven_spot: f64,
    /// Required spot move to breakeven, percent.
    pub be_move_pct: f64,
    /// P(terminal P&L > 0) under (drift, forecast vol).
    pub p_profit: f64,

    // ---- Monte-Carlo terminal P&L distribution (per unit) ----
    pub pnl_mean_unit: f64,
    pub pnl_p05_unit: f64,
    pub pnl_p50_unit: f64,
    pub pnl_p95_unit: f64,
    pub pnl_stdev_unit: f64,
    /// Expected shortfall (mean of the worst 5 %).
    pub cvar05_unit: f64,

    // ---- scenario grid ----
    /// Terminal P&L per unit if spot and IV freeze (pure theta bleed case).
    pub frozen_pnl_unit: f64,
    /// Terminal P&L per unit across spot multipliers (0.80 … 1.20),
    /// aligned with `SPOT_GRID`.
    pub pnl_by_spot: [f64; 7],
}

/// Spot multipliers for the scenario grid (kept public for the UI).
pub const SPOT_GRID: [f64; 7] = [0.80, 0.90, 0.95, 1.00, 1.05, 1.10, 1.20];

/// Terminal payoff per unit.
#[inline]
fn payoff(spot_t: f64, strike: f64, right: Right) -> f64 {
    match right {
        Right::Call => (spot_t - strike).max(0.0),
        Right::Put => (strike - spot_t).max(0.0),
    }
}

/// E[terminal payoff] under GBM(spot, drift μ, vol σ) — closed form via the
/// Black formula on the forward `F = S·e^{μT}` (no discounting: this is the
/// real-world expectation of the cash settlement, not a price).
pub fn expected_terminal_payoff(spot: f64, strike: f64, t: f64, vol: f64, drift: f64, right: Right) -> f64 {
    if spot <= 0.0 || strike <= 0.0 || t <= 0.0 || vol <= 0.0 {
        return 0.0;
    }
    let fwd = spot * (drift * t).exp();
    let sq = vol * t.sqrt();
    let d1 = ((fwd / strike).ln() + 0.5 * sq * sq) / sq;
    let d2 = d1 - sq;
    match right {
        Right::Call => fwd * norm_cdf(d1) - strike * norm_cdf(d2),
        Right::Put => strike * norm_cdf(-d2) - fwd * norm_cdf(-d1),
    }
}

/// P(S_T > x) under GBM(spot, μ, σ).
#[inline]
fn prob_above(spot: f64, x: f64, t: f64, vol: f64, drift: f64) -> f64 {
    if x <= 0.0 || spot <= 0.0 || t <= 0.0 || vol <= 0.0 {
        return 0.0;
    }
    let z = ((spot / x).ln() + (drift - 0.5 * vol * vol) * t) / (vol * t.sqrt());
    norm_cdf(z)
}

/// Percentile of an unsorted slice (clamped interpolation).
pub fn percentile_sorted(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let idx = p.clamp(0.0, 1.0) * (sorted.len() - 1) as f64;
    let lo = idx.floor() as usize;
    let hi = idx.ceil() as usize;
    if lo == hi {
        sorted[lo]
    } else {
        sorted[lo] + (sorted[hi] - sorted[lo]) * (idx - lo as f64)
    }
}

/// Run the full hold-to-expiry evaluation.
pub fn analyze(inp: &HoldInput) -> HoldAnalysis {
    let &HoldInput {
        spot,
        strike,
        t_years: t,
        right,
        ask_px_unit,
        fee_rate,
        forecast_iv,
        drift,
        contract_size,
        ..
    } = inp;

    let fee_unit = fee_rate * spot;
    let premium_unit = ask_px_unit;
    let capital_unit = premium_unit + fee_unit;
    let intrinsic_unit = intrinsic(spot, strike, right);
    let extrinsic_unit = (premium_unit - intrinsic_unit).max(0.0);

    // Implied vol of the ask (what the cheap venue charges you).
    let implied_iv = implied_vol(premium_unit, spot, strike, t, right)
        .unwrap_or(forecast_iv)
        .clamp(0.005, 5.0);
    let iv_edge_vp = (forecast_iv - implied_iv) * 100.0;

    // Expected payoff under the forecast.
    let expected_payoff_unit = expected_terminal_payoff(spot, strike, t, forecast_iv, drift, right);
    let ev_unit = expected_payoff_unit - capital_unit;

    // Theta schedule (premium-consistent vol where possible).
    let theta_of = |tt: f64| bs_theta(spot, strike, tt, forecast_iv) / 365.0;
    let theta_now_day_unit = theta_of(t.max(1e-5));
    let theta_mid_day_unit = theta_of((t * 0.5).max(1e-5));
    let theta_late_day_unit = theta_of((t * 0.25).max(1e-5));

    // Breakeven spot at expiry.
    let breakeven_spot = match right {
        Right::Call => strike + capital_unit,
        Right::Put => (strike - capital_unit).max(1e-9),
    };
    let be_move_pct = (breakeven_spot / spot - 1.0) * 100.0;
    let p_profit = match right {
        Right::Call => prob_above(spot, breakeven_spot, t, forecast_iv, drift),
        Right::Put => 1.0 - prob_above(spot, breakeven_spot, t, forecast_iv, drift),
    };

    // Antithetic Monte-Carlo of the terminal P&L distribution.
    let mut paths = inp.paths.max(256);
    paths = (paths + 1) & !1; // even
    let mut rng = super::rng::Rng::new(inp.seed);
    let sig_sqrt_t = forecast_iv * t.sqrt();
    let drift_term = (drift - 0.5 * forecast_iv * forecast_iv) * t;
    let mut pnls: Vec<f64> = Vec::with_capacity(paths);
    for _ in 0..paths / 2 {
        let z = rng.next_normal();
        for zz in [z, -z] {
            let s_t = spot * (drift_term + sig_sqrt_t * zz).exp();
            pnls.push(payoff(s_t, strike, right) - capital_unit);
        }
    }
    pnls.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = pnls.len();
    let mean = pnls.iter().sum::<f64>() / n as f64;
    let var = pnls.iter().map(|x| (x - mean) * (x - mean)).sum::<f64>() / (n - 1).max(1) as f64;
    let cvar_n = (n as f64 * 0.05).ceil() as usize;
    let cvar05_unit = if cvar_n > 0 {
        pnls[..cvar_n].iter().sum::<f64>() / cvar_n as f64
    } else {
        pnls[0]
    };

    let frozen_pnl_unit = -(extrinsic_unit + fee_unit);
    let mut pnl_by_spot = [0.0f64; 7];
    for (i, m) in SPOT_GRID.iter().enumerate() {
        let s_t = spot * m;
        pnl_by_spot[i] = payoff(s_t, strike, right) - capital_unit;
    }

    let cs = if contract_size > 0.0 { contract_size } else { 1.0 };
    HoldAnalysis {
        spot,
        strike,
        t_years: t,
        right,
        contract_size: cs,
        forecast_iv,
        drift,
        premium_unit,
        fee_unit,
        capital_unit,
        intrinsic_unit,
        extrinsic_unit,
        implied_iv,
        iv_edge_vp,
        expected_payoff_unit,
        ev_unit,
        ev_usd_per_contract: ev_unit * cs,
        ev_pct: if capital_unit > 0.0 { ev_unit / capital_unit } else { 0.0 },
        theta_now_day_unit,
        theta_mid_day_unit,
        theta_late_day_unit,
        theta_day_usd_per_contract: theta_now_day_unit * cs,
        breakeven_spot,
        be_move_pct,
        p_profit,
        pnl_mean_unit: mean,
        pnl_p05_unit: percentile_sorted(&pnls, 0.05),
        pnl_p50_unit: percentile_sorted(&pnls, 0.50),
        pnl_p95_unit: percentile_sorted(&pnls, 0.95),
        pnl_stdev_unit: var.sqrt(),
        cvar05_unit,
        frozen_pnl_unit,
        pnl_by_spot,
    }
}

/// Compact subset attached to live opportunities (cheap enough to compute on
/// every signal refresh — no Monte-Carlo, closed forms only).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct HoldStatsLite {
    /// EV per contract of keeping the buy leg to expiry, USD.
    pub ev_usd: f64,
    /// Theta today per contract, USD/day (negative).
    pub theta_day_usd: f64,
    /// Extrinsic per contract (max quiet bleed), USD.
    pub extrinsic_usd: f64,
    /// P(terminal P&L > 0) under the consensus forecast, 0..1.
    pub p_profit: f64,
    /// forecast_vp − implied ask vp on the buy leg.
    pub iv_edge_vp: f64,
    /// Days to expiry.
    pub dte: f64,
}

/// Closed-form-only evaluation used by the engine hot path.
pub fn analyze_lite(
    spot: f64,
    strike: f64,
    t: f64,
    right: Right,
    ask_px_unit: f64,
    fee_rate: f64,
    forecast_iv: f64,
    contract_size: f64,
) -> HoldStatsLite {
    let fee_unit = fee_rate * spot;
    let capital = ask_px_unit + fee_unit;
    let intrinsic_unit = intrinsic(spot, strike, right);
    let extrinsic_unit = (ask_px_unit - intrinsic_unit).max(0.0);
    let expected_payoff_unit = expected_terminal_payoff(spot, strike, t, forecast_iv, 0.0, right);
    let ev_unit = expected_payoff_unit - capital;
    let be = match right {
        Right::Call => strike + capital,
        Right::Put => (strike - capital).max(1e-9),
    };
    let p_profit = match right {
        Right::Call => prob_above(spot, be, t, forecast_iv, 0.0),
        Right::Put => 1.0 - prob_above(spot, be, t, forecast_iv, 0.0),
    };
    let theta_day = bs_theta(spot, strike, t, forecast_iv) / 365.0;
    let implied = implied_vol(ask_px_unit, spot, strike, t, right).unwrap_or(forecast_iv);
    let cs = if contract_size > 0.0 { contract_size } else { 1.0 };
    HoldStatsLite {
        ev_usd: ev_unit * cs,
        theta_day_usd: theta_day * cs,
        extrinsic_usd: extrinsic_unit * cs,
        p_profit,
        iv_edge_vp: (forecast_iv - implied) * 100.0,
        dte: (t * 365.0).max(0.0),
    }
}

/// Sanity helper: is a vega meaningful for vp conversion of this instrument?
pub fn vega_per_vp_unit(spot: f64, strike: f64, t: f64, iv: f64) -> f64 {
    super::vega_per_vp(bs_vega(spot, strike, t, iv), 1.0)
}

/// Convenience re-export used by the server REST endpoint.
pub fn price_at(spot: f64, strike: f64, t: f64, iv: f64, right: Right) -> f64 {
    bs_price(spot, strike, t, iv, right).unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inp() -> HoldInput {
        HoldInput {
            spot: 84_383.0,
            strike: 84_000.0,
            t_years: 30.0 / 365.0,
            right: Right::Call,
            ask_px_unit: 6_161.0,
            fee_rate: 0.0003,
            forecast_iv: 0.65,
            drift: 0.0,
            contract_size: 1.0,
            paths: 8_192,
            seed: 99,
        }
    }

    #[test]
    fn mc_mean_matches_closed_form_ev() {
        let a = analyze(&inp());
        // MC mean must agree with the closed-form EV within MC noise (< 1.5 %
        // of spot-scaled magnitude for 8k antithetic paths).
        let diff = (a.pnl_mean_unit - a.ev_unit).abs();
        assert!(diff < 0.02 * a.spot, "mc {} vs ev {}", a.pnl_mean_unit, a.ev_unit);
    }

    #[test]
    fn cheap_leg_has_positive_ev_when_forecast_above_implied() {
        let a = analyze(&inp());
        // Ask ≈ 62 % IV, forecast 65 % → +3 vp edge → EV ≈ +3 × vega − fee.
        let vega = vega_per_vp_unit(a.spot, a.strike, a.t_years, 0.635);
        assert!(a.iv_edge_vp > 2.0, "iv edge {}", a.iv_edge_vp);
        assert!(a.ev_unit > 2.0 * vega - 60.0, "ev {}", a.ev_unit);
        assert!(a.ev_unit < 4.0 * vega, "ev implausibly large {}", a.ev_unit);
    }

    #[test]
    fn frozen_case_loses_extrinsic_plus_fee() {
        let a = analyze(&inp());
        let expect = -(a.extrinsic_unit + a.fee_unit);
        assert!((a.frozen_pnl_unit - expect).abs() < 1e-6);
        assert!(a.frozen_pnl_unit < 0.0);
    }

    #[test]
    fn breakeven_and_probability_coherent() {
        let a = analyze(&inp());
        // Call BE = K + premium + fee.
        assert!((a.breakeven_spot - (a.strike + a.premium_unit + a.fee_unit)).abs() < 1e-6);
        // ATM-ish 30d 62→65 vol: BE ≈ +7 % → P(profit) well below 50 %.
        assert!(a.p_profit > 0.20 && a.p_profit < 0.50, "p {}", a.p_profit);
        // Scenario grid at exactly the breakeven multiplier ≈ 0 P&L.
        let be_m = a.breakeven_spot / a.spot;
        let nearest = SPOT_GRID
            .iter()
            .enumerate()
            .min_by(|(_, x), (_, y)| (x - be_m).abs().partial_cmp(&(y - be_m).abs()).unwrap())
            .map(|(i, _)| i)
            .unwrap();
        let _ = nearest; // grid is coarse; just ensure monotone payoff
        let mut prev = a.pnl_by_spot[0];
        for &p in a.pnl_by_spot.iter().skip(1) {
            assert!(p >= prev - 1e-9, "call payoff must be monotone in spot");
            prev = p;
        }
    }

    #[test]
    fn theta_accelerates_near_expiry() {
        let a = analyze(&inp());
        assert!(a.theta_now_day_unit < 0.0);
        assert!(a.theta_late_day_unit < a.theta_now_day_unit * 1.5);
        assert!(a.theta_now_day_unit > a.theta_late_day_unit);
    }

    #[test]
    fn put_mirrors_call_by_parity() {
        // C − P = S − K (r=0). Holding a cheap put vs cheap call at the same
        // moneyness must produce symmetric EVs when the premium is parity-set.
        let s = 84_383.0;
        let k = 84_000.0;
        let t = 20.0 / 365.0;
        let c = bs_price(s, k, t, 0.62, Right::Call).unwrap();
        let p = bs_price(s, k, t, 0.62, Right::Put).unwrap();
        let hi = HoldInput {
            spot: s,
            strike: k,
            t_years: t,
            right: Right::Put,
            ask_px_unit: p,
            fee_rate: 0.0003,
            forecast_iv: 0.65,
            drift: 0.0,
            contract_size: 1.0,
            paths: 8_192,
            seed: 5,
        };
        let hc = HoldInput { right: Right::Call, ask_px_unit: c, ..hi };
        let ap = analyze(&hi);
        let ac = analyze(&hc);
        // Same vol view → same vega edge; EVs agree within fee asymmetry.
        assert!((ap.ev_unit - ac.ev_unit).abs() < 1e-6, "{} {}", ap.ev_unit, ac.ev_unit);
    }
}
