use axum::{extract::State, Json};

use crate::api::AppState;
use crate::signal::SignalDetail;

pub async fn get_latest_signal(State(state): State<AppState>) -> Json<Option<SignalDetail>> {
    let sig = state.latest_signal.read().await;
    Json(sig.clone())
}

#[cfg(test)]
mod tests {
    use crate::api::AppState;
    use crate::exchange::mock::MockExchangeClient;
    use crate::signal::SignalDetail;
    use crate::storage::db::Database;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use std::sync::Arc;
    use tokio::sync::{broadcast, RwLock};
    use tower::ServiceExt;

    fn detail(target_pct: f64, confidence: f64) -> SignalDetail {
        use crate::signal::AllocationSignal;
        SignalDetail {
            aggregate: AllocationSignal::from_effective(target_pct, confidence),
            indicators: vec![],
            calculated_at: chrono::Utc::now(),
            calculation_state: "active".to_string(),
        }
    }

    async fn make_state_with_signal(sig: Option<SignalDetail>) -> AppState {
        use crate::api::server::build_router;
        let _ = build_router; // suppress unused import
        use crate::config::TradingConfig;
        let db = Database::open_in_memory().await.unwrap();
        let mock = Arc::new(MockExchangeClient::new());
        let (candle_tx, _) = broadcast::channel(16);
        let (ticker_tx, _) = broadcast::channel(16);
        let (signal_tx, _) = broadcast::channel(16);
        AppState {
            db,
            exchange: mock,
            candle_tx,
            ticker_tx,
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
    async fn returns_neutral_allocation_signal() {
        use crate::api::server::build_router;
        let app = build_router(make_state_with_signal(Some(detail(0.5, 0.0))).await);
        let req = Request::builder()
            .uri("/api/signal/latest")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let pct = json["aggregate"]["target_pct"].as_f64().unwrap();
        assert!((pct - 0.5).abs() < 1e-9);
    }

    #[tokio::test]
    async fn returns_bullish_allocation_signal() {
        use crate::api::server::build_router;
        let sig = detail(0.8, 0.9);
        let app = build_router(make_state_with_signal(Some(sig)).await);
        let req = Request::builder()
            .uri("/api/signal/latest")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let pct = json["aggregate"]["target_pct"].as_f64().unwrap();
        assert!(pct > 0.5, "expected bullish allocation, got {pct}");
        let conf = json["aggregate"]["confidence"].as_f64().unwrap();
        assert!((conf - 0.9).abs() < 1e-9);
    }
}
