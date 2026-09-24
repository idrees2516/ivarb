//! Internal message bus types flowing from venue connectors into the engine.

use crate::key::InstKey;
use ivarb_protocol::{ConnState, SpotQuote, VenueId};

#[inline]
pub fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// A normalized quote for one instrument on one venue.
///
/// Premiums are normalized to **USD per one unit of underlying exposure**
/// (e.g. per 1 BTC), regardless of the venue's margin currency or contract
/// size, so the engine can price everything with one Black-Scholes instance.
#[derive(Debug, Clone)]
pub struct InstrTick {
    pub key: InstKey,
    /// Venue-provided mark/bid/ask IVs in vol points (62.34 = 62.34%).
    /// Any of these may be missing; the engine derives them from prices.
    pub iv_mark_vp: Option<f64>,
    pub iv_bid_vp: Option<f64>,
    pub iv_ask_vp: Option<f64>,
    /// Premiums in USD per 1 underlying unit (mark / best bid / best ask).
    pub px_mark_unit: Option<f64>,
    pub px_bid_unit: Option<f64>,
    pub px_ask_unit: Option<f64>,
    /// Quoted size at best bid / ask, in contracts.
    pub size_bid: Option<f64>,
    pub size_ask: Option<f64>,
    /// Venue-local underlying price, when carried by the tick.
    pub spot: Option<f64>,
    /// Venue-published contract size (underlying units), when available.
    pub contract_size: Option<f64>,
    pub venue_ts_ms: Option<i64>,
}

#[derive(Debug)]
pub enum VenueMsg {
    /// Register/replace the venue runtime (used by sim venues and late starters).
    Register { spec: crate::config::VenueSpec, is_sim: bool },
    Ticks { venue: VenueId, ticks: Vec<InstrTick> },
    Spot(SpotQuote),
    Status { venue: VenueId, state: ConnState, note: Option<String> },
    /// Number of instruments currently tracked by the connector.
    Tracked { venue: VenueId, instruments: u32 },
}
