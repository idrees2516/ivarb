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

pub mod live;
pub mod machine;
pub mod paper;
pub mod risk;

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use rustc_hash::{FxHashMap, FxHashSet};
use tokio::sync::{broadcast, mpsc, watch};

use ivarb_engine::engine::WatchCmd;
use ivarb_engine::timing::Regime;
use ivarb_protocol::{
    ExecCommand, ExecEvent, ExecLevel, ExecMode, ExecSettings, ExecSnapshot, ExecStats,
    ExitReason, Opportunity, Position, PositionEntry, PositionMtm, Right, Snapshot, TimeRegime,
    Trade, WatchedLeg,
};

use machine::{AttemptCtx, AttemptOutcome, PositionDraft};
use paper::PaperBroker;
use risk::{canonical_inst, RiskState};

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
}

struct Core {
    deps: ExecDeps,
    out: Arc<ArcSwap<ExecSnapshot>>,
    cmd_rx: mpsc::UnboundedReceiver<ExecCommand>,
    attempt_tx: mpsc::Sender<(u64, AttemptOutcome)>,
    attempt_rx: mpsc::Receiver<(u64, AttemptOutcome)>,
    event_tx: mpsc::UnboundedSender<ExecEvent>,
    event_rx: mpsc::UnboundedReceiver<ExecEvent>,
    broker: Arc<PaperBroker>,
    settings: ExecSettings,
    armed: bool,
    kill: Option<String>,
    mode: ExecMode,
    positions: FxHashMap<u64, Position>,
    /// attempt id → (opp, reserved units) for release accounting.
    live_attempts: FxHashMap<u64, (Opportunity, f64)>,
    trades: VecDeque<Trade>,
    events: VecDeque<ExecEvent>,
    stats: Stats,
    risk: RiskState,
    next_id: u64,
    watch_set: FxHashSet<String>,
    watch_dirty: bool,
    dirty: bool,
}

/// Boot the advantage taker.
pub fn spawn(deps: ExecDeps) -> ExecutorHandle {
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<ExecCommand>();
    let (attempt_tx, attempt_rx) = mpsc::channel::<(u64, AttemptOutcome)>(64);
    let (event_tx, event_rx) = mpsc::unbounded_channel::<ExecEvent>();

    let venue_ids: Vec<String> = deps
        .snapshot
        .load()
        .venues
        .iter()
        .map(|v| v.venue.clone())
        .collect();
    let (mode, _keys) = live::live_mode_from_env();

    let out = Arc::new(ArcSwap::from_pointee(empty_snapshot(mode)));

    let mut core = Core {
        broker: Arc::new(PaperBroker::new(&venue_ids, 250_000.0)),
        settings: ExecSettings::default(),
        armed: false,
        kill: None,
        mode,
        positions: FxHashMap::default(),
        live_attempts: FxHashMap::default(),
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
        },
        risk: RiskState::new(),
        next_id: 1,
        watch_set: FxHashSet::default(),
        watch_dirty: false,
        dirty: true,
        deps,
        out: out.clone(),
        cmd_rx,
        attempt_tx,
        attempt_rx,
        event_tx,
        event_rx,
    };

    tokio::spawn(async move {
        let mut scan = tokio::time::interval(Duration::from_millis(250));
        let mut monitor = tokio::time::interval(Duration::from_secs(1));
        let mut push = tokio::time::interval(Duration::from_millis(750));
        scan.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        monitor.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
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
                ev = core.event_rx.recv() => {
                    if let Some(e) = ev {
                        core.events.push_back(e);
                        while core.events.len() > 240 { core.events.pop_front(); }
                        core.dirty = true;
                    }
                }
                _ = scan.tick() => core.scan(),
                _ = monitor.tick() => core.monitor(),
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
    }
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
    // scanner
    // ------------------------------------------------------------------
    fn scan(&mut self) {
        if !self.armed || self.kill.is_some() {
            return;
        }
        if self.live_attempts.len() as u32 >= self.settings.max_concurrent_attempts {
            return;
        }
        let snap = self.deps.snapshot.load();
        let now = now_ms();
        let mut unrealized = 0.0;
        for p in self.positions.values() {
            unrealized += p.mtm.unrealized_usd;
        }
        // Rank candidates by EV, dedup by instrument (best pair per contract).
        let mut best: FxHashMap<String, &Opportunity> = FxHashMap::default();
        for o in snap.opportunities.iter() {
            if o.stale || o.retired {
                continue;
            }
            if !matches!(o.tier, ivarb_protocol::Tier::Sniper | ivarb_protocol::Tier::Convergence) {
                continue;
            }
            let inst = canonical_inst(o);
            let better = best.get(&inst).map(|cur| o.ev_vp > cur.ev_vp).unwrap_or(true);
            if better {
                best.insert(inst, o);
            }
        }
        let mut cands: Vec<&Opportunity> = best.into_values().collect();
        cands.sort_by(|a, b| b.ev_vp.partial_cmp(&a.ev_vp).unwrap_or(std::cmp::Ordering::Equal));

        for o in cands {
            if self.live_attempts.len() as u32 >= self.settings.max_concurrent_attempts {
                break;
            }
            if o.ev_vp <= 0.05 {
                continue;
            }
            if let Ok(units) = self.risk.preflight(
                o,
                &snap,
                &self.settings,
                self.armed,
                &self.kill,
                self.stats.realized_usd,
                unrealized,
                now,
            ) {
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
        // Manual takes bypass the armed flag but respect every risk gate.
        match self.risk.preflight(&o, &snap, &self.settings, true, &self.kill, self.stats.realized_usd, unrealized, now) {
            Ok(auto_units) => {
                let units = units.unwrap_or(auto_units).min(auto_units).max(0.01);
                self.ev(Some(o.id.clone()), ExecLevel::Warn, "manual", format!("manual TAKE — spawning attempt with {units:.2} units"));
                self.spawn_attempt(o, units);
            }
            Err(reason) => {
                self.ev(Some(o.id.clone()), ExecLevel::Error, "manual", format!("manual TAKE rejected by risk gates: {reason}"));
            }
        }
    }

    fn spawn_attempt(&mut self, opp: Opportunity, units: f64) {
        let attempt = self.next_id;
        self.next_id += 1;
        self.stats.attempts_total += 1;
        self.risk.reserve(&opp, units);
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
                ivarb_protocol::Tier::Monitor => AttemptOutcome::Aborted { reason: "monitor tier".into(), cooldown_ms: 0 },
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
        let pos = Position {
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
            // We take profit when the mid spread converges to ~25% of the
            // signal, stop when it inverts past it, and cap theta burn.
            exit_take_vp: (self.settings.take_profit_frac * opp.net_vp.max(0.0)).max(0.05),
            exit_stop_vp: -(self.settings.stop_mult * opp.net_vp.max(0.0)).max(0.10),
            theta_budget_vp: (self.settings.theta_budget_frac * opp.net_vp.max(0.0)).max(0.10),
            max_hold_s: opp.hold_s.max(60.0),
            hedge_units: 0.0,
            hedge_avg_px: 0.0,
            closing: false,
        };
        self.positions.insert(id, pos);
        self.risk.promote();
        self.stats.fills_total += 2;
        self.dirty = true;
    }

    // ------------------------------------------------------------------
    // monitor: MTM, exits, hedging
    // ------------------------------------------------------------------
    fn monitor(&mut self) {
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
        let mut hedges: Vec<(u64, paper::Side, f64, f64)> = Vec::new();
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
                if self.settings.hedge_enabled && spot > 0.0 {
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
                    let target_hedge = -residual;
                    let delta_units = target_hedge - p.hedge_units;
                    if delta_units.abs() * spot > self.settings.hedge_threshold_usd {
                        hedges.push((*id, if delta_units > 0.0 { paper::Side::Buy } else { paper::Side::Sell }, delta_units.abs(), spot));
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

        // Apply hedges (sync fills — the paper broker's hedge_sync is instant).
        for (id, side, amt, spot) in hedges {
            if let Some(p) = self.positions.get_mut(&id) {
                let (px, _fee) = self.broker.hedge_sync(&p.buy_venue, side, amt, spot);
                let delta_units = if side == paper::Side::Buy { amt } else { -amt };
                let new_units = p.hedge_units + delta_units;
                p.hedge_avg_px = if new_units.abs() > 1e-9 {
                    (p.hedge_avg_px * p.hedge_units + px * delta_units) / new_units
                } else {
                    0.0
                };
                p.hedge_units = new_units;
                p.mtm.hedge_pnl_usd = p.hedge_units * (spot - p.hedge_avg_px);
            }
        }

        for (id, reason) in to_close {
            self.close_position(id, reason);
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

    fn close_position(&mut self, id: u64, reason: ExitReason) {
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
        let (sell_px, mut exit_fees) = match buy_leg
            .and_then(|l| l.px_bid_unit)
            .filter(|x| sane(*x, p.entry.buy_px_usd))
        {
            Some(bid) => {
                let f = self.broker.take_sync(
                    &p.buy_venue, paper::Side::Sell, bid,
                    buy_leg.and_then(|l| l.size_bid).unwrap_or(1.0),
                    p.units / p.contract_size_buy.max(1e-9), buy_fee, spot, p.contract_size_buy,
                    180,
                );
                (f.px_unit, f.fee_usd)
            }
            _ => (
                buy_leg
                    .and_then(|l| l.px_mark_unit)
                    .filter(|x| *x > 0.0)
                    .unwrap_or(p.entry.buy_px_usd),
                0.0,
            ), // degrade: exit at mark (or entry) when the book is dust
        };
        // Buy back the short leg at its venue's ask.
        let (buyback_px, fee2) = match sell_leg
            .and_then(|l| l.px_ask_unit)
            .filter(|x| sane(*x, p.entry.sell_px_usd))
        {
            Some(ask) => {
                let f = self.broker.take_sync(
                    &p.sell_venue, paper::Side::Buy, ask,
                    sell_leg.and_then(|l| l.size_ask).unwrap_or(1.0),
                    p.units / p.contract_size_sell.max(1e-9), sell_fee, spot, p.contract_size_sell,
                    180,
                );
                (f.px_unit, f.fee_usd)
            }
            _ => (
                sell_leg
                    .and_then(|l| l.px_mark_unit)
                    .filter(|x| *x > 0.0)
                    .unwrap_or(p.entry.sell_px_usd),
                0.0,
            ),
        };
        exit_fees += fee2;

        // Unwind the residual-delta hedge.
        let mut hedge_exit_pnl = 0.0;
        if p.hedge_units.abs() > 1e-9 {
            let side = if p.hedge_units > 0.0 { paper::Side::Sell } else { paper::Side::Buy };
            let (px, _fee) = self.broker.hedge_sync(&p.buy_venue, side, p.hedge_units.abs(), spot);
            hedge_exit_pnl = p.hedge_units * (px - p.hedge_avg_px);
            p.hedge_units = 0.0;
        }

        let legs_pnl = (sell_px - p.entry.buy_px_usd) * p.units
            + (p.entry.sell_px_usd - buyback_px) * p.units;
        let realized = legs_pnl - p.entry.fees_usd - exit_fees + p.mtm.hedge_pnl_usd + hedge_exit_pnl;
        let realized_vp = realized / (p.units * vega_unit).max(1e-9);
        let hold_s = (now - p.entry.t_ms) as f64 / 1000.0;

        self.stats.realized_usd += realized;
        self.stats.fees_usd += p.entry.fees_usd + exit_fees;
        self.stats.trades_closed += 1;
        self.stats.capture_sum += (realized_vp / p.entry.signal_net_vp).clamp(-5.0, 5.0);
        self.stats.capture_n += 1.0;
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
            fees_usd: p.entry.fees_usd + exit_fees,
            hedge_pnl_usd: p.mtm.hedge_pnl_usd + hedge_exit_pnl,
            hold_s,
            exit_reason: reason,
            opened_ms: p.entry.t_ms,
            closed_ms: now,
        };
        self.trades.push_front(trade);
        while self.trades.len() > 50 {
            self.trades.pop_back();
        }
        let reason_s = match reason {
            ExitReason::TakeProfit => "spread converged",
            ExitReason::Stop => "stop — spread inverted",
            ExitReason::ThetaBudget => "theta budget exhausted",
            ExitReason::MaxHold => "max hold window",
            ExitReason::StaleQuotes => "quotes went stale",
            ExitReason::Manual => "manual close",
            ExitReason::Killed => "kill switch",
            ExitReason::Expiry => "expiry approaching",
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
        };
        if let Ok(txt) = serde_json::to_string(&ivarb_protocol::ServerMsg::Exec { exec: snapshot.clone() }) {
            let _ = self.deps.bcast.send(Arc::new(txt));
        }
        self.out.store(Arc::new(snapshot));
        self.dirty = false;
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

fn fmt_dur(s: f64) -> String {
    if s < 90.0 {
        format!("{s:.0}s")
    } else if s < 5400.0 {
        format!("{:.0}m", s / 60.0)
    } else {
        format!("{:.1}h", s / 3600.0)
    }
}
