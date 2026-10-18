//! The agent chat panel: ask natural-language research questions, watch the
//! tool-calling trace appear step by step, get a grounded answer plus
//! structure cards.

use leptos::prelude::*;

use ivarb_protocol::research::{AgentRun, AgentToolCall, SuggestionCard};

use super::builder::SuggestionCardView;
use super::Rs;
use crate::api::{post_json_resp, spawn};
use crate::state::set_timeout;

#[derive(Clone)]
struct ChatMsg {
    who: Who,
    text: String,
    steps: Vec<AgentToolCall>,
    cards: Vec<SuggestionCard>,
    /// How many tool steps are revealed (progressive animation).
    shown: RwSignal<usize>,
    /// Whether the final answer is visible yet.
    answer_shown: RwSignal<bool>,
}

#[derive(Clone, Copy, PartialEq)]
enum Who {
    User,
    Agent,
}

const EXAMPLES: &[&str] = &[
    "Build me a bullish BTC structure for two weeks",
    "How rich is vol on ETH right now?",
    "Where is the liquidity concentrated on BTC?",
    "Compare the venues quoting SOL options",
    "What should I trade if BTC stays in range this week?",
];

#[component]
pub fn AgentChat(rs: Rs) -> impl IntoView {
    let messages: RwSignal<Vec<ChatMsg>> = RwSignal::new(Vec::new());
    let input: RwSignal<String> = RwSignal::new(String::new());
    let busy: RwSignal<bool> = RwSignal::new(false);
    let scroll_ref: NodeRef<leptos::html::Div> = NodeRef::new();

    let scroll_to_bottom = move || {
        set_timeout(40, move || {
            if let Some(el) = scroll_ref.get() {
                el.set_scroll_top(el.scroll_height());
            }
        });
    };

    let run_query = std::sync::Arc::new(move |q: String, messages: RwSignal<Vec<ChatMsg>>, busy: RwSignal<bool>, rs: Rs| {
        if q.trim().is_empty() || busy.get_untracked() {
            return;
        }
        input.set(String::new());
        messages.update(|m| {
            m.push(ChatMsg {
                who: Who::User,
                text: q.clone(),
                steps: vec![],
                cards: vec![],
                shown: RwSignal::new(0),
                answer_shown: RwSignal::new(true),
            });
        });
        scroll_to_bottom();
        busy.set(true);
        rs.agent_busy.set(true);
        let messages2 = messages;
        let rs2 = rs;
        spawn(async move {
            #[derive(serde::Serialize)]
            struct Q<'a> {
                query: &'a str,
            }
            let run = post_json_resp::<AgentRun, _>("/api/research/agent", &Q { query: &q }).await;
            busy.set(false);
            rs2.agent_busy.set(false);
            let Some(run) = run else {
                messages2.update(|m| {
                    m.push(ChatMsg {
                        who: Who::Agent,
                        text: "The research agent is unreachable (server restarting or the research plane still warming up — try again in a few seconds).".into(),
                        steps: vec![],
                        cards: vec![],
                        shown: RwSignal::new(0),
                        answer_shown: RwSignal::new(true),
                    });
                });
                scroll_to_bottom();
                return;
            };
            let msg = ChatMsg {
                who: Who::Agent,
                text: run.answer.clone(),
                steps: run.steps.clone(),
                cards: run.cards.clone(),
                shown: RwSignal::new(0),
                answer_shown: RwSignal::new(false),
            };
            // Surface the first card in the shared builder slot.
            if let Some(c) = run.cards.first() {
                rs2.card.set(Some(c.clone()));
            }
            let n = run.steps.len();
            messages2.update(|m| m.push(msg.clone()));
            scroll_to_bottom();
            // Progressive reveal: steps 420 ms apart, then the answer.
            for i in 0..n {
                let shown = msg.shown;
                set_timeout(420 * (i as i32) + 200, move || {
                    shown.set(i + 1);
                    scroll_to_bottom();
                });
            }
            let answer = msg.answer_shown;
            set_timeout(420 * (n as i32) + 450, move || {
                answer.set(true);
                scroll_to_bottom();
            });
        });
    });

    // Builder → chat handoff.
    {
        let run_q = run_query.clone();
        let ask = rs.ask_agent;
        let msgs_sig = messages;
        let busy_sig = busy;
        let rs_sig = rs;
        Effect::new(move || {
            if let Some(q) = ask.get() {
                ask.set(None);
                if !busy_sig.get_untracked() {
                    run_q(q, msgs_sig, busy_sig, rs_sig);
                }
            }
        });
    }

    let brain = move || {
        rs.pulse
            .get()
            .and_then(|p| Some(p.agent_brain))
            .unwrap_or_else(|| "desk".into())
    };
    let mode = move || rs.pulse.get().map(|p| p.data_mode.clone()).unwrap_or_else(|| "live".into());
    let run_query_scroll = run_query.clone();
    let run_query_key = run_query.clone();
    let run_query_send = run_query.clone();

    view! {
        <div class="agent-chat">
            <div class="ac-head">
                <span class="ac-title">"Ask the research desk"</span>
                <span class="ac-brain">
                    {move || match brain().as_str() {
                        "desk" => "deterministic desk brain".to_string(),
                        other => format!("LLM brain · {}", other.trim_start_matches("llm:")),
                    }}
                </span>
            </div>
            <div class="ac-scroll" node_ref=scroll_ref>
                {let rq_outer = run_query_scroll.clone();
                 move || {
                    let msgs = messages.get();
                    if msgs.is_empty() {
                        view! {
                            <div class="ac-empty">
                                <p>"Ask anything about the consolidated options market — or request a structure."</p>
                                <p class="dim">"The agent plans tool calls, runs them against live cross-venue data, and answers with only the numbers the tools returned."</p>
                                <div class="ac-examples">
                                    {EXAMPLES.iter().map(|e| {
                                        let e = *e;
                                        let rq = run_query_scroll.clone();
                                        let msgs_sig = messages;
                                        let busy_sig = busy;
                                        let rs_sig = rs;
                                        view! {
                                            <button class="ac-example" on:click=move |_| rq(e.to_string(), msgs_sig, busy_sig, rs_sig)>
                                                {e}
                                            </button>
                                        }.into_any()
                                    }).collect::<Vec<_>>()}
                                </div>
                            </div>
                        }.into_any()
                    } else {
                        msgs.into_iter().map(|m| {
                            let m_cls = m.clone();
                            let m_steps = m.clone();
                            let m_ans = m.clone();
                            let m_cards = m.clone();
                            let shown_sig = m.shown;
                            view! {
                                <div class=move || format!("ac-msg {}", if m_cls.who == Who::User { "user" } else { "agent" })>
                                    {if m.who == Who::User {
                                        view! { <div class="ac-bubble user">{m.text.clone()}</div> }.into_any()
                                    } else {
                                        view! {
                                            <div class="ac-bubble agent">
                                                <div class="ac-trace">
                                                    <For each=move || steps_of(&m_steps) key=key_of let:st>
                                                        {let (i, s) = st;
                                                         move || {
                                                            if i < shown_sig.get() {
                                                                view! {
                                                                    <div class="ac-step">
                                                                        <span class="ac-step-tool">{"◎ "}{s.tool.clone()}</span>
                                                                        <span class="ac-step-sum">{s.summary.clone()}</span>
                                                                        <span class="ac-step-ms">{s.ms}"ms"</span>
                                                                    </div>
                                                                }.into_any()
                                                            } else {
                                                                view! { <div class="ac-step pending"></div> }.into_any()
                                                            }
                                                        }}
                                                    </For>
                                                </div>
                                                {move || {
                                                    if m_ans.answer_shown.get() {
                                                        view! { <div class="ac-answer">{render_md(&m_ans.text)}</div> }.into_any()
                                                    } else {
                                                        view! { <div class="ac-typing"><span></span><span></span><span></span></div> }.into_any()
                                                    }
                                                }}
                                                {move || {
                                                    if m_cards.answer_shown.get() && !m_cards.cards.is_empty() {
                                                        m_cards.cards.iter().map(|c| view! { <SuggestionCardView card=c.clone() compact=true/> }).collect::<Vec<_>>().into_any()
                                                    } else {
                                                        ().into_any()
                                                    }
                                                }}
                                            </div>
                                        }.into_any()
                                    }}
                                </div>
                            }.into_any()
                        }).collect::<Vec<_>>().into_any()
                    }
                }}
                {move || {
                    if busy.get() {
                        view! {
                            <div class="ac-msg agent">
                                <div class="ac-bubble agent">
                                    <div class="ac-typing"><span></span><span></span><span></span></div>
                                    <div class="ac-running">"planning tool calls…"</div>
                                </div>
                            </div>
                        }.into_any()
                    } else {
                        ().into_any()
                    }
                }}
            </div>
            <div class="ac-input-row">
                <input class="ac-input" type="text"
                    placeholder="e.g. create a neutral structure on BTC for 10 days…"
                    prop:value=move || input.get()
                    on:input=move |ev| input.set(event_target_value(&ev))
                    on:keydown=move |ev| {
                        if ev.key() == "Enter" {
                            let q = input.get_untracked();
                            run_query_key(q, messages, busy, rs);
                        }
                    }/>
                <button class="ac-send" disabled=move || busy.get()
                    on:click=move |_| {
                        let q = input.get_untracked();
                        run_query_send(q, messages, busy, rs);
                    }>
                    {move || if busy.get() { "…" } else { "Ask" }}
                </button>
            </div>
            <div class="ac-foot dim">
                {move || format!(
                    "brain: {} · data: {}",
                    brain(),
                    mode(),
                )}
            </div>
        </div>
    }
}

fn steps_of(m: &ChatMsg) -> Vec<(usize, AgentToolCall)> {
    m.steps.iter().cloned().enumerate().collect()
}

fn key_of(st: &(usize, AgentToolCall)) -> String {
    format!("{}-{}-{}", st.0, st.1.tool, st.1.ms)
}

/// Markdown-lite renderer: `##` headings, `- ` bullets, `**bold**` runs.
fn render_md(text: &str) -> impl IntoView {
    let mut blocks: Vec<AnyView> = Vec::new();
    for line in text.lines() {
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        if let Some(h) = t.strip_prefix("## ") {
            blocks.push(
                view! { <div class="md-h">{render_bold(h)}</div> }.into_any(),
            );
        } else if let Some(b) = t.strip_prefix("- ") {
            blocks.push(
                view! {
                    <div class="md-li">
                        <span class="md-dot">"•"</span>
                        <span>{render_bold(b)}</span>
                    </div>
                }
                .into_any(),
            );
        } else {
            blocks.push(view! { <div class="md-p">{render_bold(t)}</div> }.into_any());
        }
    }
    blocks
}

/// Split a line on `**` into alternating plain / bold segments.
fn render_bold(text: &str) -> Vec<AnyView> {
    let mut out = Vec::new();
    let mut bold = false;
    for seg in text.split("**") {
        if seg.is_empty() {
            bold = !bold;
            continue;
        }
        let s = seg.to_string();
        if bold {
            out.push(view! { <strong>{s}</strong> }.into_any());
        } else {
            out.push(view! { <span>{s}</span> }.into_any());
        }
        bold = !bold;
    }
    if out.is_empty() {
        out.push(view! { <span>{text}</span> }.into_any());
    }
    out
}

/// Truncating summary helper used on cards and traces.
pub fn short(s: &str, n: usize) -> String {
    if s.len() <= n {
        s.to_string()
    } else {
        let cut: String = s.chars().take(n).collect();
        format!("{cut}…")
    }
}

/// `2026-10-23 · 10d`.
pub fn expiry_label(date: u32, dte: i32) -> String {
    format!(
        "{:04}-{:02}-{:02} · {dte}d",
        date / 10_000,
        (date / 100) % 100,
        date % 100
    )
}
