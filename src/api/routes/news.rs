use axum::{extract::State, http::StatusCode, Json};

use crate::api::AppState;
use crate::news::analyzer::SentimentScore;

/// GET /api/news/latest — 最新のセンチメントスコア一覧を返す。
pub async fn get_latest_news(
    State(state): State<AppState>,
) -> Result<Json<Vec<SentimentScore>>, StatusCode> {
    let scores = state.news_cache.read().await;
    Ok(Json(scores.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::AppState;
    use crate::exchange::mock::MockExchangeClient;
    use crate::news::analyzer::SentimentScore;
    use crate::storage::db::Database;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use chrono::Utc;
    use std::sync::Arc;
    use tokio::sync::{broadcast, RwLock};
    use tower::ServiceExt;

    async fn make_state_with_scores(scores: Vec<SentimentScore>) -> AppState {
        let db = Database::open_in_memory().await.unwrap();
        let mock = Arc::new(MockExchangeClient::new());
        let (candle_tx, _) = broadcast::channel(16);
        let (ticker_tx, _) = broadcast::channel(16);
        let (signal_tx, _) = broadcast::channel(16);
        AppState {
            product_code: "BTC_JPY".to_string(),
            db,
            exchange: mock,
            candle_tx,
            ticker_tx,
            signal_tx,
            aggregator_registry: std::sync::Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
            ws_tx: { let (tx, _) = broadcast::channel(16); tx },
            latest_signal: Arc::new(RwLock::new(None)),
            news_cache: Arc::new(RwLock::new(scores)),
            trading_config: Arc::new(RwLock::new(crate::config::TradingConfig::default())),
            config_tx: Arc::new(
                tokio::sync::watch::channel(crate::config::TradingConfig::default()).0
            ),
        }
    }

    #[tokio::test]
    async fn returns_empty_list_when_no_news() {
        let state = make_state_with_scores(vec![]).await;
        let app = crate::api::server::build_router(state);
        let req = Request::builder()
            .uri("/api/news/latest")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let items: Vec<SentimentScore> = serde_json::from_slice(&body).unwrap();
        assert!(items.is_empty());
    }

    #[tokio::test]
    async fn returns_scores_from_cache() {
        let scores = vec![SentimentScore {
            headline: "BTC bullish".into(),
            score: 1.0,
            analyzed_at: Utc::now(),
            published_at: None,
        }];
        let state = make_state_with_scores(scores).await;
        let app = crate::api::server::build_router(state);
        let req = Request::builder()
            .uri("/api/news/latest")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let items: Vec<SentimentScore> = serde_json::from_slice(&body).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].headline, "BTC bullish");
    }
}
