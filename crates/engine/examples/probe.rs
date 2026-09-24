//! Probe venue REST endpoints with the same reqwest client the engine uses.

use std::error::Error;
use std::time::Duration;

#[tokio::main]
async fn main() {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(45))
        .user_agent("ivarb-scanner/0.1")
        .build()
        .unwrap();
    for url in [
        "https://api.aevo.xyz/markets",
        "https://eapi.binance.com/eapi/v1/mark",
        "https://www.okx.com/api/v5/public/instruments?instType=OPTION&uly=BTC-USD",
        "https://api.delta.exchange/v2/tickers?contract_types=call_options,put_options&states=live",
    ] {
        let t0 = std::time::Instant::now();
        match client.get(url).send().await {
            Ok(resp) => {
                let status = resp.status();
                let hdrs: Vec<String> = resp
                    .headers()
                    .iter()
                    .map(|(k, v)| format!("{}: {}", k, v.to_str().unwrap_or("?")))
                    .collect();
                match resp.bytes().await {
                    Ok(b) => {
                        let len = b.len();
                        let parse = serde_json::from_slice::<serde_json::Value>(&b)
                            .map(|v| v.to_string().len())
                            .map_err(|e| e.to_string());
                        println!(
                            "{url}\n  status={status} bytes={len} elapsed={:?} parse={:?}\n  headers: {}\n",
                            t0.elapsed(),
                            parse,
                            hdrs.join(" | ")
                        );
                    }
                    Err(e) => println!(
                        "{url}\n  body err after {:?}: {e} (source: {:?})\n",
                        t0.elapsed(),
                        e.source()
                    ),
                }
            }
            Err(e) => println!(
                "{url}\n  send err after {:?}: {e} (source: {:?})\n",
                t0.elapsed(),
                e.source()
            ),
        }
    }
}
