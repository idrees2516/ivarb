//! Research charts: a canvas candlestick chart with EMA / Bollinger /
//! volume / RSI panes, and SVG charts for the IV term structure and skew.

use leptos::prelude::*;
use wasm_bindgen::JsCast;

use ivarb_protocol::research::{CandleSeries, IvAnalytics, SkewPoint};

use super::Rs;
use crate::fmt;

const W: f64 = 900.0;
const H: f64 = 470.0;
const PAD_L: f64 = 62.0;
const PAD_R: f64 = 66.0;
// Price pane.
const P_T: f64 = 12.0;
const P_B: f64 = 296.0;
// Volume pane.
const V_T: f64 = 306.0;
const V_B: f64 = 356.0;
// RSI pane.
const R_T: f64 = 368.0;
const R_B: f64 = 452.0;

#[component]
pub fn ChartsPanel(rs: Rs) -> impl IntoView {
    let intervals = ["15m", "1h", "4h", "1d"];
    view! {
        <div class="rs-charts">
            <div class="rs-chart-controls">
                <div class="rs-exp-chips">
                    {intervals.iter().map(|iv| {
                        let iv = *iv;
                        view! {
                            <button class=move || format!("rs-exp-chip{}", if rs.interval.get() == iv { " on" } else { "" })
                                on:click=move |_| rs.interval.set(iv.to_string())>
                                {iv}
                            </button>
                        }.into_any()
                    }).collect::<Vec<_>>()}
                </div>
                {move || {
                    rs.candles.get().map(|c| {
                        let r = &c.readout;
                        let trend_cls = match r.ema_trend.as_str() {
                            "up" => "pos",
                            "down" => "neg",
                            _ => "dim",
                        };
                        view! {
                            <div class="rs-readout">
                                <span class=format!("{trend_cls}")>{format!("trend {}", r.ema_trend)}</span>
                                <span>{format!("RSI {}", r.rsi14.map(|x| format!("{x:.0}")).unwrap_or_else(|| "—".into()))}</span>
                                <span>{format!("ATR {}%", r.atr14_pct.map(|x| format!("{x:.2}")).unwrap_or_else(|| "—".into()))}</span>
                                <span>{format!("RV20 {}vp", r.realized_vol_vp.map(|x| format!("{x:.1}")).unwrap_or_else(|| "—".into()))}</span>
                                <span>{format!("24h {}", r.change_24h_pct.map(|x| format!("{x:+.1}%")).unwrap_or_else(|| "—".into()))}</span>
                            </div>
                        }.into_any()
                    }).unwrap_or_else(|| ().into_any())
                }}
            </div>
            {move || {
                match rs.candles.get() {
                    None => view! { <div class="rs-loading">"loading candles…"</div> }.into_any(),
                    Some(c) if c.candles.len() < 30 => view! { <div class="rs-loading">"collecting candles…"</div> }.into_any(),
                    Some(c) => view! { <CandleChart series_sig=rs.candles.clone()/> }.into_any(),
                }
            }}
            <p class="dim rs-note">"Candles from Binance spot; EMA20/EMA50, Bollinger(20, 2σ), RSI-14, ATR-14 and realized vol computed server-side by the research plane and drawn on the WASM canvas."</p>
        </div>
    }
}

#[component]
fn CandleChart(series_sig: RwSignal<Option<CandleSeries>>) -> impl IntoView {
    let wrap_ref: NodeRef<leptos::html::Div> = NodeRef::new();
    Effect::new(move || {
        // Reactive on both the wrapper being mounted and the series data.
        let mounted = wrap_ref.get().is_some();
        let Some(s) = series_sig.get() else { return };
        if !mounted || s.candles.len() < 30 {
            return;
        }
        let Some(doc) = web_sys::window().and_then(|w| w.document()) else { return };
        let Some(el) = doc.get_element_by_id("rs-candle-canvas") else { return };
        let Ok(canvas) = el.dyn_into::<web_sys::HtmlCanvasElement>() else { return };
        canvas.set_width(W as u32);
        canvas.set_height(H as u32);
        let Ok(ctx) = canvas.get_context("2d") else { return };
        let Some(ctx) = ctx else { return };
        let ctx: web_sys::CanvasRenderingContext2d =
            ctx.unchecked_into::<web_sys::CanvasRenderingContext2d>();
        draw(&ctx, &s);
    });
    view! {
        <div class="rs-canvas-wrap" node_ref=wrap_ref>
            <canvas id="rs-candle-canvas" class="rs-canvas"></canvas>
        </div>
    }
}

fn draw(ctx: &web_sys::CanvasRenderingContext2d, s: &CandleSeries) {
    let cs = &s.candles;
    let n = cs.len();
    if n < 2 {
        return;
    }
    ctx.clear_rect(0.0, 0.0, W, H);

    // ---- scales ----
    let mut lo = f64::INFINITY;
    let mut hi = f64::NEG_INFINITY;
    for c in cs {
        lo = lo.min(c.l);
        hi = hi.max(c.h);
    }
    // include BB bounds in range
    for b in s.overlays.bb_upper.iter().flatten() {
        hi = hi.max(*b);
    }
    for b in s.overlays.bb_lower.iter().flatten() {
        lo = lo.min(*b);
    }
    let pad = (hi - lo) * 0.06;
    lo -= pad;
    hi += pad;
    let max_vol = cs.iter().map(|c| c.v).fold(0.0_f64, f64::max).max(1e-9);
    let plot_w = W - PAD_L - PAD_R;
    let x = |i: usize| PAD_L + (i as f64 / (n as f64 - 1.0)) * plot_w;
    let y = |v: f64| P_T + (hi - v) / (hi - lo) * (P_B - P_T);

    // ---- grid + price axis ----
    ctx.set_fill_style(&wasm_bindgen::JsValue::from_str("#66748c"));
    ctx.set_font("11px ui-monospace, monospace");
    ctx.set_stroke_style(&wasm_bindgen::JsValue::from_str("#1a2330"));
    ctx.set_line_width(1.0);
    for g in 0..=5 {
        let gy = P_T + (P_B - P_T) * g as f64 / 5.0;
        let gv = hi - (hi - lo) * g as f64 / 5.0;
        ctx.begin_path();
        ctx.move_to(PAD_L, gy);
        ctx.line_to(W - PAD_R, gy);
        ctx.stroke();
        ctx.fill_text(&fmt_price(gv), 6.0, gy + 4.0);
    }

    // ---- Bollinger band (faint fill via two lines) ----
    let line_of = |vals: &[Option<f64>], color: &str, width: f64, dash: Option<f64>| {
        ctx.set_stroke_style(&wasm_bindgen::JsValue::from_str(color));
        ctx.set_line_width(width);
        if let Some(d) = dash {
            ctx.set_line_dash(&wasm_bindgen::JsValue::from(js_sys::Array::of1(
                &wasm_bindgen::JsValue::from_f64(d),
            )));
        } else {
            ctx.set_line_dash(&js_sys::Array::new());
        }
        ctx.begin_path();
        let mut started = false;
        for (i, v) in vals.iter().enumerate() {
            if let Some(v) = v {
                let (px, py) = (x(i), y(*v));
                if started {
                    ctx.line_to(px, py);
                } else {
                    ctx.move_to(px, py);
                    started = true;
                }
            }
        }
        ctx.stroke();
        ctx.set_line_dash(&js_sys::Array::new());
    };
    line_of(&s.overlays.bb_upper, "#3a4556", 1.0, Some(3.0));
    line_of(&s.overlays.bb_lower, "#3a4556", 1.0, Some(3.0));
    line_of(&s.overlays.ema50, "#d29922", 1.4, None);
    line_of(&s.overlays.ema20, "#58a6ff", 1.4, None);

    // ---- candles ----
    let bw = (plot_w / n as f64 * 0.62).max(1.5);
    for (i, c) in cs.iter().enumerate() {
        let up = c.c >= c.o;
        let color = if up { "#2f9e6e" } else { "#e0566b" };
        ctx.set_stroke_style(&wasm_bindgen::JsValue::from_str(color));
        ctx.set_fill_style(&wasm_bindgen::JsValue::from_str(color));
        let px = x(i);
        // wick
        ctx.set_line_width(1.0);
        ctx.begin_path();
        ctx.move_to(px, y(c.h));
        ctx.line_to(px, y(c.l));
        ctx.stroke();
        // body
        let (top, bot) = (y(c.o.max(c.c)), y(c.o.min(c.c)));
        ctx.fill_rect(px - bw / 2.0, top, bw, (bot - top).max(1.0));
    }

    // last price tag
    let last = cs[n - 1].c;
    ctx.set_fill_style(&wasm_bindgen::JsValue::from_str("#c9d4e3"));
    ctx.set_font("bold 11px ui-monospace, monospace");
    ctx.fill_text(&fmt_price(last), W - PAD_R + 6.0, y(last) + 4.0);
    ctx.set_fill_style(&wasm_bindgen::JsValue::from_str("#66748c"));
    ctx.set_font("11px ui-monospace, monospace");

    // ---- volume pane ----
    ctx.set_stroke_style(&wasm_bindgen::JsValue::from_str("#1a2330"));
    ctx.begin_path();
    ctx.move_to(PAD_L, V_B);
    ctx.line_to(W - PAD_R, V_B);
    ctx.stroke();
    ctx.set_fill_style(&wasm_bindgen::JsValue::from_str("#66748c"));
    ctx.fill_text("vol", 6.0, V_T + 10.0);
    for (i, c) in cs.iter().enumerate() {
        let h = (c.v / max_vol) * (V_B - V_T);
        ctx.set_fill_style(&wasm_bindgen::JsValue::from_str(if c.c >= c.o {
            "#22402f"
        } else {
            "#3d2229"
        }));
        ctx.fill_rect(x(i) - bw / 2.0, V_B - h, bw, h);
    }

    // ---- RSI pane ----
    ctx.set_stroke_style(&wasm_bindgen::JsValue::from_str("#1a2330"));
    ctx.set_line_width(1.0);
    for gy in [R_T + (R_B - R_T) * 0.3, R_T + (R_B - R_T) * 0.7] {
        ctx.begin_path();
        ctx.move_to(PAD_L, gy);
        ctx.line_to(W - PAD_R, gy);
        ctx.stroke();
    }
    ctx.set_fill_style(&wasm_bindgen::JsValue::from_str("#66748c"));
    ctx.fill_text("70", 34.0, R_T + (R_B - R_T) * 0.3 + 4.0);
    ctx.fill_text("30", 34.0, R_T + (R_B - R_T) * 0.7 + 4.0);
    ctx.fill_text("RSI", 6.0, R_T + 10.0);
    let yr = |v: f64| R_T + (100.0 - v) / 100.0 * (R_B - R_T);
    ctx.set_stroke_style(&wasm_bindgen::JsValue::from_str("#bc8cff"));
    ctx.set_line_width(1.4);
    ctx.begin_path();
    let mut started = false;
    for (i, v) in s.overlays.rsi14.iter().enumerate() {
        if let Some(v) = v {
            let (px, py) = (x(i), yr(*v));
            if started {
                ctx.line_to(px, py);
            } else {
                ctx.move_to(px, py);
                started = true;
            }
        }
    }
    ctx.stroke();

    // ---- time axis ----
    ctx.set_fill_style(&wasm_bindgen::JsValue::from_str("#66748c"));
    ctx.fill_text(&fmt::fmt_short_time(cs[0].t_ms), PAD_L, H - 4.0);
    let mid = cs[n / 2].t_ms;
    ctx.fill_text(&fmt::fmt_short_time(mid), W / 2.0 - 18.0, H - 4.0);
    ctx.fill_text(&fmt::fmt_short_time(cs[n - 1].t_ms), W - PAD_R - 40.0, H - 4.0);
}

fn fmt_price(v: f64) -> String {
    if v >= 1000.0 {
        format!("{:.0}", v)
    } else if v >= 10.0 {
        format!("{:.1}", v)
    } else {
        format!("{:.3}", v)
    }
}

// ---------------------------------------------------------------------------
// Vol panel (term structure + skew + divergences)
// ---------------------------------------------------------------------------

#[component]
pub fn VolPanel(rs: Rs) -> impl IntoView {
    view! {
        <div class="rs-vol">
            {move || {
                let Some(a) = rs.iv.get() else {
                    return view! { <div class="rs-loading">"computing vol analytics…"</div> }.into_any();
                };
                let vr_cls = match a.vr_note.as_deref() {
                    Some("rich") => "rich",
                    Some("cheap") => "cheap",
                    _ => "fair",
                };
                view! {
                    <div class="rs-stat-row">
                        <div class="rs-stat">
                            <div class="rs-stat-label">{format!("{} ATM", a.und)}</div>
                            <div class="rs-stat-value">{a.atm_iv_vp.map(|v| format!("{v:.1}vp")).unwrap_or_else(|| "—".into())}</div>
                            <div class="rs-stat-sub dim">"composite cross-venue"</div>
                        </div>
                        <div class="rs-stat">
                            <div class="rs-stat-label">"IV percentile"</div>
                            <div class="rs-stat-value">{a.iv_percentile.map(|v| format!("{v:.0}")).unwrap_or_else(|| "—".into())}</div>
                            <div class="rs-stat-sub dim">"vs the trailing window"</div>
                        </div>
                        <div class="rs-stat">
                            <div class="rs-stat-label">"vol vs realized"</div>
                            <div class=format!("rs-stat-value {vr_cls}")>{a.vr_note.clone().unwrap_or_else(|| "—".into())}</div>
                            <div class="rs-stat-sub dim">"variance risk premium stance"</div>
                        </div>
                        <div class="rs-stat">
                            <div class="rs-stat-label">"put/call OI"</div>
                            <div class="rs-stat-value">{a.pc_ratio_oi.map(|v| format!("{v:.2}")).unwrap_or_else(|| "—".into())}</div>
                            <div class="rs-stat-sub dim">"front expiries"</div>
                        </div>
                    </div>
                    <div class="rs-vol-charts">
                        <div class="rs-vol-chart">
                            <div class="rs-vol-chart-title">"ATM IV term structure — composite and per venue"</div>
                            <TermChart a=a.clone()/>
                        </div>
                        <div class="rs-vol-chart">
                            <div class="rs-vol-chart-title">"Skew smile (front monthly) — IV by |delta|"</div>
                            <SkewChart a=a.clone()/>
                        </div>
                    </div>
                    <div class="rs-div-table-wrap">
                        <div class="rs-vol-chart-title">"Live cross-venue IV divergences (≥1vp)"</div>
                        <table class="rs-venue-table">
                            <thead>
                                <tr><th>"contract"</th><th>"dte"</th><th>"low"</th><th>"high"</th><th class="num">"gap"</th></tr>
                            </thead>
                            <tbody>
                                {a.top_divergences.iter().take(10).map(|d| {
                                    let iso = format!("{:04}-{:02}-{:02}", d.date / 10_000, (d.date / 100) % 100, d.date % 100);
                                    view! {
                                        <tr>
                                            <td>{format!("{} {} · {}{}", d.und, iso, fmt::fmt_strike(d.strike), d.right.letter())}</td>
                                            <td class="dim">{d.dte}</td>
                                            <td>{format!("{} {:.1}vp", fmt::venue_name(&d.low_venue), d.low_iv_vp)}</td>
                                            <td>{format!("{} {:.1}vp", fmt::venue_name(&d.high_venue), d.high_iv_vp)}</td>
                                            <td class="num pos">{format!("{:+.1}vp", d.spread_vp)}</td>
                                        </tr>
                                    }.into_any()
                                }).collect::<Vec<_>>()}
                            </tbody>
                        </table>
                    </div>
                }.into_any()
            }}
        </div>
    }
}

const TW: f64 = 640.0;
const TH: f64 = 200.0;

#[component]
fn TermChart(a: IvAnalytics) -> impl IntoView {
    let a = StoredValue::new(a);
    let svg = Memo::new(move |_| {
        let a = a.get_value();
        if a.term.len() < 2 {
            return None;
        }
        let mut all: Vec<(i32, f64)> = a.term.clone().into_iter().map(|p| (p.dte, p.iv_vp)).collect();
        for (_, pts) in &a.term_by_venue {
            for p in pts {
                all.push((p.dte, p.iv_vp));
            }
        }
        let d_min = all.iter().map(|(d, _)| *d).min()?;
        let d_max = all.iter().map(|(d, _)| *d).max()?;
        let v_min = all.iter().map(|(_, v)| *v).fold(f64::INFINITY, f64::min) - 1.0;
        let v_max = all.iter().map(|(_, v)| *v).fold(f64::NEG_INFINITY, f64::max) + 1.0;
        let (dm, dmx, vm, vmx) = (d_min as f64, d_max as f64, v_min, v_max);
        let x = |d: i32| 46.0 + (d as f64 - dm) / (dmx - dm).max(1.0) * (TW - 60.0);
        let y = |v: f64| 14.0 + (vmx - v) / (vmx - vm).max(0.1) * (TH - 34.0);
        let path = |pts: &[(i32, f64)]| -> String {
            pts.iter()
                .enumerate()
                .map(|(i, (d, v))| {
                    if i == 0 {
                        format!("M{:.1},{:.1}", x(*d), y(*v))
                    } else {
                        format!(" L{:.1},{:.1}", x(*d), y(*v))
                    }
                })
                .collect()
        };
        let composite = path(&a.term.iter().map(|p| (p.dte, p.iv_vp)).collect::<Vec<_>>());
        let venues: Vec<(String, String)> = a
            .term_by_venue
            .iter()
            .map(|(v, pts)| (v.clone(), path(&pts.iter().map(|p| (p.dte, p.iv_vp)).collect::<Vec<_>>())))
            .collect();
        let grid = (0..=3)
            .map(|g| {
                let gy = 14.0 + (TH - 34.0) * g as f64 / 3.0;
                let gv = vmx - (vmx - vm) * g as f64 / 3.0;
                format!(
                    "<line x1='46' y1='{gy:.0}' x2='{}' y2='{gy:.0}' class='tc-grid'/>",
                    TW - 14.0
                ) + &format!(
                    "<text x='4' y='{:.0}' class='tc-label'>{gv:.0}</text>",
                    gy + 4.0
                )
            })
            .collect::<String>();
        let xlabels = a
            .term
            .iter()
            .map(|p| {
                format!(
                    "<text x='{:.1}' y='{}' class='tc-label' text-anchor='middle'>{}</text>",
                    x(p.dte),
                    TH - 4.0,
                    p.dte
                )
            })
            .collect::<String>();
        let venue_paths = venues
            .iter()
            .map(|(v, p)| format!("<path d='{p}' class='tc-venue {}'/>", venue_class(v)))
            .collect::<String>();
        let venue_labels = venues
            .iter()
            .take(5)
            .enumerate()
            .map(|(i, (v, _))| {
                format!(
                    "<text x='{}' y='{}' class='tc-label {}'>{}</text>",
                    52.0 + i as f64 * 108.0,
                    11.0,
                    venue_class(v),
                    fmt::venue_name(v)
                )
            })
            .collect::<String>();
        Some(format!(
            "<svg class='term-chart' viewBox='0 0 {TW} {TH}' preserveAspectRatio='none'>{grid}{venue_paths}<path d='{composite}' class='tc-composite'/>{xlabels}{venue_labels}</svg>"
        ))
    });
    view! {
        {move || match svg.get() {
            Some(s) => view! { <div class="rs-svg" inner_html=s/> }.into_any(),
            None => view! { <div class="rs-loading">"term structure warming up…"</div> }.into_any(),
        }}
    }
}

#[component]
fn SkewChart(a: IvAnalytics) -> impl IntoView {
    let a = StoredValue::new(a);
    let svg = Memo::new(move |_| {
        let a = a.get_value();
        if a.skew.is_empty() {
            return None;
        }
        let pts = &a.skew;
        let d_min = pts.iter().map(|p| p.delta).fold(f64::INFINITY, f64::min);
        let d_max = pts.iter().map(|p| p.delta).fold(f64::NEG_INFINITY, f64::max);
        let mut vs: Vec<f64> = pts.iter().filter_map(|p| p.put_iv_vp).collect();
        vs.extend(pts.iter().filter_map(|p| p.call_iv_vp));
        if vs.is_empty() {
            return None;
        }
        let v_min = vs.iter().cloned().fold(f64::INFINITY, f64::min) - 1.0;
        let v_max = vs.iter().cloned().fold(f64::NEG_INFINITY, f64::max) + 1.0;
        let x = |d: f64| 46.0 + (d - d_min) / (d_max - d_min).max(0.01) * (TW - 60.0);
        let y = |v: f64| 14.0 + (v_max - v) / (v_max - v_min).max(0.1) * (TH - 34.0);
        let path = |sel: fn(&SkewPoint) -> Option<f64>| -> String {
            let mut s = String::new();
            let mut started = false;
            for p in pts {
                if let Some(v) = sel(p) {
                    let seg = if started {
                        format!(" L{:.1},{:.1}", x(p.delta), y(v))
                    } else {
                        format!("M{:.1},{:.1}", x(p.delta), y(v))
                    };
                    s.push_str(&seg);
                    started = true;
                }
            }
            s
        };
        let put = path(|p| p.put_iv_vp);
        let call = path(|p| p.call_iv_vp);
        Some(format!(
            "<svg class='term-chart' viewBox='0 0 {TW} {TH}' preserveAspectRatio='none'>\
             <path d='{put}' class='tc-put'/>\
             <path d='{call}' class='tc-call'/>\
             <text x='4' y='{:.0}' class='tc-label'>{v_max:.0}</text>\
             <text x='4' y='{:.0}' class='tc-label'>{v_min:.0}</text>\
             <text x='{:.1}' y='{}' class='tc-label' text-anchor='middle'>0.1Δ</text>\
             <text x='{:.1}' y='{}' class='tc-label' text-anchor='middle'>0.9Δ</text>\
             </svg>",
            TH - 20.0,
            TH - 4.0,
            46.0 + (TW - 60.0) * 0.05,
            TH - 4.0,
            46.0 + (TW - 60.0) * 0.95,
            TH - 4.0,
        ))
    });
    view! {
        {move || match svg.get() {
            Some(s) => view! { <div class="rs-svg" inner_html=s/> }.into_any(),
            None => view! { <div class="rs-loading">"skew warming up…"</div> }.into_any(),
        }}
    }
}

fn venue_class(v: &str) -> &'static str {
    match v.strip_prefix("sim:").unwrap_or(v) {
        "deribit" => "v-deribit",
        "bybit" => "v-bybit",
        "okx" => "v-okx",
        "binance" => "v-binance",
        "delta" => "v-delta",
        "aevo" => "v-aevo",
        "derive" => "v-derive",
        "paradex" => "v-paradex",
        _ => "v-other",
    }
}
