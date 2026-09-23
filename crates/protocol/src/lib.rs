//! Shared wire types between the ivarb engine, the HTTP/WS server, and the
//! WASM frontend. This crate must stay dependency-light (serde only) so it can
//! be compiled to wasm32 for the Leptos frontend.

#![forbid(unsafe_code)]

use serde::{Deserialize, Serialize};

pub type VenueId = String;

/// Option type. Serialized as `"call"` / `"put"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Right {
    Call,
    Put,
}

impl Right {
    pub fn letter(self) -> &'static str {
        match self {
            Right::Call => "C",
            Right::Put => "P",
        }
    }
}

/// How the edge on an opportunity was computed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EdgeMode {
    /// Buy leg priced at the ask IV, sell leg at the bid IV (executable edge).
    Exec,
    /// Both legs priced at venue mark IV (not directly executable).
    Mark,
}

/// Connection state of a venue connector.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnState {
    Connecting,
    Live,
    /// Connected but producing little/no data (REST fallback, partial parse).
    Degraded,
    Disconnected,
    /// Permanently disabled (endpoint unreachable / unsupported shape).
    Disabled,
}

/// Overall feed mode shown in the UI banner.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FeedMode {
    Live,
    Sim,
    Mixed,
}

/// One point of the spread-history chart: `(ts_ms, net_vp)`.
pub type HistoryPt = (i64, f64);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VenueStatus {
    pub venue: VenueId,
    pub state: ConnState,
    pub is_sim: bool,
    /// Number of instruments currently tracked for this venue.
    pub instruments: u32,
    /// Messages per second (EWMA over recent window).
    pub msg_rate: f64,
    pub last_msg_ms: i64,
    /// Indicative taker fee as a fraction of the underlying notional.
    pub taker_fee_rate: f64,
    pub coin_margined: bool,
    /// Underlying units per contract (indicative where venue does not publish).
    pub contract_size: f64,
    pub note: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SpotQuote {
    pub underlying: String,
    pub venue: VenueId,
    pub price: f64,
    pub ts_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Opportunity {
    /// Stable id: `{UND}-{yyyymmdd}-{strike}-{C|P}:{buy}>{sell}`
    pub id: String,
    pub underlying: String,
    /// ISO date, e.g. "2026-09-26"
    pub expiry: String,
    pub expiry_ms: i64,
    pub strike: f64,
    pub right: Right,

    pub buy_venue: VenueId,
    pub sell_venue: VenueId,

    /// Vol points (1 vp = 1% IV).
    pub iv_buy_vp: f64,
    pub iv_sell_vp: f64,
    pub iv_spread_vp: f64,
    pub fee_vp: f64,
    pub net_vp: f64,
    pub edge_mode: EdgeMode,

    /// USD change per vol point, per contract on the BUY venue.
    pub vega_vp_usd: f64,
    /// net_vp × vega_vp_usd (per buy-venue contract).
    pub est_usd: f64,
    /// Underlying units per contract on the buy venue.
    pub contract_size: f64,
    /// Executable size in underlying units (min of both legs) if published.
    pub max_units: Option<f64>,
    /// est_usd scaled by max_units when known.
    pub total_usd: Option<f64>,

    /// Premium in USD per buy-venue contract at the executable/mark price.
    pub buy_px_usd: Option<f64>,
    pub sell_px_usd: Option<f64>,

    pub spot: f64,
    /// Years to expiry used in the vega computation.
    pub t_years: f64,
    pub size_buy: Option<f64>,
    pub size_sell: Option<f64>,

    pub first_seen_ms: i64,
    pub updated_ms: i64,
    /// Milliseconds since the buy/sell leg quote last changed (lead-lag hint).
    pub lead_buy_ms: i64,
    pub lead_sell_ms: i64,
    /// IV drift of each leg over roughly the last minute (vp), for the
    /// "which side is likely to revert" hint.
    pub drift_buy_vp_1m: f64,
    pub drift_sell_vp_1m: f64,

    pub stale: bool,
    /// Deltas only: opportunity has fallen below the exit threshold and was retired.
    pub retired: bool,
    /// Deltas only: the latest history point.
    pub hist_pt: Option<HistoryPt>,
    /// Snapshot only: full spread history (capped).
    pub history: Option<Vec<HistoryPt>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EngineStats {
    pub ts_ms: i64,
    pub uptime_s: u64,
    pub feed_mode: FeedMode,
    pub venues_live: u32,
    pub venues_total: u32,
    pub instruments_tracked: u32,
    pub opps_active: u32,
    /// EWMA of engine loop handling time in microseconds.
    pub engine_loop_us_ewma: f64,
    pub msgs_total: u64,
    pub msg_rate: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    pub ts_ms: i64,
    pub stats: EngineStats,
    pub venues: Vec<VenueStatus>,
    /// Consensus spot per underlying.
    pub spots: Vec<SpotQuote>,
    pub opportunities: Vec<Opportunity>,
}

/// Server → client messages, tagged JSON.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMsg {
    Snapshot { snapshot: Snapshot },
    /// Batched updates; each item replaces/creates by id, or removes if `retired`.
    Opps { items: Vec<Opportunity> },
    Venues { items: Vec<VenueStatus> },
    Stats { stats: EngineStats },
    Spots { items: Vec<SpotQuote> },
}

/// Client → server messages.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMsg {
    /// Ask the server to resend the full snapshot.
    Refresh,
}
