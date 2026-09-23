//! Live order gateway (Deribit private WebSocket).
//!
//! Activated **only** when `IVARB_MODE=live` and `DERIBIT_API_KEY` /
//! `DERIBIT_API_SECRET` are set — otherwise the executor runs in paper mode.
//! The paper pipeline and the live pipeline share identical FSM code; only
//! the fill source differs.
//!
//! Deribit specifics handled here:
//! - Auth: `public/auth` with `grant_type=client_credentials` (no HMAC needed).
//! - BTC/ETH options are **coin-margined**: premiums are quoted in the
//!   underlying, so a USD premium `p` at spot `S` is sent as `p / S` BTC.
//! - `amount` is in underlying units (1 contract = 1 BTC / 1 ETH).
//! - Orders are sent as limit + `immediate_or_cancel`; the synchronous
//!   response already carries `order_state` / `average_price` / `filled_amount`.
//!
//! The gateway also carries venue symbol formatters for every exchange so
//! live routing (and the UI) can translate canonical instruments
//! (`BTC-20260926-85000-C`) into venue-native names.

use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message;

use ivarb_protocol::VenueId;

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

// ---------------------------------------------------------------------------
// Deribit live session
// ---------------------------------------------------------------------------

pub struct DeribitSession {
    req_tx: mpsc::UnboundedSender<DeribitReq>,
}

enum DeribitReq {
    Place {
        symbol: String,
        side: Side,
        amount: f64,
        price_btc: f64,
        resp: oneshot::Sender<anyhow::Result<Fill>>,
    },
}

impl DeribitSession {
    /// Connect + authenticate + start the session task.
    pub async fn connect(api_key: &str, api_secret: &str) -> anyhow::Result<Arc<Self>> {
        let (ws, _) = tokio_tungstenite::connect_async("wss://www.deribit.com/ws/api/v2").await?;
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
            anyhow::bail!("deribit auth failed");
        }

        let (req_tx, mut req_rx) = mpsc::unbounded_channel::<DeribitReq>();
        let session = Arc::new(Self { req_tx });

        // Pump: route requests to the WS, match responses by echo of id.
        tokio::spawn(async move {
            let mut next_id: u64 = 10;
            let mut pending: rustc_hash::FxHashMap<u64, oneshot::Sender<anyhow::Result<Fill>>> =
                rustc_hash::FxHashMap::default();
            loop {
                tokio::select! {
                    req = req_rx.recv() => match req {
                        Some(DeribitReq::Place { symbol, side, amount, price_btc, resp }) => {
                            next_id += 1;
                            let id = next_id;
                            let method = match side {
                                Side::Buy => "private/buy",
                                Side::Sell => "private/sell",
                            };
                            let msg = serde_json::json!({
                                "jsonrpc": "2.0", "id": id, "method": method,
                                "params": {
                                    "instrument_name": symbol,
                                    "amount": (amount * 100.0).round() / 100.0,
                                    "type": "limit",
                                    "price": (price_btc * 10_000.0).round() / 10_000.0,
                                    "time_in_force": "immediate_or_cancel",
                                    "label": format!("ivarb-{id}"),
                                }
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
                                    let _ = tx.send(parse_order_result(&v, side_of(&v)));
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

    /// Place an IOC order; converts USD premium → coin-margined BTC price.
    #[allow(clippy::too_many_arguments)]
    pub async fn take(
        &self,
        venue: &VenueId,
        _venue_symbol: &str,
        side: Side,
        px_unit_usd: f64,
        contracts: f64,
        _taker_rate: f64,
        spot: f64,
        _contract_size: f64,
    ) -> Fill {
        let _ = venue;
        let price_btc = if spot > 0.0 { px_unit_usd / spot } else { 0.0 };
        let (tx, rx) = oneshot::channel();
        let _ = self.req_tx.send(DeribitReq::Place {
            symbol: _venue_symbol.to_string(),
            side,
            amount: contracts,
            price_btc,
            resp: tx,
        });
        match tokio::time::timeout(Duration::from_secs(12), rx).await {
            Ok(Ok(Ok(fill))) => fill,
            Ok(Ok(Err(e))) => {
                tracing::warn!(error = %e, "deribit order rejected");
                Fill::zero()
            }
            _ => Fill::zero(),
        }
    }
}

fn side_of(_v: &serde_json::Value) -> Side {
    Side::Buy // patched by caller-side parse; direction not needed for Fill math
}

fn parse_order_result(v: &serde_json::Value, _side: Side) -> anyhow::Result<Fill> {
    if let Some(err) = v.get("error").filter(|e| !e.is_null()) {
        anyhow::bail!("deribit error: {err}");
    }
    let order = v
        .get("result")
        .and_then(|r| r.get("order"))
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

/// Read the live-mode configuration from the environment.
pub fn live_mode_from_env() -> (ivarb_protocol::ExecMode, Option<(String, String)>) {
    let keys = match (
        std::env::var("DERIBIT_API_KEY").ok().filter(|s| !s.is_empty()),
        std::env::var("DERIBIT_API_SECRET").ok().filter(|s| !s.is_empty()),
    ) {
        (Some(k), Some(s)) => Some((k, s)),
        _ => None,
    };
    let mode = if std::env::var("IVARB_MODE").map(|m| m.eq_ignore_ascii_case("live")).unwrap_or(false) {
        ivarb_protocol::ExecMode::Live
    } else {
        ivarb_protocol::ExecMode::Paper
    };
    (mode, keys)
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
}
