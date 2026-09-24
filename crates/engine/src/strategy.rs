//! The arbitrage strategy: fee-adjusted cross-venue edge evaluation with
//! executable-price preference, liquidity sizing and lead-lag diagnostics.

use std::collections::VecDeque;

use ivarb_protocol::{EdgeMode, Opportunity, Right as PRight};
use ivarb_quant::{bs_price, bs_vega, fee_vp, vega_per_vp, Right};

use crate::config::{EngineConfig, VenueSpec};
use crate::ga::GenomeView;
use crate::key::{iso_from_ymd, t_years, InstKey};
use crate::msg::InstrTick;
use crate::timing::{self, SignalCtx, StrategyView};

/// Live quote state for one (venue, instrument).
#[derive(Debug, Clone)]
pub struct LegQuote {
    pub iv_mark_vp: Option<f64>,
    pub iv_bid_vp: Option<f64>,
    pub iv_ask_vp: Option<f64>,
    pub px_mark_unit: Option<f64>,
    pub px_bid_unit: Option<f64>,
    pub px_ask_unit: Option<f64>,
    pub size_bid: Option<f64>,
    pub size_ask: Option<f64>,
    pub spot: Option<f64>,
    pub recv_ms: i64,
    pub venue_ts_ms: Option<i64>,
    /// (ts, mark iv) samples for drift/lead-lag, ~2 minutes kept.
    iv_hist: VecDeque<(i64, f64)>,
}

impl LegQuote {
    pub fn from_tick(t: &InstrTick, recv_ms: i64) -> Self {
        let mut q = Self {
            iv_mark_vp: t.iv_mark_vp,
            iv_bid_vp: t.iv_bid_vp,
            iv_ask_vp: t.iv_ask_vp,
            px_mark_unit: t.px_mark_unit,
            px_bid_unit: t.px_bid_unit,
            px_ask_unit: t.px_ask_unit,
            size_bid: t.size_bid,
            size_ask: t.size_ask,
            spot: t.spot,
            recv_ms,
            venue_ts_ms: t.venue_ts_ms,
            iv_hist: VecDeque::new(),
        };
        if let Some(iv) = q.iv_mark_vp {
            q.push_iv(iv, recv_ms);
        }
        q
    }

    pub fn update(&mut self, t: &InstrTick, recv_ms: i64) {
        self.px_mark_unit = t.px_mark_unit.or(self.px_mark_unit);
        // Partial rows (e.g. mark-price channel without quotes) must not wipe
        // the existing bid/ask state.
        if t.px_bid_unit.is_some() {
            self.px_bid_unit = t.px_bid_unit;
        }
        if t.px_ask_unit.is_some() {
            self.px_ask_unit = t.px_ask_unit;
        }
        if t.size_bid.is_some() {
            self.size_bid = t.size_bid;
        }
        if t.size_ask.is_some() {
            self.size_ask = t.size_ask;
        }
        if let Some(s) = t.spot {
            self.spot = Some(s);
        }
        self.venue_ts_ms = t.venue_ts_ms;
        self.recv_ms = recv_ms;
        if let Some(iv) = t.iv_mark_vp {
            self.iv_mark_vp = Some(iv);
            self.push_iv(iv, recv_ms);
        }
        if let Some(v) = t.iv_bid_vp {
            self.iv_bid_vp = Some(v);
        }
        if let Some(v) = t.iv_ask_vp {
            self.iv_ask_vp = Some(v);
        }
    }

    fn push_iv(&mut self, iv: f64, ts: i64) {
        let changed = self
            .iv_hist
            .back()
            .map(|(_, prev)| (prev - iv).abs() > 1e-9)
            .unwrap_or(true);
        self.iv_hist.push_back((ts, iv));
        while self.iv_hist.len() > 240 {
            self.iv_hist.pop_front();
        }
        // Keep a rolling ~2.5 minute window.
        while self.iv_hist.len() > 2
            && ts - self.iv_hist.front().map(|(t, _)| *t).unwrap_or(0) > 150_000
        {
            self.iv_hist.pop_front();
        }
        let _ = changed;
    }

    /// IV drift over the trailing window (vp).
    pub fn drift_vp(&self, window_ms: i64) -> f64 {
        let (latest_ts, latest_iv) = match self.iv_hist.back() {
            Some(x) => *x,
            None => return 0.0,
        };
        let mut base: Option<(i64, f64)> = None;
        for (ts, iv) in self.iv_hist.iter() {
            if latest_ts - ts <= window_ms {
                base = Some((*ts, *iv));
                break;
            }
        }
        match base {
            Some((_, b)) => latest_iv - b,
            None => 0.0,
        }
    }

    /// Ms since the last material IV change (lead-lag).
    pub fn since_iv_change_ms(&self, now: i64) -> i64 {
        if self.iv_hist.len() < 2 {
            return now - self.recv_ms;
        }
        let mut last_change = self.recv_ms;
        let mut prev: Option<f64> = None;
        for (ts, iv) in self.iv_hist.iter() {
            if let Some(p) = prev {
                if (p - iv).abs() > 0.02 {
                    last_change = *ts;
                }
            }
            prev = Some(*iv);
        }
        (now - last_change).max(0)
    }
}

fn to_pright(r: u8) -> PRight {
    if r == crate::key::RIGHT_CALL {
        PRight::Call
    } else {
        PRight::Put
    }
}

/// Format a strike for canonical ids: integers without a decimal tail.
pub fn fmt_key_strike(strike: f64) -> String {
    if (strike - strike.round()).abs() < 1e-9 {
        format!("{}", strike.round() as i64)
    } else {
        format!("{strike}")
    }
}

fn to_qright(r: u8) -> Right {
    if r == crate::key::RIGHT_CALL {
        Right::Call
    } else {
        Right::Put
    }
}

/// Public alias used by the hold tracker and multihop scanner.
pub fn qright(r: u8) -> Right {
    to_qright(r)
}

pub struct OppCore {
    pub id: String,
    pub key: InstKey,
    pub buy_venue: String,
    pub sell_venue: String,
    pub iv_buy_vp: f64,
    pub iv_sell_vp: f64,
    pub iv_spread_vp: f64,
    pub fee_vp: f64,
    pub net_vp: f64,
    pub edge_mode: EdgeMode,
    pub vega_vp_usd: f64,
    pub est_usd: f64,
    pub contract_size: f64,
    pub max_units: Option<f64>,
    pub total_usd: Option<f64>,
    pub buy_px_usd: Option<f64>,
    pub sell_px_usd: Option<f64>,
    pub size_buy: Option<f64>,
    pub size_sell: Option<f64>,
    pub lead_buy_ms: i64,
    pub lead_sell_ms: i64,
    pub drift_buy_vp_1m: f64,
    pub drift_sell_vp_1m: f64,
    pub spot: f64,
    pub updated_ms: i64,
    /// Expiry timestamp (ms) — needed by the time-aware strategy layer.
    pub expiry_ms: i64,
    /// Strategy inputs needed by the time-aware layer (quote age, depth).
    pub quote_age_ms: f64,
    pub depth_units: Option<f64>,
}

impl OppCore {
    pub fn to_opportunity(
        &self,
        und: &str,
        expiry_ms: i64,
        first_seen_ms: i64,
        _hist: Vec<(i64, f64)>,
    ) -> Opportunity {
        Opportunity {
            id: self.id.clone(),
            underlying: und.to_string(),
            expiry: iso_from_ymd(self.key.date),
            expiry_ms,
            strike: self.key.strike(),
            right: to_pright(self.key.right),
            buy_venue: self.buy_venue.clone(),
            sell_venue: self.sell_venue.clone(),
            iv_buy_vp: self.iv_buy_vp,
            iv_sell_vp: self.iv_sell_vp,
            iv_spread_vp: self.iv_spread_vp,
            fee_vp: self.fee_vp,
            net_vp: self.net_vp,
            edge_mode: self.edge_mode,
            vega_vp_usd: self.vega_vp_usd,
            est_usd: self.est_usd,
            contract_size: self.contract_size,
            max_units: self.max_units,
            total_usd: self.total_usd,
            buy_px_usd: self.buy_px_usd,
            sell_px_usd: self.sell_px_usd,
            spot: 0.0, // filled by caller
            t_years: 0.0, // filled by caller
            size_buy: self.size_buy,
            size_sell: self.size_sell,
            first_seen_ms,
            updated_ms: self.updated_ms,
            lead_buy_ms: self.lead_buy_ms,
            lead_sell_ms: self.lead_sell_ms,
            drift_buy_vp_1m: self.drift_buy_vp_1m,
            drift_sell_vp_1m: self.drift_sell_vp_1m,
            stale: false,
            retired: false,
            hist_pt: None,
            history: None,
            tier: ivarb_protocol::Tier::Monitor,
            bucket: "".into(),
            dte: 0.0,
            gate_vp: 0.0,
            ev_vp: 0.0,
            theta_vp_day: 0.0,
            patience_s: 0.0,
            hold_s: 0.0,
            conv_hint: false,
            conv_eta_s: None,
            kalman_level_vp: None,
            kalman_sigma_vp: None,
            hold: None,
        }
    }

    /// Time-aware strategy view for this core; used by the engine to stamp
    /// tier/gate/EV fields on the opportunity.
    pub fn strategy_view_ga(
        &self,
        cfg: &EngineConfig,
        signal_age_ms: f64,
        now_ms: i64,
        ga: &GenomeView,
    ) -> StrategyView {
        let iv_mid_raw = ((self.iv_buy_vp + self.iv_sell_vp) * 0.5 / 100.0).max(1e-3);
        let hour = ((now_ms / 3_600_000).rem_euclid(24)) as f64;
        timing::evaluate(
            &SignalCtx {
                net_vp: self.net_vp,
                fee_vp: self.fee_vp,
                exec_mode: matches!(self.edge_mode, EdgeMode::Exec),
                quote_age_ms: self.quote_age_ms,
                signal_age_ms,
                depth_units: self.depth_units,
                drift_buy_vp: self.drift_buy_vp_1m,
                drift_sell_vp: self.drift_sell_vp_1m,
                spot: self.spot,
                strike: self.key.strike(),
                t_years: t_years(now_ms, self.expiry_ms),
                iv_mid_raw,
                base_gate_vp: cfg.min_net_vp,
                ga_gate_mult: ga.gate_mult(hour),
                ga_patience_mult: ga.patience_mult(),
                ga_hold_mult: ga.hold_mult(),
            },
            now_ms,
        )
    }

    /// Back-compat view without genome adjustments.
    pub fn strategy_view(
        &self,
        cfg: &EngineConfig,
        signal_age_ms: f64,
        now_ms: i64,
    ) -> StrategyView {
        self.strategy_view_ga(cfg, signal_age_ms, now_ms, &GenomeView::default())
    }
}

/// Evaluate one directed pair: buy on `a`, sell on `b`.
///
/// Executable edge (ask IV on the buy leg, bid IV on the sell leg) is used
/// whenever both are known; otherwise it falls back to mark-vs-mark and the
/// opportunity is flagged `EdgeMode::Mark`.
#[allow(clippy::too_many_arguments)]
fn eval_dir(
    a: (&VenueSpec, &LegQuote),
    b: (&VenueSpec, &LegQuote),
    key: &InstKey,
    expiry_ms: i64,
    spot: f64,
    now: i64,
    cfg: &EngineConfig,
) -> Option<OppCore> {
    let (spec_a, qa) = a;
    let (spec_b, qb) = b;

    // --- the edge is a PREMIUM spread, not a venue-IV spread ----------------
    //
    // Venues publish IVs on their *own* forwards: Bybit's USDT options mark
    // on `underlyingPrice` (≈ futures, ~1.3% over spot at a 3-month tenor),
    // Binance marks with an explicit 5.4% risk-free rate, Deribit's
    // coin-margined marks sit near index. Mixing venue IVs across venues
    // fabricates 2-4 vp "spreads" that do not exist in the premiums you can
    // actually trade, and re-solving those premiums on a single r=0 basis
    // distorts deep-ITM legs instead.
    //
    // The executable truth: both legs of the same contract settle on
    // (approximately) the same index at expiry, so the capturable edge is
    // simply the premium difference, converted to vol points through ONE
    // consistent vega. Venue IVs remain on the wire as display context.
    let t = t_years(now, expiry_ms);
    let strike = key.strike();

    // Vega per vp per unit from the venue's own mark IV (a conversion factor
    // only — vega is flat enough that the forward basis does not matter).
    let vega_of = |iv_vp: Option<f64>| -> Option<f64> {
        let iv = (iv_vp.filter(|v| *v > 0.0).unwrap_or(50.0) / 100.0).clamp(0.01, 4.0);
        let v = vega_per_vp(bs_vega(spot, strike, t, iv), 1.0);
        v.is_finite().then_some(v)
    };
    let vega_buy_unit = vega_of(qa.iv_mark_vp)?;
    let vega_sell_unit = vega_of(qb.iv_mark_vp)?;
    if vega_buy_unit < cfg.min_vega_usd || vega_sell_unit < cfg.min_vega_usd {
        return None;
    }

    let pos = |p: Option<f64>| p.filter(|x| x.is_finite() && *x > 0.0);
    let (buy_px, sell_px, mode) = match (pos(qa.px_ask_unit), pos(qb.px_bid_unit)) {
        (Some(pa), Some(pb)) => (pa, pb, EdgeMode::Exec),
        // No executable pair in this direction: fall back to mark premiums so
        // the CONVERGENCE tier can see divergences inside crossed books
        // (mark signals face a stricter entry gate upstream).
        _ => (pos(qa.px_mark_unit)?, pos(qb.px_mark_unit)?, EdgeMode::Mark),
    };

    // The signal: premium spread in vol points on one common vega.
    let spread = (sell_px - buy_px) / vega_buy_unit;

    // Cross-leg price sanity: a genuine cross-venue arb never shows one leg
    // at a fraction of the other. Dust quotes on empty books (a stale
    // bidPrice of $100 on a $9,800 option) are rejected outright.
    let ratio = sell_px / buy_px;
    if !(0.5..=2.0).contains(&ratio) {
        return None;
    }

    // Display IVs: venue-quoted when present, solved from the premium as a
    // fallback (context only — never used for the edge).
    let iv_of = |px: f64| -> Option<f64> {
        ivarb_quant::implied_vol(px, spot, strike, t, to_qright(key.right))
            .map(|iv| iv * 100.0)
            .filter(|v| v.is_finite())
    };
    let iv_buy = qa.iv_ask_vp.or_else(|| iv_of(buy_px)).unwrap_or(0.0);
    let iv_sell = qb.iv_bid_vp.or_else(|| iv_of(sell_px)).unwrap_or(0.0);

    // Sanity band: legs quoted at fantasy IV levels are not tradable edges.
    if iv_buy > 0.0 && !(cfg.iv_min_vp..=cfg.iv_max_vp).contains(&iv_buy) {
        return None;
    }
    if iv_sell > 0.0 && !(cfg.iv_min_vp..=cfg.iv_max_vp).contains(&iv_sell) {
        return None;
    }
    // A spread wider than any real cross-venue divergence is a stale quote.
    if spread > cfg.max_spread_vp {
        return None;
    }
    // Moneyness band: deep wings are where cross-venue quote garbage lives.
    // The band tightens with short DTE (1.5 sd of the tenor): a 12% OTM
    // strike is reasonable at 30 days but pure dust-quote territory at 2.
    let iv_ref = (qa.iv_mark_vp.unwrap_or(50.0).max(qb.iv_mark_vp.unwrap_or(50.0)) / 100.0)
        .clamp(0.05, 3.0);
    let mny_cap = (1.5 * iv_ref * t.sqrt()).min(cfg.signal_moneyness_pct);
    if ((strike / spot).ln().abs()) > mny_cap {
        return None;
    }

    let fee_a = fee_vp(spec_a.taker_fee_rate, spot, vega_buy_unit);
    let fee_b = fee_vp(spec_b.taker_fee_rate, spot, vega_sell_unit);
    let fee = fee_a + fee_b;
    if !fee.is_finite() {
        return None;
    }

    let net = spread - fee;

    let size_a = qa.contract_size_hint().unwrap_or(spec_a.contract_size);
    let size_b = qb.contract_size_hint().unwrap_or(spec_b.contract_size);
    let vega_vp_usd = vega_buy_unit * size_a;
    let est_usd = net * vega_vp_usd;

    let max_units = match (qa.size_ask, qb.size_bid) {
        (Some(sa), Some(sb)) => Some((sa * size_a).min(sb * size_b)),
        _ => None,
    };
    let total_usd = max_units.map(|u| net * vega_buy_unit * u);

    Some(OppCore {
        id: format!(
            "{}-{}-{}-{}:{}>{}",
            key.und,
            key.date,
            strike,
            to_pright(key.right).letter(),
            spec_a.id,
            spec_b.id
        ),
        key: *key,
        buy_venue: spec_a.id.clone(),
        sell_venue: spec_b.id.clone(),
        iv_buy_vp: iv_buy,
        iv_sell_vp: iv_sell,
        iv_spread_vp: spread,
        fee_vp: fee,
        net_vp: net,
        edge_mode: mode,
        vega_vp_usd,
        est_usd,
        contract_size: size_a,
        max_units,
        total_usd,
        buy_px_usd: qa.px_ask_unit.map(|p| p * size_a),
        sell_px_usd: qb.px_bid_unit.map(|p| p * size_b),
        size_buy: qa.size_ask,
        size_sell: qb.size_bid,
        lead_buy_ms: qa.since_iv_change_ms(now),
        lead_sell_ms: qb.since_iv_change_ms(now),
        drift_buy_vp_1m: qa.drift_vp(60_000),
        drift_sell_vp_1m: qb.drift_vp(60_000),
        spot,
        updated_ms: now.max(qa.recv_ms).max(qb.recv_ms),
        expiry_ms,
        quote_age_ms: (now - qa.recv_ms.min(qb.recv_ms)).max(0) as f64,
        depth_units: max_units,
    })
}

impl LegQuote {
    /// Contract size hinted by recent ticks (venue-published).
    fn contract_size_hint(&self) -> Option<f64> {
        None // venue-level override handled by the engine
    }
}

/// Evaluate a venue pair in both directions and keep the better net edge.
/// Returns `None` when neither direction has valid quotes on both legs.
#[allow(clippy::too_many_arguments)]
pub fn evaluate_pair(
    spec_a: &VenueSpec,
    qa: &LegQuote,
    spec_b: &VenueSpec,
    qb: &LegQuote,
    key: &InstKey,
    expiry_ms: i64,
    spot: f64,
    now: i64,
    _cfg: &EngineConfig,
) -> Option<OppCore> {
    // Executable direction only: we need a buyable ask on one side and a
    // sellable bid on the other.
    let ab = eval_dir((spec_a, qa), (spec_b, qb), key, expiry_ms, spot, now, _cfg);
    let ba = eval_dir((spec_b, qb), (spec_a, qa), key, expiry_ms, spot, now, _cfg);
    match (ab, ba) {
        (Some(x), Some(y)) => Some(if x.net_vp >= y.net_vp { x } else { y }),
        (Some(x), None) => Some(x),
        (None, Some(y)) => Some(y),
        (None, None) => None,
    }
}

/// Derive missing IVs from normalized unit premiums using the engine's own
/// Black-Scholes solver (used when a venue publishes prices but no IVs).
pub fn derive_ivs(t: &mut InstrTick, spot: f64, strike: f64, right: u8, expiry_ms: i64, now: i64) {
    let right = to_qright(right);
    let tt = t_years(now, expiry_ms);
    if t.iv_mark_vp.is_none() {
        if let Some(px) = t.px_mark_unit {
            t.iv_mark_vp = ivarb_quant::implied_vol(px, spot, strike, tt, right)
                .map(|iv| iv * 100.0)
                .filter(|v| v.is_finite());
        }
    }
    if t.iv_bid_vp.is_none() {
        if let Some(px) = t.px_bid_unit {
            t.iv_bid_vp = ivarb_quant::implied_vol(px, spot, strike, tt, right)
                .map(|iv| iv * 100.0)
                .filter(|v| v.is_finite());
        }
    }
    if t.iv_ask_vp.is_none() {
        if let Some(px) = t.px_ask_unit {
            t.iv_ask_vp = ivarb_quant::implied_vol(px, spot, strike, tt, right)
                .map(|iv| iv * 100.0)
                .filter(|v| v.is_finite());
        }
    }
}

/// Price sanity helper exported for sim/tests.
pub fn unit_price(spot: f64, strike: f64, t: f64, iv_raw: f64, right: u8) -> f64 {
    bs_price(spot, strike, t, iv_raw, to_qright(right)).unwrap_or(0.0)
}
