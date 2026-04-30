use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{debug, info, warn};

use super::models::{RawExecution, RawTicker};
use crate::error::{Error, Result};
use crate::types::{market::Trade, market::Ticker};

const DEFAULT_WS_ENDPOINT: &str = "wss://ws.bitflyer.com/v1";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum WsMessage {
    Ticker(Ticker),
    Executions(Vec<Trade>),
}

#[derive(Debug, Serialize)]
struct JsonRpcRequest<P: Serialize> {
    jsonrpc: &'static str,
    method: &'static str,
    params: P,
    id: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct JsonRpcNotification {
    #[allow(dead_code)]
    jsonrpc: Option<String>,
    method: Option<String>,
    params: Option<ChannelMessage>,
}

#[derive(Debug, Deserialize)]
struct ChannelMessage {
    channel: String,
    message: serde_json::Value,
}

#[derive(Debug, Serialize)]
struct SubscribeParams {
    channel: String,
}

pub struct BitFlyerWsClient {
    endpoint: String,
    #[allow(dead_code)]
    api_key: String,
    #[allow(dead_code)]
    api_secret: String,
}

impl BitFlyerWsClient {
    pub fn new(api_key: String, api_secret: String) -> Self {
        Self::new_with_endpoint(api_key, api_secret, DEFAULT_WS_ENDPOINT.to_string())
    }

    pub fn new_with_endpoint(api_key: String, api_secret: String, endpoint: String) -> Self {
        Self {
            endpoint,
            api_key,
            api_secret,
        }
    }

    pub async fn run(
        self,
        channels: Vec<String>,
        tx: broadcast::Sender<WsMessage>,
    ) -> Result<()> {
        info!("Connecting to WebSocket: {}", self.endpoint);
        let (ws_stream, _) = connect_async(self.endpoint.as_str()).await?;
        info!("WebSocket connected");

        let (mut write, mut read) = ws_stream.split();

        // Subscribe to channels
        for channel in &channels {
            let req = JsonRpcRequest {
                jsonrpc: "2.0",
                method: "subscribe",
                params: SubscribeParams {
                    channel: channel.clone(),
                },
                id: None,
            };
            let msg = serde_json::to_string(&req)?;
            write.send(Message::Text(msg)).await?;
            debug!("Subscribed to channel: {}", channel);
        }

        while let Some(msg) = read.next().await {
            match msg {
                Ok(Message::Text(text)) => {
                    match serde_json::from_str::<JsonRpcNotification>(&text) {
                        Ok(notif) => {
                            if notif.method.as_deref() == Some("channelMessage") {
                                if let Some(cm) = notif.params {
                                    self.dispatch_channel_message(cm, &tx);
                                }
                            }
                        }
                        Err(e) => {
                            debug!("Failed to parse WS message: {} | raw: {}", e, &text[..text.len().min(200)]);
                        }
                    }
                }
                Ok(Message::Ping(data)) => {
                    write.send(Message::Pong(data)).await.ok();
                }
                Ok(Message::Close(_)) => {
                    info!("WebSocket closed by server");
                    break;
                }
                Ok(_) => {}
                Err(e) => {
                    warn!("WebSocket error: {}", e);
                    return Err(Error::WebSocket(e));
                }
            }
        }
        Ok(())
    }

    fn dispatch_channel_message(
        &self,
        cm: ChannelMessage,
        tx: &broadcast::Sender<WsMessage>,
    ) {
        if cm.channel.starts_with("lightning_ticker_") {
            match serde_json::from_value::<RawTicker>(cm.message) {
                Ok(raw) => {
                    let _ = tx.send(WsMessage::Ticker(raw.into()));
                }
                Err(e) => {
                    debug!("Failed to parse ticker: {}", e);
                }
            }
        } else if cm.channel.starts_with("lightning_executions_") {
            match serde_json::from_value::<Vec<RawExecution>>(cm.message) {
                Ok(raw) => {
                    let trades: Vec<Trade> = raw.into_iter().map(Into::into).collect();
                    let _ = tx.send(WsMessage::Executions(trades));
                }
                Err(e) => {
                    debug!("Failed to parse executions: {}", e);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;
    use tokio_tungstenite::accept_async;

    async fn make_ws_server(messages: Vec<String>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut ws = accept_async(stream).await.unwrap();
            // consume subscribe messages
            for _ in 0..1 {
                ws.next().await;
            }
            for msg in messages {
                ws.send(Message::Text(msg)).await.unwrap();
            }
        });
        format!("ws://{}", addr)
    }

    #[tokio::test]
    async fn receives_ticker_message() {
        let ticker_json = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "channelMessage",
            "params": {
                "channel": "lightning_ticker_BTC_JPY",
                "message": {
                    "product_code": "BTC_JPY",
                    "timestamp": "2024-01-01T00:00:00.000Z",
                    "best_bid": "9000000",
                    "best_ask": "9001000",
                    "best_bid_size": "0.1",
                    "best_ask_size": "0.2",
                    "ltp": "9000500",
                    "volume": "1234.5",
                    "volume_by_product": "1234.5"
                }
            }
        });
        let endpoint = make_ws_server(vec![ticker_json.to_string()]).await;

        let (tx, mut rx) = broadcast::channel(16);
        let client = BitFlyerWsClient::new_with_endpoint(
            String::new(),
            String::new(),
            endpoint,
        );
        tokio::spawn(async move {
            client
                .run(vec!["lightning_ticker_BTC_JPY".to_string()], tx)
                .await
                .ok();
        });

        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            let msg = rx.recv().await.unwrap();
            assert!(matches!(msg, WsMessage::Ticker(_)));
        })
        .await
        .expect("Timeout waiting for WS message");
    }
}
