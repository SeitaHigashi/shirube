//! Integration tests for the live trading path:
//! `IndicatorOutput` → `TradingEngine` → `RiskManager` → `ExchangeClient`.
//!
//! Unit tests cover each of these modules in isolation. These tests exercise
//! the seams between them through the public API only, which is where
//! contract mismatches (allocation maths vs. risk limits vs. order shape)
//! actually surface.

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::{indicator_output, t, test_config};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use shirube::exchange::mock::MockExchangeClient;
use shirube::risk::{manager::RiskManager, RiskParams};
use shirube::signal::IndicatorOutput;
use shirube::trading::engine::TradingEngine;
use shirube::types::balance::Balance;
use shirube::types::order::OrderSide;
use tokio::sync::broadcast;

/// Spawn a `TradingEngine` wired to `mock` and return the sender that feeds it.
fn spawn_engine(
    mock: Arc<MockExchangeClient>,
    params: RiskParams,
) -> broadcast::Sender<IndicatorOutput> {
    let (indicator_tx, indicator_rx) = broadcast::channel(16);
    let (engine, _signal_tx) = TradingEngine::new(
        indicator_rx,
        mock,
        RiskManager::new(params),
        "BTC_JPY".to_string(),
    );
    tokio::spawn(engine.with_config(test_config()).run());
    indicator_tx
}

/// Poll until `mock` has placed at least `n` orders, or fail after ~2s.
/// Polling (rather than a fixed sleep) keeps the test fast and non-flaky.
async fn await_orders(mock: &MockExchangeClient, n: usize) {
    for _ in 0..200 {
        if mock.placed_orders().len() >= n {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!(
        "expected at least {} order(s), got {}",
        n,
        mock.placed_orders().len()
    );
}

/// Assert that `mock` places no order within a short settle window.
async fn assert_no_orders(mock: &MockExchangeClient) {
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        mock.placed_orders().is_empty(),
        "expected no orders, got {:?}",
        mock.placed_orders()
    );
}

#[tokio::test]
async fn bullish_indicators_produce_a_buy_that_moves_the_balances() {
    let mock = Arc::new(MockExchangeClient::new());
    let tx = spawn_engine(mock.clone(), RiskParams::default());

    let jpy_before = mock.jpy_balance();
    assert_eq!(mock.btc_balance(), Decimal::ZERO, "starts fully in JPY");

    // close 1% above the moving averages and an oversold RSI: strongly bullish.
    tx.send(indicator_output(t(0), 9_090_000.0, 9_000_000.0, 20.0))
        .unwrap();
    await_orders(&mock, 1).await;

    let orders = mock.placed_orders();
    assert_eq!(orders.len(), 1);
    assert_eq!(orders[0].side, OrderSide::Buy);
    assert_eq!(orders[0].product_code, "BTC_JPY");

    assert!(
        mock.btc_balance() > Decimal::ZERO,
        "buy should increase BTC balance"
    );
    assert!(
        mock.jpy_balance() < jpy_before,
        "buy should decrease JPY balance"
    );
}

#[tokio::test]
async fn a_bearish_reversal_sells_the_position_back_out() {
    let mock = Arc::new(MockExchangeClient::new());
    let tx = spawn_engine(mock.clone(), RiskParams::default());

    // First go long.
    tx.send(indicator_output(t(0), 9_090_000.0, 9_000_000.0, 20.0))
        .unwrap();
    await_orders(&mock, 1).await;
    let btc_after_buy = mock.btc_balance();
    assert!(btc_after_buy > Decimal::ZERO);

    // Then flip: close 1% below the averages with an overbought RSI.
    tx.send(indicator_output(t(1), 8_910_000.0, 9_000_000.0, 80.0))
        .unwrap();
    await_orders(&mock, 2).await;

    let orders = mock.placed_orders();
    assert_eq!(orders[1].side, OrderSide::Sell, "reversal must sell");
    assert!(
        mock.btc_balance() < btc_after_buy,
        "sell should reduce BTC balance"
    );
}

#[tokio::test]
async fn a_dust_sized_rebalance_never_reaches_the_exchange() {
    let mock = Arc::new(MockExchangeClient::new());
    // A portfolio this small makes any rebalance far below the 0.001 BTC floor.
    mock.set_balances(vec![
        Balance {
            currency_code: "JPY".into(),
            amount: dec!(1000),
            available: dec!(1000),
        },
        Balance {
            currency_code: "BTC".into(),
            amount: dec!(0),
            available: dec!(0),
        },
    ]);

    let tx = spawn_engine(mock.clone(), RiskParams::default());
    tx.send(indicator_output(t(0), 9_090_000.0, 9_000_000.0, 20.0))
        .unwrap();

    // The signal is strongly bullish, so the engine *wants* to trade. The
    // order is suppressed by the 0.001 BTC minimum-lot gate in
    // `allocation_delta_to_order`, before `RiskManager::evaluate` is even
    // reached — verified by mutation: disabling the risk manager's own
    // min-size check does not make this test pass an order through.
    assert_no_orders(&mock).await;
}

#[tokio::test]
async fn an_open_circuit_breaker_rejects_orders() {
    let mock = Arc::new(MockExchangeClient::new());
    let params = RiskParams {
        circuit_breaker_enabled: true,
        // Any loss at all trips the breaker.
        max_daily_drawdown: 0.0,
        ..RiskParams::default()
    };

    // Establish today's baseline, then crash the price so the portfolio is
    // underwater before the first signal arrives.
    let mut risk = RiskManager::new(params.clone());
    risk.observe_time(t(0), dec!(1_000_000));
    assert!(risk.check_drawdown(dec!(500_000)).is_some(), "breaker trips");
    assert!(risk.is_circuit_broken());

    let (indicator_tx, indicator_rx) = broadcast::channel(16);
    let (engine, _signal_tx) =
        TradingEngine::new(indicator_rx, mock.clone(), risk, "BTC_JPY".to_string());
    tokio::spawn(engine.with_config(test_config()).run());

    indicator_tx
        .send(indicator_output(t(1), 9_090_000.0, 9_000_000.0, 20.0))
        .unwrap();

    assert_no_orders(&mock).await;
}
