mod api;
mod backtest;
mod error;
mod exchange;
mod market;
mod risk;
mod signal;
mod storage;
mod trading;
mod types;

use std::sync::Arc;

use tracing::{info, warn};

use crate::exchange::ExchangeClient;
use crate::market::bus::MarketDataBus;
use crate::risk::manager::RiskManager;
use crate::risk::RiskParams;
use crate::signal::engine::SignalEngine;
use crate::signal::indicators::{
    bollinger::Bollinger, ema::Ema, macd::Macd, rsi::Rsi, sma::Sma,
};
use crate::signal::Indicator;
use crate::storage::db::Database;
use crate::trading::engine::TradingEngine;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    info!("trader2 starting");

    let db_path = std::env::var("DATABASE_PATH").unwrap_or_else(|_| "trader2.db".to_string());
    let db = Database::open(&db_path).await?;
    info!("Database opened: {}", db_path);

    let has_api_keys =
        std::env::var("BITFLYER_API_KEY").is_ok() && std::env::var("BITFLYER_API_SECRET").is_ok();

    let client: Arc<dyn ExchangeClient> = if has_api_keys {
        let key = std::env::var("BITFLYER_API_KEY").unwrap();
        let secret = std::env::var("BITFLYER_API_SECRET").unwrap();
        info!("Using BitFlyerRestClient with real API keys");
        Arc::new(exchange::bitflyer::rest::BitFlyerRestClient::new(key, secret))
    } else {
        info!("No API keys found — using real bitFlyer public ticker, mock for orders/balances");
        Arc::new(exchange::PublicBitFlyerClient::new())
    };

    // WS broadcast channel
    let (ws_tx, _ws_rx) =
        tokio::sync::broadcast::channel::<exchange::bitflyer::ws::WsMessage>(256);

    // WS タスク
    {
        let key = std::env::var("BITFLYER_API_KEY").unwrap_or_default();
        let secret = std::env::var("BITFLYER_API_SECRET").unwrap_or_default();
        let ws_client = exchange::bitflyer::ws::BitFlyerWsClient::new(key, secret);
        let tx = ws_tx.clone();
        tokio::spawn(async move {
            let channels = vec![
                "lightning_ticker_BTC_JPY".to_string(),
                "lightning_executions_BTC_JPY".to_string(),
            ];
            if let Err(e) = ws_client.run(channels, tx).await {
                warn!("WebSocket task ended: {}", e);
            }
        });
    }

    // MarketDataBus: WS + REST → Candle broadcast + DB 保存
    let market_bus = MarketDataBus::start(
        Arc::clone(&client),
        ws_tx,
        db.clone(),
        "BTC_JPY",
        60,
    )
    .await;

    // インジケータ構築
    let indicators: Vec<Box<dyn Indicator>> = vec![
        Box::new(Sma::new(20)),
        Box::new(Ema::new(20)),
        Box::new(Rsi::new(14)),
        Box::new(Macd::default()),
        Box::new(Bollinger::default()),
    ];

    // SignalEngine: Candle → Signal
    let (signal_engine, signal_rx) = SignalEngine::new(indicators, market_bus.candle_rx());
    tokio::spawn(signal_engine.run());

    // TradingEngine: Signal → RiskManager → send_order
    let risk_params = RiskParams::default();
    let trading_engine = TradingEngine::new(
        signal_rx,
        Arc::clone(&client),
        RiskManager::new(risk_params),
        "BTC_JPY".to_string(),
    );
    tokio::spawn(trading_engine.run());

    // API サーバー起動
    let api_port: u16 = std::env::var("API_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(3000);
    let addr = std::net::SocketAddr::from(([0, 0, 0, 0], api_port));

    let (signal_tx, _signal_rx) = tokio::sync::broadcast::channel::<crate::signal::Signal>(64);
    let api_state = api::AppState {
        db: db.clone(),
        exchange: Arc::clone(&client),
        candle_tx: market_bus.candle_tx(),
        signal_tx: signal_tx.clone(),
    };
    tokio::spawn(api::server::run(api_state, addr));

    // Keep the main task alive
    tokio::signal::ctrl_c().await?;
    info!("Shutdown signal received");
    Ok(())
}
