//! Section 05: risk & reality check.

use leptos::prelude::*;

use crate::app::SectionHead;

const RISKS: &[(&str, &str, &str, &str)] = &[
    (
        "High",
        "Execution risk",
        "You need BOTH legs to fill. If leg A fills and leg B misses, you have a naked option position. Always set a maximum time you will wait for leg B. If it doesn't fill in 10 seconds, cancel leg A.",
        "high",
    ),
    (
        "High",
        "Data latency",
        "The scanner reads data from consolidated venue feeds. If a feed lags, the signal may be stale. Cross-check the IV on both exchange UIs before placing any order.",
        "high",
    ),
    (
        "Medium",
        "Liquidity",
        "Options outside the top 2-3 expiries can have very wide bid-ask spreads. The scanner's mark IV is not necessarily your fill IV. Check the actual bid/ask on both sides.",
        "medium",
    ),
    (
        "Medium",
        "Collateral on both exchanges",
        "To sell options you need margin on that exchange. Make sure you have sufficient collateral (USDC/USDT, or the underlying coin on coin-margined venues like Deribit) on both exchanges before entering the trade. Do not enter the buy leg if you can't cover the sell margin.",
        "medium",
    ),
    (
        "Low",
        "Convergence time",
        "The IV spread may not close immediately. You might hold this position for hours or days. The longer you hold, the more theta decay affects the value (though for near-expiry arbs this is manageable).",
        "low",
    ),
];

#[component]
pub fn RiskSection() -> impl IntoView {
    view! {
        <section class="section" id="risk">
            <SectionHead num="06" title="Risk & reality check"
                sub="IV arb is not free money. These are the failure modes you need to have a plan for before entering any trade."/>
            <div class="risk-grid">
                {RISKS.iter().map(|(sev, title, body, cls)| {
                    view! {
                        <div class=move || format!("risk-card {cls}")>
                            <div class="risk-head">
                                <span class=move || format!("sev-chip {cls}")>{*sev}</span>
                                <h3>{*title}</h3>
                            </div>
                            <p>{*body}</p>
                        </div>
                    }
                }).collect::<Vec<_>>()}
            </div>
            <div class="risk-note">
                <strong>"This is signal, not advice. "</strong>
                "The scanner detects pricing discrepancies. It does not account for your risk tolerance, position limits, exchange withdrawal risk, or tax implications. Trade only with capital you can afford to lose."
            </div>
        </section>
    }
}
