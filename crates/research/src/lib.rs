//! ivarb-research: the agentic research data plane.
//!
//! Consumes the engine's bounded chain digest, enriches it with venue REST
//! stats (OI / volume), underlying candles and order-book depth, consolidates
//! everything into one cross-venue book, and serves:
//! - REST read models (pulse / chain / candles / IV / liquidity / status)
//! - the option builder (market-coherent structure synthesis)
//! - a tool-calling agent (deterministic desk brain by default, any
//!   OpenAI-compatible LLM via `IVARB_LLM_*` env vars)

pub mod agent;
pub mod analytics;
pub mod consolidate;
pub mod derive;
pub mod enrich;
pub mod paradex;
pub mod sbe;
pub mod sim;
pub mod suggest;
pub mod ticks;

use std::collections::VecDeque;
use std::sync::Arc;

use arc_swap::ArcSwap;
use rustc_hash::FxHashMap;
use tokio::sync::watch;

use ivarb_protocol::research::{
    AgentRun, CandleSeries, ChainDigest, ChainDigestRow, ChainResponse, ChainRow, IvAnalytics,
    LiquidityCell, LiquidityMap, ResearchPulse, ResearchStatus, ResearchVenueStat, SpotSummary,
    SuggestionCard, TickFeedStatus, TickQuote, TicksResponse,
};
use ivarb_protocol::{Right, Snapshot};

pub use agent::LlmConfig;
pub use consolidate::ConsolidatedBook;
pub use suggest::SuggestRequest;

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Wiring from the engine + server into the research plane.
pub struct ResearchDeps {
    /// Engine tap: bounded near-ATM chain digest (refreshed every 4 s).
    pub digest: Arc<ArcSwap<ChainDigest>>,
    /// Engine snapshot: consensus spots + venue connection states.
    pub snapshot: Arc<ArcSwap<Snapshot>>,
    pub shutdown: watch::Receiver<bool>,
    /// Underlyings to track (engine config list).
    pub underlyings: Vec<String>,
}

struct Inner {
    book: ArcSwap<ConsolidatedBook>,
    candles: ArcSwap<FxHashMap<(String, String), CandleSeries>>,
    liquidity: ArcSwap<FxHashMap<String, LiquidityMap>>,
    iv_history: std::sync::Mutex<FxHashMap<String, VecDeque<f64>>>,
    last_hist_ms: std::sync::atomic::AtomicI64,
    enrich: std::sync::Mutex<enrich::EnrichStore>,
    paradex: std::sync::Mutex<paradex::ParadexState>,
    /// Live quote feed from the public (JWT-free) Paradex WS.
    paradex_live: Arc<paradex::ParadexLive>,
    /// Tick-level BBO plane (Paradex 50 ms books + Derive v3 tickers).
    ticks: Arc<ticks::TickPlane>,
    /// Desired Paradex market symbols for the 50 ms book feed.
    paradex_desired: watch::Sender<Vec<String>>,
    /// Desired Derive v3 instrument names for the ticker feed.
    derive_desired: watch::Sender<Vec<String>>,
    /// Derive v3 discovery cache: canonical instrument → raw name.
    derive_names: std::sync::Mutex<FxHashMap<String, String>>,
    derive_names_ms: std::sync::atomic::AtomicI64,
    /// Boot time — denominator for feed-rate stats.
    boot_ms: i64,
    llm: Option<LlmConfig>,
    http: reqwest::Client,
    underlyings: Vec<String>,
}

#[derive(Clone)]
pub struct ResearchHandle {
    inner: Arc<Inner>,
}

/// Boot the research plane: consolidation + enrichment + klines + books +
/// paradex loops, all fail-soft and shutdown-aware.
pub fn spawn(deps: ResearchDeps) -> ResearchHandle {
    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .user_agent("ivarb-research/0.1")
        .build()
        .expect("reqwest client");
    let llm = LlmConfig::from_env();
    if llm.is_some() {
        tracing::info!(model = %llm.as_ref().unwrap().model, "research agent LLM brain configured");
    } else {
        tracing::info!("research agent using the deterministic desk brain (set IVARB_LLM_* for an LLM brain)");
    }
    let underlyings = if deps.underlyings.is_empty() {
        vec!["BTC".into(), "ETH".into(), "SOL".into()]
    } else {
        deps.underlyings
    };

    // Paradex quotes: public WS (SBE) — no JWT, no 5-minute expiry.
    // (The REST path needs a JWT that dies every 5 min by design; the public
    // `markets_summary.ALL` channel is unauthenticated and covers the whole
    // exchange, so it replaces the authenticated poller entirely.)
    let paradex_live = Arc::new(paradex::ParadexLive::default());
    paradex::spawn_ws(paradex_live.clone(), deps.shutdown.clone());

    // Tick-level BBO plane: Paradex 50 ms order-book snapshots + Derive v3
    // 100 ms tickers. Desired sets start with the perps (instant liveness)
    // and are recomputed against the digest every 45 s.
    let ticks = Arc::new(ticks::TickPlane::default());
    let (paradex_desired, paradex_desired_rx) = watch::channel(
        underlyings.iter().map(|u| paradex::perp_symbol(u)).collect::<Vec<_>>(),
    );
    let (derive_desired, derive_desired_rx) = watch::channel(
        underlyings.iter().map(|u| derive::perp_name(u)).collect::<Vec<_>>(),
    );
    paradex::spawn_book_ws(ticks.clone(), paradex_desired_rx, deps.shutdown.clone());
    derive::spawn_v3(ticks.clone(), derive_desired_rx, deps.shutdown.clone());

    let inner = Arc::new(Inner {
        book: ArcSwap::from_pointee(ConsolidatedBook::default()),
        candles: ArcSwap::from_pointee(FxHashMap::default()),
        liquidity: ArcSwap::from_pointee(FxHashMap::default()),
        iv_history: std::sync::Mutex::new(FxHashMap::default()),
        last_hist_ms: std::sync::atomic::AtomicI64::new(0),
        enrich: std::sync::Mutex::new(enrich::EnrichStore::default()),
        paradex: std::sync::Mutex::new(paradex::ParadexState::default()),
        paradex_live,
        ticks,
        paradex_desired,
        derive_desired,
        derive_names: std::sync::Mutex::new(FxHashMap::default()),
        derive_names_ms: std::sync::atomic::AtomicI64::new(0),
        boot_ms: now_ms(),
        llm,
        http,
        underlyings,
    });

    // ---- consolidation loop (6 s) ----
    {
        let inner = inner.clone();
        let digest = deps.digest.clone();
        let snapshot = deps.snapshot.clone();
        let mut shutdown = deps.shutdown.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(6));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    _ = tick.tick() => consolidate_once(&inner, &digest, &snapshot).await,
                    _ = shutdown.changed() => {
                        if *shutdown.borrow() { break; }
                    }
                }
            }
        });
    }

    // ---- enrichment loop (30 s) ----
    {
        let inner = inner.clone();
        let mut shutdown = deps.shutdown.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(30));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    _ = tick.tick() => {
                        let unds = inner.underlyings.clone();
                        let sweep = enrich::enrichment_sweep(&inner.http, &unds).await;
                        if let Ok(mut st) = inner.enrich.lock() {
                            enrich::apply_sweep(&mut st, sweep);
                        }
                    }
                    _ = shutdown.changed() => {
                        if *shutdown.borrow() { break; }
                    }
                }
            }
        });
    }

    // ---- klines loop (60 s) ----
    {
        let inner = inner.clone();
        let mut shutdown = deps.shutdown.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(60));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            // First fetch immediately.
            klines_once(&inner).await;
            loop {
                tokio::select! {
                    _ = tick.tick() => klines_once(&inner).await,
                    _ = shutdown.changed() => {
                        if *shutdown.borrow() { break; }
                    }
                }
            }
        });
    }

    // ---- liquidity / books loop (25 s) ----
    {
        let inner = inner.clone();
        let mut shutdown = deps.shutdown.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(25));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    _ = tick.tick() => books_once(&inner).await,
                    _ = shutdown.changed() => {
                        if *shutdown.borrow() { break; }
                    }
                }
            }
        });
    }

    // ---- paradex loop (10 min; first pass immediately) ----
    {
        let inner = inner.clone();
        let mut shutdown = deps.shutdown.clone();
        tokio::spawn(async move {
            paradex_once(&inner).await;
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(600));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    _ = tick.tick() => paradex_once(&inner).await,
                    _ = shutdown.changed() => {
                        if *shutdown.borrow() { break; }
                    }
                }
            }
        });
    }

    // ---- tick-set loop (45 s; first pass immediately) ----
    // Refreshes Derive v3 discovery every 10 min, then republishes the
    // desired market sets both tick feeds reconcile against.
    {
        let inner = inner.clone();
        let digest = deps.digest.clone();
        let mut shutdown = deps.shutdown.clone();
        tokio::spawn(async move {
            tick_sets_once(&inner, &digest).await;
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(45));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    _ = tick.tick() => tick_sets_once(&inner, &digest).await,
                    _ = shutdown.changed() => {
                        if *shutdown.borrow() { break; }
                    }
                }
            }
        });
    }

    ResearchHandle { inner }
}

// ---------------------------------------------------------------------------
// Loop bodies
// ---------------------------------------------------------------------------

async fn consolidate_once(inner: &Inner, digest: &Arc<ArcSwap<ChainDigest>>, snapshot: &Arc<ArcSwap<Snapshot>>) {
    let now = now_ms();
    let digest_arc = digest.load_full();
    let snap_arc = snapshot.load_full();

    let synthetic = digest_arc.rows.is_empty();
    let (digest_ts, rows): (i64, Vec<ChainDigestRow>) = if synthetic {
        // Nothing live and no engine sim feed — synthesize a labeled book so
        // the research UI stays fully functional.
        let d = sim::synthetic_digest(now, &["deribit", "bybit", "binance", "okx"]);
        (d.ts_ms, d.rows)
    } else {
        (digest_arc.ts_ms, digest_arc.rows.clone())
    };

    let (mut enrich_snap, candle_snap) = (
        inner.enrich.lock().map(|e| e.snapshot()).unwrap_or_default(),
        inner.candles.load_full().as_ref().clone(),
    );
    let mut hist = inner
        .iv_history
        .lock()
        .map(|h| h.clone())
        .unwrap_or_default();
    let mut last_sample = inner.last_hist_ms.load(std::sync::atomic::Ordering::Relaxed);

    // Paradex rows from the public WS quote store (JWT-free, ~1 s refresh).
    let mut rows = rows;
    {
        let healthy = inner.paradex_live.healthy();
        let quote_n = inner.paradex_live.quote_count();
        if healthy && quote_n > 0 {
            let near = book_near_canon(&inner.underlyings, &rows);
            let qs = inner.paradex_live.snapshot(&near);
            if !qs.is_empty() {
                rows.extend(paradex_rows(&qs, &snap_arc));
                inject_paradex_enrich(&mut enrich_snap, &qs, &snap_arc);
            }
        }
        if let Ok(mut p) = inner.paradex.lock() {
            p.quotes_ok = healthy && quote_n > 0;
            if p.quotes_ok {
                p.note = format!(
                    "quotes live via public WS (SBE): {} markets, {:.0} frames/s, no auth",
                    quote_n,
                    inner.paradex_live.rate_per_s(now_ms() - inner.boot_ms)
                );
            } else {
                p.note = "public WS reconnecting (quotes resume automatically)".into();
            }
        }
    }

    // Derive v3 fallback synthesis: when the engine's derive venue is down
    // (e.g. the v2 host sunset) — or when Derive's coarse strike grid gets
    // crowded out of the digest's 26-nearest-strike buckets by venues with
    // finer strikes — the tick plane still carries fresh v3 BBO + IVs.
    // Materialize rows for every fresh derive tick on a tracked underlying
    // that the digest does not already cover, so Derive's chain stays
    // complete on its own grid while its v3 feed is alive.
    {
        let have_derive: std::collections::HashSet<String> = rows
            .iter()
            .filter(|r| r.venue == "derive")
            .map(|r| consolidate::canon(&r.und, r.date, r.strike, r.right))
            .collect();
        let mut added = 0usize;
        for (c, t) in inner.ticks.snapshot_venue("derive", ticks::TICK_FRESH_MS) {
            if have_derive.contains(&c) {
                continue;
            }
            let parts: Vec<&str> = c.split('-').collect();
            if parts.len() != 4 || !inner.underlyings.contains(&parts[0].to_uppercase()) {
                continue;
            }
            let (Ok(date), Ok(strike)) = (parts[1].parse::<u32>(), parts[2].parse::<f64>()) else {
                continue;
            };
            let right = match parts[3] {
                "C" => Right::Call,
                "P" => Right::Put,
                _ => continue,
            };
            // Near-ATM + DTE bounds (the tick set is near-ATM by construction;
            // this re-check keeps the guarantee even if subscriptions drift).
            let spot = snap_arc
                .spots
                .iter()
                .find(|s| s.underlying.eq_ignore_ascii_case(parts[0]))
                .map(|s| s.price)
                .unwrap_or(0.0);
            if spot <= 0.0 || ((strike / spot).ln().abs() > 0.12) {
                continue;
            }
            if consolidate::ymd_to_ms(date)
                .map(|ms| (ms - now) / 86_400_000)
                .map(|dte| !(0..=90).contains(&dte))
                .unwrap_or(true)
            {
                continue;
            }
            rows.push(ChainDigestRow {
                venue: "derive".into(),
                und: parts[0].to_uppercase(),
                date,
                strike,
                right,
                mark_px: t.mark,
                bid_px: t.bid,
                ask_px: t.ask,
                iv_mark_vp: t.iv_mark_vp,
                iv_bid_vp: t.iv_bid_vp,
                iv_ask_vp: t.iv_ask_vp,
                size_bid: t.bid_size,
                size_ask: t.ask_size,
                ts_ms: t.feed_ts_ms.max(0),
            });
            added += 1;
            if added >= 200 {
                break;
            }
        }
        if added > 0 {
            tracing::debug!(rows = added, "derive v3 tick rows synthesized");
        }
    }

    // Tick-level BBO overlay — BEFORE routing: the freshest books win.
    overlay_ticks(&inner.ticks, &mut rows, now);

    let mut book = consolidate::build(
        now,
        &rows,
        digest_ts,
        &enrich_snap,
        &snap_arc,
        &candle_snap,
        &mut hist,
        &mut last_sample,
        synthetic,
    );
    // Decorate venue quotes with their tick ages so the UI can badge
    // 50 ms-class quotes on the chain browser.
    decorate_tick_ages(&inner.ticks, &mut book, now);
    inner
        .last_hist_ms
        .store(last_sample, std::sync::atomic::Ordering::Relaxed);
    if let Ok(mut h) = inner.iv_history.lock() {
        *h = hist;
    }
    inner.book.store(Arc::new(book));
}

/// Overlay tick-fresh BBO onto the digest rows *before* consolidation, so
/// the cross-venue best-bid / best-ask routing (the execution router's price
/// reference) sees the 50 ms-class books instead of the slower venue feeds:
/// Paradex 1.2 s summaries → 50 ms order-book snapshots; Derive 4 s REST
/// polls → 100 ms v3 tickers (BBO, sizes, IVs).
fn overlay_ticks(plane: &ticks::TickPlane, rows: &mut [ChainDigestRow], now: i64) {
    for r in rows.iter_mut() {
        if r.venue != "paradex" && r.venue != "derive" {
            continue;
        }
        let c = consolidate::canon(&r.und, r.date, r.strike, r.right);
        let Some(t) = plane.get(&r.venue, &c) else { continue };
        if !t.fresh(now) {
            continue;
        }
        if t.bid.is_some() {
            r.bid_px = t.bid;
        }
        if t.ask.is_some() {
            r.ask_px = t.ask;
        }
        if t.bid_size.is_some() {
            r.size_bid = t.bid_size;
        }
        if t.ask_size.is_some() {
            r.size_ask = t.ask_size;
        }
        if t.mark.is_some() {
            r.mark_px = t.mark;
        }
        // Derive v3 tickers carry the venue's own IV surface.
        if let Some(iv) = t.iv_mark_vp {
            r.iv_mark_vp = Some(iv);
        }
        if let Some(iv) = t.iv_bid_vp {
            r.iv_bid_vp = Some(iv);
        }
        if let Some(iv) = t.iv_ask_vp {
            r.iv_ask_vp = Some(iv);
        }
        if t.feed_ts_ms > 0 {
            r.ts_ms = t.feed_ts_ms;
        }
    }
}

/// Stamp each paradex/derive venue quote with its tick age (ms) when the
/// tick plane has a fresh quote for that instrument.
fn decorate_tick_ages(plane: &ticks::TickPlane, book: &mut ConsolidatedBook, now: i64) {
    for row in book.rows.iter_mut() {
        let c = consolidate::canon(&row.und, row.date, row.strike, row.right);
        for v in row.venues.iter_mut() {
            if v.venue != "paradex" && v.venue != "derive" {
                continue;
            }
            if let Some(t) = plane.get(&v.venue, &c) {
                let age = t.age_ms(now);
                if age <= 2_000 {
                    v.tick_age_ms = Some(age as f64);
                }
            }
        }
    }
}

fn book_near_canon(unds: &[String], rows: &[ChainDigestRow]) -> Vec<String> {
    // Every tracked-underlying digest key: the WS quote map already holds the
    // whole Paradex universe, so matching is a hash lookup per key and the old
    // 60-row cap (a REST-rate-limit relic) only starved underlyings that
    // sorted late in the digest.
    rows.iter()
        .filter(|r| unds.contains(&r.und) && r.und != "sim")
        .map(|r| consolidate::canon(&r.und, r.date, r.strike, r.right))
        .take(800)
        .collect()
}

/// Fold Paradex WS stats into the enrichment index so the consolidated rows
/// and the venue table carry Paradex OI / 24h volume like every REST-enriched
/// venue. OI arrives in base-currency units (= venue contracts at unit size
/// 1.0); volume arrives as USD notional and is converted to underlying units
/// so the table's `contracts × spot × contract-size` marking round-trips
/// back to the original USD figure.
fn inject_paradex_enrich(
    enrich: &mut FxHashMap<String, FxHashMap<String, enrich::EnrichStats>>,
    qs: &FxHashMap<String, paradex::ParadexQuote>,
    snap: &Snapshot,
) {
    let entry = enrich.entry("paradex".to_string()).or_default();
    for (c, q) in qs {
        let und = c.split('-').next().unwrap_or("");
        let spot = snap
            .spots
            .iter()
            .find(|s| s.underlying == und)
            .map(|s| s.price)
            .or(q.index)
            .unwrap_or(0.0);
        let vol24 = q
            .vol24h_usd
            .filter(|v| *v > 0.0)
            .and_then(|v| if spot > 0.0 { Some(v / spot) } else { None });
        entry.insert(
            c.clone(),
            enrich::EnrichStats {
                oi: q.oi,
                vol24,
                vol_usd: q.vol24h_usd,
            },
        );
    }
}

/// Convert public-WS Paradex quotes into digest rows. Venue-published IVs
/// (mark/bid/ask) are preferred; the BS solver against the consensus spot is
/// the fallback for markets where Paradex omits them. Bid/ask sizes ride
/// along for the consolidated book's liquidity columns.
fn paradex_rows(
    qs: &FxHashMap<String, paradex::ParadexQuote>,
    snap: &Snapshot,
) -> Vec<ChainDigestRow> {
    let now = now_ms();
    let mut out = Vec::new();
    for (canon, q) in qs {
        // Stale quote (> 30 s since its feed timestamp) — drop rather than
        // publish a rotting price.
        if now - q.ts_ms > 30_000 {
            continue;
        }
        let parts: Vec<&str> = canon.split('-').collect();
        if parts.len() != 4 {
            continue;
        }
        let Some(und) = parts.first().map(|s| s.to_string()) else { continue };
        let Ok(date) = parts[1].parse::<u32>() else { continue };
        let Ok(strike) = parts[2].parse::<f64>() else { continue };
        let right = match parts[3] {
            "C" => Right::Call,
            "P" => Right::Put,
            _ => continue,
        };
        // Consensus spot first (cross-venue consistency); Paradex's own
        // oracle index as fallback for underlyings the engine doesn't track.
        let spot = snap
            .spots
            .iter()
            .find(|s| s.underlying == und)
            .map(|s| s.price)
            .or(q.index)
            .filter(|s| *s > 0.0);
        let t = ((consolidate::ymd_to_ms(date).unwrap_or(0) - now).max(3_600_000) as f64
            / 1000.0
            / (365.0 * 86400.0))
            .max(1e-4);
        let solve = |px: Option<f64>| {
            let (px, spot) = (px?, spot?) ;
            if px <= 0.0 {
                return None;
            }
            ivarb_quant::implied_vol(
                px,
                spot,
                strike,
                t,
                match right {
                    Right::Call => ivarb_quant::QuantRight::Call,
                    Right::Put => ivarb_quant::QuantRight::Put,
                },
            )
            .map(|iv| iv * 100.0)
        };
        // Venue IVs are fractions → vol points; sanity-clamped like the
        // engine's own IV fields (5–500 vp).
        let vp = |x: Option<f64>| x.map(|v| v * 100.0).filter(|v| (5.0..500.0).contains(v));
        out.push(ChainDigestRow {
            venue: "paradex".into(),
            und,
            date,
            strike,
            right,
            mark_px: q.mark,
            bid_px: q.bid,
            ask_px: q.ask,
            iv_mark_vp: vp(q.mark_iv).or_else(|| solve(q.mark)),
            iv_bid_vp: vp(q.bid_iv).or_else(|| solve(q.bid)),
            iv_ask_vp: vp(q.ask_iv).or_else(|| solve(q.ask)),
            size_bid: q.bid_size,
            size_ask: q.ask_size,
            ts_ms: q.ts_ms,
        });
    }
    out
}

async fn klines_once(inner: &Inner) {
    const INTERVALS: [(&str, u32); 4] = [("15m", 200), ("1h", 240), ("4h", 200), ("1d", 180)];
    let now = now_ms();
    for und in &inner.underlyings {
        let mut any_ok = false;
        for (iv, limit) in INTERVALS {
            let key = (und.clone(), iv.to_string());
            match enrich::fetch_klines(&inner.http, und, iv, limit).await {
                Ok(cs) => {
                    let series = analytics::indicators(und, iv, cs);
                    any_ok = true;
                    let mut map = inner.candles.load_full().as_ref().clone();
                    map.insert(key, series);
                    inner.candles.store(Arc::new(map));
                }
                Err(e) => {
                    // Keep stale candles (< 10 min); synthesize only when the
                    // book itself is not live (preview without egress).
                    let existing = inner.candles.load();
                    let have_fresh = existing
                        .get(&key)
                        .map(|c| now - c.candles.last().map(|x| x.t_ms).unwrap_or(0) < 600_000)
                        .unwrap_or(false);
                    if !have_fresh {
                        let mode = inner.book.load().data_mode;
                        if mode != "live" {
                            let mut map = existing.as_ref().clone();
                            map.insert(key, sim::synthetic_series(now, und, iv));
                            inner.candles.store(Arc::new(map));
                        }
                    }
                    tracing::debug!(und, interval = iv, error = %e, "klines fetch failed");
                }
            }
        }
        if let Ok(mut st) = inner.enrich.lock() {
            if any_ok {
                st.klines_ok.insert(und.clone());
            }
        }
    }
}

async fn books_once(inner: &Inner) {
    let book = inner.book.load_full();
    if book.rows.is_empty() {
        return;
    }
    let mut maps: FxHashMap<String, LiquidityMap> = inner.liquidity.load_full().as_ref().clone();
    for und in &inner.underlyings {
        let Some(spot) = book.spots.get(und).copied().filter(|s| s.is_finite() && *s > 0.0) else { continue };
        // Top near-ATM cells by (OI + touch size) on the two nearest expiries.
        let mut dates: Vec<u32> = book
            .rows
            .iter()
            .filter(|r| r.und == *und && r.dte >= 0)
            .map(|r| r.date)
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        dates.reverse();
        dates.truncate(2);
        let mut cand: Vec<&ChainRow> = book
            .rows
            .iter()
            .filter(|r| r.und == *und && dates.contains(&r.date))
            .filter(|r| (r.strike / spot).ln().abs() <= 0.08)
            .collect();
        cand.sort_by(|a, b| {
            let sa = a.open_interest.unwrap_or(0.0) + a.venues.iter().filter_map(|v| v.size_bid).sum::<f64>();
            let sb = b.open_interest.unwrap_or(0.0) + b.venues.iter().filter_map(|v| v.size_bid).sum::<f64>();
            sb.partial_cmp(&sa).unwrap_or(std::cmp::Ordering::Equal)
        });
        cand.truncate(10);

        let mut cells = Vec::new();
        for r in cand {
            // Baseline: touch-size estimate from the consolidated book.
            let mut bid_usd = 0.0;
            let mut ask_usd = 0.0;
            let mut best_bid_venue = r.best_bid_venue.clone();
            let mut best_ask_venue = r.best_ask_venue.clone();
            for v in &r.venues {
                if let (Some(b), Some(sb)) = (v.bid_px, v.size_bid) {
                    bid_usd += b * sb * enrich::venue_contract_units(&v.venue);
                }
                if let (Some(a), Some(sa)) = (v.ask_px, v.size_ask) {
                    ask_usd += a * sa * enrich::venue_contract_units(&v.venue);
                }
            }
            // Upgrade with real book depth (2% of touch) when reachable.
            for venue in ["deribit", "binance", "okx"] {
                if let Some((bids, asks)) =
                    enrich::fetch_book(&inner.http, venue, und, r.date, r.strike, r.right, spot).await
                {
                    let (bu, _) = enrich::notional_within(&bids, 2.0);
                    let (au, _) = enrich::notional_within(&asks, 2.0);
                    if bu > bid_usd {
                        bid_usd = bid_usd.max(bu);
                        best_bid_venue = Some(venue.to_string());
                    }
                    if au > ask_usd {
                        ask_usd = ask_usd.max(au);
                        best_ask_venue = Some(venue.to_string());
                    }
                }
            }
            cells.push(LiquidityCell {
                date: r.date,
                dte: r.dte,
                strike: r.strike,
                right: r.right,
                depth_usd: Some(bid_usd + ask_usd).filter(|x| *x > 0.0),
                oi_usd: r
                    .open_interest
                    .map(|oi| oi * spot * enrich::venue_contract_units(best_bid_venue.as_deref().unwrap_or(""))),
                best_bid_venue,
                best_ask_venue,
            });
        }
        if cells.is_empty() {
            continue;
        }
        let mut expiries: Vec<u32> = cells.iter().map(|c| c.date).collect::<std::collections::BTreeSet<_>>().into_iter().collect();
        expiries.reverse();
        let mut strikes: Vec<f64> = cells.iter().map(|c| c.strike).collect();
        strikes.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        strikes.dedup_by(|a, b| (*a - *b).abs() < 1e-9);
        maps.insert(
            und.clone(),
            LiquidityMap {
                und: und.clone(),
                spot,
                data_mode: book.data_mode.to_string(),
                expiries,
                strikes,
                cells,
                updated_ms: now_ms(),
            },
        );
    }
    inner.liquidity.store(Arc::new(maps));
}

async fn paradex_once(inner: &Inner) {
    match paradex::poll_universe(&inner.http).await {
        Ok(mut st) => {
            tracing::info!(markets = st.market_count(), "paradex universe refreshed");
            // Quote health is owned by the public WS task — carry it over.
            st.quotes_ok = inner.paradex_live.healthy() && inner.paradex_live.quote_count() > 0;
            if let Ok(mut cur) = inner.paradex.lock() {
                st.note = cur.note.clone();
                *cur = st;
            }
        }
        Err(e) => {
            tracing::debug!(error = %e, "paradex universe poll failed");
            if let Ok(mut cur) = inner.paradex.lock() {
                cur.note = format!("universe unreachable: {e}");
            }
        }
    }
}

/// Recompute the desired tick-feed market sets from the current chain digest
/// and Derive v3 discovery, then republish them to both feed tasks.
async fn tick_sets_once(inner: &Inner, digest: &Arc<ArcSwap<ChainDigest>>) {
    // --- Derive v3 discovery refresh (10 min) ---
    if now_ms() - inner.derive_names_ms.load(std::sync::atomic::Ordering::Relaxed) > 600_000 {
        match derive::fetch_live_names(&inner.http).await {
            Ok(names) => {
                let mut map = FxHashMap::default();
                for n in &names {
                    if let Some((und, date, strike, right)) = derive::parse_name(n) {
                        map.insert(consolidate::canon(&und, date, strike, right), n.clone());
                    }
                }
                tracing::info!(names = names.len(), options = map.len(), "derive v3 instruments refreshed");
                if let Ok(mut m) = inner.derive_names.lock() {
                    *m = map;
                }
                inner
                    .derive_names_ms
                    .store(now_ms(), std::sync::atomic::Ordering::Relaxed);
            }
            Err(e) => {
                tracing::debug!(error = %e, "derive v3 discovery failed");
            }
        }
    }

    // --- near-ATM canonical keys from the live digest ---
    let digest_arc = digest.load_full();
    let mut keys: Vec<String> = digest_arc
        .rows
        .iter()
        .filter(|r| inner.underlyings.contains(&r.und) && r.und != "sim")
        .map(|r| consolidate::canon(&r.und, r.date, r.strike, r.right))
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    keys.truncate(400);

    let (pdx_syms, drv_names) = {
        let pdx = inner.paradex.lock().ok();
        let drv = inner.derive_names.lock().ok();
        let mut pdx_syms: Vec<String> = inner
            .underlyings
            .iter()
            .map(|u| paradex::perp_symbol(u))
            .collect();
        let mut drv_names: Vec<String> =
            inner.underlyings.iter().map(|u| derive::perp_name(u)).collect();
        if let Some(p) = &pdx {
            for c in &keys {
                if let Some(sym) = p.symbols.get(c) {
                    pdx_syms.push(sym.clone());
                }
            }
        }
        if let Some(d) = &drv {
            for c in &keys {
                if let Some(n) = d.get(c) {
                    drv_names.push(n.clone());
                }
            }
        }
        (pdx_syms, drv_names)
    };
    let _ = inner.paradex_desired.send(pdx_syms);
    let _ = inner.derive_desired.send(drv_names);
}

// ---------------------------------------------------------------------------
// Public read API
// ---------------------------------------------------------------------------

impl ResearchHandle {
    /// One-glance pulse (the research landing strip).
    pub fn pulse(&self) -> ResearchPulse {
        let book = self.inner.book.load();
        let candles = self.inner.candles.load();
        let spots: Vec<SpotSummary> = book
            .spots
            .iter()
            .filter(|(u, _)| self.inner.underlyings.contains(u))
            .map(|(u, p)| SpotSummary {
                und: u.clone(),
                price: *p,
                change_24h_pct: candles
                    .get(&(u.clone(), "1h".into()))
                    .and_then(|c| c.readout.change_24h_pct),
            })
            .collect();
        ResearchPulse {
            ts_ms: book.ts_ms,
            data_mode: book.data_mode.to_string(),
            spots,
            venues_live: book.venue_stats.iter().filter(|v| v.online).count() as u32,
            instruments: book.venue_stats.iter().map(|v| v.instruments).sum::<u32>(),
            iv: book.iv.clone(),
            venue_stats: book.venue_stats.clone(),
            agent_brain: self
                .inner
                .llm
                .as_ref()
                .map(|c| format!("llm:{}", c.model))
                .unwrap_or_else(|| "desk".into()),
        }
    }

    /// Consolidated chain for one underlying (optionally one expiry / right).
    pub fn chain(
        &self,
        und: &str,
        expiry: Option<u32>,
        right: Option<Right>,
        window_pct: f64,
        limit: usize,
    ) -> ChainResponse {
        let book = self.inner.book.load();
        let und = und.to_uppercase();
        let spot = book.spots.get(&und).copied().unwrap_or(f64::NAN);
        let date = expiry.or_else(|| {
            book.rows
                .iter()
                .filter(|r| r.und == und && r.dte >= 1)
                .map(|r| r.date)
                .min()
        });
        let win = if window_pct > 0.0 { window_pct } else { 12.0 };
        let mut rows: Vec<ChainRow> = book
            .rows
            .iter()
            .filter(|r| r.und == und)
            .filter(|r| date.map(|d| r.date == d).unwrap_or(true))
            .filter(|r| right.map(|x| r.right == x).unwrap_or(true))
            .filter(|r| {
                !spot.is_finite() || spot <= 0.0 || ((r.strike / spot).ln().abs() * 100.0) <= win
            })
            .cloned()
            .collect();
        rows.sort_by(|a, b| {
            a.strike
                .partial_cmp(&b.strike)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(
                    ((a.right == Right::Put) as u8).cmp(&((b.right == Right::Put) as u8)),
                )
        });
        rows.truncate(limit.max(10).min(120));
        ChainResponse {
            und,
            spot,
            rows,
            data_mode: book.data_mode.to_string(),
            ts_ms: book.ts_ms,
        }
    }

    /// Candle series (fetch-fresh markers + overlays).
    pub fn candles(&self, und: &str, interval: &str) -> Option<CandleSeries> {
        let map = self.inner.candles.load();
        let iv = match interval {
            "15m" | "1h" | "4h" | "1d" => interval,
            _ => "1h",
        };
        map.get(&(und.to_uppercase(), iv.to_string())).cloned()
    }

    /// IV analytics (term structure, skew, divergences).
    pub fn iv(&self, und: &str) -> IvAnalytics {
        let book = self.inner.book.load();
        let und = if book.rows.iter().any(|r| r.und == und.to_uppercase()) {
            und.to_uppercase()
        } else {
            "BTC".into()
        };
        analytics::iv_analytics(&book, &und)
    }

    /// Liquidity heatmap.
    pub fn liquidity(&self, und: &str) -> Option<LiquidityMap> {
        let map = self.inner.liquidity.load();
        map.get(&und.to_uppercase()).cloned()
    }

    /// Builder: market-coherent option structure.
    pub fn suggest(&self, req: &SuggestRequest) -> Result<SuggestionCard, String> {
        let book = self.inner.book.load();
        let candles = self.inner.candles.load();
        let cs = candles.get(&(req.und.to_uppercase(), "1h".into()));
        suggest::build(&book, cs, req, now_ms())
    }

    /// Agent run (desk brain or LLM brain).
    pub async fn agent(&self, query: String) -> AgentRun {
        let deps = agent::AgentDeps {
            book: self.inner.book.load_full(),
            candles: self.inner.candles.load_full(),
            liquidity: self.inner.liquidity.load_full(),
            llm: self.inner.llm.clone(),
            http: self.inner.http.clone(),
        };
        agent::run(deps, query).await
    }

    /// Status / health of the research plane.
    pub fn status(&self) -> ResearchStatus {
        let book = self.inner.book.load();
        let (enrich_ok, klines_ok) = match self.inner.enrich.lock() {
            Ok(st) => (st.ok_venues(), st.klines_ok_list()),
            Err(_) => (vec![], vec![]),
        };
        let (pm, note) = match self.inner.paradex.lock() {
            Ok(st) => (st.market_count(), st.note.clone()),
            Err(_) => (0, "locked".into()),
        };
        // Quote health is read straight from the WS task — always current,
        // never a stale mirror of the last consolidation pass.
        let live = &self.inner.paradex_live;
        let pq = live.healthy() && live.quote_count() > 0;
        let (pdx_conn, pdx_subs, _pdx_frames, _pdx_recv, pdx_mkts, _) =
            self.inner.ticks.feed("paradex");
        let (drv_conn, drv_subs, _drv_frames, _drv_recv, drv_mkts, _) =
            self.inner.ticks.feed("derive");
        ResearchStatus {
            digest_ms: book.ts_ms,
            digest_rows: book.rows.len(),
            chain_rows: book.rows.len(),
            enrichment_ok: enrich_ok,
            klines_ok,
            paradex_markets: pm,
            paradex_quotes: pq,
            paradex_ws: live.connected(),
            paradex_note: note,
            llm_configured: self.inner.llm.is_some(),
            data_mode: book.data_mode.to_string(),
            paradex_book: pdx_conn,
            paradex_book_subs: pdx_subs,
            derive_ws: drv_conn,
            derive_subs: drv_subs,
            tick_markets: self.inner.ticks.market_count(ticks::TICK_FRESH_MS) as u32,
            tick_note: format!(
                "{} fresh tick markets · paradex {} @50ms book · derive {} @100ms ticker",
                pdx_mkts + drv_mkts,
                if pdx_conn { "live" } else { "down" },
                if drv_conn { "live" } else { "down" },
            ),
        }
    }

    /// Tick-level BBO table for the UI's Ticks tab (and anyone auditing the
    /// execution-routing inputs): every fresh tick quote for one underlying,
    /// freshest first, with true exchange timestamps and per-quote ages.
    pub fn ticks(&self, und: &str, limit: usize) -> TicksResponse {
        let now = now_ms();
        let book = self.inner.book.load();
        let und = und.to_uppercase();
        let spot = book.spots.get(&und).copied().unwrap_or(f64::NAN);
        let prefix = format!("{und}-");
        let mut rows: Vec<TickQuote> = Vec::new();
        for venue in ["paradex", "derive"] {
            for (canon, t) in self.inner.ticks.snapshot_venue(venue, ticks::TICK_FRESH_MS) {
                if !canon.starts_with(&prefix) {
                    continue;
                }
                let parts: Vec<&str> = canon.split('-').collect();
                if parts.len() != 4 {
                    continue;
                }
                let (Ok(date), Ok(strike)) = (parts[1].parse::<u32>(), parts[2].parse::<f64>())
                else {
                    continue;
                };
                let right = match parts[3] {
                    "C" => Right::Call,
                    "P" => Right::Put,
                    _ => continue,
                };
                let spread_pct = match (t.bid, t.ask) {
                    (Some(b), Some(a)) if b > 0.0 && a > b => Some((a / b - 1.0) * 100.0),
                    _ => None,
                };
                rows.push(TickQuote {
                    venue: venue.to_string(),
                    und: und.clone(),
                    date,
                    strike,
                    right,
                    bid: t.bid,
                    ask: t.ask,
                    bid_size: t.bid_size,
                    ask_size: t.ask_size,
                    mark: t.mark,
                    iv_mark_vp: t.iv_mark_vp,
                    spread_pct,
                    seq: t.seq,
                    feed_ts_ms: t.feed_ts_ms,
                    age_ms: t.age_ms(now),
                });
            }
        }
        rows.sort_by(|a, b| {
            a.strike
                .partial_cmp(&b.strike)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(a.date.cmp(&b.date))
                .then(a.venue.cmp(&b.venue))
        });
        rows.truncate(limit.max(20).min(300));
        let mk = |venue: &str| {
            let (conn, subs, frames, last_recv, markets, note) = self.inner.ticks.feed(venue);
            TickFeedStatus {
                venue: venue.to_string(),
                connected: conn,
                subscriptions: subs,
                frames,
                markets: markets as u32,
                last_recv_ms: last_recv,
                age_ms: (now - last_recv).max(0),
                note,
            }
        };
        TicksResponse {
            und: und.clone(),
            spot,
            rows,
            feeds: vec![mk("paradex"), mk("derive")],
            data_mode: book.data_mode.to_string(),
            ts_ms: now,
        }
    }

    /// Expiry list helper for chain UIs.
    pub fn expiries(&self, und: &str) -> Vec<(u32, String, i32)> {
        let book = self.inner.book.load();
        analytics::expiry_list(&book, &und.to_uppercase())
    }

    /// Venue stats snapshot.
    pub fn venue_stats(&self) -> Vec<ResearchVenueStat> {
        self.inner.book.load().venue_stats.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn consolidation_from_synthetic_digest() {
        let now = now_ms();
        let d = sim::synthetic_digest(now, &["deribit", "bybit"]);
        let snap = Snapshot {
            ts_ms: now,
            stats: ivarb_protocol::EngineStats {
                ts_ms: now,
                uptime_s: 1,
                feed_mode: ivarb_protocol::FeedMode::Sim,
                venues_live: 2,
                venues_total: 2,
                instruments_tracked: d.rows.len() as u32,
                opps_active: 0,
                engine_loop_us_ewma: 0.0,
                msgs_total: 0,
                msg_rate: 0.0,
                pipe_p50_us: 0.0,
                pipe_p99_us: 0.0,
                pipe_p999_us: 0.0,
                wire_p50_us: 0.0,
                wire_p99_us: 0.0,
                broadcast_ms: 250,
                sweep_ms: 250,
                multihop_routes: 0,
                holds_tracked: 0,
                ga_generation: 0,
                min_net_vp: 0.3,
                exit_net_vp: 0.1,
                moneyness_pct: 5.0,
                min_vega_usd: 25.0,
            },
            venues: vec![],
            spots: vec![ivarb_protocol::SpotQuote {
                underlying: "BTC".into(),
                venue: "sim".into(),
                price: 82450.0,
                ts_ms: now,
            }],
            opportunities: vec![],
            holds: vec![],
            multihop: vec![],
        };
        let mut hist = FxHashMap::default();
        let mut last = 0i64;
        let candles = FxHashMap::default();
        let enrich = enrich::EnrichStore::default();
        let enrich_idx = enrich.snapshot();
        let book = consolidate::build(
            now,
            &d.rows,
            d.ts_ms,
            &enrich_idx,
            &snap,
            &candles,
            &mut hist,
            &mut last,
            true,
        );
        assert_eq!(book.data_mode, "sim");
        assert!(!book.rows.is_empty());
        // every row has at least one venue quote
        assert!(book.rows.iter().all(|r| !r.venues.is_empty()));
        // ATM map populated for BTC
        assert!(book.atm_by_date.keys().any(|(u, _)| u == "BTC"));
        // venue stats for the two sim venues
        assert_eq!(book.venue_stats.len(), 2);
        assert!(book.venue_stats.iter().all(|v| v.is_sim));
        // IV summaries exist
        assert!(book.iv.iter().any(|s| s.und == "BTC"));
    }

    #[test]
    fn synthetic_candles_series_indicators() {
        let now = now_ms();
        let s = sim::synthetic_series(now, "BTC", "1h");
        assert_eq!(s.data_mode, "sim");
        assert_eq!(s.candles.len(), 240);
        assert!(s.readout.rsi14.is_some());
    }
}
