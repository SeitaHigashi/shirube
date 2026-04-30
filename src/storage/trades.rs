use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use std::str::FromStr;
use tokio_rusqlite::Connection;

use crate::error::Result;
use crate::types::market::{Trade, TradeSide};

pub struct TradeRepository {
    conn: Connection,
}

impl TradeRepository {
    pub fn new(conn: Connection) -> Self {
        Self { conn }
    }

    pub async fn insert_batch(&self, trades: &[Trade]) -> Result<()> {
        let rows: Vec<(i64, String, String, String, String, String, String)> = trades
            .iter()
            .map(|t| {
                let side = match t.side {
                    TradeSide::Buy => "BUY",
                    TradeSide::Sell => "SELL",
                };
                (
                    t.id,
                    t.exec_date.to_rfc3339(),
                    t.price.to_string(),
                    t.size.to_string(),
                    side.to_string(),
                    t.buy_child_order_acceptance_id.clone(),
                    t.sell_child_order_acceptance_id.clone(),
                )
            })
            .collect();

        self.conn
            .call(move |c| {
                let tx = c.transaction()?;
                {
                    let mut stmt = tx.prepare(
                        "INSERT OR IGNORE INTO trades (id, exec_date, price, size, side, buy_order_id, sell_order_id)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    )?;
                    for (id, exec_date, price, size, side, buy_id, sell_id) in &rows {
                        stmt.execute(rusqlite::params![id, exec_date, price, size, side, buy_id, sell_id])?;
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
        from: DateTime<Utc>,
        to: DateTime<Utc>,
        limit: Option<u32>,
    ) -> Result<Vec<Trade>> {
        let from_str = from.to_rfc3339();
        let to_str = to.to_rfc3339();
        let limit = limit.unwrap_or(10000);

        let trades = self
            .conn
            .call(move |c| {
                let mut stmt = c.prepare(
                    "SELECT id, exec_date, price, size, side, buy_order_id, sell_order_id
                     FROM trades
                     WHERE exec_date >= ?1 AND exec_date <= ?2
                     ORDER BY exec_date ASC
                     LIMIT ?3",
                )?;
                let rows = stmt.query_map(
                    rusqlite::params![from_str, to_str, limit],
                    row_to_trade,
                )?;
                Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
            })
            .await?;
        Ok(trades)
    }
}

fn row_to_trade(row: &rusqlite::Row<'_>) -> rusqlite::Result<Trade> {
    let id: i64 = row.get(0)?;
    let exec_date_str: String = row.get(1)?;
    let price_str: String = row.get(2)?;
    let size_str: String = row.get(3)?;
    let side_str: String = row.get(4)?;
    let buy_id: String = row.get(5)?;
    let sell_id: String = row.get(6)?;

    let side = match side_str.as_str() {
        "BUY" => TradeSide::Buy,
        _ => TradeSide::Sell,
    };

    Ok(Trade {
        id,
        exec_date: DateTime::parse_from_rfc3339(&exec_date_str)
            .map(|dt| dt.with_timezone(&Utc))
            .unwrap_or_default(),
        price: Decimal::from_str(&price_str).unwrap_or_default(),
        size: Decimal::from_str(&size_str).unwrap_or_default(),
        side,
        buy_child_order_acceptance_id: buy_id,
        sell_child_order_acceptance_id: sell_id,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::db::Database;
    use chrono::TimeZone;
    use rust_decimal_macros::dec;

    fn make_trade(id: i64, exec_date: DateTime<Utc>) -> Trade {
        Trade {
            id,
            exec_date,
            price: dec!(9000000),
            size: dec!(0.001),
            side: TradeSide::Buy,
            buy_child_order_acceptance_id: format!("BUY-{}", id),
            sell_child_order_acceptance_id: format!("SELL-{}", id),
        }
    }

    #[tokio::test]
    async fn insert_and_get_range() {
        let db = Database::open_in_memory().await.unwrap();
        let repo = db.trades();

        let t1 = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
        let t2 = Utc.with_ymd_and_hms(2024, 1, 1, 0, 1, 0).unwrap();
        let t3 = Utc.with_ymd_and_hms(2024, 1, 1, 0, 2, 0).unwrap();

        repo.insert_batch(&[make_trade(1, t1), make_trade(2, t2), make_trade(3, t3)])
            .await
            .unwrap();

        let result = repo.get_range(t1, t2, None).await.unwrap();
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].id, 1);
        assert_eq!(result[1].id, 2);
    }

    #[tokio::test]
    async fn insert_ignore_duplicate() {
        let db = Database::open_in_memory().await.unwrap();
        let repo = db.trades();

        let t = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
        let trade = make_trade(42, t);
        repo.insert_batch(&[trade.clone()]).await.unwrap();
        repo.insert_batch(&[trade]).await.unwrap(); // should not error

        let result = repo
            .get_range(t, Utc.with_ymd_and_hms(2024, 1, 1, 1, 0, 0).unwrap(), None)
            .await
            .unwrap();
        assert_eq!(result.len(), 1);
    }
}
