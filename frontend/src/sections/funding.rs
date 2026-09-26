//! Section 03: Funding, costs & capital — the money side of the platform.
//!
//! Three things live here:
//! 1. **Treasury** — deposit/withdraw paper money per venue (the same balances
//!    the advantage taker's risk gates check before every attempt), plus the
//!    real-money funding rails for each exchange (assets, networks, fees,
//!    settlement times) so the move from paper to live is a decision, not a
//!    surprise.
//! 2. **Comprehensive cost of arb** — every opportunity priced with the full
//!    round-trip cost sheet (entry+exit fees, slippage, margin carry, netted
//!    theta, funding round-trip) using the exact same `ivarb-quant` code the
//!    engine runs.
//! 3. **Capital requirements & profit** — per-venue segregated collateral a
//!    trade demands, return on capital, funded/not-funded against live
//!    balances, and the closed-trade profit attribution report.

use leptos::prelude::*;

use crate::app::SectionHead;
use crate::fmt;
use crate::state::ApiState;
use ivarb_protocol::{Opportunity, Right};
use ivarb_quant::{capital_req, cost_sheet, funding_round_trip, roc_pct, CapitalReq, CostInput, CostSheet, FUNDING_RAILS};

const VENUES: &[&str] = &[
    "deribit", "okx", "bybit", "binance", "delta", "aevo", "derive", "thalex",
];

fn fee_of(venue: &str) -> f64 {
    match venue {
        "binance" | "aevo" | "thalex" => 0.0002,
        _ => 0.0003,
    }
}

#[component]
pub fn FundingSection() -> impl IntoView {
    let st = use_context::<ApiState>().unwrap();
    let exec = Memo::new(move |_| st.exec.get());

    view! {
        <section class="section" id="funding">
            <SectionHead num="03" title="Funding, costs & capital"
                sub="Deposit money per venue, see the comprehensive cost of every arb (fees, slippage, margin carry, theta, funding rails), the capital each trade demands, and the profit attribution of closed trades."/>
            {move || match exec.get() {
                None => view! {
                    <div class="taker-boot"><span class="dot warn"></span>"connecting to the executor…"</div>
                }.into_any(),
                Some(s) => {
                    let s2 = s.clone();
                    let s3 = s.clone();
                    view! {
                        <TreasuryBar s=s.clone()/>
                        <div class="fund-grid">
                            <div class="tk-col">
                                <DepositForm/>
                                <VenueFundCards s=s2.clone()/>
                            </div>
                            <div class="tk-col">
                                <CapitalTable s=s3.clone()/>
                                <ProfitReport s=s.clone()/>
                            </div>
                        </div>
                    }.into_any()
                }
            }}
        </section>
    }
}

/// Big-picture account stats.
#[component]
fn TreasuryBar(s: ivarb_protocol::ExecSnapshot) -> impl IntoView {
    let pnl = s.stats.realized_usd + s.stats.unrealized_usd;
    let cells: Vec<(&'static str, String, &'static str)> = vec![
        (
            "account equity",
            fmt::fmt_usd(s.stats.equity_usd),
            if s.stats.equity_usd > 0.0 { "pos" } else { "" },
        ),
        (
            "net deposits",
            fmt::fmt_usd(s.stats.net_deposits_usd),
            "",
        ),
        (
            "session P&L",
            format!("{} ({:+.2}%)", fmt::fmt_usd(pnl), s.stats.roc_session_pct),
            if pnl >= 0.0 { "pos" } else { "neg" },
        ),
        (
            "capital in use",
            fmt::fmt_usd(s.stats.capital_reserved_usd),
            "",
        ),
        (
            "free collateral",
            fmt::fmt_usd(s.stats.capital_free_usd),
            "",
        ),
        (
            "paper / live",
            match s.stats.mode {
                ivarb_protocol::ExecMode::Paper => "PAPER — simulated fills".to_string(),
                ivarb_protocol::ExecMode::Live => "LIVE — real orders".to_string(),
            },
            "",
        ),
    ];
    view! {
        <div class="fund-treasury">
            <For each=move || cells.clone() key=|(k, _, _)| *k let:c>
                {let (k, v, cls) = c;
                 view! {
                    <div class=format!("ft-cell {}", cls)>
                        <div class="ft-k">{k}</div>
                        <div class="ft-v">{v}</div>
                    </div>
                }}
            </For>
        </div>
        <div class="fund-note">
            "Paper balances start at $250,000 per venue. Deposits and withdrawals move real
             numbers through the same risk gates the taker checks before every attempt —
             a trade is rejected with a capital reason until you fund the venue that needs it.
             P&L is measured against net deposits, so funding the account never inflates performance."
        </div>
    }
}

/// Deposit / withdraw form with quick amounts.
#[component]
fn DepositForm() -> impl IntoView {
    let st = use_context::<ApiState>().unwrap();
    let venue = RwSignal::new("okx".to_string());
    let amount = RwSignal::new(50_000.0);
    let names = store_value(VENUES.iter().map(|v| v.to_string()).collect::<Vec<_>>());

    let do_deposit = {
        let st = st.clone();
        move |amt: f64| {
            let v = venue.get();
            st.send_exec_cmd(
                ivarb_protocol::ExecCommand::Deposit { venue: v.clone(), amount_usd: amt },
                format!("DEPOSIT {:.0} USD → {} — the taker can now size up on {}", amt, v, v),
            );
        }
    };
    let do_withdraw = {
        let st = st.clone();
        move |amt: f64| {
            let v = venue.get();
            st.send_exec_cmd(
                ivarb_protocol::ExecCommand::Withdraw { venue: v.clone(), amount_usd: amt },
                format!("WITHDRAW {:.0} USD ← {}", amt, v),
            );
        }
    };
    let dep_amt = do_deposit.clone();
    let wd_amt = do_withdraw.clone();

    view! {
        <div class="tk-panel fund-form">
            <div class="tk-head">
                <h3>"Deposit / withdraw"</h3>
                <span class="tk-sub">"paper money, per venue — the taker's capital gates read these balances"</span>
            </div>
            <div class="ff-row">
                <label class="calc-field">
                    <span class="cf-label">"Venue"</span>
                    <select class="cf-input"
                        on:change=move |ev| venue.set(event_target_value(&ev))>
                        {move || {
                            names.with_value(|opts| {
                                opts.iter()
                                    .map(|o| {
                                        let sel = venue.get() == *o;
                                        view! { <option value=o.clone() selected=sel>{fmt::venue_name(o)}</option> }
                                    })
                                    .collect::<Vec<_>>()
                            })
                        }}
                    </select>
                </label>
                <label class="calc-field grow">
                    <span class="cf-label">"Amount (USD)"</span>
                    <input class="cf-input" type="number" step="1000" min="0"
                        prop:value=move || fmt_num(amount.get())
                        on:input=move |ev| {
                            if let Ok(f) = event_target_value(&ev).parse::<f64>() {
                                amount.set(f);
                            }
                        }/>
                </label>
                <div class="ff-actions">
                    <button class="tb-btn arm"
                        on:click=move |_| dep_amt(amount.get())>
                        "＋ DEPOSIT"
                    </button>
                    <button class="tb-btn kill"
                        on:click=move |_| wd_amt(amount.get())>
                        "－ WITHDRAW"
                    </button>
                </div>
            </div>
            <div class="ff-quick">
                <span class="fq-label">"quick:"</span>
                <For each=move || [10_000.0f64, 50_000.0, 100_000.0, 500_000.0].to_vec()
                    key=|a: &f64| a.to_bits() let:a>
                    {let amt = a;
                     let dep = do_deposit.clone();
                     view! {
                        <button class="fq-btn"
                            on:click=move |_| {
                                amount.set(amt);
                                dep(amt);
                            }>
                            {format!("+{}", fmt::fmt_int(amt))}
                        </button>
                    }}
                </For>
            </div>
        </div>
    }
}

/// Per-venue balance cards with the real-money funding rails.
#[component]
fn VenueFundCards(s: ivarb_protocol::ExecSnapshot) -> impl IntoView {
    let balances: Vec<(String, f64)> = s.balances.clone();
    let deposits: Vec<(String, f64)> = s.deposits_by_venue.clone();
    let cards: Vec<(String, f64, f64, ivarb_quant::FundingRail)> = VENUES
        .iter()
        .filter_map(|v| {
            let bal = balances.iter().find(|(b, _)| b == v).map(|(_, x)| *x);
            let dep = deposits.iter().find(|(b, _)| b == v).map(|(_, x)| *x).unwrap_or(0.0);
            let rail = FUNDING_RAILS.iter().find(|r| r.venue == *v).copied();
            bal.map(|b| (v.to_string(), b, dep, rail.unwrap()))
        })
        .collect();
    view! {
        <div class="tk-panel fund-cards">
            <div class="tk-head">
                <h3>"Venue accounts & real-money funding rails"</h3>
                <span class="tk-sub">"how cash actually gets to each exchange when you go live"</span>
            </div>
            <div class="fc-grid">
                <For each=move || cards.clone() key=|(v, _, _, _)| v.clone() let:c>
                    {let (v, bal, dep, rail) = c;
                     view! {
                        <div class="fc-card">
                            <div class="fc-head">
                                <span class="fc-venue">{fmt::venue_name(&v)}</span>
                                <span class="fc-bal">{fmt::fmt_usd(bal)}</span>
                            </div>
                            <div class="fc-dep">
                                {if dep.abs() > 0.5 {
                                    format!("lifetime net deposits: {}", fmt::fmt_usd(dep))
                                } else {
                                    "no deposits yet".to_string()
                                }}
                            </div>
                            <div class="fc-rail">
                                <div><span class="fact-k">"margin assets"</span><span class="fact-v">{rail.asset}</span></div>
                                <div><span class="fact-k">"network"</span><span class="fact-v">{rail.network}</span></div>
                                <div><span class="fact-k">"typical fee"</span><span class="fact-v">{format!("~${:.2}", rail.fee_usd)}</span></div>
                                <div><span class="fact-k">"credit time"</span><span class="fact-v">{fmt_rail_minutes(rail.minutes)}</span></div>
                            </div>
                            <div class="fc-note">{rail.note}</div>
                        </div>
                    }}
                </For>
            </div>
        </div>
    }
}

fn fmt_rail_minutes(m: f64) -> String {
    if m < 1.0 {
        "seconds".to_string()
    } else if m < 60.0 {
        format!("~{m:.0} min")
    } else {
        format!("~{:.1} h", m / 60.0)
    }
}

fn fmt_num(v: f64) -> String {
    if (v - v.round()).abs() < 1e-9 {
        format!("{}", v.round())
    } else {
        format!("{v}")
    }
}

/// One row of the capital table.
#[derive(Clone, PartialEq)]
struct CapRow {
    opp: Opportunity,
    sheet: CostSheet,
    cap: CapitalReq,
    roc: f64,
    funded_buy: bool,
    funded_sell: bool,
}

/// Build the cost sheet + capital requirement for one opportunity, per 1
/// underlying unit, from live snapshot data. Mirrors the executor's own
/// inputs so the projection matches what the risk gates will demand.
fn cap_row(o: &Opportunity, balances: &[(String, f64)]) -> Option<CapRow> {
    let buy_px = o.buy_px_usd?;
    let sell_px = o.sell_px_usd?;
    if buy_px <= 0.0 || sell_px <= 0.0 || o.contract_size <= 0.0 {
        return None;
    }
    let sell_ct = 1.0; // 1 option = 1 underlying on sell venues (venue metadata default)
    let buy_depth = o.size_buy.map(|u| u / o.contract_size).unwrap_or(10.0).max(1.0);
    let sell_depth = o.size_sell.unwrap_or(10.0).max(1.0);
    let hold_days = (o.hold_s / 86_400.0).min(o.dte.max(0.1)).max(0.02);
    let input = CostInput {
        buy_premium_per_contract: buy_px,
        sell_premium_per_contract: sell_px,
        buy_contract_size: o.contract_size,
        sell_contract_size: sell_ct,
        buy_fee_rate: fee_of(&o.buy_venue),
        sell_fee_rate: fee_of(&o.sell_venue),
        spot: o.spot,
        strike: o.strike,
        right: match o.right {
            Right::Call => ivarb_quant::QuantRight::Call,
            Right::Put => ivarb_quant::QuantRight::Put,
        },
        vega_per_vp_unit: o.vega_vp_usd / o.contract_size,
        units: 1.0,
        hold_days,
        theta_vp_day: o.theta_vp_day,
        buy_depth_contracts: buy_depth,
        sell_depth_contracts: sell_depth,
        funding_round_trip_usd: funding_round_trip(&o.buy_venue, &o.sell_venue),
        sell_venue: o.sell_venue.clone(),
    };
    let sheet = cost_sheet(&input);
    let cap = capital_req(&input);
    let roc = roc_pct(sheet.net_all_in_usd, &cap);
    let bal = |v: &str| balances.iter().find(|(b, _)| b == v).map(|(_, x)| *x).unwrap_or(0.0);
    Some(CapRow {
        opp: o.clone(),
        sheet,
        cap,
        roc,
        funded_buy: bal(&o.buy_venue) >= cap.buy_venue_usd,
        funded_sell: bal(&o.sell_venue) >= cap.sell_venue_usd,
    })
}

/// Top opportunities × comprehensive cost × capital requirement.
#[component]
fn CapitalTable(s: ivarb_protocol::ExecSnapshot) -> impl IntoView {
    let st = use_context::<ApiState>().unwrap();
    let balances: Vec<(String, f64)> = s.balances.clone();

    let rows = Memo::new(move |_| {
        let mut rows: Vec<CapRow> = st
            .opps
            .get()
            .iter()
            .filter(|o| !o.stale && !o.retired)
            .filter_map(|o| cap_row(o, &balances))
            .collect();
        rows.sort_by(|a, b| {
            b.roc.partial_cmp(&a.roc).unwrap_or(std::cmp::Ordering::Equal)
        });
        rows.truncate(12);
        rows
    });

    view! {
        <div class="tk-panel cap-panel">
            <div class="tk-head">
                <h3>"Capital requirements — top opportunities by return on capital"</h3>
                <span class="tk-sub">"per 1 underlying unit · comprehensive cost includes fees, slippage, margin carry, theta and funding rails"</span>
            </div>
            <div class="tablewrap tight">
                <table class="tk-table cap-table">
                    <thead>
                        <tr>
                            <th>"opportunity"</th>
                            <th class="r">"gross locked"</th>
                            <th class="r">"all-in cost"</th>
                            <th class="r">"net after costs"</th>
                            <th class="r">"capital (buy→ / sell→)"</th>
                            <th class="r">"total"</th>
                            <th class="r">"ROC"</th>
                            <th>"funded?"</th>
                        </tr>
                    </thead>
                    <tbody>
                        <For each=move || rows.get() key=|r| r.opp.id.clone() let:r>
                            {let r = r;
                             let funded = r.funded_buy && r.funded_sell;
                             view! {
                                <tr>
                                    <td>
                                        <div class="ct-opp">
                                            <span class="ct-pair">{format!("{} → {}", fmt::venue_name(&r.opp.buy_venue), fmt::venue_name(&r.opp.sell_venue))}</span>
                                            <span class="ct-inst">{format!("{} {} {} {}", r.opp.underlying, r.opp.expiry, fmt::fmt_strike(r.opp.strike), fmt::fmt_right(&r.opp.right))}</span>
                                        </div>
                                    </td>
                                    <td class="r">{fmt::fmt_usd(r.sheet.gross_locked_usd)}</td>
                                    <td class="r neg">{format!("−{}", fmt::fmt_usd(r.sheet.total_usd))}</td>
                                    <td class=format!("r {}", if r.sheet.net_all_in_usd >= 0.0 { "pos" } else { "neg" })>
                                        {fmt::fmt_usd(r.sheet.net_all_in_usd)}
                                    </td>
                                    <td class="r ct-split">
                                        {format!("{} / {}", fmt::fmt_usd(r.cap.buy_venue_usd), fmt::fmt_usd(r.cap.sell_venue_usd))}
                                    </td>
                                    <td class="r">{fmt::fmt_usd(r.cap.total_usd)}</td>
                                    <td class=format!("r {}", if r.roc >= 0.0 { "pos" } else { "neg" })>
                                        {format!("{:+.1}%", r.roc)}
                                    </td>
                                    <td>
                                        {if funded {
                                            view! { <span class="chip ok-chip">"funded"</span> }.into_any()
                                        } else {
                                            view! { <span class="chip warn-chip">"deposit needed"</span> }.into_any()
                                        }}
                                    </td>
                                </tr>
                            }}
                        </For>
                    </tbody>
                </table>
            </div>
            <div class="cap-legend">
                <span>"capital (buy→): premium + entry fee on the cheap venue · capital (sell→): short-option margin × 1.2 buffer + fee on the rich venue — venues hold segregated collateral"</span>
            </div>
        </div>
    }
}

/// Profit attribution from closed trades.
#[component]
fn ProfitReport(s: ivarb_protocol::ExecSnapshot) -> impl IntoView {
    let st = s.stats.clone();
    let win_rate = if st.trades_closed > 0 {
        100.0 * st.wins as f64 / st.trades_closed as f64
    } else {
        0.0
    };
    let pf = if st.profit_factor.is_infinite() {
        "∞".to_string()
    } else {
        format!("{:.2}", st.profit_factor)
    };
    let rows: Vec<(&'static str, String, &'static str)> = vec![
        ("spread premium locked at entry", fmt::fmt_usd(st.gross_locked_usd), "pos"),
        ("market drift to exit (convergence / stops)", fmt::fmt_usd(st.exit_drift_usd), ""),
        ("taker fees paid", format!("−{}", fmt::fmt_usd(st.fees_usd)), "neg"),
        ("delta-hedge P&L", fmt::fmt_usd(st.hedge_pnl_usd), ""),
        ("realized net profit", fmt::fmt_usd(st.realized_usd), if st.realized_usd >= 0.0 { "pos" } else { "neg" }),
    ];
    let stats: Vec<(&'static str, String)> = vec![
        ("closed trades", format!("{}", st.trades_closed)),
        ("win rate", format!("{win_rate:.0}%")),
        ("profit factor", pf),
        ("avg per trade", fmt::fmt_usd(st.avg_trade_usd)),
        ("unrealized (open)", fmt::fmt_usd(st.unrealized_usd)),
        ("session ROC on peak capital", format!("{:+.2}%", st.roc_session_pct)),
    ];
    view! {
        <div class="tk-panel profit-panel">
            <div class="tk-head">
                <h3>"Profit from these arbs — attribution"</h3>
                <span class="tk-sub">"where the money came from and went, across every closed round trip"</span>
            </div>
            {if st.trades_closed == 0 {
                view! {
                    <div class="profit-empty">
                        "No closed trades yet. Arm the advantage taker (section 02) or TAKE a signal —
                         every round trip lands here with its full cost attribution."
                    </div>
                }.into_any()
            } else {
                view! {
                    <div class="pp-grid">
                        <div class="pp-rows">
                            <For each=move || rows.clone() key=|(k, _, _)| *k let:r>
                                {let (k, v, cls) = r;
                                 view! {
                                    <div class=format!("pp-row {}", cls)>
                                        <span class="pp-k">{k}</span>
                                        <span class="pp-v">{v}</span>
                                    </div>
                                }}
                            </For>
                        </div>
                        <div class="pp-stats">
                            <For each=move || stats.clone() key=|(k, _)| *k let:x>
                                {let (k, v) = x;
                                 view! {
                                    <div class="pp-stat">
                                        <div class="fact-k">{k}</div>
                                        <div class="fact-v">{v}</div>
                                    </div>
                                }}
                            </For>
                        </div>
                    </div>
                }.into_any()
            }}
        </div>
    }
}
