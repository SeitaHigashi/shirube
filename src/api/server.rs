use std::net::SocketAddr;

use axum::{
    routing::get,
    Router,
};
use tower_http::cors::{Any, CorsLayer};
use tower_http::services::ServeDir;
use tracing::info;

use super::{
    routes::{balance, backtest, candles, config, news, orders, ticker},
    ws_handler, AppState,
};

pub fn build_router(state: AppState) -> Router {
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    Router::new()
        // REST endpoints
        .route("/api/ticker", get(ticker::get_ticker))
        .route("/api/candles", get(candles::get_candles))
        .route("/api/balance", get(balance::get_balance))
        .route("/api/orders", get(orders::get_orders).post(orders::post_order))
        .route("/api/backtest", get(backtest::list_backtests).post(backtest::run_backtest))
        .route("/api/news/latest", get(news::get_latest_news))
        .route("/api/config", get(config::get_config).put(config::put_config))
        // WebSocket endpoint
        .route("/ws/candles", get(ws_handler::ws_candles))
        .layer(cors)
        .with_state(state)
        // 静的ファイル配信 (フロントエンドダッシュボード)
        .nest_service("/", ServeDir::new("frontend/static"))
}

pub async fn run(state: AppState, addr: SocketAddr) -> anyhow::Result<()> {
    let app = build_router(state);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    info!("API server listening on {}", addr);
    axum::serve(listener, app).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::AppState;
    use crate::exchange::mock::MockExchangeClient;
    use crate::storage::db::Database;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use std::sync::Arc;
    use tokio::sync::{broadcast, RwLock};
    use tower::ServiceExt;

    async fn make_state() -> AppState {
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
            news_cache: Arc::new(RwLock::new(vec![])),
            trading_config: Arc::new(RwLock::new(TradingConfig::default())),
        }
    }

    #[tokio::test]
    async fn router_has_ticker_route() {
        let app = build_router(make_state().await);
        let req = Request::builder().uri("/api/ticker").body(Body::empty()).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        // MockExchangeClient はデフォルト Ticker を返すので 200 が期待される
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn router_has_balance_route() {
        let app = build_router(make_state().await);
        let req = Request::builder().uri("/api/balance").body(Body::empty()).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn router_has_candles_route() {
        let app = build_router(make_state().await);
        let req = Request::builder().uri("/api/candles").body(Body::empty()).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn router_has_orders_route() {
        let app = build_router(make_state().await);
        let req = Request::builder().uri("/api/orders").body(Body::empty()).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn router_has_backtest_route() {
        let app = build_router(make_state().await);
        let req = Request::builder().uri("/api/backtest").body(Body::empty()).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn router_post_backtest_returns_422_on_empty_db() {
        let app = build_router(make_state().await);
        let body = serde_json::json!({
            "from": "2024-01-01T00:00:00Z",
            "to": "2024-01-02T00:00:00Z",
            "initial_jpy": "1000000"
        });
        let req = Request::builder()
            .method("POST")
            .uri("/api/backtest")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[tokio::test]
    async fn unknown_route_returns_404() {
        let app = build_router(make_state().await);
        let req = Request::builder().uri("/api/unknown").body(Body::empty()).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }
}
