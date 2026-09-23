//! App shell: header, hero, and the six numbered sections from the product
//! spec (00 scanner · 01 reading · 02 math · 03 execution · 04 venues · 05 risk).

use leptos::prelude::*;

use crate::fmt;
use crate::sections::{calc, risk, scanner, steps, venues};
use crate::state::{ApiState, WsState};

#[component]
pub fn App() -> impl IntoView {
    let st = ApiState::start();
    provide_context(st);

    view! {
        <div class="app">
            <TopBar/>
            <FeedBanner/>
            <Hero/>
            <main class="content">
                <scanner::ScannerSection/>
                <LegendSection/>
                <calc::MathSection/>
                <steps::StepsSection/>
                <venues::VenuesSection/>
                <risk::RiskSection/>
            </main>
            <Footer/>
        </div>
    }
}

#[component]
fn TopBar() -> impl IntoView {
    let st = use_context::<ApiState>().unwrap();
    let stats = Memo::new(move |_| st.stats.get());
    let venue_pills = Memo::new(move |_| {
        st.venues
            .get()
            .into_iter()
            .map(|v| {
                let cls = match v.state {
                    ivarb_protocol::ConnState::Live => "dot live",
                    ivarb_protocol::ConnState::Connecting | ivarb_protocol::ConnState::Degraded => "dot warn",
                    _ => "dot dead",
                };
                let rate = if v.msg_rate > 0.0 {
                    format!("{:.1}/s", v.msg_rate)
                } else {
                    "—".into()
                };
                (v, cls, rate)
            })
            .collect::<Vec<_>>()
    });

    view! {
        <header class="topbar">
            <div class="topbar-inner">
                <div class="brand">
                    <span class="brand-mark">"IVARB"</span>
                    <span class="brand-sub">"cross-exchange IV arbitrage scanner"</span>
                </div>
                <div class="topbar-stats">
                    <StatChip label="feed"
                        value=move || {
                            let s = stats.get();
                            let mode = s.as_ref().map(|x| x.feed_mode).unwrap_or(ivarb_protocol::FeedMode::Live);
                            match mode {
                                ivarb_protocol::FeedMode::Live => "LIVE".to_string(),
                                ivarb_protocol::FeedMode::Sim => "SIMULATED".to_string(),
                                ivarb_protocol::FeedMode::Mixed => "MIXED".to_string(),
                            }
                        }
                        class_extra=move || {
                            let s = stats.get();
                            match s.as_ref().map(|x| x.feed_mode).unwrap_or(ivarb_protocol::FeedMode::Live) {
                                ivarb_protocol::FeedMode::Live => "",
                                _ => " amber",
                            }.to_string()
                        }/>
                    <StatChip label="venues"
                        value=move || {
                            let s = stats.get();
                            format!("{}/{}", s.as_ref().map(|x| x.venues_live).unwrap_or(0),
                                    s.as_ref().map(|x| x.venues_total).unwrap_or(8))
                        }
                        class_extra=|| String::new()/>
                    <StatChip label="instruments"
                        value=move || {
                            let s = stats.get();
                            fmt::fmt_int(s.as_ref().map(|x| x.instruments_tracked).unwrap_or(0) as f64)
                        }
                        class_extra=|| String::new()/>
                    <StatChip label="opps"
                        value=move || {
                            let s = stats.get();
                            (s.as_ref().map(|x| x.opps_active).unwrap_or(0)).to_string()
                        }
                        class_extra=|| String::new()/>
                    <StatChip label="engine"
                        value=move || {
                            let s = stats.get();
                            format!("{:.0} µs", s.as_ref().map(|x| x.engine_loop_us_ewma).unwrap_or(0.0))
                        }
                        class_extra=|| String::new()/>
                    <div class="ws-state">
                        {move || {
                            match st.ws_state.get() {
                                WsState::Live => view! { <span class="dot live"></span> "streaming" }.into_any(),
                                WsState::Connecting => view! { <span class="dot warn"></span> "connecting" }.into_any(),
                                WsState::Reconnecting => view! { <span class="dot dead"></span> "reconnecting" }.into_any(),
                            }
                        }}
                    </div>
                </div>
            </div>
            <div class="venue-pills">
                <For each=move || venue_pills.get() key=|(v, _, _)| v.venue.clone() let:p>
                    {let (v, cls, rate) = p;
                     view! {
                        <span class=move || format!("venue-pill{}", if v.is_sim { " sim" } else { "" })>
                            <span class=cls></span>
                            <span class="vp-name">{fmt::venue_name(&v.venue)}</span>
                            <span class="vp-rate">{rate}</span>
                        </span>
                    }}
                </For>
            </div>
        </header>
    }
}

#[component]
fn StatChip(
    label: &'static str,
    value: impl Fn() -> String + Send + Sync + 'static,
    class_extra: impl Fn() -> String + Send + Sync + 'static,
) -> impl IntoView {
    view! {
        <div class=move || format!("stat-chip{}", class_extra())>
            <span class="sc-label">{label}</span>
            <span class="sc-value">{move || value()}</span>
        </div>
    }
}

#[component]
fn FeedBanner() -> impl IntoView {
    let st = use_context::<ApiState>().unwrap();
    let show = Memo::new(move |_| {
        let s = st.stats.get();
        s.map(|x| x.feed_mode != ivarb_protocol::FeedMode::Live).unwrap_or(false)
    });
    view! {
        {move || if show.get() {
            view! {
                <div class="feed-banner">
                    <span class="fb-badge">"SIM"</span>
                    <span>
                        "Simulated market data — no live venue is reachable from this host. "
                        "All math, thresholds and the UI run exactly as they do on live feeds; run the server where the exchanges are reachable to go live."
                    </span>
                </div>
            }.into_any()
        } else {
            ().into_any()
        }}
    }
}

#[component]
fn Hero() -> impl IntoView {
    let st = use_context::<ApiState>().unwrap();
    let best_net = Memo::new(move |_| {
        st.opps
            .get()
            .iter()
            .map(|o| o.net_vp)
            .fold(f64::NEG_INFINITY, f64::max)
    });
    let best_usd = Memo::new(move |_| {
        st.opps
            .get()
            .iter()
            .map(|o| o.est_usd)
            .fold(f64::NEG_INFINITY, f64::max)
    });
    let spot = Memo::new(move |_| {
        let mut btc = None;
        let mut eth = None;
        for s in st.spots.get() {
            match s.underlying.as_str() {
                "BTC" => btc = Some(s.price),
                "ETH" => eth = Some(s.price),
                _ => {}
            }
        }
        (btc, eth)
    });

    view! {
        <section class="hero">
            <div class="hero-copy">
                <h1>"The Arb Scanner finds moments when the same option contract trades at a materially different implied volatility across two exchanges."</h1>
                <p>"This guide shows you how to read those signals, calculate whether the spread is worth it after fees, and execute both legs cleanly."</p>
                <div class="hero-tags">
                    <span class="tag">"8 exchanges wired"</span>
                    <span class="tag">"fee-adjusted math built in"</span>
                    <span class="tag">"7-step execution flow"</span>
                </div>
            </div>
            <div class="hero-stats">
                <HeroCard label="live opportunities"
                    value=move || (st.opps.get().len()).to_string()
                    sub=move || "fee-adjusted · live WS push".into()/>
                <HeroCard label="best net edge"
                    value=move || if best_net.get().is_finite() { format!("{:+.2} vp", best_net.get()) } else { "—".into() }
                    sub=move || "after round-trip taker fees".into()/>
                <HeroCard label="best est. / contract"
                    value=move || if best_usd.get().is_finite() { fmt::fmt_usd(best_usd.get()) } else { "—".into() }
                    sub=move || "net vp × vega, before slippage".into()/>
                <HeroCard label="consensus spot"
                    value=move || {
                        let (btc, eth) = spot.get();
                        match (btc, eth) {
                            (Some(b), Some(e)) => format!("{} / {}", fmt::fmt_spot(b), fmt::fmt_spot(e)),
                            (Some(b), None) => fmt::fmt_spot(b),
                            _ => "—".into(),
                        }
                    }
                    sub=move || "median across venues".into()/>
            </div>
        </section>
    }
}

#[component]
fn HeroCard(
    label: &'static str,
    value: impl Fn() -> String + Send + Sync + 'static,
    sub: impl Fn() -> String + Send + Sync + 'static,
) -> impl IntoView {
    view! {
        <div class="hero-card">
            <div class="hc-label">{label}</div>
            <div class="hc-value">{move || value()}</div>
            <div class="hc-sub">{move || sub()}</div>
        </div>
    }
}

/// Section 01 from the spec: what each scanner column means.
#[component]
fn LegendSection() -> impl IntoView {
    view! {
        <section class="section" id="reading">
            <SectionHead num="01" title="Reading the scanner"
                sub="Every row in the Arb Scanner is a single opportunity. These are the units and column meanings to internalize before you trade."/>
            <div class="legend-grid">
                <LegendCard title="Vol point (vp)"
                    body="1% of implied volatility. If IV moves from 60% to 61%, that is 1 vol point. All spreads, fees and edges in this tool are quoted in vp."/>
                <LegendCard title="Vega"
                    body="How much the option price changes per 1 vol point of IV move, in dollars per contract. It scales with the underlying's price: large for BTC, proportionally smaller for lower-priced assets."/>
                <LegendCard title="Net edge"
                    body="The profit you lock in per contract if both legs fill at the quoted prices, after round-trip taker fees on both venues."/>
                <LegendCard title="Edge mode"
                    body="EXEC means the edge is computed from the actual ask you would buy at and bid you would sell at. MARK falls back to venue marks and is a weaker signal."/>
                <LegendCard title="Age"
                    body="Time since the opportunity was first detected above the entry threshold. Fresh signals are actionable; stale ones are flagged and dimmed."/>
                <LegendCard title="Sizes"
                    body="Quoted size at the touch on both legs, in contracts, when venues publish it. The total capacity of the arb is the smaller of the two legs."/>
            </div>
        </section>
    }
}

#[component]
fn LegendCard(title: &'static str, body: &'static str) -> impl IntoView {
    view! {
        <div class="legend-card">
            <div class="lg-title">{title}</div>
            <p>{body}</p>
        </div>
    }
}

#[component]
pub fn SectionHead(num: &'static str, title: &'static str, sub: &'static str) -> impl IntoView {
    view! {
        <div class="section-head">
            <span class="section-num">{num}</span>
            <div>
                <h2>{title}</h2>
                <p>{sub}</p>
            </div>
        </div>
    }
}

#[component]
fn Footer() -> impl IntoView {
    view! {
        <footer class="footer">
            <div class="footer-inner">
                <div class="footer-col">
                    <div class="brand-mark">"IVARB"</div>
                    <p class="dim">"Signal, not advice. The scanner detects pricing discrepancies. It does not account for your risk tolerance, position limits, exchange withdrawal risk, or tax implications. Trade only with capital you can afford to lose."</p>
                </div>
                <div class="footer-col">
                    <h3>"Where to go next"</h3>
                    <ul>
                        <li><a href="#scanner">"Open the scanner to see live arb opportunities"</a></li>
                        <li><a href="#math">"Model your P&L at different IV outcomes in the calculator"</a></li>
                        <li><a href="#scanner">"Check the lead-lag hint on any opportunity to see which exchange is likely to revert"</a></li>
                    </ul>
                </div>
                <div class="footer-col">
                    <h3>"Under the hood"</h3>
                    <ul class="tech-list">
                        <li>"Rust end-to-end — Tokio engine, Axum server"</li>
                        <li>"This UI is compiled to WebAssembly (Leptos)"</li>
                        <li>"Incremental O(#venues) arbitrage detection"</li>
                        <li>"Black-Scholes IV solver normalizes every venue"</li>
                        <li>"Signal audit log at data/signals.jsonl"</li>
                    </ul>
                </div>
            </div>
        </footer>
    }
}
