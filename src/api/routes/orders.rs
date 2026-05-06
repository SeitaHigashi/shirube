use axum::{extract::State, http::StatusCode, Json};
use serde::Deserialize;

use crate::api::AppState;
use crate::types::order::{Order, OrderRequest, OrderSide, OrderType};

pub async fn get_orders(
    State(state): State<AppState>,
) -> Result<Json<Vec<Order>>, StatusCode> {
    state
        .db
        .orders()
        .fetch_all(50)
        .await
        .map(Json)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

#[derive(Debug, Deserialize)]
pub struct PlaceOrderRequest {
    pub side: String,
    pub size: rust_decimal::Decimal,
    pub price: Option<rust_decimal::Decimal>,
}

pub async fn post_order(
    State(state): State<AppState>,
    Json(req): Json<PlaceOrderRequest>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let side = match req.side.to_uppercase().as_str() {
        "BUY" => OrderSide::Buy,
        "SELL" => OrderSide::Sell,
        _ => return Err(StatusCode::BAD_REQUEST),
    };

    let order_type = if req.price.is_some() { OrderType::Limit } else { OrderType::Market };

    let order_req = OrderRequest {
        product_code: "BTC_JPY".into(),
        side,
        order_type,
        price: req.price,
        size: req.size,
        minute_to_expire: None,
        time_in_force: None,
    };

    state
        .exchange
        .send_order(&order_req)
        .await
        .map(|id| Json(serde_json::json!({ "acceptance_id": id })))
        .map_err(|_| StatusCode::BAD_GATEWAY)
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
        Router::new()
            .route("/api/orders", axum::routing::get(get_orders).post(post_order))
            .with_state(state)
    }

    #[tokio::test]
    async fn get_orders_returns_200() {
        let app = make_app().await;
        let req = Request::builder().uri("/api/orders").body(Body::empty()).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn post_order_buy_returns_acceptance_id() {
        let app = make_app().await;
        let body = serde_json::json!({ "side": "BUY", "size": "0.001" }).to_string();
        let req = Request::builder()
            .method("POST")
            .uri("/api/orders")
            .header("Content-Type", "application/json")
            .body(Body::from(body))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let b = axum::body::to_bytes(resp.into_body(), 1024).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&b).unwrap();
        assert!(v["acceptance_id"].is_string());
    }

    #[tokio::test]
    async fn post_order_invalid_side_returns_400() {
        let app = make_app().await;
        let body = serde_json::json!({ "side": "INVALID", "size": "0.001" }).to_string();
        let req = Request::builder()
            .method("POST")
            .uri("/api/orders")
            .header("Content-Type", "application/json")
            .body(Body::from(body))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }
}
