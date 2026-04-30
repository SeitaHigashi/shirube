use super::BacktestReport;

/// 人間が読みやすい形式でレポートを出力する
pub fn format_report(report: &BacktestReport) -> String {
    format!(
        "=== Backtest Report ===\n\
         Total Return : {:.2}%\n\
         Sharpe Ratio : {:.3}\n\
         Max Drawdown : {:.2}%\n\
         Win Rate     : {:.1}%\n\
         Total Trades : {}",
        report.total_return_pct,
        report.sharpe_ratio,
        report.max_drawdown_pct,
        report.win_rate * 100.0,
        report.total_trades,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_report_contains_all_fields() {
        let report = BacktestReport {
            total_return_pct: 12.34,
            sharpe_ratio: 1.567,
            max_drawdown_pct: 3.21,
            win_rate: 0.55,
            total_trades: 42,
        };
        let s = format_report(&report);
        assert!(s.contains("12.34"));
        assert!(s.contains("1.567"));
        assert!(s.contains("3.21"));
        assert!(s.contains("55.0"));
        assert!(s.contains("42"));
    }
}
