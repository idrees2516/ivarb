//! Spread-history chart: a reactive hand-rolled SVG line chart with a zero
//! baseline and the entry-threshold guide.

use leptos::prelude::*;

use crate::fmt;
use ivarb_protocol::HistoryPt;

const W: f64 = 640.0;
const H: f64 = 170.0;
const PAD_L: f64 = 46.0;
const PAD_R: f64 = 14.0;
const PAD_T: f64 = 14.0;
const PAD_B: f64 = 22.0;

#[component]
pub fn SpreadChart(history: Vec<HistoryPt>, threshold: f64) -> impl IntoView {
    let view = if history.len() < 2 {
        None
    } else {
        Some(())
    };
    let (min, max, path, area, zero_y, thr_y, last, first_t, last_t) = build(&history, threshold);

    view! {
        <div class="chart-wrap">
        {match view {
            None => view! {
                <div class="chart-empty">
                    <p>"Collecting spread history…"</p>
                    <p class="dim">"Points appear every few seconds while the opportunity stays above the exit threshold."</p>
                </div>
            }.into_any(),
            Some(_) => view! {
                <svg class="spread-chart" viewBox=(format!("0 0 {W} {H}")) preserveAspectRatio="none"
                     role="img" aria-label="Net edge history">
                    // gridlines
                    <line x1=PAD_L.to_string() y1=zero_y.to_string() x2=(W - PAD_R).to_string() y2=zero_y.to_string()
                          class="chart-zero"/>
                    {if (threshold - min) > 0.05 && threshold < max {
                        view! { <line x1=PAD_L.to_string() y1=thr_y.to_string() x2=(W - PAD_R).to_string() y2=thr_y.to_string()
                          class="chart-thr"/> }.into_any()
                    } else { ().into_any() }}
                    // area fill under the line
                    <path d=area.clone() class="chart-area"/>
                    // the line itself
                    <path d=path.clone() class="chart-line"/>
                    // last point marker
                    <circle cx=(W - PAD_R).to_string() cy=(zero_y + (max - last.1) * 0.0 + y_of(last.1, min, max)).to_string() r="3.5" class="chart-dot"/>
                    // y labels
                    <text x="4" y=y_of(max, min, max).to_string() class="chart-label">{fmt::fmt_vp_plain(max)}</text>
                    <text x="4" y=y_of(min, min, max).to_string() class="chart-label">{fmt::fmt_vp_plain(min)}</text>
                    <text x="4" y=zero_y.to_string() class="chart-label zero">"0.0"</text>
                    // x labels
                    <text x=PAD_L.to_string() y=(H - 6.0).to_string() class="chart-label">{fmt::fmt_short_time(first_t)}</text>
                    <text x=(W - PAD_R - 44.0).to_string() y=(H - 6.0).to_string() class="chart-label">{fmt::fmt_short_time(last_t)}</text>
                    <text x=(W - PAD_R - 20.0).to_string() y=(y_of(last.1, min, max) - 6.0).to_string() class="chart-last">{fmt::fmt_vp(last.1)}</text>
                </svg>
            }.into_any(),
        }}
        </div>
    }
}

fn y_of(v: f64, min: f64, max: f64) -> f64 {
    let span = (max - min).max(0.2);
    PAD_T + (max - v) / span * (H - PAD_T - PAD_B)
}

fn build(history: &[HistoryPt], threshold: f64) -> (f64, f64, String, String, f64, f64, HistoryPt, i64, i64) {
    let mut min = f64::MAX;
    let mut max = f64::MIN;
    for (_, v) in history {
        min = min.min(*v);
        max = max.max(*v);
    }
    let pad = ((max - min) * 0.18).max(0.15);
    min -= pad;
    max += pad;
    if min > 0.0 {
        min = 0.0;
    }
    let n = history.len();
    let first_t = history[0].0;
    let last_t = history[n - 1].0;
    let t_span = (last_t - first_t).max(1) as f64;
    let x = |i: usize| PAD_L + (i as f64 / (n as f64 - 1.0).max(1.0)) * (W - PAD_L - PAD_R);
    let mut path = String::with_capacity(n * 12);
    let mut area = String::with_capacity(n * 12);
    let zero_y = y_of(0.0, min, max);
    for (i, (t, v)) in history.iter().enumerate() {
        let _ = t;
        let px = x(i);
        let py = y_of(*v, min, max);
        if i == 0 {
            path.push_str(&format!("M{px:.1},{py:.1}"));
            area.push_str(&format!("M{px:.1},{zero_y:.1} L{px:.1},{py:.1}"));
        } else {
            path.push_str(&format!(" L{px:.1},{py:.1}"));
            area.push_str(&format!(" L{px:.1},{py:.1}"));
        }
    }
    area.push_str(&format!(
        " L{:.1},{zero_y:.1} Z",
        W - PAD_R
    ));
    let thr_y = y_of(threshold, min, max);
    let last = history[n - 1];
    (min, max, path, area, zero_y, thr_y, last, first_t, last_t)
}
