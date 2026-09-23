//! ivarb-engine: live cross-exchange IV arbitrage scanning engine.
//!
//! Public API: [`spawn`] boots the engine core plus one supervised connector
//! per enabled venue and returns shared handles for the server.

pub mod config;
pub mod engine;
pub mod jsonutil;
pub mod key;
pub mod msg;
pub mod strategy;
pub mod venues;

use std::sync::Arc;
use std::time::Duration;

use arc_swap::ArcSwap;
use rustc_hash::FxHashMap;
use tokio::sync::{broadcast, mpsc, watch};

use ivarb_protocol::Snapshot;

pub use config::{EngineConfig, SimMode, VenueKind, VenueSpec};
pub use engine::EngineHandles;
pub use venues::VenueConnector;

/// Boot the engine. Returns handles shared with the HTTP/WS server.
pub fn spawn(cfg: EngineConfig) -> anyhow::Result<EngineHandles> {
    let cfg = Arc::new(cfg);

    let (venue_tx, venue_rx) = mpsc::channel(4096);
    let (bcast, _) = broadcast::channel::<Arc<String>>(1024);
    let snapshot = Arc::new(ArcSwap::from_pointee(Snapshot {
        ts_ms: crate::msg::now_ms(),
        stats: ivarb_protocol::EngineStats {
            ts_ms: crate::msg::now_ms(),
            uptime_s: 0,
            feed_mode: ivarb_protocol::FeedMode::Live,
            venues_live: 0,
            venues_total: cfg.venues.iter().filter(|v| v.enabled).count() as u32,
            instruments_tracked: 0,
            opps_active: 0,
            engine_loop_us_ewma: 0.0,
            msgs_total: 0,
            msg_rate: 0.0,
        },
        venues: cfg
            .venues
            .iter()
            .map(|v| ivarb_protocol::VenueStatus {
                venue: v.id.clone(),
                state: if v.enabled {
                    ivarb_protocol::ConnState::Connecting
                } else {
                    ivarb_protocol::ConnState::Disabled
                },
                is_sim: false,
                instruments: 0,
                msg_rate: 0.0,
                last_msg_ms: 0,
                taker_fee_rate: v.taker_fee_rate,
                coin_margined: v.coin_margined,
                contract_size: v.contract_size,
                note: if v.enabled {
                    None
                } else {
                    Some("disabled by config".into())
                },
            })
            .collect(),
        spots: Vec::new(),
        opportunities: Vec::new(),
    }));
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let (sig_tx, sig_rx) = mpsc::channel::<String>(8192);
    let interner = Arc::new(std::sync::Mutex::new(key::UnderlyingInterner::default()));
    let spots_shared: Arc<ArcSwap<FxHashMap<String, f64>>> =
        Arc::new(ArcSwap::from_pointee(FxHashMap::default()));

    // Pre-intern the configured underlyings so ids are deterministic.
    if let Ok(mut i) = interner.lock() {
        for u in &cfg.underlyings {
            i.intern(u);
        }
    }

    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .user_agent("ivarb-scanner/0.1")
        .build()?;

    // Engine core.
    tokio::spawn(engine::Engine::new(
        cfg.clone(),
        venue_rx,
        bcast.clone(),
        snapshot.clone(),
        shutdown_rx.clone(),
        sig_tx,
        spots_shared.clone(),
        interner.clone(),
    )
    .run());

    // Venue supervisors.
    for spec in cfg.venues.iter().filter(|v| v.enabled) {
        let conn: Box<dyn VenueConnector> = match spec.kind {
            VenueKind::Deribit => Box::new(venues::deribit::Deribit::new()),
            VenueKind::Okx => Box::new(venues::okx::Okx::new()),
            VenueKind::Bybit => Box::new(venues::bybit::Bybit::new()),
            VenueKind::Binance => Box::new(venues::binance::Binance::new()),
            VenueKind::Delta => Box::new(venues::delta::Delta::new()),
            VenueKind::Aevo => Box::new(venues::aevo::Aevo::new()),
            VenueKind::Derive => Box::new(venues::derive::Derive::new()),
            VenueKind::Thalex => Box::new(venues::thalex::Thalex::new()),
        };
        let ctx = venues::VenueCtx {
            tx: venue_tx.clone(),
            cfg: cfg.clone(),
            spec: spec.clone(),
            http: http.clone(),
            shutdown: shutdown_rx.clone(),
            interner: interner.clone(),
            spots: spots_shared.clone(),
        };
        tokio::spawn(venues::supervise(conn, ctx));
    }

    // Simulation fallback.
    if cfg.sim != SimMode::Off {
        let sim_ctx = venues::VenueCtx {
            tx: venue_tx.clone(),
            cfg: cfg.clone(),
            spec: cfg
                .venues
                .first()
                .cloned()
                .unwrap_or_else(|| config::default_venues().into_iter().next().unwrap()),
            http: http.clone(),
            shutdown: shutdown_rx.clone(),
            interner: interner.clone(),
            spots: spots_shared.clone(),
        };
        let snapshot_watcher = snapshot.clone();
        let mode = cfg.sim;
        tokio::spawn(async move {
            let ids = ["deribit", "okx", "bybit", "binance", "delta"];
            if mode == SimMode::Always {
                venues::sim::spawn_sims(&sim_ctx, &ids).await;
                return;
            }
            // Auto: if no live venue produced data within 30s, fall back to sim.
            let start = std::time::Instant::now();
            loop {
                tokio::time::sleep(Duration::from_secs(5)).await;
                if sim_ctx.shutdown() {
                    return;
                }
                if start.elapsed() < Duration::from_secs(30) {
                    continue;
                }
                let live = snapshot_watcher
                    .load()
                    .stats
                    .venues_live;
                if live == 0 {
                    tracing::warn!("no live venue data — activating simulation fallback");
                    venues::sim::spawn_sims(&sim_ctx, &ids).await;
                    return;
                }
            }
        });
    }

    Ok(EngineHandles {
        venue_tx,
        bcast,
        snapshot,
        shutdown: shutdown_tx,
        signal_rx: sig_rx,
        spots_shared,
    })
}
