//! Bybit connector.
//!
//! - WS `wss://stream.bybit.com/v5/public/option`, `tickers.{symbol}`
//!   snapshot channel (native markIv/bidIv/askIv in vol points).
//! - Universe from `GET /v5/market/instruments-info?category=option`
//!   (paginated by base coin), filtered to the DTE window and an ATM strike
//!   band; subscriptions are batched 10 symbols per frame.
//! - Spot from the linear/spot ticker REST endpoint, refreshed every 30s.

use std::collections::HashMap;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message;

use ivarb_protocol::{ConnState, SpotQuote};

use crate::jsonutil::{jf, js, norm_iv_vp};
use crate::key::{cap_expiries, keep_near_atm, parse_option_symbol, InstKey};
use crate::msg::{now_ms, InstrTick, VenueMsg};
use crate::venues::{http_get_json, status, ws_connect, VenueConnector, VenueCtx};

pub struct Bybit;

impl Bybit {
    pub fn new() -> Self {
        Bybit
    }
}

async fn fetch_spot(ctx: &VenueCtx) -> HashMap<String, f64> {
    let mut out = HashMap::new();
    for und in &ctx.cfg.underlyings {
        let url = format!(
            "{}/v5/market/tickers?category=spot&symbol={und}USDT",
            ctx.spec.rest_base
        );
        if let Ok(v) = http_get_json(&ctx.http, &url).await {
            let px = v
                .pointer("/result/list/0")
                .and_then(|r| jf(r, &["lastPrice", "last_price", "markPrice"]));
            if let Some(px) = px {
                out.insert(und.clone(), px);
                let _ = ctx
                    .tx
                    .send(VenueMsg::Spot(SpotQuote {
                        underlying: und.clone(),
                        venue: "bybit".into(),
                        price: px,
                        ts_ms: now_ms(),
                    }))
                    .await;
            }
        }
    }
    out
}

#[async_trait::async_trait]
impl VenueConnector for Bybit {
    fn id(&self) -> &str {
        "bybit"
    }

    async fn run(&mut self, mut ctx: VenueCtx) -> anyhow::Result<()> {
        status(&ctx.tx, "bybit", ConnState::Connecting, None).await;

        let spots = fetch_spot(&ctx).await;
        let now = now_ms();

        // ---- instruments (paginated) ------------------------------------------
        let mut symbols: Vec<(String, InstKey)> = Vec::new();
        for und in &ctx.cfg.underlyings {
            let mut cursor: Option<String> = None;
            for _page in 0..6 {
                let mut url = format!(
                    "{}/v5/market/instruments-info?category=option&baseCoin={und}&limit=1000",
                    ctx.spec.rest_base
                );
                if let Some(c) = &cursor {
                    url.push_str(&format!("&cursor={}", urlencode(c)));
                }
                let v = http_get_json(&ctx.http, &url).await?;
                let list = v
                    .pointer("/result/list")
                    .and_then(|l| l.as_array())
                    .cloned()
                    .unwrap_or_default();
                for r in &list {
                    let Some(sym) = js(r, &["symbol"]) else { continue };
                    let Some((u, d, k, right)) = parse_option_symbol(&sym) else {
                        continue;
                    };
                    if u != *und {
                        continue;
                    }
                    let und_id = ctx.intern(&u);
                    symbols.push((
                        sym,
                        InstKey {
                            und: und_id,
                            date: d,
                            strike_ticks: (k * 10_000.0).round() as u64,
                            right,
                        },
                    ));
                }
                cursor = v
                    .pointer("/result/nextPageCursor")
                    .and_then(|c| c.as_str())
                    .map(|s| s.to_string())
                    .filter(|s| !s.is_empty());
                if cursor.is_none() {
                    break;
                }
            }
        }

        // Filter: expiry window + cap, then ATM band, then hard cap.
        let mut dates: Vec<u32> = symbols.iter().map(|(_, k)| k.date).collect();
        cap_expiries(
            &mut dates,
            now,
            ctx.cfg.min_dte,
            ctx.cfg.max_dte,
            ctx.cfg.max_expiries,
        );
        symbols.retain(|(_, k)| dates.contains(&k.date));
        {
            let mut filtered: Vec<(String, InstKey)> = Vec::new();
            for und in &ctx.cfg.underlyings {
                let und_id = ctx.intern(und);
                let spot = spots.get(und).copied().unwrap_or(0.0);
                let mut band: Vec<(f64, u32, u8)> = symbols
                    .iter()
                    .filter(|(_, k)| k.und == und_id)
                    .map(|(_, k)| (k.strike(), k.date, k.right))
                    .collect();
                if spot > 0.0 {
                    keep_near_atm(&mut band, spot, ctx.cfg.atm_pct, usize::MAX);
                }
                let keep: std::collections::HashSet<(u64, u32, u8)> = band
                    .into_iter()
                    .map(|(s, d, r)| ((s * 10_000.0).round() as u64, d, r))
                    .collect();
                filtered.extend(
                    symbols
                        .iter()
                        .filter(|(_, k)| {
                            k.und == und_id
                                && (spot <= 0.0
                                    || keep.contains(&(k.strike_ticks, k.date, k.right)))
                        })
                        .cloned(),
                );
            }
            symbols = filtered;
        }
        symbols.truncate(ctx.cfg.max_instruments_per_venue);
        if symbols.is_empty() {
            return Err(anyhow::anyhow!("bybit: no instruments after filtering"));
        }
        let symbol_index: std::collections::HashMap<String, InstKey> =
            symbols.iter().cloned().collect();
        let _ = ctx
            .tx
            .send(VenueMsg::Tracked {
                venue: "bybit".into(),
                instruments: symbol_index.len() as u32,
            })
            .await;

        // ---- ws -----------------------------------------------------------------
        let ws = ws_connect(&ctx.spec.ws_url).await?;
        let (mut sink, mut stream) = ws.split();

        // Batched subscriptions: 10 topics per frame.
        let names: Vec<String> = symbols.iter().map(|(s, _)| s.clone()).collect();
        for chunk in names.chunks(10) {
            let args: Vec<String> = chunk.iter().map(|s| format!("tickers.{s}")).collect();
            let req = serde_json::json!({ "op": "subscribe", "args": args });
            sink.send(Message::Text(req.to_string())).await?;
            tokio::time::sleep(Duration::from_millis(80)).await;
        }

        let mut keepalive = tokio::time::interval(Duration::from_secs(20));
        let mut spot_refresh = tokio::time::interval(Duration::from_secs(30));
        let size_hint = ctx.spec.contract_size;

        loop {
            tokio::select! {
                msg = stream.next() => {
                    match msg {
                        Some(Ok(Message::Text(txt))) => {
                            let v: serde_json::Value = match serde_json::from_str(&txt) {
                                Ok(v) => v,
                                Err(_) => continue,
                            };
                            let topic = v.get("topic").and_then(|t| t.as_str()).unwrap_or_default();
                            if !topic.starts_with("tickers.") {
                                // op/subscribe confirmations and errors
                                if v.get("retCode").and_then(|c| c.as_i64()) == Some(0) { continue; }
                                if let Some(ret_msg) = v.get("retMsg").and_then(|m| m.as_str()) {
                                    if !ret_msg.is_empty() && ret_msg != "OK" {
                                        tracing::warn!(venue = "bybit", msg = %ret_msg, "ws response");
                                    }
                                }
                                continue;
                            }
                            let Some(data) = v.get("data") else { continue };
                            let Some(sym) = js(data, &["symbol"]) else { continue };
                            let Some(key) = symbol_index.get(&sym) else { continue };
                            let pxu = |names: &[&str]| -> Option<f64> {
                                jf(data, names).map(|p| p / size_hint).filter(|p| p.is_finite() && *p > 0.0)
                            };
                            let tick = InstrTick {
                                key: *key,
                                iv_mark_vp: jf(data, &["markIv", "mark_iv", "markIV"]).and_then(norm_iv_vp),
                                iv_bid_vp: jf(data, &["bidIv", "bid_iv", "bidIV", "bid1Iv"]).and_then(norm_iv_vp),
                                iv_ask_vp: jf(data, &["askIv", "ask_iv", "askIV", "ask1Iv"]).and_then(norm_iv_vp),
                                px_mark_unit: pxu(&["markPrice", "mark_price"]),
                                px_bid_unit: pxu(&["bidPrice", "bid_price", "bid1Price"]),
                                px_ask_unit: pxu(&["askPrice", "ask_price", "ask1Price"]),
                                size_bid: jf(data, &["bidSize", "bid_size", "bid1Size"]),
                                size_ask: jf(data, &["askSize", "ask_size", "ask1Size"]),
                                spot: jf(data, &["underlyingPrice", "underlying_price"]),
                                contract_size: Some(size_hint),
                                venue_ts_ms: jf(&v, &["ts"]).map(|t| t as i64),
                            };
                            let _ = ctx.tx.send(VenueMsg::Ticks {
                                venue: "bybit".into(),
                                ticks: vec![tick],
                            }).await;
                        }
                        Some(Ok(Message::Ping(p))) => { sink.send(Message::Pong(p)).await?; }
                        Some(Ok(Message::Close(c))) => return Err(anyhow::anyhow!("bybit ws closed: {c:?}")),
                        Some(Ok(_)) => {}
                        Some(Err(e)) => return Err(anyhow::anyhow!("bybit ws error: {e}")),
                        None => return Err(anyhow::anyhow!("bybit ws stream ended")),
                    }
                }
                _ = keepalive.tick() => {
                    let req = serde_json::json!({ "op": "ping" });
                    sink.send(Message::Text(req.to_string())).await?;
                }
                _ = spot_refresh.tick() => { fetch_spot(&ctx).await; }
                _ = ctx.shutdown.changed() => {
                    if *ctx.shutdown.borrow() { return Ok(()); }
                }
            }
        }
    }
}

fn urlencode(s: &str) -> String {
    s.replace('%', "%25")
        .replace('+', "%2B")
        .replace(' ', "%20")
        .replace('&', "%26")
        .replace('=', "%3D")
        .replace('?', "%3F")
}
