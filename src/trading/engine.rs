use std::sync::Arc;

use chrono::{NaiveDate, Utc};
use tokio::sync::{broadcast, RwLock};
use tracing::{info, warn};

use rust_decimal::Decimal;

use crate::alert::AlertManager;
use crate::config::TradingConfig;
use crate::exchange::ExchangeClient;
use crate::risk::manager::RiskManager;
use crate::risk::RiskDecision;
use crate::signal::{Signal, SignalDetail};
use crate::storage::orders::OrderRepository;
use crate::types::order::{Order, OrderRequest, OrderSide, OrderStatus, OrderType};

pub struct TradingEngine {
    signal_rx: broadcast::Receiver<SignalDetail>,
    exchange: Arc<dyn ExchangeClient>,
    risk_manager: RiskManager,
    product_code: String,
    alert: Arc<AlertManager>,
    /// 最後に日次リセットした UTC 日付
    last_reset_date: Option<NaiveDate>,
    /// 取引設定（UIから動的変更可能）
    config: Arc<RwLock<TradingConfig>>,
    /// 注文をDBに永続化するリポジトリ（オプション）
    order_repo: Option<OrderRepository>,
}

impl TradingEngine {
    pub fn new(
        signal_rx: broadcast::Receiver<SignalDetail>,
        exchange: Arc<dyn ExchangeClient>,
        risk_manager: RiskManager,
        product_code: String,
    ) -> Self {
        Self {
            signal_rx,
            exchange,
            risk_manager,
            product_code,
            alert: Arc::new(AlertManager::new()),
            last_reset_date: None,
            config: Arc::new(RwLock::new(TradingConfig::default())),
            order_repo: None,
        }
    }

    pub fn with_order_repo(mut self, repo: OrderRepository) -> Self {
        self.order_repo = Some(repo);
        self
    }

    pub fn with_config(mut self, config: Arc<RwLock<TradingConfig>>) -> Self {
        self.config = config;
        self
    }

    pub fn with_alert(mut self, alert: Arc<AlertManager>) -> Self {
        self.alert = alert;
        self
    }

    pub async fn run(mut self) {
        loop {
            match self.signal_rx.recv().await {
                Ok(detail) => {
                    if let Err(e) = self.handle_signal(detail.aggregate).await {
                        warn!("TradingEngine error: {}", e);
                        self.alert.order_error(&e.to_string()).await;
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

        // 最新の設定を risk_manager に反映
        {
            let cfg = self.config.read().await;
            self.risk_manager.update_params(cfg.to_risk_params());
        }

        // 現在の残高・ポジションを取得
        let balances = self.exchange.get_balance().await?;
        let jpy_balance = balances.iter()
            .find(|b| b.currency_code == "JPY")
            .map(|b| b.available)
            .unwrap_or(Decimal::ZERO);

        // 日次リセット（UTC 00:00 をまたいだ場合）
        let today = Utc::now().date_naive();
        if self.last_reset_date != Some(today) {
            self.risk_manager.reset_daily(jpy_balance);
            self.last_reset_date = Some(today);
            info!("RiskManager daily reset: baseline_jpy={}", jpy_balance);
        }

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
                if let Some(repo) = &self.order_repo {
                    let now = Utc::now();
                    let order = Order {
                        id: None,
                        acceptance_id: acceptance_id.clone(),
                        product_code: self.product_code.clone(),
                        side: req.side.clone(),
                        order_type: req.order_type.clone(),
                        price: req.price,
                        size: req.size,
                        status: OrderStatus::Completed,
                        created_at: now,
                        updated_at: now,
                    };
                    if let Err(e) = repo.upsert(&order).await {
                        warn!("Failed to persist order to DB: {}", e);
                    }
                }
            }
            RiskDecision::Reject(reason) => {
                warn!(reason, "Order rejected by risk manager");
            }
            RiskDecision::CircuitBreaker { drawdown_pct } => {
                warn!(drawdown_pct, "Circuit breaker triggered");
                self.alert.circuit_breaker_triggered(drawdown_pct).await;
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
    use crate::signal::{Signal, SignalDetail};
    use rust_decimal_macros::dec;

    fn detail(signal: Signal) -> SignalDetail {
        SignalDetail { aggregate: signal, indicators: vec![] }
    }

    fn make_engine(signal_rx: broadcast::Receiver<SignalDetail>) -> TradingEngine {
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
        let (tx, rx) = broadcast::channel::<SignalDetail>(16);
        let _engine = make_engine(rx);
        let mock_exchange = Arc::new(MockExchangeClient::new());
        let params = RiskParams::default();
        let mut engine = TradingEngine::new(
            tx.subscribe(),
            mock_exchange.clone(),
            RiskManager::new(params),
            "BTC_JPY".into(),
        );

        tx.send(detail(Signal::Hold)).unwrap();
        drop(tx); // channel を閉じて run() が終了するようにする

        engine.run().await;
        assert!(mock_exchange.placed_orders().is_empty());
    }

    #[tokio::test]
    async fn buy_signal_places_order() {
        let mock_exchange = Arc::new(MockExchangeClient::new());
        let (tx, _) = broadcast::channel::<SignalDetail>(16);
        let params = RiskParams::default();
        let mut engine = TradingEngine::new(
            tx.subscribe(),
            mock_exchange.clone(),
            RiskManager::new(params),
            "BTC_JPY".into(),
        );

        tx.send(detail(Signal::Buy { price: dec!(9000000), confidence: 0.8 })).unwrap();
        drop(tx);

        engine.run().await;
        assert_eq!(mock_exchange.placed_orders().len(), 1);
        assert_eq!(mock_exchange.placed_orders()[0].side, OrderSide::Buy);
    }

    #[tokio::test]
    async fn sell_signal_places_order() {
        let mock_exchange = Arc::new(MockExchangeClient::new());
        // Sell注文が通るようにBTC残高を事前に設定
        mock_exchange.set_balances(vec![
            crate::types::balance::Balance {
                currency_code: "JPY".to_string(),
                amount: dec!(1_000_000),
                available: dec!(1_000_000),
            },
            crate::types::balance::Balance {
                currency_code: "BTC".to_string(),
                amount: dec!(0.1),
                available: dec!(0.1),
            },
        ]);
        let (tx, _) = broadcast::channel::<SignalDetail>(16);
        let params = RiskParams::default();
        let mut engine = TradingEngine::new(
            tx.subscribe(),
            mock_exchange.clone(),
            RiskManager::new(params),
            "BTC_JPY".into(),
        );

        tx.send(detail(Signal::Sell { price: dec!(9000000), confidence: 0.9 })).unwrap();
        drop(tx);

        engine.run().await;
        assert_eq!(mock_exchange.placed_orders().len(), 1);
        assert_eq!(mock_exchange.placed_orders()[0].side, OrderSide::Sell);
    }
}
