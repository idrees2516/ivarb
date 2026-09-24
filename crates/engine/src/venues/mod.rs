//! Venue connector framework: trait, supervised lifecycle, and shared helpers.

pub mod aevo;
pub mod binance;
pub mod bybit;
pub mod delta;
pub mod derive;
pub mod deribit;
pub mod okx;
pub mod sim;
pub mod thalex;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use arc_swap::ArcSwap;
use rustc_hash::FxHashMap;
use tokio::net::TcpStream;
use tokio::sync::{mpsc, watch};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use ivarb_protocol::ConnState;

use crate::config::{EngineConfig, VenueSpec};
use crate::key::UnderlyingInterner;
use crate::msg::VenueMsg;

/// Everything a connector needs. Cheap to clone (all handles/Arcs).
#[derive(Clone)]
pub struct VenueCtx {
    pub tx: mpsc::Sender<VenueMsg>,
    pub cfg: Arc<EngineConfig>,
    pub spec: VenueSpec,
    pub http: reqwest::Client,
    pub shutdown: watch::Receiver<bool>,
    pub interner: Arc<std::sync::Mutex<UnderlyingInterner>>,
    /// Consensus spots (underlying → price), updated by the engine.
    pub spots: Arc<ArcSwap<FxHashMap<String, f64>>>,
}

impl VenueCtx {
    pub fn shutdown(&self) -> bool {
        *self.shutdown.borrow()
    }
    pub fn intern(&self, name: &str) -> u16 {
        self.interner
            .lock()
            .map(|mut i| i.intern(name))
            .unwrap_or(0)
    }
    pub fn consensus_spot(&self, und: &str) -> Option<f64> {
        self.spots.load().get(und).copied()
    }
}

#[async_trait]
pub trait VenueConnector: Send {
    fn id(&self) -> &str;
    /// Run one connection session. Returning `Err` triggers a supervised
    /// reconnect with backoff. Returning `Err(Fatal(msg))` disables the venue.
    async fn run(&mut self, ctx: VenueCtx) -> anyhow::Result<()>;
}

/// Fatal error: the venue can never work from this host (unreachable or
/// unsupported payload shape) — disable instead of retrying forever.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct Fatal(pub String);

fn fatal_of(e: &anyhow::Error) -> Option<String> {
    e.downcast_ref::<Fatal>().map(|f| f.0.clone())
}

static RESTARTS: AtomicU64 = AtomicU64::new(0);

/// Supervise a connector: restart with exponential backoff + jitter,
/// reset the backoff after a healthy minute, stop on shutdown or fatal.
pub async fn supervise(mut conn: Box<dyn VenueConnector>, mut ctx: VenueCtx) {
    let id = conn.id().to_string();
    let mut backoff = Duration::from_millis(700);
    loop {
        if ctx.shutdown() {
            break;
        }
        let _ = ctx
            .tx
            .send(VenueMsg::Status {
                venue: id.clone(),
                state: ConnState::Connecting,
                note: None,
            })
            .await;
        let started = Instant::now();
        let res = conn.run(ctx.clone()).await;
        match &res {
            Ok(()) => tracing::info!(venue = %id, "connector session ended cleanly"),
            Err(e) => tracing::warn!(venue = %id, error = %e, "connector session failed"),
        }
        if let Some(msg) = res.as_ref().err().and_then(fatal_of) {
            let _ = ctx
                .tx
                .send(VenueMsg::Status {
                    venue: id.clone(),
                    state: ConnState::Disabled,
                    note: Some(format!("FATAL:{msg}")),
                })
                .await;
            tracing::error!(venue = %id, "venue disabled: {msg}");
            break;
        }
        let _ = ctx
            .tx
            .send(VenueMsg::Status {
                venue: id.clone(),
                state: ConnState::Disconnected,
                note: None,
            })
            .await;
        if started.elapsed() > Duration::from_secs(60) {
            backoff = Duration::from_millis(700);
        }
        RESTARTS.fetch_add(1, Ordering::Relaxed);
        let jitter = crate::venues::rand_range(250);
        let wait = backoff + Duration::from_millis(jitter);
        tokio::select! {
            _ = tokio::time::sleep(wait) => {}
            _ = ctx.shutdown.changed() => {
                if ctx.shutdown() { break; }
            }
        }
        backoff = (backoff * 2).min(Duration::from_secs(30));
    }
}

pub type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

pub async fn ws_connect(url: &str) -> anyhow::Result<WsStream> {
    let (ws, resp) = tokio_tungstenite::connect_async(url).await?;
    tracing::debug!(url = %url, status = ?resp.status(), "ws connected");
    Ok(ws)
}

pub async fn http_get_json(http: &reqwest::Client, url: &str) -> anyhow::Result<serde_json::Value> {
    http_get_json_tmo(http, url, EngineConfig::rest_timeout()).await
}

/// `http_get_json` with an explicit timeout (for slow endpoints like Aevo's
/// 1.5 MB `/markets` snapshot).
pub async fn http_get_json_tmo(
    http: &reqwest::Client,
    url: &str,
    tmo: std::time::Duration,
) -> anyhow::Result<serde_json::Value> {
    let v = http
        .get(url)
        .timeout(tmo)
        .send()
        .await?
        .error_for_status()?
        .json::<serde_json::Value>()
        .await?;
    Ok(v)
}

pub async fn http_post_json(
    http: &reqwest::Client,
    url: &str,
    body: &serde_json::Value,
) -> anyhow::Result<serde_json::Value> {
    let v = http
        .post(url)
        .timeout(EngineConfig::rest_timeout())
        .json(body)
        .send()
        .await?
        .error_for_status()?
        .json::<serde_json::Value>()
        .await?;
    Ok(v)
}

/// Small deterministic-ish jitter helper (rand is already an engine dep).
pub fn rand_range(max: u64) -> u64 {
    use rand::Rng;
    rand::thread_rng().gen_range(0..max.max(1))
}

/// Convenience: send a status update.
pub async fn status(tx: &mpsc::Sender<VenueMsg>, venue: &str, st: ConnState, note: Option<String>) {
    let _ = tx
        .send(VenueMsg::Status {
            venue: venue.to_string(),
            state: st,
            note,
        })
        .await;
}

/// Reply helper for WS text messages.
pub fn text_msg(s: String) -> Message {
    Message::Text(s)
}
