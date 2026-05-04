pub mod report;
pub mod simulator;

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;

use crate::config::ZoneConfig;

#[derive(Debug, Clone)]
pub struct BacktestConfig {
    pub product_code: String,
    pub from: DateTime<Utc>,
    pub to: DateTime<Utc>,
    pub resolution_secs: u32,
    /// スリッページ率（例: 0.001 = 0.1%）
    pub slippage_pct: f64,
    /// 手数料率の固定値（例: 0.0015 = 0.15%）。
    /// None の場合は MockExchangeClient のティア制手数料を自動適用する。
    pub fee_pct: Option<f64>,
    pub initial_jpy: Decimal,
    /// ゾーン制配分の設定。デフォルトは現行互換（[0.0, 1.0]）
    pub zone: ZoneConfig,
}

#[derive(Debug, Clone)]
pub struct BacktestReport {
    pub total_return_pct: f64,
    pub sharpe_ratio: f64,
    pub max_drawdown_pct: f64,
    pub win_rate: f64,
    pub total_trades: u32,
}
