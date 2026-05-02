use axum::{
    extract::State,
    http::StatusCode,
    Json,
};
use serde_json::json;

use crate::api::AppState;
use crate::config::TradingConfig;

pub async fn get_config(State(state): State<AppState>) -> Json<TradingConfig> {
    let cfg = state.trading_config.read().await.clone();
    Json(cfg)
}

pub async fn put_config(
    State(state): State<AppState>,
    Json(new_cfg): Json<TradingConfig>,
) -> Result<Json<TradingConfig>, (StatusCode, Json<serde_json::Value>)> {
    if let Err(msg) = new_cfg.validate() {
        return Err((StatusCode::BAD_REQUEST, Json(json!({ "error": msg }))));
    }

    state.db.config().save(&new_cfg).await.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
    })?;

    *state.trading_config.write().await = new_cfg.clone();

    Ok(Json(new_cfg))
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
    use tokio::sync::{broadcast, RwLock};
    use tower::ServiceExt;

    async fn make_app() -> Router {
        let db = Database::open_in_memory().await.unwrap();
        let mock = Arc::new(MockExchangeClient::new());
        let (candle_tx, _) = broadcast::channel(16);
        let (signal_tx, _) = broadcast::channel(16);
        let state = AppState {
            db,
            exchange: mock,
            candle_tx,
            signal_tx,
            latest_signal: Arc::new(RwLock::new(None)),
            news_cache: Arc::new(RwLock::new(vec![])),
            trading_config: Arc::new(RwLock::new(TradingConfig::default())),
        };
        Router::new()
            .route(
                "/api/config",
                axum::routing::get(get_config).put(put_config),
            )
            .with_state(state)
    }

    #[tokio::test]
    async fn get_config_returns_defaults() {
        let app = make_app().await;
        let req = Request::builder().uri("/api/config").body(Body::empty()).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["signal_threshold"], 0.4);
        assert_eq!(v["sma_period"], 75);
    }

    #[tokio::test]
    async fn put_config_updates_value() {
        let app = make_app().await;
        let mut cfg = TradingConfig::default();
        cfg.signal_threshold = 0.5;
        let body = serde_json::to_string(&cfg).unwrap();
        let req = Request::builder()
            .method("PUT")
            .uri("/api/config")
            .header("content-type", "application/json")
            .body(Body::from(body))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["signal_threshold"], 0.5);
    }

    #[tokio::test]
    async fn put_config_rejects_invalid() {
        let app = make_app().await;
        let mut cfg = TradingConfig::default();
        cfg.signal_threshold = 2.0; // invalid
        let body = serde_json::to_string(&cfg).unwrap();
        let req = Request::builder()
            .method("PUT")
            .uri("/api/config")
            .header("content-type", "application/json")
            .body(Body::from(body))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }
}
