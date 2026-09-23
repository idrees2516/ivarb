//! Thalex connector (experimental). The public REST surface is attempted with
//! defensive parsing; connection failures disable the venue with a note.

use std::collections::HashMap;
use std::time::Duration;

use crate::jsonutil::{jf, js, norm_iv_vp};
use crate::key::{cap_expiries, parse_option_symbol, InstKey};
use crate::msg::{now_ms, InstrTick, VenueMsg};
use crate::venues::{http_get_json, status, VenueConnector, VenueCtx};
use ivarb_protocol::ConnState;

pub struct Thalex;

impl Thalex {
    pub fn new() -> Self {
        Thalex
    }
}

fn rows_of(v: &serde_json::Value) -> Vec<serde_json::Value> {
    if let Some(a) = v.as_array() {
        return a.clone();
    }
    for key in ["result", "data", "tickers", "instruments"] {
        if let Some(a) = v.get(key).and_then(|x| x.as_array()) {
            return a.clone();
        }
    }
    Vec::new()
}

#[async_trait::async_trait]
impl VenueConnector for Thalex {
    fn id(&self) -> &str {
        "thalex"
    }

    async fn run(&mut self, mut ctx: VenueCtx) -> anyhow::Result<()> {
        status(&ctx.tx, "thalex", ConnState::Connecting, None).await;

        // Instruments.
        let url = format!("{}/api/v1/instruments", ctx.spec.rest_base);
        let v = match http_get_json(&ctx.http, &url).await {
            Ok(v) => v,
            Err(e) => {
                return Err(anyhow::Error::new(super::Fatal(format!(
                    "instruments endpoint unreachable ({e})"
                ))))
            }
        };
        let rows = rows_of(&v);
        let unds: Vec<String> = ctx.cfg.underlyings.clone();
        let now = now_ms();
        let mut dates: Vec<u32> = rows
            .iter()
            .filter_map(|r| js(r, &["instrument_name", "name", "symbol"]))
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
            let Some(name) = js(r, &["instrument_name", "name", "symbol"]) else { continue };
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
                "instruments recognized but no symbols parsed".into(),
            )));
        }
        let _ = ctx
            .tx
            .send(VenueMsg::Tracked {
                venue: "thalex".into(),
                instruments: index.len() as u32,
            })
            .await;

        let size_hint = ctx.spec.contract_size;
        let poll = Duration::from_millis(ctx.spec.poll_ms.max(4_000));
        let mut ticker = tokio::time::interval(poll);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut empty_rounds = 0;

        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    if ctx.shutdown() { return Ok(()); }
                    let url = format!("{}/api/v1/tickers", ctx.spec.rest_base);
                    let v = match http_get_json(&ctx.http, &url).await {
                        Ok(v) => v,
                        Err(e) => {
                            empty_rounds += 1;
                            if empty_rounds >= 3 {
                                return Err(anyhow::Error::new(super::Fatal(format!(
                                    "tickers endpoint unreachable ({e})"
                                ))));
                            }
                            continue;
                        }
                    };
                    let rows = rows_of(&v);
                    let mut ticks: Vec<InstrTick> = Vec::new();
                    for r in &rows {
                        let Some(name) = js(r, &["instrument_name", "name", "symbol"]) else { continue };
                        let Some(key) = index.get(&name) else { continue };
                        let ivm = jf(r, &["mark_iv", "markIv", "iv", "implied_volatility"]).and_then(norm_iv_vp);
                        let mark = jf(r, &["mark_price", "markPrice", "mark"]);
                        let bid = jf(r, &["bid", "best_bid", "bid_price"]);
                        let ask = jf(r, &["ask", "best_ask", "ask_price"]);
                        if ivm.is_none() && mark.is_none() && bid.is_none() && ask.is_none() {
                            continue;
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
                            spot: jf(r, &["index_price", "underlying_price"]),
                            contract_size: Some(size_hint),
                            venue_ts_ms: jf(r, &["timestamp", "ts"]).map(|t| t as i64),
                        });
                    }
                    if !ticks.is_empty() {
                        empty_rounds = 0;
                        let _ = ctx.tx.send(VenueMsg::Ticks {
                            venue: "thalex".into(),
                            ticks,
                        }).await;
                    } else {
                        empty_rounds += 1;
                        if empty_rounds >= 3 {
                            return Err(anyhow::Error::new(super::Fatal(
                                "tickers payload shape not recognized".into(),
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
