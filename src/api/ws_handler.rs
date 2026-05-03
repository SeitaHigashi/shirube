use axum::{
    extract::{
        ws::{Message, WebSocket},
        State, WebSocketUpgrade,
    },
    response::Response,
};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::broadcast::error::RecvError;
use tracing::{info, warn};

use crate::api::AppState;

pub async fn ws_tickers(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
) -> Response {
    ws.on_upgrade(move |socket| handle_ticker_socket(socket, state))
}

async fn handle_ticker_socket(socket: WebSocket, state: AppState) {
    let mut rx = state.ticker_tx.subscribe();
    let (mut sender, mut receiver) = socket.split();

    let mut recv_task = tokio::spawn(async move {
        while let Some(msg) = receiver.next().await {
            match msg {
                Ok(Message::Close(_)) | Err(_) => break,
                _ => {}
            }
        }
    });

    loop {
        tokio::select! {
            result = rx.recv() => {
                match result {
                    Ok(ticker) => {
                        let json = match serde_json::to_string(&ticker) {
                            Ok(s) => s,
                            Err(e) => {
                                warn!("Failed to serialize ticker: {}", e);
                                continue;
                            }
                        };
                        if sender.send(Message::Text(json.into())).await.is_err() {
                            break;
                        }
                    }
                    Err(RecvError::Lagged(n)) => {
                        warn!("WS ticker stream lagged by {}", n);
                    }
                    Err(RecvError::Closed) => {
                        info!("Ticker broadcast channel closed");
                        break;
                    }
                }
            }
            _ = &mut recv_task => break,
        }
    }
}

pub async fn ws_candles(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
) -> Response {
    ws.on_upgrade(move |socket| handle_candle_socket(socket, state))
}

async fn handle_candle_socket(socket: WebSocket, state: AppState) {
    let mut rx = state.candle_tx.subscribe();
    let (mut sender, mut receiver) = socket.split();

    // クライアントからのメッセージを受け取るタスク（ping-pong / close 検出）
    let mut recv_task = tokio::spawn(async move {
        while let Some(msg) = receiver.next().await {
            match msg {
                Ok(Message::Close(_)) | Err(_) => break,
                _ => {}
            }
        }
    });

    // Candle が来たら JSON で Push するループ
    loop {
        tokio::select! {
            result = rx.recv() => {
                match result {
                    Ok(candle) => {
                        let json = match serde_json::to_string(&candle) {
                            Ok(s) => s,
                            Err(e) => {
                                warn!("Failed to serialize candle: {}", e);
                                continue;
                            }
                        };
                        if sender.send(Message::Text(json.into())).await.is_err() {
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        warn!("WS candle stream lagged by {}", n);
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        info!("Candle broadcast channel closed");
                        break;
                    }
                }
            }
            _ = &mut recv_task => break,
        }
    }
}
