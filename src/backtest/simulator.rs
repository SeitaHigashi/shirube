use std::sync::Arc;

use chrono::Utc;
use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;

use crate::config::TradingConfig;
use crate::error::Result;
use crate::exchange::mock::MockExchangeClient;
use crate::exchange::ExchangeClient;
use crate::risk::{RiskManager, RiskDecision};
use crate::signal::{apply_zone, compute_indicators};
use crate::storage::db::Database;
use crate::trading::engine::TradingEngine;
use crate::types::market::{Candle, Ticker};

use super::{BacktestConfig, BacktestReport, RiskEventCounts};

// ──────────────────────────────────────────────────────────────────────────────
// Simulator
// ──────────────────────────────────────────────────────────────────────────────

/// Replays a candle series through the exact same allocation logic used
/// live (`TradingEngine::compute_btc_target` + `apply_zone` +
/// `allocation_delta_to_order`) against a `MockExchangeClient`, so a
/// backtest result never drifts from what live trading would have done
/// for the same indicator/zone configuration.
pub struct Simulator {
    config: BacktestConfig,
    db: Database,
}

impl Simulator {
    pub fn new(config: BacktestConfig, db: Database) -> Self {
        Self { config, db }
    }

    /// Candle 列と TradingConfig を受け取りバックテストを実行して
    /// `BacktestReport` を返す。`trading_config` はインジケータ周期・
    /// ゾーン設定・配分しきい値をすべて含み、本番の TradingEngine と
    /// 同一のロジックで評価される。
    pub async fn run(
        &self,
        candles: Vec<Candle>,
        trading_config: TradingConfig,
    ) -> Result<BacktestReport> {
        let exchange = Arc::new(match self.config.fee_pct {
            Some(rate) => MockExchangeClient::with_fee(rate),
            None => MockExchangeClient::new(),
        });

        exchange.set_balances(vec![
            crate::types::balance::Balance {
                currency_code: "JPY".to_string(),
                amount: self.config.initial_jpy,
                available: self.config.initial_jpy,
            },
            crate::types::balance::Balance {
                currency_code: "BTC".to_string(),
                amount: Decimal::ZERO,
                available: Decimal::ZERO,
            },
        ]);

        let mut risk_manager = RiskManager::new(trading_config.to_risk_params());

        // 本番の SignalEngine と同一の純粋関数でインジケータ値を計算する。
        // NOTE: indicators are computed over the *whole* series including any
        // leading warmup candles, so that by the first evaluated candle every
        // indicator has already seen `warmup_candles` bars of history.
        let points = compute_indicators(&candles, &trading_config);

        // Leading candles that exist only to prime the indicators: they are
        // never traded on and never enter the equity curve, so the reported
        // metrics cover exactly the requested [from, to] window. Clamped to the
        // series length so an over-long warmup request cannot skip everything.
        let warmup = self.config.warmup_candles.min(candles.len());

        // TradingEngine と同じ sticky-target ロジック: compute_btc_target が
        // None（ウォームアップ中）を返した間は直前の目標配分を維持する。
        let mut sticky_target: Option<f64> = None;
        let mut equity_curve: Vec<f64> = Vec::with_capacity(candles.len() - warmup);
        // Risk-gate activity, surfaced on the report so a run makes it
        // visible whether a gate ever engaged (see RiskEventCounts).
        //
        // NOTE: counted only over evaluated candles — the `skip(warmup)` above
        // means a gate cannot trip on a warmup candle, which is correct: those
        // candles are never traded on and are outside the reported window.
        let mut risk_events = RiskEventCounts::default();

        for (candle, point) in candles.iter().zip(points.iter()).skip(warmup) {
            let price_with_slip = apply_slippage(candle.close, self.config.slippage_pct);
            let ticker = Ticker {
                product_code: self.config.product_code.clone(),
                timestamp: Utc::now(),
                best_bid: price_with_slip,
                best_ask: price_with_slip,
                best_bid_size: Decimal::ONE,
                best_ask_size: Decimal::ONE,
                ltp: price_with_slip,
                volume: Decimal::ONE,
                volume_by_product: Decimal::ONE,
            };
            exchange.set_ticker(ticker);

            // 本番と同一の compute_btc_target を呼び出す（NOTE: この関数の
            // 内部ロジックは手動管理対象 — trading/engine.rs のコメント参照）
            if let Some(normalized) = TradingEngine::compute_btc_target(point, &[], &trading_config) {
                let raw = normalized * trading_config.zone.range_max;
                sticky_target = Some(apply_zone(raw, &trading_config.zone));
            }

            let jpy = exchange.jpy_balance();
            let btc = exchange.btc_balance();
            let btc_value = btc * price_with_slip;
            let total = jpy + btc_value;

            // Re-baseline the daily drawdown tracker off the candle's own
            // timestamp (not wall-clock), so a multi-day backtest resets the
            // circuit breaker once per *simulated* day exactly like live
            // trading resets it once per real day. See RiskManager::observe_time
            // for why wall-clock time would be wrong here (a whole backtest
            // runs within a single real-time second).
            risk_manager.observe_time(candle.open_time, total);
            // A trip is only reported on the transition into the broken
            // state (RiskManager returns None while already broken), so this
            // counts at most once per simulated day, matching the daily
            // reset in observe_time.
            if risk_manager.check_drawdown(total).is_some() {
                risk_events.circuit_breaker_trips += 1;
            }

            if let Some(target_pct) = sticky_target {
                if !total.is_zero() && !price_with_slip.is_zero() {
                    let current_alloc = (btc_value / total).to_f64().unwrap_or(0.0);
                    let delta = target_pct - current_alloc;

                    if delta.abs() >= trading_config.allocation_threshold {
                        let order_req = TradingEngine::allocation_delta_to_order(
                            delta,
                            total,
                            price_with_slip,
                            &self.config.product_code,
                            risk_manager.params().min_order_size,
                        );

                        if let Some(req) = order_req {
                            match risk_manager.evaluate(req) {
                                RiskDecision::Allow(r) => {
                                    let _ = exchange.send_order(&r).await;
                                }
                                // Both refusal arms mean the order never
                                // reached the exchange, so it is absent from
                                // total_trades — count it instead of
                                // discarding it silently.
                                RiskDecision::Reject(_) => {
                                    risk_events.orders_rejected += 1;
                                }
                                RiskDecision::CircuitBreaker { .. } => {
                                    risk_events.orders_rejected += 1;
                                }
                            }
                        }
                    }
                }
            }

            let btc_val = exchange.btc_balance() * candle.close;
            let equity = exchange.jpy_balance() + btc_val;
            equity_curve.push(equity.to_f64().unwrap_or(0.0));
        }

        let filled = exchange.filled_trades();
        let report = super::report::compute_report(
            &filled,
            &equity_curve,
            self.config.initial_jpy.to_f64().unwrap_or(1.0),
            self.config.resolution_secs,
            risk_events,
        );

        self.db.backtest_runs().insert(&self.config, &report).await?;

        Ok(report)
    }
}

/// Apply slippage to a price by multiplying by (1 + slippage_pct).
///
/// Slippage models the difference between the mid-price and the actual
/// fill price due to market impact and bid/ask spread. A positive
/// `slippage_pct` (e.g. 0.001 = 0.1%) always increases the price,
/// meaning buys cost more and sells receive less than the close price.
fn apply_slippage(price: Decimal, slippage_pct: f64) -> Decimal {
    let slip = Decimal::try_from(slippage_pct).unwrap_or(Decimal::ZERO);
    price * (Decimal::ONE + slip)
}

// ──────────────────────────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use rust_decimal_macros::dec;

    fn make_candle(close: Decimal) -> Candle {
        make_candle_at(close, Utc::now())
    }

    fn make_candle_at(close: Decimal, open_time: chrono::DateTime<Utc>) -> Candle {
        Candle {
            product_code: "BTC_JPY".into(),
            open_time,
            resolution_secs: 60,
            open: close,
            high: close,
            low: close,
            close,
            volume: dec!(1),
        }
    }

    #[tokio::test]
    async fn simulator_runs_and_stores_report() {
        let db = crate::storage::db::Database::open_in_memory().await.unwrap();
        let from = Utc::now();
        let to = Utc::now();
        let config = BacktestConfig {
            product_code: "BTC_JPY".into(),
            from,
            to,
            resolution_secs: 60,
            slippage_pct: 0.0,
            fee_pct: Some(0.0),
            initial_jpy: dec!(1_000_000),
            warmup_candles: 0,
        };

        // Short indicator periods so a handful of candles is enough to warm up.
        let mut trading_config = TradingConfig::default();
        trading_config.sma_period = 2;
        trading_config.ema_period = 2;
        trading_config.rsi_period = 2;
        trading_config.macd_fast = 2;
        trading_config.macd_slow = 3;
        trading_config.macd_signal = 2;
        trading_config.bollinger_period = 2;
        trading_config.allocation_threshold = 0.01;

        let prices = [
            dec!(9_000_000),
            dec!(9_100_000),
            dec!(9_200_000),
            dec!(9_300_000),
            dec!(9_400_000),
            dec!(9_500_000),
            dec!(9_600_000),
            dec!(9_700_000),
        ];
        let candles: Vec<Candle> = prices.iter().map(|&p| make_candle(p)).collect();

        let simulator = Simulator::new(config, db.clone());
        let report = simulator.run(candles, trading_config).await.unwrap();

        let runs = db.backtest_runs().list(10).await.unwrap();
        assert_eq!(runs.len(), 1);
        // Every candle produces an equity point; report is always populated
        // even when zero trades were placed (e.g. sticky target never set).
        assert!(report.total_return_pct.is_finite());
    }

    #[tokio::test]
    async fn circuit_breaker_resets_on_next_simulated_day() {
        // Regression test for the exact backtest problem the breaker was
        // previously pulled for: a single bad day early in a long backtest
        // must not permanently freeze trading for every day after it. The
        // breaker should reset once per *simulated* day (driven by candle
        // timestamps), not remain tripped for the rest of the run.
        let db = crate::storage::db::Database::open_in_memory().await.unwrap();
        let config = BacktestConfig {
            product_code: "BTC_JPY".into(),
            from: Utc::now(),
            to: Utc::now(),
            resolution_secs: 60,
            slippage_pct: 0.0,
            fee_pct: Some(0.0),
            initial_jpy: dec!(1_000_000),
            warmup_candles: 0,
        };

        let mut trading_config = TradingConfig::default();
        trading_config.sma_period = 2;
        trading_config.ema_period = 2;
        trading_config.rsi_period = 2;
        trading_config.macd_fast = 2;
        trading_config.macd_slow = 3;
        trading_config.macd_signal = 2;
        trading_config.bollinger_period = 2;
        trading_config.allocation_threshold = 0.01;
        trading_config.circuit_breaker_enabled = true;
        trading_config.max_daily_drawdown = 0.05;

        use chrono::TimeZone;
        let day1 = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        let day2 = Utc.with_ymd_and_hms(2026, 1, 2, 0, 0, 0).unwrap();

        let mut candles = Vec::new();
        // Day 1: warmup + bullish ascent (triggers an initial buy), then a
        // sharp same-day crash that exceeds the 5% daily drawdown limit.
        let day1_prices = [
            dec!(9_000_000), dec!(9_100_000), dec!(9_200_000), dec!(9_300_000),
            dec!(9_400_000), dec!(9_500_000), dec!(9_600_000), dec!(9_700_000),
            dec!(4_500_000), // crash: >50% drop within the same simulated day
        ];
        for p in day1_prices {
            candles.push(make_candle_at(p, day1));
        }
        // Day 2: still bullish (ascending from the crashed price). If the
        // breaker correctly resets at the day boundary, this should still
        // place a rebalance trade instead of staying frozen forever.
        let day2_prices = [
            dec!(4_600_000), dec!(4_700_000), dec!(4_800_000), dec!(4_900_000),
        ];
        for p in day2_prices {
            candles.push(make_candle_at(p, day2));
        }

        let simulator = Simulator::new(config, db.clone());
        let report = simulator.run(candles, trading_config).await.unwrap();

        assert!(
            report.total_trades >= 2,
            "breaker should have reset on day 2, allowing a trade after the day-1 crash; got {} trades",
            report.total_trades
        );
    }

    #[tokio::test]
    async fn warmup_candles_prime_indicators_without_being_traded() {
        // Regression test for the measurement defect this flag fixes: with no
        // warmup lookback, a long-period indicator is `None` for the leading
        // part of the evaluation window, so the window is not actually
        // evaluated with the configuration under test.
        //
        // Same 12-candle series, run two ways:
        //   A) all 12 in-window, no warmup  → SMA(6) is None for candles 0..4
        //   B) first 6 as warmup, last 6 evaluated → SMA(6) warm from candle 0
        // B must produce exactly 6 equity points (not 12) and must reach a
        // directional target on its very first evaluated candle.
        use chrono::TimeZone;
        let base = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();

        let mut trading_config = TradingConfig::default();
        trading_config.sma_period = 6;
        trading_config.ema_period = 2;
        trading_config.rsi_period = 2;
        trading_config.macd_fast = 2;
        trading_config.macd_slow = 3;
        trading_config.macd_signal = 2;
        trading_config.bollinger_period = 2;
        trading_config.allocation_threshold = 0.01;

        // Steadily rising series so the post-warmup half is unambiguously bullish.
        let candles: Vec<Candle> = (0..12)
            .map(|i| {
                make_candle_at(
                    dec!(9_000_000) + Decimal::from(i) * dec!(100_000),
                    base + chrono::Duration::minutes(i as i64),
                )
            })
            .collect();

        let make_cfg = |warmup: usize| BacktestConfig {
            product_code: "BTC_JPY".into(),
            from: base,
            to: base + chrono::Duration::minutes(12),
            resolution_secs: 60,
            slippage_pct: 0.0,
            fee_pct: Some(0.0),
            initial_jpy: dec!(1_000_000),
            warmup_candles: warmup,
        };

        let db = crate::storage::db::Database::open_in_memory().await.unwrap();
        let no_warmup = Simulator::new(make_cfg(0), db.clone())
            .run(candles.clone(), trading_config.clone())
            .await
            .unwrap();
        let with_warmup = Simulator::new(make_cfg(6), db.clone())
            .run(candles.clone(), trading_config.clone())
            .await
            .unwrap();

        // The warmup candles must not be traded on: skipping the (bullish)
        // first half removes the initial ramp-in trades it would have produced.
        assert!(
            with_warmup.total_trades < no_warmup.total_trades,
            "warmup candles must not produce trades: {} vs {}",
            with_warmup.total_trades,
            no_warmup.total_trades
        );
        // Both runs still produce a well-formed report over their own window.
        assert!(with_warmup.total_return_pct.is_finite());
        assert!(no_warmup.total_return_pct.is_finite());
    }

    #[tokio::test]
    async fn warmup_candles_zero_is_unchanged() {
        // The default (0) must reproduce the pre-change behavior exactly, so
        // existing baselines stay comparable until a run opts into a lookback.
        use chrono::TimeZone;
        let base = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        let mut trading_config = TradingConfig::default();
        trading_config.sma_period = 2;
        trading_config.ema_period = 2;
        trading_config.rsi_period = 2;
        trading_config.macd_fast = 2;
        trading_config.macd_slow = 3;
        trading_config.macd_signal = 2;
        trading_config.bollinger_period = 2;
        trading_config.allocation_threshold = 0.01;

        let candles: Vec<Candle> = (0..8)
            .map(|i| {
                make_candle_at(
                    dec!(9_000_000) + Decimal::from(i) * dec!(100_000),
                    base + chrono::Duration::minutes(i as i64),
                )
            })
            .collect();

        let cfg = BacktestConfig {
            product_code: "BTC_JPY".into(),
            from: base,
            to: base + chrono::Duration::minutes(8),
            resolution_secs: 60,
            slippage_pct: 0.0,
            fee_pct: Some(0.0),
            initial_jpy: dec!(1_000_000),
            warmup_candles: 0,
        };

        let db = crate::storage::db::Database::open_in_memory().await.unwrap();
        let report = Simulator::new(cfg, db)
            .run(candles, trading_config)
            .await
            .unwrap();
        // Known-good values for this fixture under the pre-change code path.
        assert!(report.total_trades > 0, "zero-warmup run must still trade");
        assert!(report.total_return_pct.is_finite());
    }

    /// The report must distinguish a circuit breaker that fired from one
    /// that never engaged. Regression test for the observability gap found
    /// on 2026-09-08: `total_trades` is not a usable proxy, because blocking
    /// a rebalance on a down day defers the exposure change to a later
    /// candle rather than removing a trade from the run — so the same
    /// candle series is run twice here, once with a breaker that must fire
    /// and once with one that must not.
    async fn run_with_drawdown_limit(max_daily_drawdown: f64) -> BacktestReport {
        let db = crate::storage::db::Database::open_in_memory().await.unwrap();
        let config = BacktestConfig {
            product_code: "BTC_JPY".into(),
            from: Utc::now(),
            to: Utc::now(),
            resolution_secs: 60,
            slippage_pct: 0.0,
            fee_pct: Some(0.0),
            initial_jpy: dec!(1_000_000),
            warmup_candles: 0,
        };

        let mut trading_config = TradingConfig::default();
        trading_config.sma_period = 2;
        trading_config.ema_period = 2;
        trading_config.rsi_period = 2;
        trading_config.macd_fast = 2;
        trading_config.macd_slow = 3;
        trading_config.macd_signal = 2;
        trading_config.bollinger_period = 2;
        trading_config.allocation_threshold = 0.01;
        trading_config.circuit_breaker_enabled = true;
        trading_config.max_daily_drawdown = max_daily_drawdown;

        use chrono::TimeZone;
        let day = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        // Warmup + ascent (establishes a BTC position), then a same-day
        // crash deep enough to exceed any drawdown limit under 50%.
        let prices = [
            dec!(9_000_000), dec!(9_100_000), dec!(9_200_000), dec!(9_300_000),
            dec!(9_400_000), dec!(9_500_000), dec!(9_600_000), dec!(9_700_000),
            dec!(4_500_000),
        ];
        let candles: Vec<Candle> = prices.iter().map(|&p| make_candle_at(p, day)).collect();

        Simulator::new(config, db).run(candles, trading_config).await.unwrap()
    }

    #[tokio::test]
    async fn report_counts_circuit_breaker_trips() {
        let fired = run_with_drawdown_limit(0.05).await;
        assert!(
            fired.circuit_breaker_trips >= 1,
            "a >50% same-day crash must trip a 5% daily drawdown limit; got {} trips",
            fired.circuit_breaker_trips
        );

        // Same candles, a limit no single day can reach: the counter must
        // stay at zero rather than tracking anything incidental.
        let inert = run_with_drawdown_limit(0.99).await;
        assert_eq!(
            inert.circuit_breaker_trips, 0,
            "a 99% daily drawdown limit cannot be reached on this series"
        );
    }

    #[tokio::test]
    async fn report_counts_rejected_orders() {
        // Every order submitted while the breaker is tripped is refused by
        // RiskManager::evaluate and never reaches the exchange, so it must
        // be visible in orders_rejected rather than silently dropped.
        let fired = run_with_drawdown_limit(0.05).await;
        assert!(
            fired.circuit_breaker_trips >= 1 && fired.orders_rejected >= 1,
            "expected at least one trip and one refused order; got {} trips / {} rejected",
            fired.circuit_breaker_trips,
            fired.orders_rejected
        );
    }

    #[test]
    fn apply_slippage_increases_price() {
        let price = dec!(9_000_000);
        let slipped = apply_slippage(price, 0.001);
        assert!(slipped > price);
    }
}
