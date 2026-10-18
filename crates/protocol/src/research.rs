//! Research-system wire types (engine tap → research engine → REST → WASM
//! frontend). Pure serde so the crate still compiles to wasm32.
//!
//! The research system is a separate data plane from the arb scanner:
//! it consolidates option chains across venues, enriches them with
//! OI / volume / order-book depth via REST, computes candles + indicators +
//! IV analytics, and runs a tool-calling agent that grounds every answer in
//! that live consolidated data.

use serde::{Deserialize, Serialize};

use super::Right;

// ---------------------------------------------------------------------------
// Engine tap: a bounded digest of the live per-venue quote books.
// ---------------------------------------------------------------------------

/// One normalized quote row exported by the engine tap. Premiums are USD per
/// one unit of underlying exposure (same convention as the arb engine).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChainDigestRow {
    pub venue: String,
    pub und: String,
    /// Expiry as yyyymmdd.
    pub date: u32,
    pub strike: f64,
    pub right: Right,
    pub mark_px: Option<f64>,
    pub bid_px: Option<f64>,
    pub ask_px: Option<f64>,
    pub iv_mark_vp: Option<f64>,
    pub iv_bid_vp: Option<f64>,
    pub iv_ask_vp: Option<f64>,
    pub size_bid: Option<f64>,
    pub size_ask: Option<f64>,
    pub ts_ms: i64,
}

/// Bounded near-ATM snapshot of every venue quote book, refreshed by the
/// engine every few seconds and consumed by the research engine.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ChainDigest {
    pub ts_ms: i64,
    pub rows: Vec<ChainDigestRow>,
}

// ---------------------------------------------------------------------------
// Consolidated chain (REST `/api/research/chain`)
// ---------------------------------------------------------------------------

/// One venue's quote for a canonical contract.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChainVenueQuote {
    pub venue: String,
    pub mark_px: Option<f64>,
    pub bid_px: Option<f64>,
    pub ask_px: Option<f64>,
    pub iv_mark_vp: Option<f64>,
    pub iv_bid_vp: Option<f64>,
    pub iv_ask_vp: Option<f64>,
    pub size_bid: Option<f64>,
    pub size_ask: Option<f64>,
    /// Open interest in contracts (research REST enrichment).
    pub open_interest: Option<f64>,
    /// 24h volume in contracts / underlying units.
    pub volume_24h: Option<f64>,
    /// Age of this venue's tick-level quote (ms) when it comes from a
    /// 50 ms-class feed (Paradex order-book snapshots, Derive v3 tickers).
    #[serde(default)]
    pub tick_age_ms: Option<f64>,
}

/// Consolidated cross-venue view of one canonical contract.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChainRow {
    pub und: String,
    pub date: u32,
    pub strike: f64,
    pub right: Right,
    pub spot: f64,
    pub dte: i32,
    /// Median of venue mark IVs (vol points).
    pub iv_consensus_vp: Option<f64>,
    /// Widest cross-venue IV gap (vol points).
    pub iv_spread_vp: Option<f64>,
    /// Buy here: lowest ask.
    pub best_ask_venue: Option<String>,
    pub best_ask_px: Option<f64>,
    /// Sell here: highest bid.
    pub best_bid_venue: Option<String>,
    pub best_bid_px: Option<f64>,
    pub open_interest: Option<f64>,
    pub venues: Vec<ChainVenueQuote>,
}

/// `/api/research/chain` response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChainResponse {
    pub und: String,
    pub spot: f64,
    pub rows: Vec<ChainRow>,
    pub data_mode: String,
    pub ts_ms: i64,
}

// ---------------------------------------------------------------------------
// Pulse (REST `/api/research/pulse`) — the one-glance market state.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpotSummary {
    pub und: String,
    pub price: f64,
    pub change_24h_pct: Option<f64>,
}

/// ATM implied-vol summary for one underlying.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IvSummary {
    pub und: String,
    /// Composite ATM IV around the ~30d tenor (vol points).
    pub atm_iv_vp: Option<f64>,
    /// Where that IV sits vs its trailing window (0-100).
    pub iv_percentile: Option<f64>,
    /// What the percentile was computed against: "24h" (own history) or
    /// "cross-sectional" (all strikes today, while history warms up).
    pub iv_percentile_basis: String,
    /// Annualized realized vol from candles (vol points).
    pub realized_vol_vp: Option<f64>,
    /// (IV - RV) in vol points — the variance risk premium.
    pub vr_premium_vp: Option<f64>,
    /// Put/call ratio by open interest (front expiries).
    pub pc_ratio_oi: Option<f64>,
    /// Term slope: back-expiry ATM IV minus front-expiry ATM IV (vp).
    pub term_slope_vp: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResearchVenueStat {
    pub venue: String,
    pub is_sim: bool,
    pub online: bool,
    pub instruments: u32,
    /// Venue's composite ATM IV (vol points).
    pub atm_iv_vp: Option<f64>,
    /// Mean relative bid/ask spread across its chain (%).
    pub avg_spread_pct: Option<f64>,
    /// Sum of open interest marked in USD.
    pub oi_usd: Option<f64>,
    pub volume_usd_24h: Option<f64>,
    /// Research REST enrichment active for this venue.
    pub enrichment: bool,
    pub note: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResearchPulse {
    pub ts_ms: i64,
    pub data_mode: String,
    pub spots: Vec<SpotSummary>,
    pub venues_live: u32,
    pub instruments: u32,
    pub iv: Vec<IvSummary>,
    pub venue_stats: Vec<ResearchVenueStat>,
    pub agent_brain: String,
}

// ---------------------------------------------------------------------------
// Candles + indicators (REST `/api/research/candles`)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Candle {
    pub t_ms: i64,
    pub o: f64,
    pub h: f64,
    pub l: f64,
    pub c: f64,
    pub v: f64,
}

/// Parallel overlay series (None where undefined) + the latest readout.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndicatorSeries {
    pub ema20: Vec<Option<f64>>,
    pub ema50: Vec<Option<f64>>,
    pub bb_upper: Vec<Option<f64>>,
    pub bb_lower: Vec<Option<f64>>,
    pub rsi14: Vec<Option<f64>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndicatorReadout {
    pub rsi14: Option<f64>,
    /// ATR14 as % of spot.
    pub atr14_pct: Option<f64>,
    /// Annualized realized vol over ~20 bars (vol points).
    pub realized_vol_vp: Option<f64>,
    pub ema_trend: String,
    /// Where close sits inside the Bollinger band (-1 lower .. +1 upper).
    pub bb_pos: Option<f64>,
    pub change_24h_pct: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CandleSeries {
    pub und: String,
    pub interval: String,
    pub candles: Vec<Candle>,
    pub overlays: IndicatorSeries,
    pub readout: IndicatorReadout,
    pub data_mode: String,
}

// ---------------------------------------------------------------------------
// IV analytics (REST `/api/research/iv`)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IvPoint {
    pub dte: i32,
    pub iv_vp: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkewPoint {
    /// Absolute delta of the option (0.05 .. 0.95).
    pub delta: f64,
    /// IV at that delta (vol points); puts are negative-delta space mirrored.
    pub put_iv_vp: Option<f64>,
    pub call_iv_vp: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IvDivergence {
    pub und: String,
    pub date: u32,
    pub strike: f64,
    pub right: Right,
    pub dte: i32,
    pub low_venue: String,
    pub low_iv_vp: f64,
    pub high_venue: String,
    pub high_iv_vp: f64,
    pub spread_vp: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IvAnalytics {
    pub und: String,
    pub spot: f64,
    pub data_mode: String,
    /// ATM IV per expiry (composite across venues).
    pub term: Vec<IvPoint>,
    /// ATM IV per expiry per venue, for the comparison chart.
    pub term_by_venue: Vec<(String, Vec<IvPoint>)>,
    /// Skew smile of the front monthly (composite).
    pub skew: Vec<SkewPoint>,
    pub atm_iv_vp: Option<f64>,
    pub iv_percentile: Option<f64>,
    /// Richness note: "rich" | "fair" | "cheap" vs realized vol.
    pub vr_note: Option<String>,
    pub pc_ratio_oi: Option<f64>,
    pub top_divergences: Vec<IvDivergence>,
}

// ---------------------------------------------------------------------------
// Liquidity map (REST `/api/research/liquidity`)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LiquidityCell {
    pub date: u32,
    pub dte: i32,
    pub strike: f64,
    pub right: Right,
    /// USD notional resting within 2% of the touch (bid + ask sides).
    pub depth_usd: Option<f64>,
    /// Open interest marked in USD.
    pub oi_usd: Option<f64>,
    pub best_bid_venue: Option<String>,
    pub best_ask_venue: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LiquidityMap {
    pub und: String,
    pub spot: f64,
    pub data_mode: String,
    /// Distinct expiries (columns) and strikes (rows) present in `cells`.
    pub expiries: Vec<u32>,
    pub strikes: Vec<f64>,
    pub cells: Vec<LiquidityCell>,
    pub updated_ms: i64,
}

// ---------------------------------------------------------------------------
// Option builder (REST POST `/api/research/suggest`)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MarketView {
    Bullish,
    Bearish,
    NeutralRange,
    BigMove,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RiskAppetite {
    Conservative,
    Balanced,
    Aggressive,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SuggestLeg {
    pub action: LegAction,
    pub right: Right,
    pub strike: f64,
    pub date: u32,
    pub venue: String,
    /// USD per one unit of underlying (premium at the quoted side).
    pub px: f64,
    pub iv_vp: Option<f64>,
    pub delta: Option<f64>,
    /// Minimum tradable size on the venue (contracts).
    pub min_size: Option<f64>,
    /// Display helper, e.g. "BTC 2026-10-23 84000 C".
    pub label: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LegAction {
    Buy,
    Sell,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SuggestGreeks {
    pub delta: f64,
    pub gamma: f64,
    /// Theta per day, USD per unit.
    pub theta_day: f64,
    /// Vega per vol point, USD per unit.
    pub vega_vp: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoherenceBreakdown {
    /// 0-100: how aligned the structure is with live market state.
    pub score: u32,
    pub iv_regime: String,
    pub trend: String,
    pub skew: String,
    pub liquidity: String,
    /// Plain-language rationale, grounded in live numbers.
    pub rationale: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SuggestionCard {
    pub strategy: String,
    pub und: String,
    pub view: MarketView,
    pub horizon_days: u32,
    pub risk: RiskAppetite,
    pub legs: Vec<SuggestLeg>,
    /// "debit" | "credit" | "zero-cost".
    pub flow: String,
    /// Net premium per one unit of underlying (positive = debit).
    pub net_premium_usd: f64,
    pub max_profit_usd: Option<f64>,
    pub max_loss_usd: Option<f64>,
    pub breakevens: Vec<f64>,
    pub greeks: SuggestGreeks,
    pub coherence: CoherenceBreakdown,
    pub notes: Vec<String>,
    pub created_ms: i64,
    pub data_mode: String,
}

// ---------------------------------------------------------------------------
// Agent (REST POST `/api/research/agent`)
// ---------------------------------------------------------------------------

/// One tool invocation the agent performed while answering.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentToolCall {
    pub tool: String,
    /// Tool arguments as compact JSON text (keeps this crate serde-only).
    pub args: String,
    /// Human-readable one-line result summary shown in the trace.
    pub summary: String,
    pub ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentRun {
    pub query: String,
    /// "desk" (deterministic analyst) or "llm:<model>".
    pub brain: String,
    pub steps: Vec<AgentToolCall>,
    /// Markdown-lite answer (headings `##`, bold `**`, bullets `- `).
    pub answer: String,
    pub cards: Vec<SuggestionCard>,
    pub data_mode: String,
    pub ms: u64,
}

// ---------------------------------------------------------------------------
// Status (REST `/api/research/status`)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResearchStatus {
    pub digest_ms: i64,
    pub digest_rows: usize,
    pub chain_rows: usize,
    /// Venues whose REST enrichment poll last succeeded.
    pub enrichment_ok: Vec<String>,
    /// Underlyings with live candles.
    pub klines_ok: Vec<String>,
    /// Paradex universe size + quote capability (public WS, JWT-free).
    pub paradex_markets: u32,
    pub paradex_quotes: bool,
    /// Paradex public WS transport connected (quotes may lag by one cycle).
    #[serde(default)]
    pub paradex_ws: bool,
    pub paradex_note: String,
    pub llm_configured: bool,
    pub data_mode: String,
    /// Paradex 50 ms order-book tick feed connected.
    #[serde(default)]
    pub paradex_book: bool,
    #[serde(default)]
    pub paradex_book_subs: u32,
    /// Derive v3 ticker feed connected.
    #[serde(default)]
    pub derive_ws: bool,
    #[serde(default)]
    pub derive_subs: u32,
    /// Tick markets fresh right now across both tick feeds.
    #[serde(default)]
    pub tick_markets: u32,
    #[serde(default)]
    pub tick_note: String,
}

// ---------------------------------------------------------------------------
// Tick-level BBO (REST `/api/research/ticks`) — the execution-routing plane
// ---------------------------------------------------------------------------

/// One instrument's freshest tick on one venue.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TickQuote {
    pub venue: String,
    pub und: String,
    /// Expiry as yyyymmdd.
    pub date: u32,
    pub strike: f64,
    pub right: Right,
    pub bid: Option<f64>,
    pub ask: Option<f64>,
    pub bid_size: Option<f64>,
    pub ask_size: Option<f64>,
    pub mark: Option<f64>,
    pub iv_mark_vp: Option<f64>,
    /// (ask/bid - 1) × 100.
    pub spread_pct: Option<f64>,
    /// Venue sequence / publish id (Paradex book seq).
    pub seq: i64,
    /// Exchange feed timestamp, ms since epoch.
    pub feed_ts_ms: i64,
    /// Age vs the server's now, ms.
    pub age_ms: i64,
}

/// One tick feed's health.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TickFeedStatus {
    pub venue: String,
    pub connected: bool,
    pub subscriptions: u32,
    pub frames: u64,
    /// Markets with stored quotes (fresh within 5 s).
    pub markets: u32,
    pub last_recv_ms: i64,
    /// Age of the last frame, ms.
    pub age_ms: i64,
    pub note: String,
}

/// `/api/research/ticks` response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TicksResponse {
    pub und: String,
    pub spot: f64,
    pub rows: Vec<TickQuote>,
    pub feeds: Vec<TickFeedStatus>,
    pub data_mode: String,
    pub ts_ms: i64,
}
