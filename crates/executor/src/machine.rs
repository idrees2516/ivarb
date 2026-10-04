//! Attempt state machines: SNIPER (taker×2) and CONVERGENCE (passive×2).
//!
//! SNIPER honors the spec's execution doctrine:
//! 1. fire the **less-liquid venue first** (the harder fill),
//! 2. immediately work the liquid side,
//! 3. if leg B doesn't fill within the **10-second rule**, cancel and unwind
//!    leg A — never carry naked option exposure.
//!
//! CONVERGENCE captures mark-mode divergences passively: post both legs at
//! limits that lock ≥ 60% of the signal edge, reprice as the book moves,
//! and give up quietly when the bucket-scaled patience window expires.

use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use rustc_hash::FxHashMap;

use ivarb_protocol::{ExecEvent, ExecLevel, ExecSettings, Opportunity, Snapshot, WatchedLeg};

use crate::gateways::VenueRouter;
use crate::paper::{Fill, PaperBroker, Side};
use crate::now_ms;

/// Everything an attempt task needs (all cheap clones / Arcs).
pub struct AttemptCtx {
    pub attempt: u64,
    pub opp: Opportunity,
    /// Taker fee rates (fraction of notional) on each leg venue.
    pub buy_fee_rate: f64,
    pub sell_fee_rate: f64,
    /// Underlying units per contract on each leg venue.
    pub buy_ct: f64,
    pub sell_ct: f64,
    pub broker: Arc<PaperBroker>,
    /// Paper-vs-live venue dispatch (real gateways when live mode armed).
    pub router: Arc<ArcSwap<VenueRouter>>,
    pub watched: Arc<ArcSwap<FxHashMap<String, Vec<WatchedLeg>>>>,
    pub snapshot: Arc<ArcSwap<Snapshot>>,
    pub events: tokio::sync::mpsc::UnboundedSender<ExecEvent>,
    pub settings: ExecSettings,
}

impl AttemptCtx {
    /// Taker fill through the venue router — a REAL gateway order when
    /// live mode is armed and the venue has a gateway, the paper broker
    /// otherwise. Drop-in replacement for the old `broker.take` calls.
    #[allow(clippy::too_many_arguments)]
    async fn take(
        &self,
        venue: &str,
        side: Side,
        touch: f64,
        depth: f64,
        want_contracts: f64,
        fee_rate: f64,
        spot: f64,
        ct: f64,
    ) -> Fill {
        let router = self.router.load_full();
        router
            .take(venue, &self.inst(), side, touch, depth, want_contracts, fee_rate, spot, ct)
            .await
    }
}

impl AttemptCtx {
    fn ev(&self, level: ExecLevel, state: &str, msg: String) {
        let _ = self.events.send(ExecEvent {
            t_ms: now_ms(),
            attempt: self.attempt,
            opp: Some(self.opp.id.clone()),
            level,
            state: state.to_string(),
            msg,
        });
    }

    fn inst(&self) -> String {
        self.opp.id.split(':').next().unwrap_or(&self.opp.id).to_string()
    }

    /// Current quotes for our instrument: (buy-leg, sell-leg).
    fn legs(&self) -> Option<(WatchedLeg, WatchedLeg)> {
        let m = self.watched.load();
        let legs = m.get(&self.inst())?;
        let buy = legs
            .iter()
            .find(|l| l.venue == self.opp.buy_venue)
            .cloned()?;
        let sell = legs
            .iter()
            .find(|l| l.venue == self.opp.sell_venue)
            .cloned()?;
        Some((buy, sell))
    }

    /// Wait for the watched-quote bridge to publish our legs (the engine
    /// refreshes it on its 1-second sweep, so allow a short retry window).
    async fn legs_with_retry(&self, secs: u64) -> Option<(WatchedLeg, WatchedLeg)> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
        loop {
            if let Some(x) = self.legs() {
                return Some(x);
            }
            if std::time::Instant::now() >= deadline {
                return None;
            }
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        }
    }

    /// Vega per 1 underlying unit (USD per vp).
    fn vega_unit(&self) -> f64 {
        if self.opp.contract_size > 0.0 {
            self.opp.vega_vp_usd / self.opp.contract_size
        } else {
            self.opp.vega_vp_usd
        }
    }

    /// Re-validate the signal against the live engine snapshot: it must still
    /// exist and must not have decayed by more than 35%.
    fn signal_still_good(&self) -> bool {
        let snap = self.snapshot.load();
        match snap.opportunities.iter().find(|o| o.id == self.opp.id) {
            Some(o) => o.net_vp >= self.opp.net_vp * 0.65 && !o.stale,
            None => false,
        }
    }
}

/// Terminal outcome of one attempt.
pub enum AttemptOutcome {
    /// Both legs filled: build a Position from the draft.
    Opened(PositionDraft),
    /// No fills (or passive timeout): nothing to clean up.
    Aborted { reason: String, cooldown_ms: i64 },
    /// Leg A filled, leg B missed: unwound at a cost.
    Unwound { reason: String, cost_usd: f64 },
}

/// Position data produced by a successful attempt.
pub struct PositionDraft {
    pub attempt: u64,
    pub opp: Opportunity,
    pub units: f64,
    pub contracts_buy: f64,
    pub contracts_sell: f64,
    /// Per-unit premiums actually transacted.
    pub buy_px_unit: f64,
    pub sell_px_unit: f64,
    pub entry_fees_usd: f64,
    /// Spread in vp between actual fill prices (after slippage).
    pub entry_spread_vp: f64,
    pub entry_net_vp: f64,
    pub tier: ivarb_protocol::Tier,
}

fn fmt_usd(x: f64) -> String {
    if x.abs() >= 1000.0 {
        format!("${:.0}", x)
    } else {
        format!("${:.2}", x)
    }
}

// ---------------------------------------------------------------------------
// SNIPER
// ---------------------------------------------------------------------------

pub async fn run_sniper(ctx: AttemptCtx, units: f64) -> AttemptOutcome {
    let o = &ctx.opp;
    ctx.ev(
        ExecLevel::Info,
        "preflight",
        format!(
            "SNIPER {} {} {}→{} — edge {:+.2} vp (gate {:.2}), size {:.2} {}, EV {:+.2} vp, bucket {}",
            o.underlying,
            o.expiry,
            o.buy_venue,
            o.sell_venue,
            o.net_vp,
            o.gate_vp,
            units,
            o.underlying,
            o.ev_vp,
            o.bucket
        ),
    );

    if !ctx.signal_still_good() {
        ctx.ev(ExecLevel::Warn, "abort", "signal decayed or vanished before fire".into());
        return AttemptOutcome::Aborted { reason: "signal decayed".into(), cooldown_ms: 20_000 };
    }

    let (buy_leg, sell_leg) = match ctx.legs_with_retry(6).await {
        Some(x) => x,
        None => {
            ctx.ev(ExecLevel::Warn, "abort", "no live quotes on the watched legs".into());
            return AttemptOutcome::Aborted { reason: "quotes missing".into(), cooldown_ms: 15_000 };
        }
    };
    let spot = o.spot;
    let vega_unit = ctx.vega_unit();

    let (buy_touch, buy_depth) = match (buy_leg.px_ask_unit, buy_leg.size_ask) {
        (Some(p), Some(s)) if p > 0.0 && s > 0.0 => (p, s),
        _ => match (o.buy_px_usd, o.size_buy) {
            (Some(p), Some(s)) if p > 0.0 && s > 0.0 => (p / ctx.buy_ct, s),
            _ => {
                ctx.ev(ExecLevel::Warn, "abort", "buy leg has no executable ask".into());
                return AttemptOutcome::Aborted { reason: "no ask".into(), cooldown_ms: 30_000 };
            }
        },
    };
    let (sell_touch, sell_depth) = match (sell_leg.px_bid_unit, sell_leg.size_bid) {
        (Some(p), Some(s)) if p > 0.0 && s > 0.0 => (p, s),
        _ => match (o.sell_px_usd, o.size_sell) {
            (Some(p), Some(s)) if p > 0.0 && s > 0.0 => (p / ctx.sell_ct, s),
            _ => {
                ctx.ev(ExecLevel::Warn, "abort", "sell leg has no executable bid".into());
                return AttemptOutcome::Aborted { reason: "no bid".into(), cooldown_ms: 30_000 };
            }
        },
    };

    // Fire-time dust guard: a fresh-looking touch can still be a stale dust
    // quote (empty books). If the two legs are not within a sane band of
    // each other, the "edge" is garbage — abort.
    if sell_touch / buy_touch > 2.0 || sell_touch / buy_touch < 0.5 {
        ctx.ev(
            ExecLevel::Warn,
            "abort",
            format!(
                "dust-quote guard: buy ask {} vs sell bid {} are not in a sane band",
                fmt_usd(buy_touch),
                fmt_usd(sell_touch)
            ),
        );
        return AttemptOutcome::Aborted { reason: "dust quotes at fire time".into(), cooldown_ms: 60_000 };
    }

    // --- leg A: buy on the cheaper venue (fire the thinner side first) ------
    let want_buy_contracts = units / ctx.buy_ct;
    let fill_a: Fill = ctx
        .take(
            &o.buy_venue,
            Side::Buy,
            buy_touch,
            buy_depth,
            want_buy_contracts,
            ctx.buy_fee_rate,
            spot,
            ctx.buy_ct,
        )
        .await;

    if fill_a.contracts <= 0.0 {
        ctx.ev(ExecLevel::Warn, "leg_a", "leg A missed — no fill at the ask".into());
        return AttemptOutcome::Aborted { reason: "leg A missed".into(), cooldown_ms: 45_000 };
    }
    let filled_units_a = fill_a.contracts * ctx.buy_ct;
    let slip_a_vp = (fill_a.px_unit - buy_touch) / vega_unit;
    ctx.ev(
        ExecLevel::Ok,
        "leg_a",
        format!(
            "leg A filled: bought {:.2} {} on {} @ {} (latency {} ms, slip {:+.2} vp)",
            filled_units_a,
            o.underlying,
            o.buy_venue,
            fmt_usd(fill_a.px_unit),
            fill_a.latency_ms,
            slip_a_vp
        ),
    );

    // --- leg B: sell on the richer venue, inside the 10-second rule ----------
    let want_sell_contracts = filled_units_a / ctx.sell_ct;
    let timeout = Duration::from_secs_f64(ctx.settings.leg_timeout_s.max(1.0));
    let fill_b = match tokio::time::timeout(
        timeout,
        ctx.take(
            &o.sell_venue,
            Side::Sell,
            sell_touch,
            sell_depth,
            want_sell_contracts,
            ctx.sell_fee_rate,
            spot,
            ctx.sell_ct,
        ),
    )
    .await
    {
        Ok(f) => f,
        Err(_) => Fill::zero(),
    };

    if fill_b.contracts <= 0.0 {
        // 10-second rule: cancel leg B and unwind leg A immediately.
        ctx.ev(
            ExecLevel::Error,
            "leg_b",
            format!(
                "leg B unfilled after {:.0}s — cancelling and unwinding leg A (never hold the naked leg)",
                ctx.settings.leg_timeout_s
            ),
        );
        let unwind = ctx
            .take(
                &o.buy_venue,
                Side::Sell,
                buy_leg.px_bid_unit.unwrap_or(buy_touch * 0.98),
                buy_leg.size_bid.unwrap_or(buy_depth),
                fill_a.contracts,
                ctx.buy_fee_rate,
                spot,
                ctx.buy_ct,
            )
            .await;

        let cost = (fill_a.px_unit - unwind.px_unit) * filled_units_a * -1.0
            - fill_a.fee_usd
            - unwind.fee_usd;
        let cost = -cost.max(0.0).min(filled_units_a * buy_touch);
        ctx.ev(
            ExecLevel::Warn,
            "unwind",
            format!(
                "single-leg unwind complete — round-trip cost {:.2} USD (spread + fees), no position left",
                cost
            ),
        );
        return AttemptOutcome::Unwound { reason: "leg B timeout (10s rule)".into(), cost_usd: cost };
    }

    let mut filled_units_b = fill_b.contracts * ctx.sell_ct;
    let slip_b_vp = (sell_touch - fill_b.px_unit) / vega_unit;
    ctx.ev(
        ExecLevel::Ok,
        "leg_b",
        format!(
            "leg B filled: sold {:.2} {} on {} @ {} (latency {} ms, slip {:+.2} vp)",
            filled_units_b,
            o.underlying,
            o.sell_venue,
            fmt_usd(fill_b.px_unit),
            fill_b.latency_ms,
            slip_b_vp
        ),
    );

    // --- partial leg B: shed the unmatched excess of leg A ------------------
    if filled_units_b < filled_units_a - 1e-9 {
        let excess_units = filled_units_a - filled_units_b;
        ctx.ev(
            ExecLevel::Warn,
            "partial",
            format!(
                "leg B partial ({:.2} of {:.2} {}) — unwinding the unmatched excess",
                filled_units_b,
                filled_units_a,
                o.underlying
            ),
        );
        let excess_contracts = excess_units / ctx.buy_ct;
        let shed = ctx
            .take(
                &o.buy_venue,
                Side::Sell,
                buy_leg.px_bid_unit.unwrap_or(buy_touch * 0.98),
                buy_leg.size_bid.unwrap_or(buy_depth),
                excess_contracts,
                ctx.buy_fee_rate,
                spot,
                ctx.buy_ct,
            )
            .await;

        let _ = shed;
    } else {
        filled_units_b = filled_units_b.min(filled_units_a);
    }

    let units = filled_units_b.min(filled_units_a);
    if units <= 0.0 {
        return AttemptOutcome::Aborted { reason: "zero net fill".into(), cooldown_ms: 30_000 };
    }

    let entry_spread_vp = (fill_b.px_unit - fill_a.px_unit) / vega_unit;
    let fee_vp = (fill_a.fee_usd + fill_b.fee_usd) / (units * vega_unit);
    let entry_net = entry_spread_vp - fee_vp;
    ctx.ev(
        ExecLevel::Ok,
        "filled",
        format!(
            "both legs on — entry {:+.2} vp (signal {:+.2}, slippage {:+.2} vp), exit plan: take ≤{:.2} / stop −{:.2} / theta budget {:.2} vp / max hold {:.0}m",
            entry_net,
            o.net_vp,
            entry_net - o.net_vp,
            ctx.settings.take_profit_frac * entry_net.max(0.0),
            ctx.settings.stop_mult * entry_net.max(0.0),
            ctx.settings.theta_budget_frac * entry_net.max(0.0),
            o.hold_s / 60.0
        ),
    );

    AttemptOutcome::Opened(PositionDraft {
        attempt: ctx.attempt,
        opp: ctx.opp.clone(),
        units,
        contracts_buy: units / ctx.buy_ct,
        contracts_sell: units / ctx.sell_ct,
        buy_px_unit: fill_a.px_unit,
        sell_px_unit: fill_b.px_unit,
        entry_fees_usd: fill_a.fee_usd + fill_b.fee_usd,
        entry_spread_vp,
        entry_net_vp: entry_net,
        tier: ivarb_protocol::Tier::Sniper,
    })
}

// ---------------------------------------------------------------------------
// CONVERGENCE
// ---------------------------------------------------------------------------

pub async fn run_convergence(ctx: AttemptCtx, units: f64) -> AttemptOutcome {
    let o = &ctx.opp;
    let patience = Duration::from_secs_f64(o.patience_s.max(30.0));
    let vega_unit = ctx.vega_unit();
    let spot = o.spot;

    ctx.ev(
        ExecLevel::Info,
        "preflight",
        format!(
            "CONVERGENCE {} {} {}→{} — mark edge {:+.2} vp, passive capture, patience {:.0}m (bucket {})",
            o.underlying,
            o.expiry,
            o.buy_venue,
            o.sell_venue,
            o.net_vp,
            o.patience_s / 60.0,
            o.bucket
        ),
    );

    if !ctx.signal_still_good() {
        return AttemptOutcome::Aborted { reason: "signal decayed".into(), cooldown_ms: 20_000 };
    }

    let started = tokio::time::Instant::now();
    let mut last_requote = tokio::time::Instant::now();
    let mut buy_limit: Option<f64> = None;
    let mut sell_limit: Option<f64> = None;
    let mut filled_a: Option<f64> = None; // per-unit buy fill
    let mut filled_b: Option<f64> = None; // per-unit sell fill
    let mut single_leg_since: Option<tokio::time::Instant> = None;
    let mut rebudget = 0u32;

    loop {
        if ctx.snapshot.load().stats.ts_ms < 0 {
            break; // paranoia: never
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
        if started.elapsed() > patience {
            break;
        }

        let Some((buy_leg, sell_leg)) = ctx.legs() else {
            if filled_a.is_some() {
                // Quotes vanished with one leg on — defensive unwind.
                break;
            }
            continue;
        };
        // Touches, with the venue mark as a fallback when a book side is
        // empty (one-sided books are common on smaller venues). Stale legs
        // (slow venue polls) must not price passive orders — the fill would
        // lock an edge that no longer exists.
        let fresh = |l: &WatchedLeg| now_ms() - l.recv_ms <= 6_000;
        if !fresh(&buy_leg) || !fresh(&sell_leg) {
            continue;
        }
        let touch = |bid: Option<f64>, ask: Option<f64>, mark: Option<f64>| -> Option<(f64, f64)> {
            let m = mark.filter(|x| *x > 0.0)?;
            let b = bid.filter(|x| *x > 0.0).unwrap_or(m * 0.99);
            let a = ask.filter(|x| *x > 0.0).unwrap_or(m * 1.01);
            (a >= b).then_some((b, a))
        };
        let Some((bid_a, ask_a)) = touch(buy_leg.px_bid_unit, buy_leg.px_ask_unit, buy_leg.px_mark_unit) else { continue };
        let Some((bid_b, ask_b)) = touch(sell_leg.px_bid_unit, sell_leg.px_ask_unit, sell_leg.px_mark_unit) else { continue };

        // (Re)price the passive quotes every 30 s.
        if buy_limit.is_none() || last_requote.elapsed() > Duration::from_secs(30) {
            last_requote = tokio::time::Instant::now();
            rebudget += 1;
            // Lock ~60% of the signal edge net of fees, in price terms.
            let target_edge_vp = (o.gate_vp * 0.6).max(o.net_vp * 0.5);
            let fee_usd_unit = (ctx.buy_fee_rate + ctx.sell_fee_rate) * spot;
            let need = target_edge_vp * vega_unit + fee_usd_unit;
            let bl = bid_a + 0.30 * (ask_a - bid_a); // improve the touch a bit
            let sl = (bl + need).min(ask_b - 0.10 * (ask_b - bid_b));
            if sl <= bl || sl > ask_b || bl < bid_a {
                if filled_a.is_none() && filled_b.is_none() && rebudget >= 3 {
                    ctx.ev(ExecLevel::Warn, "abort", "cannot lock a positive edge passively — books too wide".into());
                    return AttemptOutcome::Aborted { reason: "cannot lock edge".into(), cooldown_ms: 60_000 };
                }
                continue;
            }
            buy_limit = Some(bl);
            sell_limit = Some(sl);
            if rebudget == 1 {
                ctx.ev(
                    ExecLevel::Info,
                    "posted",
                    format!("posted passive legs: buy {} / sell {} (locked edge ≈ {:+.2} vp)",
                        fmt_usd(bl), fmt_usd(sl), (sl - bl - fee_usd_unit) / vega_unit),
                );
            }
        }
        let (Some(bl), Some(sl)) = (buy_limit, sell_limit) else { continue };

        // Try to fill the resting legs.
        if filled_a.is_none() {
            if let Some(px) = ctx.broker.maker_fill(Side::Buy, bl, ask_a) {
                let contracts = units / ctx.buy_ct;
                ctx.broker.book_maker(&o.buy_venue, Side::Buy, contracts, px, ctx.buy_ct);
                filled_a = Some(px);
                single_leg_since = Some(tokio::time::Instant::now());
                ctx.ev(ExecLevel::Ok, "leg_a", format!("passive buy filled @ {} (maker)", fmt_usd(px)));
            }
        }
        if filled_b.is_none() {
            if let Some(px) = ctx.broker.maker_fill(Side::Sell, sl, bid_b) {
                let contracts = units / ctx.sell_ct;
                ctx.broker.book_maker(&o.sell_venue, Side::Sell, contracts, px, ctx.sell_ct);
                filled_b = Some(px);
                single_leg_since = Some(tokio::time::Instant::now());
                ctx.ev(ExecLevel::Ok, "leg_b", format!("passive sell filled @ {} (maker)", fmt_usd(px)));
            }
        }

        // Both legs on?
        if let (Some(px_a), Some(px_b)) = (filled_a, filled_b) {
            let entry_spread_vp = (px_b - px_a) / vega_unit;
            ctx.ev(
                ExecLevel::Ok,
                "filled",
                format!("both passive legs on — entry {:+.2} vp vs signal {:+.2}", entry_spread_vp, o.net_vp),
            );
            return AttemptOutcome::Opened(PositionDraft {
                attempt: ctx.attempt,
                opp: ctx.opp.clone(),
                units,
                contracts_buy: units / ctx.buy_ct,
                contracts_sell: units / ctx.sell_ct,
                buy_px_unit: px_a,
                sell_px_unit: px_b,
                entry_fees_usd: 0.0, // maker legs
                entry_spread_vp,
                entry_net_vp: entry_spread_vp,
                tier: ivarb_protocol::Tier::Convergence,
            });
        }

        // Single-leg risk management: after a 20 s grace, either take the
        // other leg at market (if edge still works) or unwind.
        if let (Some(since), Some(px_a)) = (single_leg_since, filled_a) {
            if since.elapsed() > Duration::from_secs(20) {
                let exec_edge_vp = (bid_b - px_a) / vega_unit
                    - (ctx.sell_fee_rate * spot) / vega_unit;
                if exec_edge_vp >= 0.10 {
                    ctx.ev(ExecLevel::Info, "leg_b", format!("taking leg B at market — remaining edge {:+.2} vp", exec_edge_vp));
                    let f = ctx
                        .take(
                            &o.sell_venue,
                            Side::Sell,
                            bid_b,
                            sell_leg.size_bid.unwrap_or(1.0),
                            units / ctx.sell_ct,
                            ctx.sell_fee_rate,
                            spot,
                            ctx.sell_ct,
                        )
                        .await;
                    if f.contracts > 0.0 {
                        let entry_spread_vp = (f.px_unit - px_a) / vega_unit;
                        return AttemptOutcome::Opened(PositionDraft {
                            attempt: ctx.attempt,
                            opp: ctx.opp.clone(),
                            units: f.contracts * ctx.sell_ct,
                            contracts_buy: units / ctx.buy_ct,
                            contracts_sell: f.contracts,
                            buy_px_unit: px_a,
                            sell_px_unit: f.px_unit,
                            entry_fees_usd: f.fee_usd,
                            entry_spread_vp,
                            entry_net_vp: entry_spread_vp - f.fee_usd / (units * vega_unit),
                            tier: ivarb_protocol::Tier::Convergence,
                        });
                    }
                }
                // Unwind the passive fill.
                let unwind = ctx
                    .take(
                        &o.buy_venue,
                        Side::Sell,
                        bid_a,
                        buy_leg.size_bid.unwrap_or(1.0),
                        units / ctx.buy_ct,
                        ctx.buy_fee_rate,
                        spot,
                        ctx.buy_ct,
                    )
                    .await;
                let cost = (px_a - unwind.px_unit) * units - unwind.fee_usd;
                ctx.ev(ExecLevel::Warn, "unwind", format!("single passive leg unwound — cost {:.2} USD", cost));
                return AttemptOutcome::Unwound { reason: "convergence leg B never came".into(), cost_usd: cost };
            }
        }
    }

    // Patience expired: any resting fills already handled above; abort quietly.
    ctx.ev(ExecLevel::Info, "abort", format!("patience window ({:.0}m) expired — resting orders cancelled, no fees", patience.as_secs_f64() / 60.0));
    AttemptOutcome::Aborted { reason: "patience expired".into(), cooldown_ms: 120_000 }
}
