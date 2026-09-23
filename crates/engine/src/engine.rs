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
    ConnState, EdgeMode, EngineStats, FeedMode, HistoryPt, Opportunity, ServerMsg, Snapshot,
    SpotQuote, VenueId, VenueStatus, WatchedLeg,
};

use crate::config::{EngineConfig, VenueSpec};
use crate::key::{t_years, ymd_to_expiry_ms, InstKey, UnderlyingInterner};
use crate::msg::{now_ms, InstrTick, VenueMsg};
use crate::strategy::{derive_ivs, evaluate_pair, LegQuote, OppCore};

const MAX_BURST: usize = 2_048;

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
}

struct OppState {
    opp: Opportunity,
    hist: VecDeque<HistoryPt>,
    last_hist_ms: i64,
    last_log_ms: i64,
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

    started: Instant,
    msgs_total: AtomicU64,
    loop_us_ewma: f64,
    instruments_tracked: u32,
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
                },
            );
        }
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
            started: Instant::now(),
            msgs_total: AtomicU64::new(0),
            loop_us_ewma: 0.0,
            instruments_tracked: 0,
        }
    }

    pub async fn run(mut self) {
        self.publish_snapshot();
        self.bcast_venues();

        let mut flush = tokio::time::interval(Duration::from_millis(self.cfg.broadcast_ms));
        let mut sweep = tokio::time::interval(Duration::from_millis(self.cfg.sweep_ms));
        let mut snap_tick = tokio::time::interval(Duration::from_millis(self.cfg.snapshot_ms));
        flush.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        sweep.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

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
                        }
                        None => break,
                    }
                }
                Some(cmd) = self.watch_rx.recv() => self.handle_watch(cmd),
                _ = flush.tick() => self.flush_dirty(),
                _ = sweep.tick() => self.housekeeping(),
                _ = snap_tick.tick() => {
                    self.publish_snapshot();
                    self.broadcast(ServerMsg::Stats { stats: self.stats() });
                }
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
        // Mark-mode signals (crossed-book divergences) face a stricter entry
        // gate: they are only convergence candidates, not executable edges.
        let entry_gate = match core.edge_mode {
            EdgeMode::Exec => self.cfg.min_net_vp,
            EdgeMode::Mark => self.cfg.min_net_vp * self.cfg.mark_gate_mult,
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
                let v = core.strategy_view(&self.cfg, age_ms, now);
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
                // Stamp the time-aware strategy fields on creation.
                let _v = opp_tier_stamp(&mut opp, &core, &self.cfg, now);
                let hist = VecDeque::from(vec![(now, net)]);
                e.insert(OppState {
                    opp,
                    hist,
                    last_hist_ms: now,
                    last_log_ms: now,
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
        };
        self.snapshot.store(Arc::new(snap));
    }

    fn broadcast(&self, msg: ServerMsg) {
        if let Ok(txt) = serde_json::to_string(&msg) {
            let _ = self.bcast.send(Arc::new(txt));
        }
    }

    fn bcast_venues(&self) {
        let items: Vec<VenueStatus> = self.venues.values().map(|rt| rt.status.clone()).collect();
        self.broadcast(ServerMsg::Venues { items });
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
) -> crate::timing::StrategyView {
    let v = core.strategy_view(cfg, 0.0, now);
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
