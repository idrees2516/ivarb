//! The option builder: pick a view / horizon / risk, get a structure struck
//! from the live consolidated chain — legs routed to the best venue books,
//! greeks from the shared Black-Scholes core, and a coherence read that
//! explains *why* this structure fits the current market.

use leptos::prelude::*;

use ivarb_protocol::research::{
    MarketView, RiskAppetite, SuggestLeg, SuggestionCard,
};
use ivarb_protocol::research::LegAction;

use super::chat::short;
use super::{rs_usd, Rs};
use crate::api::{post_json_resp, spawn};
use crate::fmt;

#[derive(Clone, Copy, PartialEq, Eq)]
struct ViewOpt(MarketView);

const VIEWS: [(MarketView, &str, &str); 4] = [
    (MarketView::Bullish, "Bullish", "expect upside"),
    (MarketView::Bearish, "Bearish", "expect downside"),
    (MarketView::NeutralRange, "Range", "expect chop"),
    (MarketView::BigMove, "Big move", "expect a break"),
];

const RISKS: [(RiskAppetite, &str); 3] = [
    (RiskAppetite::Conservative, "Conservative"),
    (RiskAppetite::Balanced, "Balanced"),
    (RiskAppetite::Aggressive, "Aggressive"),
];

fn view_str(v: MarketView) -> &'static str {
    match v {
        MarketView::Bullish => "bullish",
        MarketView::Bearish => "bearish",
        MarketView::NeutralRange => "neutral_range",
        MarketView::BigMove => "big_move",
    }
}

fn risk_str(r: RiskAppetite) -> &'static str {
    match r {
        RiskAppetite::Conservative => "conservative",
        RiskAppetite::Balanced => "balanced",
        RiskAppetite::Aggressive => "aggressive",
    }
}

#[component]
pub fn BuilderPanel(rs: Rs) -> impl IntoView {
    let view: RwSignal<MarketView> = RwSignal::new(MarketView::NeutralRange);
    let risk: RwSignal<RiskAppetite> = RwSignal::new(RiskAppetite::Balanced);
    let horizon: RwSignal<u32> = RwSignal::new(14);
    let busy: RwSignal<bool> = RwSignal::new(false);
    let error: RwSignal<Option<String>> = RwSignal::new(None);

    let build = move || {
        let und = rs.und.get_untracked();
        let view = view.get_untracked();
        let risk = risk.get_untracked();
        let horizon = horizon.get_untracked();
        busy.set(true);
        error.set(None);
        let card_sig = rs.card;
        let busy2 = busy;
        let err2 = error;
        spawn(async move {
            #[derive(serde::Serialize)]
            struct Q {
                und: String,
                view: String,
                horizon_days: u32,
                risk: String,
            }
            let q = Q {
                und,
                view: view_str(view).to_string(),
                horizon_days: horizon,
                risk: risk_str(risk).to_string(),
            };
            match post_json_resp::<SuggestionCard, _>("/api/research/suggest", &q).await {
                Some(card) => card_sig.set(Some(card)),
                None => err2.set(Some(
                    "The builder could not strike a structure — the consolidated book may still be warming up (retry in a few seconds).".into(),
                )),
            }
            busy2.set(false);
        });
    };

    let ask_explain = move || {
        let und = rs.und.get_untracked();
        let view = view.get_untracked();
        let risk = risk.get_untracked();
        let horizon = horizon.get_untracked();
        let q = format!(
            "Create a {} structure on {} with a {} day horizon, {} risk, and explain why it is coherent with the current market",
            view_str(view).replace('_', " "),
            und,
            horizon,
            risk_str(risk),
        );
        rs.ask_agent.set(Some(q));
    };

    view! {
        <div class="rs-builder">
            <div class="rs-builder-form">
                <div class="rs-form-row">
                    <span class="rs-form-label">"view"</span>
                    <div class="rs-chip-row">
                        {VIEWS.iter().map(|(v, label, hint)| {
                            let v = *v;
                            view! {
                                <button class=move || format!("rs-und-chip{}", if view.get() == v { " on" } else { "" })
                                    title=*hint
                                    on:click=move |_| view.set(v)>
                                    {*label}
                                </button>
                            }.into_any()
                        }).collect::<Vec<_>>()}
                    </div>
                </div>
                <div class="rs-form-row">
                    <span class="rs-form-label">"horizon"</span>
                    <div class="rs-horizon">
                        <input type="range" min="1" max="60" step="1"
                            prop:value=move || horizon.get().to_string()
                            on:input=move |ev| {
                                if let Ok(v) = event_target_value(&ev).parse::<u32>() {
                                    horizon.set(v);
                                }
                            }/>
                        <span class="rs-horizon-val">{move || format!("{} days", horizon.get())}</span>
                    </div>
                </div>
                <div class="rs-form-row">
                    <span class="rs-form-label">"risk"</span>
                    <div class="rs-chip-row">
                        {RISKS.iter().map(|(r, label)| {
                            let r = *r;
                            view! {
                                <button class=move || format!("rs-und-chip{}", if risk.get() == r { " on" } else { "" })
                                    on:click=move |_| risk.set(r)>
                                    {*label}
                                </button>
                            }.into_any()
                        }).collect::<Vec<_>>()}
                    </div>
                </div>
                <div class="rs-form-actions">
                    <button class="rs-build-btn" disabled=move || busy.get() on:click=move |_| build()>
                        {move || if busy.get() { "striking…" } else { "Build structure" }}
                    </button>
                    <button class="rs-ask-btn" on:click=move |_| ask_explain()>
                        "Ask the agent to explain"
                    </button>
                </div>
                {move || {
                    error.get()
                        .map(|e| view! { <div class="rs-build-error">{e}</div> }.into_any())
                        .unwrap_or_else(|| ().into_any())
                }}
            </div>
            <div class="rs-builder-out">
                {move || {
                    match rs.card.get() {
                        Some(card) => view! { <SuggestionCardView card=card compact=false/> }.into_any(),
                        None => view! {
                            <div class="rs-build-empty">
                                <p>"Pick a view, horizon and risk — the builder will pick the strategy, strikes and venue routing from the live cross-venue book."</p>
                                <p class="dim">"Strategy matrix: bullish + rich vol → put credit spreads; bullish + cheap vol → call debit spreads; range + rich → iron condors; range + cheap → long butterflies; big move + cheap → straddles; big move + rich → strangles."</p>
                            </div>
                        }.into_any(),
                    }
                }}
            </div>
        </div>
    }
}

#[component]
pub fn SuggestionCardView(card: SuggestionCard, #[prop(default = false)] compact: bool) -> impl IntoView {
    let coherence_cls = if card.coherence.score >= 75 {
        "hi"
    } else if card.coherence.score >= 55 {
        "mid"
    } else {
        "lo"
    };
    let flow_cls = if card.flow == "credit" { "pos" } else { "neg" };
    view! {
        <div class=format!("sg-card{}", if compact { " compact" } else { "" })>
            <div class="sg-head">
                <div class="sg-title">
                    <span class="sg-strategy">{card.strategy.clone()}</span>
                    <span class="sg-und">{format!("{} · {}d · {:?}", card.und, card.horizon_days, card.risk)}</span>
                </div>
                <div class=format!("sg-coh {coherence_cls}") title=card.coherence.rationale.clone()>
                    {format!("{}/100", card.coherence.score)}
                    <span class="sg-coh-label">"coherence"</span>
                </div>
            </div>
            <table class="sg-legs">
                <thead>
                    <tr><th></th><th>"leg"</th><th class="num">"px"</th><th>"venue"</th><th class="num">"IV"</th><th class="num">"Δ"</th><th class="num">"size"</th></tr>
                </thead>
                <tbody>
                    {card.legs.iter().map(|l| leg_row(l)).collect::<Vec<_>>()}
                </tbody>
            </table>
            <div class="sg-stats">
                <div class="sg-stat">
                    <span class="sg-k">{format!("net {} ({})", card.flow, if card.flow == "credit" { "receive" } else { "pay" })}</span>
                    <span class=format!("sg-v {flow_cls}")>{rs_usd(card.net_premium_usd.abs())}</span>
                </div>
                <div class="sg-stat">
                    <span class="sg-k">"max profit"</span>
                    <span class="sg-v pos">{card.max_profit_usd.map(rs_usd).unwrap_or_else(|| "unbounded".into())}</span>
                </div>
                <div class="sg-stat">
                    <span class="sg-k">"max loss"</span>
                    <span class="sg-v neg">{card.max_loss_usd.map(rs_usd).unwrap_or_else(|| "unbounded".into())}</span>
                </div>
                <div class="sg-stat">
                    <span class="sg-k">"breakeven"</span>
                    <span class="sg-v">{if card.breakevens.is_empty() { "—".into() } else { card.breakevens.iter().map(|b| fmt::fmt_spot(*b)).collect::<Vec<_>>().join(" / ") }}</span>
                </div>
                <div class="sg-stat">
                    <span class="sg-k">"greeks / unit"</span>
                    <span class="sg-v dim">{format!("Δ {:+.2} · Γ {:+.4} · Θ {:+.2}/d · V {:+.2}/vp", card.greeks.delta, card.greeks.gamma, card.greeks.theta_day, card.greeks.vega_vp)}</span>
                </div>
            </div>
            {if compact {
                ().into_any()
            } else {
                view! {
                    <div class="sg-rationale">
                        <div class="sg-rah-title">"Why this fits the market"</div>
                        <p>{card.coherence.rationale.clone()}</p>
                        <div class="sg-factors">
                            <span>{format!("vol regime: {}", card.coherence.iv_regime)}</span>
                            <span>{format!("trend: {}", card.coherence.trend)}</span>
                            <span>{format!("skew: {}", card.coherence.skew)}</span>
                            <span>{format!("liquidity: {}", card.coherence.liquidity)}</span>
                        </div>
                    </div>
                    <ul class="sg-notes">
                        {card.notes.iter().map(|n| view! { <li>{n.clone()}</li> }.into_any()).collect::<Vec<_>>()}
                    </ul>
                }.into_any()
            }}
        </div>
    }
}

fn leg_row(l: &SuggestLeg) -> AnyView {
    let buy = matches!(l.action, LegAction::Buy);
    view! {
        <tr>
            <td><span class=move || format!("sg-action {}", if buy { "buy" } else { "sell" })>{if buy { "BUY" } else { "SELL" }}</span></td>
            <td class="sg-leg-label">{l.label.clone()}</td>
            <td class="num">{rs_usd(l.px)}</td>
            <td><span class="sg-venue">{fmt::venue_name(&l.venue)}</span></td>
            <td class="num">{l.iv_vp.map(|v| format!("{v:.1}")).unwrap_or_else(|| "—".into())}</td>
            <td class="num">{l.delta.map(|v| format!("{v:+.2}")).unwrap_or_else(|| "—".into())}</td>
            <td class="num dim">{l.min_size.map(|s| short(&fmt::fmt_int(s), 8)).unwrap_or_else(|| "touch".into())}</td>
        </tr>
    }
    .into_any()
}
