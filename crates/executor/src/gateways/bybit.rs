//! Bybit V5 live order gateway (options + linear perp hedges).
//!
//! API surface used (all HMAC-signed REST):
//! - `POST /v5/order/create` — place one order. IOC semantics via
//!   `timeInForce=IOC` with `orderType=Limit`, or `orderType=Market`
//!   (options support Market; taker routes use Market for immediacy).
//! - `GET /v5/order/realtime` — poll the just-created order for
//!   `cumExecQty` / `avgPrice` when the create response carries them
//!   as zero (Market options orders frequently need this second read).
//! - `GET /v5/account/wallet-balance` — UNIFIED account equity per coin
//!   (used by preflight only; strategy sizing keeps its own accounting).
//!
//! Sizing note: Bybit option `qty` is in contracts; 1 BTC contract =
//! 0.01 BTC (`contract_size` from the engine's instrument metadata is
//! passed in and converted — never assumed).

use std::time::Duration;

use serde_json::Value;

use crate::paper::{Fill, Side};

use super::sign::bybit_sign;

pub const BYBIT_BASE: &str = "https://api.bybit.com";

pub struct BybitGate {
    http: reqwest::Client,
    pub key: String,
    secret: String,
    pub base: String,
    recv_window: String,
}

pub struct GatewayCreds {
    pub key: String,
    pub secret: String,
    pub passphrase: Option<String>,
}

impl BybitGate {
    pub fn new(creds: &GatewayCreds, base: Option<String>) -> anyhow::Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .user_agent("ivarb/0.1")
            .build()?;
        Ok(Self {
            http,
            key: creds.key.clone(),
            secret: creds.secret.clone(),
            base: base.unwrap_or_else(|| BYBIT_BASE.into()),
            recv_window: "5000".into(),
        })
    }

    async fn request(&self, method: &str, path: &str, query: &str, body: &str) -> anyhow::Result<Value> {
        let ts = crate::now_ms().to_string();
        let sig = bybit_sign(&self.secret, &ts, &self.key, &self.recv_window, if body.is_empty() { query } else { body });
        let url = if query.is_empty() {
            format!("{}{}", self.base, path)
        } else {
            format!("{}{}?{}", self.base, path, query)
        };
        let mut req = self
            .http
            .request(
                if method == "GET" { reqwest::Method::GET } else { reqwest::Method::POST },
                &url,
            )
            .header("X-BAPI-API-KEY", &self.key)
            .header("X-BAPI-TIMESTAMP", &ts)
            .header("X-BAPI-RECV-WINDOW", &self.recv_window)
            .header("X-BAPI-SIGN", sig);
        if method != "GET" {
            req = req.header("Content-Type", "application/json").body(body.to_string());
        }
        let v: Value = req.send().await?.error_for_status()?.json().await?;
        if v.get("retCode").and_then(|c| c.as_i64()).unwrap_or(-1) != 0 {
            anyhow::bail!("bybit: {} ({})", v["retMsg"].as_str().unwrap_or("?"), v["retCode"]);
        }
        Ok(v["result"].clone())
    }

    /// Read-only connectivity probe: server time (unsigned endpoint).
    pub async fn ping(&self) -> anyhow::Result<()> {
        let url = format!("{}/v5/market/time", self.base);
        let v: Value = self.http.get(&url).send().await?.error_for_status()?.json().await?;
        if v.get("retCode").and_then(|c| c.as_i64()) == Some(0) {
            Ok(())
        } else {
            anyhow::bail!("bybit ping failed: {v}")
        }
    }

    /// UNIFIED account balance for one coin (total equity, USD value).
    pub async fn wallet_balance(&self, coin: &str) -> anyhow::Result<f64> {
        let q = format!("accountType=UNIFIED&coin={coin}");
        let res = self.request("GET", "/v5/account/wallet-balance", &q, "").await?;
        let list = res["list"].as_array().cloned().unwrap_or_default();
        for acct in &list {
            for c in acct["coin"].as_array().cloned().unwrap_or_default() {
                if c["coin"].as_str() == Some(coin) {
                    return Ok(c["walletBalance"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0.0));
                }
            }
        }
        Ok(0.0)
    }

    /// Place an options Market order and resolve the fill.
    ///
    /// `qty_contracts` is in venue contracts; the returned `Fill` is
    /// normalized to per-underlying-unit USD premium like every other
    /// gateway (`avgPrice` on Bybit options is already USD per unit).
    pub async fn take_option(
        &self,
        symbol: &str,
        side: Side,
        qty_contracts: f64,
        taker_rate: f64,
        spot: f64,
    ) -> Fill {
        let body = serde_json::json!({
            "category": "option",
            "symbol": symbol,
            "side": match side { Side::Buy => "Buy", Side::Sell => "Sell" },
            "orderType": "Market",
            "qty": format!("{}", (qty_contracts * 1e6).round() / 1e6),
        })
        .to_string();
        let res = match self.request("POST", "/v5/order/create", "", &body).await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(error = %e, symbol, "bybit order create rejected");
                return Fill::zero();
            }
        };
        let oid = res["orderId"].as_str().unwrap_or_default().to_string();
        if oid.is_empty() {
            return Fill::zero();
        }
        // Market options orders settle asynchronously — poll once, briefly.
        let mut order = res.clone();
        for _ in 0..3 {
            let filled = order["cumExecQty"].as_str().and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0);
            if filled > 0.0 && order["orderStatus"].as_str().map(|s| s != "New" && s != "PartiallyFilled").unwrap_or(false) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(120)).await;
            let q = format!("category=option&orderId={oid}");
            if let Ok(r) = self.request("GET", "/v5/order/realtime", &q, "").await {
                if let Some(list) = r["list"].as_array() {
                    if let Some(o) = list.first() {
                        order = o.clone();
                    }
                }
            }
        }
        parse_bybit_fill(&order, taker_rate, spot)
    }

    /// Linear perp hedge (Market). `symbol` e.g. `BTCUSDT`.
    pub async fn hedge_perp(&self, symbol: &str, side: Side, qty: f64) -> anyhow::Result<(f64, f64)> {
        let body = serde_json::json!({
            "category": "linear",
            "symbol": symbol,
            "side": match side { Side::Buy => "Buy", Side::Sell => "Sell" },
            "orderType": "Market",
            "qty": format!("{}", (qty * 1e6).round() / 1e6),
        })
        .to_string();
        let res = self.request("POST", "/v5/order/create", "", &body).await?;
        let oid = res["orderId"].as_str().unwrap_or_default().to_string();
        let mut px = 0.0;
        let mut fee = 0.0;
        if !oid.is_empty() {
            let q = format!("category=linear&orderId={oid}");
            for _ in 0..3 {
                tokio::time::sleep(Duration::from_millis(120)).await;
                if let Ok(r) = self.request("GET", "/v5/order/realtime", &q, "").await {
                    if let Some(o) = r["list"].as_array().and_then(|l| l.first()) {
                        px = o["avgPrice"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0.0);
                        fee += o["cumExecFee"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0.0);
                        if px > 0.0 {
                            break;
                        }
                    }
                }
            }
        }
        Ok((px, fee))
    }
}

fn parse_bybit_fill(order: &Value, taker_rate: f64, spot: f64) -> Fill {
    let contracts = order["cumExecQty"].as_str().and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0);
    let avg_px = order["avgPrice"].as_str().and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0);
    if contracts <= 0.0 || avg_px <= 0.0 {
        return Fill::zero();
    }
    // Bybit reports fees in USDT (options) — treat as USD.
    let fee_reported = order["cumExecFee"].as_str().and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0);
    let fee_usd = if fee_reported > 0.0 { fee_reported } else { taker_rate * spot * contracts };
    Fill {
        contracts,
        px_unit: avg_px,
        fee_usd,
        latency_ms: 0,
    }
}

/// Parse a recorded (offline) order JSON — used by tests to pin the exact
/// response shape the live path depends on.
#[cfg(test)]
pub(crate) fn test_parse_fill(order: &Value, rate: f64, spot: f64) -> Fill {
    parse_bybit_fill(order, rate, spot)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bybit_fill_from_recorded_shape() {
        let v = serde_json::json!({
            "orderId": "opt-123",
            "orderStatus": "Filled",
            "cumExecQty": "3",
            "avgPrice": "1510.5",
            "cumExecFee": "0.9",
        });
        let f = test_parse_fill(&v, 0.0003, 100_000.0);
        assert_eq!(f.contracts, 3.0);
        assert!((f.px_unit - 1510.5).abs() < 1e-9);
        assert!((f.fee_usd - 0.9).abs() < 1e-9); // reported fee wins over estimate
    }

    #[test]
    fn zero_when_unfilled() {
        let v = serde_json::json!({ "orderId": "x", "orderStatus": "Cancelled", "cumExecQty": "0", "avgPrice": "" });
        assert_eq!(test_parse_fill(&v, 0.0003, 100_000.0).contracts, 0.0);
    }

    #[test]
    fn fee_falls_back_to_estimate() {
        let v = serde_json::json!({ "cumExecQty": "1", "avgPrice": "100", "cumExecFee": "0" });
        let f = test_parse_fill(&v, 0.0003, 100_000.0);
        assert!((f.fee_usd - 30.0).abs() < 1e-9); // 0.0003 * 100000 * 1
    }
}
