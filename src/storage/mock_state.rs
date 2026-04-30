use rust_decimal::Decimal;
use rusqlite::OptionalExtension;
use std::str::FromStr;
use tokio_rusqlite::Connection;

use crate::error::Result;
use crate::exchange::mock::FilledTrade;
use crate::types::{
    balance::Balance,
    order::OrderSide,
};

pub struct MockStateRepository {
    conn: Connection,
}

impl MockStateRepository {
    pub fn new(conn: Connection) -> Self {
        Self { conn }
    }

    // ── 残高 ──────────────────────────────────────────────────────────────────

    pub async fn save_balances(&self, balances: &[Balance]) -> Result<()> {
        let rows: Vec<(String, String)> = balances
            .iter()
            .map(|b| (b.currency_code.clone(), b.amount.to_string()))
            .collect();
        self.conn
            .call(move |c| {
                let tx = c.transaction()?;
                for (currency, amount) in &rows {
                    tx.execute(
                        "INSERT INTO mock_balances (currency_code, amount)
                         VALUES (?1, ?2)
                         ON CONFLICT(currency_code) DO UPDATE SET amount = excluded.amount",
                        rusqlite::params![currency, amount],
                    )?;
                }
                tx.commit()?;
                Ok(())
            })
            .await?;
        Ok(())
    }

    pub async fn load_balances(&self) -> Result<Vec<Balance>> {
        let rows = self
            .conn
            .call(|c| {
                let mut stmt =
                    c.prepare("SELECT currency_code, amount FROM mock_balances")?;
                let rows = stmt.query_map([], |row| {
                    let currency: String = row.get(0)?;
                    let amount_str: String = row.get(1)?;
                    Ok((currency, amount_str))
                })?;
                Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
            })
            .await?;
        Ok(rows
            .into_iter()
            .map(|(currency_code, amount_str)| {
                let amount = Decimal::from_str(&amount_str).unwrap_or(Decimal::ZERO);
                Balance {
                    currency_code,
                    amount,
                    available: amount,
                }
            })
            .collect())
    }

    // ── 注文カウンター ─────────────────────────────────────────────────────────

    pub async fn save_order_counter(&self, value: u64) -> Result<()> {
        self.conn
            .call(move |c| {
                c.execute(
                    "INSERT INTO mock_state (key, value)
                     VALUES ('order_counter', ?1)
                     ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                    rusqlite::params![value.to_string()],
                )?;
                Ok(())
            })
            .await?;
        Ok(())
    }

    pub async fn load_order_counter(&self) -> Result<u64> {
        let val = self
            .conn
            .call(|c| {
                let result: Option<String> = c
                    .query_row(
                        "SELECT value FROM mock_state WHERE key = 'order_counter'",
                        [],
                        |row| row.get(0),
                    )
                    .optional()?;
                Ok(result)
            })
            .await?;
        Ok(val
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(1))
    }

    // ── 約定履歴 ──────────────────────────────────────────────────────────────

    pub async fn insert_filled_trade(&self, trade: &FilledTrade) -> Result<()> {
        let side = match trade.side {
            OrderSide::Buy => "BUY",
            OrderSide::Sell => "SELL",
        }
        .to_string();
        let price = trade.price.to_string();
        let size = trade.size.to_string();
        let fee = trade.fee.to_string();
        self.conn
            .call(move |c| {
                c.execute(
                    "INSERT INTO mock_filled_trades (side, price, size, fee)
                     VALUES (?1, ?2, ?3, ?4)",
                    rusqlite::params![side, price, size, fee],
                )?;
                Ok(())
            })
            .await?;
        Ok(())
    }

    pub async fn load_filled_trades(&self) -> Result<Vec<FilledTrade>> {
        let rows = self
            .conn
            .call(|c| {
                let mut stmt = c.prepare(
                    "SELECT side, price, size, fee FROM mock_filled_trades ORDER BY id",
                )?;
                let rows = stmt.query_map([], |row| {
                    let side_str: String = row.get(0)?;
                    let price_str: String = row.get(1)?;
                    let size_str: String = row.get(2)?;
                    let fee_str: String = row.get(3)?;
                    Ok((side_str, price_str, size_str, fee_str))
                })?;
                Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
            })
            .await?;
        Ok(rows
            .into_iter()
            .map(|(side_str, price_str, size_str, fee_str)| FilledTrade {
                side: if side_str == "BUY" {
                    OrderSide::Buy
                } else {
                    OrderSide::Sell
                },
                price: Decimal::from_str(&price_str).unwrap_or(Decimal::ZERO),
                size: Decimal::from_str(&size_str).unwrap_or(Decimal::ZERO),
                fee: Decimal::from_str(&fee_str).unwrap_or(Decimal::ZERO),
            })
            .collect())
    }
}
