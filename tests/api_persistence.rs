//! Integration tests for the HTTP API ↔ storage seam.
//!
//! Route handlers already have unit tests that assert status codes against a
//! throwaway state. These tests instead check that a request actually changes
//! durable state: that a config write survives into a freshly built
//! `AppState` reading the same database, and that an order posted through the
//! API really reaches the exchange client.

mod common;

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::{app_state, test_db};
use http_body_util::BodyExt;
use shirube::api::server::build_router;
use shirube::config::TradingConfig;
use shirube::exchange::mock::MockExchangeClient;
use shirube::types::order::OrderSide;
use tower::ServiceExt;

/// Send one request through a freshly built router and return status + body.
async fn call(
    state: shirube::api::AppState,
    req: Request<Body>,
) -> (StatusCode, serde_json::Value) {
    let resp = build_router(state).oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, json)
}

fn get(uri: &str) -> Request<Body> {
    Request::builder().uri(uri).body(Body::empty()).unwrap()
}

fn put_json(uri: &str, body: &serde_json::Value) -> Request<Body> {
    Request::builder()
        .method("PUT")
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn post_json(uri: &str, body: &serde_json::Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

#[tokio::test]
async fn a_config_written_through_the_api_survives_into_a_new_app_state() {
    let db = test_db().await;
    let mock = Arc::new(MockExchangeClient::new());

    let mut cfg = TradingConfig::default();
    cfg.allocation_threshold = 0.07;
    cfg.rsi_period = 9;
    cfg.zone.hold_btc_above = 0.9;
    let payload = serde_json::to_value(&cfg).unwrap();

    let (status, _) = call(
        app_state(db.clone(), mock.clone()),
        put_json("/api/config", &payload),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // A brand-new AppState shares only the database, so anything it reads back
    // must have come from SQLite rather than the previous in-memory cache.
    let reloaded = db
        .config()
        .load()
        .await
        .expect("config load should succeed")
        .expect("a config should have been persisted");

    assert_eq!(reloaded.allocation_threshold, 0.07);
    assert_eq!(reloaded.rsi_period, 9);
    assert_eq!(reloaded.zone.hold_btc_above, 0.9);
}

#[tokio::test]
async fn an_invalid_config_is_rejected_and_nothing_is_persisted() {
    let db = test_db().await;
    let mock = Arc::new(MockExchangeClient::new());

    // allocation_threshold must be within [0.0, 1.0].
    let mut cfg = TradingConfig::default();
    cfg.allocation_threshold = 5.0;
    let payload = serde_json::to_value(&cfg).unwrap();

    let (status, body) = call(
        app_state(db.clone(), mock.clone()),
        put_json("/api/config", &payload),
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.get("error").is_some(), "error body: {body}");
    assert!(
        db.config().load().await.unwrap().is_none(),
        "a rejected config must not be written to the database"
    );
}

#[tokio::test]
async fn posting_an_order_reaches_the_exchange_client() {
    let db = test_db().await;
    let mock = Arc::new(MockExchangeClient::new());

    let (status, body) = call(
        app_state(db, mock.clone()),
        post_json("/api/orders", &serde_json::json!({ "side": "buy", "size": 0.01 })),
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert!(body.get("acceptance_id").is_some(), "body: {body}");

    let placed = mock.placed_orders();
    assert_eq!(placed.len(), 1);
    assert_eq!(placed[0].side, OrderSide::Buy);
    assert_eq!(placed[0].size, rust_decimal_macros::dec!(0.01));
}

#[tokio::test]
async fn an_unknown_order_side_is_rejected_before_reaching_the_exchange() {
    let db = test_db().await;
    let mock = Arc::new(MockExchangeClient::new());

    let (status, _) = call(
        app_state(db, mock.clone()),
        post_json("/api/orders", &serde_json::json!({ "side": "sideways", "size": 0.01 })),
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        mock.placed_orders().is_empty(),
        "a rejected side must never reach the exchange"
    );
}

#[tokio::test]
async fn config_get_returns_the_running_config() {
    let db = test_db().await;
    let mock = Arc::new(MockExchangeClient::new());

    let (status, body) = call(app_state(db, mock), get("/api/config")).await;
    assert_eq!(status, StatusCode::OK);

    let parsed: TradingConfig = serde_json::from_value(body).unwrap();
    assert_eq!(parsed.sma_period, TradingConfig::default().sma_period);
}
