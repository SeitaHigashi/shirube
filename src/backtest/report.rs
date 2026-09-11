use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;

use crate::exchange::mock::{fee_from_volume, FilledTrade};
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
         Rejected     : {}\n\
         Below Min Lot: {}\n\
         Total Fees   : {:.2} JPY\n\
         Traded Volume: {:.2} JPY\n\
         Effective Fee: {:.4}%\n\
         Fee Drag     : {:.2}%\n\
         --- Benchmarks (reported diagnostic — NOT a promotion gate) ---\n\
         Avg BTC Exp. : {:.1}%\n\
         Hold Return  : {:.2}% (Sharpe {:.3}, MaxDD {:.2}%)\n\
         Static Mix   : {:.2}% (Sharpe {:.3}, MaxDD {:.2}%)\n\
         Excess vs Mix: {:.2}pp (Sharpe {:+.3})",
        report.total_return_pct,
        report.sharpe_ratio,
        report.max_drawdown_pct,
        report.win_rate * 100.0,
        report.total_trades,
        report.circuit_breaker_trips,
        report.orders_rejected,
        report.orders_below_min,
        report.total_fees_jpy,
        report.traded_volume_jpy,
        report.effective_fee_pct * 100.0,
        report.fee_drag_pct,
        report.avg_btc_exposure * 100.0,
        report.hold_return_pct,
        report.hold_sharpe_ratio,
        report.hold_max_drawdown_pct,
        report.static_mix_return_pct,
        report.static_mix_sharpe_ratio,
        report.static_mix_max_drawdown_pct,
        report.excess_return_vs_static_mix_pct,
        report.sharpe_minus_static_mix,
    )
}

/// Aggregate a sequence of filled trades and an equity curve into a
/// summary `BacktestReport`.  All financial metrics are computed here
/// from the raw trade log and per-candle portfolio valuations.
///
/// `fixed_fee_pct` mirrors `BacktestConfig::fee_pct`: `Some(rate)` when the
/// run used a fixed fee override (`MockExchangeClient::with_fee`) instead
/// of the tier table, so `final_fee_tier_pct` reports that fixed rate
/// rather than a tier lookup that was never actually applied.
///
/// `avg_btc_exposure` and `evaluated_closes` feed the buy-and-hold /
/// static-mix benchmarks (see `benchmark_equity_curve`); `evaluated_closes`
/// must be exactly the raw `candle.close` values of the *evaluated* candles
/// (warmup excluded, same length/order as `equity_curve`) or the benchmark
/// silently includes candles the strategy itself never traded on.
#[allow(clippy::too_many_arguments)]
pub(crate) fn compute_report(
    trades: &[FilledTrade],
    equity_curve: &[f64],
    initial_jpy: f64,
    resolution_secs: u32,
    risk_events: RiskEventCounts,
    fixed_fee_pct: Option<f64>,
    avg_btc_exposure: f64,
    evaluated_closes: &[f64],
    slippage_pct: f64,
) -> BacktestReport {
    let total_trades = trades.len() as u32;

    let final_equity = equity_curve.last().copied().unwrap_or(initial_jpy);
    let total_return_pct = (final_equity - initial_jpy) / initial_jpy * 100.0;

    let max_drawdown_pct = calculate_max_drawdown(equity_curve);
    let sharpe_ratio = calculate_sharpe(equity_curve, resolution_secs);
    let win_rate = calculate_win_rate(trades);

    // Trading-cost aggregates: total fees paid, and the JPY notional they
    // were paid on (price * size per fill — the same quantity bitFlyer's
    // fee tier table is keyed on).
    let total_fees_jpy: f64 = trades
        .iter()
        .map(|t| t.fee.to_f64().unwrap_or(0.0))
        .sum();
    let traded_volume_jpy: f64 = trades
        .iter()
        .map(|t| (t.price * t.size).to_f64().unwrap_or(0.0))
        .sum();
    let effective_fee_pct = if traded_volume_jpy == 0.0 {
        0.0
    } else {
        total_fees_jpy / traded_volume_jpy
    };
    // Tier rate that applied at the end of the run — a fixed override
    // reports its own rate rather than a tier lookup, since no tier was
    // ever actually consulted for such a run.
    let final_fee_tier_pct = match fixed_fee_pct {
        Some(rate) => rate,
        None => {
            let volume_decimal =
                Decimal::try_from(traded_volume_jpy).unwrap_or(Decimal::ZERO);
            fee_from_volume(volume_decimal)
        }
    };
    let fee_drag_pct = if initial_jpy == 0.0 {
        0.0
    } else {
        total_fees_jpy / initial_jpy * 100.0
    };

    // NOTE: benchmark entry commission uses `fixed_fee_pct` when the run
    // fixed its fee rate, otherwise the entry tier (0.0015) rather than
    // `final_fee_tier_pct` (the tier reached only after the run's *own*
    // cumulative volume). bitFlyer's tiers key on the prior 30-day volume,
    // so a fresh account's very first trade is always charged the top
    // (10万円未満) tier — this is deliberately conservative *against* the
    // benchmark (it is charged the worst plausible rate for a single
    // isolated trade), so it never flatters the benchmark relative to the
    // strategy.
    const ENTRY_TIER_FEE_PCT: f64 = 0.0015;
    let benchmark_entry_fee_pct = fixed_fee_pct.unwrap_or(ENTRY_TIER_FEE_PCT);

    let hold_curve =
        benchmark_equity_curve(evaluated_closes, initial_jpy, 1.0, slippage_pct, benchmark_entry_fee_pct);
    let hold_return_pct = benchmark_return_pct(&hold_curve, initial_jpy);
    let hold_sharpe_ratio = calculate_sharpe(&hold_curve, resolution_secs);
    let hold_max_drawdown_pct = calculate_max_drawdown(&hold_curve);

    let static_mix_curve = benchmark_equity_curve(
        evaluated_closes,
        initial_jpy,
        avg_btc_exposure,
        slippage_pct,
        benchmark_entry_fee_pct,
    );
    let static_mix_return_pct = benchmark_return_pct(&static_mix_curve, initial_jpy);
    let static_mix_sharpe_ratio = calculate_sharpe(&static_mix_curve, resolution_secs);
    let static_mix_max_drawdown_pct = calculate_max_drawdown(&static_mix_curve);

    BacktestReport {
        total_return_pct,
        sharpe_ratio,
        max_drawdown_pct,
        win_rate,
        total_trades,
        circuit_breaker_trips: risk_events.circuit_breaker_trips,
        orders_rejected: risk_events.orders_rejected,
        orders_below_min: risk_events.orders_below_min,
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
        excess_return_vs_static_mix_pct: total_return_pct - static_mix_return_pct,
        sharpe_minus_static_mix: sharpe_ratio - static_mix_sharpe_ratio,
    }
}

/// Build a benchmark equity curve for a single entry-and-hold position:
/// buy `exposure_fraction` of `initial_jpy` (inclusive of a single entry
/// commission `entry_fee_pct`) worth of BTC at `closes[0]` plus
/// `slippage_pct` slippage, hold the rest of the balance as JPY, then mark
/// to market at every subsequent close in `closes` — using the raw close
/// (no slippage on marks), matching how `Simulator::run` marks its own
/// equity curve.
///
/// `notional + fee == exposure_fraction * initial_jpy` by construction, so
/// the JPY held back is exactly `initial_jpy * (1 - exposure_fraction)`:
///
///   entry_price = closes[0] * (1 + slippage_pct)
///   notional     = exposure_fraction * initial_jpy / (1 + entry_fee_pct)
///   btc_qty      = notional / entry_price
///   jpy_held     = initial_jpy * (1 - exposure_fraction)
///   equity[t]    = jpy_held + btc_qty * closes[t]
///
/// Returns an empty curve when `closes` is empty.
pub(crate) fn benchmark_equity_curve(
    closes: &[f64],
    initial_jpy: f64,
    exposure_fraction: f64,
    slippage_pct: f64,
    entry_fee_pct: f64,
) -> Vec<f64> {
    if closes.is_empty() {
        return Vec::new();
    }
    let entry_price = closes[0] * (1.0 + slippage_pct);
    let notional = exposure_fraction * initial_jpy / (1.0 + entry_fee_pct);
    let btc_qty = if entry_price > 0.0 { notional / entry_price } else { 0.0 };
    let jpy_held = initial_jpy * (1.0 - exposure_fraction);
    closes.iter().map(|&c| jpy_held + btc_qty * c).collect()
}

/// Total return (%) of a benchmark curve relative to `initial_jpy`. Returns
/// 0.0 for an empty curve or a zero `initial_jpy` rather than dividing by
/// zero.
fn benchmark_return_pct(curve: &[f64], initial_jpy: f64) -> f64 {
    if initial_jpy == 0.0 {
        return 0.0;
    }
    let final_equity = curve.last().copied().unwrap_or(initial_jpy);
    (final_equity - initial_jpy) / initial_jpy * 100.0
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
///
/// `pub(crate)` so the buy-and-hold/static-mix benchmark curves built in
/// `compute_report` reuse this exact function instead of a second copy.
pub(crate) fn calculate_max_drawdown(equity: &[f64]) -> f64 {
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
///
/// `pub(crate)` so the buy-and-hold/static-mix benchmark curves built in
/// `compute_report` reuse this exact function instead of a second copy —
/// the annualization must use the same `resolution_secs` as the
/// strategy's own Sharpe, or the comparison is meaningless.
pub(crate) fn calculate_sharpe(equity: &[f64], resolution_secs: u32) -> f64 {
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
            orders_below_min: 13,
            total_fees_jpy: 1_234.5,
            traded_volume_jpy: 987_654.0,
            effective_fee_pct: 0.00125,
            final_fee_tier_pct: 0.0011,
            fee_drag_pct: 0.12,
            avg_btc_exposure: 0.55,
            hold_return_pct: 19.6,
            hold_sharpe_ratio: 6.7,
            hold_max_drawdown_pct: 7.7,
            static_mix_return_pct: 10.8,
            static_mix_sharpe_ratio: 6.5,
            static_mix_max_drawdown_pct: 4.6,
            excess_return_vs_static_mix_pct: 1.5,
            sharpe_minus_static_mix: 0.86,
        };
        let s = format_report(&report);
        assert!(s.contains("12.34"));
        assert!(s.contains("1.567"));
        assert!(s.contains("3.21"));
        assert!(s.contains("55.0"));
        assert!(s.contains("42"));
        assert!(s.contains("CB Trips     : 2"));
        assert!(s.contains("Rejected     : 7"));
        assert!(s.contains("Below Min Lot: 13"));
        assert!(s.contains("Total Fees   : 1234.50 JPY"));
        assert!(s.contains("Traded Volume: 987654.00 JPY"));
        assert!(s.contains("Effective Fee: 0.1250%"));
        assert!(s.contains("Fee Drag     : 0.12%"));
        assert!(s.contains("Avg BTC Exp. : 55.0%"));
        assert!(s.contains("Hold Return  : 19.60%"));
        assert!(s.contains("Static Mix   : 10.80%"));
        assert!(s.contains("Excess vs Mix: 1.50pp"));
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
            orders_below_min: 0,
            total_fees_jpy: 0.0,
            traded_volume_jpy: 0.0,
            effective_fee_pct: 0.0,
            final_fee_tier_pct: 0.0,
            fee_drag_pct: 0.0,
            avg_btc_exposure: 0.0,
            hold_return_pct: 0.0,
            hold_sharpe_ratio: 0.0,
            hold_max_drawdown_pct: 0.0,
            static_mix_return_pct: 0.0,
            static_mix_sharpe_ratio: 0.0,
            static_mix_max_drawdown_pct: 0.0,
            excess_return_vs_static_mix_pct: 0.0,
            sharpe_minus_static_mix: 0.0,
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

    /// `compute_report` must aggregate fee/volume figures exactly from the
    /// raw trade log, independent of the other statistics computed from the
    /// equity curve.
    #[test]
    fn compute_report_aggregates_fee_metrics() {
        use rust_decimal_macros::dec;

        let trades = vec![
            FilledTrade {
                side: OrderSide::Buy,
                price: dec!(9_000_000),
                size: dec!(0.001),
                fee: dec!(13.5), // 9_000 * 0.0015
            },
            FilledTrade {
                side: OrderSide::Sell,
                price: dec!(9_500_000),
                size: dec!(0.001),
                fee: dec!(14.25), // 9_500 * 0.0015
            },
        ];
        let equity_curve = vec![1_000_000.0, 1_000_500.0];
        let evaluated_closes = vec![9_000_000.0, 9_500_000.0];

        let report = compute_report(
            &trades,
            &equity_curve,
            1_000_000.0,
            60,
            RiskEventCounts::default(),
            Some(0.0015),
            0.5,
            &evaluated_closes,
            0.0,
        );

        // total_fees_jpy = 13.5 + 14.25
        assert!((report.total_fees_jpy - 27.75).abs() < 1e-9);
        // traded_volume_jpy = 9_000_000*0.001 + 9_500_000*0.001 = 9_000 + 9_500
        assert!((report.traded_volume_jpy - 18_500.0).abs() < 1e-9);
        // effective_fee_pct = 27.75 / 18_500
        assert!((report.effective_fee_pct - (27.75 / 18_500.0)).abs() < 1e-12);
        // fee_drag_pct = 27.75 / 1_000_000 * 100
        assert!((report.fee_drag_pct - (27.75 / 1_000_000.0 * 100.0)).abs() < 1e-12);
        // fixed fee override was supplied, so final_fee_tier_pct echoes it.
        assert_eq!(report.final_fee_tier_pct, 0.0015);
    }

    /// With no trades, the fee metrics must be 0.0 rather than NaN/Inf from
    /// a division by zero.
    #[test]
    fn compute_report_zero_trades_has_zero_fee_metrics() {
        let report = compute_report(
            &[],
            &[1_000_000.0],
            1_000_000.0,
            60,
            RiskEventCounts::default(),
            None,
            0.0,
            &[9_000_000.0],
            0.0,
        );
        assert_eq!(report.total_fees_jpy, 0.0);
        assert_eq!(report.traded_volume_jpy, 0.0);
        assert_eq!(report.effective_fee_pct, 0.0);
        assert_eq!(report.fee_drag_pct, 0.0);
        // No trades and no fixed override -> tier lookup on zero volume,
        // which is the highest (10万円未満) tier.
        assert_eq!(report.final_fee_tier_pct, 0.0015);
    }

    /// Report JSON saved by a version of this codebase before the fee
    /// metrics existed must still deserialize — `shirube compare-backtest`
    /// reads baseline reports written by earlier runs.
    #[test]
    fn backtest_report_deserializes_without_fee_fields() {
        let old_json = r#"{
            "total_return_pct": 12.34,
            "sharpe_ratio": 1.5,
            "max_drawdown_pct": 3.2,
            "win_rate": 0.5,
            "total_trades": 10,
            "circuit_breaker_trips": 0,
            "orders_rejected": 0
        }"#;
        let report: BacktestReport = serde_json::from_str(old_json).unwrap();
        assert_eq!(report.total_return_pct, 12.34);
        assert_eq!(report.total_fees_jpy, 0.0);
        assert_eq!(report.traded_volume_jpy, 0.0);
        assert_eq!(report.effective_fee_pct, 0.0);
        assert_eq!(report.final_fee_tier_pct, 0.0);
        assert_eq!(report.fee_drag_pct, 0.0);
        // The benchmark fields added in this change must default the same
        // way for report JSON written before they existed.
        assert_eq!(report.avg_btc_exposure, 0.0);
        assert_eq!(report.hold_return_pct, 0.0);
        assert_eq!(report.hold_sharpe_ratio, 0.0);
        assert_eq!(report.hold_max_drawdown_pct, 0.0);
        assert_eq!(report.static_mix_return_pct, 0.0);
        assert_eq!(report.static_mix_sharpe_ratio, 0.0);
        assert_eq!(report.static_mix_max_drawdown_pct, 0.0);
        assert_eq!(report.excess_return_vs_static_mix_pct, 0.0);
        assert_eq!(report.sharpe_minus_static_mix, 0.0);
        // Likewise for the lot-size counter added by this change.
        assert_eq!(report.orders_below_min, 0);
    }

    /// `hold_return_pct` on a strictly monotonically rising price series
    /// must equal the price change minus entry costs, computed by hand
    /// (not via `benchmark_equity_curve` — that would just restate the
    /// implementation under test).
    #[test]
    fn hold_return_matches_hand_computed_value_on_rising_series() {
        let evaluated_closes = vec![9_000_000.0, 10_000_000.0];
        let slippage_pct = 0.001;
        let fee_pct = 0.0015;

        let report = compute_report(
            &[],
            &[1_000_000.0, 1_000_000.0], // strategy's own equity curve is irrelevant here
            1_000_000.0,
            60,
            RiskEventCounts::default(),
            Some(fee_pct),
            0.5, // avg_btc_exposure is irrelevant to the hold benchmark
            &evaluated_closes,
            slippage_pct,
        );

        // By hand: entry buys (1 / (1+fee)) JPY-worth of BTC at
        // close[0]*(1+slippage), then marks to market at close[1].
        // equity_final / initial = (close[1]/close[0]) / ((1+fee)*(1+slippage))
        let expected_hold_return_pct =
            ((10_000_000.0 / 9_000_000.0) / (1.0015 * 1.001) - 1.0) * 100.0;
        assert!(
            (report.hold_return_pct - expected_hold_return_pct).abs() < 1e-9,
            "got {}, expected {}",
            report.hold_return_pct,
            expected_hold_return_pct
        );
    }

    /// On a flat price series, a full-exposure benchmark curve is constant
    /// (return is entirely the entry slippage+fee, and there is zero
    /// variance to annualize into a Sharpe ratio).
    #[test]
    fn flat_price_series_benchmark_loses_only_entry_cost_with_zero_sharpe() {
        let evaluated_closes = vec![9_000_000.0; 5];
        let slippage_pct = 0.001;
        let fee_pct = 0.0015;

        let report = compute_report(
            &[],
            &[1_000_000.0; 5],
            1_000_000.0,
            60,
            RiskEventCounts::default(),
            Some(fee_pct),
            1.0, // full exposure so static-mix == hold on this fixture
            &evaluated_closes,
            slippage_pct,
        );

        let expected_return_pct = (1.0 / (1.0015 * 1.001) - 1.0) * 100.0;
        assert!(
            (report.hold_return_pct - expected_return_pct).abs() < 1e-9,
            "got {}",
            report.hold_return_pct
        );
        assert!(
            (report.static_mix_return_pct - expected_return_pct).abs() < 1e-9,
            "got {}",
            report.static_mix_return_pct
        );
        assert_eq!(report.hold_sharpe_ratio, 0.0);
        assert_eq!(report.static_mix_sharpe_ratio, 0.0);
    }

    /// When `avg_btc_exposure == 1.0`, the static-mix benchmark is buying
    /// the entire balance too — it must equal the buy-and-hold benchmark
    /// exactly, not just approximately.
    #[test]
    fn static_mix_equals_hold_when_avg_exposure_is_full() {
        let evaluated_closes = vec![9_000_000.0, 9_200_000.0, 8_900_000.0, 9_500_000.0];

        let report = compute_report(
            &[],
            &[1_000_000.0; 4],
            1_000_000.0,
            60,
            RiskEventCounts::default(),
            Some(0.0015),
            1.0,
            &evaluated_closes,
            0.001,
        );

        assert_eq!(report.static_mix_return_pct, report.hold_return_pct);
        assert_eq!(report.static_mix_sharpe_ratio, report.hold_sharpe_ratio);
        assert_eq!(report.static_mix_max_drawdown_pct, report.hold_max_drawdown_pct);
        assert_eq!(
            report.excess_return_vs_static_mix_pct,
            report.total_return_pct - report.hold_return_pct
        );
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
