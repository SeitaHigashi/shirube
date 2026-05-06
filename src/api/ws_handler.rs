use std::sync::Arc;

use axum::{
    extract::{
        ws::{Message, WebSocket},
        Query, State, WebSocketUpgrade,
    },
    response::Response,
};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio::sync::broadcast::{self, error::RecvError};
use tracing::{info, warn};

use crate::api::AppState;
use crate::exchange::bitflyer::ws::WsMessage;
use crate::market::candle_aggregator::build_seeded_aggregator;
use crate::types::market::Candle;

/// WS /ws/candles のクエリパラメータ
#[derive(Deserialize)]
pub struct CandleQuery {
    /// ローソク足の解像度（秒）。省略時は 60（1分足）
    pub resolution: Option<u32>,
}

/// AggregatorRegistry からエントリを取得、または新規作成する。
///
/// - 既存の Arc<Sender> が生きていれば subscribe() して返す
/// - 死んでいれば（全接続が切れていれば）新規 CandleAggregator タスクを spawn する
///
/// 返却した Arc<Sender> を WS接続が保持することで、接続中は Sender が生存し続ける。
/// 最後の接続が Arc を drop した時点で Sender が自動 drop され、
/// タスク内の Weak::upgrade() が None を返してタスクが自動終了する。
async fn get_aggregator(
    state: &AppState,
    resolution_secs: u32,
) -> (Arc<broadcast::Sender<Candle>>, broadcast::Receiver<Candle>) {
    let mut registry = state.aggregator_registry.lock().await;

    // 既存エントリの Weak が生きているか確認
    if let Some(weak) = registry.get(&resolution_secs) {
        if let Some(strong) = weak.upgrade() {
            let rx = strong.subscribe();
            return (strong, rx);
        }
    }

    // 新規: broadcast チャンネルと Arc<Sender> を作成してレジストリに登録
    let (tx, rx) = broadcast::channel::<Candle>(256);
    let arc_tx = Arc::new(tx);
    registry.insert(resolution_secs, Arc::downgrade(&arc_tx));

    // バックグラウンドタスク: Weak<Sender> を持ち、upgrade 失敗時に自動終了
    // NOTE: ticker_tx (ltp のみ) ではなく ws_tx (Executions + Ticker) を購読することで
    // MarketDataBus と同じ方式で個別約定価格から正確な high/low を計算する
    let weak_tx = Arc::downgrade(&arc_tx);
    let mut ws_rx = state.ws_tx.subscribe();
    // NOTE: state.db を clone して spawn 内で pre-seed に使用する
    let db_clone = state.db.clone();
    let pc = state.product_code.clone();
    tokio::spawn(async move {
        // 起動時バケット内の保存済み Ticker を DB から取得して pre-seed し、
        // 接続タイミングがバケット途中でも OHLCV が欠損しないようにする。
        let mut agg = build_seeded_aggregator(&pc, resolution_secs, &db_clone).await;
        info!("CandleAggregator task started (resolution={}s)", resolution_secs);
        loop {
            // 全接続が切れた（Arc refcount=0）ならタスク終了
            let Some(tx) = weak_tx.upgrade() else {
                info!("CandleAggregator task stopping (resolution={}s, no active connections)", resolution_secs);
                break;
            };

            match ws_rx.recv().await {
                Ok(WsMessage::Executions(trades)) => {
                    // 個別約定価格で high/low を正確に更新（MarketDataBus と同じ処理）
                    for candle in agg.feed_trades(&trades) {
                        let _ = tx.send(candle);
                    }
                    if let Some(partial) = agg.peek_current() {
                        let _ = tx.send(partial);
                    }
                    drop(tx);
                }
                Ok(WsMessage::Ticker(ticker)) => {
                    // バケット確定トリガー + partial candle 更新
                    if let Some(candle) = agg.feed_ticker(&ticker) {
                        let _ = tx.send(candle);
                    }
                    if let Some(partial) = agg.peek_current() {
                        let _ = tx.send(partial);
                    }
                    drop(tx);
                }
                Err(RecvError::Lagged(n)) => {
                    warn!("CandleAggregator ws stream lagged by {} (resolution={}s)", n, resolution_secs);
                    drop(tx);
                }
                Err(RecvError::Closed) => {
                    info!("WS broadcast channel closed (resolution={}s)", resolution_secs);
                    break;
                }
            }
        }
        // agg はここで自動 drop
    });

    (arc_tx, rx)
}

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

pub async fn ws_signal(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
) -> Response {
    ws.on_upgrade(move |socket| handle_signal_socket(socket, state))
}

async fn handle_signal_socket(socket: WebSocket, state: AppState) {
    let mut rx = state.signal_tx.subscribe();
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
                    Ok(detail) => {
                        let json = match serde_json::to_string(&detail) {
                            Ok(s) => s,
                            Err(e) => {
                                warn!("Failed to serialize signal: {}", e);
                                continue;
                            }
                        };
                        if sender.send(Message::Text(json.into())).await.is_err() {
                            break;
                        }
                    }
                    Err(RecvError::Lagged(n)) => {
                        warn!("WS signal stream lagged by {}", n);
                    }
                    Err(RecvError::Closed) => {
                        info!("Signal broadcast channel closed");
                        break;
                    }
                }
            }
            _ = &mut recv_task => break,
        }
    }
}

/// WebSocket エンドポイント: /ws/candles?resolution=<秒>
///
/// resolution パラメータで要求された解像度の CandleAggregator を
/// AggregatorRegistry から取得（または新規作成）してリアルタイムに配信する。
/// Arc<Sender> を保持することで接続中はアグリゲーターが生存し続け、
/// 全接続切断時は Sender が自動 drop されてバックグラウンドタスクも終了する。
pub async fn ws_candles(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
    Query(params): Query<CandleQuery>,
) -> Response {
    let resolution = params.resolution.unwrap_or(60);
    ws.on_upgrade(move |socket| handle_candle_socket(socket, state, resolution))
}

async fn handle_candle_socket(socket: WebSocket, state: AppState, resolution_secs: u32) {
    // Arc<Sender> を受け取ることで接続中は Sender（とタスク）が生存し続ける
    let (arc_sender, mut rx) = get_aggregator(&state, resolution_secs).await;

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
                    Err(RecvError::Lagged(n)) => {
                        warn!("WS candle stream lagged by {} (resolution={}s)", n, resolution_secs);
                    }
                    Err(RecvError::Closed) => {
                        info!("Candle broadcast channel closed (resolution={}s)", resolution_secs);
                        break;
                    }
                }
            }
            _ = &mut recv_task => break,
        }
    }

    // arc_sender がここで drop → 最後の接続なら Sender の refcount=0 → タスク自動終了
    drop(arc_sender);
}
