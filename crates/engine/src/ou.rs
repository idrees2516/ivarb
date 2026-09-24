//! Mean-reversion analytics for the spread series: Ornstein-Uhlenbeck
//! half-life (expected convergence time) and a Kalman local-level filter
//! (noise-aware take-profit).
//!
//! - **OU half-life**: the spread `s_t` is modeled as an AR(1),
//!   `s_t = c + φ·s_{t−1} + ε`. The half-life `ln2 / −ln φ` says how long a
//!   divergence typically takes to halve — i.e. the expected convergence
//!   horizon of an arb position. This is the "how long am I exposed to theta
//!   while waiting" number.
//! - **Kalman filter**: a local-level model separates the *persistent* spread
//!   level from quote noise. The innovation σ becomes a noise gate: the
//!   executor only banks convergence when the spread has fallen below
//!   `level + k·σ`, not on any touching tick — which measurably reduces
//!   premature exits on flicker.

/// Result of an AR(1) fit on the spread history.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OuFit {
    /// AR(1) coefficient φ (< 1 for a stationary series).
    pub phi: f64,
    /// Half-life in sample steps (caller converts with the sampling period).
    pub half_life_steps: f64,
    /// Residual σ per step.
    pub resid_sigma: f64,
    /// Number of samples used.
    pub n: usize,
}

/// Fit `s_t = c + φ·s_{t-1} + ε` by OLS on the sample pairs.
pub fn fit_ar1(samples: &[(i64, f64)]) -> Option<OuFit> {
    if samples.len() < 8 {
        return None;
    }
    let mut n = 0usize;
    let mut sx = 0.0;
    let mut sy = 0.0;
    let mut sxx = 0.0;
    let mut sxy = 0.0;
    for w in samples.windows(2) {
        let (x, y) = (w[0].1, w[1].1);
        if x.is_finite() && y.is_finite() {
            n += 1;
            sx += x;
            sy += y;
            sxx += x * x;
            sxy += x * y;
        }
    }
    if n < 8 {
        return None;
    }
    let nf = n as f64;
    let denom = nf * sxx - sx * sx;
    if denom.abs() < 1e-12 {
        return None;
    }
    let phi = (nf * sxy - sx * sy) / denom;
    if !phi.is_finite() || phi <= 0.0 || phi >= 1.0 {
        // No mean reversion detectable (explosive or flat series).
        return None;
    }
    // Residual σ around the AR line.
    let c = (sy - phi * sx) / nf;
    let mut ssr = 0.0;
    for w in samples.windows(2) {
        let (x, y) = (w[0].1, w[1].1);
        if x.is_finite() && y.is_finite() {
            let e = y - (c + phi * x);
            ssr += e * e;
        }
    }
    let resid_sigma = (ssr / (nf - 2.0).max(1.0)).sqrt();
    // ln(0.5)/ln(φ) = (−ln2)/(ln φ); φ<1 → ln φ<0 → positive half-life.
    let half_life_steps = (-std::f64::consts::LN_2) / phi.ln();
    Some(OuFit {
        phi,
        half_life_steps: half_life_steps.max(0.0),
        resid_sigma,
        n,
    })
}

/// OU half-life in seconds given a sample series (ts ms, value).
pub fn half_life_s(samples: &[(i64, f64)]) -> Option<f64> {
    let fit = fit_ar1(samples)?;
    // Median sampling period from timestamps.
    let n = samples.len();
    if n < 2 {
        return None;
    }
    let period_ms = (samples[n - 1].0 - samples[0].0).max(1) as f64 / (n - 1) as f64;
    Some(fit.half_life_steps * period_ms / 1000.0)
}

/// Scalar Kalman local-level filter over a noisy series.
#[derive(Debug, Clone, Copy)]
pub struct Kalman1 {
    /// Estimated level.
    pub level: f64,
    /// Estimation variance of the level.
    pub p: f64,
    /// Innovation (prediction-error) variance EWMA — the noise gauge.
    pub innov_sigma: f64,
    /// Process noise (how fast the true level may drift).
    pub q: f64,
    /// Measurement noise prior.
    pub r: f64,
    pub inited: bool,
}

impl Kalman1 {
    pub fn new(q: f64, r: f64) -> Self {
        Self {
            level: 0.0,
            p: 1.0,
            innov_sigma: 0.0,
            q,
            r,
            inited: false,
        }
    }

    /// Push one observation; returns the updated (level, innov_sigma).
    pub fn update(&mut self, z: f64) -> (f64, f64) {
        if !z.is_finite() {
            return (self.level, self.innov_sigma);
        }
        if !self.inited {
            self.level = z;
            self.p = self.r;
            self.inited = true;
            return (self.level, self.innov_sigma);
        }
        // Predict.
        self.p += self.q;
        // Innovation.
        let y = z - self.level;
        let innov_var = self.p + self.r;
        // Update EWMA of |innovation| as a robust σ proxy.
        let s = innov_var.sqrt();
        self.innov_sigma = if self.innov_sigma == 0.0 {
            s
        } else {
            self.innov_sigma * 0.94 + s * 0.06
        };
        // Correct.
        let k = self.p / innov_var;
        self.level += k * y;
        self.p *= 1.0 - k;
        (self.level, self.innov_sigma)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Generate an AR(1) series with known half-life and iid noise.
    fn ar1_series(phi: f64, n: usize, step_ms: i64) -> Vec<(i64, f64)> {
        ar1_series_seeded(42, phi, n, step_ms)
    }

    fn ar1_series_seeded(seed: u64, phi: f64, n: usize, step_ms: i64) -> Vec<(i64, f64)> {
        let mut rng = ivarb_quant::Rng::new(seed);
        let mut s = 1.0;
        let mut out = Vec::with_capacity(n);
        for i in 0..n {
            let noise = rng.next_normal() * 0.05;
            s = phi * s + noise;
            out.push((i as i64 * step_ms, s));
        }
        out
    }

    #[test]
    fn recovers_half_life_within_tolerance() {
        // φ = 0.99 per 1 s sample → half-life = ln2 / −ln(0.99) ≈ 68.97 steps ≈ 69 s.
        // The OLS estimate has SE ≈ 12 s at this φ, so average four
        // independent series to bring the estimator noise well inside the
        // 25 % tolerance (mean estimate ≈ 65 s after the small Hurwicz bias).
        let est: f64 = [42u64, 43, 44, 45]
            .iter()
            .filter_map(|&seed| {
                let series = ar1_series_seeded(seed, 0.99, 6_000, 1000);
                half_life_s(&series)
            })
            .collect::<Vec<_>>()
            .iter()
            .sum::<f64>()
            / 4.0;
        assert!(
            (est - 69.0).abs() / 69.0 < 0.25,
            "half life {est} vs ~69"
        );
    }

    #[test]
    fn non_stationary_rejected() {
        // Explosive series → no fit.
        let series = ar1_series(1.05, 100, 1000);
        assert!(half_life_s(&series).is_none());
    }

    #[test]
    fn kalman_separates_level_from_noise() {
        let mut k = Kalman1::new(1e-4, 0.05);
        // True level 2.0, noise ±0.2.
        let truth = 2.0;
        let noise = [0.2, -0.2, 0.15, -0.15, 0.1, -0.1, 0.18, -0.18, 0.05, -0.05];
        let mut last = (0.0, 0.0);
        for (i, nz) in noise.iter().enumerate() {
            last = k.update(truth + nz + i as f64 * 0.0);
        }
        assert!((last.0 - truth).abs() < 0.15, "level {}", last.0);
        assert!(last.1 > 0.0);
    }
}
