//! Section 00: the live opportunities table with sorting, filters, row
//! selection and the detail panel.

use leptos::prelude::*;

use crate::app::SectionHead;
use crate::fmt;
use crate::state::{ApiState, SortMode, UndFilter};

const MAX_ROWS: usize = 120;

#[component]
pub fn ScannerSection() -> impl IntoView {
    let st = use_context::<ApiState>().unwrap();

    let filtered = Memo::new(move |_| {
        let und = st.und_filter.get();
        st.opps
            .get()
            .into_iter()
            .filter(|o| match und {
                UndFilter::All => true,
                UndFilter::Btc => o.underlying == "BTC",
                UndFilter::Eth => o.underlying == "ETH",
            })
            .collect::<Vec<_>>()
    });

    let sorted = Memo::new(move |_| {
        let mut v = filtered.get();
        match st.sort.get() {
            SortMode::Newest => v.sort_by(|a, b| b.first_seen_ms.cmp(&a.first_seen_ms)),
            SortMode::NetVp => v.sort_by(|a, b| {
                b.net_vp.partial_cmp(&a.net_vp).unwrap_or(std::cmp::Ordering::Equal)
            }),
            SortMode::EstUsd => v.sort_by(|a, b| {
                b.est_usd.partial_cmp(&a.est_usd).unwrap_or(std::cmp::Ordering::Equal)
            }),
        }
        v.truncate(MAX_ROWS);
        v
    });

    let count = Memo::new(move |_| filtered.get().len());

    view! {
        <section class="section" id="scanner">
            <SectionHead num="00" title="Live arb opportunities"
                sub="Current fee-adjusted IV arbitrage opportunities from live exchange feeds. Click a row for the plain-English trade description and spread history. Sorted newest-first; act on fresh signals only."/>

            <div class="toolbar">
                <div class="seg" role="tablist">
                    <SegButton active=move || st.und_filter.get() == UndFilter::All
                        on_click=move || st.und_filter.set(UndFilter::All) label="All"/>
                    <SegButton active=move || st.und_filter.get() == UndFilter::Btc
                        on_click=move || st.und_filter.set(UndFilter::Btc) label="BTC"/>
                    <SegButton active=move || st.und_filter.get() == UndFilter::Eth
                        on_click=move || st.und_filter.set(UndFilter::Eth) label="ETH"/>
                </div>
                <div class="seg" role="tablist">
                    <SegButton active=move || st.sort.get() == SortMode::Newest
                        on_click=move || st.sort.set(SortMode::Newest) label="Newest"/>
                    <SegButton active=move || st.sort.get() == SortMode::NetVp
                        on_click=move || st.sort.set(SortMode::NetVp) label="Top net vp"/>
                    <SegButton active=move || st.sort.get() == SortMode::EstUsd
                        on_click=move || st.sort.set(SortMode::EstUsd) label="Top est $"/>
                </div>
                <div class="toolbar-note">
                    {move || {
                        let shown = sorted.get().len();
                        let total = count.get();
                        if total > MAX_ROWS {
                            format!("showing {shown} of {total} — tighten filters for the sharpest signals")
                        } else {
                            format!("{total} live opportunities")
                        }
                    }}
                </div>
            </div>

            <div class="tablewrap">
                <table class="arb-table">
                    <thead>
                        <tr>
                            <th class="th-age">"Age"</th>
                            <th class="th-expiry">"Expiry"</th>
                            <th class="th-strike">"Strike"</th>
                            <th class="th-dir">"Direction (buy → sell)"</th>
                            <th class="th-spread">"Spread − Fee"</th>
                            <th class="th-net">"Net"</th>
                            <th class="th-vega">"Vega"</th>
                            <th class="th-est">"Est $/ct"</th>
                            <th class="th-mode">"Mode"</th>
                        </tr>
                    </thead>
                    <tbody>
                        <For each=move || sorted.get() key=|o| o.id.clone() let:o>
                            <ScannerRow o=o.clone()/>
                        </For>
                    </tbody>
                </table>
                {move || if sorted.get().is_empty() {
                    view! {
                        <div class="table-empty">
                            <p>"No live opportunities above the entry threshold right now."</p>
                            <p class="dim">"The scanner signals when the fee-adjusted spread reaches +0.50 vp. Divergements cluster around macro prints and liquidation cascades — keep this page open."</p>
                        </div>
                    }.into_any()
                } else {
                    ().into_any()
                }}
            </div>

            <crate::sections::detail::DetailPanel/>
        </section>
    }
}

#[component]
fn SegButton(
    active: impl Fn() -> bool + Send + Sync + 'static,
    on_click: impl Fn() + Send + Sync + 'static,
    label: &'static str,
) -> impl IntoView {
    view! {
        <button class=move || if active() { "seg-btn active" } else { "seg-btn" }
                on:click=move |_| on_click()>
            {label}
        </button>
    }
}

#[component]
fn ScannerRow(o: ivarb_protocol::Opportunity) -> impl IntoView {
    let st = use_context::<ApiState>().unwrap();
    let now = st.now;
    let o2 = o.clone();
    let o3 = o.clone();
    let o4 = o.clone();
    let id = o.id.clone();
    let selected = Memo::new(move |_| st.selected.get().as_deref() == Some(id.as_str()));
    let stale = o.stale;
    let mode_exec = matches!(o.edge_mode, ivarb_protocol::EdgeMode::Exec);
    let buy_name = fmt::venue_name(&o.buy_venue);
    let sell_name = fmt::venue_name(&o.sell_venue);
    let click_id = o.id.clone();

    view! {
        <tr class=move || {
            let mut c = String::from("arb-row");
            if selected.get() { c.push_str(" sel"); }
            if stale { c.push_str(" stale"); }
            c
        }
            on:click=move |_| st.selected.set(Some(click_id.clone()))>
            <td class="td-age">
                {move || fmt::fmt_age(now.get() - o2.updated_ms)}
            </td>
            <td class="td-expiry">{o.expiry.clone()}</td>
            <td class="td-strike">
                <span class="strike">{fmt::fmt_strike(o.strike)}</span>
                <span class=(move || format!("right-chip {}", if o.right == ivarb_protocol::Right::Call { "call" } else { "put" }))>
                    {fmt::fmt_right(&o.right)}
                </span>
                <span class="und-chip">{o.underlying.clone()}</span>
            </td>
            <td class="td-dir">
                <span class="dir-buy">{buy_name.clone()}</span>
                <span class="dir-arrow">"→"</span>
                <span class="dir-sell">{sell_name.clone()}</span>
            </td>
            <td class="td-spread">
                {format!("{:.2} vp − {:.2}", o3.iv_spread_vp, o3.fee_vp)}
            </td>
            <td class="td-net">
                <span class=(move || if o4.net_vp >= 0.0 { "net-pos" } else { "net-neg" })>
                    {move || format!("{:+.2} vp", o4.net_vp)}
                </span>
            </td>
            <td class="td-vega">{fmt::fmt_usd(o.vega_vp_usd)}</td>
            <td class="td-est">{fmt::fmt_usd(o.est_usd)}</td>
            <td class="td-mode">
                <span class=(move || if mode_exec { "mode-chip exec" } else { "mode-chip mark" })>
                    {if mode_exec { "EXEC" } else { "MARK" }}
                </span>
            </td>
        </tr>
    }
}
