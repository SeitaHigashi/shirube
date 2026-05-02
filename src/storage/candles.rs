use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use std::str::FromStr;
use tokio_rusqlite::Connection;

use crate::error::Result;
use crate::types::market::Candle;

pub struct CandleRepository {
    conn: Connection,
}

impl CandleRepository {
    pub fn new(conn: Connection) -> Self {
        Self { conn }
    }

    pub async fn upsert(&self, candle: &Candle) -> Result<()> {
        let product_code = candle.product_code.clone();
        let open_time = candle.open_time.to_rfc3339();
        let resolution_secs = candle.resolution_secs;
        let open = candle.open.to_string();
        let high = candle.high.to_string();
        let low = candle.low.to_string();
        let close = candle.close.to_string();
        let volume = candle.volume.to_string();

        self.conn
            .call(move |c| {
                c.execute(
                    "INSERT INTO candles (product_code, open_time, resolution_secs, open, high, low, close, volume)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                     ON CONFLICT(product_code, open_time, resolution_secs) DO UPDATE SET
                         high = MAX(excluded.high, high),
                         low = MIN(excluded.low, low),
                         close = excluded.close,
                         volume = excluded.volume",
                    rusqlite::params![
                        product_code, open_time, resolution_secs,
                        open, high, low, close, volume
                    ],
                )?;
                Ok(())
            })
            .await?;
        Ok(())
    }

    pub async fn upsert_batch(&self, candles: &[Candle]) -> Result<()> {
        let rows: Vec<(String, String, u32, String, String, String, String, String)> = candles
            .iter()
            .map(|c| {
                (
                    c.product_code.clone(),
                    c.open_time.to_rfc3339(),
                    c.resolution_secs,
                    c.open.to_string(),
                    c.high.to_string(),
                    c.low.to_string(),
                    c.close.to_string(),
                    c.volume.to_string(),
                )
            })
            .collect();

        self.conn
            .call(move |c| {
                let tx = c.transaction()?;
                {
                    let mut stmt = tx.prepare(
                        "INSERT INTO candles (product_code, open_time, resolution_secs, open, high, low, close, volume)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                         ON CONFLICT(product_code, open_time, resolution_secs) DO UPDATE SET
                             high = MAX(excluded.high, high),
                             low = MIN(excluded.low, low),
                             close = excluded.close,
                             volume = excluded.volume",
                    )?;
                    for (product_code, open_time, resolution_secs, open, high, low, close, volume) in &rows {
                        stmt.execute(rusqlite::params![
                            product_code, open_time, resolution_secs,
                            open, high, low, close, volume
                        ])?;
                    }
                }
                tx.commit()?;
                Ok(())
            })
            .await?;
        Ok(())
    }

    pub async fn get_range(
        &self,
        product_code: &str,
        resolution_secs: u32,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
        limit: Option<u32>,
    ) -> Result<Vec<Candle>> {
        let product_code = product_code.to_string();
        let from_str = from.to_rfc3339();
        let to_str = to.to_rfc3339();
        let limit = limit.unwrap_or(1000);

        let candles = self
            .conn
            .call(move |c| {
                let mut stmt = c.prepare(
                    "SELECT product_code, open_time, resolution_secs, open, high, low, close, volume
                     FROM candles
                     WHERE product_code = ?1
                       AND resolution_secs = ?2
                       AND open_time >= ?3
                       AND open_time <= ?4
                     ORDER BY open_time ASC
                     LIMIT ?5",
                )?;
                let rows = stmt.query_map(
                    rusqlite::params![product_code, resolution_secs, from_str, to_str, limit],
                    row_to_candle,
                )?;
                Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
            })
            .await?;
        Ok(candles)
    }

    pub async fn get_latest(
        &self,
        product_code: &str,
        resolution_secs: u32,
        count: u32,
    ) -> Result<Vec<Candle>> {
        let product_code = product_code.to_string();

        let candles = self
            .conn
            .call(move |c| {
                let mut stmt = c.prepare(
                    "SELECT product_code, open_time, resolution_secs, open, high, low, close, volume
                     FROM candles
                     WHERE product_code = ?1 AND resolution_secs = ?2
                     ORDER BY open_time DESC
                     LIMIT ?3",
                )?;
                let rows = stmt.query_map(
                    rusqlite::params![product_code, resolution_secs, count],
                    row_to_candle,
                )?;
                let mut candles: Vec<Candle> = rows.collect::<std::result::Result<Vec<_>, rusqlite::Error>>()?;
                candles.reverse();
                Ok(candles)
            })
            .await?;
        Ok(candles)
    }

    /// 任意の resolution_secs でキャンドルを集約して返す。
    /// 常に DB 内の resolution_secs=60 の1分足を元データとして使用する。
    /// resolution_secs=60 を指定した場合は1分足をそのまま返す。
    pub async fn get_aggregated(
        &self,
        product_code: &str,
        resolution_secs: u32,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
        limit: Option<u32>,
    ) -> Result<Vec<Candle>> {
        let product_code = product_code.to_string();
        let from_str = from.to_rfc3339();
        let to_str = to.to_rfc3339();
        let limit = limit.unwrap_or(1000);
        let res = resolution_secs as i64;

        let candles = self
            .conn
            .call(move |c| {
                let mut stmt = c.prepare(
                    "WITH numbered AS (
                       SELECT product_code, open_time,
                         (strftime('%s', open_time) / ?1) * ?1 AS bucket_ts,
                         CAST(open   AS REAL) AS open,
                         CAST(high   AS REAL) AS high,
                         CAST(low    AS REAL) AS low,
                         CAST(close  AS REAL) AS close,
                         CAST(volume AS REAL) AS volume,
                         ROW_NUMBER() OVER (
                           PARTITION BY product_code, (strftime('%s', open_time) / ?1)
                           ORDER BY open_time ASC
                         ) AS rn_asc,
                         ROW_NUMBER() OVER (
                           PARTITION BY product_code, (strftime('%s', open_time) / ?1)
                           ORDER BY open_time DESC
                         ) AS rn_desc
                       FROM candles
                       WHERE product_code = ?2
                         AND resolution_secs = 60
                         AND open_time >= ?3
                         AND open_time <= ?4
                     ),
                     agg AS (
                       SELECT
                         product_code,
                         bucket_ts,
                         MAX(CASE WHEN rn_asc  = 1 THEN open  END) AS open,
                         MAX(high)  AS high,
                         MIN(low)   AS low,
                         MAX(CASE WHEN rn_desc = 1 THEN close END) AS close,
                         SUM(volume) AS volume
                       FROM numbered
                       GROUP BY product_code, bucket_ts
                     )
                     SELECT
                       product_code,
                       datetime(bucket_ts, 'unixepoch') AS open_time,
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
                    rusqlite::params![res, product_code, from_str, to_str, limit],
                    row_to_candle,
                )?;
                Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
            })
            .await?;
        Ok(candles)
    }
}

fn row_to_candle(row: &rusqlite::Row<'_>) -> rusqlite::Result<Candle> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::db::Database;
    use chrono::TimeZone;
    use rust_decimal_macros::dec;

    fn make_candle(open_time: DateTime<Utc>) -> Candle {
        Candle {
            product_code: "BTC_JPY".to_string(),
            open_time,
            resolution_secs: 60,
            open: dec!(9000000),
            high: dec!(9010000),
            low: dec!(8990000),
            close: dec!(9005000),
            volume: dec!(1.5),
        }
    }

    #[tokio::test]
    async fn upsert_and_get_latest() {
        let db = Database::open_in_memory().await.unwrap();
        let repo = db.candles();

        let t1 = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
        let t2 = Utc.with_ymd_and_hms(2024, 1, 1, 0, 1, 0).unwrap();
        repo.upsert(&make_candle(t1)).await.unwrap();
        repo.upsert(&make_candle(t2)).await.unwrap();

        let candles = repo.get_latest("BTC_JPY", 60, 10).await.unwrap();
        assert_eq!(candles.len(), 2);
        assert_eq!(candles[0].open_time, t1);
        assert_eq!(candles[1].open_time, t2);
    }

    #[tokio::test]
    async fn upsert_updates_high_low() {
        let db = Database::open_in_memory().await.unwrap();
        let repo = db.candles();

        let t = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
        let candle = make_candle(t);
        repo.upsert(&candle).await.unwrap();

        // Upsert with higher high
        let updated = Candle {
            high: dec!(9020000),
            low: dec!(8980000),
            close: dec!(9015000),
            ..candle
        };
        repo.upsert(&updated).await.unwrap();

        let result = repo.get_latest("BTC_JPY", 60, 1).await.unwrap();
        assert_eq!(result[0].high, dec!(9020000));
        assert_eq!(result[0].low, dec!(8980000));
        assert_eq!(result[0].close, dec!(9015000));
    }

    #[tokio::test]
    async fn get_range_filters_correctly() {
        let db = Database::open_in_memory().await.unwrap();
        let repo = db.candles();

        let t1 = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
        let t2 = Utc.with_ymd_and_hms(2024, 1, 1, 0, 1, 0).unwrap();
        let t3 = Utc.with_ymd_and_hms(2024, 1, 1, 0, 2, 0).unwrap();
        repo.upsert_batch(&[make_candle(t1), make_candle(t2), make_candle(t3)])
            .await
            .unwrap();

        let result = repo.get_range("BTC_JPY", 60, t1, t2, None).await.unwrap();
        assert_eq!(result.len(), 2);
    }
}
