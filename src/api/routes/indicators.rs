use axum::{
    extract::{Query, State},
    http::StatusCode,
    Json,
};
use chrono::{DateTime, Utc};
use serde::Deserialize;

use crate::api::AppState;
use crate::signal::{compute_indicators, IndicatorPoint};

#[derive(Debug, Deserialize)]
pub struct IndicatorQuery {
    #[serde(default = "default_resolution")]
    pub resolution: u32,
    #[serde(default = "default_count")]
    pub count: u32,
    pub from: Option<DateTime<Utc>>,
    pub to: Option<DateTime<Utc>>,
}

fn default_resolution() -> u32 {
    60
}

fn default_count() -> u32 {
    200
}

/// GET /api/indicators
///
/// 指定した解像度・期間のキャンドルに対してインジケーターを計算して返す。
/// シグナルエンジンと同一のロジックを使用するため、バックエンド側に計算を一本化できる。
/// フロントエンドはこのエンドポイントを利用することで独自計算が不要になる。
pub async fn get_indicators(
    State(state): State<AppState>,
    Query(q): Query<IndicatorQuery>,
) -> Result<Json<Vec<IndicatorPoint>>, StatusCode> {
    let cfg = state.trading_config.read().await.clone();

    let to = q.to.unwrap_or_else(Utc::now);
    let from = q.from.unwrap_or_else(|| {
        to - chrono::Duration::seconds((q.resolution as i64) * (q.count as i64))
    });

    // 指定解像度のキャンドルを取得（DB の 1 分足を resolution でアグリゲート）
    let candles = state
        .db
        .tickers()
        .get_aggregated("BTC_JPY", q.resolution, from, to, Some(q.count))
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let points = compute_indicators(&candles, &cfg);

    Ok(Json(points))
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
    use std::sync::Arc;
    use tokio::sync::broadcast;
    use tower::ServiceExt;

    async fn make_app() -> Router {
        let db = Database::open_in_memory().await.unwrap();
        let mock = Arc::new(MockExchangeClient::new());
        let (candle_tx, _) = broadcast::channel(16);
        let (ticker_tx, _) = broadcast::channel(16);
        let (signal_tx, _) = broadcast::channel(16);
        let state = AppState {
            db,
            exchange: mock,
            candle_tx,
            ticker_tx,
            signal_tx,
            aggregator_registry: std::sync::Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
            latest_signal: Arc::new(tokio::sync::RwLock::new(None)),
            news_cache: Arc::new(tokio::sync::RwLock::new(vec![])),
            trading_config: Arc::new(tokio::sync::RwLock::new(
                crate::config::TradingConfig::default(),
            )),
            config_tx: Arc::new(
                tokio::sync::watch::channel(crate::config::TradingConfig::default()).0,
            ),
        };
        Router::new()
            .route("/api/indicators", axum::routing::get(get_indicators))
            .with_state(state)
    }

    #[tokio::test]
    async fn returns_empty_array_when_no_data() {
        let app = make_app().await;
        let req = Request::builder()
            .uri("/api/indicators")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 1024 * 64).await.unwrap();
        let points: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
        assert!(points.is_empty());
    }
}
