//! Research panels: market pulse, consolidated chain browser, liquidity
//! heatmap, and the Paradex universe strip.

use leptos::prelude::*;

use ivarb_protocol::research::{ChainRow, LiquidityCell, ResearchVenueStat};
use ivarb_protocol::Right;

use super::chat::expiry_label;
use super::{rs_usd, Rs};
use crate::fmt;

#[component]
pub fn PulsePanel(rs: Rs) -> impl IntoView {
    view! {
        <div class="rs-pulse">
            {move || {
                let Some(p) = rs.pulse.get() else {
                    return view! { <div class="rs-loading">"collecting the consolidated book…"</div> }.into_any();
                };
                let spots = p.spots.iter().map(|s| {
                    let chg = s.change_24h_pct.map(|c| format!("{c:+.1}%")).unwrap_or_else(|| "—".into());
                    let cls = s.change_24h_pct.map(|c| if c >= 0.0 { "pos" } else { "neg" }).unwrap_or("dim");
                    view! {
                        <div class="rs-stat">
                            <div class="rs-stat-label">{s.und.clone()}</div>
                            <div class="rs-stat-value">{fmt::fmt_spot(s.price)}</div>
                            <div class=format!("rs-stat-sub {cls}")>{chg}" 24h"</div>
                        </div>
                    }
                }).collect::<Vec<_>>();
                let iv_cards = p.iv.iter().map(|s| {
                    let vr = s.vr_premium_vp.map(|v| format!("{v:+.1}vp")).unwrap_or_else(|| "—".into());
                    let vr_cls = s.vr_premium_vp.map(|v| if v >= 8.0 { "rich" } else if v <= -3.0 { "cheap" } else { "fair" }).unwrap_or("dim");
                    view! {
                        <div class="rs-stat">
                            <div class="rs-stat-label">{format!("{} ATM30", s.und)}</div>
                            <div class="rs-stat-value">{s.atm_iv_vp.map(|v| format!("{v:.1}vp")).unwrap_or_else(|| "—".into())}</div>
                            <div class=format!("rs-stat-sub {vr_cls}")>
                                {format!("VR {vr} · pct {} ({})", s.iv_percentile.map(|x| format!("{x:.0}")).unwrap_or_else(|| "—".into()), s.iv_percentile_basis)}
                            </div>
                        </div>
                    }
                }).collect::<Vec<_>>();
                view! {
                    <div class="rs-stat-row">{spots}{iv_cards}
                        <div class="rs-stat">
                            <div class="rs-stat-label">"venues live"</div>
                            <div class="rs-stat-value">{format!("{}/{}", p.venues_live, p.venue_stats.len())}</div>
                            <div class="rs-stat-sub dim">{format!("{} instruments", fmt::fmt_int(p.instruments as f64))}</div>
                        </div>
                    </div>
                    <VenueTable stats=p.venue_stats.clone()/>
                    <ParadexStrip rs=rs.clone()/>
                }.into_any()
            }}
        </div>
    }
}

#[component]
fn VenueTable(stats: Vec<ResearchVenueStat>) -> impl IntoView {
    view! {
        <div class="rs-venue-table-wrap">
            <table class="rs-venue-table">
                <thead>
                    <tr>
                        <th>"venue"</th>
                        <th>"instruments"</th>
                        <th>"ATM IV"</th>
                        <th>"avg spread"</th>
                        <th>"open interest (est.)"</th>
                        <th>"24h volume (est.)"</th>
                        <th>"status"</th>
                    </tr>
                </thead>
                <tbody>
                    {stats.iter().map(|v| {
                        let cls = if v.is_sim { "sim" } else if v.online { "live" } else { "warn" };
                        view! {
                            <tr>
                                <td>
                                    <span class="rs-venue-name">
                                        {fmt::venue_name(&v.venue)}
                                        {if v.is_sim { view! { <span class="rs-sim-chip">"SIM"</span> }.into_any() } else { ().into_any() }}
                                        {if v.enrichment { view! { <span class="rs-enr-chip" title="research REST enrichment (OI / volume) live">"ENR"</span> }.into_any() } else { ().into_any() }}
                                    </span>
                                </td>
                                <td class="num">{fmt::fmt_int(v.instruments as f64)}</td>
                                <td class="num">{v.atm_iv_vp.map(|x| format!("{x:.1}vp")).unwrap_or_else(|| "—".into())}</td>
                                <td class="num">{v.avg_spread_pct.map(|x| format!("{x:.2}%")).unwrap_or_else(|| "—".into())}</td>
                                <td class="num">{v.oi_usd.map(|x| format!("${:.2}m", x / 1e6)).unwrap_or_else(|| "—".into())}</td>
                                <td class="num">{v.volume_usd_24h.map(|x| format!("${:.2}m", x / 1e6)).unwrap_or_else(|| "—".into())}</td>
                                <td><span class=format!("rs-pill {cls}")>{if v.is_sim { "sim" } else if v.online { "live" } else { "?" } }</span></td>
                            </tr>
                        }.into_any()
                    }).collect::<Vec<_>>()}
                </tbody>
            </table>
        </div>
        <p class="dim rs-note">"OI and volume are marked to USD with each venue's contract size — indicative, not settlement-grade. Prices, IVs and books come from the live engine feed."</p>
    }
}

#[component]
fn ParadexStrip(rs: Rs) -> impl IntoView {
    view! {
        {move || {
            let s = rs.status.get();
            let Some(s) = s else { return ().into_any() };
            let quotes = if s.paradex_quotes {
                view! { <span class="rs-pill live" title="Unauthenticated public WebSocket (SBE market-summary feed) — no JWT, no expiry">"quotes live · public WS"</span> }.into_any()
            } else if s.paradex_ws {
                view! { <span class="rs-pill warn" title="Public WS connected; waiting for the first fresh summaries">"WS warming up"</span> }.into_any()
            } else {
                view! { <span class="rs-pill warn" title="Public WS transport down — reconnecting with backoff, quotes resume automatically">"WS reconnecting"</span> }.into_any()
            };
            let book = if s.paradex_book {
                view! {
                    <span class="rs-pill live tk-pill-50"
                          title="order_book.{market}.snapshot@15@50ms — full 15-level API book snapshots on every change, exchange-throttled to 50 ms per market. Feeds the execution-routing BBO.">
                        {format!("50ms book · {} subs", s.paradex_book_subs)}
                    </span>
                }.into_any()
            } else {
                view! { <span class="rs-pill warn" title="50 ms order-book tick feed reconnecting">"50ms book …"</span> }.into_any()
            };
            let derive = if s.derive_ws {
                view! { <span class="rs-pill live" title="Derive v3 WebSocket ticker_slim.{instrument}.100 — best bid/ask + sizes + venue IVs on the exchange's 100 ms interval">{format!("derive v3 · {} ticks", s.derive_subs)}</span> }.into_any()
            } else {
                view! { <span class="rs-pill warn" title="Derive v3 ticker feed reconnecting">"derive v3 …"</span> }.into_any()
            };
            view! {
                <div class="rs-paradex">
                    <span class="rs-paradex-name">"Paradex TradeOptions universe"</span>
                    <span class="rs-paradex-count">{format!("{} option markets", fmt::fmt_int(s.paradex_markets as f64))}</span>
                    {quotes}
                    {book}
                    {derive}
                    <span class="dim">{format!("— {}", s.tick_note)}</span>
                </div>
            }.into_any()
        }}
    }
}

// ---------------------------------------------------------------------------
// Tick-level BBO (50 ms-class feeds) — the execution-routing plane
// ---------------------------------------------------------------------------

#[component]
pub fn TicksPanel(rs: Rs) -> impl IntoView {
    view! {
        <div class="rs-ticks">
            {move || {
                let Some(t) = rs.ticks.get() else {
                    return view! { <div class="rs-loading">"connecting the tick plane…"</div> }.into_any();
                };
                // Feed chips: transport health + live counters.
                let feeds = t.feeds.iter().map(|f| {
                    let cls = if f.connected { "live" } else { "warn" };
                    let what = if f.venue == "paradex" {
                        "Paradex order_book @15@50ms — public, SBE, no auth"
                    } else {
                        "Derive v3 ticker_slim @100ms — public JSON-RPC"
                    };
                    view! {
                        <span class=format!("tk-feed {}", cls) title=what>
                            <span class="tk-feed-name">{fmt::venue_name(&f.venue)}</span>
                            <span class="tk-feed-sep">"·"</span>
                            {if f.venue == "paradex" { "50ms book" } else { "100ms ticker" }}
                            <span class="tk-feed-sep">"·"</span>
                            {format!("{} subs", f.subscriptions)}
                            <span class="tk-feed-sep">"·"</span>
                            {format!("{} frames", fmt::fmt_int(f.frames as f64))}
                            <span class="tk-feed-sep">"·"</span>
                            <span class="tk-feed-age">{format!("last {} ms ago", fmt::fmt_int(f.age_ms as f64))}</span>
                        </span>
                    }.into_any()
                }).collect::<Vec<_>>();
                if t.rows.is_empty() {
                    return view! {
                        <div class="tk-feeds">{feeds}</div>
                        <div class="rs-loading">
                            "tick plane connected — waiting for the first 50 ms-class books (option markets tick when quotes move)…"
                        </div>
                    }.into_any();
                }
                // Routing readout: per (date, strike, right) the best ask to BUY
                // and the best bid to SELL across the tick venues.
                let mut best_ask: Vec<(String, f64)> = Vec::new(); // (key, ask)
                let mut best_bid: Vec<(String, f64)> = Vec::new();
                for r in &t.rows {
                    let k2 = format!("{}-{}-{}", r.date, fmt::fmt_strike(r.strike), r.right.letter());
                    if let Some(a) = r.ask {
                        if !best_ask.iter().any(|(k, v)| k == &k2 && *v <= a) {
                            best_ask.retain(|(k, _)| k != &k2);
                            best_ask.push((k2.clone(), a));
                        }
                    }
                    if let Some(b) = r.bid {
                        if !best_bid.iter().any(|(k, v)| k == &k2 && *v >= b) {
                            best_bid.retain(|(k, _)| k != &k2);
                            best_bid.push((k2.clone(), b));
                        }
                    }
                }
                // Precompute owned display rows (Leptos 0.8: closures own data).
                let view_rows: Vec<(String, String, String, String, String, String, String, String, String, bool, bool, String, String)> = t.rows.iter().map(|r| {
                    let k2 = format!("{}-{}-{}", r.date, fmt::fmt_strike(r.strike), r.right.letter());
                    let is_buy = best_ask.iter().any(|(k, v)| k == &k2 && Some(*v) == r.ask);
                    let is_sell = best_bid.iter().any(|(k, v)| k == &k2 && Some(*v) == r.bid);
                    (
                        format!("{} {}", fmt::fmt_strike(r.strike), r.right.letter()),
                        expiry_label(r.date, 0),
                        fmt::venue_name(&r.venue),
                        r.bid.map(|b| format!("{} × {}", fmt::fmt_usd(b), sz(r.bid_size))).unwrap_or_else(|| "—".into()),
                        r.ask.map(|a| format!("{} × {}", fmt::fmt_usd(a), sz(r.ask_size))).unwrap_or_else(|| "—".into()),
                        r.spread_pct.map(|s| format!("{s:.1}%")).unwrap_or_else(|| "—".into()),
                        r.iv_mark_vp.map(|v| format!("{v:.1}vp")).unwrap_or_else(|| "—".into()),
                        format!("{} ms", fmt::fmt_int(r.age_ms as f64)),
                        if r.seq > 0 { format!("#{}", fmt::fmt_int(r.seq as f64)) } else { "—".into() },
                        is_buy,
                        is_sell,
                        format!("tk-row {}", age_class(r.age_ms)),
                        format!("num {}", age_class(r.age_ms)),
                    )
                }).collect();
                let n_rows = view_rows.len();
                view! {
                    <div class="tk-feeds">{feeds}</div>
                    <div class="tk-sub dim">
                        {format!(
                            "Execution-routing BBO for {} — spot {}. BUY rows mark the lowest ask and SELL rows the highest bid across the tick venues; the consolidated chain routes legs on these quotes. Age is measured from each venue's own frame timestamp.",
                            t.und, fmt::fmt_spot(t.spot),
                        )}
                    </div>
                    <div class="tk-table-wrap">
                        <table class="tk-table">
                            <thead>
                                <tr>
                                    <th>"strike"</th>
                                    <th>"expiry"</th>
                                    <th>"venue"</th>
                                    <th class="num">"bid × size"</th>
                                    <th class="num">"ask × size"</th>
                                    <th class="num">"spread"</th>
                                    <th class="num">"IV"</th>
                                    <th class="num">"age"</th>
                                    <th class="num">"seq"</th>
                                    <th></th>
                                </tr>
                            </thead>
                            <tbody>
                                {view_rows.into_iter().map(|(strike, exp, venue, bid, ask, spr, iv, age, seq, is_buy, is_sell, row_cls, cell_cls)| {
                                    view! {
                                        <tr class=move || row_cls.clone()>
                                            <td class="num strike">{strike.clone()}</td>
                                            <td class="dim">{exp.clone()}</td>
                                            <td>{venue.clone()}{if is_buy { view! { <span class="tk-chip buy" title="lowest ask across tick venues — buy leg routes here">"BUY"</span> }.into_any() } else { ().into_any() }}{if is_sell { view! { <span class="tk-chip sell" title="highest bid across tick venues — sell leg routes here">"SELL"</span> }.into_any() } else { ().into_any() }}</td>
                                            <td class="num pos">{bid.clone()}</td>
                                            <td class="num neg">{ask.clone()}</td>
                                            <td class="num dim">{spr.clone()}</td>
                                            <td class="num dim">{iv.clone()}</td>
                                            <td class=move || cell_cls.clone()>{age.clone()}</td>
                                            <td class="num dim">{seq.clone()}</td>
                                            <td></td>
                                        </tr>
                                    }.into_any()
                                }).collect::<Vec<_>>()}
                            </tbody>
                        </table>
                    </div>
                    <p class="dim rs-note">{format!(
                        "{} tick quotes · paradex books arrive as full 15-level API-book snapshots (RPI excluded), throttled by the exchange to one frame per 50 ms per market — active markets tick at 50–150 ms, quiet option books tick when quotes move. Derive v3 tickers publish on the exchange's 100 ms interval.",
                        n_rows,
                    )}</p>
                }.into_any()
            }}
        </div>
    }
}

fn sz(v: Option<f64>) -> String {
    v.map(|x| if x >= 100.0 { format!("{x:.0}") } else { format!("{x:.2}") }).unwrap_or_else(|| "—".into())
}

fn age_class(age_ms: i64) -> String {
    if age_ms <= 150 { "age-fresh" } else if age_ms <= 1_000 { "age-ok" } else { "age-old" }.into()
}

// ---------------------------------------------------------------------------
// Chain browser
// ---------------------------------------------------------------------------

#[component]
pub fn ChainPanel(rs: Rs) -> impl IntoView {
    let expiries = move || {
        rs.expiry_list
            .get()
            .iter()
            .take(6)
            .map(|(d, iso, dte)| (*d, iso.clone(), *dte))
            .collect::<Vec<_>>()
    };

    view! {
        <div class="rs-chain">
            <div class="rs-chain-controls">
                <div class="rs-exp-chips">
                    {move || {
                        let cur = rs.expiry.get();
                        let exps = expiries();
                        exps.into_iter().map(|(d, iso, dte)| {
                            let active = cur == Some(d);
                            view! {
                                <button class=move || format!("rs-exp-chip{}", if active { " on" } else { "" })
                                    on:click=move |_| rs.expiry.set(Some(d))>
                                    {format!("{iso} · {dte}d")}
                                </button>
                            }.into_any()
                        }).collect::<Vec<_>>()
                    }}
                </div>
                <div class="rs-right-chips">
                    {move || {
                        let cur = rs.right_filter.get();
                        [(None::<Right>, "C+P"), (Some(Right::Call), "Calls"), (Some(Right::Put), "Puts")].iter().map(|(r, label)| {
                            let active = *r == cur;
                            view! {
                                <button class=move || format!("rs-exp-chip{}", if active { " on" } else { "" })
                                    on:click=move |_| rs.right_filter.set(*r)>
                                    {*label}
                                </button>
                            }.into_any()
                        }).collect::<Vec<_>>()
                    }}
                </div>
            </div>
            {move || {
                let Some(c) = rs.chain.get() else {
                    return view! { <div class="rs-loading">"loading the consolidated chain…"</div> }.into_any();
                };
                let spot = c.spot;
                let right_filter = rs.right_filter.get();
                let rows: Vec<ChainRow> = c.rows.iter()
                    .filter(|r| right_filter.map(|f| r.right == f).unwrap_or(true))
                    .cloned()
                    .collect();
                if rows.is_empty() {
                    return view! { <div class="rs-loading">"no rows for this filter (yet)"</div> }.into_any();
                }
                // ATM strike = closest to spot.
                let atm = rows.iter().map(|r| r.strike).fold(f64::INFINITY, |a, k| {
                    if (k - spot).abs() < (a - spot).abs() { k } else { a }
                });
                // Precompute every display cell (owned) so no view closure
                // borrows the signal-owned response.
                let view_rows: Vec<(bool, String, ivarb_protocol::Right, String, String, String, String, String, String, usize)> = rows.iter().map(|r| {
                    // 50 ms-class badge: routing ran on tick-fresh quotes.
                    let tick_n = r.venues.iter().filter(|v| v.tick_age_ms.map(|a| a < 2_000.0).unwrap_or(false)).count();
                    let route = format!(
                        "{}/{}{}",
                        r.best_bid_venue.as_deref().map(fmt::venue_name).unwrap_or_else(|| "—".into()),
                        r.best_ask_venue.as_deref().map(fmt::venue_name).unwrap_or_else(|| "—".into()),
                        if tick_n > 0 { format!(" ⚡{}", tick_n) } else { String::new() },
                    );
                    (
                        (r.strike - atm).abs() < 1e-9,
                        fmt::fmt_strike(r.strike),
                        r.right,
                        r.iv_consensus_vp.map(|v| format!("{v:.1}")).unwrap_or_else(|| "—".into()),
                        r.iv_spread_vp.map(|v| format!("{v:.1}vp")).unwrap_or_else(|| "—".into()),
                        r.best_bid_px.map(rs_usd).unwrap_or_else(|| "—".into()),
                        r.best_ask_px.map(rs_usd).unwrap_or_else(|| "—".into()),
                        route,
                        r.open_interest.map(|o| fmt::fmt_int(o)).unwrap_or_else(|| "—".into()),
                        r.venues.len(),
                    )
                }).collect();
                view! {
                    <div class="rs-chain-table-wrap">
                        <table class="rs-chain-table">
                            <thead>
                                <tr>
                                    <th>"strike"</th>
                                    <th></th>
                                    <th class="num">"IV"</th>
                                    <th class="num">"x-venue Δ"</th>
                                    <th class="num">"best bid"</th>
                                    <th class="num">"best ask"</th>
                                    <th class="num">"route"</th>
                                    <th class="num">"OI"</th>
                                    <th class="num">"venues"</th>
                                </tr>
                            </thead>
                            <tbody>
                                {view_rows.into_iter().map(|(is_atm, strike_s, right, iv, spread, bid, ask, route, oi, n_venues)| {
                                    view! {
                                        <tr class=move || if is_atm { "atm" } else { "" }>
                                            <td class="num strike">{strike_s.clone()}</td>
                                            <td><span class=move || format!("rs-right-chip {}", if right == Right::Call { "c" } else { "p" })>{right.letter()}</span></td>
                                            <td class="num">{iv.clone()}</td>
                                            <td class="num dim">{spread.clone()}</td>
                                            <td class="num pos">{bid.clone()}</td>
                                            <td class="num neg">{ask.clone()}</td>
                                            <td class="num dim">{route.clone()}</td>
                                            <td class="num">{oi.clone()}</td>
                                            <td class="num dim">{n_venues}</td>
                                        </tr>
                                    }.into_any()
                                }).collect::<Vec<_>>()}
                            </tbody>
                        </table>
                    </div>
                    <p class="dim rs-note">{format!(
                        "Consolidated near-ATM chain for {} — spot {}. IV is the cross-venue median mark; route shows which venue pays the best bid / asks the lowest offer. ⚡n marks rows whose routing ran on tick-fresh (50 ms-class) books.",
                        c.und, fmt::fmt_spot(c.spot),
                    )}</p>
                }.into_any()
            }}
        </div>
    }
}

// ---------------------------------------------------------------------------
// Liquidity heatmap
// ---------------------------------------------------------------------------

#[component]
pub fn LiquidityPanel(rs: Rs) -> impl IntoView {
    view! {
        <div class="rs-liq">
            {move || {
                let Some(m) = rs.liquidity.get() else {
                    return view! { <div class="rs-loading">"sampling top-strike order books…"</div> }.into_any();
                };
                if m.cells.is_empty() {
                    return view! { <div class="rs-loading">"no book samples for this underlying (yet)"</div> }.into_any();
                }
                let max_depth = m.cells.iter().filter_map(|c| c.depth_usd).fold(0.0_f64, f64::max).max(1.0);
                let max_oi = m.cells.iter().filter_map(|c| c.oi_usd).fold(0.0_f64, f64::max).max(1.0);
                let cells = m.cells.clone();
                let expiries = m.expiries.clone();
                let strikes = m.strikes.clone();
                view! {
                    <div class="rs-heat-wrap">
                        <table class="rs-heat">
                            <thead>
                                <tr>
                                    <th>"strike"</th>
                                    {expiries.iter().map(|d| {
                                        let label = cells.iter().find(|c| c.date == *d).map(|c| expiry_label(c.date, c.dte)).unwrap_or_default();
                                        view! { <th>{label}</th> }.into_any()
                                    }).collect::<Vec<_>>()}
                                </tr>
                            </thead>
                            <tbody>
                                {strikes.iter().map(|k| {
                                    view! {
                                        <tr>
                                            <td class="num strike">{fmt::fmt_strike(*k)}</td>
                                            {expiries.iter().map(|d| {
                                                let cell = cells.iter().find(|c| c.strike == *k && c.date == *d);
                                                view! {
                                                    <td>
                                                        {match cell {
                                                            None => view! { <span class="dim">"·"</span> }.into_any(),
                                                            Some(c) => heat_cell(c, max_depth, max_oi),
                                                        }}
                                                    </td>
                                                }.into_any()
                                            }).collect::<Vec<_>>()}
                                        </tr>
                                    }.into_any()
                                }).collect::<Vec<_>>()}
                            </tbody>
                        </table>
                    </div>
                    <div class="rs-heat-legend dim">
                        <span class="rs-heat-legend-title">"depth near the touch"</span>
                        <span class="rs-heat-sw sw0">"≤$10k"</span>
                        <span class="rs-heat-sw sw1">"$50k"</span>
                        <span class="rs-heat-sw sw2">"$200k"</span>
                        <span class="rs-heat-sw sw3">"$1m+"</span>
                        <span>"— OI tint behind the number. C = call side, P = put side."</span>
                    </div>
                    <p class="dim rs-note">"Depth = USD resting within 2% of the touch on Deribit / Binance / OKX books where reachable, else the quoted touch size across every venue in the consolidated book. Route legs through the venue paying best per side."</p>
                }.into_any()
            }}
        </div>
    }
}

fn heat_cell(c: &LiquidityCell, max_depth: f64, max_oi: f64) -> AnyView {
    let depth = c.depth_usd.unwrap_or(0.0);
    let oi = c.oi_usd.unwrap_or(0.0);
    // Log-ish intensity buckets.
    let d_lvl = if depth <= 0.0 {
        0
    } else if depth < 10_000.0 {
        1
    } else if depth < 50_000.0 {
        2
    } else if depth < 200_000.0 {
        3
    } else if depth < 1_000_000.0 {
        4
    } else {
        5
    };
    let _ = max_depth;
    let oi_frac = (oi / max_oi).clamp(0.0, 1.0);
    let txt = if depth >= 1_000_000.0 {
        format!("${:.1}m", depth / 1e6)
    } else if depth >= 1_000.0 {
        format!("${:.0}k", depth / 1e3)
    } else {
        "—".into()
    };
    let right = c.right;
    let title = format!(
        "{} {:.1} OI-est ${:.0}k · bid {} · ask {}",
        right.letter(),
        c.strike,
        oi / 1e3,
        c.best_bid_venue.as_deref().map(fmt::venue_name).unwrap_or_else(|| "—".into()),
        c.best_ask_venue.as_deref().map(fmt::venue_name).unwrap_or_else(|| "—".into()),
    );
    let _ = oi_frac;
    view! {
        <span class=move || format!("rs-heat-cell d{}", d_lvl).to_string()
              title=title.clone()>
            <span class=move || format!("rs-heat-r {}", if right == Right::Call { "c" } else { "p" })>{right.letter()}</span>
            <span class="rs-heat-v">{txt}</span>
        </span>
    }
    .into_any()
}
