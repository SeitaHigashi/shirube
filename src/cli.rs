//! Headless subcommands used by the automated variant-validation pipeline
//! (see `docs/self-improvement-loop.md`). Each worktree agent in that
//! pipeline shells out to these instead of writing its own backtest glue,
//! so every variant is scored by the exact same code path.
//!
//! Usage:
//!   shirube backtest-variant --db <path> --config <trading_config.json>
//!       --from <RFC3339> --to <RFC3339>
//!       [--product BTC_JPY] [--resolution-secs 3600]
//!       [--initial-jpy 1000000] [--slippage-pct 0.001] [--fee-pct 0.0015]
//!     → prints a BacktestReport as JSON to stdout
//!
//!   shirube compare-backtest --baseline <report.json> --candidate <report.json>
//!     → prints the Pros/Cons verdict to stderr and a BacktestComparison
//!       JSON (with a `promoted` boolean) to stdout

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use std::str::FromStr;

use crate::backtest::report::{compare, format_comparison};
use crate::backtest::simulator::Simulator;
use crate::backtest::{BacktestConfig, BacktestReport};
use crate::config::TradingConfig;
use crate::storage::db::Database;

/// Inspect argv for a known subcommand and run it to completion.
/// Returns `Ok(true)` when a subcommand was handled (the caller should
/// exit immediately afterwards); `Ok(false)` when argv doesn't match any
/// subcommand, so normal server startup should proceed.
pub async fn dispatch() -> anyhow::Result<bool> {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("backtest-variant") => {
            run_backtest_variant(&args).await?;
            Ok(true)
        }
        Some("compare-backtest") => {
            run_compare_backtest(&args)?;
            Ok(true)
        }
        Some("print-default-config") => {
            println!("{}", serde_json::to_string_pretty(&TradingConfig::default())?);
            Ok(true)
        }
        _ => Ok(false),
    }
}

fn flag_value(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

fn required_flag(args: &[String], name: &str) -> anyhow::Result<String> {
    flag_value(args, name).ok_or_else(|| anyhow::anyhow!("missing required flag {name}"))
}

fn parse_rfc3339(s: &str) -> anyhow::Result<DateTime<Utc>> {
    Ok(DateTime::parse_from_rfc3339(s)?.with_timezone(&Utc))
}

async fn run_backtest_variant(args: &[String]) -> anyhow::Result<()> {
    let db_path = flag_value(args, "--db").unwrap_or_else(|| "shirube.db".to_string());
    let config_path = required_flag(args, "--config")?;
    let product_code = flag_value(args, "--product").unwrap_or_else(|| "BTC_JPY".to_string());
    let from = parse_rfc3339(&required_flag(args, "--from")?)?;
    let to = parse_rfc3339(&required_flag(args, "--to")?)?;
    let resolution_secs: u32 = flag_value(args, "--resolution-secs")
        .unwrap_or_else(|| "3600".to_string())
        .parse()?;
    let initial_jpy = Decimal::from_str(
        &flag_value(args, "--initial-jpy").unwrap_or_else(|| "1000000".to_string()),
    )?;
    let slippage_pct: f64 = flag_value(args, "--slippage-pct")
        .unwrap_or_else(|| "0.001".to_string())
        .parse()?;
    let fee_pct: Option<f64> = flag_value(args, "--fee-pct")
        .map(|s| s.parse::<f64>())
        .transpose()?;

    let trading_config_json = std::fs::read_to_string(&config_path)
        .map_err(|e| anyhow::anyhow!("failed to read --config {config_path}: {e}"))?;
    let trading_config: TradingConfig = serde_json::from_str(&trading_config_json)
        .map_err(|e| anyhow::anyhow!("failed to parse {config_path} as TradingConfig: {e}"))?;
    trading_config
        .validate()
        .map_err(|e| anyhow::anyhow!("invalid trading config in {config_path}: {e}"))?;

    let db = Database::open(&db_path).await?;
    let candles = db
        .tickers()
        .get_aggregated(&product_code, resolution_secs, from, to, None)
        .await?;
    if candles.is_empty() {
        anyhow::bail!("no candles found for {product_code} in [{from}, {to}] (resolution_secs={resolution_secs})");
    }

    let bt_config = BacktestConfig {
        product_code,
        from,
        to,
        resolution_secs,
        slippage_pct,
        fee_pct,
        initial_jpy,
    };
    let simulator = Simulator::new(bt_config, db);
    let report = simulator.run(candles, trading_config).await?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

fn run_compare_backtest(args: &[String]) -> anyhow::Result<()> {
    let baseline_path = required_flag(args, "--baseline")?;
    let candidate_path = required_flag(args, "--candidate")?;

    let baseline: BacktestReport = serde_json::from_str(&std::fs::read_to_string(&baseline_path)?)
        .map_err(|e| anyhow::anyhow!("failed to parse {baseline_path}: {e}"))?;
    let candidate: BacktestReport =
        serde_json::from_str(&std::fs::read_to_string(&candidate_path)?)
            .map_err(|e| anyhow::anyhow!("failed to parse {candidate_path}: {e}"))?;

    let cmp = compare(&baseline, &candidate);
    eprintln!("{}", format_comparison(&cmp));
    println!("{}", serde_json::to_string_pretty(&cmp)?);
    Ok(())
}
