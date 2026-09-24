//! Black-Scholes pricing and greeks with zero rates/dividends (the standard
//! convention for crypto options, matching how Deribit/OKX/Bybit marks work).

use super::Right;
use std::f64::consts::PI;

/// Standard normal density φ(x).
#[inline]
pub fn norm_pdf(x: f64) -> f64 {
    (-0.5 * x * x).exp() / (2.0 * PI).sqrt()
}

/// Standard normal CDF N(x).
///
/// Abramowitz & Stegun 26.2.17 (Hastings five-coefficient fit);
/// absolute error < 7.5e-8 — far below anything relevant for vol-point math.
#[inline]
pub fn norm_cdf(x: f64) -> f64 {
    const P: f64 = 0.2316419;
    const B1: f64 = 0.319381530;
    const B2: f64 = -0.356563782;
    const B3: f64 = 1.781477937;
    const B4: f64 = -1.821255978;
    const B5: f64 = 1.330274429;

    fn poly(t: f64) -> f64 {
        t * (B1 + t * (B2 + t * (B3 + t * (B4 + t * B5))))
    }

    if x >= 0.0 {
        let t = 1.0 / (1.0 + P * x);
        1.0 - norm_pdf(x) * poly(t)
    } else {
        1.0 - norm_cdf(-x)
    }
}

/// (d1, d2) for spot `s`, strike `k`, time `t` (years) and vol `sig` (raw, 0..1).
#[inline]
pub fn d1_d2(s: f64, k: f64, t: f64, sig: f64) -> (f64, f64) {
    let sq = (t * sig * sig).sqrt();
    let d1 = ((s / k).ln() + 0.5 * sig * sig * t) / sq;
    (d1, d1 - sq)
}

/// Black-Scholes price for one unit of underlying exposure (r = q = 0).
///
/// Returns `None` for non-finite or non-positive inputs.
pub fn bs_price(s: f64, k: f64, t: f64, sig: f64, right: Right) -> Option<f64> {
    if !(s.is_finite() && k.is_finite() && t.is_finite() && sig.is_finite())
        || s <= 0.0
        || k <= 0.0
        || t <= 0.0
        || sig <= 0.0
    {
        return None;
    }
    let (d1, d2) = d1_d2(s, k, t, sig);
    Some(match right {
        Right::Call => s * norm_cdf(d1) - k * norm_cdf(d2),
        Right::Put => k * norm_cdf(-d2) - s * norm_cdf(-d1),
    })
}

/// Black-Scholes vega per unit of **raw** volatility (σ moving 0.60 → 0.61),
/// per one unit of underlying exposure, in the pricing currency.
#[inline]
pub fn bs_vega(s: f64, k: f64, t: f64, sig: f64) -> f64 {
    if s <= 0.0 || k <= 0.0 || t <= 0.0 || sig <= 0.0 {
        return 0.0;
    }
    let (d1, _) = d1_d2(s, k, t, sig);
    s * t.sqrt() * norm_pdf(d1)
}

/// Black-Scholes theta per **year** per one unit of underlying exposure
/// (always ≤ 0 for r = q = 0 European options), in the pricing currency.
#[inline]
pub fn bs_theta(s: f64, k: f64, t: f64, sig: f64) -> f64 {
    if s <= 0.0 || k <= 0.0 || t <= 0.0 || sig <= 0.0 {
        return 0.0;
    }
    let (d1, _) = d1_d2(s, k, t, sig);
    -s * norm_pdf(d1) * sig / (2.0 * t.sqrt())
}

/// Black-Scholes delta per one unit of underlying exposure.
#[inline]
pub fn bs_delta(s: f64, k: f64, t: f64, sig: f64, right: Right) -> f64 {
    if s <= 0.0 || k <= 0.0 || t <= 0.0 || sig <= 0.0 {
        return 0.0;
    }
    let (d1, _) = d1_d2(s, k, t, sig);
    match right {
        Right::Call => norm_cdf(d1),
        Right::Put => norm_cdf(d1) - 1.0,
    }
}

/// Discount-less intrinsic value.
#[inline]
pub fn intrinsic(s: f64, k: f64, right: Right) -> f64 {
    match right {
        Right::Call => (s - k).max(0.0),
        Right::Put => (k - s).max(0.0),
    }
}
