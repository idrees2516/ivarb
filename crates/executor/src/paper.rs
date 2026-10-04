//! Paper broker: realistic fill simulation so the whole execution pipeline —
//! sizing, leg sequencing, the 10-second rule, unwinds, hedging, exits — runs
//! end-to-end without exchange keys, and behaves like a real venue.
//!
//! Fill model:
//! - **Taker**: latency 60–250 ms; fills up to `0.9 × displayed depth` at the
//!   touch, with slippage `2 bp + 8 bp × √(size/depth)` against us; anything
//!   beyond the depth walks to a second level 40% worse.
//! - **Maker**: no latency; fills when the market's opposite touch crosses the
//!   limit, or probabilistically per second when joined at the touch.
//! - **Fees**: `taker_rate × spot × units` deducted from per-venue balances;
//!   maker legs are free (Deribit/Derive-style maker economics).

use rustc_hash::FxHashMap;
use std::sync::Mutex;

use rand::Rng;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Buy,
    Sell,
}

/// Result of one simulated fill.
#[derive(Debug, Clone, Copy)]
pub struct Fill {
    /// Contracts filled on this venue.
    pub contracts: f64,
    /// Average premium per 1 underlying unit (after slippage).
    pub px_unit: f64,
    pub fee_usd: f64,
    pub latency_ms: u64,
}

impl Fill {
    pub fn zero() -> Self {
        Fill {
            contracts: 0.0,
            px_unit: 0.0,
            fee_usd: 0.0,
            latency_ms: 0,
        }
    }
}

pub struct PaperBroker {
    balances: Mutex<FxHashMap<String, f64>>,
    /// Cumulative fees paid (USD), for stats.
    pub fees_total: Mutex<f64>,
    start_balance: f64,
    /// Treasury: lifetime deposits / withdrawals (USD), global.
    deposits_total: Mutex<f64>,
    withdrawals_total: Mutex<f64>,
    /// Per-venue lifetime net deposits (for the funding panel).
    deposits_by_venue: Mutex<FxHashMap<String, f64>>,
}

impl PaperBroker {
    pub fn new(venues: &[String], start_balance: f64) -> Self {
        let mut m = FxHashMap::default();
        for v in venues {
            m.insert(v.clone(), start_balance);
        }
        Self {
            balances: Mutex::new(m),
            fees_total: Mutex::new(0.0),
            start_balance,
            deposits_total: Mutex::new(0.0),
            withdrawals_total: Mutex::new(0.0),
            deposits_by_venue: Mutex::new(FxHashMap::default()),
        }
    }

    pub fn start_balance(&self) -> f64 {
        self.start_balance
    }

    /// Provision an account for a venue that registered after boot (sim
    /// feeds, late venue connects): same starting balance as the boot set.
    /// Returns true when a new account was created.
    pub fn ensure_venue(&self, venue: &str) -> bool {
        let mut b = self.balances.lock().unwrap();
        if b.contains_key(venue) {
            return false;
        }
        b.insert(venue.to_string(), self.start_balance);
        true
    }

    pub fn balances(&self) -> Vec<(String, f64)> {
        let b = self.balances.lock().unwrap();
        let mut v: Vec<(String, f64)> = b.iter().map(|(k, x)| (k.clone(), *x)).collect();
        v.sort_by(|a, b| a.0.cmp(&b.0));
        v
    }

    /// Balance of one venue (0.0 when unknown).
    pub fn balance_of(&self, venue: &str) -> f64 {
        self.balances.lock().unwrap().get(venue).copied().unwrap_or(0.0)
    }

    /// Total account equity across venues.
    pub fn equity(&self) -> f64 {
        self.balances.lock().unwrap().values().sum()
    }

    /// Lifetime net deposits (deposits − withdrawals), all venues.
    pub fn net_deposits(&self) -> f64 {
        *self.deposits_total.lock().unwrap() - *self.withdrawals_total.lock().unwrap()
    }

    /// Deposit paper money onto a venue. Returns false on non-positive input.
    pub fn deposit(&self, venue: &str, amount_usd: f64) -> bool {
        if !amount_usd.is_finite() || amount_usd <= 0.0 {
            return false;
        }
        self.adjust(venue, amount_usd);
        *self.deposits_total.lock().unwrap() += amount_usd;
        *self
            .deposits_by_venue
            .lock()
            .unwrap()
            .entry(venue.to_string())
            .or_insert(0.0) += amount_usd;
        true
    }

    /// Withdraw paper money from a venue (refuses to overdraw).
    pub fn withdraw(&self, venue: &str, amount_usd: f64) -> bool {
        if !amount_usd.is_finite() || amount_usd <= 0.0 {
            return false;
        }
        let mut b = self.balances.lock().unwrap();
        let bal = b.get_mut(venue).copied().unwrap_or(self.start_balance);
        if bal < amount_usd {
            return false;
        }
        *b.entry(venue.to_string()).or_insert(self.start_balance) -= amount_usd;
        drop(b);
        *self.withdrawals_total.lock().unwrap() += amount_usd;
        *self
            .deposits_by_venue
            .lock()
            .unwrap()
            .entry(venue.to_string())
            .or_insert(0.0) -= amount_usd;
        true
    }

    /// Per-venue lifetime net deposits, sorted by venue name.
    pub fn deposits_by_venue(&self) -> Vec<(String, f64)> {
        let d = self.deposits_by_venue.lock().unwrap();
        let mut v: Vec<(String, f64)> = d.iter().map(|(k, x)| (k.clone(), *x)).collect();
        v.sort_by(|a, b| a.0.cmp(&b.0));
        v
    }

    fn adjust(&self, venue: &str, delta: f64) {
        let mut b = self.balances.lock().unwrap();
        *b.entry(venue.to_string()).or_insert(self.start_balance) += delta;
    }

    /// Simulate an aggressive (taker) fill.
    ///
    /// `touch_unit` is the side of the book we cross (ask when buying, bid
    /// when selling), `depth_contracts` the displayed size there.
    #[allow(clippy::too_many_arguments)]
    pub async fn take(
        &self,
        venue: &str,
        side: Side,
        touch_unit: f64,
        depth_contracts: f64,
        want_contracts: f64,
        taker_rate: f64,
        spot: f64,
        contract_size: f64,
    ) -> Fill {
        let latency_ms = rand::thread_rng().gen_range(60..=250);
        tokio::time::sleep(std::time::Duration::from_millis(latency_ms)).await;
        self.take_sync(
            venue,
            side,
            touch_unit,
            depth_contracts,
            want_contracts,
            taker_rate,
            spot,
            contract_size,
            latency_ms,
        )
    }

    /// Synchronous fill (no latency sleep) — used by the monitor's exit path
    /// so the executor core never blocks.
    #[allow(clippy::too_many_arguments)]
    pub fn take_sync(
        &self,
        venue: &str,
        side: Side,
        touch_unit: f64,
        depth_contracts: f64,
        want_contracts: f64,
        taker_rate: f64,
        spot: f64,
        contract_size: f64,
        latency_ms: u64,
    ) -> Fill {
        if want_contracts <= 0.0 || touch_unit <= 0.0 || depth_contracts <= 0.0 {
            return Fill::zero();
        }
        let depth = depth_contracts * 0.9;
        let (filled, px) = if want_contracts <= depth {
            let ratio = (want_contracts / depth).clamp(0.0, 1.0);
            let slip = 0.0002 + 0.0008 * ratio.sqrt();
            let px = match side {
                Side::Buy => touch_unit * (1.0 + slip),
                Side::Sell => touch_unit * (1.0 - slip),
            };
            (want_contracts, px)
        } else {
            // Walk one extra level: first `depth` at the touch, remainder 40% worse.
            let px2 = match side {
                Side::Buy => touch_unit * 1.0009,
                Side::Sell => touch_unit * 0.9991,
            };
            let avg = (depth * touch_unit + (want_contracts - depth) * px2) / want_contracts;
            (want_contracts, avg)
        };
        let units = filled * contract_size;
        let fee_usd = taker_rate * spot * units;
        let cash = match side {
            Side::Buy => -(filled * px * contract_size) - fee_usd,
            Side::Sell => filled * px * contract_size - fee_usd,
        };
        self.adjust(venue, cash);
        *self.fees_total.lock().unwrap() += fee_usd;
        Fill {
            contracts: filled,
            px_unit: px,
            fee_usd,
            latency_ms,
        }
    }

    /// Simulate a maker fill decision for one resting order, given the
    /// venue's current opposite touch and our limit price.
    ///
    /// Returns Some(limit_px) when filled this second.
    pub fn maker_fill(
        &self,
        side: Side,
        limit_unit: f64,
        opposite_touch_unit: f64,
    ) -> Option<f64> {
        if limit_unit <= 0.0 {
            return None;
        }
        // Marketable: the market came to us.
        let crossed = match side {
            Side::Buy => opposite_touch_unit > 0.0 && opposite_touch_unit <= limit_unit,
            Side::Sell => opposite_touch_unit > 0.0 && opposite_touch_unit >= limit_unit,
        };
        if crossed {
            return Some(limit_unit);
        }
        // Joined at the touch: probabilistic fill from queue turnover.
        let p = 0.16;
        if rand::thread_rng().gen::<f64>() < p {
            Some(limit_unit)
        } else {
            None
        }
    }

    /// Book a maker fill (no slippage, maker fees ≈ 0).
    pub fn book_maker(&self, venue: &str, side: Side, contracts: f64, px_unit: f64, contract_size: f64) -> f64 {
        let cash = match side {
            Side::Buy => -(contracts * px_unit * contract_size),
            Side::Sell => contracts * px_unit * contract_size,
        };
        self.adjust(venue, cash);
        cash
    }

    /// Underlying (hedge) fill at spot with 3 bp of slip, plus a taker-style
    /// fee of 5 bp (perp-style).
    pub async fn hedge(
        &self,
        venue: &str,
        side: Side,
        units: f64,
        spot: f64,
    ) -> (f64, f64) {
        let latency_ms = rand::thread_rng().gen_range(40..=140);
        tokio::time::sleep(std::time::Duration::from_millis(latency_ms)).await;
        self.hedge_sync(venue, side, units, spot)
    }

    /// Synchronous hedge fill (monitor path).
    pub fn hedge_sync(&self, venue: &str, side: Side, units: f64, spot: f64) -> (f64, f64) {
        let px = match side {
            Side::Buy => spot * 1.0003,
            Side::Sell => spot * 0.9997,
        };
        let fee = units * spot * 0.0005;
        let cash = match side {
            Side::Buy => -(units * px) - fee,
            Side::Sell => units * px - fee,
        };
        self.adjust(venue, cash);
        *self.fees_total.lock().unwrap() += fee;
        (px, fee)
    }
}
