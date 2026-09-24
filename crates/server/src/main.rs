//! HTTP + WebSocket server for the IV arb scanner.
//!
//! - `GET /`              — serves the compiled Leptos frontend (frontend/dist)
//! - `GET /ws`            — live engine feed (snapshot on connect + deltas)
//! - `GET /api/snapshot`  — full engine snapshot (opps + holds + multihop)
//! - `GET /api/opps`      — active opportunities
//! - `GET /api/venues`    — venue connection statuses
//! - `GET /api/holds`     — hold-to-expiry lab candidates
//! - `GET /api/multihop`  — live multi-hop arbitrage routes
//! - `GET /api/ga`        — genetic-optimizer status
//! - `POST /api/ga/cmd`   — control the genetic optimizer
//! - `GET /api/exec`      — advantage-taker state
//! - `POST /api/exec/cmd` — control the advantage taker
//! - `GET /healthz`       — liveness probe
//!
//! Engine→client frames are serialized exactly once and fanned out through a
//! broadcast channel; snapshots are read wait-free from ArcSwap.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::Json;
use ivarb_engine::{spawn as spawn_engine, EngineConfig, GaHandle};
use ivarb_executor::{spawn as spawn_executor, ExecDeps};
use ivarb_protocol::{
    ClientMsg, ExecCommand, ExecSnapshot, GaCommand, GaStatus, ServerMsg, Snapshot,
};
use tokio::sync::{broadcast, mpsc};

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[derive(Clone)]
pub struct AppState {
    pub snapshot: Arc<ArcSwap<Snapshot>>,
    pub bcast: broadcast::Sender<Arc<String>>,
    pub exec_snap: Arc<ArcSwap<ExecSnapshot>>,
    pub exec_cmd: mpsc::UnboundedSender<ExecCommand>,
    /// Genetic-optimizer handle (status reads + commands).
    pub ga: GaHandle,
}

fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,ivarb_engine=info".into()),
        )
        .init();

    // `ivarb-server --live-preflight`: validate the authenticated Deribit
    // path (env gates + credentials + account) and exit — no engine, no
    // orders. Zero-risk way to check keys before arming live mode.
    if std::env::args().any(|a| a == "--live-preflight") {
        println!("IVARB live preflight — checking the authenticated Deribit path…\n");
        let checks = ivarb_executor::live::live_preflight().await;
        let mut all_ok = true;
        for c in &checks {
            println!("  [{}] {}", if c.ok { " OK " } else { "FAIL" }, c.line());
            if !c.ok {
                all_ok = false;
            }
        }
        println!(
            "\n{} — set IVARB_MODE=live + DERIBIT_API_KEY/SECRET + IVARB_LIVE_CONFIRM=yes to arm; \
             DERIBIT_WS_URL=wss://test.deribit.com/ws/api/v2 rehearses on testnet.",
            if all_ok { "PREFLIGHT PASSED" } else { "PREFLIGHT FAILED" }
        );
        std::process::exit(if all_ok { 0 } else { 1 });
    }

    let cfg = EngineConfig::from_env();
    std::fs::create_dir_all(&cfg.data_dir)?;
    let data_dir = cfg.data_dir.clone();
    let handles = spawn_engine(cfg)?;

    // The advantage taker: consumes engine snapshots, watches live legs,
    // executes risk-gated two-leg attempts (paper mode by default).
    let exec = spawn_executor(ExecDeps {
        snapshot: handles.snapshot.clone(),
        watched: handles.watched.clone(),
        watch_tx: handles.watch_tx.clone(),
        bcast: handles.bcast.clone(),
        shutdown: handles.shutdown.subscribe(),
    });

    let state = AppState {
        snapshot: handles.snapshot.clone(),
        bcast: handles.bcast.clone(),
        exec_snap: exec.snapshot.clone(),
        exec_cmd: exec.cmd_tx.clone(),
        ga: handles.ga.clone(),
    };

    // GA status pump: the optimizer thread is event-driven (own OS thread,
    // crossbeam channels), so the server polls its wait-free shared status
    // once per second and republishes it on the broadcast bus.
    {
        let st = state.clone();
        let mut shutdown = handles.shutdown.subscribe();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(1));
            let mut last_gen: u32 = 0;
            loop {
                tokio::select! {
                    _ = tick.tick() => {
                        let (status, changed) = {
                            let sh = st.ga.shared.load();
                            (sh.status.clone(), sh.status.generation != last_gen)
                        };
                        if changed {
                            last_gen = status.generation;
                        }
                        let msg = ServerMsg::Ga { status };
                        if let Ok(txt) = serde_json::to_string(&msg) {
                            let _ = st.bcast.send(Arc::new(txt));
                        }
                    }
                    _ = shutdown.changed() => {
                        if *shutdown.borrow() { break; }
                    }
                }
            }
        });
    }

    // JSONL signal audit writer (best-effort, non-blocking).
    tokio::spawn(signal_writer(
        handles.signal_rx,
        PathBuf::from(data_dir).join("signals.jsonl"),
    ));

    // Static frontend + routes.
    let dist = PathBuf::from("frontend/dist");
    let index = dist.join("index.html");
    let app = if index.exists() {
        let serve = tower_http::services::ServeDir::new(&dist)
            .fallback(tower_http::services::ServeFile::new(index));
        axum::Router::new()
            .route("/ws", get(ws_handler))
            .route("/api/snapshot", get(api_snapshot))
            .route("/api/opps", get(api_opps))
            .route("/api/venues", get(api_venues))
            .route("/api/holds", get(api_holds))
            .route("/api/multihop", get(api_multihop))
            .route("/api/ga", get(api_ga))
            .route("/api/ga/cmd", post(api_ga_cmd))
            .route("/api/exec", get(api_exec))
            .route("/api/exec/cmd", post(api_exec_cmd))
            .route("/healthz", get(healthz))
            .fallback_service(serve)
            .with_state(state)
    } else {
        axum::Router::new()
            .route("/ws", get(ws_handler))
            .route("/api/snapshot", get(api_snapshot))
            .route("/api/opps", get(api_opps))
            .route("/api/venues", get(api_venues))
            .route("/api/holds", get(api_holds))
            .route("/api/multihop", get(api_multihop))
            .route("/api/ga", get(api_ga))
            .route("/api/ga/cmd", post(api_ga_cmd))
            .route("/api/exec", get(api_exec))
            .route("/api/exec/cmd", post(api_exec_cmd))
            .route("/healthz", get(healthz))
            .fallback(get(frontend_missing))
            .with_state(state)
    };

    let bind: SocketAddr = format!(
        "{}:{}",
        std::env::var("BIND").unwrap_or_else(|_| "0.0.0.0".into()),
        std::env::var("PORT").unwrap_or_else(|_| "3000".into())
    )
    .parse()?;
    let listener = tokio::net::TcpListener::bind(bind).await?;
    tracing::info!("ivarb-server listening on http://{bind}");

    let shutdown = handles.shutdown;
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(async move {
        let ctrl_c = async {
            let _ = tokio::signal::ctrl_c().await;
        };
        let term = async {
            use tokio::signal::unix::{signal, SignalKind};
            match signal(SignalKind::terminate()) {
                Ok(mut s) => {
                    s.recv().await;
                }
                Err(_) => std::future::pending::<()>().await,
            }
        };
        tokio::select! {
            _ = ctrl_c => {},
            _ = term => {},
        }
        tracing::info!("shutdown signal received");
        let _ = shutdown.send(true);
    })
    .await?;
    Ok(())
}

async fn healthz() -> Json<serde_json::Value> {
    Json(serde_json::json!({ "ok": true, "ts": now_ms() }))
}

async fn api_snapshot(State(st): State<AppState>) -> Json<Snapshot> {
    let snap: Arc<Snapshot> = Arc::clone(&st.snapshot.load());
    Json(snap.as_ref().clone())
}

async fn api_opps(State(st): State<AppState>) -> Json<Vec<ivarb_protocol::Opportunity>> {
    let snap: Arc<Snapshot> = Arc::clone(&st.snapshot.load());
    Json(snap.opportunities.clone())
}

async fn api_venues(State(st): State<AppState>) -> Json<Vec<ivarb_protocol::VenueStatus>> {
    let snap: Arc<Snapshot> = Arc::clone(&st.snapshot.load());
    Json(snap.venues.clone())
}

async fn api_holds(State(st): State<AppState>) -> Json<Vec<ivarb_protocol::HoldCandidate>> {
    let snap: Arc<Snapshot> = Arc::clone(&st.snapshot.load());
    Json(snap.holds.clone())
}

async fn api_multihop(State(st): State<AppState>) -> Json<Vec<ivarb_protocol::MultiHopRoute>> {
    let snap: Arc<Snapshot> = Arc::clone(&st.snapshot.load());
    Json(snap.multihop.clone())
}

async fn api_ga(State(st): State<AppState>) -> Json<GaStatus> {
    let sh = st.ga.shared.load();
    Json(sh.status.clone())
}

async fn api_ga_cmd(
    State(st): State<AppState>,
    Json(cmd): Json<GaCommand>,
) -> Result<Json<serde_json::Value>, (StatusCode, &'static str)> {
    st.ga
        .cmd_tx
        .send(cmd)
        .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "optimizer stopped"))?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

async fn api_exec(State(st): State<AppState>) -> Json<ExecSnapshot> {
    let snap = st.exec_snap.load();
    Json(snap.as_ref().clone())
}

async fn api_exec_cmd(
    State(st): State<AppState>,
    Json(cmd): Json<ExecCommand>,
) -> Result<Json<serde_json::Value>, (StatusCode, &'static str)> {
    st.exec_cmd
        .send(cmd)
        .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "executor stopped"))?;
    Ok(Json(serde_json::json!({ "ok": true })))
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    State(st): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
) -> impl IntoResponse {
    tracing::debug!(peer = %addr, "ws client connected");
    ws.on_upgrade(move |socket| client_loop(socket, st))
}

async fn client_loop(socket: WebSocket, st: AppState) {
    let (mut sender, mut receiver) = socket.split();
    use futures_util::{SinkExt, StreamExt};

    // Snapshot on connect.
    let snap = st.snapshot.load();
    let snap_arc: Arc<Snapshot> = Arc::clone(&snap);
    let msg = ServerMsg::Snapshot {
        snapshot: snap_arc.as_ref().clone(),
    };
    if let Ok(txt) = serde_json::to_string(&msg) {
        if sender.send(Message::Text(txt)).await.is_err() {
            return;
        }
    }

    let mut rx = st.bcast.subscribe();
    let mut last_msg = tokio::time::Instant::now();

    loop {
        tokio::select! {
            from_client = receiver.next() => {
                match from_client {
                    Some(Ok(Message::Text(txt))) => {
                        if let Ok(ClientMsg::Refresh) = serde_json::from_str::<ClientMsg>(&txt) {
                            let snap = st.snapshot.load();
                            let msg = ServerMsg::Snapshot { snapshot: snap.as_ref().clone() };
                            if let Ok(t) = serde_json::to_string(&msg) {
                                if sender.send(Message::Text(t)).await.is_err() { break; }
                            }
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Ok(_)) => {}
                    Some(Err(_)) => break,
                }
            }
            from_engine = rx.recv() => {
                match from_engine {
                    Ok(txt) => {
                        if sender.send(Message::Text((*txt).clone())).await.is_err() { break; }
                        last_msg = tokio::time::Instant::now();
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::debug!(skipped = n, "ws client lagged; resyncing");
                        let snap = st.snapshot.load();
                        let msg = ServerMsg::Snapshot { snapshot: snap.as_ref().clone() };
                        if let Ok(t) = serde_json::to_string(&msg) {
                            if sender.send(Message::Text(t)).await.is_err() { break; }
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
            _ = tokio::time::sleep(Duration::from_secs(45)) => {
                // Heartbeat: if the engine has been silent (no data), ping the
                // client so proxies keep the socket open.
                if last_msg.elapsed() > Duration::from_secs(60) {
                    if sender.send(Message::Ping(vec![])).await.is_err() { break; }
                    last_msg = tokio::time::Instant::now();
                }
            }
        }
    }
}

async fn signal_writer(mut rx: tokio::sync::mpsc::Receiver<String>, path: PathBuf) {
    use tokio::io::AsyncWriteExt;
    let mut file = match tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .await
    {
        Ok(f) => f,
        Err(e) => {
            tracing::warn!("cannot open {}: {e}", path.display());
            return;
        }
    };
    while let Some(line) = rx.recv().await {
        if file.write_all(line.as_bytes()).await.is_err() {
            break;
        }
        if file.write_all(b"\n").await.is_err() {
            break;
        }
    }
    let _ = file.flush().await;
}

async fn frontend_missing() -> (StatusCode, &'static str) {
    (
        StatusCode::OK,
        r#"<!doctype html><html><head><meta charset="utf-8"><title>IVARB</title>
<style>body{background:#0a0e14;color:#c9d4e3;font-family:ui-monospace,monospace;display:flex;align-items:center;justify-content:center;height:100vh;margin:0}
div{text-align:center}</style></head>
<body><div><h1>IVARB server is running</h1>
<p>Frontend not built yet. Run <code>trunk build --release</code> in <code>frontend/</code>
and restart, or use the REST API: <code>/api/snapshot</code>, <code>/ws</code>.</p>
</div></body></html>"#,
    )
}
