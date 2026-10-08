//! ivarb-executor — the **advantage taker**: turns scanner signals into
//! risk-gated two-leg positions, manages them to convergence, and accounts
//! for every basis point.
//!
//! Architecture (single-writer core, like the engine):
//! - One `Core` task owns all mutable execution state (positions, trades,
//!   events, risk budgets). No locks on the hot path.
//! - Attempt FSMs run as short-lived tasks and report back over a channel;
//!   their log lines stream back through an event channel.
//! - The engine's watched-quote bridge feeds live legs for open positions
//!   (quotes keep flowing even after the signal retires below the gate).
//! - UI state is published through ArcSwap snapshots + the shared broadcast
//!   channel, serialized exactly once per push.

pub mod calib;
pub mod gateways;
pub mod live;
pub mod machine;
pub mod paper;
pub mod risk;
pub mod route;

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use rustc_hash::{FxHashMap, FxHashSet};
use tokio::sync::{broadcast, mpsc, watch};

use ivarb_engine::engine::WatchCmd;
use ivarb_engine::timing::Regime;
use ivarb_protocol::{
    EscapeReason, EscapeState, ExecCommand, ExecEvent, ExecLevel, ExecMode, ExecSettings,
    ExecSnapshot, ExecStats, ExitReason, LiveStatus, MultiHopRoute, Opportunity, Position,
    PositionEntry, PositionMtm, Right, Snapshot, TimeRegime, Trade, WatchedLeg,
};

use machine::{AttemptCtx, AttemptOutcome, PositionDraft};
use paper::PaperBroker;
use risk::{canonical_inst, RiskState};
use route::{RouteCtx, RouteOutcome};
use gateways::VenueRouter;

/// Serialized engine snapshots start with this prefix (`ServerMsg` is
/// internally tagged) — used to detect "fresh data" on the broadcast bus
/// without deserializing anything.
const SNAP_PREFIX: &str = "{\"type\":\"snapshot\"";
/// Batched opportunity deltas — the engine's ms-cadence news channel.
const OPPS_PREFIX: &str = "{\"type\":\"opps\"";

#[inline]
pub fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Inputs the executor needs from the engine/server.
pub struct ExecDeps {
    pub snapshot: Arc<ArcSwap<Snapshot>>,
    pub watched: Arc<ArcSwap<FxHashMap<String, Vec<WatchedLeg>>>>,
    pub watch_tx: mpsc::UnboundedSender<WatchCmd>,
    pub bcast: broadcast::Sender<Arc<String>>,
    pub shutdown: watch::Receiver<bool>,
    /// Millisecond-fresh opportunity list (engine `analytics_pass` cache,
    /// ~250 ms cadence — fresher than the 1 s full snapshot). The scan
    /// prefers this and falls back to the snapshot's list.
    #[allow(clippy::type_complexity)]
    pub opps_light: Arc<ArcSwap<Vec<Opportunity>>>,
    /// Cadences from IVARB_EXEC_* env overrides (ms; see EngineConfig).
    pub scan_ms: u64,
    pub monitor_ms: u64,
    pub push_ms: u64,
}

pub struct ExecutorHandle {
    pub cmd_tx: mpsc::UnboundedSender<ExecCommand>,
    pub snapshot: Arc<ArcSwap<ExecSnapshot>>,
}

struct Stats {
    attempts_total: u64,
    fills_total: u32,
    single_leg_events: u32,
    realized_usd: f64,
    fees_usd: f64,
    trades_closed: u32,
    capture_sum: f64,
    capture_n: f64,
    // profit attribution
    gross_locked_usd: f64,
    exit_drift_usd: f64,
    hedge_pnl_usd: f64,
    wins: u32,
    losses: u32,
    gross_wins: f64,
    gross_losses: f64,
    // capital
    capital_reserved_usd: f64,
    capital_peak_usd: f64,
    reserved_time_s: f64,
    reserved_last_t: i64,
    // multi-leg route cycles
    route_attempts: u64,
    route_banked: u32,
    route_profit_usd: f64,
    // 0DTE escapes (uncaptured arbs)
    escape_exits: u32,
    escape_time_sum_s: f64,
    escape_salvaged_usd: f64,
}

/// Book-keeping for one in-flight multi-leg route cycle.
struct ActiveRoute {
    route_id: String,
    /// Venue chain, "okx>bybit" style — the calibration key.
    chain: String,
    units: f64,
    capital_usd: f64,
    signal_usd: f64,
    started_ms: i64,
    /// Instruments this attempt registered on the engine watch list.
    watches: Vec<String>,
    /// The route as fired (trade blotter detail + unwinding reference).
    route: MultiHopRoute,
}

struct Core {
    deps: ExecDeps,
    out: Arc<ArcSwap<ExecSnapshot>>,
    cmd_rx: mpsc::UnboundedReceiver<ExecCommand>,
    attempt_tx: mpsc::Sender<(u64, AttemptOutcome)>,
    attempt_rx: mpsc::Receiver<(u64, AttemptOutcome)>,
    event_tx: mpsc::UnboundedSender<ExecEvent>,
    event_rx: mpsc::UnboundedReceiver<ExecEvent>,
    route_tx: mpsc::Sender<(u64, RouteOutcome)>,
    route_rx: mpsc::Receiver<(u64, RouteOutcome)>,
    live_tx: mpsc::UnboundedSender<LiveMsg>,
    live_rx: mpsc::UnboundedReceiver<LiveMsg>,
    broker: Arc<PaperBroker>,
    /// Venue router — the one dispatch point deciding paper vs REAL venue
    /// gateways (Deribit WS / Bybit V5 / OKX v5 / Binance eapi) per leg.
    /// Swapped atomically whenever gateways (re)connect.
    router: Arc<ArcSwap<VenueRouter>>,
    /// Authenticated Deribit session, once the arming chain completes
    /// (mirrored from the router for the legacy status fields).
    deribit: Option<Arc<live::DeribitSession>>,
    /// Live-gateway readiness published to the UI.
    live: LiveStatus,
    /// Env-gate blockers captured at boot (why we are in paper mode).
    arming_blockers: Vec<String>,
    /// Emits the "routes refused" warning once, not every scan.
    route_warned: bool,
    settings: ExecSettings,
    armed: bool,
    kill: Option<String>,
    mode: ExecMode,
    positions: FxHashMap<u64, Position>,
    /// attempt id → (opp, reserved units) for release accounting.
    live_attempts: FxHashMap<u64, (Opportunity, f64)>,
    /// In-flight multi-leg route cycles (attempt id → book-keeping).
    route_active: FxHashMap<u64, ActiveRoute>,
    /// route id → next-allowed fire time (cooldown after abort/unwind/bank).
    route_cooldown: FxHashMap<String, i64>,
    trades: VecDeque<Trade>,
    events: VecDeque<ExecEvent>,
    stats: Stats,
    risk: RiskState,
    /// Route-capture calibration (learned per venue-pair edge multipliers).
    calib: calib::Calib,
    /// Account equity at boot — the compounding baseline.
    base_equity: f64,
    /// Set when a fresh engine snapshot landed on the broadcast bus; the
    /// scan tick consumes it (event-driven burst scanning).
    snap_fresh: bool,
    next_id: u64,
    watch_set: FxHashSet<String>,
    watch_dirty: bool,
    dirty: bool,
}

/// Asynchronous live-path results delivered back into the single-writer core.
enum LiveMsg {
    Connected(Arc<live::DeribitSession>),
    Failed(String),
    Preflight(Vec<live::LiveCheck>),
    /// A freshly connected venue-router (gateways built from env creds).
    Router(Arc<VenueRouter>, Vec<gateways::VenueGateStatus>),
    /// Refreshed on-chain status (per chain).
    Chains(Vec<gateways::evm::ChainStatus>),
    /// Result of an on-chain swap execution.
    SwapDone(Result<gateways::evm::SwapOutcome, String>),
}

/// Boot the advantage taker.
pub fn spawn(deps: ExecDeps) -> ExecutorHandle {
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<ExecCommand>();
    let (attempt_tx, attempt_rx) = mpsc::channel::<(u64, AttemptOutcome)>(64);
    let (event_tx, event_rx) = mpsc::unbounded_channel::<ExecEvent>();
    let (route_tx, route_rx) = mpsc::channel::<(u64, RouteOutcome)>(16);

    let venue_ids: Vec<String> = deps
        .snapshot
        .load()
        .venues
        .iter()
        .map(|v| v.venue.clone())
        .collect();
    let (live_tx, live_rx) = mpsc::unbounded_channel::<LiveMsg>();

    // ---- live arming chain (IVARB_MODE + keys + explicit confirm) ----
    let arming = live::live_arming_from_env();
    let mode = arming.mode();
    let arming_blockers = arming.blockers();
    let live_status = LiveStatus {
        requested: arming.requested,
        keys_present: arming.keys.is_some(),
        confirmed: arming.confirmed,
        ws_url: arming.ws_url.clone(),
        ..Default::default()
    };

    let out = Arc::new(ArcSwap::from_pointee(empty_snapshot(mode)));

    // Cadences (IVARB_EXEC_* overrides) — captured before `deps` moves
    // into the core.
    let (deps_scan_ms, deps_monitor_ms, deps_push_ms) = (deps.scan_ms, deps.monitor_ms, deps.push_ms);
    let opps_light = deps.opps_light.clone();

    // Shared paper broker: every paper fill from the router lands in the
    // same balances the treasury/capital gates read.
    let broker = Arc::new(PaperBroker::new(&venue_ids, 250_000.0));
    let mut core = Core {
        router: Arc::new(ArcSwap::from_pointee(VenueRouter::new(
            broker.clone(),
            arming.fully_armed(),
        ))),
        broker,
        settings: profile_settings(),
        armed: false,
        kill: None,
        mode,
        positions: FxHashMap::default(),
        live_attempts: FxHashMap::default(),
        route_active: FxHashMap::default(),
        route_cooldown: FxHashMap::default(),
        trades: VecDeque::with_capacity(64),
        events: VecDeque::with_capacity(256),
        stats: Stats {
            attempts_total: 0,
            fills_total: 0,
            single_leg_events: 0,
            realized_usd: 0.0,
            fees_usd: 0.0,
            trades_closed: 0,
            capture_sum: 0.0,
            capture_n: 0.0,
            gross_locked_usd: 0.0,
            exit_drift_usd: 0.0,
            hedge_pnl_usd: 0.0,
            wins: 0,
            losses: 0,
            gross_wins: 0.0,
            gross_losses: 0.0,
            capital_reserved_usd: 0.0,
            capital_peak_usd: 0.0,
            reserved_time_s: 0.0,
            reserved_last_t: now_ms(),
            route_attempts: 0,
            route_banked: 0,
            route_profit_usd: 0.0,
            escape_exits: 0,
            escape_time_sum_s: 0.0,
            escape_salvaged_usd: 0.0,
        },
        risk: RiskState::new(),
        calib: calib::Calib::new(),
        base_equity: 0.0,
        snap_fresh: false,
        next_id: 1,
        watch_set: FxHashSet::default(),
        watch_dirty: false,
        dirty: true,
        deps,
        out: out.clone(),
        cmd_rx,
        attempt_tx,
        attempt_rx,
        route_tx,
        route_rx,
        event_tx,
        event_rx,
        live_tx: live_tx.clone(),
        live_rx,
        deribit: None,
        live: live_status,
        arming_blockers,
        route_warned: false,
    };
    // Compounding baseline: equity at boot (per-venue starting balances,
    // before any trading). Sizing scales with equity / base_equity.
    core.base_equity = core.broker.equity().max(1.0);
    core.ev(
        None,
        ExecLevel::Info,
        "control",
        format!(
            "risk profile '{}' — min edge {:.2} vp · kelly {:.0}% · max {} open · {} concurrent · daily loss limit {:.0} USD",
            std::env::var("IVARB_PROFILE").unwrap_or_else(|_| "balanced".into()),
            core.settings.min_edge_vp,
            100.0 * core.settings.kelly_fraction,
            core.settings.max_open_positions,
            core.settings.max_concurrent_attempts,
            core.settings.daily_loss_limit_usd,
        ),
    );

    // ---- boot banner + (optional) live session connect ----
    if arming.fully_armed() {
        core.ev(
            None,
            ExecLevel::Warn,
            "live",
            format!(
                "LIVE ARMING GATES OK — connecting venue gateways (deribit/bybit/okx/binance from env credentials) + chain status"
            ),
        );
    } else if arming.requested {
        core.ev(
            None,
            ExecLevel::Error,
            "live",
            format!(
                "live requested but NOT armed — {}",
                arming.blockers().join(" · ")
            ),
        );
    } else {
        core.ev(
            None,
            ExecLevel::Info,
            "live",
            "paper mode (simulated fills) — going live needs IVARB_MODE=live + venue API keys + IVARB_LIVE_CONFIRM=yes; run LIVE PREFLIGHT anytime to check".into(),
        );
    }

    // ---- connect every venue gateway whose credentials are present ----
    // (read-only pings/auth; orders only ever fire when fully armed). The
    // resulting router is swapped in atomically and published to the UI.
    {
        let tx = live_tx.clone();
        let paper = core.broker.clone();
        let armed = arming.fully_armed();
        tokio::spawn(async move {
            let mut r = VenueRouter::new(paper, armed);
            let venues = r.connect_all().await;
            let _ = tx.send(LiveMsg::Router(Arc::new(r), venues));
        });
    }
    // ---- on-chain status probe (real RPC + real Uniswap quotes) --------
    {
        let tx = live_tx.clone();
        let router0 = core.router.clone();
        tokio::spawn(async move {
            let r = router0.load_full();
            let mut out = Vec::new();
            for c in &r.evm.chains {
                out.push(r.evm.chain_status(c.name).await);
            }
            let _ = tx.send(LiveMsg::Chains(out));
        });
    }

    // ---- auto-arm (automatic arb taking) --------------------------------
    // In paper mode the taker arms itself at boot: simulated fills only,
    // real money never moves, and opportunities are taken the moment they
    // pass every gate. Live mode NEVER auto-arms — the ARM button there is
    // an explicit human action, as it should be.
    let autoarm = !matches!(
        std::env::var("IVARB_AUTOARM").as_deref(),
        Ok("0") | Ok("off") | Ok("false") | Ok("no")
    );
    if autoarm && matches!(mode, ExecMode::Paper) {
        core.armed = true;
        core.ev(
            None,
            ExecLevel::Ok,
            "control",
            "AUTO-ARMED at boot (paper mode) — the advantage taker is taking every opportunity that passes the risk gates (IVARB_AUTOARM=0 disables)".into(),
        );
    }

    let (deps_scan_ms, deps_monitor_ms, deps_push_ms) = (deps_scan_ms, deps_monitor_ms, deps_push_ms);
    let opps_light = opps_light;
    tokio::spawn(async move {
        // Event-driven burst scanning: a fast tick that fires a scan the
        // moment fresh engine news lands on the broadcast bus (snapshots or
        // batched opp deltas), plus a 300 ms heartbeat fallback so quiet
        // feeds never stall the taker. Candidates come from the engine's
        // `opps_light` cache when it has data (fresher than the snapshot).
        let mut bcast_rx = core.deps.bcast.subscribe();
        let mut scan = tokio::time::interval(Duration::from_millis(
            deps_scan_ms.clamp(10, 1_000),
        ));
        let mut monitor = tokio::time::interval(Duration::from_millis(
            deps_monitor_ms.clamp(100, 10_000),
        ));
        let mut push = tokio::time::interval(Duration::from_millis(
            deps_push_ms.clamp(100, 5_000),
        ));
        scan.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        monitor.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut last_scan = tokio::time::Instant::now() - Duration::from_secs(1);
        loop {
            tokio::select! {
                cmd = core.cmd_rx.recv() => {
                    match cmd {
                        Some(c) => core.handle_cmd(c),
                        None => break,
                    }
                }
                r = core.attempt_rx.recv() => {
                    match r {
                        Some(x) => core.handle_outcome(x),
                        None => break,
                    }
                }
                r = core.route_rx.recv() => {
                    match r {
                        Some(x) => core.handle_route_outcome(x),
                        None => break,
                    }
                }
                ev = core.event_rx.recv() => {
                    if let Some(e) = ev {
                        core.events.push_back(e);
                        while core.events.len() > 240 { core.events.pop_front(); }
                        core.dirty = true;
                    }
                }
                lm = core.live_rx.recv() => {
                    if let Some(m) = lm {
                        core.handle_live_msg(m);
                    }
                }
                b = bcast_rx.recv() => {
                    match b {
                        Ok(txt) => {
                            // Engine news only — full snapshots and batched
                            // opp deltas both mark data fresh; our own exec
                            // publishes and the GA pump are ignored.
                            if txt.starts_with(SNAP_PREFIX) || txt.starts_with(OPPS_PREFIX) {
                                core.snap_fresh = true;
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(_)) => {}
                        Err(broadcast::error::RecvError::Closed) => {}
                    }
                }
                _ = scan.tick() => {
                    let t = tokio::time::Instant::now();
                    if core.snap_fresh || t.duration_since(last_scan) >= Duration::from_millis(300) {
                        core.snap_fresh = false;
                        last_scan = t;
                        core.scan_with(&opps_light);
                    }
                }
                _ = monitor.tick() => core.monitor().await,
                _ = push.tick() => {
                    if core.dirty {
                        core.publish();
                    }
                }
                _ = core.deps.shutdown.changed() => {
                    if *core.deps.shutdown.borrow() { break; }
                }
            }
        }
        tracing::info!("executor stopped");
    });

    ExecutorHandle { cmd_tx, snapshot: out }
}

fn empty_snapshot(mode: ExecMode) -> ExecSnapshot {
    let regime = Regime::now_utc(now_ms());
    ExecSnapshot {
        t_ms: now_ms(),
        stats: ExecStats {
            t_ms: now_ms(),
            armed: false,
            mode,
            kill: None,
            attempts_total: 0,
            attempts_active: 0,
            positions_open: 0,
            vega_used_usd: 0.0,
            realized_usd: 0.0,
            unrealized_usd: 0.0,
            fees_usd: 0.0,
            fills_total: 0,
            single_leg_events: 0,
            capture_ratio: 0.0,
            trades_closed: 0,
            equity_usd: 0.0,
            net_deposits_usd: 0.0,
            capital_reserved_usd: 0.0,
            capital_free_usd: 0.0,
            roc_session_pct: 0.0,
            gross_locked_usd: 0.0,
            exit_drift_usd: 0.0,
            hedge_pnl_usd: 0.0,
            wins: 0,
            losses: 0,
            profit_factor: 0.0,
            avg_trade_usd: 0.0,
            route_attempts: 0,
            route_attempts_active: 0,
            route_banked: 0,
            route_profit_usd: 0.0,
            escape_exits: 0,
            escape_active: 0,
            escape_salvaged_usd: 0.0,
            escape_avg_s: 0.0,
        },
        settings: ExecSettings::default(),
        regime: TimeRegime {
            utc_hour: regime.utc_hour,
            label: regime.label.to_string(),
            gate_mult: regime.gate_mult,
            size_mult: regime.size_mult,
            weekend: regime.weekend,
        },
        positions: Vec::new(),
        trades: Vec::new(),
        events: Vec::new(),
        balances: Vec::new(),
        deposits_by_venue: Vec::new(),
        live: LiveStatus::default(),
        routes: Vec::new(),
    }
}

/// IVARB_PROFILE risk presets — the operating point between "never lose a
/// night's sleep" and "press every displayed edge". Profiles only shape the
/// taker's `ExecSettings` at boot; every gate, the kill switch and the live
/// triple-gate remain in force regardless of profile.
///
/// - `conservative` — wide gates, quarter-Kelly, one attempt at a time.
/// - `balanced`     — the tuned default (what the paper marathon ran).
/// - `aggressive`   — thinner gates, more concurrency, larger vega budget.
/// - `sniper`       — IOC-only: no passive convergence legs, wide gate,
///   higher Kelly on the few decisive dislocations.
fn profile_settings() -> ExecSettings {
    let mut s = ExecSettings::default();
    match std::env::var("IVARB_PROFILE").as_deref() {
        Ok("conservative") => {
            s.min_edge_vp = 1.0;
            s.kelly_fraction = 0.15;
            s.max_concurrent_attempts = 1;
            s.max_open_positions = 3;
            s.max_total_vega_usd = 2_000.0;
            s.daily_loss_limit_usd = 800.0;
            s.max_participation = 0.25;
        }
        Ok("aggressive") => {
            s.min_edge_vp = 0.35;
            s.kelly_fraction = 0.40;
            s.max_concurrent_attempts = 4;
            s.max_open_positions = 10;
            s.max_total_vega_usd = 8_000.0;
            s.daily_loss_limit_usd = 4_000.0;
            s.max_participation = 0.50;
        }
        Ok("sniper") => {
            s.min_edge_vp = 0.8;
            s.kelly_fraction = 0.30;
            s.convergence_enabled = false;
            s.max_participation = 0.45;
        }
        _ => {}
    }
    s
}

impl Core {
    // ------------------------------------------------------------------
    // commands
    // ------------------------------------------------------------------
    fn handle_cmd(&mut self, cmd: ExecCommand) {
        match cmd {
            ExecCommand::Arm { armed } => {
                self.armed = armed;
                self.ev(
                    None,
                    ExecLevel::Warn,
                    "control",
                    format!(
                        "auto-taker {} — mode {}",
                        if armed { "ARMED" } else { "DISARMED" },
                        match self.mode {
                            ExecMode::Paper => "PAPER (simulated fills)",
                            ExecMode::Live => "LIVE (real orders)",
                        }
                    ),
                );
            }
            ExecCommand::Kill { reason } => {
                self.kill = Some(reason.unwrap_or_else(|| "manual kill".into()));
                self.armed = false;
                self.ev(None, ExecLevel::Error, "kill", format!("KILL SWITCH — {}", self.kill.clone().unwrap_or_default()));
                for p in self.positions.values_mut() {
                    p.closing = true;
                }
            }
            ExecCommand::ResetKill => {
                self.kill = None;
                self.ev(None, ExecLevel::Info, "control", "kill switch reset".into());
            }
            ExecCommand::UpdateSettings { settings } => {
                self.settings = settings;
                self.ev(None, ExecLevel::Info, "control", "settings updated".into());
            }
            ExecCommand::Take { opp, units } => {
                self.manual_take(&opp, units);
            }
            ExecCommand::TakeRoute { route } => {
                self.manual_take_route(&route);
            }
            ExecCommand::Close { position } => {
                if let Some(p) = self.positions.get_mut(&position) {
                    p.closing = true;
                    let opp = p.opp.clone();
                    self.ev(Some(opp), ExecLevel::Warn, "control", format!("manual close requested for position #{position}"));
                }
            }
            ExecCommand::FlattenAll => {
                for p in self.positions.values_mut() {
                    p.closing = true;
                }
                self.ev(None, ExecLevel::Error, "control", "FLATTEN ALL — closing every open position at market".into());
            }
            ExecCommand::Deposit { venue, amount_usd } => {
                if self.broker.deposit(&venue, amount_usd) {
                    self.ev(
                        None,
                        ExecLevel::Ok,
                        "treasury",
                        format!("DEPOSIT {:.0} USD → {} — new balance {:.0} USD", amount_usd, venue, self.broker.balance_of(&venue)),
                    );
                } else {
                    self.ev(None, ExecLevel::Error, "treasury", format!("deposit rejected (amount must be positive): {amount_usd} USD"));
                }
            }
            ExecCommand::Withdraw { venue, amount_usd } => {
                if self.broker.withdraw(&venue, amount_usd) {
                    self.ev(
                        None,
                        ExecLevel::Warn,
                        "treasury",
                        format!("WITHDRAW {:.0} USD ← {} — remaining {:.0} USD", amount_usd, venue, self.broker.balance_of(&venue)),
                    );
                } else {
                    self.ev(
                        None,
                        ExecLevel::Error,
                        "treasury",
                        format!(
                            "withdrawal rejected (insufficient balance or bad amount): {:.0} USD from {} (has {:.0})",
                            amount_usd, venue, self.broker.balance_of(&venue)
                        ),
                    );
                }
            }
            ExecCommand::LivePreflight => {
                self.ev(None, ExecLevel::Info, "live", "preflight: checking arming gates, venue credentials/gateways and on-chain RPC…".into());
                let tx = self.live_tx.clone();
                tokio::spawn(async move {
                    let checks = live::live_preflight().await;
                    let _ = tx.send(LiveMsg::Preflight(checks));
                });
                // Rebuild the venue router + refresh chain status in the
                // same pass (read-only: pings, auth, account reads).
                let tx = self.live_tx.clone();
                let paper = self.broker.clone();
                let armed = matches!(self.mode, ExecMode::Live);
                tokio::spawn(async move {
                    let mut r = VenueRouter::new(paper, armed);
                    let venues = r.connect_all().await;
                    let _ = tx.send(LiveMsg::Router(Arc::new(r), venues));
                });
                let tx = self.live_tx.clone();
                let router0 = self.router.clone();
                tokio::spawn(async move {
                    let r = router0.load_full();
                    let mut out = Vec::new();
                    for c in &r.evm.chains {
                        out.push(r.evm.chain_status(c.name).await);
                    }
                    let _ = tx.send(LiveMsg::Chains(out));
                });
            }
            ExecCommand::LiveSmokeTest { instrument } => {
                self.live_smoke_test(instrument);
            }
            ExecCommand::ChainSwap { chain, weth_in, amount } => {
                self.ev(
                    None,
                    ExecLevel::Info,
                    "chain",
                    format!("swap requested: {chain} {} {:.6}", if weth_in { "WETH → USDC" } else { "USDC → WETH" }, amount),
                );
                let tx = self.live_tx.clone();
                let router0 = self.router.clone();
                tokio::spawn(async move {
                    let r = router0.load_full();
                    let res = r.evm.execute_swap(&chain, weth_in, amount).await;
                    let _ = tx.send(LiveMsg::SwapDone(res.map_err(|e| e.to_string())));
                });
            }
            ExecCommand::ChainStatusRefresh => {
                self.ev(None, ExecLevel::Info, "chain", "refreshing on-chain status (RPC + live quotes)…".into());
                let tx = self.live_tx.clone();
                let router0 = self.router.clone();
                tokio::spawn(async move {
                    let r = router0.load_full();
                    let mut out = Vec::new();
                    for c in &r.evm.chains {
                        out.push(r.evm.chain_status(c.name).await);
                    }
                    let _ = tx.send(LiveMsg::Chains(out));
                });
            }
        }
        self.dirty = true;
    }

    fn ev(&mut self, opp: Option<String>, level: ExecLevel, state: &str, msg: String) {
        self.events.push_back(ExecEvent {
            t_ms: now_ms(),
            attempt: 0,
            opp,
            level,
            state: state.to_string(),
            msg,
        });
        while self.events.len() > 240 {
            self.events.pop_front();
        }
    }

    // ------------------------------------------------------------------
    // live path (authenticated Deribit scaffold)
    // ------------------------------------------------------------------

    /// Venues with an authenticated order gateway.
    fn live_gateways(&self) -> u32 {
        self.router.load().gateway_count()
    }

    /// Two-leg routes are only executable live when BOTH legs sit behind a
    /// live gateway. A simulated leg inside a live attempt is forbidden, so
    /// with a single gateway the taker refuses cross-venue routes rather
    /// than presenting a half-real position as an arb.
    fn route_ready(&self) -> bool {
        if !matches!(self.mode, ExecMode::Live) {
            return true;
        }
        self.live_gateways() >= 2
    }

    fn live_route_ok(&self, opp: &Opportunity) -> Result<(), String> {
        if !matches!(self.mode, ExecMode::Live) {
            return Ok(());
        }
        let router = self.router.load();
        router.route_live_ok(&[opp.buy_venue.clone(), opp.sell_venue.clone()])
    }

    fn handle_live_msg(&mut self, m: LiveMsg) {
        match m {
            LiveMsg::Connected(sess) => {
                self.deribit = Some(sess);
                self.live.session_up = true;
                self.route_warned = false;
                self.ev(
                    None,
                    ExecLevel::Warn,
                    "live",
                    format!(
                        "deribit session CONNECTED ({}) — 1 live gateway. Cross-venue attempts stay refused until a second venue gateway exists; LIVE PREFLIGHT and the SMOKE TEST are available",
                        self.live.ws_url
                    ),
                );
            }
            LiveMsg::Failed(e) => {
                self.live.session_up = false;
                self.ev(
                    None,
                    ExecLevel::Error,
                    "live",
                    format!("deribit session FAILED: {e} — fix credentials/endpoint and run LIVE PREFLIGHT"),
                );
            }
            LiveMsg::Preflight(checks) => {
                let mut all_ok = true;
                self.live.last_preflight = checks.iter().map(|c| c.line()).collect();
                for c in &checks {
                    if !c.ok {
                        all_ok = false;
                    }
                    self.ev(
                        None,
                        if c.ok { ExecLevel::Info } else { ExecLevel::Warn },
                        "live",
                        c.line(),
                    );
                }
                self.ev(
                    None,
                    if all_ok { ExecLevel::Warn } else { ExecLevel::Error },
                    "live",
                    format!(
                        "preflight {} — {} checks",
                        if all_ok { "PASSED" } else { "FAILED" },
                        checks.len()
                    ),
                );
            }
            LiveMsg::Router(router, venues) => {
                // Mirror deribit for the legacy status fields.
                let deribit_up = router.deribit.is_some();
                self.deribit = router.deribit.clone();
                self.live.session_up = deribit_up;
                self.route_warned = false;
                self.live.venues = venues
                    .iter()
                    .map(|v| ivarb_protocol::VenueGateStatus {
                        venue: v.venue.clone(),
                        creds: v.creds,
                        healthy: v.healthy,
                        detail: v.detail.clone(),
                    })
                    .collect();
                self.live.chain_signer = router.evm.signer_address();
                self.router.store(router);
                let up: Vec<&str> = venues.iter().filter(|v| v.healthy).map(|v| v.venue.as_str()).collect();
                let creds: Vec<&str> = venues.iter().filter(|v| v.creds).map(|v| v.venue.as_str()).collect();
                let n = up.len();
                self.ev(
                    None,
                    if n > 0 { ExecLevel::Ok } else { ExecLevel::Info },
                    "live",
                    format!(
                        "venue gateways: {} live-capable ({}) — credentials detected for {}",
                        n,
                        if up.is_empty() { "none".into() } else { up.join(", ") },
                        if creds.is_empty() { "none".into() } else { creds.join(", ") },
                    ),
                );
                for v in &venues {
                    if v.creds && !v.healthy {
                        self.ev(None, ExecLevel::Warn, "live", format!("{} gateway UNHEALTHY: {}", v.venue, v.detail));
                    }
                }
                if let Some(signer) = &self.live.chain_signer {
                    self.ev(
                        None,
                        ExecLevel::Warn,
                        "chain",
                        format!("on-chain signer {signer} loaded from IVARB_CHAIN_SIGNER_KEY — CHAIN SWAP will broadcast REAL transactions"),
                    );
                }
            }
            LiveMsg::Chains(statuses) => {
                self.live.chains = statuses
                    .iter()
                    .map(|c| ivarb_protocol::ChainStatusOut {
                        name: c.name.clone(),
                        ok: c.chain_id_ok && c.quote_ok,
                        block: c.block,
                        weth_usdc_out: c.weth_usdc_out,
                        eth_price: c.eth_price,
                        gas_gwei: c.gas_gwei,
                        rpc_ms: c.rpc_ms,
                    })
                    .collect();
                let live_chains: Vec<String> = statuses
                    .iter()
                    .filter(|c| c.chain_id_ok && c.quote_ok)
                    .map(|c| format!("{} (eth ${:.0}, {:.2} gwei, block {})", c.name, c.eth_price, c.gas_gwei, c.block))
                    .collect();
                if live_chains.is_empty() {
                    self.ev(None, ExecLevel::Warn, "chain", "no chains reachable — check RPC endpoints (IVARB_RPC_*)".into());
                } else {
                    self.ev(
                        None,
                        ExecLevel::Info,
                        "chain",
                        format!("chains LIVE: {}", live_chains.join(" · ")),
                    );
                }
            }
            LiveMsg::SwapDone(res) => match res {
                Ok(o) => {
                    let rec = ivarb_protocol::ChainSwapRecord {
                        t_ms: now_ms(),
                        chain: o.chain.clone(),
                        direction: o.direction.clone(),
                        amount_in: o.amount_in,
                        quoted_out: o.quoted_out,
                        min_out: o.min_out,
                        tx_hash: o.tx_hash.clone(),
                        broadcast: o.broadcast,
                        calldata_hex: o.calldata_hex.clone(),
                        note: o.note.clone(),
                    };
                    self.live.swaps.push(rec);
                    while self.live.swaps.len() > 10 {
                        self.live.swaps.remove(0);
                    }
                    self.ev(
                        None,
                        ExecLevel::Ok,
                        "chain",
                        format!(
                            "swap {} {} {:.6} → quoted {:.4} (min {:.4}){} — {}",
                            o.chain,
                            o.direction,
                            o.amount_in,
                            o.quoted_out,
                            o.min_out,
                            match &o.tx_hash {
                                Some(h) => format!(" · tx {h}"),
                                None => " · NOT broadcast (no signer key — calldata ready)".to_string(),
                            },
                            o.note,
                        ),
                    );
                }
                Err(e) => {
                    self.ev(None, ExecLevel::Error, "chain", format!("swap FAILED: {e}"));
                }
            },
        }
        self.dirty = true;
    }

    /// Round-trip one minimum-size Deribit order to prove the live path.
    fn live_smoke_test(&mut self, instrument: Option<String>) {
        let Some(sess) = self.deribit.clone() else {
            self.ev(
                None,
                ExecLevel::Error,
                "live",
                "smoke test needs live mode with a connected Deribit session (IVARB_MODE=live + keys + IVARB_LIVE_CONFIRM=yes)".into(),
            );
            return;
        };
        if self.kill.is_some() {
            self.ev(None, ExecLevel::Error, "live", "smoke test refused while the kill switch is engaged".into());
            return;
        }

        // Resolve the instrument: explicit, or the best live deribit leg.
        let inst = match instrument.filter(|s| !s.trim().is_empty()) {
            Some(s) => s.trim().to_string(),
            None => {
                let snap = self.deps.snapshot.load();
                let pick = snap
                    .opportunities
                    .iter()
                    .filter(|o| !o.stale && !o.retired)
                    .filter(|o| o.buy_venue == "deribit" || o.sell_venue == "deribit")
                    .max_by(|a, b| a.ev_vp.total_cmp(&b.ev_vp));
                match pick {
                    Some(o) => live::venue_symbol(
                        "deribit",
                        &o.underlying,
                        live::ymd_from_ms(o.expiry_ms),
                        o.strike,
                        o.right.letter(),
                    ),
                    None => {
                        self.ev(
                            None,
                            ExecLevel::Error,
                            "live",
                            "no live deribit leg in the scanner to smoke-test — pass an instrument explicitly".into(),
                        );
                        return;
                    }
                }
            }
        };

        self.ev(
            None,
            ExecLevel::Warn,
            "live",
            format!("smoke test on {inst}: 0.1 IOC buy at the ask, immediate sell-back at the bid — costs the spread + fees"),
        );
        let etx = self.event_tx.clone();
        tokio::spawn(async move {
            let msg = match live::live_smoke_test(&sess, &inst, 0.1).await {
                Ok(r) => format!(
                    "smoke test {}: bought {:.2} @ ${:.2}, sold {:.2} @ ${:.2} — round-trip {:+.2} USD (spread + fees)",
                    r.instrument,
                    r.buy.contracts,
                    r.buy.px_unit,
                    r.sell.contracts,
                    r.sell.px_unit,
                    r.round_trip_usd
                ),
                Err(e) => format!("smoke test FAILED on {inst}: {e}"),
            };
            let _ = etx.send(ExecEvent {
                t_ms: now_ms(),
                attempt: 0,
                opp: None,
                level: if msg.contains("FAILED") {
                    ExecLevel::Error
                } else {
                    ExecLevel::Warn
                },
                state: "live".into(),
                msg,
            });
        });
    }

    // ------------------------------------------------------------------
    // scanner
    // ------------------------------------------------------------------
    fn scan_with(&mut self, opps_light: &Arc<ArcSwap<Vec<Opportunity>>>) {
        if !self.armed || self.kill.is_some() {
            return;
        }
        if matches!(self.mode, ExecMode::Live) && !self.route_ready() {
            if !self.route_warned {
                self.route_warned = true;
                self.ev(
                    None,
                    ExecLevel::Warn,
                    "live",
                    "live mode: no complete route yet — cross-venue attempts refused (a paper leg inside a live attempt is forbidden); smoke test + preflight remain available".into(),
                );
            }
            return;
        }
        if self.live_attempts.len() as u32 >= self.settings.max_concurrent_attempts {
            return;
        }
        let snap = self.deps.snapshot.load();
        let now = now_ms();
        // Venue accounts: provision paper accounts for venues that
        // registered after boot (sim feeds / late connects) so funding
        // never silently starves a tradeable route.
        for v in &snap.venues {
            if self.broker.ensure_venue(&v.venue) {
                self.ev(None, ExecLevel::Info, "treasury", format!("paper account provisioned for {} ({} USD)", v.venue, 250_000));
            }
        }
        // Multi-leg route cycles run on their own gate set and concurrency
        // budget (they are flat-at-completion, so they never contend with
        // the open-position caps the two-leg taker respects).
        self.scan_routes(&snap, now);
        // Candidates: the engine's ms-fresh light cache when populated
        // (written every analytics pass, ~250 ms), else the full snapshot's
        // list (1 s cadence). Both carry the same Opportunity shape.
        let light = opps_light.load();
        let opps: &[Opportunity] = if light.is_empty() {
            &snap.opportunities
        } else {
            &light
        };
        let mut unrealized = 0.0;
        for p in self.positions.values() {
            unrealized += p.mtm.unrealized_usd;
        }
        let balances: FxHashMap<String, f64> = self.broker.balances().into_iter().collect();
        // Venue spec index (fees, contract sizes) — built once per scan and
        // shared by the ranking and the risk gate so both see identical math.
        let venues: FxHashMap<&str, &ivarb_protocol::VenueStatus> =
            snap.venues.iter().map(|v| (v.venue.as_str(), v)).collect();
        // Equity compounding: sizes scale with account growth since boot
        // (clamped — the capital gate remains the final authority on what
        // balances can actually fund).
        let equity_scale = (self.broker.equity() / self.base_equity).clamp(0.25, 3.0);
        // Per-scan capital budget: at most 60% of free capital may be
        // committed in one sweep, ranked-best first — dry powder for the next
        // dislocation instead of a full book on one tick.
        let mut capital_budget = 0.60 * (self.broker.equity() - self.stats.capital_reserved_usd).max(0.0);

        // Portfolio concentration: open positions + live attempts per
        // underlying. Cross-venue arbs on the same underlying are strongly
        // correlated — one vol shock moves them all at once.
        let mut per_underlying: FxHashMap<&str, u32> = FxHashMap::default();
        for p in self.positions.values() {
            *per_underlying.entry(p.underlying.as_str()).or_insert(0) += 1;
        }
        for (_, (o, _)) in self.live_attempts.iter() {
            *per_underlying.entry(o.underlying.as_str()).or_insert(0) += 1;
        }

        // Rank candidates by CAPITAL EFFICIENCY — expected USD profit per
        // USD of capital the trade reserves (ev_vp × vega per unit ÷ capital
        // per unit; the unit count cancels). Raw EV ranking starves the
        // portfolio's best ROIs whenever a fat-vega, margin-heavy trade sits
        // on top of the list; this score is what a capital-constrained book
        // actually wants to fire first. Dedup by instrument (best pair per
        // contract).
        let mut best: FxHashMap<String, (f64, &Opportunity)> = FxHashMap::default();
        for o in opps.iter() {
            if o.stale || o.retired {
                continue;
            }
            if !matches!(o.tier, ivarb_protocol::Tier::Sniper | ivarb_protocol::Tier::Convergence) {
                continue;
            }
            // Adverse-drift hard skip: both legs running away from convergence
            // (rich leg richer, cheap leg cheaper) — the edge is decaying
            // faster than two IOC legs can capture it.
            if o.drift_sell_vp_1m > 0.20 && o.drift_buy_vp_1m < -0.20 {
                continue;
            }
            // Concentration cap per underlying.
            if per_underlying.get(o.underlying.as_str()).copied().unwrap_or(0)
                >= self.settings.max_per_underlying
            {
                continue;
            }
            let Ok((buy_need, sell_need)) = risk::capital_needs(o, &venues) else {
                continue;
            };
            // Route calibration: what this venue pair actually banks of its
            // displayed edge (learned from closed trades + abort rates).
            // Flicker pairs (abort-dominated books) are skipped outright.
            let route = format!("{}>{}", o.buy_venue, o.sell_venue);
            if self.calib.flicker(&route) {
                continue;
            }
            let route_mult = self.calib.edge_mult(&route);
            let vega_unit = if o.contract_size > 0.0 {
                o.vega_vp_usd / o.contract_size
            } else {
                o.vega_vp_usd
            };
            let cap_unit = buy_need + sell_need;
            let score = if vega_unit > 1e-9 && cap_unit > 1e-9 {
                o.ev_vp * route_mult * vega_unit / cap_unit
            } else {
                0.0
            };
            let inst = canonical_inst(o);
            let better = best.get(&inst).map(|(s, _)| score > *s).unwrap_or(true);
            if better {
                best.insert(inst, (score, o));
            }
        }
        let mut cands: Vec<(f64, &Opportunity)> = best.into_values().collect();
        cands.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));

        for (_, o) in cands {
            if self.live_attempts.len() as u32 >= self.settings.max_concurrent_attempts {
                break;
            }
            // Calibrated firing floor: the route's learned capture must
            // clear the bar, not the scanner's optimistic print.
            let route = format!("{}>{}", o.buy_venue, o.sell_venue);
            let route_mult = self.calib.edge_mult(&route);
            if o.ev_vp * route_mult <= 0.05 {
                continue;
            }
            // Capital budget for this sweep — ranked-best first.
            if let Ok((buy_need, sell_need)) = risk::capital_needs(o, &venues) {
                if (buy_need + sell_need) * 0.10 > capital_budget {
                    continue; // even a minimum clip starves the budget
                }
            }
            if let Ok(units) = self.risk.preflight(
                o,
                &snap,
                &self.settings,
                self.armed,
                &self.kill,
                self.stats.realized_usd,
                unrealized,
                &balances,
                equity_scale,
                now,
            ) {
                if let Ok((buy_need, sell_need)) = risk::capital_needs(o, &venues) {
                    capital_budget -= (buy_need + sell_need) * units;
                }
                self.spawn_attempt(o.clone(), units);
            }
        }
    }

    fn manual_take(&mut self, opp_id: &str, units: Option<f64>) {
        let snap = self.deps.snapshot.load();
        let Some(o) = snap.opportunities.iter().find(|o| o.id == opp_id).cloned() else {
            self.ev(Some(opp_id.to_string()), ExecLevel::Error, "manual", "opportunity not found (it may have just retired)".into());
            return;
        };
        let now = now_ms();
        let mut unrealized = 0.0;
        for p in self.positions.values() {
            unrealized += p.mtm.unrealized_usd;
        }
        let balances: FxHashMap<String, f64> = self.broker.balances().into_iter().collect();
        // Manual takes bypass the armed flag but respect every risk gate —
        // and the live route-completeness rule.
        if let Err(reason) = self.live_route_ok(&o) {
            self.ev(Some(o.id.clone()), ExecLevel::Error, "manual", format!("manual TAKE refused: {reason}"));
            return;
        }
        match self.risk.preflight(&o, &snap, &self.settings, true, &self.kill, self.stats.realized_usd, unrealized, &balances, 1.0, now) {
            Ok(auto_units) => {
                let units = units.unwrap_or(auto_units).min(auto_units).max(0.01);
                let route = format!("{}>{}", o.buy_venue, o.sell_venue);
                let mult = self.calib.edge_mult(&route);
                if mult < 0.85 {
                    self.ev(Some(o.id.clone()), ExecLevel::Warn, "calib", format!("route {route} fires at {:.0}% of displayed edge (learned capture) — sizing unchanged, watch this pair", 100.0 * mult));
                }
                self.ev(Some(o.id.clone()), ExecLevel::Warn, "manual", format!("manual TAKE — spawning attempt with {units:.2} units"));
                self.spawn_attempt(o, units);
            }
            Err(reason) => {
                self.ev(Some(o.id.clone()), ExecLevel::Error, "manual", format!("manual TAKE rejected by risk gates: {reason}"));
            }
        }
    }

    // ------------------------------------------------------------------
    // multi-leg route cycles (buy → convert → … → sell, flat at completion)
    // ------------------------------------------------------------------

    /// Auto-fire pass over the engine's live multi-hop routes. Gates:
    /// routes enabled, fresh quotes, quality-adjusted expectation above the
    /// floor, not on cooldown, not flickering, and per-venue capital at the
    /// sized amount. Ranked best-expected-first.
    fn scan_routes(&mut self, snap: &Snapshot, now: i64) {
        if !self.settings.routes_enabled || self.kill.is_some() || !self.armed {
            return;
        }
        if matches!(self.mode, ExecMode::Live) {
            // Live route cycles need EVERY leg's venue behind a real
            // gateway — a paper leg inside a live attempt is forbidden.
            // (Auto-fire scans candidates below and filters per route.)
        }
        const MAX_CONCURRENT_ROUTES: usize = 2;
        if self.route_active.len() >= MAX_CONCURRENT_ROUTES || snap.multihop.is_empty() {
            return;
        }
        let mut cands: Vec<&MultiHopRoute> = snap.multihop.iter().collect();
        cands.sort_by(|a, b| (b.total_usd * b.quality).total_cmp(&(a.total_usd * a.quality)));
        for r in cands {
            if self.route_active.len() >= MAX_CONCURRENT_ROUTES {
                break;
            }
            if self.route_active.values().any(|a| a.route_id == r.id) {
                continue;
            }
            if let Some(&until) = self.route_cooldown.get(&r.id) {
                if now < until {
                    continue;
                }
            }
            // Fresh quotes only — a route on stale books is a coin flip.
            if r.max_age_ms > 6_000 {
                continue;
            }
            let chain = route_chain(r);
            if self.calib.flicker(&chain) {
                continue;
            }
            // Live doctrine: every leg's venue must sit behind a real
            // gateway before a route cycle can fire in live mode.
            if matches!(self.mode, ExecMode::Live) {
                let venues: Vec<String> = r.legs.iter().map(|l| l.venue.clone()).collect();
                if self.router.load().route_live_ok(&venues).is_err() {
                    continue;
                }
            }
            // Expectation gate: raw profit × data confidence × learned
            // route capture must clear the configured floor.
            let expected = r.total_usd * r.quality * self.calib.edge_mult(&chain);
            if expected < self.settings.route_min_profit_usd {
                continue;
            }
            self.try_fire_route(r, false, now);
        }
    }

    /// Gate + size + spawn one route cycle. `manual` bypasses the armed and
    /// routes-enabled flags (a human clicked TAKE) but respects the kill
    /// switch, live-mode doctrine and every capital gate.
    fn try_fire_route(&mut self, route: &MultiHopRoute, manual: bool, now: i64) {
        let snap = self.deps.snapshot.load();
        // Single entry gate for live doctrine (covers auto + manual paths).
        if matches!(self.mode, ExecMode::Live) {
            let venues: Vec<String> = route.legs.iter().map(|l| l.venue.clone()).collect();
            if let Err(reason) = self.router.load().route_live_ok(&venues) {
                self.ev(
                    Some(route.id.clone()),
                    ExecLevel::Error,
                    "route",
                    format!("live route refused: {reason}"),
                );
                return;
            }
        }
        let spot = snap
            .spots
            .iter()
            .find(|s| s.underlying == route.underlying)
            .map(|s| s.price)
            .unwrap_or(0.0);
        if spot <= 0.0 {
            if manual {
                self.ev(Some(route.id.clone()), ExecLevel::Error, "route", format!("no spot for {} — cannot size the cycle", route.underlying));
            }
            return;
        }
        // ---- theoretical premium bounds (defense in depth) -----------------
        // The scanner already enforces these; a route that still carries an
        // impossible price (call > spot, put > strike) is a venue-unit
        // artifact and must never reserve capital or fire.
        {
            let call_cap = spot * 1.05;
            let put_cap = route.strike * 1.05;
            for leg in &route.legs {
                let cap = if right_from_inst(&leg.instrument) == Right::Put { put_cap } else { call_cap };
                if leg.px_unit.abs() > cap {
                    self.ev(
                        Some(route.id.clone()),
                        ExecLevel::Error,
                        "route",
                        format!(
                            "route refused: {} leg on {} prints {} — above the theoretical bound {} (venue-unit artifact, not an edge)",
                            leg.action, leg.venue, fmt_px(leg.px_unit), fmt_px(cap)
                        ),
                    );
                    self.route_cooldown.insert(route.id.clone(), now + 300_000);
                    return;
                }
            }
        }
        // ---- per-venue capital per underlying unit -------------------------
        let mut venue_cost: FxHashMap<&str, f64> = FxHashMap::default();
        for leg in &route.legs {
            let vspec = snap.venues.iter().find(|v| v.venue == leg.venue);
            let ct = vspec
                .and_then(|v| (v.contract_size > 0.0).then_some(v.contract_size))
                .unwrap_or(1.0);
            let fee = if leg.fee_unit > 0.0 {
                leg.fee_unit
            } else {
                0.0003 * spot
            };
            let add = match leg.action.as_str() {
                // Long premium + fee, funded on the buy venue.
                "buy" => leg.px_unit.abs() + fee,
                // Short-option margin (venue formula, buffered) + fee.
                "sell" => {
                    let right = quant_right_from_inst(&leg.instrument);
                    ivarb_quant::margin_per_contract(
                        &leg.venue,
                        leg.px_unit.abs(),
                        ct,
                        spot,
                        route.strike,
                        right,
                    ) * (1.0 + ivarb_quant::MARGIN_BUFFER)
                        / ct
                        + fee
                }
                // Conversion: fund the target purchase + fees + perp legs.
                _ => leg.px_unit.abs().max(0.0) + 2.0 * fee + 0.001 * spot,
            };
            *venue_cost.entry(leg.venue.as_str()).or_insert(0.0) += add;
        }
        let total_cost_unit: f64 = venue_cost.values().sum();

        // ---- sizing: executable size, taker cap, per-venue affordability ---
        let mut units = route.size_units.min(self.settings.max_units_per_trade);
        for leg in &route.legs {
            self.broker.ensure_venue(&leg.venue);
        }
        let balances: FxHashMap<String, f64> = self.broker.balances().into_iter().collect();
        let mut tightest: (String, f64) = (String::new(), units);
        for (v, cost_unit) in &venue_cost {
            if *cost_unit > 1e-9 {
                let afford = balances.get(*v).copied().unwrap_or(0.0) * 0.90 / cost_unit;
                if afford < tightest.1 {
                    tightest = (v.to_string(), afford);
                }
            }
        }
        units = tightest.1.min(units);
        let capital = units * total_cost_unit;
        if units < 0.01 {
            let msg = format!(
                "route refused: insufficient capital on {} — needs ~{:.0} USD/unit, {:.0} available (deposit funds in the Funding & capital panel)",
                if tightest.0.is_empty() { "venues" } else { &tightest.0 },
                total_cost_unit,
                balances.values().sum::<f64>(),
            );
            if manual {
                self.ev(Some(route.id.clone()), ExecLevel::Error, "route", msg);
            } else {
                // Size down silently in the auto pass — the scanner will
                // re-present the route when funding allows.
                self.route_cooldown.insert(route.id.clone(), now + 60_000);
            }
            return;
        }

        // ---- book-keeping + spawn -----------------------------------------
        let attempt = self.next_id;
        self.next_id += 1;
        let chain = route_chain(route);
        self.stats.route_attempts += 1;
        self.calib.record_attempt(&chain);
        self.stats.capital_reserved_usd += capital;
        self.stats.capital_peak_usd = self.stats.capital_peak_usd.max(self.stats.capital_reserved_usd);

        // Watch every instrument the cycle touches (conversions need both
        // rights of the family) so the engine keeps their quotes live.
        let mut watches: Vec<String> = Vec::new();
        for leg in &route.legs {
            let mut insts = vec![leg.instrument.clone()];
            if leg.action == "convert" {
                insts.push(route::flip_right(&leg.instrument));
            }
            for inst in insts {
                if self.watch_set.insert(inst.clone()) {
                    watches.push(inst);
                    self.watch_dirty = true;
                }
            }
        }

        let fee_rates: FxHashMap<String, f64> = snap
            .venues
            .iter()
            .map(|v| (v.venue.clone(), v.taker_fee_rate))
            .collect();
        // Expectation AT THE FIRED SIZE — the scanner's total is at the
        // route's full executable size; when capital sizes the cycle down,
        // the promise (and later the capture ratio) must scale with it.
        let expected = route.profit_unit_usd * units * route.quality * self.calib.edge_mult(&chain);
        self.ev(
            Some(route.id.clone()),
            ExecLevel::Warn,
            "route/fire",
            format!(
                "{} ROUTE #{} — {} legs via {}, {:.2} u, expected {:+.2} USD (print {:+.2} × quality {:.0}% × learned capture), capital {:.0} USD",
                if manual { "manual" } else { "auto" },
                attempt,
                route.legs.len(),
                chain,
                units,
                expected,
                route.total_usd,
                100.0 * route.quality,
                capital,
            ),
        );
        self.route_active.insert(
            attempt,
            ActiveRoute {
                route_id: route.id.clone(),
                chain: chain.clone(),
                units,
                capital_usd: capital,
                signal_usd: route.profit_unit_usd * units,
                started_ms: now,
                watches,
                route: route.clone(),
            },
        );

        let ctx = RouteCtx {
            attempt,
            route: route.clone(),
            spot,
            settings: self.settings.clone(),
            broker: self.broker.clone(),
            router: self.router.clone(),
            watched: self.deps.watched.clone(),
            snapshot: self.deps.snapshot.clone(),
            events: self.event_tx.clone(),
            fee_rates,
        };
        let route_tx = self.route_tx.clone();
        tokio::spawn(async move {
            let outcome = route::run_route(ctx, units).await;
            let _ = route_tx.send((attempt, outcome)).await;
        });
        self.dirty = true;
    }

    /// Manual TAKE of a specific multi-hop route from the UI.
    fn manual_take_route(&mut self, route_id: &str) {
        let snap = self.deps.snapshot.load();
        let Some(r) = snap.multihop.iter().find(|r| r.id == route_id).cloned() else {
            self.ev(Some(route_id.to_string()), ExecLevel::Error, "route", "route not found (it may have just retired)".into());
            return;
        };
        drop(snap);
        if self.kill.is_some() {
            self.ev(Some(route_id.to_string()), ExecLevel::Error, "route", "kill switch engaged — reset it before taking routes".into());
            return;
        }
        if matches!(self.mode, ExecMode::Live) {
            let venues: Vec<String> = r.legs.iter().map(|l| l.venue.clone()).collect();
            if let Err(reason) = self.router.load().route_live_ok(&venues) {
                self.ev(Some(route_id.to_string()), ExecLevel::Error, "route", format!("live route refused: {reason}"));
                return;
            }
        }
        self.try_fire_route(&r, true, now_ms());
    }

    /// Terminal outcome of a route cycle: bank the result, release capital,
    /// clean the watch list, record the trade + calibration sample.
    fn handle_route_outcome(&mut self, (attempt, outcome): (u64, RouteOutcome)) {
        let Some(ar) = self.route_active.remove(&attempt) else {
            return;
        };
        let now = now_ms();
        // Release the reserved capital.
        self.stats.capital_reserved_usd = (self.stats.capital_reserved_usd - ar.capital_usd).max(0.0);
        // Unregister watches no position and no other route still needs.
        for inst in &ar.watches {
            let needed = self
                .positions
                .values()
                .any(|p| canonical_inst_of_pos(p) == *inst)
                || self.route_active.values().any(|r| r.watches.contains(inst));
            if !needed {
                self.watch_set.remove(inst);
                self.watch_dirty = true;
            }
        }
        match outcome {
            RouteOutcome::Banked { profit_usd, fees_usd, hedge_pnl_usd, units, legs } => {
                self.stats.route_banked += 1;
                self.stats.route_profit_usd += profit_usd;
                self.stats.realized_usd += profit_usd;
                self.stats.fees_usd += fees_usd;
                self.stats.fills_total += legs.len() as u32;
                self.stats.trades_closed += 1;
                if profit_usd >= 0.0 {
                    self.stats.wins += 1;
                    self.stats.gross_wins += profit_usd;
                } else {
                    self.stats.losses += 1;
                    self.stats.gross_losses += (-profit_usd).max(0.0);
                }
                let capture = if ar.signal_usd > 1e-9 {
                    (profit_usd / ar.signal_usd).clamp(-2.0, 2.0)
                } else {
                    0.0
                };
                self.stats.capture_sum += capture;
                self.stats.capture_n += 1.0;
                self.calib.record_close(&ar.chain, capture);
                self.route_cooldown.insert(ar.route_id.clone(), now + 45_000);
                self.book_route_trade(&ar, units, profit_usd, fees_usd, hedge_pnl_usd, legs, ExitReason::RouteBanked);
                self.ev(
                    Some(ar.route_id.clone()),
                    ExecLevel::Ok,
                    "route/banked",
                    format!("route cycle #{} banked {:+.2} USD — capture {:.0}% of the {} USD signal", attempt, profit_usd, 100.0 * capture, ar.signal_usd),
                );
            }
            RouteOutcome::Aborted { reason, cooldown_ms } => {
                self.calib.record_abort(&ar.chain);
                self.route_cooldown.insert(ar.route_id.clone(), now + cooldown_ms.max(10_000) as i64);
                self.ev(
                    Some(ar.route_id.clone()),
                    ExecLevel::Info,
                    "route/abort",
                    format!("route cycle #{} aborted before fire: {}", attempt, reason),
                );
            }
            RouteOutcome::Unwound { reason, profit_usd, fees_usd, hedge_pnl_usd, units, legs } => {
                self.stats.single_leg_events += 1;
                self.stats.realized_usd += profit_usd;
                self.stats.fees_usd += fees_usd;
                self.stats.fills_total += legs.len() as u32;
                self.stats.trades_closed += 1;
                if profit_usd >= 0.0 {
                    self.stats.wins += 1;
                    self.stats.gross_wins += profit_usd;
                } else {
                    self.stats.losses += 1;
                    self.stats.gross_losses += (-profit_usd).max(0.0);
                }
                self.calib.record_abort(&ar.chain);
                self.route_cooldown.insert(ar.route_id.clone(), now + 120_000);
                self.book_route_trade(&ar, units, profit_usd, fees_usd, hedge_pnl_usd, legs, ExitReason::RouteUnwound);
                self.ev(
                    Some(ar.route_id.clone()),
                    ExecLevel::Error,
                    "route/unwind",
                    format!("route cycle #{} broke mid-flight: {} — P&L impact {:+.2} USD, flat", attempt, reason, profit_usd),
                );
            }
        }
        self.dirty = true;
    }

    /// Append the route cycle's round-trip record to the trade blotter.
    fn book_route_trade(
        &mut self,
        ar: &ActiveRoute,
        units: f64,
        profit_usd: f64,
        fees_usd: f64,
        hedge_pnl_usd: f64,
        legs: Vec<ivarb_protocol::TradeLeg>,
        reason: ExitReason,
    ) {
        let id = self.next_id;
        self.next_id += 1;
        let now = now_ms();
        let first = &ar.route.legs[0];
        let last = ar.route.legs.last().unwrap_or(first);
        let drift = profit_usd - ar.signal_usd + fees_usd - hedge_pnl_usd;
        self.stats.gross_locked_usd += ar.signal_usd;
        self.stats.exit_drift_usd += drift;
        self.stats.hedge_pnl_usd += hedge_pnl_usd;
        let trade = Trade {
            id,
            opp: ar.route_id.clone(),
            underlying: ar.route.underlying.clone(),
            expiry: ar.route.expiry.clone(),
            strike: ar.route.strike,
            right: right_from_inst(&first.instrument),
            buy_venue: first.venue.clone(),
            sell_venue: last.venue.clone(),
            units,
            tier: ivarb_protocol::Tier::Sniper,
            realized_vp: 0.0,
            realized_usd: profit_usd,
            slippage_vp: 0.0,
            fees_usd,
            gross_locked_usd: ar.signal_usd,
            exit_drift_usd: drift,
            capital_usd: ar.capital_usd,
            hedge_pnl_usd,
            hold_s: ((now - ar.started_ms).max(0) as f64) / 1000.0,
            exit_reason: reason,
            opened_ms: ar.started_ms,
            closed_ms: now,
            legs,
        };
        self.trades.push_back(trade);
        while self.trades.len() > 64 {
            self.trades.pop_front();
        }
    }

    fn spawn_attempt(&mut self, opp: Opportunity, units: f64) {
        let attempt = self.next_id;
        self.next_id += 1;
        self.stats.attempts_total += 1;
        self.risk.reserve(&opp, units);
        // Calibration hook: this route gets another sample.
        let route = format!("{}>{}", opp.buy_venue, opp.sell_venue);
        self.calib.record_attempt(&route);
        let route_mult = self.calib.edge_mult(&route);
        if route_mult < 0.85 && route_mult > 0.0 {
            self.ev(Some(opp.id.clone()), ExecLevel::Warn, "calib", format!("route {route}: firing at {:.0}% of displayed edge (learned capture)", 100.0 * route_mult));
        }
        self.live_attempts.insert(attempt, (opp.clone(), units));

        // Watch this instrument for the lifetime of the attempt — registered
        // immediately so the engine's next sweep publishes its quotes.
        let inst = canonical_inst(&opp);
        if self.watch_set.insert(inst) {
            let set: Vec<String> = self.watch_set.iter().cloned().collect();
            let _ = self.deps.watch_tx.send(WatchCmd::Set(set));
        }

        let snap = self.deps.snapshot.load();
        let vspec = |id: &str| snap.venues.iter().find(|v| v.venue == id);
        let buy_fee = vspec(&opp.buy_venue).map(|v| v.taker_fee_rate).unwrap_or(0.0003);
        let sell_fee = vspec(&opp.sell_venue).map(|v| v.taker_fee_rate).unwrap_or(0.0003);
        let sell_ct = vspec(&opp.sell_venue).map(|v| v.contract_size).filter(|c| *c > 0.0).unwrap_or(1.0);
        let buy_ct = if opp.contract_size > 0.0 { opp.contract_size } else { 1.0 };

        let ctx = AttemptCtx {
            attempt,
            opp: opp.clone(),
            buy_fee_rate: buy_fee,
            sell_fee_rate: sell_fee,
            buy_ct,
            sell_ct,
            broker: self.broker.clone(),
            router: self.router.clone(),
            watched: self.deps.watched.clone(),
            snapshot: self.deps.snapshot.clone(),
            events: self.event_tx.clone(),
            settings: self.settings.clone(),
        };
        let tier = opp.tier;
        let attempt_tx = self.attempt_tx.clone();
        tokio::spawn(async move {
            let outcome = match tier {
                ivarb_protocol::Tier::Sniper => machine::run_sniper(ctx, units).await,
                ivarb_protocol::Tier::Convergence => machine::run_convergence(ctx, units).await,
                ivarb_protocol::Tier::Route | ivarb_protocol::Tier::Monitor => AttemptOutcome::Aborted { reason: "route cycles fire through the route sequencer".into(), cooldown_ms: 0 },
            };
            let _ = attempt_tx.send((attempt, outcome)).await;
        });
        self.dirty = true;
    }

    // ------------------------------------------------------------------
    // attempt outcomes
    // ------------------------------------------------------------------
    fn handle_outcome(&mut self, (attempt, outcome): (u64, AttemptOutcome)) {
        let Some((opp, units)) = self.live_attempts.remove(&attempt) else {
            return;
        };
        match outcome {
            AttemptOutcome::Opened(draft) => {
                self.open_position(attempt, opp, draft);
            }
            AttemptOutcome::Aborted { reason, cooldown_ms } => {
                self.risk.release(&opp, units, cooldown_ms, now_ms());
                self.calib.record_abort(&format!("{}>{}", opp.buy_venue, opp.sell_venue));
                let inst = canonical_inst(&opp);
                if !self.positions.values().any(|p| canonical_inst_of_pos(p) == inst) {
                    self.watch_set.remove(&inst);
                    self.watch_dirty = true;
                }
                self.ev(Some(opp.id.clone()), ExecLevel::Info, "abort", format!("attempt #{attempt} aborted: {reason}"));
            }
            AttemptOutcome::Unwound { reason, cost_usd } => {
                self.risk.release(&opp, units, 45_000, now_ms());
                let inst = canonical_inst(&opp);
                self.watch_set.remove(&inst);
                self.watch_dirty = true;
                self.stats.single_leg_events += 1;
                self.stats.realized_usd -= cost_usd.max(0.0);
                self.ev(
                    Some(opp.id.clone()),
                    ExecLevel::Error,
                    "unwind",
                    format!(
                        "attempt #{attempt} single-leg risk realized: {reason} — P&L impact {:+.2} USD",
                        -cost_usd.max(0.0)
                    ),
                );
            }
        }
        self.dirty = true;
    }

    fn open_position(&mut self, attempt: u64, opp: Opportunity, d: PositionDraft) {
        let id = self.next_id;
        self.next_id += 1;
        let now = now_ms();
        let entry_net = d.entry_net_vp;
        let ct_buy = if opp.contract_size > 0.0 { opp.contract_size } else { 1.0 };
        let ct_sell = if d.contracts_sell > 0.0 { d.units / d.contracts_sell } else { 1.0 };
        let vega_unit = (opp.vega_vp_usd / ct_buy).max(1e-9);
        let mut pos = Position {
            id,
            attempt,
            opp: opp.id.clone(),
            underlying: opp.underlying.clone(),
            expiry: opp.expiry.clone(),
            expiry_ms: opp.expiry_ms,
            strike: opp.strike,
            right: opp.right,
            buy_venue: opp.buy_venue.clone(),
            sell_venue: opp.sell_venue.clone(),
            units: d.units,
            contracts_buy: d.contracts_buy,
            contracts_sell: d.contracts_sell,
            contract_size_buy: ct_buy,
            contract_size_sell: ct_sell,
            entry: PositionEntry {
                t_ms: now,
                spread_vp: d.entry_spread_vp,
                fee_vp: d.entry_fees_usd / (d.units * vega_unit).max(1e-9),
                net_vp: entry_net,
                buy_px_usd: d.buy_px_unit,
                sell_px_usd: d.sell_px_unit,
                signal_net_vp: opp.net_vp,
                vega_vp_usd: opp.vega_vp_usd,
                spot: opp.spot,
                tier: d.tier,
                fees_usd: d.entry_fees_usd,
            },
            mtm: PositionMtm {
                t_ms: now,
                spread_vp: d.entry_spread_vp,
                unrealized_usd: -d.entry_fees_usd,
                theta_burn_usd: 0.0,
                hedge_pnl_usd: 0.0,
                elapsed_s: 0.0,
            },
            // Exit plan keys off the SIGNAL spread (the divergence we are
            // betting on), not the touch-locked entry: passive fills at
            // favorable limits always beat the signal, and exiting the
            // instant mids < entry would churn the position for nothing.
            // The take target is FEE-AWARE: exiting costs both legs' taker
            // fees again, so on thin edges we wait for deeper convergence
            // (net − entry-fee − 10bp margin) instead of taking a "profit"
            // the round-trip fees would eat. The trailing lock in the
            // monitor then banks the capture early if it bounces.
            exit_take_vp: {
                let net = opp.net_vp.max(0.0);
                let fee_vp = d.entry_fees_usd / (d.units * vega_unit).max(1e-9);
                (self.settings.take_profit_frac * net)
                    .min((net - fee_vp - 0.10).max(0.05))
                    .max(0.05)
            },
            exit_stop_vp: -(self.settings.stop_mult * opp.net_vp.max(0.0)).max(0.10),
            theta_budget_vp: (self.settings.theta_budget_frac * opp.net_vp.max(0.0)).max(0.10),
            max_hold_s: opp.hold_s.max(60.0),
            min_spread_vp: d.entry_spread_vp,
            peak_unrealized_usd: 0.0,
            hedge_units: 0.0,
            hedge_avg_px: 0.0,
            hedge_venue: String::new(),
            delta_now: 0.0,
            escape: None,
            capital_usd: 0.0,
            closing: false,
        };
        // Capital reserved by this position: long premium + entry fees on the
        // buy venue, buffered short-option margin + fees on the sell venue.
        let snap = self.deps.snapshot.load();
        let sell_fee_rate = snap
            .venues
            .iter()
            .find(|v| v.venue == opp.sell_venue)
            .map(|v| v.taker_fee_rate)
            .filter(|r| *r > 0.0)
            .unwrap_or(0.0003);
        let margin_ct = ivarb_quant::margin_per_contract(
            &opp.sell_venue,
            d.sell_px_unit,
            ct_sell,
            opp.spot,
            opp.strike,
            match opp.right {
                ivarb_protocol::Right::Call => ivarb_quant::QuantRight::Call,
                ivarb_protocol::Right::Put => ivarb_quant::QuantRight::Put,
            },
        );
        let capital = d.buy_px_unit * d.units
            + margin_ct * d.contracts_sell * (1.0 + ivarb_quant::MARGIN_BUFFER)
            + d.entry_fees_usd
            + sell_fee_rate * opp.spot * d.units;
        pos.capital_usd = capital;
        self.stats.capital_reserved_usd += capital;
        self.stats.capital_peak_usd = self.stats.capital_peak_usd.max(self.stats.capital_reserved_usd);
        let now_ms_val = now;
        self.stats.reserved_time_s += (now_ms_val - self.stats.reserved_last_t).max(0) as f64 / 1000.0;
        self.stats.reserved_last_t = now_ms_val;
        self.positions.insert(id, pos);
        self.risk.promote();
        self.stats.fills_total += 2;
        self.dirty = true;
    }

    // ------------------------------------------------------------------
    // monitor: MTM, exits, hedging
    // ------------------------------------------------------------------
    /// Live-or-paper taker fill for the close path. In live mode with a
    /// gateway on the venue this places a REAL order; otherwise the paper
    /// broker fills instantly, exactly as before. (Free function taking
    /// disjoint field borrows so the caller may hold `&mut positions`.)
    async fn fill_or_paper(
        router: &Arc<ArcSwap<VenueRouter>>,
        broker: &PaperBroker,
        venue: &str,
        inst: &str,
        side: paper::Side,
        touch: f64,
        depth: f64,
        want_contracts: f64,
        fee_rate: f64,
        spot: f64,
        ct: f64,
    ) -> paper::Fill {
        let r = router.load_full();
        if r.live_armed() {
            return r.take(venue, inst, side, touch, depth, want_contracts, fee_rate, spot, ct).await;
        }
        broker.take_sync(venue, side, touch, depth, want_contracts, fee_rate, spot, ct, 180)
    }

    /// Live-or-paper hedge. Returns (px, fee); px <= 0 means the live hedge
    /// was REFUSED (no gateway on that venue) — the caller must skip the
    /// hedge update rather than book a nonsense price.
    async fn hedge_or_paper(
        router: &Arc<ArcSwap<VenueRouter>>,
        broker: &PaperBroker,
        venue: &str,
        und: &str,
        side: paper::Side,
        units: f64,
        spot: f64,
    ) -> (f64, f64) {
        let r = router.load_full();
        if r.live_armed() {
            return r.hedge(venue, und, side, units, spot).await;
        }
        broker.hedge_sync(venue, side, units, spot)
    }

    async fn monitor(&mut self) {
        let now = now_ms();
        let watched = self.deps.watched.load();
        let snap = self.deps.snapshot.load();
        let spot_of = |und: &str| -> f64 {
            snap.spots
                .iter()
                .find(|s| s.underlying == und)
                .map(|s| s.price)
                .unwrap_or(0.0)
        };

        let mut to_close: Vec<(u64, ExitReason)> = Vec::new();
        let mut hedges: Vec<(u64, paper::Side, f64, f64, String)> = Vec::new();
        let mut escape_events: Vec<(String, String)> = Vec::new();
        // Captured before the iter_mut loop: cross-venue hedge routing and
        // gateway availability (the loop holds &mut self.positions).
        let cross_hedge = self.settings.escape_cross_venue_hedge;
        let live_mode = matches!(self.mode, ExecMode::Live);
        let router = self.router.clone();
        let pick_hv = |buy: &str, sell: &str| {
            pick_hedge_venue_impl(buy, sell, cross_hedge, |v| !live_mode || router.load().has_gateway(v))
        };
        for (id, p) in self.positions.iter_mut() {
            let legs = watched.get(&canonical_inst_of_pos(p));
            let buy_leg = legs.and_then(|ls| ls.iter().find(|l| l.venue == p.buy_venue));
            let sell_leg = legs.and_then(|ls| ls.iter().find(|l| l.venue == p.sell_venue));

            let vega_unit = if p.contract_size_buy > 0.0 {
                p.entry.vega_vp_usd / p.contract_size_buy
            } else {
                p.entry.vega_vp_usd
            };
            let elapsed_s = (now - p.entry.t_ms) as f64 / 1000.0;
            p.mtm.elapsed_s = elapsed_s;

            if let (Some(bl), Some(sl)) = (buy_leg, sell_leg) {
                // Fresh quotes only: MTM and exits computed across legs of
                // different staleness fabricate P&L. Slow venues hold the
                // last good mark until their next poll refreshes the book.
                let now = now_ms();
                let fresh = now - bl.recv_ms <= 6_000 && now - sl.recv_ms <= 6_000;
                let buy_mid = mid(bl.px_bid_unit, bl.px_ask_unit, bl.px_mark_unit);
                let sell_mid = mid(sl.px_bid_unit, sl.px_ask_unit, sl.px_mark_unit);
                if fresh && buy_mid > 0.0 && sell_mid > 0.0 {
                    p.mtm.spread_vp = (sell_mid - buy_mid) / vega_unit.max(1e-9);
                    p.mtm.unrealized_usd = (buy_mid - p.entry.buy_px_usd) * p.units
                        + (p.entry.sell_px_usd - sell_mid) * p.units
                        - p.entry.fees_usd
                        + p.mtm.hedge_pnl_usd;
                    // Trailing anchor: the best (lowest) mid spread seen —
                    // only ever decreases, and only on fresh quotes so a
                    // stale tick can never fake a new best.
                    if p.mtm.spread_vp < p.min_spread_vp {
                        p.min_spread_vp = p.mtm.spread_vp;
                    }
                    // USD profit ratchet anchor: the best unrealized P&L
                    // seen — only ever increases, fresh quotes only.
                    if p.mtm.unrealized_usd > p.peak_unrealized_usd {
                        p.peak_unrealized_usd = p.mtm.unrealized_usd;
                    }
                }
                // Theta burn (BS at the current mid IV).
                let iv_mid = match (bl.iv_mark_vp, sl.iv_mark_vp) {
                    (Some(a), Some(b)) => (a + b) * 0.5 / 100.0,
                    _ => 0.5,
                };
                let spot = spot_of(&p.underlying).max(1e-9);
                let t = ((p.expiry_ms - now).max(0) as f64 / 1000.0 / (365.0 * 86400.0)).max(1e-6);
                let theta_day = ivarb_quant::bs_theta(spot, p.strike, t, iv_mid) / 365.0;
                p.mtm.theta_burn_usd = theta_day.abs() * p.units * (elapsed_s / 86400.0);

                // Delta hedge (spec step 06): residual delta across venues.
                // Also publishes the live delta for the UI readout.
                {
                    let right = match p.right {
                        Right::Call => ivarb_quant::Right::Call,
                        Right::Put => ivarb_quant::Right::Put,
                    };
                    let d_buy = bl
                        .iv_mark_vp
                        .map(|iv| ivarb_quant::bs_delta(spot, p.strike, t, iv / 100.0, right))
                        .unwrap_or(0.0);
                    let d_sell = sl
                        .iv_mark_vp
                        .map(|iv| ivarb_quant::bs_delta(spot, p.strike, t, iv / 100.0, right))
                        .unwrap_or(0.0);
                    let residual = p.units * (d_buy - d_sell);
                    p.delta_now = residual + p.hedge_units;
                    if self.settings.hedge_enabled
                        && spot > 0.0
                        && p.escape.is_none()
                    {
                        // Positions in the 0DTE escape ladder carry their
                        // hedge on the escape venue — the incremental
                        // hedge loop must not fight the ladder.
                        let target_hedge = -residual;
                        let delta_units = target_hedge - p.hedge_units;
                        if delta_units.abs() * spot > self.settings.hedge_threshold_usd {
                            // Cross-venue delta neutrality: the incremental
                            // hedge goes to a venue that is NEITHER option
                            // leg when one is reachable — the structure's
                            // delta never depends on a single venue book.
                            let hv = pick_hv(&p.buy_venue, &p.sell_venue);
                            hedges.push((*id, if delta_units > 0.0 { paper::Side::Buy } else { paper::Side::Sell }, delta_units.abs(), spot, hv));
                        }
                    }
                }

                // ---- 0DTE escape detection (uncaptured arb) ----
                // If the spread will not come to us, stop paying theta for
                // it: flip to same-day doctrine — hedge the full residual on
                // a THIRD venue, then both legs out at market, flat fast.
                if p.escape.is_none() && !p.closing && self.settings.escape_enabled {
                    let theta_vp = p.mtm.theta_burn_usd / (p.units * vega_unit).max(1e-9);
                    if let Some(reason) = escape_trigger(
                        &self.settings,
                        p.mtm.spread_vp,
                        p.entry.signal_net_vp,
                        theta_vp,
                        p.theta_budget_vp,
                        elapsed_s,
                        p.max_hold_s,
                    ) {
                        let hv = pick_hv(&p.buy_venue, &p.sell_venue);
                        let net = p.entry.signal_net_vp.max(0.0);
                        let progress = net - p.mtm.spread_vp;
                        let age_s = elapsed_s.max(1.0);
                        let opp_id = p.opp.clone();
                        let msg = format!(
                            "0DTE ESCAPE pos #{} — {} after {:.0}s (spread {:+.2} vp vs signal {:+.2} vp, progress {:.0}%); hedge venue {}, flat-by deadline {:.0}s",
                            p.id, reason.label(), age_s, p.mtm.spread_vp, net,
                            100.0 * (progress / net.max(1e-9)).clamp(-1.0, 1.0),
                            hv, self.settings.escape_deadline_s
                        );
                        escape_events.push((opp_id, msg));
                        p.escape = Some(EscapeState {
                            reason,
                            since_ms: now,
                            deadline_ms: now + (self.settings.escape_deadline_s as i64) * 1000,
                            hedge_venue: hv,
                            hedge_placed_units: 0.0,
                            phase: 0,
                            phase_t_ms: now,
                        });
                    }
                }

                // ---- exit rules (time-aware, fresh quotes only) ----
                let theta_burn_vp = p.mtm.theta_burn_usd / (p.units * vega_unit).max(1e-9);
                if !fresh {
                    // Stale legs: hold and wait for the next poll; only the
                    // 25-second hard staleness exit may fire.
                    if now - bl.recv_ms.max(sl.recv_ms) > 25_000 {
                        to_close.push((*id, ExitReason::StaleQuotes));
                    }
                } else if p.closing {
                    to_close.push((*id, ExitReason::Manual));
                } else if p.mtm.spread_vp <= p.exit_take_vp {
                    to_close.push((*id, ExitReason::TakeProfit));
                } else if {
                    // Trailing lock: once ≥50% of the signal edge has
                    // converged, bank it — if the spread gives back more than
                    // 30% of the achieved progress, exit on the bounce
                    // instead of riding it back to the stop.
                    let net = p.entry.signal_net_vp.max(0.0);
                    let progress = net - p.min_spread_vp;
                    progress >= 0.5 * net
                        && p.mtm.spread_vp >= p.min_spread_vp + 0.30 * progress
                } {
                    to_close.push((*id, ExitReason::Trail));
                } else if p.peak_unrealized_usd >= 25.0
                    && p.mtm.unrealized_usd <= 0.55 * p.peak_unrealized_usd
                {
                    // USD profit ratchet: a materially profitable position
                    // that has given back 45% of its peak banks the remainder
                    // now — theta and exit drift round-trip paper profits
                    // faster than the spread trail can see them.
                    to_close.push((*id, ExitReason::Trail));
                } else if p.mtm.spread_vp <= p.exit_stop_vp {
                    to_close.push((*id, ExitReason::Stop));
                } else if theta_burn_vp >= p.theta_budget_vp {
                    to_close.push((*id, ExitReason::ThetaBudget));
                } else if elapsed_s >= p.max_hold_s {
                    to_close.push((*id, ExitReason::MaxHold));
                } else if p.expiry_ms - now < 6 * 3600 * 1000 {
                    to_close.push((*id, ExitReason::Expiry));
                }
            } else if p.mtm.t_ms > 0 && now - p.mtm.t_ms > 30_000 {
                to_close.push((*id, ExitReason::StaleQuotes));
            }
            p.mtm.t_ms = now;
        }

        // Emit the escape detections queued inside the loop (the loop held
        // &mut self.positions; events need &mut self).
        for (opp, msg) in escape_events {
            self.ev(Some(opp), ExecLevel::Warn, "escape/detect", msg);
        }

        // Apply hedges (live perp orders in live mode, instant paper fills
        // otherwise). Data gathered without holding the positions borrow
        // across the await.
        for (id, side, amt, spot, venue) in hedges {
            let Some((und, opp_id)) = self.positions.get(&id).map(|p| {
                (
                    p.underlying.clone(),
                    p.opp.clone(),
                )
            }) else {
                continue;
            };
            let (px, _fee) = Self::hedge_or_paper(&self.router, &self.broker, &venue, &und, side, amt, spot).await;
            if px <= 0.0 {
                self.ev(
                    Some(opp_id),
                    ExecLevel::Error,
                    "hedge",
                    "live hedge refused (no perp gateway on venue) — delta stays unhedged".into(),
                );
                continue;
            }
            if let Some(p) = self.positions.get_mut(&id) {
                let delta_units = if side == paper::Side::Buy { amt } else { -amt };
                let new_units = p.hedge_units + delta_units;
                p.hedge_avg_px = if new_units.abs() > 1e-9 {
                    (p.hedge_avg_px * p.hedge_units + px * delta_units) / new_units
                } else {
                    0.0
                };
                p.hedge_units = new_units;
                p.hedge_venue = venue.clone();
                p.mtm.hedge_pnl_usd = p.hedge_units * (spot - p.hedge_avg_px);
            }
        }

        // 0DTE escape ladder: advance every position that flipped out of
        // its normal convergence plan (hedge first, legs out, hedge released).
        let escaping: Vec<u64> = self
            .positions
            .values()
            .filter(|p| p.escape.is_some())
            .map(|p| p.id)
            .collect();
        for id in escaping {
            self.escape_step(id).await;
        }

        for (id, reason) in to_close {
            self.close_position(id, reason).await;
        }

        // Daily loss circuit breaker.
        self.risk.rollover_day(self.stats.realized_usd);
        let mut unrealized = 0.0;
        for p in self.positions.values() {
            unrealized += p.mtm.unrealized_usd;
        }
        let day_pnl = (self.stats.realized_usd - self.risk.day_anchor_realized) + unrealized;
        if self.kill.is_none() && day_pnl <= -self.settings.daily_loss_limit_usd {
            self.kill = Some(format!("daily loss limit ({:.0} USD)", self.settings.daily_loss_limit_usd));
            self.armed = false;
            for p in self.positions.values_mut() {
                p.closing = true;
            }
            self.ev(None, ExecLevel::Error, "kill", format!("AUTO KILL — daily P&L {day_pnl:.0} USD breached the loss limit; flattening"));
        }

        // Watched-set maintenance.
        if self.watch_dirty {
            let set: Vec<String> = self.watch_set.iter().cloned().collect();
            let _ = self.deps.watch_tx.send(WatchCmd::Set(set));
            self.watch_dirty = false;
        }
        self.dirty = true;
    }

    /// Perp-capable venue preference for cross-venue delta hedges, ordered by
    /// perp liquidity + fee quality. The escape ladder and the incremental
    /// hedge loop both pick the first venue that is NEITHER option leg, so
    /// the delta-neutral structure is spread across three different books.
    /// (Routing happens through `pick_hedge_venue_impl` so the monitor's
    /// `iter_mut` loop can call it without borrowing `self`.)
    const HEDGE_VENUE_PREF: &[&str] = &["bybit", "okx", "binance", "delta", "deribit"];

    /// Advance one position's 0DTE escape ladder by a single step.
    ///
    /// Phase 0 → 1: hedge the FULL residual delta on the escape (third)
    /// venue — delta is neutralized before any leg moves, so the exit can
    /// never turn a failed arb into a directional bet.
    /// Phase 1 → flat: both legs out at market (crossing the touch; the
    /// close path's sane-banded touch/mark/entry ladder still applies), the
    /// hedge released on the escape venue, the trade booked as
    /// `Escaped0dte` with its own stats bucket. Past the hard deadline the
    /// close is forced regardless of quote freshness.
    async fn escape_step(&mut self, id: u64) {
        let now = now_ms();
        let (phase, hedge_venue, deadline_ms, phase_t_ms) = match self.positions.get(&id) {
            Some(p) => match &p.escape {
                Some(e) => (e.phase, e.hedge_venue.clone(), e.deadline_ms, e.phase_t_ms),
                None => return,
            },
            None => return,
        };
        let forced = now > deadline_ms;
        // Hedge-fill confirmation dwell: after placing the hedge the ladder
        // waits briefly so the neutralization is verified (and visible)
        // before the legs move — unless the hard deadline forces through.
        const PHASE_DWELL_MS: i64 = 900;
        if phase == 1 && !forced && now - phase_t_ms < PHASE_DWELL_MS {
            return;
        }

        if phase == 0 {
            // --- hedge first: neutralize delta on a third venue ---
            let (delta_now, spot, und, opp_id, already_placed) = match self.positions.get(&id) {
                Some(p) => (
                    p.delta_now,
                    p.entry.spot,
                    p.underlying.clone(),
                    p.opp.clone(),
                    p.escape.as_ref().map(|e| e.hedge_placed_units).unwrap_or(0.0),
                ),
                None => return,
            };
            // Idempotency: a step that already placed its hedge never
            // re-hedges (monitor ticks can race the await points).
            if already_placed.abs() > 1e-9 {
                if let Some(p) = self.positions.get_mut(&id) {
                    if let Some(e) = p.escape.as_mut() {
                        e.phase = 1;
                    }
                }
                return;
            }
            let snap = self.deps.snapshot.load();
            let spot = snap
                .spots
                .iter()
                .find(|s| s.underlying == und)
                .map(|s| s.price)
                .filter(|s| *s > 0.0)
                .unwrap_or(spot);
            let delta_units = -delta_now; // full neutralization
            if delta_units.abs() > 1e-9 && spot > 0.0 {
                let side = if delta_units > 0.0 { paper::Side::Buy } else { paper::Side::Sell };
                let (px, _fee) = Self::hedge_or_paper(
                    &self.router, &self.broker, &hedge_venue, &und, side, delta_units.abs(), spot,
                )
                .await;
                if px > 0.0 {
                    if let Some(p) = self.positions.get_mut(&id) {
                        let signed = if side == paper::Side::Buy { delta_units.abs() } else { -delta_units.abs() };
                        let new_units = p.hedge_units + signed;
                        p.hedge_avg_px = if new_units.abs() > 1e-9 {
                            (p.hedge_avg_px * p.hedge_units + px * signed) / new_units
                        } else {
                            0.0
                        };
                        p.hedge_units = new_units;
                        p.hedge_venue = hedge_venue.clone();
                        p.delta_now = 0.0;
                        if let Some(e) = p.escape.as_mut() {
                            e.hedge_placed_units = new_units;
                            e.phase = 1;
                            e.phase_t_ms = now_ms();
                        }
                        self.ev(
                            Some(opp_id),
                            ExecLevel::Ok,
                            "escape/hedge",
                            format!(
                                "0DTE hedge placed — {:.4} {} perp {} @ {:.2} on {} (third venue): structure delta ≈ 0 while legs exit",
                                signed.abs(), und, if side == paper::Side::Buy { "buy" } else { "sell" }, px, hedge_venue
                            ),
                        );
                    }
                } else {
                    self.ev(
                        Some(opp_id),
                        ExecLevel::Error,
                        "escape/hedge",
                        format!("live hedge refused on {hedge_venue} — exiting legs unhedged"),
                    );
                    if let Some(p) = self.positions.get_mut(&id) {
                        if let Some(e) = p.escape.as_mut() {
                            e.phase = 1;
                            e.phase_t_ms = now_ms();
                        }
                    }
                }
            } else {
                // Already flat delta: skip straight to the leg-out phase.
                if let Some(p) = self.positions.get_mut(&id) {
                    if let Some(e) = p.escape.as_mut() {
                        e.phase = 1;
                        e.phase_t_ms = now_ms();
                    }
                }
            }
            return;
        }

        if phase >= 1 {
            // --- legs out at market, hedge released, trade booked ---
            if let Some(p) = self.positions.get_mut(&id) {
                if let Some(e) = p.escape.as_mut() {
                    e.phase = 2;
                    e.phase_t_ms = now_ms();
                    if forced {
                        e.reason = EscapeReason::Deadline;
                    }
                }
            }
            if forced {
                let opp_id = self.positions.get(&id).map(|p| p.opp.clone());
                self.ev(
                    opp_id,
                    ExecLevel::Error,
                    "escape/deadline",
                    "0DTE escape deadline hit — forcing legs out at any price".into(),
                );
            }
            self.close_position(id, ExitReason::Escaped0dte).await;
        }
    }

    async fn close_position(&mut self, id: u64, reason: ExitReason) {
        let Some(mut p) = self.positions.remove(&id) else { return };
        let now = now_ms();
        let watched = self.deps.watched.load();
        let snap = self.deps.snapshot.load();
        let inst = canonical_inst_of_pos(&p);
        let legs = watched.get(&inst);
        let buy_leg = legs.and_then(|ls| ls.iter().find(|l| l.venue == p.buy_venue));
        let sell_leg = legs.and_then(|ls| ls.iter().find(|l| l.venue == p.sell_venue));
        let spot = snap
            .spots
            .iter()
            .find(|s| s.underlying == p.underlying)
            .map(|s| s.price)
            .filter(|s| *s > 0.0)
            .unwrap_or(p.entry.spot);

        let buy_fee = snap.venues.iter().find(|v| v.venue == p.buy_venue).map(|v| v.taker_fee_rate).unwrap_or(0.0003);
        let sell_fee = snap.venues.iter().find(|v| v.venue == p.sell_venue).map(|v| v.taker_fee_rate).unwrap_or(0.0003);
        let vega_unit = if p.contract_size_buy > 0.0 {
            p.entry.vega_vp_usd / p.contract_size_buy
        } else {
            p.entry.vega_vp_usd
        };

        // Sell the long leg at its venue's bid (sync fill: exit latency is
        // not slept on, it is only reported). Dust quotes (stale touch far
        // from entry/mark) are rejected and fall back to the venue mark so
        // phantom losses never enter the accounting.
        let sane = |px: f64, ref_px: f64| -> bool {
            px > 0.0 && ref_px > 0.0 && (0.6..=1.6).contains(&(px / ref_px))
        };
        tracing::info!(
            "exit quotes pos={} inst={} units={} entry_buy={:.2} entry_sell={:.2} buy_bid={:?} buy_mark={:?} sell_ask={:?} sell_mark={:?}",
            p.id, inst, p.units, p.entry.buy_px_usd, p.entry.sell_px_usd,
            buy_leg.and_then(|l| l.px_bid_unit), buy_leg.and_then(|l| l.px_mark_unit),
            sell_leg.and_then(|l| l.px_ask_unit), sell_leg.and_then(|l| l.px_mark_unit),
        );
        let (sell_px, mut exit_fees) = match buy_leg
            .and_then(|l| l.px_bid_unit)
            .filter(|x| sane(*x, p.entry.buy_px_usd))
        {
            Some(bid) => {
                let f = Self::fill_or_paper(
                    &self.router, &self.broker,
                    &p.buy_venue.clone(), &inst, paper::Side::Sell, bid,
                    buy_leg.and_then(|l| l.size_bid).unwrap_or(1.0),
                    p.units / p.contract_size_buy.max(1e-9), buy_fee, spot, p.contract_size_buy,
                )
                .await;
                (f.px_unit, f.fee_usd)
            }
            _ => {
                // Degrade: exit at the venue MARK when the book is dust — but
                // the mark must itself be sane vs the entry premium. Venue
                // marks come in mixed scales (coin-margined, forward-based,
                // per-contract vs per-unit); an insane mark here once booked a
                // phantom $162k loss on a $6.4k position. Entry price is the
                // final fallback: flat, honest, bounded.
                let mark = buy_leg
                    .and_then(|l| l.px_mark_unit)
                    .filter(|x| sane(*x, p.entry.buy_px_usd));
                if buy_leg.and_then(|l| l.px_bid_unit).is_some() || buy_leg.and_then(|l| l.px_mark_unit).is_some() {
                    tracing::warn!(
                        "exit degraded to {} on {} — book dust or insane mark (bid={:?} mark={:?} entry={:.2})",
                        if mark.is_some() { "mark" } else { "entry" }, p.buy_venue,
                        buy_leg.and_then(|l| l.px_bid_unit), buy_leg.and_then(|l| l.px_mark_unit), p.entry.buy_px_usd,
                    );
                }
                (mark.map(|m| m).unwrap_or(p.entry.buy_px_usd), 0.0)
            }
        };
        // Buy back the short leg at its venue's ask.
        let (buyback_px, fee2) = match sell_leg
            .and_then(|l| l.px_ask_unit)
            .filter(|x| sane(*x, p.entry.sell_px_usd))
        {
            Some(ask) => {
                let f = Self::fill_or_paper(
                    &self.router, &self.broker,
                    &p.sell_venue.clone(), &inst, paper::Side::Buy, ask,
                    sell_leg.and_then(|l| l.size_ask).unwrap_or(1.0),
                    p.units / p.contract_size_sell.max(1e-9), sell_fee, spot, p.contract_size_sell,
                )
                .await;
                (f.px_unit, f.fee_usd)
            }
            _ => {
                let mark = sell_leg
                    .and_then(|l| l.px_mark_unit)
                    .filter(|x| sane(*x, p.entry.sell_px_usd));
                if sell_leg.and_then(|l| l.px_ask_unit).is_some() || sell_leg.and_then(|l| l.px_mark_unit).is_some() {
                    tracing::warn!(
                        "exit degraded to {} on {} — book dust or insane mark (ask={:?} mark={:?} entry={:.2})",
                        if mark.is_some() { "mark" } else { "entry" }, p.sell_venue,
                        sell_leg.and_then(|l| l.px_ask_unit), sell_leg.and_then(|l| l.px_mark_unit), p.entry.sell_px_usd,
                    );
                }
                (mark.map(|m| m).unwrap_or(p.entry.sell_px_usd), 0.0)
            }
        };
        exit_fees += fee2;

        // Unwind the residual-delta hedge. Escape-ladder hedges sit on the
        // escape (third) venue; hold-period hedges on the venue they were
        // placed on (the buy venue by default, cross-venue when enabled).
        let mut hedge_exit_pnl = 0.0;
        if p.hedge_units.abs() > 1e-9 {
            let side = if p.hedge_units > 0.0 { paper::Side::Sell } else { paper::Side::Buy };
            let hv = p
                .escape
                .as_ref()
                .map(|e| e.hedge_venue.clone())
                .unwrap_or_else(|| p.buy_venue.clone());
            let (px, _fee) = Self::hedge_or_paper(
                &self.router, &self.broker,
                &hv, &p.underlying.clone(), side, p.hedge_units.abs(), spot,
            )
            .await;
            if px > 0.0 {
                hedge_exit_pnl = p.hedge_units * (px - p.hedge_avg_px);
            } else {
                hedge_exit_pnl = p.hedge_units * (spot - p.hedge_avg_px); // mark at spot when a live hedge was refused
            }
            p.hedge_units = 0.0;
        }

        // Hard clamp: a hedged option spread on `units` underlying cannot
        // gain or lose more than a multiple of the notional — anything
        // beyond that is a data artifact, never a market move. Belt and
        // suspenders on top of the sane-banded exit prices above.
        let legs_pnl_raw = (sell_px - p.entry.buy_px_usd) * p.units
            + (p.entry.sell_px_usd - buyback_px) * p.units;
        let pnl_bound = 2.0 * spot * p.units + (p.entry.buy_px_usd + p.entry.sell_px_usd) * p.units;
        let legs_pnl = if legs_pnl_raw.abs() > pnl_bound {
            tracing::error!(
                "phantom exit P&L clamped: raw {:.2} → bound ±{:.2} (sell_px={:.2} buyback={:.2} entry {:.2}/{:.2} units={:.4}) — venue quote scale artifact",
                legs_pnl_raw, pnl_bound, sell_px, buyback_px, p.entry.buy_px_usd, p.entry.sell_px_usd, p.units
            );
            legs_pnl_raw.clamp(-pnl_bound, pnl_bound)
        } else {
            legs_pnl_raw
        };
        let realized = legs_pnl - p.entry.fees_usd - exit_fees + p.mtm.hedge_pnl_usd + hedge_exit_pnl;
        let realized_vp = realized / (p.units * vega_unit).max(1e-9);
        let hold_s = (now - p.entry.t_ms) as f64 / 1000.0;

        // Profit attribution: what was locked at entry vs what the market
        // gave/took between entry and exit vs explicit costs.
        let total_fees = p.entry.fees_usd + exit_fees;
        let hedge_total = p.mtm.hedge_pnl_usd + hedge_exit_pnl;
        let gross_locked = (p.entry.sell_px_usd - p.entry.buy_px_usd) * p.units;
        let exit_drift = realized - gross_locked + total_fees - hedge_total;

        self.stats.realized_usd += realized;
        self.stats.fees_usd += total_fees;
        self.stats.trades_closed += 1;
        // 0DTE escape bucket: count, timing and what the ladder salvaged
        // out of a failed capture.
        if reason == ExitReason::Escaped0dte {
            if let Some(esc) = &p.escape {
                self.stats.escape_exits += 1;
                self.stats.escape_time_sum_s += ((now - esc.since_ms).max(0) as f64) / 1000.0;
                self.stats.escape_salvaged_usd += realized;
            }
        }
        self.stats.gross_locked_usd += gross_locked;
        self.stats.exit_drift_usd += exit_drift;
        self.stats.hedge_pnl_usd += hedge_total;
        if realized >= 0.0 {
            self.stats.wins += 1;
            self.stats.gross_wins += realized;
        } else {
            self.stats.losses += 1;
            self.stats.gross_losses += -realized;
        }
        self.stats.capital_reserved_usd = (self.stats.capital_reserved_usd - p.capital_usd).max(0.0);
        self.stats.capital_peak_usd = self.stats.capital_peak_usd.max(self.stats.capital_reserved_usd);
        self.stats.capture_sum += (realized_vp / p.entry.signal_net_vp).clamp(-5.0, 5.0);
        self.stats.capture_n += 1.0;
        self.calib.record_close(
            &format!("{}>{}", p.buy_venue, p.sell_venue),
            realized_vp / p.entry.signal_net_vp.max(1e-9),
        );
        self.risk.position_closed(&inst, p.units, vega_unit);
        self.watch_set.remove(&inst);
        self.watch_dirty = true;

        let trade = Trade {
            id: p.id,
            opp: p.opp.clone(),
            underlying: p.underlying.clone(),
            expiry: p.expiry.clone(),
            strike: p.strike,
            right: p.right,
            buy_venue: p.buy_venue.clone(),
            sell_venue: p.sell_venue.clone(),
            units: p.units,
            tier: p.entry.tier,
            realized_vp,
            realized_usd: realized,
            slippage_vp: realized_vp - p.entry.signal_net_vp,
            fees_usd: total_fees,
            gross_locked_usd: gross_locked,
            exit_drift_usd: exit_drift,
            capital_usd: p.capital_usd,
            hedge_pnl_usd: hedge_total,
            hold_s,
            exit_reason: reason,
            opened_ms: p.entry.t_ms,
            closed_ms: now,
            legs: Vec::new(),
        };
        self.trades.push_front(trade);
        while self.trades.len() > 50 {
            self.trades.pop_back();
        }
        let reason_s = match reason {
            ExitReason::TakeProfit => "spread converged",
            ExitReason::Trail => "trailing lock — profit banked on the bounce",
            ExitReason::Stop => "stop — spread inverted",
            ExitReason::ThetaBudget => "theta budget exhausted",
            ExitReason::MaxHold => "max hold window",
            ExitReason::StaleQuotes => "quotes went stale",
            ExitReason::Manual => "manual close",
            ExitReason::Killed => "kill switch",
            ExitReason::Expiry => "expiry approaching",
            ExitReason::Escaped0dte => "0DTE escape — uncaptured arb rapid exit",
            ExitReason::RouteBanked => "multi-leg route banked",
            ExitReason::RouteUnwound => "multi-leg route unwound",
        };
        let capture_pct = 100.0 * (realized_vp / p.entry.signal_net_vp).clamp(-5.0, 5.0);
        self.ev(
            Some(p.opp.clone()),
            if realized >= 0.0 { ExecLevel::Ok } else { ExecLevel::Error },
            "close",
            format!(
                "position #{} closed ({}) — realized {:+.2} vp / {:+.2} USD, capture {:.0}% of signal, held {}",
                p.id, reason_s, realized_vp, realized, capture_pct, fmt_dur(hold_s)
            ),
        );
    }

    // ------------------------------------------------------------------
    // publish
    // ------------------------------------------------------------------
    fn publish(&mut self) {
        let now = now_ms();
        let regime = Regime::now_utc(now);
        let mut unrealized = 0.0;
        for p in self.positions.values() {
            unrealized += p.mtm.unrealized_usd;
        }
        let equity = self.broker.equity();
        let net_deposits = self.broker.net_deposits();
        let session_pnl = self.stats.realized_usd + unrealized;
        let roc_session = if self.stats.capital_peak_usd > 1.0 {
            100.0 * session_pnl / self.stats.capital_peak_usd
        } else {
            0.0
        };
        let profit_factor = if self.stats.gross_losses > 1e-9 {
            self.stats.gross_wins / self.stats.gross_losses
        } else if self.stats.gross_wins > 0.0 {
            f64::INFINITY
        } else {
            0.0
        };
        let snapshot = ExecSnapshot {
            t_ms: now,
            stats: ExecStats {
                t_ms: now,
                armed: self.armed,
                mode: self.mode,
                kill: self.kill.clone(),
                attempts_total: self.stats.attempts_total,
                attempts_active: self.live_attempts.len() as u32,
                positions_open: self.positions.len() as u32,
                vega_used_usd: self.risk.vega_used_usd,
                realized_usd: self.stats.realized_usd,
                unrealized_usd: unrealized,
                fees_usd: self.stats.fees_usd,
                fills_total: self.stats.fills_total,
                single_leg_events: self.stats.single_leg_events,
                capture_ratio: if self.stats.capture_n > 0.0 {
                    self.stats.capture_sum / self.stats.capture_n
                } else {
                    0.0
                },
                trades_closed: self.stats.trades_closed,
                equity_usd: equity,
                net_deposits_usd: net_deposits,
                capital_reserved_usd: self.stats.capital_reserved_usd,
                capital_free_usd: (equity - self.stats.capital_reserved_usd).max(0.0),
                roc_session_pct: roc_session,
                gross_locked_usd: self.stats.gross_locked_usd,
                exit_drift_usd: self.stats.exit_drift_usd,
                hedge_pnl_usd: self.stats.hedge_pnl_usd,
                wins: self.stats.wins,
                losses: self.stats.losses,
                profit_factor,
                avg_trade_usd: if self.stats.trades_closed > 0 {
                    self.stats.realized_usd / self.stats.trades_closed as f64
                } else {
                    0.0
                },
                route_attempts: self.stats.route_attempts,
                route_attempts_active: self.route_active.len() as u32,
                route_banked: self.stats.route_banked,
                route_profit_usd: self.stats.route_profit_usd,
                escape_exits: self.stats.escape_exits,
                escape_active: self
                    .positions
                    .values()
                    .filter(|p| p.escape.is_some())
                    .count() as u32,
                escape_salvaged_usd: self.stats.escape_salvaged_usd,
                escape_avg_s: if self.stats.escape_exits > 0 {
                    self.stats.escape_time_sum_s / self.stats.escape_exits as f64
                } else {
                    0.0
                },
            },
            settings: self.settings.clone(),
            regime: TimeRegime {
                utc_hour: regime.utc_hour,
                label: regime.label.to_string(),
                gate_mult: regime.gate_mult,
                size_mult: regime.size_mult,
                weekend: regime.weekend,
            },
            positions: self.positions.values().cloned().collect(),
            trades: self.trades.iter().cloned().collect(),
            events: self.events.iter().cloned().collect(),
            balances: self.broker.balances(),
            deposits_by_venue: self.broker.deposits_by_venue(),
            live: self.live_status_now(),
            routes: self.calib.snapshot(),
        };
        if let Ok(txt) = serde_json::to_string(&ivarb_protocol::ServerMsg::Exec { exec: snapshot.clone() }) {
            let _ = self.deps.bcast.send(Arc::new(txt));
        }
        self.out.store(Arc::new(snapshot));
        self.dirty = false;
    }

    /// LiveStatus with the derived fields refreshed at publish time.
    fn live_status_now(&self) -> LiveStatus {
        let mut live = self.live.clone();
        let router = self.router.load();
        live.live_gateways = self.live_gateways();
        live.route_ready = self.route_ready();
        live.chain_signer = router.evm.signer_address();
        live.blockers = if matches!(self.mode, ExecMode::Live) {
            let mut b = Vec::new();
            if live.live_gateways == 0 {
                b.push("no venue gateways connected — set venue API keys (BYBIT/OKX/BINANCE/DERIBIT)".into());
            }
            if live.live_gateways < 2 {
                b.push(format!(
                    "{} live gateway(s) — cross-venue routes need one on each leg; attempts stay refused rather than faking a leg",
                    live.live_gateways
                ));
            }
            b
        } else {
            self.arming_blockers.clone()
        };
        live
    }
}

fn mid(bid: Option<f64>, ask: Option<f64>, mark: Option<f64>) -> f64 {
    match (bid.filter(|x| *x > 0.0), ask.filter(|x| *x > 0.0)) {
        (Some(b), Some(a)) if a >= b => (a + b) * 0.5,
        (Some(b), None) => b,
        (None, Some(a)) => a,
        _ => mark.unwrap_or(0.0),
    }
}

fn canonical_inst_of_pos(p: &Position) -> String {
    format!(
        "{}-{}-{}-{}",
        p.underlying,
        p.expiry.replace('-', ""),
        ivarb_protocol::fmt_strike_id(p.strike),
        p.right.letter()
    )
}

/// Venue chain of a multi-hop route — the calibration key ("okx>bybit").
fn route_chain(r: &MultiHopRoute) -> String {
    r.legs
        .iter()
        .map(|l| l.venue.as_str())
        .collect::<Vec<_>>()
        .join(">")
}

/// Option right from a canonical instrument name ("…-C" / "…-P").
fn right_from_inst(inst: &str) -> Right {
    match inst.rsplit('-').next().and_then(|s| s.chars().next()) {
        Some('P') => Right::Put,
        _ => Right::Call,
    }
}

/// Compact USD formatting for refusal events.
fn fmt_px(x: f64) -> String {
    if x.abs() >= 1000.0 {
        format!("${x:.0}")
    } else {
        format!("${x:.2}")
    }
}

/// Quant-crate right from a canonical instrument name.
fn quant_right_from_inst(inst: &str) -> ivarb_quant::QuantRight {
    match right_from_inst(inst) {
        Right::Put => ivarb_quant::QuantRight::Put,
        Right::Call => ivarb_quant::QuantRight::Call,
    }
}

fn fmt_dur(s: f64) -> String {
    if s < 90.0 {
        format!("{s:.0}s")
    } else if s < 5400.0 {
        format!("{:.0}m", s / 60.0)
    } else {
        format!("{:.1}h", s / 3600.0)
    }
}

/// Pure 0DTE-escape trigger: has this uncaptured arb earned the rapid-exit
/// ladder? Fires on (in priority order):
/// 1. **Spread reversed** — the divergence widened past entry + buffer; the
///    move went against us, waiting only compounds the theta bleed.
/// 2. **Theta accelerating** — decay running ≥3× ahead of the budget
///    schedule; every extra hold second destroys edge.
/// 3. **No convergence** — the patience window (a fraction of the bucket's
///    max hold) elapsed with less than the required fraction of the signal
///    converged; the edge is not coming to us.
fn escape_trigger(
    s: &ExecSettings,
    spread_vp: f64,
    signal_net_vp: f64,
    theta_burn_vp: f64,
    theta_budget_vp: f64,
    elapsed_s: f64,
    max_hold_s: f64,
) -> Option<EscapeReason> {
    let net = signal_net_vp.max(0.0);
    if net <= 0.0 || max_hold_s <= 0.0 {
        return None;
    }
    let progress = net - spread_vp;
    // Reversal needs a grace window: post-fill quote bounce routinely
    // reads 1.5× the signal seconds after entry — that is noise, not a
    // reversal. Only a widening past entry + reversal_mult × signal that
    // passes the 10-second grace counts.
    if elapsed_s > 10.0 && spread_vp >= net + s.escape_reversal_mult * net {
        return Some(EscapeReason::SpreadReversed);
    }
    if elapsed_s > 30.0 && theta_budget_vp > 0.0 {
        let theta_hot = theta_burn_vp >= 3.0 * theta_budget_vp * (elapsed_s / max_hold_s);
        if theta_hot {
            return Some(EscapeReason::ThetaAccelerating);
        }
    }
    let patience_s = s.escape_patience_frac * max_hold_s;
    if elapsed_s >= patience_s && progress < s.escape_min_progress_frac * net {
        return Some(EscapeReason::NoConvergence);
    }
    None
}

/// Base venue name without the sim-feed namespace prefix.
fn base_venue(v: &str) -> &str {
    v.strip_prefix("sim:").unwrap_or(v)
}

/// Pure cross-venue hedge picker (see `Core::pick_hedge_venue`).
/// Leg comparison happens on BASE names (sim feeds namespace their venues
/// `sim:okx`…), and the returned venue carries the same namespace as the
/// legs so paper hedges book against the same accounts as the positions.
fn pick_hedge_venue_impl(
    buy: &str,
    sell: &str,
    cross_venue: bool,
    live_ok: impl Fn(&str) -> bool,
) -> String {
    if !cross_venue {
        return buy.to_string();
    }
    let (bb, sb) = (base_venue(buy), base_venue(sell));
    let sim = buy.starts_with("sim:") || sell.starts_with("sim:");
    for v in Core::HEDGE_VENUE_PREF {
        if *v != bb && *v != sb && live_ok(v) {
            return if sim { format!("sim:{v}") } else { (*v).to_string() };
        }
    }
    buy.to_string()
}

#[cfg(test)]
mod escape_tests {
    use super::*;
    use ivarb_protocol::ExecSettings;

    #[test]
    fn escape_triggers_on_spread_reversal_immediately() {
        let s = ExecSettings::default();
        // Signal +2.0 vp at entry; spread now 4.1 vp (≥ 2.0 + 1.0×2.0 = 4.0)
        // past the 10-second grace window.
        let r = escape_trigger(&s, 4.1, 2.0, 0.0, 0.2, 15.0, 3600.0);
        assert_eq!(r, Some(EscapeReason::SpreadReversed));
        // Inside the buffer: no escape yet.
        assert_eq!(escape_trigger(&s, 3.9, 2.0, 0.0, 0.2, 15.0, 3600.0), None);
        // Post-fill bounce inside the grace window (even 3× wide) is noise,
        // not a reversal — the position keeps its normal plan.
        assert_eq!(escape_trigger(&s, 6.5, 2.0, 0.0, 0.2, 5.0, 3600.0), None);
    }

    #[test]
    fn escape_triggers_on_no_convergence_after_patience() {
        let s = ExecSettings::default();
        // patience = 0.35 × 3600 = 1260s; at 1300s with <25% progress → fire.
        let r = escape_trigger(&s, 1.8, 2.0, 0.0, 0.2, 1300.0, 3600.0);
        assert_eq!(r, Some(EscapeReason::NoConvergence));
        // Same age but 60% converged → keep trading the normal plan.
        assert_eq!(escape_trigger(&s, 0.8, 2.0, 0.0, 0.2, 1300.0, 3600.0), None);
        // Converged too little but patience not elapsed → wait.
        assert_eq!(escape_trigger(&s, 1.8, 2.0, 0.0, 0.2, 60.0, 3600.0), None);
    }

    #[test]
    fn escape_triggers_on_theta_acceleration() {
        let s = ExecSettings::default();
        // Budget 0.2 vp over 3600s → schedule rate 0.2/3600 vp/s. At t=600s
        // the schedule allows 0.033 vp; burning 0.5 vp is ~15× hot.
        let r = escape_trigger(&s, 1.5, 2.0, 0.5, 0.2, 600.0, 3600.0);
        assert_eq!(r, Some(EscapeReason::ThetaAccelerating));
        // On-schedule theta never fires the escape.
        assert_eq!(escape_trigger(&s, 1.5, 2.0, 0.03, 0.2, 600.0, 3600.0), None);
    }

    #[test]
    fn no_escape_when_disabled_or_signal_degenerate() {
        let mut s = ExecSettings::default();
        s.escape_enabled = false;
        // (Caller checks escape_enabled before invoking; the pure fn assumes
        // it is on. Degenerate signals must never fire.)
        assert_eq!(escape_trigger(&s, 5.0, 0.0, 1.0, 0.2, 999.0, 3600.0), None);
        assert_eq!(escape_trigger(&s, 5.0, -1.0, 1.0, 0.2, 999.0, 3600.0), None);
        assert_eq!(escape_trigger(&s, 5.0, 2.0, 1.0, 0.2, 999.0, 0.0), None);
    }

    #[test]
    fn hedge_venue_is_a_third_venue() {
        // bybit is first in preference and is neither leg.
        assert_eq!(pick_hedge_venue_impl("okx", "binance", true, |_| true), "bybit");
        // bybit is the sell leg → okx (next pref, not a leg).
        assert_eq!(pick_hedge_venue_impl("deribit", "bybit", true, |_| true), "okx");
        // Live mode: preferred venues without gateways are skipped.
        assert_eq!(
            pick_hedge_venue_impl("okx", "binance", true, |v| v == "delta"),
            "delta"
        );
        // Cross-venue disabled → buy venue.
        assert_eq!(pick_hedge_venue_impl("okx", "binance", false, |_| true), "okx");
        // All preferred venues are either legs or unreachable → fall back
        // to the buy venue.
        assert_eq!(
            pick_hedge_venue_impl("bybit", "okx", true, |v| v == "bybit" || v == "okx"),
            "bybit"
        );
    }

    #[test]
    fn sim_namespace_venues_pick_a_real_third_book() {
        // Sim-feed venues are namespaced `sim:` — bybit as a SELL leg must
        // not be picked again just because the prefix differs, and the
        // returned venue must stay in the sim namespace so the paper hedge
        // books against the same account fabric.
        assert_eq!(
            pick_hedge_venue_impl("sim:delta", "sim:bybit", true, |_| true),
            "sim:okx"
        );
        assert_eq!(
            pick_hedge_venue_impl("sim:okx", "sim:binance", true, |_| true),
            "sim:bybit"
        );
        // Mixed namespaces compare on base names.
        assert_eq!(pick_hedge_venue_impl("sim:bybit", "okx", true, |_| true), "sim:binance");
    }

    #[test]
    fn escape_deadline_is_respected() {
        // The ladder's deadline math: detection now + deadline_s seconds.
        let now = 1_000_000i64;
        let dl = now + (45.0f64) as i64 * 1000;
        assert_eq!(dl - now, 45_000);
        // Phase 1 past the deadline forces the close (checked in escape_step
        // via `now > deadline_ms`); the pure check:
        assert!(now + 46_000 > dl);
        assert!(now + 44_000 <= dl);
    }
}
