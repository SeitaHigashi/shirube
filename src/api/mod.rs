pub mod routes;
pub mod server;
pub mod ws_handler;

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::{broadcast, watch, Mutex, RwLock};

use crate::config::TradingConfig;
use crate::exchange::ExchangeClient;
use crate::news::analyzer::SentimentScore;
use crate::signal::SignalDetail;
use crate::storage::db::Database;
use crate::types::market::{Candle, Ticker};

/// Arc/Weak パターンを用いた per-resolution CandleAggregator レジストリ。
///
/// キー: resolution_secs, 値: broadcast::Sender<Candle> への弱参照。
/// WS接続が Arc<Sender> を保持している間だけ Sender が生存し、
/// 全接続が切れると refcount=0 で Sender が自動 drop される。
/// バックグラウンドタスクは Weak::upgrade() で生存を確認し、
/// None になった時点で終了してアグリゲーターも自動 drop する。
pub type AggregatorRegistry =
    Arc<Mutex<HashMap<u32, std::sync::Weak<broadcast::Sender<Candle>>>>>;

#[derive(Clone)]
pub struct AppState {
    pub db: Database,
    pub exchange: Arc<dyn ExchangeClient>,
    pub candle_tx: broadcast::Sender<Candle>,
    pub ticker_tx: broadcast::Sender<Ticker>,
    pub signal_tx: broadcast::Sender<SignalDetail>,
    /// Per-resolution CandleAggregator レジストリ（Arc/Weak による自動 cleanup）
    pub aggregator_registry: AggregatorRegistry,
    /// Rust SignalEngine が最後に出した最新シグナルのキャッシュ（集計 + 各インジケータ）
    pub latest_signal: Arc<RwLock<Option<SignalDetail>>>,
    /// 最新のニュースセンチメントスコアキャッシュ
    pub news_cache: Arc<RwLock<Vec<SentimentScore>>>,
    /// 取引設定（UIから変更可能、RwLock経由でAPIが読み書き）
    pub trading_config: Arc<RwLock<TradingConfig>>,
    /// 設定変更を SignalEngine にライブ配信するチャネル
    pub config_tx: Arc<watch::Sender<TradingConfig>>,
}
