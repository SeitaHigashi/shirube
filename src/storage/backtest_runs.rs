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
        let fee_pct = config.fee_pct.unwrap_or(0.0015);
        let initial_jpy = config.initial_jpy.to_string();
        let total_return_pct = report.total_return_pct;
        let sharpe_ratio = report.sharpe_ratio;
        let max_drawdown_pct = report.max_drawdown_pct;
        let win_rate = report.win_rate;
        let total_trades = report.total_trades;
        let total_fees_jpy = report.total_fees_jpy;
        let traded_volume_jpy = report.traded_volume_jpy;
        let effective_fee_pct = report.effective_fee_pct;
        let final_fee_tier_pct = report.final_fee_tier_pct;
        let fee_drag_pct = report.fee_drag_pct;
        let avg_btc_exposure = report.avg_btc_exposure;
        let hold_return_pct = report.hold_return_pct;
        let hold_sharpe_ratio = report.hold_sharpe_ratio;
        let hold_max_drawdown_pct = report.hold_max_drawdown_pct;
        let static_mix_return_pct = report.static_mix_return_pct;
        let static_mix_sharpe_ratio = report.static_mix_sharpe_ratio;
        let static_mix_max_drawdown_pct = report.static_mix_max_drawdown_pct;
        let excess_return_vs_static_mix_pct = report.excess_return_vs_static_mix_pct;
        let sharpe_minus_static_mix = report.sharpe_minus_static_mix;

        let id = self
            .conn
            .call(move |c| {
                c.execute(
                    "INSERT INTO backtest_runs (
                        product_code, from_time, to_time, resolution_secs,
                        slippage_pct, fee_pct, initial_jpy,
                        total_return_pct, sharpe_ratio, max_drawdown_pct,
                        win_rate, total_trades,
                        total_fees_jpy, traded_volume_jpy, effective_fee_pct,
                        final_fee_tier_pct, fee_drag_pct,
                        avg_btc_exposure, hold_return_pct, hold_sharpe_ratio,
                        hold_max_drawdown_pct, static_mix_return_pct,
                        static_mix_sharpe_ratio, static_mix_max_drawdown_pct,
                        excess_return_vs_static_mix_pct, sharpe_minus_static_mix
                    ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,
                              ?18,?19,?20,?21,?22,?23,?24,?25,?26)",
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
                        total_fees_jpy,
                        traded_volume_jpy,
                        effective_fee_pct,
                        final_fee_tier_pct,
                        fee_drag_pct,
                        avg_btc_exposure,
                        hold_return_pct,
                        hold_sharpe_ratio,
                        hold_max_drawdown_pct,
                        static_mix_return_pct,
                        static_mix_sharpe_ratio,
                        static_mix_max_drawdown_pct,
                        excess_return_vs_static_mix_pct,
                        sharpe_minus_static_mix,
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
                            win_rate, total_trades,
                            total_fees_jpy, traded_volume_jpy, effective_fee_pct,
                            final_fee_tier_pct, fee_drag_pct,
                            avg_btc_exposure, hold_return_pct, hold_sharpe_ratio,
                            hold_max_drawdown_pct, static_mix_return_pct,
                            static_mix_sharpe_ratio, static_mix_max_drawdown_pct,
                            excess_return_vs_static_mix_pct, sharpe_minus_static_mix
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
                        row.get::<_, f64>(13)?,
                        row.get::<_, f64>(14)?,
                        row.get::<_, f64>(15)?,
                        row.get::<_, f64>(16)?,
                        row.get::<_, f64>(17)?,
                        row.get::<_, f64>(18)?,
                        row.get::<_, f64>(19)?,
                        row.get::<_, f64>(20)?,
                        row.get::<_, f64>(21)?,
                        row.get::<_, f64>(22)?,
                        row.get::<_, f64>(23)?,
                        row.get::<_, f64>(24)?,
                        row.get::<_, f64>(25)?,
                        row.get::<_, f64>(26)?,
                    ))
                })?;
                Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
            })
            .await?;

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
            total_fees_jpy,
            traded_volume_jpy,
            effective_fee_pct,
            final_fee_tier_pct,
            fee_drag_pct,
            avg_btc_exposure,
            hold_return_pct,
            hold_sharpe_ratio,
            hold_max_drawdown_pct,
            static_mix_return_pct,
            static_mix_sharpe_ratio,
            static_mix_max_drawdown_pct,
            excess_return_vs_static_mix_pct,
            sharpe_minus_static_mix,
        ) in records
        {
            let from = DateTime::parse_from_rfc3339(&from_time)
                .map(|dt| dt.with_timezone(&Utc))
                .unwrap_or(DateTime::<Utc>::UNIX_EPOCH);
            let to = DateTime::parse_from_rfc3339(&to_time)
                .map(|dt| dt.with_timezone(&Utc))
                .unwrap_or(DateTime::<Utc>::UNIX_EPOCH);
            result.push(BacktestRunRecord {
                id,
                config: BacktestConfig {
                    product_code,
                    from,
                    to,
                    resolution_secs,
                    slippage_pct,
                    fee_pct: Some(fee_pct),
                    initial_jpy: Decimal::from_str(&initial_jpy).unwrap_or_default(),
                    // NOTE: warmup is an input to the run, not a property of
                    // the measured window, so it is not persisted in
                    // backtest_runs; reconstructed rows report 0.
                    warmup_candles: 0,
                },
                report: BacktestReport {
                    total_return_pct,
                    sharpe_ratio,
                    max_drawdown_pct,
                    win_rate,
                    total_trades,
                    // NOTE: the backtest_runs table predates the risk-event
                    // counters and does not store them, so a report read back
                    // from the DB reports zero rather than the run's real
                    // counts. The live report returned by Simulator::run
                    // carries the true values.
                    circuit_breaker_trips: 0,
                    orders_rejected: 0,
                    orders_below_min: 0,
                    total_fees_jpy,
                    traded_volume_jpy,
                    effective_fee_pct,
                    final_fee_tier_pct,
                    fee_drag_pct,
                    avg_btc_exposure,
                    hold_return_pct,
                    hold_sharpe_ratio,
                    hold_max_drawdown_pct,
                    static_mix_return_pct,
                    static_mix_sharpe_ratio,
                    static_mix_max_drawdown_pct,
                    excess_return_vs_static_mix_pct,
                    sharpe_minus_static_mix,
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
            fee_pct: Some(0.0015),
            initial_jpy: dec!(1_000_000),
            warmup_candles: 0,
        }
    }

    fn make_report(ret: f64) -> BacktestReport {
        BacktestReport {
            total_return_pct: ret,
            sharpe_ratio: 1.2,
            max_drawdown_pct: 3.5,
            win_rate: 0.55,
            total_trades: 42,
            circuit_breaker_trips: 0,
            orders_rejected: 0,
            orders_below_min: 0,
            total_fees_jpy: 123.45,
            traded_volume_jpy: 98_765.0,
            effective_fee_pct: 0.00125,
            final_fee_tier_pct: 0.0011,
            fee_drag_pct: 0.05,
            avg_btc_exposure: 0.55,
            hold_return_pct: 19.6,
            hold_sharpe_ratio: 6.7,
            hold_max_drawdown_pct: 7.7,
            static_mix_return_pct: 10.8,
            static_mix_sharpe_ratio: 6.5,
            static_mix_max_drawdown_pct: 4.6,
            excess_return_vs_static_mix_pct: 0.07,
            sharpe_minus_static_mix: 0.86,
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
        // Fee metrics must round-trip through the DB unchanged.
        assert_eq!(list[0].report.total_fees_jpy, 123.45);
        assert_eq!(list[0].report.traded_volume_jpy, 98_765.0);
        assert_eq!(list[0].report.effective_fee_pct, 0.00125);
        assert_eq!(list[0].report.final_fee_tier_pct, 0.0011);
        assert_eq!(list[0].report.fee_drag_pct, 0.05);
        // Benchmark metrics must round-trip through the DB unchanged too.
        assert_eq!(list[0].report.avg_btc_exposure, 0.55);
        assert_eq!(list[0].report.hold_return_pct, 19.6);
        assert_eq!(list[0].report.hold_sharpe_ratio, 6.7);
        assert_eq!(list[0].report.hold_max_drawdown_pct, 7.7);
        assert_eq!(list[0].report.static_mix_return_pct, 10.8);
        assert_eq!(list[0].report.static_mix_sharpe_ratio, 6.5);
        assert_eq!(list[0].report.static_mix_max_drawdown_pct, 4.6);
        assert_eq!(list[0].report.excess_return_vs_static_mix_pct, 0.07);
        assert_eq!(list[0].report.sharpe_minus_static_mix, 0.86);
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
