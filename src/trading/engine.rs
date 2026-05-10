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
use crate::signal::{AllocationSignal, IndicatorOutput, SignalDetail};
use crate::storage::orders::OrderRepository;
use crate::types::order::{Order, OrderRequest, OrderSide, OrderStatus, OrderType};

/// Core trading loop that converts IndicatorOutput into orders and SignalDetail broadcasts.
///
/// Listens on an `IndicatorOutput` broadcast channel, computes the allocation signal
/// via `aggregate_with_zone()`, applies guard logic, and submits market orders.
/// Broadcasts `SignalDetail` for API/WebSocket consumers after each indicator update.
/// Also handles daily UTC midnight resets for the risk manager.
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
    indicator_rx: broadcast::Receiver<IndicatorOutput>,
    /// Broadcasts SignalDetail to API/WebSocket consumers (signal.rs route, ws_handler.rs).
    signal_tx: broadcast::Sender<SignalDetail>,
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
        indicator_rx: broadcast::Receiver<IndicatorOutput>,
        exchange: Arc<dyn ExchangeClient>,
        risk_manager: RiskManager,
        product_code: String,
    ) -> (Self, broadcast::Sender<SignalDetail>) {
        let (signal_tx, _) = broadcast::channel(256);
        (
            Self {
                indicator_rx,
                signal_tx: signal_tx.clone(),
                exchange,
                risk_manager,
                product_code,
                last_reset_date: None,
                config: Arc::new(RwLock::new(TradingConfig::default())),
                order_repo: None,
                sticky_target: None,
            },
            signal_tx,
        )
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
        // Separate indicator reception from order processing so that a slow
        // handle_indicator() call never blocks the broadcast receiver.
        //
        // Buffer=1: if the worker is busy when a new output arrives, the
        // old queued output is discarded and only the latest one is kept.
        // This ensures we always act on the most recent market state.
        let (work_tx, mut work_rx) = tokio::sync::mpsc::channel::<IndicatorOutput>(1);

        // Extract indicator_rx from self using channel replacement so that `self`
        // remains valid (not partially moved) for the worker loop below.
        // The dummy receiver is immediately dropped and never polled.
        let (dummy_tx, dummy_rx) = broadcast::channel(1);
        drop(dummy_tx);
        let mut indicator_rx = std::mem::replace(&mut self.indicator_rx, dummy_rx);

        // Receiver task: lightweight, always ready to consume from the broadcast
        // channel immediately regardless of how long order processing takes.
        tokio::spawn(async move {
            loop {
                match indicator_rx.recv().await {
                    Ok(output) => {
                        // try_send drops the output when the worker is busy
                        // (buffer full), keeping only the most recent output.
                        let _ = work_tx.try_send(output);
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        warn!("TradingEngine lagged by {} indicator outputs", n);
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        info!("TradingEngine indicator channel closed, shutting down");
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
                        Some(output) => {
                            if let Err(e) = self.handle_indicator(output).await {
                                warn!("TradingEngine error: {}", e);
                            }
                        }
                        None => break, // channel closed; shut down
                    }
                }
                _ = rebalance_ticker.tick() => {
                    // Periodic rebalance: re-evaluate the sticky target against the
                    // current allocation to catch drift from price movements.
                    if let Some(target) = self.sticky_target {
                        debug!(target, "Periodic rebalance check");
                        if let Err(e) = self.handle_rebalance(target).await {
                            warn!("TradingEngine rebalance error: {}", e);
                        }
                    }
                }
            }
        }
    }

    /// Process a single IndicatorOutput: aggregate signals, update sticky target,
    /// broadcast SignalDetail for API consumers, apply guard logic, and place orders.
    ///
    /// # Sticky target logic
    ///
    /// Only outputs where aggregate `confidence > 0` update `sticky_target`.
    /// When `confidence == 0` (all indicators Hold), the existing `sticky_target`
    /// is used unchanged so that a neutral market does not force a rebalance back
    /// to 50%.  If no `sticky_target` has been set yet, the output is skipped.
    async fn handle_indicator(&mut self, output: IndicatorOutput) -> crate::error::Result<()> {
        // Aggregate indicator signals into an allocation signal (judgment step).
        // This is the only place aggregate_with_zone() is called; SignalEngine
        // only computes raw values and does not make allocation decisions.
        let raw_signals: Vec<Option<crate::signal::Signal>> = output.indicators.iter()
            .map(|is| is.signal.clone())
            .collect();
        let signal = {
            let cfg = self.config.read().await;
            crate::signal::aggregate_with_zone(&raw_signals, &cfg.zone)
        };

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

        // Broadcast SignalDetail for API/WebSocket consumers (GET /api/signal, /ws/candles).
        // This happens before order placement so consumers can see the signal even if
        // no order is placed (e.g. delta below threshold, guard suppressed).
        let detail = SignalDetail {
            aggregate: signal.clone(),
            indicators: output.indicators.clone(),
            raw_indicators: Some(output.raw.clone()),
            calculated_at: output.calculated_at,
            calculation_state: "active".to_string(),
        };
        let _ = self.signal_tx.send(detail);

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
        let btc_value = btc_held * btc_price;
        let total_value = btc_value + jpy_balance;

        let min_order_size = self.risk_manager.params().min_order_size;
        let order_req = if total_value.is_zero() || btc_price.is_zero() {
            None
        } else {
            let current_alloc = (btc_value / total_value)
                .to_f64()
                .unwrap_or(0.0);
            let delta = target_pct - current_alloc;
            if delta.abs() < allocation_threshold {
                None
            } else {
                // --- Indicator guard logic ---
                // Applied only for indicator-driven signals (not periodic rebalances).
                // Uses output.raw directly since IndicatorPoint is always present in IndicatorOutput.
                let mut effective_delta = delta;
                let ri = &output.raw;
                let cfg = self.config.read().await;

                // RSI guard: suppress orders when market is overbought/oversold.
                if cfg.rsi_guard_enabled {
                    if let Some(rsi) = ri.rsi {
                        if rsi > cfg.rsi_overbought && effective_delta > 0.0 {
                            debug!(rsi, threshold = cfg.rsi_overbought, "RSI overbought guard: suppressing buy");
                            return Ok(());
                        }
                        if rsi < cfg.rsi_oversold && effective_delta < 0.0 {
                            debug!(rsi, threshold = cfg.rsi_oversold, "RSI oversold guard: suppressing sell");
                            return Ok(());
                        }
                    }
                }

                // MACD confirmation guard: skip when histogram direction conflicts with signal.
                if cfg.macd_confirmation_enabled {
                    if let Some(hist) = ri.histogram {
                        let macd_bullish = hist > 0.0;
                        let delta_bullish = effective_delta > 0.0;
                        if macd_bullish != delta_bullish {
                            debug!(histogram = hist, delta = effective_delta, "MACD confirmation guard: direction mismatch, skipping");
                            return Ok(());
                        }
                    }
                }

                // Bollinger Band squeeze: halve order size during low-volatility squeeze.
                if let (Some(upper), Some(middle), Some(lower)) = (ri.bb_upper, ri.bb_middle, ri.bb_lower) {
                    if middle > 0.0 {
                        let squeeze_ratio = (upper - lower) / middle;
                        if squeeze_ratio < cfg.bb_squeeze_threshold {
                            debug!(squeeze_ratio, threshold = cfg.bb_squeeze_threshold, "BB squeeze detected: halving order size");
                            effective_delta *= 0.5;
                        }
                    }
                }

                if effective_delta.abs() < allocation_threshold {
                    None
                } else {
                    Self::allocation_delta_to_order(
                        effective_delta,
                        total_value,
                        btc_price,
                        &self.product_code,
                        min_order_size,
                    )
                }
            }
        };

        self.submit_order(order_req, btc_position, jpy_balance).await
    }

    /// Periodic rebalance handler: uses the existing sticky_target to correct
    /// allocation drift from price movements.  No aggregation or guard logic is
    /// applied — this is a pure delta-rebalance with the last known target.
    ///
    /// Broadcasts a synthetic SignalDetail (confidence=0, no raw_indicators) so
    /// that API consumers remain aware of periodic rebalance activity.
    async fn handle_rebalance(&mut self, target_pct: f64) -> crate::error::Result<()> {
        // Broadcast synthetic SignalDetail so API/WS consumers see rebalance activity.
        let rebalance_detail = SignalDetail {
            aggregate: AllocationSignal {
                raw_signal: target_pct,
                target_pct,
                confidence: 0.0,
            },
            indicators: vec![],
            raw_indicators: None,
            calculated_at: Utc::now(),
            calculation_state: "active".to_string(),
        };
        let _ = self.signal_tx.send(rebalance_detail);

        let allocation_threshold = {
            let cfg = self.config.read().await;
            self.risk_manager.update_params(cfg.to_risk_params());
            cfg.allocation_threshold
        };

        let (balances, positions, ticker) = tokio::try_join!(
            self.exchange.get_balance(),
            self.exchange.get_positions(&self.product_code),
            self.exchange.get_ticker(&self.product_code),
        )?;

        let jpy_balance = balances.iter()
            .find(|b| b.currency_code == "JPY")
            .map(|b| b.available)
            .unwrap_or(Decimal::ZERO);

        let today = Utc::now().date_naive();
        if self.last_reset_date != Some(today) {
            self.risk_manager.reset_daily(jpy_balance);
            self.last_reset_date = Some(today);
            info!("RiskManager daily reset: baseline_jpy={}", jpy_balance);
        }

        let btc_position: Decimal = positions.iter().map(|p| p.size).sum();
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
            let current_alloc = (btc_value / total_value).to_f64().unwrap_or(0.0);
            let delta = target_pct - current_alloc;
            if delta.abs() < allocation_threshold {
                None
            } else {
                Self::allocation_delta_to_order(delta, total_value, btc_price, &self.product_code, min_order_size)
            }
        };

        self.submit_order(order_req, btc_position, jpy_balance).await
    }

    /// Evaluate the order request through the risk manager and submit to the exchange.
    async fn submit_order(
        &mut self,
        order_req: Option<OrderRequest>,
        btc_position: Decimal,
        jpy_balance: Decimal,
    ) -> crate::error::Result<()> {
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
        // Round DOWN to avoid the computed cost slightly exceeding the available balance
        // due to rounding. For buy orders, cost = price * size must not exceed total_value.
        let size = (delta_dec * total_value / btc_price)
            .round_dp_with_strategy(8, rust_decimal::RoundingStrategy::ToZero);
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
    use crate::config::TradingConfig;
    use crate::exchange::mock::MockExchangeClient;
    use crate::risk::{RiskManager, RiskParams};
    use crate::signal::{IndicatorOutput, IndicatorPoint, IndicatorSignal, Signal};
    use rust_decimal_macros::dec;

    /// Build an IndicatorOutput from a list of signals.
    /// The raw IndicatorPoint has all None values (no guard-triggering indicators).
    fn make_output(signals: Vec<Option<Signal>>) -> IndicatorOutput {
        let indicators = signals.into_iter().enumerate()
            .map(|(i, sig)| IndicatorSignal {
                name: format!("mock_{}", i),
                signal: sig,
                value: None,
            })
            .collect();
        IndicatorOutput {
            indicators,
            raw: IndicatorPoint {
                time: chrono::Utc::now(),
                sma: None, ema: None, rsi: None,
                macd_line: None, signal_line: None, histogram: None,
                bb_upper: None, bb_middle: None, bb_lower: None,
            },
            calculated_at: chrono::Utc::now(),
        }
    }

    /// All Hold signals → aggregate confidence = 0 → no sticky_target → no order.
    fn hold_output() -> IndicatorOutput {
        make_output(vec![None, None, None])
    }

    /// Strong Buy signals → aggregate produces target_pct well above 0.5.
    fn bullish_output() -> IndicatorOutput {
        make_output(vec![
            Some(Signal::Buy { price: dec!(9000000), confidence: 1.0 }),
        ])
    }

    /// Strong Sell signals → aggregate produces target_pct well below 0.5.
    fn bearish_output() -> IndicatorOutput {
        make_output(vec![
            Some(Signal::Sell { price: dec!(9000000), confidence: 1.0 }),
        ])
    }

    #[tokio::test]
    async fn neutral_allocation_does_not_place_order() {
        // All Hold signals → confidence=0 → sticky_target=None → no order placed.
        let mock_exchange = Arc::new(MockExchangeClient::new());
        let (indicator_tx, indicator_rx) = broadcast::channel::<IndicatorOutput>(16);
        let params = RiskParams::default();
        let (engine, _signal_tx) = TradingEngine::new(
            indicator_rx,
            mock_exchange.clone(),
            RiskManager::new(params),
            "BTC_JPY".into(),
        );

        indicator_tx.send(hold_output()).unwrap();
        drop(indicator_tx);

        engine.run().await;
        assert!(mock_exchange.placed_orders().is_empty());
    }

    #[tokio::test]
    async fn bullish_allocation_places_buy_order() {
        // JPY=1_000_000, BTC=0, price=9_000_500, fee=0
        // Strong Buy → aggregate target_pct=1.0 → delta=1.0 → size≈0.111 BTC
        // Use TradingConfig override with max_position_btc=1.0 so risk manager allows it.
        // fee=0 to avoid insufficient-balance edge case at 100% allocation.
        let mock_exchange = Arc::new(MockExchangeClient::with_fee(0.0));
        let (indicator_tx, indicator_rx) = broadcast::channel::<IndicatorOutput>(16);
        let params = RiskParams::default();
        let (engine, _signal_tx) = TradingEngine::new(
            indicator_rx,
            mock_exchange.clone(),
            RiskManager::new(params),
            "BTC_JPY".into(),
        );
        // Override TradingConfig to allow large BTC position
        let mut cfg = TradingConfig::default();
        cfg.max_position_btc = 1.0;
        let engine = engine.with_config(Arc::new(RwLock::new(cfg)));

        indicator_tx.send(bullish_output()).unwrap();
        drop(indicator_tx);

        engine.run().await;
        assert_eq!(mock_exchange.placed_orders().len(), 1);
        assert_eq!(mock_exchange.placed_orders()[0].side, OrderSide::Buy);
    }

    #[tokio::test]
    async fn bearish_allocation_places_sell_order() {
        let mock_exchange = Arc::new(MockExchangeClient::new());
        // BTC を多く保有した状態 → current_alloc 高い → Strong Sell → sell
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
        // total ≈ 1_000_050 JPY, current_alloc ≈ 0.9 → strong Sell → target_pct ≈ 0 → sell

        let (indicator_tx, indicator_rx) = broadcast::channel::<IndicatorOutput>(16);
        let params = RiskParams::default();
        let (engine, _signal_tx) = TradingEngine::new(
            indicator_rx,
            mock_exchange.clone(),
            RiskManager::new(params),
            "BTC_JPY".into(),
        );

        indicator_tx.send(bearish_output()).unwrap();
        drop(indicator_tx);

        engine.run().await;
        assert_eq!(mock_exchange.placed_orders().len(), 1);
        assert_eq!(mock_exchange.placed_orders()[0].side, OrderSide::Sell);
    }

    #[tokio::test]
    async fn neutral_signal_after_bullish_does_not_rebalance_to_50pct() {
        // Regression test for sticky target behaviour:
        // 1. Bullish signal (confidence>0) establishes sticky_target → Buy
        // 2. Hold signal (confidence=0) must NOT reset target to 0.5 and sell
        let mock_exchange = Arc::new(MockExchangeClient::new());
        let (indicator_tx, indicator_rx) = broadcast::channel::<IndicatorOutput>(16);
        let params = RiskParams {
            max_position_btc: dec!(1.0),
            max_daily_drawdown: 0.5,
            stop_loss_pct: 0.5,
            min_order_size: dec!(0.001),
            circuit_breaker_enabled: false,
        };
        let (engine, _signal_tx) = TradingEngine::new(
            indicator_rx,
            mock_exchange.clone(),
            RiskManager::new(params),
            "BTC_JPY".into(),
        );

        // Bullish signal: sticky_target set → Buy
        indicator_tx.send(bullish_output()).unwrap();
        // Hold signal: confidence=0 → sticky_target preserved (no sell-back to 50%)
        indicator_tx.send(hold_output()).unwrap();
        drop(indicator_tx);

        engine.run().await;

        let orders = mock_exchange.placed_orders();
        // Should not contain a Sell order (no rebalance back to 50%)
        assert!(
            orders.iter().all(|o| o.side == OrderSide::Buy),
            "unexpected sell order: {:?}", orders
        );
    }

    #[tokio::test]
    async fn no_directional_signal_skips_rebalance() {
        // When only Hold signals arrive (e.g. warm-up with all None),
        // sticky_target remains None and no order should be placed.
        let mock_exchange = Arc::new(MockExchangeClient::new());
        let (indicator_tx, indicator_rx) = broadcast::channel::<IndicatorOutput>(16);
        let params = RiskParams::default();
        let (engine, _signal_tx) = TradingEngine::new(
            indicator_rx,
            mock_exchange.clone(),
            RiskManager::new(params),
            "BTC_JPY".into(),
        );

        // Only Hold signals — no sticky_target established
        indicator_tx.send(hold_output()).unwrap();
        indicator_tx.send(hold_output()).unwrap();
        drop(indicator_tx);

        engine.run().await;
        assert!(mock_exchange.placed_orders().is_empty(), "should not trade without a directional signal");
    }
}
