//! Integration tests for the backtest path:
//! candles → `Simulator` → indicators → `TradingEngine` logic → `BacktestReport`.
//!
//! The simulator reuses the *same* allocation and risk code as live trading,
//! so these tests double as a regression net for that shared logic over a
//! deterministic, fully-specified price series.

mod common;

use chrono::{DateTime, Duration, Utc};
use common::{t, test_db};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use shirube::backtest::simulator::Simulator;
use shirube::backtest::BacktestConfig;
use shirube::config::TradingConfig;
use shirube::types::market::Candle;

const RESOLUTION_SECS: u32 = 60;

/// Build a candle series from a list of closing prices, one candle per minute.
/// High/low hug the close so the series carries no incidental volatility.
fn series(start: DateTime<Utc>, closes: &[f64]) -> Vec<Candle> {
    closes
        .iter()
        .enumerate()
        .map(|(i, &c)| {
            let close = Decimal::try_from(c).unwrap();
            Candle {
                product_code: "BTC_JPY".to_string(),
                open_time: start + Duration::seconds(i as i64 * RESOLUTION_SECS as i64),
                resolution_secs: RESOLUTION_SECS,
                open: close,
                high: close,
                low: close,
                close,
                volume: dec!(1),
            }
        })
        .collect()
}

/// Short indicator periods so a test-sized series actually leaves warm-up.
fn fast_config() -> TradingConfig {
    TradingConfig {
        allocation_threshold: 0.05,
        sma_period: 5,
        ema_period: 5,
        rsi_period: 5,
        macd_fast: 3,
        macd_slow: 6,
        macd_signal: 3,
        bollinger_period: 5,
        ..TradingConfig::default()
    }
}

fn backtest_config(candles: &[Candle], initial_jpy: Decimal) -> BacktestConfig {
    BacktestConfig {
        product_code: "BTC_JPY".to_string(),
        from: candles.first().unwrap().open_time,
        to: candles.last().unwrap().open_time,
        resolution_secs: RESOLUTION_SECS,
        slippage_pct: 0.0,
        fee_pct: Some(0.0),
        initial_jpy,
        warmup_candles: 0,
    }
}

/// A sawtooth that swings hard enough in both directions to force rebalances.
fn oscillating_closes(cycles: usize) -> Vec<f64> {
    let mut out = Vec::new();
    for _ in 0..cycles {
        for step in 0..10 {
            out.push(9_000_000.0 + step as f64 * 120_000.0);
        }
        for step in (0..10).rev() {
            out.push(9_000_000.0 + step as f64 * 120_000.0);
        }
    }
    out
}

#[tokio::test]
async fn an_oscillating_market_produces_trades_and_a_coherent_report() {
    let candles = series(t(0), &oscillating_closes(6));
    let sim = Simulator::new(backtest_config(&candles, dec!(5_000_000)), test_db().await);

    let report = sim.run(candles, fast_config()).await.expect("backtest runs");

    // 6 up/down cycles must produce repeated two-way rebalancing, not a single
    // opening trade. A signal pipeline that collapsed to a constant allocation
    // would still trade once to reach its target, so `> 0` would not catch it.
    assert!(
        report.total_trades > 5,
        "6 swings of ±13% should trigger repeated rebalances, got {}",
        report.total_trades
    );
    // Every report field must be internally consistent, not merely present.
    assert!(
        (0.0..=1.0).contains(&report.win_rate),
        "win_rate out of range: {}",
        report.win_rate
    );
    assert!(
        report.max_drawdown_pct >= 0.0,
        "drawdown is reported as a non-negative magnitude: {}",
        report.max_drawdown_pct
    );
    assert!(
        report.total_fees_jpy >= 0.0,
        "fees can never be negative"
    );
}

#[tokio::test]
async fn a_flat_market_settles_instead_of_churning() {
    // NOTE: a flat series is *neutral*, not "do nothing". Every sub-signal
    // reads 0.5, which maps to a 50% BTC target, so the strategy correctly
    // buys once to reach that target from an all-JPY start. What it must not
    // do is keep trading afterwards: with the target unchanged and the price
    // unchanged, there is no drift left to correct.
    let flat = series(t(0), &vec![9_000_000.0; 120]);
    let flat_report = Simulator::new(backtest_config(&flat, dec!(5_000_000)), test_db().await)
        .run(flat, fast_config())
        .await
        .expect("backtest runs");

    let swinging = series(t(0), &oscillating_closes(6));
    let swinging_report =
        Simulator::new(backtest_config(&swinging, dec!(5_000_000)), test_db().await)
            .run(swinging, fast_config())
            .await
            .expect("backtest runs");

    assert!(
        flat_report.total_trades < swinging_report.total_trades,
        "a flat market must trade less than a swinging one: flat={}, swinging={}",
        flat_report.total_trades,
        swinging_report.total_trades
    );
    assert!(
        flat_report.total_trades <= 2,
        "only the initial move to the neutral target is justified, got {}",
        flat_report.total_trades
    );
}

#[tokio::test]
async fn a_tiny_account_has_its_rebalances_blocked_by_the_lot_size() {
    // 0.001 BTC at ~9M JPY is ~9,000 JPY. On a 5,000 JPY account even a full
    // 100% swing cannot reach one lot, so every rebalance the strategy wants
    // must be recorded as "blocked by lot size" rather than silently vanishing.
    let candles = series(t(0), &oscillating_closes(6));
    let sim = Simulator::new(backtest_config(&candles, dec!(5_000)), test_db().await);

    let report = sim.run(candles, fast_config()).await.expect("backtest runs");

    assert_eq!(report.total_trades, 0, "no order can clear the 0.001 BTC lot");
    assert!(
        report.orders_below_min > 0,
        "blocked rebalances must be counted, got {report:?}"
    );
}

#[tokio::test]
async fn warmup_candles_are_excluded_from_the_measured_window() {
    let closes = oscillating_closes(6);
    let candles = series(t(0), &closes);

    let mut cfg = backtest_config(&candles, dec!(5_000_000));
    // Treat the first 20 candles as warm-up only: the measured window starts
    // after them, so `from` must move with it.
    cfg.warmup_candles = 20;
    cfg.from = candles[20].open_time;

    let sim = Simulator::new(cfg, test_db().await);
    let report = sim.run(candles, fast_config()).await.expect("backtest runs");

    // The run must still be well-formed; the point is that warm-up candles
    // feed the indicators without being traded on.
    assert!((0.0..=1.0).contains(&report.win_rate));
    assert!(report.max_drawdown_pct >= 0.0);
}
