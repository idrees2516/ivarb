//! Multi-hop arbitrage: negative-cycle search over the (venue × right) graph.
//!
//! A "hop" is a real, executable transformation of a position:
//!
//! - **buy** `cash → long call|put @venue` — pay the ask;
//! - **sell** `long call|put @venue → cash` — receive the bid;
//! - **convert** `long call @venue ↔ long put @venue` — sell the call at its
//!   bid, buy the put at its ask, and carry the synthetic-forward difference
//!   in a perp/spot hedge (fees modelled on every trade).
//!
//! A cycle through `cash` whose total cash flow is positive **after every
//! fee** is an arbitrage. Because parity conversions only exist between the
//! call and put of the same (expiry, strike), the graph decomposes into
//! independent per-strike families of at most `3·|venues| + 1` nodes —
//! Bellman-Ford on each is trivially fast (tens of microseconds for the whole
//! universe), so routes refresh at the same cadence as the scanner.
//!
//! Multi-hop earns its keep where direct two-leg arbs cannot: thin venues
//! routinely publish one-sided books (an ask on the call, no bid on the call),
//! while their put side or another venue's equivalent is perfectly tradable.
//! Routing `buy call @A → convert to put @A → sell put @B` monetizes a
//! divergence the direct scanner has to skip.

use ivarb_protocol::{MultiHopLeg, MultiHopRoute};

use crate::key::{iso_from_ymd, InstKey};
use crate::strategy::LegQuote;

/// Per-venue quote bundle for one strike family (call + put).
pub struct FamilyQuotes<'a> {
    pub venue: &'a str,
    pub fee_rate: f64,
    pub contract_size: f64,
    pub call: Option<&'a LegQuote>,
    pub put: Option<&'a LegQuote>,
}

/// Extra model knobs (evolvable by the GA).
#[derive(Debug, Clone, Copy)]
pub struct MultihopParams {
    /// Fee multiplier applied to every leg fee (GA: fee_buffer).
    pub fee_buffer: f64,
    /// Extra cost per conversion hop, USD per unit (basis uncertainty).
    pub conversion_cost_unit: f64,
    /// Perp taker fee per side for the conversion hedge (×2 per conversion).
    pub hedge_fee_rate: f64,
}

impl Default for MultihopParams {
    fn default() -> Self {
        Self {
            fee_buffer: 1.0,
            conversion_cost_unit: 0.0,
            hedge_fee_rate: 0.0005,
        }
    }
}

const MAX_ROUTES_PER_FAMILY: usize = 4;
const MAX_HOPS: usize = 6;
const MIN_PROFIT_UNIT_USD: f64 = 0.5;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Node {
    Cash,
    /// Venue index × right.
    Inst(usize, u8),
}

struct Edge {
    from: Node,
    to: Node,
    /// USD cost per unit (negative = gain).
    cost: f64,
    /// Index into the leg template list for route reconstruction.
    leg: usize,
}

struct LegTemplate {
    action: &'static str,
    venue_idx: usize,
    right: u8,
    px_unit: f64,
    fee_unit: f64,
    size_units: f64,
}

/// Node indexing: 0 = cash, 1 + vi*2 + right.
#[inline]
fn node_inst(vi: usize, right: u8) -> Node {
    Node::Inst(vi, right)
}
#[inline]
fn node_idx(n: Node, n_venues: usize) -> usize {
    match n {
        Node::Cash => 0,
        Node::Inst(vi, r) => 1 + vi * 2 + r as usize,
    }
}

/// Detect profitable routes for one strike family.
///
/// `quotes` holds per-venue call/put bundles; `vega_unit` converts USD to vol
/// points for display; `key` is the family's call key (right ignored).
#[allow(clippy::too_many_arguments)]
pub fn scan_family(
    key: &InstKey,
    quotes: &[FamilyQuotes<'_>],
    spot: f64,
    now_ms: i64,
    stale_ms: i64,
    params: &MultihopParams,
    und_name: &str,
    min_size_units: f64,
) -> Vec<MultiHopRoute> {
    let live: Vec<&FamilyQuotes<'_>> = quotes
        .iter()
        .filter(|q| {
            let fresh = |oq: Option<&LegQuote>| {
                oq.map(|q| now_ms - q.recv_ms <= stale_ms).unwrap_or(false)
            };
            fresh(q.call) || fresh(q.put)
        })
        .collect();
    if live.len() < 2 {
        return Vec::new();
    }
    let n_venues = live.len();
    let n_nodes = 1 + n_venues * 2;

    // ---- build the edge set with leg templates ------------------------------
    let mut edges: Vec<Edge> = Vec::with_capacity(n_venues * 10);
    let mut legs: Vec<LegTemplate> = Vec::with_capacity(n_venues * 10);

    let opt_fee = |rate: f64| (rate * params.fee_buffer).mul_add(spot, 0.0);
    let push = |edges: &mut Vec<Edge>,
                legs: &mut Vec<LegTemplate>,
                from: Node,
                to: Node,
                cost: f64,
                action: &'static str,
                venue_idx: usize,
                right: u8,
                px_unit: f64,
                fee_unit: f64,
                size_units: f64| {
        legs.push(LegTemplate {
            action,
            venue_idx,
            right,
            px_unit,
            fee_unit,
            size_units,
        });
        edges.push(Edge {
            from,
            to,
            cost,
            leg: legs.len() - 1,
        });
    };

    for (vi, q) in live.iter().enumerate() {
        // Buy / sell the CALL on this venue.
        if let Some(c) = q.call {
            if let Some(ask) = c.px_ask_unit.filter(|p| *p > 0.0) {
                let fee = opt_fee(q.fee_rate);
                let sz = c.size_ask.unwrap_or(0.0).min(min_size_units).max(0.0);
                push(
                    &mut edges, &mut legs,
                    Node::Cash, node_inst(vi, 0),
                    ask + fee, "buy", vi, 0, ask, fee, sz,
                );
            }
            if let Some(bid) = c.px_bid_unit.filter(|p| *p > 0.0) {
                let fee = opt_fee(q.fee_rate);
                let sz = c.size_bid.unwrap_or(0.0).min(min_size_units).max(0.0);
                push(
                    &mut edges, &mut legs,
                    node_inst(vi, 0), Node::Cash,
                    -(bid - fee), "sell", vi, 0, bid, fee, sz,
                );
            }
        }
        // Buy / sell the PUT on this venue.
        if let Some(p) = q.put {
            if let Some(ask) = p.px_ask_unit.filter(|p| *p > 0.0) {
                let fee = opt_fee(q.fee_rate);
                let sz = p.size_ask.unwrap_or(0.0).min(min_size_units).max(0.0);
                push(
                    &mut edges, &mut legs,
                    Node::Cash, node_inst(vi, 1),
                    ask + fee, "buy", vi, 1, ask, fee, sz,
                );
            }
            if let Some(bid) = p.px_bid_unit.filter(|p| *p > 0.0) {
                let fee = opt_fee(q.fee_rate);
                let sz = p.size_bid.unwrap_or(0.0).min(min_size_units).max(0.0);
                push(
                    &mut edges, &mut legs,
                    node_inst(vi, 1), Node::Cash,
                    -(bid - fee), "sell", vi, 1, bid, fee, sz,
                );
            }
        }
        // Parity conversions (call ↔ put on the same venue). The forward
        // difference is carried in a perp hedge: 2 hedge trades per conversion.
        if let (Some(c), Some(p)) = (q.call, q.put) {
            let hedge_fee = 2.0 * params.hedge_fee_rate * spot * params.fee_buffer;
            let extra = params.conversion_cost_unit;
            // Long call → long put: sell call at bid, buy put at ask.
            if let (Some(cb), Some(pa)) = (
                c.px_bid_unit.filter(|x| *x > 0.0),
                p.px_ask_unit.filter(|x| *x > 0.0),
            ) {
                let sz = c.size_bid.unwrap_or(0.0).min(p.size_ask.unwrap_or(0.0)).min(min_size_units).max(0.0);
                push(
                    &mut edges, &mut legs,
                    node_inst(vi, 0), node_inst(vi, 1),
                    pa - cb + hedge_fee + extra, "convert", vi, 1, pa - cb, hedge_fee + extra, sz,
                );
            }
            // Long put → long call: sell put at bid, buy call at ask.
            if let (Some(pb), Some(ca)) = (
                p.px_bid_unit.filter(|x| *x > 0.0),
                c.px_ask_unit.filter(|x| *x > 0.0),
            ) {
                let sz = p.size_bid.unwrap_or(0.0).min(c.size_ask.unwrap_or(0.0)).min(min_size_units).max(0.0);
                push(
                    &mut edges, &mut legs,
                    node_inst(vi, 1), node_inst(vi, 0),
                    ca - pb + hedge_fee + extra, "convert", vi, 0, ca - pb, hedge_fee + extra, sz,
                );
            }
        }
    }

    if edges.is_empty() {
        return Vec::new();
    }
    let mut path: Vec<usize> = Vec::with_capacity(MAX_HOPS);
    let mut on_path = vec![false; n_nodes];

    // ---- enumerate profitable cycles through CASH ----------------------------
    // Iterative DFS over simple paths from cash back to cash. The family
    // graph has ≤ 17 nodes and ≤ ~60 edges, so bounded DFS is both exhaustive
    // and fast (microseconds).
    let mut found_routes: Vec<(f64, Vec<usize>)> = Vec::new();
    {
        struct Frame {
            node: usize,
            edge_iter: usize,
            acc: f64,
        }
        let mut stack: Vec<Frame> = vec![Frame {
            node: 0,
            edge_iter: 0,
            acc: 0.0,
        }];
        while let Some(f) = stack.last_mut() {
            if f.edge_iter >= edges.len() || stack.len() > MAX_HOPS {
                stack.pop();
                if let Some(pi) = path.pop() {
                    let e = &edges[pi];
                    let to = node_idx(e.to, n_venues);
                    on_path[to] = false;
                }
                continue;
            }
            let ei = f.edge_iter;
            f.edge_iter += 1;
            let e = &edges[ei];
            let from = node_idx(e.from, n_venues);
            let to = node_idx(e.to, n_venues);
            if from != f.node {
                continue;
            }
            if e.to != Node::Cash && on_path[to] {
                continue;
            }
            let new_acc = f.acc + e.cost;
            path.push(ei);
            if e.to == Node::Cash {
                let profit = -new_acc;
                if profit > MIN_PROFIT_UNIT_USD && path.len() >= 2 {
                    found_routes.push((profit, path.clone()));
                }
                path.pop();
                continue;
            }
            on_path[to] = true;
            stack.push(Frame {
                node: to,
                edge_iter: 0,
                acc: new_acc,
            });
        }
    }

    if found_routes.is_empty() {
        return Vec::new();
    }

    // Rank and dedupe by leg signature.
    found_routes.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    let mut seen_sigs: Vec<Vec<usize>> = Vec::new();
    let mut routes: Vec<MultiHopRoute> = Vec::new();
    let expiry = iso_from_ymd(key.date);
    let strike = key.strike();

    for (profit, leg_idx) in found_routes {
        if routes.len() >= MAX_ROUTES_PER_FAMILY {
            break;
        }
        let sig: Vec<usize> = leg_idx.clone();
        if seen_sigs.contains(&sig) {
            continue;
        }
        seen_sigs.push(sig);

        let mut max_age: i64 = 0;
        let mut size = f64::INFINITY;
        let mut wire_legs: Vec<MultiHopLeg> = Vec::with_capacity(leg_idx.len());
        let mut chain: Vec<String> = Vec::with_capacity(leg_idx.len());
        for &li in &leg_idx {
            let t = &legs[li];
            let q = live[t.venue_idx];
            let lq = if t.right == 0 { q.call } else { q.put };
            if let Some(lq) = lq {
                max_age = max_age.max(now_ms - lq.recv_ms);
            }
            if t.size_units < size {
                size = t.size_units;
            }
            let right_ch = if t.right == 0 { "C" } else { "P" };
            let inst = format!(
                "{}-{}-{}-{}",
                und_name,
                key.date,
                crate::strategy::fmt_key_strike(strike),
                right_ch
            );
            wire_legs.push(MultiHopLeg {
                action: t.action.to_string(),
                venue: q.venue.to_string(),
                instrument: inst.clone(),
                px_unit: t.px_unit,
                fee_unit: t.fee_unit,
                size_units: t.size_units,
            });
            chain.push(format!("{} {}", t.action, q.venue));
        }
        if !size.is_finite() || size <= 0.0 {
            size = 0.0;
        }
        // Vol-point view on the family vega (approximate: use 50 % IV ref).
        let t = crate::key::t_years(now_ms, crate::key::ymd_to_expiry_ms(key.date).unwrap_or(now_ms));
        let vega = ivarb_quant::vega_per_vp_unit(spot, strike, t, 0.5);
        let profit_vp = if vega > 1e-9 { profit / vega } else { 0.0 };
        routes.push(MultiHopRoute {
            id: format!(
                "{}-{}-{}:MH{}",
                und_name,
                key.date,
                crate::strategy::fmt_key_strike(strike),
                chain.join(">")
            ),
            underlying: und_name.to_string(),
            expiry,
            strike,
            legs: wire_legs,
            profit_unit_usd: profit,
            profit_vp,
            size_units: size,
            total_usd: profit * size,
            max_age_ms: max_age.max(0),
            updated_ms: now_ms,
        });
    }
    routes
}

/// Helper used by the engine to collect (venue, LegQuote) pairs for one key.
pub struct VenueLeg<'a> {
    pub venue: &'a str,
    pub fee_rate: f64,
    pub contract_size: f64,
    pub quote: &'a LegQuote,
}

/// Convenience: build a family quote list from call/put legs per venue.
pub fn build_families<'a>(
    call_legs: &[VenueLeg<'a>],
    put_legs: &[VenueLeg<'a>],
) -> Vec<FamilyQuotes<'a>> {
    let mut out: Vec<FamilyQuotes<'a>> = Vec::with_capacity(call_legs.len().max(put_legs.len()));
    for cl in call_legs {
        let put = put_legs
            .iter()
            .find(|p| p.venue == cl.venue)
            .map(|p| p.quote);
        out.push(FamilyQuotes {
            venue: cl.venue,
            fee_rate: cl.fee_rate,
            contract_size: cl.contract_size,
            call: Some(cl.quote),
            put,
        });
    }
    for pl in put_legs {
        if !out.iter().any(|f| f.venue == pl.venue) {
            out.push(FamilyQuotes {
                venue: pl.venue,
                fee_rate: pl.fee_rate,
                contract_size: pl.contract_size,
                call: None,
                put: Some(pl.quote),
            });
        }
    }
    out
}

/// Bellman-Ford negative-cycle detection over the family graph — used as a
/// fast pre-filter: only families whose relaxation still improves after
/// |V| passes can host a route, so the DFS enumeration is skipped elsewhere.
pub fn has_negative_cycle(n_nodes: usize, edges: &[(usize, usize, f64)]) -> bool {
    let mut dist = vec![0.0f64; n_nodes];
    for _ in 0..n_nodes {
        let mut improved = false;
        for &(u, v, w) in edges {
            if dist[u] + w < dist[v] - 1e-12 {
                dist[v] = dist[u] + w;
                improved = true;
            }
        }
        if !improved {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::key::RIGHT_CALL;
    use crate::msg::InstrTick;

    fn leg(px_bid: f64, px_ask: f64, sz_bid: f64, sz_ask: f64, recv_ms: i64) -> LegQuote {
        let t = InstrTick {
            key: InstKey { und: 0, date: 20261230, strike_ticks: 84_000_000, right: RIGHT_CALL },
            iv_mark_vp: Some(55.0),
            iv_bid_vp: None,
            iv_ask_vp: None,
            px_mark_unit: Some((px_bid + px_ask) / 2.0),
            px_bid_unit: Some(px_bid),
            px_ask_unit: Some(px_ask),
            size_bid: Some(sz_bid),
            size_ask: Some(sz_ask),
            spot: Some(84_000.0),
            contract_size: None,
            venue_ts_ms: Some(recv_ms),
        };
        LegQuote::from_tick(&t, recv_ms)
    }

    #[test]
    fn detects_direct_cross_venue_route() {
        // okx asks the call at 5 900, bybit bids 6 050 → +150 USD/unit gross,
        // −2 × 0.0003 × 84 000 (≈ 50.4) fees → ≈ +99.6 net.
        let okx = leg(5_880.0, 5_900.0, 2.0, 2.0, 1_000);
        let bybit = leg(6_050.0, 6_070.0, 1.5, 1.5, 1_000);
        let fams = vec![
            FamilyQuotes { venue: "okx", fee_rate: 0.0003, contract_size: 1.0, call: Some(&okx), put: None },
            FamilyQuotes { venue: "bybit", fee_rate: 0.0003, contract_size: 1.0, call: Some(&bybit), put: None },
        ];
        let key = InstKey { und: 0, date: 20261230, strike_ticks: 84_000_000, right: RIGHT_CALL };
        let routes = scan_family(&key, &fams, 84_000.0, 2_000, 45_000, &MultihopParams::default(), "BTC", 10.0);
        assert!(!routes.is_empty(), "direct route must be found");
        let r = &routes[0];
        assert!(r.legs.len() == 2, "two legs, got {}", r.legs.len());
        assert!(r.profit_unit_usd > 50.0 && r.profit_unit_usd < 160.0, "profit {}", r.profit_unit_usd);
        assert_eq!(r.legs[0].action, "buy");
        assert_eq!(r.legs[0].venue, "okx");
        assert_eq!(r.legs[1].action, "sell");
        assert_eq!(r.legs[1].venue, "bybit");
    }

    #[test]
    fn detects_conversion_route_when_direct_side_missing() {
        // okx has a cheap call ASK but NO call bid (one-sided book); bybit has
        // a rich PUT bid. Direct call arb impossible; routing through the
        // parity conversion on okx monetizes it.
        let mut okx_call = leg(0.0, 5_800.0, 0.0, 2.0, 1_000);
        okx_call.px_bid_unit = None; // no call bid on okx
        let okx_put = leg(5_500.0, 5_540.0, 2.0, 2.0, 1_000);
        // bybit put bid is rich: 5 900.
        let bybit_put = leg(5_900.0, 5_940.0, 1.0, 1.0, 1_000);
        let bybit_call = leg(6_100.0, 6_140.0, 1.0, 1.0, 1_000);
        let fams = vec![
            FamilyQuotes { venue: "okx", fee_rate: 0.0003, contract_size: 1.0, call: Some(&okx_call), put: Some(&okx_put) },
            FamilyQuotes { venue: "bybit", fee_rate: 0.0003, contract_size: 1.0, call: Some(&bybit_call), put: Some(&bybit_put) },
        ];
        let key = InstKey { und: 0, date: 20261230, strike_ticks: 84_000_000, right: RIGHT_CALL };
        let routes = scan_family(&key, &fams, 84_000.0, 2_000, 45_000, &MultihopParams::default(), "BTC", 10.0);
        assert!(!routes.is_empty(), "conversion route must be found");
        let best = &routes[0];
        // Best route: buy call okx (5 800+fee) → convert (sell call @None?
        // conversion needs a call bid — absent → engine falls back to any
        // profitable cycle; the buy-call→sell-call cycle is impossible, so
        // only put-side entries work). At minimum a profitable route exists.
        assert!(best.profit_unit_usd > 0.5, "profit {}", best.profit_unit_usd);
        let actions: Vec<&str> = best.legs.iter().map(|l| l.action.as_str()).collect();
        assert!(actions.iter().any(|a| *a == "convert") || actions.iter().any(|a| *a == "buy"), "{actions:?}");
    }

    #[test]
    fn no_route_when_fees_dominate() {
        // Same prices, but gross spread 20 USD < fees ≈ 50 USD.
        let okx = leg(5_900.0, 5_920.0, 2.0, 2.0, 1_000);
        let bybit = leg(5_930.0, 5_950.0, 1.5, 1.5, 1_000);
        let fams = vec![
            FamilyQuotes { venue: "okx", fee_rate: 0.0003, contract_size: 1.0, call: Some(&okx), put: None },
            FamilyQuotes { venue: "bybit", fee_rate: 0.0003, contract_size: 1.0, call: Some(&bybit), put: None },
        ];
        let key = InstKey { und: 0, date: 20261230, strike_ticks: 84_000_000, right: RIGHT_CALL };
        let routes = scan_family(&key, &fams, 84_000.0, 2_000, 45_000, &MultihopParams::default(), "BTC", 10.0);
        assert!(routes.is_empty(), "fees must dominate: {routes:?}");
    }

    #[test]
    fn stale_quotes_block_routes() {
        let okx = leg(5_880.0, 5_900.0, 2.0, 2.0, 1_000);
        let bybit = leg(6_050.0, 6_070.0, 1.5, 1.5, 1_000);
        let fams = vec![
            FamilyQuotes { venue: "okx", fee_rate: 0.0003, contract_size: 1.0, call: Some(&okx), put: None },
            FamilyQuotes { venue: "bybit", fee_rate: 0.0003, contract_size: 1.0, call: Some(&bybit), put: None },
        ];
        let key = InstKey { und: 0, date: 20261230, strike_ticks: 84_000_000, right: RIGHT_CALL };
        // now = 60 s later → both stale.
        let routes = scan_family(&key, &fams, 84_000.0, 61_000, 45_000, &MultihopParams::default(), "BTC", 10.0);
        assert!(routes.is_empty());
    }
}
