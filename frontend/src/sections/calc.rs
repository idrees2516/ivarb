//! Section 02: the live fee/edge calculator (mirrors the product spec math,
//! running the same `ivarb-quant` code as the engine, compiled to WASM).

use leptos::prelude::*;

use crate::app::SectionHead;
use crate::fmt;
use crate::state::ApiState;
use ivarb_quant::fee_vp;

const VENUES: &[(&str, f64, bool)] = &[
    ("deribit", 0.0003, true),
    ("okx", 0.0003, false),
    ("bybit", 0.0003, false),
    ("binance", 0.0002, false),
    ("delta", 0.0003, false),
    ("aevo", 0.0002, false),
    ("derive", 0.0003, false),
    ("thalex", 0.0002, false),
];

fn fee_of(venue: &str) -> f64 {
    VENUES
        .iter()
        .find(|(v, _, _)| *v == venue)
        .map(|(_, f, _)| *f)
        .unwrap_or(0.0003)
}

fn venue_list() -> Vec<String> {
    VENUES.iter().map(|(v, _, _)| v.to_string()).collect()
}

#[component]
pub fn MathSection() -> impl IntoView {
    let st = use_context::<ApiState>().unwrap();

    let venue_a = RwSignal::new("okx".to_string());
    let venue_b = RwSignal::new("bybit".to_string());
    let iv_a = RwSignal::new(62.0);
    let iv_b = RwSignal::new(68.0);
    let vega = RwSignal::new(843.83);
    let spot = RwSignal::new(84_383.0);

    // Seed the spot from consensus when it first arrives.
    Effect::new(move |_| {
        let spots = st.spots.get();
        if let Some(s) = spots.iter().find(|s| s.underlying == "BTC") {
            let d = spot.get() - 84_383.0f64;
            if d.abs() < 1.0 && s.price > 0.0 {
                spot.set(s.price);
            }
        }
    });

    let out = Memo::new(move |_| {
        let a = venue_a.get();
        let b = venue_b.get();
        let iva = iv_a.get();
        let ivb = iv_b.get();
        let v = vega.get();
        let s = spot.get();
        let spread = ivb - iva;
        let fee = if v > 0.0 {
            fee_of(&a) * s / v + fee_of(&b) * s / v
        } else {
            f64::INFINITY
        };
        let net = spread - fee;
        (a, b, spread, fee, net, v, s)
    });

    let viable = Memo::new(move |_| out.get().4 >= 0.5);

    let fill_from_best = move |_| {
        let mut best: Option<ivarb_protocol::Opportunity> = None;
        for o in st.opps.get() {
            if best.as_ref().map(|b| o.est_usd > b.est_usd).unwrap_or(true) {
                best = Some(o);
            }
        }
        if let Some(o) = best {
            venue_a.set(fmt::venue_name(&o.buy_venue));
            venue_b.set(fmt::venue_name(&o.sell_venue));
            iv_a.set(o.iv_buy_vp);
            iv_b.set(o.iv_sell_vp);
            vega.set((o.vega_vp_usd * 100.0).round() / 100.0);
            spot.set(o.spot);
        }
    };

    let names = store_value(venue_list());

    view! {
        <section class="section" id="math">
            <SectionHead num="07" title="The math: live calculator"
                sub="Sanity-check any signal before trading. The key question is always: does the IV spread cover both legs of taker fees?"/>
            <div class="calc-panel">
                <div class="calc-inputs">
                    <CalcSelect label="Exchange A" value=venue_a options=names/>
                    <CalcNum label="ATM IV A (%)" value=iv_a step="0.01"/>
                    <CalcSelect label="Exchange B" value=venue_b options=names/>
                    <CalcNum label="ATM IV B (%)" value=iv_b step="0.01"/>
                    <CalcNum label="ATM Vega (USD/vp per contract)" value=vega step="0.01"/>
                    <CalcNum label="Spot (USD)" value=spot step="1"/>
                    <div class="calc-fill">
                        <button class="btn" on:click=fill_from_best>"Fill from best live opportunity"</button>
                    </div>
                </div>
                <div class="calc-outputs">
                    <div class="calc-row">
                        <CalcOut label="IV Spread" value=move || format!("{:+.2} vp", out.get().2)/>
                        <CalcOut label="Fee Cost" value=move || format!("\u{2212}{:.3} vp", out.get().3)/>
                        <CalcOut label="Net Edge" value=move || format!("{:+.3} vp", out.get().4)/>
                    </div>
                    {move || {
                        let (a, b, _sp, _f, net, v, _s) = out.get();
                        let profit = net * v;
                        if viable.get() {
                            let (buy, sell) = if iv_a.get() <= iv_b.get() { (a.clone(), b.clone()) } else { (b.clone(), a.clone()) };
                            view! {
                                <div class="calc-verdict ok">
                                    <span class="cv-check">"✓"</span>
                                    <span>
                                        "Viable. BUY on "<strong>{buy}</strong>" · SELL on "<strong>{sell}</strong>
                                        " · Est. profit \u{2248} "<strong>{fmt::fmt_usd(profit)}</strong>" / contract"
                                    </span>
                                </div>
                            }.into_any()
                        } else {
                            view! {
                                <div class="calc-verdict bad">
                                    <span class="cv-check">"✗"</span>
                                    <span>
                                        "Not viable — the spread does not clear the 0.50 vp entry gate after "
                                        "round-trip fees. You need "<strong>{format!("{:+.2} vp", 0.5 - net)}</strong>" more spread."
                                    </span>
                                </div>
                            }.into_any()
                        }
                    }}
                    <div class="calc-notes">
                        <p>
                            <strong>"Deribit is coin-margined: "</strong>
                            "it reports premiums and vega in the underlying coin (BTC), not USD. If using Deribit, enter vega × spot as the USD equivalent above. Every other venue here quotes vega in USD directly."
                        </p>
                        <p>
                            <strong>"Fee formula: "</strong>
                            <code>"fee_vp = (fee_rate × spot_price) ÷ vega_usd"</code>
                            ". Worked example: spot $65,000, vega $1,000, Deribit's 0.03% rate: (0.0003 × 65000) ÷ 1000 = 0.0195 vp per leg. Two legs ≈ 0.039 vp. You need at least ~0.539 vp of IV spread to clear the 0.5 vp gate. The ratio is roughly asset-invariant: both the fee (a fraction of notional) and vega scale with spot, so the per-leg fee_vp stays near 0.02 for any asset."
                        </p>
                    </div>
                </div>
            </div>
        </section>
    }
}

#[component]
fn CalcSelect(
    label: &'static str,
    value: RwSignal<String>,
    options: leptos::prelude::StoredValue<Vec<String>>,
) -> impl IntoView {
    view! {
        <label class="calc-field">
            <span class="cf-label">{label}</span>
            <select class="cf-input"
                on:change=move |ev| {
                    let v = event_target_value(&ev);
                    value.set(v);
                }>
                {move || {
                    options.with_value(|opts| {
                        opts.iter()
                            .map(|o| {
                                let sel = value.get() == *o;
                                view! { <option value=o.clone() selected=sel>{o.clone()}</option> }
                            })
                            .collect::<Vec<_>>()
                    })
                }}
            </select>
        </label>
    }
}

#[component]
fn CalcNum(
    label: &'static str,
    value: RwSignal<f64>,
    step: &'static str,
) -> impl IntoView {
    view! {
        <label class="calc-field">
            <span class="cf-label">{label}</span>
            <input class="cf-input" type="number"
                step=step
                prop:value=move || fmt_num(value.get())
                on:input=move |ev| {
                    let v = event_target_value(&ev);
                    if let Ok(f) = v.parse::<f64>() {
                        value.set(f);
                    }
                }/>
        </label>
    }
}

fn fmt_num(v: f64) -> String {
    if (v - v.round()).abs() < 1e-9 {
        format!("{}", v.round())
    } else {
        format!("{v}")
    }
}

#[component]
fn CalcOut(label: &'static str, value: impl Fn() -> String + Send + Sync + 'static) -> impl IntoView {
    view! {
        <div class="calc-out">
            <div class="co-label">{label}</div>
            <div class="co-value">{move || value()}</div>
        </div>
    }
}

#[allow(dead_code)]
fn unused(v: f64) -> f64 {
    fee_vp(0.0, v, 1.0)
}
