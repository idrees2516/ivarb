//! Pure, dependency-free quantitative core for the IV arbitrage engine.
//!
//! Everything here is `no_std`-friendly plain f64 math so the exact same code
//! can run inside the backend engine and inside the WASM frontend calculator.

mod bs;
mod cost;
mod fees;
mod hold;
mod iv;
mod rng;

pub use bs::{bs_delta, bs_price, bs_theta, bs_vega, d1_d2, intrinsic, norm_cdf, norm_pdf};
pub use cost::{
    capital_req, cost_sheet, funding_round_trip, margin_per_contract, roc_pct, slippage_frac,
    CapitalReq, CostInput, CostSheet, FundingRail, FUNDING_RAILS, MARGIN_BUFFER,
    MARGIN_CARRY_RATE, THETA_NETTING,
};
pub use fees::{fee_vp, vega_per_vp};
pub use hold::{
    analyze as hold_analyze, analyze_lite as hold_analyze_lite, expected_terminal_payoff,
    percentile_sorted, price_at, vega_per_vp_unit, HoldAnalysis, HoldInput, HoldStatsLite,
    SPOT_GRID,
};
pub use iv::implied_vol;
pub use rng::Rng;

/// Re-exported for downstream users of the cost model.
pub use Right as QuantRight;

#[cfg(test)]
mod tests;

/// Standard crypto convention: 365-day year, no discrete compounding.
pub const SECONDS_PER_YEAR: f64 = 365.0 * 24.0 * 3600.0;

/// Option right shared by the quant crate (kept dep-free; the protocol crate
/// has its own serde-friendly copy).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Right {
    Call,
    Put,
}
