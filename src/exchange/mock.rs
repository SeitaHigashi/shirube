use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use chrono::Utc;
use rust_decimal_macros::dec;

use crate::error::Result;
use crate::exchange::ExchangeClient;
use crate::types::{
    balance::{Balance, Position},
    market::Ticker,
    order::{Order, OrderRequest, OrderSide, OrderStatus, OrderType},
};

pub struct MockExchangeClient {
    ticker: Arc<RwLock<Ticker>>,
    balances: Arc<RwLock<Vec<Balance>>>,
    orders: Arc<RwLock<Vec<Order>>>,
    order_counter: Arc<AtomicU64>,
}

impl Default for MockExchangeClient {
    fn default() -> Self {
        Self::new()
    }
}

impl MockExchangeClient {
    pub fn new() -> Self {
        let now = Utc::now();
        let ticker = Ticker {
            product_code: "BTC_JPY".to_string(),
            timestamp: now,
            best_bid: dec!(9000000),
            best_ask: dec!(9001000),
            best_bid_size: dec!(0.1),
            best_ask_size: dec!(0.1),
            ltp: dec!(9000500),
            volume: dec!(100.0),
            volume_by_product: dec!(100.0),
        };
        let balances = vec![
            Balance {
                currency_code: "JPY".to_string(),
                amount: dec!(1000000),
                available: dec!(1000000),
            },
            Balance {
                currency_code: "BTC".to_string(),
                amount: dec!(0),
                available: dec!(0),
            },
        ];
        Self {
            ticker: Arc::new(RwLock::new(ticker)),
            balances: Arc::new(RwLock::new(balances)),
            orders: Arc::new(RwLock::new(Vec::new())),
            order_counter: Arc::new(AtomicU64::new(1)),
        }
    }

    pub fn set_ticker(&self, ticker: Ticker) {
        *self.ticker.write().unwrap() = ticker;
    }

    pub fn set_balances(&self, balances: Vec<Balance>) {
        *self.balances.write().unwrap() = balances;
    }

    pub fn placed_orders(&self) -> Vec<Order> {
        self.orders.read().unwrap().clone()
    }
}

#[async_trait]
impl ExchangeClient for MockExchangeClient {
    async fn get_ticker(&self, product_code: &str) -> Result<Ticker> {
        let mut ticker = self.ticker.read().unwrap().clone();
        ticker.product_code = product_code.to_string();
        ticker.timestamp = Utc::now();
        Ok(ticker)
    }

    async fn get_balance(&self) -> Result<Vec<Balance>> {
        Ok(self.balances.read().unwrap().clone())
    }

    async fn get_positions(&self, _product_code: &str) -> Result<Vec<Position>> {
        Ok(vec![])
    }

    async fn send_order(&self, req: &OrderRequest) -> Result<String> {
        let id = self.order_counter.fetch_add(1, Ordering::SeqCst);
        let acceptance_id = format!("MOCK-{:06}", id);
        let now = Utc::now();
        let order = Order {
            id: Some(id as i64),
            acceptance_id: acceptance_id.clone(),
            product_code: req.product_code.clone(),
            side: req.side.clone(),
            order_type: req.order_type.clone(),
            price: req.price,
            size: req.size,
            status: OrderStatus::Active,
            created_at: now,
            updated_at: now,
        };
        self.orders.write().unwrap().push(order);
        Ok(acceptance_id)
    }

    async fn cancel_all_orders(&self, _product_code: &str) -> Result<()> {
        let mut orders = self.orders.write().unwrap();
        for order in orders.iter_mut() {
            if order.status == OrderStatus::Active {
                order.status = OrderStatus::Canceled;
                order.updated_at = Utc::now();
            }
        }
        Ok(())
    }

    async fn get_orders(
        &self,
        product_code: &str,
        status: Option<&str>,
        count: Option<u32>,
    ) -> Result<Vec<Order>> {
        let orders = self.orders.read().unwrap();
        let mut result: Vec<Order> = orders
            .iter()
            .filter(|o| o.product_code == product_code)
            .filter(|o| {
                if let Some(s) = status {
                    let order_status = match &o.status {
                        OrderStatus::Active => "ACTIVE",
                        OrderStatus::Completed => "COMPLETED",
                        OrderStatus::Canceled => "CANCELED",
                        OrderStatus::Expired => "EXPIRED",
                        OrderStatus::Rejected => "REJECTED",
                    };
                    order_status == s
                } else {
                    true
                }
            })
            .cloned()
            .collect();
        if let Some(c) = count {
            result.truncate(c as usize);
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::order::{OrderRequest, OrderSide, OrderType};
    use rust_decimal_macros::dec;

    #[tokio::test]
    async fn mock_get_ticker_returns_default() {
        let client = MockExchangeClient::new();
        let ticker = client.get_ticker("BTC_JPY").await.unwrap();
        assert_eq!(ticker.product_code, "BTC_JPY");
        assert_eq!(ticker.ltp, dec!(9000500));
    }

    #[tokio::test]
    async fn mock_send_order_records_it() {
        let client = MockExchangeClient::new();
        let req = OrderRequest {
            product_code: "BTC_JPY".to_string(),
            side: OrderSide::Buy,
            order_type: OrderType::Limit,
            price: Some(dec!(9000000)),
            size: dec!(0.001),
            minute_to_expire: None,
            time_in_force: None,
        };
        let id = client.send_order(&req).await.unwrap();
        assert!(id.starts_with("MOCK-"));

        let orders = client.placed_orders();
        assert_eq!(orders.len(), 1);
        assert_eq!(orders[0].acceptance_id, id);
        assert_eq!(orders[0].status, OrderStatus::Active);
    }

    #[tokio::test]
    async fn mock_cancel_all_orders_cancels_active() {
        let client = MockExchangeClient::new();
        let req = OrderRequest {
            product_code: "BTC_JPY".to_string(),
            side: OrderSide::Buy,
            order_type: OrderType::Market,
            price: None,
            size: dec!(0.001),
            minute_to_expire: None,
            time_in_force: None,
        };
        client.send_order(&req).await.unwrap();
        client.cancel_all_orders("BTC_JPY").await.unwrap();

        let orders = client.placed_orders();
        assert_eq!(orders[0].status, OrderStatus::Canceled);
    }
}
