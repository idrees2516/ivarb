//! Paradex ("TradeOptions") connector — **JWT-free**.
//!
//! ## Why no JWT
//! Paradex REST market data (`/v1/bbo`, `/v1/orderbook`, `/v1/mark-price`)
//! and private WS channels require a JWT that **expires every 5 minutes by
//! design and cannot be extended** (official docs). Re-authenticating means
//! signing with the account's Starknet key — wrong tool for a read-only
//! research data plane, and a manually pasted token dies within minutes.
//!
//! ## The other method: the public WS market-summary feed
//! Since 2026-09-21 the public channels speak SBE binary, are **completely
//! unauthenticated**, and one subscription covers the whole exchange:
//!
//! ```text
//! wss://ws.api.prod.paradex.trade/v1?sbeSchemaId=1&sbeSchemaVersion=2
//! → {"method":"subscribe","params":{"channel":"markets_summary.ALL"}}
//! ```
//!
//! Every market — all ~9.5k option instruments *plus* perps — pushes a full
//! summary roughly every second: mark / index / last, best bid & ask with
//! sizes, venue IVs (mark/bid/ask/last), greeks, open interest and 24h USD
//! volume. Measured live: ~8.3k frames/s, ~2.2 MB/s, 9,529 distinct markets
//! (2026-10-13). That also pre-stocks the legacy-underlying bridge
//! (TSLA/NVDA/US100/XAU…) for the legacy-protocols phase.
//!
//! The universe of *instruments* still comes from the public REST endpoint
//! `/v1/markets?asset_kind=OPTION` (strike/expiry metadata, 10 min refresh).

use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use rustc_hash::FxHashMap;
use tokio::sync::watch;
use tokio_tungstenite::tungstenite::Message;

use ivarb_protocol::Right;

use crate::consolidate::{canon, format_strike};
use crate::enrich::parse_symbol;
use crate::sbe;
use crate::ticks::{DesiredMarkets, TickBbo, TickPlane};

const WS_BASE: &str = "wss://ws.api.prod.paradex.trade/v1";
/// SBE schema versions to negotiate, most recent first (prod serves 1:2; a
/// rejected version is an HTTP 400 at connect, not a silent downgrade).
const SBE_VERSIONS: [u16; 3] = [2, 1, 0];
const CHANNEL: &str = "markets_summary.ALL";
/// Quotes older than this are stale for consolidation purposes.
const QUOTE_STALE_MS: i64 = 30_000;
/// The tick-level order-book channel: a full ≤15-level snapshot of the API
/// book on every change, throttled by the exchange to one frame per 50 ms.
/// (Probed live: `deltas`/`interactive_deltas` reject the `@rate` suffix —
/// "no parameters expected" — and `snapshot@15@50ms` cannot share a session
/// with a `deltas` sub on the same market, so the tick plane runs its own
/// connection with the snapshot feed only.)
pub const BOOK_CHANNEL_SUFFIX: &str = ".snapshot@15@50ms";

// ---------------------------------------------------------------------------
// Live quote store — written by the WS task, read by the consolidation loop
// ---------------------------------------------------------------------------

/// One market's rolling summary, decoded from an SBE frame.
#[derive(Debug, Clone, Copy, Default)]
pub struct ParadexQuote {
    pub bid: Option<f64>,
    pub ask: Option<f64>,
    pub bid_size: Option<f64>,
    pub ask_size: Option<f64>,
    pub mark: Option<f64>,
    pub last: Option<f64>,
    /// Paradex's own underlying oracle for the option.
    pub index: Option<f64>,
    /// Venue-published IVs, fractions (0.4975 → 49.75 vp).
    pub mark_iv: Option<f64>,
    pub bid_iv: Option<f64>,
    pub ask_iv: Option<f64>,
    pub oi: Option<f64>,
    pub vol24h_usd: Option<f64>,
    pub delta: Option<f64>,
    pub ts_ms: i64,
}

/// Shared live state between the WS task and the research plane.
pub struct ParadexLive {
    /// canonical instrument → latest quote.
    quotes: Mutex<FxHashMap<String, ParadexQuote>>,
    connected: AtomicBool,
    /// Feed timestamp of the last decoded frame (ms).
    last_frame_ms: AtomicI64,
    /// Wall-clock ms of the last decoded frame.
    last_recv_ms: AtomicI64,
    frames: AtomicU64,
    decoded: AtomicU64,
    /// Subscribed-channel acks seen (health signal).
    subs: AtomicU64,
}

impl Default for ParadexLive {
    fn default() -> Self {
        Self {
            quotes: Mutex::new(FxHashMap::default()),
            connected: AtomicBool::new(false),
            last_frame_ms: AtomicI64::new(0),
            last_recv_ms: AtomicI64::new(0),
            frames: AtomicU64::new(0),
            decoded: AtomicU64::new(0),
            subs: AtomicU64::new(0),
        }
    }
}

impl ParadexLive {
    pub fn connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }
    pub fn last_recv_ms(&self) -> i64 {
        self.last_recv_ms.load(Ordering::Relaxed)
    }
    pub fn quote_count(&self) -> usize {
        self.quotes.lock().map(|q| q.len()).unwrap_or(0)
    }
    pub fn frames(&self) -> u64 {
        self.frames.load(Ordering::Relaxed)
    }
    /// Healthy = connected with fresh frames in the last 30 s.
    pub fn healthy(&self) -> bool {
        self.connected() && now_ms() - self.last_recv_ms() < QUOTE_STALE_MS
    }

    /// Approximate feed rate over an elapsed window (frames/s).
    pub fn rate_per_s(&self, elapsed_ms: i64) -> f64 {
        if elapsed_ms <= 0 {
            return 0.0;
        }
        self.frames.load(Ordering::Relaxed) as f64 / (elapsed_ms as f64 / 1000.0)
    }

    fn apply(&self, market: &str, s: sbe::MarketSummary) {
        let Some((und, date, strike, right)) = parse_symbol(market) else {
            return; // perps / spot / unparseable → not an option market
        };
        let q = ParadexQuote {
            bid: s.bid.filter(|x| *x > 0.0),
            ask: s.ask.filter(|x| *x > 0.0),
            bid_size: s.bid_size,
            ask_size: s.ask_size,
            mark: s.mark.filter(|x| *x > 0.0),
            last: s.last.filter(|x| *x > 0.0),
            index: s.index.filter(|x| *x > 0.0),
            mark_iv: s.mark_iv.filter(|x| *x > 0.0),
            bid_iv: s.bid_iv.filter(|x| *x > 0.0),
            ask_iv: s.ask_iv.filter(|x| *x > 0.0),
            oi: s.oi.filter(|x| *x > 0.0),
            vol24h_usd: s.vol24h_usd.filter(|x| *x > 0.0),
            delta: s.delta,
            ts_ms: s.ts_ms,
        };
        if let Ok(mut map) = self.quotes.lock() {
            map.insert(canon(&und, date, strike, right), q);
        }
    }

    /// Snapshot quotes for the given canonical instruments.
    pub fn snapshot(&self, canon_ids: &[String]) -> FxHashMap<String, ParadexQuote> {
        let mut out = FxHashMap::default();
        if let Ok(map) = self.quotes.lock() {
            for c in canon_ids {
                if let Some(q) = map.get(c) {
                    out.insert(c.clone(), *q);
                }
            }
        }
        out
    }

    /// Snapshot quotes for one underlying (option markets only), freshest
    /// first — used for status and the legacy-bridge later.
    pub fn snapshot_underlying(&self, und: &str) -> Vec<(String, ParadexQuote)> {
        let mut out = Vec::new();
        if let Ok(map) = self.quotes.lock() {
            for (c, q) in map.iter() {
                if c.starts_with(und) && c.contains('-') {
                    out.push((c.clone(), *q));
                }
            }
        }
        out
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Universe (public REST — instrument metadata)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct ParadexState {
    /// canonical instrument → (strike, right, date, underlying).
    pub universe: FxHashMap<String, (f64, Right, u32, String)>,
    /// canonical instrument → raw Paradex symbol (for tick subscriptions).
    pub symbols: FxHashMap<String, String>,
    /// Underlying → instrument count.
    pub by_und: FxHashMap<String, usize>,
    /// True when the public WS quote feed is healthy.
    pub quotes_ok: bool,
    pub note: String,
}

impl ParadexState {
    pub fn market_count(&self) -> u32 {
        self.universe.len() as u32
    }

    /// Crypto underlyings we track (the engine's list), for chain display.
    pub fn crypto_universe(&self, unds: &[String]) -> Vec<(String, usize, Vec<u32>)> {
        unds
            .iter()
            .filter_map(|u| {
                let n = *self.by_und.get(u)?;
                let mut dates: Vec<u32> = self
                    .universe
                    .values()
                    .filter(|(_, _, d, und)| und == u && *d > 20260101)
                    .map(|(_, _, d, _)| *d)
                    .collect::<std::collections::BTreeSet<_>>()
                    .into_iter()
                    .collect();
                dates.sort_unstable();
                Some((u.clone(), n, dates))
            })
            .collect()
    }
}

/// Refresh the public market universe (instrument metadata only; quotes come
/// from the WS feed).
pub async fn poll_universe(http: &reqwest::Client) -> anyhow::Result<ParadexState> {
    let mut st = ParadexState {
        note: "universe live (public API); quotes via public WS".into(),
        ..Default::default()
    };
    let v: serde_json::Value = http
        .get("https://api.prod.paradex.trade/v1/markets?asset_kind=OPTION")
        .timeout(Duration::from_secs(20))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let results = v
        .get("results")
        .and_then(|r| r.as_array())
        .cloned()
        .unwrap_or_default();
    for r in &results {
        let Some(sym) = r.get("symbol").and_then(|s| s.as_str()) else { continue };
        let Some((und, date, strike, right)) = parse_symbol(sym) else { continue };
        let c = canon(&und, date, strike, right);
        st.universe.insert(c.clone(), (strike, right, date, und.clone()));
        st.symbols.insert(c, sym.to_string());
        *st.by_und.entry(und).or_insert(0) += 1;
    }
    if st.universe.is_empty() {
        anyhow::bail!("paradex universe empty");
    }
    Ok(st)
}

/// Build a display symbol `BTC-USD-23OCT26-80000-P` from parts.
pub fn display_symbol(und: &str, date: u32, strike: f64, right: Right) -> String {
    format!("{und}-USD-{}-{}-{}", date, format_strike(strike), right.letter())
}

/// Paradex perp symbol for an underlying (verified live: `BTC-USD-PERP`).
pub fn perp_symbol(und: &str) -> String {
    format!("{und}-USD-PERP")
}

// ---------------------------------------------------------------------------
// 50 ms order-book tick task — `order_book.{market}.snapshot@15@50ms`
// ---------------------------------------------------------------------------

/// Spawn the tick-level book task. `desired` carries raw market symbols
/// (e.g. `BTC-USD-19OCT26-82000-C`, `BTC-USD-PERP`); the task reconciles
/// subscriptions incrementally as the set moves, so there is no reconnect
/// and no data gap when the chain rolls to new expiries.
pub fn spawn_book_ws(plane: Arc<TickPlane>, mut desired: DesiredMarkets, shutdown: watch::Receiver<bool>) {
    tokio::spawn(async move {
        let mut backoff = Duration::from_secs(1);
        let mut shutdown = shutdown;
        loop {
            if *shutdown.borrow() {
                break;
            }
            match book_session(&plane, &mut desired, &mut shutdown).await {
                Ok(()) => {
                    tracing::info!("paradex 50ms book session ended cleanly");
                }
                Err(e) => {
                    tracing::warn!(error = %e, "paradex 50ms book session failed");
                }
            }
            plane.feed_state("paradex", false, 0, "50ms book reconnecting");
            let jitter = (rand::random::<u64>() % 500) as u64;
            let wait = backoff + Duration::from_millis(jitter);
            tracing::info!(?wait, "paradex 50ms book reconnecting");
            tokio::select! {
                _ = tokio::time::sleep(wait) => {}
                _ = shutdown.changed() => {
                    if *shutdown.borrow() { break; }
                }
            }
            backoff = (backoff * 2).min(Duration::from_secs(60));
        }
    });
}

/// One 50 ms book connection: subscribe, reconcile on set changes, pump.
async fn book_session(
    plane: &Arc<TickPlane>,
    desired: &mut DesiredMarkets,
    shutdown: &mut watch::Receiver<bool>,
) -> anyhow::Result<()> {
    let mut ws = None;
    let mut last_err: Option<String> = None;
    for ver in SBE_VERSIONS {
        let url = format!("{WS_BASE}?sbeSchemaId=1&sbeSchemaVersion={ver}");
        match tokio_tungstenite::connect_async(&url).await {
            Ok((w, _resp)) => {
                tracing::info!(version = ver, "paradex 50ms book ws connected (SBE 1:{ver}, no auth)");
                ws = Some(w);
                break;
            }
            Err(e) => {
                tracing::warn!(version = ver, error = %e, "paradex book ws negotiate failed");
                last_err = Some(e.to_string());
            }
        }
    }
    let ws = ws.ok_or_else(|| anyhow::anyhow!("all SBE versions rejected: {last_err:?}"))?;
    let (mut sink, mut stream) = ws.split();
    plane.feed_state("paradex", true, 0, "50ms book connected");

    let mut req_id = 0u64;
    let mut current: Vec<String> = Vec::new();

    // Initial reconcile against the current desired set.
    let want = desired.borrow().clone();
    book_reconcile(&mut sink, &mut req_id, &mut current, &want).await?;
    plane.feed_state("paradex", true, current.len() as u32, "");

    loop {
        tokio::select! {
            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    let _ = sink.close().await;
                    return Ok(());
                }
            }
            changed = desired.changed() => {
                if changed.is_ok() {
                    let want = desired.borrow().clone();
                    book_reconcile(&mut sink, &mut req_id, &mut current, &want).await?;
                    plane.feed_state("paradex", true, current.len() as u32, "");
                }
            }
            msg = stream.next() => {
                match msg {
                    Some(Ok(Message::Binary(b))) => {
                        plane.note_frame("paradex");
                        if let Ok((market, bu)) = sbe::decode_book(&b) {
                            if bu.is_snapshot {
                                let (bid, ask) = bu.bbo();
                                if let Some((und, date, strike, right)) = parse_symbol(&market) {
                                    let c = canon(&und, date, strike, right);
                                    plane.apply("paradex", &c, TickBbo {
                                        bid: bid.map(|(p, _)| p),
                                        ask: ask.map(|(p, _)| p),
                                        bid_size: bid.map(|(_, s)| s),
                                        ask_size: ask.map(|(_, s)| s),
                                        seq: bu.seq,
                                        feed_ts_ms: bu.ts_ms,
                                        recv_ms: crate::now_ms(),
                                        ..Default::default()
                                    });
                                }
                                // Perp frames keep the feed's liveness hot but
                                // are not option-chain quotes — nothing stored.
                            }
                        }
                        // Non-book templates / malformed frames are skipped
                        // per the schema contract; the feed keeps flowing.
                    }
                    Some(Ok(Message::Text(t))) => {
                        if t.contains("\"error\"") {
                            tracing::warn!(msg = %t, "paradex book ws control error");
                        }
                    }
                    Some(Ok(Message::Ping(p))) => {
                        sink.send(Message::Pong(p)).await?;
                    }
                    Some(Ok(Message::Pong(_))) => {}
                    Some(Ok(Message::Close(c))) => {
                        anyhow::bail!("paradex book ws closed by server: {c:?}");
                    }
                    Some(Ok(_)) => {}
                    Some(Err(e)) => anyhow::bail!("paradex book ws read error: {e}"),
                    None => anyhow::bail!("paradex book ws stream ended"),
                }
            }
        }
    }
}

/// Subscribe to new markets, unsubscribe gone ones, keep `current` in sync.
async fn book_reconcile<S, E>(
    sink: &mut S,
    req_id: &mut u64,
    current: &mut Vec<String>,
    desired: &[String],
) -> anyhow::Result<()>
where
    S: futures_util::Sink<Message, Error = E> + Unpin,
    anyhow::Error: From<E>,
{
    let (add, rm) = crate::ticks::reconcile_sets(current, desired);
    let n_add = add.len();
    let n_rm = rm.len();
    for sym in add {
        *req_id += 1;
        let msg = serde_json::json!({
            "id": *req_id,
            "jsonrpc": "2.0",
            "method": "subscribe",
            "params": {"channel": format!("order_book.{sym}{BOOK_CHANNEL_SUFFIX}")}
        });
        sink.send(Message::Text(msg.to_string())).await?;
        current.push(sym);
    }
    for sym in rm {
        *req_id += 1;
        let msg = serde_json::json!({
            "id": *req_id,
            "jsonrpc": "2.0",
            "method": "unsubscribe",
            "params": {"channel": format!("order_book.{sym}{BOOK_CHANNEL_SUFFIX}")}
        });
        sink.send(Message::Text(msg.to_string())).await?;
        current.retain(|s| s != &sym);
    }
    if n_add > 0 || n_rm > 0 {
        tracing::info!(added = n_add, removed = n_rm, total = current.len(), "paradex 50ms book set reconciled");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Public WS task — supervised connect → subscribe → decode loop
// ---------------------------------------------------------------------------

/// Spawn the public market-data WS task. Fail-soft: on error it reconnects
/// with exponential backoff; `live` always stays usable (stale quotes simply
/// stop updating).
pub fn spawn_ws(live: Arc<ParadexLive>, shutdown: watch::Receiver<bool>) {
    tokio::spawn(async move {
        let mut backoff = Duration::from_secs(1);
        let mut shutdown = shutdown;
        loop {
            if *shutdown.borrow() {
                break;
            }
            match session(&live, &mut shutdown).await {
                Ok(()) => {
                    tracing::info!("paradex public ws session ended cleanly");
                }
                Err(e) => {
                    tracing::warn!(error = %e, "paradex public ws session failed");
                }
            }
            live.connected.store(false, Ordering::Relaxed);
            let jitter = (rand::random::<u64>() % 500) as u64;
            let wait = backoff + Duration::from_millis(jitter);
            tracing::info!(?wait, "paradex public ws reconnecting");
            tokio::select! {
                _ = tokio::time::sleep(wait) => {}
                _ = shutdown.changed() => {
                    if *shutdown.borrow() { break; }
                }
            }
            backoff = (backoff * 2).min(Duration::from_secs(60));
        }
    });
}

/// One connection session: negotiate SBE, subscribe, pump frames until error.
async fn session(
    live: &ParadexLive,
    shutdown: &mut watch::Receiver<bool>,
) -> anyhow::Result<()> {
    // Negotiate the newest SBE schema version the server accepts.
    let mut ws = None;
    let mut last_err: Option<String> = None;
    for ver in SBE_VERSIONS {
        let url = format!("{WS_BASE}?sbeSchemaId=1&sbeSchemaVersion={ver}");
        match tokio_tungstenite::connect_async(&url).await {
            Ok((w, _resp)) => {
                tracing::info!(version = ver, "paradex public ws connected (SBE 1:{ver}, no auth)");
                ws = Some(w);
                break;
            }
            Err(e) => {
                tracing::warn!(version = ver, error = %e, "paradex ws negotiate failed");
                last_err = Some(e.to_string());
            }
        }
    }
    let ws = ws.ok_or_else(|| anyhow::anyhow!("all SBE versions rejected: {last_err:?}"))?;
    live.connected.store(true, Ordering::Relaxed);
    let (mut sink, mut stream) = ws.split();

    // Subscribe to the whole exchange's rolling summaries — public, no auth.
    let sub = serde_json::json!({
        "id": 1,
        "jsonrpc": "2.0",
        "method": "subscribe",
        "params": {"channel": CHANNEL}
    });
    sink.send(Message::Text(sub.to_string())).await?;

    loop {
        tokio::select! {
            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    let _ = sink.close().await;
                    return Ok(());
                }
            }
            msg = stream.next() => {
                match msg {
                    Some(Ok(Message::Binary(b))) => {
                        live.frames.fetch_add(1, Ordering::Relaxed);
                        if let Ok((market, s)) = sbe::decode_with_market(&b) {
                            live.decoded.fetch_add(1, Ordering::Relaxed);
                            live.apply(&market, s);
                            live.last_frame_ms.store(s.ts_ms, Ordering::Relaxed);
                            live.last_recv_ms.store(now_ms(), Ordering::Relaxed);
                        }
                        // Unknown templates / malformed frames are skipped
                        // per the schema contract — the feed keeps flowing.
                    }
                    Some(Ok(Message::Text(t))) => {
                        // JSON-RPC control plane: acks and errors.
                        if t.contains("\"error\"") {
                            tracing::warn!(msg = %t, "paradex ws control error");
                        } else if t.contains("markets_summary") {
                            live.subs.fetch_add(1, Ordering::Relaxed);
                            tracing::debug!(msg = %t, "paradex ws subscribed");
                        }
                    }
                    Some(Ok(Message::Ping(p))) => {
                        // Server pings every 55 s; must pong within 5 s.
                        sink.send(Message::Pong(p)).await?;
                    }
                    Some(Ok(Message::Pong(_))) => {}
                    Some(Ok(Message::Close(c))) => {
                        anyhow::bail!("paradex ws closed by server: {c:?}");
                    }
                    Some(Ok(_)) => {} // future frame kinds
                    Some(Err(e)) => anyhow::bail!("paradex ws read error: {e}"),
                    None => anyhow::bail!("paradex ws stream ended"),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn universe_parse_shape() {
        // Sample symbols seen live from /v1/markets.
        for s in [
            "SOL-USD-30OCT26-300-C",
            "ZEC-USD-23OCT26-2400-P",
            "BTC-USD-23OCT26-80000-P",
        ] {
            assert!(parse_symbol(s).is_some(), "{s}");
        }
        assert_eq!(
            parse_symbol("BTC-USD-23OCT26-80000-P").unwrap().0,
            "BTC"
        );
    }

    #[test]
    fn perp_symbols_are_not_option_markets() {
        // The ALL stream includes perps — they must be filtered out.
        assert!(parse_symbol("BTC-USD-PERP").is_none());
    }

    #[test]
    fn live_store_applies_and_snapshots() {
        let live = ParadexLive::default();
        live.apply(
            "BTC-USD-16OCT26-84000-C",
            sbe::MarketSummary {
                ts_ms: 123,
                bid: Some(8.4),
                ask: Some(8.6),
                bid_size: Some(1.25),
                ask_size: Some(0.75),
                mark: Some(8.49),
                last: None,
                index: Some(83000.0),
                mark_iv: Some(0.4975),
                bid_iv: None,
                ask_iv: Some(0.53),
                last_iv: None,
                vol24h_usd: Some(1000.0),
                oi: Some(42.0),
                delta: Some(-0.8),
                gamma: None,
                vega: None,
                theta: None,
            },
        );
        let canon_id = canon("BTC", 20261016, 84000.0, Right::Call);
        let snap = live.snapshot(&[canon_id.clone()]);
        let q = snap.get(&canon_id).unwrap();
        assert_eq!(q.bid, Some(8.4));
        assert_eq!(q.mark_iv, Some(0.4975));
        assert_eq!(live.quote_count(), 1);
        assert!(live.snapshot(&["nope".to_string()]).is_empty());
        // Not healthy until a frame arrives recently (last_recv_ms == 0).
        assert!(!live.healthy());
    }
}
