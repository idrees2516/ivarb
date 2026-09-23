//! Derive connector (experimental, best-effort RPC-style REST).
//!
//! The public RPC surface has shifted over time; this connector tries the
//! documented instrument/ticker endpoints with defensive parsing and disables
//! itself with an explanatory note when the payload shape is not recognized.

use std::collections::HashMap;
use std::time::Duration;

use crate::jsonutil::{jf, js, norm_iv_vp};
use crate::key::{cap_expiries, parse_option_symbol, InstKey};
use crate::msg::{now_ms, InstrTick, VenueMsg};
use crate::venues::{http_post_json, status, VenueConnector, VenueCtx};
use ivarb_protocol::ConnState;

pub struct Derive;

impl Derive {
    pub fn new() -> Self {
        Derive
    }
}

async fn try_instruments(ctx: &VenueCtx) -> anyhow::Result<Vec<serde_json::Value>> {
    let base = &ctx.spec.rest_base;
    let und = ctx.cfg.underlyings.first().cloned().unwrap_or("BTC".into());
    let attempts: Vec<serde_json::Value> = vec![
        serde_json::json!({ "expired": false, "instrument_type": "option", "currency": und }),
        serde_json::json!({ "expired": false, "instrument_type": "option" }),
        serde_json::json!({ "expired": false, "instrument_type": "erc20" }),
        serde_json::json!({ "expired": false, "currency": und, "instrument_type": "option", "count": 500 }),
    ];
    let mut last_err = String::new();
    for body in &attempts {
        match http_post_json(&ctx.http, &format!("{base}/public/get_instruments"), body).await {
            Ok(v) => {
                if let Some(err) = v.get("error").filter(|e| !e.is_null()) {
                    last_err = format!("rpc error: {err}");
                    continue;
                }
                if let Some(arr) = v
                    .get("result")
                    .and_then(|r| r.as_array())
                    .filter(|a| !a.is_empty())
                {
                    return Ok(arr.clone());
                }
                if let Some(arr) = v.as_array().filter(|a| !a.is_empty()) {
                    return Ok(arr.clone());
                }
                last_err = "empty result".into();
            }
            Err(e) => last_err = e.to_string(),
        }
    }
    Err(anyhow::anyhow!(last_err))
}

#[async_trait::async_trait]
impl VenueConnector for Derive {
    fn id(&self) -> &str {
        "derive"
    }

    async fn run(&mut self, mut ctx: VenueCtx) -> anyhow::Result<()> {
        status(&ctx.tx, "derive", ConnState::Connecting, None).await;

        let rows = match try_instruments(&ctx).await {
            Ok(rows) => rows,
            Err(e) => {
                return Err(anyhow::Error::new(super::Fatal(format!(
                    "instrument endpoint not recognized ({e})"
                ))))
            }
        };

        // Collect option instruments for the configured underlyings.
        let unds: Vec<String> = ctx.cfg.underlyings.clone();
        let now = now_ms();
        let mut dates: Vec<u32> = rows
            .iter()
            .filter_map(|r| js(r, &["instrument_name", "isin"]))
            .filter_map(|s| parse_option_symbol(&s))
            .filter(|(u, _, _, _)| unds.contains(u))
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
            let Some(name) = js(r, &["instrument_name", "isin"]) else { continue };
            let Some((u, d, k, right)) = parse_option_symbol(&name) else { continue };
            if !unds.contains(&u) || !dates.contains(&d) {
                continue;
            }
            let und_id = ctx.intern(&u);
            index.insert(
                name,
                InstKey {
                    und: und_id,
                    date: d,
                    strike_ticks: (k * 10_000.0).round() as u64,
                    right,
                },
            );
        }
        if index.is_empty() {
            return Err(anyhow::Error::new(super::Fatal(
                "instruments recognized by neither name nor isin".into(),
            )));
        }
        // Cap to a sane polling set (nearest-ATM unknown pre-spot; keep all).
        let _ = ctx
            .tx
            .send(VenueMsg::Tracked {
                venue: "derive".into(),
                instruments: index.len() as u32,
            })
            .await;

        let size_hint = ctx.spec.contract_size;
        let poll = Duration::from_millis(ctx.spec.poll_ms.max(10_000));
        let mut ticker = tokio::time::interval(poll);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut empty_rounds = 0;

        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    if ctx.shutdown() { return Ok(()); }
                    let mut ticks: Vec<InstrTick> = Vec::new();
                    let mut spot: Option<f64> = None;
                    let mut spot_und = String::new();
                    for (name, key) in &index {
                        let body = serde_json::json!({ "isin": name });
                        let Ok(v) = http_post_json(
                            &ctx.http,
                            &format!("{}/public/get_ticker", ctx.spec.rest_base),
                            &body,
                        ).await else { continue };
                        let r = v.get("result").unwrap_or(&v);
                        let ivm = jf(r, &["iv", "mark_iv", "markIv"]).and_then(norm_iv_vp);
                        let mark = jf(r, &["price", "mark_price", "markPrice"]);
                        let bid = jf(r, &["bid", "best_bid"]);
                        let ask = jf(r, &["ask", "best_ask"]);
                        if ivm.is_none() && mark.is_none() && bid.is_none() && ask.is_none() {
                            continue;
                        }
                        if spot.is_none() {
                            if let Some(s) = jf(r, &["index", "index_price", "spot", "underlying_price"]) {
                                spot = Some(s);
                                spot_und = ctx.interner.lock().ok()
                                    .map(|i| i.name(key.und).to_string())
                                    .unwrap_or_default();
                            }
                        }
                        let pxu = |p: Option<f64>| p.map(|x| x / size_hint).filter(|x| x.is_finite() && *x > 0.0);
                        ticks.push(InstrTick {
                            key: *key,
                            iv_mark_vp: ivm,
                            iv_bid_vp: jf(r, &["bid_iv", "bidIv"]).and_then(norm_iv_vp),
                            iv_ask_vp: jf(r, &["ask_iv", "askIv"]).and_then(norm_iv_vp),
                            px_mark_unit: pxu(mark),
                            px_bid_unit: pxu(bid),
                            px_ask_unit: pxu(ask),
                            size_bid: jf(r, &["bid_size"]),
                            size_ask: jf(r, &["ask_size"]),
                            spot,
                            contract_size: Some(size_hint),
                            venue_ts_ms: jf(r, &["timestamp", "ts"]).map(|t| t as i64),
                        });
                        if ticks.len() >= 400 { break; }
                    }
                    if let (Some(px), und) = (spot, spot_und) {
                        if !und.is_empty() {
                            let _ = ctx.tx.send(VenueMsg::Spot(ivarb_protocol::SpotQuote {
                                underlying: und,
                                venue: "derive".into(),
                                price: px,
                                ts_ms: now_ms(),
                            })).await;
                        }
                    }
                    if !ticks.is_empty() {
                        empty_rounds = 0;
                        let _ = ctx.tx.send(VenueMsg::Ticks {
                            venue: "derive".into(),
                            ticks,
                        }).await;
                    } else {
                        empty_rounds += 1;
                        if empty_rounds >= 3 {
                            return Err(anyhow::Error::new(super::Fatal(
                                "ticker endpoint not recognized".into(),
                            )));
                        }
                    }
                }
                _ = ctx.shutdown.changed() => {
                    if *ctx.shutdown.borrow() { return Ok(()); }
                }
            }
        }
    }
}
