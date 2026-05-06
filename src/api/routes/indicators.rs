use axum::{
    extract::{Query, State},
    http::StatusCode,
    Json,
};
use chrono::{DateTime, Utc};
use serde::Deserialize;

use crate::api::AppState;
use crate::config::TradingConfig;
use crate::signal::{compute_indicators, IndicatorPoint};

#[derive(Debug, Deserialize)]
pub struct IndicatorQuery {
    #[serde(default = "default_resolution")]
    pub resolution: u32,
    #[serde(default = "default_count")]
    pub count: u32,
    pub from: Option<DateTime<Utc>>,
    pub to: Option<DateTime<Utc>>,
}

fn default_resolution() -> u32 {
    60
}

fn default_count() -> u32 {
    200
}

/// シグナルエンジンの基準足（秒）。TradingConfig のピリオド値はこの解像度基準。
const BASE_RESOLUTION: u32 = 60;

/// ピリオドをベース解像度から指定解像度にスケール変換する。
///
/// シグナルエンジンは常に 1 分足でインジケーターを計算するため、設定値（例: SMA=200）は
/// 200 分間のウィンドウを意味する。より粗い足種（例: 1 時間足）で同じ実時間ウィンドウを
/// カバーするには、`ceil(period * BASE / resolution)` 本のキャンドルで足りる。
/// 最低 2 本は確保する（1 本だと指標が計算できないため）。
fn scale_period(period: usize, resolution: u32) -> usize {
    if resolution <= BASE_RESOLUTION {
        return period;
    }
    // ceil division を整数演算で計算
    let scaled = ((period as u64 * BASE_RESOLUTION as u64 + resolution as u64 - 1)
        / resolution as u64) as usize;
    scaled.max(2)
}

/// 全インジケーターピリオドを解像度に合わせてスケール変換した config を返す。
/// 元の cfg は変更しない。
fn scale_config(cfg: &TradingConfig, resolution: u32) -> TradingConfig {
    if resolution == BASE_RESOLUTION {
        return cfg.clone();
    }
    TradingConfig {
        sma_period: scale_period(cfg.sma_period, resolution),
        ema_period: scale_period(cfg.ema_period, resolution),
        rsi_period: scale_period(cfg.rsi_period, resolution),
        macd_fast: scale_period(cfg.macd_fast, resolution),
        macd_slow: scale_period(cfg.macd_slow, resolution),
        macd_signal: scale_period(cfg.macd_signal, resolution),
        bollinger_period: scale_period(cfg.bollinger_period, resolution),
        ..cfg.clone()
    }
}

/// GET /api/indicators
///
/// 指定した解像度・期間のキャンドルに対してインジケーターを計算して返す。
/// シグナルエンジンと同一のロジックを使用するため、バックエンド側に計算を一本化できる。
/// フロントエンドはこのエンドポイントを利用することで独自計算が不要になる。
pub async fn get_indicators(
    State(state): State<AppState>,
    Query(q): Query<IndicatorQuery>,
) -> Result<Json<Vec<IndicatorPoint>>, StatusCode> {
    let cfg = state.trading_config.read().await.clone();

    let to = q.to.unwrap_or_else(Utc::now);
    let from = q.from.unwrap_or_else(|| {
        to - chrono::Duration::seconds((q.resolution as i64) * (q.count as i64))
    });

    // 指定解像度のキャンドルを取得（DB の 1 分足を resolution でアグリゲート）
    let candles = state
        .db
        .tickers()
        .get_aggregated("BTC_JPY", q.resolution, from, to, Some(q.count))
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    // resolution が 1 分足でない場合、ピリオドを時間窓ベースでスケール変換して
    // シグナルエンジンと同じ実時間ウィンドウをカバーするようにする。
    let scaled_cfg = scale_config(&cfg, q.resolution);
    let points = compute_indicators(&candles, &scaled_cfg);

    Ok(Json(points))
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
            product_code: "BTC_JPY".to_string(),
            db,
            exchange: mock,
            candle_tx,
            ticker_tx,
            signal_tx,
            aggregator_registry: std::sync::Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
            ws_tx: { let (tx, _) = broadcast::channel(16); tx },
            latest_signal: Arc::new(tokio::sync::RwLock::new(None)),
            news_cache: Arc::new(tokio::sync::RwLock::new(vec![])),
            trading_config: Arc::new(tokio::sync::RwLock::new(
                crate::config::TradingConfig::default(),
            )),
            config_tx: Arc::new(
                tokio::sync::watch::channel(crate::config::TradingConfig::default()).0,
            ),
        };
        Router::new()
            .route("/api/indicators", axum::routing::get(get_indicators))
            .with_state(state)
    }

    #[tokio::test]
    async fn returns_empty_array_when_no_data() {
        let app = make_app().await;
        let req = Request::builder()
            .uri("/api/indicators")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), 1024 * 64).await.unwrap();
        let points: Vec<serde_json::Value> = serde_json::from_slice(&body).unwrap();
        assert!(points.is_empty());
    }
}
