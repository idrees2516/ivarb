//! The option builder: turn a market view + risk budget into a concrete,
//! live-data-grounded option structure. Every strike, expiry, venue and
//! premium comes from the consolidated cross-venue book; the greeks come
//! from the same Black-Scholes core the arb engine uses.

use ivarb_protocol::research::{
    CandleSeries, ChainRow, MarketView, RiskAppetite, SuggestGreeks, SuggestLeg,
    SuggestionCard, CoherenceBreakdown, LegAction,
};
use ivarb_protocol::Right;

use ivarb_quant::{bs_delta, bs_theta, bs_vega, norm_pdf, d1_d2, QuantRight};

use crate::consolidate::{dte_of, iso_date, ymd_to_ms, ConsolidatedBook};

#[derive(Debug, Clone)]
pub struct SuggestRequest {
    pub und: String,
    pub view: MarketView,
    pub horizon_days: u32,
    pub risk: RiskAppetite,
}

/// Build a suggestion card from the live consolidated book.
pub fn build(
    book: &ConsolidatedBook,
    candles: Option<&CandleSeries>,
    req: &SuggestRequest,
    now_ms: i64,
) -> Result<SuggestionCard, String> {
    let und = req.und.to_uppercase();
    let spot = book
        .spots
        .get(&und)
        .copied()
        .filter(|s| s.is_finite() && *s > 0.0)
        .ok_or_else(|| format!("no live spot for {und}"))?;
    let rows: Vec<&ChainRow> = book
        .rows
        .iter()
        .filter(|r| r.und == und && r.dte >= 1)
        .collect();
    if rows.is_empty() {
        return Err(format!("no live option chain for {und}"));
    }

    // ---- market readout ----
    let summ = book.iv.iter().find(|s| s.und == und);
    let iv_pct = summ.and_then(|s| s.iv_percentile);
    let vr = summ.and_then(|s| s.vr_premium_vp);
    let term_slope = summ.and_then(|s| s.term_slope_vp);
    let readout = candles.map(|c| c.readout.clone());
    let trend = readout.as_ref().map(|r| r.ema_trend.clone()).unwrap_or_else(|| "flat".into());
    let rsi = readout.as_ref().and_then(|r| r.rsi14);

    let regime = iv_regime(vr, iv_pct);
    let regime_txt = match regime {
        Regime::Rich => "rich",
        Regime::Cheap => "cheap",
        Regime::Fair => "fair",
    };
    let _ = regime_txt;

    // ---- expiry: first listed date with dte >= horizon * 0.8 ----
    let want_dte = (req.horizon_days as f64 * 0.8).ceil().max(1.0) as i32;
    let dates: Vec<u32> = rows.iter().map(|r| r.date).collect::<std::collections::BTreeSet<_>>().into_iter().collect();
    let date = *dates
        .iter()
        .find(|d| dte_of(now_ms, **d) >= want_dte)
        .or_else(|| dates.last())
        .ok_or("no listed expiry")?;
    let dte = dte_of(now_ms, date);
    let t = ((ymd_to_ms(date).unwrap_or(now_ms) - now_ms).max(3_600_000) as f64 / 1000.0
        / (365.0 * 86400.0))
        .max(1e-4);

    // ---- strategy matrix ----
    let (strategy, spec) = pick_strategy(req.view, regime);
    let expiry_rows: Vec<&ChainRow> = rows.iter().filter(|r| r.date == date).copied().collect();

    // ---- strike selection ----
    let (short_delta_target, _long_delta_target) = delta_targets(req.risk);
    let legs = match spec {
        Spec::DebitSpread => {
            let long = pick_by_delta(&expiry_rows, Right::Call, 0.50, spot, t)?;
            let short = pick_by_delta(&expiry_rows, Right::Call, short_delta_target, spot, t)?;
            if short.strike <= long.strike {
                return Err("no suitable call spread strikes listed".into());
            }
            vec![
                leg(LegAction::Buy, long, spot, t)?,
                leg(LegAction::Sell, short, spot, t)?,
            ]
        }
        Spec::PutDebitSpread => {
            let long = pick_by_delta(&expiry_rows, Right::Put, 0.50, spot, t)?;
            let short = pick_by_delta(&expiry_rows, Right::Put, short_delta_target, spot, t)?;
            if short.strike >= long.strike {
                return Err("no suitable put spread strikes listed".into());
            }
            vec![
                leg(LegAction::Buy, long, spot, t)?,
                leg(LegAction::Sell, short, spot, t)?,
            ]
        }
        Spec::PutCreditSpread => {
            let (short, wing) = pick_short_with_wing(
                &expiry_rows,
                Right::Put,
                short_delta_target,
                spot,
                t,
                false,
            )
            .ok_or("no suitable put credit strikes listed")?;
            vec![
                leg(LegAction::Sell, short, spot, t)?,
                leg(LegAction::Buy, wing, spot, t)?,
            ]
        }
        Spec::CallCreditSpread => {
            let (short, wing) = pick_short_with_wing(
                &expiry_rows,
                Right::Call,
                short_delta_target,
                spot,
                t,
                true,
            )
            .ok_or("no suitable call credit strikes listed")?;
            vec![
                leg(LegAction::Sell, short, spot, t)?,
                leg(LegAction::Buy, wing, spot, t)?,
            ]
        }
        Spec::IronCondor => {
            let (ps, pw) = pick_short_with_wing(
                &expiry_rows,
                Right::Put,
                short_delta_target,
                spot,
                t,
                false,
            )
            .ok_or("no suitable iron condor put side")?;
            let (cs, cw) = pick_short_with_wing(
                &expiry_rows,
                Right::Call,
                short_delta_target,
                spot,
                t,
                true,
            )
            .ok_or("no suitable iron condor call side")?;
            vec![
                leg(LegAction::Sell, ps, spot, t)?,
                leg(LegAction::Buy, pw, spot, t)?,
                leg(LegAction::Sell, cs, spot, t)?,
                leg(LegAction::Buy, cw, spot, t)?,
            ]
        }
        Spec::Butterfly => {
            let body = nearest_strike(&expiry_rows, Right::Call, spot)?;
            let step = strike_step(&expiry_rows, Right::Call).max(spot * 0.005);
            let low = nearest_listed(&expiry_rows, Right::Call, body.strike - step)?;
            let high = nearest_listed(&expiry_rows, Right::Call, body.strike + step)?;
            if low.strike >= body.strike || high.strike <= body.strike {
                return Err("no suitable butterfly strikes listed".into());
            }
            vec![
                leg(LegAction::Buy, low, spot, t)?,
                leg(LegAction::Buy, body, spot, t)?, // replaced below with 2 units
                leg(LegAction::Buy, body, spot, t)?,
                leg(LegAction::Buy, high, spot, t)?,
            ]
        }
        Spec::Straddle => {
            let c = nearest_strike(&expiry_rows, Right::Call, spot)?;
            let p = nearest_strike(&expiry_rows, Right::Put, spot)?;
            vec![
                leg(LegAction::Buy, c, spot, t)?,
                leg(LegAction::Buy, p, spot, t)?,
            ]
        }
        Spec::Strangle => {
            let c = pick_by_delta(&expiry_rows, Right::Call, short_delta_target, spot, t)?;
            let p = pick_by_delta(&expiry_rows, Right::Put, short_delta_target, spot, t)?;
            vec![
                leg(LegAction::Buy, c, spot, t)?,
                leg(LegAction::Buy, p, spot, t)?,
            ]
        }
    };

    // Butterfly: fix the middle legs into sell-2.
    let mut legs = legs;
    if matches!(spec, Spec::Butterfly) {
        for (i, l) in legs.iter_mut().enumerate() {
            if i == 1 || i == 2 {
                l.action = LegAction::Sell;
            }
        }
    }

    // ---- premium / greeks ----
    let mut net = 0.0;
    let (mut delta, mut gamma, mut theta, mut vega) = (0.0, 0.0, 0.0, 0.0);
    for l in &legs {
        let sign = if matches!(l.action, LegAction::Buy) { 1.0 } else { -1.0 };
        net += sign * l.px;
        let right = match l.right {
            Right::Call => QuantRight::Call,
            Right::Put => QuantRight::Put,
        };
        let iv = l.iv_vp.unwrap_or(60.0).max(5.0) / 100.0;
        delta += sign * bs_delta(spot, l.strike, t, iv, right);
        gamma += sign * gamma_of(spot, l.strike, t, iv);
        theta += sign * bs_theta(spot, l.strike, t, iv) / 365.0;
        vega += sign * bs_vega(spot, l.strike, t, iv) / 100.0; // per vol point
    }
    let flow = if net > 1e-9 {
        "debit"
    } else if net < -1e-9 {
        "credit"
    } else {
        "zero-cost"
    };

    // ---- payoff envelope on a spot grid ----
    let (max_profit, max_loss, breakevens) = payoff_envelope(&legs, net, spot);

    // ---- coherence ----
    let coherence = coherence(
        req.view,
        regime,
        &trend,
        term_slope,
        &legs,
        spot,
        iv_pct,
        vr,
        rsi,
    );

    let mut notes = vec![
        format!(
            "Per 1 {} of underlying exposure; premiums are live cross-venue quotes routed to the best book.",
            und
        ),
        format!(
            "Struck on the {} expiry ({dte} DTE) — nearest listed tenor covering a {}-day horizon.",
            iso_date(date),
            req.horizon_days
        ),
        "Fees, slippage beyond the touch and margin requirements are not included; short legs carry assignment/margin risk.".to_string(),
    ];
    if book.data_mode != "live" {
        notes.push(format!(
            "Data mode is \"{}\" — numbers come from the simulated feed.",
            book.data_mode
        ));
    }

    Ok(SuggestionCard {
        strategy: strategy.to_string(),
        und: und.clone(),
        view: req.view,
        horizon_days: req.horizon_days,
        risk: req.risk,
        legs,
        flow: flow.to_string(),
        net_premium_usd: net,
        max_profit_usd: max_profit,
        max_loss_usd: max_loss,
        breakevens,
        greeks: SuggestGreeks {
            delta,
            gamma,
            theta_day: theta,
            vega_vp: vega,
        },
        coherence,
        notes,
        created_ms: now_ms,
        data_mode: book.data_mode.to_string(),
    })
}

// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum Regime {
    Rich,
    Cheap,
    Fair,
}

#[derive(Clone, Copy)]
enum Spec {
    DebitSpread,
    PutDebitSpread,
    PutCreditSpread,
    CallCreditSpread,
    IronCondor,
    Butterfly,
    Straddle,
    Strangle,
}

fn iv_regime(vr: Option<f64>, pct: Option<f64>) -> Regime {
    let rich = vr.map(|v| v >= 8.0).unwrap_or(false) || pct.map(|p| p >= 70.0).unwrap_or(false);
    let cheap = vr.map(|v| v <= -3.0).unwrap_or(false) || pct.map(|p| p <= 30.0).unwrap_or(false);
    if rich {
        Regime::Rich
    } else if cheap {
        Regime::Cheap
    } else {
        Regime::Fair
    }
}

fn pick_strategy(view: MarketView, regime: Regime) -> (&'static str, Spec) {
    match (view, regime) {
        (MarketView::Bullish, Regime::Rich) => ("Put Credit Spread", Spec::PutCreditSpread),
        (MarketView::Bullish, _) => ("Bull Call Spread", Spec::DebitSpread),
        (MarketView::Bearish, Regime::Rich) => ("Call Credit Spread", Spec::CallCreditSpread),
        (MarketView::Bearish, _) => ("Bear Put Spread", Spec::PutDebitSpread),
        (MarketView::NeutralRange, Regime::Cheap) => ("Long Call Butterfly", Spec::Butterfly),
        (MarketView::NeutralRange, _) => ("Iron Condor", Spec::IronCondor),
        (MarketView::BigMove, Regime::Rich) => ("Long Strangle", Spec::Strangle),
        (MarketView::BigMove, _) => ("Long Straddle", Spec::Straddle),
    }
}

/// (short/wing delta target, long delta target) by risk appetite.
fn delta_targets(risk: RiskAppetite) -> (f64, f64) {
    match risk {
        RiskAppetite::Conservative => (0.18, 0.50),
        RiskAppetite::Balanced => (0.25, 0.50),
        RiskAppetite::Aggressive => (0.35, 0.50),
    }
}

fn gamma_of(s: f64, k: f64, t: f64, sig: f64) -> f64 {
    if s <= 0.0 || k <= 0.0 || t <= 0.0 || sig <= 0.0 {
        return 0.0;
    }
    let (d1, _) = d1_d2(s, k, t, sig);
    norm_pdf(d1) / (s * sig * t.sqrt())
}

/// Row-level helper: delta of a chain row from its consensus IV.
fn row_delta(r: &ChainRow, spot: f64, t: f64) -> f64 {
    let iv = r.iv_consensus_vp.unwrap_or(60.0).max(5.0) / 100.0;
    let right = match r.right {
        Right::Call => QuantRight::Call,
        Right::Put => QuantRight::Put,
    };
    bs_delta(spot, r.strike, t, iv, right).abs()
}

fn liquidity_score(r: &ChainRow) -> f64 {
    let bid = r.best_bid_px.unwrap_or(0.0);
    let ask = r.best_ask_px.unwrap_or(0.0);
    let oi = r.open_interest.unwrap_or(0.0);
    let two_sided = if bid > 0.0 && ask > 0.0 { 1.0 } else { 0.0 };
    let spread_pen = if bid > 0.0 && ask > bid {
        ((ask - bid) / (0.5 * (ask + bid))).min(1.0)
    } else {
        1.0
    };
    two_sided * (1.0 - 0.5 * spread_pen) + (oi > 0.0) as usize as f64 * 0.5
}

/// Pick the listed row whose delta is nearest the target, liquidity-weighted.
fn pick_by_delta<'a>(
    rows: &[&'a ChainRow],
    right: Right,
    target_delta: f64,
    spot: f64,
    t: f64,
) -> Result<&'a ChainRow, String> {
    let mut best: Option<(&ChainRow, f64)> = None;
    for r in rows.iter().filter(|r| r.right == right) {
        if r.strike <= 0.0 {
            continue;
        }
        let d = row_delta(r, spot, t);
        if !(0.02..=0.98).contains(&d) {
            continue;
        }
        let dist = (d - target_delta).abs() - 0.03 * liquidity_score(r);
        if best.map(|(_, bd)| dist < bd).unwrap_or(true) {
            best = Some((r, dist));
        }
    }
    best.map(|(r, _)| r)
        .ok_or_else(|| format!("no listed {} strikes near delta {target_delta:.2}", right.letter()))
}

fn nearest_strike<'a>(rows: &[&'a ChainRow], right: Right, spot: f64) -> Result<&'a ChainRow, String> {
    rows.iter()
        .copied()
        .filter(|r| r.right == right)
        .min_by(|a, b| {
            let da = (a.strike - spot).abs();
            let db = (b.strike - spot).abs();
            da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
        })
        .ok_or_else(|| "empty chain".into())
}

fn nearest_listed<'a>(
    rows: &[&'a ChainRow],
    right: Right,
    target: f64,
) -> Result<&'a ChainRow, String> {
    rows.iter()
        .copied()
        .filter(|r| r.right == right)
        .min_by(|a, b| {
            let da = (a.strike - target).abs();
            let db = (b.strike - target).abs();
            da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
        })
        .ok_or_else(|| "empty chain".into())
}

/// The listed strike strictly below (`below`-side) / above the anchor, closest
/// to it — used for credit-spread and condor wings.
fn pick_adjacent<'a>(
    rows: &[&'a ChainRow],
    right: Right,
    anchor: f64,
    above: bool,
) -> Option<&'a ChainRow> {
    rows.iter()
        .copied()
        .filter(|r| {
            r.right == right
                && if above {
                    r.strike > anchor + 1e-9
                } else {
                    r.strike < anchor - 1e-9
                }
        })
        .max_by(|a, b| {
            if above {
                b.strike.partial_cmp(&a.strike).unwrap_or(std::cmp::Ordering::Equal)
            } else {
                a.strike.partial_cmp(&b.strike).unwrap_or(std::cmp::Ordering::Equal)
            }
        })
}

/// Pick the short leg nearest the target delta **that still has a wing
/// beyond it** — walking inward through candidates until one works.
fn pick_short_with_wing<'a>(
    rows: &[&'a ChainRow],
    right: Right,
    target_delta: f64,
    spot: f64,
    t: f64,
    wing_above: bool,
) -> Option<(&'a ChainRow, &'a ChainRow)> {
    let mut cands: Vec<&ChainRow> = rows
        .iter()
        .copied()
        .filter(|r| {
            r.right == right && (0.02..=0.98).contains(&row_delta(r, spot, t))
        })
        .collect();
    cands.sort_by(|a, b| {
        let da = (row_delta(a, spot, t) - target_delta).abs() - 0.03 * liquidity_score(a);
        let db = (row_delta(b, spot, t) - target_delta).abs() - 0.03 * liquidity_score(b);
        da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
    });
    for short in cands {
        if let Some(wing) = pick_adjacent(rows, right, short.strike, wing_above) {
            return Some((short, wing));
        }
    }
    None
}

/// Median strike spacing of one right's chain.
fn strike_step(rows: &[&ChainRow], right: Right) -> f64 {
    let mut ks: Vec<f64> = rows
        .iter()
        .filter(|r| r.right == right)
        .map(|r| r.strike)
        .collect();
    ks.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    ks.dedup();
    if ks.len() < 2 {
        return 0.0;
    }
    let mut steps: Vec<f64> = ks.windows(2).map(|w| w[1] - w[0]).collect();
    steps.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    steps[steps.len() / 2]
}

/// Route one leg to the best venue book and build the wire leg.
/// Buys take the lowest ask, sales hit the highest bid — both preferring
/// venues quoting size at the touch; falls back to marks.
fn leg(action: LegAction, r: &ChainRow, spot: f64, t: f64) -> Result<SuggestLeg, String> {
    let right = match r.right {
        Right::Call => QuantRight::Call,
        Right::Put => QuantRight::Put,
    };
    let iv_frac = r.iv_consensus_vp.unwrap_or(60.0).max(5.0) / 100.0;
    let delta = bs_delta(spot, r.strike, t, iv_frac, right);

    let routed = if matches!(action, LegAction::Sell) {
        r.venues
            .iter()
            .filter_map(|v| {
                v.bid_px
                    .filter(|p| *p > 0.0)
                    .map(|p| (p, v.venue.clone(), v.size_bid))
            })
            .filter(|(_, _, s)| s.unwrap_or(0.0) > 0.0)
            .max_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal))
    } else {
        r.venues
            .iter()
            .filter_map(|v| {
                v.ask_px
                    .filter(|p| *p > 0.0)
                    .map(|p| (p, v.venue.clone(), v.size_ask))
            })
            .filter(|(_, _, s)| s.unwrap_or(0.0) > 0.0)
            .min_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal))
    }
    .or_else(|| {
        r.venues.iter().find_map(|v| {
            v.mark_px
                .filter(|p| *p > 0.0)
                .map(|p| (p, v.venue.clone(), None))
        })
    });
    let (px, venue, size_at_touch) = routed.ok_or_else(|| format!("no quote for strike {}", r.strike))?;

    Ok(SuggestLeg {
        action,
        right: r.right,
        strike: r.strike,
        date: r.date,
        venue,
        px,
        iv_vp: r.iv_consensus_vp,
        delta: Some(delta),
        min_size: size_at_touch.filter(|s| *s > 0.0),
        label: format!(
            "{} {} {} {}",
            r.und,
            iso_date(r.date),
            crate::consolidate::format_strike(r.strike),
            r.right.letter()
        ),
    })
}

/// Numeric payoff envelope. Returns (max profit, max loss, breakevens);
/// None = unbounded (grid-edge extremum that is still sloping outward).
fn payoff_envelope(legs: &[SuggestLeg], net: f64, spot: f64) -> (Option<f64>, Option<f64>, Vec<f64>) {
    let n = 2001;
    let lo = spot * 0.4;
    let hi = spot * 1.8;
    let step = (hi - lo) / (n as f64 - 1.0);
    let profit = |s: f64| -> f64 {
        let mut v = -net;
        for l in legs {
            let sign = if matches!(l.action, LegAction::Buy) { 1.0 } else { -1.0 };
            let intr = match l.right {
                Right::Call => (s - l.strike).max(0.0),
                Right::Put => (l.strike - s).max(0.0),
            };
            v += sign * intr;
        }
        v
    };
    let mut vals = Vec::with_capacity(n);
    for i in 0..n {
        vals.push(profit(lo + i as f64 * step));
    }
    let max_i = vals
        .iter()
        .enumerate()
        .max_by(|a, b| a.1.partial_cmp(b.1).unwrap_or(std::cmp::Ordering::Equal))
        .map(|(i, _)| i)
        .unwrap_or(0);
    let min_i = vals
        .iter()
        .enumerate()
        .min_by(|a, b| a.1.partial_cmp(b.1).unwrap_or(std::cmp::Ordering::Equal))
        .map(|(i, _)| i)
        .unwrap_or(0);
    let max_profit = if (max_i == 0 || max_i == n - 1) && !edge_is_plateau(&vals, max_i) {
        None
    } else {
        Some(vals[max_i])
    };
    let max_loss = if (min_i == 0 || min_i == n - 1) && !edge_is_plateau(&vals, min_i) {
        None
    } else {
        Some(vals[min_i])
    };
    // Breakevens: linear-interp sign changes, near spot only.
    let mut bes = Vec::new();
    for i in 0..n - 1 {
        let (a, b) = (vals[i], vals[i + 1]);
        if (a < 0.0 && b >= 0.0) || (a > 0.0 && b <= 0.0) {
            let f = if (b - a).abs() < 1e-12 {
                0.5
            } else {
                -a / (b - a)
            };
            let s = lo + (i as f64 + f) * step;
            if (0.3..3.0).contains(&(s / spot)) {
                bes.push(s);
            }
        }
    }
    bes.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    bes.dedup_by(|a, b| (*a - *b).abs() < spot * 0.005);
    (max_profit, max_loss, bes)
}

/// True when the grid-edge index `i` sits on a flat plateau (bounded), not a
/// slope running out of the grid (unbounded).
fn edge_is_plateau(vals: &[f64], i: usize) -> bool {
    let n = vals.len();
    if n < 3 {
        return true;
    }
    if i == 0 {
        (vals[1] - vals[0]).abs() < 1e-9 * vals[0].abs().max(1.0)
    } else if i == n - 1 {
        (vals[n - 1] - vals[n - 2]).abs() < 1e-9 * vals[n - 1].abs().max(1.0)
    } else {
        true
    }
}

#[allow(clippy::too_many_arguments)]
fn coherence(
    view: MarketView,
    regime: Regime,
    trend: &str,
    term_slope: Option<f64>,
    legs: &[SuggestLeg],
    spot: f64,
    iv_pct: Option<f64>,
    vr: Option<f64>,
    rsi: Option<f64>,
) -> CoherenceBreakdown {
    let mut score = 50u32;
    let regime_txt = match regime {
        Regime::Rich => "rich",
        Regime::Cheap => "cheap",
        Regime::Fair => "fair",
    };
    // Premium stance vs regime.
    let selling = legs
        .iter()
        .any(|l| matches!(l.action, LegAction::Sell))
        && legs.iter().filter(|l| matches!(l.action, LegAction::Sell)).count()
            >= legs.iter().filter(|l| matches!(l.action, LegAction::Buy)).count();
    let stance_ok = match regime {
        Regime::Rich => selling,
        Regime::Cheap => !selling,
        Regime::Fair => true,
    };
    if stance_ok {
        score += 15;
    }
    // Trend vs view.
    let trend_ok = match view {
        MarketView::Bullish => trend == "up",
        MarketView::Bearish => trend == "down",
        _ => trend == "flat",
    };
    if trend_ok {
        score += 10;
    }
    // Term slope vs vega stance.
    if let Some(ts) = term_slope {
        let short_vega = legs_greeks_sign_vega(legs, spot);
        if (ts > 2.0 && short_vega) || (ts < -2.0 && !short_vega) {
            score += 5;
        }
    }
    // Liquidity: all legs two-sided at the routed venue.
    let liq = legs
        .iter()
        .filter(|l| l.min_size.unwrap_or(0.0) > 0.0)
        .count();
    let liq_txt = if liq == legs.len() {
        score += 10;
        "deep at the touch"
    } else if liq > 0 {
        score += 5;
        "partial"
    } else {
        "thin — marks only"
    };
    // Momentum overextension check.
    if let Some(r) = rsi {
        let over = match view {
            MarketView::Bullish => r > 74.0,
            MarketView::Bearish => r < 26.0,
            _ => false,
        };
        if over {
            score = score.saturating_sub(10);
        }
    }
    score = score.min(100);

    let pct_txt = iv_pct
        .map(|p| format!("IV percentile {p:.0}"))
        .unwrap_or_else(|| "IV percentile warming up".into());
    let vr_txt = vr.map(|v| format!("variance risk premium {v:+.1}vp")).unwrap_or_default();
    let skew_txt = "skew-aware strike pick";
    let mut rationale = format!(
        "Vol is {regime_txt} ({pct_txt}{}), trend reads {trend}, so the structure {} premium. ",
        if vr_txt.is_empty() { String::new() } else { format!(", {vr_txt}") },
        if stance_ok { "matches" } else { "fights" },
    );
    rationale.push_str(&format!(
        "Strikes were picked from live cross-venue quotes ({} legs routed to their best books) rather than fixed percentages. ",
        legs.len()
    ));
    if !trend_ok {
        rationale.push_str(&format!(
            "Caution: the EMA trend ({trend}) does not confirm a {view:?} stance. ",
        ));
    }
    rationale.push_str("Execution stays inside displayed liquidity; re-check the card if IV moves more than a few vol points.");

    CoherenceBreakdown {
        score,
        iv_regime: regime_txt.to_string(),
        trend: trend.to_string(),
        skew: skew_txt.to_string(),
        liquidity: liq_txt.to_string(),
        rationale,
    }
}

fn legs_greeks_sign_vega(legs: &[SuggestLeg], spot: f64) -> bool {
    // net vega sign from deltas as a cheap proxy (OTM legs dominate sign)
    let _ = spot;
    let buy = legs
        .iter()
        .filter(|l| matches!(l.action, LegAction::Buy))
        .count();
    buy < legs.len() - buy
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::consolidate::canon;

    fn synth_book(now: i64) -> ConsolidatedBook {
        // 30-day BTC chain around 80k, 7 strikes, calls+puts, two venues.
        let mut rows = Vec::new();
        let spot = 80000.0;
        let strikes: [f64; 7] = [74000.0, 76000.0, 78000.0, 80000.0, 82000.0, 84000.0, 86000.0];
        for right in [Right::Call, Right::Put] {
            for (i, k) in strikes.iter().enumerate() {
                let lm = (k / spot).ln();
                let iv = 55.0 + if right == Right::Put { -120.0 * lm } else { 40.0 * lm } + 80.0 * lm * lm;
                let t = 30.0 / 365.0;
                let prem = ivarb_quant::bs_price(
                    spot,
                    *k,
                    t,
                    iv / 100.0,
                    match right {
                        Right::Call => QuantRight::Call,
                        Right::Put => QuantRight::Put,
                    },
                )
                .unwrap();
                let mut venues = Vec::new();
                for v in ["deribit", "bybit"] {
                    venues.push(ivarb_protocol::research::ChainVenueQuote {
                        venue: v.into(),
                        mark_px: Some(prem),
                        bid_px: Some(prem * 0.985),
                        ask_px: Some(prem * 1.015),
                        iv_mark_vp: Some(iv),
                        iv_bid_vp: Some(iv - 0.5),
                        iv_ask_vp: Some(iv + 0.5),
                        size_bid: Some(10.0 + i as f64),
                        size_ask: Some(10.0 + i as f64),
                        open_interest: Some(120.0),
                        volume_24h: Some(40.0),
                        tick_age_ms: None,
                    });
                }
                rows.push(ChainRow {
                    und: "BTC".into(),
                    date: 20261112,
                    strike: *k,
                    right,
                    spot,
                    dte: 30,
                    iv_consensus_vp: Some(iv),
                    iv_spread_vp: Some(0.0),
                    best_ask_venue: Some("deribit".into()),
                    best_ask_px: Some(prem * 1.015),
                    best_bid_venue: Some("bybit".into()),
                    best_bid_px: Some(prem * 0.985),
                    open_interest: Some(240.0),
                    venues,
                });
            }
        }
        let mut spots = rustc_hash::FxHashMap::default();
        spots.insert("BTC".to_string(), spot);
        let mut atm = rustc_hash::FxHashMap::default();
        atm.insert(("BTC".to_string(), 20261112), 55.0);
        ConsolidatedBook {
            ts_ms: now,
            data_mode: "live",
            spots,
            rows,
            venue_stats: vec![],
            iv: vec![],
            atm_by_date: atm,
        }
    }

    #[test]
    fn builds_bull_call_spread_with_live_quotes() {
        let book = synth_book(1_790_000_000_000);
        let card = build(
            &book,
            None,
            &SuggestRequest {
                und: "BTC".into(),
                view: MarketView::Bullish,
                horizon_days: 21,
                risk: RiskAppetite::Balanced,
            },
            1_790_000_000_000,
        )
        .expect("card");
        assert_eq!(card.strategy, "Bull Call Spread");
        assert_eq!(card.legs.len(), 2);
        assert_eq!(card.legs[0].action, LegAction::Buy);
        assert_eq!(card.legs[0].venue, "deribit"); // lowest ask
        assert!(card.legs[1].strike > card.legs[0].strike);
        assert!(card.net_premium_usd > 0.0);
        assert!(card.max_profit_usd.is_some());
        assert!(card.max_loss_usd.unwrap() <= card.net_premium_usd + 1e-6);
        assert!(!card.breakevens.is_empty());
        assert!(card.greeks.delta > 0.0 && card.greeks.delta < 1.0);
        assert!(card.coherence.score >= 50);
        assert!(card.legs.iter().all(|l| l.label.starts_with("BTC 2026-11-12")));
    }

    #[test]
    fn rich_vol_bullish_flips_to_credit_spread() {
        let mut book = synth_book(1_790_000_000_000);
        book.iv = vec![ivarb_protocol::research::IvSummary {
            und: "BTC".into(),
            atm_iv_vp: Some(60.0),
            iv_percentile: Some(88.0),
            iv_percentile_basis: "cross-sectional".to_string(),
            realized_vol_vp: Some(45.0),
            vr_premium_vp: Some(15.0),
            pc_ratio_oi: Some(0.9),
            term_slope_vp: Some(2.0),
        }];
        let card = build(
            &book,
            None,
            &SuggestRequest {
                und: "BTC".into(),
                view: MarketView::Bullish,
                horizon_days: 21,
                risk: RiskAppetite::Balanced,
            },
            1_790_000_000_000,
        )
        .expect("card");
        assert_eq!(card.strategy, "Put Credit Spread");
        assert_eq!(card.flow, "credit");
        assert!(card.net_premium_usd < 0.0);
    }

    #[test]
    fn iron_condor_on_neutral() {
        let mut book = synth_book(1_790_000_000_000);
        book.iv = vec![ivarb_protocol::research::IvSummary {
            und: "BTC".into(),
            atm_iv_vp: Some(55.0),
            iv_percentile: Some(55.0),
            iv_percentile_basis: "cross-sectional".to_string(),
            realized_vol_vp: Some(50.0),
            vr_premium_vp: Some(5.0),
            pc_ratio_oi: Some(1.0),
            term_slope_vp: Some(1.0),
        }];
        let card = build(
            &book,
            None,
            &SuggestRequest {
                und: "BTC".into(),
                view: MarketView::NeutralRange,
                horizon_days: 21,
                risk: RiskAppetite::Balanced,
            },
            1_790_000_000_000,
        )
        .expect("card");
        assert_eq!(card.strategy, "Iron Condor");
        assert_eq!(card.legs.len(), 4);
        assert!(card.max_profit_usd.unwrap() > 0.0);
        assert!(card.max_loss_usd.unwrap() < 0.0);
        // breakevens bracket spot
        assert!(card.breakevens[0] < 80000.0 && *card.breakevens.last().unwrap() > 80000.0);
    }

    #[test]
    fn canon_helper() {
        assert_eq!(canon("BTC", 20261112, 80000.0, Right::Call), "BTC-20261112-80000-C");
    }
}
