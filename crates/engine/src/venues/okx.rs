//! OKX connector.
//!
//! - WS `wss://ws.okx.com:8443/ws/v5/public`.
//! - Universe per `uly` (`GET /api/v5/public/instruments?instType=OPTION&uly=BTC-USD`).
//!   BTC-USD options are inverse (coin-margined): premiums are in BTC, so the
//!   connector converts them to USD per 1 underlying unit via the index price.
//! - Subscribes `opt-tickers` per instFamily plus `index-tickers` for spot and
//!   `mark-price` for marks. If `opt-tickers` is rejected, falls back to
//!   `tickers`.
//! - Keepalive is the raw text frame `ping` (OKX requirement).
//! - IV fields, when present, may be fractions or vol points — normalized.

use std::collections::HashMap;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message;

use ivarb_protocol::{ConnState, SpotQuote};

use crate::jsonutil::{jf, js, norm_iv_vp};
use crate::key::{cap_expiries, parse_option_symbol, InstKey};
use crate::msg::{now_ms, InstrTick, VenueMsg};
use crate::venues::{http_get_json, status, ws_connect, VenueConnector, VenueCtx};

pub struct Okx;

impl Okx {
    pub fn new() -> Self {
        Okx
    }
}

struct InstMeta {
    key: InstKey,
    ct_val: f64,
    /// Coin-margined family (premiums in the underlying coin).
    inverse: bool,
    und: String,
}

#[async_trait::async_trait]
impl VenueConnector for Okx {
    fn id(&self) -> &str {
        "okx"
    }

    async fn run(&mut self, mut ctx: VenueCtx) -> anyhow::Result<()> {
        status(&ctx.tx, "okx", ConnState::Connecting, None).await;

        // ---- universe bootstrap (per uly — OKX requires it for OPTION) ------
        let unds: Vec<String> = ctx.cfg.underlyings.clone();
        let now = now_ms();
        let mut meta: HashMap<String, InstMeta> = HashMap::new();
        let mut families: Vec<String> = Vec::new();
        for und in &unds {
            let uly = format!("{und}-USD");
            let url = format!(
                "{}/api/v5/public/instruments?instType=OPTION&uly={uly}",
                ctx.spec.rest_base
            );
            let v = http_get_json(&ctx.http, &url).await?;
            let rows = v
                .get("data")
                .and_then(|d| d.as_array())
                .cloned()
                .unwrap_or_default();
            for r in &rows {
                let Some(inst_id) = js(r, &["instId"]) else { continue };
                let Some((u, d, k, right)) = parse_option_symbol(&inst_id) else {
                    continue;
                };
                if u != *und {
                    continue;
                }
                let ct_val = jf(r, &["ctVal"]).filter(|c| *c > 0.0).unwrap_or(1.0);
                let inverse = js(r, &["ctType"]).as_deref() == Some("inverse")
                    || js(r, &["settleCcy"]).as_deref() == Some(&u);
                let und_id = ctx.intern(&u);
                if let Some(f) = js(r, &["instFamily"]) {
                    if !families.contains(&f) {
                        families.push(f);
                    }
                }
                meta.insert(
                    inst_id,
                    InstMeta {
                        key: InstKey {
                            und: und_id,
                            date: d,
                            strike_ticks: (k * 10_000.0).round() as u64,
                            right,
                        },
                        ct_val,
                        inverse,
                        und: u,
                    },
                );
            }
        }
        // Expiry window filter.
        let mut dates: Vec<u32> = meta.values().map(|m| m.key.date).collect();
        cap_expiries(
            &mut dates,
            now,
            ctx.cfg.min_dte,
            ctx.cfg.max_dte,
            ctx.cfg.max_expiries,
        );
        meta.retain(|_, m| dates.contains(&m.key.date));
        if meta.is_empty() {
            return Err(anyhow::anyhow!("okx: no USD-margined option instruments"));
        }
        families.truncate(24);
        let _ = ctx
            .tx
            .send(VenueMsg::Tracked {
                venue: "okx".into(),
                instruments: meta.len() as u32,
            })
            .await;

        // ---- ws -----------------------------------------------------------------
        let ws = tokio::time::timeout(
            Duration::from_secs(15),
            ws_connect(&ctx.spec.ws_url),
        )
        .await
        .map_err(|_| anyhow::anyhow!("okx ws connect timeout"))??;
        let (mut sink, mut stream) = ws.split();

        // 1) index tickers for spot.
        let idx_args: Vec<serde_json::Value> = unds
            .iter()
            .map(|u| {
                serde_json::json!({
                    "channel": "index-tickers",
                    "instId": format!("{u}-USD")
                })
            })
            .collect();
        let req = serde_json::json!({ "op": "subscribe", "args": idx_args });
        sink.send(Message::Text(req.to_string())).await?;

        // 2) wait briefly for the first index prints to filter strikes around ATM.
        let mut spot_by_und: HashMap<String, f64> = HashMap::new();
        let wait_until = std::time::Instant::now() + Duration::from_secs(4);
        while spot_by_und.len() < unds.len() && std::time::Instant::now() < wait_until {
            match tokio::time::timeout(Duration::from_millis(500), stream.next()).await {
                Ok(Some(Ok(Message::Text(txt)))) => {
                    let Ok(v) = serde_json::from_str::<serde_json::Value>(&txt) else { continue };
                    if let Some(rows) = v.get("data").and_then(|d| d.as_array()) {
                        if let Some(inst) = v
                            .pointer("/arg/instId")
                            .and_then(|i| i.as_str())
                            .map(|s| s.to_string())
                        {
                            let und = inst.split('-').next().unwrap_or(&inst).to_string();
                            for row in rows {
                                if let Some(px) = jf(row, &["idxPx", "index", "last"]) {
                                    spot_by_und.insert(und.clone(), px);
                                    let _ = ctx.tx.send(VenueMsg::Spot(SpotQuote {
                                        underlying: und.clone(),
                                        venue: "okx".into(),
                                        price: px,
                                        ts_ms: now_ms(),
                                    })).await;
                                }
                            }
                        }
                    }
                }
                Ok(Some(Ok(Message::Ping(p)))) => { sink.send(Message::Pong(p)).await?; }
                _ => continue,
            }
        }

        // 3) filter to the ATM band and subscribe `tickers` per instrument
        //    (family subscriptions are no longer accepted for options).
        if !spot_by_und.is_empty() {
            meta.retain(|_, m| {
                match spot_by_und.get(&m.und) {
                    Some(&s) if s > 0.0 => {
                        (m.key.strike() / s).ln().abs() <= ctx.cfg.atm_pct
                    }
                    _ => true,
                }
            });
        }
        let inst_ids: Vec<String> = {
            let mut ids: Vec<String> = meta.keys().cloned().collect();
            ids.sort();
            ids.truncate(ctx.cfg.max_instruments_per_venue);
            ids
        };
        let _ = ctx
            .tx
            .send(VenueMsg::Tracked {
                venue: "okx".into(),
                instruments: inst_ids.len() as u32,
            })
            .await;
        tracing::info!(venue = "okx", count = inst_ids.len(), "subscribing tickers");
        for chunk in inst_ids.chunks(50) {
            let args: Vec<serde_json::Value> = chunk
                .iter()
                .map(|id| serde_json::json!({ "channel": "tickers", "instId": id }))
                .collect();
            let req = serde_json::json!({ "op": "subscribe", "args": args });
            sink.send(Message::Text(req.to_string())).await?;
            tokio::time::sleep(Duration::from_millis(150)).await;
        }

        let mut keepalive = tokio::time::interval(Duration::from_secs(18));
        keepalive.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                msg = stream.next() => {
                    match msg {
                        Some(Ok(Message::Text(txt))) => {
                            if txt == "pong" { continue; }
                            let v: serde_json::Value = match serde_json::from_str(&txt) {
                                Ok(v) => v,
                                Err(_) => continue,
                            };
                            if v.get("event").and_then(|e| e.as_str()) == Some("error") {
                                let m = v.get("msg").and_then(|m| m.as_str()).unwrap_or_default();
                                tracing::warn!(venue = "okx", msg = %m, "subscribe error");
                                continue;
                            }
                            let Some(arg) = v.get("arg") else { continue };
                            let ch = arg.get("channel").and_then(|c| c.as_str()).unwrap_or_default();
                            let Some(data) = v.get("data").and_then(|d| d.as_array()) else { continue };
                            if ch.starts_with("index") {
                                if let Some(inst) = arg.get("instId").and_then(|i| i.as_str()) {
                                    let und = inst.split('-').next().unwrap_or(inst).to_string();
                                    for row in data {
                                        if let Some(px) = jf(row, &["idxPx", "index", "last"]) {
                                            let _ = ctx.tx.send(VenueMsg::Spot(SpotQuote {
                                                underlying: und.clone(),
                                                venue: "okx".into(),
                                                price: px,
                                                ts_ms: now_ms(),
                                            })).await;
                                        }
                                    }
                                }
                                continue;
                            }
                            let mut ticks = Vec::with_capacity(data.len());
                            for row in data {
                                let Some(inst_id) = js(row, &["instId"]) else { continue };
                                let Some(m) = meta.get(&inst_id) else { continue };
                                let spot = ctx.consensus_spot(&m.und)
                                    .or_else(|| jf(row, &["idxPx"]));
                                let pxu = |names: &[&str]| -> Option<f64> {
                                    let p = jf(row, names)?;
                                    let p = if m.inverse {
                                        p * spot? / m.ct_val
                                    } else {
                                        p / m.ct_val
                                    };
                                    p.is_finite().then_some(p).filter(|p| *p > 0.0)
                                };
                                ticks.push(InstrTick {
                                    key: m.key,
                                    iv_mark_vp: jf(row, &["markVol", "markIv", "mark_iv", "iv"]).and_then(norm_iv_vp),
                                    iv_bid_vp: jf(row, &["bidVol", "bidIv", "bid_iv"]).and_then(norm_iv_vp),
                                    iv_ask_vp: jf(row, &["askVol", "askIv", "ask_iv"]).and_then(norm_iv_vp),
                                    px_mark_unit: pxu(&["markPx", "mark_px"]),
                                    px_bid_unit: pxu(&["bidPx", "bid_px"]),
                                    px_ask_unit: pxu(&["askPx", "ask_px"]),
                                    size_bid: jf(row, &["bidSz", "bidVol", "bidSize"]),
                                    size_ask: jf(row, &["askSz", "askVol", "askSize"]),
                                    spot,
                                    contract_size: Some(m.ct_val),
                                    venue_ts_ms: jf(row, &["ts"]).map(|t| t as i64),
                                });
                            }
                            if !ticks.is_empty() {
                                let _ = ctx.tx.send(VenueMsg::Ticks {
                                    venue: "okx".into(),
                                    ticks,
                                }).await;
                            }
                        }
                        Some(Ok(Message::Ping(p))) => { sink.send(Message::Pong(p)).await?; }
                        Some(Ok(Message::Close(c))) => return Err(anyhow::anyhow!("okx ws closed: {c:?}")),
                        Some(Ok(_)) => {}
                        Some(Err(e)) => return Err(anyhow::anyhow!("okx ws error: {e}")),
                        None => return Err(anyhow::anyhow!("okx ws stream ended")),
                    }
                }
                _ = keepalive.tick() => {
                    sink.send(Message::Text("ping".into())).await?;
                }
                _ = ctx.shutdown.changed() => {
                    if *ctx.shutdown.borrow() { return Ok(()); }
                }
            }
        }
    }
}
