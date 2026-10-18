//! Derive v3 (formerly Lyra) tick feed — `ticker_slim.{instrument}.100`.
//!
//! Transport (probed live 2026-10-17 against the v3 API):
//! - Discovery: `POST https://api.derive.xyz/v3/public/get_all_live_instruments`
//!   → every live instrument name (`BTC-20261019-82000-C`, `ETH-PERP`, …).
//!   3,912 names at probe time; needs a real User-Agent (403 otherwise).
//! - Ticks: `wss://api.derive.xyz/v3/ws` — JSON-RPC 2.0, public (no login for
//!   market data), max 4 concurrent connections per IP.
//!   ```json
//!   {"id":1,"method":"subscribe","params":{"channels":["ticker_slim.BTC-PERP.100"]}}
//!   {"id":2,"method":"public/unsubscribe","params":{"channels":[...]}}
//!   ```
//!   Notifications carry `params.channel` + `params.data.instrument_ticker`
//!   with best bid/ask **and sizes** (`b`/`B`, `a`/`A` — lowercase price,
//!   uppercase amount), mark `M`, index `I`, the venue's own option IV
//!   surface (`option_pricing.i/bi/ai` fractions) and 24h stats
//!   (`stats.v` USD volume, `stats.oi` open interest).
//!
//! The `.100` suffix is the venue's 100 ms publish interval — the fastest
//! documented. The engine's Derive REST connector already feeds the scanner
//! (4 s polls of the whole-expiry `get_tickers`); this WS overlays the
//! research plane's consolidated book with exchange-fresh BBO so cross-venue
//! routing decisions no longer wait for the next REST sweep.

use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::sync::watch;
use tokio_tungstenite::tungstenite::Message;

use ivarb_protocol::Right;

use crate::consolidate::canon;
use crate::ticks::{DesiredMarkets, TickBbo, TickPlane};

const WS_URL: &str = "wss://api.derive.xyz/v3/ws";
const REST_BASE: &str = "https://api.derive.xyz/v3";
const TICKER_INTERVAL_MS: u32 = 100;

/// Fetch every live instrument name (public; UA required).
pub async fn fetch_live_names(http: &reqwest::Client) -> anyhow::Result<Vec<String>> {
    let v: serde_json::Value = http
        .post(format!("{REST_BASE}/public/get_all_live_instruments"))
        .timeout(Duration::from_secs(20))
        .json(&serde_json::json!({}))
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;
    let names = v
        .get("result")
        .and_then(|r| r.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|n| n.as_str().map(|s| s.to_string()))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if names.is_empty() {
        anyhow::bail!("derive v3 live instruments empty");
    }
    Ok(names)
}

/// Parse a Derive instrument name: `BTC-20261019-82000-C` /
/// `ADA-20261030-0_15-P` (underscore = decimal point) / `BTC-PERP`.
pub fn parse_name(name: &str) -> Option<(String, u32, f64, Right)> {
    let parts: Vec<&str> = name.split('-').collect();
    if parts.len() != 4 {
        return None; // perps / spot
    }
    let right = match parts[3] {
        "C" => Right::Call,
        "P" => Right::Put,
        _ => return None,
    };
    let date: u32 = parts[1].parse().ok()?;
    let strike: f64 = parts[2].replace('_', ".").parse().ok()?;
    if !(strike.is_finite() && strike > 0.0) {
        return None;
    }
    let und = parts[0].to_uppercase();
    if und.len() < 2 || und.len() > 6 || !und.chars().all(|c| c.is_ascii_alphabetic()) {
        return None;
    }
    Some((und, date, strike, right))
}

/// Perp instrument name for an underlying (`BTC-PERP`).
pub fn perp_name(und: &str) -> String {
    format!("{und}-PERP")
}

/// Spawn the Derive v3 tick task. `desired` carries raw instrument names;
/// the task reconciles `ticker_slim.{name}.100` subscriptions incrementally.
pub fn spawn_v3(plane: Arc<TickPlane>, mut desired: DesiredMarkets, shutdown: watch::Receiver<bool>) {
    tokio::spawn(async move {
        let mut backoff = Duration::from_secs(1);
        let mut shutdown = shutdown;
        loop {
            if *shutdown.borrow() {
                break;
            }
            match v3_session(&plane, &mut desired, &mut shutdown).await {
                Ok(()) => {
                    tracing::info!("derive v3 ticker session ended cleanly");
                }
                Err(e) => {
                    tracing::warn!(error = %e, "derive v3 ticker session failed");
                }
            }
            plane.feed_state("derive", false, 0, "v3 ws reconnecting");
            let jitter = (rand::random::<u64>() % 500) as u64;
            let wait = backoff + Duration::from_millis(jitter);
            tracing::info!(?wait, "derive v3 ws reconnecting");
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

async fn v3_session(
    plane: &Arc<TickPlane>,
    desired: &mut DesiredMarkets,
    shutdown: &mut watch::Receiver<bool>,
) -> anyhow::Result<()> {
    let (ws, _resp) = tokio_tungstenite::connect_async(WS_URL).await?;
    tracing::info!("derive v3 ws connected (ticker_slim @{}ms, public)", TICKER_INTERVAL_MS);
    let (mut sink, mut stream) = ws.split();
    plane.feed_state("derive", true, 0, "v3 ticker connected");

    let mut req_id = 0u64;
    let mut current: Vec<String> = Vec::new();
    let want = desired.borrow().clone();
    v3_reconcile(&mut sink, &mut req_id, &mut current, &want).await?;
    plane.feed_state("derive", true, current.len() as u32, "");

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
                    v3_reconcile(&mut sink, &mut req_id, &mut current, &want).await?;
                    plane.feed_state("derive", true, current.len() as u32, "");
                }
            }
            msg = stream.next() => {
                match msg {
                    Some(Ok(Message::Text(t))) => {
                        let Ok(v) = serde_json::from_str::<serde_json::Value>(&t) else { continue };
                        let Some(channel) = v.pointer("/params/channel").and_then(|c| c.as_str()) else {
                            // JSON-RPC control plane (acks / errors).
                            if v.get("error").is_some() {
                                tracing::warn!(msg = %t, "derive v3 control error");
                            }
                            continue;
                        };
                        if !channel.starts_with("ticker_slim.") {
                            continue;
                        }
                        plane.note_frame("derive");
                        let Some(it) = v.pointer("/params/data/instrument_ticker") else { continue };
                        let Some(inst) = channel
                            .strip_prefix("ticker_slim.")
                            .and_then(|c| c.strip_suffix(&format!(".{TICKER_INTERVAL_MS}")))
                        else { continue };
                        let Some((und, date, strike, right)) = parse_name(inst) else { continue };
                        let f = |k: &str| it.get(k).and_then(|x| x.as_str()).and_then(|s| s.parse::<f64>().ok()).filter(|x| x.is_finite() && *x > 0.0);
                        let op = it.get("option_pricing").filter(|o| !o.is_null());
                        let iv = |k: &str| {
                            op.and_then(|o| o.get(k))
                                .and_then(|x| x.as_str())
                                .and_then(|s| s.parse::<f64>().ok())
                                .map(|v| v * 100.0)
                                .filter(|v| (5.0..500.0).contains(v))
                        };
                        let stats = it.get("stats");
                        let sf = |k: &str| {
                            stats
                                .and_then(|s| s.get(k))
                                .and_then(|x| x.as_str())
                                .and_then(|s| s.parse::<f64>().ok())
                                .filter(|v| v.is_finite() && *v > 0.0)
                        };
                        let ts = it
                            .get("t")
                            .and_then(|x| x.as_i64())
                            .or_else(|| v.pointer("/params/data/timestamp").and_then(|x| x.as_i64()))
                            .unwrap_or_else(crate::now_ms);
                        plane.apply("derive", &canon(&und, date, strike, right), TickBbo {
                            bid: f("b"),
                            ask: f("a"),
                            bid_size: f("B"),
                            ask_size: f("A"),
                            mark: f("M"),
                            index: f("I"),
                            iv_mark_vp: iv("i"),
                            iv_bid_vp: iv("bi"),
                            iv_ask_vp: iv("ai"),
                            oi: sf("oi"),
                            vol24_usd: sf("v"),
                            seq: 0,
                            feed_ts_ms: ts,
                            recv_ms: crate::now_ms(),
                        });
                    }
                    Some(Ok(Message::Ping(p))) => {
                        // Server pings ~every 180 s; a pong (or any text)
                        // keeps the session alive.
                        sink.send(Message::Pong(p)).await?;
                    }
                    Some(Ok(Message::Close(c))) => {
                        anyhow::bail!("derive v3 ws closed by server: {c:?}");
                    }
                    Some(Ok(_)) => {}
                    Some(Err(e)) => anyhow::bail!("derive v3 ws read error: {e}"),
                    None => anyhow::bail!("derive v3 ws stream ended"),
                }
            }
        }
    }
}

async fn v3_reconcile<S, E>(
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
    if !add.is_empty() {
        *req_id += 1;
        let chans: Vec<String> = add
            .iter()
            .map(|n| format!("ticker_slim.{n}.{TICKER_INTERVAL_MS}"))
            .collect();
        let msg = serde_json::json!({"id": *req_id, "method": "subscribe", "params": {"channels": chans}});
        sink.send(Message::Text(msg.to_string())).await?;
        current.extend(add.iter().cloned());
    }
    if !rm.is_empty() {
        *req_id += 1;
        let chans: Vec<String> = rm
            .iter()
            .map(|n| format!("ticker_slim.{n}.{TICKER_INTERVAL_MS}"))
            .collect();
        let msg = serde_json::json!({"id": *req_id, "method": "public/unsubscribe", "params": {"channels": chans}});
        sink.send(Message::Text(msg.to_string())).await?;
        current.retain(|s| !rm.contains(s));
    }
    if !add.is_empty() || !rm.is_empty() {
        tracing::info!(added = add.len(), removed = rm.len(), total = current.len(), "derive v3 ticker set reconciled");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_option_names() {
        assert_eq!(
            parse_name("BTC-20261019-82000-C").unwrap(),
            ("BTC".into(), 20261019, 82000.0, Right::Call)
        );
        assert_eq!(
            parse_name("ADA-20261030-0_15-P").unwrap(),
            ("ADA".into(), 20261030, 0.15, Right::Put)
        );
        assert!(parse_name("BTC-PERP").is_none());
        assert!(parse_name("ETH-USDC").is_none());
        assert!(parse_name("BTC-20261O19-82000-C").is_none()); // letter O, not digit
    }

    #[test]
    fn perp_names() {
        assert_eq!(perp_name("BTC"), "BTC-PERP");
    }
}
