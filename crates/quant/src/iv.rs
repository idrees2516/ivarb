//! Implied volatility solver: Newton-Raphson with an analytic vega and a
//! maintained bracket (bisection fallback), robust across the 1%-500% vol
//! range and near-zero-premium options.

use super::{bs_price, bs_vega, intrinsic, Right};

const VOL_FLOOR: f64 = 0.005;
const VOL_CAP: f64 = 5.0;

/// Solve the implied volatility (raw, e.g. 0.62 = 62%) of a premium quoted
/// **per one unit of underlying exposure**.
///
/// Returns `None` when the premium is not strictly above intrinsic value or
/// inputs are invalid.
pub fn implied_vol(px: f64, s: f64, k: f64, t: f64, right: Right) -> Option<f64> {
    if !(px.is_finite() && s.is_finite() && k.is_finite() && t.is_finite())
        || s <= 0.0
        || k <= 0.0
        || t <= 0.0
        || px <= 0.0
    {
        return None;
    }
    let intr = intrinsic(s, k, right);
    if px <= intr + 1e-12 {
        // At or below intrinsic: vol is effectively zero / quote is stale.
        return None;
    }
    let cap_px = match right {
        Right::Call => s,
        Right::Put => k,
    };
    if px >= cap_px * 0.999 {
        return None;
    }

    // Price tolerance scaled to the underlying (sub-milli-cent on BTC).
    let tol = 1e-10 * s;

    let mut lo = VOL_FLOOR;
    let mut hi = VOL_CAP;
    let pl = bs_price(s, k, t, lo, right)?;
    if px <= pl {
        return Some(lo);
    }
    let ph = bs_price(s, k, t, hi, right)?;
    if px >= ph {
        return Some(hi);
    }

    // Seed from moneyness-aware guess.
    let mut sig = 0.5 * ((s / k).ln().abs() + 0.2).clamp(0.05, 1.5);

    // Newton with bracket maintenance.
    for _ in 0..48 {
        let p = match bs_price(s, k, t, sig, right) {
            Some(p) => p,
            None => break,
        };
        let diff = p - px;
        if diff.abs() <= tol {
            return Some(sig.clamp(VOL_FLOOR, VOL_CAP));
        }
        if p > px {
            hi = sig;
        } else {
            lo = sig;
        }
        let v = bs_vega(s, k, t, sig);
        let mut next = if v > 1e-12 { sig - diff / v } else { 0.5 * (lo + hi) };
        if !(next > lo && next < hi) {
            next = 0.5 * (lo + hi);
        }
        if (next - sig).abs() < 1e-12 {
            return Some(next.clamp(VOL_FLOOR, VOL_CAP));
        }
        sig = next;
    }

    // Bisection fallback (rarely needed).
    for _ in 0..60 {
        let mid = 0.5 * (lo + hi);
        let p = match bs_price(s, k, t, mid, right) {
            Some(p) => p,
            None => continue,
        };
        if p > px {
            hi = mid;
        } else {
            lo = mid;
        }
        if (p - px).abs() <= tol || (hi - lo) < 1e-10 {
            return Some(mid.clamp(VOL_FLOOR, VOL_CAP));
        }
    }
    Some(0.5 * (lo + hi))
}
