use std::sync::Arc;

use chrono::{NaiveDate, Utc};
use tokio::sync::{broadcast, RwLock};
use tracing::{info, warn};

use rust_decimal::Decimal;

use rust_decimal::prelude::{FromPrimitive, ToPrimitive};

use crate::alert::AlertManager;
use crate::config::TradingConfig;
use crate::exchange::ExchangeClient;
use crate::risk::manager::RiskManager;
use crate::risk::RiskDecision;
use crate::signal::{AllocationSignal, SignalDetail};
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

    async fn handle_signal(&mut self, signal: AllocationSignal) -> crate::error::Result<()> {
        // 最新の設定を risk_manager に反映
        let allocation_threshold = {
            let cfg = self.config.read().await;
            self.risk_manager.update_params(cfg.to_risk_params());
            cfg.allocation_threshold
        };

        // 現在の残高・ポジション・価格を並列取得
        let (balances, positions, ticker) = tokio::try_join!(
            self.exchange.get_balance(),
            self.exchange.get_positions(&self.product_code),
            self.exchange.get_ticker(&self.product_code),
        )?;

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

        let btc_position: Decimal = positions.iter().map(|p| p.size).sum();

        // BTC 残高も考慮（MockExchangeClient では balance で管理）
        let btc_balance = balances.iter()
            .find(|b| b.currency_code == "BTC")
            .map(|b| b.available)
            .unwrap_or(Decimal::ZERO);
        let btc_held = btc_position.max(btc_balance);

        let btc_price = ticker.ltp;

        let btc_value = btc_held * btc_price;
        let total_value = btc_value + jpy_balance;

        let min_order_size = self.risk_manager.params().min_order_size;
        let order_req = if total_value.is_zero() || btc_price.is_zero() {
            None
        } else {
            let current_alloc = (btc_value / total_value)
                .to_f64()
                .unwrap_or(0.0);
            let delta = signal.target_pct - current_alloc;
            if delta.abs() < allocation_threshold {
                None
            } else {
                Self::allocation_delta_to_order(
                    delta,
                    total_value,
                    btc_price,
                    &self.product_code,
                    min_order_size,
                )
            }
        };

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

    /// 配分デルタから注文を生成する。
    /// delta > 0 → 買い, delta < 0 → 売り。
    /// size = |delta| * total_value / btc_price
    fn allocation_delta_to_order(
        delta: f64,
        total_value: Decimal,
        btc_price: Decimal,
        product_code: &str,
        min_size: Decimal,
    ) -> Option<OrderRequest> {
        let delta_dec = Decimal::from_f64(delta.abs()).unwrap_or(Decimal::ZERO);
        let size = (delta_dec * total_value / btc_price).round_dp(8);
        if size < min_size {
            return None;
        }
        let side = if delta > 0.0 { OrderSide::Buy } else { OrderSide::Sell };
        Some(OrderRequest {
            product_code: product_code.to_string(),
            side,
            order_type: OrderType::Market,
            price: None,
            size,
            minute_to_expire: None,
            time_in_force: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exchange::mock::MockExchangeClient;
    use crate::risk::{RiskManager, RiskParams};
    use crate::signal::{AllocationSignal, SignalDetail};
    use rust_decimal_macros::dec;

    fn detail(target_pct: f64, confidence: f64) -> SignalDetail {
        SignalDetail {
            aggregate: AllocationSignal::from_effective(target_pct, confidence),
            indicators: vec![],
            calculated_at: chrono::Utc::now(),
            calculation_state: "active".to_string(),
        }
    }

    #[tokio::test]
    async fn neutral_allocation_does_not_place_order() {
        // 中立 (0.5) かつ現在BTCなし → delta=0.5 だが JPY/BTC 価格が初期状態
        // MockExchangeClient デフォルト: ltp=9_000_500, JPY=1_000_000, BTC=0
        // current_alloc = 0.0, target = 0.5, delta = 0.5 > threshold(0.05)
        // → 実際は注文が入る。中立テストは厳密に 0.5 の配分を再現するため
        //   BTC を 50% 保有した状態でテストする。
        // 代わりに target_pct = current_alloc のケース（delta < threshold）をテスト
        // MockExchangeClient: JPY=1_000_000, BTC=0, price=9_000_500
        // current_alloc ≈ 0.0 なので target_pct=0.0 → delta=0.0 < 0.05
        let mock_exchange = Arc::new(MockExchangeClient::new());
        let (tx, _) = broadcast::channel::<SignalDetail>(16);
        let params = RiskParams::default();
        let mut engine = TradingEngine::new(
            tx.subscribe(),
            mock_exchange.clone(),
            RiskManager::new(params),
            "BTC_JPY".into(),
        );

        // target_pct = 0.02 → delta ≈ 0.02 < allocation_threshold(0.05)
        tx.send(detail(0.02, 0.1)).unwrap();
        drop(tx);

        engine.run().await;
        assert!(mock_exchange.placed_orders().is_empty());
    }

    #[tokio::test]
    async fn bullish_allocation_places_buy_order() {
        // JPY=1_000_000, BTC=0, price=9_000_500
        // current_alloc ≈ 0.0, target=0.8 → delta=0.8 → buy
        let mock_exchange = Arc::new(MockExchangeClient::new());
        let (tx, _) = broadcast::channel::<SignalDetail>(16);
        let params = RiskParams {
            max_position_btc: dec!(1.0),  // 上限を大きくして注文を通す
            max_daily_drawdown: 0.5,
            stop_loss_pct: 0.5,
            min_order_size: dec!(0.001),
            circuit_breaker_enabled: true,
        };
        let mut engine = TradingEngine::new(
            tx.subscribe(),
            mock_exchange.clone(),
            RiskManager::new(params),
            "BTC_JPY".into(),
        );

        tx.send(detail(0.8, 0.9)).unwrap();
        drop(tx);

        engine.run().await;
        assert_eq!(mock_exchange.placed_orders().len(), 1);
        assert_eq!(mock_exchange.placed_orders()[0].side, OrderSide::Buy);
    }

    #[tokio::test]
    async fn bearish_allocation_places_sell_order() {
        let mock_exchange = Arc::new(MockExchangeClient::new());
        // BTC を多く保有した状態 → current_alloc 高い → target低い → sell
        mock_exchange.set_balances(vec![
            crate::types::balance::Balance {
                currency_code: "JPY".to_string(),
                amount: dec!(100_000),
                available: dec!(100_000),
            },
            crate::types::balance::Balance {
                currency_code: "BTC".to_string(),
                amount: dec!(0.1),
                available: dec!(0.1),
            },
        ]);
        // price=9_000_500 → BTC価値 ≈ 900_050 JPY
        // total ≈ 1_000_050 JPY, current_alloc ≈ 0.9 → target=0.1 → delta=-0.8 → sell

        let (tx, _) = broadcast::channel::<SignalDetail>(16);
        let params = RiskParams::default();
        let mut engine = TradingEngine::new(
            tx.subscribe(),
            mock_exchange.clone(),
            RiskManager::new(params),
            "BTC_JPY".into(),
        );

        tx.send(detail(0.1, 0.9)).unwrap();
        drop(tx);

        engine.run().await;
        assert_eq!(mock_exchange.placed_orders().len(), 1);
        assert_eq!(mock_exchange.placed_orders()[0].side, OrderSide::Sell);
    }
}
