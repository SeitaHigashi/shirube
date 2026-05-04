use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::types::order::OrderSide;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Ticker {
    pub product_code: String,
    pub timestamp: DateTime<Utc>,
    pub best_bid: Decimal,
    pub best_ask: Decimal,
    pub best_bid_size: Decimal,
    pub best_ask_size: Decimal,
    pub ltp: Decimal,
    pub volume: Decimal,
    pub volume_by_product: Decimal,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Trade {
    pub id: i64,
    pub exec_date: DateTime<Utc>,
    pub price: Decimal,
    pub size: Decimal,
    pub side: TradeSide,
    pub buy_child_order_acceptance_id: String,
    pub sell_child_order_acceptance_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "UPPERCASE")]
pub enum TradeSide {
    Buy,
    Sell,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Candle {
    pub product_code: String,
    pub open_time: DateTime<Utc>,
    pub resolution_secs: u32,
    pub open: Decimal,
    pub high: Decimal,
    pub low: Decimal,
    pub close: Decimal,
    pub volume: Decimal,
}

/// DB に保存された Ticker。生ティックでは ltp_open = ltp = ltp_high = ltp_low。
/// ダウンサンプリング後はバケット内の OHLC が集約される。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredTicker {
    pub product_code: String,
    /// バケット開始時刻（生ティックはティック自身の timestamp）
    pub timestamp: DateTime<Utc>,
    pub best_bid: Decimal,
    pub best_ask: Decimal,
    pub best_bid_size: Decimal,
    pub best_ask_size: Decimal,
    /// バケット内の始値（最初の ltp）
    pub ltp_open: Decimal,
    /// バケット内の終値（最後の ltp）
    pub ltp: Decimal,
    /// バケット内の高値
    pub ltp_high: Decimal,
    /// バケット内の安値
    pub ltp_low: Decimal,
    pub volume: Decimal,
    pub volume_by_product: Decimal,
}

/// 自分の約定履歴（`GET /v1/me/getexecutions` レスポンス）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MyExecution {
    pub id: i64,
    pub exec_date: DateTime<Utc>,
    pub side: OrderSide,
    pub price: Decimal,
    pub size: Decimal,
    pub commission: Decimal,
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use rust_decimal_macros::dec;

    #[test]
    fn ticker_json_round_trip() {
        let ticker = Ticker {
            product_code: "BTC_JPY".to_string(),
            timestamp: Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap(),
            best_bid: dec!(9000000),
            best_ask: dec!(9001000),
            best_bid_size: dec!(0.1),
            best_ask_size: dec!(0.2),
            ltp: dec!(9000500),
            volume: dec!(1234.5),
            volume_by_product: dec!(1234.5),
        };
        let json = serde_json::to_string(&ticker).unwrap();
        let decoded: Ticker = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.product_code, ticker.product_code);
        assert_eq!(decoded.best_bid, ticker.best_bid);
        assert_eq!(decoded.ltp, ticker.ltp);
    }

    #[test]
    fn trade_side_serialization() {
        let side = TradeSide::Buy;
        let json = serde_json::to_string(&side).unwrap();
        assert_eq!(json, "\"BUY\"");
        let decoded: TradeSide = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, TradeSide::Buy);
    }

    #[test]
    fn candle_json_round_trip() {
        let candle = Candle {
            product_code: "BTC_JPY".to_string(),
            open_time: Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap(),
            resolution_secs: 60,
            open: dec!(9000000),
            high: dec!(9010000),
            low: dec!(8990000),
            close: dec!(9005000),
            volume: dec!(10.5),
        };
        let json = serde_json::to_string(&candle).unwrap();
        let decoded: Candle = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.open, candle.open);
        assert_eq!(decoded.high, candle.high);
    }
}
