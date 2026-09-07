use chrono::{DateTime, Duration, Utc};

use crate::error::Result;
use crate::storage::db::Database;
use crate::types::market::Candle;

/// Historical candles split into a training window and a fixed holdout
/// window, for use in the variant-comparison pipeline.
///
/// `holdout` always covers the most recent `holdout_days` days up to
/// `now`; `train` covers everything before that back to `now -
/// lookback_days`. Variants must only be selected/tuned using `train`;
/// promotion decisions are made by comparing reports computed on
/// `holdout` (see `report::compare`).
pub struct TrainHoldoutSplit {
    pub train: Vec<Candle>,
    pub holdout: Vec<Candle>,
    pub train_range: (DateTime<Utc>, DateTime<Utc>),
    pub holdout_range: (DateTime<Utc>, DateTime<Utc>),
}

/// Load historical candles for `product_code` from the tickers DB and
/// split them into a train/holdout pair.
///
/// `lookback_days` is the total history to load (train + holdout);
/// `holdout_days` is how much of the most recent history to reserve as
/// the holdout period. `lookback_days` must be greater than
/// `holdout_days`.
pub async fn load_train_holdout_split(
    db: &Database,
    product_code: &str,
    resolution_secs: u32,
    lookback_days: i64,
    holdout_days: i64,
) -> Result<TrainHoldoutSplit> {
    assert!(
        lookback_days > holdout_days,
        "lookback_days ({lookback_days}) must exceed holdout_days ({holdout_days})"
    );

    let now = Utc::now();
    let lookback_start = now - Duration::days(lookback_days);
    let holdout_start = now - Duration::days(holdout_days);

    let train = db
        .tickers()
        .get_aggregated(product_code, resolution_secs, lookback_start, holdout_start, None)
        .await?;
    let holdout = db
        .tickers()
        .get_aggregated(product_code, resolution_secs, holdout_start, now, None)
        .await?;

    Ok(TrainHoldoutSplit {
        train,
        holdout,
        train_range: (lookback_start, holdout_start),
        holdout_range: (holdout_start, now),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::market::Ticker;
    use rust_decimal_macros::dec;

    async fn seed_tickers(db: &Database, from_days_ago: i64, to_days_ago: i64) {
        let now = Utc::now();
        let mut t = now - Duration::days(from_days_ago);
        let end = now - Duration::days(to_days_ago);
        while t < end {
            db.tickers()
                .insert(&Ticker {
                    product_code: "BTC_JPY".into(),
                    timestamp: t,
                    best_bid: dec!(9_000_000),
                    best_ask: dec!(9_000_000),
                    best_bid_size: dec!(0.1),
                    best_ask_size: dec!(0.1),
                    ltp: dec!(9_000_000),
                    volume: dec!(1),
                    volume_by_product: dec!(1),
                })
                .await
                .unwrap();
            t += Duration::hours(6);
        }
    }

    #[tokio::test]
    async fn splits_into_train_and_holdout_ranges() {
        let db = Database::open_in_memory().await.unwrap();
        seed_tickers(&db, 30, 0).await;

        let split = load_train_holdout_split(&db, "BTC_JPY", 3600, 30, 14)
            .await
            .unwrap();

        assert!(!split.train.is_empty());
        assert!(!split.holdout.is_empty());
        assert!(split.train_range.1 <= split.holdout_range.0 + Duration::seconds(1));
    }

    #[tokio::test]
    #[should_panic(expected = "lookback_days")]
    async fn panics_when_holdout_not_smaller_than_lookback() {
        let db = Database::open_in_memory().await.unwrap();
        let _ = load_train_holdout_split(&db, "BTC_JPY", 3600, 14, 14).await;
    }
}
