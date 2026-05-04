use axum::{extract::State, http::StatusCode, Json};

use crate::api::AppState;
use crate::types::balance::Balance;

pub async fn get_balance(
    State(state): State<AppState>,
) -> Result<Json<Vec<Balance>>, StatusCode> {
    state
        .exchange
        .get_balance()
        .await
        .map(Json)
        .map_err(|_| StatusCode::BAD_GATEWAY)
}

pub async fn get_fee(State(state): State<AppState>) -> Json<serde_json::Value> {
    Json(serde_json::json!({ "fee_pct": state.exchange.fee_pct() }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::AppState;
    use crate::exchange::mock::MockExchangeClient;
    use crate::storage::db::Database;
    use crate::types::balance::Balance;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use axum::Router;
    use rust_decimal_macros::dec;
    use std::sync::Arc;
    use tokio::sync::broadcast;
    use tower::ServiceExt;

    async fn make_app() -> Router {
        let db = Database::open_in_memory().await.unwrap();
        let mock = Arc::new(MockExchangeClient::new());
        mock.set_balances(vec![
            Balance { currency_code: "JPY".into(), amount: dec!(1_000_000), available: dec!(1_000_000) },
            Balance { currency_code: "BTC".into(), amount: dec!(0.01), available: dec!(0.01) },
        ]);
        let (candle_tx, _) = broadcast::channel(16);
        let (ticker_tx, _) = broadcast::channel(16);
        let (signal_tx, _) = broadcast::channel(16);
        let state = AppState {
            db,
            exchange: mock,
            candle_tx,
            ticker_tx,
            signal_tx,
            latest_signal: std::sync::Arc::new(tokio::sync::RwLock::new(None)),
            news_cache: std::sync::Arc::new(tokio::sync::RwLock::new(vec![])),
            trading_config: std::sync::Arc::new(tokio::sync::RwLock::new(
                crate::config::TradingConfig::default(),
            )),
        };
        Router::new()
            .route("/api/balance", axum::routing::get(get_balance))
            .with_state(state)
    }

    #[tokio::test]
    async fn get_balance_returns_200() {
        let app = make_app().await;
        let req = Request::builder().uri("/api/balance").body(Body::empty()).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn get_balance_contains_jpy_and_btc() {
        let app = make_app().await;
        let req = Request::builder().uri("/api/balance").body(Body::empty()).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        let body = axum::body::to_bytes(resp.into_body(), 1024 * 64).await.unwrap();
        let balances: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
        assert_eq!(balances.len(), 2);
        let codes: Vec<&str> = balances.iter().map(|b| b["currency_code"].as_str().unwrap()).collect();
        assert!(codes.contains(&"JPY"));
        assert!(codes.contains(&"BTC"));
    }
}
