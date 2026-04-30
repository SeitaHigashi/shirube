use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::api::AppState;
use crate::backtest::{BacktestConfig, BacktestReport};
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
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use axum::Router;
    use rust_decimal_macros::dec;
    use std::sync::Arc;
    use tokio::sync::broadcast;
    use tower::ServiceExt;

    async fn make_app() -> (Router, Database) {
        let db = Database::open_in_memory().await.unwrap();
        let mock = Arc::new(MockExchangeClient::new());
        let (candle_tx, _) = broadcast::channel(16);
        let (signal_tx, _) = broadcast::channel(16);
        let state = AppState { db: db.clone(), exchange: mock, candle_tx, signal_tx };
        let app = Router::new()
            .route("/api/backtest", axum::routing::get(list_backtests))
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
}
