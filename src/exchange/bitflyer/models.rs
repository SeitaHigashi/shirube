use chrono::{DateTime, NaiveDateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Deserializer, Serialize};

use crate::types::{
    balance::{Balance, Position},
    market::{Ticker, Trade, TradeSide},
    order::{Order, OrderSide, OrderStatus, OrderType},
};

// bitFlyer の timestamp は "2024-01-01T00:00:00.039" 形式 (タイムゾーンなし) で返ってくる
fn deserialize_bitflyer_timestamp<'de, D>(d: D) -> std::result::Result<DateTime<Utc>, D::Error>
where
    D: Deserializer<'de>,
{
    let s = String::deserialize(d)?;
    // まず RFC3339 を試みる（テスト用フィクスチャなど）
    if let Ok(dt) = DateTime::parse_from_rfc3339(&s) {
        return Ok(dt.with_timezone(&Utc));
    }
    // bitFlyer 形式: マイクロ秒あり / なし どちらも試みる
    for fmt in &[
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%dT%H:%M:%S",
    ] {
        if let Ok(naive) = NaiveDateTime::parse_from_str(&s, fmt) {
            return Ok(naive.and_utc());
        }
    }
    Err(serde::de::Error::custom(format!(
        "cannot parse timestamp: {}",
        s
    )))
}

// ---- Ticker ----

#[derive(Debug, Deserialize)]
pub struct RawTicker {
    pub product_code: String,
    #[serde(deserialize_with = "deserialize_bitflyer_timestamp")]
    pub timestamp: DateTime<Utc>,
    pub best_bid: Decimal,
    pub best_ask: Decimal,
    pub best_bid_size: Decimal,
    pub best_ask_size: Decimal,
    pub ltp: Decimal,
    pub volume: Decimal,
    pub volume_by_product: Decimal,
}

impl From<RawTicker> for Ticker {
    fn from(r: RawTicker) -> Self {
        Self {
            product_code: r.product_code,
            timestamp: r.timestamp,
            best_bid: r.best_bid,
            best_ask: r.best_ask,
            best_bid_size: r.best_bid_size,
            best_ask_size: r.best_ask_size,
            ltp: r.ltp,
            volume: r.volume,
            volume_by_product: r.volume_by_product,
        }
    }
}

// ---- Balance ----

#[derive(Debug, Deserialize)]
pub struct RawBalance {
    pub currency_code: String,
    pub amount: Decimal,
    pub available: Decimal,
}

impl From<RawBalance> for Balance {
    fn from(r: RawBalance) -> Self {
        Self {
            currency_code: r.currency_code,
            amount: r.amount,
            available: r.available,
        }
    }
}

// ---- Position ----

#[derive(Debug, Deserialize)]
pub struct RawPosition {
    pub product_code: String,
    pub side: String,
    pub price: Decimal,
    pub size: Decimal,
    pub commission: Decimal,
    pub sfd: Decimal,
}

impl From<RawPosition> for Position {
    fn from(r: RawPosition) -> Self {
        Self {
            product_code: r.product_code,
            side: r.side,
            price: r.price,
            size: r.size,
            commission: r.commission,
            sfd: r.sfd,
        }
    }
}

// ---- Order ----

#[derive(Debug, Deserialize)]
pub struct RawOrder {
    pub id: Option<i64>,
    pub child_order_acceptance_id: String,
    pub product_code: String,
    pub side: String,
    pub child_order_type: String,
    pub price: Option<Decimal>,
    pub size: Decimal,
    pub child_order_state: String,
    #[serde(deserialize_with = "deserialize_bitflyer_timestamp")]
    pub child_order_date: DateTime<Utc>,
}

impl From<RawOrder> for Order {
    fn from(r: RawOrder) -> Self {
        let side = match r.side.as_str() {
            "BUY" => OrderSide::Buy,
            _ => OrderSide::Sell,
        };
        let order_type = match r.child_order_type.as_str() {
            "LIMIT" => OrderType::Limit,
            _ => OrderType::Market,
        };
        let status = match r.child_order_state.as_str() {
            "ACTIVE" => OrderStatus::Active,
            "COMPLETED" => OrderStatus::Completed,
            "CANCELED" => OrderStatus::Canceled,
            "EXPIRED" => OrderStatus::Expired,
            _ => OrderStatus::Rejected,
        };
        Self {
            id: r.id,
            acceptance_id: r.child_order_acceptance_id,
            product_code: r.product_code,
            side,
            order_type,
            price: r.price,
            size: r.size,
            status,
            created_at: r.child_order_date,
            updated_at: r.child_order_date,
        }
    }
}

// ---- Send order request / response ----

#[derive(Debug, Serialize)]
pub struct SendOrderRequest {
    pub product_code: String,
    pub child_order_type: String,
    pub side: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub price: Option<Decimal>,
    pub size: Decimal,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub minute_to_expire: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub time_in_force: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct SendOrderResponse {
    pub child_order_acceptance_id: String,
}

// ---- Cancel all orders request ----

#[derive(Debug, Serialize)]
pub struct CancelAllOrdersRequest {
    pub product_code: String,
}

// ---- WS execution ----

#[derive(Debug, Deserialize)]
pub struct RawExecution {
    pub id: i64,
    #[serde(deserialize_with = "deserialize_bitflyer_timestamp")]
    pub exec_date: DateTime<Utc>,
    pub price: Decimal,
    pub size: Decimal,
    pub side: String,
    pub buy_child_order_acceptance_id: String,
    pub sell_child_order_acceptance_id: String,
}

impl From<RawExecution> for Trade {
    fn from(r: RawExecution) -> Self {
        let side = match r.side.as_str() {
            "BUY" => TradeSide::Buy,
            _ => TradeSide::Sell,
        };
        Self {
            id: r.id,
            exec_date: r.exec_date,
            price: r.price,
            size: r.size,
            side,
            buy_child_order_acceptance_id: r.buy_child_order_acceptance_id,
            sell_child_order_acceptance_id: r.sell_child_order_acceptance_id,
        }
    }
}

// ---- API error ----

#[derive(Debug, Deserialize)]
pub struct ApiErrorBody {
    pub status: Option<i32>,
    pub error_message: Option<String>,
    pub data: Option<serde_json::Value>,
}
