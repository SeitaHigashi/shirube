pub mod report;
pub mod simulator;

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;

#[derive(Debug, Clone)]
pub struct BacktestConfig {
    pub product_code: String,
    pub from: DateTime<Utc>,
    pub to: DateTime<Utc>,
    pub resolution_secs: u32,
    /// スリッページ率（例: 0.001 = 0.1%）
    pub slippage_pct: f64,
    /// 手数料率（例: 0.0015 = 0.15%）
    pub fee_pct: f64,
    pub initial_jpy: Decimal,
}

#[derive(Debug, Clone)]
pub struct BacktestReport {
    pub total_return_pct: f64,
    pub sharpe_ratio: f64,
    pub max_drawdown_pct: f64,
    pub win_rate: f64,
    pub total_trades: u32,
}
