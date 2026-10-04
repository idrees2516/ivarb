//! Section 04 — Multi-hop arbitrage routes.
//!
//! The two-leg scanner needs a tradable bid on the rich venue and a tradable
//! ask on the cheap venue of the *same* option. Multi-hop removes that
//! restriction: routes through the (venue × call/put) graph — including
//! call↔put parity conversions hedged with a perp — so a divergence can be
//! monetized even when the direct side of the book is one-sided or missing.
//! Found by negative-cycle search over log-priced edges, re-scanned every
//! 250 ms on touched strike families, ranked by EXPECTED profit (print ×
//! data-confidence quality), and executed leg-by-leg by the route sequencer
//! (hardest-first ordering, generalized 10-second rule, mid-flight
//! abandonment, partial-fill settlement).

use leptos::prelude::*;

use crate::fmt;
use crate::state::ApiState;
use ivarb_protocol::{ExecCommand, MultiHopRoute};

#[component]
pub fn MultihopSection() -> impl IntoView {
    let st = use_context::<ApiState>().unwrap();
    let routes = Memo::new(move |_| {
        let mut v = st.multihop.get();
        v.sort_by(|a, b| (b.total_usd * b.quality).total_cmp(&(a.total_usd * a.quality)));
        v.truncate(12);
        v
    });
    let now = st.now;
    let taker_state = Memo::new(move |_| st.exec.get());
    let live_routes = Memo::new(move |_| {
        taker_state
            .get()
            .map(|e| e.stats.route_attempts_active)
            .unwrap_or(0)
    });
    let routes_on = Memo::new(move |_| {
        taker_state
            .get()
            .map(|e| e.settings.routes_enabled)
            .unwrap_or(false)
    });

    view! {
        <section class="section" id="multihop">
            <crate::app::SectionHead num="05"
                title="Multi-hop routes — negative-cycle arbitrage across venues and rights"
                sub="Every hop is executable: buy an option at an ask, sell at a bid, or convert call↔put on the same venue via put-call parity with a perp hedge. A cycle whose cash flow is positive after every fee is free money — the engine searches all touched strike families for them every 250 ms and the taker fires the ones whose EXPECTED profit (print × data quality) clears the bar."/>
            <div class="hold-intro">
                <p>
                    "Why hops matter: thin venues often publish one-sided books — an ask on the call but no bid, while "
                    "their put side or another venue's equivalent is perfectly tradable. A route like "
                    <code>"buy call @okx → convert to put @okx → sell put @bybit"</code>
                    "monetizes a divergence the direct scanner has to skip. Conversion legs carry the perp hedge fees "
                    "and a basis buffer, so they must clear a higher bar than a direct two-leg arb."
                </p>
                <div class="mh-status">
                    {move || if routes_on.get() {
                        view! {
                            <span class="mh-autochip on">
                                {format!("◈ auto-taking live — {} cycle(s) executing", live_routes.get())}
                            </span>
                        }.into_any()
                    } else {
                        view! {
                            <span class="mh-autochip off">
                                "◈ auto-taking off — enable ROUTES in the Advantage taker"
                            </span>
                        }.into_any()
                    }}
                </div>
            </div>
            {move || {
                let rs = routes.get();
                if rs.is_empty() {
                    view! {
                        <div class="detail-empty">
                            <p>"No profitable cycles right now. That is the normal state of an efficient market — "
                               "multi-hop routes appear in bursts around one-sided quotes, thin books and vol events. "
                               "The search itself runs continuously on every touched strike family."</p>
                        </div>
                    }.into_any()
                } else {
                    view! {
                        <div class="mh-list">
                            <For each=move || routes.get() key=|r| r.id.clone() let:r>
                                <RouteCard r=r now=now.get()/>
                            </For>
                        </div>
                    }.into_any()
                }
            }}
        </section>
    }
}

#[component]
fn RouteCard(r: MultiHopRoute, now: i64) -> impl IntoView {
    let st = use_context::<ApiState>().unwrap();
    let expected = r.total_usd * r.quality;
    let id_take = r.id.clone();
    let n_legs = r.legs.len();
    let exp_txt = fmt::fmt_usd(expected);
    view! {
        <div class="mh-card">
            <div class="mh-head">
                <div class="mh-inst">
                    <span class="i-und">{r.underlying.clone()}</span>
                    <span class="i-date">{r.expiry.clone()}</span>
                    <span class="i-strike">{fmt::fmt_strike(r.strike)}{" strike"}</span>
                    <span class=move || format!("mh-quality {}", if r.quality >= 0.6 { "hi" } else if r.quality >= 0.3 { "mid" } else { "lo" })>
                        {format!("quality {:.0}%", 100.0 * r.quality)}
                    </span>
                </div>
                <div class="mh-profits">
                    <span class="mh-profit">{fmt::fmt_usd(r.profit_unit_usd)}</span>
                    <span class="mh-sub">"per unit"</span>
                    <span class=move || format!("num {}", if r.total_usd >= 0.0 { "pos" } else { "neg" })>
                        {format!("{} at size", fmt::fmt_usd(r.total_usd))}
                    </span>
                    <span class="mh-sub">{format!("{:+.2} vp · {} {} units", r.profit_vp, trim(r.size_units), r.underlying)}</span>
                </div>
            </div>
            <div class="mh-legs">
                <For each=move || r.legs.clone() key=|l| format!("{}-{}-{}", l.venue, l.instrument, l.px_unit) let:l>
                    {let action_cls = match l.action.as_str() {
                        "buy" => "leg-buy",
                        "sell" => "leg-sell",
                        _ => "leg-convert",
                    };
                     view! {
                        <div class=move || format!("mh-leg {}", action_cls)>
                            <span class="leg-action">{l.action.clone()}</span>
                            <span class="leg-venue">{fmt::venue_name(&l.venue)}</span>
                            <span class="leg-inst">{l.instrument.clone()}</span>
                            <span class="leg-px">
                                {move || if l.px_unit >= 0.0 { format!("pay {}", fmt::fmt_usd(l.px_unit)) } else { format!("receive {}", fmt::fmt_usd(-l.px_unit)) }}
                            </span>
                            <span class="leg-fee">{format!("fee {}", fmt::fmt_usd(l.fee_unit))}</span>
                            <span class="leg-sz">{format!("{} u", trim(l.size_units))}</span>
                        </div>
                    }}
                </For>
            </div>
            <div class="mh-foot">
                <span class="mh-exp">
                    {format!("expected {} (print × quality) · quotes {} old", fmt::fmt_usd(expected), fmt::fmt_age(r.max_age_ms))}
                </span>
                <button class="mh-take"
                    on:click=move |_| st.send_exec_cmd(
                        ExecCommand::TakeRoute { route: id_take.clone() },
                        format!("route take sent — {n_legs} legs, expected {exp_txt}"),
                    )>
                    {format!("TAKE THIS ROUTE · {}", exp_txt)}
                </button>
                <span class="dim">{format!("updated {} ago", fmt::fmt_age(now - r.updated_ms))}</span>
            </div>
        </div>
    }
}

fn trim(v: f64) -> String {
    if v >= 100.0 {
        format!("{:.0}", v)
    } else {
        format!("{:.2}", v)
    }
}
