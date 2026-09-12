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
        // Raw (unslipped) close of every evaluated candle, in order — feeds
        // the buy-and-hold / static-mix benchmarks in `compute_report`.
        // NOTE: must stay exactly aligned with `equity_curve` (same warmup
        // skip) or the benchmark curves silently include candles the
        // strategy itself never traded on.
        let mut evaluated_closes: Vec<f64> = Vec::with_capacity(candles.len() - warmup);
        // BTC allocation fraction (btc_value / total) sampled at each
        // evaluated candle, averaged after the loop into
        // `BacktestReport::avg_btc_exposure` — the weight used to build the
        // "static mix" benchmark so it matches the strategy's own average
        // exposure.
        let mut exposure_samples: Vec<f64> = Vec::with_capacity(candles.len() - warmup);
        // Risk-gate activity, surfaced on the report so a run makes it
        // visible whether a gate ever engaged (see RiskEventCounts).
        //
        // NOTE: counted only over evaluated candles — the `skip(warmup)` above
        // means a gate cannot trip on a warmup candle, which is correct: those
        // candles are never traded on and are outside the reported window.
        let mut risk_events = RiskEventCounts::default();

        // Simulated-time clock for rebalance spacing: the timestamp of the
        // candle on which the allocation delta was last *evaluated*.
        //
        // NOTE: `TradingConfig::rebalance_interval_secs` is a DB-persisted,
        // live-honoured knob that this simulator ignored entirely until
        // 2026-09-12 — it evaluated the delta on every candle regardless, so a
        // backtest measured a different strategy from the one the engine runs.
        // The spacing is measured against the candle's own `open_time`, never
        // wall-clock: a whole backtest runs inside a single real-time second,
        // so a wall-clock comparison would make every candle eligible (see the
        // same argument at `risk_manager.observe_time` below).
        //
        // `None` initially so the first evaluated candle always evaluates, and
        // updated on every *evaluation* (not only when an order is actually
        // sent), so the spacing is between evaluations exactly as in the live
        // engine's periodic rebalance ticker.
        let mut last_rebalance_at: Option<chrono::DateTime<Utc>> = None;
        let rebalance_interval = chrono::Duration::seconds(
            i64::try_from(trading_config.rebalance_interval_secs).unwrap_or(i64::MAX),
        );

        for (candle, point) in candles.iter().zip(points.iter()).skip(warmup) {
            // The candle's close is the mid: it prices the portfolio and sizes
            // orders. Slippage widens a synthetic book around it, so
            // MockExchangeClient fills a buy at the ask and a sell at the bid
            // and a round trip actually pays ~2 * slippage_pct. Valuing the
            // book at the mid (rather than at a slipped price, as this did
            // until 2026-09-09) keeps `slippage_pct` a pure cost instead of a
            // distortion of the equity curve's level.
            let mid = candle.close;
            let ticker = Ticker {
                product_code: self.config.product_code.clone(),
                timestamp: Utc::now(),
                best_bid: sell_price(mid, self.config.slippage_pct),
                best_ask: buy_price(mid, self.config.slippage_pct),
                best_bid_size: Decimal::ONE,
                best_ask_size: Decimal::ONE,
                ltp: mid,
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
            let btc_value = btc * mid;
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

            // Rebalance spacing gate. Everything above this point (ticker
            // update, compute_btc_target, the sticky_target update,
            // observe_time and the circuit-breaker check) runs on EVERY
            // candle: they are per-candle state machines, and skipping them
            // would change drawdown accounting and signal state rather than
            // only the rebalance cadence. Only the allocation-delta
            // evaluation below is spaced.
            //
            // `>=` (not `>`) so that an interval equal to the candle
            // resolution — 60s, the default — leaves every candle eligible
            // and reproduces the pre-2026-09-12 behaviour bar-for-bar.
            let rebalance_due = match last_rebalance_at {
                None => true,
                Some(prev) => candle.open_time - prev >= rebalance_interval,
            };

            if rebalance_due {
                if let Some(target_pct) = sticky_target {
                    if !total.is_zero() && !mid.is_zero() {
                        // Count the evaluation itself, not the order: the
                        // live engine spaces its rebalance *checks*, and a
                        // check that finds the delta below threshold still
                        // consumed the slot.
                        last_rebalance_at = Some(candle.open_time);

                        let current_alloc = (btc_value / total).to_f64().unwrap_or(0.0);
                        let delta = target_pct - current_alloc;

                        if delta.abs() >= trading_config.allocation_threshold {
                            let order_req = TradingEngine::allocation_delta_to_order(
                                delta,
                                total,
                                mid,
                                &self.config.product_code,
                                risk_manager.params().min_order_size,
                            );

                            // A `None` here means the delta had already cleared
                            // `allocation_threshold` but the resulting order was
                            // under the exchange lot size, so the rebalance was
                            // dropped *before* RiskManager ever saw it — invisible
                            // in both total_trades and orders_rejected. Count it.
                            // NOTE: counting only; the control flow below is
                            // unchanged, so this cannot alter any trading decision.
                            let order_req = match order_req {
                                Some(req) => Some(req),
                                None => {
                                    risk_events.orders_below_min += 1;
                                    None
                                }
                            };

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
            }

            let btc_val = exchange.btc_balance() * candle.close;
            let equity = exchange.jpy_balance() + btc_val;
            let equity_f64 = equity.to_f64().unwrap_or(0.0);
            equity_curve.push(equity_f64);
            evaluated_closes.push(candle.close.to_f64().unwrap_or(0.0));
            // Post-trade exposure at this candle; guard against a zero-equity
            // edge case rather than dividing by zero.
            exposure_samples.push(if equity_f64 > 0.0 {
                btc_val.to_f64().unwrap_or(0.0) / equity_f64
            } else {
                0.0
            });
        }

        let avg_btc_exposure = if exposure_samples.is_empty() {
            0.0
        } else {
            exposure_samples.iter().sum::<f64>() / exposure_samples.len() as f64
        };

        let filled = exchange.filled_trades();
        let report = super::report::compute_report(
            &filled,
            &equity_curve,
            self.config.initial_jpy.to_f64().unwrap_or(1.0),
            self.config.resolution_secs,
            risk_events,
            self.config.fee_pct,
            avg_btc_exposure,
            &evaluated_closes,
            self.config.slippage_pct,
        );

        self.db.backtest_runs().insert(&self.config, &report).await?;

        Ok(report)
    }
}

/// Fill price for a buy: the candle's close widened *up* by `slippage_pct`.
///
/// Slippage models the difference between the mid-price and the actual fill
/// price due to market impact and the bid/ask spread, so it must move against
/// the trader on **both** sides — see `sell_price` for the other half.
///
/// NOTE: until 2026-09-09 a single `apply_slippage` multiplied by
/// `(1 + slippage_pct)` and was applied to the bid, the ask and the
/// mark-to-market price alike. That is a uniform price-level shift, not a
/// cost: a buy-then-sell round trip at an unchanged close returned exactly
/// what it paid, so the backtest charged nothing whatsoever for turnover
/// (only the mock's tiered exchange fee applied). The old doc comment
/// claimed "buys cost more and sells receive less" — the code did the
/// opposite for sells, subsidising every one of them.
fn buy_price(price: Decimal, slippage_pct: f64) -> Decimal {
    let slip = Decimal::try_from(slippage_pct).unwrap_or(Decimal::ZERO);
    price * (Decimal::ONE + slip)
}

/// Fill price for a sell: the candle's close widened *down* by `slippage_pct`.
/// Clamped at zero so a nonsensical `slippage_pct > 1.0` cannot produce a
/// negative price.
fn sell_price(price: Decimal, slippage_pct: f64) -> Decimal {
    let slip = Decimal::try_from(slippage_pct).unwrap_or(Decimal::ZERO);
    let p = price * (Decimal::ONE - slip);
    if p < Decimal::ZERO {
        Decimal::ZERO
    } else {
        p
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// Tests
// ──────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use rust_decimal_macros::dec;

    /// Build a candle series spaced one minute apart, i.e. matching the
    /// `resolution_secs: 60` these fixtures declare.
    ///
    /// NOTE: the spacing is load-bearing. `Simulator::run` spaces
    /// allocation-delta evaluations by `rebalance_interval_secs` of
    /// *simulated* time, so a fixture whose candles all carry the same
    /// timestamp (as these did before 2026-09-12, via `Utc::now()` per
    /// candle) would be evaluated exactly once no matter how many candles it
    /// contains — an artefact of the fixture, not of the strategy.
    fn make_series(prices: &[Decimal], base: chrono::DateTime<Utc>) -> Vec<Candle> {
        prices
            .iter()
            .enumerate()
            .map(|(i, &p)| make_candle_at(p, base + chrono::Duration::minutes(i as i64)))
            .collect()
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
        let candles: Vec<Candle> = make_series(&prices, Utc::now());

        let simulator = Simulator::new(config, db.clone());
        let report = simulator.run(candles, trading_config).await.unwrap();

        let runs = db.backtest_runs().list(10).await.unwrap();
        assert_eq!(runs.len(), 1);
        // Every candle produces an equity point; report is always populated
        // even when zero trades were placed (e.g. sticky target never set).
        assert!(report.total_return_pct.is_finite());
    }

    /// The report's fee aggregates must reflect the trades a real run
    /// actually placed: `total_fees_jpy` > 0 whenever at least one trade
    /// filled with a non-zero fee rate, and `traded_volume_jpy` must equal
    /// the sum of price*size over those same fills.
    #[tokio::test]
    async fn report_fee_metrics_match_filled_trades() {
        let db = crate::storage::db::Database::open_in_memory().await.unwrap();
        let config = BacktestConfig {
            product_code: "BTC_JPY".into(),
            from: Utc::now(),
            to: Utc::now(),
            resolution_secs: 60,
            slippage_pct: 0.0,
            // Fixed 0.15% fee so at least one filled trade carries a fee.
            fee_pct: Some(0.0015),
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
        let candles: Vec<Candle> = make_series(&prices, Utc::now());

        let simulator = Simulator::new(config, db);
        let report = simulator.run(candles, trading_config).await.unwrap();

        assert!(
            report.total_trades > 0,
            "fixture must produce at least one trade to exercise fee accounting"
        );
        assert!(
            report.total_fees_jpy > 0.0,
            "a 0.15% fixed fee on a filled trade must be reflected in total_fees_jpy"
        );
        assert!(report.traded_volume_jpy > 0.0);
        // Effective fee rate on a fixed-fee run must equal the fixed rate,
        // since every fill paid exactly that rate.
        assert!((report.effective_fee_pct - 0.0015).abs() < 1e-9);
        assert_eq!(report.final_fee_tier_pct, 0.0015);
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
        // One minute apart, all still inside simulated day 1.
        candles.extend(make_series(&day1_prices, day1));
        // Day 2: still bullish (ascending from the crashed price). If the
        // breaker correctly resets at the day boundary, this should still
        // place a rebalance trade instead of staying frozen forever.
        let day2_prices = [
            dec!(4_600_000), dec!(4_700_000), dec!(4_800_000), dec!(4_900_000),
        ];
        candles.extend(make_series(&day2_prices, day2));

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
        // One minute apart, all still inside the same simulated day.
        let candles: Vec<Candle> = make_series(&prices, day);

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
    fn slippage_widens_the_book_around_the_mid() {
        let price = dec!(9_000_000);
        // A buy pays above the mid and a sell receives below it — the two
        // must straddle the mid, or slippage is a price shift, not a cost.
        assert!(buy_price(price, 0.001) > price);
        assert!(sell_price(price, 0.001) < price);
        assert_eq!(buy_price(price, 0.001), dec!(9_009_000));
        assert_eq!(sell_price(price, 0.001), dec!(8_991_000));
    }

    #[test]
    fn slippage_of_zero_leaves_both_sides_at_the_mid() {
        let price = dec!(9_000_000);
        assert_eq!(buy_price(price, 0.0), price);
        assert_eq!(sell_price(price, 0.0), price);
    }

    #[test]
    fn sell_price_never_goes_negative() {
        // A nonsensical slippage above 100% must clamp rather than invert the
        // trade's sign, which would credit the seller for selling.
        assert_eq!(sell_price(dec!(9_000_000), 1.5), Decimal::ZERO);
    }

    /// REGRESSION (2026-09-09): slippage must cost the strategy something.
    ///
    /// The simulator used to set bid, ask and the mark-to-market price all to
    /// `close * (1 + slippage_pct)`, so a round trip at an unchanged price
    /// returned exactly what it paid and turnover was free. This asserts the
    /// property that broke: with everything else held equal, raising
    /// `slippage_pct` must not *improve* the reported return.
    #[tokio::test]
    async fn higher_slippage_never_improves_return() {
        let cheap = run_with_slippage(0.0).await;
        let dear = run_with_slippage(0.01).await;

        assert!(
            dear.total_return_pct <= cheap.total_return_pct,
            "slippage 1% returned {:.4}% but slippage 0% returned {:.4}% — \
             slippage is not being charged as a cost",
            dear.total_return_pct,
            cheap.total_return_pct
        );
        // And it must actually bite: the same trades at a 1% spread cannot
        // come out identical to a frictionless run.
        assert!(
            dear.total_trades > 0,
            "test set-up produced no trades, so it proves nothing"
        );
        assert!(
            (dear.total_return_pct - cheap.total_return_pct).abs() > 1e-9,
            "slippage made no difference at all to the result"
        );
    }

    /// Drive a short oscillating series through the simulator at one
    /// slippage level, holding every other input fixed.
    async fn run_with_slippage(slippage_pct: f64) -> BacktestReport {
        let db = crate::storage::db::Database::open_in_memory().await.unwrap();
        let config = BacktestConfig {
            product_code: "BTC_JPY".into(),
            from: Utc::now(),
            to: Utc::now(),
            resolution_secs: 60,
            slippage_pct,
            // Isolate slippage: no exchange fee, so any difference between the
            // two runs is attributable to the spread alone.
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
        trading_config.circuit_breaker_enabled = false;

        // Oscillate so the allocation target keeps crossing back and forth and
        // the run actually round-trips rather than buying once and holding.
        let mut candles = Vec::new();
        let start = Utc::now();
        for i in 0..60 {
            let wave = if i % 2 == 0 { dec!(200_000) } else { dec!(-200_000) };
            candles.push(make_candle_at(
                dec!(9_000_000) + wave,
                start + chrono::Duration::seconds(60 * i),
            ));
        }

        Simulator::new(config, db)
            .run(candles, trading_config)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn avg_btc_exposure_reports_known_constant_allocation() {
        // Force the sticky target to exactly 1.0 (100% BTC) for every
        // evaluated candle by collapsing the zone's neutral band to a
        // single point at 0.0: `compute_btc_target`'s combined signal is
        // always clamped to [0.0, 1.0], so `raw` is always >= hold_btc_above
        // and `apply_zone` always returns 1.0 (see `signal::apply_zone`'s
        // span<=0 branch). This drives a known constant allocation through
        // the real production allocation logic rather than a shortcut.
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
        trading_config.zone = crate::config::ZoneConfig {
            range_max: 1.0,
            hold_jpy_below: 0.0,
            hold_btc_above: 0.0,
        };

        // 4 warmup candles (>= the longest indicator period, 3) followed by
        // 8 evaluated candles with mild price movement; the target stays
        // 1.0 throughout regardless of the exact price path.
        let candles: Vec<Candle> = (0..12)
            .map(|i| {
                make_candle_at(
                    dec!(9_000_000) + Decimal::from(i % 3) * dec!(10_000),
                    base + chrono::Duration::minutes(i as i64),
                )
            })
            .collect();

        let cfg = BacktestConfig {
            product_code: "BTC_JPY".into(),
            from: base + chrono::Duration::minutes(4),
            to: base + chrono::Duration::minutes(12),
            resolution_secs: 60,
            slippage_pct: 0.0,
            fee_pct: Some(0.0),
            initial_jpy: dec!(1_000_000),
            warmup_candles: 4,
        };

        let db = crate::storage::db::Database::open_in_memory().await.unwrap();
        let report = Simulator::new(cfg, db)
            .run(candles, trading_config)
            .await
            .unwrap();

        // Zero fee/slippage in this fixture, so once the target is reached
        // (immediately, since indicators are warm from the start of the
        // evaluated window) the allocation stays effectively exactly 1.0 for
        // every evaluated candle. The epsilon only absorbs the deliberate
        // round-down-to-8dp in `allocation_delta_to_order`, far tighter than
        // it would need to be to catch a real accounting bug.
        assert!(
            (report.avg_btc_exposure - 1.0).abs() < 1e-4,
            "expected avg_btc_exposure ~= 1.0, got {}",
            report.avg_btc_exposure
        );
    }

    /// Regression test for the most important benchmark defect: the
    /// buy-and-hold / static-mix curves must be built ONLY from the
    /// *evaluated* candle slice. If the implementation accidentally fed the
    /// full candle series (warmup included) into the benchmark, this test
    /// would catch it, because the two runs below use identical evaluated
    /// closes but wildly different (and differently-sized) warmup prices.
    #[tokio::test]
    async fn warmup_candles_do_not_change_benchmark_figures() {
        use chrono::TimeZone;
        let base = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();

        // A threshold no reachable delta (at most 1.0) can ever cross, so
        // zero trades occur in either run regardless of how differently
        // "warm" the indicators are at the start of the evaluated window —
        // this removes any dependency of this test on indicator warm-up
        // timing, which is a *separate*, already-covered concern (see
        // `warmup_candles_prime_indicators_without_being_traded`).
        let mut trading_config = TradingConfig::default();
        trading_config.allocation_threshold = 2.0;

        // The evaluated window: 6 candles with a clear price trend.
        let evaluated_prices = [
            dec!(9_000_000), dec!(9_100_000), dec!(9_200_000),
            dec!(9_300_000), dec!(9_400_000), dec!(9_500_000),
        ];

        // Run A: no warmup, candles = the evaluated window only.
        let candles_a: Vec<Candle> = evaluated_prices
            .iter()
            .enumerate()
            .map(|(i, &p)| make_candle_at(p, base + chrono::Duration::minutes(i as i64)))
            .collect();
        let cfg_a = BacktestConfig {
            product_code: "BTC_JPY".into(),
            from: base,
            to: base + chrono::Duration::minutes(6),
            resolution_secs: 60,
            slippage_pct: 0.001,
            fee_pct: Some(0.0015),
            initial_jpy: dec!(1_000_000),
            warmup_candles: 0,
        };

        // Run B: 6 leading warmup candles at wildly different prices,
        // followed by the exact same evaluated window.
        let warmup_prices = [
            dec!(5_000_000), dec!(5_000_000), dec!(5_000_000),
            dec!(5_000_000), dec!(5_000_000), dec!(5_000_000),
        ];
        let mut candles_b: Vec<Candle> = warmup_prices
            .iter()
            .enumerate()
            .map(|(i, &p)| make_candle_at(p, base - chrono::Duration::minutes(6) + chrono::Duration::minutes(i as i64)))
            .collect();
        candles_b.extend(
            evaluated_prices
                .iter()
                .enumerate()
                .map(|(i, &p)| make_candle_at(p, base + chrono::Duration::minutes(i as i64))),
        );
        let cfg_b = BacktestConfig {
            product_code: "BTC_JPY".into(),
            from: base,
            to: base + chrono::Duration::minutes(6),
            resolution_secs: 60,
            slippage_pct: 0.001,
            fee_pct: Some(0.0015),
            initial_jpy: dec!(1_000_000),
            warmup_candles: 6,
        };

        let db = crate::storage::db::Database::open_in_memory().await.unwrap();
        let report_a = Simulator::new(cfg_a, db.clone())
            .run(candles_a, trading_config.clone())
            .await
            .unwrap();
        let report_b = Simulator::new(cfg_b, db.clone())
            .run(candles_b, trading_config)
            .await
            .unwrap();

        // Sanity: the threshold really did block every trade in both runs,
        // so this test is actually isolating the benchmark computation.
        assert_eq!(report_a.total_trades, 0);
        assert_eq!(report_b.total_trades, 0);

        assert_eq!(report_a.avg_btc_exposure, report_b.avg_btc_exposure);
        assert_eq!(report_a.hold_return_pct, report_b.hold_return_pct);
        assert_eq!(report_a.hold_sharpe_ratio, report_b.hold_sharpe_ratio);
        assert_eq!(report_a.hold_max_drawdown_pct, report_b.hold_max_drawdown_pct);
        assert_eq!(report_a.static_mix_return_pct, report_b.static_mix_return_pct);
        assert_eq!(report_a.static_mix_sharpe_ratio, report_b.static_mix_sharpe_ratio);
        assert_eq!(
            report_a.static_mix_max_drawdown_pct,
            report_b.static_mix_max_drawdown_pct
        );
        assert_eq!(
            report_a.excess_return_vs_static_mix_pct,
            report_b.excess_return_vs_static_mix_pct
        );
        assert_eq!(report_a.sharpe_minus_static_mix, report_b.sharpe_minus_static_mix);

        // And the hold benchmark must actually reflect the rising evaluated
        // window's price move, not the flat 5,000,000 warmup — otherwise
        // this test would trivially "pass" with both sides at 0.
        assert!(report_a.hold_return_pct > 5.0, "got {}", report_a.hold_return_pct);
    }

    // ──────────────────────────────────────────────────────────────────────
    // rebalance_interval_secs
    // ──────────────────────────────────────────────────────────────────────

    /// Drive one fixed oscillating 1-minute series through the simulator at a
    /// given `rebalance_interval_secs`, holding every other input fixed.
    ///
    /// The series oscillates so the allocation target keeps crossing back and
    /// forth: without spacing, essentially every candle produces a rebalance,
    /// which is exactly the behaviour the interval is supposed to thin out.
    async fn run_with_rebalance_interval(rebalance_interval_secs: u64) -> BacktestReport {
        use chrono::TimeZone;
        let base = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();

        let db = crate::storage::db::Database::open_in_memory().await.unwrap();
        let config = BacktestConfig {
            product_code: "BTC_JPY".into(),
            from: base,
            to: base + chrono::Duration::minutes(120),
            resolution_secs: 60,
            slippage_pct: 0.0,
            fee_pct: Some(0.0),
            initial_jpy: dec!(10_000_000),
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
        trading_config.circuit_breaker_enabled = false;
        trading_config.rebalance_interval_secs = rebalance_interval_secs;

        let prices: Vec<Decimal> = (0..120)
            .map(|i| {
                let wave = if i % 2 == 0 { dec!(200_000) } else { dec!(-200_000) };
                dec!(9_000_000) + wave
            })
            .collect();

        Simulator::new(config, db)
            .run(make_series(&prices, base), trading_config)
            .await
            .unwrap()
    }

    /// REGRESSION (2026-09-12): `Simulator::run` must honour
    /// `TradingConfig::rebalance_interval_secs`.
    ///
    /// Until this fix the simulator evaluated the allocation delta on every
    /// candle and never read the field, so a backtest measured a different
    /// strategy from the one the live `TradingEngine` runs — and the knob was
    /// untestable as a hypothesis. Spacing the evaluations 15 minutes apart
    /// on a 1-minute series must leave strictly fewer trades than evaluating
    /// every minute.
    #[tokio::test]
    async fn longer_rebalance_interval_produces_fewer_trades() {
        let every_minute = run_with_rebalance_interval(60).await;
        let every_15_minutes = run_with_rebalance_interval(900).await;

        assert!(
            every_minute.total_trades > 0,
            "fixture produced no trades at all, so it proves nothing"
        );
        assert!(
            every_15_minutes.total_trades < every_minute.total_trades,
            "spacing evaluations 900s apart on a 60s series must trade strictly \
             less than evaluating every candle; got {} vs {}",
            every_15_minutes.total_trades,
            every_minute.total_trades
        );
    }

    /// The default (60s, equal to the backtest resolution) must be a strict
    /// no-op, so every baseline recorded before this change stays comparable:
    /// a 60s interval on a 60s series leaves every candle eligible.
    ///
    /// Asserted against a run whose `rebalance_interval_secs` is 1 — i.e. an
    /// interval too short to gate anything, reproducing the pre-change
    /// "evaluate on every candle" path exactly.
    #[tokio::test]
    async fn rebalance_interval_of_one_resolution_is_unchanged() {
        let ungated = run_with_rebalance_interval(1).await;
        let default_60 = run_with_rebalance_interval(60).await;

        assert_eq!(
            default_60.total_trades, ungated.total_trades,
            "a 60s interval on a 60s series must not skip any evaluation"
        );
        assert_eq!(default_60.total_return_pct, ungated.total_return_pct);
        assert_eq!(default_60.traded_volume_jpy, ungated.traded_volume_jpy);
        assert_eq!(default_60.orders_below_min, ungated.orders_below_min);
        assert_eq!(default_60.orders_rejected, ungated.orders_rejected);
        assert_eq!(default_60.avg_btc_exposure, ungated.avg_btc_exposure);
    }
}
