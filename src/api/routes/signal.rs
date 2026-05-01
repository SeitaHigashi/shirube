use axum::{extract::State, Json};

use crate::api::AppState;
use crate::signal::Signal;

pub async fn get_latest_signal(State(state): State<AppState>) -> Json<Option<Signal>> {
    let sig = state.latest_signal.read().await;
    Json(sig.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::AppState;
    use crate::exchange::mock::MockExchangeClient;
    use crate::signal::Signal;
    use crate::storage::db::Database;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use rust_decimal_macros::dec;
    use std::sync::Arc;
    use tokio::sync::{broadcast, RwLock};
    use tower::ServiceExt;

    async fn make_state_with_signal(sig: Option<Signal>) -> AppState {
        use crate::api::server::build_router;
        let _ = build_router; // suppress unused import
        use crate::config::TradingConfig;
        let db = Database::open_in_memory().await.unwrap();
        let mock = Arc::new(MockExchangeClient::new());
        let (candle_tx, _) = broadcast::channel(16);
        let (signal_tx, _) = broadcast::channel(16);
        AppState {
            db,
            exchange: mock,
            candle_tx,
            signal_tx,
            latest_signal: Arc::new(RwLock::new(sig)),
            news_cache: Arc::new(RwLock::new(vec![])),
            trading_config: Arc::new(RwLock::new(TradingConfig::default())),
        }
    }

    #[tokio::test]
    async fn returns_null_when_no_signal() {
        use crate::api::server::build_router;
        let app = build_router(make_state_with_signal(None).await);
        let req = Request::builder()
            .uri("/api/signal/latest")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        assert_eq!(&body[..], b"null");
    }

    #[tokio::test]
    async fn returns_hold_signal() {
        use crate::api::server::build_router;
        let app = build_router(make_state_with_signal(Some(Signal::Hold)).await);
        let req = Request::builder()
            .uri("/api/signal/latest")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        assert_eq!(&body[..], b"\"Hold\"");
    }

    #[tokio::test]
    async fn returns_buy_signal() {
        use crate::api::server::build_router;
        let sig = Signal::Buy { price: dec!(9000000), confidence: 0.8 };
        let app = build_router(make_state_with_signal(Some(sig)).await);
        let req = Request::builder()
            .uri("/api/signal/latest")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["Buy"]["confidence"], 0.8);
    }
}
