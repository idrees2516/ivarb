//! Section 05 — The genetic strategy optimizer.
//!
//! Strategy parameters (entry-gate multiplier, exit fraction, patience,
//! Kelly fraction, fee buffer, theta netting, staleness cap, time-of-day
//! curve, hold-lab EV gate, multi-hop fee buffer) are not hand-tuned
//! forever: a genetic algorithm evolves them against the recorded tape of
//! live signal episodes. Fitness is risk-adjusted P&L per 100 episodes
//! (capture − drawdown penalty − churn), replayed in the optimizer's own
//! OS thread; the best genome can be applied to live gates at any time.

use leptos::prelude::*;

use crate::fmt;
use crate::state::ApiState;
use ivarb_protocol::GaStatus;

#[component]
pub fn GaSection() -> impl IntoView {
    let st = use_context::<ApiState>().unwrap();
    let ga = Memo::new(move |_| st.ga.get());

    view! {
        <section class="section" id="ga">
            <crate::app::SectionHead num="05"
                title="Genetic strategy optimizer — parameters that evolve against live tape"
                sub="Every signal episode is recorded to a tape. A genetic algorithm replays that tape thousands of times per generation, mutating and crossing over the strategy genome, and keeps the parameter set with the best risk-adjusted capture. You decide when it drives the live gates."/>
            {move || {
                match ga.get() {
                    None => view! {
                        <div class="detail-empty"><p>"Waiting for the optimizer status feed…"</p></div>
                    }.into_any(),
                    Some(g) => view! { <GaPanel g=g/> }.into_any(),
                }
            }}
        </section>
    }
}

#[component]
fn GaPanel(g: GaStatus) -> impl IntoView {
    let st = use_context::<ApiState>().unwrap();
    let applied = g.applied;
    let running = g.running;

    let headline: Vec<(&'static str, String, &'static str)> = vec![
        ("generation", g.generation.to_string(), ""),
        (
            "fitness (best)",
            format!("{:+.2}", g.best_fitness),
            if g.best_fitness >= 0.0 { "pos" } else { "neg" },
        ),
        (
            "vs hand-tuned",
            format!("{:+.1}%", g.improvement_pct),
            if g.improvement_pct >= 0.0 { "pos" } else { "neg" },
        ),
        ("tape episodes", fmt::fmt_int(g.tape_episodes as f64), ""),
        ("replay trades", g.replay_trades.to_string(), ""),
        (
            "replay P&L",
            format!("{:+.1} vp", g.replay_pnl_vp),
            if g.replay_pnl_vp >= 0.0 { "pos" } else { "neg" },
        ),
        (
            "replay max drawdown",
            format!("\u{2212}{:.1} vp", g.replay_max_dd_vp),
            "neg",
        ),
        ("generation time", format!("{} ms", fmt::fmt_int(g.gen_ms as f64)), ""),
    ];

    view! {
        <div class="ga-panel">
            <div class="ga-topline">
                <div class="ga-status">
                    <span class=move || format!("chip {}", if running { "ok-chip" } else { "warn-chip" })>
                        {if running { "EVOLVING" } else { "PAUSED" }}
                    </span>
                    <span class=move || format!("chip {}", if applied { "ok-chip" } else { "" })>
                        {if applied { "APPLIED — live gates use the evolved genome" } else { "SANDBOXED — defaults drive live gates" }}
                    </span>
                </div>
                <div class="ga-controls">
                    {if running {
                        let st_pause = st.clone();
                        view! {
                            <button class="btn"
                                on:click=move |_| st_pause.send_ga_cmd(
                                    ivarb_protocol::GaCommand::Pause,
                                    "optimizer paused".into(),
                                )>
                                "Pause"
                            </button>
                        }.into_any()
                    } else {
                        let st_resume = st.clone();
                        view! {
                            <button class="btn"
                                on:click=move |_| st_resume.send_ga_cmd(
                                    ivarb_protocol::GaCommand::Resume,
                                    "optimizer resumed".into(),
                                )>
                                "Resume"
                            </button>
                        }.into_any()
                    }}
                    {let st_apply = st.clone();
                     view! {
                        <button class=move || format!("btn {}", if applied { "amber" } else { "go" })
                            on:click=move |_| st_apply.send_ga_cmd(
                                ivarb_protocol::GaCommand::SetApplied { applied: !applied },
                                if applied {
                                    "genome detached — live gates back to defaults".into()
                                } else {
                                    "best genome applied to live gates".into()
                                },
                            )>
                            {if applied { "Un-apply genome" } else { "Apply best genome" }}
                        </button>
                     }}
                    {let st_reset = st.clone();
                     view! {
                        <button class="btn"
                            on:click=move |_| st_reset.send_ga_cmd(
                                ivarb_protocol::GaCommand::Reset,
                                "population re-seeded from defaults".into(),
                            )>
                            "Reset"
                        </button>
                     }}
                </div>
            </div>

            <div class="ga-headline">
                <For each=move || headline.clone() key=|(k, _, _)| *k let:h>
                    {let (k, v, cls) = h;
                     view! {
                        <div class="fact">
                            <div class="fact-k">{k}</div>
                            <div class=move || format!("fact-v {}", cls)>{v}</div>
                        </div>
                     }}
                </For>
            </div>

            <div class="ga-cols">
                <div class="ga-genes">
                    <div class="dc-title">"Evolved genome (best individual)"</div>
                    <For each=move || g.genes.clone() key=|x| x.key.clone() let:gene>
                        {let span = (gene.hi - gene.lo).max(1e-9);
                         let frac = ((gene.value - gene.lo) / span).clamp(0.0, 1.0) * 100.0;
                         let default_frac = ((gene.default - gene.lo) / span).clamp(0.0, 1.0) * 100.0;
                         view! {
                            <div class="gene-row">
                                <div class="gene-label">
                                    <span class="gene-name">{gene.label.clone()}</span>
                                    <span class="gene-val">{format!("{:.3}", gene.value)}</span>
                                </div>
                                <div class="gene-track">
                                    <div class="gene-fill" style=move || format!("width:{}%", frac)></div>
                                    <div class="gene-default" style=move || format!("left:{}%", default_frac)></div>
                                </div>
                                <div class="gene-range">
                                    <span>{format!("{:.2}", gene.lo)}</span>
                                    <span class="dim">{format!("default {:.2}", gene.default)}</span>
                                    <span>{format!("{:.2}", gene.hi)}</span>
                                </div>
                            </div>
                        }}
                    </For>
                    <p class="dim small">
                        "The white tick marks the hand-tuned default. The filled bar is the current best value; "
                        "the search bounds encode what the strategy layer considers sane."
                    </p>
                </div>
                <div class="ga-history">
                    <div class="dc-title">"Best fitness per generation"</div>
                    <FitnessHistory hist=g.history.clone() best=g.best_fitness default_fit=g.default_fitness/>
                    <p class="dim small">
                        "Fitness = (captured vp \u{2212} 0.5 \u{00d7} max drawdown \u{2212} churn) per 100 tape episodes, "
                        "replayed over the last 8 000 recorded episodes with realistic fees, fill probability, "
                        "single-leg risk and theta burn."
                    </p>
                </div>
            </div>
        </div>
    }
}

/// Inline SVG sparkline of the fitness history with a baseline at the
/// hand-tuned default's fitness.
#[component]
fn FitnessHistory(hist: Vec<f64>, best: f64, default_fit: f64) -> impl IntoView {
    let (path, area, lo, hi) = sparkline(&hist);
    let n = hist.len();
    view! {
        <div class="ga-chart">
            {move || {
                if n < 2 {
                    view! {
                        <div class="detail-empty"><p>{format!("Collecting generations — {n} recorded so far. The curve appears after the second generation.")}</p></div>
                    }.into_any()
                } else {
                    view! {
                        <svg viewBox="0 0 640 200" class="spark-svg" preserveAspectRatio="none">
                            <path d=area.clone() class="spark-area"></path>
                            <path d=path.clone() class="spark-line"></path>
                        </svg>
                        <div class="ga-chart-meta">
                            <span class="dim">{format!("worst {:.2} · best {:.2} · now {:+.2}", lo, hi, best)}</span>
                            <span class="dim">{format!("default-genome fitness {:+.2} on the same tape", default_fit)}</span>
                        </div>
                    }.into_any()
                }
            }}
        </div>
    }
}

fn sparkline(hist: &[f64]) -> (String, String, f64, f64) {
    let n = hist.len();
    if n < 2 {
        return (String::new(), String::new(), 0.0, 0.0);
    }
    let mut lo = f64::INFINITY;
    let mut hi = f64::NEG_INFINITY;
    for &v in hist {
        lo = lo.min(v);
        hi = hi.max(v);
    }
    if (hi - lo).abs() < 1e-9 {
        hi = lo + 1.0;
    }
    let w = 640.0;
    let h = 200.0;
    let pad = 8.0;
    let x = |i: usize| pad + (i as f64 / (n - 1) as f64) * (w - 2.0 * pad);
    let y = |v: f64| h - pad - ((v - lo) / (hi - lo)) * (h - 2.0 * pad);
    let mut path = String::with_capacity(n * 12);
    for (i, &v) in hist.iter().enumerate() {
        if i == 0 {
            path.push_str(&format!("M {:.1} {:.1}", x(i), y(v)));
        } else {
            path.push_str(&format!(" L {:.1} {:.1}", x(i), y(v)));
        }
    }
    let mut area = path.clone();
    area.push_str(&format!(
        " L {:.1} {:.1} L {:.1} {:.1} Z",
        x(n - 1),
        h - pad,
        x(0),
        h - pad
    ));
    (path, area, lo, hi)
}
