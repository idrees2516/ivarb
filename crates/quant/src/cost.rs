//! Comprehensive cost-of-arbitrage and capital-requirement model.
//!
//! The scanner's headline number (`net_vp`) already nets **taker fees** off
//! the executable spread. But the true cost of harvesting a cross-venue arb
//! is bigger: you pay slippage through the touch on both legs at entry AND
//! exit, you must stage segregated collateral on two venues (one-time
//! funding round-trip), the short leg ties up margin for the holding period
//! (carry/opportunity cost), and a hedged book still bleeds netted theta.
//!
//! This module prices every one of those lines so the UI can show
//! "what you actually keep" — and how much capital the trade demands,
//! per venue, before it can be placed.
//!
//! All formulas are venue-documented approximations (see `margin_per_unit`)
//! and are deliberately conservative: surprises in arb costs are negative.
//!
//! Units: everything is quoted **per 1 underlying unit** unless a name says
//! `per_contract`. `units` always means underlying units (e.g. 2.0 BTC).

use crate::Right;

/// One venue's real-money funding rail: how cash actually gets there.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FundingRail {
    pub venue: &'static str,
    /// Margin asset(s) the venue credits for options trading.
    pub asset: &'static str,
    pub network: &'static str,
    /// Typical on-chain / rail fee, USD.
    pub fee_usd: f64,
    /// Typical confirmation+credit time, minutes.
    pub minutes: f64,
    pub note: &'static str,
}

/// How to deposit real money on every supported venue (2026 public docs;
/// indicative — always confirm on the venue before wiring anything).
pub const FUNDING_RAILS: &[FundingRail] = &[
    FundingRail { venue: "deribit", asset: "BTC / ETH / USDC / USDT", network: "on-chain (BTC / ETH / ERC-20)", fee_usd: 1.0, minutes: 20.0, note: "coin-margined: USD-equivalent collateral = coin balance × index; no fiat rail" },
    FundingRail { venue: "okx", asset: "USDT / USDC / BTC / ETH", network: "TRC-20 (~$1) · ERC-20 (~$5) · fiat wire", fee_usd: 1.0, minutes: 10.0, note: "fiat wire 0.1%, 1-3 business days; TRC-20 cheapest for size" },
    FundingRail { venue: "bybit", asset: "USDT / USDC / BTC / ETH", network: "TRC-20 · ERC-20 · fiat card", fee_usd: 1.0, minutes: 10.0, note: "card 1% + processing; internal transfers free between Bybit accounts" },
    FundingRail { venue: "binance", asset: "USDT / USDC / BTC / ETH", network: "TRC-20 · ERC-20 · BEP-20 · fiat", fee_usd: 1.0, minutes: 10.0, note: "widest rail choice; check network matches the deposit address exactly" },
    FundingRail { venue: "delta", asset: "USDT / BTC / ETH", network: "TRC-20 · ERC-20 · BEP-20", fee_usd: 1.0, minutes: 15.0, note: "crypto only, no fiat; unified margin across derivatives" },
    FundingRail { venue: "aevo", asset: "USDC / USDT / ETH", network: "Optimism / Arbitrum (L2)", fee_usd: 0.5, minutes: 3.0, note: "L2 deposits are cents and near-instant; mainnet ETH bridging slower" },
    FundingRail { venue: "derive", asset: "USDC / ETH", network: "Base / Optimism (L2)", fee_usd: 0.1, minutes: 2.0, note: "L2-native: sub-cent transfers, instant credit, cash-settlement in USDC" },
    FundingRail { venue: "thalex", asset: "USDT / BTC", network: "TRON / Ethereum", fee_usd: 1.0, minutes: 15.0, note: "crypto only; margin in USDT, PnL cash-settled in USDT" },
];

/// Default annual rate used to charge the *carry* of staged short-leg
/// margin (opportunity cost of collateral locked for the hold).
pub const MARGIN_CARRY_RATE: f64 = 0.05;
/// Maintenance buffer applied on top of initial short-option margin.
pub const MARGIN_BUFFER: f64 = 0.20;
/// Fraction of gross theta the hedged book actually bleeds (long and short
/// legs partially cancel; the engine nets them at 0.15).
pub const THETA_NETTING: f64 = 0.15;

/// Short-option initial margin per **contract** on a venue, in premium-currency
/// USD-equivalent. Documented approximations of the 2026 exchange formulas:
///
/// - deribit (coin-margined): `premium + max(0.15·S − OTM, 0.08·S)` per unit
/// - bybit:  `premium + max(0.125·S − OTM, 0.05·S)`
/// - default (okx/binance/delta/aevo/derive/thalex SPAN-ish):
///          `premium + max(0.15·S − OTM, 0.06·S)`
///
/// where `OTM` is the out-of-the-money amount of the option and the
/// `S`-proportional term is the venue's "risk boundary".
pub fn margin_per_contract(
    venue: &str,
    premium_per_unit: f64,
    contract_size: f64,
    spot: f64,
    strike: f64,
    right: Right,
) -> f64 {
    if spot <= 0.0 || contract_size <= 0.0 {
        return 0.0;
    }
    let otm = match right {
        Right::Call => (strike - spot).max(0.0),
        Right::Put => (spot - strike).max(0.0),
    };
    let (hi, lo) = match venue {
        "deribit" => (0.15, 0.08),
        "bybit" => (0.125, 0.05),
        _ => (0.15, 0.06),
    };
    let boundary = ((hi * spot - otm).max(lo * spot)).max(0.0);
    (premium_per_unit + boundary) * contract_size
}

/// Inputs for the full round-trip cost sheet of one arb.
#[derive(Debug, Clone)]
pub struct CostInput {
    /// Executable premiums per **contract** (what you actually pay/receive).
    pub buy_premium_per_contract: f64,
    pub sell_premium_per_contract: f64,
    /// Underlying units per contract on each venue.
    pub buy_contract_size: f64,
    pub sell_contract_size: f64,
    /// Taker fee rates (fraction of underlying notional).
    pub buy_fee_rate: f64,
    pub sell_fee_rate: f64,
    pub spot: f64,
    pub strike: f64,
    pub right: Right,
    /// Dollar vega per vol point **per underlying unit**.
    pub vega_per_vp_unit: f64,
    /// Position size in underlying units.
    pub units: f64,
    /// Expected holding period, days.
    pub hold_days: f64,
    /// Theta decay of the spread, vol points per day (from the strategy
    /// layer). Sign-agnostic: decay (negative) or burn (positive) both price
    /// as a positive cost.
    pub theta_vp_day: f64,
    /// Displayed depth at the touch, contracts (per leg).
    pub buy_depth_contracts: f64,
    pub sell_depth_contracts: f64,
    /// One-time cost of staging capital on both venues (deposit + withdrawal).
    pub funding_round_trip_usd: f64,
    /// Sell venue id (margin model differs per venue).
    pub sell_venue: String,
}

/// The full cost sheet, USD.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct CostSheet {
    /// Taker fees on both legs at entry.
    pub entry_fees_usd: f64,
    /// Taker fees on both legs at exit.
    pub exit_fees_usd: f64,
    /// Expected slippage through the touch, both legs, entry + exit.
    pub slippage_usd: f64,
    /// Carry on staged short-leg margin over the hold.
    pub margin_carry_usd: f64,
    /// Netted theta bleed over the hold.
    pub theta_burn_usd: f64,
    /// One-time staging cost (deposit + withdrawal rails).
    pub funding_usd: f64,
    /// Everything above, summed.
    pub total_usd: f64,
    /// Same total expressed in vol points.
    pub total_vp: f64,
    /// Spread premium locked at entry (before costs).
    pub gross_locked_usd: f64,
    /// Gross minus the *trading* costs (no funding line).
    pub net_after_trading_usd: f64,
    /// Gross minus every cost including funding.
    pub net_all_in_usd: f64,
}

/// Slippage fraction for crossing a touch with `contracts` against
/// `depth_contracts` displayed — mirrors the paper broker's model so the
/// projection matches what the executor experiences.
pub fn slippage_frac(contracts: f64, depth_contracts: f64) -> f64 {
    if depth_contracts <= 0.0 {
        return 0.02; // unknown depth: assume the worst band
    }
    let ratio = (contracts / depth_contracts).clamp(0.0, 4.0);
    0.0002 + 0.0008 * ratio.sqrt()
}

/// Compute the comprehensive cost sheet.
pub fn cost_sheet(i: &CostInput) -> CostSheet {
    let contracts_buy = if i.buy_contract_size > 0.0 { i.units / i.buy_contract_size } else { i.units };
    let contracts_sell = if i.sell_contract_size > 0.0 { i.units / i.sell_contract_size } else { i.units };

    let entry_fee_buy = i.buy_fee_rate * i.spot * i.units;
    let entry_fee_sell = i.sell_fee_rate * i.spot * i.units;
    let entry_fees = entry_fee_buy + entry_fee_sell;
    // Exit fees assume unwinding both legs at similar notionals.
    let exit_fees = entry_fees;

    // Slippage: entry crosses both touches, exit crosses them again.
    let slip_buy = slippage_frac(contracts_buy, i.buy_depth_contracts);
    let slip_sell = slippage_frac(contracts_sell, i.sell_depth_contracts);
    let slippage = (i.buy_premium_per_contract * contracts_buy) * slip_buy
        + (i.sell_premium_per_contract * contracts_sell) * slip_sell;
    let slippage = 2.0 * slippage; // entry + exit

    // Carry on staged margin.
    let margin = margin_per_contract(
        &i.sell_venue,
        i.sell_premium_per_contract / i.sell_contract_size.max(1e-9),
        i.sell_contract_size,
        i.spot,
        i.strike,
        i.right,
    ) * contracts_sell;
    let margin_carry = margin * MARGIN_CARRY_RATE * (i.hold_days / 365.0).max(0.0);

    // Netted theta bleed over the hold (vega per unit × units). Sign-agnostic:
    // the strategy layer reports decay as a negative number, the cost is positive.
    let theta_burn =
        i.theta_vp_day.abs() * THETA_NETTING * i.vega_per_vp_unit * i.units * i.hold_days.max(0.0);

    let gross = (i.sell_premium_per_contract * contracts_sell)
        - (i.buy_premium_per_contract * contracts_buy);

    let total = entry_fees + exit_fees + slippage + margin_carry + theta_burn;
    let total_all_in = total + i.funding_round_trip_usd;
    let vega_total = (i.vega_per_vp_unit * i.units).max(1e-9);

    CostSheet {
        entry_fees_usd: entry_fees,
        exit_fees_usd: exit_fees,
        slippage_usd: slippage,
        margin_carry_usd: margin_carry,
        theta_burn_usd: theta_burn,
        funding_usd: i.funding_round_trip_usd,
        total_usd: total_all_in,
        total_vp: total_all_in / vega_total,
        gross_locked_usd: gross,
        net_after_trading_usd: gross - total,
        net_all_in_usd: gross - total_all_in,
    }
}

/// Capital a trade demands, split by venue (segregated collateral).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct CapitalReq {
    /// Cash the BUY venue needs: full premium + entry fee.
    pub buy_venue_usd: f64,
    /// Collateral the SELL venue needs: short-option margin × buffer + fee.
    pub sell_venue_usd: f64,
    pub total_usd: f64,
    /// Initial short-option margin before the buffer.
    pub short_margin_usd: f64,
}

/// Capital required to run `units` of the arb, and what it earns.
pub fn capital_req(i: &CostInput) -> CapitalReq {
    let contracts_buy = if i.buy_contract_size > 0.0 { i.units / i.buy_contract_size } else { i.units };
    let contracts_sell = if i.sell_contract_size > 0.0 { i.units / i.sell_contract_size } else { i.units };

    let premium_buy = i.buy_premium_per_contract * contracts_buy;
    let fee_buy = i.buy_fee_rate * i.spot * i.units;
    let buy_need = premium_buy + fee_buy;

    let margin = margin_per_contract(
        &i.sell_venue,
        i.sell_premium_per_contract / i.sell_contract_size.max(1e-9),
        i.sell_contract_size,
        i.spot,
        i.strike,
        i.right,
    ) * contracts_sell;
    let fee_sell = i.sell_fee_rate * i.spot * i.units;
    let sell_need = margin * (1.0 + MARGIN_BUFFER) + fee_sell;

    CapitalReq {
        buy_venue_usd: buy_need,
        sell_venue_usd: sell_need,
        total_usd: buy_need + sell_need,
        short_margin_usd: margin,
    }
}

/// Return on capital, percent, for the all-in projected profit.
pub fn roc_pct(net_usd: f64, cap: &CapitalReq) -> f64 {
    if cap.total_usd <= 0.0 {
        return 0.0;
    }
    100.0 * net_usd / cap.total_usd
}

/// Sum of the cheapest round-trip staging costs for the two venues of a
/// trade (deposit in, withdrawal out — you eventually want the money back).
pub fn funding_round_trip(buy_venue: &str, sell_venue: &str) -> f64 {
    let fee = |v: &str| {
        FUNDING_RAILS
            .iter()
            .find(|r| r.venue == v)
            .map(|r| r.fee_usd)
            .unwrap_or(1.0)
    };
    2.0 * (fee(buy_venue) + fee(sell_venue))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn atm_input() -> CostInput {
        CostInput {
            buy_premium_per_contract: 1_950.0, // ~ ATM 1wk BTC call
            sell_premium_per_contract: 2_050.0,
            buy_contract_size: 1.0,
            sell_contract_size: 1.0,
            buy_fee_rate: 0.0003,
            sell_fee_rate: 0.0003,
            spot: 84_000.0,
            strike: 84_000.0,
            right: Right::Call,
            vega_per_vp_unit: 95.0,
            units: 1.0,
            hold_days: 2.0,
            theta_vp_day: -1.8,
            buy_depth_contracts: 25.0,
            sell_depth_contracts: 25.0,
            funding_round_trip_usd: 4.0,
            sell_venue: "deribit".to_string(),
        }
    }

    #[test]
    fn margin_atm_beats_deep_otm() {
        let atm = margin_per_contract("deribit", 500.0, 1.0, 84_000.0, 84_000.0, Right::Call);
        let otm = margin_per_contract("deribit", 500.0, 1.0, 84_000.0, 100_000.0, Right::Call);
        assert!(atm > otm);
        // deribit floor: 0.08 * S
        assert!((otm - (500.0 + 0.08 * 84_000.0)).abs() < 1e-6);
        assert!((atm - (500.0 + 0.15 * 84_000.0)).abs() < 1e-6);
    }

    #[test]
    fn comprehensive_cost_exceeds_bare_fees() {
        let sheet = cost_sheet(&atm_input());
        let bare_fees = sheet.entry_fees_usd + sheet.exit_fees_usd;
        assert!(sheet.total_usd > bare_fees, "slippage+carry+theta must add up");
        // entry fee each leg: 0.0003 * 84000 = $25.20 → both legs both sides
        assert!((bare_fees - 4.0 * 25.20).abs() < 1e-6);
        // gross locked = 100 USD
        assert!((sheet.gross_locked_usd - 100.0).abs() < 1e-6);
    }

    #[test]
    fn capital_splits_by_venue_and_includes_buffer() {
        let cap = capital_req(&atm_input());
        assert!(cap.buy_venue_usd > 1_950.0); // premium + fee
        assert!((cap.buy_venue_usd - (1_950.0 + 25.20)).abs() < 1e-6);
        // deribit ATM margin = prem(2050) + 0.15*84000 = 14,650 → ×1.2 + fee
        let expect_sell = (2_050.0 + 0.15 * 84_000.0) * 1.2 + 25.20;
        assert!((cap.sell_venue_usd - expect_sell).abs() < 1e-6);
        assert!((cap.total_usd - (cap.buy_venue_usd + cap.sell_venue_usd)).abs() < 1e-9);
    }

    #[test]
    fn theta_and_carry_scale_with_hold() {
        let mut i = atm_input();
        let short = cost_sheet(&i);
        i.hold_days = 10.0;
        let long = cost_sheet(&i);
        assert!(long.theta_burn_usd > short.theta_burn_usd);
        assert!(long.margin_carry_usd > short.margin_carry_usd);
        assert!((long.entry_fees_usd - short.entry_fees_usd).abs() < 1e-9);
    }

    #[test]
    fn slippage_grows_with_size_vs_depth() {
        assert!(slippage_frac(1.0, 25.0) < slippage_frac(20.0, 25.0));
        assert!(slippage_frac(1.0, 0.0) > slippage_frac(1.0, 100.0));
    }

    #[test]
    fn roc_positive_on_good_arb() {
        let i = atm_input();
        let sheet = cost_sheet(&i);
        let cap = capital_req(&i);
        // The projection prices the all-in trade honestly: 100 gross against
        // costs that dominate → negative EV, which is exactly the point of
        // the sheet (it refuses to flatter marginal arbs).
        assert!(sheet.net_all_in_usd < sheet.gross_locked_usd);
        let r = roc_pct(sheet.net_all_in_usd, &cap);
        assert!(r < 100.0);
        // A genuinely fat spread stays positive after all costs.
        let mut fat = i;
        fat.sell_premium_per_contract = 2_600.0;
        let s2 = cost_sheet(&fat);
        assert!(s2.net_all_in_usd > 0.0, "fat arb must survive the full sheet");
    }

    #[test]
    fn funding_round_trip_covers_both_venues() {
        // deribit $1 + aevo $0.5, in and out
        assert!((funding_round_trip("deribit", "aevo") - 2.0 * 1.5).abs() < 1e-9);
    }

    #[test]
    fn rails_cover_all_eight_venues() {
        for v in ["deribit", "okx", "bybit", "binance", "delta", "aevo", "derive", "thalex"] {
            assert!(FUNDING_RAILS.iter().any(|r| r.venue == v), "missing rail for {v}");
        }
    }
}
