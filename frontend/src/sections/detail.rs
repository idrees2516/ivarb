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

                            <CostCapitalBlock o=o.clone()/>

                            <HoldBlock o=o.clone()/>

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
        ivarb_protocol::Tier::Route => "route",
        ivarb_protocol::Tier::Monitor => "watch",
    };
    let plan: Vec<(&'static str, String)> = vec![
        ("strategy tier", format!("{} — {}", tier.label(), match tier {
            ivarb_protocol::Tier::Sniper => "both legs at the touch immediately",
            ivarb_protocol::Tier::Convergence => "passive quotes on both venues, capture on convergence",
            ivarb_protocol::Tier::Route => "multi-hop cycle — fired through the route sequencer (see section 05)",
            ivarb_protocol::Tier::Monitor => "not executable right now (edge below gate or quotes stale)",
        })),
        ("theta bucket", format!("{} ({:.1} days to expiry)", o.bucket, o.dte)),
        ("effective gate", format!("{:+.2} vp (base × bucket × regime × age)", o.gate_vp)),
        ("expected value", format!("{:+.2} vp after fill-probability, slippage, single-leg risk and theta burn", o.ev_vp)),
        ("theta burn", format!("{:.2} vp/day raw · hedged arb nets most of it", o.theta_vp_day)),
        ("patience / max hold", format!("{} / {}", fmt_secs(o.patience_s), fmt_secs(o.hold_s))),
        ("convergence hint", if o.conv_hint { "favorable — the rich leg is fading toward the cheap leg".into() } else { "neutral".into() }),
        ("if not captured", "0DTE escape — hedge Δ on a third venue, both legs out at market, flat fast".into()),
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

/// The comprehensive cost sheet + capital requirement for this exact
/// opportunity, per 1 underlying unit, computed in-browser with the same
/// `ivarb-quant` code the executor's risk gates use.
#[component]
fn CostCapitalBlock(o: Opportunity) -> impl IntoView {
    let st = use_context::<ApiState>().unwrap();
    let o_id = o.id.clone();

    let analysis = Memo::new(move |_| {
        let o = st
            .opps
            .get()
            .into_iter()
            .find(|x| x.id == o_id)
            .unwrap_or_else(|| o.clone());
        let (buy_px, sell_px) = match (o.buy_px_usd, o.sell_px_usd) {
            (Some(b), Some(s)) if b > 0.0 && s > 0.0 => (b, s),
            _ => return None,
        };
        let buy_ct = if o.contract_size > 0.0 { o.contract_size } else { 1.0 };
        let buy_depth = o.size_buy.map(|u| u / buy_ct).unwrap_or(10.0).max(1.0);
        let sell_depth = o.size_sell.unwrap_or(10.0).max(1.0);
        let hold_days = (o.hold_s / 86_400.0).min(o.dte.max(0.1)).max(0.02);
        let fee = |v: &str| match v {
            "binance" | "aevo" | "thalex" => 0.0002,
            _ => 0.0003,
        };
        let input = ivarb_quant::CostInput {
            buy_premium_per_contract: buy_px,
            sell_premium_per_contract: sell_px,
            buy_contract_size: buy_ct,
            sell_contract_size: 1.0,
            buy_fee_rate: fee(&o.buy_venue),
            sell_fee_rate: fee(&o.sell_venue),
            spot: o.spot,
            strike: o.strike,
            right: match o.right {
                ivarb_protocol::Right::Call => ivarb_quant::QuantRight::Call,
                ivarb_protocol::Right::Put => ivarb_quant::QuantRight::Put,
            },
            vega_per_vp_unit: (o.vega_vp_usd / buy_ct).max(1e-9),
            units: 1.0,
            hold_days,
            theta_vp_day: o.theta_vp_day,
            buy_depth_contracts: buy_depth,
            sell_depth_contracts: sell_depth,
            funding_round_trip_usd: ivarb_quant::funding_round_trip(&o.buy_venue, &o.sell_venue),
            sell_venue: o.sell_venue.clone(),
        };
        let sheet = ivarb_quant::cost_sheet(&input);
        let cap = ivarb_quant::capital_req(&input);
        let roc = ivarb_quant::roc_pct(sheet.net_all_in_usd, &cap);
        // funded? against live paper balances
        let bal = |v: &str| {
            st.exec
                .get()
                .map(|s| {
                    s.balances
                        .iter()
                        .find(|(b, _)| b == v)
                        .map(|(_, x)| *x)
                        .unwrap_or(0.0)
                })
                .unwrap_or(0.0)
        };
        let funded_buy = bal(&o.buy_venue) >= cap.buy_venue_usd;
        let funded_sell = bal(&o.sell_venue) >= cap.sell_venue_usd;
        let buy_v = o.buy_venue.clone();
        let sell_v = o.sell_venue.clone();
        Some((sheet, cap, roc, funded_buy, funded_sell, buy_v, sell_v))
    });

    view! {
        <div class="exec-plan cost-block">
            <div class="ep-head">
                <h4>"Comprehensive cost & capital"</h4>
                <span class="ep-sub">"per 1 underlying unit — every cost of harvesting this arb, and the collateral it ties up"</span>
            </div>
            {move || match analysis.get() {
                None => view! {
                    <div class="cb-empty">"Executable premiums not published for this pair — the comprehensive sheet needs live two-sided quotes on both legs."</div>
                }.into_any(),
                Some((sheet, cap, roc, funded_buy, funded_sell, buy_v, sell_v)) => {
                    let rows: Vec<(&'static str, String, &'static str)> = vec![
                        ("spread locked at entry", fmt::fmt_usd(sheet.gross_locked_usd), "pos"),
                        ("entry taker fees (both legs)", format!("−{}", fmt::fmt_usd(sheet.entry_fees_usd)), "neg"),
                        ("exit taker fees (both legs)", format!("−{}", fmt::fmt_usd(sheet.exit_fees_usd)), "neg"),
                        ("slippage through the touch (entry+exit)", format!("−{}", fmt::fmt_usd(sheet.slippage_usd)), "neg"),
                        ("margin carry over the hold", format!("−{}", fmt::fmt_usd(sheet.margin_carry_usd)), "neg"),
                        ("netted theta burn", format!("−{}", fmt::fmt_usd(sheet.theta_burn_usd)), "neg"),
                        ("funding round-trip (deposit+withdraw)", format!("−{}", fmt::fmt_usd(sheet.funding_usd)), "neg"),
                        ("TOTAL COST", format!("{} = {} vp", fmt::fmt_usd(sheet.total_usd), fmt::fmt_vp(sheet.total_vp)), "neg"),
                        ("net after ALL costs", fmt::fmt_usd(sheet.net_all_in_usd), if sheet.net_all_in_usd >= 0.0 { "pos" } else { "neg" }),
                        ("capital on buy venue", fmt::fmt_usd(cap.buy_venue_usd), ""),
                        ("capital on sell venue (margin ×1.2)", fmt::fmt_usd(cap.sell_venue_usd), ""),
                        ("total capital / ROC", format!("{} · {:+.1}%", fmt::fmt_usd(cap.total_usd), roc), ""),
                    ];
                    view! {
                        <div class="ep-grid">
                            <For each=move || rows.clone() key=|(k, _, _)| *k let:r>
                                {let (k, v, cls) = r;
                                 view! {
                                    <div class="ep-cell">
                                        <div class="fact-k">{k}</div>
                                        <div class=format!("fact-v {}", cls)>{v}</div>
                                    </div>
                                }}
                            </For>
                        </div>
                        <div class="cb-funded">
                            {if funded_buy && funded_sell {
                                view! { <span class="chip ok-chip">"capital: funded on both venues"</span> }.into_any()
                            } else {
                                view! {
                                    <span class="chip warn-chip">
                                        {format!(
                                            "deposit needed: {}",
                                            if !funded_buy && !funded_sell {
                                                format!("{} and {}", fmt::venue_name(&buy_v), fmt::venue_name(&sell_v))
                                            } else if !funded_buy {
                                                fmt::venue_name(&buy_v)
                                            } else {
                                                fmt::venue_name(&sell_v)
                                            }
                                        )}
                                    </span>
                                }.into_any()
                            }}
                            <span class="cb-hint">"see section 03 — Funding, costs & capital"</span>
                        </div>
                    }.into_any()
                }
            }}
        </div>
    }
}

/// The hold-to-expiry view of the selected opportunity's cheap leg: the
/// "arb is good — but what if I just kept the option?" alternative, with
/// theta cost, EV, P(profit) and the OU convergence ETA.
#[component]
fn HoldBlock(o: Opportunity) -> impl IntoView {
    let hold = o.hold;
    let eta = o.conv_eta_s;
    let kalman = (o.kalman_level_vp, o.kalman_sigma_vp);
    view! {
        <div class="hold-block">
            <div class="ep-head">
                <h4>"Alternative: keep the cheap leg to expiry"</h4>
                <span class="ep-sub">"the cost and profit of NOT round-tripping this trade"</span>
            </div>
            {match hold {
                None => view! {
                    <p class="dim">"Hold analytics not yet computed for this signal — it refreshes on the analytics cadence."</p>
                }.into_any(),
                Some(h) => {
                    let verdict = if h.ev_usd > 0.0 {
                        format!(
                            "Keeping the {} leg instead of selling it away has an expected value of {} per contract under the mid-IV forecast ({:+.2} vp vol edge). The cost side: {} of theta per day and up to {} of extrinsic bleed if the market sits still. P(finishing profitable): {:.0}%.",
                            o.buy_venue, fmt::fmt_usd(h.ev_usd), h.iv_edge_vp,
                            fmt::fmt_usd(-h.theta_day_usd), fmt::fmt_usd(-h.extrinsic_usd),
                            h.p_profit * 100.0,
                        )
                    } else {
                        format!(
                            "Holding to expiry is NOT attractive here: expected value {} per contract, theta bleeds {} per day. The two-leg arb captures the edge now and nets the theta — take the round trip.",
                            fmt::fmt_usd(h.ev_usd), fmt::fmt_usd(-h.theta_day_usd),
                        )
                    };
                    let cells: Vec<(&'static str, String, &'static str)> = vec![
                        ("hold EV / ct", fmt::fmt_usd(h.ev_usd), if h.ev_usd >= 0.0 { "pos" } else { "neg" }),
                        ("vol edge", format!("{:+.2} vp", h.iv_edge_vp), if h.iv_edge_vp >= 0.0 { "pos" } else { "neg" }),
                        ("theta / day", format!("\u{2212}{}", fmt::fmt_usd(-h.theta_day_usd)), "neg"),
                        ("extrinsic / ct", fmt::fmt_usd(h.extrinsic_usd), ""),
                        ("P(profit)", format!("{:.0}%", h.p_profit * 100.0), ""),
                        ("days to expiry", format!("{:.1}", h.dte), ""),
                        (
                            "convergence ETA (OU)",
                            eta.map(|e| fmt_secs(e)).unwrap_or_else(|| "fitting…".into()),
                            "",
                        ),
                        (
                            "spread noise (Kalman σ)",
                            kalman.1.map(|s| format!("{:.2} vp", s)).unwrap_or_else(|| "—".into()),
                            "",
                        ),
                    ];
                    view! {
                        <p class="trade-desc">{verdict}</p>
                        <div class="ep-grid">
                            <For each=move || cells.clone() key=|(k, _, _)| *k let:c>
                                {let (k, v, cls) = c;
                                 view! {
                                    <div class="ep-cell">
                                        <div class="fact-k">{k}</div>
                                        <div class=move || format!("fact-v {}", cls)>{v}</div>
                                    </div>
                                 }}
                            </For>
                        </div>
                    }.into_any()
                }
            }}
        </div>
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
