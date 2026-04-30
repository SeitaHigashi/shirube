mod alert;
mod api;
mod backtest;
mod error;
mod exchange;
mod market;
mod news;
mod risk;
mod signal;
mod storage;
mod trading;
mod types;

use std::sync::Arc;

use tokio::sync::RwLock;
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

    // アラートマネージャー初期化（SLACK_WEBHOOK_URL があれば Slack 通知）
    let alert = std::sync::Arc::new(alert::AlertManager::new());

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

    // WS タスク（指数バックオフで自動再接続）
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
            ws_client.run_with_reconnect(channels, tx).await;
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
    )
    .with_alert(Arc::clone(&alert));
    tokio::spawn(trading_engine.run());

    // News AI タスク起動（5分ごとにRSSフィードを取得してセンチメント分析）
    let news_cache = Arc::new(RwLock::new(vec![]));
    {
        let cache = Arc::clone(&news_cache);
        let ollama_url = std::env::var("OLLAMA_URL")
            .unwrap_or_else(|_| "http://localhost:11434".to_string());
        let ollama_model = std::env::var("OLLAMA_MODEL")
            .unwrap_or_else(|_| "qwen3.6:27b".to_string());
        let feed_urls: Vec<String> = std::env::var("NEWS_FEED_URLS")
            .unwrap_or_else(|_| {
                "https://feeds.feedburner.com/CoinDesk,https://cointelegraph.com/rss".to_string()
            })
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        let fetcher = news::fetcher::NewsFetcher::new(feed_urls);
        let analyzer = news::analyzer::NewsAnalyzer::new(ollama_url, ollama_model);
        let scorer = news::scorer::NewsScorer::default();
        tokio::spawn(async move {
            loop {
                match news::fetcher::FeedSource::fetch_latest(&fetcher).await {
                    Ok(items) => {
                        let scores = analyzer.analyze_batch(&items).await;
                        let avg = scorer.average_score(&scores);
                        info!("News sentiment: avg_score={:.2}, items={}", avg, scores.len());
                        *cache.write().await = scores;
                    }
                    Err(e) => {
                        warn!("News fetch failed: {}", e);
                    }
                }
                tokio::time::sleep(std::time::Duration::from_secs(300)).await;
            }
        });
    }

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
        news_cache,
    };
    tokio::spawn(api::server::run(api_state, addr));

    // Keep the main task alive
    tokio::signal::ctrl_c().await?;
    info!("Shutdown signal received");
    Ok(())
}
