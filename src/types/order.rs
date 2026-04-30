use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "UPPERCASE")]
pub enum OrderSide {
    Buy,
    Sell,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "UPPERCASE")]
pub enum OrderType {
    Limit,
    Market,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "UPPERCASE")]
pub enum OrderStatus {
    Active,
    Completed,
    Canceled,
    Expired,
    Rejected,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Order {
    pub id: Option<i64>,
    pub acceptance_id: String,
    pub product_code: String,
    pub side: OrderSide,
    pub order_type: OrderType,
    pub price: Option<Decimal>,
    pub size: Decimal,
    pub status: OrderStatus,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct OrderRequest {
    pub product_code: String,
    pub side: OrderSide,
    pub order_type: OrderType,
    pub price: Option<Decimal>,
    pub size: Decimal,
    pub minute_to_expire: Option<u32>,
    pub time_in_force: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use rust_decimal_macros::dec;

    #[test]
    fn order_side_serialization() {
        assert_eq!(serde_json::to_string(&OrderSide::Buy).unwrap(), "\"BUY\"");
        assert_eq!(serde_json::to_string(&OrderSide::Sell).unwrap(), "\"SELL\"");
    }

    #[test]
    fn order_type_serialization() {
        assert_eq!(serde_json::to_string(&OrderType::Limit).unwrap(), "\"LIMIT\"");
        assert_eq!(serde_json::to_string(&OrderType::Market).unwrap(), "\"MARKET\"");
    }

    #[test]
    fn order_status_serialization() {
        assert_eq!(
            serde_json::to_string(&OrderStatus::Active).unwrap(),
            "\"ACTIVE\""
        );
        assert_eq!(
            serde_json::to_string(&OrderStatus::Completed).unwrap(),
            "\"COMPLETED\""
        );
    }

    #[test]
    fn order_json_round_trip() {
        let now = Utc::now();
        let order = Order {
            id: Some(1),
            acceptance_id: "JRF20240101-000000-000000".to_string(),
            product_code: "BTC_JPY".to_string(),
            side: OrderSide::Buy,
            order_type: OrderType::Limit,
            price: Some(dec!(9000000)),
            size: dec!(0.001),
            status: OrderStatus::Active,
            created_at: now,
            updated_at: now,
        };
        let json = serde_json::to_string(&order).unwrap();
        let decoded: Order = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.acceptance_id, order.acceptance_id);
        assert_eq!(decoded.side, order.side);
        assert_eq!(decoded.price, order.price);
    }
}
