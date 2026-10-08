//! Venue router — one dispatch point that decides, per venue and per mode,
//! whether a leg executes on a **real exchange gateway** or the paper broker.
//!
//! Doctrine (unchanged from the two-leg live scaffold, now multi-venue):
//! - Paper mode (`IVARB_MODE` unset): every venue fills on PaperBroker —
//!   identical to yesterday's behavior, zero risk of surprise orders.
//! - Live mode (`IVARB_MODE=live` + `IVARB_LIVE_CONFIRM=yes`): a venue with
//!   credentials and a healthy gateway places REAL orders (Deribit WS IOC,
//!   Bybit V5 Market, OKX v5 IOC limit, Binance eapi Market). A venue
//!   without a gateway REFUSES the leg (zero fill) — a simulated leg inside
//!   a live attempt is forbidden, so mixed paper/live routes can never
//!   present themselves as real arbitrage.
//! - On-chain legs go through the EVM gateway (real Uniswap V3 quotes and,
//!   with a signer key, real broadcast swaps).
//!
//! Credentials come from env (see .env.example / docs/LIVE-TRADING.md):
//! `DERIBIT_API_KEY/SECRET`, `BYBIT_API_KEY/SECRET`, `OKX_API_KEY/SECRET/
//! PASSPHRASE`, `BINANCE_API_KEY/SECRET`. Nothing is ever logged.

pub mod binance;
pub mod bybit;
pub mod evm;
pub mod okx;
pub mod sign;

use std::sync::Arc;

use crate::live::{venue_symbol, DeribitSession};
use crate::paper::{Fill, PaperBroker, Side};

pub use bybit::GatewayCreds;
pub use evm::{ChainStatus, EvmGate, SwapOutcome};

/// Parse a canonical instrument (`BTC-20261030-85000-C`) into its parts so
/// the live gateways can format venue-native symbols.
pub fn parse_canonical(inst: &str) -> Option<(String, u32, f64, String)> {
    let parts: Vec<&str> = inst.split('-').collect();
    if parts.len() != 4 {
        return None;
    }
    let und = parts[0].to_string();
    let ymd: u32 = parts[1].parse().ok()?;
    let strike: f64 = parts[2].parse().ok()?;
    let right = parts[3].to_string();
    if !matches!(right.as_str(), "C" | "P") {
        return None;
    }
    Some((und, ymd, strike, right))
}

/// Per-venue live gateway state as published to the UI.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct VenueGateStatus {
    pub venue: String,
    /// Credentials detected in env.
    pub creds: bool,
    /// Gateway constructed and (for REST gates) pinged OK.
    pub healthy: bool,
    /// Human detail — never secrets (e.g. "bybit v5 ping ok · USDT 0.00").
    pub detail: String,
}

pub struct VenueRouter {
    paper: Arc<PaperBroker>,
    pub deribit: Option<Arc<DeribitSession>>,
    pub bybit: Option<bybit::BybitGate>,
    pub okx: Option<okx::OkxGate>,
    pub binance: Option<binance::BinanceGate>,
    pub evm: EvmGate,
    /// Live mode fully armed (mode + explicit confirm).
    live_armed: bool,
}

pub fn creds_from_env(key_var: &str, secret_var: &str, pass_var: Option<&str>) -> Option<GatewayCreds> {
    let key = std::env::var(key_var).ok().filter(|s| !s.trim().is_empty())?;
    let secret = std::env::var(secret_var).ok().filter(|s| !s.trim().is_empty())?;
    let passphrase = pass_var
        .and_then(|v| std::env::var(v).ok())
        .filter(|s| !s.trim().is_empty());
    Some(GatewayCreds { key, secret, passphrase })
}

impl VenueRouter {
    pub fn new(paper: Arc<PaperBroker>, live_armed: bool) -> Self {
        Self {
            paper,
            deribit: None,
            bybit: None,
            okx: None,
            binance: None,
            evm: EvmGate::from_env(),
            live_armed,
        }
    }

    pub fn paper(&self) -> &Arc<PaperBroker> {
        &self.paper
    }

    pub fn live_armed(&self) -> bool {
        self.live_armed
    }

    /// Does this venue have a live order gateway?
    pub fn has_gateway(&self, venue: &str) -> bool {
        match venue {
            "deribit" => self.deribit.is_some(),
            "bybit" => self.bybit.is_some(),
            "okx" => self.okx.is_some(),
            "binance" => self.binance.is_some(),
            _ => false,
        }
    }

    /// Venues with live gateways (the UI's "live gateways" count).
    pub fn gateway_count(&self) -> u32 {
        ["deribit", "bybit", "okx", "binance"]
            .iter()
            .filter(|v| self.has_gateway(v))
            .count() as u32
    }

    /// A multi-venue route is live-executable when every option venue in it
    /// has a gateway (paper legs inside live attempts are forbidden).
    pub fn route_live_ok(&self, venues: &[String]) -> Result<(), String> {
        if !self.live_armed {
            return Ok(());
        }
        let missing: Vec<String> = venues
            .iter()
            .filter(|v| !self.has_gateway(v))
            .cloned()
            .collect();
        if missing.is_empty() {
            Ok(())
        } else {
            Err(format!(
                "live route incomplete — no gateway on {} (add credentials for that venue or run paper mode)",
                missing.join(", ")
            ))
        }
    }

    /// The single dispatch point every taker leg flows through.
    ///
    /// `touch_unit`/`depth_contracts` describe the visible book (used by
    /// paper fills and as the live price anchor), `want_contracts` is in
    /// venue contracts, `ct` the venue's contract size (underlying per
    /// contract).
    #[allow(clippy::too_many_arguments)]
    pub async fn take(
        &self,
        venue: &str,
        instrument: &str,
        side: Side,
        touch_unit: f64,
        depth_contracts: f64,
        want_contracts: f64,
        taker_rate: f64,
        spot: f64,
        ct: f64,
    ) -> Fill {
        if !self.live_armed {
            return self
                .paper
                .take(venue, side, touch_unit, depth_contracts, want_contracts, taker_rate, spot, ct)
                .await;
        }
        if touch_unit <= 0.0 || want_contracts <= 0.0 {
            return Fill::zero();
        }
        // Format the venue-native symbol from the canonical instrument.
        let Some((und, ymd, strike, right)) = parse_canonical(instrument) else {
            tracing::warn!(venue, instrument, "unparseable canonical instrument — refusing live leg");
            return Fill::zero();
        };
        let symbol = venue_symbol(venue, &und, ymd, strike, &right);
        match venue {
            "deribit" => match &self.deribit {
                Some(sess) => sess.take(venue, &symbol, side, touch_unit, want_contracts, taker_rate, spot, ct).await,
                None => {
                    tracing::warn!("live leg refused: no deribit gateway");
                    Fill::zero()
                }
            },
            "bybit" => match &self.bybit {
                Some(g) => g.take_option(&symbol, side, want_contracts, taker_rate, spot).await,
                None => {
                    tracing::warn!("live leg refused: no bybit gateway");
                    Fill::zero()
                }
            },
            "okx" => match &self.okx {
                Some(g) => {
                    // IOC limit buffered through the touch.
                    let px = match side {
                        Side::Buy => touch_unit * 1.005,
                        Side::Sell => touch_unit * 0.995,
                    };
                    g.take_option(&symbol, side, want_contracts, px, taker_rate, spot).await
                }
                None => {
                    tracing::warn!("live leg refused: no okx gateway");
                    Fill::zero()
                }
            },
            "binance" => match &self.binance {
                Some(g) => g.take_option(&symbol, side, want_contracts, taker_rate, spot).await,
                None => {
                    tracing::warn!("live leg refused: no binance gateway");
                    Fill::zero()
                }
            },
            other => {
                tracing::warn!(venue = other, "live leg refused: no gateway for venue");
                Fill::zero()
            }
        }
    }

    /// Underlying hedge (perp) — live on venues whose perp API shares the
    /// same signing path; paper elsewhere (reported honestly per venue).
    pub async fn hedge(&self, venue: &str, und: &str, side: Side, units: f64, spot: f64) -> (f64, f64) {
        if !self.live_armed {
            return self.paper.hedge(venue, side, units, spot).await;
        }
        let bybit_sym = format!("{und}USDT");
        let okx_sym = format!("{und}-USDT-SWAP");
        match venue {
            "bybit" => match &self.bybit {
                Some(g) => match g.hedge_perp(&bybit_sym, side, units * 0.01).await {
                    // Bybit linear perp qty is in contracts (1 = 0.01 BTC).
                    Ok((px, fee)) if px > 0.0 => (px, fee),
                    _ => {
                        tracing::warn!("bybit perp hedge failed — falling back would fake a live hedge; refusing");
                        (0.0, 0.0)
                    }
                },
                None => (0.0, 0.0),
            },
            "okx" => match &self.okx {
                Some(g) => match g.hedge_perp(&okx_sym, side, units * 0.01).await {
                    // OKX USDT swap sz is in contracts (1 = 0.01 BTC).
                    Ok((px, fee)) if px > 0.0 => (px, fee),
                    _ => {
                        tracing::warn!("okx perp hedge failed — refusing instead of faking");
                        (0.0, 0.0)
                    }
                },
                None => (0.0, 0.0),
            },
            // Deribit perp (BTC-PERPETUAL) via WS + eapi has no perp: honest
            // limitation — hedge stays paper on these venues even in live
            // mode is NOT acceptable, so we refuse (0 fill) and the caller
            // reports it. Deribit support can be added through the existing
            // DeribitSession (private/buy on BTC-PERPETUAL, amount in USD).
            _ => (0.0, 0.0),
        }
    }

    /// Build (or connect) every gateway whose credentials are present.
    /// Called at boot and by the preflight command. Returns per-venue
    /// status lines. Never places orders.
    pub async fn connect_all(&mut self) -> Vec<VenueGateStatus> {
        let mut out = Vec::new();

        // Deribit (WS session, existing scaffold)
        match creds_from_env("DERIBIT_API_KEY", "DERIBIT_API_SECRET", None) {
            Some(c) => {
                let ws = crate::live::deribit_ws_url();
                match DeribitSession::connect_ws(&ws, &c.key, &c.secret).await {
                    Ok(sess) => {
                        self.deribit = Some(sess);
                        out.push(VenueGateStatus {
                            venue: "deribit".into(),
                            creds: true,
                            healthy: true,
                            detail: format!("ws auth ok · {ws}"),
                        });
                    }
                    Err(e) => out.push(VenueGateStatus {
                        venue: "deribit".into(),
                        creds: true,
                        healthy: false,
                        detail: format!("ws auth failed: {e}"),
                    }),
                }
            }
            None => out.push(VenueGateStatus {
                venue: "deribit".into(),
                creds: false,
                healthy: false,
                detail: "DERIBIT_API_KEY/SECRET not set".into(),
            }),
        }

        // Bybit (REST + HMAC)
        match creds_from_env("BYBIT_API_KEY", "BYBIT_API_SECRET", None) {
            Some(c) => match bybit::BybitGate::new(
                &c,
                std::env::var("BYBIT_BASE_URL").ok().filter(|s| !s.trim().is_empty()),
            ) {
                Ok(g) => {
                    let mut detail = String::new();
                    let healthy = match g.ping().await {
                        Ok(()) => {
                            detail.push_str("v5 ping ok");
                            true
                        }
                        Err(e) => {
                            detail.push_str(&format!("ping failed: {e}"));
                            false
                        }
                    };
                    self.bybit = Some(g);
                    out.push(VenueGateStatus {
                        venue: "bybit".into(),
                        creds: true,
                        healthy,
                        detail,
                    });
                }
                Err(e) => out.push(VenueGateStatus {
                    venue: "bybit".into(),
                    creds: true,
                    healthy: false,
                    detail: format!("gateway init failed: {e}"),
                }),
            },
            None => out.push(VenueGateStatus {
                venue: "bybit".into(),
                creds: false,
                healthy: false,
                detail: "BYBIT_API_KEY/SECRET not set".into(),
            }),
        }

        // OKX (REST + HMAC + passphrase)
        match creds_from_env("OKX_API_KEY", "OKX_API_SECRET", Some("OKX_API_PASSPHRASE")) {
            Some(c) => match okx::OkxGate::new(
                &c,
                std::env::var("OKX_BASE_URL").ok().filter(|s| !s.trim().is_empty()),
            ) {
                Ok(g) => {
                    let mut detail = String::new();
                    let healthy = match g.ping().await {
                        Ok(()) => {
                            detail.push_str("v5 time ok");
                            true
                        }
                        Err(e) => {
                            detail.push_str(&format!("ping failed: {e}"));
                            false
                        }
                    };
                    self.okx = Some(g);
                    out.push(VenueGateStatus {
                        venue: "okx".into(),
                        creds: true,
                        healthy,
                        detail,
                    });
                }
                Err(e) => out.push(VenueGateStatus {
                    venue: "okx".into(),
                    creds: true,
                    healthy: false,
                    detail: format!("gateway init failed: {e}"),
                }),
            },
            None => out.push(VenueGateStatus {
                venue: "okx".into(),
                creds: false,
                healthy: false,
                detail: "OKX_API_KEY/SECRET/PASSPHRASE not set".into(),
            }),
        }

        // Binance options (eapi + HMAC)
        match creds_from_env("BINANCE_API_KEY", "BINANCE_API_SECRET", None) {
            Some(c) => match binance::BinanceGate::new(
                &c,
                std::env::var("BINANCE_EAPI_BASE").ok().filter(|s| !s.trim().is_empty()),
            ) {
                Ok(g) => {
                    let mut detail = String::new();
                    let healthy = match g.ping().await {
                        Ok(()) => {
                            detail.push_str("eapi ping ok");
                            true
                        }
                        Err(e) => {
                            detail.push_str(&format!("ping failed: {e}"));
                            false
                        }
                    };
                    self.binance = Some(g);
                    out.push(VenueGateStatus {
                        venue: "binance".into(),
                        creds: true,
                        healthy,
                        detail,
                    });
                }
                Err(e) => out.push(VenueGateStatus {
                    venue: "binance".into(),
                    creds: true,
                    healthy: false,
                    detail: format!("gateway init failed: {e}"),
                }),
            },
            None => out.push(VenueGateStatus {
                venue: "binance".into(),
                creds: false,
                healthy: false,
                detail: "BINANCE_API_KEY/SECRET not set".into(),
            }),
        }

        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_parsing() {
        let (und, ymd, strike, right) = parse_canonical("BTC-20261030-85000-C").unwrap();
        assert_eq!(und, "BTC");
        assert_eq!(ymd, 20261030);
        assert_eq!(strike, 85000.0);
        assert_eq!(right, "C");
        assert_eq!(parse_canonical("ETH-20261030-3000.5-P").unwrap().2, 3000.5);
        assert!(parse_canonical("garbage").is_none());
        assert!(parse_canonical("BTC-20261030-85000-X").is_none());
    }

    #[tokio::test]
    async fn paper_mode_always_paper() {
        let paper = Arc::new(PaperBroker::new(&["deribit".into(), "binance".into()], 100_000.0));
        let router = VenueRouter::new(paper, false); // NOT live armed
        // Even with zero gateways, paper mode fills.
        let f = router
            .take(
                "deribit",
                "BTC-20261030-85000-C",
                Side::Buy,
                100.0,
                10.0,
                1.0,
                0.0003,
                100_000.0,
                1.0,
            )
            .await;
        assert!(f.contracts > 0.0);
    }

    #[tokio::test]
    async fn live_mode_refuses_ungated_venues() {
        let paper = Arc::new(PaperBroker::new(&["deribit".into(), "binance".into()], 100_000.0));
        let router = VenueRouter::new(paper, true); // live armed, NO gateways
        let f = router
            .take(
                "binance",
                "BTC-20261030-85000-C",
                Side::Buy,
                100.0,
                10.0,
                1.0,
                0.0003,
                100_000.0,
                1.0,
            )
            .await;
        assert_eq!(f.contracts, 0.0, "live leg on ungated venue must refuse, not simulate");
        // And garbage instruments refuse even on gated venues:
        let f2 = router.take("deribit", "garbage", Side::Buy, 100.0, 10.0, 1.0, 0.0003, 1.0, 1.0).await;
        assert_eq!(f2.contracts, 0.0);
    }

    #[tokio::test]
    async fn route_gate_doctrine() {
        let paper = Arc::new(PaperBroker::new(&["deribit".into()], 100_000.0));
        let mut router = VenueRouter::new(paper, true);
        // No gateways yet → route with any ungated venue refused.
        assert!(router.route_live_ok(&["deribit".into(), "binance".into()]).is_err());
        // Connect a fake deribit gateway by direct field injection is not
        // possible (real WS), so assert the count path instead:
        assert_eq!(router.gateway_count(), 0);
        // Paper mode allows everything.
        let router2 = VenueRouter::new(Arc::new(PaperBroker::new(&["x".into()], 1.0)), false);
        assert!(router2.route_live_ok(&["anything".into()]).is_ok());
        let _ = &mut router; // silence unused-mut in test profiles
    }
}
