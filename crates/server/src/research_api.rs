//! Research-plane REST surface:
//!
//! - `GET  /api/research/pulse`          — one-glance market state
//! - `GET  /api/research/chain`          — consolidated option chain
//! - `GET  /api/research/ticks`          — tick-level BBO (50 ms-class feeds)
//! - `GET  /api/research/candles`        — underlying candles + indicators
//! - `GET  /api/research/iv`             — term structure / skew / divergences
//! - `GET  /api/research/liquidity`      — strike × expiry depth heatmap
//! - `GET  /api/research/venues`         — per-venue research stats
//! - `GET  /api/research/expiries`       — listed expiries helper
//! - `GET  /api/research/status`         — research-plane health
//! - `POST /api/research/agent`          — run the tool-calling agent
//! - `POST /api/research/suggest`        — direct option-builder call

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::Json;
use serde::Deserialize;

use ivarb_protocol::research::{AgentRun, ResearchPulse, ResearchStatus, SuggestionCard};
use ivarb_protocol::Right;

use crate::AppState;

pub fn router() -> axum::Router<AppState> {
    axum::Router::new()
        .route("/api/research/pulse", get(pulse))
        .route("/api/research/chain", get(chain))
        .route("/api/research/ticks", get(ticks))
        .route("/api/research/candles", get(candles))
        .route("/api/research/iv", get(iv))
        .route("/api/research/liquidity", get(liquidity))
        .route("/api/research/venues", get(venues))
        .route("/api/research/expiries", get(expiries))
        .route("/api/research/status", get(status))
        .route("/api/research/agent", post(agent))
        .route("/api/research/suggest", post(suggest))
}

async fn pulse(State(st): State<AppState>) -> Json<ResearchPulse> {
    Json(st.research.pulse())
}

#[derive(Deserialize)]
struct ChainQuery {
    und: Option<String>,
    expiry: Option<u32>,
    right: Option<String>,
    #[serde(default)]
    window: Option<f64>,
    #[serde(default)]
    limit: Option<usize>,
}

async fn chain(
    State(st): State<AppState>,
    Query(q): Query<ChainQuery>,
) -> Json<ivarb_protocol::research::ChainResponse> {
    let right = q.right.as_deref().and_then(|r| match r.to_uppercase().as_str() {
        "C" | "CALL" => Some(Right::Call),
        "P" | "PUT" => Some(Right::Put),
        _ => None,
    });
    Json(st.research.chain(
        q.und.as_deref().unwrap_or("BTC"),
        q.expiry,
        right,
        q.window.unwrap_or(12.0),
        q.limit.unwrap_or(60),
    ))
}

#[derive(Deserialize)]
struct CandleQuery {
    und: Option<String>,
    interval: Option<String>,
}

async fn candles(
    State(st): State<AppState>,
    Query(q): Query<CandleQuery>,
) -> impl IntoResponse {
    match st
        .research
        .candles(q.und.as_deref().unwrap_or("BTC"), q.interval.as_deref().unwrap_or("1h"))
    {
        Some(s) => Json(s).into_response(),
        None => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({
                "error": "candles not ready",
                "hint": "retry in a minute — the klines poller runs every 60 s"
            })),
        )
            .into_response(),
    }
}

#[derive(Deserialize)]
struct UndQuery {
    und: Option<String>,
}

async fn iv(
    State(st): State<AppState>,
    Query(q): Query<UndQuery>,
) -> Json<ivarb_protocol::research::IvAnalytics> {
    Json(st.research.iv(q.und.as_deref().unwrap_or("BTC")))
}

async fn liquidity(
    State(st): State<AppState>,
    Query(q): Query<UndQuery>,
) -> impl IntoResponse {
    match st.research.liquidity(q.und.as_deref().unwrap_or("BTC")) {
        Some(m) => Json(m).into_response(),
        None => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({
                "error": "liquidity map not sampled yet",
                "hint": "the books poller samples top strikes every 25 s"
            })),
        )
            .into_response(),
    }
}

async fn venues(State(st): State<AppState>) -> Json<Vec<ivarb_protocol::research::ResearchVenueStat>> {
    Json(st.research.venue_stats())
}

#[derive(Deserialize)]
struct TicksQuery {
    und: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
}

async fn ticks(
    State(st): State<AppState>,
    Query(q): Query<TicksQuery>,
) -> Json<ivarb_protocol::research::TicksResponse> {
    Json(st.research.ticks(q.und.as_deref().unwrap_or("BTC"), q.limit.unwrap_or(120)))
}

async fn expiries(
    State(st): State<AppState>,
    Query(q): Query<UndQuery>,
) -> Json<Vec<(u32, String, i32)>> {
    Json(st.research.expiries(q.und.as_deref().unwrap_or("BTC")))
}

async fn status(State(st): State<AppState>) -> Json<ResearchStatus> {
    Json(st.research.status())
}

#[derive(Deserialize)]
struct AgentQuery {
    query: String,
    #[serde(default)]
    und: Option<String>,
    #[serde(default)]
    view: Option<String>,
    #[serde(default)]
    horizon_days: Option<u32>,
    #[serde(default)]
    risk: Option<String>,
}

async fn agent(
    State(st): State<AppState>,
    Json(q): Json<AgentQuery>,
) -> Json<AgentRun> {
    // Structured form (builder UI "ask the agent" button) → natural language.
    let query = if let Some(view) = q.view {
        let und = q.und.unwrap_or_else(|| "BTC".into());
        let horizon = q.horizon_days.unwrap_or(14);
        let risk = q.risk.unwrap_or_else(|| "balanced".into());
        format!(
            "Create a {view} structure on {und} with a {horizon} day horizon, {risk} risk, and explain why it is coherent with the current market"
        )
    } else {
        q.query
    };
    let run = st.research.agent(query).await;
    Json(run)
}

#[derive(Deserialize)]
struct SuggestQuery {
    und: String,
    view: String,
    #[serde(default)]
    horizon_days: Option<u32>,
    #[serde(default)]
    risk: Option<String>,
}

async fn suggest(
    State(st): State<AppState>,
    Json(q): Json<SuggestQuery>,
) -> impl IntoResponse {
    let view = match q.view.to_lowercase().as_str() {
        "bullish" | "bull" | "up" => ivarb_protocol::research::MarketView::Bullish,
        "bearish" | "bear" | "down" => ivarb_protocol::research::MarketView::Bearish,
        "big_move" | "volatile" | "move" | "volatility" => ivarb_protocol::research::MarketView::BigMove,
        _ => ivarb_protocol::research::MarketView::NeutralRange,
    };
    let risk = match q.risk.as_deref().map(|s| s.to_lowercase()).as_deref() {
        Some("conservative") | Some("cons") | Some("safe") => ivarb_protocol::research::RiskAppetite::Conservative,
        Some("aggressive") | Some("aggr") => ivarb_protocol::research::RiskAppetite::Aggressive,
        _ => ivarb_protocol::research::RiskAppetite::Balanced,
    };
    let req = ivarb_research::SuggestRequest {
        und: q.und.to_uppercase(),
        view,
        horizon_days: q.horizon_days.unwrap_or(14).clamp(1, 180),
        risk,
    };
    match st.research.suggest(&req) {
        Ok(card) => Json(card).into_response(),
        Err(e) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": e })),
        )
            .into_response(),
    }
}

/// Keep the SuggestionCard type in scope for docs.
#[allow(dead_code)]
type _Card = Option<SuggestionCard>;
