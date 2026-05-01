pub mod routes;
pub mod server;
pub mod ws_handler;

use std::sync::Arc;

use tokio::sync::{broadcast, RwLock};

use crate::config::TradingConfig;
use crate::exchange::ExchangeClient;
use crate::news::analyzer::SentimentScore;
use crate::signal::SignalDetail;
use crate::storage::db::Database;
use crate::types::market::Candle;

#[derive(Clone)]
pub struct AppState {
    pub db: Database,
    pub exchange: Arc<dyn ExchangeClient>,
    pub candle_tx: broadcast::Sender<Candle>,
    pub signal_tx: broadcast::Sender<SignalDetail>,
    /// Rust SignalEngine が最後に出した最新シグナルのキャッシュ（集計 + 各インジケータ）
    pub latest_signal: Arc<RwLock<Option<SignalDetail>>>,
    /// 最新のニュースセンチメントスコアキャッシュ
    pub news_cache: Arc<RwLock<Vec<SentimentScore>>>,
    /// 取引設定（UIから変更可能）
    pub trading_config: Arc<RwLock<TradingConfig>>,
}
