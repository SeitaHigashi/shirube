use chrono::{DateTime, TimeZone, Utc};
use rust_decimal::Decimal;
use tracing::{info, warn};

use crate::storage::db::Database;
use crate::types::market::{Candle, Ticker, Trade};

/// Accumulates raw price events (trades or tickers) into fixed-duration
/// OHLCV candles.
///
/// Time buckets are aligned to UTC epoch boundaries:
///   bucket_start = floor(unix_timestamp / resolution_secs) * resolution_secs
///
/// A candle is finalized (and returned to the caller) when the first event
/// belonging to a *new* bucket arrives.  Until that point the accumulating
/// candle is available as a snapshot via `peek_current` without being closed.
pub struct CandleAggregator {
    product_code: String,
    resolution_secs: u32,
    /// Candle currently being accumulated; `None` until the first event
    current: Option<PartialCandle>,
}

#[derive(Clone)]
struct PartialCandle {
    open_time: DateTime<Utc>,
    open: Decimal,
    high: Decimal,
    low: Decimal,
    close: Decimal,
    volume: Decimal,
}

impl PartialCandle {
    fn new(open_time: DateTime<Utc>, price: Decimal, volume: Decimal) -> Self {
        Self {
            open_time,
            open: price,
            high: price,
            low: price,
            close: price,
            volume,
        }
    }

    fn update(&mut self, price: Decimal, volume: Decimal) {
        if price > self.high {
            self.high = price;
        }
        if price < self.low {
            self.low = price;
        }
        self.close = price;
        self.volume += volume;
    }

    fn finalize(self, product_code: &str, resolution_secs: u32) -> Candle {
        Candle {
            product_code: product_code.to_string(),
            open_time: self.open_time,
            resolution_secs,
            open: self.open,
            high: self.high,
            low: self.low,
            close: self.close,
            volume: self.volume,
        }
    }
}

impl CandleAggregator {
    pub fn new(product_code: String, resolution_secs: u32) -> Self {
        Self {
            product_code,
            resolution_secs,
            current: None,
        }
    }

    /// Truncate `ts` to the nearest preceding bucket boundary.
    ///
    /// Uses integer arithmetic on the Unix timestamp so that bucket
    /// boundaries are always aligned to the UTC epoch, regardless of
    /// local timezone or DST changes.
    fn bucket_time(&self, ts: DateTime<Utc>) -> DateTime<Utc> {
        let ts_secs = ts.timestamp();
        // Integer division floors towards zero, giving the bucket start
        let bucket = (ts_secs / self.resolution_secs as i64) * self.resolution_secs as i64;
        Utc.timestamp_opt(bucket, 0).unwrap()
    }

    /// Ticker から更新（REST ポーリング由来）。
    /// 新しい bucket に入ったとき、旧 Candle を確定して返す。
    pub fn feed_ticker(&mut self, ticker: &Ticker) -> Option<Candle> {
        let bucket = self.bucket_time(ticker.timestamp);
        self.feed_price(bucket, ticker.ltp, Decimal::ZERO)
    }

    /// Trade スライスから更新（WS executions 由来）。
    /// bucket をまたぐごとに確定 Candle を返す。
    pub fn feed_trades(&mut self, trades: &[Trade]) -> Vec<Candle> {
        let mut finalized = Vec::new();
        for trade in trades {
            let bucket = self.bucket_time(trade.exec_date);
            if let Some(candle) = self.feed_price(bucket, trade.price, trade.size) {
                finalized.push(candle);
            }
        }
        finalized
    }

    /// 現在の確定前 partial candle をスナップショットとして返す（確定しない）。
    /// Ticker 到着ごとにリアルタイムでチャートに反映するために使用する。
    pub fn peek_current(&self) -> Option<Candle> {
        self.current
            .as_ref()
            .map(|pc| pc.clone().finalize(&self.product_code, self.resolution_secs))
    }

    /// Core price ingestion logic shared by `feed_ticker` and `feed_trades`.
    ///
    /// State machine with three cases:
    /// 1. No candle yet  → open a new partial candle, return `None`
    /// 2. Same bucket    → update OHLCV of the current partial, return `None`
    /// 3. New bucket     → finalize the old candle, open a new partial,
    ///                     return the finalized `Candle`
    fn feed_price(
        &mut self,
        bucket: DateTime<Utc>,
        price: Decimal,
        volume: Decimal,
    ) -> Option<Candle> {
        match &mut self.current {
            None => {
                // First event ever: start accumulating
                self.current = Some(PartialCandle::new(bucket, price, volume));
                None
            }
            Some(cur) if cur.open_time == bucket => {
                // Same time bucket: update high/low/close/volume in place
                cur.update(price, volume);
                None
            }
            Some(_) => {
                // New bucket: close the previous candle and open a fresh one
                let old = self.current.take().unwrap();
                let finalized = old.finalize(&self.product_code, self.resolution_secs);
                self.current = Some(PartialCandle::new(bucket, price, volume));
                Some(finalized)
            }
        }
    }
}

/// `CandleAggregator` を生成し、起動時バケット内の保存済み Ticker を DB から
/// 取得して事前投入（pre-seed）する。
///
/// 起動タイミングがバケットの途中であっても、バケット先頭からのデータを
/// アグリゲーターに反映させることで OHLCV の欠損を防ぐ。
///
/// DB 取得に失敗した場合は警告ログを出力し、空の状態でアグリゲーターを返す。
pub async fn build_seeded_aggregator(
    product_code: &str,
    resolution_secs: u32,
    db: &Database,
) -> CandleAggregator {
    let mut agg = CandleAggregator::new(product_code.to_string(), resolution_secs);

    // 現在バケットの開始・終了時刻を計算する
    let now = Utc::now();
    let bucket_start_ts =
        (now.timestamp() / resolution_secs as i64) * resolution_secs as i64;
    let bucket_start = Utc.timestamp_opt(bucket_start_ts, 0).unwrap();
    let bucket_end_ts = bucket_start_ts + resolution_secs as i64;
    let bucket_end = Utc.timestamp_opt(bucket_end_ts, 0).unwrap();

    match db.tickers().range(product_code, bucket_start, bucket_end).await {
        Ok(stored) => {
            let count = stored.len();
            for st in &stored {
                agg.feed_ticker(&Ticker::from(st));
            }
            if count > 0 {
                info!(
                    "Pre-seeded {} tickers into CandleAggregator for bucket {}",
                    count, bucket_start
                );
            }
        }
        Err(e) => {
            warn!(
                "Failed to pre-seed CandleAggregator from DB: {}. Starting from scratch.",
                e
            );
        }
    }

    agg
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use rust_decimal_macros::dec;

    fn ticker(ts_secs: i64, ltp: Decimal) -> Ticker {
        use crate::types::market::Ticker;
        Ticker {
            product_code: "BTC_JPY".into(),
            timestamp: Utc.timestamp_opt(ts_secs, 0).unwrap(),
            best_bid: ltp,
            best_ask: ltp,
            best_bid_size: dec!(0.1),
            best_ask_size: dec!(0.1),
            ltp,
            volume: dec!(100),
            volume_by_product: dec!(100),
        }
    }

    fn trade(ts_secs: i64, price: Decimal, size: Decimal) -> Trade {
        use crate::types::market::{Trade, TradeSide};
        Trade {
            id: ts_secs,
            exec_date: Utc.timestamp_opt(ts_secs, 0).unwrap(),
            price,
            size,
            side: TradeSide::Buy,
            buy_child_order_acceptance_id: "".into(),
            sell_child_order_acceptance_id: "".into(),
        }
    }

    #[test]
    fn first_ticker_returns_none() {
        let mut agg = CandleAggregator::new("BTC_JPY".into(), 60);
        assert!(agg.feed_ticker(&ticker(0, dec!(9000000))).is_none());
    }

    #[test]
    fn same_bucket_updates_high_low_close() {
        let mut agg = CandleAggregator::new("BTC_JPY".into(), 60);
        agg.feed_ticker(&ticker(0, dec!(9000000)));
        agg.feed_ticker(&ticker(10, dec!(9010000))); // 同 bucket, high 更新
        agg.feed_ticker(&ticker(20, dec!(8990000))); // 同 bucket, low 更新
        // 新 bucket で旧 Candle 確定
        let candle = agg.feed_ticker(&ticker(60, dec!(9005000))).unwrap();
        assert_eq!(candle.open, dec!(9000000));
        assert_eq!(candle.high, dec!(9010000));
        assert_eq!(candle.low, dec!(8990000));
        assert_eq!(candle.close, dec!(8990000));
    }

    #[test]
    fn new_bucket_finalizes_previous_candle() {
        let mut agg = CandleAggregator::new("BTC_JPY".into(), 60);
        agg.feed_ticker(&ticker(0, dec!(9000000)));
        let candle = agg.feed_ticker(&ticker(60, dec!(9001000)));
        assert!(candle.is_some());
        let c = candle.unwrap();
        assert_eq!(c.open_time, Utc.timestamp_opt(0, 0).unwrap());
        assert_eq!(c.resolution_secs, 60);
    }

    #[test]
    fn feed_trades_empty_returns_nothing() {
        let mut agg = CandleAggregator::new("BTC_JPY".into(), 60);
        assert!(agg.feed_trades(&[]).is_empty());
    }

    #[test]
    fn feed_trades_spanning_two_buckets() {
        let mut agg = CandleAggregator::new("BTC_JPY".into(), 60);
        let trades = vec![
            trade(0, dec!(9000000), dec!(0.1)),
            trade(30, dec!(9005000), dec!(0.2)),
            trade(60, dec!(9010000), dec!(0.1)), // 新 bucket
        ];
        let finalized = agg.feed_trades(&trades);
        assert_eq!(finalized.len(), 1);
        assert_eq!(finalized[0].open, dec!(9000000));
        assert_eq!(finalized[0].close, dec!(9005000));
    }

    #[test]
    fn volume_accumulates_from_trades() {
        let mut agg = CandleAggregator::new("BTC_JPY".into(), 60);
        let trades = vec![
            trade(0, dec!(9000000), dec!(0.1)),
            trade(10, dec!(9000000), dec!(0.2)),
            trade(60, dec!(9000000), dec!(0.3)), // 新 bucket で確定
        ];
        let finalized = agg.feed_trades(&trades);
        assert_eq!(finalized.len(), 1);
        assert_eq!(finalized[0].volume, dec!(0.3)); // 0.1 + 0.2
    }

    #[test]
    fn peek_current_returns_partial_candle() {
        let mut agg = CandleAggregator::new("BTC_JPY".into(), 60);
        assert!(agg.peek_current().is_none());
        agg.feed_ticker(&ticker(0, dec!(9000000)));
        let partial = agg.peek_current().unwrap();
        assert_eq!(partial.open, dec!(9000000));
        assert_eq!(partial.close, dec!(9000000));
        assert_eq!(partial.open_time, Utc.timestamp_opt(0, 0).unwrap());
    }

    #[test]
    fn peek_current_does_not_finalize() {
        let mut agg = CandleAggregator::new("BTC_JPY".into(), 60);
        agg.feed_ticker(&ticker(0, dec!(9000000)));
        agg.feed_ticker(&ticker(30, dec!(9010000)));
        let _ = agg.peek_current();
        let _ = agg.peek_current(); // 2回呼んでも current は消えない
        let candle = agg.feed_ticker(&ticker(60, dec!(9005000))).unwrap();
        assert_eq!(candle.high, dec!(9010000));
    }

    /// 起動前のバケット内データを事前投入した場合、最初の確定 Candle が
    /// 事前投入分のデータを含むことを確認する。
    /// DB からウォームアップした Ticker を feed_ticker で投入するシナリオを模擬する。
    #[test]
    fn pre_seeded_tickers_are_reflected_in_first_candle() {
        let mut agg = CandleAggregator::new("BTC_JPY".into(), 60);
        // バケット前半（0〜29秒）: 起動前に DB に保存済みのデータとして事前投入
        agg.feed_ticker(&ticker(0, dec!(9000000)));
        agg.feed_ticker(&ticker(15, dec!(9010000)));
        agg.feed_ticker(&ticker(29, dec!(8990000)));
        // バケット後半（30〜59秒）: WS 受信後のデータ
        agg.feed_ticker(&ticker(45, dec!(9005000)));
        // 新バケット到着で旧バケット確定
        let candle = agg.feed_ticker(&ticker(60, dec!(9020000))).unwrap();
        assert_eq!(candle.open, dec!(9000000));  // 事前投入の最初
        assert_eq!(candle.high, dec!(9010000));  // 事前投入の高値
        assert_eq!(candle.low, dec!(8990000));   // 事前投入の安値
        assert_eq!(candle.close, dec!(9005000)); // WS 受信の最後
    }
}
