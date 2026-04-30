use rusqlite::OptionalExtension;
use tokio_rusqlite::Connection;

use crate::config::TradingConfig;
use crate::error::Result;

pub struct ConfigRepository {
    conn: Connection,
}

impl ConfigRepository {
    pub fn new(conn: Connection) -> Self {
        Self { conn }
    }

    pub async fn load(&self) -> Result<Option<TradingConfig>> {
        let result = self
            .conn
            .call(|c| {
                let val: Option<String> = c
                    .query_row(
                        "SELECT value FROM app_config WHERE key = 'trading_config'",
                        [],
                        |row| row.get(0),
                    )
                    .optional()?;
                Ok(val)
            })
            .await?;

        match result {
            Some(json) => {
                let cfg = serde_json::from_str(&json)
                    .map_err(|e| crate::error::Error::Other(anyhow::anyhow!(e)))?;
                Ok(Some(cfg))
            }
            None => Ok(None),
        }
    }

    pub async fn save(&self, cfg: &TradingConfig) -> Result<()> {
        let json = serde_json::to_string(cfg)
            .map_err(|e| crate::error::Error::Other(anyhow::anyhow!(e)))?;
        self.conn
            .call(move |c| {
                c.execute(
                    "INSERT INTO app_config (key, value)
                     VALUES ('trading_config', ?1)
                     ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                    rusqlite::params![json],
                )?;
                Ok(())
            })
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::db::Database;

    #[tokio::test]
    async fn load_returns_none_when_empty() {
        let db = Database::open_in_memory().await.unwrap();
        let repo = db.config();
        let result = repo.load().await.unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn save_and_load_roundtrip() {
        let db = Database::open_in_memory().await.unwrap();
        let repo = db.config();
        let mut cfg = TradingConfig::default();
        cfg.signal_threshold = 0.5;
        cfg.sma_period = 30;

        repo.save(&cfg).await.unwrap();
        let loaded = repo.load().await.unwrap().unwrap();
        assert_eq!(loaded.signal_threshold, 0.5);
        assert_eq!(loaded.sma_period, 30);
    }

    #[tokio::test]
    async fn save_overwrites_previous() {
        let db = Database::open_in_memory().await.unwrap();
        let repo = db.config();
        let mut cfg = TradingConfig::default();
        cfg.signal_threshold = 0.4;
        repo.save(&cfg).await.unwrap();

        cfg.signal_threshold = 0.6;
        repo.save(&cfg).await.unwrap();

        let loaded = repo.load().await.unwrap().unwrap();
        assert_eq!(loaded.signal_threshold, 0.6);
    }
}
