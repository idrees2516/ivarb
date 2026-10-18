//! Binance options connector (European options, eapi) — WS-first.
//!
//! The old 5s REST polling of `/eapi/v1/mark` + `/eapi/v1/ticker` burned
//! ~600 request-weight per minute (the `/ticker` whole-market call alone is
//! weight 40) against a **400/min** IP budget, earning permanent 429 → 418
//! auto-bans ("Way too many requests; IP banned … Please use the websocket
//! for live updates to avoid bans.").
//!
//! Binance has since unified the options market streams onto the futures
//! host (verified live; `nbstream.binance.com` is dead — 404):
//!
//! - `wss://fstream.binance.com/public/stream?streams={sym}@depth5@100ms/…`
//!   partial top-5 book with **prices and quantities** at a 100ms cadence —
//!   strictly better than the old REST `/ticker` (5s, no sizes).
//! - `wss://fstream.binance.com/market/stream?streams=!index@arr` — index
//!   price for every underlying in one stream.
//! - REST `/eapi/v1/mark` (mark prices + venue IVs) polled at a gentle 60s
//!   (adaptive backoff on 429/418) purely to refresh marks — book-driven
//!   bid/ask IVs are Black-Scholes solved at full 100ms freshness between
//!   refreshes.
//!
//! Streams are lowercase in the URL; payloads carry uppercase symbols.
//! A connection may carry at most 200 streams, so tracked symbols are
//! sharded across book connections (batches of 150) with per-connection
//! supervised reconnects.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use futures_util::StreamExt;
use tokio::sync::{mpsc, watch};
use tokio_tungstenite::tungstenite::Message;

use ivarb_protocol::{ConnState, SpotQuote};

use crate::jsonutil::{jf, js, norm_iv_vp};
use crate::key::{cap_expiries, parse_option_symbol, ymd_to_expiry_ms, InstKey};
use crate::msg::{now_ms, InstrTick, VenueMsg};
use crate::venues::{http_get_json, status, ws_connect, VenueConnector, VenueCtx};

/// Tracked symbols per book connection (server cap is 200).
const BOOK_BATCH: usize = 150;
/// Book/BBO flush cadence while aggregating 100ms depth events.
const FLUSH_MS: u64 = 400;
/// Mark refresh cadence (request-weight friendly).
const MARK_REFRESH_S: u64 = 60;
/// How long without any data before the session restarts.
const STARVATION_S: u64 = 150;

pub struct Binance;

impl Binance {
    pub fn new() -> Self {
        Binance
    }
}

#[derive(Clone, Default, Debug)]
struct Bbo {
    bid_px: f64,
    bid_qty: f64,
    ask_px: f64,
    ask_qty: f64,
    ts: i64,
}

#[derive(Clone, Default, Debug)]
struct MarkInfo {
    px_mark: f64,
    iv_mark_vp: Option<f64>,
    iv_bid_vp: Option<f64>,
    iv_ask_vp: Option<f64>,
}

/// Events forwarded from the WS tasks to the session loop.
enum Feed {
    Book { sym: String, bbo: Bbo },
    Index(Vec<(String, f64)>),
}

#[async_trait::async_trait]
impl VenueConnector for Binance {
    fn id(&self) -> &str {
        "binance"
    }

    async fn run(&mut self, mut ctx: VenueCtx) -> anyhow::Result<()> {
        status(&ctx.tx, "binance", ConnState::Connecting, None).await;

        // ---- universe bootstrap (single REST call) --------------------------
        let url = format!("{}/eapi/v1/exchangeInfo", ctx.spec.rest_base);
        let v = http_get_json(&ctx.http, &url).await?;
        let rows = v
            .get("optionSymbols")
            .and_then(|r| r.as_array())
            .cloned()
            .unwrap_or_default();
        let now = now_ms();
        let unds: Vec<String> = ctx.cfg.underlyings.clone();
        let und_set: HashSet<String> = unds.iter().cloned().collect();
        let mut dates: Vec<u32> = rows
            .iter()
            .filter_map(|r| js(r, &["symbol"]))
            .filter_map(|s| parse_option_symbol(&s))
            .filter(|(u, _, _, _)| und_set.contains(u))
            .map(|(_, d, _, _)| d)
            .collect();
        cap_expiries(
            &mut dates,
            now,
            ctx.cfg.min_dte,
            ctx.cfg.max_dte,
            ctx.cfg.max_expiries,
        );
        let mut index: HashMap<String, InstKey> = HashMap::new();
        for r in &rows {
            let Some(sym) = js(r, &["symbol"]) else { continue };
            let Some((u, d, k, right)) = parse_option_symbol(&sym) else { continue };
            if !und_set.contains(&u) || !dates.contains(&d) {
                continue;
            }
            let und_id = ctx.intern(&u);
            index.insert(
                sym,
                InstKey {
                    und: und_id,
                    date: d,
                    strike_ticks: (k * 10_000.0).round() as u64,
                    right,
                },
            );
        }
        if index.is_empty() {
            return Err(anyhow::anyhow!("binance: no option symbols"));
        }
        let _ = ctx
            .tx
            .send(VenueMsg::Tracked {
                venue: "binance".into(),
                instruments: index.len() as u32,
            })
            .await;

        // ---- state ---------------------------------------------------------
        let mut marks: HashMap<String, MarkInfo> = HashMap::new();
        let mut bbos: HashMap<String, Bbo> = HashMap::new();
        let mut spots: HashMap<String, f64> = HashMap::new();
        let mut dirty: HashSet<String> = HashSet::new();
        let size_hint = ctx.spec.contract_size;
        // symbol → underlying name (via the shared interner, one lock).
        let sym_und: HashMap<String, String> = {
            let guard = ctx.interner.lock().ok();
            index
                .iter()
                .filter_map(|(s, k)| {
                    let name = guard
                        .as_ref()
                        .map(|i| i.name(k.und).to_string())
                        .unwrap_or_default();
                    if name.is_empty() || name == "?" {
                        None
                    } else {
                        Some((s.clone(), name))
                    }
                })
                .collect()
        };

        // Strike/right/time cache per symbol for the BS solve.
        let sym_terms: HashMap<String, (f64, u8, u32)> = index
            .iter()
            .map(|(s, k)| {
                (
                    s.clone(),
                    (k.strike_ticks as f64 / 10_000.0, k.right, k.date),
                )
            })
            .collect();

        // ---- seed marks (one call; failures are non-fatal) ------------------
        let mark_url = format!("{}/eapi/v1/mark", ctx.spec.rest_base);
        match http_get_json(&ctx.http, &mark_url).await {
            Ok(v) => {
                let rows = v.as_array().cloned().unwrap_or_default();
                for (sym, m) in parse_mark_rows(&rows) {
                    marks.insert(sym, m);
                }
                tracing::info!(venue = "binance", n = marks.len(), "mark seed ok");
            }
            Err(e) => tracing::warn!(venue = "binance", error = %e, "mark seed failed"),
        }

        // ---- feed tasks ------------------------------------------------------
        let (feed_tx, mut feed_rx) = mpsc::unbounded_channel::<Feed>();

        // Index stream (all underlyings, one connection).
        tokio::spawn(index_ws_task(
            feed_tx.clone(),
            ctx.shutdown.clone(),
        ));

        // Book streams sharded across connections.
        let mut syms: Vec<String> = index.keys().cloned().collect();
        syms.sort();
        for chunk in syms.chunks(BOOK_BATCH) {
            tokio::spawn(book_ws_task(
                chunk.to_vec(),
                feed_tx.clone(),
                ctx.shutdown.clone(),
            ));
        }
        drop(feed_tx);

        let mut flush = tokio::time::interval(Duration::from_millis(FLUSH_MS));
        flush.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mark_every = Duration::from_millis(ctx.spec.poll_ms.max(30_000));
        let mut mark_tick = tokio::time::interval(mark_every);
        mark_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut watchdog = tokio::time::interval(Duration::from_secs(30));
        let mut mark_backoff_s: u64 = 0;
        let mut last_data = tokio::time::Instant::now();
        let mut last_mark_attempt = tokio::time::Instant::now();

        loop {
            tokio::select! {
                feed = feed_rx.recv() => {
                    match feed {
                        Some(Feed::Book { sym, bbo }) => {
                            if index.contains_key(&sym) {
                                bbos.insert(sym.clone(), bbo);
                                dirty.insert(sym);
                                last_data = tokio::time::Instant::now();
                            }
                        }
                        Some(Feed::Index(pairs)) => {
                            for (und, px) in pairs {
                                spots.insert(und.clone(), px);
                                let _ = ctx.tx.send(VenueMsg::Spot(SpotQuote {
                                    underlying: und,
                                    venue: "binance".into(),
                                    price: px,
                                    ts_ms: now_ms(),
                                })).await;
                            }
                            last_data = tokio::time::Instant::now();
                        }
                        None => {
                            // All feed tasks gone (shutdown races).
                            if ctx.shutdown() { return Ok(()); }
                        }
                    }
                }
                _ = flush.tick() => {
                    if ctx.shutdown() { return Ok(()); }
                    if dirty.is_empty() { continue; }
                    let mut ticks: Vec<InstrTick> = Vec::with_capacity(dirty.len());
                    for sym in dirty.drain() {
                        let Some(key) = index.get(&sym) else { continue };
                        let bbo = bbos.get(&sym);
                        let mk = marks.get(&sym);
                        let und = sym_und.get(&sym);
                        let spot = und.and_then(|u| spots.get(u)).copied();
                        let (strike, right, date) = sym_terms
                            .get(&sym)
                            .map(|(k, r, d)| (*k, *r, *d))
                            .unwrap_or((0.0, 0, 0));
                        // eapi prices are quoted per 1 unit of underlying
                        // (BS-verified live) — never divide by the 0.01 lot
                        // quantum.
                        let mut tick = InstrTick {
                            key: *key,
                            iv_mark_vp: mk.and_then(|m| m.iv_mark_vp),
                            iv_bid_vp: mk.and_then(|m| m.iv_bid_vp),
                            iv_ask_vp: mk.and_then(|m| m.iv_ask_vp),
                            px_mark_unit: mk.map(|m| m.px_mark).filter(|p| *p > 0.0),
                            px_bid_unit: None,
                            px_ask_unit: None,
                            size_bid: None,
                            size_ask: None,
                            spot,
                            contract_size: Some(size_hint),
                            venue_ts_ms: None,
                        };
                        if let Some(b) = bbo {
                            tick.px_bid_unit = if b.bid_px > 0.0 { Some(b.bid_px) } else { None };
                            tick.px_ask_unit = if b.ask_px > 0.0 { Some(b.ask_px) } else { None };
                            tick.size_bid = if b.bid_qty > 0.0 { Some(b.bid_qty) } else { None };
                            tick.size_ask = if b.ask_qty > 0.0 { Some(b.ask_qty) } else { None };
                            tick.venue_ts_ms = Some(b.ts);
                            // Fresh executable IVs from the 100ms book
                            // (fall back to venue IVs when un-solvable).
                            if let (Some(px), Some(s)) = (tick.px_bid_unit, spot) {
                                if let Some(iv) = solve_iv_vp(px, s, strike, right, date, now) {
                                    tick.iv_bid_vp = Some(iv);
                                }
                            }
                            if let (Some(px), Some(s)) = (tick.px_ask_unit, spot) {
                                if let Some(iv) = solve_iv_vp(px, s, strike, right, date, now) {
                                    tick.iv_ask_vp = Some(iv);
                                }
                            }
                        }
                        ticks.push(tick);
                    }
                    if !ticks.is_empty() {
                        let _ = ctx.tx.send(VenueMsg::Ticks {
                            venue: "binance".into(),
                            ticks,
                        }).await;
                    }
                }
                _ = mark_tick.tick() => {
                    if ctx.shutdown() { return Ok(()); }
                    if mark_backoff_s > 0 {
                        // Respect the ban window; do not hammer (418 escalation).
                        if last_mark_attempt.elapsed() < Duration::from_secs(mark_backoff_s) {
                            continue;
                        }
                    }
                    last_mark_attempt = tokio::time::Instant::now();
                    match http_get_json(&ctx.http, &mark_url).await {
                        Ok(v) => {
                            let rows = v.as_array().cloned().unwrap_or_default();
                            for (sym, m) in parse_mark_rows(&rows) {
                                if index.contains_key(&sym) {
                                    marks.insert(sym.clone(), m);
                                    dirty.insert(sym);
                                }
                            }
                            mark_backoff_s = 0;
                            last_data = tokio::time::Instant::now();
                        }
                        Err(e) => {
                            let banned = e.to_string().contains("418") || e.to_string().contains("429");
                            mark_backoff_s = if banned {
                                (mark_backoff_s.max(MARK_REFRESH_S) * 2).min(300)
                            } else {
                                MARK_REFRESH_S
                            };
                            tracing::warn!(venue = "binance", error = %e, backoff_s = mark_backoff_s, "mark refresh failed");
                        }
                    }
                }
                _ = watchdog.tick() => {
                    if ctx.shutdown() { return Ok(()); }
                    if last_data.elapsed() > Duration::from_secs(STARVATION_S) {
                        return Err(anyhow::anyhow!(
                            "binance: no data from any stream for {}s",
                            STARVATION_S
                        ));
                    }
                }
                _ = ctx.shutdown.changed() => {
                    if *ctx.shutdown.borrow() { return Ok(()); }
                }
            }
        }
    }
}

/// BS-solve an implied vol (vol points) from an executable price.
fn solve_iv_vp(
    px: f64,
    spot: f64,
    strike: f64,
    right: u8,
    date: u32,
    now: i64,
) -> Option<f64> {
    if !(px.is_finite() && spot.is_finite() && strike > 0.0) || px <= 0.0 || spot <= 0.0 {
        return None;
    }
    let exp_ms = ymd_to_expiry_ms(date)?;
    let t_years = (exp_ms - now) as f64 / (365.0 * 86_400_000.0);
    if t_years <= 1.0 / (365.0 * 24.0) {
        return None;
    }
    let right = match right {
        0 => ivarb_quant::Right::Call,
        _ => ivarb_quant::Right::Put,
    };
    let iv = ivarb_quant::implied_vol(px, spot, strike, t_years, right)?;
    Some(iv * 100.0)
}

/// `/eapi/v1/mark` rows → per-symbol mark info (IVs normalized to vol points).
fn parse_mark_rows(rows: &[serde_json::Value]) -> Vec<(String, MarkInfo)> {
    let mut out = Vec::with_capacity(rows.len());
    for r in rows {
        let Some(sym) = js(r, &["symbol"]) else { continue };
        let m = MarkInfo {
            px_mark: jf(r, &["markPrice"]).unwrap_or(0.0),
            iv_mark_vp: jf(r, &["markIV", "markIv", "mark_iv"]).and_then(norm_iv_vp),
            iv_bid_vp: jf(r, &["bidIV", "bidIv", "bid_iv"]).and_then(norm_iv_vp),
            iv_ask_vp: jf(r, &["askIV", "askIv", "ask_iv"]).and_then(norm_iv_vp),
        };
        out.push((sym, m));
    }
    out
}

/// Partial depth5 payload → (symbol, best bid px/qty, best ask px/qty).
///
/// Levels arrive as `[["3965.000","2.98"], …]` (price, qty strings); the
/// best bid is the highest bid price and the best ask the lowest ask price
/// among entries with positive quantity (zero-qty entries mean "removed").
fn parse_depth_update(data: &serde_json::Value) -> Option<(String, Bbo)> {
    let sym = js(data, &["s"])?;
    let pick = |side: &str, want_max: bool| -> Option<(f64, f64)> {
        let levels = data.get(side)?.as_array()?;
        let mut best: Option<(f64, f64)> = None;
        for lv in levels {
            let a = lv.as_array()?;
            let px: f64 = a.first()?.as_str()?.parse().ok()?;
            let qty: f64 = a.get(1)?.as_str()?.parse().ok()?;
            if !px.is_finite() || !qty.is_finite() || px <= 0.0 || qty <= 0.0 {
                continue;
            }
            best = match best {
                None => Some((px, qty)),
                Some((bp, bq)) => {
                    let better = if want_max { px > bp } else { px < bp };
                    if better { Some((px, qty)) } else { Some((bp, bq)) }
                }
            };
        }
        best
    };
    let (bid_px, bid_qty) = pick("b", true)?;
    let (ask_px, ask_qty) = pick("a", false)?;
    Some((
        sym,
        Bbo {
            bid_px,
            bid_qty,
            ask_px,
            ask_qty,
            ts: jf(data, &["E", "T"]).map(|t| t as i64).unwrap_or(0),
        },
    ))
}

/// `!index@arr` payload → (underlying, price) for `*USDT` entries.
fn parse_index_events(data: &serde_json::Value) -> Vec<(String, f64)> {
    let arr = match data.as_array() {
        Some(a) => a,
        None => return Vec::new(),
    };
    let mut out = Vec::new();
    for e in arr {
        let Some(s) = js(e, &["s"]) else { continue };
        let Some(p) = jf(e, &["p"]) else { continue };
        if let Some(und) = s.strip_suffix("USDT") {
            if !und.is_empty() && und.chars().all(|c| c.is_ascii_alphabetic()) {
                out.push((und.to_string(), p));
            }
        }
    }
    out
}

/// Combined-stream frame → payload (unwrap `{"stream":…,"data":…}`).
fn unwrap_combined(msg: &serde_json::Value) -> serde_json::Value {
    if let Some(d) = msg.get("data") {
        d.clone()
    } else {
        msg.clone()
    }
}

/// Supervised book-connection task: `{sym}@depth5@100ms` for a batch of
/// symbols, reconnecting with backoff until shutdown.
async fn book_ws_task(
    syms: Vec<String>,
    tx: mpsc::UnboundedSender<Feed>,
    mut shutdown: watch::Receiver<bool>,
) {
    let streams: Vec<String> = syms
        .iter()
        .map(|s| format!("{}@depth5@100ms", s.to_lowercase()))
        .collect();
    let url = format!(
        "wss://fstream.binance.com/public/stream?streams={}",
        streams.join("/")
    );
    let mut backoff = Duration::from_millis(1_000);
    loop {
        if *shutdown.borrow() {
            return;
        }
        match ws_connect(&url).await {
            Ok(mut ws) => {
                backoff = Duration::from_millis(1_000);
                tracing::debug!(venue = "binance", syms = syms.len(), "book ws connected");
                loop {
                    tokio::select! {
                        frame = ws.next() => {
                            match frame {
                                Some(Ok(Message::Text(txt))) => {
                                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&txt) {
                                        let data = unwrap_combined(&v);
                                        if let Some((sym, bbo)) = parse_depth_update(&data) {
                                            if tx.send(Feed::Book { sym, bbo }).is_err() {
                                                return;
                                            }
                                        }
                                    }
                                }
                                // Ping frames are auto-answered by tungstenite
                                // while the stream is being read — keep reading.
                                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => {
                                    break;
                                }
                                Some(Ok(_)) => {}
                            }
                        }
                        _ = shutdown.changed() => {
                            if *shutdown.borrow() {
                                let _ = ws.close(None).await;
                                return;
                            }
                        }
                    }
                }
            }
            Err(e) => {
                tracing::warn!(venue = "binance", error = %e, "book ws connect failed");
            }
        }
        let jitter = Duration::from_millis(crate::venues::rand_range(500));
        tokio::select! {
            _ = tokio::time::sleep(backoff + jitter) => {}
            _ = shutdown.changed() => {
                if *shutdown.borrow() { return; }
            }
        }
        backoff = (backoff * 2).min(Duration::from_secs(30));
    }
}

/// Supervised index-connection task: `!index@arr` on the `/market` root.
async fn index_ws_task(tx: mpsc::UnboundedSender<Feed>, mut shutdown: watch::Receiver<bool>) {
    let url = "wss://fstream.binance.com/market/stream?streams=!index@arr";
    let mut backoff = Duration::from_millis(1_000);
    loop {
        if *shutdown.borrow() {
            return;
        }
        match ws_connect(url).await {
            Ok(mut ws) => {
                backoff = Duration::from_millis(1_000);
                tracing::debug!(venue = "binance", "index ws connected");
                loop {
                    tokio::select! {
                        frame = ws.next() => {
                            match frame {
                                Some(Ok(Message::Text(txt))) => {
                                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&txt) {
                                        let pairs = parse_index_events(&unwrap_combined(&v));
                                        if !pairs.is_empty() {
                                            if tx.send(Feed::Index(pairs)).is_err() {
                                                return;
                                            }
                                        }
                                    }
                                }
                                // Ping frames are auto-answered by tungstenite
                                // while the stream is being read — keep reading.
                                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => {
                                    break;
                                }
                                Some(Ok(_)) => {}
                            }
                        }
                        _ = shutdown.changed() => {
                            if *shutdown.borrow() {
                                let _ = ws.close(None).await;
                                return;
                            }
                        }
                    }
                }
            }
            Err(e) => {
                tracing::warn!(venue = "binance", error = %e, "index ws connect failed");
            }
        }
        let jitter = Duration::from_millis(crate::venues::rand_range(500));
        tokio::select! {
            _ = tokio::time::sleep(backoff + jitter) => {}
            _ = shutdown.changed() => {
                if *shutdown.borrow() { return; }
            }
        }
        backoff = (backoff * 2).min(Duration::from_secs(30));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> serde_json::Value {
        serde_json::from_str(s).unwrap()
    }

    #[test]
    fn depth_update_parses_best_levels() {
        let d = v(r#"{"e":"depthUpdate","E":1792331134096,"T":1792331134095,"s":"BTC-261225-85000-C","U":1,"u":2,"pu":0,"b":[["3965.000","2.98"],["3955.000","0.08"],["3950.000","0.00"]],"a":[["4025.000","0.43"],["4030.000","2.97"],["4050.000","0.00"]]}"#);
        let (sym, bbo) = parse_depth_update(&d).unwrap();
        assert_eq!(sym, "BTC-261225-85000-C");
        assert_eq!(bbo.bid_px, 3965.0);
        assert_eq!(bbo.bid_qty, 2.98);
        assert_eq!(bbo.ask_px, 4025.0);
        assert_eq!(bbo.ask_qty, 0.43);
        assert_eq!(bbo.ts, 1792331134096);
    }

    #[test]
    fn depth_update_unsorted_levels_picks_extremes() {
        // Defensive: if a venue ever ships unsorted levels, best = extremes.
        let d = v(r#"{"e":"depthUpdate","s":"ETH-261225-2400-P","b":[["100.0","1.0"],["120.0","2.0"]],"a":[["150.0","1.0"],["140.0","5.0"]]}"#);
        let (_, bbo) = parse_depth_update(&d).unwrap();
        assert_eq!(bbo.bid_px, 120.0);
        assert_eq!(bbo.ask_px, 140.0);
    }

    #[test]
    fn depth_update_rejects_empty_sides() {
        let d = v(r#"{"e":"depthUpdate","s":"X","b":[],"a":[["1","1"]]}"#);
        assert!(parse_depth_update(&d).is_none());
    }

    #[test]
    fn index_events_strip_usdt() {
        let d = v(r#"[{"e":"indexPrice","E":1,"s":"BTCUSDT","p":"81400.5"},{"e":"indexPrice","E":1,"s":"XAGUSDT","p":"57.05"},{"e":"indexPrice","E":1,"s":"SOLUSDT","p":"102.8"}]"#);
        let pairs = parse_index_events(&d);
        assert_eq!(pairs.len(), 3);
        assert_eq!(pairs[0], ("BTC".to_string(), 81400.5));
        assert_eq!(pairs[2].0, "SOL");
    }

    #[test]
    fn combined_unwrap() {
        let m = v(r#"{"stream":"btc-261225-85000-c@depth5@100ms","data":{"e":"depthUpdate","s":"BTC-261225-85000-C","b":[["10","1"]],"a":[["11","1"]]}}"#);
        let d = unwrap_combined(&m);
        assert_eq!(d.get("s").unwrap().as_str().unwrap(), "BTC-261225-85000-C");
    }

    #[test]
    fn mark_rows_normalize_fractions_to_vp() {
        let rows = v(r#"[{"symbol":"BTC-261225-85000-C","markPrice":"4146.824","bidIV":"0.36329448","askIV":"0.3672117","markIV":"0.362","riskFreeInterest":"0.053"}]"#);
        let rows = rows.as_array().unwrap().clone();
        let out = parse_mark_rows(&rows);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].0, "BTC-261225-85000-C");
        assert_eq!(out[0].1.px_mark, 4146.824);
        let m = &out[0].1;
        assert!((m.iv_mark_vp.unwrap() - 36.2).abs() < 0.01);
        assert!((m.iv_bid_vp.unwrap() - 36.329).abs() < 0.01);
        assert!((m.iv_ask_vp.unwrap() - 36.721).abs() < 0.01);
    }

    #[test]
    fn iv_solve_round_trips_a_live_quote() {
        // BTC 85000 call, ~68 DTE, spot 81400: a 3965 bid should solve to a
        // sane vol (roughly the 36% the venue publishes).
        let now = now_ms();
        let date = 20261225u32;
        let iv = solve_iv_vp(3965.0, 81_400.0, 85_000.0, 0, date, now).unwrap();
        assert!(iv > 20.0 && iv < 55.0, "iv = {iv}");
        // At/below intrinsic must not solve.
        assert!(solve_iv_vp(0.01, 81_400.0, 10_000.0, 0, date, now).is_none());
    }
}
