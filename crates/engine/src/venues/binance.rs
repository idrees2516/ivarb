//! Binance options connector (European options, eapi).
//!
//! `GET /eapi/v1/mark` returns every option's mark price, mark IV and
//! bid/ask IVs with the index price in a single lightweight call — a perfect
//! scanner feed. Polled at `poll_ms`; IVs arrive natively in vol points.

use std::collections::HashMap;
use std::time::Duration;

use ivarb_protocol::{ConnState, SpotQuote};

use crate::jsonutil::{jf, js, norm_iv_vp};
use crate::key::{cap_expiries, parse_option_symbol, InstKey};
use crate::msg::{now_ms, InstrTick, VenueMsg};
use crate::venues::{http_get_json, status, VenueConnector, VenueCtx};

pub struct Binance;

impl Binance {
    pub fn new() -> Self {
        Binance
    }
}

#[async_trait::async_trait]
impl VenueConnector for Binance {
    fn id(&self) -> &str {
        "binance"
    }

    async fn run(&mut self, mut ctx: VenueCtx) -> anyhow::Result<()> {
        status(&ctx.tx, "binance", ConnState::Connecting, None).await;

        // Universe from exchangeInfo.
        let url = format!("{}/eapi/v1/exchangeInfo", ctx.spec.rest_base);
        let v = http_get_json(&ctx.http, &url).await?;
        let rows = v
            .get("optionSymbols")
            .and_then(|r| r.as_array())
            .cloned()
            .unwrap_or_default();
        let now = now_ms();
        let unds: Vec<String> = ctx.cfg.underlyings.clone();
        let mut dates: Vec<u32> = rows
            .iter()
            .filter_map(|r| js(r, &["symbol"]))
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
            let Some(sym) = js(r, &["symbol"]) else { continue };
            let Some((u, d, k, right)) = parse_option_symbol(&sym) else { continue };
            if !unds.contains(&u) || !dates.contains(&d) {
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

        let size_hint = ctx.spec.contract_size;
        let poll = Duration::from_millis(ctx.spec.poll_ms.max(2_000));
        let mut ticker = tokio::time::interval(poll);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    if ctx.shutdown() { return Ok(()); }
                    let url = format!("{}/eapi/v1/mark", ctx.spec.rest_base);
                    let v = match http_get_json(&ctx.http, &url).await {
                        Ok(v) => v,
                        Err(e) => {
                            tracing::warn!(venue = "binance", error = %e, "mark poll failed");
                            continue;
                        }
                    };
                    let rows = v.as_array().cloned().unwrap_or_default();
                    let mut ticks: Vec<InstrTick> = Vec::with_capacity(rows.len().min(2048));
                    let mut spot_seen: HashMap<String, (f64, i64)> = HashMap::new();
                    for r in &rows {
                        let Some(sym) = js(r, &["symbol"]) else { continue };
                        let Some(key) = index.get(&sym) else { continue };
                        // indexPrice is optional in /mark — fall back to the
                        // shared consensus spot when absent.
                        let ipx = jf(r, &["indexPrice"]);
                        let und = ctx.interner.lock().ok()
                            .map(|i| i.name(key.und).to_string())
                            .unwrap_or_default();
                        if ipx.is_some() {
                            spot_seen.insert(und, (ipx.unwrap(), now_ms()));
                        }
                        let pxu = |names: &[&str]| -> Option<f64> {
                            jf(r, names).map(|p| p / size_hint).filter(|p| p.is_finite() && *p > 0.0)
                        };
                        ticks.push(InstrTick {
                            key: *key,
                            iv_mark_vp: jf(r, &["markIV", "markIv", "mark_iv"]).and_then(norm_iv_vp),
                            iv_bid_vp: jf(r, &["bidIV", "bidIv", "bid_iv"]).and_then(norm_iv_vp),
                            iv_ask_vp: jf(r, &["askIV", "askIv", "ask_iv"]).and_then(norm_iv_vp),
                            px_mark_unit: pxu(&["markPrice", "mark_price"]),
                            px_bid_unit: None,
                            px_ask_unit: None,
                            size_bid: jf(r, &["bidQty", "bid_qty"]),
                            size_ask: jf(r, &["askQty", "ask_qty"]),
                            spot: ipx,
                            contract_size: Some(size_hint),
                            venue_ts_ms: jf(r, &["time"]).map(|t| t as i64),
                        });
                    }
                    for (und, (px, ts)) in spot_seen {
                        let _ = ctx.tx.send(VenueMsg::Spot(SpotQuote {
                            underlying: und,
                            venue: "binance".into(),
                            price: px,
                            ts_ms: ts,
                        })).await;
                    }
                    if !ticks.is_empty() {
                        let _ = ctx.tx.send(VenueMsg::Ticks {
                            venue: "binance".into(),
                            ticks,
                        }).await;
                    }
                }
                _ = ctx.shutdown.changed() => {
                    if *ctx.shutdown.borrow() { return Ok(()); }
                }
            }
        }
    }
}
