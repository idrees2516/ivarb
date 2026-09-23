//! Deribit channel-variant probe.

use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let variants: Vec<String> = vec![
        "ticker.BTC".into(),
        "ticker.BTC.1000ms".into(),
        "ticker.BTC.option.1000ms".into(),
        "ticker.BTC-24SEP26-65000-C.1000ms".into(),
        "ticker.BTC-PERPETUAL.raw".into(),
    ];
    for chans in &variants {
        let (ws, _) = tokio_tungstenite::connect_async("wss://www.deribit.com/ws/api/v2").await?;
        let (mut sink, mut stream) = ws.split();
        let req = serde_json::json!({
            "jsonrpc":"2.0","id":1,"method":"public/subscribe",
            "params":{"channels":[chans]}
        });
        sink.send(Message::Text(req.to_string())).await?;
        let mut out = String::new();
        for _ in 0..4 {
            match tokio::time::timeout(std::time::Duration::from_secs(3), stream.next()).await {
                Ok(Some(Ok(Message::Text(t)))) => {
                    out.push_str(&format!("  {}\n", &t[..t.len().min(220)]));
                    if t.contains("\"data\"") {
                        break;
                    }
                }
                _ => break,
            }
        }
        println!("== {chans}:\n{out}");
        let _ = sink.close().await;
    }
    Ok(())
}
