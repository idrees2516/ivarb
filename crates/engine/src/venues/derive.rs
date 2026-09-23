//! Derive (formerly Lyra v2) connector — live via the RPC API on
//! `api.lyra.finance`.
//!
//! Transport strategy ("whatever works", probed live on 2026-09-24):
//! - `POST /public/get_instruments` `{expired:false, instrument_type:"option",
//!   currency:UND}` → full option chain metadata (830 BTC rows), refreshed
//!   every 10 minutes to pick up new expiries.
//! - `POST /public/get_tickers` `{instrument_type:"option", currency:UND,
//!   expired:false, expiry_date:YYYYMMDD}` → **the whole expiry chain in one
//!   call** (44-94 rows), including best bid/ask (+sizes), mark, index and the
//!   venue's own IV surface (`option_pricing.{i,bi,ai}`).
//!
//! One poll cycle = one request per (underlying × selected expiry) — the
//! cheapest live transport available for this venue, no per-instrument calls.
//!
//! Response shape (compact keys):
//! ```json
//! { "t": 1790185230143, "A": "7.1386", "a": "4113", "B": "0.12", "b": "3900",
//!   "I": "84047", "M": "4009",
//!   "option_pricing": { "d": "-0.897", "t": "-102.27", "g": "0.00006",
//!     "v": "12.75", "i": "0.41661", "bi": "0.3011", "ai": "0.48813",
//!     "f": "84129", "m": "4009", "df": "0.999" } }
//! ```
//! Amounts are in underlying units (BTC), prices in USDC per 1 unit of
//! underlying exposure → contract_size = 1.0.

use std::collections::HashMap;
use std::time::Duration;

use crate::jsonutil::{jf, norm_iv_vp};
use crate::key::{cap_expiries, parse_option_symbol, InstKey};
use crate::msg::{now_ms, InstrTick, VenueMsg};
use crate::venues::{http_post_json, status, VenueConnector, VenueCtx};
use ivarb_protocol::{ConnState, SpotQuote};

pub struct Derive {
    /// instrument_name → key, for the selected (near-ATM) universe.
    index: HashMap<String, InstKey>,
    /// Selected expiry dates per underlying (yyyymmdd).
    expiries: HashMap<String, Vec<u32>>,
    last_refresh_ms: i64,
}

impl Derive {
    pub fn new() -> Self {
        Derive {
            index: HashMap::new(),
            expiries: HashMap::new(),
            last_refresh_ms: 0,
        }
    }
}

/// One row of the get_tickers response, normalized.
struct DeriveTick {
    key: InstKey,
    bid: Option<f64>,
    ask: Option<f64>,
    bid_sz: Option<f64>,
    ask_sz: Option<f64>,
    mark: Option<f64>,
    iv_mark: Option<f64>,
    iv_bid: Option<f64>,
    iv_ask: Option<f64>,
    index: Option<f64>,
    ts_ms: Option<i64>,
}

/// Parse the compact get_tickers payload (dict form keyed by instrument, or
/// array form from get_ticker).
fn parse_ticker_row(name: &str, v: &serde_json::Value) -> Option<DeriveTick> {
    let (_und, date, strike, right) = parse_option_symbol(name)?;
    let key = InstKey {
        und: 0, // fixed up by the caller via the interner
        date,
        strike_ticks: (strike * 10_000.0).round() as u64,
        right,
    };
    // Compact dict form uses single letters; array/full form uses long names.
    let bid = jf(v, &["b", "best_bid", "best_bid_price", "bid"])
        .filter(|x| *x > 0.0);
    let ask = jf(v, &["a", "best_ask", "best_ask_price", "ask"])
        .filter(|x| *x > 0.0);
    let bid_sz = jf(v, &["B", "best_bid_amount", "bid_size"]);
    let ask_sz = jf(v, &["A", "best_ask_amount", "ask_size"]);
    let mark = jf(v, &["M", "mark_price"]).filter(|x| *x > 0.0);
    let index = jf(v, &["I", "index", "index_price", "spot"]).filter(|x| *x > 0.0);
    let ts_ms = jf(v, &["t", "timestamp"]).map(|t| t as i64);
    let op = v.get("option_pricing").filter(|o| !o.is_null());
    let (iv_mark, iv_bid, iv_ask) = match op {
        Some(o) => (
            jf(o, &["i", "iv"]).and_then(norm_iv_vp),
            jf(o, &["bi", "bid_iv"]).and_then(norm_iv_vp),
            jf(o, &["ai", "ask_iv"]).and_then(norm_iv_vp),
        ),
        None => (None, None, None),
    };
    if bid.is_none() && ask.is_none() && mark.is_none() && iv_mark.is_none() {
        return None;
    }
    Some(DeriveTick {
        key,
        bid,
        ask,
        bid_sz,
        ask_sz,
        mark,
        iv_mark,
        iv_bid,
        iv_ask,
        index,
        ts_ms,
    })
}

#[async_trait::async_trait]
impl VenueConnector for Derive {
    fn id(&self) -> &str {
        "derive"
    }

    async fn run(&mut self, mut ctx: VenueCtx) -> anyhow::Result<()> {
        status(&ctx.tx, "derive", ConnState::Connecting, None).await;

        let unds: Vec<String> = ctx.cfg.underlyings.clone();
        let mut spot_last_sent: HashMap<String, i64> = HashMap::new();
        let mut empty_rounds = 0u32;
        let mut first = true;

        let poll = Duration::from_millis(ctx.spec.poll_ms.max(4_000));
        let mut ticker = tokio::time::interval(poll);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    if ctx.shutdown() { return Ok(()); }
                    let now = now_ms();

                    // --- universe refresh (every 10 min, or on first run) ----
                    if first || now - self.last_refresh_ms > 600_000 {
                        match self.refresh_universe(&ctx, &unds).await {
                            Ok(n) => {
                                tracing::info!(venue = "derive", instruments = n, "universe refreshed");
                                first = false;
                                self.last_refresh_ms = now;
                                empty_rounds = 0;
                            }
                            Err(e) => {
                                if first {
                                    return Err(anyhow::Error::new(super::Fatal(format!(
                                        "instrument endpoint not recognized ({e})"
                                    ))));
                                }
                                tracing::warn!(venue = "derive", error = %e, "universe refresh failed; keeping old set");
                            }
                        }
                    }

                    if self.index.is_empty() {
                        continue;
                    }

                    // --- poll one request per (underlying × expiry) ---------
                    let mut ticks: Vec<InstrTick> = Vec::with_capacity(256);
                    let mut spot: Option<(String, f64)> = None;
                    for und in &unds {
                        let Some(dates) = self.expiries.get(und) else { continue };
                        for date in dates {
                            if ctx.shutdown() { return Ok(()); }
                            let body = serde_json::json!({
                                "instrument_type": "option",
                                "currency": und,
                                "expired": false,
                                "expiry_date": format!("{date}"),
                            });
                            let v = match http_post_json(
                                &ctx.http,
                                &format!("{}/public/get_tickers", ctx.spec.rest_base),
                                &body,
                            ).await {
                                Ok(v) => v,
                                Err(e) => {
                                    tracing::debug!(venue = "derive", error = %e, "get_tickers failed");
                                    continue;
                                }
                            };
                            let rows = v
                                .get("result")
                                .and_then(|r| r.get("tickers"))
                                .and_then(|t| t.as_object())
                                .map(|m| m.iter().collect::<Vec<_>>());
                            let Some(rows) = rows else { continue };
                            for (name, row) in rows {
                                let Some(dt) = parse_ticker_row(name, row) else { continue };
                                let Some(real_key) = self.index.get(name) else { continue };
                                if spot.is_none() {
                                    if let Some(px) = dt.index {
                                        spot = Some((und.clone(), px));
                                    }
                                }
                                let px = |p: Option<f64>| p.filter(|x| x.is_finite() && *x > 0.0);
                                ticks.push(InstrTick {
                                    key: *real_key,
                                    iv_mark_vp: dt.iv_mark,
                                    iv_bid_vp: dt.iv_bid,
                                    iv_ask_vp: dt.iv_ask,
                                    px_mark_unit: px(dt.mark),
                                    px_bid_unit: px(dt.bid),
                                    px_ask_unit: px(dt.ask),
                                    size_bid: dt.bid_sz,
                                    size_ask: dt.ask_sz,
                                    spot: dt.index,
                                    contract_size: Some(1.0),
                                    venue_ts_ms: dt.ts_ms,
                                });
                            }
                        }
                    }

                    // Publish consensus spot at a modest cadence.
                    if let Some((und, px)) = spot {
                        let last = spot_last_sent.get(&und).copied().unwrap_or(0);
                        if now - last > 4_000 {
                            let _ = ctx.tx.send(VenueMsg::Spot(SpotQuote {
                                underlying: und.clone(),
                                venue: "derive".into(),
                                price: px,
                                ts_ms: now,
                            })).await;
                            spot_last_sent.insert(und, now);
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
                        if empty_rounds >= 5 {
                            return Err(anyhow::anyhow!("5 consecutive empty poll rounds"));
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

impl Derive {
    /// Refresh the (underlying → expiries → instruments) universe from
    /// `get_instruments`, keeping strikes within the configured ATM band.
    async fn refresh_universe(&mut self, ctx: &VenueCtx, unds: &[String]) -> anyhow::Result<usize> {
        let now = now_ms();
        let mut index = HashMap::new();
        let mut expiries: HashMap<String, Vec<u32>> = HashMap::new();

        for und in unds {
            let body = serde_json::json!({
                "expired": false,
                "instrument_type": "option",
                "currency": und,
            });
            let v = http_post_json(
                &ctx.http,
                &format!("{}/public/get_instruments", ctx.spec.rest_base),
                &body,
            )
            .await?;
            if let Some(err) = v.get("error").filter(|e| !e.is_null()) {
                anyhow::bail!("rpc error: {err}");
            }
            let rows = v
                .get("result")
                .and_then(|r| r.as_array())
                .ok_or_else(|| anyhow::anyhow!("unexpected get_instruments shape"))?;

            // Collect option expiries within the DTE window.
            let mut dates: Vec<u32> = rows
                .iter()
                .filter_map(|r| {
                    r.get("option_details")
                        .and_then(|o| o.get("expiry"))
                        .and_then(|e| e.as_i64())
                        .map(|sec| {
                            // unix sec → yyyymmdd (UTC)
                            let days = (sec / 86_400) as u32;
                            civil_from_days(days as i64)
                        })
                })
                .collect();
            cap_expiries(
                &mut dates,
                now,
                ctx.cfg.min_dte,
                ctx.cfg.max_dte,
                ctx.cfg.max_expiries,
            );
            expiries.insert(und.clone(), dates.clone());

            let und_id = ctx.intern(und);
            let spot = ctx.consensus_spot(und);
            let mut kept = 0usize;
            for r in rows {
                let Some(name) = r.get("instrument_name").and_then(|n| n.as_str()) else { continue };
                let Some((u, d, k, right)) = parse_option_symbol(name) else { continue };
                if u != *und || !dates.contains(&d) {
                    continue;
                }
                // Near-ATM band once a consensus spot exists.
                if let Some(s) = spot {
                    if s > 0.0 && ((k / s).ln().abs()) > ctx.cfg.atm_pct {
                        continue;
                    }
                }
                index.insert(
                    name.to_string(),
                    InstKey {
                        und: und_id,
                        date: d,
                        strike_ticks: (k * 10_000.0).round() as u64,
                        right,
                    },
                );
                kept += 1;
            }
            let _ = kept;
        }

        if index.is_empty() {
            anyhow::bail!("no option instruments recognized");
        }
        let n = index.len();
        self.index = index;
        self.expiries = expiries;
        let _ = ctx
            .tx
            .try_send(VenueMsg::Tracked {
                venue: "derive".into(),
                instruments: n as u32,
            });
        Ok(n)
    }
}

/// Days since epoch → yyyymmdd (Howard Hinnant's civil_from_days).
fn civil_from_days(z: i64) -> u32 {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y };
    (y as u32) * 10_000 + m * 100 + d
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_dates() {
        assert_eq!(civil_from_days(0), 19700101);
        assert_eq!(civil_from_days(20_720), 20260924); // 2026-09-24 (verified)
    }

    #[test]
    fn parses_compact_row() {
        let row: serde_json::Value = serde_json::json!({
            "t": 1790185230143_i64, "A": "7.1386", "a": "4113", "B": "0.12", "b": "3900",
            "I": "84047", "M": "4009",
            "option_pricing": {"d": "-0.897", "v": "12.75", "i": "0.41661",
                               "bi": "0.3011", "ai": "0.48813", "f": "84129", "m": "4009"}
        });
        let dt = parse_ticker_row("BTC-20260926-88000-P", &row).unwrap();
        assert_eq!(dt.bid, Some(3900.0));
        assert_eq!(dt.ask, Some(4113.0));
        assert_eq!(dt.bid_sz, Some(0.12));
        assert_eq!(dt.ask_sz, Some(7.1386));
        assert_eq!(dt.iv_mark, Some(41.661));
        assert_eq!(dt.iv_bid, Some(30.11));
        assert_eq!(dt.iv_ask, Some(48.813));
        assert_eq!(dt.index, Some(84047.0));
    }
}
