use std::net::SocketAddr;

use axum::{
    routing::get,
    Router,
};
use tower_http::cors::{Any, CorsLayer};
use tower_http::services::ServeDir;
use tracing::info;

use super::{
    routes::{balance, candles, config, news, orders, signal, ticker},
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
        .route("/api/fee", get(balance::get_fee))
        .route("/api/orders", get(orders::get_orders).post(orders::post_order))
        .route("/api/news/latest", get(news::get_latest_news))
        .route("/api/signal/latest", get(signal::get_latest_signal))
        .route("/api/config", get(config::get_config).put(config::put_config))
        // WebSocket endpoints
        .route("/ws/candles", get(ws_handler::ws_candles))
        .route("/ws/tickers", get(ws_handler::ws_tickers))
        .route("/ws/signal", get(ws_handler::ws_signal))
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
        let (ticker_tx, _) = broadcast::channel(16);
        let (signal_tx, _) = broadcast::channel(16);
        AppState {
            db,
            exchange: mock,
            candle_tx,
            ticker_tx,
            signal_tx,
            latest_signal: Arc::new(RwLock::new(None)),
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
    async fn unknown_route_returns_404() {
        let app = build_router(make_state().await);
        let req = Request::builder().uri("/api/unknown").body(Body::empty()).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }
}
