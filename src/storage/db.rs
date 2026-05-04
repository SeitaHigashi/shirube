use tokio_rusqlite::Connection;

use super::{
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
}
