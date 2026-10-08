//! Section 02: the Advantage Taker — auto-execution control room.
//!
//! Arm/disarm, kill switch, live risk meters, the time-aware strategy
//! matrix, open positions with mark-to-market, closed trades with capture
//! accounting, and the streaming execution log.

use leptos::prelude::*;

use crate::app::SectionHead;
use crate::fmt;
use crate::state::ApiState;
use ivarb_protocol::{ExecCommand, ExecLevel, ExecMode, ExecSettings, ExitReason, Position, Tier, Trade};

#[component]
pub fn TakerSection() -> impl IntoView {
    let st = use_context::<ApiState>().unwrap();
    let snap = Memo::new(move |_| st.exec.get());

    view! {
        <section class="section" id="taker">
            <SectionHead num="02" title="Advantage taker"
                sub="The execution engine that reaps the scanner's signals: risk-gated two-leg attempts, the 10-second leg rule, delta hedging and time-aware exits. Paper mode simulates latency, slippage, partial fills and fees exactly as live order flow would."/>
            {move || match snap.get() {
                None => view! {
                    <div class="taker-boot">
                        <span class="dot warn"></span>
                        "connecting to the executor…"
                    </div>
                }.into_any(),
                Some(s) => {
                    let s2 = s.clone();
                    let s3 = s.clone();
                    let s4 = s.clone();
                    let s5 = s.clone();
                    let s6 = s.clone();
                    view! {
                        <ControlBar s=s.clone()/>
                        <EscapePanel s=s.clone()/>
                        <div class="taker-grid">
                            <div class="tk-col">
                                <PositionsPanel s=s2.clone()/>
                                <TradesPanel s=s3.clone()/>
                            </div>
                            <div class="tk-col">
                                <ExecLogPanel s=s4.clone()/>
                                <RouteCalibPanel s=s5.clone()/>
                                <StrategyMatrix s=s6.clone()/>
                            </div>
                        </div>
                    }.into_any()
                }
            }}
        </section>
    }
}

#[component]
fn ControlBar(s: ivarb_protocol::ExecSnapshot) -> impl IntoView {
    let st = use_context::<ApiState>().unwrap();
    let st_arm = st.clone();
    let st_disarm = st.clone();
    let st_kill = st.clone();
    let st_reset = st.clone();
    let st_flat = st.clone();
    let st_routes = st.clone();
    let st_escape = st.clone();
    let s1 = s.clone();
    let s2 = s.clone();
    let s3 = s.clone();
    let s4 = s.clone();
    let s5 = s.clone();
    let s6 = s.clone();
    let s7 = s.clone();
    let s8 = s.clone();
    let s9 = s.clone();

    let mode_label = match s.stats.mode {
        ExecMode::Paper => "PAPER",
        ExecMode::Live => "LIVE",
    };
    let day_pnl = s.stats.realized_usd + s.stats.unrealized_usd;
    let vega_pct = if s.settings.max_total_vega_usd > 0.0 {
        (s.stats.vega_used_usd / s.settings.max_total_vega_usd * 100.0).clamp(0.0, 100.0)
    } else {
        0.0
    };

    view! {
        <div class=move || {
            let mut c = String::from("taker-bar");
            if s1.stats.armed { c.push_str(" armed"); }
            if s1.stats.kill.is_some() { c.push_str(" killed"); }
            c
        }>
            <div class="tb-main">
                {if s2.stats.armed {
                    view! {
                        <button class="tb-btn disarm"
                            on:click=move |_| st_disarm.send_exec_cmd(ExecCommand::Arm { armed: false }, "auto-taker disarmed".into())>
                            "■ DISARM"
                        </button>
                    }.into_any()
                } else {
                    view! {
                        <button class="tb-btn arm"
                            on:click=move |_| st_arm.send_exec_cmd(ExecCommand::Arm { armed: true }, "auto-taker ARMED — scanning for qualifying edges".into())>
                            "▶ ARM THE TAKER"
                        </button>
                    }.into_any()
                }}
                <span class=move || format!("tb-mode {}", if matches!(s3.stats.mode, ExecMode::Live) { "live" } else { "paper" })>
                    {mode_label}
                </span>
                {match s4.stats.kill.clone() {
                    Some(reason) => view! {
                        <button class="tb-btn reset"
                            on:click=move |_| st_reset.send_exec_cmd(ExecCommand::ResetKill, "kill switch reset".into())>
                            {format!("KILL: {reason} — tap to reset")}
                        </button>
                    }.into_any(),
                    None => view! {
                        <button class="tb-btn kill"
                            on:click=move |_| st_kill.send_exec_cmd(ExecCommand::Kill { reason: None }, "KILL SWITCH engaged — flattening all positions".into())>
                            "☠ KILL SWITCH"
                        </button>
                    }.into_any(),
                }}
                {if s7.settings.routes_enabled {
                    view! {
                        <button class="tb-btn routes on"
                            on:click=move |_| {
                                let mut set = s7.settings.clone();
                                set.routes_enabled = false;
                                st_routes.send_exec_cmd(ExecCommand::UpdateSettings { settings: set }, "multi-leg route cycles OFF".into())
                            }>
                            "◈ ROUTES ON"
                        </button>
                    }.into_any()
                } else {
                    view! {
                        <button class="tb-btn routes off"
                            on:click=move |_| {
                                let mut set = s7.settings.clone();
                                set.routes_enabled = true;
                                st_routes.send_exec_cmd(ExecCommand::UpdateSettings { settings: set }, "multi-leg route cycles ON — auto-firing quality-gated cycles".into())
                            }>
                            "◈ ROUTES OFF"
                        </button>
                    }.into_any()
                }}
                {if s9.settings.escape_enabled {
                    view! {
                        <button class="tb-btn escape on"
                            title="0DTE escape: uncaptured arbs flip to same-day doctrine — hedge on a third venue, legs out at market, flat fast"
                            on:click=move |_| {
                                let mut set = s9.settings.clone();
                                set.escape_enabled = false;
                                st_escape.send_exec_cmd(ExecCommand::UpdateSettings { settings: set }, "0DTE escape OFF — failed captures trade their normal exit ladder".into())
                            }>
                            "⟁ 0DTE ESCAPE ON"
                        </button>
                    }.into_any()
                } else {
                    view! {
                        <button class="tb-btn escape off"
                            on:click=move |_| {
                                let mut set = s9.settings.clone();
                                set.escape_enabled = true;
                                st_escape.send_exec_cmd(ExecCommand::UpdateSettings { settings: set }, "0DTE escape ARMED — uncaptured arbs will rapid-exit delta-neutral".into())
                            }>
                            "⟁ 0DTE ESCAPE OFF"
                        </button>
                    }.into_any()
                }}
                <button class="tb-btn flat"
                    on:click=move |_| st_flat.send_exec_cmd(ExecCommand::FlattenAll, "flatten-all requested".into())>
                    "FLATTEN ALL"
                </button>
                <div class="tb-regime">
                    <span class="tbr-label">"regime"</span>
                    <span class="tbr-value">{format!("{} UTC · gate ×{:.2} · size ×{:.2}", s5.regime.utc_hour, s5.regime.gate_mult, s5.regime.size_mult)}</span>
                    <span class="tbr-sub">{s5.regime.label.clone()}</span>
                </div>
            </div>
            <div class="tb-meters">
                <Meter label="session P&L" value=fmt::fmt_usd(day_pnl)
                    class_extra={if day_pnl >= 0.0 { "pos" } else { "neg" }}/>
                <Meter label="realized" value=fmt::fmt_usd(s6.stats.realized_usd)
                    class_extra={if s6.stats.realized_usd >= 0.0 { "pos" } else { "neg" }}/>
                <Meter label="open / max" value=format!("{}/{}", s6.stats.positions_open, s6.settings.max_open_positions) class_extra=""/>
                <Meter label="vega at risk" value=format!("{} / {}", fmt::fmt_usd(s6.stats.vega_used_usd), fmt::fmt_usd(s6.settings.max_total_vega_usd)) class_extra=""/>
                <div class="tb-meter">
                    <span class="tbm-label">"vega budget"</span>
                    <div class="tbm-bar"><div class="tbm-fill" style=move || format!("width:{vega_pct:.0}%")></div></div>
                </div>
                <Meter label="attempts" value=format!("{} total · {} live", s6.stats.attempts_total, s6.stats.attempts_active) class_extra=""/>
                <Meter label="route cycles" value=format!("{} fired · {} live · {} banked", s6.stats.route_attempts, s6.stats.route_attempts_active, s6.stats.route_banked)
                    class_extra={if s6.stats.route_banked > 0 { "pos" } else { "" }}/>
                <Meter label="route P&L" value=fmt::fmt_usd(s6.stats.route_profit_usd)
                    class_extra={if s6.stats.route_profit_usd >= 0.0 { "pos" } else { "neg" }}/>
                <Meter label="single-leg events" value=s6.stats.single_leg_events.to_string()
                    class_extra={if s6.stats.single_leg_events > 0 { "neg" } else { "" }}/>
                <Meter label="capture ratio" value={if s6.stats.trades_closed > 0 { format!("{:.0}% of signal", s6.stats.capture_ratio * 100.0) } else { "—".into() }} class_extra=""/>
                <Meter label="0DTE escapes" value=format!("{} · {} live", s6.stats.escape_exits, s6.stats.escape_active)
                    class_extra={if s6.stats.escape_active > 0 { "neg" } else if s6.stats.escape_exits > 0 { "" } else { "" }}/>
                <Meter label="escape salvaged" value=fmt::fmt_usd(s6.stats.escape_salvaged_usd)
                    class_extra={if s6.stats.escape_salvaged_usd >= 0.0 { "pos" } else { "neg" }}/>
                <Meter label="escape speed" value={if s6.stats.escape_exits > 0 { format!("{:.0}s to flat", s6.stats.escape_avg_s) } else { "—".into() }} class_extra=""/>
            </div>
        </div>
        {move || st.flash.get().map(|f| view! { <div class="flash">{f}</div> }.into_any())}
    }
}

/// The 0DTE escape control surface: doctrine, live ladder map, portfolio
/// delta readout and salvage stats.
///
/// When an arb is NOT captured (spread stalls or reverses), the position
/// flips to same-day doctrine: hedge the full residual delta on a THIRD
/// venue first, then both legs out at market, hedge released — flat within
/// the deadline. The legs and the hedge deliberately sit on three different
/// venue books, so the structure stays delta-neutral no matter which single
/// venue gaps during the exit.
#[component]
fn EscapePanel(s: ivarb_protocol::ExecSnapshot) -> impl IntoView {
    let escaping: Vec<Position> = s.positions.iter().filter(|p| p.escape.is_some()).cloned().collect();
    let portfolio_delta: f64 = s.positions.iter().map(|p| p.delta_now).sum();
    let now = st_now();
    let stats = s.stats.clone();
    let enabled = s.settings.escape_enabled;
    view! {
        <div class=move || format!("esc-panel {}", if enabled { "on" } else { "off" })>
            <div class="esc-head">
                <div class="esc-title">
                    <span class="esc-icon">"⟁"</span>
                    <div>
                        <h3>"0DTE escape — uncaptured-arb rapid exit"</h3>
                        <p>"When a taken arb will not converge, stop paying theta for it: hedge the full delta on a third venue, then both legs out at market, flat within the deadline. The exit never turns a failed arb into a directional bet."</p>
                    </div>
                </div>
                <div class="esc-stats">
                    <div class="esc-stat">
                        <span class="escs-l">"escapes"</span>
                        <span class="escs-v">{format!("{} · {} live", stats.escape_exits, stats.escape_active)}</span>
                    </div>
                    <div class="esc-stat">
                        <span class="escs-l">"salvaged"</span>
                        <span class=move || format!("escs-v {}", if stats.escape_salvaged_usd >= 0.0 { "pos" } else { "neg" })>
                            {fmt::fmt_usd(stats.escape_salvaged_usd)}
                        </span>
                    </div>
                    <div class="esc-stat">
                        <span class="escs-l">"avg to flat"</span>
                        <span class="escs-v">{if stats.escape_exits > 0 { format!("{:.0}s", stats.escape_avg_s) } else { "—".into() }}</span>
                    </div>
                    <div class="esc-stat">
                        <span class="escs-l">"portfolio Δ"</span>
                        <span class=move || format!("escs-v {}", if portfolio_delta.abs() < 0.05 { "neutral" } else { "warn" })
                            title="sum of live structure deltas (options + hedges) across all open positions">
                            {format!("{:+.3}", portfolio_delta)}
                        </span>
                    </div>
                </div>
            </div>
            {move || if !enabled {
                view! {
                    <div class="esc-note off">"0DTE escape is OFF — failed captures will ride their normal patience/hold exit ladder instead."</div>
                }.into_any()
            } else if escaping.is_empty() {
                view! {
                    <div class="esc-note">{
                        format!(
                            "Armed. Detection: spread stalls below {:.0}% of the signal by {:.0}% of the hold window · spread reverses past +{:.0}% · theta runs 3× hot — any of these flips the position into the ladder below.",
                            100.0 * s.settings.escape_min_progress_frac,
                            100.0 * s.settings.escape_patience_frac,
                            100.0 * s.settings.escape_reversal_mult
                        )
                    }</div>
                }.into_any()
            } else {
                escaping.iter().map(|p| {
                    let e = p.escape.clone().unwrap();
                    let secs_left = ((e.deadline_ms - now).max(0) / 1000) as u64;
                    let phase_label = match e.phase {
                        0 => "detected",
                        1 => "hedged — legs going out",
                        2 => "legs out — releasing hedge",
                        _ => "flat",
                    };
                    view! {
                        <div class="esc-row escaping">
                            <div class="esc-inst">
                                <span class="und-chip">{p.underlying.clone()}</span>
                                <span class="mono">{p.expiry.clone()}{" "}{fmt::fmt_strike(p.strike)}{" "}{fmt::fmt_right(&p.right)}</span>
                                <span class="esc-reason">{e.reason.label()}</span>
                            </div>
                            <div class="esc-map">
                                <span class="esc-node buy" title="long option leg">{fmt::venue_name(&p.buy_venue)}</span>
                                <span class="esc-link">"⟷"</span>
                                <span class="esc-node sell" title="short option leg">{fmt::venue_name(&p.sell_venue)}</span>
                                <span class="esc-link">"+ Δ hedge on"</span>
                                <span class="esc-node hedge" title="perp delta hedge — third venue">{fmt::venue_name(&e.hedge_venue)}</span>
                            </div>
                            <div class="esc-ladder">
                                <span class=move || format!("esc-step {}", if e.phase >= 0 { "done" } else { "" })>"detect"</span>
                                <span class="esc-sep">"→"</span>
                                <span class=move || format!("esc-step {}", if e.phase >= 1 { "done" } else { "" })>"hedge Δ"</span>
                                <span class="esc-sep">"→"</span>
                                <span class=move || format!("esc-step {}", if e.phase >= 2 { "done" } else { "" })>"legs out"</span>
                                <span class="esc-sep">"→"</span>
                                <span class="esc-step">"flat"</span>
                            </div>
                            <div class="esc-meta">
                                <span class="esc-phase">{phase_label}</span>
                                <span class=move || format!("esc-count {}", if secs_left <= 10 { "hot" } else { "" })>
                                    {format!("flat in ≤{}s", secs_left)}
                                </span>
                                <span class="esc-delta">{format!("Δ {:+.3} → 0", p.delta_now)}</span>
                            </div>
                        </div>
                    }.into_any()
                }).collect::<Vec<_>>().into_any()
            }}
        </div>
    }
}

/// Current epoch ms (the side panel clock signal is not shared; the escape
/// countdown refreshes on every exec snapshot push ~1 Hz anyway).
fn st_now() -> i64 {
    js_sys::Date::now() as i64
}

#[component]
fn Meter(label: &'static str, value: String, class_extra: &'static str) -> impl IntoView {
    view! {
        <div class=format!("tb-meter {}", class_extra)>
            <span class="tbm-label">{label}</span>
            <span class="tbm-value">{value}</span>
        </div>
    }
}

#[component]
fn PositionsPanel(s: ivarb_protocol::ExecSnapshot) -> impl IntoView {
    let st = use_context::<ApiState>().unwrap();
    let empty = s.positions.is_empty();
    view! {
        <div class="tk-panel">
            <div class="tk-head">
                <h3>"Open positions"</h3>
                <span class="tk-count">{format!("{} live · MTM every second", s.positions.len())}</span>
            </div>
            {if empty {
                view! {
                    <div class="tk-empty">
                        "No open positions. When the taker fills both legs of a qualifying signal, "
                        "the position appears here with its exit plan (take-profit, stop, theta budget, max hold)."
                    </div>
                }.into_any()
            } else {
                view! {
                    <div class="tablewrap tight">
                        <table class="tk-table">
                            <thead><tr>
                                <th>"#"</th><th>"Instrument"</th><th>"Tier"</th><th>"Units"</th>
                                <th>"Entry net"</th><th>"Spread now"</th><th>"MTM"</th>
                                <th>"Theta burn"</th><th>"Hold / max"</th><th>"Exit plan"</th><th></th>
                            </tr></thead>
                            <tbody>
                                <For each=move || s.positions.clone() key=|p| p.id let:p>
                                    {let st2 = st.clone();
                                     view! { <PositionRow p=p st=st2/> }}
                                </For>
                            </tbody>
                        </table>
                    </div>
                }.into_any()
            }}
        </div>
    }
}

#[component]
fn PositionRow(p: Position, st: ApiState) -> impl IntoView {
    let pid = p.id;
    let p2 = p.clone();
    let p3 = p.clone();
    let p4 = p.clone();
    let p5 = p.clone();
    let p6 = p.clone();
    let p7 = p.clone();
    let esc2 = p.escape.clone();
    let delta = p.delta_now;
    let now = st.now;
    let pos = p.mtm.unrealized_usd >= 0.0;
    view! {
        <tr class=move || {
            let mut c = if p.closing { String::from("tk-row closing") } else { String::from("tk-row") };
            if p.escape.is_some() { c.push_str(" escaping"); }
            c
        }>
            <td class="mono">#{p.id}</td>
            <td>
                <div class="tk-inst">
                    <span class="und-chip">{p.underlying.clone()}</span>
                    <span class="mono">{p.expiry.clone()}{" "}{fmt::fmt_strike(p.strike)}{" "}{fmt::fmt_right(&p.right)}</span>
                </div>
                <div class="tk-sub">{format!("{} → {}", fmt::venue_name(&p.buy_venue), fmt::venue_name(&p.sell_venue))}</div>
            </td>
            <td><span class=format!("tier-chip {}", tier_class(&p.entry.tier))>{p.entry.tier.label()}</span></td>
            <td class="mono">{format!("{:.2}", p.units)}</td>
            <td class="mono">{fmt::fmt_vp(p.entry.net_vp)}</td>
            <td class="mono">{fmt::fmt_vp(p.mtm.spread_vp)}</td>
            <td class=move || format!("mono {}", if pos { "net-pos" } else { "net-neg" })>
                {fmt::fmt_usd(p3.mtm.unrealized_usd)}
            </td>
            <td class="mono dim">{fmt::fmt_usd(p4.mtm.theta_burn_usd)}</td>
            <td class="mono dim">{format!("{} / {}", fmt_hold(p5.mtm.elapsed_s), fmt_hold(p6.max_hold_s))}</td>
            <td class="tk-exit">
                {move || match esc2.clone() {
                    Some(e) => view! {
                        <span class="exo escape" title={format!("0DTE escape — {} — hedge venue {} · phase {}", e.reason.label(), fmt::venue_name(&e.hedge_venue), e.phase)}>
                            {format!("⟁ 0DTE · {} · {}s left", e.reason.label(), ((e.deadline_ms - now.get()).max(0) / 1000))}
                        </span>
                    }.into_any(),
                    None => ().into_any(),
                }}
                <span class="exo take">{format!("≤{:.2}vp", p7.exit_take_vp)}</span>
                <span class="exo stop">{format!("{:.2}vp", p7.exit_stop_vp)}</span>
                <span class="exo theta">{format!("θ{:.2}vp", p7.theta_budget_vp)}</span>
                {if p.hedge_units.abs() > 1e-9 {
                    view! {
                        <span class="exo hedge"
                            title={format!("delta hedge — structure delta {:.4} underlying units", delta)}>
                            {if p.hedge_venue.is_empty() {
                                format!("Δhedge {:.2}", p.hedge_units)
                            } else {
                                format!("Δhedge {:.2} → {}", p.hedge_units, fmt::venue_name(&p.hedge_venue))
                            }}
                        </span>
                    }.into_any()
                } else { ().into_any() }}
            </td>
            <td>
                <button class="tk-close"
                    on:click=move |_| st.send_exec_cmd(ExecCommand::Close { position: pid }, format!("closing position #{pid}"))>
                    "CLOSE"
                </button>
            </td>
        </tr>
    }
}

#[component]
fn TradesPanel(s: ivarb_protocol::ExecSnapshot) -> impl IntoView {
    view! {
        <div class="tk-panel">
            <div class="tk-head">
                <h3>"Closed trades"</h3>
                <span class="tk-count">{format!("{} this session", s.stats.trades_closed)}</span>
            </div>
            {if s.trades.is_empty() {
                view! { <div class="tk-empty">"No closed trades yet — realized capture, slippage versus signal and exit reasons land here."</div> }.into_any()
            } else {
                view! {
                    <div class="tablewrap tight">
                        <table class="tk-table">
                            <thead><tr>
                                <th>"#"</th><th>"Instrument"</th><th>"Tier"</th><th>"Units"</th>
                                <th>"Realized"</th><th>"vs signal"</th><th>"Capture"</th>
                                <th>"Hold"</th><th>"Exit"</th>
                            </tr></thead>
                            <tbody>
                                <For each=move || s.trades.clone() key=|t| (t.id, t.closed_ms) let:t>
                                    {view! { <TradeRow t=t/> }}
                                </For>
                            </tbody>
                        </table>
                    </div>
                }.into_any()
            }}
        </div>
    }
}

#[component]
fn TradeRow(t: Trade) -> impl IntoView {
    let pos = t.realized_usd >= 0.0;
    view! {
        <tr class="tk-row">
            <td class="mono">#{t.id}</td>
            <td>
                <div class="tk-inst">
                    <span class="und-chip">{t.underlying.clone()}</span>
                    <span class="mono">{t.expiry.clone()}{" "}{fmt::fmt_strike(t.strike)}{" "}{fmt::fmt_right(&t.right)}</span>
                </div>
                <div class="tk-sub">{format!("{} → {}", fmt::venue_name(&t.buy_venue), fmt::venue_name(&t.sell_venue))}</div>
            </td>
            <td><span class=format!("tier-chip {}", tier_class(&t.tier))>{t.tier.label()}</span></td>
            <td class="mono">{format!("{:.2}", t.units)}</td>
            <td class=move || format!("mono {}", if pos { "net-pos" } else { "net-neg" })>{fmt::fmt_usd(t.realized_usd)}</td>
            <td class=move || format!("mono {}", if t.slippage_vp >= 0.0 { "net-pos" } else { "net-neg" })>{fmt::fmt_vp(t.realized_vp)}</td>
            <td class="mono dim">{
                if t.legs.is_empty() {
                    format!("{:+.2} vp slip", t.slippage_vp)
                } else {
                    format!("{} legs · {} hops", t.legs.len(), t.legs.iter().filter(|l| l.action == "convert").count())
                }
            }</td>
            <td class="mono dim">{fmt_hold(t.hold_s)}</td>
            <td><span class=format!("exit-chip {}", exit_class(&t.exit_reason))>{exit_label(&t.exit_reason)}</span></td>
        </tr>
    }
}

#[component]
fn ExecLogPanel(s: ivarb_protocol::ExecSnapshot) -> impl IntoView {
    let events: Vec<_> = s.events.iter().rev().take(40).cloned().collect();
    view! {
        <div class="tk-panel log-panel">
            <div class="tk-head">
                <h3>"Execution log"</h3>
                <span class="tk-count">"live state machine"</span>
            </div>
            <div class="exec-log">
                <For each=move || events.clone() key=|e| (e.t_ms, e.attempt, e.state.clone()) let:e>
                    {view! {
                        <div class=format!("elog-line {}", level_class(&e.level))>
                            <span class="elog-t">{fmt::fmt_short_time(e.t_ms)}</span>
                            <span class="elog-state">{e.state.clone()}</span>
                            <span class="elog-msg">
                                {e.opp.as_ref().map(|id| view! { <span class="elog-opp">{inst_of(id)}</span> }.into_any())}
                                {e.msg.clone()}
                            </span>
                        </div>
                    }}
                </For>
                {if s.events.is_empty() {
                    view! { <div class="tk-empty">"Attempt lifecycle events stream here: preflight, leg fills, the 10-second rule, unwinds, hedges and exits."</div> }.into_any()
                } else { ().into_any() }}
            </div>
        </div>
    }
}

#[component]
fn RouteCalibPanel(s: ivarb_protocol::ExecSnapshot) -> impl IntoView {
    let rows = s.routes.clone();
    view! {
        <div class="tk-panel">
            <div class="tk-head">
                <h3>"Route calibration"</h3>
                <span class="tk-count">{format!("{} routes learned", rows.len())}</span>
            </div>
            {if rows.is_empty() {
                view! {
                    <div class="matrix-note">
                        "Every venue pair carries its own reality — quote flicker, leg latency, depth
                        shape. Each closed trade books its capture ratio (realized vs signaled edge)
                        into a per-route EWMA; the taker then fires only when the CALIBRATED edge
                        (displayed EV × learned multiplier) clears the bar. Warms up after the first
                        closed trades per route."
                    </div>
                }.into_any()
            } else {
                view! {
                    <div class="tablewrap tight">
                        <table class="tk-table matrix">
                            <thead><tr>
                                <th>"Route"</th><th>"Closed"</th><th>"Capture"</th>
                                <th>"Abort rate"</th><th>"Edge ×"</th>
                            </tr></thead>
                            <tbody>
                                <For each=move || rows.clone() key=|r| r.route.clone() let: r>
                                    <tr class:cold=r.flicker>
                                        <td class="mono">{r.route.clone()}</td>
                                        <td class="mono">{r.closed}</td>
                                        <td class="mono">
                                            <span class=cap_class(r.capture_ewma)>
                                                {format!("{:+.0}%", r.capture_ewma * 100.0)}
                                            </span>
                                        </td>
                                        <td class="mono">{format!("{:.0}%", r.abort_ewma * 100.0)}</td>
                                        <td class="mono">
                                            <span class=mult_class(r.edge_mult)>
                                                {format!("×{:.2}", r.edge_mult)}
                                            </span>
                                            {if r.flicker {
                                                view! { <span class="chip warn-chip rc-flick">"FLICKER"</span> }.into_any()
                                            } else {
                                                ().into_any()
                                            }}
                                        </td>
                                    </tr>
                                </For>
                            </tbody>
                        </table>
                    </div>
                    <div class="matrix-note">
                        "Edge × is what the route's displayed EV is worth in the account. Routes that
                        consistently bank their edge keep ×1.0+; abort-dominated pairs (FLICKER) are
                        discounted or skipped until their books behave. Manual takes are warned, never
                        blocked — the human keeps the last word."
                    </div>
                }.into_any()
            }}
        </div>
    }
}

fn cap_class(c: f64) -> &'static str {
    if c >= 0.6 { "rc-pos" } else if c >= 0.0 { "rc-mid" } else { "rc-neg" }
}

fn mult_class(m: f64) -> &'static str {
    if m >= 0.8 { "rc-pos" } else if m >= 0.4 { "rc-mid" } else { "rc-neg" }
}

#[component]
fn StrategyMatrix(s: ivarb_protocol::ExecSnapshot) -> impl IntoView {
    view! {
        <div class="tk-panel">
            <div class="tk-head">
                <h3>"Time-aware strategy matrix"</h3>
                <span class="tk-count">"parameters shift with the clock"</span>
            </div>
            <div class="tablewrap tight">
                <table class="tk-table matrix">
                    <thead><tr>
                        <th>"Theta bucket"</th><th>"Gate ×"</th><th>"Patience"</th>
                        <th>"Max hold"</th><th>"θ budget"</th><th>"Size ×"</th>
                    </tr></thead>
                    <tbody>
                        <tr><td>"0–2 days"</td><td class="mono">"×1.6"</td><td>"8 min"</td><td>"45 min"</td><td>"0.03 d"</td><td>"×0.5"</td></tr>
                        <tr><td>"3–7 days"</td><td class="mono">"×1.2"</td><td>"25 min"</td><td>"6 h"</td><td>"0.2 d"</td><td>"×0.8"</td></tr>
                        <tr><td>"8–30 days"</td><td class="mono">"×1.0"</td><td>"90 min"</td><td>"24 h"</td><td>"1 d"</td><td>"×1.0"</td></tr>
                        <tr><td>"30+ days"</td><td class="mono">"×1.0"</td><td>"4 h"</td><td>"72 h"</td><td>"2.5 d"</td><td>"×1.0"</td></tr>
                    </tbody>
                </table>
            </div>
            <div class="matrix-note">
                "Entry gate = base 0.50 vp × bucket × regime × signal age. The same 0.8 vp signal that is a
                SNIPER at 30 days is MONITOR-only at 1 day — vega collapses and fees in vol points explode
                as expiry approaches, so the engine demands more edge the less time remains. Passive
                CONVERGENCE attempts scale their patience window the same way, and every position carries a
                theta budget that closes it before decay eats the locked edge."
            </div>
            <div class="matrix-settings">
                <span>{format!("taker gate ≥ {:.2} vp · leg timeout {:.0}s · participation {:.0}% · Kelly {:.0}%",
                    s.settings.min_edge_vp, s.settings.leg_timeout_s, s.settings.max_participation * 100.0, s.settings.kelly_fraction * 100.0)}</span>
                <span>{format!("tiers: SNIPER {} · CONVERGENCE {} · hedging {}",
                    on_off(s.settings.sniper_enabled), on_off(s.settings.convergence_enabled), on_off(s.settings.hedge_enabled))}</span>
            </div>
        </div>
    }
}

fn on_off(b: bool) -> &'static str {
    if b { "on" } else { "off" }
}

fn tier_class(t: &Tier) -> &'static str {
    match t {
        Tier::Sniper => "sniper",
        Tier::Convergence => "conv",
        Tier::Route => "route",
        Tier::Monitor => "watch",
    }
}

fn level_class(l: &ExecLevel) -> &'static str {
    match l {
        ExecLevel::Info => "info",
        ExecLevel::Ok => "ok",
        ExecLevel::Warn => "warn",
        ExecLevel::Error => "err",
    }
}

fn exit_class(r: &ExitReason) -> &'static str {
    match r {
        ExitReason::TakeProfit | ExitReason::Trail | ExitReason::RouteBanked => "take",
        ExitReason::Stop | ExitReason::Killed | ExitReason::RouteUnwound => "stop",
        ExitReason::ThetaBudget => "theta",
        ExitReason::Escaped0dte => "escape",
        _ => "neutral",
    }
}

fn exit_label(r: &ExitReason) -> &'static str {
    match r {
        ExitReason::TakeProfit => "TAKE PROFIT",
        ExitReason::Trail => "TRAIL LOCK",
        ExitReason::Stop => "STOP",
        ExitReason::ThetaBudget => "THETA BUDGET",
        ExitReason::MaxHold => "MAX HOLD",
        ExitReason::StaleQuotes => "STALE FEED",
        ExitReason::Manual => "MANUAL",
        ExitReason::Killed => "KILLED",
        ExitReason::Expiry => "EXPIRY",
        ExitReason::Escaped0dte => "0DTE ESCAPE",
        ExitReason::RouteBanked => "ROUTE BANKED",
        ExitReason::RouteUnwound => "ROUTE UNWOUND",
    }
}

fn fmt_hold(s: f64) -> String {
    if s < 90.0 {
        format!("{s:.0}s")
    } else if s < 5400.0 {
        format!("{:.0}m", s / 60.0)
    } else {
        format!("{:.1}h", s / 3600.0)
    }
}

fn inst_of(opp_id: &str) -> String {
    opp_id.split(':').next().unwrap_or(opp_id).to_string()
}
