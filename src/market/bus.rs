use tokio::sync::broadcast;
use tracing::{info, warn};

use super::candle_aggregator::CandleAggregator;
use crate::exchange::bitflyer::ws::WsMessage;
use crate::storage::db::Database;
use crate::types::market::Candle;

pub struct MarketDataBus {
    candle_tx: broadcast::Sender<Candle>,
}

impl MarketDataBus {
    /// WS を統合してキャンドルを broadcast する。
    ///
    /// - `ws_tx`: bitFlyer WS タスクが publish している broadcast sender
    /// - `db`: CandleRepository への保存に使用
    pub async fn start(
        ws_tx: broadcast::Sender<WsMessage>,
        db: Database,
        product_code: &str,
        resolution_secs: u32,
    ) -> Self {
        let (candle_tx, _) = broadcast::channel::<Candle>(512);

        // WS task: executions → Candle 集計
        {
            let tx = candle_tx.clone();
            let pc = product_code.to_string();
            let mut ws_rx = ws_tx.subscribe();
            tokio::spawn(async move {
                let mut agg = CandleAggregator::new(pc, resolution_secs);
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
                        }
                        Err(broadcast::error::RecvError::Lagged(n)) => {
                            warn!("MarketDataBus WS lagged by {}", n);
                        }
                        Err(broadcast::error::RecvError::Closed) => break,
                    }
                }
            });
        }

        // DB 保存 task: candle_tx を購読して永続化
        {
            let mut candle_rx = candle_tx.subscribe();
            tokio::spawn(async move {
                loop {
                    match candle_rx.recv().await {
                        Ok(candle) => {
                            if let Err(e) = db.candles().upsert(&candle).await {
                                warn!("Failed to upsert candle: {}", e);
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(n)) => {
                            warn!("MarketDataBus DB task lagged by {}", n);
                        }
                        Err(broadcast::error::RecvError::Closed) => break,
                    }
                }
            });
        }

        info!("MarketDataBus started (product={}, resolution={}s)", product_code, resolution_secs);
        Self { candle_tx }
    }

    pub fn candle_rx(&self) -> broadcast::Receiver<Candle> {
        self.candle_tx.subscribe()
    }

    pub fn candle_tx(&self) -> broadcast::Sender<Candle> {
        self.candle_tx.clone()
    }
}
