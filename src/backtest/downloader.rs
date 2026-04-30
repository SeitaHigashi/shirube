use async_trait::async_trait;
use chrono::{DateTime, Utc};
use tracing::info;

use crate::error::Result;
use crate::market::candle_aggregator::CandleAggregator;
use crate::storage::db::Database;
use crate::types::market::{Candle, Trade};

use super::BacktestConfig;

/// 過去約定履歴を提供するトレイト（テスト差し替え可能）
#[async_trait]
pub trait ExecutionSource: Send + Sync {
    /// `before_id` より古い約定を `count` 件取得する（ページネーション用）
    async fn fetch_executions(
        &self,
        product_code: &str,
        before_id: Option<i64>,
        count: u32,
    ) -> Result<Vec<Trade>>;
}

/// 過去 OHLCV データを取得し DB にキャッシュするダウンローダー
pub struct Downloader {
    source: Box<dyn ExecutionSource>,
    db: Database,
}

impl Downloader {
    pub fn new(source: Box<dyn ExecutionSource>, db: Database) -> Self {
        Self { source, db }
    }

    /// DB にキャッシュ済みのデータを優先して返す。
    /// キャッシュがない期間は `source` からフェッチして DB に保存する。
    pub async fn load(&self, config: &BacktestConfig) -> Result<Vec<Candle>> {
        let cached = self
            .db
            .candles()
            .get_range(
                &config.product_code,
                config.resolution_secs,
                config.from,
                config.to,
                Some(100_000),
            )
            .await?;

        if !cached.is_empty() {
            info!(
                "Loaded {} cached candles for {} [{} - {}]",
                cached.len(),
                config.product_code,
                config.from,
                config.to
            );
            return Ok(cached);
        }

        // キャッシュなし — REST からフェッチ
        let candles = self.fetch_and_store(config).await?;
        Ok(candles)
    }

    /// 約定履歴をページネーションで全取得し、Candle に集計して DB 保存する
    async fn fetch_and_store(&self, config: &BacktestConfig) -> Result<Vec<Candle>> {
        let mut aggregator =
            CandleAggregator::new(config.product_code.clone(), config.resolution_secs);
        let mut all_candles: Vec<Candle> = Vec::new();
        let mut before_id: Option<i64> = None;
        const PAGE_SIZE: u32 = 500;

        loop {
            let executions = self
                .source
                .fetch_executions(&config.product_code, before_id, PAGE_SIZE)
                .await?;

            if executions.is_empty() {
                break;
            }

            // 最後の id を次ページの before_id にする（bitFlyer は降順で返すので最後が最古）
            before_id = executions.last().map(|e| e.id - 1);

            // 取得した約定を時系列順（昇順）にソートして Aggregator へ流す
            let mut sorted = executions;
            sorted.sort_by_key(|e| e.exec_date);

            // config.from より前のものは除外し、config.to 以後なら終了
            let relevant: Vec<Trade> = sorted
                .into_iter()
                .filter(|e| e.exec_date >= config.from && e.exec_date <= config.to)
                .collect();

            if relevant.is_empty() {
                // この取得バッチが全て範囲外 → 古くなりすぎたので終了
                break;
            }

            let candles = aggregator.feed_trades(&relevant);
            all_candles.extend(candles);
        }

        if !all_candles.is_empty() {
            self.db.candles().upsert_batch(&all_candles).await?;
            info!(
                "Downloaded and stored {} candles for {}",
                all_candles.len(),
                config.product_code
            );
        }

        Ok(all_candles)
    }
}

// ──────────────────────────────────────────────────────────────────────────────
// bitFlyer REST を `ExecutionSource` として使うアダプタは rest.rs 側で実装する。
// ここではテスト用の MockExecutionSource を定義する。
// ──────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
pub mod mock {
    use super::*;
    use crate::types::market::TradeSide;
    use rust_decimal::Decimal;
    use std::sync::{Arc, Mutex};

    pub struct MockExecutionSource {
        trades: Arc<Mutex<Vec<Trade>>>,
    }

    impl MockExecutionSource {
        pub fn new(trades: Vec<Trade>) -> Self {
            Self {
                trades: Arc::new(Mutex::new(trades)),
            }
        }
    }

    #[async_trait]
    impl ExecutionSource for MockExecutionSource {
        async fn fetch_executions(
            &self,
            _product_code: &str,
            before_id: Option<i64>,
            count: u32,
        ) -> Result<Vec<Trade>> {
            let trades = self.trades.lock().unwrap();
            // before_id より小さい id のものを最大 count 件返す（降順）
            let mut result: Vec<Trade> = trades
                .iter()
                .filter(|t| before_id.map_or(true, |bid| t.id < bid))
                .take(count as usize)
                .cloned()
                .collect();
            // bitFlyer は降順で返す
            result.sort_by(|a, b| b.id.cmp(&a.id));
            Ok(result)
        }
    }

    pub fn make_trade(id: i64, exec_date: DateTime<Utc>, price: Decimal) -> Trade {
        Trade {
            id,
            exec_date,
            price,
            size: Decimal::new(1, 3), // 0.001
            side: TradeSide::Buy,
            buy_child_order_acceptance_id: format!("buy-{}", id),
            sell_child_order_acceptance_id: format!("sell-{}", id),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::mock::{make_trade, MockExecutionSource};
    use super::*;
    use crate::storage::db::Database;
    use chrono::TimeZone;
    use rust_decimal_macros::dec;

    fn make_config(from: DateTime<Utc>, to: DateTime<Utc>) -> BacktestConfig {
        BacktestConfig {
            product_code: "BTC_JPY".into(),
            from,
            to,
            resolution_secs: 60,
            slippage_pct: 0.001,
            fee_pct: 0.0015,
            initial_jpy: dec!(1_000_000),
        }
    }

    #[tokio::test]
    async fn fetch_and_store_aggregates_trades_into_candles() {
        let from = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
        let to = Utc.with_ymd_and_hms(2024, 1, 1, 0, 5, 0).unwrap();

        // 2本のcandle分のトレードを生成（0:00 と 0:01 のバケット）
        let trades = vec![
            make_trade(1, Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 10).unwrap(), dec!(9_000_000)),
            make_trade(2, Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 30).unwrap(), dec!(9_010_000)),
            make_trade(3, Utc.with_ymd_and_hms(2024, 1, 1, 0, 1, 10).unwrap(), dec!(9_020_000)),
        ];

        let source = MockExecutionSource::new(trades);
        let db = Database::open_in_memory().await.unwrap();
        let downloader = Downloader::new(Box::new(source), db.clone());

        let config = make_config(from, to);
        let candles = downloader.load(&config).await.unwrap();

        // 0:00 と 0:01 の 2 candle が出来る（0:01 は最後の trade で finalize される）
        assert!(!candles.is_empty());
        // DB にも保存されているはず
        let cached = db
            .candles()
            .get_range("BTC_JPY", 60, from, to, None)
            .await
            .unwrap();
        assert!(!cached.is_empty());
    }

    #[tokio::test]
    async fn load_returns_cached_candles_without_fetching() {
        let from = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
        let to = Utc.with_ymd_and_hms(2024, 1, 1, 1, 0, 0).unwrap();

        let db = Database::open_in_memory().await.unwrap();

        // 事前にキャッシュを挿入
        let candle = crate::types::market::Candle {
            product_code: "BTC_JPY".into(),
            open_time: from,
            resolution_secs: 60,
            open: dec!(9_000_000),
            high: dec!(9_010_000),
            low: dec!(8_990_000),
            close: dec!(9_005_000),
            volume: dec!(1),
        };
        db.candles().upsert(&candle).await.unwrap();

        // ソースが呼ばれないことを確認（空のソース）
        let source = MockExecutionSource::new(vec![]);
        let downloader = Downloader::new(Box::new(source), db.clone());

        let config = make_config(from, to);
        let result = downloader.load(&config).await.unwrap();
        assert_eq!(result.len(), 1);
    }

    #[tokio::test]
    async fn trades_outside_range_are_filtered() {
        let from = Utc.with_ymd_and_hms(2024, 1, 1, 1, 0, 0).unwrap();
        let to = Utc.with_ymd_and_hms(2024, 1, 1, 2, 0, 0).unwrap();

        // 範囲外のトレードだけ
        let trades = vec![
            make_trade(1, Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap(), dec!(9_000_000)),
        ];

        let source = MockExecutionSource::new(trades);
        let db = Database::open_in_memory().await.unwrap();
        let downloader = Downloader::new(Box::new(source), db);

        let config = make_config(from, to);
        let candles = downloader.load(&config).await.unwrap();
        // 範囲外なので candle なし
        assert!(candles.is_empty());
    }
}
