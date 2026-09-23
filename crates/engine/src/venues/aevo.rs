//! Aevo connector (REST polling of the public markets snapshot).
//!
//! `GET /markets` returns every instrument including options. Fields are
//! parsed defensively (mark price / IV / bid / ask / index) — when IV fields
//! are absent the venue still contributes mark prices from which the engine
//! derives IVs.

use std::collections::HashMap;
use std::time::Duration;

use ivarb_protocol::{ConnState, SpotQuote};

use crate::jsonutil::{jf, js, norm_iv_vp};
use crate::key::{cap_expiries, parse_option_symbol, InstKey};
use crate::msg::{now_ms, InstrTick, VenueMsg};
use crate::venues::{status, VenueConnector, VenueCtx};

pub struct Aevo;

impl Aevo {
    pub fn new() -> Self {
        Aevo
    }
}

#[async_trait::async_trait]
impl VenueConnector for Aevo {
    fn id(&self) -> &str {
        "aevo"
    }

    async fn run(&mut self, mut ctx: VenueCtx) -> anyhow::Result<()> {
        status(&ctx.tx, "aevo", ConnState::Connecting, None).await;

        let url = format!("{}/markets", ctx.spec.rest_base);
        // /markets is a ~1.5 MB snapshot that can stream slowly over HTTP/1.1.
        let v = crate::venues::http_get_json_tmo(&ctx.http, &url, std::time::Duration::from_secs(75)).await?;
        let rows = v.as_array().cloned().unwrap_or_default();
        let unds: Vec<String> = ctx.cfg.underlyings.clone();
        let now = now_ms();

        let mut dates: Vec<u32> = rows
            .iter()
            .filter(|r| {
                js(r, &["instrument_type", "instrumentType"]).as_deref() == Some("OPTION")
            })
            .filter_map(|r| js(r, &["instrument_name", "symbol"]))
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
            if js(r, &["instrument_type", "instrumentType"]).as_deref() != Some("OPTION") {
                continue;
            }
            let Some(name) = js(r, &["instrument_name", "symbol"]) else { continue };
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
            return Err(anyhow::anyhow!(
                "aevo: no option instruments recognized in /markets"
            ));
        }
        let _ = ctx
            .tx
            .send(VenueMsg::Tracked {
                venue: "aevo".into(),
                instruments: index.len() as u32,
            })
            .await;

        let size_hint = ctx.spec.contract_size;
        let poll = Duration::from_millis(ctx.spec.poll_ms.max(5_000));
        let mut ticker = tokio::time::interval(poll);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    if ctx.shutdown() { return Ok(()); }
                    let v = match crate::venues::http_get_json_tmo(
                        &ctx.http,
                        &url,
                        std::time::Duration::from_secs(75),
                    ).await {
                        Ok(v) => v,
                        Err(e) => { tracing::warn!(venue = "aevo", error = %e, "markets poll failed"); continue; }
                    };
                    let rows = v.as_array().cloned().unwrap_or_default();
                    let mut ticks: Vec<InstrTick> = Vec::new();
                    let mut spot_seen: HashMap<String, (f64, i64)> = HashMap::new();
                    let mut usable = 0usize;
                    for r in &rows {
                        let Some(name) = js(r, &["instrument_name", "symbol"]) else { continue };
                        let Some(key) = index.get(&name) else { continue };
                        let mark = jf(r, &["mark_price", "markPrice", "marking_price", "mark"]);
                        let bid = jf(r, &["best_bid", "bid", "bid_price"]);
                        let ask = jf(r, &["best_ask", "ask", "ask_price"]);
                        let ivm = jf(r, &["mark_iv", "markIv", "iv", "implied_volatility"])
                            .and_then(norm_iv_vp);
                        let ivb = jf(r, &["bid_iv", "bidIv"]).and_then(norm_iv_vp);
                        let iva = jf(r, &["ask_iv", "askIv"]).and_then(norm_iv_vp);
                        if mark.is_none() && ivm.is_none() && bid.is_none() && ask.is_none() {
                            continue;
                        }
                        usable += 1;
                        let spot = jf(r, &["index_price", "indexPrice", "underlying_price", "spot_price"]);
                        if let Some(s) = spot {
                            let und = ctx.interner.lock().ok()
                                .map(|i| i.name(key.und).to_string())
                                .unwrap_or_default();
                            spot_seen.insert(und, (s, now_ms()));
                        }
                        let pxu = |p: Option<f64>| -> Option<f64> {
                            p.map(|x| x / size_hint).filter(|x| x.is_finite() && *x > 0.0)
                        };
                        ticks.push(InstrTick {
                            key: *key,
                            iv_mark_vp: ivm,
                            iv_bid_vp: ivb,
                            iv_ask_vp: iva,
                            px_mark_unit: pxu(mark),
                            px_bid_unit: pxu(bid),
                            px_ask_unit: pxu(ask),
                            size_bid: jf(r, &["bid_size", "best_bid_size"]),
                            size_ask: jf(r, &["ask_size", "best_ask_size"]),
                            spot,
                            contract_size: Some(size_hint),
                            venue_ts_ms: jf(r, &["timestamp", "ts"]).map(|t| t as i64),
                        });
                    }
                    for (und, (px, ts)) in spot_seen {
                        let _ = ctx.tx.send(VenueMsg::Spot(SpotQuote {
                            underlying: und,
                            venue: "aevo".into(),
                            price: px,
                            ts_ms: ts,
                        })).await;
                    }
                    if !ticks.is_empty() {
                        let _ = ctx.tx.send(VenueMsg::Ticks {
                            venue: "aevo".into(),
                            ticks,
                        }).await;
                    } else if usable == 0 {
                        return Err(anyhow::anyhow!("aevo: /markets rows carry no usable quote fields"));
                    }
                }
                _ = ctx.shutdown.changed() => {
                    if *ctx.shutdown.borrow() { return Ok(()); }
                }
            }
        }
    }
}
