//! Section 03: the seven-step execution flow.

use leptos::prelude::*;

use crate::app::SectionHead;

const STEPS: &[(&str, &str)] = &[
    (
        "Spot the signal",
        "The Arb Scanner fires when net_edge_vp ≥ 0.50. Watch the scanner table above. When a row appears, note four things: expiry, strike, buy-exchange and sell-exchange. The direction is chosen so you always buy the cheaper IV and sell the richer one.",
    ),
    (
        "Verify freshness",
        "Check the age chip on the row. Signals confirmed seconds ago are actionable; anything older than a minute decays quickly in reliability. Cross-check both IVs on the exchange UIs themselves before committing capital — the scanner reads from consolidated feeds and a stale print is the number one false signal.",
    ),
    (
        "Open both exchanges simultaneously",
        "Arrange both trading windows side by side with the order tickets pre-filled: the exact instrument (same expiry, strike and type), the direction on each venue, and your size. The most critical window is the next 10 seconds.",
    ),
    (
        "Place the BUY leg (cheaper exchange)",
        "Submit a limit order at the current ask (or one tick through it) on the venue with the lower IV. Use IOC or post-only-aware limits; never market-sell into a wide book. This leg pays the taker fee that the fee-adjusted math has already accounted for.",
    ),
    (
        "Place the SELL leg (expensive exchange)",
        "Within seconds of the buy fill, sell the identical contract on the richer venue at the current bid. If this leg does not fill within 10 seconds, cancel the remaining order and unwind the buy leg — holding one naked leg converts an arbitrage into a directional bet.",
    ),
    (
        "Delta-hedge the net position",
        "A long call on venue A and a short call on venue B leave you with residual delta only from the price difference between the two fills. Offset it with the perpetual or future on either venue so the position is purely a volatility spread.",
    ),
    (
        "Monitor and close",
        "Track the spread history chart. Exit when the net edge compresses toward zero (the divergence has converged), when either leg's quote goes stale, or at your maximum holding time. The longer you hold, the more theta and pin risk accumulate, though near-expiry arbs usually converge within hours.",
    ),
];

#[component]
pub fn StepsSection() -> impl IntoView {
    let step = RwSignal::new(0usize);
    let n = STEPS.len();

    view! {
        <section class="section" id="execution">
            <SectionHead num="03" title="Step-by-step execution"
                sub="Click through each step. The most critical moment is steps 04-05: placing both legs within seconds of each other."/>
            <div class="steps-panel">
                <div class="steps-rail">
                    {STEPS.iter().enumerate().map(|(i, (title, _))| {
                        let idx = i;
                        view! {
                            <button class=move || {
                                let mut c = String::from("step-tab");
                                if step.get() == idx { c.push_str(" active"); }
                                else if step.get() > idx { c.push_str(" done"); }
                                c
                            }
                                on:click=move |_| step.set(idx)>
                                <span class="st-num">{format!("{:02}", idx + 1)}</span>
                                <span class="st-title">{*title}</span>
                            </button>
                        }
                    }).collect::<Vec<_>>()}
                </div>
                <div class="step-body">
                    {move || {
                        let (title, body) = STEPS[step.get().min(n - 1)];
                        view! {
                            <div class="step-card">
                                <div class="step-card-head">
                                    <span class="step-big">{format!("{:02}", step.get() + 1)}</span>
                                    <h3>{title}</h3>
                                </div>
                                <p>{body}</p>
                                <div class="step-nav">
                                    <button class="btn ghost" on:click=move |_| {
                                        step.update(|s| *s = s.saturating_sub(1));
                                    }>"← prev"</button>
                                    <span class="step-count">{move || format!("{} / {}", step.get() + 1, n)}</span>
                                    <button class="btn" on:click=move |_| {
                                        step.update(|s| *s = (*s + 1).min(n - 1));
                                    }>"next →"</button>
                                </div>
                            </div>
                        }
                    }}
                </div>
            </div>
        </section>
    }
}
