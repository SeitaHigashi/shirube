use std::time::Duration;

use tokio::sync::broadcast;
use tracing::{info, warn};

use super::candle_aggregator::CandleAggregator;
use crate::exchange::bitflyer::ws::WsMessage;
use crate::storage::db::Database;
use crate::types::market::{Candle, Ticker};

pub struct MarketDataBus {
    candle_tx: broadcast::Sender<Candle>,
    ticker_tx: broadcast::Sender<Ticker>,
}

impl MarketDataBus {
    /// WS を統合してキャンドルを broadcast する。
    ///
    /// - `ws_tx`: bitFlyer WS タスクが publish している broadcast sender
    /// - `db`: CandleRepository / TickerRepository への保存に使用
    pub async fn start(
        ws_tx: broadcast::Sender<WsMessage>,
        db: Database,
        product_code: &str,
        resolution_secs: u32,
    ) -> Self {
        let (candle_tx, _) = broadcast::channel::<Candle>(512);
        let (ticker_tx, _) = broadcast::channel::<Ticker>(64);

        // WS task: executions → Candle 集計 + Ticker 保存
        {
            let tx = candle_tx.clone();
            let ticker_tx_ws = ticker_tx.clone();
            let pc = product_code.to_string();
            let db_ws = db.clone();
            let mut ws_rx = ws_tx.subscribe();
            tokio::spawn(async move {
                let mut agg = CandleAggregator::new(pc.clone(), resolution_secs);
                loop {
                    match ws_rx.recv().await {
                        Ok(WsMessage::Executions(trades)) => {
                            for candle in agg.feed_trades(&trades) {
                                let _ = tx.send(candle);
                            }
                        }
                        Ok(WsMessage::Ticker(ticker)) => {
                            if let Some(candle) = agg.feed_ticker(&ticker) {
                                let _ = tx.send(candle);
                            }
                            // Ticker を全件 DB に保存（スロットルなし）
                            if let Err(e) = db_ws.tickers().insert(&ticker).await {
                                warn!("Failed to insert ticker: {}", e);
                            }
                            let _ = ticker_tx_ws.send(ticker);
                        }
                        Err(broadcast::error::RecvError::Lagged(n)) => {
                            warn!("MarketDataBus WS lagged by {}", n);
                        }
                        Err(broadcast::error::RecvError::Closed) => break,
                    }
                }
            });
        }

        // ダウンサンプルタスク: 起動時に即実行 + 1時間ごと
        {
            let pc = product_code.to_string();
            let db_ds = db.clone();
            tokio::spawn(async move {
                // 起動直後に一度実行（前回停止中に溜まった分を処理）
                match db_ds.tickers().downsample(&pc).await {
                    Ok(s) => info!(
                        "Ticker downsample (startup): -{} tickers",
                        s.tickers_deleted
                    ),
                    Err(e) => warn!("Ticker downsample failed: {}", e),
                }

                let mut iv = tokio::time::interval(Duration::from_secs(3600));
                loop {
                    iv.tick().await;
                    match db_ds.tickers().downsample(&pc).await {
                        Ok(s) => info!(
                            "Ticker downsample: -{} tickers",
                            s.tickers_deleted
                        ),
                        Err(e) => warn!("Ticker downsample failed: {}", e),
                    }
                }
            });
        }

        info!("MarketDataBus started (product={}, resolution={}s)", product_code, resolution_secs);
        Self { candle_tx, ticker_tx }
    }

    pub fn candle_rx(&self) -> broadcast::Receiver<Candle> {
        self.candle_tx.subscribe()
    }

    pub fn candle_tx(&self) -> broadcast::Sender<Candle> {
        self.candle_tx.clone()
    }

    pub fn ticker_tx(&self) -> broadcast::Sender<Ticker> {
        self.ticker_tx.clone()
    }
}
