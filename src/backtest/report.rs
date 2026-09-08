use crate::exchange::mock::FilledTrade;
use crate::types::order::OrderSide;

use super::{BacktestComparison, BacktestReport, RiskEventCounts};

/// 人間が読みやすい形式でレポートを出力する
pub fn format_report(report: &BacktestReport) -> String {
    format!(
        "=== Backtest Report ===\n\
         Total Return : {:.2}%\n\
         Sharpe Ratio : {:.3}\n\
         Max Drawdown : {:.2}%\n\
         Win Rate     : {:.1}%\n\
         Total Trades : {}\n\
         CB Trips     : {}\n\
         Rejected     : {}",
        report.total_return_pct,
        report.sharpe_ratio,
        report.max_drawdown_pct,
        report.win_rate * 100.0,
        report.total_trades,
        report.circuit_breaker_trips,
        report.orders_rejected,
    )
}

/// Aggregate a sequence of filled trades and an equity curve into a
/// summary `BacktestReport`.  All financial metrics are computed here
/// from the raw trade log and per-candle portfolio valuations.
pub(crate) fn compute_report(
    trades: &[FilledTrade],
    equity_curve: &[f64],
    initial_jpy: f64,
    resolution_secs: u32,
    risk_events: RiskEventCounts,
) -> BacktestReport {
    let total_trades = trades.len() as u32;

    let final_equity = equity_curve.last().copied().unwrap_or(initial_jpy);
    let total_return_pct = (final_equity - initial_jpy) / initial_jpy * 100.0;

    let max_drawdown_pct = calculate_max_drawdown(equity_curve);
    let sharpe_ratio = calculate_sharpe(equity_curve, resolution_secs);
    let win_rate = calculate_win_rate(trades);

    BacktestReport {
        total_return_pct,
        sharpe_ratio,
        max_drawdown_pct,
        win_rate,
        total_trades,
        circuit_breaker_trips: risk_events.circuit_breaker_trips,
        orders_rejected: risk_events.orders_rejected,
    }
}

/// Calculate the maximum peak-to-trough drawdown as a percentage.
///
/// For each point in the equity curve, the drawdown from the running
/// peak is:
///
///   drawdown_pct = (peak - current) / peak * 100
///
/// The maximum such value across the entire series is returned.
/// Returns 0.0 for an empty series.
fn calculate_max_drawdown(equity: &[f64]) -> f64 {
    if equity.is_empty() {
        return 0.0;
    }
    let mut peak = equity[0];
    let mut max_dd = 0.0f64;
    for &e in equity {
        if e > peak {
            peak = e;
        }
        // Drawdown from the highest equity seen so far
        let dd = (peak - e) / peak * 100.0;
        if dd > max_dd {
            max_dd = dd;
        }
    }
    max_dd
}

/// Number of seconds in a 365-day year, used to derive how many candles
/// of a given resolution fall in one year.
const SECONDS_PER_YEAR: f64 = 365.0 * 24.0 * 60.0 * 60.0;

/// Calculate the annualized Sharpe ratio from a per-candle equity curve.
///
/// Per-candle returns are computed as:
///   r_t = (equity[t] - equity[t-1]) / equity[t-1]
///
/// The Sharpe ratio is then:
///   Sharpe = mean(r) / std(r) * sqrt(periods_per_year)
///
/// `periods_per_year` is derived from `resolution_secs` rather than being
/// fixed, since the annualization factor differs by an order of magnitude
/// across resolutions — e.g. sqrt(525_600) ≈ 725 for 60s candles but
/// sqrt(8_760) ≈ 93.6 for 1h candles. A hardcoded 60s factor previously
/// inflated every 1h backtest's Sharpe by sqrt(60) ≈ 7.75x.
///
/// Returns 0.0 for flat or insufficient equity data, and for a
/// `resolution_secs` of 0 (no meaningful period length).
fn calculate_sharpe(equity: &[f64], resolution_secs: u32) -> f64 {
    if equity.len() < 2 || resolution_secs == 0 {
        return 0.0;
    }
    let returns: Vec<f64> = equity
        .windows(2)
        .map(|w| (w[1] - w[0]) / w[0])
        .collect();
    let n = returns.len() as f64;
    let mean = returns.iter().sum::<f64>() / n;
    let variance = returns.iter().map(|r| (r - mean).powi(2)).sum::<f64>() / n;
    let std_dev = variance.sqrt();
    if std_dev == 0.0 {
        return 0.0;
    }
    // Convert the per-candle Sharpe to an annualized one using the number
    // of candles of this resolution that fit in a year.
    let periods_per_year = SECONDS_PER_YEAR / f64::from(resolution_secs);
    mean / std_dev * periods_per_year.sqrt()
}

/// Calculate the win rate using a LIFO buy-price stack.
///
/// Each buy price is pushed onto a stack; each sell is matched against
/// the most recently pushed buy (last-in first-out pairing).  A "win"
/// is recorded when the sell price strictly exceeds the matched buy
/// price.  Unpaired sells (no open position) are ignored.
///
/// Returns 0.0 when no round-trips have been completed.
fn calculate_win_rate(trades: &[FilledTrade]) -> f64 {
    // Stack of buy prices waiting to be matched with a future sell
    let mut buy_prices: Vec<rust_decimal::Decimal> = Vec::new();
    let mut wins = 0u32;
    let mut total_closed = 0u32;

    for trade in trades {
        match trade.side {
            OrderSide::Buy => buy_prices.push(trade.price),
            OrderSide::Sell => {
                // Match sell against the most recent unpaired buy (LIFO)
                if let Some(buy_price) = buy_prices.pop() {
                    total_closed += 1;
                    if trade.price > buy_price {
                        wins += 1;
                    }
                }
            }
        }
    }

    if total_closed == 0 {
        return 0.0;
    }
    wins as f64 / total_closed as f64
}

// ──────────────────────────────────────────────────────────────────────────────
// Holdout comparison — the "pros/cons" verdict used to promote or reject a
// worktree variant (see docs discussion: variants are only promoted when
// they clear ALL of the strict thresholds below on the holdout period).
// ──────────────────────────────────────────────────────────────────────────────

/// Minimum relative Sharpe ratio improvement (%) required for promotion
/// when the baseline Sharpe ratio is positive.
const MIN_SHARPE_IMPROVEMENT_PCT: f64 = 10.0;

/// Minimum absolute Sharpe ratio improvement required for promotion when
/// the baseline Sharpe ratio is <= 0 (a relative percentage against a
/// non-positive baseline is meaningless/undefined).
const MIN_SHARPE_ABSOLUTE_DELTA: f64 = 0.1;

/// A candidate whose trade count drops below this fraction of the
/// baseline's is rejected even if other metrics look better — too few
/// trades makes the comparison statistically unreliable.
const MIN_TRADE_COUNT_RATIO: f64 = 0.5;

/// Compare a candidate variant's backtest report against a baseline
/// report computed over the *same* (holdout) period, and decide whether
/// the candidate should be promoted (i.e. a PR should be opened for it).
///
/// Promotion requires ALL of:
/// - Sharpe ratio improves by >= 10% (relative) or, if the baseline
///   Sharpe is <= 0, by >= 0.1 (absolute)
/// - Max drawdown does not get worse (candidate <= baseline)
/// - Trade count does not collapse to a statistically unreliable sample
///   (candidate >= 50% of baseline's trade count)
pub fn compare(baseline: &BacktestReport, candidate: &BacktestReport) -> BacktestComparison {
    let sharpe_absolute_delta = candidate.sharpe_ratio - baseline.sharpe_ratio;
    let sharpe_improvement_pct = if baseline.sharpe_ratio > 0.0 {
        Some(sharpe_absolute_delta / baseline.sharpe_ratio * 100.0)
    } else {
        None
    };
    let drawdown_delta_pct = candidate.max_drawdown_pct - baseline.max_drawdown_pct;
    let trade_count_ratio = if baseline.total_trades == 0 {
        // No baseline trades to compare against; treat any candidate
        // trade count as "not degenerate" (ratio 1.0) rather than
        // dividing by zero.
        1.0
    } else {
        candidate.total_trades as f64 / baseline.total_trades as f64
    };

    let mut reasons = Vec::new();

    let sharpe_ok = match sharpe_improvement_pct {
        Some(pct) if pct >= MIN_SHARPE_IMPROVEMENT_PCT => {
            reasons.push(format!(
                "Pro: Sharpe ratio improved {pct:.1}% ({:.3} -> {:.3})",
                baseline.sharpe_ratio, candidate.sharpe_ratio
            ));
            true
        }
        Some(pct) => {
            reasons.push(format!(
                "Con: Sharpe ratio improvement {pct:.1}% is below the {MIN_SHARPE_IMPROVEMENT_PCT}% threshold"
            ));
            false
        }
        None if sharpe_absolute_delta >= MIN_SHARPE_ABSOLUTE_DELTA => {
            reasons.push(format!(
                "Pro: Sharpe ratio improved by {sharpe_absolute_delta:.3} (baseline was non-positive: {:.3})",
                baseline.sharpe_ratio
            ));
            true
        }
        None => {
            reasons.push(format!(
                "Con: Sharpe ratio absolute improvement {sharpe_absolute_delta:.3} is below the {MIN_SHARPE_ABSOLUTE_DELTA} threshold (baseline non-positive)"
            ));
            false
        }
    };

    let drawdown_ok = drawdown_delta_pct <= 0.0;
    if drawdown_ok {
        reasons.push(format!(
            "Pro: max drawdown did not worsen ({:.2}% -> {:.2}%)",
            baseline.max_drawdown_pct, candidate.max_drawdown_pct
        ));
    } else {
        reasons.push(format!(
            "Con: max drawdown worsened by {drawdown_delta_pct:.2}pp ({:.2}% -> {:.2}%)",
            baseline.max_drawdown_pct, candidate.max_drawdown_pct
        ));
    }

    let trade_count_ok = trade_count_ratio >= MIN_TRADE_COUNT_RATIO;
    if trade_count_ok {
        reasons.push(format!(
            "Pro: trade count is {:.0}% of baseline ({} vs {}), sample size acceptable",
            trade_count_ratio * 100.0, candidate.total_trades, baseline.total_trades
        ));
    } else {
        reasons.push(format!(
            "Con: trade count collapsed to {:.0}% of baseline ({} vs {}) — sample too small to trust",
            trade_count_ratio * 100.0, candidate.total_trades, baseline.total_trades
        ));
    }

    let promoted = sharpe_ok && drawdown_ok && trade_count_ok;

    BacktestComparison {
        baseline: baseline.clone(),
        candidate: candidate.clone(),
        sharpe_improvement_pct,
        sharpe_absolute_delta,
        drawdown_delta_pct,
        trade_count_ratio,
        promoted,
        reasons,
    }
}

/// 人間が読みやすい Pros/Cons レポート形式で出力する（PR説明への埋め込み用）。
pub fn format_comparison(cmp: &BacktestComparison) -> String {
    let verdict = if cmp.promoted { "PROMOTE" } else { "REJECT" };
    let mut lines = vec![format!("=== Backtest Comparison ({verdict}) ===")];
    lines.extend(cmp.reasons.iter().cloned());
    lines.join("\n")
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
            circuit_breaker_trips: 2,
            orders_rejected: 7,
        };
        let s = format_report(&report);
        assert!(s.contains("12.34"));
        assert!(s.contains("1.567"));
        assert!(s.contains("3.21"));
        assert!(s.contains("55.0"));
        assert!(s.contains("42"));
        assert!(s.contains("CB Trips     : 2"));
        assert!(s.contains("Rejected     : 7"));
    }

    /// The annualization factor must follow `resolution_secs`; a 1h
    /// backtest's Sharpe should be sqrt(60) smaller than the same equity
    /// curve interpreted as 60s candles, not identical to it.
    #[test]
    fn sharpe_annualization_scales_with_resolution() {
        // Alternating small gains/losses with a positive drift.
        let equity: Vec<f64> = (0..100)
            .map(|i| 1_000_000.0 * (1.0 + 0.001 * i as f64 + if i % 2 == 0 { 0.0002 } else { 0.0 }))
            .collect();

        let sharpe_1m = calculate_sharpe(&equity, 60);
        let sharpe_1h = calculate_sharpe(&equity, 3600);

        // sqrt(3600/60) = sqrt(60) ≈ 7.746
        let ratio = sharpe_1m / sharpe_1h;
        assert!(
            (ratio - 60.0f64.sqrt()).abs() < 1e-6,
            "expected sqrt(60) ratio, got {ratio}"
        );
    }

    #[test]
    fn sharpe_is_zero_for_zero_resolution() {
        let equity = vec![1.0, 1.1, 1.2];
        assert_eq!(calculate_sharpe(&equity, 0), 0.0);
    }

    fn report(sharpe: f64, dd: f64, trades: u32) -> BacktestReport {
        BacktestReport {
            total_return_pct: 0.0,
            sharpe_ratio: sharpe,
            max_drawdown_pct: dd,
            win_rate: 0.5,
            total_trades: trades,
            circuit_breaker_trips: 0,
            orders_rejected: 0,
        }
    }

    #[test]
    fn promotes_when_all_criteria_pass() {
        let baseline = report(1.0, 10.0, 100);
        let candidate = report(1.2, 9.0, 90); // +20% sharpe, DD improves, trades 90%
        let cmp = compare(&baseline, &candidate);
        assert!(cmp.promoted, "{:?}", cmp.reasons);
    }

    #[test]
    fn rejects_when_sharpe_improvement_too_small() {
        let baseline = report(1.0, 10.0, 100);
        let candidate = report(1.05, 9.0, 100); // only +5% sharpe
        let cmp = compare(&baseline, &candidate);
        assert!(!cmp.promoted);
    }

    #[test]
    fn rejects_when_drawdown_worsens() {
        let baseline = report(1.0, 10.0, 100);
        let candidate = report(1.5, 12.0, 100); // sharpe way up but DD worse
        let cmp = compare(&baseline, &candidate);
        assert!(!cmp.promoted);
    }

    #[test]
    fn rejects_when_trade_count_collapses() {
        let baseline = report(1.0, 10.0, 100);
        let candidate = report(2.0, 5.0, 10); // huge sharpe gain but only 10% of trades
        let cmp = compare(&baseline, &candidate);
        assert!(!cmp.promoted);
    }

    #[test]
    fn uses_absolute_delta_when_baseline_sharpe_non_positive() {
        let baseline = report(0.0, 10.0, 100);
        let candidate = report(0.2, 9.0, 100);
        let cmp = compare(&baseline, &candidate);
        assert!(cmp.sharpe_improvement_pct.is_none());
        assert!(cmp.promoted, "{:?}", cmp.reasons);
    }

    #[test]
    fn baseline_with_zero_trades_does_not_divide_by_zero() {
        let baseline = report(0.0, 10.0, 0);
        let candidate = report(0.5, 9.0, 5);
        let cmp = compare(&baseline, &candidate);
        assert_eq!(cmp.trade_count_ratio, 1.0);
        assert!(cmp.promoted, "{:?}", cmp.reasons);
    }

    #[test]
    fn format_comparison_shows_verdict_and_reasons() {
        let baseline = report(1.0, 10.0, 100);
        let candidate = report(1.2, 9.0, 90);
        let cmp = compare(&baseline, &candidate);
        let s = format_comparison(&cmp);
        assert!(s.contains("PROMOTE"));
        assert!(s.contains("Sharpe ratio improved"));
    }
}
