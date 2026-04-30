use tokio_rusqlite::Connection;

use crate::backtest::{BacktestConfig, BacktestReport};
use crate::error::Result;

pub struct BacktestRunRecord {
    pub id: i64,
    pub config: BacktestConfig,
    pub report: BacktestReport,
}

pub struct BacktestRunRepository {
    conn: Connection,
}

impl BacktestRunRepository {
    pub fn new(conn: Connection) -> Self {
        Self { conn }
    }

    pub async fn insert(&self, config: &BacktestConfig, report: &BacktestReport) -> Result<i64> {
        let product_code = config.product_code.clone();
        let from_time = config.from.to_rfc3339();
        let to_time = config.to.to_rfc3339();
        let resolution_secs = config.resolution_secs;
        let slippage_pct = config.slippage_pct;
        let fee_pct = config.fee_pct;
        let initial_jpy = config.initial_jpy.to_string();
        let total_return_pct = report.total_return_pct;
        let sharpe_ratio = report.sharpe_ratio;
        let max_drawdown_pct = report.max_drawdown_pct;
        let win_rate = report.win_rate;
        let total_trades = report.total_trades;

        let id = self
            .conn
            .call(move |c| {
                c.execute(
                    "INSERT INTO backtest_runs (
                        product_code, from_time, to_time, resolution_secs,
                        slippage_pct, fee_pct, initial_jpy,
                        total_return_pct, sharpe_ratio, max_drawdown_pct,
                        win_rate, total_trades
                    ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
                    rusqlite::params![
                        product_code,
                        from_time,
                        to_time,
                        resolution_secs,
                        slippage_pct,
                        fee_pct,
                        initial_jpy,
                        total_return_pct,
                        sharpe_ratio,
                        max_drawdown_pct,
                        win_rate,
                        total_trades,
                    ],
                )?;
                Ok(c.last_insert_rowid())
            })
            .await?;
        Ok(id)
    }

    pub async fn list(&self, limit: u32) -> Result<Vec<BacktestRunRecord>> {
        use chrono::{DateTime, Utc};
        use rust_decimal::Decimal;
        use std::str::FromStr;

        let records = self
            .conn
            .call(move |c| {
                let mut stmt = c.prepare(
                    "SELECT id, product_code, from_time, to_time, resolution_secs,
                            slippage_pct, fee_pct, initial_jpy,
                            total_return_pct, sharpe_ratio, max_drawdown_pct,
                            win_rate, total_trades
                     FROM backtest_runs
                     ORDER BY created_at DESC, id DESC
                     LIMIT ?1",
                )?;
                let rows = stmt.query_map(rusqlite::params![limit], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, u32>(4)?,
                        row.get::<_, f64>(5)?,
                        row.get::<_, f64>(6)?,
                        row.get::<_, String>(7)?,
                        row.get::<_, f64>(8)?,
                        row.get::<_, f64>(9)?,
                        row.get::<_, f64>(10)?,
                        row.get::<_, f64>(11)?,
                        row.get::<_, u32>(12)?,
                    ))
                })?;
                Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
            })
            .await?;

        use chrono::TimeZone;
        let mut result = Vec::with_capacity(records.len());
        for (
            id,
            product_code,
            from_time,
            to_time,
            resolution_secs,
            slippage_pct,
            fee_pct,
            initial_jpy,
            total_return_pct,
            sharpe_ratio,
            max_drawdown_pct,
            win_rate,
            total_trades,
        ) in records
        {
            let from = DateTime::parse_from_rfc3339(&from_time)
                .map(|dt| dt.with_timezone(&Utc))
                .unwrap_or_else(|_| Utc.timestamp_opt(0, 0).unwrap());
            let to = DateTime::parse_from_rfc3339(&to_time)
                .map(|dt| dt.with_timezone(&Utc))
                .unwrap_or_else(|_| Utc.timestamp_opt(0, 0).unwrap());
            result.push(BacktestRunRecord {
                id,
                config: BacktestConfig {
                    product_code,
                    from,
                    to,
                    resolution_secs,
                    slippage_pct,
                    fee_pct,
                    initial_jpy: Decimal::from_str(&initial_jpy).unwrap_or_default(),
                },
                report: BacktestReport {
                    total_return_pct,
                    sharpe_ratio,
                    max_drawdown_pct,
                    win_rate,
                    total_trades,
                },
            });
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::db::Database;
    use chrono::Utc;
    use rust_decimal_macros::dec;

    fn make_config() -> BacktestConfig {
        let now = Utc::now();
        BacktestConfig {
            product_code: "BTC_JPY".into(),
            from: now,
            to: now,
            resolution_secs: 60,
            slippage_pct: 0.001,
            fee_pct: 0.0015,
            initial_jpy: dec!(1_000_000),
        }
    }

    fn make_report(ret: f64) -> BacktestReport {
        BacktestReport {
            total_return_pct: ret,
            sharpe_ratio: 1.2,
            max_drawdown_pct: 3.5,
            win_rate: 0.55,
            total_trades: 42,
        }
    }

    #[tokio::test]
    async fn insert_and_list() {
        let db = Database::open_in_memory().await.unwrap();
        let repo = db.backtest_runs();

        let id = repo.insert(&make_config(), &make_report(5.0)).await.unwrap();
        assert!(id > 0);

        let list = repo.list(10).await.unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].report.total_return_pct, 5.0);
    }

    #[tokio::test]
    async fn list_returns_most_recent_first() {
        let db = Database::open_in_memory().await.unwrap();
        let repo = db.backtest_runs();

        repo.insert(&make_config(), &make_report(1.0)).await.unwrap();
        repo.insert(&make_config(), &make_report(2.0)).await.unwrap();

        let list = repo.list(10).await.unwrap();
        assert_eq!(list.len(), 2);
        // 最新が先頭（DESC ORDER）— insert 順なので 2.0 が先頭
        assert_eq!(list[0].report.total_return_pct, 2.0);
    }
}
