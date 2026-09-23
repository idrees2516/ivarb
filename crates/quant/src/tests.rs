//! Reference tests for the quant core, including the worked example from the
//! product spec (Deribit 0.03% taker fee ≈ 0.0195 vp per leg).

use crate::{bs_price, bs_vega, fee_vp, implied_vol, intrinsic, norm_cdf, vega_per_vp, Right};

fn close(a: f64, b: f64, tol: f64) -> bool {
    (a - b).abs() <= tol
}

#[test]
fn norm_cdf_references() {
    assert!(close(norm_cdf(0.0), 0.5, 1e-9));
    assert!(close(norm_cdf(1.959964), 0.975, 1e-5));
    assert!(close(norm_cdf(-1.281552), 0.10, 1e-5));
    assert!(close(norm_cdf(3.0), 0.99865, 1e-5));
    // symmetry
    for x in [0.3, 1.0, 2.5] {
        assert!(close(norm_cdf(x) + norm_cdf(-x), 1.0, 1e-9));
    }
}

#[test]
fn bs_price_reference() {
    // S=K=100, T=1, σ=0.2, r=0 → call = put ≈ 7.9656
    let c = bs_price(100.0, 100.0, 1.0, 0.2, Right::Call).unwrap();
    let p = bs_price(100.0, 100.0, 1.0, 0.2, Right::Put).unwrap();
    assert!(close(c, 7.9656, 2e-3), "call {c}");
    assert!(close(p, 7.9656, 2e-3), "put {p}");
    // put-call parity with r=0: C - P = S - K
    for (s, k) in [(105.0, 90.0), (70.0, 120.0), (100.0, 100.0)] {
        let c = bs_price(s, k, 0.5, 0.8, Right::Call).unwrap();
        let p = bs_price(s, k, 0.5, 0.8, Right::Put).unwrap();
        assert!(close(c - p, s - k, 1e-9));
    }
}

#[test]
fn bs_price_edges() {
    assert!(bs_price(0.0, 100.0, 1.0, 0.2, Right::Call).is_none());
    assert!(bs_price(100.0, 100.0, 0.0, 0.2, Right::Call).is_none());
    // vol → 0 converges to intrinsic (checked via very small vol)
    let c = bs_price(120.0, 100.0, 0.01, 1e-4, Right::Call).unwrap();
    assert!(close(c, intrinsic(120.0, 100.0, Right::Call), 1e-6));
}

#[test]
fn bs_vega_reference() {
    // S=K=100, T=1, σ=0.2 → vega ≈ 39.695 per raw vol unit
    let v = bs_vega(100.0, 100.0, 1.0, 0.2);
    assert!(close(v, 39.695, 2e-2), "vega {v}");
    // vega peaks ATM and is symmetric in moneyness around ATM
    let atm = bs_vega(100.0, 100.0, 1.0, 0.5);
    let otm = bs_vega(140.0, 100.0, 1.0, 0.5);
    assert!(otm < atm);
}

#[test]
fn vega_per_vp_conversion() {
    // 1-month ATM BTC: raw vega ≈ $9,670 per unit σ, contract 1 BTC → $96.7/vp
    assert!(close(vega_per_vp(9_670.0, 1.0), 96.70, 1e-9));
    // 0.01 BTC contract
    assert!(close(vega_per_vp(9_670.0, 0.01), 0.967, 1e-9));
}

#[test]
fn fee_worked_example_from_spec() {
    // spot $65,000, vega $1,000/vp, Deribit 0.03% taker:
    // (0.0003 × 65000) / 1000 = 0.0195 vp per leg
    assert!(close(fee_vp(0.0003, 65_000.0, 1_000.0), 0.0195, 1e-12));
    // Round trip of two legs ≈ 0.039 vp; the 0.5 vp gate needs ≥ 0.539 vp spread.
    let rt = 2.0 * fee_vp(0.0003, 65_000.0, 1_000.0);
    assert!(close(rt, 0.039, 1e-12));
    // Zero vega → infinite fee (engine skips such quotes)
    assert!(fee_vp(0.0003, 65_000.0, 0.0).is_infinite());
}

#[test]
fn implied_vol_roundtrip() {
    for &sig in &[0.05, 0.10, 0.25, 0.45, 0.62, 1.0, 1.8, 3.0] {
        for &(s, k) in &[(100.0, 100.0), (100.0, 130.0), (100.0, 70.0)] {
            for &t in &[0.02, 0.25, 1.0] {
                for &right in &[Right::Call, Right::Put] {
                    let px = bs_price(s, k, t, sig, right).unwrap();
                    // Quotes whose time value beyond intrinsic is unrepresentable
                    // (deep ITM/OTM at tiny vol/tenor) are skipped by design:
                    // no venue can quote them, so no IV is solvable.
                    if px - intrinsic(s, k, right) < 1e-8 * s {
                        continue;
                    }
                    let iv = implied_vol(px, s, k, t, right)
                        .unwrap_or_else(|| panic!("no iv for sig={sig} s={s} k={k} t={t}"));
                    let rel = ((iv - sig) / sig).abs();
                    assert!(rel < 1e-4, "sig={sig} iv={iv} rel={rel} s={s} k={k} t={t}");
                }
            }
        }
    }
}

#[test]
fn implied_vol_rejects_bad_quotes() {
    // at/below intrinsic
    assert!(implied_vol(20.0, 100.0, 80.0, 0.5, Right::Call).is_none());
    assert!(implied_vol(30.0, 100.0, 70.0, 0.5, Right::Call).is_none());
    // call premium at/above spot
    assert!(implied_vol(100.0, 100.0, 0.5, 0.9, Right::Call).is_none());
    assert!(implied_vol(150.0, 100.0, 0.5, 0.9, Right::Call).is_none());
    // garbage inputs
    assert!(implied_vol(f64::NAN, 100.0, 100.0, 0.5, Right::Call).is_none());
    assert!(implied_vol(5.0, 0.0, 100.0, 0.5, Right::Call).is_none());
    assert!(implied_vol(5.0, 100.0, 100.0, 0.0, Right::Call).is_none());
}

#[test]
fn implied_vol_btc_scale() {
    // BTC-scale sanity: 1M ATM, 3 days, 60% vol.
    let px = bs_price(84_383.0, 84_000.0, 3.0 / 365.0, 0.60, Right::Call).unwrap();
    let iv = implied_vol(px, 84_383.0, 84_000.0, 3.0 / 365.0, Right::Call).unwrap();
    assert!(close(iv, 0.60, 1e-6), "iv {iv}");
}
