mod api;
mod config;
mod error;
mod exchange;
mod market;
mod news;
mod risk;
mod signal;
mod storage;
mod trading;
mod types;
mod updater;

use std::sync::Arc;

use tokio::sync::RwLock;
use tracing::{info, warn};

use crate::config::TradingConfig;
use crate::exchange::ExchangeClient;
use crate::market::bus::MarketDataBus;
use crate::risk::manager::RiskManager;
use crate::signal::engine::SignalEngine;
use crate::signal::indicators::{
    bollinger::Bollinger, ema::Ema, macd::Macd, rsi::Rsi, sma::Sma,
};
use crate::signal::{Indicator, IndicatorSignal, SignalDetail};
use crate::storage::db::Database;
use crate::trading::engine::TradingEngine;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Print version and exit (used by the auto-updater smoke test)
    if std::env::args().any(|a| a == "--version") {
        println!("{}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    dotenvy::dotenv().ok();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    info!("導 starting");

    let db_path = std::env::var("DATABASE_PATH").unwrap_or_else(|_| "shirube.db".to_string());
    let db = Database::open(&db_path).await?;
    info!("Database opened: {}", db_path);

    // 取引設定を DB から読み込む（なければデフォルト値）
    let trading_config_value = match db.config().load().await {
        Ok(Some(cfg)) => {
            info!("Loaded trading config from DB");
            cfg
        }
        Ok(None) => {
            info!("No trading config in DB, using defaults");
            TradingConfig::default()
        }
        Err(e) => {
            warn!("Failed to load trading config from DB: {}, using defaults", e);
            TradingConfig::default()
        }
    };
    let trading_config = Arc::new(RwLock::new(trading_config_value));

    let has_api_keys =
        std::env::var("BITFLYER_API_KEY").is_ok() && std::env::var("BITFLYER_API_SECRET").is_ok();

    let client: Arc<dyn ExchangeClient> = if has_api_keys {
        let key = std::env::var("BITFLYER_API_KEY").unwrap();
        let secret = std::env::var("BITFLYER_API_SECRET").unwrap();
        info!("Using BitFlyerRestClient with real API keys");
        Arc::new(exchange::bitflyer::rest::BitFlyerRestClient::new(key, secret))
    } else {
        let mock_db_path = std::env::var("MOCK_DB_PATH")
            .unwrap_or_else(|_| "mock_exchange.db".to_string());
        let mock_db = storage::db::Database::open(&mock_db_path).await?;
        info!(
            "No API keys found — using PublicBitFlyerClient (real ticker) + MockExchangeClient (DB: {})",
            mock_db_path
        );
        let public_client =
            exchange::PublicBitFlyerClient::new_with_db(mock_db.mock_state()).await?;
        Arc::new(public_client)
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
    let ws_tx_for_api = ws_tx.clone();
    let market_bus = MarketDataBus::start(
        ws_tx,
        db.clone(),
        "BTC_JPY",
        60,
    )
    .await;

    // インジケータ構築（起動時の config から）
    let init_cfg = trading_config.read().await.clone();
    let mut indicators: Vec<Box<dyn Indicator>> = vec![
        Box::new(Sma::new(init_cfg.sma_period)),
        Box::new(Ema::new(init_cfg.ema_period)),
        Box::new(Rsi::new(init_cfg.rsi_period)),
        Box::new(Macd::new(init_cfg.macd_fast, init_cfg.macd_slow, init_cfg.macd_signal)),
        Box::new(Bollinger::new(init_cfg.bollinger_period, init_cfg.bollinger_std)),
    ];
    let init_risk_params = init_cfg.to_risk_params();

    // SignalEngine へライブ設定変更を配信するチャネル
    let (config_tx, config_rx) = tokio::sync::watch::channel(init_cfg.clone());
    let config_tx = std::sync::Arc::new(config_tx);

    // インジケータのウォームアップ: 起動前にDBの過去Candleを流し込む
    // 最後のCandleのシグナルを捕捉してキャッシュ事前初期化に使う
    let mut warmup_signals: Vec<IndicatorSignal> = Vec::new();
    {
        let warmup_count = indicators.iter().map(|i| i.min_periods()).max().unwrap_or(0);
        // 余裕を持って2倍のCandleを取得（クロス検出等に前後の値が必要なため）
        let fetch_count = (warmup_count * 2).max(1) as u32;
        match db.tickers().get_latest_as_candles("BTC_JPY", 60, fetch_count).await {
            Ok(hist) if !hist.is_empty() => {
                info!(
                    "Warming up {} indicators with {} historical candles (need={})",
                    indicators.len(),
                    hist.len(),
                    warmup_count
                );
                for candle in &hist {
                    warmup_signals = indicators
                        .iter_mut()
                        .map(|ind| {
                            let signal = ind.update(candle);
                            let value = ind.value();
                            IndicatorSignal { name: ind.name().to_string(), signal, value }
                        })
                        .collect();
                }
            }
            Ok(_) => {
                info!("No historical candles in DB for warmup — indicators start cold");
            }
            Err(e) => {
                warn!("Failed to load historical candles for warmup: {}", e);
            }
        }
    }

    // SignalEngine: Candle → Signal（config_rx 経由でライブ設定変更を受信）
    let (signal_engine, signal_engine_tx, signal_rx) =
        SignalEngine::new(indicators, market_bus.candle_rx(), config_rx);
    tokio::spawn(signal_engine.run());

    // 最新シグナルをキャッシュするタスク（API配信用）
    let latest_signal: Arc<RwLock<Option<crate::signal::SignalDetail>>> =
        Arc::new(RwLock::new(None));
    {
        let cache = Arc::clone(&latest_signal);
        let mut rx = signal_engine_tx.subscribe();
        tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(sig) => { *cache.write().await = Some(sig); }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => break,
                }
            }
        });
    }
    // ウォームアップ結果でシグナルキャッシュを事前初期化（ライブCandle待ちで様子見になるのを防ぐ）
    if !warmup_signals.is_empty() {
        use crate::signal::Signal;
        let raw: Vec<Option<Signal>> =
            warmup_signals.iter().map(|is| is.signal.clone()).collect();
        let aggregated = SignalEngine::aggregate(&raw, init_cfg.signal_threshold);
        let detail = SignalDetail {
            aggregate: aggregated,
            indicators: warmup_signals,
            calculated_at: chrono::Utc::now(),
            calculation_state: "active".to_string(),
        };
        *latest_signal.write().await = Some(detail);
        info!("Pre-populated signal cache from warmup data");
    }

    // TradingEngine: Signal → RiskManager → send_order
    let trading_engine = TradingEngine::new(
        signal_rx,
        Arc::clone(&client),
        RiskManager::new(init_risk_params),
        "BTC_JPY".to_string(),
    )
    .with_config(Arc::clone(&trading_config))
    .with_order_repo(db.orders());
    tokio::spawn(trading_engine.run());

    // News AI タスク起動（5分ごとにRSSフィードを取得、新規記事のみOllamaでセンチメント分析）
    let news_cache = Arc::new(RwLock::new(vec![]));
    {
        let cache = Arc::clone(&news_cache);
        let news_repo = db.news_sentiments();
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
        // 起動時にDBから既存スコアをキャッシュに読み込む
        match news_repo.get_latest(50).await {
            Ok(existing) if !existing.is_empty() => {
                info!("Loaded {} news scores from DB into cache", existing.len());
                *cache.write().await = existing;
            }
            Ok(_) => {}
            Err(e) => warn!("Failed to load news from DB on startup: {}", e),
        }

        tokio::spawn(async move {
            loop {
                match news::fetcher::FeedSource::fetch_latest(&fetcher).await {
                    Ok(items) => {
                        // 既存URL取得して重複除外
                        let existing_urls = news_repo.get_existing_urls().await.unwrap_or_default();
                        let new_items: Vec<_> = items
                            .into_iter()
                            .filter(|item| !item.url.is_empty() && !existing_urls.contains(&item.url))
                            .collect();

                        if new_items.is_empty() {
                            info!("No new news items (all duplicates in DB)");
                        } else {
                            let scores = analyzer.analyze_batch(&new_items).await;
                            let avg = scorer.average_score(&scores);
                            info!(
                                "News sentiment: avg_score={:.2}, new_items={}",
                                avg,
                                scores.len()
                            );
                            if let Err(e) = news_repo.insert_batch(&new_items, &scores).await {
                                warn!("Failed to save news sentiments to DB: {}", e);
                            }
                        }

                        // 常にDBから最新スコアをキャッシュに反映
                        match news_repo.get_latest(50).await {
                            Ok(latest) => *cache.write().await = latest,
                            Err(e) => warn!("Failed to refresh news cache from DB: {}", e),
                        }
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

    let api_state = api::AppState {
        product_code: "BTC_JPY".to_string(),
        db: db.clone(),
        exchange: Arc::clone(&client),
        candle_tx: market_bus.candle_tx(),
        ticker_tx: market_bus.ticker_tx(),
        signal_tx: signal_engine_tx,
        ws_tx: ws_tx_for_api,
        aggregator_registry: std::sync::Arc::new(tokio::sync::Mutex::new(
            std::collections::HashMap::new(),
        )),
        latest_signal,
        news_cache,
        trading_config: Arc::clone(&trading_config),
        config_tx: Arc::clone(&config_tx),
    };
    tokio::spawn(api::server::run(api_state, addr));

    // Auto-updater: check GitHub Releases every 60 minutes.
    // The first check is delayed so the process is fully initialised before
    // any binary replacement occurs.
    updater::spawn_update_loop("seita", "shirube", 60 * 60);

    // Keep the main task alive
    tokio::signal::ctrl_c().await?;
    info!("Shutdown signal received");
    Ok(())
}
