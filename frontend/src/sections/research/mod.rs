//! Section R: the agentic research system — consolidated cross-venue data,
//! an AI-SDK-style tool-calling agent, candles/indicators, vol analytics,
//! liquidity heatmaps and the market-coherent option builder.

pub mod builder;
pub mod chat;
pub mod charts;
pub mod panels;

use leptos::prelude::*;

use ivarb_protocol::research::{
    CandleSeries, ChainResponse, IvAnalytics, LiquidityMap, ResearchPulse, ResearchStatus,
    SuggestionCard, TicksResponse,
};
use ivarb_protocol::Right;

use crate::api::{get_json, set_interval, spawn};
use crate::app::SectionHead;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum RsTab {
    Pulse,
    Ticks,
    Chain,
    Charts,
    Vol,
    Liquidity,
    Builder,
}

impl RsTab {
    fn label(self) -> &'static str {
        match self {
            RsTab::Pulse => "Pulse",
            RsTab::Ticks => "Ticks 50ms",
            RsTab::Chain => "Chains",
            RsTab::Charts => "Charts",
            RsTab::Vol => "Vol",
            RsTab::Liquidity => "Liquidity",
            RsTab::Builder => "Builder",
        }
    }
}

/// Shared research-section state (all cheap signal clones).
#[derive(Clone, Copy)]
pub struct Rs {
    pub und: RwSignal<String>,
    pub tab: RwSignal<RsTab>,
    pub pulse: RwSignal<Option<ResearchPulse>>,
    pub status: RwSignal<Option<ResearchStatus>>,
    pub chain: RwSignal<Option<ChainResponse>>,
    pub expiry: RwSignal<Option<u32>>,
    pub right_filter: RwSignal<Option<Right>>,
    pub expiry_list: RwSignal<Vec<(u32, String, i32)>>,
    pub candles: RwSignal<Option<CandleSeries>>,
    pub interval: RwSignal<String>,
    pub iv: RwSignal<Option<IvAnalytics>>,
    pub liquidity: RwSignal<Option<LiquidityMap>>,
    /// Tick-level BBO plane (Paradex 50 ms books + Derive v3 tickers).
    pub ticks: RwSignal<Option<TicksResponse>>,
    /// Latest builder card (also surfaced by the agent chat).
    pub card: RwSignal<Option<SuggestionCard>>,
    /// Builder → chat handoff: a natural-language ask to run.
    pub ask_agent: RwSignal<Option<String>>,
    /// Agent busy flag shared with the chat panel.
    pub agent_busy: RwSignal<bool>,
}

impl Rs {
    fn start() -> Self {
        let und: RwSignal<String> = RwSignal::new("BTC".into());
        let tab: RwSignal<RsTab> = RwSignal::new(RsTab::Pulse);
        let rs = Self {
            und,
            tab,
            pulse: RwSignal::new(None),
            status: RwSignal::new(None),
            chain: RwSignal::new(None),
            expiry: RwSignal::new(None),
            right_filter: RwSignal::new(None),
            expiry_list: RwSignal::new(Vec::new()),
            candles: RwSignal::new(None),
            interval: RwSignal::new("1h".into()),
            iv: RwSignal::new(None),
            liquidity: RwSignal::new(None),
            ticks: RwSignal::new(None),
            card: RwSignal::new(None),
            ask_agent: RwSignal::new(None),
            agent_busy: RwSignal::new(false),
        };
        provide_context(rs.clone());

        // Initial fetch + periodic refresh.
        let rs2 = rs.clone();
        spawn(async move {
            refresh_pulse(&rs2);
            refresh_status(&rs2);
            refresh_expiries(&rs2);
            refresh_tab(&rs2);
        });
        let rs3 = rs.clone();
        set_interval(6_000, move || {
            refresh_pulse(&rs3);
        });
        let rs4 = rs.clone();
        set_interval(25_000, move || {
            refresh_tab(&rs4);
        });
        let rs5 = rs.clone();
        set_interval(60_000, move || {
            refresh_status(&rs5);
        });
        // The Ticks tab is a live wire: poll the tick plane at 800 ms while it
        // is open (ages/quotes visibly refresh; per-frame exchange cadence is
        // 50 ms class — the REST snapshot shows the freshest state each poll).
        let rs6b = rs.clone();
        set_interval(800, move || {
            if rs6b.tab.get_untracked() == RsTab::Ticks {
                refresh_ticks(&rs6b);
            }
        });

        // React to tab / und / interval / expiry changes with an immediate
        // refresh of exactly the panels that changed.
        let rs6 = rs.clone();
        Effect::new(move || {
            let _ = (rs6.tab.get(), rs6.und.get(), rs6.interval.get(), rs6.expiry.get());
            let rs7 = rs6.clone();
            spawn(async move {
                refresh_tab(&rs7);
            });
        });

        rs
    }
}

fn refresh_pulse(rs: &Rs) {
    let pulse = rs.pulse;
    spawn(async move {
        if let Some(p) = get_json::<ResearchPulse>("/api/research/pulse").await {
            pulse.set(Some(p));
        }
    });
}

fn refresh_status(rs: &Rs) {
    let status = rs.status;
    spawn(async move {
        if let Some(s) = get_json::<ResearchStatus>("/api/research/status").await {
            status.set(Some(s));
        }
    });
}

fn refresh_expiries(rs: &Rs) {
    let und = rs.und.get_untracked();
    let list = rs.expiry_list;
    spawn(async move {
        let url = format!("/api/research/expiries?und={und}");
        if let Some(l) = get_json::<Vec<(u32, String, i32)>>(&url).await {
            list.set(l);
        }
    });
}

fn refresh_ticks(rs: &Rs) {
    let und = rs.und.get_untracked();
    let ticks = rs.ticks;
    spawn(async move {
        let url = format!("/api/research/ticks?und={und}&limit=90");
        if let Some(t) = get_json::<TicksResponse>(&url).await {
            ticks.set(Some(t));
        }
    });
}

/// Fetch whatever the currently active tab needs.
fn refresh_tab(rs: &Rs) {
    let tab = rs.tab.get_untracked();
    let und = rs.und.get_untracked();
    match tab {
        RsTab::Pulse => refresh_pulse(rs),
        RsTab::Ticks => refresh_ticks(rs),
        RsTab::Chain => {
            let expiry = rs
                .expiry
                .get_untracked()
                .map(|e| format!("&expiry={e}"))
                .unwrap_or_default();
            let chain = rs.chain;
            spawn(async move {
                let url = format!("/api/research/chain?und={und}&window=10{expiry}");
                if let Some(c) = get_json::<ChainResponse>(&url).await {
                    chain.set(Some(c));
                }
            });
            refresh_expiries(rs);
        }
        RsTab::Charts => {
            let interval = rs.interval.get_untracked();
            let candles = rs.candles;
            spawn(async move {
                let url = format!("/api/research/candles?und={und}&interval={interval}");
                if let Some(c) = get_json::<CandleSeries>(&url).await {
                    candles.set(Some(c));
                }
            });
        }
        RsTab::Vol => {
            let iv = rs.iv;
            spawn(async move {
                let url = format!("/api/research/iv?und={und}");
                if let Some(a) = get_json::<IvAnalytics>(&url).await {
                    iv.set(Some(a));
                }
            });
        }
        RsTab::Liquidity => {
            let liq = rs.liquidity;
            spawn(async move {
                let url = format!("/api/research/liquidity?und={und}");
                if let Some(m) = get_json::<LiquidityMap>(&url).await {
                    liq.set(Some(m));
                }
            });
        }
        RsTab::Builder => refresh_pulse(rs),
    }
}

#[component]
pub fn ResearchSection() -> impl IntoView {
    let rs = Rs::start();
    let tabs = [
        RsTab::Pulse,
        RsTab::Ticks,
        RsTab::Chain,
        RsTab::Charts,
        RsTab::Vol,
        RsTab::Liquidity,
        RsTab::Builder,
    ];

    view! {
        <section class="section rs-section" id="research">
            <SectionHead num="R"
                title="Research agent — create options in coherence with the market"
                sub="A tool-calling agent over a live consolidated cross-venue book: every venue's chain, candles, IV structure, liquidity maps and a builder that strikes structures from real quotes — Deribit, Bybit, OKX, Binance, Delta, Aevo, Derive and the Paradex TradeOptions universe."/>
            <div class="rs-mode">
                {move || {
                    let p = rs.pulse.get();
                    let mode = p.as_ref().map(|p| p.data_mode.clone()).unwrap_or_else(|| "live".into());
                    if mode != "live" {
                        view! {
                            <span class="rs-mode-badge sim">{
                                if mode == "sim" { "SIM DATA" } else { "MIXED FEED" }
                            }</span>
                            <span class="dim">{
                                if mode == "sim" {
                                    "No live venue is reachable from this host — the research plane is running on a clearly-labeled synthetic book so every panel stays demonstrable."
                                } else {
                                    "Some venues are simulated."
                                }
                            }</span>
                        }.into_any()
                    } else {
                        ().into_any()
                    }
                }}
            </div>
            <div class="rs-grid">
                <div class="rs-chat-col">
                    <chat::AgentChat rs=rs.clone()/>
                </div>
                <div class="rs-main-col">
                    <div class="rs-controls">
                        <div class="rs-und-chips">
                            {move || {
                                let cur = rs.und.get();
                                ["BTC", "ETH", "SOL"].iter().map(|u| {
                                    let u = *u;
                                    let active = cur == u;
                                    view! {
                                        <button class=move || format!("rs-und-chip{}", if active { " on" } else { "" })
                                            on:click=move |_| {
                                                rs.und.set(u.to_string());
                                                rs.expiry.set(None);
                                            }>
                                            {u}
                                        </button>
                                    }.into_any()
                                }).collect::<Vec<_>>()
                            }}
                        </div>
                        <div class="rs-tabs">
                            {tabs.iter().map(|t| {
                                let t = *t;
                                view! {
                                    <button class=move || format!("rs-tab{}", if rs.tab.get() == t { " on" } else { "" })
                                        on:click=move |_| rs.tab.set(t)>
                                        {t.label()}
                                    </button>
                                }.into_any()
                            }).collect::<Vec<_>>()}
                        </div>
                    </div>
                    <div class="rs-panel">
                        {move || match rs.tab.get() {
                            RsTab::Pulse => view! { <panels::PulsePanel rs=rs.clone()/> }.into_any(),
                            RsTab::Ticks => view! { <panels::TicksPanel rs=rs.clone()/> }.into_any(),
                            RsTab::Chain => view! { <panels::ChainPanel rs=rs.clone()/> }.into_any(),
                            RsTab::Charts => view! { <charts::ChartsPanel rs=rs.clone()/> }.into_any(),
                            RsTab::Vol => view! { <charts::VolPanel rs=rs.clone()/> }.into_any(),
                            RsTab::Liquidity => view! { <panels::LiquidityPanel rs=rs.clone()/> }.into_any(),
                            RsTab::Builder => view! { <builder::BuilderPanel rs=rs.clone()/> }.into_any(),
                        }}
                    </div>
                </div>
            </div>
        </section>
    }
}

/// Shared formatting for USD money in the research section.
pub fn rs_usd(v: f64) -> String {
    crate::fmt::fmt_usd(v)
}

pub fn rs_pct(v: f64) -> String {
    format!("{v:+.1}%")
}
