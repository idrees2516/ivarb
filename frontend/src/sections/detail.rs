//! The "How to trade it" panel bound to the selected scanner row.

use leptos::prelude::*;

use crate::chart::SpreadChart;
use crate::fmt;
use crate::state::ApiState;
use ivarb_protocol::Opportunity;

#[component]
pub fn DetailPanel() -> impl IntoView {
    let st = use_context::<ApiState>().unwrap();

    let selected_opp = Memo::new(move |_| {
        match st.selected.get() {
            Some(id) => st.opps.get().into_iter().find(|o| o.id == id),
            None => None,
        }
    });
    let history = Memo::new(move |_| {
        match st.selected.get() {
            Some(id) => st.histories.get().get(&id).cloned().unwrap_or_default(),
            None => Vec::new(),
        }
    });
    let now = st.now;

    view! {
        {move || {
            match selected_opp.get() {
                None => view! {
                    <div class="detail-empty">
                        <span class="de-arrow">"↑"</span>
                        <p>"Click any row in the scanner to see the plain-English trade, "
                           "the spread history chart and the lead-lag hint."</p>
                    </div>
                }.into_any(),
                Some(o) => {
                    let o_age = o.clone();
                    let o_desc = o.clone();
                    let o_lag = o.clone();
                    let o_grid = o.clone();
                    view! {
                        <div class="detail-panel">
                            <div class="detail-head">
                                <h3>"How to trade it"</h3>
                                <div class="detail-chips">
                                    {move || {
                                        let age = now.get() - o_age.updated_ms;
                                        if o_age.stale {
                                            view! { <span class="chip warn-chip">{format!("stale {} ago", fmt::fmt_age(age))}</span> }.into_any()
                                        } else {
                                            view! { <span class="chip ok-chip">{format!("updated {} ago", fmt::fmt_age(age))}</span> }.into_any()
                                        }
                                    }}
                                    <span class=move || {
                                        let mut c = String::from("chip ");
                                        c.push_str(if matches!(o.edge_mode, ivarb_protocol::EdgeMode::Exec) { "ok-chip" } else { "warn-chip" });
                                        c
                                    }>
                                        {move || format!("{} net", fmt::fmt_vp(o.net_vp))}
                                    </span>
                                    <span class="chip">{o.underlying.clone()}{" "}{o.expiry.clone()}{" "}{fmt::fmt_strike(o.strike)}{" "}{fmt::fmt_right(&o.right)}</span>
                                </div>
                            </div>

                            <p class="trade-desc">
                                {move || describe(&o_desc)}
                            </p>

                            <div class="detail-warn">
                                <strong>"Before acting: "</strong>
                                "Verify the IV spread on both exchange UIs right now. The scanner reads from live feeds; confirm it matches the order book you will trade against. Place both legs within seconds of each other — if the second leg does not fill in 10 seconds, cancel and unwind."
                            </div>

                            <ExecPlan o=o.clone()/>

                            <div class="detail-grid">
                                <div class="detail-chart">
                                    <div class="dc-title">
                                        "Spread history"
                                        <span class="dc-sub">"net_edge_vp · expiry "</span>
                                        <span class="dc-sub2">{o.expiry.clone()}</span>
                                    </div>
                                    <SpreadChart history=history.get() threshold=0.5/>
                                    <div class="lead-lag">
                                        {move || lead_lag_hint(&o_lag, now.get())}
                                    </div>
                                </div>
                                <div class="detail-facts">
                                    <FactGrid o=o_grid/>
                                </div>
                            </div>
                        </div>
                    }.into_any()
                }
            }
        }}
    }
}

fn describe(o: &Opportunity) -> String {
    let buy = fmt::venue_name(&o.buy_venue);
    let sell = fmt::venue_name(&o.sell_venue);
    format!(
        "Buy the option on {buy} (lower IV, {ivb:.2}%). Sell the same contract on {sell} (higher IV, {ivs:.2}%). \
Expiry: {expiry}. Raw spread is {spread:.2} vol points. After round-trip taker fees ({fee:.2} vp), \
you collect {net:.2} vol points net. At ~{vega} vega/contract, that is \u{2248} {est} per contract before slippage.",
        ivb = o.iv_buy_vp,
        ivs = o.iv_sell_vp,
        expiry = o.expiry,
        spread = o.iv_spread_vp,
        fee = o.fee_vp,
        net = o.net_vp,
        vega = fmt::fmt_usd(o.vega_vp_usd),
        est = fmt::fmt_usd(o.est_usd),
    )
}

/// The advantage taker's plan for this signal: tier, effective gate,
/// expected value, patience/hold windows and theta burn, plus a manual TAKE.
#[component]
fn ExecPlan(o: Opportunity) -> impl IntoView {
    let st = use_context::<ApiState>().unwrap();
    let take_id = o.id.clone();
    let tier = o.tier;
    let tier_cls = match tier {
        ivarb_protocol::Tier::Sniper => "sniper",
        ivarb_protocol::Tier::Convergence => "conv",
        ivarb_protocol::Tier::Monitor => "watch",
    };
    let plan: Vec<(&'static str, String)> = vec![
        ("strategy tier", format!("{} — {}", tier.label(), match tier {
            ivarb_protocol::Tier::Sniper => "both legs at the touch immediately",
            ivarb_protocol::Tier::Convergence => "passive quotes on both venues, capture on convergence",
            ivarb_protocol::Tier::Monitor => "not executable right now (edge below gate or quotes stale)",
        })),
        ("theta bucket", format!("{} ({:.1} days to expiry)", o.bucket, o.dte)),
        ("effective gate", format!("{:+.2} vp (base × bucket × regime × age)", o.gate_vp)),
        ("expected value", format!("{:+.2} vp after fill-probability, slippage, single-leg risk and theta burn", o.ev_vp)),
        ("theta burn", format!("{:.2} vp/day raw · hedged arb nets most of it", o.theta_vp_day)),
        ("patience / max hold", format!("{} / {}", fmt_secs(o.patience_s), fmt_secs(o.hold_s))),
        ("convergence hint", if o.conv_hint { "favorable — the rich leg is fading toward the cheap leg".into() } else { "neutral".into() }),
    ];
    view! {
        <div class="exec-plan">
            <div class="ep-head">
                <h4>"Execution plan"</h4>
                <span class="ep-sub">"what the advantage taker will do with this signal"</span>
                <button class="take-btn big"
                    on:click=move |_| st.send_exec_cmd(
                        ivarb_protocol::ExecCommand::Take { opp: take_id.clone(), units: None },
                        format!("TAKE sent — risk gates will size it"),
                    )>
                    "⚡ TAKE THIS TRADE"
                </button>
            </div>
            <div class="ep-grid">
                <For each=move || plan.clone() key=|(k, _)| *k let:f>
                    {let (k, v) = f;
                     view! {
                        <div class="ep-cell">
                            <div class="fact-k">{k}</div>
                            <div class=format!("fact-v {}", if k == "strategy tier" { tier_cls } else { "" })>{v}</div>
                        </div>
                    }}
                </For>
            </div>
        </div>
    }
}

fn fmt_secs(s: f64) -> String {
    if s < 90.0 {
        format!("{s:.0}s")
    } else if s < 5400.0 {
        format!("{:.0}m", s / 60.0)
    } else {
        format!("{:.1}h", s / 3600.0)
    }
}

fn lead_lag_hint(o: &Opportunity, now: i64) -> String {
    // Which venue's quote moved most recently is the more likely leader; the
    // lagging side tends to revert toward it.
    let _ = now;
    let buy = fmt::venue_name(&o.buy_venue);
    let sell = fmt::venue_name(&o.sell_venue);
    let (leader, lagger, lead_ms, drift) = if o.lead_buy_ms <= o.lead_sell_ms {
        (&buy, &sell, o.lead_buy_ms, o.drift_sell_vp_1m)
    } else {
        (&sell, &buy, o.lead_sell_ms, o.drift_buy_vp_1m)
    };
    format!(
        "Lead-lag: {leader} repriced {age} ago, {lagger} is the laggard (its IV drifted {drift:+.2} vp over the last minute) — \
expect the {lagger} side to revert toward {leader}.",
        age = fmt::fmt_age(lead_ms),
    )
}

#[component]
fn FactGrid(o: Opportunity) -> impl IntoView {
    let facts: Vec<(&'static str, String)> = vec![
        ("buy leg", fmt::venue_name(&o.buy_venue)),
        ("sell leg", fmt::venue_name(&o.sell_venue)),
        ("strike", format!("{} {}", fmt::fmt_strike(o.strike), fmt::fmt_right(&o.right))),
        ("underlying", o.underlying.clone()),
        ("expiry", o.expiry.clone()),
        ("days to expiry", format!("{:.1}", o.t_years * 365.0)),
        ("consensus spot", fmt::fmt_usd(o.spot)),
        (
            "buy premium / ct",
            o.buy_px_usd.map(|p| fmt::fmt_usd(p)).unwrap_or_else(|| "—".into()),
        ),
        (
            "sell premium / ct",
            o.sell_px_usd.map(|p| fmt::fmt_usd(p)).unwrap_or_else(|| "—".into()),
        ),
        (
            "contract size (buy venue)",
            format!("{} {}", trim_trailing(o.contract_size), o.underlying),
        ),
        (
            "quoted size buy / sell",
            match (o.size_buy, o.size_sell) {
                (Some(a), Some(b)) => format!("{} / {}", trim(a), trim(b)),
                (Some(a), None) => format!("{} / —", trim(a)),
                (None, Some(b)) => format!("— / {}", trim(b)),
                _ => "not published".into(),
            },
        ),
        (
            "executable capacity",
            o.max_units
                .map(|u| format!("{} {} (\u{2248} {})", trim(u), o.underlying, fmt::fmt_usd(o.total_usd.unwrap_or(0.0))))
                .unwrap_or_else(|| "not published".into()),
        ),
    ];
    view! {
        <div class="fact-grid">
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
    }
}

fn trim(v: f64) -> String {
    if v >= 100.0 {
        format!("{:.0}", v)
    } else {
        format!("{:.2}", v)
    }
}

fn trim_trailing(v: f64) -> String {
    if (v - v.round()).abs() < 1e-9 {
        format!("{}", v.round())
    } else {
        format!("{v}")
    }
}
