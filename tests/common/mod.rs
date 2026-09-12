#![allow(dead_code)] // helpers are shared across test binaries; each uses a subset

//! Shared helpers for integration tests.
//!
//! Everything here goes through the crate's *public* API only — integration
//! tests must not reach into private internals. Keep helpers minimal: a helper
//! that hides the behaviour under test makes failures harder to diagnose.

use std::sync::Arc;

use chrono::{DateTime, TimeZone, Utc};
use shirube::config::TradingConfig;
use shirube::signal::{IndicatorOutput, IndicatorPoint, IndicatorSignal};
use shirube::storage::db::Database;

/// Open a fresh in-memory database with the full schema applied.
pub async fn test_db() -> Database {
    Database::open_in_memory()
        .await
        .expect("in-memory database should open")
}

/// A stable, ordered timestamp for driving the engine.
///
/// NOTE: anchored to *today's* UTC date rather than a fixed calendar date.
/// `TradingEngine::handle_indicator` re-baselines the daily drawdown tracker
/// off the candle timestamp, while `handle_rebalance` re-baselines off
/// `Utc::now()`. A fixed past date would make those two disagree on the
/// trading day, and the periodic rebalance would silently clear a tripped
/// circuit breaker mid-test.
pub fn t(minute: u32) -> DateTime<Utc> {
    let today = Utc::now().date_naive();
    Utc.from_utc_datetime(
        &today
            .and_hms_opt(0, minute, 0)
            .expect("minute must be < 60"),
    )
}

/// Build an `IndicatorOutput` that drives `TradingEngine` towards a given
/// bullishness. `close` above `sma`/`ema` reads as bullish, below as bearish;
/// `rsi` reinforces the same direction.
pub fn indicator_output(at: DateTime<Utc>, close: f64, ma: f64, rsi: f64) -> IndicatorOutput {
    IndicatorOutput {
        indicators: vec![
            IndicatorSignal { name: "sma".into(), value: Some(ma) },
            IndicatorSignal { name: "ema".into(), value: Some(ma) },
            IndicatorSignal { name: "rsi".into(), value: Some(rsi) },
        ],
        raw: IndicatorPoint {
            time: at,
            close: Some(close),
            sma: Some(ma),
            ema: Some(ma),
            rsi: Some(rsi),
            macd_line: Some(close - ma),
            signal_line: Some(0.0),
            histogram: Some(close - ma),
            bb_upper: Some(ma * 1.02),
            bb_middle: Some(ma),
            bb_lower: Some(ma * 0.98),
        },
        calculated_at: at,
    }
}

/// A `TradingConfig` tuned for tests: a low allocation threshold so a single
/// signal actually crosses it instead of being filtered out as noise.
pub fn test_config() -> Arc<tokio::sync::RwLock<TradingConfig>> {
    let mut cfg = TradingConfig::default();
    cfg.allocation_threshold = 0.01;
    Arc::new(tokio::sync::RwLock::new(cfg))
}

/// Build an `AppState` over the given database and exchange.
///
/// Takes both so a test can hand the *same* `Database` to two successive
/// states and check that data actually survived the round trip, rather than
/// only observing the in-memory `RwLock` cache.
pub fn app_state(db: Database, exchange: Arc<dyn shirube::exchange::ExchangeClient>) -> shirube::api::AppState {
    use std::collections::HashMap;
    use tokio::sync::{broadcast, watch, Mutex, RwLock};

    shirube::api::AppState {
        product_code: "BTC_JPY".to_string(),
        db,
        exchange,
        candle_tx: broadcast::channel(16).0,
        ticker_tx: broadcast::channel(16).0,
        signal_tx: broadcast::channel(16).0,
        ws_tx: broadcast::channel(16).0,
        aggregator_registry: Arc::new(Mutex::new(HashMap::new())),
        latest_signal: Arc::new(RwLock::new(None)),
        news_cache: Arc::new(RwLock::new(vec![])),
        trading_config: Arc::new(RwLock::new(TradingConfig::default())),
        config_tx: Arc::new(watch::channel(TradingConfig::default()).0),
    }
}
