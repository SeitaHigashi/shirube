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
//!       [--warmup-candles 0]   ← extra candles fetched *before* --from purely
//!                                to warm the indicators; excluded from the report
//!     → prints a BacktestReport as JSON to stdout
//!
//!   shirube backfill-executions --db <path>
//!       [--product BTC_JPY] [--days 31] [--resolution-secs 60]
//!     → pages bitFlyer's public execution history backwards, writes one
//!       OHLCV bar per bucket into `tickers`, prints BackfillStats as JSON.
//!       Capped at 31 days by bitFlyer's retention window.
//!
//!   shirube compare-backtest --baseline <report.json> --candidate <report.json>
//!     → prints the Pros/Cons verdict to stderr and a BacktestComparison
//!       JSON (with a `promoted` boolean) to stdout
//!
//!   shirube hypothesis-hash <hypothesis.json> [<hypothesis.json> ...]
//!     → prints `<sha256-hex>  <path>` per file: the canonical
//!       `content_hash` used as the dedup key in `experiments/tried.json`

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use std::str::FromStr;

use crate::backtest::backfill::backfill_executions;
use crate::backtest::report::{compare, format_comparison};
use crate::backtest::simulator::Simulator;
use crate::backtest::{BacktestConfig, BacktestReport};
use crate::config::TradingConfig;
use crate::exchange::bitflyer::rest::BitFlyerRestClient;
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
        Some("backfill-executions") => {
            run_backfill_executions(&args).await?;
            Ok(true)
        }
        Some("hypothesis-hash") => {
            run_hypothesis_hash(&args)?;
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
    // Default matches live trading (MarketDataBus runs at 60s and
    // TradingConfig periods are expressed in 1-minute bars); running the
    // backtest at a coarser resolution would silently reinterpret e.g.
    // sma_period=200 as 200 hours instead of 200 minutes.
    let resolution_secs: u32 = flag_value(args, "--resolution-secs")
        .unwrap_or_else(|| "60".to_string())
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

    // Optional indicator warmup lookback, in candles. Candles before `from` are
    // fetched purely to prime the indicators and are excluded from trading and
    // from the report, so a long-period indicator (e.g. SMA(200)) is already
    // warm at the first evaluated candle instead of being `None` for the
    // leading 59% of the window. Defaults to 0 = previous behavior.
    let warmup_candles: usize = flag_value(args, "--warmup-candles")
        .unwrap_or_else(|| "0".to_string())
        .parse()?;

    let db = Database::open(&db_path).await?;
    // Widen the fetch window backwards by the requested warmup, then count how
    // many returned candles actually precede `from`. Counting (rather than
    // trusting the request) keeps the split correct when the extra history is
    // sparse or entirely absent.
    let fetch_from = from - chrono::Duration::seconds(warmup_candles as i64 * resolution_secs as i64);
    let candles = db
        .tickers()
        .get_aggregated(&product_code, resolution_secs, fetch_from, to, None)
        .await?;
    let actual_warmup = candles.iter().filter(|c| c.open_time < from).count();
    if candles.len() == actual_warmup {
        anyhow::bail!("no candles found for {product_code} in [{from}, {to}] (resolution_secs={resolution_secs})");
    }
    if warmup_candles > 0 {
        eprintln!(
            "warmup: requested {warmup_candles} candles before {from}, got {actual_warmup}; \
             evaluating {} candles in-window",
            candles.len() - actual_warmup
        );
    }

    let bt_config = BacktestConfig {
        product_code,
        from,
        to,
        resolution_secs,
        slippage_pct,
        fee_pct,
        initial_jpy,
        warmup_candles: actual_warmup,
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

/// Compute the canonical `content_hash` of one hypothesis document — the
/// dedup key stored in `experiments/tried.json` (see the "Tried-hypothesis
/// registry" section of `docs/self-improvement-loop.md`).
///
/// The hash covers exactly three fields of the hypothesis — `trading_config`,
/// `kind` and `code_change_summary` — so that renaming a hypothesis file or
/// rewriting its `rationale` does not let an already-tested idea back into
/// the candidate list, while genuinely changing what is being tested does
/// produce a fresh hash.
///
/// # Why this lives in the binary
///
/// The serialization was previously re-derived by hand on every pipeline
/// run, and four consecutive runs (2026-09-07 through 2026-09-08) recorded
/// the same ambiguity: hashes written by one run did not reproduce on the
/// next, so the registry had to be matched by hypothesis *name* instead —
/// exactly the bypass `content_hash` exists to prevent. Pinning it here
/// means the registry and the agent cannot disagree.
///
/// # Canonical form
///
/// `sha256` of the compact JSON encoding of the object
/// `{"code_change_summary": ..., "kind": ..., "trading_config": ...}` with
/// every object key sorted lexicographically (at every nesting depth) and no
/// whitespace between tokens — i.e. Python's
/// `json.dumps(obj, sort_keys=True, separators=(',', ':'))`. A field absent
/// from the hypothesis file is hashed as JSON `null`, so a `kind:
/// "parameter"` hypothesis (which has no `code_change_summary`) hashes the
/// same way whether the field is omitted or explicitly null.
///
/// `serde_json::Value`'s object representation is a `BTreeMap`, so parsing
/// the file already sorts keys at every depth and `to_string` already emits
/// the compact form; the sorting is a property of the type, not something
/// this function re-applies.
pub(crate) fn hypothesis_content_hash(doc: &serde_json::Value) -> String {
    use sha2::{Digest, Sha256};

    let field = |name: &str| doc.get(name).cloned().unwrap_or(serde_json::Value::Null);
    let canonical = serde_json::json!({
        "trading_config": field("trading_config"),
        "kind": field("kind"),
        "code_change_summary": field("code_change_summary"),
    });

    let encoded = serde_json::to_string(&canonical)
        .expect("a serde_json::Value built from Values always serializes");
    format!("{:x}", Sha256::digest(encoded.as_bytes()))
}

/// `shirube hypothesis-hash <file.json> [...]` — print `<hash>  <path>` per
/// file, in the order given, so the pipeline can filter already-tried
/// hypotheses without reimplementing the hash.
fn run_hypothesis_hash(args: &[String]) -> anyhow::Result<()> {
    // Everything after the subcommand is a path; there are no flags.
    let paths = &args[2..];
    if paths.is_empty() {
        anyhow::bail!("usage: shirube hypothesis-hash <hypothesis.json> [<hypothesis.json> ...]");
    }

    for path in paths {
        let raw = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("failed to read {path}: {e}"))?;
        let doc: serde_json::Value = serde_json::from_str(&raw)
            .map_err(|e| anyhow::anyhow!("failed to parse {path} as JSON: {e}"))?;
        println!("{}  {}", hypothesis_content_hash(&doc), path);
    }
    Ok(())
}

/// `shirube backfill-executions` — seed a backtest DB with real bitFlyer
/// OHLCV bars derived from public execution history.
///
/// Replaces the CoinGecko hourly bootstrap the pipeline used previously,
/// which produced degenerate bars (open == high == low == close, constant
/// volume, zero spread). See `backtest::backfill` for the full rationale.
///
/// `--days` defaults to 31 — bitFlyer's entire retention window — which is
/// one day more than the pipeline's 30-day backtest span so indicators have
/// warmup candles ahead of the evaluated range.
async fn run_backfill_executions(args: &[String]) -> anyhow::Result<()> {
    let db_path = flag_value(args, "--db").unwrap_or_else(|| "shirube.db".to_string());
    let product_code = flag_value(args, "--product").unwrap_or_else(|| "BTC_JPY".to_string());
    let days: i64 = flag_value(args, "--days")
        .unwrap_or_else(|| "31".to_string())
        .parse()?;
    let resolution_secs: u32 = flag_value(args, "--resolution-secs")
        .unwrap_or_else(|| "60".to_string())
        .parse()?;

    // `dispatch` runs before main()'s tracing setup so subcommands stay quiet
    // and fast. This one is the exception: it runs for 10+ minutes and can sit
    // in rate-limit backoff, which is indistinguishable from a hang without
    // progress output. Install a stderr subscriber locally so stdout stays
    // clean JSON for the pipeline to parse. Ignore the error when a subscriber
    // is somehow already set — progress logging is not worth failing over.
    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .try_init();

    let db = Database::open(&db_path).await?;
    // Public endpoint — no credentials needed. `backtest::backfill` paces
    // itself well below the client's 200 req/min bucket, because that bucket
    // starts full and would otherwise burst past bitFlyer's public IP limit.
    let client = BitFlyerRestClient::new(String::new(), String::new());

    let stats =
        backfill_executions(&client, &db, &product_code, resolution_secs, days).await?;
    println!("{}", serde_json::to_string_pretty(&stats)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The canonical hash of a minimal parameter hypothesis. The expected
    /// value was produced independently with Python
    /// (`hashlib.sha256(json.dumps({"trading_config": ..., "kind": ...,
    /// "code_change_summary": None}, sort_keys=True,
    /// separators=(',', ':')).encode()).hexdigest()`), so this test is what
    /// keeps the Rust implementation and the registry's historical
    /// serialization in agreement.
    #[test]
    fn hypothesis_content_hash_matches_canonical_serialization() {
        let doc: serde_json::Value = serde_json::from_str(
            r#"{"name":"x","kind":"parameter","rationale":"r","trading_config":{"b":2,"a":1.5}}"#,
        )
        .unwrap();
        assert_eq!(
            hypothesis_content_hash(&doc),
            "2ac13fd2d6b4550edce09f24f93d1af70266abbdc4698d661c07e32ce5968785"
        );
    }

    /// Key order and whitespace in the source file must not affect the hash:
    /// the same hypothesis written two ways is the same hypothesis.
    #[test]
    fn hypothesis_content_hash_ignores_key_order_and_whitespace() {
        let a: serde_json::Value =
            serde_json::from_str(r#"{"kind":"parameter","trading_config":{"a":1,"b":2}}"#).unwrap();
        let b: serde_json::Value = serde_json::from_str(
            "{\n  \"trading_config\": {\n    \"b\": 2,\n    \"a\": 1\n  },\n  \"kind\": \"parameter\"\n}",
        )
        .unwrap();
        assert_eq!(hypothesis_content_hash(&a), hypothesis_content_hash(&b));
    }

    /// Fields the pipeline does not hash (`name`, `rationale`,
    /// `paper_reference`) must not change the hash — renaming a hypothesis
    /// file must never let an already-tried idea back into the candidate
    /// list.
    #[test]
    fn hypothesis_content_hash_ignores_unhashed_fields() {
        let a: serde_json::Value =
            serde_json::from_str(r#"{"name":"one","kind":"parameter","trading_config":{"a":1}}"#)
                .unwrap();
        let b: serde_json::Value = serde_json::from_str(
            r#"{"name":"two","rationale":"different","paper_reference":{"title":"t"},"kind":"parameter","trading_config":{"a":1}}"#,
        )
        .unwrap();
        assert_eq!(hypothesis_content_hash(&a), hypothesis_content_hash(&b));
    }

    /// An omitted `code_change_summary` hashes identically to an explicit
    /// `null`, so a parameter hypothesis cannot get two different hashes
    /// depending on whether the field was written out.
    #[test]
    fn hypothesis_content_hash_treats_missing_field_as_null() {
        let omitted: serde_json::Value =
            serde_json::from_str(r#"{"kind":"parameter","trading_config":{"a":1}}"#).unwrap();
        let explicit: serde_json::Value = serde_json::from_str(
            r#"{"kind":"parameter","trading_config":{"a":1},"code_change_summary":null}"#,
        )
        .unwrap();
        assert_eq!(
            hypothesis_content_hash(&omitted),
            hypothesis_content_hash(&explicit)
        );
    }

    /// Changing what is actually being tested must produce a fresh hash, so
    /// a genuinely new variant is eligible again.
    #[test]
    fn hypothesis_content_hash_changes_with_trading_config() {
        let a: serde_json::Value =
            serde_json::from_str(r#"{"kind":"parameter","trading_config":{"a":1}}"#).unwrap();
        let b: serde_json::Value =
            serde_json::from_str(r#"{"kind":"parameter","trading_config":{"a":2}}"#).unwrap();
        assert_ne!(hypothesis_content_hash(&a), hypothesis_content_hash(&b));
    }
}
