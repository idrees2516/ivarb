//! Side panel — the one-glance control surface.
//!
//! The engine underneath is sophisticated (multi-leg route sequencer,
//! calibration, risk gates); the panel is deliberately simple: session P&L,
//! taker state, route-cycle counters, the latest execution events and the
//! two buttons that matter (ARM / DISARM). Collapsible, overlays the page
//! (never reflows content), and stays usable at side-panel/narrow widths.

use leptos::prelude::*;

use crate::fmt;
use crate::state::ApiState;
use ivarb_protocol::{ExecCommand, ExecLevel};

#[component]
pub fn SidePanel() -> impl IntoView {
    let st = use_context::<ApiState>().unwrap();
    let open = RwSignal::new(false);
    let exec = Memo::new(move |_| st.exec.get());
    let flash_line = Memo::new(move |_| st.flash.get());

    let pnl = Memo::new(move |_| {
        exec.get()
            .map(|e| e.stats.realized_usd + e.stats.unrealized_usd)
            .unwrap_or(0.0)
    });
    let armed = Memo::new(move |_| exec.get().map(|e| e.stats.armed).unwrap_or(false));
    let killed = Memo::new(move |_| exec.get().map(|e| e.stats.kill.is_some()).unwrap_or(false));
    let events: Memo<Vec<(String, String, String, &'static str)>> = Memo::new(move |_| {
        exec.get()
            .map(|e| {
                e.events
                    .iter()
                    .rev()
                    .take(10)
                    .map(|ev| {
                        (
                            fmt::fmt_short_time(ev.t_ms),
                            ev.state.clone(),
                            ev.msg.clone(),
                            match ev.level {
                                ExecLevel::Info => "sp-info",
                                ExecLevel::Ok => "sp-ok",
                                ExecLevel::Warn => "sp-warn",
                                ExecLevel::Error => "sp-err",
                            },
                        )
                    })
                    .collect()
            })
            .unwrap_or_default()
    });

    let st_arm = st.clone();
    let st_disarm = st.clone();
    let st_top = st.clone();

    view! {
        <button class="sp-toggle" class:open=move || open.get()
            on:click=move |_| open.update(|o| *o = !*o)
            title="execution side panel">
            <span class="sp-toggle-dot" class:armed=move || armed.get() class:killed=move || killed.get()></span>
            <span class="sp-toggle-pnl" class:neg=move || pnl.get() < 0.0>
                {move || fmt::fmt_usd(pnl.get())}
            </span>
            <span class="sp-toggle-chev">"‹"</span>
        </button>
        <div class="sp-viewport">
            <div class="sp-panel" class:open=move || open.get()>
            <div class="sp-head">
                <span class="sp-title">"TAKER"</span>
                <span class=move || format!("sp-badge {}", if killed.get() { "killed" } else if armed.get() { "armed" } else { "off" })>
                    {move || if killed.get() { "KILLED" } else if armed.get() { "ARMED" } else { "OFF" }}
                </span>
                <button class="sp-close" on:click=move |_| open.set(false)>"✕"</button>
            </div>

            <div class=move || format!("sp-pnl {}", if pnl.get() >= 0.0 { "pos" } else { "neg" })>
                {move || fmt::fmt_usd(pnl.get())}
                <span class="sp-pnl-sub">"session P&L"</span>
            </div>

            <div class="sp-grid">
                {move || match exec.get() {
                    None => view! { <div class="sp-empty">"connecting…"</div> }.into_any(),
                    Some(e) => view! {
                        <SpStat label="realized" value=fmt::fmt_usd(e.stats.realized_usd)
                            cls={if e.stats.realized_usd >= 0.0 { "pos" } else { "neg" }}/>
                        <SpStat label="open" value=format!("{} pos", e.stats.positions_open) cls=""/>
                        <SpStat label="2-leg attempts" value=format!("{} · {} live", e.stats.attempts_total, e.stats.attempts_active) cls=""/>
                        <SpStat label="route cycles" value=format!("{} · {} live", e.stats.route_attempts, e.stats.route_attempts_active)
                            cls={if e.stats.route_banked > 0 { "pos" } else { "" }}/>
                        <SpStat label="routes banked" value=e.stats.route_banked.to_string() cls=""/>
                        <SpStat label="route P&L" value=fmt::fmt_usd(e.stats.route_profit_usd)
                            cls={if e.stats.route_profit_usd >= 0.0 { "pos" } else { "neg" }}/>
                        <SpStat label="vega" value=format!("{} / {}", fmt::fmt_usd(e.stats.vega_used_usd), fmt::fmt_usd(e.settings.max_total_vega_usd)) cls=""/>
                        <SpStat label="capture" value={if e.stats.trades_closed > 0 { format!("{:.0}%", e.stats.capture_ratio * 100.0) } else { "—".into() }} cls=""/>
                    }.into_any(),
                }}
            </div>

            {move || {
                let is_armed = armed.get();
                if is_armed {
                    let st_d = st_disarm.clone();
                    view! {
                        <button class="sp-btn disarm"
                            on:click=move |_| st_d.send_exec_cmd(ExecCommand::Arm { armed: false }, "auto-taker disarmed".into())>
                            "■ DISARM"
                        </button>
                    }.into_any()
                } else {
                    let st_a = st_arm.clone();
                    view! {
                        <button class="sp-btn arm"
                            on:click=move |_| st_a.send_exec_cmd(ExecCommand::Arm { armed: true }, "auto-taker ARMED".into())>
                            "▶ ARM"
                        </button>
                    }.into_any()
                }
            }}
            <button class="sp-btn top"
                on:click=move |_| {
                    st_top.send_exec_cmd(ExecCommand::FlattenAll, "flatten-all requested".into());
                    let _ = web_sys::window().map(|w| w.scroll_to_with_x_and_y(0.0, 0.0));
                }>
                "⇧ FLATTEN ALL · TOP"
            </button>

            <SpLiveCard st=st.clone() />

            <div class="sp-log">
                <For each=move || events.get() key=|(t, s, m, _)| format!("{t}{s}{m}") let:ev>
                    {let (t, s, m, cls) = ev;
                     view! {
                        <div class=format!("sp-line {}", cls)>
                            <span class="sp-t">{t}</span>
                            <span class="sp-s">{s}</span>
                            <span class="sp-m">{m}</span>
                        </div>
                    }}
                </For>
            </div>

            {move || flash_line.get().map(|f| view! { <div class="sp-flash">{f}</div> }.into_any())}
            </div>
        </div>
    }
}

#[component]
fn SpStat(label: &'static str, value: String, cls: &'static str) -> impl IntoView {
    view! {
        <div class=format!("sp-stat {}", cls)>
            <span class="sp-l">{label}</span>
            <span class="sp-v">{value}</span>
        </div>
    }
}

/// Live connectivity + instant on-chain swap card.
///
/// One compact surface for "is this real?": per-venue gateway chips
/// (LIVE when credentials + gateway are up), per-chain chips with the LIVE
/// on-chain ETH price from Uniswap quotes, a preflight button, and the
/// CHAIN SWAP form — quotes the real QuoterV2 and, when a signer key is
/// configured, broadcasts a real EIP-1559 transaction.
#[component]
fn SpLiveCard(st: ApiState) -> impl IntoView {
    let exec = Memo::new(move |_| st.exec.get());
    let live = Memo::new(move |_| exec.get().map(|e| e.live.clone()).unwrap_or_default());

    let chain_pick = RwSignal::new("base".to_string());
    let weth_in = RwSignal::new(true);
    let amount = RwSignal::new("0.01".to_string());

    let st_pf = st.clone();
    let st_sw = st.clone();
    let st_rf = st.clone();

    view! {
        <div class="sp-live">
            <div class="sp-live-head">
                <span class="sp-live-title">"LIVE CONNECTIVITY"</span>
                <button class="sp-mini-btn" title="check venue credentials + gateways + on-chain RPC (never orders)"
                    on:click=move |_| st_pf.send_exec_cmd(ExecCommand::LivePreflight, "preflight running — see event log".into())>
                    "PREFLIGHT"
                </button>
            </div>

            <div class="sp-chips">
                {move || {
                    let l = live.get();
                    let venues: Vec<_> = l
                        .venues
                        .iter()
                        .map(|v| {
                            let cls = if v.healthy {
                                "chip live"
                            } else if v.creds {
                                "chip warn"
                            } else {
                                "chip off"
                            };
                            let title = v.detail.clone();
                            (v.venue.clone(), cls.to_string(), title)
                        })
                        .collect();
                    if venues.is_empty() {
                        vec![("deribit · bybit · okx · binance".into(), "chip off".into(), "run PREFLIGHT to detect venue credentials".into())]
                    } else {
                        venues
                    }
                    .into_iter()
                    .map(|(name, cls, title)| {
                        view! {
                            <span class=cls class:chip-dim=title.is_empty() title=title.clone()>
                                {name.clone()}
                            </span>
                        }
                        .into_any()
                    })
                    .collect::<Vec<_>>()
                }}
            </div>

            <div class="sp-chips">
                {move || {
                    let l = live.get();
                    if l.chains.is_empty() {
                        vec![view! {
                            <span class="chip off" title="on-chain status not yet probed">"chains: probing…"</span>
                        }.into_any()]
                    } else {
                        l.chains
                            .iter()
                            .map(|c| {
                                let cls = if c.ok { "chip live" } else { "chip warn" };
                                let title = format!(
                                    "block {} · {:.2} gwei · rpc {} ms · 0.01 WETH → {:.2} USDC (live Uniswap V3 quote)",
                                    c.block, c.gas_gwei, c.rpc_ms, c.weth_usdc_out
                                );
                                view! {
                                    <span class=cls title=title>
                                        {format!("{} ${:.0}", c.name, c.eth_price)}
                                    </span>
                                }
                                .into_any()
                            })
                            .collect::<Vec<_>>()
                    }
                }}
            </div>

            <div class="sp-swap">
                <div class="sp-swap-row">
                    <select class="sp-select" prop:value=move || chain_pick.get()
                        on:change=move |ev| {
                            let v = event_target_value(&ev);
                            chain_pick.set(v);
                        }>
                        <option value="base">"base"</option>
                        <option value="arbitrum">"arbitrum"</option>
                        <option value="optimism">"optimism"</option>
                        <option value="ethereum">"ethereum"</option>
                    </select>
                    <div class="sp-dir">
                        <button class=move || format!("sp-dir-btn {}", if weth_in.get() { "on" } else { "" })
                            on:click=move |_| weth_in.set(true)>"ETH→USDC"</button>
                        <button class=move || format!("sp-dir-btn {}", if !weth_in.get() { "on" } else { "" })
                            on:click=move |_| weth_in.set(false)>"USDC→ETH"</button>
                    </div>
                </div>
                <div class="sp-swap-row">
                    <input class="sp-input" type="text" inputmode="decimal"
                        prop:value=move || amount.get()
                        on:input=move |ev| amount.set(event_target_value(&ev))
                        placeholder="amount" />
                    <button class="sp-swap-btn" title="quote the real on-chain Uniswap V3 pool and execute (broadcasts when a signer key is configured)"
                        on:click=move |_| {
                            let amt: f64 = amount.get().trim().parse().unwrap_or(0.0);
                            if amt <= 0.0 {
                                st.send_exec_cmd(ExecCommand::ChainStatusRefresh, "enter a positive amount first".into());
                                return;
                            }
                            st_sw.send_exec_cmd(
                                ExecCommand::ChainSwap {
                                    chain: chain_pick.get(),
                                    weth_in: weth_in.get(),
                                    amount: amt,
                                },
                                format!("swap sent to {} — quoting live…", chain_pick.get()),
                            );
                        }>
                        "EXECUTE"
                    </button>
                </div>
                {move || {
                    let l = live.get();
                    l.swaps.iter().rev().take(3)
                        .map(|s| {
                            let line = match (&s.tx_hash, s.broadcast) {
                                (Some(h), true) => format!("{} {} {:.6} → {:.4} · tx {}…", s.chain, s.direction, s.amount_in, s.quoted_out, &h[..h.len().min(10)]),
                                _ => format!("{} {} {:.6} → quoted {:.4} · payload ready (no signer)", s.chain, s.direction, s.amount_in, s.quoted_out),
                            };
                            view! { <div class="sp-swap-line" title=s.calldata_hex.clone()>{line}</div> }.into_any()
                        })
                        .collect::<Vec<_>>()
                }}
                {move || {
                    let l = live.get();
                    l.chain_signer.as_ref().map(|a| {
                        view! { <div class="sp-signer" title="IVARB_CHAIN_SIGNER_KEY is set — swaps broadcast REAL transactions from this address">{
                            format!("signer {}… (real broadcast)", &a[..a.len().min(10)])
                        }</div> }.into_any()
                    })
                }}
                <button class="sp-mini-btn wide" on:click=move |_| st_rf.send_exec_cmd(ExecCommand::ChainStatusRefresh, "chain status refreshing…".into())>
                    "REFRESH CHAINS"
                </button>
            </div>
        </div>
    }
}
