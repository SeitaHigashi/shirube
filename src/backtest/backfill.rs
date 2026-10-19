//! Backfill historical OHLCV bars into the `tickers` table from bitFlyer's
//! public execution history (`GET /v1/getexecutions`).
//!
//! ## Why this exists
//!
//! The variant-validation pipeline (see `docs/self-improvement-loop.md`)
//! used to seed its backtest DB from CoinGecko's hourly market chart. That
//! series is a cross-exchange volume-weighted *aggregate* with no intra-bar
//! detail: every row carried `open == high == low == close`, a constant
//! `volume` of 1, and a zero bid/ask spread. Under that data any hypothesis
//! about range or turnover was mathematically untestable (a volume-weighted
//! MA on constant volume is identically an SMA), and the smoothing inherent
//! in cross-exchange averaging understated realized volatility, inflating
//! backtest Sharpe ratios.
//!
//! Executions are bitFlyer's own trade prints, so bucketing them yields real
//! OHLC and real traded volume for the exact market the bot trades.
//!
//! ## Retention limit
//!
//! bitFlyer serves only the most recent **31 days** of public execution
//! history; older `before` cursors are rejected with
//! `ERR_EXECUTION_HISTORY_LIMIT` (-156). That is a *rolling* window, so
//! history not captured today is unrecoverable tomorrow — this is why the
//! pipeline backfills eagerly rather than on demand.

use std::collections::HashMap;
use std::time::Duration as StdDuration;

use chrono::{DateTime, Duration, Utc};
use rust_decimal::Decimal;
use tracing::{info, warn};

use crate::error::{Error, Result};
use crate::exchange::bitflyer::rest::{
    BitFlyerRestClient, ERR_EXECUTION_HISTORY_LIMIT, MAX_EXECUTIONS_PER_REQUEST,
};
use crate::storage::db::Database;
use crate::types::market::{StoredTicker, Trade};

/// bitFlyer's status for "Over API limit per period, per IP address".
const ERR_OVER_API_LIMIT: i32 = -1;

/// Minimum spacing between backfill requests.
///
/// `BitFlyerRestClient`'s shared token bucket is sized for 200 req/min and
/// starts full, so it happily fires a 200-request burst — fine for the
/// handful of calls live trading makes, fatal for the ~900 sequential calls
/// a 31-day backfill needs. bitFlyer's public IP limit is roughly 500
/// requests per 5 minutes (~100/min), so pace at ~80/min for margin.
const REQUEST_INTERVAL: StdDuration = StdDuration::from_millis(750);

/// Attempts per page before giving up, and the base delay for exponential
/// backoff when bitFlyer reports the IP rate limit.
const MAX_RETRIES: u32 = 6;
const BACKOFF_BASE: StdDuration = StdDuration::from_secs(5);

/// One page of execution history, with `ERR_OVER_API_LIMIT` retried under
/// exponential backoff.
///
/// The retention wall (-156) is deliberately *not* retried: it is a terminal
/// answer about how far history goes, and the caller uses it to stop paging.
async fn fetch_page_with_retry(
    client: &BitFlyerRestClient,
    product_code: &str,
    before: Option<i64>,
) -> Result<Vec<Trade>> {
    let mut attempt = 0;
    loop {
        match client
            .get_public_executions(product_code, MAX_EXECUTIONS_PER_REQUEST, before)
            .await
        {
            Ok(page) => return Ok(page),
            Err(Error::ApiError { code, .. }) if code == ERR_OVER_API_LIMIT => {
                attempt += 1;
                if attempt > MAX_RETRIES {
                    return Err(Error::ApiError {
                        code,
                        message: format!(
                            "still rate-limited after {MAX_RETRIES} retries; \
                             lower the backfill request rate"
                        ),
                    });
                }
                let wait = BACKOFF_BASE * 2u32.pow(attempt - 1);
                warn!(
                    "Rate limited by bitFlyer; backing off {:?} (attempt {}/{})",
                    wait, attempt, MAX_RETRIES
                );
                tokio::time::sleep(wait).await;
            }
            Err(e) => return Err(e),
        }
    }
}

/// Summary of one backfill run, printed as JSON by the
/// `shirube backfill-executions` subcommand.
#[derive(Debug, Clone, serde::Serialize)]
pub struct BackfillStats {
    pub product_code: String,
    pub resolution_secs: u32,
    /// Total executions pulled from the API across all pages.
    pub executions_fetched: u64,
    /// Bars actually written (partial edge buckets excluded).
    pub bars_written: u64,
    /// HTTP requests issued (each returns at most 500 executions).
    pub requests: u32,
    /// Timestamp of the oldest / newest bar written, if any.
    pub oldest_bar: Option<DateTime<Utc>>,
    pub newest_bar: Option<DateTime<Utc>>,
    /// True when paging stopped because bitFlyer refused to serve older
    /// history (status -156) rather than because `days` was satisfied.
    pub hit_history_limit: bool,
}

/// Mutable accumulator for one resolution bucket.
///
/// Executions arrive newest-first, so "open" is the print with the smallest
/// id seen in the bucket and "close" the one with the largest — keying on id
/// rather than arrival order keeps open/close correct no matter which
/// direction the caller pages in.
///
/// NOTE: `volume` is a running sum and is therefore *not* idempotent — each
/// execution must be fed exactly once. The backwards walk guarantees that
/// because `before` is exclusive (`id < before`), so consecutive pages never
/// overlap. Any future change that could replay a page (e.g. retrying with
/// an inclusive cursor) must dedup by id first or it will inflate volume.
struct BarAcc {
    open_id: i64,
    open: Decimal,
    close_id: i64,
    close: Decimal,
    high: Decimal,
    low: Decimal,
    volume: Decimal,
}

impl BarAcc {
    fn new(t: &Trade) -> Self {
        Self {
            open_id: t.id,
            open: t.price,
            close_id: t.id,
            close: t.price,
            high: t.price,
            low: t.price,
            volume: t.size,
        }
    }

    fn update(&mut self, t: &Trade) {
        if t.id < self.open_id {
            self.open_id = t.id;
            self.open = t.price;
        }
        if t.id > self.close_id {
            self.close_id = t.id;
            self.close = t.price;
        }
        if t.price > self.high {
            self.high = t.price;
        }
        if t.price < self.low {
            self.low = t.price;
        }
        self.volume += t.size;
    }
}

/// Floor `ts` to the start of its `resolution_secs` bucket (UTC epoch aligned,
/// matching the `(strftime('%s', timestamp) / res) * res` bucketing that
/// `TickerRepository::get_aggregated` performs when reading back).
fn bucket_start(ts: DateTime<Utc>, resolution_secs: u32) -> DateTime<Utc> {
    let res = resolution_secs as i64;
    let secs = ts.timestamp();
    DateTime::from_timestamp(secs - secs.rem_euclid(res), 0).unwrap_or(ts)
}

/// Page backwards through `product_code`'s execution history for `days` days
/// and write one OHLCV bar per `resolution_secs` bucket into `tickers`.
///
/// Paging stops at the first of: reaching `days` back, an empty page, or
/// bitFlyer's 31-day retention wall (-156, recorded in
/// `BackfillStats::hit_history_limit`).
///
/// Rows are written with `INSERT OR IGNORE`, so re-running against a DB that
/// already holds live-collected ticks will not overwrite them — existing rows
/// win. Bars are keyed by bucket start, matching how `get_aggregated` reads.
///
/// `best_bid` / `best_ask` are set to the bar's close: executions carry no
/// order-book state, and no backtest code path reads bid/ask (the simulator
/// prices fills off `Candle::close` plus a configured slippage).
pub async fn backfill_executions(
    client: &BitFlyerRestClient,
    db: &Database,
    product_code: &str,
    resolution_secs: u32,
    days: i64,
) -> Result<BackfillStats> {
    let now = Utc::now();
    let cutoff = now - Duration::days(days);

    let mut bars: HashMap<i64, BarAcc> = HashMap::new();
    let mut before: Option<i64> = None;
    let mut executions_fetched: u64 = 0;
    let mut requests: u32 = 0;
    let mut hit_history_limit = false;
    // Oldest/newest execution actually seen; used to drop the two partial
    // edge buckets (the run started mid-bucket at both ends).
    let mut oldest_seen: Option<DateTime<Utc>> = None;
    let mut newest_seen: Option<DateTime<Utc>> = None;

    info!(
        "Backfilling {} executions: {} days back (cutoff {}), {}s bars",
        product_code,
        days,
        cutoff.to_rfc3339(),
        resolution_secs
    );

    loop {
        if requests > 0 {
            tokio::time::sleep(REQUEST_INTERVAL).await;
        }
        let page = match fetch_page_with_retry(client, product_code, before).await {
            Ok(p) => p,
            Err(Error::ApiError { code, ref message })
                if code == ERR_EXECUTION_HISTORY_LIMIT =>
            {
                info!("Reached bitFlyer's execution retention wall: {}", message);
                hit_history_limit = true;
                break;
            }
            Err(e) => return Err(e),
        };
        requests += 1;

        if page.is_empty() {
            info!("Empty page after {} requests; history exhausted", requests);
            break;
        }

        executions_fetched += page.len() as u64;
        let mut min_id = i64::MAX;
        let mut page_oldest = page[0].exec_date;

        for t in &page {
            min_id = min_id.min(t.id);
            if t.exec_date < page_oldest {
                page_oldest = t.exec_date;
            }
            newest_seen = Some(match newest_seen {
                Some(n) if n >= t.exec_date => n,
                _ => t.exec_date,
            });
            oldest_seen = Some(match oldest_seen {
                Some(o) if o <= t.exec_date => o,
                _ => t.exec_date,
            });

            let key = bucket_start(t.exec_date, resolution_secs).timestamp();
            bars.entry(key)
                .and_modify(|b| b.update(t))
                .or_insert_with(|| BarAcc::new(t));
        }

        // Progress every ~50 pages (~25k executions) so a multi-minute run
        // is observable in the log rather than silent.
        if requests % 50 == 0 {
            info!(
                "  {} requests, {} executions, {} bars, at {}",
                requests,
                executions_fetched,
                bars.len(),
                page_oldest.to_rfc3339()
            );
        }

        if page_oldest < cutoff {
            info!(
                "Reached cutoff after {} requests ({} executions)",
                requests, executions_fetched
            );
            break;
        }
        before = Some(min_id);
    }

    // Drop the buckets containing the first and last execution seen: both are
    // partial (the walk began and ended mid-bucket), and a partial bar would
    // misreport its open/close and understate its volume.
    let (drop_lo, drop_hi) = match (oldest_seen, newest_seen) {
        (Some(o), Some(n)) => (
            Some(bucket_start(o, resolution_secs).timestamp()),
            Some(bucket_start(n, resolution_secs).timestamp()),
        ),
        _ => (None, None),
    };

    let mut rows: Vec<StoredTicker> = bars
        .into_iter()
        .filter(|(k, _)| Some(*k) != drop_lo && Some(*k) != drop_hi)
        .filter_map(|(k, b)| {
            let ts = DateTime::from_timestamp(k, 0)?;
            Some(StoredTicker {
                product_code: product_code.to_string(),
                timestamp: ts,
                // Executions carry no book state; close is the best available
                // proxy and nothing in the backtest path reads bid/ask.
                best_bid: b.close,
                best_ask: b.close,
                best_bid_size: Decimal::ZERO,
                best_ask_size: Decimal::ZERO,
                ltp_open: b.open,
                ltp: b.close,
                ltp_high: b.high,
                ltp_low: b.low,
                volume: b.volume,
                volume_by_product: b.volume,
            })
        })
        .collect();
    rows.sort_by_key(|r| r.timestamp);

    if rows.is_empty() {
        warn!("Backfill produced no bars (fetched {} executions)", executions_fetched);
    } else {
        db.tickers().insert_stored_batch(&rows).await?;
    }

    let stats = BackfillStats {
        product_code: product_code.to_string(),
        resolution_secs,
        executions_fetched,
        bars_written: rows.len() as u64,
        requests,
        oldest_bar: rows.first().map(|r| r.timestamp),
        newest_bar: rows.last().map(|r| r.timestamp),
        hit_history_limit,
    };
    info!(
        "Backfill done: {} bars from {} executions in {} requests",
        stats.bars_written, stats.executions_fetched, stats.requests
    );
    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::market::TradeSide;
    use chrono::TimeZone;
    use rust_decimal_macros::dec;

    fn trade(id: i64, secs: i64, price: Decimal, size: Decimal) -> Trade {
        Trade {
            id,
            exec_date: Utc.timestamp_opt(secs, 0).unwrap(),
            price,
            size,
            side: TradeSide::Buy,
            buy_child_order_acceptance_id: String::new(),
            sell_child_order_acceptance_id: String::new(),
        }
    }

    #[test]
    fn bucket_start_floors_to_resolution() {
        let ts = Utc.with_ymd_and_hms(2026, 9, 9, 3, 43, 34).unwrap();
        assert_eq!(
            bucket_start(ts, 60),
            Utc.with_ymd_and_hms(2026, 9, 9, 3, 43, 0).unwrap()
        );
        assert_eq!(
            bucket_start(ts, 3600),
            Utc.with_ymd_and_hms(2026, 9, 9, 3, 0, 0).unwrap()
        );
    }

    #[test]
    fn bar_acc_orders_open_close_by_id_not_arrival() {
        // Executions arrive newest-first, so the highest id is fed first;
        // open must still resolve to the lowest-id print.
        let mut acc = BarAcc::new(&trade(30, 0, dec!(300), dec!(1)));
        acc.update(&trade(10, 0, dec!(100), dec!(2)));
        acc.update(&trade(20, 0, dec!(500), dec!(3)));

        assert_eq!(acc.open, dec!(100)); // id=10
        assert_eq!(acc.close, dec!(300)); // id=30
        assert_eq!(acc.high, dec!(500));
        assert_eq!(acc.low, dec!(100));
        assert_eq!(acc.volume, dec!(6));
    }
}
