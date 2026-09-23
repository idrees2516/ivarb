//! Bybit WS probe: infer the option contract size from an ITM mark price.

use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let (ws, _) = tokio_tungstenite::connect_async("wss://stream.bybit.com/v5/public/option").await?;
    let (mut sink, mut stream) = ws.split();
    let req = serde_json::json!({
        "op": "subscribe",
        "args": ["tickers.BTC-26SEP26-75000-C", "tickers.BTC-26SEP26-70000-P"]
    });
    sink.send(Message::Text(req.to_string())).await?;
    for _ in 0..12 {
        match tokio::time::timeout(std::time::Duration::from_secs(4), stream.next()).await {
            Ok(Some(Ok(Message::Text(t)))) => {
                if t.contains("topic") {
                    println!("{t}");
                }
            }
            _ => break,
        }
    }
    Ok(())
}
