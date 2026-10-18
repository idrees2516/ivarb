//! The research agent: a tool-calling loop in the spirit of the Vercel AI
//! SDK (`tools` + multi-step execution), with two interchangeable brains:
//!
//! - **desk** (default, zero-config): a deterministic analyst that plans
//!   tool calls from the query, executes them against the live consolidated
//!   book, and composes a grounded answer. Every number in the answer comes
//!   from a tool result.
//! - **llm** (optional): any OpenAI-compatible `/chat/completions` endpoint
//!   with function calling, configured through `IVARB_LLM_BASE_URL`,
//!   `IVARB_LLM_API_KEY`, `IVARB_LLM_MODEL`, `IVARB_LLM_MAX_STEPS`.

use std::sync::Arc;

use rustc_hash::FxHashMap;

use ivarb_protocol::research::{
    AgentRun, AgentToolCall, CandleSeries, LiquidityMap, MarketView, RiskAppetite,
    SuggestionCard,
};

use crate::consolidate::{format_strike, iso_date, ConsolidatedBook};
use crate::suggest::{self, SuggestRequest};

/// LLM brain configuration (OpenAI-compatible).
#[derive(Debug, Clone)]
pub struct LlmConfig {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub max_steps: u32,
}

impl LlmConfig {
    pub fn from_env() -> Option<Self> {
        let api_key = std::env::var("IVARB_LLM_API_KEY").ok()?;
        if api_key.trim().is_empty() {
            return None;
        }
        Some(Self {
            base_url: std::env::var("IVARB_LLM_BASE_URL")
                .unwrap_or_else(|_| "https://api.openai.com/v1".into())
                .trim_end_matches('/')
                .to_string(),
            api_key,
            model: std::env::var("IVARB_LLM_MODEL").unwrap_or_else(|_| "gpt-4o-mini".into()),
            max_steps: std::env::var("IVARB_LLM_MAX_STEPS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(6),
        })
    }
}

/// Everything a run needs, owned (no lifetimes — runs on the request path).
#[derive(Clone)]
pub struct AgentDeps {
    pub book: Arc<ConsolidatedBook>,
    pub candles: Arc<FxHashMap<(String, String), CandleSeries>>,
    pub liquidity: Arc<FxHashMap<String, LiquidityMap>>,
    pub llm: Option<LlmConfig>,
    pub http: reqwest::Client,
}

// ---------------------------------------------------------------------------
// Tools
// ---------------------------------------------------------------------------

pub const TOOL_SPECS: &[(&str, &str)] = &[
    ("market_pulse", "One-glance market state: spot, 24h change, venues live, instruments, ATM IV, IV percentile, realized vol, put/call OI ratio. Args: {\"und\":\"BTC\"}"),
    ("candles_indicators", "Trend read from hourly candles: EMA20/50 stance, RSI14, ATR, Bollinger position, realized vol. Args: {\"und\":\"BTC\"}"),
    ("option_chain", "Consolidated option chain rows for an expiry: strikes with cross-venue consensus IV, best bid/ask + venue, OI. Args: {\"und\":\"BTC\",\"max_rows\":12}"),
    ("iv_structure", "IV term structure, skew smile, IV percentile, variance risk premium, cross-venue IV divergences. Args: {\"und\":\"BTC\"}"),
    ("liquidity_map", "Where resting liquidity sits: strike x expiry depth heatmap + best venue per leg. Args: {\"und\":\"BTC\"}"),
    ("venue_compare", "Per-venue stats: instruments, ATM IV, average spread, OI, 24h volume. Args: {\"und\":\"BTC\"}"),
    ("build_structure", "Create an option structure coherent with live data. Args: {\"und\":\"BTC\",\"view\":\"bullish|bearish|neutral_range|big_move\",\"horizon_days\":14,\"risk\":\"conservative|balanced|aggressive\"}"),
];

struct ToolOut {
    summary: String,
    detail: serde_json::Value,
    card: Option<SuggestionCard>,
}

fn jstr(v: &serde_json::Value, names: &[&str]) -> String {
    for n in names {
        if let Some(s) = v.get(*n).and_then(|x| x.as_str()) {
            return s.to_string();
        }
    }
    String::new()
}

fn jnum(v: &serde_json::Value, names: &[&str]) -> Option<f64> {
    for n in names {
        if let Some(x) = v.get(*n) {
            if let Some(f) = x.as_f64() {
                return Some(f);
            }
            if let Some(s) = x.as_str() {
                if let Ok(f) = s.parse::<f64>() {
                    return Some(f);
                }
            }
        }
    }
    None
}

fn exec_tool(name: &str, args: &serde_json::Value, d: &AgentDeps) -> Result<ToolOut, String> {
    let und = {
        let u = jstr(args, &["und", "underlying", "base"]).to_uppercase();
        if d.book.rows.iter().any(|r| r.und == u) {
            u
        } else if d.book.rows.iter().any(|r| r.und == "BTC") {
            "BTC".to_string()
        } else {
            d.book
                .rows
                .first()
                .map(|r| r.und.clone())
                .unwrap_or_else(|| "BTC".into())
        }
    };

    match name {
        "market_pulse" => {
            let spot = d.book.spots.get(&und).copied();
            let summ = d.book.iv.iter().find(|s| s.und == und);
            let live = d.book.venue_stats.iter().filter(|v| v.online).count();
            let insts = d.book.venue_stats.iter().map(|v| v.instruments).sum::<u32>();
            let change = d
                .candles
                .get(&(und.clone(), "1h".into()))
                .and_then(|c| c.readout.change_24h_pct);
            let mut detail = serde_json::json!({
                "und": und,
                "spot": spot,
                "change_24h_pct": change,
                "venues_live": live,
                "instruments": insts,
                "data_mode": d.book.data_mode,
            });
            if let Some(s) = summ {
                detail["atm_iv_vp"] = serde_json::json!(s.atm_iv_vp);
                detail["iv_percentile"] = serde_json::json!(s.iv_percentile);
                detail["realized_vol_vp"] = serde_json::json!(s.realized_vol_vp);
                detail["vr_premium_vp"] = serde_json::json!(s.vr_premium_vp);
                detail["pc_ratio_oi"] = serde_json::json!(s.pc_ratio_oi);
                detail["term_slope_vp"] = serde_json::json!(s.term_slope_vp);
            }
            let fmt = |x: Option<f64>, suffix: &str| {
                x.map(|v| format!("{v:.1}{suffix}")).unwrap_or_else(|| "—".into())
            };
            let summary = format!(
                "{} {} · {} venues · {} instruments · ATM30 {} · IV pct {} · RV {}",
                und,
                spot.map(|p| format_usd(p)).unwrap_or_else(|| "—".into()),
                live,
                insts,
                fmt(summ.and_then(|s| s.atm_iv_vp), "vp"),
                fmt(summ.and_then(|s| s.iv_percentile), ""),
                fmt(summ.and_then(|s| s.realized_vol_vp), "vp"),
            );
            Ok(ToolOut { summary, detail, card: None })
        }
        "candles_indicators" => {
            let cs = d
                .candles
                .get(&(und.clone(), "1h".into()))
                .ok_or_else(|| format!("no candles for {und}"))?;
            let r = &cs.readout;
            let detail = serde_json::json!({
                "und": und,
                "interval": "1h",
                "bars": cs.candles.len(),
                "ema_trend": r.ema_trend,
                "rsi14": r.rsi14,
                "atr14_pct": r.atr14_pct,
                "bb_pos": r.bb_pos,
                "realized_vol_vp": r.realized_vol_vp,
                "change_24h_pct": r.change_24h_pct,
            });
            let summary = format!(
                "{} 1h trend {} · RSI {} · ATR {}% · BB pos {}",
                und,
                r.ema_trend,
                r.rsi14.map(|v| format!("{v:.0}")).unwrap_or_else(|| "—".into()),
                r.atr14_pct.map(|v| format!("{v:.2}")).unwrap_or_else(|| "—".into()),
                r.bb_pos.map(|v| format!("{v:+.2}")).unwrap_or_else(|| "—".into()),
            );
            Ok(ToolOut { summary, detail, card: None })
        }
        "option_chain" => {
            let max_rows = jnum(args, &["max_rows", "rows", "limit"]).unwrap_or(12.0) as usize;
            // nearest expiry
            let date = d
                .book
                .rows
                .iter()
                .filter(|r| r.und == und && r.dte >= 1)
                .map(|r| r.date)
                .min()
                .ok_or_else(|| format!("no chain for {und}"))?;
            let mut rows: Vec<_> = d
                .book
                .rows
                .iter()
                .filter(|r| r.und == und && r.date == date)
                .collect();
            rows.sort_by(|a, b| a.strike.partial_cmp(&b.strike).unwrap_or(std::cmp::Ordering::Equal));
            let spot = d.book.spots.get(&und).copied().unwrap_or(f64::NAN);
            // ATM-centered window
            if spot.is_finite() {
                let atm_i = rows
                    .iter()
                    .enumerate()
                    .min_by(|(_, a), (_, b)| {
                        let da = (a.strike - spot).abs();
                        let db = (b.strike - spot).abs();
                        da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
                    })
                    .map(|(i, _)| i)
                    .unwrap_or(0);
                let half = (max_rows / 2).max(1);
                let lo = atm_i.saturating_sub(half);
                let hi = (atm_i + half + 1).min(rows.len());
                rows = rows[lo..hi].to_vec();
            } else {
                rows.truncate(max_rows);
            }
            let jrows: Vec<serde_json::Value> = rows
                .iter()
                .map(|r| {
                    serde_json::json!({
                        "strike": r.strike,
                        "right": r.right.letter(),
                        "dte": r.dte,
                        "iv_vp": r.iv_consensus_vp,
                        "best_bid": r.best_bid_px,
                        "best_bid_venue": r.best_bid_venue,
                        "best_ask": r.best_ask_px,
                        "best_ask_venue": r.best_ask_venue,
                        "oi": r.open_interest,
                        "iv_spread_vp": r.iv_spread_vp,
                    })
                })
                .collect();
            let detail = serde_json::json!({
                "und": und, "date": date, "expiry": iso_date(date), "spot": spot, "rows": jrows,
            });
            Ok(ToolOut {
                summary: format!(
                    "{} {} chain ({} DTE): {} strikes, consensus IVs + best books",
                    und,
                    iso_date(date),
                    d.book.rows.iter().find(|r| r.date == date).map(|r| r.dte).unwrap_or(0),
                    rows.len()
                ),
                detail,
                card: None,
            })
        }
        "iv_structure" => {
            let a = crate::analytics::iv_analytics(&d.book, &und);
            let detail = serde_json::to_value(&a).map_err(|e| e.to_string())?;
            let term_txt = a
                .term
                .first()
                .zip(a.term.last())
                .map(|(f, b)| format!("front {}vp → back {}vp", f.iv_vp, b.iv_vp))
                .unwrap_or_else(|| "—".into());
            let top_div = a
                .top_divergences
                .first()
                .map(|x| {
                    format!(
                        "top div {} vs {}: {:+.1}vp",
                        x.low_venue, x.high_venue, x.spread_vp
                    )
                })
                .unwrap_or_else(|| "no cross-venue divergence ≥1vp".into());
            let summary = format!(
                "{} ATM {} · {} · VR {} · {}",
                und,
                a.atm_iv_vp.map(|v| format!("{v:.1}vp")).unwrap_or_else(|| "—".into()),
                term_txt,
                a.vr_note.clone().unwrap_or_else(|| "—".into()),
                top_div
            );
            Ok(ToolOut { summary, detail, card: None })
        }
        "liquidity_map" => {
            let m = d
                .liquidity
                .get(&und)
                .ok_or_else(|| format!("liquidity map for {und} not sampled yet"))?;
            let total_depth: f64 = m.cells.iter().filter_map(|c| c.depth_usd).sum();
            let best = m
                .cells
                .iter()
                .filter_map(|c| c.depth_usd.map(|x| (x, c)))
                .max_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
            let detail = serde_json::to_value(m).map_err(|e| e.to_string())?;
            let summary = format!(
                "{} liquidity: {} cells, {:.0}$k near-touch total, deepest {}",
                und,
                m.cells.len(),
                total_depth / 1000.0,
                best.map(|(_, c)| format!("{} {} {}", iso_date(c.date), format_strike(c.strike), c.right.letter()))
                    .unwrap_or_else(|| "—".into())
            );
            Ok(ToolOut { summary, detail, card: None })
        }
        "venue_compare" => {
            let stats: Vec<&ivarb_protocol::research::ResearchVenueStat> = d
                .book
                .venue_stats
                .iter()
                .filter(|v| v.instruments > 0)
                .collect();
            let detail = serde_json::json!({
                "venues": stats.iter().map(|v| serde_json::json!({
                    "venue": v.venue,
                    "online": v.online,
                    "is_sim": v.is_sim,
                    "instruments": v.instruments,
                    "atm_iv_vp": v.atm_iv_vp,
                    "avg_spread_pct": v.avg_spread_pct,
                    "oi_usd": v.oi_usd,
                    "volume_usd_24h": v.volume_usd_24h,
                    "enrichment": v.enrichment,
                })).collect::<Vec<_>>(),
            });
            let summary = format!(
                "{} venues quoting {}: {}",
                stats.len(),
                und,
                stats
                    .iter()
                    .map(|v| format!("{}{}", v.venue, if v.online { "" } else { "?" }))
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            Ok(ToolOut { summary, detail, card: None })
        }
        "build_structure" => {
            let view = match jstr(args, &["view", "stance"]).to_lowercase().as_str() {
                "bullish" | "bull" | "up" | "long" => MarketView::Bullish,
                "bearish" | "bear" | "down" | "short" => MarketView::Bearish,
                "big_move" | "volatility" | "volatile" | "move" | "breakout" => MarketView::BigMove,
                _ => MarketView::NeutralRange,
            };
            let risk = match jstr(args, &["risk"]).to_lowercase().as_str() {
                "conservative" | "cons" | "safe" => RiskAppetite::Conservative,
                "aggressive" | "aggr" => RiskAppetite::Aggressive,
                _ => RiskAppetite::Balanced,
            };
            let horizon = jnum(args, &["horizon_days", "horizon", "days"])
                .map(|x| x.clamp(1.0, 180.0) as u32)
                .unwrap_or(14);
            let candles = d.candles.get(&(und.clone(), "1h".into()));
            let card = suggest::build(
                &d.book,
                candles,
                &SuggestRequest {
                    und: und.clone(),
                    view,
                    horizon_days: horizon,
                    risk,
                },
                d.book.ts_ms,
            )?;
            let detail = serde_json::to_value(&card).map_err(|e| e.to_string())?;
            let summary = format!(
                "{} {} · {} legs · {} {:.0}$ · coherence {}/100",
                und,
                card.strategy,
                card.legs.len(),
                card.flow,
                card.net_premium_usd.abs(),
                card.coherence.score
            );
            Ok(ToolOut { summary, detail, card: Some(card) })
        }
        other => Err(format!("unknown tool {other}")),
    }
}

// ---------------------------------------------------------------------------
// Run entry point
// ---------------------------------------------------------------------------

pub async fn run(deps: AgentDeps, query: String) -> AgentRun {
    let t0 = std::time::Instant::now();
    let brain_name = deps
        .llm
        .as_ref()
        .map(|c| format!("llm:{}", c.model))
        .unwrap_or_else(|| "desk".into());
    let (steps, answer, cards) = match &deps.llm {
        Some(cfg) => llm_brain(deps.clone(), &query, cfg).await,
        None => desk_brain(&deps, &query),
    };
    AgentRun {
        query,
        brain: brain_name,
        steps,
        answer,
        cards,
        data_mode: deps.book.data_mode.to_string(),
        ms: t0.elapsed().as_millis() as u64,
    }
}

// ---------------------------------------------------------------------------
// Desk brain (deterministic, zero-config)
// ---------------------------------------------------------------------------

fn parse_und(q: &str, d: &AgentDeps) -> String {
    let ql = q.to_lowercase();
    for (tok, und) in [
        ("btc", "BTC"),
        ("bitcoin", "BTC"),
        ("eth", "ETH"),
        ("ethereum", "ETH"),
        ("ether", "ETH"),
        ("sol", "SOL"),
        ("solana", "SOL"),
    ] {
        if ql.contains(tok) {
            let u = und.to_string();
            if d.book.rows.iter().any(|r| r.und == u) {
                return u;
            }
        }
    }
    if d.book.rows.iter().any(|r| r.und == "BTC") {
        "BTC".into()
    } else {
        d.book.rows.first().map(|r| r.und.clone()).unwrap_or_else(|| "BTC".into())
    }
}

fn parse_view(q: &str) -> MarketView {
    let ql = q.to_lowercase();
    let bull = ["bull", "upside", "rally", "long ", "buy dips", "up move", "pump"];
    let bear = ["bear", "downside", "selloff", "sell-off", "short ", "down move", "dump", "crash"];
    let movey = ["volatile", "volatility", "big move", "breakout", "event", "cpi", "fed", "earnings", "explosive", "straddle"];
    let neutral = ["range", "sideways", "consolidat", "quiet", "chop", "neutral", "grind"];
    let score = |tokens: &[&str]| tokens.iter().filter(|t| ql.contains(*t)).count();
    let (sb, se, sm, sn) = (score(&bull), score(&bear), score(&movey), score(&neutral));
    if sm > 0 && sm >= sb && sm >= se {
        MarketView::BigMove
    } else if sb > se && sb > 0 {
        MarketView::Bullish
    } else if se > sb && se > 0 {
        MarketView::Bearish
    } else if sn > 0 {
        MarketView::NeutralRange
    } else {
        MarketView::NeutralRange
    }
}

fn parse_horizon(q: &str) -> u32 {
    let ql = q.to_lowercase();
    if let Some(days) = extract_number(&ql, "day") {
        return days.clamp(1, 180);
    }
    if let Some(w) = extract_number(&ql, "week") {
        return (w * 7).clamp(1, 180);
    }
    if let Some(m) = extract_number(&ql, "month") {
        return (m * 30).clamp(1, 180);
    }
    if ql.contains("0dte") || ql.contains("today") {
        return 1;
    }
    if ql.contains("week") {
        return 7;
    }
    if ql.contains("month") {
        return 30;
    }
    14
}

fn extract_number(ql: &str, after_word: &str) -> Option<u32> {
    // "3 days", "two weeks" (words up to twelve supported)
    let words: Vec<&str> = ql.split_whitespace().collect();
    for (i, w) in words.iter().enumerate() {
        if w.starts_with(|c: char| c.is_ascii_digit()) {
            let n: u32 = w.trim_matches(|c: char| !c.is_ascii_digit()).parse().ok()?;
            if let Some(next) = words.get(i + 1) {
                if next.starts_with(after_word) {
                    return Some(n);
                }
            }
        }
    }
    let word_nums = [
        ("one", 1u32), ("two", 2), ("three", 3), ("four", 4), ("five", 5),
        ("six", 6), ("seven", 7), ("eight", 8), ("nine", 9), ("ten", 10),
        ("eleven", 11), ("twelve", 12),
    ];
    for (i, w) in words.iter().enumerate() {
        if let Some((_, n)) = word_nums.iter().find(|(name, _)| *name == *w) {
            if let Some(next) = words.get(i + 1) {
                if next.starts_with(after_word) {
                    return Some(*n);
                }
            }
        }
    }
    None
}

fn parse_risk(q: &str) -> RiskAppetite {
    let ql = q.to_lowercase();
    if ql.contains("conservative") || ql.contains("safe") || ql.contains("careful") {
        RiskAppetite::Conservative
    } else if ql.contains("aggressive") || ql.contains("bold") || ql.contains("yolo") {
        RiskAppetite::Aggressive
    } else {
        RiskAppetite::Balanced
    }
}

fn desk_brain(d: &AgentDeps, query: &str) -> (Vec<AgentToolCall>, String, Vec<SuggestionCard>) {
    let und = parse_und(query, d);
    let ql = query.to_lowercase();

    // Plan: pulse + candles always; then topical tools; build last.
    let mut plan: Vec<(String, serde_json::Value)> = vec![
        ("market_pulse".into(), serde_json::json!({"und": und})),
        ("candles_indicators".into(), serde_json::json!({"und": und})),
    ];
    if ql.contains("chain") || ql.contains("strike") || ql.contains("expir") || ql.contains("quote") {
        plan.push(("option_chain".into(), serde_json::json!({"und": und, "max_rows": 10})));
    }
    if ql.contains("iv") || ql.contains("vol") || ql.contains("skew") || ql.contains("premium") || ql.contains("rich") || ql.contains("cheap") {
        plan.push(("iv_structure".into(), serde_json::json!({"und": und})));
    }
    if ql.contains("liquid") || ql.contains("depth") || ql.contains("size") || ql.contains("book") {
        plan.push(("liquidity_map".into(), serde_json::json!({"und": und})));
    }
    if ql.contains("venue") || ql.contains("exchange") || ql.contains("compare") || ql.contains("where") {
        plan.push(("venue_compare".into(), serde_json::json!({"und": und})));
    }
    let wants_build = [
        "creat", "build", "suggest", "structure", "strategy", "trade", "position", "ticket",
        "should i", "what option", "help me", "construct", "design", "give me",
    ]
    .iter()
    .any(|t| ql.contains(t));
    if wants_build {
        plan.push((
            "build_structure".into(),
            serde_json::json!({
                "und": und,
                "view": match parse_view(query) {
                    MarketView::Bullish => "bullish",
                    MarketView::Bearish => "bearish",
                    MarketView::NeutralRange => "neutral_range",
                    MarketView::BigMove => "big_move",
                },
                "horizon_days": parse_horizon(query),
                "risk": match parse_risk(query) {
                    RiskAppetite::Conservative => "conservative",
                    RiskAppetite::Balanced => "balanced",
                    RiskAppetite::Aggressive => "aggressive",
                },
            }),
        ));
    }

    let mut steps = Vec::new();
    let mut outs: Vec<(String, ToolOut)> = Vec::new();
    for (name, args) in plan {
        let t0 = std::time::Instant::now();
        match exec_tool(&name, &args, d) {
            Ok(o) => {
                steps.push(AgentToolCall {
                    tool: name.clone(),
                    args: serde_json::to_string(&args).unwrap_or_else(|_| "{}".into()),
                    summary: o.summary.clone(),
                    ms: t0.elapsed().as_millis() as u64,
                });
                outs.push((name, o));
            }
            Err(e) => {
                steps.push(AgentToolCall {
                    tool: name.clone(),
                    args: serde_json::to_string(&args).unwrap_or_else(|_| "{}".into()),
                    summary: format!("unavailable: {e}"),
                    ms: t0.elapsed().as_millis() as u64,
                });
            }
        }
    }
    let cards: Vec<SuggestionCard> = outs
        .iter()
        .filter_map(|(_, o)| o.card.clone())
        .collect();
    let answer = compose_answer(query, &und, &outs);
    (steps, answer, cards)
}

fn fmt_opt(x: Option<f64>, f: fn(f64) -> String) -> String {
    x.map(f).unwrap_or_else(|| "—".into())
}

fn format_usd(x: f64) -> String {
    if x >= 1000.0 {
        format!("${}", group_thousands(x.round() as i64))
    } else if x >= 1.0 {
        format!("${x:.2}")
    } else {
        format!("${x:.4}")
    }
}

/// 82450 → "82,450" (Rust format strings have no numeric grouping).
fn group_thousands(n: i64) -> String {
    let neg = n < 0;
    let s = n.unsigned_abs().to_string();
    let b = s.as_bytes();
    let mut out = String::with_capacity(s.len() + s.len() / 3 + 1);
    for (i, c) in b.iter().enumerate() {
        if i > 0 && (b.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(*c as char);
    }
    if neg {
        format!("-{out}")
    } else {
        out
    }
}

fn compose_answer(query: &str, und: &str, outs: &[(String, ToolOut)]) -> String {
    let get = |name: &str| outs.iter().find(|(n, _)| n == name).map(|(_, o)| &o.detail);
    let mut s = String::with_capacity(1600);

    // Snapshot
    if let Some(p) = get("market_pulse") {
        let spot = jnum(p, &["spot"]).unwrap_or(f64::NAN);
        let chg = jnum(p, &["change_24h_pct"]);
        s.push_str("## Market snapshot\n");
        s.push_str(&format!(
            "**{und}** trades at **{}** ({} over 24h). {} venues are quoting **{}** instruments in the consolidated book",
            format_usd(spot),
            chg.map(|c| format!("{c:+.1}%")).unwrap_or_else(|| "change n/a".into()),
            jnum(p, &["venues_live"]).map(|v| format!("{v:.0}")).unwrap_or_else(|| "0".into()),
            jnum(p, &["instruments"]).map(|v| format!("{v:.0}")).unwrap_or_else(|| "0".into()),
        ));
        let atm = jnum(p, &["atm_iv_vp"]);
        let _pct = jnum(p, &["iv_percentile"]);
        let rv = jnum(p, &["realized_vol_vp"]);
        let vr = jnum(p, &["vr_premium_vp"]);
        if atm.is_some() || rv.is_some() {
            s.push_str(&format!(
                ". ATM ~30d IV is **{}**, realized vol **{}** — the variance risk premium is **{}**",
                fmt_opt(atm, |v| format!("{v:.1}vp")),
                fmt_opt(rv, |v| format!("{v:.1}vp")),
                fmt_opt(vr, |v| format!("{v:+.1}vp")),
            ));
        }
        let pc = jnum(p, &["pc_ratio_oi"]);
        if let Some(pc) = pc {
            s.push_str(&format!(", put/call OI ratio **{pc:.2}**",));
        }
        s.push_str(".\n\n");
    }

    // Trend
    if let Some(c) = get("candles_indicators") {
        let trend = jstr(c, &["ema_trend"]);
        let rsi = jnum(c, &["rsi14"]);
        let atr = jnum(c, &["atr14_pct"]);
        let bb = jnum(c, &["bb_pos"]);
        s.push_str("## Trend & momentum\n");
        s.push_str(&format!(
            "Hourly candles read **{trend}** (EMA20 vs EMA50): RSI-14 {} ({}), ATR-14 {} of price, price sits {} inside the Bollinger band.\n\n",
            fmt_opt(rsi, |v| format!("{v:.0}")),
            rsi.map(|v| {
                if v >= 70.0 {
                    "overbought".to_string()
                } else if v <= 30.0 {
                    "oversold".to_string()
                } else {
                    "mid-range".to_string()
                }
            })
            .unwrap_or_else(|| "n/a".to_string()),
            fmt_opt(atr, |v| format!("{v:.2}%")),
            fmt_opt(bb, |v| format!("{v:+.2} σ-halfwidths")),
        ));
    }

    // IV structure
    if let Some(v) = get("iv_structure") {
        s.push_str("## Vol structure\n");
        let term = v.get("term").and_then(|t| t.as_array()).cloned().unwrap_or_default();
        if !term.is_empty() {
            let pts: Vec<String> = term
                .iter()
                .filter_map(|p| {
                    Some(format!(
                        "{}d {:.1}vp",
                        jnum(p, &["dte"])?,
                        jnum(p, &["iv_vp"])?
                    ))
                })
                .collect();
            s.push_str(&format!("Term structure: {}\n", pts.join(" · ")));
        }
        let vr_note = jstr(v, &["vr_note"]);
        let pct = jnum(v, &["iv_percentile"]);
        let divs = v.get("top_divergences").and_then(|t| t.as_array()).cloned().unwrap_or_default();
        s.push_str(&format!(
            "IV reads **{vr_note}** vs realized (percentile {}); ",
            fmt_opt(pct, |p| format!("{p:.0}"))
        ));
        s.push_str(&format!(
            "{} cross-venue divergences ≥1vp are live right now",
            divs.len()
        ));
        if let Some(first) = divs.first() {
            s.push_str(&format!(
                " — widest: **{} {} {}** at {:.1}vp on {} vs {:.1}vp on {}",
                iso_date(jnum(first, &["date"]).unwrap_or(0.0) as u32),
                format_strike(jnum(first, &["strike"]).unwrap_or(0.0)),
                jstr(first, &["right"]),
                jnum(first, &["low_iv_vp"]).unwrap_or(0.0),
                jstr(first, &["low_venue"]),
                jnum(first, &["high_iv_vp"]).unwrap_or(0.0),
                jstr(first, &["high_venue"]),
            ));
        }
        s.push_str(".\n\n");
    }

    // Liquidity
    if let Some(l) = get("liquidity_map") {
        let cells = l.get("cells").and_then(|c| c.as_array()).map(|a| a.len()).unwrap_or(0);
        let depth: f64 = l
            .get("cells")
            .and_then(|c| c.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.get("depth_usd").and_then(|d| d.as_f64()))
                    .sum::<f64>()
            })
            .unwrap_or(0.0);
        s.push_str("## Liquidity\n");
        s.push_str(&format!(
            "Sampled resting depth across {cells} strike/expiry cells totals **{:.0}$k** near the touch; the heatmap panel shows where the size concentrates and which venue pays best per leg.\n\n",
            depth / 1000.0
        ));
    }

    // Venue comparison
    if let Some(v) = get("venue_compare") {
        let vs = v.get("venues").and_then(|x| x.as_array()).cloned().unwrap_or_default();
        if !vs.is_empty() {
            s.push_str("## Venues\n");
            for q in vs.iter().take(8) {
                s.push_str(&format!(
                    "- **{}** — {} instruments, ATM {}, avg spread {}, OI ≈ {}, 24h vol ≈ {}\n",
                    jstr(q, &["venue"]),
                    jnum(q, &["instruments"]).map(|x| format!("{x:.0}")).unwrap_or_else(|| "—".into()),
                    fmt_opt(jnum(q, &["atm_iv_vp"]), |x| format!("{x:.1}vp")),
                    fmt_opt(jnum(q, &["avg_spread_pct"]), |x| format!("{x:.2}%")),
                    fmt_opt(jnum(q, &["oi_usd"]), |x| format!("{:.1}$m", x / 1e6)),
                    fmt_opt(jnum(q, &["volume_usd_24h"]), |x| format!("{:.1}$m", x / 1e6)),
                ));
            }
            s.push('\n');
        }
    }

    // Chain
    if let Some(c) = get("option_chain") {
        let rows = c.get("rows").and_then(|r| r.as_array()).cloned().unwrap_or_default();
        if !rows.is_empty() {
            s.push_str("## Chain (nearest expiry)\n");
            for r in rows.iter().take(10) {
                let ask_v = jstr(r, &["best_ask_venue"]);
                let at_venue = if ask_v.is_empty() {
                    String::new()
                } else {
                    format!(" @ {ask_v}")
                };
                s.push_str(&format!(
                    "- {} {} — IV {}, {} bid / {} ask{}, OI {}\n",
                    format_strike(jnum(r, &["strike"]).unwrap_or(0.0)),
                    jstr(r, &["right"]),
                    fmt_opt(jnum(r, &["iv_vp"]), |x| format!("{x:.1}vp")),
                    fmt_opt(jnum(r, &["best_bid"]), format_usd),
                    fmt_opt(jnum(r, &["best_ask"]), format_usd),
                    at_venue,
                    fmt_opt(jnum(r, &["oi"]), |x| format!("{x:.0}")),
                ));
            }
            s.push('\n');
        }
    }

    // Structure
    for (_, o) in outs.iter() {
        if let Some(card) = &o.card {
            s.push_str("## Suggested structure\n");
            s.push_str(&format!(
                "**{}** on **{und}** — {} legs, net **{} {}** (per 1 {und} exposure), max profit **{}**, max loss **{}**.\n",
                card.strategy,
                card.legs.len(),
                card.flow,
                format_usd(card.net_premium_usd.abs()),
                fmt_opt(card.max_profit_usd, format_usd),
                fmt_opt(card.max_loss_usd, format_usd),
            ));
            s.push_str("Legs (routed to the best live books):\n");
            for l in &card.legs {
                s.push_str(&format!(
                    "- **{} {}** {} @ {} — IV {}, Δ {}\n",
                    match l.action {
                        ivarb_protocol::research::LegAction::Buy => "BUY",
                        ivarb_protocol::research::LegAction::Sell => "SELL",
                    },
                    l.label,
                    format_usd(l.px),
                    l.venue,
                    fmt_opt(l.iv_vp, |x| format!("{x:.1}vp")),
                    fmt_opt(l.delta, |x| format!("{x:+.2}")),
                ));
            }
            if !card.breakevens.is_empty() {
                let bes: Vec<String> = card.breakevens.iter().map(|b| format_usd(*b)).collect();
                s.push_str(&format!("Breakeven{} at {}.\n", if bes.len() > 1 { "s" } else { "" }, bes.join(" / ")));
            }
            s.push_str(&format!(
                "Portfolio greeks: Δ {:+.2} · Γ {:+.4} · Θ {:+.2}/day · V {:+.2}/vp. Coherence **{}/100** — {}\n",
                card.greeks.delta,
                card.greeks.gamma,
                card.greeks.theta_day,
                card.greeks.vega_vp,
                card.coherence.score,
                one_line(&card.coherence.rationale),
            ));
        }
    }

    // Closing read (only if nothing was built)
    if !outs.iter().any(|(_, o)| o.card.is_some()) {
        s.push_str("## Read\n");
        let pulse = get("market_pulse");
        let candles = get("candles_indicators");
        let (atm, rv) = (
            pulse.and_then(|p| jnum(p, &["atm_iv_vp"])),
            pulse.and_then(|p| jnum(p, &["realized_vol_vp"])),
        );
        let trend = candles.map(|c| jstr(c, &["ema_trend"])).unwrap_or_default();
        match (atm, rv) {
            (Some(a), Some(r)) if a - r >= 8.0 => s.push_str(&format!(
                "Implied ({a:.1}vp) is running well above realized ({r:.1}vp) and the hourly trend is {trend} — premium selling structures (credit spreads, condors) are the coherent stance right now. Ask me to *build a neutral structure on {und}* and I will strike it from the live chain.\n"
            )),
            (Some(a), Some(r)) if a - r <= -3.0 => s.push_str(&format!(
                "Implied ({a:.1}vp) sits below realized ({r:.1}vp) — options are cheap vs the actual movement, which favors long-premium structures (debit spreads, straddles on a directional view). Tell me your view and horizon and I will build it.\n"
            )),
            _ => s.push_str(&format!(
                "Implied and realized vol are broadly aligned and the hourly trend reads {trend}. Ask me to *create a {und} structure* with your view (bullish / bearish / range / big move) and horizon, and I will pick strikes from the live cross-venue chain.\n"
            )),
        }
    } else {
        s.push_str("## Caveats\n");
        s.push_str("Numbers are live reads, not guarantees: re-run before executing, size to the quoted touch, and remember short legs carry margin and early-assignment risk. Fees and slippage beyond the touch are not modeled here.\n");
    }

    let _ = query;
    s
}

fn one_line(s: &str) -> String {
    // First sentence of the rationale.
    s.split(". ")
        .next()
        .map(|x| x.trim_end_matches('.').to_string())
        .unwrap_or_else(|| s.to_string())
}

// ---------------------------------------------------------------------------
// LLM brain (OpenAI-compatible function calling)
// ---------------------------------------------------------------------------

async fn llm_brain(
    deps: AgentDeps,
    query: &str,
    cfg: &LlmConfig,
) -> (Vec<AgentToolCall>, String, Vec<SuggestionCard>) {
    let system = "You are the IVARB research desk agent for crypto options. \
You have tools that read a live consolidated cross-venue option book (prices, IVs, OI, candles, liquidity). \
RULES: (1) Always call tools before answering; ground every number in tool results. \
(2) Never invent prices, IVs or strikes. (3) When the user wants to create an option or structure, \
call build_structure with the parsed view/horizon/risk and present its legs exactly as returned. \
(4) Be concise and quantitative; use markdown-lite (## headings, **bold**, - bullets).";

    let mut messages: Vec<serde_json::Value> = vec![
        serde_json::json!({"role": "system", "content": system}),
        serde_json::json!({"role": "user", "content": query}),
    ];
    let tools: Vec<serde_json::Value> = TOOL_SPECS
        .iter()
        .map(|(name, desc)| {
            serde_json::json!({
                "type": "function",
                "function": {
                    "name": name,
                    "description": desc,
                    "parameters": {"type": "object", "properties": {}},
                }
            })
        })
        .collect();

    let mut steps = Vec::new();
    let mut cards = Vec::new();
    let http = &deps.http;
    let mut final_text = String::new();

    for _ in 0..cfg.max_steps.max(1) {
        let body = serde_json::json!({
            "model": cfg.model,
            "messages": messages,
            "tools": tools,
            "tool_choice": "auto",
            "temperature": 0.2,
        });
        let resp = match http
            .post(format!("{}/chat/completions", cfg.base_url))
            .bearer_auth(&cfg.api_key)
            .timeout(std::time::Duration::from_secs(60))
            .json(&body)
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                final_text = format!("LLM transport error: {e}. Falling back to the desk brain's tools:\n\n");
                let (mut s2, ans, c2) = desk_brain(&deps, query);
                steps.append(&mut s2);
                return (steps, format!("{final_text}{ans}"), c2);
            }
        };
        let v: serde_json::Value = match resp.json().await {
            Ok(v) => v,
            Err(e) => {
                final_text = format!("LLM response error: {e}.");
                break;
            }
        };
        let choice = v
            .pointer("/choices/0")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        let msg = choice.get("message").cloned().unwrap_or(serde_json::Value::Null);
        let tool_calls = msg
            .get("tool_calls")
            .and_then(|t| t.as_array())
            .cloned()
            .unwrap_or_default();
        if tool_calls.is_empty() {
            final_text = msg
                .get("content")
                .and_then(|c| c.as_str())
                .unwrap_or("(empty response)")
                .to_string();
            break;
        }
        // Execute each requested tool, append results.
        messages.push(msg.clone());
        for tc in &tool_calls {
            let name = tc
                .pointer("/function/name")
                .and_then(|n| n.as_str())
                .unwrap_or("");
            let args_txt = tc.pointer("/function/arguments").and_then(|a| a.as_str()).unwrap_or("{}");
            let args: serde_json::Value = serde_json::from_str(args_txt).unwrap_or(serde_json::json!({}));
            let id = tc.get("id").and_then(|i| i.as_str()).unwrap_or("call").to_string();
            let t0 = std::time::Instant::now();
            let (ok, out_json, summary, card) = match exec_tool(name, &args, &deps) {
                Ok(o) => (
                    true,
                    o.detail.clone(),
                    o.summary.clone(),
                    o.card.clone(),
                ),
                Err(e) => (false, serde_json::json!({"error": e}), format!("error: {e}"), None),
            };
            let _ = ok;
            if let Some(c) = card {
                cards.push(c);
            }
            steps.push(AgentToolCall {
                tool: name.to_string(),
                args: serde_json::to_string(&args).unwrap_or_else(|_| "{}".into()),
                summary,
                ms: t0.elapsed().as_millis() as u64,
            });
            messages.push(serde_json::json!({
                "role": "tool",
                "tool_call_id": id,
                "content": serde_json::to_string(&out_json).unwrap_or_else(|_| "{}".into()),
            }));
        }
    }

    if final_text.is_empty() {
        final_text = "Reached the tool-step budget before a final answer; see the tool trace.".into();
    }
    (steps, final_text, cards)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ivarb_protocol::Right;

    fn deps() -> AgentDeps {
        // Minimal book with one BTC row so parse_und works.
        let mut spots = rustc_hash::FxHashMap::default();
        spots.insert("BTC".to_string(), 80000.0);
        let rows = vec![ivarb_protocol::research::ChainRow {
            und: "BTC".into(),
            date: 20261112,
            strike: 80000.0,
            right: Right::Call,
            spot: 80000.0,
            dte: 30,
            iv_consensus_vp: Some(55.0),
            iv_spread_vp: Some(1.0),
            best_ask_venue: Some("deribit".into()),
            best_ask_px: Some(3400.0),
            best_bid_venue: Some("bybit".into()),
            best_bid_px: Some(3300.0),
            open_interest: Some(100.0),
            venues: vec![],
        }];
        let book = Arc::new(ConsolidatedBook {
            ts_ms: 1_790_000_000_000,
            data_mode: "live",
            spots,
            rows,
            venue_stats: vec![],
            iv: vec![],
            atm_by_date: rustc_hash::FxHashMap::default(),
        });
        AgentDeps {
            book,
            candles: Arc::new(rustc_hash::FxHashMap::default()),
            liquidity: Arc::new(rustc_hash::FxHashMap::default()),
            llm: None,
            http: reqwest::Client::new(),
        }
    }

    #[test]
    fn parses_views_and_horizons() {
        assert!(matches!(parse_view("i am bullish on btc"), MarketView::Bullish));
        assert!(matches!(parse_view("bearish tilt"), MarketView::Bearish));
        assert!(matches!(parse_view("expect a big move into cpi"), MarketView::BigMove));
        assert!(matches!(parse_view("should grind sideways"), MarketView::NeutralRange));
        assert_eq!(parse_horizon("two weeks"), 14);
        assert_eq!(parse_horizon("30 day horizon"), 30);
        assert_eq!(parse_horizon("next month or so"), 30);
        assert_eq!(parse_horizon("no horizon given"), 14);
    }

    #[test]
    fn desk_brain_plans_tools_and_answers() {
        let d = deps();
        let (steps, answer, cards) = desk_brain(&d, "help me create a bullish btc structure for two weeks");
        let names: Vec<&str> = steps.iter().map(|s| s.tool.as_str()).collect();
        assert!(names.contains(&"market_pulse"));
        assert!(names.contains(&"candles_indicators"));
        assert!(names.contains(&"build_structure"));
        assert!(!answer.is_empty());
        assert!(answer.contains("##"));
        // one row chain → build may fail (no spread strikes) — card optional
        let _ = cards;
    }

    #[test]
    fn unknown_tool_errors_cleanly() {
        let d = deps();
        assert!(exec_tool("nope", &serde_json::json!({}), &d).is_err());
    }
}
