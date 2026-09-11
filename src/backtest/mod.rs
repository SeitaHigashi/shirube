pub mod backfill;
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
    /// How many times the daily-drawdown circuit breaker tripped during the
    /// run (at most once per simulated day, since `RiskManager` clears the
    /// flag at each day boundary).
    ///
    /// NOTE: `#[serde(default)]` so that report JSON written before this
    /// field existed still parses — `shirube compare-backtest` reads
    /// baseline reports saved by earlier runs.
    #[serde(default)]
    pub circuit_breaker_trips: u32,
    /// How many orders `RiskManager::evaluate` refused (circuit breaker
    /// active, or size below the exchange minimum). An order counted here
    /// never reached the exchange, so it is absent from `total_trades`.
    #[serde(default)]
    pub orders_rejected: u32,
    /// Sum of every filled trade's fee (JPY), i.e. total trading costs paid
    /// over the run.
    ///
    /// NOTE: `#[serde(default)]` so that report JSON written before this
    /// field existed still parses — `shirube compare-backtest` reads
    /// baseline reports saved by earlier runs.
    #[serde(default)]
    pub total_fees_jpy: f64,
    /// Sum of `price * size` over every filled trade — the cumulative JPY
    /// notional traded, i.e. the same quantity bitFlyer's fee-tier table is
    /// keyed on (see `FEE_TIERS` in `exchange::mock`).
    #[serde(default)]
    pub traded_volume_jpy: f64,
    /// `total_fees_jpy / traded_volume_jpy` — the blended fee rate actually
    /// paid across the whole run. 0.0 when no trades were placed (or
    /// `traded_volume_jpy` is 0), rather than dividing by zero.
    #[serde(default)]
    pub effective_fee_pct: f64,
    /// The bitFlyer fee tier rate that applied by the end of the run, i.e.
    /// looked up from the final cumulative `traded_volume_jpy`. When the run
    /// used a fixed `--fee-pct` override instead of the tier table, this is
    /// simply that fixed rate.
    #[serde(default)]
    pub final_fee_tier_pct: f64,
    /// `total_fees_jpy / initial_jpy * 100.0` — total fees as a percentage
    /// of starting capital, using the same denominator convention as
    /// `total_return_pct` so the two are directly comparable (e.g. "fees ate
    /// N percentage points of the return"). 0.0 when `initial_jpy` is 0.
    #[serde(default)]
    pub fee_drag_pct: f64,
}

/// Risk-gate activity observed during a backtest run.
///
/// # Why this is counted
///
/// `Simulator::run` discards every `RiskDecision` it does not act on, so a
/// risk-gate setting that never fires and one that fires constantly used to
/// be indistinguishable from the report alone. Establishing which had
/// happened for a single `max_daily_drawdown` hypothesis (2026-09-08) took
/// four extra backtests plus a separate price-series analysis, and the
/// obvious proxy is wrong: blocking a rebalance on a down day defers the
/// exposure change to a later candle rather than removing a trade, so
/// `total_trades` can be identical whether or not the breaker fired.
#[derive(Debug, Clone, Copy, Default)]
pub struct RiskEventCounts {
    pub circuit_breaker_trips: u32,
    pub orders_rejected: u32,
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
