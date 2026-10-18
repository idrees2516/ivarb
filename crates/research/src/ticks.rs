//! The tick-level BBO plane — exchange-fresh best-bid/offer for stricter
//! execution routing.
//!
//! Two feeds write into one shared store, keyed by canonical instrument:
//!
//! - **Paradex 50 ms book subscription** — `order_book.{market}.snapshot@15@50ms`
//!   on the public (unauthenticated, SBE) WebSocket. The exchange pushes a
//!   complete ≤15-level snapshot of the API book (RPI excluded) whenever it
//!   changes, throttled to at most one frame per 50 ms per market. Live BTC
//!   perp books tick at ~10 Hz; option books tick when quotes move.
//! - **Derive v3 ticker** — `ticker_slim.{instrument}.100` on
//!   `wss://api.derive.xyz/v3/ws`: best bid/ask + sizes, mark, index, the
//!   venue's IV surface (`option_pricing.i/bi/ai`) and 24h stats, published
//!   on the exchange's 100 ms interval.
//!
//! Consumers:
//! - the consolidation loop overlays tick-fresh BBO onto the digest rows for
//!   `paradex` / `derive` *before* best-bid/ask routing runs, so cross-venue
//!   route decisions use the freshest prices available anywhere in the system;
//! - `GET /api/research/ticks` serves a live per-venue tick table (the UI's
//!   Ticks tab) with true exchange timestamps and ages in milliseconds.

use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::Mutex;

use rustc_hash::FxHashMap;
use tokio::sync::watch;

use crate::now_ms;

/// A quote considered fresh enough to drive routing decisions.
pub const TICK_FRESH_MS: i64 = 5_000;

/// One instrument's freshest tick, as written by a feed task.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct TickBbo {
    pub bid: Option<f64>,
    pub ask: Option<f64>,
    pub bid_size: Option<f64>,
    pub ask_size: Option<f64>,
    pub mark: Option<f64>,
    pub index: Option<f64>,
    /// Venue-published IVs in vol points (Derive `option_pricing`).
    pub iv_mark_vp: Option<f64>,
    pub iv_bid_vp: Option<f64>,
    pub iv_ask_vp: Option<f64>,
    pub oi: Option<f64>,
    pub vol24_usd: Option<f64>,
    /// Venue sequence / publish id (Paradex seq, Derive publish id).
    pub seq: i64,
    /// Exchange feed timestamp, ms since epoch.
    pub feed_ts_ms: i64,
    /// Local receive time, ms since epoch.
    pub recv_ms: i64,
}

impl TickBbo {
    /// Age of this tick relative to `now` (local receive clock), ms.
    pub fn age_ms(&self, now: i64) -> i64 {
        (now - self.recv_ms).max(0)
    }
    pub fn fresh(&self, now: i64) -> bool {
        self.age_ms(now) <= TICK_FRESH_MS
    }
}

/// Per-venue feed state + quote map.
#[derive(Default)]
pub struct VenueTicks {
    pub quotes: FxHashMap<String, TickBbo>,
    /// Feed-level stats, written by the feed task.
    pub connected: bool,
    pub subscriptions: u32,
    pub frames: u64,
    pub last_recv_ms: i64,
    pub note: String,
}

impl VenueTicks {
    fn snapshot(&self, now: i64, max_age_ms: i64) -> Vec<(String, TickBbo)> {
        self.quotes
            .iter()
            .filter(|(_, t)| t.age_ms(now) <= max_age_ms)
            .map(|(k, t)| (k.clone(), *t))
            .collect()
    }
}

/// The shared tick store: venue → canonical instrument → latest tick.
pub struct TickPlane {
    venues: Mutex<FxHashMap<String, VenueTicks>>,
    /// Subscribed-market count per venue (what the feed task reconciled to).
    sub_count: AtomicU64,
    /// Total frames decoded across all venues.
    frames: AtomicU64,
    /// Last apply on any venue.
    last_apply_ms: AtomicI64,
    any_connected: AtomicBool,
}

impl Default for TickPlane {
    fn default() -> Self {
        Self {
            venues: Mutex::new(FxHashMap::default()),
            sub_count: AtomicU64::new(0),
            frames: AtomicU64::new(0),
            last_apply_ms: AtomicI64::new(0),
            any_connected: AtomicBool::new(false),
        }
    }
}

impl TickPlane {
    pub fn apply(&self, venue: &str, canon: &str, tick: TickBbo) {
        if let Ok(mut map) = self.venues.lock() {
            let v = map.entry(venue.to_string()).or_default();
            v.quotes.insert(canon.to_string(), tick);
            v.last_recv_ms = tick.recv_ms;
        }
        self.frames.fetch_add(1, Ordering::Relaxed);
        self.last_apply_ms.store(tick.recv_ms, Ordering::Relaxed);
    }

    /// Feed-task bookkeeping (connection state + reconciled subscriptions).
    pub fn feed_state(&self, venue: &str, connected: bool, subscriptions: u32, note: &str) {
        if let Ok(mut map) = self.venues.lock() {
            let v = map.entry(venue.to_string()).or_default();
            v.connected = connected;
            v.subscriptions = subscriptions;
            if !note.is_empty() {
                v.note = note.to_string();
            }
        }
        self.sub_count.store(
            self.venues
                .lock()
                .map(|m| m.values().map(|v| v.subscriptions as u64).sum())
                .unwrap_or(0),
            Ordering::Relaxed,
        );
        self.any_connected.store(
            self.venues
                .lock()
                .map(|m| m.values().any(|v| v.connected))
                .unwrap_or(false),
            Ordering::Relaxed,
        );
    }

    /// Per-frame liveness bookkeeping from a feed task (any frame, including
    /// frames for markets whose quotes are not stored — e.g. perps).
    pub fn note_frame(&self, venue: &str) {
        let now = now_ms();
        if let Ok(mut map) = self.venues.lock() {
            let v = map.entry(venue.to_string()).or_default();
            v.frames = v.frames.saturating_add(1);
            v.last_recv_ms = now;
        }
        self.frames.fetch_add(1, Ordering::Relaxed);
        self.last_apply_ms.store(now, Ordering::Relaxed);
    }

    /// Latest tick for one (venue, instrument).
    pub fn get(&self, venue: &str, canon: &str) -> Option<TickBbo> {
        self.venues
            .lock()
            .ok()?
            .get(venue)?
            .quotes
            .get(canon)
            .copied()
    }

    /// All fresh ticks for one venue, as (canon, tick) pairs.
    pub fn snapshot_venue(&self, venue: &str, max_age_ms: i64) -> Vec<(String, TickBbo)> {
        match self.venues.lock() {
            Ok(map) => map
                .get(venue)
                .map(|v| v.snapshot(now_ms(), max_age_ms))
                .unwrap_or_default(),
            Err(_) => Vec::new(),
        }
    }

    /// Feed health summary for status reporting.
    pub fn feed(&self, venue: &str) -> (bool, u32, u64, i64, usize, String) {
        // (connected, subscriptions, frames, last_recv_ms, markets, note)
        match self.venues.lock() {
            Ok(map) => map.get(venue).map(|v| {
                (
                    v.connected,
                    v.subscriptions,
                    v.frames,
                    v.last_recv_ms,
                    v.quotes.len(),
                    v.note.clone(),
                )
            }),
            Err(_) => None,
        }
        .unwrap_or((false, 0, 0, 0, 0, String::new()))
    }

    /// Total tick markets across venues (fresh within `max_age_ms`).
    pub fn market_count(&self, max_age_ms: i64) -> usize {
        let now = now_ms();
        match self.venues.lock() {
            Ok(map) => map
                .values()
                .map(|v| v.quotes.values().filter(|t| t.age_ms(now) <= max_age_ms).count())
                .sum(),
            Err(_) => 0,
        }
    }

    pub fn any_connected(&self) -> bool {
        self.any_connected.load(Ordering::Relaxed)
    }
}

/// A feed task's view of what it should be subscribed to.
///
/// The research loop recomputes the desired market set whenever the chain
/// digest moves and sends it through a `watch` channel; each feed task
/// reconciles its subscriptions incrementally (subscribe new, unsubscribe
/// gone) — no reconnect, no data gap.
pub type DesiredMarkets = watch::Receiver<Vec<String>>;

/// Diff two sorted-or-unsorted string sets into (to_add, to_remove).
pub fn reconcile_sets(current: &[String], desired: &[String]) -> (Vec<String>, Vec<String>) {
    let cur: std::collections::HashSet<&String> = current.iter().collect();
    let des: std::collections::HashSet<&String> = desired.iter().collect();
    let add = desired.iter().filter(|s| !cur.contains(*s)).cloned().collect();
    let rm = current.iter().filter(|s| !des.contains(*s)).cloned().collect();
    (add, rm)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plane_apply_snapshot_and_feed_state() {
        let p = TickPlane::default();
        let now = now_ms();
        let t = TickBbo {
            bid: Some(82154.9),
            ask: Some(82181.2),
            bid_size: Some(0.012),
            ask_size: Some(0.023),
            seq: 42,
            feed_ts_ms: now - 4,
            recv_ms: now - 2,
            ..Default::default()
        };
        p.apply("paradex", "BTC-20261019-82000-C", t);
        let got = p.get("paradex", "BTC-20261019-82000-C").unwrap();
        assert_eq!(got.bid, Some(82154.9));
        assert_eq!(got.seq, 42);
        assert_eq!(got.age_ms(now + 8), 10);
        assert!(p.get("derive", "BTC-20261019-82000-C").is_none());
        p.feed_state("paradex", true, 180, "50ms book live");
        let (conn, subs, _f, _lr, mkts, note) = p.feed("paradex");
        assert!(conn && subs == 180 && mkts == 1 && note == "50ms book live");
        // freshness windows are relative to the store's clock
        let snap = p.snapshot_venue("paradex", 10_000);
        assert_eq!(snap.len(), 1);
        let snap = p.snapshot_venue("paradex", 0);
        assert_eq!(snap.len(), 0);
    }

    #[test]
    fn reconcile_adds_and_removes() {
        let cur = vec!["a".into(), "b".into(), "c".into()];
        let des = vec!["b".into(), "c".into(), "d".into(), "e".into()];
        let (add, rm) = reconcile_sets(&cur, &des);
        assert_eq!(add, vec!["d".to_string(), "e".to_string()]);
        assert_eq!(rm, vec!["a".to_string()]);
        let (add, rm) = reconcile_sets(&cur, &cur);
        assert!(add.is_empty() && rm.is_empty());
    }
}
