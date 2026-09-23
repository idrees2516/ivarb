//! Deribit connector.
//!
//! Deribit's public WS ticker channels require authentication in 2026
//! (`raw` only, error 13778 for anonymous sessions), so this connector polls
//! `GET /api/v2/public/get_book_summary_by_currency` — a single call per
//! currency that returns every option's mark IV (vol points), mark/bid/ask
//! premiums (BTC) and the underlying price. Bid/ask IVs are derived by the
//! engine's solver from the quoted prices.
//!
//! Contract is coin-margined, 1 BTC per contract.

use std::time::Duration;

use ivarb_protocol::{ConnState, SpotQuote};

use crate::jsonutil::{jf, js, norm_iv_vp};
use crate::key::{cap_expiries, dte_days, parse_option_symbol};
use crate::msg::{now_ms, InstrTick, VenueMsg};
use crate::venues::{http_get_json, status, VenueConnector, VenueCtx};

pub struct Deribit;

impl Deribit {
    pub fn new() -> Self {
        Deribit
    }
}

#[async_trait::async_trait]
impl VenueConnector for Deribit {
    fn id(&self) -> &str {
        "deribit"
    }

    async fn run(&mut self, mut ctx: VenueCtx) -> anyhow::Result<()> {
        status(&ctx.tx, "deribit", ConnState::Connecting, None).await;

        let poll = Duration::from_millis(3_000);
        let mut ticker = tokio::time::interval(poll);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut failures = 0u32;

        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    if ctx.shutdown() { return Ok(()); }
                    let mut all_ticks: Vec<InstrTick> = Vec::new();
                    let mut total = 0u32;
                    for und in &ctx.cfg.underlyings {
                        let url = format!(
                            "{}/api/v2/public/get_book_summary_by_currency?currency={und}&kind=option",
                            ctx.spec.rest_base
                        );
                        let v = match http_get_json(&ctx.http, &url).await {
                            Ok(v) => v,
                            Err(e) => {
                                tracing::warn!(venue = "deribit", error = %e, "book summary poll failed");
                                failures += 1;
                                if failures >= 5 { return Err(anyhow::anyhow!("deribit: polls keep failing")); }
                                continue;
                            }
                        };
                        failures = 0;
                        let rows = v.get("result").and_then(|r| r.as_array()).cloned().unwrap_or_default();
                        let now = now_ms();
                        let mut dates: Vec<u32> = rows.iter()
                            .filter_map(|r| js(r, &["instrument_name"]))
                            .filter_map(|n| parse_option_symbol(&n))
                            .map(|(_, d, _, _)| d)
                            .collect();
                        cap_expiries(&mut dates, now, ctx.cfg.min_dte, ctx.cfg.max_dte, ctx.cfg.max_expiries);
                        let mut und_spot: Option<f64> = None;
                        let mut und_ticks = Vec::with_capacity(rows.len().min(1024));
                        for r in &rows {
                            let Some(name) = js(r, &["instrument_name"]) else { continue };
                            let Some((u, d, k, right)) = parse_option_symbol(&name) else { continue };
                            if u != *und || !dates.contains(&d) {
                                continue;
                            }
                            let spot = jf(r, &["underlying_price"]);
                            if spot.is_some() { und_spot = spot; }
                            let s = spot.unwrap_or(0.0);
                            if s <= 0.0 { continue; }
                            // Strike window around ATM keeps the feed bounded.
                            let m = (k / s).ln().abs();
                            if m > ctx.cfg.atm_pct {
                                continue;
                            }
                            let pxu = |names: &[&str]| -> Option<f64> {
                                jf(r, names).map(|p| p * s).filter(|p| p.is_finite() && *p > 0.0)
                            };
                            und_ticks.push(InstrTick {
                                key: crate::key::InstKey {
                                    und: ctx.intern(&u),
                                    date: d,
                                    strike_ticks: (k * 10_000.0).round() as u64,
                                    right,
                                },
                                iv_mark_vp: jf(r, &["mark_iv"]).and_then(norm_iv_vp),
                                iv_bid_vp: None, // engine derives from bid_price
                                iv_ask_vp: None, // engine derives from ask_price
                                px_mark_unit: pxu(&["mark_price"]),
                                px_bid_unit: pxu(&["bid_price"]),
                                px_ask_unit: pxu(&["ask_price"]),
                                size_bid: None,
                                size_ask: None,
                                spot,
                                contract_size: Some(1.0),
                                venue_ts_ms: jf(r, &["creation_timestamp"]).map(|t| t as i64),
                            });
                        }
                        total += und_ticks.len() as u32;
                        if let Some(s) = und_spot {
                            let _ = ctx.tx.send(VenueMsg::Spot(SpotQuote {
                                underlying: und.clone(),
                                venue: "deribit".into(),
                                price: s,
                                ts_ms: now_ms(),
                            })).await;
                        }
                        all_ticks.extend(und_ticks);
                    }
                    if total > 0 {
                        let _ = ctx.tx.send(VenueMsg::Tracked {
                            venue: "deribit".into(),
                            instruments: total,
                        }).await;
                        let _ = ctx.tx.send(VenueMsg::Ticks {
                            venue: "deribit".into(),
                            ticks: all_ticks,
                        }).await;
                    } else {
                        return Err(anyhow::anyhow!("deribit: book summary had no parseable rows"));
                    }
                }
                _ = ctx.shutdown.changed() => {
                    if *ctx.shutdown.borrow() { return Ok(()); }
                }
            }
        }
    }
}

#[allow(dead_code)]
fn unused_dte(now: i64, d: u32) -> i64 {
    dte_days(now, d)
}
