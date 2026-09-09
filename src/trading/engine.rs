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
    /// The normalized composite signal value ∈ [0.0, 1.0] that produced the
    /// current `sticky_target`. Carried alongside it so the cost-aware
    /// execution filter (`cost_filter_allows`) always has the real conviction
    /// behind the standing target, instead of inventing a neutral 0.5 on
    /// iterations where `compute_btc_target` returned `None` (warm-up /
    /// neutral) or where no fresh indicator point exists at all (the periodic
    /// rebalance tick). Moves in lockstep with `sticky_target`.
    last_normalized: Option<f64>,
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
                last_normalized: None,
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
    /// # Sub-signal family weighting
    ///
    /// The five sub-signals are not combined as a plain average. They are split
    /// into two families and combined as a *weighted* mean:
    ///
    /// - mean-reverting family (RSI, Bollinger %B) — weight
    ///   `REVERSION_SUBSIGNAL_WEIGHT` = 1.0
    /// - trend-following family (SMA cross, EMA cross, MACD histogram) — weight
    ///   `TREND_SUBSIGNAL_WEIGHT` = 0.25
    ///
    /// Rationale (hypothesis `mean-reversion-weighted-subsignals`): over the
    /// 76-day pre-holdout training window (2026-06-11 → 2026-08-26) the lag-1
    /// autocorrelation of hourly BTC/JPY returns measured -0.028, i.e. hourly
    /// returns are weakly *mean-reverting* at exactly the resolution this engine
    /// runs at. The mirror-image experiment (`trend-weighted-subsignals`, trend
    /// 1.0 / reversion 0.25) was the worst variant of its run on every axis
    /// (Sharpe -1.237, max drawdown +1.35pp, win rate 6.7%), which is the
    /// strongest available evidence for tilting the composite the other way.
    /// Every individual sub-signal formula is byte-for-byte unchanged; only the
    /// weights used to combine them differ.
    ///
    /// Because the weighted mean divides by the sum of the weights actually
    /// present, it degrades correctly during warmup: a point where only RSI is
    /// available still yields exactly the RSI sub-signal, and the result always
    /// stays in [0.0, 1.0].
    ///
    /// # Formula
    ///   cross_sub_signal = ((x / ref) / SATURATION).clamp(-1, 1) * 0.5 + 0.5
    ///                                                     ∈ [0.0, 1.0]
    ///   ta_normalized    = Σ(value * weight) / Σ(weight)  ∈ [0.0, 1.0]
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

        /// Aggregation weight of the mean-reverting sub-signals (RSI, Bollinger %B).
        ///
        /// NOTE: hypothesis `mean-reversion-weighted-subsignals`. The lag-1
        /// autocorrelation of hourly BTC/JPY returns over the 76-day pre-holdout
        /// training window (2026-06-11 → 2026-08-26) is -0.028 — weakly
        /// mean-reverting at the 1h resolution this engine trades at — so the
        /// mean-reverting family carries full weight.
        const REVERSION_SUBSIGNAL_WEIGHT: f64 = 1.0;

        /// Aggregation weight of the trend-following sub-signals (SMA cross,
        /// EMA cross, MACD histogram).
        ///
        /// NOTE: same hypothesis. This is the mirror of the rejected
        /// `trend-weighted-subsignals` variant (trend 1.0 / reversion 0.25),
        /// which was that run's worst result on every axis (Sharpe -1.237,
        /// max drawdown +1.35pp, win rate 6.7%). Trend sub-signals are kept
        /// rather than dropped so they still break ties, at quarter influence.
        const TREND_SUBSIGNAL_WEIGHT: f64 = 0.25;

        // (sub-signal value ∈ [0.0, 1.0], family weight) pairs. Combined below
        // as a weighted mean over whichever sub-signals this point supplied.
        let mut sub_signals: Vec<(f64, f64)> = Vec::new();

        // RSI: oversold (low RSI) → bullish (1.0), overbought (high RSI) → bearish (0.0)
        // NOTE: already continuous — intentionally left unchanged.
        if let Some(rsi) = raw.rsi {
            sub_signals.push((1.0 - (rsi / 100.0).clamp(0.0, 1.0), REVERSION_SUBSIGNAL_WEIGHT));
        }

        // SMA cross (damped): price above SMA → bullish, below → bearish, with
        // the swing proportional to the relative gap until it saturates at 0.5%.
        // A non-positive SMA would make the relative distance meaningless, so
        // such a point contributes no sub-signal.
        if let (Some(close), Some(sma)) = (raw.close, raw.sma) {
            if sma > 0.0 {
                sub_signals.push((damped_cross((close - sma) / sma), TREND_SUBSIGNAL_WEIGHT));
            }
        }

        // EMA cross (damped): identical mapping with the EMA as the reference line.
        if let (Some(close), Some(ema)) = (raw.close, raw.ema) {
            if ema > 0.0 {
                sub_signals.push((damped_cross((close - ema) / ema), TREND_SUBSIGNAL_WEIGHT));
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
                    sub_signals.push((damped_cross(hist / bb_middle), TREND_SUBSIGNAL_WEIGHT));
                }
                _ => {
                    let step = if hist > 0.0 { 1.0 } else if hist < 0.0 { 0.0 } else { 0.5 };
                    sub_signals.push((step, TREND_SUBSIGNAL_WEIGHT));
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
            sub_signals.push((1.0 - (pct_b / 100.0).clamp(0.0, 1.0), REVERSION_SUBSIGNAL_WEIGHT));
        }

        // Warmup: no indicator data available yet
        if sub_signals.is_empty() {
            return None;
        }

        // Weighted mean over the sub-signals actually present. Dividing by the
        // realised weight sum (never zero here, since both family weights are
        // positive and the vec is non-empty) keeps the result in [0.0, 1.0] and
        // makes a partial warmup point reduce to exactly its available signals.
        let weight_sum: f64 = sub_signals.iter().map(|(_, w)| w).sum();
        let ta_normalized =
            sub_signals.iter().map(|(v, w)| v * w).sum::<f64>() / weight_sum;

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

    /// Cost-aware execution filter: decide whether a rebalance of size `delta`
    /// is worth the round-trip transaction cost it will incur, given the
    /// conviction behind it.
    ///
    /// This is an *additional* gate that runs alongside — never instead of —
    /// the existing `allocation_threshold` check. It can only ever SUPPRESS a
    /// rebalance that `allocation_threshold` would have allowed; it never
    /// enables one that `allocation_threshold` rejects.
    ///
    /// # Formula
    ///
    /// Adapted from arXiv:2606.00060 (cost-aware execution filter for
    /// walk-forward BTC forecasting), whose rule is
    /// `|r_hat| > lambda * c * |pos* - pos|` — permit a position change only
    /// when the expected one-step return exceeds the cost of making it.
    ///
    /// shirube has no calibrated return forecast, so `|r_hat|` is substituted
    /// with a conviction proxy: the composite signal's distance from neutral,
    /// rescaled to a per-bar expected move.
    ///
    /// ```text
    /// conviction   = 2 * |normalized_signal - 0.5|            ∈ [0.0, 1.0]
    /// expected_move = conviction * EXPECTED_MOVE_PER_BAR      ∈ [0, 6.5e-4]
    /// cost          = LAMBDA * ROUND_TRIP_COST_PCT * |delta|
    /// allow         = expected_move > cost
    /// ```
    ///
    /// `normalized_signal` is the value `compute_btc_target` returned (or, on
    /// an iteration with no fresh value, the last one it returned — see
    /// `last_normalized`); `delta` is the signed allocation change in
    /// fractional units (0.1 = shift 10% of portfolio value).
    ///
    /// NOTE: the substitution of a signal-distance proxy for a calibrated
    /// return forecast is this port's main risk — the source paper says
    /// nothing about it. See `experiments/hypotheses/cost-aware-rebalance-filter.json`.
    pub(crate) fn cost_filter_allows(normalized_signal: f64, delta: f64) -> bool {
        /// Strictness multiplier on the cost term. Source: arXiv:2606.00060
        /// uses lambda = 2.0 for its reported best configuration — a published
        /// figure, not fitted on any shirube data.
        const LAMBDA: f64 = 2.0;

        /// All-in round-trip cost of a rebalance, as a fraction of the traded
        /// notional. 0.25% = bitFlyer's 0.15% top-tier taker fee plus the
        /// backtest pipeline's 0.1% default `--slippage-pct`. Source: published
        /// venue fee schedule + pipeline default, not fitted on any window.
        const ROUND_TRIP_COST_PCT: f64 = 0.0025;

        /// Typical absolute size of a one-bar BTC/JPY return, used to convert
        /// the unitless conviction proxy into a comparable return magnitude.
        /// 0.00065 = the standard deviation of 1-minute BTC/JPY returns
        /// (0.0647%) measured over the PRE-HOLDOUT training window only,
        /// 2026-08-09T04:03Z → 2026-08-26T19:15Z (18,648 bars). Derived from
        /// data strictly older than HOLDOUT_START, so no holdout data enters
        /// this constant.
        const EXPECTED_MOVE_PER_BAR: f64 = 0.00065;

        // Conviction proxy: distance from the neutral 0.5 midpoint, doubled so
        // that a fully saturated signal (0.0 or 1.0) maps to 1.0.
        let conviction = 2.0 * (normalized_signal - 0.5).abs();
        let expected_move = conviction * EXPECTED_MOVE_PER_BAR;
        let cost = LAMBDA * ROUND_TRIP_COST_PCT * delta.abs();

        expected_move > cost
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
            // Keep the conviction that produced this target in lockstep with it,
            // so the cost filter below always gates on the real signal strength
            // rather than a neutral placeholder.
            self.last_normalized = Some(agg_normalized);
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
            // Two independent gates, both of which must permit the rebalance:
            //   1. allocation_threshold — fixed minimum move size (pre-existing)
            //   2. cost_filter_allows   — conviction must outweigh round-trip cost
            // The cost filter is applied to the delta *after* compute_btc_target
            // has produced its target; it never changes the target itself.
            let gate_normalized = self.last_normalized.unwrap_or(0.5);
            if delta.abs() < allocation_threshold
                || !Self::cost_filter_allows(gate_normalized, delta)
            {
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
            // Same two-gate rule as handle_indicator. No fresh normalized value
            // exists on a periodic rebalance tick, so the conviction carried
            // alongside the sticky target is used rather than a made-up one.
            let gate_normalized = self.last_normalized.unwrap_or(0.5);
            if delta.abs() < allocation_threshold
                || !Self::cost_filter_allows(gate_normalized, delta)
            {
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

    /// Build an IndicatorOutput whose every sub-signal is fully bullish, so
    /// `compute_btc_target` returns the saturated maximum (ta_normalized = 1.0
    /// → combined = 1.0*0.7 + 0.5*0.3 = 0.85 with an empty news slice).
    ///
    /// NOTE: added for the cost-aware execution filter. The filter admits a
    /// rebalance only while `|delta| < 0.26 * |normalized - 0.5|`, so the
    /// partially-bullish `bullish_output()` (normalized ≈ 0.589, band width
    /// 0.023) leaves almost no room to place an order at all. Tests that need
    /// to exercise the order path use this saturated fixture instead, which
    /// gives the widest band the filter can ever offer (0.091).
    fn saturated_bullish_output() -> IndicatorOutput {
        IndicatorOutput {
            indicators: vec![IndicatorSignal { name: "RSI".into(), value: Some(0.0) }],
            raw: IndicatorPoint {
                time: chrono::Utc::now(),
                close: Some(9_800.0),
                sma: Some(9_000.0),      // close 8.9% above → cross saturated 1.0
                ema: Some(9_000.0),      // close 8.9% above → cross saturated 1.0
                rsi: Some(0.0),          // maximally oversold → 1.0
                macd_line: Some(50.0),
                signal_line: Some(30.0),
                histogram: Some(100.0),  // strongly positive → saturated 1.0
                bb_upper: Some(10_200.0),
                bb_middle: Some(10_000.0),
                bb_lower: Some(9_800.0), // close == lower band → %B = 0 → 1.0
            },
            calculated_at: chrono::Utc::now(),
        }
    }

    /// Mirror image of `saturated_bullish_output()`: every sub-signal fully
    /// bearish, so `compute_btc_target` returns 0.15 (the saturated minimum).
    fn saturated_bearish_output() -> IndicatorOutput {
        IndicatorOutput {
            indicators: vec![IndicatorSignal { name: "RSI".into(), value: Some(100.0) }],
            raw: IndicatorPoint {
                time: chrono::Utc::now(),
                close: Some(10_200.0),
                sma: Some(11_000.0),      // close 7.3% below → cross saturated 0.0
                ema: Some(11_000.0),      // close 7.3% below → cross saturated 0.0
                rsi: Some(100.0),         // maximally overbought → 0.0
                macd_line: Some(-50.0),
                signal_line: Some(-30.0),
                histogram: Some(-100.0),  // strongly negative → saturated 0.0
                bb_upper: Some(10_200.0),
                bb_middle: Some(10_000.0),
                bb_lower: Some(9_800.0),  // close == upper band → %B = 100 → 0.0
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
        // price=9_000_000, fee=0. Starting allocation 95% BTC
        // (BTC 0.095 = 855_000 JPY, JPY 45_000 → total 900_000).
        //
        // NOTE: rebalancing in from 0% BTC — as this test used to — is no
        // longer reachable under the cost-aware execution filter, which admits
        // a rebalance only while |delta| < 0.26 * |normalized - 0.5| (at most
        // 0.091, at full saturation). The scenario is therefore a saturated
        // bullish signal (normalized 0.85 → zone target 1.0) against an
        // already-high 95% allocation, giving delta = 0.05: above
        // allocation_threshold and inside the cost filter's band, so both
        // gates permit the order and the Buy path is still covered end to end.
        let mock_exchange = Arc::new(MockExchangeClient::with_fee(0.0));
        mock_exchange.set_price(dec!(9_000_000));
        mock_exchange.set_balances(vec![
            crate::types::balance::Balance {
                currency_code: "JPY".to_string(),
                amount: dec!(45_000),
                available: dec!(45_000),
            },
            crate::types::balance::Balance {
                currency_code: "BTC".to_string(),
                amount: dec!(0.095),
                available: dec!(0.095),
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
        let mut cfg = TradingConfig::default();
        cfg.allocation_threshold = 0.01;
        let engine = engine.with_config(Arc::new(RwLock::new(cfg)));

        indicator_tx.send(saturated_bullish_output()).unwrap();
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
        mock_exchange.set_price(dec!(9_000_000));
        // Start at 75% BTC (0.075 BTC = 675_000 JPY, JPY 225_000 → 900_000).
        mock_exchange.set_balances(vec![
            crate::types::balance::Balance {
                currency_code: "JPY".to_string(),
                amount: dec!(225_000),
                available: dec!(225_000),
            },
            crate::types::balance::Balance {
                currency_code: "BTC".to_string(),
                amount: dec!(0.075),
                available: dec!(0.075),
            },
        ]);
        let (_indicator_tx, indicator_rx) = broadcast::channel::<IndicatorOutput>(16);
        let params = RiskParams {
            min_order_size: dec!(0.001),
            circuit_breaker_enabled: true,
            max_daily_drawdown: 0.05,
        };
        let mut cfg = TradingConfig::default();
        cfg.circuit_breaker_enabled = true;
        cfg.max_daily_drawdown = 0.05;
        // NOTE: sized so BOTH the second rebalance's gates permit an order and
        // only the circuit breaker refuses it — otherwise the assertion below
        // would pass vacuously because the cost filter had already suppressed
        // the rebalance. hold_btc_above = 1.0 maps the saturated signal
        // (normalized 0.85) to target 0.8125 rather than an all-in 1.0, so the
        // post-crash allocation drift stays inside the filter's band.
        cfg.allocation_threshold = 0.01;
        cfg.zone.hold_btc_above = 1.0;
        let (mut engine, _signal_tx) = TradingEngine::new(
            indicator_rx,
            mock_exchange.clone(),
            RiskManager::new(params),
            "BTC_JPY".into(),
        );
        engine = engine.with_config(Arc::new(RwLock::new(cfg)));

        // First bullish signal: establishes the daily drawdown baseline and buys
        // from 75% up to the 81.25% target (delta 0.0625, inside both gates).
        engine.handle_indicator(saturated_bullish_output()).await.unwrap();
        assert_eq!(mock_exchange.placed_orders().len(), 1, "first signal should buy");

        // Crash the price 15% (same simulated day): equity falls ~12.2%, well
        // past the 5% daily drawdown limit, while the resulting allocation
        // drift (delta ≈ 0.026) still clears both allocation gates.
        let ticker = mock_exchange.get_ticker("BTC_JPY").await.unwrap();
        let crashed_price = ticker.ltp * dec!(0.85);
        mock_exchange.set_ticker(crate::types::market::Ticker {
            ltp: crashed_price,
            best_bid: crashed_price,
            best_ask: crashed_price,
            ..ticker
        });

        // Same-day bullish signal again: the drawdown check should trip the
        // breaker before the rebalance order is evaluated, so no second order
        // is placed.
        engine.handle_indicator(saturated_bullish_output()).await.unwrap();
        assert_eq!(
            mock_exchange.placed_orders().len(),
            1,
            "breaker should block the rebalance triggered by the crash"
        );
    }

    #[tokio::test]
    async fn bearish_allocation_places_sell_order() {
        let mock_exchange = Arc::new(MockExchangeClient::new());
        mock_exchange.set_price(dec!(9_000_000));
        // Mirror of the Buy test: a saturated bearish signal (normalized 0.15
        // → zone target 0.0) against a small 5% BTC allocation
        // (BTC 0.005 = 45_000 JPY, JPY 855_000 → total 900_000), so
        // delta = -0.05 clears allocation_threshold while staying inside the
        // cost filter's 0.091-wide band at full saturation.
        mock_exchange.set_balances(vec![
            crate::types::balance::Balance {
                currency_code: "JPY".to_string(),
                amount: dec!(855_000),
                available: dec!(855_000),
            },
            crate::types::balance::Balance {
                currency_code: "BTC".to_string(),
                amount: dec!(0.005),
                available: dec!(0.005),
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
        let mut cfg = TradingConfig::default();
        cfg.allocation_threshold = 0.01;
        let engine = engine.with_config(Arc::new(RwLock::new(cfg)));

        indicator_tx.send(saturated_bearish_output()).unwrap();
        drop(indicator_tx);

        engine.run().await;
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
        // RSI sub_signal=0.8 (mean-reverting, weight 1.0). SMA-cross:
        // (10100-10000)/10000 = +1.0% relative, past the 0.5% saturation band
        // → sub_signal=1.0 (trend-following, weight 0.25).
        // → ta_normalized = (0.8*1.0 + 1.0*0.25) / 1.25 = 0.84.
        // No news → sentiment_normalized=0.5. combined = 0.84*0.7 + 0.5*0.3 = 0.738.
        let val = result.unwrap();
        let expected = (0.8 * 1.0 + 1.0 * 0.25) / 1.25 * 0.7 + 0.5 * 0.3;
        assert!((val - expected).abs() < 1e-9, "expected ~{}, got {}", expected, val);
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
        // RSI sub_signal=0.2 (weight 1.0). SMA-cross: (9900-10000)/10000 = -1.0%
        // relative, past the 0.5% saturation band → sub_signal=0.0 (weight 0.25).
        // → ta_normalized = (0.2*1.0 + 0.0*0.25) / 1.25 = 0.16.
        // No news → sentiment_normalized=0.5. combined = 0.16*0.7 + 0.5*0.3 = 0.262.
        let val = result.unwrap();
        let expected = (0.2 * 1.0 + 0.0 * 0.25) / 1.25 * 0.7 + 0.5 * 0.3;
        assert!((val - expected).abs() < 1e-9, "expected ~{}, got {}", expected, val);
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
        // RSI sub_signal=0.5 (weight 1.0), SMA-cross=0.75, EMA-cross=0.75
        // (weight 0.25 each) → ta_normalized =
        //   (0.5*1.0 + 0.75*0.25 + 0.75*0.25) / 1.5 = 0.5833333...
        // No news → sentiment_normalized=0.5.
        // combined = 0.5833333*0.7 + 0.5*0.3 = 0.5583333.
        let val = result.unwrap();
        let expected = (0.5 * 1.0 + 0.75 * 0.25 + 0.75 * 0.25) / 1.5 * 0.7 + 0.5 * 0.3;
        assert!((val - expected).abs() < 1e-9, "expected ~{}, got {}", expected, val);
        // Strictly between neutral and the fully-saturated 0.738 of the strong-buy case.
        assert!(val > 0.5 && val < 0.738);
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
        // ta_normalized = 0.75: a single sub-signal divides by its own weight
        // (0.75*0.25 / 0.25), so family weighting cannot change a lone value.
        // → 0.75*0.7 + 0.5*0.3 = 0.675
        assert!((val - 0.675).abs() < 1e-9, "expected ~0.675, got {}", val);

        // Without bb_middle the sub-signal falls back to the sign-based step
        // mapping (positive histogram → 1.0) instead of being dropped.
        let fallback = IndicatorPoint { bb_middle: None, ..damped };
        let val = TradingEngine::compute_btc_target(&fallback, &[], &make_cfg()).unwrap();
        // ta_normalized = 1.0 → 1.0*0.7 + 0.5*0.3 = 0.85
        assert!((val - 0.85).abs() < 1e-9, "expected ~0.85, got {}", val);
    }

    #[test]
    fn compute_btc_target_mean_reverting_family_dominates_trend_disagreement() {
        // Sub-signal families point in opposite directions:
        //   trend-following  — close +1% over both SMA and EMA and a +1% MACD
        //                      histogram → all three saturate at 1.0 (bullish)
        //   mean-reverting   — RSI=90 (→0.1) and close pinned to the upper
        //                      Bollinger band, %B=100 (→0.0) (both bearish)
        // Under the weighted mean the mean-reverting family (weight 1.0 each)
        // must outweigh the trend family (weight 0.25 each), so the composite
        // comes out bearish. The old unweighted mean would have produced
        // (0.1+0.0+1.0+1.0+1.0)/5 = 0.62 → combined 0.584, i.e. bullish, so this
        // assertion genuinely discriminates between the two aggregations.
        let raw = IndicatorPoint {
            time: chrono::Utc::now(),
            close: Some(10_100.0),
            sma: Some(10_000.0),
            ema: Some(10_000.0),
            rsi: Some(90.0),
            macd_line: None,
            signal_line: None,
            histogram: Some(100.0),
            bb_upper: Some(10_100.0),
            bb_middle: Some(10_000.0),
            bb_lower: Some(9_900.0),
        };
        let val = TradingEngine::compute_btc_target(&raw, &[], &make_cfg()).unwrap();
        // ta_normalized = (0.1*1.0 + 0.0*1.0 + 1.0*0.25 * 3) / (1.0 + 1.0 + 0.75)
        //               = 0.85 / 2.75 = 0.309090...
        // combined = 0.309090*0.7 + 0.5*0.3 = 0.366363...
        let expected = (0.1 + 0.0 + 3.0 * 0.25) / 2.75 * 0.7 + 0.5 * 0.3;
        assert!((val - expected).abs() < 1e-9, "expected ~{}, got {}", expected, val);
        assert!(
            val < 0.5,
            "mean-reverting family must dominate: expected a bearish composite, got {}",
            val
        );
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

    // ---- cost_filter_allows unit tests ----
    //
    // Gate: 2*|normalized-0.5| * 6.5e-4  >  2.0 * 0.0025 * |delta|
    // ⇔ |delta| < |normalized-0.5| * 0.26
    // A fully saturated signal at ta_weight 0.7 with an empty news slice is
    // 0.85 (or 0.15), i.e. |normalized-0.5| = 0.35, so the largest delta a
    // saturated signal can ever pass is 0.35 * 0.26 = 0.091.

    #[test]
    fn cost_filter_saturated_signal_passes_small_delta() {
        // Saturated bullish signal (0.85) with a 5% allocation move: the
        // conviction term clears the cost term, so the trade is permitted.
        assert!(TradingEngine::cost_filter_allows(0.85, 0.05));
        // Mirror-image bearish saturation behaves identically (sign-symmetric).
        assert!(TradingEngine::cost_filter_allows(0.15, -0.05));
    }

    #[test]
    fn cost_filter_near_neutral_signal_rejects_same_delta() {
        // Same 5% move, but on a barely-off-neutral signal: the conviction term
        // is ~1/17 of the saturated case and no longer covers the cost.
        assert!(!TradingEngine::cost_filter_allows(0.52, 0.05));
        assert!(!TradingEngine::cost_filter_allows(0.48, -0.05));
        // Exactly neutral has zero conviction and can never pass any delta.
        assert!(!TradingEngine::cost_filter_allows(0.5, 0.001));
    }

    #[test]
    fn cost_filter_is_monotone_in_delta() {
        // Property: for fixed conviction, a larger |delta| is never easier to
        // pass than a smaller one — cost grows linearly in |delta| while the
        // conviction term is independent of it.
        for &normalized in &[0.15, 0.3, 0.5, 0.62, 0.85, 1.0] {
            let mut prev = true;
            for step in 0..60 {
                let delta = step as f64 * 0.005;
                let allowed = TradingEngine::cost_filter_allows(normalized, delta);
                assert!(
                    !(allowed && !prev),
                    "gate re-opened at larger delta: normalized={} delta={}",
                    normalized,
                    delta
                );
                prev = allowed;
            }
        }
    }

    #[test]
    fn cost_filter_rejects_every_trade_at_default_allocation_threshold() {
        // FINDING (documented, not a design goal): with the hypothesis's own
        // trading_config (allocation_threshold = 0.1) the two gates are
        // mutually exclusive. allocation_threshold requires |delta| >= 0.1,
        // while even a fully saturated 0.85/0.15 signal only clears the cost
        // gate below |delta| = 0.091. The composite gate therefore admits no
        // trade at all under that config.
        //
        // The constants are deliberately left as the hypothesis specifies
        // them (LAMBDA 2.0, ROUND_TRIP_COST_PCT 0.0025, EXPECTED_MOVE_PER_BAR
        // 0.00065) rather than retuned to make this test pass — see the
        // hypothesis file's instruction to report the finding instead.
        assert!(!TradingEngine::cost_filter_allows(0.85, 0.1));
        assert!(!TradingEngine::cost_filter_allows(0.15, -0.1));
        // The break-even delta for full saturation, for the record.
        assert!(TradingEngine::cost_filter_allows(0.85, 0.0909));
        assert!(!TradingEngine::cost_filter_allows(0.85, 0.0911));
    }

    #[test]
    fn cost_filter_never_enables_a_trade_allocation_threshold_rejects() {
        // Structural guarantee: the filter is only ever consulted with AND, so
        // it can only subtract trades. Verified here at the decision level —
        // for any (normalized, delta) pair, allowed-by-both implies
        // allowed-by-threshold.
        let threshold = 0.1_f64;
        for step in 0..200 {
            let delta = step as f64 * 0.002 - 0.2;
            for &normalized in &[0.0, 0.15, 0.5, 0.85, 1.0] {
                let both = delta.abs() >= threshold
                    && TradingEngine::cost_filter_allows(normalized, delta);
                if both {
                    assert!(delta.abs() >= threshold);
                }
            }
        }
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
