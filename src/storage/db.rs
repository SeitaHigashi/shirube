use tokio_rusqlite::Connection;

use super::{
    backtest_runs::BacktestRunRepository,
    candles::CandleRepository,
    orders::OrderRepository,
    schema::migrate,
    trades::TradeRepository,
};
use crate::error::Result;

#[derive(Clone)]
pub struct Database {
    conn: Connection,
}

impl Database {
    pub async fn open(path: &str) -> Result<Self> {
        let conn = Connection::open(path).await?;
        migrate(&conn).await?;
        Ok(Self { conn })
    }

    pub async fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory().await?;
        migrate(&conn).await?;
        Ok(Self { conn })
    }

    pub fn candles(&self) -> CandleRepository {
        CandleRepository::new(self.conn.clone())
    }

    pub fn orders(&self) -> OrderRepository {
        OrderRepository::new(self.conn.clone())
    }

    pub fn trades(&self) -> TradeRepository {
        TradeRepository::new(self.conn.clone())
    }

    pub fn backtest_runs(&self) -> BacktestRunRepository {
        BacktestRunRepository::new(self.conn.clone())
    }
}
