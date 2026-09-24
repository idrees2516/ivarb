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

// ---------------------------------------------------------------------------
// Execution engine (advantage taker) wire types
// ---------------------------------------------------------------------------

/// Execution strategy tier. `time-aware`: the tier depends on the theta bucket
/// (time to expiry), the time-of-day liquidity regime, and signal freshness.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tier {
    /// Both legs immediately at the touch (executable edge ≥ gate, fresh quotes).
    Sniper,
    /// Marks diverged but the executable edge is thin: post passive orders on
    /// both venues to capture the spread as it converges, with a patience
    /// window scaled by the theta bucket.
    Convergence,
    /// Track and alert only; no orders.
    Monitor,
}

impl Tier {
    pub fn label(self) -> &'static str {
        match self {
            Tier::Sniper => "SNIPER",
            Tier::Convergence => "CONV",
            Tier::Monitor => "WATCH",
        }
    }
}

/// Brokerage mode of the advantage taker.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecMode {
    /// Realistic fill simulation (latency, slippage, partial fills, fees).
    Paper,
    /// Real orders on Deribit via private WS (requires API keys in env).
    Live,
}

/// Log severity for execution events.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecLevel {
    Info,
    Ok,
    Warn,
    Error,
}

/// One line of the execution log.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExecEvent {
    pub t_ms: i64,
    pub attempt: u64,
    /// Opportunity id (instrument+venues) the event belongs to.
    pub opp: Option<String>,
    pub level: ExecLevel,
    /// Short state tag, e.g. `preflight`, `leg_a`, `leg_b`, `filled`, `abort`.
    pub state: String,
    pub msg: String,
}

/// Live quote for one instrument on one venue, published by the engine for
/// instruments the executor is watching (open positions / active attempts).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WatchedLeg {
    pub venue: VenueId,
    /// Premiums per 1 underlying unit.
    pub px_bid_unit: Option<f64>,
    pub px_ask_unit: Option<f64>,
    pub px_mark_unit: Option<f64>,
    pub iv_bid_vp: Option<f64>,
    pub iv_ask_vp: Option<f64>,
    pub iv_mark_vp: Option<f64>,
    pub size_bid: Option<f64>,
    pub size_ask: Option<f64>,
    /// Underlying units per contract on this venue.
    pub contract_size: f64,
    pub recv_ms: i64,
}

/// Entry record of an open position.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PositionEntry {
    pub t_ms: i64,
    /// Entry spread between legs (vp) and round-trip entry fees (vp).
    pub spread_vp: f64,
    pub fee_vp: f64,
    pub net_vp: f64,
    /// Premiums actually paid/received per contract (after slippage).
    pub buy_px_usd: f64,
    pub sell_px_usd: f64,
    /// Signal net at decision time (for slippage-vs-signal accounting).
    pub signal_net_vp: f64,
    pub vega_vp_usd: f64,
    pub spot: f64,
    pub tier: Tier,
    /// Entry fees paid on both legs (USD).
    pub fees_usd: f64,
}

/// Live mark-to-market of an open position.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PositionMtm {
    pub t_ms: i64,
    /// Current spread between the two legs (vp; executable when available).
    pub spread_vp: f64,
    /// Unrealized P&L in USD (legs + hedge, after fees paid so far).
    pub unrealized_usd: f64,
    /// Cumulative theta decay absorbed since entry, USD.
    pub theta_burn_usd: f64,
    pub hedge_pnl_usd: f64,
    pub elapsed_s: f64,
}

/// A completed two-leg arbitrage position.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Position {
    pub id: u64,
    pub attempt: u64,
    pub opp: String,
    pub underlying: String,
    pub expiry: String,
    pub expiry_ms: i64,
    pub strike: f64,
    pub right: Right,
    pub buy_venue: VenueId,
    pub sell_venue: VenueId,
    /// Underlying units held on each leg.
    pub units: f64,
    /// Contracts per leg (venue-local sizing).
    pub contracts_buy: f64,
    pub contracts_sell: f64,
    pub contract_size_buy: f64,
    pub contract_size_sell: f64,
    pub entry: PositionEntry,
    pub mtm: PositionMtm,
    /// Exit plan (time-aware): convergence target, stop, theta budget, max hold.
    pub exit_take_vp: f64,
    pub exit_stop_vp: f64,
    pub theta_budget_vp: f64,
    pub max_hold_s: f64,
    /// Residual delta hedge state (spec step 06).
    pub hedge_units: f64,
    pub hedge_avg_px: f64,
    pub closing: bool,
}

impl Position {
    /// Canonical instrument id (`UND-yyyymmdd-strike-R`).
    pub fn inst_id(&self) -> String {
        format!(
            "{}-{}-{}-{}",
            self.underlying,
            self.expiry.replace('-', ""),
            fmt_strike_id(self.strike),
            self.right.letter()
        )
    }
}

/// Format a strike for ids: integers without a decimal tail.
pub fn fmt_strike_id(strike: f64) -> String {
    if (strike - strike.round()).abs() < 1e-9 {
        format!("{}", strike.round() as i64)
    } else {
        format!("{strike}")
    }
}

/// Why a position was closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExitReason {
    /// Spread converged — target captured.
    TakeProfit,
    /// Spread inverted beyond the stop — cut.
    Stop,
    /// Theta budget exhausted (time decay ate the expected edge).
    ThetaBudget,
    /// Maximum holding window for the bucket elapsed.
    MaxHold,
    /// Quotes went stale; defensive close.
    StaleQuotes,
    /// Manual close from the UI.
    Manual,
    /// Kill switch / flatten-all.
    Killed,
    /// Expiry approaching — forced wind-down.
    Expiry,
}

/// A closed round trip.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Trade {
    pub id: u64,
    pub opp: String,
    pub underlying: String,
    pub expiry: String,
    pub strike: f64,
    pub right: Right,
    pub buy_venue: VenueId,
    pub sell_venue: VenueId,
    pub units: f64,
    pub tier: Tier,
    /// Net vp actually captured after all fees and slippage.
    pub realized_vp: f64,
    pub realized_usd: f64,
    /// Realized vp minus signal net vp — negative means we gave back edge.
    pub slippage_vp: f64,
    pub fees_usd: f64,
    pub hedge_pnl_usd: f64,
    pub hold_s: f64,
    pub exit_reason: ExitReason,
    pub opened_ms: i64,
    pub closed_ms: i64,
}

/// Runtime risk state + limits of the advantage taker.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExecStats {
    pub t_ms: i64,
    pub armed: bool,
    pub mode: ExecMode,
    /// Kill reason when tripped.
    pub kill: Option<String>,
    pub attempts_total: u64,
    pub attempts_active: u32,
    pub positions_open: u32,
    /// Aggregate vega currently at risk (USD per vp).
    pub vega_used_usd: f64,
    /// Session P&L (USD): realized + unrealized.
    pub realized_usd: f64,
    pub unrealized_usd: f64,
    pub fees_usd: f64,
    pub fills_total: u32,
    pub single_leg_events: u32,
    /// Average capture ratio: realized vp / signal vp across closed trades.
    pub capture_ratio: f64,
    /// Trades closed this session.
    pub trades_closed: u32,
}

/// User-tunable settings (all env-overridable, UI-editable at runtime).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExecSettings {
    /// Minimum executable net edge (vp) the taker will fire on, before the
    /// time-aware bucket/regime multipliers.
    pub min_edge_vp: f64,
    /// Hard cap on units per attempt (underlying units).
    pub max_units_per_trade: f64,
    pub max_concurrent_attempts: u32,
    pub max_open_positions: u32,
    /// Aggregate vega cap (USD per vp).
    pub max_total_vega_usd: f64,
    /// Daily loss limit (USD) — trips the kill switch.
    pub daily_loss_limit_usd: f64,
    /// Leg-B timeout (spec's 10-second rule) in seconds.
    pub leg_timeout_s: f64,
    /// How much of displayed depth we are willing to take per leg.
    pub max_participation: f64,
    /// Fractional-Kelly scaling of position size.
    pub kelly_fraction: f64,
    pub sniper_enabled: bool,
    pub convergence_enabled: bool,
    /// Auto-exit when spread converges to this fraction of entry net.
    pub take_profit_frac: f64,
    /// Stop when spread inverts to −this multiple of entry net.
    pub stop_mult: f64,
    /// Close when cumulative theta burn exceeds this fraction of entry net.
    pub theta_budget_frac: f64,
    /// Residual-delta hedging on/off (spec step 06).
    pub hedge_enabled: bool,
    /// Hedge when |net delta × spot| exceeds this USD.
    pub hedge_threshold_usd: f64,
}

impl Default for ExecSettings {
    fn default() -> Self {
        Self {
            min_edge_vp: 0.5,
            max_units_per_trade: 2.0,
            max_concurrent_attempts: 2,
            max_open_positions: 6,
            max_total_vega_usd: 4_000.0,
            daily_loss_limit_usd: 1_500.0,
            leg_timeout_s: 10.0,
            max_participation: 0.35,
            kelly_fraction: 0.25,
            sniper_enabled: true,
            convergence_enabled: true,
            take_profit_frac: 0.25,
            stop_mult: 1.0,
            theta_budget_frac: 0.6,
            hedge_enabled: true,
            hedge_threshold_usd: 250.0,
        }
    }
}

/// Current time-aware regime context (computed server-side each push).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TimeRegime {
    /// UTC hour 0-23.
    pub utc_hour: u32,
    /// e.g. "EU/US overlap", "Asia quiet hours".
    pub label: String,
    /// Gate multiplier (higher = demand more edge in thin hours).
    pub gate_mult: f64,
    pub size_mult: f64,
    pub weekend: bool,
}

/// Full state of the advantage taker, pushed to the UI.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExecSnapshot {
    pub t_ms: i64,
    pub stats: ExecStats,
    pub settings: ExecSettings,
    pub regime: TimeRegime,
    pub positions: Vec<Position>,
    /// Most recent trades first, capped.
    pub trades: Vec<Trade>,
    /// Execution log tail, newest last.
    pub events: Vec<ExecEvent>,
    /// Paper balances per venue (USD-margined simulation).
    pub balances: Vec<(VenueId, f64)>,
    /// Live-gateway readiness (env gates, session, route completeness).
    #[serde(default)]
    pub live: LiveStatus,
}

/// Commands accepted by the advantage taker (REST → executor).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ExecCommand {
    Arm { armed: bool },
    Kill { reason: Option<String> },
    ResetKill,
    UpdateSettings { settings: ExecSettings },
    /// Manually take a specific opportunity (still risk-gated).
    Take { opp: String, units: Option<f64> },
    Close { position: u64 },
    FlattenAll,
    /// Validate the authenticated live path without placing strategy orders:
    /// checks env arming gates, connects, authenticates and reads the account
    /// summary. Safe in paper mode — it never orders.
    LivePreflight,
    /// Round-trip one minimum-size Deribit IOC order (buy then immediately
    /// sell back) to prove the live order path end-to-end. Live mode only;
    /// costs the spread + fees (typically a few USD on 0.1 BTC).
    LiveSmokeTest { instrument: Option<String> },
}

/// Live-gateway readiness, published in every executor snapshot.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct LiveStatus {
    /// IVARB_MODE=live requested.
    pub requested: bool,
    /// DERIBIT_API_KEY / DERIBIT_API_SECRET present.
    pub keys_present: bool,
    /// IVARB_LIVE_CONFIRM accepted (explicit arming acknowledgement).
    pub confirmed: bool,
    /// Authenticated Deribit session is connected.
    pub session_up: bool,
    /// WebSocket endpoint in use (prod or test.deribit.com).
    pub ws_url: String,
    /// Number of venues with a live order gateway (routes need ≥ 2).
    pub live_gateways: u32,
    /// True when at least two legs of a route could be executed live.
    pub route_ready: bool,
    /// Human-readable blockers, newest last.
    pub blockers: Vec<String>,
    /// Last preflight report lines ("name: OK detail" / "name: FAIL detail").
    pub last_preflight: Vec<String>,
}

pub type HistoryPt = (i64, f64);

// ---------------------------------------------------------------------------
// Hold-to-expiry analytics ("too thin to arb — what if I just keep it?")
// ---------------------------------------------------------------------------

/// Compact hold-to-expiry stats attached to every opportunity (closed-form
/// only; the full Monte-Carlo view is served on demand by `/api/hold`).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Default)]
pub struct HoldStats {
    /// EV per buy-venue contract of keeping the cheap leg to expiry, USD.
    pub ev_usd: f64,
    /// Theta today per contract, USD/day (negative).
    pub theta_day_usd: f64,
    /// Extrinsic per contract — the maximum quiet-case bleed, USD.
    pub extrinsic_usd: f64,
    /// P(terminal P&L > 0) under the consensus forecast, 0..1.
    pub p_profit: f64,
    /// forecast_vp − implied ask vp on the cheap leg.
    pub iv_edge_vp: f64,
    /// Days to expiry.
    pub dte: f64,
}

/// A near-miss pair: the premium spread exists but fees make the *arb*
/// unprofitable — surfaced so the hold-to-expiry lab can evaluate keeping the
/// cheap leg as an outright long-vol position instead.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HoldCandidate {
    /// Canonical instrument id (`UND-yyyymmdd-strike-R`).
    pub inst_id: String,
    pub underlying: String,
    pub expiry: String,
    pub strike: f64,
    pub right: Right,
    /// Venue with the cheap ask.
    pub venue: VenueId,
    /// Premium per 1 underlying unit on the cheap venue.
    pub ask_px_unit: f64,
    /// Cross-venue consensus mark IV (vp) — the default vol forecast.
    pub consensus_iv_vp: f64,
    /// IV implied by the ask (vp).
    pub implied_iv_vp: f64,
    /// Buy-venue taker fee rate (fraction of underlying).
    pub fee_rate: f64,
    pub spot: f64,
    pub t_years: f64,
    /// Underlying units per contract on the cheap venue.
    pub contract_size: f64,
    /// The arb view for contrast: spread vp, fee vp, net vp (negative/thin).
    pub arb_spread_vp: f64,
    pub arb_fee_vp: f64,
    pub arb_net_vp: f64,
    pub vega_vp_usd: f64,
    /// Closed-form hold stats under the consensus forecast.
    pub hold: HoldStats,
    pub updated_ms: i64,
}

// ---------------------------------------------------------------------------
// Multi-hop arbitrage routes
// ---------------------------------------------------------------------------

/// One executable step of a multi-hop route.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MultiHopLeg {
    /// buy | sell | convert (call↔put on the same venue via parity).
    pub action: String,
    pub venue: VenueId,
    /// Display instrument (venue-agnostic canonical name).
    pub instrument: String,
    /// Premium per unit that this leg pays (+) or receives (−), USD.
    pub px_unit: f64,
    /// Fee per unit, USD.
    pub fee_unit: f64,
    /// Executable size in underlying units.
    pub size_units: f64,
}

/// A detected cross-venue / parity-conversion route whose round-trip cash
/// flow is positive after every fee — found by negative-cycle search over the
/// (venue × right) graph per strike family.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MultiHopRoute {
    pub id: String,
    pub underlying: String,
    pub expiry: String,
    pub strike: f64,
    /// Route hops, e.g. buy call on okx → convert to put on deribit → sell put
    /// on bybit.
    pub legs: Vec<MultiHopLeg>,
    /// Total USD profit per 1 underlying unit after all fees.
    pub profit_unit_usd: f64,
    /// Profit in vol points on the family vega.
    pub profit_vp: f64,
    /// Executable size in underlying units (min across legs).
    pub size_units: f64,
    /// Total profit at the executable size, USD.
    pub total_usd: f64,
    /// Max quote age across the legs, ms.
    pub max_age_ms: i64,
    pub updated_ms: i64,
}

// ---------------------------------------------------------------------------
// Genetic-algorithm strategy optimizer
// ---------------------------------------------------------------------------

/// One gene of the evolved strategy genome.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GaGene {
    /// Stable gene key (e.g. `entry_gate_mult`).
    pub key: String,
    /// Human label.
    pub label: String,
    /// Current value.
    pub value: f64,
    /// Search bounds.
    pub lo: f64,
    pub hi: f64,
    /// Hand-tuned default (generation 0 baseline).
    pub default: f64,
}

/// Live status of the genetic optimizer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GaStatus {
    pub t_ms: i64,
    pub generation: u32,
    /// Whether evolution is running.
    pub running: bool,
    /// Whether the best genome is applied to live strategy parameters.
    pub applied: bool,
    /// Fitness of the best genome (risk-adjusted vp per 100 episodes).
    pub best_fitness: f64,
    /// Fitness of the hand-tuned default genome on the same tape.
    pub default_fitness: f64,
    /// Improvement of best over default, percent.
    pub improvement_pct: f64,
    /// Best-genome genes (ordered).
    pub genes: Vec<GaGene>,
    /// Best fitness per generation (recent tail).
    pub history: Vec<f64>,
    /// Episodes currently in the tape.
    pub tape_episodes: u32,
    /// Simulated trades in the last replay.
    pub replay_trades: u32,
    /// Replay P&L of the best genome, vp.
    pub replay_pnl_vp: f64,
    /// Replay max drawdown of the best genome, vp.
    pub replay_max_dd_vp: f64,
    /// Milliseconds per generation.
    pub gen_ms: u64,
}

/// Commands for the genetic optimizer (REST → GA thread).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum GaCommand {
    Pause,
    Resume,
    /// Apply / un-apply the best genome to live strategy parameters.
    SetApplied { applied: bool },
    /// Re-seed the population from defaults.
    Reset,
    SetGenMs { ms: u64 },
}

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
    /// EWMA of venue-timestamp vs local-arrival lag, ms (feed delay).
    #[serde(default)]
    pub lag_ms_ewma: f64,
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

    // ---- time-aware strategy fields (advantage taker) ----
    /// Strategy tier chosen for this signal right now.
    pub tier: Tier,
    /// Theta bucket label: "0-2d", "3-7d", "8-30d", "30d+".
    pub bucket: String,
    /// Days to expiry.
    pub dte: f64,
    /// Effective entry gate (vp) after bucket × regime × age multipliers.
    pub gate_vp: f64,
    /// Expected value per contract (vp) after fill-probability, single-leg
    /// risk, slippage and theta-burn modelling.
    pub ev_vp: f64,
    /// Theta decay per day in vol points (burn rate while holding).
    pub theta_vp_day: f64,
    /// Patience window for a passive (convergence) attempt, seconds.
    pub patience_s: f64,
    /// Maximum holding window for the bucket, seconds.
    pub hold_s: f64,
    /// Convergence hint from IV drift: true when the rich leg is fading
    /// toward the cheap leg (favorable for our position).
    pub conv_hint: bool,
    /// OU half-life of the spread (seconds) — expected convergence time.
    #[serde(default)]
    pub conv_eta_s: Option<f64>,
    /// Kalman-filtered spread level (vp) and innovation σ (vp) — the noise
    /// gate used by the adaptive take-profit exit.
    #[serde(default)]
    pub kalman_level_vp: Option<f64>,
    #[serde(default)]
    pub kalman_sigma_vp: Option<f64>,
    /// Hold-to-expiry view of the cheap leg (the "arb is too thin, keep the
    /// option" alternative), computed under the cross-venue consensus IV.
    #[serde(default)]
    pub hold: Option<HoldStats>,
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
    // ---- millisecond-pipeline telemetry ----
    /// Quote-arrival → signal-computed latency percentiles, µs.
    #[serde(default)]
    pub pipe_p50_us: f64,
    #[serde(default)]
    pub pipe_p99_us: f64,
    #[serde(default)]
    pub pipe_p999_us: f64,
    /// Signal → serialized WS frame latency percentiles, µs.
    #[serde(default)]
    pub wire_p50_us: f64,
    #[serde(default)]
    pub wire_p99_us: f64,
    /// Broadcast (delta push) cadence, ms.
    #[serde(default)]
    pub broadcast_ms: u64,
    /// Sweep (housekeeping) cadence, ms.
    #[serde(default)]
    pub sweep_ms: u64,
    /// Live multi-hop routes.
    #[serde(default)]
    pub multihop_routes: u32,
    /// Hold-to-expiry candidates currently tracked.
    #[serde(default)]
    pub holds_tracked: u32,
    /// GA generation of the applied genome (0 = defaults).
    #[serde(default)]
    pub ga_generation: u32,
    // ---- effective tuning (what the strategy layer is actually using) ----
    /// Entry gate in vol points (before GA / regime multipliers).
    #[serde(default)]
    pub min_net_vp: f64,
    /// Retirement gate (hysteresis), vp.
    #[serde(default)]
    pub exit_net_vp: f64,
    /// Hard cap on the dynamic moneyness band (fraction, e.g. 0.10 = ±10%).
    #[serde(default)]
    pub moneyness_pct: f64,
    /// Minimum tradable vega per contract, $/vp.
    #[serde(default)]
    pub min_vega_usd: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    pub ts_ms: i64,
    pub stats: EngineStats,
    pub venues: Vec<VenueStatus>,
    /// Consensus spot per underlying.
    pub spots: Vec<SpotQuote>,
    pub opportunities: Vec<Opportunity>,
    /// Near-miss pairs feeding the hold-to-expiry lab.
    #[serde(default)]
    pub holds: Vec<HoldCandidate>,
    /// Live multi-hop routes.
    #[serde(default)]
    pub multihop: Vec<MultiHopRoute>,
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
    /// Advantage-taker state (positions, trades, log, settings).
    Exec { exec: ExecSnapshot },
    /// Hold-to-expiry near-miss candidates.
    Holds { items: Vec<HoldCandidate> },
    /// Multi-hop arbitrage routes.
    Multihop { items: Vec<MultiHopRoute> },
    /// Genetic-optimizer status.
    Ga { status: GaStatus },
}

/// Client → server messages.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMsg {
    /// Ask the server to resend the full snapshot.
    Refresh,
}
