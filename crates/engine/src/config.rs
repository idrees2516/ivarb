//! Engine configuration: thresholds, universe filters and per-venue specs.
//! Everything is overridable through environment variables.

use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VenueKind {
    Deribit,
    Okx,
    Bybit,
    Binance,
    Delta,
    Derive,
    Thalex,
    Aevo,
}

#[derive(Debug, Clone)]
pub struct VenueSpec {
    pub id: String,
    pub kind: VenueKind,
    /// Indicative taker fee as a fraction of underlying notional.
    pub taker_fee_rate: f64,
    pub coin_margined: bool,
    /// Fallback contract size (underlying units) when the venue does not publish one.
    pub contract_size: f64,
    pub ws_url: String,
    pub rest_base: String,
    /// REST polling period for poll-based connectors.
    pub poll_ms: u64,
    pub enabled: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SimMode {
    /// Start sims only if no live venue produces data within 30s.
    Auto,
    Always,
    Off,
}

#[derive(Debug, Clone)]
pub struct EngineConfig {
    pub venues: Vec<VenueSpec>,
    pub underlyings: Vec<String>,
    /// Entry threshold in vol points (spec default: 0.5).
    pub min_net_vp: f64,
    /// Exit threshold in vol points (hysteresis, spec: retire below 0.25).
    pub exit_net_vp: f64,
    /// A quote older than this marks the opportunity stale.
    pub stale_ms: i64,
    /// A quote older than this retires the opportunity entirely.
    pub retire_ms: i64,
    pub history_len: usize,
    pub history_min_dt_ms: i64,
    /// Max expiries tracked per venue (nearest).
    pub max_expiries: usize,
    /// Strike window around ATM (log-moneyness fraction).
    pub atm_pct: f64,
    pub min_dte: i32,
    pub max_dte: i32,
    pub sim: SimMode,
    pub data_dir: String,
    pub max_instruments_per_venue: usize,
    /// How long a venue may produce zero messages before being marked Degraded.
    pub degraded_after_ms: i64,
    // ---- signal sanity filters (quote-garbage rejection) ----
    /// Legs outside this IV band (vol points) are ignored.
    pub iv_min_vp: f64,
    pub iv_max_vp: f64,
    /// Signals are only emitted within this log-moneyness band of spot.
    pub signal_moneyness_pct: f64,
    /// Minimum dollar vega per contract for a signal.
    pub min_vega_usd: f64,
    /// Spreads wider than this (vp) are treated as stale/fantasy quotes.
    pub max_spread_vp: f64,
    /// Entry-gate multiplier for mark-mode (convergence-candidate) signals
    /// relative to the executable-edge gate.
    pub mark_gate_mult: f64,
    // ---- millisecond pipeline & advanced analytics ----
    /// WS delta push cadence (ms). Default 1 — the pipeline is millisecond-grade.
    pub broadcast_ms: u64,
    /// Housekeeping sweep cadence (ms).
    pub sweep_ms: u64,
    /// Full snapshot republish cadence (ms).
    pub snapshot_ms: u64,
    /// Multi-hop route scan cadence (ms).
    pub multihop_ms: u64,
    /// Hold-candidate refresh cadence (ms).
    pub holds_ms: u64,
    /// GA: ms per generation.
    pub ga_gen_ms: u64,
    /// GA: apply the best genome to live strategy parameters.
    pub ga_apply: bool,
    /// GA: enabled at all.
    pub ga_enabled: bool,
    /// OU/Kalman refit cadence for opportunity histories (ms).
    pub ou_refit_ms: u64,
    /// Executor cadences (ms): scan / monitor / push.
    pub exec_scan_ms: u64,
    pub exec_monitor_ms: u64,
    pub exec_push_ms: u64,
    /// Max underlying units considered per multi-hop leg (display cap).
    pub multihop_max_units: f64,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            venues: default_venues(),
            underlyings: vec!["BTC".into(), "ETH".into(), "SOL".into()],
            min_net_vp: 0.5,
            exit_net_vp: 0.25,
            stale_ms: 45_000,
            retire_ms: 180_000,
            history_len: 240,
            history_min_dt_ms: 2_000,
            max_expiries: 8,
            atm_pct: 0.25,
            min_dte: 1,
            max_dte: 400,
            sim: SimMode::Auto,
            broadcast_ms: 1,
            sweep_ms: 250,
            snapshot_ms: 1_000,
            data_dir: "./data".into(),
            max_instruments_per_venue: 1_200,
            degraded_after_ms: 20_000,
            iv_min_vp: 15.0,
            iv_max_vp: 250.0,
            signal_moneyness_pct: 0.12,
            min_vega_usd: 1.0,
            max_spread_vp: 25.0,
            mark_gate_mult: 1.5,
            multihop_ms: 250,
            holds_ms: 500,
            ga_gen_ms: 15_000,
            ga_apply: true,
            ga_enabled: true,
            ou_refit_ms: 2_000,
            exec_scan_ms: 1,
            exec_monitor_ms: 10,
            exec_push_ms: 25,
            multihop_max_units: 5.0,
        }
    }
}

pub fn default_venues() -> Vec<VenueSpec> {
    use VenueKind::*;
    vec![
        VenueSpec {
            id: "deribit".into(),
            kind: Deribit,
            taker_fee_rate: 0.0003, // 0.03% of underlying (taker)
            coin_margined: true,
            contract_size: 1.0,
            ws_url: "wss://www.deribit.com/ws/api/v2".into(),
            rest_base: "https://www.deribit.com".into(),
            poll_ms: 0,
            enabled: true,
        },
        VenueSpec {
            id: "okx".into(),
            kind: Okx,
            taker_fee_rate: 0.0003,
            coin_margined: false,
            contract_size: 0.01,
            ws_url: "wss://ws.okx.com:8443/ws/v5/public".into(),
            rest_base: "https://www.okx.com".into(),
            poll_ms: 0,
            enabled: true,
        },
        VenueSpec {
            id: "bybit".into(),
            kind: Bybit,
            taker_fee_rate: 0.0003,
            coin_margined: false,
            // Verified against deep-ITM parity: mark = intrinsic per 1 BTC.
            contract_size: 1.0,
            ws_url: "wss://stream.bybit.com/v5/public/option".into(),
            rest_base: "https://api.bybit.com".into(),
            poll_ms: 0,
            enabled: true,
        },
        VenueSpec {
            id: "binance".into(),
            kind: Binance,
            taker_fee_rate: 0.0002,
            coin_margined: false,
            contract_size: 0.01,
            ws_url: "wss://nbstream.binance.com/eoptions/ws".into(),
            rest_base: "https://eapi.binance.com".into(),
            poll_ms: 5_000,
            enabled: true,
        },
        VenueSpec {
            id: "delta".into(),
            kind: Delta,
            taker_fee_rate: 0.0003,
            coin_margined: false,
            // /v2/products publishes contract_value per instrument (parsed).
            contract_size: 0.01,
            ws_url: String::new(),
            rest_base: "https://api.delta.exchange".into(),
            poll_ms: 5_000,
            enabled: true,
        },
        VenueSpec {
            id: "aevo".into(),
            kind: Aevo,
            taker_fee_rate: 0.0002,
            coin_margined: false,
            contract_size: 1.0,
            ws_url: String::new(),
            rest_base: "https://api.aevo.xyz".into(),
            poll_ms: 8_000,
            enabled: true,
        },
        VenueSpec {
            id: "derive".into(),
            kind: Derive,
            taker_fee_rate: 0.0003,
            coin_margined: false,
            contract_size: 1.0,
            // Derive's RPC API lives on the lyra.finance host (probed live:
            // POST /public/get_instruments + /public/get_tickers per expiry).
            ws_url: "wss://api.lyra.finance/ws".into(),
            rest_base: "https://api.lyra.finance".into(),
            poll_ms: 6_000,
            enabled: true,
        },
        VenueSpec {
            id: "thalex".into(),
            kind: Thalex,
            taker_fee_rate: 0.0002,
            coin_margined: false,
            contract_size: 1.0,
            ws_url: "wss://api.thalex.com/ws".into(),
            rest_base: "https://api.thalex.com".into(),
            poll_ms: 8_000,
            enabled: true,
        },
    ]
}

fn env_str(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|s| !s.trim().is_empty())
}
fn env_f64(key: &str) -> Option<f64> {
    env_str(key).and_then(|v| v.trim().parse().ok())
}
fn env_u64(key: &str) -> Option<u64> {
    env_str(key).and_then(|v| v.trim().parse().ok())
}
impl EngineConfig {
    pub fn from_env() -> Self {
        let mut cfg = Self::default();

        if let Some(v) = env_f64("IVARB_MIN_NET_VP") {
            cfg.min_net_vp = v;
        }
        if let Some(v) = env_f64("IVARB_EXIT_VP") {
            cfg.exit_net_vp = v;
        }
        if let Some(v) = env_i64_or("IVARB_STALE_MS") {
            cfg.stale_ms = v;
        }
        if let Some(v) = env_i64_or("IVARB_RETIRE_MS") {
            cfg.retire_ms = v;
        }
        if let Some(v) = env_u64("IVARB_BROADCAST_MS") {
            cfg.broadcast_ms = v.clamp(1, 10_000);
        }
        if let Some(v) = env_u64("IVARB_SWEEP_MS") {
            cfg.sweep_ms = v.clamp(10, 10_000);
        }
        if let Some(v) = env_u64("IVARB_SNAPSHOT_MS") {
            cfg.snapshot_ms = v.clamp(50, 60_000);
        }
        if let Some(v) = env_u64("IVARB_MULTIHOP_MS") {
            cfg.multihop_ms = v.clamp(50, 10_000);
        }
        if let Some(v) = env_u64("IVARB_HOLDS_MS") {
            cfg.holds_ms = v.clamp(100, 10_000);
        }
        if let Some(v) = env_u64("IVARB_GA_GEN_MS") {
            cfg.ga_gen_ms = v.clamp(1_000, 600_000);
        }
        if let Some(v) = env_str("IVARB_GA_APPLY") {
            cfg.ga_apply = !matches!(v.to_lowercase().as_str(), "0" | "false" | "off" | "no");
        }
        if let Some(v) = env_str("IVARB_GA_ENABLED") {
            cfg.ga_enabled = !matches!(v.to_lowercase().as_str(), "0" | "false" | "off" | "no");
        }
        if let Some(v) = env_u64("IVARB_EXEC_SCAN_MS") {
            cfg.exec_scan_ms = v.clamp(1, 10_000);
        }
        if let Some(v) = env_u64("IVARB_EXEC_MONITOR_MS") {
            cfg.exec_monitor_ms = v.clamp(5, 10_000);
        }
        if let Some(v) = env_u64("IVARB_EXEC_PUSH_MS") {
            cfg.exec_push_ms = v.clamp(10, 10_000);
        }
        if let Some(v) = env_str("IVARB_UNDERLYINGS") {
            cfg.underlyings = v
                .split(',')
                .map(|s| s.trim().to_uppercase())
                .filter(|s| !s.is_empty())
                .collect();
        }
        if let Some(v) = env_str("IVARB_DATA_DIR") {
            cfg.data_dir = v;
        }
        if let Some(v) = env_str("IVARB_SIM") {
            cfg.sim = match v.to_lowercase().as_str() {
                "always" | "1" | "true" => SimMode::Always,
                "off" | "never" | "0" | "false" => SimMode::Off,
                _ => SimMode::Auto,
            };
        }
        if let Some(v) = env_str("IVARB_VENUES") {
            let enabled: Vec<String> = v
                .split(',')
                .map(|s| s.trim().to_lowercase())
                .filter(|s| !s.is_empty())
                .collect();
            for spec in &mut cfg.venues {
                spec.enabled = enabled.iter().any(|e| e == &spec.id);
            }
        }
        cfg
    }

    pub fn rest_timeout() -> Duration {
        Duration::from_secs(20)
    }
}

fn env_i64_or(key: &str) -> Option<i64> {
    env_str(key).and_then(|v| v.trim().parse().ok())
}
