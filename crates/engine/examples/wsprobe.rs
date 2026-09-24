//! WS subscription-shape probe for OKX and Deribit connectivity.

use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message;

#[tokio::main]
async fn main() {
    // ---- OKX subscription variants ----
    let subs: Vec<(String, serde_json::Value)> = vec![
        (
            "opt-tickers + instId".into(),
            serde_json::json!({"op":"subscribe","args":[{"channel":"opt-tickers","instId":"BTC-USD-260925-75000-C"}]}),
        ),
        (
            "opt-tickers + expiry family".into(),
            serde_json::json!({"op":"subscribe","args":[{"channel":"opt-tickers","instFamily":"BTC-USD-260925"}]}),
        ),
        (
            "tickers + instId".into(),
            serde_json::json!({"op":"subscribe","args":[{"channel":"tickers","instId":"BTC-USD-260925-75000-C"}]}),
        ),
        (
            "tickers + expiry family".into(),
            serde_json::json!({"op":"subscribe","args":[{"channel":"tickers","instFamily":"BTC-USD-260925"}]}),
        ),
        (
            "opt-tickers + uly family".into(),
            serde_json::json!({"op":"subscribe","args":[{"channel":"opt-tickers","instFamily":"BTC-USD-260925-75000-C"}]}),
        ),
    ];
    for (label, req) in subs {
        match tokio::time::timeout(
            std::time::Duration::from_secs(12),
            try_okx(&req),
        )
        .await
        {
            Ok(Ok(logs)) => println!("== {label}:\n{logs}"),
            Ok(Err(e)) => println!("== {label}: ERR {e}"),
            Err(_) => println!("== {label}: TIMEOUT"),
        }
    }

    // ---- Deribit connectivity ----
    println!("== deribit ws:");
    match tokio::time::timeout(std::time::Duration::from_secs(15), try_deribit()).await {
        Ok(Ok(logs)) => println!("{logs}"),
        Ok(Err(e)) => println!("ERR {e}"),
        Err(_) => println!("TIMEOUT"),
    }
}

async fn try_okx(req: &serde_json::Value) -> anyhow::Result<String> {
    let (ws, _) = tokio_tungstenite::connect_async("wss://ws.okx.com:8443/ws/v5/public").await?;
    let (mut sink, mut stream) = ws.split();
    sink.send(Message::Text(req.to_string())).await?;
    sink.send(Message::Text("ping".into())).await?;
    let mut logs = String::new();
    for _ in 0..8 {
        match tokio::time::timeout(std::time::Duration::from_secs(4), stream.next()).await {
            Ok(Some(Ok(Message::Text(t)))) => {
                logs.push_str(&format!("  {t}\n"));
                if t.contains("\"data\"") {
                    break;
                }
            }
            Ok(Some(Ok(_))) => {}
            Ok(Some(Err(e))) => return Err(anyhow::anyhow!("{e}")),
            Ok(None) => break,
            Err(_) => break,
        }
    }
    let _ = sink.close().await;
    Ok(logs)
}

async fn try_deribit() -> anyhow::Result<String> {
    let t0 = std::time::Instant::now();
    let (ws, _) = tokio_tungstenite::connect_async("wss://www.deribit.com/ws/api/v2").await?;
    let connect_ms = t0.elapsed().as_millis();
    let (mut sink, mut stream) = ws.split();
    let req = serde_json::json!({
        "jsonrpc":"2.0","id":1,"method":"public/subscribe",
        "params":{"channels":["ticker.BTC.1000ms"]}
    });
    sink.send(Message::Text(req.to_string())).await?;
    let mut logs = format!("  connect in {connect_ms}ms\n");
    for _ in 0..10 {
        match tokio::time::timeout(std::time::Duration::from_secs(4), stream.next()).await {
            Ok(Some(Ok(Message::Text(t)))) => {
                logs.push_str(&format!("  {}\n", &t[..t.len().min(300)]));
                if t.contains("\"data\"") {
                    logs.push_str("  ... ticker data flowing\n");
                    break;
                }
            }
            Ok(Some(Ok(_))) => {}
            Ok(Some(Err(e))) => return Err(anyhow::anyhow!("{e}")),
            Ok(None) => break,
            Err(_) => {
                logs.push_str("  (no more frames)\n");
                break;
            }
        }
    }
    let _ = sink.close().await;
    Ok(logs)
}
