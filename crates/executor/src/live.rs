//! Live order gateway (Deribit private WebSocket) — the authenticated path
//! scaffolded from the paper-first executor.
//!
//! Arming chain (ALL must hold, otherwise the executor stays in paper mode):
//! 1. `IVARB_MODE=live`
//! 2. `DERIBIT_API_KEY` + `DERIBIT_API_SECRET` (trade scope, withdrawal OFF)
//! 3. `IVARB_LIVE_CONFIRM=yes` — an explicit acknowledgement that real orders
//!    will be placed. This third gate exists so a stray `IVARB_MODE=live`
//!    alone can never arm the book.
//!
//! What the scaffold does safely today:
//! - `live_preflight()` — env-gate report + connect + `public/auth` +
//!   `private/get_account_summary` (equity / available funds). Never orders.
//! - `live_smoke_test()` — one minimum-size IOC buy followed by an immediate
//!   sell-back on the same instrument, priced off the live order book. Proves
//!   the whole order path end-to-end for the cost of the spread + fees
//!   (free on test.deribit.com).
//! - Two-leg attempts in live mode additionally require a live gateway on
//!   BOTH venues (see `Core::live_route_ok`) — a simulated leg inside a live
//!   attempt is forbidden, so with only Deribit keys the taker refuses
//!   cross-venue routes rather than faking the second leg.
//!
//! Deribit specifics handled here:
//! - Auth: `public/auth` with `grant_type=client_credentials` (no HMAC needed).
//! - BTC/ETH options are **coin-margined**: premiums are quoted in the
//!   underlying, so a USD premium `p` at spot `S` is sent as `p / S` BTC.
//! - `amount` is in underlying units (1 contract = 1 BTC / 1 ETH; 0.1 min).
//! - Options price tick is 0.0001 BTC; orders are limit +
//!   `immediate_or_cancel`; the synchronous response already carries
//!   `order_state` / `average_price` / `filled_amount`.
//!
//! The gateway also carries venue symbol formatters for every exchange so
//! live routing (and the UI) can translate canonical instruments
//! (`BTC-20260926-85000-C`) into venue-native names.

use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message;

use ivarb_protocol::ExecMode;

use crate::paper::{Fill, Side};

// ---------------------------------------------------------------------------
// Venue symbol formatting (canonical → venue-native)
// ---------------------------------------------------------------------------

const MONTHS: [&str; 12] = [
    "JAN", "FEB", "MAR", "APR", "MAY", "JUN", "JUL", "AUG", "SEP", "OCT", "NOV", "DEC",
];

fn ddmmmyy(ymd: u32) -> String {
    let (y, m, d) = ((ymd / 10_000) as i32, (ymd / 100) % 100, ymd % 100);
    format!("{d:02}{}{:02}", MONTHS[(m - 1) as usize], y % 100)
}

fn yymmdd(ymd: u32) -> String {
    format!("{:02}{:02}{:02}", (ymd / 10_000) % 100, (ymd / 100) % 100, ymd % 100)
}

fn ddmmyy(ymd: u32) -> String {
    format!("{:02}{:02}{:02}", ymd % 100, (ymd / 100) % 100, (ymd / 10_000) % 100)
}

fn strike_s(strike: f64) -> String {
    if (strike - strike.round()).abs() < 1e-9 {
        format!("{}", strike.round() as i64)
    } else {
        format!("{strike}")
    }
}

/// Render a canonical instrument in a venue's native symbol style.
pub fn venue_symbol(venue: &str, und: &str, ymd: u32, strike: f64, right: &str) -> String {
    let k = strike_s(strike);
    match venue {
        "deribit" => format!("{und}-{}-{k}-{right}", ddmmmyy(ymd)),
        "bybit" => format!("{und}-{}-{k}-{right}-USDT", ddmmmyy(ymd)),
        "okx" => format!("{und}-USD-{}-{k}-{right}", yymmdd(ymd)),
        "binance" | "aevo" => format!("{und}-{}-{k}-{right}", yymmdd(ymd)),
        "derive" => format!("{und}-{ymd}-{k}-{right}"),
        "delta" => format!("{right}-{und}-{k}-{}", ddmmyy(ymd)),
        "thalex" => format!("{und}-{}-{k}-{right}", yymmdd(ymd)),
        _ => format!("{und}-{ymd}-{k}-{right}"),
    }
}

/// yyyymmdd from a unix-ms timestamp (UTC).
pub fn ymd_from_ms(ms: i64) -> u32 {
    let days = (ms / 86_400_000).max(0) as i64;
    // civil_from_days (Howard Hinnant's algorithm)
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y * 10_000 + m * 100 + d) as u32
}

// ---------------------------------------------------------------------------
// Arming: which env gates are satisfied, and why not
// ---------------------------------------------------------------------------

/// Everything needed to decide (and explain) live arming.
#[derive(Debug, Clone)]
pub struct LiveArming {
    pub requested: bool,
    pub keys: Option<(String, String)>,
    pub ws_url: String,
    pub confirmed: bool,
}

impl LiveArming {
    pub fn fully_armed(&self) -> bool {
        self.requested && self.keys.is_some() && self.confirmed
    }

    pub fn mode(&self) -> ExecMode {
        if self.fully_armed() {
            ExecMode::Live
        } else {
            ExecMode::Paper
        }
    }

    /// Human-readable list of exactly what stands between here and live mode.
    pub fn blockers(&self) -> Vec<String> {
        let mut b = Vec::new();
        if !self.requested {
            b.push("IVARB_MODE=live is not set — the taker runs PAPER fills".into());
        }
        if self.keys.is_none() {
            b.push("DERIBIT_API_KEY / DERIBIT_API_SECRET are not set".into());
        }
        if !self.confirmed {
            b.push("IVARB_LIVE_CONFIRM=yes is not set (explicit acknowledgement \
                    that real orders will be placed)"
                .into());
        }
        b
    }
}

/// Deribit WS endpoint. Point DERIBIT_WS_URL at `wss://test.deribit.com/ws/api/v2`
/// to rehearse the whole live path on testnet for free.
pub fn deribit_ws_url() -> String {
    std::env::var("DERIBIT_WS_URL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "wss://www.deribit.com/ws/api/v2".into())
}

pub fn live_arming_from_env() -> LiveArming {
    let keys = match (
        std::env::var("DERIBIT_API_KEY").ok().filter(|s| !s.is_empty()),
        std::env::var("DERIBIT_API_SECRET").ok().filter(|s| !s.is_empty()),
    ) {
        (Some(k), Some(s)) => Some((k, s)),
        _ => None,
    };
    LiveArming {
        requested: std::env::var("IVARB_MODE")
            .map(|m| m.eq_ignore_ascii_case("live"))
            .unwrap_or(false),
        keys,
        ws_url: deribit_ws_url(),
        confirmed: std::env::var("IVARB_LIVE_CONFIRM")
            .map(|v| v.trim().eq_ignore_ascii_case("yes"))
            .unwrap_or(false),
    }
}

/// Back-compat wrapper.
pub fn live_mode_from_env() -> (ExecMode, Option<(String, String)>) {
    let a = live_arming_from_env();
    (a.mode(), a.keys.clone())
}

// ---------------------------------------------------------------------------
// Deribit live session
// ---------------------------------------------------------------------------

pub struct DeribitSession {
    req_tx: mpsc::UnboundedSender<DeribitReq>,
}

enum DeribitReq {
    Rpc {
        method: String,
        params: serde_json::Value,
        resp: oneshot::Sender<anyhow::Result<serde_json::Value>>,
    },
}

/// Options price tick in BTC (0.0001) and amount step (0.1).
const PX_TICK_BTC: f64 = 0.0001;
const AMOUNT_STEP: f64 = 0.1;

fn ceil_tick(x: f64, tick: f64) -> f64 {
    snap((x / tick).ceil() * tick)
}
fn floor_tick(x: f64, tick: f64) -> f64 {
    snap((x / tick).floor() * tick)
}
/// Kill f64 representation noise so a serialized price lands exactly on a
/// tick (124 * 0.0001 must serialize as 0.0124, not 0.012400000000000001 —
/// venues reject off-tick prices).
fn snap(v: f64) -> f64 {
    (v * 1e10).round() / 1e10
}
/// Snap an order size to the 0.1-contract amount step, decimal-exact.
/// (`x / 0.1` in binary floats turns 0.15 into 1.4999…8 → rounds DOWN to
/// 0.1; scaling first keeps 0.15 → 1.5 → 0.2 as the venue intends.)
fn snap_amount(v: f64) -> f64 {
    (v * 10.0).round() / 10.0
}

impl DeribitSession {
    /// Connect + authenticate against the default (prod) endpoint.
    pub async fn connect(api_key: &str, api_secret: &str) -> anyhow::Result<Arc<Self>> {
        Self::connect_ws(&deribit_ws_url(), api_key, api_secret).await
    }

    /// Connect + authenticate + start the session task against a specific
    /// endpoint (prod or test.deribit.com).
    pub async fn connect_ws(
        ws_url: &str,
        api_key: &str,
        api_secret: &str,
    ) -> anyhow::Result<Arc<Self>> {
        let (ws, _) = tokio_tungstenite::connect_async(ws_url).await?;
        let (mut sink, mut stream) = ws.split();

        // Authenticate.
        let auth = serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "public/auth",
            "params": {
                "grant_type": "client_credentials",
                "client_id": api_key,
                "client_secret": api_secret,
            }
        });
        sink.send(Message::Text(auth.to_string())).await?;
        let resp = tokio::time::timeout(Duration::from_secs(10), stream.next()).await;
        let ok = matches!(
            resp,
            Ok(Some(Ok(Message::Text(ref t)))) if t.contains("\"result\"") && t.contains("access_token")
        );
        if !ok {
            anyhow::bail!("deribit auth failed (check key/secret and endpoint)");
        }

        let (req_tx, mut req_rx) = mpsc::unbounded_channel::<DeribitReq>();
        let session = Arc::new(Self { req_tx });

        // Pump: route requests to the WS, match responses by echo of id.
        tokio::spawn(async move {
            let mut next_id: u64 = 10;
            let mut pending: rustc_hash::FxHashMap<
                u64,
                oneshot::Sender<anyhow::Result<serde_json::Value>>,
            > = rustc_hash::FxHashMap::default();
            loop {
                tokio::select! {
                    req = req_rx.recv() => match req {
                        Some(DeribitReq::Rpc { method, params, resp }) => {
                            next_id += 1;
                            let id = next_id;
                            let msg = serde_json::json!({
                                "jsonrpc": "2.0", "id": id, "method": method,
                                "params": params
                            });
                            if sink.send(Message::Text(msg.to_string())).await.is_ok() {
                                pending.insert(id, resp);
                            } else {
                                let _ = resp.send(Err(anyhow::anyhow!("deribit ws closed")));
                            }
                        }
                        None => break,
                    },
                    msg = stream.next() => match msg {
                        Some(Ok(Message::Text(txt))) => {
                            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&txt) {
                                let id = v.get("id").and_then(|i| i.as_u64()).unwrap_or(0);
                                if let Some(tx) = pending.remove(&id) {
                                    let r = match v.get("error").filter(|e| !e.is_null()) {
                                        Some(err) => Err(anyhow::anyhow!("deribit error: {err}")),
                                        None => Ok(v["result"].clone()),
                                    };
                                    let _ = tx.send(r);
                                }
                            }
                        }
                        Some(Ok(Message::Ping(p))) => { let _ = sink.send(Message::Pong(p)).await; }
                        Some(Ok(_)) => {}
                        Some(Err(_)) | None => break,
                    },
                    _ = tokio::time::sleep(Duration::from_secs(20)) => {
                        let hb = serde_json::json!({
                            "jsonrpc": "2.0", "id": 0, "method": "public/test"
                        });
                        if sink.send(Message::Text(hb.to_string())).await.is_err() {
                            break;
                        }
                    }
                }
            }
            tracing::warn!("deribit live session ended");
        });

        Ok(session)
    }

    /// Generic JSON-RPC call over the authenticated session.
    pub async fn rpc(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> anyhow::Result<serde_json::Value> {
        let (tx, rx) = oneshot::channel();
        self.req_tx
            .send(DeribitReq::Rpc {
                method: method.into(),
                params,
                resp: tx,
            })
            .map_err(|_| anyhow::anyhow!("deribit session gone"))?;
        match tokio::time::timeout(Duration::from_secs(12), rx).await {
            Ok(Ok(r)) => r,
            Ok(Err(_)) => Err(anyhow::anyhow!("deribit rpc dropped: {method}")),
            Err(_) => Err(anyhow::anyhow!("deribit rpc timeout: {method}")),
        }
    }

    /// `private/get_account_summary` for one currency (BTC/ETH/…).
    pub async fn account_summary(&self, currency: &str) -> anyhow::Result<serde_json::Value> {
        self.rpc(
            "private/get_account_summary",
            serde_json::json!({ "currency": currency }),
        )
        .await
    }

    /// Best bid/ask (in BTC premium) + spot for one option instrument.
    pub async fn order_book(&self, instrument: &str) -> anyhow::Result<(f64, f64, f64)> {
        let v = self
            .rpc(
                "public/get_order_book",
                serde_json::json!({ "instrument_name": instrument }),
            )
            .await?;
        let f = |k: &str| v.get(k).and_then(|x| x.as_f64()).unwrap_or(0.0);
        Ok((f("best_bid_price"), f("best_ask_price"), f("underlying_price")))
    }

    /// Place one IOC limit order. `price_btc` is the coin-margined premium.
    pub async fn place_ioc(
        &self,
        symbol: &str,
        side: Side,
        amount: f64,
        price_btc: f64,
        label: &str,
    ) -> anyhow::Result<Fill> {
        let method = match side {
            Side::Buy => "private/buy",
            Side::Sell => "private/sell",
        };
        let amount = snap_amount(amount);
        let v = self
            .rpc(
                method,
                serde_json::json!({
                    "instrument_name": symbol,
                    "amount": (amount * 10.0).round() / 10.0,
                    "type": "limit",
                    "price": (price_btc * 10_000.0).round() / 10_000.0,
                    "time_in_force": "immediate_or_cancel",
                    "label": label,
                }),
            )
            .await?;
        parse_order_result(&v)
    }

    /// Place an IOC order; converts USD premium → coin-margined BTC price.
    /// Kept for the future live broker swap (same shape as PaperBroker::take).
    #[allow(clippy::too_many_arguments)]
    pub async fn take(
        &self,
        venue: &str,
        venue_symbol: &str,
        side: Side,
        px_unit_usd: f64,
        contracts: f64,
        _taker_rate: f64,
        spot: f64,
        _contract_size: f64,
    ) -> Fill {
        let _ = venue;
        let price_btc = if spot > 0.0 { px_unit_usd / spot } else { 0.0 };
        match self
            .place_ioc(venue_symbol, side, contracts, price_btc, "ivarb-live")
            .await
        {
            Ok(fill) => fill,
            Err(e) => {
                tracing::warn!(error = %e, "deribit order rejected");
                Fill::zero()
            }
        }
    }
}

fn parse_order_result(v: &serde_json::Value) -> anyhow::Result<Fill> {
    let order = v
        .get("order")
        .ok_or_else(|| anyhow::anyhow!("no order in result"))?;
    let filled = order
        .get("filled_amount")
        .and_then(|x| x.as_f64())
        .unwrap_or(0.0);
    let avg_px_btc = order
        .get("average_price")
        .and_then(|x| x.as_f64())
        .unwrap_or(0.0);
    // Coin-margined: premium in BTC → USD per unit.
    let spot = order
        .get("underlying_price")
        .and_then(|x| x.as_f64())
        .unwrap_or(0.0);
    let px_unit_usd = if spot > 0.0 { avg_px_btc * spot } else { 0.0 };
    Ok(Fill {
        contracts: filled,
        px_unit: px_unit_usd,
        fee_usd: 0.0, // reported in the currency-margined reconciliation
        latency_ms: 0,
    })
}

// ---------------------------------------------------------------------------
// Preflight: validate the authenticated path without strategy orders
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct LiveCheck {
    pub name: String,
    pub ok: bool,
    pub detail: String,
}

impl LiveCheck {
    pub fn line(&self) -> String {
        format!(
            "{}: {} — {}",
            self.name,
            if self.ok { "OK" } else { "FAIL" },
            self.detail
        )
    }
}

/// Full arming + credential + account check. Never places orders, so it is
/// safe to run from the UI at any time, even in paper mode.
pub async fn live_preflight() -> Vec<LiveCheck> {
    let arm = live_arming_from_env();
    let mut checks = vec![
        LiveCheck {
            name: "mode".into(),
            ok: arm.requested,
            detail: if arm.requested {
                "IVARB_MODE=live".into()
            } else {
                "IVARB_MODE=live not set (paper fills)".into()
            },
        },
        LiveCheck {
            name: "keys".into(),
            ok: arm.keys.is_some(),
            detail: if arm.keys.is_some() {
                "DERIBIT_API_KEY / DERIBIT_API_SECRET present".into()
            } else {
                "DERIBIT_API_KEY / DERIBIT_API_SECRET missing".into()
            },
        },
        LiveCheck {
            name: "confirm".into(),
            ok: arm.confirmed,
            detail: if arm.confirmed {
                "IVARB_LIVE_CONFIRM=yes".into()
            } else {
                "IVARB_LIVE_CONFIRM=yes not set".into()
            },
        },
    ];

    if let Some((k, s)) = &arm.keys {
        match DeribitSession::connect_ws(&arm.ws_url, k, s).await {
            Ok(sess) => {
                checks.push(LiveCheck {
                    name: "auth".into(),
                    ok: true,
                    detail: format!("authenticated against {}", arm.ws_url),
                });
                match sess.account_summary("BTC").await {
                    Ok(v) => {
                        let f = |k: &str| v.get(k).and_then(|x| x.as_f64()).unwrap_or(0.0);
                        let (eq, avail) = (f("equity"), f("available_funds"));
                        checks.push(LiveCheck {
                            name: "account".into(),
                            ok: eq > 0.0,
                            detail: format!("BTC equity {eq:.4} · available {avail:.4}"),
                        });
                        let vol = f("options_vol");
                        let pos = f("open_options_orders");
                        checks.push(LiveCheck {
                            name: "trading".into(),
                            ok: true,
                            detail: format!("options volume {vol} · open option orders {pos}"),
                        });
                    }
                    Err(e) => checks.push(LiveCheck {
                        name: "account".into(),
                        ok: false,
                        detail: e.to_string(),
                    }),
                }
            }
            Err(e) => checks.push(LiveCheck {
                name: "auth".into(),
                ok: false,
                detail: e.to_string(),
            }),
        }
    }
    checks
}

// ---------------------------------------------------------------------------
// Smoke test: prove the order path with one bounded round-trip
// ---------------------------------------------------------------------------

pub struct SmokeResult {
    pub instrument: String,
    pub buy: Fill,
    pub sell: Fill,
    /// Sell proceeds − buy cost (USD, negative — the deliberate cost of the
    /// test: spread given away + fees on both legs).
    pub round_trip_usd: f64,
}

/// Buy `amount` (default 0.1 = Deribit's option minimum) of `instrument` IOC
/// at the touch, then immediately sell it back IOC at the opposite touch.
/// Prices are taken from the live book, so the cost is bounded by the visible
/// spread plus fees. Free on test.deribit.com.
pub async fn live_smoke_test(
    sess: &DeribitSession,
    instrument: &str,
    amount: f64,
) -> anyhow::Result<SmokeResult> {
    let (bid, ask, spot) = sess.order_book(instrument).await?;
    if ask <= 0.0 || spot <= 0.0 {
        anyhow::bail!("no live ask on {instrument} (empty book?)");
    }
    let amount = if amount <= 0.0 { AMOUNT_STEP } else { amount };

    // Buy across the ask with a 1% buffer so the IOC fills through jitter.
    let buy_btc = ceil_tick(ask * 1.01, PX_TICK_BTC).max(PX_TICK_BTC);
    let buy = sess
        .place_ioc(instrument, Side::Buy, amount, buy_btc, "ivarb-smoke-buy")
        .await?;
    if buy.contracts <= 0.0 {
        anyhow::bail!("smoke buy did not fill (ask {ask} BTC, sent {buy_btc} BTC)");
    }

    // Sell back across the (fresh) bid with a 1% buffer.
    let (bid2, _, _) = sess.order_book(instrument).await?;
    let bid_ref = if bid2 > 0.0 { bid2 } else { bid };
    let sell_btc = floor_tick(bid_ref * 0.99, PX_TICK_BTC).max(PX_TICK_BTC);
    let sell = sess
        .place_ioc(
            instrument,
            Side::Sell,
            buy.contracts,
            sell_btc,
            "ivarb-smoke-sell",
        )
        .await?;

    let round_trip = sell.contracts * sell.px_unit - buy.contracts * buy.px_unit;
    Ok(SmokeResult {
        instrument: instrument.to_string(),
        buy,
        sell,
        round_trip_usd: round_trip,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn symbol_formats() {
        // canonical: BTC 2026-09-26 85000 C
        assert_eq!(venue_symbol("deribit", "BTC", 20260926, 85000.0, "C"), "BTC-26SEP26-85000-C");
        assert_eq!(venue_symbol("okx", "BTC", 20260926, 85000.0, "C"), "BTC-USD-260926-85000-C");
        assert_eq!(venue_symbol("bybit", "BTC", 20260926, 85000.0, "P"), "BTC-26SEP26-85000-P-USDT");
        assert_eq!(venue_symbol("derive", "BTC", 20260926, 85000.0, "C"), "BTC-20260926-85000-C");
        assert_eq!(venue_symbol("delta", "BTC", 20260926, 83400.0, "C"), "C-BTC-83400-260926");
        assert_eq!(venue_symbol("binance", "ETH", 20260926, 3000.5, "P"), "ETH-260926-3000.5-P");
        assert_eq!(ddmmmyy(20260926), "26SEP26");
        assert_eq!(yymmdd(20260926), "260926");
        assert_eq!(ddmmyy(20260926), "260926");
    }

    #[test]
    fn ymd_roundtrip() {
        // 2026-09-26 00:00 UTC = 1789968000000 ms? Verify via a known epoch:
        // 1970-01-01 → ymd 19700101; 2026-09-26 → 20260926.
        assert_eq!(ymd_from_ms(0), 19700101);
        // 2024-02-29 (leap) 12:00 UTC
        assert_eq!(ymd_from_ms(1709208000000), 20240229);
        // year boundary: 2025-12-31 23:59 UTC
        assert_eq!(ymd_from_ms(1767225540000), 20251231);
    }

    #[test]
    fn arming_gates() {
        let mut a = LiveArming {
            requested: false,
            keys: None,
            ws_url: deribit_ws_url(),
            confirmed: false,
        };
        assert!(!a.fully_armed());
        assert_eq!(a.blockers().len(), 3);
        assert!(matches!(a.mode(), ExecMode::Paper));

        a.requested = true;
        assert!(!a.fully_armed());
        assert_eq!(a.blockers().len(), 2);

        a.keys = Some(("k".into(), "s".into()));
        assert!(!a.fully_armed());
        assert_eq!(a.blockers().len(), 1);
        assert!(a.blockers()[0].contains("IVARB_LIVE_CONFIRM"));

        a.confirmed = true;
        assert!(a.fully_armed());
        assert!(a.blockers().is_empty());
        assert!(matches!(a.mode(), ExecMode::Live));
    }

    #[test]
    fn tick_rounding() {
        // Buy rounds up through the touch, sell rounds down through it.
        assert!(ceil_tick(0.0123, PX_TICK_BTC) >= 0.0123);
        assert_eq!(ceil_tick(0.0123, PX_TICK_BTC), 0.0123);
        assert_eq!(ceil_tick(0.0123001, PX_TICK_BTC), 0.0124);
        assert_eq!(floor_tick(0.0123, PX_TICK_BTC), 0.0123);
        assert_eq!(floor_tick(0.0123999, PX_TICK_BTC), 0.0123);
        // Amounts snap to the 0.1 contract step in place_ioc.
        assert_eq!(snap_amount(0.15), 0.2);
        assert_eq!(snap_amount(0.14), 0.1);
        assert_eq!(snap_amount(0.1), 0.1);
    }
}
