//! Binance European options live gateway (eapi).
//!
//! API surface used:
//! - `POST /eapi/v1/order` — place one Market options order (`type=MARKET`,
//!   no price needed; quantity in contracts).
//! - `GET /eapi/v1/order?orderId=` — poll for `avgPrice` / `cumQty`.
//! - `GET /eapi/v1/balance` — options asset balances (preflight only).
//!
//! Auth: query-string parameters + `signature=hex(hmac(secret, query))`,
//! key in the `X-MBX-APIKEY` header. POST body is sent as a query string
//! (eapi signs the query regardless of HTTP verb).
//!
//! Note: eapi has **no** perp futures — hedges on the binance venue stay
//! paper/simulated unless FAPI keys are added later; the router reports
//! this honestly in its per-venue status.

use std::time::Duration;

use serde_json::Value;

use crate::paper::{Fill, Side};

use super::sign::binance_signed_query;

pub const BINANCE_EAPI_BASE: &str = "https://eapi.binance.com";

pub struct BinanceGate {
    http: reqwest::Client,
    pub key: String,
    secret: String,
    pub base: String,
}

impl BinanceGate {
    pub fn new(creds: &super::bybit::GatewayCreds, base: Option<String>) -> anyhow::Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .user_agent("ivarb/0.1")
            .build()?;
        Ok(Self {
            http,
            key: creds.key.clone(),
            secret: creds.secret.clone(),
            base: base.unwrap_or_else(|| BINANCE_EAPI_BASE.into()),
        })
    }

    async fn signed(&self, method: &str, path: &str, params: &str) -> anyhow::Result<Value> {
        let ts = crate::now_ms();
        let q = if params.is_empty() {
            format!("timestamp={ts}&recvWindow=5000")
        } else {
            format!("{params}&timestamp={ts}&recvWindow=5000")
        };
        let signed = binance_signed_query(&self.secret, &q);
        let url = format!("{}{}?{}", self.base, path, signed);
        let req = self
            .http
            .request(
                if method == "GET" { reqwest::Method::GET } else { reqwest::Method::POST },
                &url,
            )
            .header("X-MBX-APIKEY", &self.key);
        let v: Value = req.send().await?.error_for_status()?.json().await?;
        if let Some(code) = v.get("code").and_then(|c| c.as_i64()) {
            if code != 0 {
                anyhow::bail!("binance: {} ({})", v["msg"].as_str().unwrap_or("?"), code);
            }
        }
        Ok(v)
    }

    pub async fn ping(&self) -> anyhow::Result<()> {
        let url = format!("{}/eapi/v1/ping", self.base);
        let resp = self.http.get(&url).send().await?;
        if resp.status().is_success() {
            Ok(())
        } else {
            anyhow::bail!("binance eapi ping: HTTP {}", resp.status())
        }
    }

    /// Options-asset balances (USD-quoted where available).
    pub async fn balance(&self, asset: &str) -> anyhow::Result<f64> {
        let v = self.signed("GET", "/eapi/v1/balance", "").await?;
        for row in v.as_array().cloned().unwrap_or_default() {
            if row["asset"].as_str() == Some(asset) {
                return Ok(row["balance"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0.0));
            }
        }
        Ok(0.0)
    }

    /// Place a Market options order and resolve the fill by polling.
    pub async fn take_option(
        &self,
        symbol: &str,
        side: Side,
        quantity_contracts: f64,
        taker_rate: f64,
        _spot: f64,
    ) -> Fill {
        let params = format!(
            "symbol={symbol}&side={}&type=MARKET&quantity={:.6}",
            match side { Side::Buy => "BUY", Side::Sell => "SELL" },
            quantity_contracts
        );
        let v = match self.signed("POST", "/eapi/v1/order", &params).await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, symbol, "binance order rejected");
                return Fill::zero();
            }
        };
        let oid = v["orderId"].as_u64().unwrap_or(0);
        if oid == 0 {
            return Fill::zero();
        }
        let path = "/eapi/v1/order";
        let q = format!("orderId={oid}&symbol={symbol}");
        for _ in 0..3 {
            tokio::time::sleep(Duration::from_millis(140)).await;
            if let Ok(o) = self.signed("GET", path, &q).await {
                let cum = o["cumQty"].as_str().and_then(|s| s.parse::<f64>().ok())
                    .or_else(|| o["cumQty"].as_f64())
                    .unwrap_or(0.0);
                if cum > 0.0 {
                    return parse_binance_fill(&o, taker_rate, _spot);
                }
            }
        }
        Fill::zero()
    }
}

fn parse_binance_fill(order: &Value, taker_rate: f64, _spot: f64) -> Fill {
    let contracts = order["cumQty"].as_str()
        .and_then(|s| s.parse::<f64>().ok())
        .or_else(|| order["cumQty"].as_f64())
        .unwrap_or(0.0);
    let avg_px = order["avgPrice"].as_str()
        .and_then(|s| s.parse::<f64>().ok())
        .or_else(|| order["avgPrice"].as_f64())
        .unwrap_or(0.0);
    if contracts <= 0.0 || avg_px <= 0.0 {
        return Fill::zero();
    }
    // eapi reports fees in multiple rows (fee/feeRate arrays); the practical
    // estimate is the taker rate × premium notional until reconciliation.
    let fee_usd = taker_rate * contracts * avg_px;
    Fill { contracts, px_unit: avg_px, fee_usd, latency_ms: 0 }
}

#[cfg(test)]
pub(crate) fn test_parse_fill(order: &Value, rate: f64, spot: f64) -> Fill {
    parse_binance_fill(order, rate, spot)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binance_fill_from_recorded_shape() {
        // eapi order detail: cumQty/avgPrice as strings.
        let v = serde_json::json!({
            "orderId": 123,
            "status": "FILLED",
            "cumQty": "5",
            "avgPrice": "1620.25",
        });
        let f = test_parse_fill(&v, 0.0003, 1.0);
        assert_eq!(f.contracts, 5.0);
        assert!((f.px_unit - 1620.25).abs() < 1e-9);
        assert!((f.fee_usd - 0.0003 * 5.0 * 1620.25).abs() < 1e-9);
    }

    #[test]
    fn numeric_variants_too() {
        let v = serde_json::json!({ "cumQty": 2, "avgPrice": 100.0 });
        let f = test_parse_fill(&v, 0.0, 1.0);
        assert_eq!(f.contracts, 2.0);
        assert_eq!(f.px_unit, 100.0);
        assert_eq!(f.fee_usd, 0.0);
    }
}
