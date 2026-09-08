use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use tokio::sync::{broadcast, RwLock};
use tokio::time::{interval, MissedTickBehavior};
use tracing::{debug, info, warn};

use rust_decimal::Decimal;

use rust_decimal::prelude::{FromPrimitive, ToPrimitive};

use crate::config::TradingConfig;
use crate::exchange::ExchangeClient;
use crate::news::analyzer::SentimentScore;
use crate::risk::manager::RiskManager;
use crate::risk::RiskDecision;
use crate::signal::{AllocationSignal, IndicatorOutput, IndicatorPoint, SignalDetail};
use crate::storage::orders::OrderRepository;
use crate::types::order::{Order, OrderRequest, OrderSide, OrderStatus, OrderType};

/// Core trading loop that converts IndicatorOutput into orders and SignalDetail broadcasts.
///
/// Listens on an `IndicatorOutput` broadcast channel, computes the allocation signal
/// via `compute_btc_target()` (TA + news sentiment), applies zone mapping, and submits
/// market orders. Broadcasts `SignalDetail` for API/WebSocket consumers after each update.
/// Also handles daily UTC midnight resets for the risk manager.
///
/// # Sticky Target Allocation
///
/// To prevent churning (e.g. buying to 70% then immediately selling back
/// to 50% when indicators go neutral), this engine maintains a
/// `sticky_target` — the last BTC allocation target set by a *directional*
/// signal. When both TA and news are neutral, the sticky target is preserved
/// so that a neutral market does not force a rebalance back to 50%.
pub struct TradingEngine {
    indicator_rx: broadcast::Receiver<IndicatorOutput>,
    /// Broadcasts SignalDetail to API/WebSocket consumers (signal.rs route, ws_handler.rs).
    signal_tx: broadcast::Sender<SignalDetail>,
    exchange: Arc<dyn ExchangeClient>,
    risk_manager: RiskManager,
    product_code: String,
    /// Trading configuration (allocation threshold, indicator weights) that can
    /// be updated live from the UI without restarting the engine.
    config: Arc<RwLock<TradingConfig>>,
    /// Optional repository for persisting placed orders to SQLite.
    order_repo: Option<OrderRepository>,
    /// The most recent target BTC allocation established by a directional
    /// signal. None until the first directional signal arrives (warm-up phase);
    /// used as-is when subsequent signals are neutral so that a neutral market
    /// does not force a rebalance back to 50%.
    sticky_target: Option<f64>,
    /// Latest news sentiment scores from the news analysis task.
    /// Used to incorporate news into the BTC allocation target calculation.
    news_cache: Arc<RwLock<Vec<SentimentScore>>>,
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
                config: Arc::new(RwLock::new(TradingConfig::default())),
                order_repo: None,
                sticky_target: None,
                news_cache: Arc::new(RwLock::new(vec![])),
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

    /// ニュースセンチメントキャッシュを設定する。
    /// AppState の news_cache を共有することで最新スコアをリアルタイムに参照できる。
    pub fn with_news_cache(mut self, news_cache: Arc<RwLock<Vec<SentimentScore>>>) -> Self {
        self.news_cache = news_cache;
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

        // Worker loop: owns all mutable state (risk_manager, sticky_target)
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

    /// Compute the BTC allocation ratio from raw indicator values and news sentiment.
    ///
    /// NOTE: This function's body may be modified by the self-improvement-loop
    /// pipeline's `kind: "algorithm"` hypotheses (see
    /// `experiments/hypotheses/README.md`), but only on that hypothesis's own
    /// dedicated worktree branch, never as a direct commit to `dev`/`main`.
    /// Every such change lands as its own PR for human review before merging —
    /// see `docs/self-improvement-loop.md`.
    ///
    /// Restored 2026-09-08 to the multi-indicator formula below after a
    /// hand-edit (`fbce478`, "chore: modified trading expression") replaced it
    /// with `0.9 * ema / bb_middle` (the `sma` term canceled out
    /// algebraically), which kept the signal pinned inside a single zone for
    /// an entire 30-day backtest and produced exactly one trade. The
    /// crossover-based sub-signals below flip direction far more often,
    /// which is the intended fix.
    ///
    /// Derives a directional sub-signal from each available indicator in `raw`,
    /// averages them into a TA normalized value [0.0, 1.0], then combines with
    /// news sentiment using the configured weights.
    ///
    /// Returns `None` when all indicator fields are `None` (still warming up).
    ///
    /// # Sub-signal mappings
    /// - RSI:       (1.0 - rsi/100)       — low RSI (oversold) → 1.0 (bullish)
    /// - SMA cross: damped continuous cross, saturating at ±0.5% from the SMA
    ///              `(((close - sma) / sma) / 0.005).clamp(-1, 1) * 0.5 + 0.5`
    /// - EMA cross: same damped mapping with the EMA as the reference line
    /// - MACD hist: `((histogram / bb_middle) / 0.005).clamp(-1, 1) * 0.5 + 0.5`,
    ///              i.e. the histogram scaled by its magnitude relative to the
    ///              Bollinger midline, saturating at the same 0.5% relative
    ///              distance. Falls back to the sign-based step mapping
    ///              (>0 → 1.0, <0 → 0.0, ==0 → 0.5) when `bb_middle` is `None`
    ///              or non-positive, so the sub-signal is never dropped.
    /// - BB %B:     (1.0 - pct_b/100) clamped — near lower band → bullish
    ///
    /// NOTE: The SMA/EMA/MACD sub-signals were originally hard step functions
    /// (`close > sma → 1.0` etc.). They are damped continuous ramps instead so
    /// that a price hovering a few basis points either side of its reference
    /// line produces a proportionally small tilt rather than a full-swing
    /// 0.0/1.0 flip. The 0.5% saturation band is deliberately narrow: beyond
    /// half a percent of separation the mapping is identical to the old step
    /// function, so only genuinely marginal crossings are damped. RSI's
    /// sub-signal and Bollinger %B's sub-signal are already continuous and are
    /// intentionally left unchanged.
    ///
    /// # Formula
    ///   cross_sub_signal = ((x / ref) / SATURATION).clamp(-1, 1) * 0.5 + 0.5
    ///                                                     ∈ [0.0, 1.0]
    ///   ta_normalized    = avg(sub_signals)              ∈ [0.0, 1.0]
    ///   sentiment_norm   = (avg_sentiment + 1.0) / 2.0  ∈ [0.0, 1.0]
    ///   combined         = ta_normalized * ta_weight + sentiment_norm * sentiment_weight
    pub(crate) fn compute_btc_target(
        raw: &IndicatorPoint,
        news_scores: &[SentimentScore],
        config: &TradingConfig,
    ) -> Option<f64> {
        /// Relative distance from a reference line at which a damped crossover
        /// sub-signal reaches full saturation (0.0 or 1.0). 0.5% — beyond this
        /// separation the mapping matches the old hard step function exactly.
        const CROSS_SATURATION: f64 = 0.005;

        /// Map a signed relative deviation onto [0.0, 1.0] with linear damping
        /// inside ±`CROSS_SATURATION` and hard saturation outside it.
        ///
        /// `(relative / 0.005).clamp(-1.0, 1.0) * 0.5 + 0.5`
        /// → relative = 0 gives 0.5 (neutral), +0.5% or more gives 1.0
        ///   (fully bullish), -0.5% or less gives 0.0 (fully bearish).
        fn damped_cross(relative: f64) -> f64 {
            (relative / CROSS_SATURATION).clamp(-1.0, 1.0) * 0.5 + 0.5
        }

        let mut sub_signals: Vec<f64> = Vec::new();

        // RSI: oversold (low RSI) → bullish (1.0), overbought (high RSI) → bearish (0.0)
        // NOTE: already continuous — intentionally left unchanged.
        if let Some(rsi) = raw.rsi {
            sub_signals.push(1.0 - (rsi / 100.0).clamp(0.0, 1.0));
        }

        // SMA cross (damped): price above SMA → bullish, below → bearish, with
        // the swing proportional to the relative gap until it saturates at 0.5%.
        // A non-positive SMA would make the relative distance meaningless, so
        // such a point contributes no sub-signal.
        if let (Some(close), Some(sma)) = (raw.close, raw.sma) {
            if sma > 0.0 {
                sub_signals.push(damped_cross((close - sma) / sma));
            }
        }

        // EMA cross (damped): identical mapping with the EMA as the reference line.
        if let (Some(close), Some(ema)) = (raw.close, raw.ema) {
            if ema > 0.0 {
                sub_signals.push(damped_cross((close - ema) / ema));
            }
        }

        // MACD histogram (damped): the histogram is an absolute price-unit
        // quantity, so it is scaled by the Bollinger midline to get a relative
        // magnitude before the same 0.5%-saturation mapping is applied.
        // Fallback: without a usable bb_middle there is no scale reference, so
        // the original sign-based step mapping is used rather than dropping the
        // sub-signal entirely.
        if let Some(hist) = raw.histogram {
            match raw.bb_middle {
                Some(bb_middle) if bb_middle > 0.0 => {
                    sub_signals.push(damped_cross(hist / bb_middle));
                }
                _ => {
                    sub_signals.push(if hist > 0.0 { 1.0 } else if hist < 0.0 { 0.0 } else { 0.5 });
                }
            }
        }

        // Bollinger %B: near lower band (oversold) → bullish, near upper band → bearish
        // Computed from close, bb_upper, bb_lower since IndicatorPoint stores band values.
        if let (Some(close), Some(bb_upper), Some(bb_lower)) = (raw.close, raw.bb_upper, raw.bb_lower) {
            let bandwidth = bb_upper - bb_lower;
            let pct_b = if bandwidth > 0.0 {
                (close - bb_lower) / bandwidth * 100.0
            } else {
                50.0 // flat bands → neutral
            };
            sub_signals.push(1.0 - (pct_b / 100.0).clamp(0.0, 1.0));
        }

        // Warmup: no indicator data available yet
        if sub_signals.is_empty() {
            return None;
        }

        let ta_normalized = sub_signals.iter().sum::<f64>() / sub_signals.len() as f64;

        // Average news sentiment: [-1.0, 1.0] → normalize to [0.0, 1.0]
        // Empty cache (Ollama unavailable or not yet run) → neutral 0.0
        let avg_sentiment = if news_scores.is_empty() {
            0.0
        } else {
            news_scores.iter().map(|s| s.score).sum::<f64>() / news_scores.len() as f64
        };
        let sentiment_normalized = ((avg_sentiment + 1.0) / 2.0).clamp(0.0, 1.0);

        // Weighted combination of TA and sentiment signals
        let combined = ta_normalized * config.ta_weight + sentiment_normalized * config.sentiment_weight;
        Some(combined.clamp(0.0, 1.0))
    }

    /// Process a single IndicatorOutput: compute BTC target from raw indicator values,
    /// update sticky target, broadcast SignalDetail for API consumers and place orders.
    ///
    /// # Sticky target logic
    ///
    /// Only outputs where `compute_btc_target` returns `Some` update `sticky_target`.
    /// When it returns `None` (both TA and news neutral), the existing `sticky_target` is
    /// preserved so that a neutral market does not force a rebalance back to 50%.
    /// If no `sticky_target` has been set yet, processing is skipped.
    async fn handle_indicator(&mut self, output: IndicatorOutput) -> crate::error::Result<()> {
        // Compute combined TA + news normalized value [0.0, 1.0], then apply zone mapping.
        // Returns None when all indicators are in warmup or both TA and news are neutral.
        let (maybe_target, agg_normalized) = {
            let cfg = self.config.read().await;
            let news_scores = self.news_cache.read().await.clone();
            let combined = Self::compute_btc_target(&output.raw, &news_scores, &cfg);
            let agg_norm = combined.unwrap_or(0.5);
            let target = combined.map(|normalized| {
                // Scale normalized value by range_max, then map through zone boundaries
                // into the final BTC allocation ratio.
                let raw = normalized * cfg.zone.range_max;
                crate::signal::apply_zone(raw, &cfg.zone)
            });
            (target, agg_norm)
        };

        // Update sticky_target only when compute_btc_target returned a valid target.
        // When it returns None (neutral), preserve the previous target so that
        // a neutral market does not force a rebalance back to 50%.
        if let Some(t) = maybe_target {
            self.sticky_target = Some(t);
            debug!(target = t, "Sticky target updated");
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
        // no order is placed (e.g. delta below threshold).
        // target_pct reflects sticky_target so neutral signals don't reset the displayed value.
        let detail = SignalDetail {
            aggregate: AllocationSignal { normalized: agg_normalized },
            target_pct: self.sticky_target.unwrap_or(0.5),
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

        // Re-baseline the daily drawdown tracker off the candle's own timestamp
        // (not wall-clock) so the same code path also resets correctly when
        // driven by historical data in a backtest. See RiskManager::observe_time.
        self.risk_manager.observe_time(output.raw.time, total_value);
        if let Some(RiskDecision::CircuitBreaker { drawdown_pct }) =
            self.risk_manager.check_drawdown(total_value)
        {
            warn!(drawdown_pct, "Circuit breaker triggered by daily drawdown");
        }

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
                Self::allocation_delta_to_order(
                    delta,
                    total_value,
                    btc_price,
                    &self.product_code,
                    min_order_size,
                )
            }
        };

        self.submit_order(order_req).await
    }

    /// Periodic rebalance handler: uses the existing sticky_target to correct
    /// allocation drift from price movements.  No aggregation or guard logic is
    /// applied — this is a pure delta-rebalance with the last known target.
    ///
    /// Broadcasts a synthetic SignalDetail (no raw_indicators) so
    /// that API consumers remain aware of periodic rebalance activity.
    async fn handle_rebalance(&mut self, target_pct: f64) -> crate::error::Result<()> {
        // Broadcast synthetic SignalDetail so API/WS consumers see rebalance activity.
        let rebalance_detail = SignalDetail {
            aggregate: AllocationSignal { normalized: target_pct },
            target_pct,
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

        let btc_position: Decimal = positions.iter().map(|p| p.size).sum();
        let btc_balance = balances.iter()
            .find(|b| b.currency_code == "BTC")
            .map(|b| b.available)
            .unwrap_or(Decimal::ZERO);
        let btc_held = btc_position.max(btc_balance);

        let btc_price = ticker.ltp;
        let btc_value = btc_held * btc_price;
        let total_value = btc_value + jpy_balance;

        // This handler only runs off a live wall-clock ticker (never in a
        // backtest), so Utc::now() is the correct "current time" here.
        self.risk_manager.observe_time(Utc::now(), total_value);
        if let Some(RiskDecision::CircuitBreaker { drawdown_pct }) =
            self.risk_manager.check_drawdown(total_value)
        {
            warn!(drawdown_pct, "Circuit breaker triggered by daily drawdown");
        }

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

        self.submit_order(order_req).await
    }

    /// Evaluate the order request through the risk manager and submit to the exchange.
    async fn submit_order(
        &mut self,
        order_req: Option<OrderRequest>,
    ) -> crate::error::Result<()> {
        let order_req = match order_req {
            Some(r) => r,
            None => return Ok(()),
        };

        match self.risk_manager.evaluate(order_req) {
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
    pub(crate) fn allocation_delta_to_order(
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
    use crate::signal::{IndicatorOutput, IndicatorPoint, IndicatorSignal};
    use rust_decimal_macros::dec;

    /// Build an IndicatorOutput with all-None IndicatorPoint (warmup state).
    /// No directional signal → sticky_target stays None → no order.
    fn hold_output() -> IndicatorOutput {
        IndicatorOutput {
            indicators: vec![],
            raw: IndicatorPoint {
                time: chrono::Utc::now(),
                close: None,
                sma: None, ema: None, rsi: None,
                macd_line: None, signal_line: None, histogram: None,
                bb_upper: None, bb_middle: None, bb_lower: None,
            },
            calculated_at: chrono::Utc::now(),
        }
    }

    /// Build an IndicatorOutput with strong bullish raw values.
    /// RSI=20 (oversold), close > SMA, positive histogram, low %B.
    fn bullish_output() -> IndicatorOutput {
        IndicatorOutput {
            indicators: vec![IndicatorSignal { name: "RSI".into(), value: Some(20.0) }],
            raw: IndicatorPoint {
                time: chrono::Utc::now(),
                close: Some(10_100.0),
                sma: Some(10_000.0),   // close > sma → bullish
                ema: Some(10_050.0),   // close > ema → bullish
                rsi: Some(20.0),       // oversold → bullish
                macd_line: Some(50.0),
                signal_line: Some(30.0),
                histogram: Some(20.0), // positive → bullish
                bb_upper: Some(10_200.0),
                bb_middle: Some(10_000.0),
                bb_lower: Some(9_800.0), // close near middle, %B ≈ 50 → neutral from BB
            },
            calculated_at: chrono::Utc::now(),
        }
    }

    /// Build an IndicatorOutput with strong bearish raw values.
    /// RSI=80 (overbought), close < SMA, negative histogram, high %B.
    fn bearish_output() -> IndicatorOutput {
        IndicatorOutput {
            indicators: vec![IndicatorSignal { name: "RSI".into(), value: Some(80.0) }],
            raw: IndicatorPoint {
                time: chrono::Utc::now(),
                close: Some(9_900.0),
                sma: Some(10_000.0),   // close < sma → bearish
                ema: Some(9_950.0),    // close < ema → bearish
                rsi: Some(80.0),       // overbought → bearish
                macd_line: Some(-50.0),
                signal_line: Some(-30.0),
                histogram: Some(-20.0), // negative → bearish
                bb_upper: Some(10_200.0),
                bb_middle: Some(10_000.0),
                bb_lower: Some(9_800.0), // close near lower, %B ≈ 50 → neutral from BB
            },
            calculated_at: chrono::Utc::now(),
        }
    }

    #[tokio::test]
    async fn neutral_allocation_does_not_place_order() {
        // All-None IndicatorPoint → warmup → sticky_target=None → no order placed.
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
        // All-None IndicatorPoint → compute_btc_target returns None (no
        // sub-signal derivable) → sticky_target stays None → rebalancing is
        // skipped entirely, so no order is placed.
        assert!(mock_exchange.placed_orders().is_empty());
    }

    #[tokio::test]
    async fn bullish_allocation_places_buy_order() {
        // JPY=1_000_000, BTC=0, price=9_000_500, fee=0
        // Strong bullish → target_pct≈0.86 → delta≈0.86 (well above
        // allocation_threshold) → Buy
        let mock_exchange = Arc::new(MockExchangeClient::with_fee(0.0));
        let (indicator_tx, indicator_rx) = broadcast::channel::<IndicatorOutput>(16);
        let params = RiskParams::default();
        let (engine, _signal_tx) = TradingEngine::new(
            indicator_rx,
            mock_exchange.clone(),
            RiskManager::new(params),
            "BTC_JPY".into(),
        );
        let engine = engine.with_config(Arc::new(RwLock::new(TradingConfig::default())));

        indicator_tx.send(bullish_output()).unwrap();
        drop(indicator_tx);

        engine.run().await;
        assert_eq!(mock_exchange.placed_orders().len(), 1);
        assert_eq!(mock_exchange.placed_orders()[0].side, OrderSide::Buy);
    }

    #[tokio::test]
    async fn circuit_breaker_blocks_order_after_same_day_drawdown() {
        // Regression test for the daily-drawdown circuit breaker: a sharp
        // same-day price crash after an initial buy must trip the breaker
        // and block the very next rebalance attempt.
        let mock_exchange = Arc::new(MockExchangeClient::with_fee(0.0));
        let (_indicator_tx, indicator_rx) = broadcast::channel::<IndicatorOutput>(16);
        let params = RiskParams {
            min_order_size: dec!(0.001),
            circuit_breaker_enabled: true,
            max_daily_drawdown: 0.05,
        };
        let mut cfg = TradingConfig::default();
        cfg.circuit_breaker_enabled = true;
        cfg.max_daily_drawdown = 0.05;
        let (mut engine, _signal_tx) = TradingEngine::new(
            indicator_rx,
            mock_exchange.clone(),
            RiskManager::new(params),
            "BTC_JPY".into(),
        );
        engine = engine.with_config(Arc::new(RwLock::new(cfg)));

        // First bullish signal: establishes the daily drawdown baseline and buys.
        engine.handle_indicator(bullish_output()).await.unwrap();
        assert_eq!(mock_exchange.placed_orders().len(), 1, "first signal should buy");

        // Crash the price 50% (same simulated day) — the BTC just bought loses
        // half its value, which should exceed the 5% daily drawdown limit.
        let ticker = mock_exchange.get_ticker("BTC_JPY").await.unwrap();
        let crashed_price = ticker.ltp * dec!(0.5);
        mock_exchange.set_ticker(crate::types::market::Ticker {
            ltp: crashed_price,
            best_bid: crashed_price,
            best_ask: crashed_price,
            ..ticker
        });

        // Same-day bullish signal again: the drawdown check should trip the
        // breaker before the (otherwise large) rebalance order is evaluated,
        // so no second order is placed.
        engine.handle_indicator(bullish_output()).await.unwrap();
        assert_eq!(
            mock_exchange.placed_orders().len(),
            1,
            "breaker should block the rebalance triggered by the crash"
        );
    }

    #[tokio::test]
    async fn bearish_allocation_places_sell_order() {
        let mock_exchange = Arc::new(MockExchangeClient::new());
        // BTC を多く保有した状態 → current_alloc 高い → bearish → sell
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
        // sub_signals: RSI=0.2, SMA-cross=0.0, EMA-cross=0.0, MACD-hist=0.0,
        // BB%B=0.75 (close sits near the lower band) → ta_normalized=0.19.
        // combined = 0.19*0.7 + 0.5*0.3 = 0.283 → zone-mapped target≈0.14,
        // far below the ~0.90 already-held allocation → Sell.
        let orders = mock_exchange.placed_orders();
        assert_eq!(orders.len(), 1);
        assert_eq!(orders[0].side, OrderSide::Sell);
    }

    #[tokio::test]
    async fn neutral_signal_after_bullish_does_not_rebalance_to_50pct() {
        // Regression test for sticky target behaviour:
        // 1. Bullish signal establishes sticky_target → Buy
        // 2. Warmup (all-None) signal must NOT reset target to 0.5 and sell
        let mock_exchange = Arc::new(MockExchangeClient::new());
        let (indicator_tx, indicator_rx) = broadcast::channel::<IndicatorOutput>(16);
        let params = RiskParams::default();
        let (engine, _signal_tx) = TradingEngine::new(
            indicator_rx,
            mock_exchange.clone(),
            RiskManager::new(params),
            "BTC_JPY".into(),
        );

        // Bullish signal: sticky_target set → Buy
        indicator_tx.send(bullish_output()).unwrap();
        // Warmup signal: all None → compute_btc_target returns None → sticky_target preserved
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

    // ---- compute_btc_target unit tests ----

    fn make_cfg() -> TradingConfig {
        TradingConfig::default()
    }

    #[test]
    fn compute_btc_target_warmup_all_none_returns_none() {
        // All-None IndicatorPoint → warmup → None
        let raw = IndicatorPoint {
            time: chrono::Utc::now(),
            close: None,
            sma: None, ema: None, rsi: None,
            macd_line: None, signal_line: None, histogram: None,
            bb_upper: None, bb_middle: None, bb_lower: None,
        };
        // No indicator field is present, so no sub-signal can be derived → warmup → None.
        assert_eq!(TradingEngine::compute_btc_target(&raw, &[], &make_cfg()), None);
    }

    #[test]
    fn compute_btc_target_neutral_rsi_returns_some() {
        // RSI=50 → sub_signal=0.5 → neutral TA, no news → Some(0.5)
        let raw = IndicatorPoint {
            time: chrono::Utc::now(),
            close: None,
            sma: None, ema: None, rsi: Some(50.0),
            macd_line: None, signal_line: None, histogram: None,
            bb_upper: None, bb_middle: None, bb_lower: None,
        };
        let result = TradingEngine::compute_btc_target(&raw, &[], &make_cfg());
        assert!(result.is_some());
        let val = result.unwrap();
        // ta_normalized=0.5, sentiment_normalized=0.5 → combined=0.5
        assert!((val - 0.5).abs() < 1e-9, "expected ~0.5, got {}", val);
    }

    #[test]
    fn compute_btc_target_strong_buy_returns_target_above_half() {
        // RSI=20, close > SMA → strong bullish → combined > 0.5
        let raw = IndicatorPoint {
            time: chrono::Utc::now(),
            close: Some(10_100.0),
            sma: Some(10_000.0),
            ema: None, rsi: Some(20.0),
            macd_line: None, signal_line: None, histogram: None,
            bb_upper: None, bb_middle: None, bb_lower: None,
        };
        let result = TradingEngine::compute_btc_target(&raw, &[], &make_cfg());
        // RSI sub_signal=0.8. SMA-cross: (10100-10000)/10000 = +1.0% relative,
        // which is past the 0.5% saturation band → sub_signal=1.0.
        // → ta_normalized=0.9.
        // No news → sentiment_normalized=0.5. combined = 0.9*0.7 + 0.5*0.3 = 0.78.
        let val = result.unwrap();
        assert!((val - 0.78).abs() < 1e-9, "expected ~0.78, got {}", val);
        assert!(val > 0.5);
    }

    #[test]
    fn compute_btc_target_strong_sell_returns_target_below_half() {
        // RSI=80, close < SMA → strong bearish → combined < 0.5
        let raw = IndicatorPoint {
            time: chrono::Utc::now(),
            close: Some(9_900.0),
            sma: Some(10_000.0),
            ema: None, rsi: Some(80.0),
            macd_line: None, signal_line: None, histogram: None,
            bb_upper: None, bb_middle: None, bb_lower: None,
        };
        let result = TradingEngine::compute_btc_target(&raw, &[], &make_cfg());
        // RSI sub_signal=0.2. SMA-cross: (9900-10000)/10000 = -1.0% relative,
        // past the 0.5% saturation band → sub_signal=0.0.
        // → ta_normalized=0.1.
        // No news → sentiment_normalized=0.5. combined = 0.1*0.7 + 0.5*0.3 = 0.22.
        let val = result.unwrap();
        assert!((val - 0.22).abs() < 1e-9, "expected ~0.22, got {}", val);
        assert!(val < 0.5);
    }

    #[test]
    fn compute_btc_target_damped_cross_inside_saturation_band() {
        // close is only +0.25% above both SMA and EMA — half of the 0.5%
        // saturation band — so the damped crossover sub-signals must be 0.75
        // rather than the 1.0 a hard step function would produce.
        let raw = IndicatorPoint {
            time: chrono::Utc::now(),
            close: Some(10_025.0),
            sma: Some(10_000.0),
            ema: Some(10_000.0),
            rsi: Some(50.0),
            macd_line: None, signal_line: None, histogram: None,
            bb_upper: None, bb_middle: None, bb_lower: None,
        };
        let result = TradingEngine::compute_btc_target(&raw, &[], &make_cfg());
        // RSI sub_signal=0.5, SMA-cross=0.75, EMA-cross=0.75
        // → ta_normalized = (0.5 + 0.75 + 0.75) / 3 = 0.666666...
        // No news → sentiment_normalized=0.5.
        // combined = 0.6666667*0.7 + 0.5*0.3 = 0.6166667.
        let val = result.unwrap();
        let expected = (2.0 / 3.0) * 0.7 + 0.5 * 0.3;
        assert!((val - expected).abs() < 1e-9, "expected ~{}, got {}", expected, val);
        // Strictly between neutral and the fully-saturated 0.78 of the strong-buy case.
        assert!(val > 0.5 && val < 0.78);
    }

    #[test]
    fn compute_btc_target_macd_histogram_damped_and_fallback() {
        // With a usable bb_middle the histogram is scaled relative to it:
        // 25 / 10_000 = +0.25% → damped sub_signal = 0.75.
        let damped = IndicatorPoint {
            time: chrono::Utc::now(),
            close: None,
            sma: None, ema: None, rsi: None,
            macd_line: None, signal_line: None, histogram: Some(25.0),
            bb_upper: None, bb_middle: Some(10_000.0), bb_lower: None,
        };
        let val = TradingEngine::compute_btc_target(&damped, &[], &make_cfg()).unwrap();
        // ta_normalized = 0.75 (single sub-signal) → 0.75*0.7 + 0.5*0.3 = 0.675
        assert!((val - 0.675).abs() < 1e-9, "expected ~0.675, got {}", val);

        // Without bb_middle the sub-signal falls back to the sign-based step
        // mapping (positive histogram → 1.0) instead of being dropped.
        let fallback = IndicatorPoint { bb_middle: None, ..damped };
        let val = TradingEngine::compute_btc_target(&fallback, &[], &make_cfg()).unwrap();
        // ta_normalized = 1.0 → 1.0*0.7 + 0.5*0.3 = 0.85
        assert!((val - 0.85).abs() < 1e-9, "expected ~0.85, got {}", val);
    }

    #[test]
    fn compute_btc_target_neutral_ta_strong_news_returns_some() {
        // RSI=50 (neutral TA) but bullish news → Some and > 0.5
        let raw = IndicatorPoint {
            time: chrono::Utc::now(),
            close: None,
            sma: None, ema: None, rsi: Some(50.0),
            macd_line: None, signal_line: None, histogram: None,
            bb_upper: None, bb_middle: None, bb_lower: None,
        };
        let news = vec![SentimentScore {
            headline: "BTC soars".into(),
            score: 0.8,
            analyzed_at: chrono::Utc::now(),
            published_at: None,
        }];
        let result = TradingEngine::compute_btc_target(&raw, &news, &make_cfg());
        // ta_normalized=0.5 (neutral RSI). sentiment_normalized=(0.8+1.0)/2=0.9.
        // combined = 0.5*0.7 + 0.9*0.3 = 0.62.
        let val = result.unwrap();
        assert!((val - 0.62).abs() < 1e-9, "expected ~0.62, got {}", val);
        assert!(val > 0.5);
    }

    #[tokio::test]
    async fn no_directional_signal_skips_rebalance() {
        // When only warmup (all-None) signals arrive with no news,
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

        // Only warmup (all-None) signals — compute_btc_target returns None
        // for both, so sticky_target is never established and no order is
        // ever placed.
        indicator_tx.send(hold_output()).unwrap();
        indicator_tx.send(hold_output()).unwrap();
        drop(indicator_tx);

        engine.run().await;
        assert!(mock_exchange.placed_orders().is_empty());
    }
}
