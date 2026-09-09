pub mod data;
pub mod report;
pub mod simulator;

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

/// Parameters controlling a single backtest run: time range, resolution,
/// and simulated trading costs. The allocation model (indicator periods,
/// zone config, weights) is supplied separately as a `TradingConfig` so
/// that a run always exercises the exact same allocation logic
/// (`TradingEngine::compute_btc_target`) as live trading.
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
    /// Number of leading candles in the series handed to `Simulator::run`
    /// that lie *before* `from` and exist only to warm the indicators up.
    ///
    /// NOTE: without this, indicators are warmed from the first in-window
    /// candle, so a long-period indicator is `None` for a large leading
    /// fraction of the evaluation window (e.g. SMA(200) over a 336-candle
    /// hourly window is undefined for 199 of them — 59% of the run). Those
    /// candles are fed through the indicator pipeline but are excluded from
    /// trading, the equity curve and the report, so the measured window is
    /// exactly `[from, to]` with fully warmed indicators throughout.
    ///
    /// Defaults to 0, which reproduces the previous behavior byte-for-byte.
    pub warmup_candles: usize,
}

/// Aggregate performance metrics produced by a completed backtest run.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BacktestReport {
    pub total_return_pct: f64,
    pub sharpe_ratio: f64,
    pub max_drawdown_pct: f64,
    pub win_rate: f64,
    pub total_trades: u32,
}

/// Result of comparing a candidate variant's report against a baseline
/// report over the same (holdout) period. See `report::compare` for the
/// promotion rule this encodes.
#[derive(Debug, Clone, Serialize)]
pub struct BacktestComparison {
    pub baseline: BacktestReport,
    pub candidate: BacktestReport,
    /// Relative Sharpe ratio improvement, in percent. `None` when the
    /// baseline Sharpe ratio is <= 0 (relative comparison is meaningless
    /// against a zero/negative baseline; see `sharpe_absolute_delta`).
    pub sharpe_improvement_pct: Option<f64>,
    /// Absolute Sharpe ratio delta (candidate - baseline). Always computed,
    /// used as the promotion signal when the baseline Sharpe is <= 0.
    pub sharpe_absolute_delta: f64,
    /// candidate.max_drawdown_pct - baseline.max_drawdown_pct. Negative or
    /// zero means the candidate did not make drawdown worse.
    pub drawdown_delta_pct: f64,
    /// candidate.total_trades / baseline.total_trades. Used to catch
    /// degenerate "improvements" from a near-empty sample.
    pub trade_count_ratio: f64,
    /// True when the candidate passes every promotion criterion.
    pub promoted: bool,
    /// Human-readable pros/cons lines explaining the verdict.
    pub reasons: Vec<String>,
}
