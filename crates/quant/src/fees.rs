//! Fee and vega conversion helpers implementing the scanner's core identity:
//!
//! `fee_vp = (taker_fee_rate × spot) / vega_vp_usd`
//!
//! where `vega_vp_usd` is the dollar change of one contract per **vol point**
//! (1 vp = 1% IV). The ratio is roughly asset-invariant because both the fee
//! (a fraction of notional) and vega scale with spot.

/// Taker fee of one leg expressed in vol points.
///
/// `taker_rate` is the fee as a fraction of the underlying notional (e.g.
/// 0.0003 for Deribit's 0.03%), `spot` the underlying price and `vega_vp_usd`
/// the contract's dollar vega per vol point.
pub fn fee_vp(taker_rate: f64, spot: f64, vega_vp_usd: f64) -> f64 {
    if vega_vp_usd <= 0.0 || !(vega_vp_usd.is_finite()) {
        return f64::INFINITY;
    }
    (taker_rate * spot) / vega_vp_usd
}

/// Convert a raw Black-Scholes vega (per unit raw σ, per one unit of
/// underlying) into dollars-per-vol-point for a contract covering
/// `contract_size` underlying units.
///
/// A raw vega of `$9,670` (1-month ATM BTC) on a 1 BTC contract is
/// `9_670 × 1 × 0.01 = $96.70` per vol point.
pub fn vega_per_vp(vega_raw: f64, contract_size: f64) -> f64 {
    vega_raw * contract_size * 0.01
}
