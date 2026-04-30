use std::sync::Arc;

use tokio::sync::broadcast;
use tracing::{info, warn};

use rust_decimal::Decimal;

use crate::exchange::ExchangeClient;
use crate::risk::manager::RiskManager;
use crate::risk::{RiskDecision, RiskParams};
use crate::signal::Signal;
use crate::types::order::{OrderRequest, OrderSide, OrderType};

pub struct TradingEngine {
    signal_rx: broadcast::Receiver<Signal>,
    exchange: Arc<dyn ExchangeClient>,
    risk_manager: RiskManager,
    product_code: String,
}

impl TradingEngine {
    pub fn new(
        signal_rx: broadcast::Receiver<Signal>,
        exchange: Arc<dyn ExchangeClient>,
        risk_manager: RiskManager,
        product_code: String,
    ) -> Self {
        Self { signal_rx, exchange, risk_manager, product_code }
    }

    pub async fn run(mut self) {
        loop {
            match self.signal_rx.recv().await {
                Ok(signal) => {
                    if let Err(e) = self.handle_signal(signal).await {
                        warn!("TradingEngine error: {}", e);
                    }
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    warn!("TradingEngine lagged by {} signals", n);
                }
                Err(broadcast::error::RecvError::Closed) => {
                    info!("TradingEngine signal channel closed, shutting down");
                    break;
                }
            }
        }
    }

    async fn handle_signal(&mut self, signal: Signal) -> crate::error::Result<()> {
        match signal {
            Signal::Hold => return Ok(()),
            Signal::Buy { .. } | Signal::Sell { .. } => {}
        }

        // 現在の残高・ポジションを取得
        let balances = self.exchange.get_balance().await?;
        let jpy_balance = balances.iter()
            .find(|b| b.currency_code == "JPY")
            .map(|b| b.available)
            .unwrap_or(Decimal::ZERO);

        let positions = self.exchange.get_positions(&self.product_code).await?;
        let btc_position: Decimal = positions.iter().map(|p| p.size).sum();

        let order_req = self.signal_to_order(&signal, jpy_balance);
        let order_req = match order_req {
            Some(r) => r,
            None => return Ok(()),
        };

        match self.risk_manager.evaluate(order_req, btc_position, jpy_balance) {
            RiskDecision::Allow(req) => {
                let acceptance_id = self.exchange.send_order(&req).await?;
                info!(
                    side = ?req.side,
                    size = %req.size,
                    acceptance_id,
                    "Order placed"
                );
            }
            RiskDecision::Reject(reason) => {
                warn!(reason, "Order rejected by risk manager");
            }
        }

        Ok(())
    }

    fn signal_to_order(&self, signal: &Signal, jpy_balance: Decimal) -> Option<OrderRequest> {
        let min_size = self.risk_manager.params().min_order_size;

        match signal {
            Signal::Buy { .. } => {
                // 利用可能 JPY の 10% を BTC 購入に充てる（シンプルなポジションサイジング）
                // 実際の価格は Market 注文なので price=None
                // サイズは min_order_size を使用（安全側）
                let _ = jpy_balance;
                Some(OrderRequest {
                    product_code: self.product_code.clone(),
                    side: OrderSide::Buy,
                    order_type: OrderType::Market,
                    price: None,
                    size: min_size,
                    minute_to_expire: None,
                    time_in_force: None,
                })
            }
            Signal::Sell { .. } => {
                Some(OrderRequest {
                    product_code: self.product_code.clone(),
                    side: OrderSide::Sell,
                    order_type: OrderType::Market,
                    price: None,
                    size: min_size,
                    minute_to_expire: None,
                    time_in_force: None,
                })
            }
            Signal::Hold => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exchange::mock::MockExchangeClient;
    use crate::risk::{RiskManager, RiskParams};
    use crate::signal::Signal;
    use rust_decimal_macros::dec;

    fn make_engine(signal_rx: broadcast::Receiver<Signal>) -> TradingEngine {
        let mock = Arc::new(MockExchangeClient::new());
        let params = RiskParams {
            max_position_btc: dec!(0.1),
            max_daily_drawdown: 0.05,
            stop_loss_pct: 0.02,
            min_order_size: dec!(0.001),
        };
        TradingEngine::new(signal_rx, mock, RiskManager::new(params), "BTC_JPY".into())
    }

    #[tokio::test]
    async fn hold_signal_does_not_place_order() {
        let (tx, rx) = broadcast::channel::<Signal>(16);
        let engine = make_engine(rx);
        let mock_exchange = Arc::new(MockExchangeClient::new());
        let params = RiskParams::default();
        let mut engine = TradingEngine::new(
            tx.subscribe(),
            mock_exchange.clone(),
            RiskManager::new(params),
            "BTC_JPY".into(),
        );

        tx.send(Signal::Hold).unwrap();
        drop(tx); // channel を閉じて run() が終了するようにする

        engine.run().await;
        assert!(mock_exchange.placed_orders().is_empty());
    }

    #[tokio::test]
    async fn buy_signal_places_order() {
        let mock_exchange = Arc::new(MockExchangeClient::new());
        let (tx, _) = broadcast::channel::<Signal>(16);
        let params = RiskParams::default();
        let mut engine = TradingEngine::new(
            tx.subscribe(),
            mock_exchange.clone(),
            RiskManager::new(params),
            "BTC_JPY".into(),
        );

        tx.send(Signal::Buy { price: dec!(9000000), confidence: 0.8 }).unwrap();
        drop(tx);

        engine.run().await;
        assert_eq!(mock_exchange.placed_orders().len(), 1);
        assert_eq!(mock_exchange.placed_orders()[0].side, OrderSide::Buy);
    }

    #[tokio::test]
    async fn sell_signal_places_order() {
        let mock_exchange = Arc::new(MockExchangeClient::new());
        let (tx, _) = broadcast::channel::<Signal>(16);
        let params = RiskParams::default();
        let mut engine = TradingEngine::new(
            tx.subscribe(),
            mock_exchange.clone(),
            RiskManager::new(params),
            "BTC_JPY".into(),
        );

        tx.send(Signal::Sell { price: dec!(9000000), confidence: 0.9 }).unwrap();
        drop(tx);

        engine.run().await;
        assert_eq!(mock_exchange.placed_orders().len(), 1);
        assert_eq!(mock_exchange.placed_orders()[0].side, OrderSide::Sell);
    }
}
