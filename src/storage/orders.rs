use chrono::DateTime;
use chrono::Utc;
use rust_decimal::Decimal;
use std::str::FromStr;
use tokio_rusqlite::Connection;

use crate::error::Result;
use crate::types::order::{Order, OrderSide, OrderStatus, OrderType};

pub struct OrderRepository {
    conn: Connection,
}

impl OrderRepository {
    pub fn new(conn: Connection) -> Self {
        Self { conn }
    }

    pub async fn upsert(&self, order: &Order) -> Result<()> {
        let acceptance_id = order.acceptance_id.clone();
        let product_code = order.product_code.clone();
        let side = match order.side {
            OrderSide::Buy => "BUY",
            OrderSide::Sell => "SELL",
        }
        .to_string();
        let order_type = match order.order_type {
            OrderType::Limit => "LIMIT",
            OrderType::Market => "MARKET",
        }
        .to_string();
        let price = order.price.map(|p| p.to_string());
        let size = order.size.to_string();
        let status = order_status_str(&order.status).to_string();
        let created_at = order.created_at.to_rfc3339();
        let updated_at = order.updated_at.to_rfc3339();

        self.conn
            .call(move |c| {
                c.execute(
                    "INSERT INTO orders (acceptance_id, product_code, side, order_type, price, size, status, created_at, updated_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                     ON CONFLICT(acceptance_id) DO UPDATE SET
                         status = excluded.status,
                         updated_at = excluded.updated_at",
                    rusqlite::params![
                        acceptance_id, product_code, side, order_type,
                        price, size, status, created_at, updated_at
                    ],
                )?;
                Ok(())
            })
            .await?;
        Ok(())
    }

    pub async fn get_by_acceptance_id(&self, acceptance_id: &str) -> Result<Option<Order>> {
        let acceptance_id = acceptance_id.to_string();
        let order = self
            .conn
            .call(move |c| {
                let mut stmt = c.prepare(
                    "SELECT id, acceptance_id, product_code, side, order_type, price, size, status, created_at, updated_at
                     FROM orders WHERE acceptance_id = ?1",
                )?;
                let mut rows = stmt.query_map(rusqlite::params![acceptance_id], row_to_order)?;
                Ok(rows.next().transpose()?)
            })
            .await?;
        Ok(order)
    }

    pub async fn get_by_status(
        &self,
        product_code: &str,
        status: &str,
        count: u32,
    ) -> Result<Vec<Order>> {
        let product_code = product_code.to_string();
        let status = status.to_string();
        let orders = self
            .conn
            .call(move |c| {
                let mut stmt = c.prepare(
                    "SELECT id, acceptance_id, product_code, side, order_type, price, size, status, created_at, updated_at
                     FROM orders
                     WHERE product_code = ?1 AND status = ?2
                     ORDER BY created_at DESC
                     LIMIT ?3",
                )?;
                let rows =
                    stmt.query_map(rusqlite::params![product_code, status, count], row_to_order)?;
                Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
            })
            .await?;
        Ok(orders)
    }
}

fn order_status_str(status: &OrderStatus) -> &'static str {
    match status {
        OrderStatus::Active => "ACTIVE",
        OrderStatus::Completed => "COMPLETED",
        OrderStatus::Canceled => "CANCELED",
        OrderStatus::Expired => "EXPIRED",
        OrderStatus::Rejected => "REJECTED",
    }
}

fn row_to_order(row: &rusqlite::Row<'_>) -> rusqlite::Result<Order> {
    let id: i64 = row.get(0)?;
    let acceptance_id: String = row.get(1)?;
    let product_code: String = row.get(2)?;
    let side_str: String = row.get(3)?;
    let type_str: String = row.get(4)?;
    let price_str: Option<String> = row.get(5)?;
    let size_str: String = row.get(6)?;
    let status_str: String = row.get(7)?;
    let created_at_str: String = row.get(8)?;
    let updated_at_str: String = row.get(9)?;

    let side = match side_str.as_str() {
        "BUY" => OrderSide::Buy,
        _ => OrderSide::Sell,
    };
    let order_type = match type_str.as_str() {
        "LIMIT" => OrderType::Limit,
        _ => OrderType::Market,
    };
    let status = match status_str.as_str() {
        "ACTIVE" => OrderStatus::Active,
        "COMPLETED" => OrderStatus::Completed,
        "CANCELED" => OrderStatus::Canceled,
        "EXPIRED" => OrderStatus::Expired,
        _ => OrderStatus::Rejected,
    };

    Ok(Order {
        id: Some(id),
        acceptance_id,
        product_code,
        side,
        order_type,
        price: price_str.and_then(|s| Decimal::from_str(&s).ok()),
        size: Decimal::from_str(&size_str).unwrap_or_default(),
        status,
        created_at: DateTime::parse_from_rfc3339(&created_at_str)
            .map(|dt| dt.with_timezone(&Utc))
            .unwrap_or_default(),
        updated_at: DateTime::parse_from_rfc3339(&updated_at_str)
            .map(|dt| dt.with_timezone(&Utc))
            .unwrap_or_default(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::db::Database;
    use rust_decimal_macros::dec;

    fn make_order(acceptance_id: &str) -> Order {
        let now = Utc::now();
        Order {
            id: None,
            acceptance_id: acceptance_id.to_string(),
            product_code: "BTC_JPY".to_string(),
            side: OrderSide::Buy,
            order_type: OrderType::Limit,
            price: Some(dec!(9000000)),
            size: dec!(0.001),
            status: OrderStatus::Active,
            created_at: now,
            updated_at: now,
        }
    }

    #[tokio::test]
    async fn upsert_and_get_by_acceptance_id() {
        let db = Database::open_in_memory().await.unwrap();
        let repo = db.orders();

        let order = make_order("JRF-001");
        repo.upsert(&order).await.unwrap();

        let found = repo.get_by_acceptance_id("JRF-001").await.unwrap();
        assert!(found.is_some());
        let found = found.unwrap();
        assert_eq!(found.acceptance_id, "JRF-001");
        assert_eq!(found.status, OrderStatus::Active);
    }

    #[tokio::test]
    async fn upsert_updates_status() {
        let db = Database::open_in_memory().await.unwrap();
        let repo = db.orders();

        let order = make_order("JRF-002");
        repo.upsert(&order).await.unwrap();

        let completed = Order {
            status: OrderStatus::Completed,
            updated_at: Utc::now(),
            ..order
        };
        repo.upsert(&completed).await.unwrap();

        let found = repo.get_by_acceptance_id("JRF-002").await.unwrap().unwrap();
        assert_eq!(found.status, OrderStatus::Completed);
    }

    #[tokio::test]
    async fn get_by_status_filters() {
        let db = Database::open_in_memory().await.unwrap();
        let repo = db.orders();

        repo.upsert(&make_order("JRF-001")).await.unwrap();
        repo.upsert(&make_order("JRF-002")).await.unwrap();

        let active = repo.get_by_status("BTC_JPY", "ACTIVE", 10).await.unwrap();
        assert_eq!(active.len(), 2);
    }
}
