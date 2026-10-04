//! Delta Exchange connector (REST polling).
//!
//! - Universe: `GET /v2/products?contract_types=call_options,put_options&states=live`
//!   (cursor-paginated).
//! - Quotes: `GET /v2/tickers?contract_types=call_options,put_options&states=live`
//!   polled at `poll_ms`; rows carry mark price and best bid/offer quotes.
//!   Where IV fields are absent the engine derives them from premiums.

use std::collections::HashMap;
use std::time::Duration;

use ivarb_protocol::ConnState;

use crate::jsonutil::{jf, js, norm_iv_vp};
use crate::key::{cap_expiries, parse_option_symbol, InstKey};
use crate::msg::{now_ms, InstrTick, VenueMsg};
use crate::venues::{http_get_json, status, VenueConnector, VenueCtx};

pub struct Delta;

impl Delta {
    pub fn new() -> Self {
        Delta
    }
}

struct Meta {
    key: InstKey,
    contract_value: f64,
}

async fn fetch_products(ctx: &VenueCtx) -> anyhow::Result<HashMap<String, Meta>> {
    let mut out: HashMap<String, Meta> = HashMap::new();
    let mut after: Option<String> = None;
    let unds: Vec<String> = ctx.cfg.underlyings.clone();
    for _page in 0..10 {
        let mut url = format!(
            "{}/v2/products?contract_types=call_options,put_options&states=live&page_size=1000",
            ctx.spec.rest_base
        );
        if let Some(a) = &after {
            url.push_str(&format!("&after={a}"));
        }
        let v = http_get_json(&ctx.http, &url).await?;
        let rows = v
            .get("result")
            .and_then(|r| r.as_array())
            .cloned()
            .unwrap_or_default();
        let now = now_ms();
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
        for r in &rows {
            let Some(sym) = js(r, &["symbol"]) else { continue };
            let Some((u, d, k, right)) = parse_option_symbol(&sym) else { continue };
            if !unds.contains(&u) || !dates.contains(&d) {
                continue;
            }
            let cv = jf(r, &["contract_value", "contractValue"])
                .filter(|c| *c > 0.0)
                .unwrap_or(1.0);
            let und_id = ctx.intern(&u);
            out.insert(
                sym,
                Meta {
                    key: InstKey {
                        und: und_id,
                        date: d,
                        strike_ticks: (k * 10_000.0).round() as u64,
                        right,
                    },
                    contract_value: cv,
                },
            );
        }
        after = js(&v, &["after"])
            .or_else(|| {
                v.get("meta").and_then(|m| js(m, &["after"]))
            });
        if after.is_none() {
            break;
        }
    }
    Ok(out)
}

#[async_trait::async_trait]
impl VenueConnector for Delta {
    fn id(&self) -> &str {
        "delta"
    }

    async fn run(&mut self, mut ctx: VenueCtx) -> anyhow::Result<()> {
        status(&ctx.tx, "delta", ConnState::Connecting, None).await;

        let mut products = fetch_products(&ctx).await?;
        if products.is_empty() {
            return Err(anyhow::anyhow!("delta: no option products"));
        }
        let _ = ctx
            .tx
            .send(VenueMsg::Tracked {
                venue: "delta".into(),
                instruments: products.len() as u32,
            })
            .await;

        let poll = Duration::from_millis(ctx.spec.poll_ms.max(3_000));
        let mut ticker = tokio::time::interval(poll);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut refresh = tokio::time::interval(Duration::from_secs(900));

        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    if ctx.shutdown() { return Ok(()); }
                    let url = format!(
                        "{}/v2/tickers?contract_types=call_options,put_options&states=live",
                        ctx.spec.rest_base
                    );
                    let v = match http_get_json(&ctx.http, &url).await {
                        Ok(v) => v,
                        Err(e) => { tracing::warn!(venue = "delta", error = %e, "tickers poll failed"); continue; }
                    };
                    // Response is {"success":true,"result":[...]} (or a bare array).
                    let rows = v
                        .get("result")
                        .and_then(|r| r.as_array())
                        .or_else(|| v.as_array())
                        .cloned()
                        .unwrap_or_default();
                    let mut ticks: Vec<InstrTick> = Vec::with_capacity(rows.len());
                    for r in &rows {
                        let Some(sym) = js(r, &["symbol"]) else { continue };
                        let Some(m) = products.get(&sym) else { continue };
                        let cv = m.contract_value;
                        // Delta tickers quote option premiums in USD **per 1
                        // unit of underlying** (verified live: C-BTC-88000
                        // mark ≈ $853 on $84.8k spot — the correct per-BTC
                        // value; `contract_value` is the lot quantum, NOT a
                        // price divisor). Dividing by it inflated premiums
                        // ×1000 and fabricated phantom routes.
                        let pxu = |names: &[&str]| -> Option<f64> {
                            jf(r, names).filter(|p| p.is_finite() && *p > 0.0)
                        };
                        let quotes = r.get("quotes");
                        let jq = |names: &[&str]| -> Option<f64> {
                            quotes.and_then(|q| jf(q, names))
                        };
                        // Native quote IVs arrive as raw fractions (0.449 = 44.9%);
                        // spot is carried inside `greeks`.
                        let greeks = r.get("greeks");
                        let spot = greeks.and_then(|g| jf(g, &["spot"]));
                        ticks.push(InstrTick {
                            key: m.key,
                            iv_mark_vp: jq(&["mark_iv"]).or_else(|| jf(r, &["implied_volatility", "mark_iv"])).and_then(norm_iv_vp),
                            iv_bid_vp: jq(&["bid_iv"]).and_then(norm_iv_vp),
                            iv_ask_vp: jq(&["ask_iv"]).and_then(norm_iv_vp),
                            px_mark_unit: pxu(&["mark_price", "markPrice"]),
                            px_bid_unit: jq(&["best_bid"]).filter(|p| p.is_finite() && *p > 0.0),
                            px_ask_unit: jq(&["best_offer", "best_ask"]).filter(|p| p.is_finite() && *p > 0.0),
                            size_bid: jq(&["bid_size", "best_bid_size"]),
                            size_ask: jq(&["ask_size", "best_ask_size"]),
                            spot,
                            contract_size: Some(cv),
                            venue_ts_ms: jf(r, &["timestamp", "ts"]).map(|t| if t > 1e15 { (t / 1_000.0) as i64 } else { t as i64 }),
                        });
                    }
                    if !ticks.is_empty() {
                        let _ = ctx.tx.send(VenueMsg::Ticks {
                            venue: "delta".into(),
                            ticks,
                        }).await;
                        // Emit spot from one representative row if carried.
                    } else {
                        // First poll produced nothing usable — check reachability
                        // so the venue disables cleanly instead of idling.
                        return Err(anyhow::anyhow!("delta: tickers response had no parseable option rows"));
                    }
                }
                _ = refresh.tick() => {
                    if let Ok(p) = fetch_products(&ctx).await {
                        if !p.is_empty() {
                            products = p;
                            let _ = ctx.tx.send(VenueMsg::Tracked {
                                venue: "delta".into(),
                                instruments: products.len() as u32,
                            }).await;
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
