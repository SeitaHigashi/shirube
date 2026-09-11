use tokio_rusqlite::Connection;

use super::{
    backtest_runs::BacktestRunRepository,
    candles::CandleRepository,
    config::ConfigRepository,
    mock_state::MockStateRepository,
    news_sentiments::NewsSentimentRepository,
    orders::OrderRepository,
    schema::migrate,
    tickers::TickerRepository,
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

    pub fn news_sentiments(&self) -> NewsSentimentRepository {
        NewsSentimentRepository::new(self.conn.clone())
    }

    pub fn mock_state(&self) -> MockStateRepository {
        MockStateRepository::new(self.conn.clone())
    }

    pub fn config(&self) -> ConfigRepository {
        ConfigRepository::new(self.conn.clone())
    }

    pub fn tickers(&self) -> TickerRepository {
        TickerRepository::new(self.conn.clone())
    }

    pub fn backtest_runs(&self) -> BacktestRunRepository {
        BacktestRunRepository::new(self.conn.clone())
    }

    /// Checkpoint the WAL into the main file and VACUUM, so an on-disk copy
    /// of the DB reflects every committed write and carries no free pages.
    /// Used by `shirube backtest-data push` before gzip-compressing the file
    /// for upload — without this, the uploaded asset can miss recently
    /// written bars (still sitting in `-wal`) and is larger than necessary.
    pub async fn checkpoint_and_vacuum(&self) -> Result<()> {
        self.conn
            .call(|c| {
                c.execute_batch("PRAGMA wal_checkpoint(TRUNCATE); VACUUM;")?;
                Ok(())
            })
            .await?;
        Ok(())
    }
}
