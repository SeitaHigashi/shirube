use std::sync::Arc;
use std::time::Duration;

use chrono::{NaiveDate, Utc};
use tokio::sync::{broadcast, RwLock};
use tokio::time::{interval, MissedTickBehavior};
use tracing::{debug, info, warn};

use rust_decimal::Decimal;

use rust_decimal::prelude::{FromPrimitive, ToPrimitive};

use crate::config::TradingConfig;
use crate::exchange::ExchangeClient;
use crate::risk::manager::RiskManager;
use crate::risk::RiskDecision;
use crate::signal::{AllocationSignal, SignalDetail};
use crate::storage::orders::OrderRepository;
use crate::types::order::{Order, OrderRequest, OrderSide, OrderStatus, OrderType};

/// Core trading loop that converts allocation signals into real orders.
///
/// Listens on a `SignalDetail` broadcast channel and for each received
/// signal: fetches live balances/positions/price, computes the required
/// BTC delta, validates through `RiskManager`, and submits a market
/// order.  Also handles daily UTC midnight resets for the risk manager.
///
/// # Sticky Target Allocation
///
/// To prevent churning (e.g. buying to 70% then immediately selling back
/// to 50% when indicators go neutral), this engine maintains a
/// `sticky_target` — the last BTC allocation target set by a *directional*
/// signal (`confidence > 0`).  When all indicators are Hold
/// (`confidence == 0`), the sticky target is preserved and used for the
/// rebalance delta calculation instead of the raw 50% neutral value.
pub struct TradingEngine {
    signal_rx: broadcast::Receiver<SignalDetail>,
    exchange: Arc<dyn ExchangeClient>,
    risk_manager: RiskManager,
    product_code: String,
    /// Last UTC date on which `reset_daily` was called; used to detect
    /// midnight crossings without a dedicated timer task.
    last_reset_date: Option<NaiveDate>,
    /// Trading configuration (allocation threshold, risk params) that can
    /// be updated live from the UI without restarting the engine.
    config: Arc<RwLock<TradingConfig>>,
    /// Optional repository for persisting placed orders to SQLite.
    order_repo: Option<OrderRepository>,
    /// The most recent target BTC allocation established by a directional
    /// signal (`confidence > 0`).  None until the first directional signal
    /// arrives (warm-up phase); used as-is when subsequent signals have
    /// `confidence == 0` so that neutral indicators do not force a rebalance
    /// back to 50%.
    sticky_target: Option<f64>,
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
            last_reset_date: None,
            config: Arc::new(RwLock::new(TradingConfig::default())),
            order_repo: None,
            sticky_target: None,
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

    pub async fn run(mut self) {
        // Separate signal reception from order processing so that a slow
        // handle_signal() call never blocks the broadcast receiver.
        //
        // Buffer=1: if the worker is busy when a new signal arrives, the
        // old queued signal is discarded and only the latest one is kept.
        // This ensures we always act on the most recent market state.
        let (work_tx, mut work_rx) = tokio::sync::mpsc::channel::<AllocationSignal>(1);

        // Extract signal_rx from self using channel replacement so that `self`
        // remains valid (not partially moved) for the worker loop below.
        // The dummy receiver is immediately dropped and never polled.
        let (dummy_tx, dummy_rx) = broadcast::channel(1);
        drop(dummy_tx);
        let mut signal_rx = std::mem::replace(&mut self.signal_rx, dummy_rx);

        // Receiver task: lightweight, always ready to consume from the broadcast
        // channel immediately regardless of how long order processing takes.
        tokio::spawn(async move {
            loop {
                match signal_rx.recv().await {
                    Ok(detail) => {
                        // try_send drops the signal when the worker is busy
                        // (buffer full), keeping only the most recent signal.
                        let _ = work_tx.try_send(detail.aggregate);
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
        });

        // Set up a periodic rebalance timer to catch allocation drift caused
        // by price movements even when no new signals are arriving.
        // Interval is read once from config at startup.
        let rebalance_interval_secs = self.config.read().await.rebalance_interval_secs;
        let mut rebalance_ticker = interval(Duration::from_secs(rebalance_interval_secs));
        rebalance_ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

        // Worker loop: owns all mutable state (risk_manager, last_reset_date, sticky_target)
        // and processes signals one at a time without racing the receiver.
        loop {
            tokio::select! {
                result = work_rx.recv() => {
                    match result {
                        Some(signal) => {
                            if let Err(e) = self.handle_signal(signal).await {
                                warn!("TradingEngine error: {}", e);
                            }
                        }
                        None => break, // channel closed; shut down
                    }
                }
                _ = rebalance_ticker.tick() => {
                    // Periodic rebalance: re-evaluate the sticky target against the
                    // current allocation to catch drift from price movements.
                    // Uses confidence=0 so that sticky_target is not overwritten.
                    if let Some(target) = self.sticky_target {
                        debug!(target, "Periodic rebalance check");
                        let rebalance_signal = AllocationSignal {
                            raw_signal: target,
                            target_pct: target,
                            confidence: 0.0,
                        };
                        if let Err(e) = self.handle_signal(rebalance_signal).await {
                            warn!("TradingEngine rebalance error: {}", e);
                        }
                    }
                }
            }
        }
    }

    /// Process a single allocation signal: fetch market state, compute
    /// the required BTC delta, and place an order if the delta exceeds
    /// the configured threshold.
    ///
    /// Also performs a daily reset of the risk manager when the UTC date
    /// has changed since the last signal (detected lazily here rather
    /// than via a separate timer task to avoid an extra tokio::spawn).
    ///
    /// # Sticky target logic
    ///
    /// Only signals with `confidence > 0` (at least one directional indicator)
    /// update `sticky_target`.  When `confidence == 0` (all indicators Hold),
    /// the existing `sticky_target` is used unchanged so that a neutral market
    /// does not force a rebalance back to 50%.  If no `sticky_target` has been
    /// set yet (warm-up period), the signal is silently skipped.
    async fn handle_signal(&mut self, signal: AllocationSignal) -> crate::error::Result<()> {
        // Update sticky_target only when there is directional conviction.
        // confidence == 0.0 means all indicators returned Hold; in that case we
        // preserve the previously established target to avoid churning back to 50%.
        if signal.confidence > 0.0 {
            self.sticky_target = Some(signal.target_pct);
            debug!(target = signal.target_pct, confidence = signal.confidence, "Sticky target updated");
        }

        // Skip rebalancing until the first directional signal establishes a target.
        // This prevents spurious 50% rebalances during indicator warm-up.
        let target_pct = match self.sticky_target {
            Some(t) => t,
            None => {
                debug!("No sticky target yet, skipping rebalance");
                return Ok(());
            }
        };

        // Pull the latest config snapshot and push params into risk manager
        let allocation_threshold = {
            let cfg = self.config.read().await;
            self.risk_manager.update_params(cfg.to_risk_params());
            cfg.allocation_threshold
        };

        // Fetch balance, positions, and best price concurrently to
        // minimise latency between signal receipt and order submission
        let (balances, positions, ticker) = tokio::try_join!(
            self.exchange.get_balance(),
            self.exchange.get_positions(&self.product_code),
            self.exchange.get_ticker(&self.product_code),
        )?;

        let jpy_balance = balances.iter()
            .find(|b| b.currency_code == "JPY")
            .map(|b| b.available)
            .unwrap_or(Decimal::ZERO);

        // Detect UTC midnight crossing and reset the daily risk baseline.
        // We use the JPY available balance as the new day's starting value.
        let today = Utc::now().date_naive();
        if self.last_reset_date != Some(today) {
            self.risk_manager.reset_daily(jpy_balance);
            self.last_reset_date = Some(today);
            info!("RiskManager daily reset: baseline_jpy={}", jpy_balance);
        }

        // Positions represent open CFD/FX exposure; on the spot market
        // the BTC holding lives in the balance instead.
        let btc_position: Decimal = positions.iter().map(|p| p.size).sum();

        // NOTE: take the maximum of position size and BTC balance so that
        // both real (bitFlyer spot balance) and mock (MockExchangeClient
        // which stores BTC in balance, not positions) clients work correctly.
        let btc_balance = balances.iter()
            .find(|b| b.currency_code == "BTC")
            .map(|b| b.available)
            .unwrap_or(Decimal::ZERO);
        let btc_held = btc_position.max(btc_balance);

        let btc_price = ticker.ltp;

        // Compute current allocation fraction: BTC value / total portfolio value
        let btc_value = btc_held * btc_price;
        let total_value = btc_value + jpy_balance;

        let min_order_size = self.risk_manager.params().min_order_size;
        let order_req = if total_value.is_zero() || btc_price.is_zero() {
            // Cannot compute allocation without a non-zero price or portfolio
            None
        } else {
            let current_alloc = (btc_value / total_value)
                .to_f64()
                .unwrap_or(0.0);
            // delta > 0 → buy more BTC; delta < 0 → sell BTC
            // NOTE: use sticky_target (not signal.target_pct) so that
            // neutral signals (confidence=0) do not pull the allocation
            // back to 50% when the engine is already at the intended level.
            let delta = target_pct - current_alloc;
            if delta.abs() < allocation_threshold {
                // Change is too small to warrant a trade (avoids churn)
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
            }
        }

        Ok(())
    }

    /// Convert a signed allocation delta into a market `OrderRequest`.
    ///
    /// - `delta > 0` → Buy (increase BTC allocation)
    /// - `delta < 0` → Sell (decrease BTC allocation)
    ///
    /// BTC order size formula:
    ///   size = |delta| * total_value / btc_price  (rounded to 8 d.p.)
    ///
    /// Returns `None` if the computed size is below `min_size`, preventing
    /// dust orders that would be rejected by the exchange API.
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
        let engine = TradingEngine::new(
            tx.subscribe(),
            mock_exchange.clone(),
            RiskManager::new(params),
            "BTC_JPY".into(),
        );

        // target_pct = 0.02 → delta ≈ 0.02 < allocation_threshold(0.15)
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
        let engine = TradingEngine::new(
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
        let engine = TradingEngine::new(
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

    #[tokio::test]
    async fn neutral_signal_after_bullish_does_not_rebalance_to_50pct() {
        // Regression test for sticky target behaviour:
        // 1. Bullish signal (confidence=0.8) establishes sticky_target=0.8 → Buy
        // 2. Neutral signal (confidence=0.0) must NOT reset target to 0.5 and sell
        //
        // MockExchangeClient: JPY=1_000_000, BTC=0, price=9_000_500
        // After the Buy we don't actually update the mock balance here, so
        // for the neutral signal the mock still has BTC=0.
        // sticky_target=0.8, current_alloc≈0.0, delta=0.8 > threshold → only 1 Buy.
        // Then neutral signal: confidence=0, sticky_target stays 0.8, delta unchanged → no sell.
        let mock_exchange = Arc::new(MockExchangeClient::new());
        let (tx, _) = broadcast::channel::<SignalDetail>(16);
        let params = RiskParams {
            max_position_btc: dec!(1.0),
            max_daily_drawdown: 0.5,
            stop_loss_pct: 0.5,
            min_order_size: dec!(0.001),
            circuit_breaker_enabled: false,
        };
        let engine = TradingEngine::new(
            tx.subscribe(),
            mock_exchange.clone(),
            RiskManager::new(params),
            "BTC_JPY".into(),
        );

        // Bullish signal: sticky_target = 0.8 → Buy
        tx.send(detail(0.8, 0.8)).unwrap();
        // Neutral signal: confidence=0 → sticky_target stays 0.8, no sell to 50%
        tx.send(detail(0.5, 0.0)).unwrap();
        drop(tx);

        engine.run().await;

        let orders = mock_exchange.placed_orders();
        // Only the initial Buy should have been placed; no sell-back to 50%
        assert_eq!(orders.len(), 1, "expected 1 order, got {}: {:?}", orders.len(), orders);
        assert_eq!(orders[0].side, OrderSide::Buy);
    }

    #[tokio::test]
    async fn no_directional_signal_skips_rebalance() {
        // When only confidence=0 signals arrive (e.g. warm-up with all Hold),
        // sticky_target remains None and no order should be placed.
        let mock_exchange = Arc::new(MockExchangeClient::new());
        let (tx, _) = broadcast::channel::<SignalDetail>(16);
        let params = RiskParams::default();
        let engine = TradingEngine::new(
            tx.subscribe(),
            mock_exchange.clone(),
            RiskManager::new(params),
            "BTC_JPY".into(),
        );

        // Only neutral signals (confidence=0) — no sticky_target established
        tx.send(detail(0.5, 0.0)).unwrap();
        tx.send(detail(0.5, 0.0)).unwrap();
        drop(tx);

        engine.run().await;
        assert!(mock_exchange.placed_orders().is_empty(), "should not trade without a directional signal");
    }
}
