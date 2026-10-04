//! OKX v5 live order gateway (options + SWAP perp hedges).
//!
//! API surface used:
//! - `POST /api/v5/trade/order` — place one order. Taker immediacy via
//!   `ordType=ioc` (limit with `px` buffered through the touch) — OKX
//!   options do not support `optimal_limit_ioc`.
//! - `GET /api/v5/trade/order?instId=&ordId=` — poll for `accFillSz` /
//!   `avgPx` / `fee` (the create response only returns `ordId` + `sCode`).
//! - `GET /api/v5/account/balance` — account equity (preflight only).
//!
//! Auth: `OK-ACCESS-SIGN = base64(hmac(secret, ts + METHOD + path + body))`,
//! `OK-ACCESS-TIMESTAMP` ISO-8601, `OK-ACCESS-PASSPHRASE`, `OK-ACCESS-KEY`.
//!
//! Sizing: OKX option `sz` is in contracts (1 sheet = 0.01 BTC / 0.1 ETH —
//! converted via the engine's contract_size, never assumed). Options `px`
//! is in USD per underlying unit.

use std::time::Duration;

use serde_json::Value;

use crate::paper::{Fill, Side};

use super::sign::{iso8601_utc, okx_sign};

pub const OKX_BASE: &str = "https://www.okx.com";

pub struct OkxGate {
    http: reqwest::Client,
    pub key: String,
    secret: String,
    passphrase: String,
    pub base: String,
}

impl OkxGate {
    pub fn new(creds: &super::bybit::GatewayCreds, base: Option<String>) -> anyhow::Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .user_agent("ivarb/0.1")
            .build()?;
        Ok(Self {
            http,
            key: creds.key.clone(),
            secret: creds.secret.clone(),
            passphrase: creds.passphrase.clone().unwrap_or_default(),
            base: base.unwrap_or_else(|| OKX_BASE.into()),
        })
    }

    async fn request(&self, method: &str, path: &str, body: &str) -> anyhow::Result<Value> {
        let ts = iso8601_utc(crate::now_ms() as i64);
        let sig = okx_sign(&self.secret, &ts, method, path, body);
        let mut req = self
            .http
            .request(
                if method == "GET" { reqwest::Method::GET } else { reqwest::Method::POST },
                format!("{}{}", self.base, path),
            )
            .header("OK-ACCESS-KEY", &self.key)
            .header("OK-ACCESS-SIGN", sig)
            .header("OK-ACCESS-TIMESTAMP", ts)
            .header("OK-ACCESS-PASSPHRASE", &self.passphrase);
        if method != "GET" {
            req = req.header("Content-Type", "application/json").body(body.to_string());
        }
        let v: Value = req.send().await?.error_for_status()?.json().await?;
        // OKX wraps errors per-item: top-level code 0, then data[0].sCode.
        let top = v["code"].as_str().unwrap_or("1");
        if top != "0" {
            anyhow::bail!("okx: {} ({})", v["msg"].as_str().unwrap_or("?"), top);
        }
        if let Some(d) = v["data"].as_array() {
            if let Some(first) = d.first() {
                let sc = first["sCode"].as_str().unwrap_or("0");
                if sc != "0" {
                    anyhow::bail!("okx item: {} ({})", first["sMsg"].as_str().unwrap_or("?"), sc);
                }
            }
        }
        Ok(v["data"].clone())
    }

    pub async fn ping(&self) -> anyhow::Result<()> {
        let url = format!("{}/api/v5/public/time", self.base);
        let v: Value = self.http.get(&url).send().await?.error_for_status()?.json().await?;
        if v["code"].as_str() == Some("0") {
            Ok(())
        } else {
            anyhow::bail!("okx ping failed")
        }
    }

    pub async fn balance(&self, ccy: &str) -> anyhow::Result<f64> {
        let path = format!("/api/v5/account/balance?ccy={ccy}");
        let data = self.request("GET", &path, "").await?;
        let d = data.as_array().and_then(|a| a.first()).cloned().unwrap_or(Value::Null);
        for c in d["details"].as_array().cloned().unwrap_or_default() {
            if c["ccy"].as_str() == Some(ccy) {
                return Ok(c["eqUsd"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0.0));
            }
        }
        Ok(0.0)
    }

    /// Place an options IOC limit order and resolve the fill by polling.
    pub async fn take_option(
        &self,
        inst_id: &str,
        side: Side,
        sz_contracts: f64,
        px_unit: f64,
        taker_rate: f64,
        spot: f64,
    ) -> Fill {
        let body = serde_json::json!({
            "instId": inst_id,
            "tdMode": "cross",
            "side": match side { Side::Buy => "buy", Side::Sell => "sell" },
            "ordType": "ioc",
            "sz": format!("{}", (sz_contracts * 1e6).round() / 1e6),
            "px": format!("{}", (px_unit * 100.0).round() / 100.0),
        })
        .to_string();
        let data = match self.request("POST", "/api/v5/trade/order", &body).await {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!(error = %e, inst_id, "okx order rejected");
                return Fill::zero();
            }
        };
        let ord_id = data.as_array()
            .and_then(|a| a.first())
            .and_then(|d| d["ordId"].as_str())
            .unwrap_or_default()
            .to_string();
        if ord_id.is_empty() {
            return Fill::zero();
        }
        let path = format!("/api/v5/trade/order?instId={inst_id}&ordId={ord_id}");
        for _ in 0..3 {
            tokio::time::sleep(Duration::from_millis(140)).await;
            if let Ok(d) = self.request("GET", &path, "").await {
                if let Some(o) = d.as_array().and_then(|a| a.first()) {
                    let filled = o["accFillSz"].as_str().and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0);
                    if filled > 0.0 {
                        return parse_okx_fill(o, taker_rate, spot);
                    }
                }
            }
        }
        Fill::zero()
    }

    /// SWAP perp hedge (Market). `inst_id` e.g. `BTC-USDT-SWAP`; sz in contracts.
    pub async fn hedge_perp(&self, inst_id: &str, side: Side, sz: f64) -> anyhow::Result<(f64, f64)> {
        let body = serde_json::json!({
            "instId": inst_id,
            "tdMode": "cross",
            "side": match side { Side::Buy => "buy", Side::Sell => "sell" },
            "ordType": "market",
            "sz": format!("{}", (sz * 1e6).round() / 1e6),
        })
        .to_string();
        let data = self.request("POST", "/api/v5/trade/order", &body).await?;
        let ord_id = data.as_array()
            .and_then(|a| a.first())
            .and_then(|d| d["ordId"].as_str())
            .unwrap_or_default()
            .to_string();
        let mut px = 0.0;
        let mut fee = 0.0;
        if !ord_id.is_empty() {
            let path = format!("/api/v5/trade/order?instId={inst_id}&ordId={ord_id}");
            for _ in 0..3 {
                tokio::time::sleep(Duration::from_millis(140)).await;
                if let Ok(d) = self.request("GET", &path, "").await {
                    if let Some(o) = d.as_array().and_then(|a| a.first()) {
                        px = o["avgPx"].as_str().and_then(|s| s.parse().ok()).unwrap_or(0.0);
                        fee = o["fee"].as_str().and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0).abs();
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

fn parse_okx_fill(order: &Value, taker_rate: f64, spot: f64) -> Fill {
    let contracts = order["accFillSz"].as_str().and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0);
    let avg_px = order["avgPx"].as_str().and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0);
    if contracts <= 0.0 || avg_px <= 0.0 {
        return Fill::zero();
    }
    // OKX option fee is in the settlement coin (USDT for USD-margined options).
    let fee_reported = order["fee"].as_str().and_then(|s| s.parse::<f64>().ok()).unwrap_or(0.0).abs();
    let fee_usd = if fee_reported > 0.0 { fee_reported } else { taker_rate * spot * contracts };
    Fill { contracts, px_unit: avg_px, fee_usd, latency_ms: 0 }
}

#[cfg(test)]
pub(crate) fn test_parse_fill(order: &Value, rate: f64, spot: f64) -> Fill {
    parse_okx_fill(order, rate, spot)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn okx_fill_from_recorded_shape() {
        let v = serde_json::json!({
            "instId": "BTC-USD-260926-85000-C",
            "state": "filled",
            "accFillSz": "2",
            "avgPx": "1845.6",
            "fee": "-1.1",
        });
        let f = test_parse_fill(&v, 0.0003, 100_000.0);
        assert_eq!(f.contracts, 2.0);
        assert!((f.px_unit - 1845.6).abs() < 1e-9);
        assert!((f.fee_usd - 1.1).abs() < 1e-9); // negative reported fee → abs
    }

    #[test]
    fn zero_when_live_partial_zero() {
        let v = serde_json::json!({ "state": "live", "accFillSz": "0", "avgPx": "" });
        assert_eq!(test_parse_fill(&v, 0.0003, 1.0).contracts, 0.0);
    }
}
