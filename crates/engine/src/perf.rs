//! Nanosecond latency telemetry for the millisecond pipeline.
//!
//! The engine records how long each stage takes (quote arrival → signal
//! computed → serialized onto the wire) into fixed 1 024-sample ring buffers.
//! Percentiles are computed on demand (once per stats push) by sorting a
//! scratch copy — ~30 µs for 1 024 f64s, far below the 1 s cadence at which
//! stats are published, and zero cost on the hot path beyond one store.

/// Fixed-capacity ring of latency samples (nanoseconds).
#[derive(Debug)]
pub struct LatencyHist {
    ring: Box<[f64; CAP]>,
    idx: usize,
    len: usize,
}

const CAP: usize = 1024;

impl Default for LatencyHist {
    fn default() -> Self {
        Self::new()
    }
}

impl LatencyHist {
    pub fn new() -> Self {
        Self {
            ring: Box::new([0.0; CAP]),
            idx: 0,
            len: 0,
        }
    }

    /// Record one sample (nanoseconds).
    #[inline]
    pub fn record_ns(&mut self, ns: u64) {
        self.ring[self.idx] = ns as f64;
        self.idx = (self.idx + 1) % CAP;
        if self.len < CAP {
            self.len += 1;
        }
    }

    /// Record an elapsed duration (microseconds helper).
    #[inline]
    pub fn record_us(&mut self, us: f64) {
        if us.is_finite() && us >= 0.0 {
            self.record_ns((us * 1000.0) as u64);
        }
    }

    /// Number of samples kept.
    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// (p50, p90, p99, p999) in microseconds.
    pub fn percentiles_us(&self) -> (f64, f64, f64, f64) {
        if self.len == 0 {
            return (0.0, 0.0, 0.0, 0.0);
        }
        let mut scratch: Vec<f64> = self.ring[..self.len].to_vec();
        scratch.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let at = |p: f64| -> f64 {
            let idx = (p.clamp(0.0, 1.0) * (scratch.len() - 1) as f64).floor() as usize;
            scratch[idx] / 1000.0
        };
        (at(0.50), at(0.90), at(0.99), at(0.999))
    }

    /// Arithmetic mean in microseconds (EWMA-friendly).
    pub fn mean_us(&self) -> f64 {
        if self.len == 0 {
            return 0.0;
        }
        self.ring[..self.len].iter().sum::<f64>() / self.len as f64 / 1000.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentiles_monotone_and_centered() {
        let mut h = LatencyHist::new();
        for i in 0..1000u64 {
            h.record_ns(i * 1_000); // 0 µs … 999 µs
        }
        let (p50, p90, p99, p999) = h.percentiles_us();
        assert!((p50 - 500.0).abs() <= 1.0, "p50 {p50}");
        assert!((p90 - 900.0).abs() <= 1.0, "p90 {p90}");
        assert!((p99 - 990.0).abs() <= 1.0, "p99 {p99}");
        assert!(p999 >= p99 && p99 >= p90 && p90 >= p50);
    }

    #[test]
    fn ring_wraps() {
        let mut h = LatencyHist::new();
        for i in 0..(CAP + 100) {
            h.record_ns(i as u64 * 7);
        }
        assert_eq!(h.len(), CAP);
        let (p50, _, _, _) = h.percentiles_us();
        // After wrap the newest CAP samples dominate: median ≈ middle of the
        // last 1024 values (≈ (100+1024/2)×7 ns ≈ 4.3 µs).
        assert!(p50 > 2.0 && p50 < 8.0, "p50 {p50}");
    }
}
