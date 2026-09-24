//! Section 03 — The Hold-to-Expiry Lab.
//!
//! Not every divergence clears the arb gate: when fees eat the spread, the
//! two-leg trade is unprofitable — but the *cheap leg itself* can still be a
//! positive-EV outright long-vol position. This lab surfaces those near-misses
//! and prices what keeping the option to expiry actually costs and pays:
//! theta bleed per day, maximum quiet-case loss (extrinsic), expected value
//! under the cross-venue consensus vol, break-even move and P(profit) — with a
//! full antithetic Monte-Carlo terminal P&L distribution run in your browser.

use leptos::prelude::*;

use crate::fmt;
use crate::state::ApiState;
use ivarb_protocol::HoldCandidate;

#[component]
pub fn HoldLabSection() -> impl IntoView {
    let st = use_context::<ApiState>().unwrap();
    let holds = Memo::new(move |_| {
        let mut v = st.holds.get();
        v.sort_by(|a, b| b.hold.ev_usd.total_cmp(&a.hold.ev_usd));
        v.truncate(24);
        v
    });
    let now = st.now;

    view! {
        <section class="section" id="holdlab">
            <crate::app::SectionHead num="03"
                title="Hold-to-expiry lab — when the arb fails but the option is still cheap"
                sub="These divergences sit below the 0.5 vp fee-adjusted arb gate: round-tripping both legs loses money. But buying only the cheap leg and holding to expiry can still pay — here is the exact profit and cost of that strategy, computed live."/>
            <div class="hold-intro">
                <p>
                    "A near-miss is not a dead signal. The cheap venue's ask still implies a lower vol than the "
                    "cross-venue consensus — you are buying vol below its fair value. The cost is theta: the option "
                    "bleeds every day the market sits still, and the maximum quiet-case loss is the extrinsic premium "
                    "you paid. Every row below shows both sides of that trade-off: what the arb view would have made "
                    "(negative), and what the hold view is expected to make (EV, P&L distribution, break-even)."
                </p>
            </div>
            {move || {
                let rows = holds.get();
                if rows.is_empty() {
                    view! {
                        <div class="detail-empty">
                            <p>"No near-miss candidates right now — every tracked divergence either clears the arb gate "
                               "(see the scanner above) or is too small to be worth holding. Candidates appear here "
                               "automatically as quotes diverge below the gate."</p>
                        </div>
                    }.into_any()
                } else {
                    view! {
                        <div class="hold-table-wrap">
                            <table class="scan-table hold-table">
                                <thead>
                                    <tr>
                                        <th>"Instrument"</th>
                                        <th>"Cheapest ask"</th>
                                        <th>"Ask premium"</th>
                                        <th class="num">"IV edge"</th>
                                        <th class="num">"Arb net (for contrast)"</th>
                                        <th class="num">"Hold EV / ct"</th>
                                        <th class="num">"Theta / day"</th>
                                        <th class="num">"Max bleed"</th>
                                        <th class="num">"P(profit)"</th>
                                        <th>"DTE"</th>
                                        <th></th>
                                    </tr>
                                </thead>
                                <tbody>
                                    <For each=move || holds.get() key=|c| c.inst_id.clone() let:c>
                                        {let c2 = c.clone();
                                         let sel_id = c.inst_id.clone();
                                         let sel = Memo::new(move |_| st.selected_hold.get().as_deref() == Some(sel_id.as_str()));
                                         view! {
                                            <tr class=move || {
                                                let mut cls = String::from("hold-row");
                                                if sel.get() { cls.push_str(" sel"); }
                                                if c2.hold.ev_usd > 0.0 { cls.push_str(" pos-ev"); }
                                                cls
                                            }
                                                on:click=move |_| {
                                                    let id = c.inst_id.clone();
                                                    st.selected_hold.set(Some(id));
                                                }>
                                                <td class="inst">
                                                    <span class="i-und">{c.underlying.clone()}</span>
                                                    <span class="i-date">{c.expiry.clone()}</span>
                                                    <span class="i-strike">{fmt::fmt_strike(c.strike)}{" "}{fmt::fmt_right(&c.right)}</span>
                                                </td>
                                                <td>{fmt::venue_name(&c.venue)}</td>
                                                <td class="num">{fmt::fmt_usd(c.ask_px_unit * c.contract_size)}</td>
                                                <td class=move || format!("num {}", if c.hold.iv_edge_vp >= 0.5 { "pos" } else { "neg" })>
                                                    {format!("{:+.2} vp", c.hold.iv_edge_vp)}
                                                </td>
                                                <td class="num neg">{format!("{:+.2} vp", c.arb_net_vp)}</td>
                                                <td class=move || format!("num {}", if c.hold.ev_usd >= 0.0 { "pos" } else { "neg" })>
                                                    {fmt::fmt_usd(c.hold.ev_usd)}
                                                </td>
                                                <td class="num neg">{format!("\u{2212}{}", fmt::fmt_usd(-c.hold.theta_day_usd))}</td>
                                                <td class="num neg">{fmt::fmt_usd(-c.hold.extrinsic_usd)}</td>
                                                <td class="num">{format!("{:.0}%", c.hold.p_profit * 100.0)}</td>
                                                <td>{format!("{:.0}d", c.hold.dte)}</td>
                                                <td class="num dim">{fmt::fmt_age(now.get() - c.updated_ms)}</td>
                                            </tr>
                                        }}
                                    </For>
                                </tbody>
                            </table>
                        </div>
                        <HoldDetail/>
                    }.into_any()
                }
            }}
        </section>
    }
}

/// Full analysis of the selected near-miss: closed-form + in-browser
/// antithetic Monte-Carlo of the terminal P&L if you keep the option.
#[component]
fn HoldDetail() -> impl IntoView {
    let st = use_context::<ApiState>().unwrap();
    let selected = Memo::new(move |_| {
        match st.selected_hold.get() {
            Some(id) => st.holds.get().into_iter().find(|c| c.inst_id == id),
            None => None,
        }
    });

    view! {
        {move || {
            match selected.get() {
                None => view! {
                    <div class="detail-empty">
                        <span class="de-arrow">"↑"</span>
                        <p>"Click a candidate to see the full hold-to-expiry analysis: theta schedule, "
                           "break-even move, Monte-Carlo P&L distribution and the plain-English verdict."</p>
                    </div>
                }.into_any(),
                Some(c) => view! { <HoldAnalysisCard c=c/> }.into_any(),
            }
        }}
    }
}

#[component]
fn HoldAnalysisCard(c: HoldCandidate) -> impl IntoView {
    // Full Monte-Carlo runs in the browser (same Rust quant crate as the
    // engine, compiled to WASM): 4 000 antithetic paths.
    let mc = Memo::new(move |_| {
        let input = ivarb_quant::HoldInput {
            spot: c.spot,
            strike: c.strike,
            t_years: c.t_years,
            right: if matches!(c.right, ivarb_protocol::Right::Call) {
                ivarb_quant::Right::Call
            } else {
                ivarb_quant::Right::Put
            },
            ask_px_unit: c.ask_px_unit,
            fee_rate: c.fee_rate,
            forecast_iv: c.consensus_iv_vp / 100.0,
            drift: 0.0,
            contract_size: c.contract_size,
            paths: 4_000,
            seed: 0x1A2B3C4D,
        };
        ivarb_quant::hold_analyze(&input)
    });
    let a = mc.get();

    let cs = a.contract_size;
    let premium_outlay = a.premium_unit * cs;
    let verdict = if a.ev_usd_per_contract > 0.0 && a.iv_edge_vp > 0.5 {
        format!(
            "KEEP-WORTHY — the ask implies {:.1}% vol but the cross-venue consensus is {:.1}%. Buying and holding to expiry has an expected value of {} per contract ({}% of the {} premium outlay), with a {:.0}% chance of finishing profitable. The cost: {} of theta per day and a maximum quiet-case loss of {}.",
            a.implied_iv * 100.0,
            a.forecast_iv * 100.0,
            fmt::fmt_usd(a.ev_usd_per_contract),
            (a.ev_pct * 100.0).max(0.0),
            fmt::fmt_usd(premium_outlay),
            a.p_profit * 100.0,
            fmt::fmt_usd(-a.theta_day_usd_per_contract),
            fmt::fmt_usd(-a.extrinsic_unit * cs),
        )
    } else if a.iv_edge_vp > 0.0 {
        format!(
            "MARGINAL — the vol edge is only {:+.2} vp and the hold EV is {} per contract. The theta bleed ({} per day) roughly offsets the cheapness unless realized vol runs above the consensus. Treat as a watchlist item, not a trade.",
            a.iv_edge_vp,
            fmt::fmt_usd(a.ev_usd_per_contract),
            fmt::fmt_usd(-a.theta_day_usd_per_contract),
        )
    } else {
        format!(
            "DISCARD — the ask implies {:.1}% vol, ABOVE the {:.1}% consensus. The option is not cheap; the near-miss was on the sell side. Holding to expiry has a negative expected value of {} per contract.",
            a.implied_iv * 100.0,
            a.forecast_iv * 100.0,
            fmt::fmt_usd(a.ev_usd_per_contract),
        )
    };

    let facts: Vec<(&'static str, String)> = vec![
        ("premium outlay", fmt::fmt_usd(premium_outlay)),
        ("entry fee", fmt::fmt_usd(a.fee_unit * cs)),
        ("total capital at risk", fmt::fmt_usd(a.capital_unit * cs)),
        ("intrinsic value", fmt::fmt_usd(a.intrinsic_unit * cs)),
        ("extrinsic (max quiet bleed)", fmt::fmt_usd(a.extrinsic_unit * cs)),
        ("implied vol of the ask", format!("{:.1}%", a.implied_iv * 100.0)),
        ("consensus forecast vol", format!("{:.1}%", a.forecast_iv * 100.0)),
        ("vol edge", format!("{:+.2} vp", a.iv_edge_vp)),
        ("expected value / contract", fmt::fmt_usd(a.ev_usd_per_contract)),
        ("EV as % of premium", format!("{:+.1}%", a.ev_pct * 100.0)),
        ("theta today", format!("\u{2212}{} / day", fmt::fmt_usd(a.theta_day_usd_per_contract))),
        ("theta at half-life", format!("\u{2212}{} / day", fmt::fmt_usd(a.theta_mid_day_unit * cs))),
        ("theta near expiry", format!("\u{2212}{} / day", fmt::fmt_usd(a.theta_late_day_unit * cs))),
        ("break-even at expiry", format!("{} ({:+.1}% move)", fmt::fmt_spot(a.breakeven_spot), a.be_move_pct)),
        ("P(terminal P&L > 0)", format!("{:.0}%", a.p_profit * 100.0)),
        ("MC mean P&L", fmt::fmt_usd(a.pnl_mean_unit * cs)),
        ("MC p05 / p50 / p95", format!("{} / {} / {}", fmt::fmt_usd(a.pnl_p05_unit * cs), fmt::fmt_usd(a.pnl_p50_unit * cs), fmt::fmt_usd(a.pnl_p95_unit * cs))),
        ("worst 5% average (CVaR)", fmt::fmt_usd(a.cvar05_unit * cs)),
        ("arb view (for contrast)", format!("{:+.2} vp net \u{2212} fees beat the spread", c.arb_net_vp)),
    ];

    view! {
        <div class="hold-detail">
            <div class="detail-head">
                <h3>{format!("Keeping {} {} {} {} to expiry", c.underlying, c.expiry, fmt::fmt_strike(c.strike), fmt::fmt_right(&c.right))}</h3>
                <div class="detail-chips">
                    <span class="chip">{format!("buy on {} \u{00b7} ask {}", fmt::venue_name(&c.venue), fmt::fmt_usd(c.ask_px_unit))}</span>
                    <span class=move || format!("chip {}", if a.ev_usd_per_contract >= 0.0 { "ok-chip" } else { "warn-chip" })>
                        {format!("hold EV {} / ct", fmt::fmt_usd(a.ev_usd_per_contract))}
                    </span>
                    <span class="chip warn-chip">{format!("arb view {:+.2} vp", c.arb_net_vp)}</span>
                </div>
            </div>
            <p class="trade-desc">{verdict}</p>
            <div class="hold-cols">
                <div class="hold-facts">
                    <For each=move || facts.clone() key=|(k, _)| *k let:f>
                        {let (k, v) = f;
                         view! {
                            <div class="fact">
                                <div class="fact-k">{k}</div>
                                <div class="fact-v">{v}</div>
                            </div>
                        }}
                    </For>
                </div>
                <div class="hold-scenarios">
                    <div class="dc-title">"Terminal P&L by spot at expiry"</div>
                    <ScenarioTable a=a.clone() cs=cs/>
                    <div class="dc-title" style="margin-top:14px">"Monte-Carlo P&L distribution (4 000 antithetic paths)"</div>
                    <McBar a=a.clone() cs=cs/>
                    <p class="dim small">
                        "Model: log-normal terminal spot under the consensus vol forecast, no drift. "
                        "Frozen-P&L (if you could sell instantly at the same vol) is marked for reference."
                    </p>
                </div>
            </div>
        </div>
    }
}

/// P&L at expiry for the 7 standard spot multiples, reusing the MC result.
#[component]
fn ScenarioTable(a: ivarb_quant::HoldAnalysis, cs: f64) -> impl IntoView {
    let multiples = ivarb_quant::SPOT_GRID;
    let rows: Vec<(f64, f64, f64)> = multiples
        .iter()
        .enumerate()
        .map(|(i, &m)| (m, a.spot * m, a.pnl_by_spot[i]))
        .collect();
    view! {
        <table class="scan-table scenario-table">
            <thead>
                <tr><th>"Spot multiple"</th><th class="num">"Spot at expiry"</th><th class="num">"P&L / contract"</th></tr>
            </thead>
            <tbody>
                <For each=move || rows.clone() key=|r| format!("{:.2}", r.0) let:r>
                    {let (m, s, pnl) = r;
                     let spot_fmt = fmt::fmt_spot(s);
                     view! {
                        <tr>
                            <td>{format!("{:+.0}%", (m - 1.0) * 100.0)}</td>
                            <td class="num">{spot_fmt}</td>
                            <td class=move || format!("num {}", if pnl >= 0.0 { "pos" } else { "neg" })>
                                {fmt::fmt_usd(pnl * cs)}
                            </td>
                        </tr>
                    }}
                </For>
            </tbody>
        </table>
    }
}

/// p05 / p50 / p95 / mean bar visualization of the MC distribution.
#[component]
fn McBar(a: ivarb_quant::HoldAnalysis, cs: f64) -> impl IntoView {
    let bars: Vec<(&'static str, f64, &'static str)> = vec![
        ("p05", a.pnl_p05_unit * cs, "neg"),
        ("p50", a.pnl_p50_unit * cs, "mid"),
        ("mean", a.pnl_mean_unit * cs, "mid"),
        ("p95", a.pnl_p95_unit * cs, "pos"),
        ("CVaR05", a.cvar05_unit * cs, "neg"),
    ];
    let max_abs = bars
        .iter()
        .map(|(_, v, _)| v.abs())
        .fold(1.0_f64, f64::max);
    view! {
        <div class="mc-bars">
            <For each=move || bars.clone() key=|(k, _, _)| *k let:b>
                {let (k, v, cls) = b;
                 let pct = (v.abs() / max_abs * 100.0).clamp(2.0, 100.0);
                 view! {
                    <div class="mc-row">
                        <span class="mc-k">{k}</span>
                        <div class="mc-track">
                            <div class=move || format!("mc-fill {}", if v >= 0.0 { "pos" } else { "neg" })
                                style=move || format!("width:{}%", pct)>
                            </div>
                        </div>
                        <span class=move || format!("mc-v {}", cls)>
                            {move || if v >= 0.0 { fmt::fmt_usd(v) } else { format!("\u{2212}{}", fmt::fmt_usd(-v)) }}
                        </span>
                    </div>
                }}
            </For>
        </div>
    }
}
