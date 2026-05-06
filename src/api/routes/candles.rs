use axum::{
    extract::{Query, State},
    http::StatusCode,
    Json,
};
use chrono::{DateTime, Utc};
use serde::Deserialize;

use crate::api::AppState;
use crate::types::market::Candle;

#[derive(Debug, Deserialize)]
pub struct CandleQuery {
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

pub async fn get_candles(
    State(state): State<AppState>,
    Query(q): Query<CandleQuery>,
) -> Result<Json<Vec<Candle>>, StatusCode> {
    let repo = state.db.tickers();

    let to = q.to.unwrap_or_else(Utc::now);
    let from = q.from.unwrap_or_else(|| {
        to - chrono::Duration::seconds((q.resolution as i64) * (q.count as i64))
    });

    repo.get_aggregated("BTC_JPY", q.resolution, from, to, Some(q.count))
        .await
        .map(Json)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::AppState;
    use crate::exchange::mock::MockExchangeClient;
    use crate::storage::db::Database;
    use crate::types::market::Ticker;
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
        let (ticker_tx, _) = broadcast::channel(16);
        let (signal_tx, _) = broadcast::channel(16);
        let state = AppState {
            db: db.clone(),
            exchange: mock,
            candle_tx,
            ticker_tx,
            signal_tx,
            aggregator_registry: std::sync::Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
            ws_tx: { let (tx, _) = broadcast::channel(16); tx },
            latest_signal: std::sync::Arc::new(tokio::sync::RwLock::new(None)),
            news_cache: std::sync::Arc::new(tokio::sync::RwLock::new(vec![])),
            trading_config: std::sync::Arc::new(tokio::sync::RwLock::new(
                crate::config::TradingConfig::default(),
            )),
            config_tx: std::sync::Arc::new(
                tokio::sync::watch::channel(crate::config::TradingConfig::default()).0
            ),
        };
        let app = Router::new()
            .route("/api/candles", axum::routing::get(get_candles))
            .with_state(state);
        (app, db)
    }

    #[tokio::test]
    async fn get_candles_returns_empty_array_when_no_data() {
        let (app, _db) = make_app().await;
        let req = Request::builder()
            .uri("/api/candles")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 1024 * 64).await.unwrap();
        let candles: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
        assert!(candles.is_empty());
    }

    #[tokio::test]
    async fn get_candles_returns_stored_candles() {
        let (app, db) = make_app().await;
        let t = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
        db.tickers()
            .insert(&Ticker {
                product_code: "BTC_JPY".into(),
                timestamp: t,
                best_bid: dec!(8_990_000),
                best_ask: dec!(9_010_000),
                best_bid_size: dec!(0.1),
                best_ask_size: dec!(0.1),
                ltp: dec!(9_005_000),
                volume: dec!(1),
                volume_by_product: dec!(1),
            })
            .await
            .unwrap();

        let from = t - chrono::Duration::seconds(60);
        let to = t + chrono::Duration::seconds(60);
        let from_str = from.format("%Y-%m-%dT%H:%M:%SZ").to_string();
        let to_str = to.format("%Y-%m-%dT%H:%M:%SZ").to_string();
        let uri = format!("/api/candles?resolution=60&from={from_str}&to={to_str}");
        let req = Request::builder()
            .uri(&uri)
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 1024 * 64).await.unwrap();
        let candles: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
        assert_eq!(candles.len(), 1);
    }
}
