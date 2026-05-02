use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use std::collections::HashMap;
use std::str::FromStr;
use tokio_rusqlite::Connection;

use crate::error::Result;
use crate::types::market::{Candle, StoredTicker, Ticker};

pub struct TickerRepository {
    conn: Connection,
}

/// ダウンサンプリングの結果統計
pub struct DownsampleStats {
    /// 削除されたティック数
    pub tickers_deleted: u64,
}

/// 内部処理用の生行（id 込み）
struct RawRow {
    id: i64,
    timestamp: DateTime<Utc>,
    best_bid: Decimal,
    best_ask: Decimal,
    best_bid_size: Decimal,
    best_ask_size: Decimal,
    ltp_open: Decimal,
    ltp: Decimal,
    ltp_high: Decimal,
    ltp_low: Decimal,
    volume: Decimal,
    volume_by_product: Decimal,
}

impl TickerRepository {
    pub fn new(conn: Connection) -> Self {
        Self { conn }
    }

    /// Ticker を生ティックとして保存する（ltp_open = ltp = ltp_high = ltp_low）。
    /// 同一 (product_code, timestamp) は INSERT OR IGNORE で無視する。
    pub async fn insert(&self, ticker: &Ticker) -> Result<()> {
        let product_code = ticker.product_code.clone();
        let timestamp = ticker.timestamp.to_rfc3339();
        let best_bid = ticker.best_bid.to_string();
        let best_ask = ticker.best_ask.to_string();
        let best_bid_size = ticker.best_bid_size.to_string();
        let best_ask_size = ticker.best_ask_size.to_string();
        let ltp = ticker.ltp.to_string();
        let volume = ticker.volume.to_string();
        let volume_by_product = ticker.volume_by_product.to_string();

        self.conn
            .call(move |c| {
                c.execute(
                    "INSERT OR IGNORE INTO tickers
                        (product_code, timestamp,
                         best_bid, best_ask, best_bid_size, best_ask_size,
                         ltp_open, ltp, ltp_high, ltp_low,
                         volume, volume_by_product)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7, ?7, ?7, ?8, ?9)",
                    rusqlite::params![
                        product_code, timestamp,
                        best_bid, best_ask, best_bid_size, best_ask_size,
                        ltp,
                        volume, volume_by_product
                    ],
                )?;
                Ok(())
            })
            .await?;
        Ok(())
    }

    /// StoredTicker を OHLCV を保持したまま保存する。
    /// 同一 (product_code, timestamp) は INSERT OR IGNORE で無視する。
    pub async fn insert_stored(&self, ticker: &StoredTicker) -> Result<()> {
        let product_code = ticker.product_code.clone();
        let timestamp = ticker.timestamp.to_rfc3339();
        let best_bid = ticker.best_bid.to_string();
        let best_ask = ticker.best_ask.to_string();
        let best_bid_size = ticker.best_bid_size.to_string();
        let best_ask_size = ticker.best_ask_size.to_string();
        let ltp_open = ticker.ltp_open.to_string();
        let ltp = ticker.ltp.to_string();
        let ltp_high = ticker.ltp_high.to_string();
        let ltp_low = ticker.ltp_low.to_string();
        let volume = ticker.volume.to_string();
        let volume_by_product = ticker.volume_by_product.to_string();

        self.conn
            .call(move |c| {
                c.execute(
                    "INSERT OR IGNORE INTO tickers
                        (product_code, timestamp,
                         best_bid, best_ask, best_bid_size, best_ask_size,
                         ltp_open, ltp, ltp_high, ltp_low,
                         volume, volume_by_product)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                    rusqlite::params![
                        product_code, timestamp,
                        best_bid, best_ask, best_bid_size, best_ask_size,
                        ltp_open, ltp, ltp_high, ltp_low,
                        volume, volume_by_product
                    ],
                )?;
                Ok(())
            })
            .await?;
        Ok(())
    }

    /// 直近 N 件の StoredTicker を返す（古い順）。
    pub async fn latest(&self, product_code: &str, count: u32) -> Result<Vec<StoredTicker>> {
        let pc = product_code.to_string();
        let rows = self
            .conn
            .call(move |c| {
                let mut stmt = c.prepare(
                    "SELECT product_code, timestamp,
                            best_bid, best_ask, best_bid_size, best_ask_size,
                            ltp_open, ltp, ltp_high, ltp_low, volume, volume_by_product
                     FROM tickers
                     WHERE product_code = ?1
                     ORDER BY timestamp DESC
                     LIMIT ?2",
                )?;
                let rows = stmt
                    .query_map(rusqlite::params![pc, count], row_to_stored_ticker)?
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                Ok(rows)
            })
            .await?;
        let mut rows = rows;
        rows.reverse();
        Ok(rows)
    }

    /// 時間範囲の StoredTicker を返す（古い順）。
    pub async fn range(
        &self,
        product_code: &str,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<Vec<StoredTicker>> {
        let pc = product_code.to_string();
        let from_str = from.to_rfc3339();
        let to_str = to.to_rfc3339();
        let rows = self
            .conn
            .call(move |c| {
                let mut stmt = c.prepare(
                    "SELECT product_code, timestamp,
                            best_bid, best_ask, best_bid_size, best_ask_size,
                            ltp_open, ltp, ltp_high, ltp_low, volume, volume_by_product
                     FROM tickers
                     WHERE product_code = ?1
                       AND timestamp >= ?2
                       AND timestamp <= ?3
                     ORDER BY timestamp ASC",
                )?;
                let rows = stmt
                    .query_map(
                        rusqlite::params![pc, from_str, to_str],
                        row_to_stored_ticker,
                    )?
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                Ok(rows)
            })
            .await?;
        Ok(rows)
    }

    /// tickers テーブルから任意の resolution_secs でキャンドルを集約して返す。
    /// ltp_open/ltp_high/ltp_low/ltp を OHLC として使用する。
    pub async fn get_aggregated(
        &self,
        product_code: &str,
        resolution_secs: u32,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
        limit: Option<u32>,
    ) -> Result<Vec<Candle>> {
        let pc = product_code.to_string();
        let from_str = from.to_rfc3339();
        let to_str = to.to_rfc3339();
        let limit = limit.unwrap_or(1000);
        let res = resolution_secs as i64;

        let candles = self
            .conn
            .call(move |c| {
                let mut stmt = c.prepare(
                    "WITH numbered AS (
                       SELECT product_code, timestamp,
                         (strftime('%s', timestamp) / ?1) * ?1 AS bucket_ts,
                         CAST(ltp_open AS REAL) AS open,
                         CAST(ltp_high AS REAL) AS high,
                         CAST(ltp_low  AS REAL) AS low,
                         CAST(ltp      AS REAL) AS close,
                         CAST(volume   AS REAL) AS volume,
                         ROW_NUMBER() OVER (
                           PARTITION BY product_code, (strftime('%s', timestamp) / ?1)
                           ORDER BY timestamp ASC
                         ) AS rn_asc,
                         ROW_NUMBER() OVER (
                           PARTITION BY product_code, (strftime('%s', timestamp) / ?1)
                           ORDER BY timestamp DESC
                         ) AS rn_desc
                       FROM tickers
                       WHERE product_code = ?2
                         AND timestamp >= ?3
                         AND timestamp <= ?4
                     ),
                     agg AS (
                       SELECT product_code, bucket_ts,
                         MAX(CASE WHEN rn_asc  = 1 THEN open  END) AS open,
                         MAX(high) AS high,
                         MIN(low)  AS low,
                         MAX(CASE WHEN rn_desc = 1 THEN close END) AS close,
                         SUM(volume) AS volume
                       FROM numbered
                       GROUP BY product_code, bucket_ts
                     )
                     SELECT product_code,
                       strftime('%Y-%m-%dT%H:%M:%SZ', bucket_ts, 'unixepoch') AS open_time,
                       ?1 AS resolution_secs,
                       CAST(open   AS TEXT),
                       CAST(high   AS TEXT),
                       CAST(low    AS TEXT),
                       CAST(close  AS TEXT),
                       CAST(volume AS TEXT)
                     FROM agg
                     ORDER BY bucket_ts ASC
                     LIMIT ?5",
                )?;
                let rows = stmt.query_map(
                    rusqlite::params![res, pc, from_str, to_str, limit],
                    row_to_candle,
                )?;
                Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
            })
            .await?;
        Ok(candles)
    }

    /// 直近 count バケット分のキャンドルをウォームアップ用に返す。
    pub async fn get_latest_as_candles(
        &self,
        product_code: &str,
        resolution_secs: u32,
        count: u32,
    ) -> Result<Vec<Candle>> {
        let to = Utc::now();
        let from = to
            - chrono::Duration::seconds((resolution_secs as i64) * (count as i64) * 2);
        let mut candles = self
            .get_aggregated(product_code, resolution_secs, from, to, Some(count))
            .await?;
        // 最新 count 件に絞る（超過分を先頭から削除）
        if candles.len() > count as usize {
            candles.drain(..candles.len() - count as usize);
        }
        Ok(candles)
    }

    /// 段階的ダウンサンプリングを実行する。
    ///
    /// - 1〜7日前: 5 秒バケットに集約
    /// - 7日以前 : 60 秒バケットに集約
    /// - 直近1日 : 変更なし
    ///
    /// 各バケットの代表行（最小 id）を OHLCV 集計値で UPDATE してから、
    /// 残りの行を DELETE する。
    pub async fn downsample(&self, product_code: &str) -> Result<DownsampleStats> {
        let now = Utc::now();
        let day1_ago = now - chrono::Duration::days(1);
        let day7_ago = now - chrono::Duration::days(7);

        // zone1: 1〜7日前 → 5秒バケット
        let deleted1 = self
            .compact_zone(product_code, day7_ago, day1_ago, 5)
            .await?;

        // zone2: 7日以前 → 60秒バケット
        let deleted2 = self
            .compact_zone(
                product_code,
                DateTime::<Utc>::from_timestamp(0, 0).unwrap(),
                day7_ago,
                60,
            )
            .await?;

        Ok(DownsampleStats {
            tickers_deleted: deleted1 + deleted2,
        })
    }

    /// 指定ゾーンを bucket_secs 秒バケットに圧縮する。
    /// 各バケットで MIN(id) 行を代表として OHLCV を更新し、他の行を削除する。
    /// 戻り値: 削除した行数
    async fn compact_zone(
        &self,
        product_code: &str,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
        bucket_secs: i64,
    ) -> Result<u64> {
        let pc = product_code.to_string();
        let from_str = from.to_rfc3339();
        let to_str = to.to_rfc3339();

        // ゾーン内の全行を取得（id 昇順）
        let raw_rows: Vec<RawRow> = self
            .conn
            .call({
                let pc = pc.clone();
                let from_str = from_str.clone();
                let to_str = to_str.clone();
                move |c| {
                    let mut stmt = c.prepare(
                        "SELECT id, timestamp,
                                best_bid, best_ask, best_bid_size, best_ask_size,
                                ltp_open, ltp, ltp_high, ltp_low, volume, volume_by_product
                         FROM tickers
                         WHERE product_code = ?1
                           AND timestamp >= ?2
                           AND timestamp <  ?3
                         ORDER BY id ASC",
                    )?;
                    let rows = stmt
                        .query_map(rusqlite::params![pc, from_str, to_str], row_to_raw)?
                        .collect::<std::result::Result<Vec<_>, _>>()?;
                    Ok(rows)
                }
            })
            .await?;

        if raw_rows.is_empty() {
            return Ok(0);
        }

        // バケットごとにグループ化
        let mut buckets: HashMap<i64, Vec<usize>> = HashMap::new();
        for (i, row) in raw_rows.iter().enumerate() {
            let bucket = row.timestamp.timestamp() / bucket_secs;
            buckets.entry(bucket).or_default().push(i);
        }

        // 各バケットの集計: 代表行 UPDATE + 削除 id 収集
        struct BucketUpdate {
            id: i64,
            ltp_high: String,
            ltp_low: String,
            ltp_close: String,        // バケット内の last ltp
            best_bid: String,
            best_ask: String,
            best_bid_size: String,
            best_ask_size: String,
            volume: String,
            volume_by_product: String,
        }

        let mut updates: Vec<BucketUpdate> = Vec::with_capacity(buckets.len());
        let mut ids_to_delete: Vec<i64> = Vec::new();

        for indices in buckets.values() {
            // id 昇順なので indices[0] が MIN(id) = 代表行
            let rep_idx = *indices.iter().min().unwrap();
            let rep = &raw_rows[rep_idx];

            let high = indices
                .iter()
                .map(|&i| raw_rows[i].ltp_high)
                .max()
                .unwrap();
            let low = indices
                .iter()
                .map(|&i| raw_rows[i].ltp_low)
                .min()
                .unwrap();
            // 最後の行（最大 id）の終値・スプレッド・出来高を代表値として使う
            let last_idx = *indices.iter().max().unwrap();
            let last = &raw_rows[last_idx];

            updates.push(BucketUpdate {
                id: rep.id,
                ltp_high: high.to_string(),
                ltp_low: low.to_string(),
                ltp_close: last.ltp.to_string(),
                best_bid: last.best_bid.to_string(),
                best_ask: last.best_ask.to_string(),
                best_bid_size: last.best_bid_size.to_string(),
                best_ask_size: last.best_ask_size.to_string(),
                volume: last.volume.to_string(),
                volume_by_product: last.volume_by_product.to_string(),
            });

            for &i in indices {
                if i != rep_idx {
                    ids_to_delete.push(raw_rows[i].id);
                }
            }
        }

        if updates.is_empty() {
            return Ok(0);
        }

        let deleted_count = ids_to_delete.len() as u64;

        self.conn
            .call(move |c| {
                let tx = c.transaction()?;

                // 代表行を OHLCV で更新
                {
                    let mut stmt = tx.prepare(
                        "UPDATE tickers
                         SET ltp_high = ?1, ltp_low = ?2, ltp = ?3,
                             best_bid = ?4, best_ask = ?5,
                             best_bid_size = ?6, best_ask_size = ?7,
                             volume = ?8, volume_by_product = ?9
                         WHERE id = ?10",
                    )?;
                    for u in &updates {
                        stmt.execute(rusqlite::params![
                            u.ltp_high, u.ltp_low, u.ltp_close,
                            u.best_bid, u.best_ask,
                            u.best_bid_size, u.best_ask_size,
                            u.volume, u.volume_by_product,
                            u.id
                        ])?;
                    }
                }

                // 不要行を削除（チャンク分割で SQLite パラメータ上限を回避）
                for chunk in ids_to_delete.chunks(500) {
                    let placeholders = chunk
                        .iter()
                        .enumerate()
                        .map(|(i, _)| format!("?{}", i + 1))
                        .collect::<Vec<_>>()
                        .join(",");
                    let sql = format!("DELETE FROM tickers WHERE id IN ({})", placeholders);
                    let mut stmt = tx.prepare(&sql)?;
                    let params: Vec<&dyn rusqlite::types::ToSql> = chunk
                        .iter()
                        .map(|id| id as &dyn rusqlite::types::ToSql)
                        .collect();
                    stmt.execute(params.as_slice())?;
                }

                tx.commit()?;
                Ok(())
            })
            .await?;

        Ok(deleted_count)
    }
}

fn row_to_candle(row: &rusqlite::Row<'_>) -> rusqlite::Result<Candle> {
    use std::str::FromStr;
    let product_code: String = row.get(0)?;
    let open_time_str: String = row.get(1)?;
    let resolution_secs: u32 = row.get(2)?;
    let open_str: String = row.get(3)?;
    let high_str: String = row.get(4)?;
    let low_str: String = row.get(5)?;
    let close_str: String = row.get(6)?;
    let volume_str: String = row.get(7)?;

    let open_time = DateTime::parse_from_rfc3339(&open_time_str)
        .map(|dt| dt.with_timezone(&Utc))
        .unwrap_or_default();

    Ok(Candle {
        product_code,
        open_time,
        resolution_secs,
        open: Decimal::from_str(&open_str).unwrap_or_default(),
        high: Decimal::from_str(&high_str).unwrap_or_default(),
        low: Decimal::from_str(&low_str).unwrap_or_default(),
        close: Decimal::from_str(&close_str).unwrap_or_default(),
        volume: Decimal::from_str(&volume_str).unwrap_or_default(),
    })
}

fn row_to_stored_ticker(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredTicker> {
    let product_code: String = row.get(0)?;
    let timestamp_str: String = row.get(1)?;
    let timestamp = DateTime::parse_from_rfc3339(&timestamp_str)
        .map(|dt| dt.with_timezone(&Utc))
        .unwrap_or_default();

    Ok(StoredTicker {
        product_code,
        timestamp,
        best_bid: parse_decimal(row.get::<_, String>(2)?),
        best_ask: parse_decimal(row.get::<_, String>(3)?),
        best_bid_size: parse_decimal(row.get::<_, String>(4)?),
        best_ask_size: parse_decimal(row.get::<_, String>(5)?),
        ltp_open: parse_decimal(row.get::<_, String>(6)?),
        ltp: parse_decimal(row.get::<_, String>(7)?),
        ltp_high: parse_decimal(row.get::<_, String>(8)?),
        ltp_low: parse_decimal(row.get::<_, String>(9)?),
        volume: parse_decimal(row.get::<_, String>(10)?),
        volume_by_product: parse_decimal(row.get::<_, String>(11)?),
    })
}

fn row_to_raw(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawRow> {
    let timestamp_str: String = row.get(1)?;
    let timestamp = DateTime::parse_from_rfc3339(&timestamp_str)
        .map(|dt| dt.with_timezone(&Utc))
        .unwrap_or_default();

    Ok(RawRow {
        id: row.get(0)?,
        timestamp,
        best_bid: parse_decimal(row.get::<_, String>(2)?),
        best_ask: parse_decimal(row.get::<_, String>(3)?),
        best_bid_size: parse_decimal(row.get::<_, String>(4)?),
        best_ask_size: parse_decimal(row.get::<_, String>(5)?),
        ltp_open: parse_decimal(row.get::<_, String>(6)?),
        ltp: parse_decimal(row.get::<_, String>(7)?),
        ltp_high: parse_decimal(row.get::<_, String>(8)?),
        ltp_low: parse_decimal(row.get::<_, String>(9)?),
        volume: parse_decimal(row.get::<_, String>(10)?),
        volume_by_product: parse_decimal(row.get::<_, String>(11)?),
    })
}

fn parse_decimal(s: String) -> Decimal {
    Decimal::from_str(&s).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::db::Database;
    use chrono::TimeZone;
    use rust_decimal_macros::dec;

    fn make_ticker(timestamp: DateTime<Utc>, ltp: Decimal) -> Ticker {
        Ticker {
            product_code: "BTC_JPY".to_string(),
            timestamp,
            best_bid: ltp - dec!(100),
            best_ask: ltp + dec!(100),
            best_bid_size: dec!(0.1),
            best_ask_size: dec!(0.1),
            ltp,
            volume: dec!(1000),
            volume_by_product: dec!(1000),
        }
    }

    #[tokio::test]
    async fn insert_and_latest() {
        let db = Database::open_in_memory().await.unwrap();
        let repo = db.tickers();

        let t1 = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
        let t2 = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 1).unwrap();
        repo.insert(&make_ticker(t1, dec!(9000000))).await.unwrap();
        repo.insert(&make_ticker(t2, dec!(9001000))).await.unwrap();

        let rows = repo.latest("BTC_JPY", 10).await.unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].ltp, dec!(9000000));
        assert_eq!(rows[0].ltp_open, dec!(9000000));
        assert_eq!(rows[0].ltp_high, dec!(9000000));
        assert_eq!(rows[0].ltp_low, dec!(9000000));
        assert_eq!(rows[1].ltp, dec!(9001000));
    }

    #[tokio::test]
    async fn duplicate_timestamp_ignored() {
        let db = Database::open_in_memory().await.unwrap();
        let repo = db.tickers();

        let t = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
        repo.insert(&make_ticker(t, dec!(9000000))).await.unwrap();
        repo.insert(&make_ticker(t, dec!(9999999))).await.unwrap(); // 同一timestamp → 無視

        let rows = repo.latest("BTC_JPY", 10).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].ltp, dec!(9000000)); // 最初のものが残る
    }

    #[tokio::test]
    async fn range_returns_correct_slice() {
        let db = Database::open_in_memory().await.unwrap();
        let repo = db.tickers();

        let base = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
        for i in 0..5i64 {
            let t = base + chrono::Duration::seconds(i);
            repo.insert(&make_ticker(t, dec!(9000000) + Decimal::from(i * 1000)))
                .await
                .unwrap();
        }

        let from = base + chrono::Duration::seconds(1);
        let to = base + chrono::Duration::seconds(3);
        let rows = repo.range("BTC_JPY", from, to).await.unwrap();
        assert_eq!(rows.len(), 3); // t1, t2, t3
    }

    #[tokio::test]
    async fn downsample_compacts_zone_and_preserves_ohlc() {
        let db = Database::open_in_memory().await.unwrap();
        let repo = db.tickers();

        // 5秒バケット内に4ティックを作成
        let base = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
        let prices = [dec!(9000000), dec!(9005000), dec!(8995000), dec!(9002000)];
        for (i, &price) in prices.iter().enumerate() {
            let t = base + chrono::Duration::seconds(i as i64);
            repo.insert(&make_ticker(t, price)).await.unwrap();
        }

        // バケット境界を base - 1秒 〜 base + 10秒 に設定してコンパクト実行
        let from = base - chrono::Duration::seconds(1);
        let to = base + chrono::Duration::seconds(10);
        let deleted = repo.compact_zone("BTC_JPY", from, to, 5).await.unwrap();

        assert_eq!(deleted, 3); // 4行 → 1行 (3行削除)

        let rows = repo.latest("BTC_JPY", 10).await.unwrap();
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row.ltp_open, dec!(9000000)); // 最初
        assert_eq!(row.ltp_high, dec!(9005000)); // 最大
        assert_eq!(row.ltp_low, dec!(8995000));  // 最小
        assert_eq!(row.ltp, dec!(9002000));       // 最後
    }

    #[tokio::test]
    async fn downsample_does_not_touch_recent_zone() {
        let db = Database::open_in_memory().await.unwrap();
        let repo = db.tickers();

        // 12時間前のティック（直近1日ゾーン = 削除されない）
        let t = Utc::now() - chrono::Duration::hours(12);
        repo.insert(&make_ticker(t, dec!(9000000))).await.unwrap();

        let stats = repo.downsample("BTC_JPY").await.unwrap();
        assert_eq!(stats.tickers_deleted, 0);

        let rows = repo.latest("BTC_JPY", 10).await.unwrap();
        assert_eq!(rows.len(), 1); // 削除されていない
    }
}
