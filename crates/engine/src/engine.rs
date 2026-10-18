//! The engine core: a single-writer event loop owning all quote state.
//!
//! Design notes (performance):
//! - One task owns everything; venue connectors push messages over an mpsc
//!   channel, so the hot path needs no locks at all.
//! - Bursts are drained in batches to amortize wakeups.
//! - Arbitrage detection is *incremental*: a tick for (venue, key) only
//!   re-evaluates that instrument against the other venues quoting it —
//!   O(#venues) per update instead of a full rescan.
//! - Snapshots for REST/WS readers are published through ArcSwap
//!   (wait-free reads, no locking).
//! - Compact `InstKey` (u16/u32/u64/u8) with FxHash keeps hashing cheap.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use arc_swap::ArcSwap;
use rustc_hash::{FxHashMap, FxHashSet};
use tokio::sync::{broadcast, mpsc, watch};

use ivarb_protocol::{
    ConnState, EdgeMode, EngineStats, FeedMode, HistoryPt, MultiHopRoute, Opportunity,
    ServerMsg, Snapshot, SpotQuote, VenueId, VenueStatus, WatchedLeg,
};
use ivarb_protocol::research::ChainDigestRow;

use crate::config::{EngineConfig, VenueSpec};
use crate::ga::{GaHandle, GaShared, GenomeView};
use crate::hold::HoldTracker;
use crate::key::{t_years, ymd_to_expiry_ms, InstKey, UnderlyingInterner};
use crate::msg::{now_ms, InstrTick, VenueMsg};
use crate::multihop;
use crate::ou;
use crate::perf::LatencyHist;
use crate::strategy::{derive_ivs, evaluate_pair, LegQuote, OppCore};

const MAX_BURST: usize = 2_048;

/// Strike families re-scanned per analytics pass (chunking bound).
const ANALYTICS_FAMILIES_PER_PASS: usize = 120;

pub struct EngineHandles {
    pub venue_tx: mpsc::Sender<VenueMsg>,
    pub bcast: broadcast::Sender<Arc<String>>,
    pub snapshot: Arc<ArcSwap<Snapshot>>,
    pub shutdown: watch::Sender<bool>,
    /// JSONL audit lines (consumed by the server's file writer).
    pub signal_rx: mpsc::Receiver<String>,
    /// Shared consensus spots (underlying → price) readable by connectors.
    pub spots_shared: Arc<ArcSwap<FxHashMap<String, f64>>>,
    /// Advantage-taker → engine: replace the watched-instrument set.
    pub watch_tx: mpsc::UnboundedSender<WatchCmd>,
    /// Engine → advantage-taker: live quotes for watched instruments.
    pub watched: Arc<ArcSwap<FxHashMap<String, Vec<WatchedLeg>>>>,
    /// Genetic-optimizer handle (status, commands, tape input).
    pub ga: GaHandle,
    /// Millisecond-fresh opportunity list for the executor's 1 ms scan loop
    /// (cheaper than republishing full snapshots).
    pub opps_light: Arc<ArcSwap<Vec<Opportunity>>>,
    /// Bounded near-ATM digest of every venue quote book, consumed by the
    /// research engine (agent / chains / IV analytics).
    pub research_out: Arc<ArcSwap<ivarb_protocol::research::ChainDigest>>,
}

/// Replace the executor's watched-instrument set (canonical ids like
/// `BTC-20260926-85000-C`).
#[derive(Debug)]
pub enum WatchCmd {
    Set(Vec<String>),
}

struct VenueRt {
    spec: VenueSpec,
    is_sim: bool,
    status: VenueStatus,
    quotes: FxHashMap<InstKey, LegQuote>,
    contract_size_override: Option<f64>,
    msg_window: VecDeque<i64>,
    last_msg_ms: i64,
    fatal: bool,
    /// EWMA of venue-timestamp vs arrival lag (ms).
    lag_ms_ewma: f64,
}

struct OppState {
    opp: Opportunity,
    hist: VecDeque<HistoryPt>,
    last_hist_ms: i64,
    last_log_ms: i64,
    last_ou_ms: i64,
    last_hold_ms: i64,
    last_tape_ms: i64,
    /// Kalman filter state for the spread level.
    kalman: ou::Kalman1,
}

pub struct Engine {
    cfg: Arc<EngineConfig>,
    rx: mpsc::Receiver<VenueMsg>,
    bcast: broadcast::Sender<Arc<String>>,
    snapshot: Arc<ArcSwap<Snapshot>>,
    shutdown: watch::Receiver<bool>,
    signal_tx: mpsc::Sender<String>,
    spots_shared: Arc<ArcSwap<FxHashMap<String, f64>>>,
    watch_rx: mpsc::UnboundedReceiver<WatchCmd>,
    watched_out: Arc<ArcSwap<FxHashMap<String, Vec<WatchedLeg>>>>,
    watched_keys: FxHashSet<InstKey>,

    interner: Arc<std::sync::Mutex<UnderlyingInterner>>,
    venues: FxHashMap<VenueId, VenueRt>,
    /// expiry_ms cache per key.
    expiry_ms: FxHashMap<InstKey, i64>,
    /// Per-underlying venue spot quotes for consensus.
    spots: FxHashMap<String, Vec<SpotQuote>>,
    consensus: FxHashMap<String, f64>,

    opps: FxHashMap<String, OppState>,
    dirty: FxHashSet<String>,
    retire_now: Vec<String>,
    opps_light: Arc<ArcSwap<Vec<Opportunity>>>,

    // ---- advanced analytics ----
    /// Near-miss (below-gate) candidates for the hold-to-expiry lab.
    holds: HoldTracker,
    /// Live multi-hop routes.
    multihop: Vec<MultiHopRoute>,
    /// Families (strike heads) touched since the last analytics pass.
    family_dirty: FxHashSet<InstKey>,
    /// Genome view refreshed once per second (engine-side gates etc.).
    ga_view: GenomeView,
    ga: GaHandle,
    _ga_shared: Arc<ArcSwap<GaShared>>,
    /// OU refit bookkeeping.
    ou_tick_ms: i64,

    // ---- latency telemetry ----
    pipe_hist: LatencyHist,
    wire_hist: LatencyHist,

    started: Instant,
    msgs_total: AtomicU64,
    loop_us_ewma: f64,
    instruments_tracked: u32,
    /// Research digest output (near-ATM chain export).
    research_out: Arc<ArcSwap<ivarb_protocol::research::ChainDigest>>,
}

fn log_signal_line(tx: &mpsc::Sender<String>, o: &Opportunity, event: &str, now: i64) {
    let line = format!(
        "{{\"ts\":{now},\"event\":\"{event}\",\"id\":\"{}\",\"und\":\"{}\",\"expiry\":\"{}\",\"strike\":{},\"right\":\"{}\",\"buy\":\"{}\",\"sell\":\"{}\",\"iv_buy\":{:.4},\"iv_sell\":{:.4},\"spread\":{:.4},\"fee\":{:.4},\"net\":{:.4},\"mode\":\"{}\",\"vega\":{:.2},\"est_usd\":{:.2}}}",
        o.id,
        o.underlying,
        o.expiry,
        o.strike,
        o.right.letter(),
        o.buy_venue,
        o.sell_venue,
        o.iv_buy_vp,
        o.iv_sell_vp,
        o.iv_spread_vp,
        o.fee_vp,
        o.net_vp,
        match o.edge_mode {
            EdgeMode::Exec => "exec",
            EdgeMode::Mark => "mark",
        },
        o.vega_vp_usd,
        o.est_usd,
    );
    let _ = tx.try_send(line);
}

impl Engine {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        cfg: Arc<EngineConfig>,
        rx: mpsc::Receiver<VenueMsg>,
        bcast: broadcast::Sender<Arc<String>>,
        snapshot: Arc<ArcSwap<Snapshot>>,
        shutdown: watch::Receiver<bool>,
        signal_tx: mpsc::Sender<String>,
        spots_shared: Arc<ArcSwap<FxHashMap<String, f64>>>,
        interner: Arc<std::sync::Mutex<UnderlyingInterner>>,
        watch_rx: mpsc::UnboundedReceiver<WatchCmd>,
        watched_out: Arc<ArcSwap<FxHashMap<String, Vec<WatchedLeg>>>>,
        ga: GaHandle,
        opps_light: Arc<ArcSwap<Vec<Opportunity>>>,
        research_out: Arc<ArcSwap<ivarb_protocol::research::ChainDigest>>,
    ) -> Self {
        let mut venues = FxHashMap::default();
        for spec in &cfg.venues {
            let status = VenueStatus {
                venue: spec.id.clone(),
                state: if spec.enabled {
                    ConnState::Connecting
                } else {
                    ConnState::Disabled
                },
                is_sim: false,
                instruments: 0,
                msg_rate: 0.0,
                last_msg_ms: 0,
                taker_fee_rate: spec.taker_fee_rate,
                coin_margined: spec.coin_margined,
                contract_size: spec.contract_size,
                lag_ms_ewma: 0.0,
                note: if spec.enabled {
                    None
                } else {
                    Some("disabled by config".into())
                },
            };
            venues.insert(
                spec.id.clone(),
                VenueRt {
                    spec: spec.clone(),
                    is_sim: false,
                    status,
                    quotes: FxHashMap::default(),
                    contract_size_override: None,
                    msg_window: VecDeque::new(),
                    last_msg_ms: 0,
                    fatal: false,
                    lag_ms_ewma: 0.0,
                },
            );
        }
        let ga_view = GenomeView::from_shared(&ga.shared, &ga.applied);
        let _ga_shared = ga.shared.clone();
        Self {
            cfg,
            rx,
            bcast,
            snapshot,
            shutdown,
            signal_tx,
            spots_shared,
            watch_rx,
            watched_out,
            watched_keys: FxHashSet::default(),
            interner,
            venues,
            expiry_ms: FxHashMap::default(),
            spots: FxHashMap::default(),
            consensus: FxHashMap::default(),
            opps: FxHashMap::default(),
            dirty: FxHashSet::default(),
            retire_now: Vec::new(),
            opps_light,
            holds: HoldTracker::new(),
            multihop: Vec::new(),
            family_dirty: FxHashSet::default(),
            ga_view,
            ga,
            _ga_shared,
            ou_tick_ms: 0,
            pipe_hist: LatencyHist::new(),
            wire_hist: LatencyHist::new(),
            started: Instant::now(),
            msgs_total: AtomicU64::new(0),
            loop_us_ewma: 0.0,
            instruments_tracked: 0,
            research_out,
        }
    }

    pub async fn run(mut self) {
        self.publish_snapshot();
        self.bcast_venues();

        let mut flush = tokio::time::interval(Duration::from_millis(self.cfg.broadcast_ms));
        let mut sweep = tokio::time::interval(Duration::from_millis(self.cfg.sweep_ms));
        let mut snap_tick = tokio::time::interval(Duration::from_millis(self.cfg.snapshot_ms));
        let mut mh_tick = tokio::time::interval(Duration::from_millis(self.cfg.multihop_ms));
        let mut ga_tick = tokio::time::interval(Duration::from_secs(1));
        let mut research_tick = tokio::time::interval(Duration::from_secs(4));
        flush.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        sweep.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        mh_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        ga_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        research_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                msg = self.rx.recv() => {
                    match msg {
                        Some(m) => {
                            let t0 = Instant::now();
                            self.handle(m);
                            self.drain_burst();
                            let us = t0.elapsed().as_micros() as f64;
                            self.loop_us_ewma = if self.loop_us_ewma == 0.0 { us } else { self.loop_us_ewma * 0.98 + us * 0.02 };
                            self.pipe_hist.record_us(us);
                        }
                        None => break,
                    }
                }
                Some(cmd) = self.watch_rx.recv() => self.handle_watch(cmd),
                _ = flush.tick() => self.flush_dirty(),
                _ = sweep.tick() => self.housekeeping(),
                _ = mh_tick.tick() => self.analytics_pass(),
                _ = ga_tick.tick() => {
                    self.ga_view = GenomeView::from_shared(&self.ga.shared, &self.ga.applied);
                    self.holds.set_ev_gate(self.ga_view.hold_ev_gate_usd());
                }
                _ = snap_tick.tick() => {
                    self.publish_snapshot();
                    self.broadcast(ServerMsg::Stats { stats: self.stats() });
                }
                _ = research_tick.tick() => self.publish_research_digest(),
                _ = self.shutdown.changed() => {
                    if *self.shutdown.borrow() { break; }
                }
            }
        }
        tracing::info!("engine core stopped");
    }

    fn drain_burst(&mut self) {
        let mut n = 0;
        while n < MAX_BURST {
            match self.rx.try_recv() {
                Ok(m) => {
                    self.handle(m);
                    n += 1;
                }
                Err(_) => break,
            }
        }
    }

    fn handle(&mut self, msg: VenueMsg) {
        match msg {
            VenueMsg::Register { spec, is_sim } => {
                if !self.venues.contains_key(&spec.id) {
                    self.venues.insert(
                        spec.id.clone(),
                        VenueRt {
                            status: VenueStatus {
                                venue: spec.id.clone(),
                                state: ConnState::Connecting,
                                is_sim,
                                instruments: 0,
                                msg_rate: 0.0,
                                last_msg_ms: 0,
                                taker_fee_rate: spec.taker_fee_rate,
                                coin_margined: spec.coin_margined,
                                contract_size: spec.contract_size,
                                lag_ms_ewma: 0.0,
                                note: if is_sim {
                                    Some("simulated feed".into())
                                } else {
                                    None
                                },
                            },
                            spec,
                            is_sim,
                            quotes: FxHashMap::default(),
                            contract_size_override: None,
                            msg_window: VecDeque::new(),
                            last_msg_ms: 0,
                            fatal: false,
                            lag_ms_ewma: 0.0,
                        },
                    );
                    self.bcast_venues();
                }
            }
            VenueMsg::Ticks { venue, ticks } => self.on_ticks(&venue, ticks),
            VenueMsg::Spot(sq) => self.on_spot(sq),
            VenueMsg::Status { venue, state, note } => {
                let mut changed = false;
                let mut fatal = false;
                if let Some(rt) = self.venues.get_mut(&venue) {
                    if let Some(n) = &note {
                        if n.starts_with("FATAL:") {
                            fatal = true;
                        }
                    }
                    changed = rt.status.state != state || rt.status.note != note;
                    rt.status.state = state;
                    rt.status.note = note;
                    if fatal {
                        rt.fatal = true;
                    }
                }
                if changed {
                    self.bcast_venues();
                }
            }
            VenueMsg::Tracked { venue, instruments } => {
                if let Some(rt) = self.venues.get_mut(&venue) {
                    rt.status.instruments = instruments;
                }
                self.recount_instruments();
            }
        }
    }

    fn on_ticks(&mut self, venue: &str, ticks: Vec<InstrTick>) {
        let now = now_ms();
        let spot_map = self.spots_shared.load().clone();
        let interner = self.interner.clone();
        let mut keys_to_scan: Vec<InstKey> = Vec::with_capacity(ticks.len());
        let mut went_live = false;

        {
            let rt = match self.venues.get_mut(venue) {
                Some(rt) => rt,
                None => return,
            };
            if rt.fatal {
                return;
            }
            rt.msg_window.push_back(now);
            while rt.msg_window.len() > 64 {
                rt.msg_window.pop_front();
            }
            rt.last_msg_ms = now;
            if rt.status.state != ConnState::Live {
                rt.status.state = ConnState::Live;
                rt.status.note = None;
                went_live = true;
            }

            for mut tick in ticks {
                // Venue-timestamp vs arrival lag (feed delay telemetry).
                if let Some(vts) = tick.venue_ts_ms {
                    if vts > 0 && now >= vts && now - vts < 60_000 {
                        let lag = (now - vts) as f64;
                        rt.lag_ms_ewma = if rt.lag_ms_ewma == 0.0 {
                            lag
                        } else {
                            rt.lag_ms_ewma * 0.95 + lag * 0.05
                        };
                    }
                }
                let expiry = *self
                    .expiry_ms
                    .entry(tick.key)
                    .or_insert_with(|| ymd_to_expiry_ms(tick.key.date).unwrap_or(now));
                if expiry <= now {
                    continue;
                }
                let und_name = interner
                    .lock()
                    .map(|i| i.name(tick.key.und).to_string())
                    .unwrap_or_else(|_| "?".into());
                let spot = tick
                    .spot
                    .or_else(|| self.consensus.get(&und_name).copied())
                    .or_else(|| spot_map.get(&und_name).copied())
                    .unwrap_or(0.0);
                if spot <= 0.0 {
                    continue; // no reference price yet; can't normalize
                }
                if tick.iv_mark_vp.is_none() || tick.iv_bid_vp.is_none() || tick.iv_ask_vp.is_none()
                {
                    let (strike, right) = (tick.key.strike(), tick.key.right);
                    derive_ivs(&mut tick, spot, strike, right, expiry, now);
                }
                if tick.iv_mark_vp.is_none() && tick.iv_bid_vp.is_none() && tick.iv_ask_vp.is_none()
                {
                    continue;
                }
                if let Some(cs) = tick.contract_size {
                    if cs.is_finite() && cs > 0.0 {
                        rt.contract_size_override = Some(cs);
                        // Keep the spec in sync so pair evaluation picks it up.
                        rt.spec.contract_size = cs;
                    }
                }
                keys_to_scan.push(tick.key);
                match rt.quotes.get_mut(&tick.key) {
                    Some(q) => q.update(&tick, now),
                    None => {
                        rt.quotes.insert(tick.key, LegQuote::from_tick(&tick, now));
                    }
                }
            }
        }

        if went_live {
            self.bcast_venues();
        }
        self.msgs_total.fetch_add(1, Ordering::Relaxed);
        for key in keys_to_scan {
            self.scan_key(&key, now);
        }
    }

    /// Incremental cross-venue evaluation of one instrument.
    fn scan_key(&mut self, key: &InstKey, now: i64) {
        // Mark the strike family dirty for the analytics pass (multi-hop
        // routes + hold-lab candidates are re-scanned on the mh cadence).
        self.family_dirty.insert(*key);
        let expiry = match self.expiry_ms.get(key) {
            Some(e) => *e,
            None => return,
        };
        if expiry <= now {
            return;
        }
        let und_name = self
            .interner
            .lock()
            .map(|i| i.name(key.und).to_string())
            .unwrap_or_else(|_| "?".into());

        // Phase 1 (immutable): evaluate all venue pairs for this key.
        let mut cores: Vec<OppCore> = Vec::new();
        {
            let spot = self
                .consensus
                .get(&und_name)
                .copied()
                .unwrap_or_else(|| {
                    self.venues
                        .values()
                        .find_map(|rt| rt.quotes.get(key).and_then(|q| q.spot))
                        .unwrap_or(0.0)
                });
            if spot <= 0.0 {
                return;
            }
            let mut legs: Vec<(&VenueSpec, &LegQuote)> = Vec::with_capacity(8);
            for rt in self.venues.values() {
                if rt.status.state == ConnState::Disabled || rt.fatal {
                    continue;
                }
                if let Some(q) = rt.quotes.get(key) {
                    if now - q.recv_ms <= self.cfg.stale_ms {
                        legs.push((&rt.spec, q));
                    }
                }
            }
            if legs.len() < 2 {
                return;
            }
            for i in 0..legs.len() {
                for j in (i + 1)..legs.len() {
                    if let Some(core) =
                        evaluate_pair(legs[i].0, legs[i].1, legs[j].0, legs[j].1, key, expiry, spot, now, &self.cfg)
                    {
                        cores.push(core);
                    }
                }
            }
        }

        // Phase 2 (mutable): persist opportunity state.
        for core in cores {
            self.upsert_opp(core, &und_name, expiry, now);
        }
    }

    fn upsert_opp(&mut self, mut core: OppCore, und: &str, expiry_ms: i64, now: i64) {
        // Rebuild the id with the readable underlying name: the executor's
        // watch bridge parses ids back into instruments, so the numeric
        // interner id must not leak into the wire format.
        core.id = format!(
            "{}-{}-{}-{}:{}>{}",
            und,
            core.key.date,
            crate::strategy::fmt_key_strike(core.key.strike()),
            if core.key.right == crate::key::RIGHT_CALL { "C" } else { "P" },
            core.buy_venue,
            core.sell_venue
        );
        let id = core.id.clone();
        let net = core.net_vp;
        let spot = core.spot;
        // GA-aware entry gate: the applied genome multiplies the base gate
        // (per time-of-day) before the mode multiplier.
        let hour = ((now / 3_600_000).rem_euclid(24)) as f64;
        let ga_mult = self.ga_view.gate_mult(hour);
        // Mark-mode signals (crossed-book divergences) face a stricter entry
        // gate: they are only convergence candidates, not executable edges.
        let entry_gate = match core.edge_mode {
            EdgeMode::Exec => self.cfg.min_net_vp * ga_mult,
            EdgeMode::Mark => self.cfg.min_net_vp * self.cfg.mark_gate_mult * ga_mult,
        };
        match self.opps.entry(id.clone()) {
            std::collections::hash_map::Entry::Occupied(mut e) => {
                let st = e.get_mut();
                st.opp.iv_buy_vp = core.iv_buy_vp;
                st.opp.iv_sell_vp = core.iv_sell_vp;
                st.opp.iv_spread_vp = core.iv_spread_vp;
                st.opp.fee_vp = core.fee_vp;
                st.opp.net_vp = core.net_vp;
                st.opp.edge_mode = core.edge_mode;
                st.opp.vega_vp_usd = core.vega_vp_usd;
                st.opp.est_usd = core.est_usd;
                st.opp.contract_size = core.contract_size;
                st.opp.max_units = core.max_units;
                st.opp.total_usd = core.total_usd;
                st.opp.buy_px_usd = core.buy_px_usd;
                st.opp.sell_px_usd = core.sell_px_usd;
                st.opp.size_buy = core.size_buy;
                st.opp.size_sell = core.size_sell;
                st.opp.lead_buy_ms = core.lead_buy_ms;
                st.opp.lead_sell_ms = core.lead_sell_ms;
                st.opp.drift_buy_vp_1m = core.drift_buy_vp_1m;
                st.opp.drift_sell_vp_1m = core.drift_sell_vp_1m;
                st.opp.updated_ms = core.updated_ms;
                st.opp.spot = spot;
                st.opp.t_years = t_years(now, expiry_ms);
                // Time-aware strategy refresh (bucket may shift as expiry
                // approaches — the strategy changes with the clock).
                let age_ms = (now - st.opp.first_seen_ms).max(0) as f64;
                let v = core.strategy_view_ga(&self.cfg, age_ms, now, &self.ga_view);
                st.opp.tier = v.tier;
                st.opp.bucket = v.bucket.label().to_string();
                st.opp.dte = v.dte;
                st.opp.gate_vp = v.gate_vp;
                st.opp.ev_vp = v.ev_vp;
                st.opp.theta_vp_day = v.theta_vp_day;
                st.opp.patience_s = v.patience_s;
                st.opp.hold_s = v.hold_s;
                st.opp.conv_hint = v.conv_hint;

                if net < self.cfg.exit_net_vp {
                    st.opp.retired = true;
                    st.opp.hist_pt = None;
                    log_signal_line(&self.signal_tx, &st.opp, "retire", now);
                    self.retire_now.push(id.clone());
                    self.dirty.insert(id);
                } else {
                    st.opp.retired = false;
                    let dt = now - st.last_hist_ms;
                    if dt >= self.cfg.history_min_dt_ms
                        || (dt >= 500
                            && st
                                .hist
                                .back()
                                .map(|(_, v)| (v - net).abs() >= 0.15)
                                .unwrap_or(true))
                    {
                        st.hist.push_back((now, net));
                        while st.hist.len() > self.cfg.history_len {
                            st.hist.pop_front();
                        }
                        st.last_hist_ms = now;
                        st.opp.hist_pt = Some((now, net));
                    }
                    if now - st.last_log_ms > 10_000 {
                        log_signal_line(&self.signal_tx, &st.opp, "update", now);
                        st.last_log_ms = now;
                    }
                    self.dirty.insert(id);
                }
            }
            std::collections::hash_map::Entry::Vacant(e) => {
                if net < entry_gate {
                    return; // below the entry gate — not a signal yet
                }
                let mut opp = core.to_opportunity(und, expiry_ms, now, Vec::new());
                opp.spot = spot;
                opp.t_years = t_years(now, expiry_ms);
                // Stamp the time-aware strategy fields on creation (GA-aware).
                let _v = opp_tier_stamp(&mut opp, &core, &self.cfg, now, &self.ga_view);
                let hist = VecDeque::from(vec![(now, net)]);
                e.insert(OppState {
                    opp,
                    hist,
                    last_hist_ms: now,
                    last_log_ms: now,
                    last_ou_ms: now,
                    last_hold_ms: 0,
                    last_tape_ms: now,
                    kalman: ou::Kalman1::new(1e-4, 0.05),
                });
                if let Some(st) = self.opps.get(&id) {
                    log_signal_line(&self.signal_tx, &st.opp, "new", now);
                }
                self.dirty.insert(id);
            }
        }
    }

    fn flush_dirty(&mut self) {
        let mut items: Vec<Opportunity> = Vec::with_capacity(self.dirty.len() + 8);
        for id in self.retire_now.drain(..) {
            if let Some(st) = self.opps.get(&id) {
                items.push(st.opp.clone());
            }
            // Episode over for the GA tape.
            let _ = self
                .ga
                .tape_tx
                .send(crate::ga::TapeEvent::End { id: id.clone() });
            self.opps.remove(&id);
        }
        for id in &self.dirty {
            if let Some(st) = self.opps.get(id) {
                if !st.opp.retired {
                    items.push(st.opp.clone());
                }
            }
        }
        self.dirty.clear();
        if !items.is_empty() {
            items.truncate(300);
            self.broadcast(ServerMsg::Opps { items });
        }
    }

    fn housekeeping(&mut self) {
        let now = now_ms();
        self.publish_watched(now);
        let mut venue_changes = false;
        for rt in self.venues.values_mut() {
            while rt
                .msg_window
                .front()
                .map(|t| now - *t > 10_000)
                .unwrap_or(false)
            {
                rt.msg_window.pop_front();
            }
            let rate = rt.msg_window.len() as f64 / 10.0;
            rt.status.lag_ms_ewma = rt.lag_ms_ewma;
            if (rt.status.msg_rate - rate).abs() > 0.05 {
                rt.status.msg_rate = rate;
                venue_changes = true;
            }
            if rt.status.state == ConnState::Live
                && now - rt.last_msg_ms > self.cfg.degraded_after_ms
            {
                rt.status.state = ConnState::Degraded;
                rt.status.note = Some("no data recently (feed lag or venue outage)".into());
                venue_changes = true;
            }
        }
        if venue_changes {
            self.bcast_venues();
        }

        let mut to_remove: Vec<String> = Vec::new();
        for (id, st) in self.opps.iter_mut() {
            let age = now - st.opp.updated_ms;
            let was_stale = st.opp.stale;
            st.opp.stale = age > self.cfg.stale_ms;
            if st.opp.stale && !was_stale {
                self.dirty.insert(id.clone());
            }
            if age > self.cfg.retire_ms {
                to_remove.push(id.clone());
            }
        }
        for id in to_remove {
            if let Some(st) = self.opps.get(&id) {
                log_signal_line(&self.signal_tx, &st.opp, "retire-stale", now);
            }
            let _ = self
                .ga
                .tape_tx
                .send(crate::ga::TapeEvent::End { id: id.clone() });
            self.opps.remove(&id);
            self.dirty.insert(id);
        }
    }

    fn on_spot(&mut self, sq: SpotQuote) {
        let und = sq.underlying.clone();
        let venue = sq.venue.clone();
        let now = now_ms();
        let bucket = self.spots.entry(und).or_default();
        bucket.retain(|q| q.venue != venue && now - q.ts_ms < 30_000);
        bucket.push(sq);
        self.recompute_consensus();
    }

    fn recompute_consensus(&mut self) {
        let now = now_ms();
        let mut new_c = FxHashMap::default();
        for (und, bucket) in &self.spots {
            let mut fresh: Vec<f64> = bucket
                .iter()
                .filter(|q| now - q.ts_ms < 15_000)
                .map(|q| q.price)
                .filter(|p| p.is_finite() && *p > 0.0)
                .collect();
            if fresh.is_empty() {
                continue;
            }
            fresh.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            new_c.insert(und.clone(), fresh[fresh.len() / 2]);
        }
        self.consensus = new_c;
        self.spots_shared.store(Arc::new(self.consensus.clone()));
        let spots: Vec<SpotQuote> = self
            .consensus
            .iter()
            .map(|(u, p)| SpotQuote {
                underlying: u.clone(),
                venue: "consensus".into(),
                price: *p,
                ts_ms: now,
            })
            .collect();
        self.broadcast(ServerMsg::Spots { items: spots });
    }

    fn recount_instruments(&mut self) {
        self.instruments_tracked = self.venues.values().map(|rt| rt.quotes.len() as u32).sum();
    }

    fn stats(&self) -> EngineStats {
        let venues_live = self
            .venues
            .values()
            .filter(|rt| rt.status.state == ConnState::Live)
            .count() as u32;
        let any_sim = self
            .venues
            .values()
            .any(|rt| rt.is_sim && rt.status.state == ConnState::Live);
        let venues_total = self.cfg.venues.iter().filter(|v| v.enabled).count() as u32;
        let feed_mode = if any_sim && venues_live > 0 {
            FeedMode::Mixed
        } else if any_sim {
            FeedMode::Sim
        } else {
            FeedMode::Live
        };
        let window = self.started.elapsed().as_secs_f64().max(1.0);
        let (p50, _, p99, p999) = self.pipe_hist.percentiles_us();
        let (w50, _, w99, _) = self.wire_hist.percentiles_us();
        EngineStats {
            ts_ms: now_ms(),
            uptime_s: self.started.elapsed().as_secs(),
            feed_mode,
            venues_live,
            venues_total,
            instruments_tracked: self.instruments_tracked,
            opps_active: self.opps.len() as u32,
            engine_loop_us_ewma: self.loop_us_ewma,
            msgs_total: self.msgs_total.load(Ordering::Relaxed),
            msg_rate: self.msgs_total.load(Ordering::Relaxed) as f64 / window,
            pipe_p50_us: p50,
            pipe_p99_us: p99,
            pipe_p999_us: p999,
            wire_p50_us: w50,
            wire_p99_us: w99,
            broadcast_ms: self.cfg.broadcast_ms,
            sweep_ms: self.cfg.sweep_ms,
            multihop_routes: self.multihop.len() as u32,
            holds_tracked: self.holds.len() as u32,
            ga_generation: self.ga_view.generation,
            min_net_vp: self.cfg.min_net_vp,
            exit_net_vp: self.cfg.exit_net_vp,
            moneyness_pct: self.cfg.signal_moneyness_pct,
            min_vega_usd: self.cfg.min_vega_usd,
        }
    }

    fn publish_snapshot(&mut self) {
        let now = now_ms();
        self.recount_instruments();
        let mut opps: Vec<Opportunity> = self
            .opps
            .values()
            .map(|st| {
                let mut o = st.opp.clone();
                o.history = Some(st.hist.iter().copied().collect());
                o.hist_pt = None;
                o
            })
            .collect();
        opps.sort_by(|a, b| b.updated_ms.cmp(&a.updated_ms));
        let spots: Vec<SpotQuote> = self
            .consensus
            .iter()
            .map(|(u, p)| SpotQuote {
                underlying: u.clone(),
                venue: "consensus".into(),
                price: *p,
                ts_ms: now,
            })
            .collect();
        let stats = self.stats();
        let venues: Vec<VenueStatus> =
            self.venues.values().map(|rt| rt.status.clone()).collect();
        let snap = Snapshot {
            ts_ms: now,
            stats,
            venues,
            spots,
            opportunities: opps,
            holds: self.holds.snapshot(),
            multihop: self.multihop.clone(),
        };
        self.snapshot.store(Arc::new(snap));
    }

    fn broadcast(&mut self, msg: ServerMsg) {
        let t0 = Instant::now();
        if let Ok(txt) = serde_json::to_string(&msg) {
            let _ = self.bcast.send(Arc::new(txt));
        }
        // Signal → wire latency, only measured when clients are attached
        // (a no-op send would understate the serialization cost).
        if self.bcast.receiver_count() > 0 {
            self.wire_hist.record_us(t0.elapsed().as_micros() as f64);
        }
    }

    /// Publish the bounded near-ATM chain digest for the research engine
    /// (every 4 s). Keeps only quotes fresher than 90 s, DTE 0..=90, strikes
    /// within ±12% (log) of the consensus spot, at most 5 expiries and 20
    /// strikes per (und, expiry, right) — bounded so the ArcSwap stays cheap.
    fn publish_research_digest(&self) {
        const MAX_DTE: i64 = 90;
        const MAX_EXPIRIES: usize = 5;
        const STRIKE_WINDOW: f64 = 0.22; // log-moneyness ± (covers ~25Δ wings at 30d)
        const MAX_STRIKES: usize = 26;
        const FRESH_MS: i64 = 90_000;

        let now = now_ms();
        // Venue name table (index into `venue_list`).
        let venue_list: Vec<String> = self.venues.keys().cloned().collect();
        // (und, date, right) → (|log-moneyness|, venue idx, key, quote)
        let mut buckets: rustc_hash::FxHashMap<
            (u16, u32, u8),
            Vec<(f64, usize, InstKey, &LegQuote)>,
        > = FxHashMap::default();
        let mut und_names: rustc_hash::FxHashMap<u16, String> = FxHashMap::default();
        let configured: rustc_hash::FxHashSet<&str> =
            self.cfg.underlyings.iter().map(|s| s.as_str()).collect();

        for (v_idx, (v_name, rt)) in self.venues.iter().enumerate() {
            debug_assert_eq!(venue_list.get(v_idx).map(String::as_str), Some(v_name.as_str()));
            if rt.status.state == ConnState::Disabled {
                continue;
            }
            for (key, q) in &rt.quotes {
                if now - q.recv_ms > FRESH_MS {
                    continue;
                }
                let und = match und_names.entry(key.und) {
                    std::collections::hash_map::Entry::Occupied(o) => o.get().clone(),
                    std::collections::hash_map::Entry::Vacant(v) => {
                        let name = self
                            .interner
                            .lock()
                            .ok()
                            .and_then(|i| Some(i.name(key.und).to_string()))
                            .unwrap_or_default();
                        v.insert(name.clone());
                        name
                    }
                };
                if und.is_empty() || !configured.contains(und.as_str()) {
                    continue;
                }
                let Some(spot) = self.consensus.get(&und) else {
                    continue;
                };
                if *spot <= 0.0 || !spot.is_finite() {
                    continue;
                }
                let Some(exp) = ymd_to_expiry_ms(key.date) else {
                    continue;
                };
                let dte = (exp - now) / 86_400_000;
                if !(0..=MAX_DTE).contains(&dte) {
                    continue;
                }
                // Need at least one usable number.
                if q.px_mark_unit.is_none()
                    && q.px_bid_unit.is_none()
                    && q.px_ask_unit.is_none()
                    && q.iv_mark_vp.is_none()
                {
                    continue;
                }
                let lm = (key.strike() / *spot).ln().abs();
                if lm > STRIKE_WINDOW {
                    continue;
                }
                buckets
                    .entry((key.und, key.date, key.right))
                    .or_default()
                    .push((lm, v_idx, *key, q));
            }
        }

        // Cap expiries per underlying (nearest first) and strikes per bucket.
        let mut dates_per_und: rustc_hash::FxHashMap<u16, Vec<u32>> = FxHashMap::default();
        for ((und, date, _), v) in buckets.iter_mut() {
            v.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
            v.truncate(MAX_STRIKES);
            dates_per_und.entry(*und).or_default().push(*date);
        }
        let mut keep_dates: rustc_hash::FxHashSet<(u16, u32)> = rustc_hash::FxHashSet::default();
        keep_dates.extend(
            dates_per_und
                .into_iter()
                .flat_map(|(und, mut ds)| {
                    ds.sort_unstable();
                    ds.dedup();
                    ds.truncate(MAX_EXPIRIES);
                    ds.into_iter().map(move |d| (und, d))
                })
                .collect::<Vec<_>>(),
        );

        let mut rows: Vec<ChainDigestRow> = Vec::with_capacity(1024);
        for ((und_id, date, right), v) in buckets {
            if !keep_dates.contains(&(und_id, date)) {
                continue;
            }
            let und = und_names.get(&und_id).cloned().unwrap_or_default();
            if und.is_empty() {
                continue;
            }
            for (_, v_idx, key, q) in v {
                rows.push(ChainDigestRow {
                    venue: venue_list.get(v_idx).cloned().unwrap_or_default(),
                    und: und.clone(),
                    date,
                    strike: key.strike(),
                    right: if right == crate::key::RIGHT_CALL {
                        ivarb_protocol::Right::Call
                    } else {
                        ivarb_protocol::Right::Put
                    },
                    mark_px: q.px_mark_unit,
                    bid_px: q.px_bid_unit,
                    ask_px: q.px_ask_unit,
                    iv_mark_vp: q.iv_mark_vp,
                    iv_bid_vp: q.iv_bid_vp,
                    iv_ask_vp: q.iv_ask_vp,
                    size_bid: q.size_bid,
                    size_ask: q.size_ask,
                    ts_ms: q.recv_ms,
                });
            }
        }

        let digest = ivarb_protocol::research::ChainDigest { ts_ms: now, rows };
        self.research_out.store(Arc::new(digest));
    }

    fn bcast_venues(&mut self) {
        let items: Vec<VenueStatus> = self.venues.values().map(|rt| rt.status.clone()).collect();
        self.broadcast(ServerMsg::Venues { items });
    }

    /// Periodic advanced-analytics pass, driven on the multi-hop cadence
    /// (`multihop_ms`, default 250 ms):
    ///
    /// 1. republish the millisecond-fresh light opportunity cache the
    ///    executor's 1 ms scan loop reads;
    /// 2. per-opportunity analytics — Kalman spread filter on every fresh
    ///    observation, OU half-life refits (convergence ETA), hold-to-expiry
    ///    stats on the cheap leg, GA tape samples;
    /// 3. multi-hop negative-cycle search over every strike family touched
    ///    since the last pass, plus hold-lab candidate refresh;
    /// 4. broadcast the results as dedicated WS messages.
    fn analytics_pass(&mut self) {
        let now = now_ms();
        let ga = self.ga_view;

        // ---- 1. light opps cache for the executor's 1 ms loop -------------
        let mut light: Vec<Opportunity> = Vec::with_capacity(self.opps.len());
        for st in self.opps.values() {
            let mut o = st.opp.clone();
            o.history = None;
            o.hist_pt = None;
            light.push(o);
        }
        light.sort_by(|a, b| b.net_vp.total_cmp(&a.net_vp));
        self.opps_light.store(Arc::new(light));

        // ---- 2. per-opportunity analytics ---------------------------------
        let mut tape: Vec<crate::ga::TapeEvent> = Vec::new();
        let ou_due = now - self.ou_tick_ms >= self.cfg.ou_refit_ms as i64;
        if ou_due {
            self.ou_tick_ms = now;
        }
        for (id, st) in self.opps.iter_mut() {
            // Kalman filter on each new spread observation (noise-aware TP).
            if st.opp.updated_ms > st.last_ou_ms {
                st.last_ou_ms = st.opp.updated_ms;
                let (lvl, sig) = st.kalman.update(st.opp.net_vp);
                st.opp.kalman_level_vp = Some(lvl);
                st.opp.kalman_sigma_vp = Some(sig);
            }
            // OU half-life refit (convergence ETA for the detail panel).
            if ou_due && st.hist.len() >= 16 {
                let samples: Vec<(i64, f64)> = st.hist.iter().copied().collect();
                if let Some(hl) = ou::half_life_s(&samples) {
                    if hl.is_finite() && hl > 0.0 && hl < 86_400.0 * 30.0 {
                        st.opp.conv_eta_s = Some(hl);
                    }
                }
            }
            // Hold-to-expiry view of the cheap leg, throttled per opportunity.
            if now - st.last_hold_ms >= self.cfg.holds_ms as i64
                && st.opp.buy_px_usd.is_some()
            {
                st.last_hold_ms = now;
                st.opp.hold = Some(crate::hold::hold_stats_for_opp(
                    st.opp.spot,
                    st.opp.strike,
                    st.opp.t_years,
                    st.opp.right == ivarb_protocol::Right::Call,
                    st.opp.buy_px_usd.unwrap_or(0.0) / st.opp.contract_size.max(1e-9),
                    st.opp.fee_vp,
                    st.opp.vega_vp_usd / st.opp.contract_size.max(1e-9),
                    ((st.opp.iv_buy_vp + st.opp.iv_sell_vp) * 0.5 / 100.0).max(1e-3),
                    st.opp.contract_size,
                ));
            }
            // GA tape sample (≤ 1 per 2 s per episode keeps the tape compact).
            if now - st.last_tape_ms >= 2_000 && !st.opp.retired {
                st.last_tape_ms = now;
                tape.push(crate::ga::TapeEvent::Sample {
                    id: id.clone(),
                    s: crate::ga::TapeSample {
                        t_ms: now,
                        net_vp: st.opp.net_vp,
                        fee_vp: st.opp.fee_vp,
                        theta_vp_day: st.opp.theta_vp_day,
                        dte: st.opp.dte,
                        exec_mode: matches!(st.opp.edge_mode, EdgeMode::Exec),
                        quote_age_ms: (now - st.opp.updated_ms).max(0) as f64,
                        gate_base: self.cfg.min_net_vp,
                    },
                });
            }
        }
        for ev in tape {
            let _ = self.ga.tape_tx.send(ev);
        }

        // ---- 3. family scan: multi-hop routes + hold candidates ------------
        // Chunked: at most ANALYTICS_FAMILIES_PER_PASS strike families are
        // re-scanned per pass (the rest stay dirty for the next one) so the
        // synchronous analytics never blocks the millisecond engine loop.
        if !self.family_dirty.is_empty() {
            let keys: Vec<InstKey> = if self.family_dirty.len() > ANALYTICS_FAMILIES_PER_PASS {
                let all: Vec<InstKey> = self.family_dirty.drain().collect();
                for k in &all[ANALYTICS_FAMILIES_PER_PASS..] {
                    self.family_dirty.insert(*k);
                }
                all[..ANALYTICS_FAMILIES_PER_PASS].to_vec()
            } else {
                self.family_dirty.drain().collect()
            };
            let params = multihop::MultihopParams {
                fee_buffer: ga.mh_fee_buffer(),
                ..Default::default()
            };
            let mut routes: Vec<MultiHopRoute> = Vec::new();
            let und_names: Vec<String> = {
                let ig = self.interner.lock();
                match ig {
                    Ok(i) => i.names.clone(),
                    Err(_) => Vec::new(),
                }
            };
            for key in keys {
                let sib = InstKey {
                    und: key.und,
                    date: key.date,
                    strike_ticks: key.strike_ticks,
                    right: if key.right == crate::key::RIGHT_CALL {
                        crate::key::RIGHT_PUT
                    } else {
                        crate::key::RIGHT_CALL
                    },
                };
                let call_key = if key.right == crate::key::RIGHT_CALL {
                    key
                } else {
                    sib
                };
                let expiry = match ymd_to_expiry_ms(key.date) {
                    Some(e) => e,
                    None => continue,
                };
                if expiry <= now {
                    continue;
                }
                let und_name = und_names
                    .get(key.und as usize)
                    .cloned()
                    .unwrap_or_else(|| "?".into());
                let spot = self
                    .consensus
                    .get(&und_name)
                    .copied()
                    .unwrap_or_else(|| {
                        self.venues
                            .values()
                            .find_map(|rt| rt.quotes.get(&key).and_then(|q| q.spot))
                            .unwrap_or(0.0)
                    });
                if spot <= 0.0 {
                    continue;
                }

                // Gather per-venue call/put legs (fresh only).
                let mut call_legs: Vec<multihop::VenueLeg> = Vec::with_capacity(8);
                let mut put_legs: Vec<multihop::VenueLeg> = Vec::with_capacity(8);
                let mut hold_call: Vec<(&str, f64, f64, &LegQuote)> = Vec::with_capacity(8);
                let mut hold_put: Vec<(&str, f64, f64, &LegQuote)> = Vec::with_capacity(8);
                for rt in self.venues.values() {
                    if rt.status.state == ConnState::Disabled || rt.fatal {
                        continue;
                    }
                    let vid = rt.spec.id.as_str();
                    let fee = rt.spec.taker_fee_rate;
                    let cs = rt.spec.contract_size;
                    if let Some(q) = rt.quotes.get(&call_key) {
                        if now - q.recv_ms <= self.cfg.stale_ms {
                            call_legs.push(multihop::VenueLeg {
                                venue: vid,
                                fee_rate: fee,
                                contract_size: cs,
                                quote: q,
                            });
                            hold_call.push((vid, fee, cs, q));
                        }
                    }
                    if let Some(q) = rt.quotes.get(&sib) {
                        if now - q.recv_ms <= self.cfg.stale_ms {
                            put_legs.push(multihop::VenueLeg {
                                venue: vid,
                                fee_rate: fee,
                                contract_size: cs,
                                quote: q,
                            });
                            hold_put.push((vid, fee, cs, q));
                        }
                    }
                }
                if call_legs.len() + put_legs.len() < 2 {
                    continue;
                }

                // Multi-hop negative-cycle search on this strike family.
                if call_legs.len() + put_legs.len() >= 2 {
                    let fams = multihop::build_families(&call_legs, &put_legs);
                    let found = multihop::scan_family(
                        &call_key,
                        &fams,
                        spot,
                        now,
                        self.cfg.stale_ms,
                        &params,
                        &und_name,
                        self.cfg.multihop_max_units,
                    );
                    routes.extend(found);
                }

                // Hold-lab candidates (near-miss divergences below the gate).
                self.holds
                    .refresh(&call_key, &und_name, &hold_call, spot, expiry, now, self.cfg.stale_ms);
                self.holds
                    .refresh(&sib, &und_name, &hold_put, spot, expiry, now, self.cfg.stale_ms);
            }
            // Merge: routes from families NOT re-scanned this pass survive
            // (their quotes are unchanged or will be refreshed on a later
            // pass), families that were re-scanned get their routes replaced.
            let mut scanned: FxHashSet<String> = FxHashSet::default();
            let mut merged: Vec<MultiHopRoute> = Vec::with_capacity(routes.len() + 8);
            for r in routes {
                scanned.insert(r.id.split(":MH").next().unwrap_or("").to_string());
                merged.push(r);
            }
            for old_r in self.multihop.drain(..) {
                let fam = old_r.id.split(":MH").next().unwrap_or("").to_string();
                if !scanned.contains(&fam) {
                    merged.push(old_r);
                }
            }
            merged.sort_by(|a, b| b.profit_unit_usd.total_cmp(&a.profit_unit_usd));
            merged.truncate(60);
            self.multihop = merged;
            let items = self.multihop.clone();
            self.broadcast(ServerMsg::Multihop { items });
        }

        // Hold-lab housekeeping + publish.
        self.holds.housekeeping(now, self.cfg.stale_ms);
        if !self.holds.is_empty() {
            let items = self.holds.snapshot();
            self.broadcast(ServerMsg::Holds { items });
        }
    }

    /// Replace the watched-instrument set published for the advantage taker.
    fn handle_watch(&mut self, cmd: WatchCmd) {
        let WatchCmd::Set(ids) = cmd;
        let mut keys = FxHashSet::default();
        for id in ids {
            if let Some((u, d, k, r)) = crate::key::parse_option_symbol(&id) {
                let und = self
                    .interner
                    .lock()
                    .map(|mut i| i.intern(&u))
                    .unwrap_or(0);
                keys.insert(InstKey {
                    und,
                    date: d,
                    strike_ticks: (k * 10_000.0).round() as u64,
                    right: r,
                });
            }
        }
        self.watched_keys = keys;
        self.publish_watched(now_ms());
    }

    /// Publish live quotes for watched instruments (called every sweep).
    fn publish_watched(&mut self, now: i64) {
        if self.watched_keys.is_empty() {
            if !self.watched_out.load().is_empty() {
                self.watched_out.store(Arc::new(FxHashMap::default()));
            }
            return;
        }
        let mut out: FxHashMap<String, Vec<WatchedLeg>> = FxHashMap::default();
        let und_names: Vec<String> = {
            let ig = self.interner.lock();
            match ig {
                Ok(i) => i.names.clone(),
                Err(_) => Vec::new(),
            }
        };
        for key in &self.watched_keys {
            let und = und_names
                .get(key.und as usize)
                .cloned()
                .unwrap_or_else(|| "?".into());
            let id = format!(
                "{}-{}-{}-{}",
                und,
                key.date,
                crate::strategy::fmt_key_strike(key.strike()),
                if key.right == crate::key::RIGHT_CALL {
                    "C"
                } else {
                    "P"
                }
            );
            let mut legs: Vec<WatchedLeg> = Vec::with_capacity(4);
            for rt in self.venues.values() {
                if rt.status.state == ConnState::Disabled || rt.fatal {
                    continue;
                }
                if let Some(q) = rt.quotes.get(key) {
                    legs.push(WatchedLeg {
                        venue: rt.spec.id.clone(),
                        px_bid_unit: q.px_bid_unit,
                        px_ask_unit: q.px_ask_unit,
                        px_mark_unit: q.px_mark_unit,
                        iv_bid_vp: q.iv_bid_vp,
                        iv_ask_vp: q.iv_ask_vp,
                        iv_mark_vp: q.iv_mark_vp,
                        size_bid: q.size_bid,
                        size_ask: q.size_ask,
                        contract_size: rt.spec.contract_size,
                        recv_ms: q.recv_ms,
                    });
                }
            }
            let _ = now;
            if !legs.is_empty() {
                out.insert(id, legs);
            }
        }
        self.watched_out.store(Arc::new(out));
    }
}

/// Stamp time-aware strategy fields onto a freshly created opportunity.
fn opp_tier_stamp(
    opp: &mut Opportunity,
    core: &OppCore,
    cfg: &EngineConfig,
    now: i64,
    ga: &GenomeView,
) -> crate::timing::StrategyView {
    let v = core.strategy_view_ga(cfg, 0.0, now, ga);
    opp.tier = v.tier;
    opp.bucket = v.bucket.label().to_string();
    opp.dte = v.dte;
    opp.gate_vp = v.gate_vp;
    opp.ev_vp = v.ev_vp;
    opp.theta_vp_day = v.theta_vp_day;
    opp.patience_s = v.patience_s;
    opp.hold_s = v.hold_s;
    opp.conv_hint = v.conv_hint;
    v
}
