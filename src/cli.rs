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
//!
//!   shirube db-stats --db <path> [--product BTC_JPY]
//!     → prints a JSON object with `rows`, `oldest`, `newest` (RFC3339,
//!       null when empty) and `recommended_backfill_days` for the given
//!       product's stored ticker bars. Replaces the `sqlite3`/`date` shell
//!       logic `scripts/backtest-data.sh` used to use, which is not
//!       available in the self-improvement loop's cloud-routine container.
//!
//!   shirube backtest-data pull --db <path> [--repo <owner/name>] [--tag backtest-data]
//!   shirube backtest-data push --db <path> [--repo <owner/name>] [--tag backtest-data]
//!     → restores/publishes the accumulated backtest DB from/to a GitHub
//!       Release asset via the REST API (see `docs/self-improvement-loop.md`
//!       "Why the DB is carried over"). Replaces `scripts/backtest-data.sh`'s
//!       former dependency on the `gh` CLI, also unavailable in that
//!       container. `pull` prints `BACKFILL_DAYS=<n>` to stdout, exactly as
//!       the shell script did, for the caller to `eval`.

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
use crate::storage::tickers::TickerStats;

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
        Some("db-stats") => {
            run_db_stats(&args).await?;
            Ok(true)
        }
        Some("backtest-data") => {
            match args.get(2).map(String::as_str) {
                Some("pull") => run_backtest_data_pull(&args[2..]).await?,
                Some("push") => run_backtest_data_push(&args[2..]).await?,
                other => anyhow::bail!(
                    "usage: shirube backtest-data <pull|push> --db <path> \
                     [--repo <owner/name>] [--tag backtest-data] (got {other:?})"
                ),
            }
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

    // Reject a config carrying keys `TradingConfig` does not have. serde drops
    // unknown fields silently, which in this pipeline is worse than a crash: a
    // hypothesis whose field was never implemented (or whose name was typo'd)
    // would run the *baseline* config, report "no measurable effect", and be
    // recorded in experiments/tried.json as rejected — a false verdict that the
    // registry then prevents anyone from ever re-testing. This check is
    // deliberately confined to the backtest CLI; `ConfigRepository`'s live load
    // path must stay permissive so an older persisted config still boots.
    let unknown = unknown_config_fields(&trading_config_json)?;
    if !unknown.is_empty() {
        anyhow::bail!(
            "{config_path} has {} field(s) TradingConfig does not define: {}. \
             For a kind:\"algorithm\" hypothesis this means the code change has not been \
             implemented yet — implement it before running the backtest, or the run will \
             silently measure the baseline config instead.",
            unknown.len(),
            unknown.join(", ")
        );
    }

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
    let (candles, actual_warmup) =
        fetch_with_warmup(&db, &product_code, resolution_secs, from, to, warmup_candles).await?;
    if candles.len() == actual_warmup {
        anyhow::bail!("no candles found for {product_code} in [{from}, {to}] (resolution_secs={resolution_secs})");
    }
    if warmup_candles > 0 {
        eprintln!(
            "warmup: requested {warmup_candles} candles before {from}, got {actual_warmup}; \
             evaluating {} candles in-window",
            candles.len() - actual_warmup
        );
        if actual_warmup < warmup_candles {
            eprintln!(
                "warmup: WARNING only {actual_warmup} of {warmup_candles} warmup candles were \
                 available — indicators with a period above {actual_warmup} start the window \
                 unseeded. See warmup_candles_actual in the report JSON."
            );
        }
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
    let mut report = simulator.run(candles, trading_config).await?;
    // Surface the warmup accounting in the report itself, not only on stderr,
    // so `compare-backtest` and the self-improvement loop can see when a run's
    // indicators were under-seeded.
    report.warmup_candles_requested = warmup_candles;
    report.warmup_candles_actual = actual_warmup;
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

/// Names of every key in `config_json` that `TradingConfig` does not define,
/// as dotted paths (e.g. `zone.hold_jpy_bellow`).
///
/// Compares the parsed JSON against a serialized `TradingConfig::default()`
/// rather than a hardcoded list, so the check tracks the struct automatically
/// whenever a field is added or removed. Recurses into nested objects so a
/// typo inside `zone` is caught too.
///
/// NOTE (2026-09-20): added after a run observed that `TradingConfig` has no
/// `#[serde(deny_unknown_fields)]`, so a stale or misspelled key in a
/// hypothesis file is dropped without a word. See the call site in
/// `run_backtest_variant` for why a silent drop is especially damaging here.
fn unknown_config_fields(config_json: &str) -> anyhow::Result<Vec<String>> {
    let actual: serde_json::Value = serde_json::from_str(config_json)?;
    let reference = serde_json::to_value(TradingConfig::default())?;
    let mut out = Vec::new();
    collect_unknown_fields(&actual, &reference, "", &mut out);
    out.sort();
    Ok(out)
}

/// Recursive worker for `unknown_config_fields`.
fn collect_unknown_fields(
    actual: &serde_json::Value,
    reference: &serde_json::Value,
    prefix: &str,
    out: &mut Vec<String>,
) {
    let (Some(actual_map), Some(reference_map)) = (actual.as_object(), reference.as_object())
    else {
        return;
    };
    for (key, value) in actual_map {
        let path = if prefix.is_empty() {
            key.clone()
        } else {
            format!("{prefix}.{key}")
        };
        match reference_map.get(key) {
            None => out.push(path),
            Some(reference_value) => collect_unknown_fields(value, reference_value, &path, out),
        }
    }
}

/// Fetch `[from, to]` plus exactly `warmup_candles` bars preceding `from`.
///
/// Returns the full series (warmup bars first, then the in-window bars) and
/// how many leading bars actually precede `from`.
///
/// NOTE (2026-09-20): this used to widen the fetch window by a fixed
/// `warmup_candles * resolution_secs` of wall-clock time and take whatever
/// came back. That silently under-delivers, because a 1-minute bar exists
/// only for minutes that actually traded — on BTC_JPY roughly 57-80% of
/// minutes do. Measured on the accumulated backtest DB, `--warmup-candles
/// 300` yielded **165** bars, so the baseline config's `sma_period: 200` was
/// never seeded and every run began its window on a `None` SMA, despite
/// 30,421 bars of history sitting in the DB before the window start. The
/// lookback is therefore widened geometrically until enough *bars* (not
/// seconds) precede `from`, and then trimmed back to exactly the requested
/// count so the result does not depend on how many doublings it took.
async fn fetch_with_warmup(
    db: &Database,
    product_code: &str,
    resolution_secs: u32,
    from: chrono::DateTime<chrono::Utc>,
    to: chrono::DateTime<chrono::Utc>,
    warmup_candles: usize,
) -> anyhow::Result<(Vec<crate::types::market::Candle>, usize)> {
    // Bound the search so a DB with no pre-window history terminates promptly
    // rather than widening forever. 12 doublings is a 4096x lookback, far past
    // any plausible bar sparsity.
    const MAX_WIDENINGS: u32 = 12;

    let mut lookback_secs = warmup_candles as i64 * resolution_secs as i64;
    let mut best: Vec<crate::types::market::Candle> = Vec::new();
    let mut best_warmup = 0usize;

    for attempt in 0..=MAX_WIDENINGS {
        let fetch_from = from - chrono::Duration::seconds(lookback_secs);
        let candles = db
            .tickers()
            .get_aggregated(product_code, resolution_secs, fetch_from, to, None)
            .await?;
        let actual_warmup = candles.iter().filter(|c| c.open_time < from).count();

        // A widening that returned no additional history means the DB is
        // exhausted; keep what we have rather than spinning to the cap.
        let exhausted = attempt > 0 && candles.len() == best.len();
        best = candles;
        best_warmup = actual_warmup;

        if warmup_candles == 0 || best_warmup >= warmup_candles || exhausted {
            break;
        }
        lookback_secs = lookback_secs.saturating_mul(2);
    }

    // Trim any surplus so the series is exactly `warmup_candles` bars of
    // history plus the window. Without this the indicator state at the first
    // evaluated candle would depend on the widening path, making runs
    // irreproducible across DBs with different bar density.
    if best_warmup > warmup_candles {
        best.drain(0..(best_warmup - warmup_candles));
        best_warmup = warmup_candles;
    }

    Ok((best, best_warmup))
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

// ---------------------------------------------------------------------------
// db-stats / backtest-data — cloud-routine data bootstrap without `gh`/`sqlite3`
// ---------------------------------------------------------------------------

/// Default GitHub repository for the `backtest-data` release, used when
/// neither `--repo` nor `GITHUB_REPOSITORY` is set.
const DEFAULT_REPO: &str = "SeitaHigashi/shirube";
/// Default tag for the backtest-data release. Deliberately non-semver (see
/// `docs/self-improvement-loop.md` "Why the tag is `backtest-data`") so the
/// auto-updater's `bump_is_greater` check drops it; do not rename.
const DEFAULT_TAG: &str = "backtest-data";
/// Name of the release asset holding the gzip-compressed backtest DB.
const ASSET_NAME: &str = "backtest.db.gz";
const GITHUB_API: &str = "https://api.github.com";
const GITHUB_UPLOADS: &str = "https://uploads.github.com";
const RELEASE_TITLE: &str = "Backtest data (generated)";
const RELEASE_NOTES: &str = "Accumulated bitFlyer BTC_JPY OHLCV bars for the self-improvement loop. Generated data, not a software release — the tag is deliberately non-semver so the auto-updater ignores it. See docs/self-improvement-loop.md.";

/// Days of history missing from a DB whose newest stored bar is `newest`, as
/// of `now`. Mirrors `recommended_days()` from the pre-Rust
/// `scripts/backtest-data.sh` exactly: round the gap up to whole days, add
/// one day of overlap so a backfill always covers the seam rather than
/// stopping just short of it, and clamp to `[MIN_BACKFILL_DAYS,
/// MAX_BACKFILL_DAYS]`. `None` — no stored bars at all, or a DB that could
/// not be read — always recommends the maximum: a full backfill.
fn recommended_backfill_days(newest: Option<DateTime<Utc>>, now: DateTime<Utc>) -> i64 {
    const MIN_BACKFILL_DAYS: i64 = 2;
    const MAX_BACKFILL_DAYS: i64 = 31;

    let Some(newest) = newest else {
        return MAX_BACKFILL_DAYS;
    };

    let gap_secs = now.timestamp() - newest.timestamp();
    // Round the gap up to whole days, matching bash's `(gap + 86399) / 86400`
    // integer truncation, then add one day of overlap.
    let gap_days = (gap_secs + 86399) / 86400 + 1;
    gap_days.clamp(MIN_BACKFILL_DAYS, MAX_BACKFILL_DAYS)
}

/// Resolve the GitHub `owner/repo` slug for the backtest-data release, in
/// precedence order: explicit `--repo` flag, then `GITHUB_REPOSITORY` (set
/// by GitHub Actions and Actions-like environments), then the hardcoded
/// default. Takes the env lookup as a parameter rather than reading it
/// internally so the precedence logic can be unit tested without mutating
/// process-wide environment state.
fn resolve_repo(repo_flag: Option<&str>, github_repository_env: Option<&str>) -> String {
    repo_flag
        .or(github_repository_env)
        .unwrap_or(DEFAULT_REPO)
        .to_string()
}

/// Read the GitHub token from `GITHUB_TOKEN`, falling back to `GH_TOKEN`.
/// Returns a human-readable error naming both variables when neither is set.
fn resolve_github_token() -> Result<String, String> {
    std::env::var("GITHUB_TOKEN")
        .or_else(|_| std::env::var("GH_TOKEN"))
        .map_err(|_| "no GitHub token found: set GITHUB_TOKEN or GH_TOKEN".to_string())
}

/// Build a `reqwest::Client` with the GitHub REST API auth/version headers
/// that every call needs. `Accept` is intentionally left per-request: the
/// JSON endpoints and the raw-asset download endpoint need different values,
/// and a default header here would otherwise be sent alongside them.
fn github_client(token: &str) -> anyhow::Result<reqwest::Client> {
    use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION, USER_AGENT};
    let mut headers = HeaderMap::new();
    headers.insert(
        AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {token}"))?,
    );
    headers.insert(
        "X-GitHub-Api-Version",
        HeaderValue::from_static("2022-11-28"),
    );
    headers.insert(USER_AGENT, HeaderValue::from_static("shirube-backtest-data"));
    Ok(crate::http::client_builder()
        .timeout(std::time::Duration::from_secs(60))
        .default_headers(headers)
        .build()?)
}

/// A GitHub release, trimmed to the fields `backtest-data` needs.
#[derive(serde::Deserialize)]
struct GhRelease {
    id: u64,
    assets: Vec<GhAsset>,
}

#[derive(serde::Deserialize)]
struct GhAsset {
    id: u64,
    name: String,
}

/// `GET /repos/{repo}/releases/tags/{tag}`. Returns `Ok(None)` for a 404
/// (no release at that tag yet — the documented first-run state) rather than
/// an error, since that is an expected, non-fatal outcome for `pull`.
async fn fetch_release(
    client: &reqwest::Client,
    repo: &str,
    tag: &str,
) -> anyhow::Result<Option<GhRelease>> {
    let url = format!("{GITHUB_API}/repos/{repo}/releases/tags/{tag}");
    let resp = client
        .get(&url)
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .send()
        .await?;
    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("GET {url} failed: {status}: {body}");
    }
    Ok(Some(resp.json::<GhRelease>().await?))
}

/// Download one release asset's raw bytes. Needs `Accept:
/// application/octet-stream` — without it the API returns the asset's JSON
/// metadata instead of its content. `reqwest` follows the redirect to the
/// underlying blob store automatically (default policy: up to 10 hops).
async fn download_asset(
    client: &reqwest::Client,
    repo: &str,
    asset_id: u64,
) -> anyhow::Result<Vec<u8>> {
    let url = format!("{GITHUB_API}/repos/{repo}/releases/assets/{asset_id}");
    let resp = client
        .get(&url)
        .header(reqwest::header::ACCEPT, "application/octet-stream")
        .send()
        .await?;
    if !resp.status().is_success() {
        anyhow::bail!("GET {url} failed: {}", resp.status());
    }
    Ok(resp.bytes().await?.to_vec())
}

/// Create the backtest-data release (prerelease, fixed title/notes matching
/// what the shell script used to pass to `gh release create`).
async fn create_release(client: &reqwest::Client, repo: &str, tag: &str) -> anyhow::Result<GhRelease> {
    let url = format!("{GITHUB_API}/repos/{repo}/releases");
    let body = serde_json::json!({
        "tag_name": tag,
        "name": RELEASE_TITLE,
        "body": RELEASE_NOTES,
        "prerelease": true,
    });
    let resp = client
        .post(&url)
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .json(&body)
        .send()
        .await?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        anyhow::bail!("POST {url} failed: {status}: {text}");
    }
    Ok(resp.json::<GhRelease>().await?)
}

/// Delete a release asset by id. There is no REST equivalent of `gh release
/// upload --clobber`: an upload whose name collides with an existing asset
/// is rejected, so the existing one must be deleted first.
async fn delete_asset(client: &reqwest::Client, repo: &str, asset_id: u64) -> anyhow::Result<()> {
    let url = format!("{GITHUB_API}/repos/{repo}/releases/assets/{asset_id}");
    let resp = client.delete(&url).send().await?;
    if !resp.status().is_success() && resp.status() != reqwest::StatusCode::NOT_FOUND {
        anyhow::bail!("DELETE {url} failed: {}", resp.status());
    }
    Ok(())
}

/// Upload `data` as a new asset named `ASSET_NAME` on `release_id`. Uses the
/// `uploads.github.com` host, which the REST API requires for asset uploads
/// (as opposed to `api.github.com` for everything else).
async fn upload_asset(
    client: &reqwest::Client,
    repo: &str,
    release_id: u64,
    data: Vec<u8>,
) -> anyhow::Result<()> {
    let url = format!("{GITHUB_UPLOADS}/repos/{repo}/releases/{release_id}/assets?name={ASSET_NAME}");
    let resp = client
        .post(&url)
        .header(reqwest::header::CONTENT_TYPE, "application/gzip")
        .body(data)
        .send()
        .await?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        anyhow::bail!("POST {url} failed: {status}: {text}");
    }
    Ok(())
}

/// Gzip-decompress `data` fully into memory. The backtest DB is a few MB at
/// most (see `docs/self-improvement-loop.md`), so streaming isn't needed.
fn gunzip(data: &[u8]) -> anyhow::Result<Vec<u8>> {
    use flate2::read::GzDecoder;
    use std::io::Read;
    let mut decoder = GzDecoder::new(data);
    let mut out = Vec::new();
    decoder.read_to_end(&mut out)?;
    Ok(out)
}

/// Gzip-compress `data` at the same level (`-9`, best compression) the shell
/// script's `gzip -9` used.
fn gzip_best(data: &[u8]) -> anyhow::Result<Vec<u8>> {
    use flate2::write::GzEncoder;
    use flate2::Compression;
    use std::io::Write;
    let mut encoder = GzEncoder::new(Vec::new(), Compression::best());
    encoder.write_all(data)?;
    Ok(encoder.finish()?)
}

/// `shirube db-stats` — report row count, time range and recommended
/// backfill window for one product's stored ticker bars.
///
/// Replaces the `sqlite3`/`date` logic `scripts/backtest-data.sh`'s
/// `recommended_days()` used to shell out to, neither of which is available
/// in the self-improvement loop's cloud-routine container (see
/// `docs/self-improvement-loop.md` "Running as a cloud routine: data
/// bootstrap").
async fn run_db_stats(args: &[String]) -> anyhow::Result<()> {
    let db_path = required_flag(args, "--db")?;
    let product_code = flag_value(args, "--product").unwrap_or_else(|| "BTC_JPY".to_string());

    // An unreadable or missing DB degrades to "empty" rather than failing:
    // db-stats runs at the very start of the cloud routine's bootstrap, and
    // a bad/missing file there must not be fatal — it just means "backfill
    // everything", exactly like a fresh checkout with no DB at all.
    let stats = match Database::open(&db_path).await {
        Ok(db) => db
            .tickers()
            .stats(&product_code)
            .await
            .unwrap_or_else(|_| TickerStats::empty()),
        Err(_) => TickerStats::empty(),
    };

    let recommended = recommended_backfill_days(stats.newest, Utc::now());
    let out = serde_json::json!({
        "rows": stats.rows,
        "oldest": stats.oldest.map(|t| t.to_rfc3339()),
        "newest": stats.newest.map(|t| t.to_rfc3339()),
        "recommended_backfill_days": recommended,
    });
    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(())
}

/// `shirube backtest-data pull` — restore the accumulated backtest DB from
/// the GitHub Release asset, in place of `scripts/backtest-data.sh pull`'s
/// former `gh release download | gunzip` pipeline.
///
/// A missing token or a missing release are both treated as the documented
/// first-run case (see `docs/self-improvement-loop.md`): the reason is
/// printed to stderr and `BACKFILL_DAYS=31` is still printed to stdout, so
/// the caller falls back to a full backfill instead of the loop dying.
async fn run_backtest_data_pull(args: &[String]) -> anyhow::Result<()> {
    let db_path = required_flag(args, "--db")?;
    let repo = resolve_repo(
        flag_value(args, "--repo").as_deref(),
        std::env::var("GITHUB_REPOSITORY").ok().as_deref(),
    );
    let tag = flag_value(args, "--tag").unwrap_or_else(|| DEFAULT_TAG.to_string());

    let token = match resolve_github_token() {
        Ok(t) => t,
        Err(reason) => {
            eprintln!("{reason} — first run, full backfill needed");
            println!("BACKFILL_DAYS=31");
            return Ok(());
        }
    };

    let client = github_client(&token)?;
    let release = fetch_release(&client, &repo, &tag).await?;
    let asset = release
        .as_ref()
        .and_then(|r| r.assets.iter().find(|a| a.name == ASSET_NAME));

    let Some(asset) = asset else {
        eprintln!("no stored DB at tag '{tag}' — first run, full backfill needed");
        println!("BACKFILL_DAYS=31");
        return Ok(());
    };

    let compressed = download_asset(&client, &repo, asset.id).await?;
    let decompressed = gunzip(&compressed)?;
    let tmp_path = format!("{db_path}.tmp");
    std::fs::write(&tmp_path, &decompressed)?;
    std::fs::rename(&tmp_path, &db_path)?;
    // A downloaded DB may carry -wal/-shm from the uploader's process; they
    // are not part of the asset, so clear any stale local ones.
    let _ = std::fs::remove_file(format!("{db_path}-wal"));
    let _ = std::fs::remove_file(format!("{db_path}-shm"));

    let db = Database::open(&db_path).await?;
    let stats = db.tickers().stats("BTC_JPY").await?;
    eprintln!(
        "restored {} bars: {} .. {}",
        stats.rows,
        stats.oldest.map(|t| t.to_rfc3339()).unwrap_or_default(),
        stats.newest.map(|t| t.to_rfc3339()).unwrap_or_default(),
    );
    println!(
        "BACKFILL_DAYS={}",
        recommended_backfill_days(stats.newest, Utc::now())
    );
    Ok(())
}

/// `shirube backtest-data push` — checkpoint, VACUUM, gzip and upload the
/// backtest DB to the GitHub Release asset, in place of
/// `scripts/backtest-data.sh push`'s former `sqlite3`/`gh release` pipeline.
///
/// Unlike `pull`, a missing token here is a hard error: pushing is how the
/// accumulated history is preserved, so silently skipping it would lose
/// data instead of merely deferring a backfill.
async fn run_backtest_data_push(args: &[String]) -> anyhow::Result<()> {
    let db_path = required_flag(args, "--db")?;
    let repo = resolve_repo(
        flag_value(args, "--repo").as_deref(),
        std::env::var("GITHUB_REPOSITORY").ok().as_deref(),
    );
    let tag = flag_value(args, "--tag").unwrap_or_else(|| DEFAULT_TAG.to_string());

    if !std::path::Path::new(&db_path).exists() {
        anyhow::bail!("no such DB: {db_path}");
    }
    let token = resolve_github_token().map_err(|reason| anyhow::anyhow!(reason))?;

    // Checkpoint the WAL into the main file, then VACUUM — without this the
    // uploaded asset can miss recently written bars and carries free pages.
    let db = Database::open(&db_path).await?;
    db.checkpoint_and_vacuum().await?;
    let row_count = db.tickers().stats("BTC_JPY").await?.rows;
    drop(db); // release the connection before reading the file's raw bytes below

    let raw = std::fs::read(&db_path)?;
    let compressed = gzip_best(&raw)?;

    let client = github_client(&token)?;
    let release = fetch_release(&client, &repo, &tag).await?;
    let (release_id, existing_asset_id) = match release {
        Some(r) => {
            let asset_id = r.assets.iter().find(|a| a.name == ASSET_NAME).map(|a| a.id);
            (r.id, asset_id)
        }
        None => (create_release(&client, &repo, &tag).await?.id, None),
    };
    if let Some(asset_id) = existing_asset_id {
        delete_asset(&client, &repo, asset_id).await?;
    }
    upload_asset(&client, &repo, release_id, compressed).await?;

    eprintln!("uploaded {row_count} bars to release '{tag}'");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    /// No stored bars at all (or a DB that could not be read) always
    /// recommends the maximum backfill window.
    #[test]
    fn recommended_backfill_days_empty_db_recommends_max() {
        let now = Utc.with_ymd_and_hms(2026, 9, 11, 0, 0, 0).unwrap();
        assert_eq!(recommended_backfill_days(None, now), 31);
    }

    /// A DB whose newest bar is "now" has zero gap; the +1 day overlap still
    /// applies but the floor clamps it to the minimum.
    #[test]
    fn recommended_backfill_days_fresh_db_recommends_min() {
        let now = Utc.with_ymd_and_hms(2026, 9, 11, 12, 0, 0).unwrap();
        assert_eq!(recommended_backfill_days(Some(now), now), 2);
    }

    /// A 10-day-old newest bar recommends 11 days: the 10-day gap rounded up
    /// plus 1 day of overlap.
    #[test]
    fn recommended_backfill_days_ten_day_gap() {
        let now = Utc.with_ymd_and_hms(2026, 9, 11, 0, 0, 0).unwrap();
        let newest = now - chrono::Duration::days(10);
        assert_eq!(recommended_backfill_days(Some(newest), now), 11);
    }

    /// A 60-day gap exceeds bitFlyer's 31-day retention window, so the
    /// recommendation clamps to the maximum rather than requesting more days
    /// than the API can ever return.
    #[test]
    fn recommended_backfill_days_sixty_day_gap_clamped_to_max() {
        let now = Utc.with_ymd_and_hms(2026, 9, 11, 0, 0, 0).unwrap();
        let newest = now - chrono::Duration::days(60);
        assert_eq!(recommended_backfill_days(Some(newest), now), 31);
    }

    /// An explicit `--repo` flag wins over everything else.
    #[test]
    fn resolve_repo_prefers_flag_over_env_and_default() {
        assert_eq!(
            resolve_repo(Some("explicit/repo"), Some("env/repo")),
            "explicit/repo"
        );
    }

    /// Without a flag, `GITHUB_REPOSITORY` (set in Actions-like
    /// environments) is used.
    #[test]
    fn resolve_repo_falls_back_to_env_when_no_flag() {
        assert_eq!(resolve_repo(None, Some("env/repo")), "env/repo");
    }

    /// With neither a flag nor the env var, the hardcoded default applies.
    #[test]
    fn resolve_repo_falls_back_to_default_when_neither_set() {
        assert_eq!(resolve_repo(None, None), DEFAULT_REPO);
    }

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

    /// A config matching `TradingConfig` exactly has no unknown fields.
    #[test]
    fn unknown_config_fields_accepts_the_default_config() {
        let json = serde_json::to_string(&TradingConfig::default()).unwrap();
        assert!(unknown_config_fields(&json).unwrap().is_empty());
    }

    /// Regression test for the 2026-09-20 finding: `TradingConfig` has no
    /// `deny_unknown_fields`, so a hypothesis naming a field that was never
    /// implemented would otherwise run the baseline config and be recorded
    /// as "rejected, no effect".
    #[test]
    fn unknown_config_fields_flags_an_unimplemented_field() {
        let mut v = serde_json::to_value(TradingConfig::default()).unwrap();
        v.as_object_mut()
            .unwrap()
            .insert("signal_damping_factor".into(), serde_json::json!(0.85));
        let found = unknown_config_fields(&v.to_string()).unwrap();
        assert_eq!(found, vec!["signal_damping_factor".to_string()]);
    }

    /// A typo nested inside `zone` is caught with its dotted path, not
    /// silently accepted because the top-level key `zone` exists.
    #[test]
    fn unknown_config_fields_recurses_into_nested_objects() {
        let mut v = serde_json::to_value(TradingConfig::default()).unwrap();
        v.get_mut("zone")
            .unwrap()
            .as_object_mut()
            .unwrap()
            .insert("hold_jpy_bellow".into(), serde_json::json!(0.1));
        let found = unknown_config_fields(&v.to_string()).unwrap();
        assert_eq!(found, vec!["zone.hold_jpy_bellow".to_string()]);
    }

    /// Regression test for the 2026-09-20 warmup defect.
    ///
    /// A 1-minute bar exists only for minutes that actually traded, so a
    /// warmup lookback expressed as `warmup_candles * resolution_secs` of
    /// wall-clock time under-delivers on a sparse tape. Here only every
    /// third minute trades, so the old fixed-span fetch would have returned
    /// roughly a third of the requested warmup; `fetch_with_warmup` must
    /// return exactly the number of bars asked for by widening until it
    /// has them.
    #[tokio::test]
    async fn fetch_with_warmup_delivers_requested_bar_count_on_a_sparse_tape() {
        use crate::types::market::Ticker;
        use rust_decimal_macros::dec;

        let db = crate::storage::db::Database::open_in_memory().await.unwrap();
        let from = Utc.with_ymd_and_hms(2026, 9, 6, 12, 0, 0).unwrap();

        // 600 minutes of history before `from`, but only every third minute
        // actually trades -> 200 bars of real pre-window history.
        // Plus 30 in-window minutes, likewise sparse.
        for i in -600i64..30 {
            if i.rem_euclid(3) != 0 {
                continue;
            }
            let ts = from + chrono::Duration::minutes(i);
            db.tickers()
                .insert(&Ticker {
                    product_code: "BTC_JPY".into(),
                    timestamp: ts,
                    best_bid: dec!(12_000_000),
                    best_ask: dec!(12_000_001),
                    best_bid_size: dec!(1),
                    best_ask_size: dec!(1),
                    ltp: dec!(12_000_000),
                    volume: dec!(1),
                    volume_by_product: dec!(1),
                })
                .await
                .unwrap();
        }

        let to = from + chrono::Duration::minutes(30);

        // The naive fixed-span fetch: 150 minutes back for 150 candles. Only
        // one minute in three traded, so it can never see more than ~50.
        let naive = db
            .tickers()
            .get_aggregated(
                "BTC_JPY",
                60,
                from - chrono::Duration::seconds(150 * 60),
                to,
                None,
            )
            .await
            .unwrap();
        let naive_warmup = naive.iter().filter(|c| c.open_time < from).count();
        assert!(
            naive_warmup < 150,
            "precondition: the fixed-span fetch must under-deliver, got {naive_warmup}"
        );

        // The fix: widen until 150 real bars precede `from`.
        let (candles, warmup) = fetch_with_warmup(&db, "BTC_JPY", 60, from, to, 150)
            .await
            .unwrap();
        assert_eq!(warmup, 150, "warmup must be satisfied by bar count");
        assert_eq!(
            candles.iter().filter(|c| c.open_time < from).count(),
            150,
            "exactly the requested warmup must precede `from`"
        );
        assert!(
            candles.len() > 150,
            "the in-window candles must still be present"
        );
    }

    /// When the DB genuinely lacks enough history, `fetch_with_warmup`
    /// returns what exists rather than widening to the cap or hanging, and
    /// reports the true (short) count so the caller can warn on it.
    #[tokio::test]
    async fn fetch_with_warmup_reports_shortfall_when_history_is_absent() {
        use crate::types::market::Ticker;
        use rust_decimal_macros::dec;

        let db = crate::storage::db::Database::open_in_memory().await.unwrap();
        let from = Utc.with_ymd_and_hms(2026, 9, 6, 12, 0, 0).unwrap();

        // Only 5 bars of pre-window history exist, against a request for 150.
        for i in -5i64..20 {
            let ts = from + chrono::Duration::minutes(i);
            db.tickers()
                .insert(&Ticker {
                    product_code: "BTC_JPY".into(),
                    timestamp: ts,
                    best_bid: dec!(12_000_000),
                    best_ask: dec!(12_000_001),
                    best_bid_size: dec!(1),
                    best_ask_size: dec!(1),
                    ltp: dec!(12_000_000),
                    volume: dec!(1),
                    volume_by_product: dec!(1),
                })
                .await
                .unwrap();
        }

        let to = from + chrono::Duration::minutes(20);
        let (_candles, warmup) = fetch_with_warmup(&db, "BTC_JPY", 60, from, to, 150)
            .await
            .unwrap();
        assert_eq!(warmup, 5, "a real shortfall must be reported, not padded");
    }
}
