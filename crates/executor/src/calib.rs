//! Route-capture calibration — the executor's learning loop.
//!
//! Every venue pair ("route", `buy_venue>sell_venue`) has its own reality:
//! quote flicker, leg latency, depth shape and settlement quirks decide how
//! much of the *displayed* edge actually lands in the account. The scanner's
//! EV is an estimate; the calibration table replaces belief with measurement.
//!
//! ## How it learns
//! - Every closed trade books its **capture ratio** (`realized_vp /
//!   signal_net_vp`) into an EWMA for the route (α = 0.30 — three or four
//!   trades swing the estimate).
//! - Every aborted attempt books into an abort-rate EWMA (quote moved away /
//!   dust / timeout before both legs filled).
//!
//! ## How it is applied
//! - **Calibrated edge** = `displayed EV × route multiplier`. Ranking, the
//!   firing floor and (via the same multiplier) manual takes all see the
//!   calibrated number, so a route that historically banks 40% of its edge
//!   must show 2.5× the gate before it fires.
//! - **Quote-flicker refusal**: ≥8 attempts with >85% aborts → the pair's
//!   book is fiction; stop paying latency for it until the EWMA recovers.
//! - **Inversion refusal**: ≥5 closes with negative capture EWMA → the
//!   "edge" on this route is systematically inverted (stale marks, fee
//!   model mismatch); multiplier floors at 0.05, not 0, so a genuinely good
//!   print can still revive the route.
//!
//! Warm-up is neutral: below 3 closed trades the multiplier is exactly 1.0 —
//! the calibration never invents an opinion it has not earned.

use rustc_hash::FxHashMap;

/// EWMA learning rate for capture and abort statistics.
const ALPHA: f64 = 0.30;
/// Closed trades required before the multiplier departs from neutral.
const WARMUP_CLOSES: u32 = 3;
/// Attempts required before the abort-rate penalty engages.
const WARMUP_ATTEMPTS: u32 = 8;
/// Closes required before a negative-capture route is refused outright.
const INVERSION_CLOSES: u32 = 5;

/// Per-route learned state.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RouteStat {
    pub attempts: u32,
    pub aborts: u32,
    pub closed: u32,
    /// EWMA of `realized_vp / signal_net_vp` (clamped ±5).
    pub capture_ewma: f64,
    /// EWMA of `aborted / attempted` (0 = every attempt fills both legs).
    pub abort_ewma: f64,
}

impl RouteStat {
    fn fresh() -> Self {
        RouteStat {
            attempts: 0,
            aborts: 0,
            closed: 0,
            capture_ewma: 1.0,
            abort_ewma: 0.0,
        }
    }

    /// The learned edge multiplier for this route (see module docs).
    pub fn edge_mult(&self) -> f64 {
        // Capture learning needs closed trades; until then neutral.
        let mut m = if self.closed < WARMUP_CLOSES {
            1.0
        } else {
            let c = self.capture_ewma.clamp(0.05, 1.15);
            if self.capture_ewma <= 0.0 && self.closed >= INVERSION_CLOSES {
                0.05 // systematically inverted — effectively refuse
            } else {
                c
            }
        };
        // Fill reliability is learnable from attempts alone — applies even
        // before any trade closes (a pair that never fills never trades).
        if self.attempts >= WARMUP_ATTEMPTS && self.abort_ewma > 0.85 {
            m *= 0.5; // fill reliability discount
        }
        m
    }

    /// True when the route's book is mostly fiction (abort-dominated).
    pub fn flicker(&self) -> bool {
        self.attempts >= WARMUP_ATTEMPTS && self.abort_ewma > 0.85
    }
}

/// The calibration table. Owned by the single-writer executor core.
#[derive(Debug, Default)]
pub struct Calib {
    routes: FxHashMap<String, RouteStat>,
}

impl Calib {
    pub fn new() -> Self {
        Self::default()
    }

    fn stat(&mut self, route: &str) -> &mut RouteStat {
        self.routes.entry(route.to_string()).or_insert_with(RouteStat::fresh)
    }

    /// An attempt was spawned on this route.
    pub fn record_attempt(&mut self, route: &str) {
        let s = self.stat(route);
        s.attempts += 1;
        // Abort EWMA updates on events, not on attempts; attempts only arm
        // the sample counter. Initialize on first attempt.
        if s.attempts == 1 {
            s.abort_ewma = 0.0;
        }
    }

    /// An attempt aborted before both legs filled.
    pub fn record_abort(&mut self, route: &str) {
        let s = self.stat(route);
        s.aborts += 1;
        let x = (s.aborts as f64 / s.attempts.max(1) as f64).clamp(0.0, 1.0);
        s.abort_ewma = if s.aborts + s.closed <= 1 { x } else { s.abort_ewma + ALPHA * (x - s.abort_ewma) };
    }

    /// A position on this route closed; `capture` is the realized fraction of
    /// the signal edge (clamped ±5 upstream).
    pub fn record_close(&mut self, route: &str, capture: f64) {
        let s = self.stat(route);
        s.closed += 1;
        let x = capture.clamp(-5.0, 5.0);
        s.capture_ewma = if s.closed == 1 { x } else { s.capture_ewma + ALPHA * (x - s.capture_ewma) };
    }

    /// Learned multiplier for a route (1.0 when unknown — neutral).
    pub fn edge_mult(&self, route: &str) -> f64 {
        self.routes.get(route).map(|s| s.edge_mult()).unwrap_or(1.0)
    }

    /// Quote-flicker refusal check for a route.
    pub fn flicker(&self, route: &str) -> bool {
        self.routes.get(route).map(|s| s.flicker()).unwrap_or(false)
    }

    /// Compact, UI-facing copy of the table, best-calibrated routes first.
    pub fn snapshot(&self) -> Vec<RouteStatOut> {
        let mut v: Vec<RouteStatOut> = self
            .routes
            .iter()
            .map(|(route, s)| RouteStatOut {
                route: route.clone(),
                attempts: s.attempts,
                aborts: s.aborts,
                closed: s.closed,
                capture_ewma: s.capture_ewma,
                abort_ewma: s.abort_ewma,
                edge_mult: s.edge_mult(),
                flicker: s.flicker(),
            })
            .collect();
        v.sort_by(|a, b| {
            b.closed
                .cmp(&a.closed)
                .then(b.attempts.cmp(&a.attempts))
                .then(a.route.cmp(&b.route))
        });
        v.truncate(16);
        v
    }
}

/// Wire copy of one route's learned state — the serde type lives in
/// `ivarb-protocol` so the frontend deserializes it from the exec snapshot.
pub use ivarb_protocol::RouteStatOut;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn warmup_is_neutral() {
        let mut c = Calib::new();
        c.record_attempt("okx>binance");
        c.record_abort("okx>binance");
        c.record_close("okx>binance", 0.2);
        c.record_close("okx>binance", 0.1);
        // 2 closes < 3 warm-up → neutral
        assert!((c.edge_mult("okx>binance") - 1.0).abs() < 1e-9);
        c.record_close("okx>binance", 0.3);
        // 3rd close arms the multiplier; EWMA after 3 samples of ~0.2 capture
        let m = c.edge_mult("okx>binance");
        assert!(m > 0.05 && m < 0.5, "multiplier should track ~0.2 capture: {m}");
    }

    #[test]
    fn inversion_and_flicker_refuse() {
        let mut c = Calib::new();
        for i in 0..6 {
            c.record_attempt("deribit>aevo");
            if i < 5 {
                c.record_close("deribit>aevo", -0.8); // systematically loses
            }
        }
        let m = c.edge_mult("deribit>aevo");
        assert!(m <= 0.051, "inverted route must be refused: {m}");

        let mut f = Calib::new();
        for _ in 0..10 {
            f.record_attempt("x>y");
            f.record_abort("x>y");
        }
        assert!(f.flicker("x>y"), "abort-dominated route must be flagged");
        assert!(f.edge_mult("x>y") <= 0.5, "flicker discount must bite");
    }

    #[test]
    fn good_route_gets_credit() {
        let mut c = Calib::new();
        for _ in 0..4 {
            c.record_attempt("a>b");
            c.record_close("a>b", 0.95);
        }
        let m = c.edge_mult("a>b");
        assert!(m > 0.8, "high-capture route keeps most of its edge: {m}");
        assert!(!c.flicker("a>b"));
    }
}
