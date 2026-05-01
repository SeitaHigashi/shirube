use axum::{extract::State, http::StatusCode, Json};

use crate::api::AppState;
use crate::types::market::Ticker;

pub async fn get_ticker(
    State(state): State<AppState>,
) -> Result<Json<Ticker>, StatusCode> {
    state
        .exchange
        .get_ticker("BTC_JPY")
        .await
        .map(Json)
        .map_err(|_| StatusCode::BAD_GATEWAY)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::AppState;
    use crate::exchange::mock::MockExchangeClient;
    use crate::signal::Signal;
    use crate::storage::db::Database;
    use crate::types::market::Ticker;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use axum::Router;
    use chrono::Utc;
    use rust_decimal_macros::dec;
    use std::sync::Arc;
    use tokio::sync::broadcast;
    use tower::ServiceExt;

    async fn make_app() -> Router {
        let db = Database::open_in_memory().await.unwrap();
        let mock = Arc::new(MockExchangeClient::new());
        mock.set_ticker(Ticker {
            product_code: "BTC_JPY".into(),
            timestamp: Utc::now(),
            best_bid: dec!(9_000_000),
            best_ask: dec!(9_010_000),
            best_bid_size: dec!(0.1),
            best_ask_size: dec!(0.1),
            ltp: dec!(9_005_000),
            volume: dec!(100),
            volume_by_product: dec!(50),
        });
        let (candle_tx, _) = broadcast::channel(16);
        let (signal_tx, _) = broadcast::channel(16);
        let state = AppState {
            db,
            exchange: mock,
            candle_tx,
            signal_tx,
            latest_signal: std::sync::Arc::new(tokio::sync::RwLock::new(None)),
            news_cache: std::sync::Arc::new(tokio::sync::RwLock::new(vec![])),
            trading_config: std::sync::Arc::new(tokio::sync::RwLock::new(
                crate::config::TradingConfig::default(),
            )),
        };
        Router::new()
            .route("/api/ticker", axum::routing::get(get_ticker))
            .with_state(state)
    }

    #[tokio::test]
    async fn get_ticker_returns_200() {
        let app = make_app().await;
        let req = Request::builder()
            .uri("/api/ticker")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn get_ticker_returns_json_with_ltp() {
        let app = make_app().await;
        let req = Request::builder()
            .uri("/api/ticker")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        let body = axum::body::to_bytes(resp.into_body(), 1024 * 64).await.unwrap();
        let ticker: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(ticker["ltp"], "9005000");
    }
}
