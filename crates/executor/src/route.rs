//! Multi-leg route execution — the sequencer that turns a detected multi-hop
//! cycle (buy → convert → … → sell) into one atomic batch of taker fills.
//!
//! A route cycle is *flat at expiry by construction*: every option bought is
//! sold or converted away, so a completed route leaves no inventory and books
//! its cash immediately. The execution problem is therefore not position
//! management but **sequencing and contingency**:
//!
//! 1. **Feasible hardest-first ordering** — a `convert` leg requires holding
//!    its source right on that venue, so the order must respect data
//!    dependencies; among the legs that are executable *now*, the thinnest
//!    book fires first (the doctrine the two-leg SNIPER already follows:
//!    fill the hard side while the edge is still there).
//! 2. **Generalized 10-second rule** — any leg whose quotes vanish or go
//!    stale fails the attempt; every previously filled leg is then unwound
//!    in strict reverse order at current prices.
//! 3. **Mid-flight abandonment** — before each leg the sequencer re-prices
//!    the *remaining* legs from live quotes. If the projected completion is
//!    negative AND worse than the estimated cost of unwinding what is
//!    already on, the route is abandoned mid-cycle instead of completed —
//!    never finish a broken cycle just to avoid admitting it broke.
//! 4. **Partial-fill sizing** — the cycle size is the running minimum of
//!    what each leg actually filled; residual cross-venue imbalance is
//!    settled at market in a final `settle` pass.
//! 5. **Parity conversions** — a `convert` executes as sell-source @bid +
//!    buy-target @ask + a perp hedge trade for the delta jump (−1 per
//!    unit); the perp is closed at route end, mirroring the two hedge fees
//!    the scanner already charged the route.

use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use rustc_hash::FxHashMap;

use ivarb_protocol::{
    ExecEvent, ExecLevel, ExecSettings, MultiHopLeg, MultiHopRoute, Snapshot, TradeLeg, WatchedLeg,
};

use crate::gateways::VenueRouter;
use crate::now_ms;
use crate::paper::{Fill, PaperBroker, Side};

/// Everything a route attempt task needs (cheap clones / Arcs).
pub struct RouteCtx {
    pub attempt: u64,
    pub route: MultiHopRoute,
    pub spot: f64,
    pub settings: ExecSettings,
    pub broker: Arc<PaperBroker>,
    /// Paper-vs-live dispatch per venue leg (real gateways when armed).
    pub router: Arc<ArcSwap<VenueRouter>>,
    pub watched: Arc<ArcSwap<FxHashMap<String, Vec<WatchedLeg>>>>,
    pub snapshot: Arc<ArcSwap<Snapshot>>,
    pub events: tokio::sync::mpsc::UnboundedSender<ExecEvent>,
    /// Taker fee rate (fraction of notional) per venue.
    pub fee_rates: FxHashMap<String, f64>,
}

/// Terminal outcome of one route attempt.
#[derive(Debug)]
pub enum RouteOutcome {
    /// Cycle completed flat; cash banked.
    Banked {
        profit_usd: f64,
        fees_usd: f64,
        hedge_pnl_usd: f64,
        units: f64,
        legs: Vec<TradeLeg>,
    },
    /// Nothing filled; no cleanup needed.
    Aborted { reason: String, cooldown_ms: i64 },
    /// Some legs filled then the cycle broke; unwound at a cost. `profit_usd`
    /// is the signed realized result of the whole round trip (≤ 0 typically).
    Unwound {
        reason: String,
        profit_usd: f64,
        fees_usd: f64,
        hedge_pnl_usd: f64,
        units: f64,
        legs: Vec<TradeLeg>,
    },
}

/// Signed perp exposure carried through conversions.
#[derive(Default)]
struct Hedge {
    units: f64,
    avg_px: f64,
    realized_usd: f64,
}

impl Hedge {
    fn trade(&mut self, side: Side, units: f64, px: f64) {
        let signed = if side == Side::Buy { units } else { -units };
        let new_units = self.units + signed;
        self.avg_px = if new_units.abs() > 1e-9 {
            (self.avg_px * self.units + px * signed) / new_units
        } else {
            0.0
        };
        self.units = new_units;
    }
    fn mark_pnl(&self, spot: f64) -> f64 {
        self.realized_usd + self.units * (spot - self.avg_px)
    }
}

/// Mutable execution state of one route attempt.
struct ExecState {
    /// Signed option inventory per (venue, right char), underlying units.
    inv: FxHashMap<(String, char), f64>,
    /// Legs actually executed, in execution order (for reverse unwind).
    done: Vec<TradeLeg>,
    /// Cash flow so far (premiums and fees; positive = received), USD.
    cash: f64,
    fees: f64,
    /// Cycle size in underlying units — the running min of filled legs.
    units: f64,
    hedge: Hedge,
    started: std::time::Instant,
}

impl RouteCtx {
    fn ev(&self, level: ExecLevel, state: &str, msg: String) {
        let _ = self.events.send(ExecEvent {
            t_ms: now_ms(),
            attempt: self.attempt,
            opp: Some(self.route.id.clone()),
            level,
            state: state.to_string(),
            msg,
        });
    }

    fn fee_rate(&self, venue: &str) -> f64 {
        self.fee_rates.get(venue).copied().unwrap_or(0.0003)
    }

    /// Live quote for (instrument, venue) — None when absent.
    fn quote(&self, inst: &str, venue: &str) -> Option<WatchedLeg> {
        let m = self.watched.load();
        m.get(inst)?
            .iter()
            .find(|l| l.venue == venue)
            .cloned()
    }

    /// Quote with a short retry window (the engine's watch sweep publishes
    /// newly registered instruments within ~1 s).
    async fn quote_with_retry(&self, inst: &str, venue: &str, secs: u64) -> Option<WatchedLeg> {
        let deadline = std::time::Instant::now() + Duration::from_secs(secs);
        loop {
            if let Some(q) = self.quote(inst, venue) {
                return Some(q);
            }
            if std::time::Instant::now() >= deadline {
                return None;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }
}

/// The other right of the same strike family ("…-C" ↔ "…-P").
pub fn flip_right(inst: &str) -> String {
    let mut s = inst.to_string();
    if s.ends_with("-C") {
        s.pop();
        s.push('P');
    } else if s.ends_with("-P") {
        s.pop();
        s.push('C');
    }
    s
}

fn right_of(inst: &str) -> char {
    inst.rsplit('-').next().and_then(|c| c.chars().next()).unwrap_or('C')
}

/// Plan the execution order: repeatedly pick the thinnest feasible leg.
///
/// Feasibility: `buy`/`sell` are always executable (shorting needs no
/// inventory); `convert` requires a long position in its source right on the
/// same venue. Returns None when no progress is possible (should not happen
/// for scanner-produced routes, which are dependency-consistent by
/// construction).
fn plan_order(route: &MultiHopRoute) -> Option<Vec<usize>> {
    let mut inv: FxHashMap<(String, char), f64> = FxHashMap::default();
    let mut remaining: Vec<usize> = (0..route.legs.len()).collect();
    let mut order = Vec::with_capacity(route.legs.len());
    while !remaining.is_empty() {
        let mut best: Option<(f64, usize)> = None;
        for &i in &remaining {
            let l = &route.legs[i];
            let feasible = if l.action == "convert" {
                let src_inst = flip_right(&l.instrument);
                inv.get(&(l.venue.clone(), right_of(&src_inst))).copied().unwrap_or(0.0) > 1e-9
            } else {
                true
            };
            if feasible && best.map(|(thinnest, _)| l.size_units < thinnest).unwrap_or(true) {
                best = Some((l.size_units, i));
            }
        }
        let (_, i) = best?;
        remaining.retain(|&x| x != i);
        order.push(i);
        // Apply the planned inventory effect (at route size; execution
        // re-tracks at actual filled units).
        let l = &route.legs[i];
        let u = route.size_units.max(0.0);
        match l.action.as_str() {
            "buy" => *inv.entry((l.venue.clone(), right_of(&l.instrument))).or_insert(0.0) += u,
            "sell" => *inv.entry((l.venue.clone(), right_of(&l.instrument))).or_insert(0.0) -= u,
            _ => {
                let src = flip_right(&l.instrument);
                *inv.entry((l.venue.clone(), right_of(&src))).or_insert(0.0) -= u;
                *inv.entry((l.venue.clone(), right_of(&l.instrument))).or_insert(0.0) += u;
            }
        }
    }
    Some(order)
}

/// One planned leg re-priced from live quotes.
struct LiveLeg {
    /// Net cash per unit at live prices (negative = pay).
    live_cost_unit: f64,
    /// Touch prices for execution (buy → ask; sell → bid).
    touch: f64,
    depth: f64,
}

fn price_leg(leg: &MultiHopLeg, quote: Option<&WatchedLeg>, fee_rate: f64, spot: f64) -> LiveLeg {
    let fallback_px = leg.px_unit.abs().max(1.0);
    match leg.action.as_str() {
        "buy" => {
            let (ask, depth) = quote
                .and_then(|q| q.px_ask_unit.zip(q.size_ask))
                .filter(|(p, s)| *p > 0.0 && *s > 0.0)
                .unwrap_or((leg.px_unit.max(1.0), leg.size_units));
            LiveLeg { live_cost_unit: ask + fee_rate * spot, touch: ask, depth }
        }
        "sell" => {
            let (bid, depth) = quote
                .and_then(|q| q.px_bid_unit.zip(q.size_bid))
                .filter(|(p, s)| *p > 0.0 && *s > 0.0)
                .unwrap_or((leg.px_unit.max(1.0), leg.size_units));
            LiveLeg { live_cost_unit: -(bid - fee_rate * spot), touch: bid, depth }
        }
        // convert: sell source @bid, buy target @ask, + perp hedge fee.
        _ => {
            let (pa, da) = quote
                .and_then(|q| q.px_ask_unit.zip(q.size_ask))
                .filter(|(p, s)| *p > 0.0 && *s > 0.0)
                .unwrap_or((leg.px_unit.max(1.0), leg.size_units));
            let (pb, _) = quote
                .and_then(|q| q.px_bid_unit.zip(q.size_bid))
                .filter(|(p, _)| *p > 0.0)
                .unwrap_or((fallback_px, 0.0));
            // net = pay target ask, receive source bid, pay option fees both
            // sides + 2 perp fees (mirrors the scanner's model).
            let cost = pa - pb + 2.0 * fee_rate * spot + 2.0 * 0.0005 * spot;
            LiveLeg { live_cost_unit: cost, touch: pa, depth: da }
        }
    }
}

/// Dust guard: a live touch wildly away from the route's printed price (or
/// the venue mark) is a flicker artifact, not an edge. `cap` is the
/// theoretical premium bound for this leg's right (call ≤ spot, put ≤
/// strike, ×1.05 headroom) — a touch above it is a venue-unit artifact
/// (wrong contract-size divisor, decimal shift), never a tradeable price.
fn sane_touch(touch: f64, printed: f64, mark: Option<f64>, cap: f64) -> bool {
    if touch <= 0.0 {
        return false;
    }
    if cap > 0.0 && touch > cap {
        return false;
    }
    if printed.abs() > 1.0 && !(0.5..=1.6).contains(&(touch / printed)) {
        return false;
    }
    if let Some(m) = mark.filter(|m| *m > 0.0) {
        if !(0.5..=1.6).contains(&(touch / m)) {
            return false;
        }
    }
    true
}

/// Theoretical premium cap for an instrument's right at the route's spot
/// and strike (call bounded by the underlying, put by the strike).
fn premium_cap(ctx: &RouteCtx, inst: &str) -> f64 {
    let cap = if right_of(inst) == 'C' {
        ctx.spot
    } else {
        ctx.route.strike
    };
    (cap * 1.05).max(0.0)
}

#[allow(clippy::too_many_arguments)]
async fn take_leg(
    ctx: &RouteCtx,
    venue: &str,
    instrument: &str,
    side: Side,
    touch: f64,
    depth_units: f64,
    contracts: f64,
    ct: f64,
) -> Fill {
    let router = ctx.router.load_full();
    router
        .take(
            venue,
            instrument,
            side,
            touch,
            (depth_units / ct.max(1e-9)).max(contracts.min(1.0)),
            contracts,
            ctx.fee_rate(venue),
            ctx.spot,
            ct,
        )
        .await
}

/// Execute one planned leg at the current cycle size.
async fn exec_leg(ctx: &RouteCtx, st: &mut ExecState, leg: &MultiHopLeg, k: usize, n: usize) -> Result<(), String> {
    let quote = ctx
        .quote_with_retry(&leg.instrument, &leg.venue, 2)
        .await
        .ok_or_else(|| format!("no quotes on {} {}", leg.venue, leg.instrument))?;
    let fresh = now_ms() - quote.recv_ms <= 6_000;
    if !fresh {
        return Err(format!("stale quotes on {} (>6 s)", leg.venue));
    }
    let ct = if quote.contract_size > 0.0 { quote.contract_size } else { 1.0 };

    match leg.action.as_str() {
        "buy" => {
            let (touch, depth) = quote
                .px_ask_unit
                .zip(quote.size_ask)
                .filter(|(p, s)| *p > 0.0 && *s > 0.0)
                .ok_or("buy leg has no executable ask")?;
            if !sane_touch(touch, leg.px_unit, quote.px_mark_unit, premium_cap(ctx, &leg.instrument)) {
                return Err(format!("dust ask on {} ({:.0} vs print {:.0})", leg.venue, touch, leg.px_unit));
            }
            let want = st.units / ct;
            let f = take_leg(ctx, &leg.venue, &leg.instrument, Side::Buy, touch, depth, want, ct).await;
            if f.contracts <= 0.0 {
                return Err("no fill at the ask".into());
            }
            apply_fill(ctx, st, leg, "buy", f, ct, touch, depth, k, n).await
        }
        "sell" => {
            let (touch, depth) = quote
                .px_bid_unit
                .zip(quote.size_bid)
                .filter(|(p, s)| *p > 0.0 && *s > 0.0)
                .ok_or("sell leg has no executable bid")?;
            if !sane_touch(touch, leg.px_unit, quote.px_mark_unit, premium_cap(ctx, &leg.instrument)) {
                return Err(format!("dust bid on {} ({:.0} vs print {:.0})", leg.venue, touch, leg.px_unit));
            }
            let want = st.units / ct;
            let f = take_leg(ctx, &leg.venue, &leg.instrument, Side::Sell, touch, depth, want, ct).await;
            if f.contracts <= 0.0 {
                return Err("no fill at the bid".into());
            }
            apply_fill(ctx, st, leg, "sell", f, ct, touch, depth, k, n).await
        }
        _ => {
            // Parity conversion: sell the source right, buy the target right,
            // perp-hedge the delta jump. Both sub-books must be tradable.
            let src_inst = flip_right(&leg.instrument);
            let src_quote = ctx
                .quote_with_retry(&src_inst, &leg.venue, 2)
                .await
                .ok_or_else(|| format!("no quotes on conversion source {} {}", leg.venue, src_inst))?;
            if now_ms() - src_quote.recv_ms > 6_000 {
                return Err("stale conversion source quotes".into());
            }
            let src_ct = if src_quote.contract_size > 0.0 { src_quote.contract_size } else { ct };
            let (sb, sd) = src_quote
                .px_bid_unit
                .zip(src_quote.size_bid)
                .filter(|(p, s)| *p > 0.0 && *s > 0.0)
                .ok_or("conversion source has no bid")?;
            let (ta, td) = quote
                .px_ask_unit
                .zip(quote.size_ask)
                .filter(|(p, s)| *p > 0.0 && *s > 0.0)
                .ok_or("conversion target has no ask")?;
            if !sane_touch(ta, quote.px_mark_unit.unwrap_or(ta), None, premium_cap(ctx, &leg.instrument))
                || !sane_touch(sb, src_quote.px_mark_unit.unwrap_or(sb), None, premium_cap(ctx, &src_inst))
            {
                return Err("dust book on conversion leg".into());
            }

            // Precondition: we hold the source right on this venue.
            let held = st
                .inv
                .get(&(leg.venue.clone(), right_of(&src_inst)))
                .copied()
                .unwrap_or(0.0);
            if held < st.units - 1e-9 {
                return Err(format!(
                    "conversion precondition failed: hold {:.4} of {} on {}, need {:.4}",
                    held, src_inst, leg.venue, st.units
                ));
            }

            let want = st.units / ct;
            let f_src = take_leg(ctx, &leg.venue, &src_inst, Side::Sell, sb, sd, want, src_ct).await;
            if f_src.contracts <= 0.0 {
                return Err("conversion source sell missed".into());
            }
            let f_tgt = take_leg(ctx, &leg.venue, &leg.instrument, Side::Buy, ta, td, want, ct).await;
            if f_tgt.contracts <= 0.0 {
                // Source sold but target not bought: buy the source back —
                // the conversion simply did not happen.
                let undo = take_leg(ctx, &leg.venue, &src_inst, Side::Buy, sb * 1.01, sd, f_src.contracts, src_ct).await;
                st.cash += undo.contracts * src_ct * (sb - undo.px_unit) - undo.fee_usd;
                st.fees += undo.fee_usd;
                return Err("conversion target buy missed (source restored)".into());
            }
            let done_units = (f_src.contracts * src_ct).min(f_tgt.contracts * ct);
            // Perp hedge: selling the call and buying the put shifts delta
            // by −1 per unit (parity) — sell perp to stay flat through the hop.
            let (hpx, hfee) = ctx.router.load_full().hedge(&leg.venue, &ctx.route.underlying, Side::Sell, done_units, ctx.spot).await;
            st.hedge.trade(Side::Sell, done_units, hpx);
            st.cash += -hfee;
            st.fees += hfee;

            let fee = f_src.fee_usd + f_tgt.fee_usd + hfee;
            st.cash += f_src.contracts * src_ct * f_src.px_unit - f_tgt.contracts * ct * f_tgt.px_unit - fee;
            st.fees += f_src.fee_usd + f_tgt.fee_usd;
            *st.inv.entry((leg.venue.clone(), right_of(&src_inst))).or_insert(0.0) -= f_src.contracts * src_ct;
            *st.inv.entry((leg.venue.clone(), right_of(&leg.instrument))).or_insert(0.0) += f_tgt.contracts * ct;
            st.units = st.units.min(done_units);
            st.done.push(TradeLeg {
                venue: leg.venue.clone(),
                instrument: leg.instrument.clone(),
                action: "convert".into(),
                contracts: f_tgt.contracts,
                contract_size: ct,
                px_unit: f_tgt.px_unit - f_src.px_unit,
                fee_usd: fee,
            });
            ctx.ev(
                ExecLevel::Ok,
                "route/leg",
                format!(
                    "leg {}/{}: convert {} → {} on {} @ {:.0}−{:.0} ({:.2} u, perp hedged)",
                    k, n, short_inst(&src_inst), short_inst(&leg.instrument), leg.venue,
                    f_src.px_unit, f_tgt.px_unit, done_units
                ),
            );
            Ok(())
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn apply_fill(
    ctx: &RouteCtx,
    st: &mut ExecState,
    leg: &MultiHopLeg,
    action: &str,
    f: Fill,
    ct: f64,
    _touch: f64,
    _depth: f64,
    k: usize,
    n: usize,
) -> Result<(), String> {
    let units = f.contracts * ct;
    let sign = if action == "buy" { -1.0 } else { 1.0 };
    st.cash += sign * units * f.px_unit - f.fee_usd;
    st.fees += f.fee_usd;
    *st.inv.entry((leg.venue.clone(), right_of(&leg.instrument))).or_insert(0.0) += if action == "buy" { units } else { -units };
    st.units = st.units.min(units);
    st.done.push(TradeLeg {
        venue: leg.venue.clone(),
        instrument: leg.instrument.clone(),
        action: action.into(),
        contracts: f.contracts,
        contract_size: ct,
        px_unit: f.px_unit,
        fee_usd: f.fee_usd,
    });
    let slip = if action == "buy" { f.px_unit - leg.px_unit } else { leg.px_unit - f.px_unit };
    ctx.ev(
        ExecLevel::Ok,
        "route/leg",
        format!(
            "leg {}/{}: {} {:.2} u of {} on {} @ {} (latency {} ms, slip {:+.2}/u)",
            k, n, action, units, short_inst(&leg.instrument), leg.venue,
            fmt_usd(f.px_unit), f.latency_ms, slip
        ),
    );
    Ok(())
}

fn short_inst(inst: &str) -> String {
    // "BTC-260930-84000-C" → "84000-C"
    inst.rsplit_once('-')
        .and_then(|(head, r)| head.rsplit_once('-').map(|(_, s)| format!("{s}-{r}")))
        .unwrap_or_else(|| inst.to_string())
}

fn fmt_usd(x: f64) -> String {
    if x.abs() >= 1000.0 {
        format!("${x:.0}")
    } else {
        format!("${x:.2}")
    }
}

/// Projected profit if the route completed right now: cash so far + live
/// cost of every remaining leg (fallback to the route print when a quote is
/// missing — the per-leg execution re-checks anyway).
fn projected(ctx: &RouteCtx, st: &ExecState, remaining: &[usize]) -> f64 {
    let mut p = st.cash;
    for &i in remaining {
        let leg = &ctx.route.legs[i];
        let q = ctx.quote(&leg.instrument, &leg.venue);
        let ll = price_leg(leg, q.as_ref(), ctx.fee_rate(&leg.venue), ctx.spot);
        p -= ll.live_cost_unit * st.units;
    }
    p
}

/// Estimated cost of unwinding everything already filled: half-spread plus
/// fees per executed leg (the spread we cross to get out).
fn est_unwind_cost(ctx: &RouteCtx, st: &ExecState) -> f64 {
    let mut c = 0.0f64;
    for leg in &st.done {
        let q = ctx.quote(&leg.instrument, &leg.venue);
        let spread_unit = match q {
            Some(q) => match (q.px_bid_unit, q.px_ask_unit) {
                (Some(b), Some(a)) if a > b => (a - b) * 0.5,
                _ => leg.px_unit.abs() * 0.005,
            },
            None => leg.px_unit.abs() * 0.005,
        };
        c += (spread_unit + ctx.fee_rate(&leg.venue) * ctx.spot) * leg.units();
    }
    c
}

/// Final settlement: flatten any cross-venue net imbalance left by partial
/// fills. `buy C@A + sell C@B` is flat at expiry (cells ±, net 0) — only a
/// NET residual per right needs a market trade, executed on the venue
/// carrying the offsetting position.
async fn settle(ctx: &RouteCtx, st: &mut ExecState) {
    for right in ['C', 'P'] {
        let cells: Vec<((String, char), f64)> = st
            .inv
            .iter()
            .filter(|((_, r), u)| *r == right && u.abs() > 1e-6)
            .map(|(k, v)| (k.clone(), *v))
            .collect();
        let net: f64 = cells.iter().map(|(_, u)| u).sum();
        if net.abs() <= 1e-6 || cells.is_empty() {
            continue;
        }
        // Long residual → sell where we are longest; short → buy back where
        // we are most short.
        let ((venue, _), _) = if net > 0.0 {
            cells.iter().max_by(|a, b| a.1.partial_cmp(&b.1).unwrap()).unwrap()
        } else {
            cells.iter().min_by(|a, b| a.1.partial_cmp(&b.1).unwrap()).unwrap()
        };
        let inst = ctx
            .route
            .legs
            .iter()
            .map(|l| l.instrument.clone())
            .chain(st.done.iter().map(|l| l.instrument.clone()))
            .find(|i| right_of(i) == right)
            .unwrap_or_default();
        if inst.is_empty() {
            continue;
        }
        let q = ctx.quote(&inst, venue);
        let ct = q.as_ref().and_then(|q| (q.contract_size > 0.0).then_some(q.contract_size)).unwrap_or(1.0);
        // Reference price for degraded fallbacks: a previously transacted
        // leg on this venue beats any fantasy print.
        let ref_px = st
            .done
            .iter()
            .find(|l| l.venue == *venue && l.instrument == inst)
            .map(|l| l.px_unit)
            .or_else(|| ctx.route.legs.iter().find(|l| l.venue == *venue && l.instrument == inst).map(|l| l.px_unit.abs()))
            .filter(|p| *p > 0.0);
        let (side, touch, depth) = if net > 0.0 {
            let Some((b, d)) = q
                .and_then(|q| q.px_bid_unit.zip(q.size_bid))
                .filter(|(p, s)| *p > 0.0 && *s > 0.0)
                .or_else(|| ref_px.map(|r| (r * 0.99, net)))
            else {
                ctx.ev(ExecLevel::Error, "route/settle", format!("cannot settle +{net:.2} u residual on {venue} — no bid and no reference price"));
                continue;
            };
            (Side::Sell, b, d)
        } else {
            let Some((a, d)) = q
                .and_then(|q| q.px_ask_unit.zip(q.size_ask))
                .filter(|(p, s)| *p > 0.0 && *s > 0.0)
                .or_else(|| ref_px.map(|r| (r * 1.01, -net)))
            else {
                ctx.ev(ExecLevel::Error, "route/settle", format!("cannot settle {net:.2} u residual on {venue} — no ask and no reference price"));
                continue;
            };
            (Side::Buy, a, d)
        };
        let contracts = net.abs() / ct;
        let f = take_leg(ctx, venue, &inst, side, touch, depth, contracts, ct).await;
        if f.contracts > 0.0 {
            let units = f.contracts * ct;
            let sign = if side == Side::Sell { 1.0 } else { -1.0 };
            st.cash += sign * units * f.px_unit - f.fee_usd;
            st.fees += f.fee_usd;
            st.done.push(TradeLeg {
                venue: venue.clone(),
                instrument: inst.clone(),
                action: "settle".into(),
                contracts: f.contracts,
                contract_size: ct,
                px_unit: f.px_unit,
                fee_usd: f.fee_usd,
            });
            *st.inv.entry((venue.clone(), right)).or_insert(0.0) += if side == Side::Sell { -units } else { units };
            ctx.ev(
                ExecLevel::Warn,
                "route/settle",
                format!(
                    "partial-fill imbalance settled: {} {:.2} u of {} on {} @ {}",
                    if side == Side::Sell { "sold" } else { "bought back" },
                    units, short_inst(&inst), venue, fmt_usd(f.px_unit)
                ),
            );
        }
    }
    // Close any perp carried through conversions.
    if st.hedge.units.abs() > 1e-9 {
        let venue = st
            .done
            .iter()
            .find(|l| l.action == "convert")
            .map(|l| l.venue.clone())
            .unwrap_or_else(|| ctx.route.legs[0].venue.clone());
        let side = if st.hedge.units > 0.0 { Side::Sell } else { Side::Buy };
        let (px, fee) = ctx
            .router
            .load_full()
            .hedge(&venue, &ctx.route.underlying, side, st.hedge.units.abs(), ctx.spot)
            .await;
        st.hedge.trade(side, st.hedge.units.abs(), px);
        st.cash -= fee;
        st.fees += fee;
        st.hedge.realized_usd -= fee;
        ctx.ev(
            ExecLevel::Info,
            "route/settle",
            format!("perp hedge closed on {} ({:.2} u) — flat", venue, st.hedge.units.abs()),
        );
    }
}

/// Reverse every executed leg in strict reverse order (the generalized
/// 10-second rule): buys are sold back, sells bought back, conversions
/// reversed with the perp restored.
async fn unwind_all(ctx: &RouteCtx, st: &mut ExecState, reason: &str) -> RouteOutcome {
    ctx.ev(ExecLevel::Error, "route/unwind", format!("cycle broke: {reason} — unwinding {} filled leg(s) in reverse order", st.done.len()));
    for leg in st.done.iter().rev() {
        let q = ctx.quote(&leg.instrument, &leg.venue);
        let ct = if leg.contract_size > 0.0 { leg.contract_size } else { 1.0 };
        let src_inst = flip_right(&leg.instrument);
        match leg.action.as_str() {
            "buy" => {
                let (touch, depth) = q
                    .and_then(|q| q.px_bid_unit.zip(q.size_bid))
                    .filter(|(p, s)| *p > 0.0 && *s > 0.0)
                    .unwrap_or((leg.px_unit * 0.99, leg.units()));
                let f = take_leg(ctx, &leg.venue, &leg.instrument, Side::Sell, touch, depth, leg.contracts, ct).await;
                if f.contracts > 0.0 {
                    st.cash += f.contracts * ct * f.px_unit - f.fee_usd;
                    st.fees += f.fee_usd;
                    *st.inv.entry((leg.venue.clone(), right_of(&leg.instrument))).or_insert(0.0) -= f.contracts * ct;
                }
            }
            "sell" => {
                let (touch, depth) = q
                    .and_then(|q| q.px_ask_unit.zip(q.size_ask))
                    .filter(|(p, s)| *p > 0.0 && *s > 0.0)
                    .unwrap_or((leg.px_unit * 1.01, leg.units()));
                let f = take_leg(ctx, &leg.venue, &leg.instrument, Side::Buy, touch, depth, leg.contracts, ct).await;
                if f.contracts > 0.0 {
                    st.cash -= f.contracts * ct * f.px_unit + f.fee_usd;
                    st.fees += f.fee_usd;
                    *st.inv.entry((leg.venue.clone(), right_of(&leg.instrument))).or_insert(0.0) += f.contracts * ct;
                }
            }
            "convert" => {
                // Reverse: sell the acquired right, buy back the source,
                // restore the perp.
                let sq = ctx.quote(&src_inst, &leg.venue);
                let sct = sq.as_ref().and_then(|q| (q.contract_size > 0.0).then_some(q.contract_size)).unwrap_or(ct);
                let (tb, td) = q
                    .and_then(|q| q.px_bid_unit.zip(q.size_bid))
                    .filter(|(p, s)| *p > 0.0 && *s > 0.0)
                    .unwrap_or((leg.px_unit.abs().max(1.0) * 0.99, leg.units()));
                let (sa, sd) = sq
                    .and_then(|q| q.px_ask_unit.zip(q.size_ask))
                    .filter(|(p, s)| *p > 0.0 && *s > 0.0)
                    .unwrap_or((1.0, leg.units()));
                let f_tgt = take_leg(ctx, &leg.venue, &leg.instrument, Side::Sell, tb, td, leg.contracts, ct).await;
                let f_src = take_leg(ctx, &leg.venue, &src_inst, Side::Buy, sa, sd, leg.contracts, sct).await;
                if f_tgt.contracts > 0.0 && f_src.contracts > 0.0 {
                    let u = (f_tgt.contracts * ct).min(f_src.contracts * sct);
                    let fee = f_tgt.fee_usd + f_src.fee_usd;
                    st.cash += f_tgt.contracts * ct * f_tgt.px_unit - f_src.contracts * sct * f_src.px_unit - fee;
                    st.fees += fee;
                    *st.inv.entry((leg.venue.clone(), right_of(&src_inst))).or_insert(0.0) += f_src.contracts * sct;
                    *st.inv.entry((leg.venue.clone(), right_of(&leg.instrument))).or_insert(0.0) -= f_tgt.contracts * ct;
                    let (hpx, hfee) = ctx.router.load_full().hedge(&leg.venue, &ctx.route.underlying, Side::Buy, u, ctx.spot).await;
                    st.hedge.trade(Side::Buy, u, hpx);
                    st.cash -= hfee;
                    st.fees += hfee;
                }
            }
            // settle legs are already-flat residual trades; nothing to undo.
            _ => {}
        }
    }
    settle(ctx, st).await;
    let profit = st.cash + st.hedge.mark_pnl(ctx.spot);
    ctx.ev(
        ExecLevel::Warn,
        "route/unwind",
        format!(
            "unwind complete — flat, no exposure left; P&L impact {:+.2} USD (fees {:.2})",
            profit, st.fees
        ),
    );
    RouteOutcome::Unwound {
        reason: reason.to_string(),
        profit_usd: profit,
        fees_usd: st.fees,
        hedge_pnl_usd: st.hedge.mark_pnl(ctx.spot),
        units: st.units,
        legs: st.done.clone(),
    }
}

/// Run one multi-leg route cycle to completion (banked), abort or unwind.
pub async fn run_route(ctx: RouteCtx, units: f64) -> RouteOutcome {
    let n = ctx.route.legs.len();
    let chain = ctx
        .route
        .legs
        .iter()
        .map(|l| l.venue.as_str())
        .collect::<Vec<_>>()
        .join(">");
    ctx.ev(
        ExecLevel::Info,
        "route/preflight",
        format!(
            "ROUTE {} {} — {} legs via {}, projected {:+.2} USD on {:.2} u (quality {:.0}%, quotes {} old)",
            ctx.route.underlying,
            short_inst(&ctx.route.legs[0].instrument),
            n,
            chain,
            ctx.route.total_usd,
            units,
            100.0 * ctx.route.quality,
            fmt_dur(ctx.route.max_age_ms),
        ),
    );

    // Signal re-validation: the route must still exist and must not have
    // decayed by more than 40 % since the scanner printed it.
    {
        let snap = ctx.snapshot.load();
        match snap.multihop.iter().find(|r| r.id == ctx.route.id) {
            None => {
                ctx.ev(ExecLevel::Warn, "route/abort", "route vanished before fire".into());
                return RouteOutcome::Aborted { reason: "route vanished".into(), cooldown_ms: 20_000 };
            }
            Some(r) => {
                if r.profit_unit_usd < ctx.route.profit_unit_usd * 0.60 {
                    ctx.ev(
                        ExecLevel::Warn,
                        "route/abort",
                        format!(
                            "route decayed: {:+.2} → {:+.2} USD/unit before fire",
                            ctx.route.profit_unit_usd, r.profit_unit_usd
                        ),
                    );
                    return RouteOutcome::Aborted { reason: "signal decayed".into(), cooldown_ms: 20_000 };
                }
            }
        }
    }

    let Some(order) = plan_order(&ctx.route) else {
        ctx.ev(ExecLevel::Warn, "route/abort", "no feasible execution order".into());
        return RouteOutcome::Aborted { reason: "infeasible order".into(), cooldown_ms: 60_000 };
    };

    // Wait briefly for the first leg's quotes (watch registration latency).
    let first = &ctx.route.legs[order[0]];
    if ctx
        .quote_with_retry(&first.instrument, &first.venue, 4)
        .await
        .is_none()
    {
        ctx.ev(ExecLevel::Warn, "route/abort", "first leg quotes never arrived".into());
        return RouteOutcome::Aborted { reason: "quotes missing".into(), cooldown_ms: 15_000 };
    }

    let mut st = ExecState {
        inv: FxHashMap::default(),
        done: Vec::new(),
        cash: 0.0,
        fees: 0.0,
        units,
        hedge: Hedge::default(),
        started: std::time::Instant::now(),
    };
    let hard_deadline = Duration::from_secs_f64((ctx.settings.leg_timeout_s * n as f64 + 6.0).max(12.0));

    for (step, &li) in order.iter().enumerate() {
        if st.started.elapsed() > hard_deadline {
            return unwind_all(&ctx, &mut st, "hard timeout").await;
        }
        // Mid-flight abandonment: completing must beat unwinding.
        if step > 0 && !st.done.is_empty() {
            let remaining: Vec<usize> = order[step..].to_vec();
            let proj = projected(&ctx, &st, &remaining);
            let unw = est_unwind_cost(&ctx, &st);
            if proj + unw < 0.0 {
                return unwind_all(
                    &ctx,
                    &mut st,
                    &format!("projected {:+.2} USD < unwind cost {:.2} — cycle broke mid-flight", proj, unw),
                )
                .await;
            }
            if proj < 0.0 && step + 1 < n {
                ctx.ev(
                    ExecLevel::Warn,
                    "route/decay",
                    format!("projected completion {:+.2} USD — completing anyway (cheaper than unwinding at {:.2})", proj, unw),
                );
            }
        }
        let leg = ctx.route.legs[li].clone();
        if let Err(reason) = exec_leg(&ctx, &mut st, &leg, step + 1, n).await {
            if st.done.is_empty() {
                ctx.ev(ExecLevel::Warn, "route/abort", format!("first leg failed: {reason}"));
                return RouteOutcome::Aborted { reason, cooldown_ms: 30_000 };
            }
            return unwind_all(&ctx, &mut st, &reason).await;
        }
    }

    settle(&ctx, &mut st).await;
    let hedge_pnl = st.hedge.mark_pnl(ctx.spot);
    let profit = st.cash + hedge_pnl;
    ctx.ev(
        ExecLevel::Ok,
        "route/banked",
        format!(
            "route banked {:+.2} USD in {} — {} legs flat, fees {:.2}, cycle size {:.2} u",
            profit,
            fmt_dur(st.started.elapsed().as_millis() as i64),
            st.done.len(),
            st.fees,
            st.units
        ),
    );
    RouteOutcome::Banked { profit_usd: profit, fees_usd: st.fees, hedge_pnl_usd: hedge_pnl, units: st.units, legs: st.done }
}

fn fmt_dur(ms: i64) -> String {
    if ms < 1200 {
        format!("{ms} ms")
    } else {
        format!("{:.1}s", ms as f64 / 1000.0)
    }
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use ivarb_protocol::MultiHopLeg;

    fn wl(bid: f64, ask: f64, sz: f64, ct: f64) -> WatchedLeg {
        WatchedLeg {
            venue: String::new(),
            px_bid_unit: (bid > 0.0).then_some(bid),
            px_ask_unit: (ask > 0.0).then_some(ask),
            px_mark_unit: Some((bid + ask) / 2.0),
            iv_bid_vp: None,
            iv_ask_vp: None,
            iv_mark_vp: Some(55.0),
            size_bid: (bid > 0.0).then_some(sz),
            size_ask: (ask > 0.0).then_some(sz),
            contract_size: ct,
            recv_ms: now_ms(),
        }
    }

    fn leg(action: &str, venue: &str, inst: &str, px: f64, fee: f64, size: f64) -> MultiHopLeg {
        MultiHopLeg {
            action: action.into(),
            venue: venue.into(),
            instrument: inst.into(),
            px_unit: px,
            fee_unit: fee,
            size_units: size,
        }
    }

    fn ctx_for(route: MultiHopRoute, quotes: Vec<(&str, &str, WatchedLeg)>) -> (RouteCtx, Arc<PaperBroker>) {
        let broker = Arc::new(PaperBroker::new(&["okx".into(), "bybit".into(), "deribit".into()], 1_000_000.0));
        let mut watched: FxHashMap<String, Vec<WatchedLeg>> = FxHashMap::default();
        for (inst, venue, q) in quotes {
            let mut q = q;
            q.venue = venue.to_string();
            watched.entry(inst.to_string()).or_default().push(q);
        }
        let watched = Arc::new(ArcSwap::from_pointee(watched));
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let snap = Arc::new(ArcSwap::from_pointee(Snapshot::default()));
        let mut fee_rates = FxHashMap::default();
        fee_rates.insert("okx".to_string(), 0.0003);
        fee_rates.insert("bybit".to_string(), 0.0003);
        fee_rates.insert("deribit".to_string(), 0.0003);
        let router = Arc::new(ArcSwap::from_pointee(crate::gateways::VenueRouter::new(
            broker.clone(),
            false, // tests run the paper path
        )));
        (
            RouteCtx {
                attempt: 1,
                route,
                spot: 84_000.0,
                settings: ExecSettings::default(),
                broker: broker.clone(),
                router,
                watched,
                snapshot: snap,
                events: tx,
                fee_rates,
            },
            broker,
        )
    }

    fn spot_snapshot_with(route: &MultiHopRoute) -> Arc<ArcSwap<Snapshot>> {
        let mut snap = Snapshot::default();
        snap.multihop = vec![route.clone()];
        Arc::new(ArcSwap::from_pointee(snap))
    }

    #[test]
    fn plan_order_respects_convert_dependency_and_thin_first() {
        // buy C okx (thin), buy C bybit (fat), convert on okx, sell P bybit.
        let route = MultiHopRoute {
            id: "t".into(),
            underlying: "BTC".into(),
            expiry: "2026-12-30".into(),
            strike: 84_000.0,
            legs: vec![
                leg("buy", "bybit", "BTC-261230-84000-C", 5_900.0, 25.0, 5.0),
                leg("buy", "okx", "BTC-261230-84000-C", 5_900.0, 25.0, 1.0),
                leg("convert", "okx", "BTC-261230-84000-P", 0.0, 10.0, 1.0),
                leg("sell", "bybit", "BTC-261230-84000-P", 6_100.0, 25.0, 5.0),
            ],
            profit_unit_usd: 100.0,
            profit_vp: 1.0,
            size_units: 1.0,
            total_usd: 100.0,
            quality: 1.0,
            max_age_ms: 0,
            updated_ms: now_ms(),
        };
        let order = plan_order(&route).expect("feasible");
        // Thin okx buy (size 1) must precede the fat bybit buy (size 5);
        // the okx convert must come after the okx buy.
        let pos = |venue: &str, action: &str| order.iter().position(|&i| {
            route.legs[i].venue == venue && route.legs[i].action == action
        }).unwrap();
        assert!(pos("okx", "buy") < pos("bybit", "buy"), "thinner leg first: {order:?}");
        assert!(pos("okx", "buy") < pos("okx", "convert"), "convert after its source buy: {order:?}");
        assert_eq!(order.len(), 4);
    }

    #[test]
    fn plan_order_infeasible_when_convert_source_unbuyable() {
        let route = MultiHopRoute {
            id: "t".into(),
            underlying: "BTC".into(),
            expiry: "2026-12-30".into(),
            strike: 84_000.0,
            legs: vec![
                leg("convert", "okx", "BTC-261230-84000-P", 0.0, 10.0, 1.0),
                leg("sell", "bybit", "BTC-261230-84000-P", 6_100.0, 25.0, 5.0),
            ],
            profit_unit_usd: 100.0,
            profit_vp: 1.0,
            size_units: 1.0,
            total_usd: 100.0,
            quality: 1.0,
            max_age_ms: 0,
            updated_ms: now_ms(),
        };
        assert!(plan_order(&route).is_none(), "convert with no source buy is infeasible");
    }

    #[tokio::test]
    async fn two_venue_cycle_banks_profit() {
        // okx asks 5 900, bybit bids 6 050 → ≈ +99.6/unit net of fees.
        let route = MultiHopRoute {
            id: "BTC-261230-84000:MHbuy okx>sell bybit".into(),
            underlying: "BTC".into(),
            expiry: "2026-12-30".into(),
            strike: 84_000.0,
            legs: vec![
                leg("buy", "okx", "BTC-261230-84000-C", 5_900.0, 25.2, 2.0),
                leg("sell", "bybit", "BTC-261230-84000-C", 6_050.0, 25.2, 2.0),
            ],
            profit_unit_usd: 99.6,
            profit_vp: 1.0,
            size_units: 2.0,
            total_usd: 199.2,
            quality: 1.0,
            max_age_ms: 0,
            updated_ms: now_ms(),
        };
        let (mut ctx, broker) = ctx_for(
            route.clone(),
            vec![
                ("BTC-261230-84000-C", "okx", wl(5_880.0, 5_900.0, 2.0, 1.0)),
                ("BTC-261230-84000-C", "bybit", wl(6_050.0, 6_070.0, 2.0, 1.0)),
            ],
        );
        ctx.snapshot = spot_snapshot_with(&route);
        let out = run_route(ctx, 2.0).await;
        match out {
            RouteOutcome::Banked { profit_usd, legs, .. } => {
                assert!(profit_usd > 50.0 && profit_usd < 220.0, "profit {profit_usd}");
                assert_eq!(legs.len(), 2, "two legs executed");
                assert_eq!(legs[0].action, "buy");
                assert_eq!(legs[1].action, "sell");
            }
            other => panic!("expected banked, got {other:?}"),
        }
        // Balances moved: okx paid, bybit received.
        let okx = broker.balance_of("okx");
        let bybit = broker.balance_of("bybit");
        assert!(okx < 1_000_000.0, "okx paid premium: {okx}");
        assert!(bybit > 1_000_000.0, "bybit received premium: {bybit}");
    }

    #[tokio::test]
    async fn missing_second_leg_quotes_trigger_unwind() {
        // Route expects a bybit bid, but bybit publishes no book for the inst.
        let route = MultiHopRoute {
            id: "BTC-261230-84000:MHbuy okx>sell bybit".into(),
            underlying: "BTC".into(),
            expiry: "2026-12-30".into(),
            strike: 84_000.0,
            legs: vec![
                leg("buy", "okx", "BTC-261230-84000-C", 5_900.0, 25.2, 2.0),
                leg("sell", "bybit", "BTC-261230-84000-C", 6_050.0, 25.2, 2.0),
            ],
            profit_unit_usd: 99.6,
            profit_vp: 1.0,
            size_units: 2.0,
            total_usd: 199.2,
            quality: 1.0,
            max_age_ms: 0,
            updated_ms: now_ms(),
        };
        let (mut ctx, _broker) = ctx_for(
            route.clone(),
            vec![("BTC-261230-84000-C", "okx", wl(5_880.0, 5_900.0, 2.0, 1.0))],
        );
        ctx.snapshot = spot_snapshot_with(&route);
        let out = run_route(ctx, 2.0).await;
        match out {
            RouteOutcome::Unwound { reason, legs, .. } => {
                assert!(reason.contains("quotes") || reason.contains("bid"), "reason: {reason}");
                assert!(!legs.is_empty(), "unwind legs recorded");
            }
            other => panic!("expected unwound, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn mid_flight_decay_abandons_rather_than_completes() {
        // Route print says +99.6/unit, but the live bybit bid collapsed to
        // 5 700 → completing loses ~400 on 2 u; unwinding the okx buy costs
        // ~half-spread + fee ≈ 130. The sequencer must unwind, not finish.
        let route = MultiHopRoute {
            id: "BTC-261230-84000:MHbuy okx>sell bybit".into(),
            underlying: "BTC".into(),
            expiry: "2026-12-30".into(),
            strike: 84_000.0,
            legs: vec![
                leg("buy", "okx", "BTC-261230-84000-C", 5_900.0, 25.2, 2.0),
                leg("sell", "bybit", "BTC-261230-84000-C", 6_050.0, 25.2, 2.0),
            ],
            profit_unit_usd: 99.6,
            profit_vp: 1.0,
            size_units: 2.0,
            total_usd: 199.2,
            quality: 1.0,
            max_age_ms: 0,
            updated_ms: now_ms(),
        };
        let (mut ctx, _broker) = ctx_for(
            route.clone(),
            vec![
                ("BTC-261230-84000-C", "okx", wl(5_880.0, 5_900.0, 2.0, 1.0)),
                ("BTC-261230-84000-C", "bybit", wl(5_700.0, 5_720.0, 2.0, 1.0)),
            ],
        );
        ctx.snapshot = spot_snapshot_with(&route);
        let out = run_route(ctx, 2.0).await;
        match out {
            RouteOutcome::Unwound { reason, .. } => {
                assert!(reason.contains("mid-flight") || reason.contains("projected"), "reason: {reason}");
            }
            RouteOutcome::Banked { profit_usd, .. } => panic!("must not complete a broken cycle (banked {profit_usd})"),
            other => panic!("expected unwound, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn conversion_route_executes_with_perp() {
        // buy C okx → convert okx C→P → sell P bybit.
        let route = MultiHopRoute {
            id: "BTC-261230-84000:MHbuy okx>convert okx>sell bybit".into(),
            underlying: "BTC".into(),
            expiry: "2026-12-30".into(),
            strike: 84_000.0,
            legs: vec![
                leg("buy", "okx", "BTC-261230-84000-C", 5_800.0, 25.2, 1.0),
                leg("convert", "okx", "BTC-261230-84000-P", -150.0, 110.0, 1.0),
                leg("sell", "bybit", "BTC-261230-84000-P", 6_300.0, 25.2, 1.0),
            ],
            profit_unit_usd: 50.0,
            profit_vp: 0.5,
            size_units: 1.0,
            total_usd: 50.0,
            quality: 1.0,
            max_age_ms: 0,
            updated_ms: now_ms(),
        };
        let (mut ctx, broker) = ctx_for(
            route.clone(),
            vec![
                ("BTC-261230-84000-C", "okx", wl(5_780.0, 5_800.0, 1.0, 1.0)),
                ("BTC-261230-84000-P", "okx", wl(5_500.0, 5_530.0, 1.0, 1.0)),
                ("BTC-261230-84000-P", "bybit", wl(6_300.0, 6_330.0, 1.0, 1.0)),
            ],
        );
        ctx.snapshot = spot_snapshot_with(&route);
        let out = run_route(ctx, 1.0).await;
        match out {
            RouteOutcome::Banked { profit_usd, legs, hedge_pnl_usd, .. } => {
                // Roughly: −5800 (buy C) + 5500 (sell C in convert) − 5530
                // (buy P) + 6300 (sell P) − fees ≈ +400.
                assert!(profit_usd > 100.0, "profit {profit_usd}");
                assert!(legs.iter().any(|l| l.action == "convert"), "conversion leg executed");
                assert!(hedge_pnl_usd.abs() < 50.0, "perp closed ~flat: {hedge_pnl_usd}");
            }
            other => panic!("expected banked, got {other:?}"),
        }
        let _ = broker;
    }

    #[tokio::test]
    async fn decayed_route_aborts_before_fire() {
        let route = MultiHopRoute {
            id: "BTC-261230-84000:MHbuy okx>sell bybit".into(),
            underlying: "BTC".into(),
            expiry: "2026-12-30".into(),
            strike: 84_000.0,
            legs: vec![
                leg("buy", "okx", "BTC-261230-84000-C", 5_900.0, 25.2, 2.0),
                leg("sell", "bybit", "BTC-261230-84000-C", 6_050.0, 25.2, 2.0),
            ],
            profit_unit_usd: 99.6,
            profit_vp: 1.0,
            size_units: 2.0,
            total_usd: 199.2,
            quality: 1.0,
            max_age_ms: 0,
            updated_ms: now_ms(),
        };
        // Snapshot holds a DECAYED version of the same route.
        let mut decayed = route.clone();
        decayed.profit_unit_usd = 10.0;
        let (mut ctx, _broker) = ctx_for(
            route.clone(),
            vec![
                ("BTC-261230-84000-C", "okx", wl(5_880.0, 5_900.0, 2.0, 1.0)),
                ("BTC-261230-84000-C", "bybit", wl(6_050.0, 6_070.0, 2.0, 1.0)),
            ],
        );
        let mut snap = Snapshot::default();
        snap.multihop = vec![decayed];
        ctx.snapshot = Arc::new(ArcSwap::from_pointee(snap));
        match run_route(ctx, 2.0).await {
            RouteOutcome::Aborted { reason, .. } => assert!(reason.contains("decay"), "{reason}"),
            other => panic!("expected abort, got {other:?}"),
        }
    }
}
