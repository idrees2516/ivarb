//! Deterministic, dependency-free pseudo-random generation for Monte-Carlo
//! simulations and the genetic optimizer.
//!
//! SplitMix64 is a tiny, extremely fast generator with excellent statistical
//! properties (passes BigCrush). Pairing it with a cached Box-Muller
//! transform gives standard-normal draws at ~5 ns each — the hold-to-expiry
//! Monte-Carlo (4 096 antithetic paths) costs well under a millisecond.

/// SplitMix64 RNG (seedable, reproducible) with a cached Box-Muller variate.
#[derive(Debug, Clone)]
pub struct Rng {
    state: u64,
    cache: Option<f64>,
}

impl Rng {
    #[inline]
    pub fn new(seed: u64) -> Self {
        Self { state: seed, cache: None }
    }

    /// Seed from a runtime nonce (pid/time mix) when reproducibility is not
    /// required.
    pub fn from_entropy() -> Self {
        let t = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0x9E3779B97F4A7C15);
        let pid = std::process::id() as u64;
        Self {
            state: t ^ (pid.wrapping_mul(0x9E3779B97F4A7C15)),
            cache: None,
        }
    }

    /// Next uniform u64.
    #[inline]
    pub fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E3779B97F4A7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
        z ^ (z >> 31)
    }

    /// Uniform in [0, 1).
    #[inline]
    pub fn next_f64(&mut self) -> f64 {
        // 53-bit mantissa → full f64 precision.
        (self.next_u64() >> 11) as f64 * (1.0 / (1u64 << 53) as f64)
    }

    /// Uniform in [lo, hi).
    #[inline]
    pub fn next_range(&mut self, lo: f64, hi: f64) -> f64 {
        lo + (hi - lo) * self.next_f64()
    }

    /// Standard normal draw (Box-Muller with the cached second variate).
    #[inline]
    pub fn next_normal(&mut self) -> f64 {
        if let Some(z1) = self.cache.take() {
            return z1;
        }
        // Guard against u == 0 (log(0)).
        let mut u1 = self.next_f64();
        if u1 <= f64::MIN_POSITIVE {
            u1 = f64::MIN_POSITIVE;
        }
        let u2 = self.next_f64();
        let r = (-2.0 * u1.ln()).sqrt();
        let theta = 2.0 * std::f64::consts::PI * u2;
        self.cache = Some(r * theta.sin());
        r * theta.cos()
    }

    /// Gaussian mutation step, clipped to ±3σ.
    #[inline]
    pub fn next_normal_clipped(&mut self, sigma: f64) -> f64 {
        let z = self.next_normal();
        z.clamp(-3.0, 3.0) * sigma
    }

    /// True with probability `p`.
    #[inline]
    pub fn bernoulli(&mut self, p: f64) -> bool {
        self.next_f64() < p
    }

    /// Random index in [0, n).
    #[inline]
    pub fn gen_range_usize(&mut self, n: usize) -> usize {
        if n == 0 {
            return 0;
        }
        (self.next_u64() % n as u64) as usize
    }
}

impl Default for Rng {
    fn default() -> Self {
        Self::from_entropy()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uniform_bounds_and_reproducibility() {
        let mut a = Rng::new(42);
        let mut b = Rng::new(42);
        for _ in 0..10_000 {
            let (x, y) = (a.next_f64(), b.next_f64());
            assert_eq!(x.to_bits(), y.to_bits());
            assert!((0.0..1.0).contains(&x));
        }
    }

    #[test]
    fn normal_mean_and_std() {
        let mut r = Rng::new(7);
        let n = 200_000;
        let mut mean = 0.0;
        let mut m2 = 0.0;
        for i in 0..n {
            let x = r.next_normal();
            let delta = x - mean;
            mean += delta / (i + 1) as f64;
            m2 += delta * (x - mean);
        }
        let var = m2 / (n - 1) as f64;
        assert!(mean.abs() < 0.01, "mean {mean}");
        assert!((var - 1.0).abs() < 0.03, "var {var}");
    }
}
