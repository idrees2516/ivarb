//! Section 04: per-exchange execution instructions with live status.

use leptos::prelude::*;

use crate::app::SectionHead;
use crate::fmt;
use crate::state::ApiState;

struct VenueGuide {
    id: &'static str,
    fee_note: &'static str,
    min_note: &'static str,
    margin_note: &'static str,
    steps: &'static [&'static str],
}

const GUIDES: &[VenueGuide] = &[
    VenueGuide {
        id: "deribit",
        fee_note: "0.03% of underlying (taker) — ≈ $25 per 1 BTC contract at $84k",
        min_note: "0.1 BTC per order",
        margin_note: "Coin-margined: premiums, P&L and vega are quoted in BTC. Multiply by spot for USD.",
        steps: &[
            "Go to deribit.com → Trade → Options → BTC",
            "Select the expiry from the top expiry row",
            "Find the strike (the scanner row shows it exactly)",
            "Click the IV column to see mark IV; confirm it matches the scanner",
            "To BUY: click the Ask price to open the order form → Limit, size in BTC → Submit",
            "To SELL: click the Bid price → same flow with direction = Sell",
            "Feed for this venue: REST book-summary poll (Deribit's public WS tickers now require authentication)",
        ],
    },
    VenueGuide {
        id: "okx",
        fee_note: "0.03% of underlying (taker, indicative)",
        min_note: "per contract (ctVal published per instrument)",
        margin_note: "BTC-USD options are inverse: premiums quoted in BTC per contract.",
        steps: &[
            "Go to okx.com → Trade → Options",
            "Pick the expiry and strike from the chain",
            "Open the order panel for the exact contract",
            "BUY leg: take the ask with a limit order at or inside the touch",
            "SELL leg: hit the bid; confirm the size shown at the touch",
            "Feed for this venue: WS tickers per instrument + index feed",
        ],
    },
    VenueGuide {
        id: "bybit",
        fee_note: "0.03% of underlying (taker, indicative)",
        min_note: "per instrument lot size filter",
        margin_note: "USDT-margined; premiums in USDT. Contract covers 1 BTC (verified via deep-ITM parity).",
        steps: &[
            "Go to bybit.com → Trade → Options → BTC",
            "Select expiry, then strike from the chain",
            "Verify mark IV and the bid/ask against the scanner row",
            "BUY leg: limit at the ask; SELL leg: limit at the bid",
            "Sizes at the touch are published — respect the smaller leg",
            "Feed for this venue: WS tickers snapshot channel",
        ],
    },
    VenueGuide {
        id: "binance",
        fee_note: "0.02% of underlying (taker, indicative)",
        min_note: "check eapi exchangeInfo per symbol",
        margin_note: "European options, USDT-settled.",
        steps: &[
            "Go to binance.com → Derivatives → Options",
            "Select the underlying and expiry",
            "Locate the strike in the chain",
            "Quotes on wings can be stale — always verify against the book",
            "BUY leg: limit at ask; SELL leg: limit at bid",
            "Feed for this venue: eapi mark-price poll (5s)",
        ],
    },
    VenueGuide {
        id: "delta",
        fee_note: "0.03% of underlying (taker, indicative)",
        min_note: "0.01 BTC contract value (published per product)",
        margin_note: "USDT-settled.",
        steps: &[
            "Go to delta.exchange → Options",
            "Pick the expiry and strike",
            "Confirm bid/ask IVs against the scanner",
            "Execute both legs with limit orders at the touch",
            "Feed for this venue: REST ticker poll (5s) with native quote IVs",
        ],
    },
    VenueGuide {
        id: "aevo",
        fee_note: "0.02% of underlying (taker, indicative)",
        min_note: "per instrument amount step",
        margin_note: "USDC-settled options DEX.",
        steps: &[
            "Go to aevo.xyz → Trade → Options",
            "Select the expiry and strike",
            "RFQ or take the quoted book as appropriate",
            "Confirm the mark and IV against the scanner before both legs",
            "Feed for this venue: REST markets poll",
        ],
    },
    VenueGuide {
        id: "derive",
        fee_note: "0.03% of underlying (indicative)",
        min_note: "per instrument",
        margin_note: "USD-settled options DEX on an L2.",
        steps: &[
            "Connector is experimental — endpoint shape changed and auto-disabled on this host",
            "Enable by running the server where api.derive.xyz is reachable, then verify instruments",
            "IVs are quoted natively by the venue",
        ],
    },
    VenueGuide {
        id: "thalex",
        fee_note: "0.02% of underlying (indicative)",
        min_note: "per instrument",
        margin_note: "USD-margined.",
        steps: &[
            "Connector is experimental — host could not reach api.thalex.com",
            "Run the server from a network with access to enable it",
            "Symbols parse in the standard BTC-date-strike-right format",
        ],
    },
];

#[component]
pub fn VenuesSection() -> impl IntoView {
    let st = use_context::<ApiState>().unwrap();
    let tab = RwSignal::new("deribit".to_string());

    let live_status = Memo::new(move |_| {
        st.venues
            .get()
            .into_iter()
            .map(|v| (v.venue.clone(), v))
            .collect::<Vec<_>>()
    });

    view! {
        <section class="section" id="venues">
            <SectionHead num="04" title="Exchange instructions"
                sub="Each exchange has a slightly different UI flow for placing options orders. Select the exchange you need — fee rates are indicative; verify on the venue."/>
            <div class="venues-panel">
                <div class="venue-tabs">
                    {GUIDES.iter().map(|g| {
                        let id = g.id;
                        view! {
                            <button class=move || if tab.get() == id { "vt-btn active" } else { "vt-btn" }
                                on:click=move |_| tab.set(id.to_string())>
                                {id}
                            </button>
                        }
                    }).collect::<Vec<_>>()}
                </div>
                {move || {
                    let id = tab.get();
                    let g = GUIDES.iter().find(|g| g.id == id).unwrap_or(&GUIDES[0]);
                    let status = live_status.get().into_iter().find(|(v, _)| *v == g.id).map(|(_, s)| s);
                    let (state_cls, state_txt, inst) = match &status {
                        Some(s) => (
                            match s.state {
                                ivarb_protocol::ConnState::Live => "vs-dot live".to_string(),
                                ivarb_protocol::ConnState::Connecting | ivarb_protocol::ConnState::Degraded => "vs-dot warn".to_string(),
                                _ => "vs-dot dead".to_string(),
                            },
                            fmt::fmt_conn_state(&s.state).to_string(),
                            s.instruments,
                        ),
                        None => ("vs-dot dead".to_string(), "not connected".to_string(), 0),
                    };
                    view! {
                        <div class="venue-guide">
                            <div class="vg-head">
                                <span class=state_cls.clone()></span>
                                <h3>{g.id}</h3>
                                <span class="vs-state">{state_txt}</span>
                                <span class="vs-inst">{format!("{} instruments", inst)}</span>
                            </div>
                            <div class="vg-meta">
                                <div class="vg-meta-item"><span>"Taker fee"</span><strong>{g.fee_note}</strong></div>
                                <div class="vg-meta-item"><span>"Min size"</span><strong>{g.min_note}</strong></div>
                                <div class="vg-meta-item"><span>"Margin"</span><strong>{g.margin_note}</strong></div>
                            </div>
                            <ol class="vg-steps">
                                {g.steps.iter().map(|s| view! { <li>{*s}</li> }).collect::<Vec<_>>()}
                            </ol>
                        </div>
                    }.into_any()
                }}
            </div>
        </section>
    }
}
