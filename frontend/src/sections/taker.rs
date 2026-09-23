//! Section 02: the Advantage Taker — auto-execution control room.
//!
//! Arm/disarm, kill switch, live risk meters, the time-aware strategy
//! matrix, open positions with mark-to-market, closed trades with capture
//! accounting, and the streaming execution log.

use leptos::prelude::*;

use crate::app::SectionHead;
use crate::fmt;
use crate::state::ApiState;
use ivarb_protocol::{ExecCommand, ExecLevel, ExecMode, ExitReason, Position, Tier, Trade};

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
                    view! {
                        <ControlBar s=s.clone()/>
                        <div class="taker-grid">
                            <div class="tk-col">
                                <PositionsPanel s=s2.clone()/>
                                <TradesPanel s=s3.clone()/>
                            </div>
                            <div class="tk-col">
                                <ExecLogPanel s=s4.clone()/>
                                <StrategyMatrix s=s5.clone()/>
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
    let s1 = s.clone();
    let s2 = s.clone();
    let s3 = s.clone();
    let s4 = s.clone();
    let s5 = s.clone();
    let s6 = s.clone();

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
                <Meter label="single-leg events" value=s6.stats.single_leg_events.to_string()
                    class_extra={if s6.stats.single_leg_events > 0 { "neg" } else { "" }}/>
                <Meter label="capture ratio" value={if s6.stats.trades_closed > 0 { format!("{:.0}% of signal", s6.stats.capture_ratio * 100.0) } else { "—".into() }} class_extra=""/>
            </div>
        </div>
        {move || st.flash.get().map(|f| view! { <div class="flash">{f}</div> }.into_any())}
    }
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
    let pos = p.mtm.unrealized_usd >= 0.0;
    view! {
        <tr class=move || if p.closing { "tk-row closing" } else { "tk-row" }>
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
                <span class="exo take">{format!("≤{:.2}vp", p7.exit_take_vp)}</span>
                <span class="exo stop">{format!("{:.2}vp", p7.exit_stop_vp)}</span>
                <span class="exo theta">{format!("θ{:.2}vp", p7.theta_budget_vp)}</span>
                {if p.hedge_units.abs() > 1e-9 {
                    view! { <span class="exo hedge">{format!("Δhedge {:.2}", p.hedge_units)}</span> }.into_any()
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
            <td class="mono dim">{format!("{:+.2} vp slip", t.slippage_vp)}</td>
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
                        <th>"Max hold"</th><th>"E[hold]"</th><th>"Size ×"</th>
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
        ExitReason::TakeProfit => "take",
        ExitReason::Stop | ExitReason::Killed => "stop",
        ExitReason::ThetaBudget => "theta",
        _ => "neutral",
    }
}

fn exit_label(r: &ExitReason) -> &'static str {
    match r {
        ExitReason::TakeProfit => "TAKE PROFIT",
        ExitReason::Stop => "STOP",
        ExitReason::ThetaBudget => "THETA BUDGET",
        ExitReason::MaxHold => "MAX HOLD",
        ExitReason::StaleQuotes => "STALE FEED",
        ExitReason::Manual => "MANUAL",
        ExitReason::Killed => "KILLED",
        ExitReason::Expiry => "EXPIRY",
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
