use axum::{
    extract::State,
    http::StatusCode,
    Json,
};
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::api::AppState;
use crate::backtest::{BacktestConfig, BacktestReport};
use crate::backtest::simulator::Simulator;
use crate::risk::RiskParams;
use crate::signal::indicators::{
    bollinger::Bollinger, ema::Ema, macd::Macd, rsi::Rsi, sma::Sma,
};
use crate::signal::Indicator;
use crate::storage::backtest_runs::BacktestRunRecord;

#[derive(Debug, Deserialize)]
pub struct RunBacktestRequest {
    pub from: DateTime<Utc>,
    pub to: DateTime<Utc>,
    #[serde(default = "default_resolution")]
    pub resolution_secs: u32,
    #[serde(default = "default_slippage")]
    pub slippage_pct: f64,
    #[serde(default = "default_fee")]
    pub fee_pct: f64,
    pub initial_jpy: Decimal,
}

fn default_resolution() -> u32 { 60 }
fn default_slippage() -> f64 { 0.001 }
fn default_fee() -> f64 { 0.0015 }

#[derive(Debug, Serialize)]
pub struct BacktestRunResponse {
    pub id: i64,
    pub report: BacktestReportJson,
}

#[derive(Debug, Serialize)]
pub struct BacktestReportJson {
    pub total_return_pct: f64,
    pub sharpe_ratio: f64,
    pub max_drawdown_pct: f64,
    pub win_rate: f64,
    pub total_trades: u32,
}

impl From<&BacktestReport> for BacktestReportJson {
    fn from(r: &BacktestReport) -> Self {
        Self {
            total_return_pct: r.total_return_pct,
            sharpe_ratio: r.sharpe_ratio,
            max_drawdown_pct: r.max_drawdown_pct,
            win_rate: r.win_rate,
            total_trades: r.total_trades,
        }
    }
}

impl From<&BacktestRunRecord> for BacktestRunResponse {
    fn from(r: &BacktestRunRecord) -> Self {
        Self {
            id: r.id,
            report: (&r.report).into(),
        }
    }
}

pub async fn run_backtest(
    State(state): State<AppState>,
    Json(req): Json<RunBacktestRequest>,
) -> Result<Json<BacktestRunResponse>, (StatusCode, Json<serde_json::Value>)> {
    let config = BacktestConfig {
        product_code: "BTC_JPY".to_string(),
        from: req.from,
        to: req.to,
        resolution_secs: req.resolution_secs,
        slippage_pct: req.slippage_pct,
        fee_pct: req.fee_pct,
        initial_jpy: req.initial_jpy,
    };

    let candles = state
        .db
        .candles()
        .get_range("BTC_JPY", req.resolution_secs, req.from, req.to, None)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e.to_string() })),
            )
        })?;

    if candles.is_empty() {
        return Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({ "error": "指定期間にローソク足データがありません" })),
        ));
    }

    let tc = state.trading_config.read().await.clone();
    let indicators: Vec<Box<dyn Indicator>> = vec![
        Box::new(Sma::new(tc.sma_period)),
        Box::new(Ema::new(tc.ema_period)),
        Box::new(Rsi::new(tc.rsi_period)),
        Box::new(Macd::new(tc.macd_fast, tc.macd_slow, tc.macd_signal)),
        Box::new(Bollinger::new(tc.bollinger_period, tc.bollinger_std)),
    ];
    let risk_params = tc.to_risk_params();

    let simulator = Simulator::new(config, state.db.clone());
    let report = simulator
        .run(candles, indicators, risk_params)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e.to_string() })),
            )
        })?;

    let runs = state
        .db
        .backtest_runs()
        .list(1)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e.to_string() })),
            )
        })?;

    let id = runs.first().map(|r| r.id).unwrap_or(0);
    Ok(Json(BacktestRunResponse {
        id,
        report: (&report).into(),
    }))
}

pub async fn list_backtests(
    State(state): State<AppState>,
) -> Result<Json<Vec<BacktestRunResponse>>, StatusCode> {
    state
        .db
        .backtest_runs()
        .list(50)
        .await
        .map(|runs| Json(runs.iter().map(|r| r.into()).collect()))
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::AppState;
    use crate::exchange::mock::MockExchangeClient;
    use crate::storage::db::Database;
    use crate::types::market::Candle;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use axum::Router;
    use chrono::TimeZone;
    use rust_decimal_macros::dec;
    use std::sync::Arc;
    use tokio::sync::broadcast;
    use tower::ServiceExt;

    async fn make_app() -> (Router, Database) {
        let db = Database::open_in_memory().await.unwrap();
        let mock = Arc::new(MockExchangeClient::new());
        let (candle_tx, _) = broadcast::channel(16);
        let (signal_tx, _) = broadcast::channel(16);
        let state = AppState {
            db: db.clone(),
            exchange: mock,
            candle_tx,
            signal_tx,
            news_cache: std::sync::Arc::new(tokio::sync::RwLock::new(vec![])),
            trading_config: std::sync::Arc::new(tokio::sync::RwLock::new(
                crate::config::TradingConfig::default(),
            )),
        };
        let app = Router::new()
            .route(
                "/api/backtest",
                axum::routing::get(list_backtests).post(run_backtest),
            )
            .with_state(state);
        (app, db)
    }

    #[tokio::test]
    async fn list_backtests_returns_empty_initially() {
        let (app, _) = make_app().await;
        let req = Request::builder().uri("/api/backtest").body(Body::empty()).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 1024).await.unwrap();
        let v: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
        assert!(v.is_empty());
    }

    #[tokio::test]
    async fn list_backtests_returns_stored_runs() {
        let (app, db) = make_app().await;
        let config = BacktestConfig {
            product_code: "BTC_JPY".into(),
            from: Utc::now(),
            to: Utc::now(),
            resolution_secs: 60,
            slippage_pct: 0.001,
            fee_pct: 0.0015,
            initial_jpy: dec!(1_000_000),
        };
        let report = BacktestReport {
            total_return_pct: 5.0,
            sharpe_ratio: 1.2,
            max_drawdown_pct: 2.0,
            win_rate: 0.6,
            total_trades: 10,
        };
        db.backtest_runs().insert(&config, &report).await.unwrap();

        let req = Request::builder().uri("/api/backtest").body(Body::empty()).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        let body = axum::body::to_bytes(resp.into_body(), 1024 * 64).await.unwrap();
        let v: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
        assert_eq!(v.len(), 1);
        assert_eq!(v[0]["report"]["total_trades"], 10);
    }

    #[tokio::test]
    async fn post_backtest_returns_422_when_no_candles() {
        let (app, _) = make_app().await;
        let body = serde_json::json!({
            "from": "2024-01-01T00:00:00Z",
            "to": "2024-01-02T00:00:00Z",
            "initial_jpy": "1000000"
        });
        let req = Request::builder()
            .method("POST")
            .uri("/api/backtest")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[tokio::test]
    async fn post_backtest_returns_200_with_report() {
        let (app, db) = make_app().await;

        // candle を事前挿入
        let t1 = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
        let t2 = Utc.with_ymd_and_hms(2024, 1, 1, 0, 1, 0).unwrap();
        for t in [t1, t2] {
            db.candles()
                .upsert(&Candle {
                    product_code: "BTC_JPY".into(),
                    open_time: t,
                    resolution_secs: 60,
                    open: dec!(9_000_000),
                    high: dec!(9_010_000),
                    low: dec!(8_990_000),
                    close: dec!(9_005_000),
                    volume: dec!(1),
                })
                .await
                .unwrap();
        }

        let body = serde_json::json!({
            "from": "2024-01-01T00:00:00Z",
            "to": "2024-01-01T00:02:00Z",
            "initial_jpy": "1000000"
        });
        let req = Request::builder()
            .method("POST")
            .uri("/api/backtest")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), 1024 * 64).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(v["id"].as_i64().unwrap() > 0);
        assert!(v["report"]["total_trades"].as_u64().is_some());
    }
}
