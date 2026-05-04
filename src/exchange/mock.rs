use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use chrono::Utc;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

use crate::error::{Error, Result};
use crate::exchange::ExchangeClient;
use crate::storage::mock_state::MockStateRepository;
use crate::types::{
    balance::{Balance, Position},
    market::{MyExecution, Ticker},
    order::{Order, OrderRequest, OrderSide, OrderStatus, OrderType},
};

// ── 約定履歴 ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct FilledTrade {
    pub side: OrderSide,
    pub price: Decimal,
    pub size: Decimal,
    pub fee: Decimal,
}

// ── bitFlyer Lightning 現物 手数料ティアテーブル ──────────────────────────────
//
// 直近30日取引量（JPY）→ 手数料率
// 公式: https://bitflyer.com/ja-jp/s/commission
const FEE_TIERS: &[(u64, f64)] = &[
    (500_000_000, 0.0001), // 5億円以上
    (100_000_000, 0.0002), // 1〜5億円
    (50_000_000,  0.0003), // 5000万〜1億円
    (20_000_000,  0.0005), // 2000万〜5000万円
    (10_000_000,  0.0007), // 1000万〜2000万円
    (5_000_000,   0.0009), // 500万〜1000万円
    (2_000_000,   0.0010), // 200万〜500万円
    (1_000_000,   0.0011), // 100万〜200万円
    (500_000,     0.0012), // 50万〜100万円
    (200_000,     0.0013), // 20万〜50万円
    (100_000,     0.0014), // 10万〜20万円
    (0,           0.0015), // 10万円未満
];

fn fee_from_volume(volume_jpy: Decimal) -> f64 {
    use rust_decimal::prelude::ToPrimitive;
    let vol = volume_jpy.to_u64().unwrap_or(0);
    for &(threshold, rate) in FEE_TIERS {
        if vol >= threshold {
            return rate;
        }
    }
    0.0015
}

// ── MockExchangeClient ────────────────────────────────────────────────────────

pub struct MockExchangeClient {
    ticker: Arc<RwLock<Ticker>>,
    balances: Arc<RwLock<Vec<Balance>>>,
    orders: Arc<RwLock<Vec<Order>>>,
    filled_trades: Arc<RwLock<Vec<FilledTrade>>>,
    order_counter: Arc<AtomicU64>,
    /// テスト用に手数料率を固定する場合は Some(rate) を指定する。
    /// None の場合は累計取引量に基づいてティア制手数料を計算する。
    override_fee: Option<f64>,
    /// 累計JPY建て取引量（ティア制手数料の計算に使用）
    volume_jpy: Arc<RwLock<Decimal>>,
    db: Option<Arc<MockStateRepository>>,
}

impl Default for MockExchangeClient {
    fn default() -> Self {
        Self::new()
    }
}

impl MockExchangeClient {
    /// ティア制手数料（累計取引量ベース）でクライアントを生成する。
    pub fn new() -> Self {
        Self::new_inner(None)
    }

    /// 手数料率を固定値で指定してクライアントを生成する（テスト用）。
    pub fn with_fee(fee_pct: f64) -> Self {
        Self::new_inner(Some(fee_pct))
    }

    fn new_inner(override_fee: Option<f64>) -> Self {
        let now = Utc::now();
        let ticker = Ticker {
            product_code: "BTC_JPY".to_string(),
            timestamp: now,
            best_bid: dec!(9_000_000),
            best_ask: dec!(9_001_000),
            best_bid_size: dec!(0.1),
            best_ask_size: dec!(0.1),
            ltp: dec!(9_000_500),
            volume: dec!(100.0),
            volume_by_product: dec!(100.0),
        };
        let balances = vec![
            Balance {
                currency_code: "JPY".to_string(),
                amount: dec!(1_000_000),
                available: dec!(1_000_000),
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
            filled_trades: Arc::new(RwLock::new(Vec::new())),
            order_counter: Arc::new(AtomicU64::new(1)),
            override_fee,
            volume_jpy: Arc::new(RwLock::new(Decimal::ZERO)),
            db: None,
        }
    }

    /// DBリポジトリを渡してDB永続化を有効にしたクライアントを生成する。
    /// 既存の残高・注文カウンター・約定履歴をDBから復元する。
    /// 手数料はティア制（volume_jpy ベース）を使用する。
    pub async fn new_with_db(repo: MockStateRepository) -> Result<Self> {
        let mut client = Self::new();

        let saved_balances = repo.load_balances().await?;
        if !saved_balances.is_empty() {
            client.set_balances(saved_balances);
        }

        let counter = repo.load_order_counter().await?;
        client.order_counter.store(counter, Ordering::SeqCst);

        let trades = repo.load_filled_trades().await?;
        // 累計取引量を約定履歴から再計算してティア制手数料を正しく復元する
        let volume: Decimal = trades.iter().map(|t| t.price * t.size).sum();
        *client.volume_jpy.write().unwrap() = volume;
        *client.filled_trades.write().unwrap() = trades;

        client.db = Some(Arc::new(repo));
        Ok(client)
    }

    /// ticker の ltp / bid / ask を一括更新する（バックテスト用）。
    pub fn set_price(&self, price: Decimal) {
        let mut t = self.ticker.write().unwrap();
        t.ltp = price;
        t.best_bid = price;
        t.best_ask = price;
        t.timestamp = Utc::now();
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

    pub fn filled_trades(&self) -> Vec<FilledTrade> {
        self.filled_trades.read().unwrap().clone()
    }

    pub fn jpy_balance(&self) -> Decimal {
        self.balances
            .read()
            .unwrap()
            .iter()
            .find(|b| b.currency_code == "JPY")
            .map(|b| b.amount)
            .unwrap_or(Decimal::ZERO)
    }

    pub fn btc_balance(&self) -> Decimal {
        self.balances
            .read()
            .unwrap()
            .iter()
            .find(|b| b.currency_code == "BTC")
            .map(|b| b.amount)
            .unwrap_or(Decimal::ZERO)
    }

    fn update_balance(&self, currency: &str, delta: Decimal) -> Result<()> {
        let mut balances = self.balances.write().unwrap();
        let bal = balances
            .iter_mut()
            .find(|b| b.currency_code == currency)
            .ok_or_else(|| Error::Other(anyhow::anyhow!("currency not found: {currency}")))?;
        let new_amount = bal.amount + delta;
        if new_amount < Decimal::ZERO {
            return Err(Error::Other(anyhow::anyhow!(
                "insufficient {currency} balance"
            )));
        }
        bal.amount = new_amount;
        bal.available = new_amount;
        Ok(())
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
        let exec_price = self.ticker.read().unwrap().ltp;
        if exec_price == Decimal::ZERO {
            return Err(Error::Other(anyhow::anyhow!("current price not set")));
        }

        let cost = exec_price * req.size;
        let fee_rate = Decimal::try_from(self.fee_pct()).unwrap_or(Decimal::ZERO);
        let fee = cost * fee_rate;

        // 累計取引量を加算（ティア制手数料の計算に使用）
        if self.override_fee.is_none() {
            *self.volume_jpy.write().unwrap() += cost;
        }

        match req.side {
            OrderSide::Buy => {
                let total = cost + fee;
                // JPY の残高チェックと減算
                self.update_balance("JPY", -total)?;
                self.update_balance("BTC", req.size)?;
            }
            OrderSide::Sell => {
                // BTC の残高チェックと減算
                self.update_balance("BTC", -req.size)?;
                self.update_balance("JPY", cost - fee)?;
            }
        }

        let trade = FilledTrade {
            side: req.side.clone(),
            price: exec_price,
            size: req.size,
            fee,
        };
        self.filled_trades.write().unwrap().push(trade.clone());

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
            status: OrderStatus::Completed,
            created_at: now,
            updated_at: now,
        };
        self.orders.write().unwrap().push(order);

        // DB が設定されている場合は状態を永続化する（失敗しても注文自体は成功扱い）
        if let Some(db) = &self.db {
            let balances = self.balances.read().unwrap().clone();
            let next_counter = id + 1;
            if let Err(e) = db.save_balances(&balances).await {
                tracing::warn!("MockExchangeClient: failed to save balances to DB: {}", e);
            }
            if let Err(e) = db.save_order_counter(next_counter).await {
                tracing::warn!("MockExchangeClient: failed to save order counter to DB: {}", e);
            }
            if let Err(e) = db.insert_filled_trade(&trade).await {
                tracing::warn!("MockExchangeClient: failed to save filled trade to DB: {}", e);
            }
        }

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

    async fn cancel_order(&self, _product_code: &str, acceptance_id: &str) -> Result<()> {
        let mut orders = self.orders.write().unwrap();
        let order = orders
            .iter_mut()
            .find(|o| o.acceptance_id == acceptance_id)
            .ok_or_else(|| {
                Error::Other(anyhow::anyhow!("order not found: {acceptance_id}"))
            })?;
        if order.status != OrderStatus::Active {
            return Err(Error::Other(anyhow::anyhow!(
                "order {} is not active (status: {:?})",
                acceptance_id,
                order.status
            )));
        }
        order.status = OrderStatus::Canceled;
        order.updated_at = Utc::now();
        Ok(())
    }

    async fn get_executions(
        &self,
        _product_code: &str,
        count: Option<u32>,
        before: Option<i64>,
        after: Option<i64>,
    ) -> Result<Vec<MyExecution>> {
        let trades = self.filled_trades.read().unwrap();
        let mut result: Vec<MyExecution> = trades
            .iter()
            .enumerate()
            .map(|(i, t)| MyExecution {
                id: i as i64 + 1,
                exec_date: Utc::now(),
                side: t.side.clone(),
                price: t.price,
                size: t.size,
                commission: t.fee,
            })
            .filter(|e| before.map_or(true, |b| e.id < b))
            .filter(|e| after.map_or(true, |a| e.id > a))
            .collect();
        if let Some(c) = count {
            result.truncate(c as usize);
        }
        Ok(result)
    }

    async fn get_trading_commission(&self, _product_code: &str) -> Result<f64> {
        Ok(self.fee_pct())
    }

    fn fee_pct(&self) -> f64 {
        if let Some(rate) = self.override_fee {
            return rate;
        }
        fee_from_volume(*self.volume_jpy.read().unwrap())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::order::{OrderRequest, OrderSide, OrderType};
    use rust_decimal_macros::dec;

    fn buy_req(size: Decimal) -> OrderRequest {
        OrderRequest {
            product_code: "BTC_JPY".to_string(),
            side: OrderSide::Buy,
            order_type: OrderType::Market,
            price: None,
            size,
            minute_to_expire: None,
            time_in_force: None,
        }
    }

    fn sell_req(size: Decimal) -> OrderRequest {
        OrderRequest {
            product_code: "BTC_JPY".to_string(),
            side: OrderSide::Sell,
            order_type: OrderType::Market,
            price: None,
            size,
            minute_to_expire: None,
            time_in_force: None,
        }
    }

    #[tokio::test]
    async fn mock_get_ticker_returns_default() {
        let client = MockExchangeClient::new();
        let ticker = client.get_ticker("BTC_JPY").await.unwrap();
        assert_eq!(ticker.product_code, "BTC_JPY");
        assert_eq!(ticker.ltp, dec!(9_000_500));
    }

    #[tokio::test]
    async fn buy_order_deducts_jpy_adds_btc() {
        let client = MockExchangeClient::with_fee(0.0); // 手数料なし
        client.set_price(dec!(9_000_000));

        client.send_order(&buy_req(dec!(0.001))).await.unwrap();

        // 9_000_000 * 0.001 = 9_000 JPY
        assert_eq!(client.jpy_balance(), dec!(991_000));
        assert_eq!(client.btc_balance(), dec!(0.001));
    }

    #[tokio::test]
    async fn sell_order_adds_jpy_deducts_btc() {
        let client = MockExchangeClient::with_fee(0.0);
        client.set_price(dec!(9_000_000));

        client.send_order(&buy_req(dec!(0.001))).await.unwrap();
        client.send_order(&sell_req(dec!(0.001))).await.unwrap();

        // 同価格で売り → JPY は元に戻る（手数料0のため）
        assert_eq!(client.jpy_balance(), dec!(1_000_000));
        assert_eq!(client.btc_balance(), dec!(0));
    }

    #[tokio::test]
    async fn fee_reduces_balance() {
        let client = MockExchangeClient::with_fee(0.0015); // 0.15%
        client.set_price(dec!(9_000_000));

        client.send_order(&buy_req(dec!(0.001))).await.unwrap();

        // cost = 9_000, fee = 9_000 * 0.0015 = 13.5 → total = 9_013.5
        let expected_jpy = dec!(1_000_000) - dec!(9_013.5);
        assert_eq!(client.jpy_balance(), expected_jpy);
        assert_eq!(client.btc_balance(), dec!(0.001));

        let trades = client.filled_trades();
        assert_eq!(trades.len(), 1);
        assert_eq!(trades[0].fee, dec!(13.5));
    }

    #[tokio::test]
    async fn insufficient_jpy_returns_error() {
        let client = MockExchangeClient::with_fee(0.0);
        client.set_price(dec!(9_000_000));
        // 初期JPY=1_000_000、0.2BTC買おうとすると 1_800_000 JPY 必要
        let result = client.send_order(&buy_req(dec!(0.2))).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn insufficient_btc_returns_error() {
        let client = MockExchangeClient::with_fee(0.0);
        client.set_price(dec!(9_000_000));
        // BTC残高0のまま売ろうとする
        let result = client.send_order(&sell_req(dec!(0.001))).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn tiered_fee_starts_at_max() {
        // 取引量ゼロ → 最高手数料ティア (0.15%)
        let client = MockExchangeClient::new();
        assert_eq!(client.fee_pct(), 0.0015);
    }

    #[tokio::test]
    async fn tiered_fee_decreases_with_volume() {
        // 取引量が増えるにつれ手数料が下がることを確認
        let client = MockExchangeClient::new();
        // 初期: 0.15% (10万円未満)
        assert_eq!(client.fee_pct(), 0.0015);

        // 初期残高を増やしてから取引
        client.set_balances(vec![
            crate::types::balance::Balance {
                currency_code: "JPY".to_string(),
                amount: dec!(5_000_000),
                available: dec!(5_000_000),
            },
            crate::types::balance::Balance {
                currency_code: "BTC".to_string(),
                amount: dec!(0),
                available: dec!(0),
            },
        ]);
        // 価格を 1,000,000 JPY に設定し、1 BTC 取引 → 累計 1,000,000 JPY
        client.set_price(dec!(1_000_000));
        client
            .send_order(&buy_req(dec!(1)))
            .await
            .unwrap();
        // 累計100万円 → 0.11% ティア
        assert_eq!(client.fee_pct(), 0.0011);
    }

    #[tokio::test]
    async fn cancel_order_cancels_single() {
        let client = MockExchangeClient::new();
        // Active な注文を2件手動挿入
        {
            let mut orders = client.orders.write().unwrap();
            for (i, id) in ["MOCK-A", "MOCK-B"].iter().enumerate() {
                orders.push(Order {
                    id: Some(i as i64 + 1),
                    acceptance_id: id.to_string(),
                    product_code: "BTC_JPY".to_string(),
                    side: OrderSide::Buy,
                    order_type: crate::types::order::OrderType::Limit,
                    price: Some(dec!(9_000_000)),
                    size: dec!(0.001),
                    status: OrderStatus::Active,
                    created_at: Utc::now(),
                    updated_at: Utc::now(),
                });
            }
        }
        client.cancel_order("BTC_JPY", "MOCK-A").await.unwrap();
        let orders = client.placed_orders();
        assert_eq!(orders[0].status, OrderStatus::Canceled);
        assert_eq!(orders[1].status, OrderStatus::Active); // MOCK-B はそのまま
    }

    #[tokio::test]
    async fn cancel_order_not_found_returns_error() {
        let client = MockExchangeClient::new();
        let result = client.cancel_order("BTC_JPY", "NONEXISTENT").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn cancel_order_non_active_returns_error() {
        let client = MockExchangeClient::new();
        client.set_price(dec!(9_000_000));
        let id = client.send_order(&buy_req(dec!(0.001))).await.unwrap();
        // send_order は即 Completed → キャンセル不可
        let result = client.cancel_order("BTC_JPY", &id).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn get_executions_returns_filled_trades() {
        let client = MockExchangeClient::with_fee(0.0);
        client.set_price(dec!(9_000_000));
        client.send_order(&buy_req(dec!(0.001))).await.unwrap();
        client.send_order(&buy_req(dec!(0.001))).await.unwrap();

        let execs = client.get_executions("BTC_JPY", None, None, None).await.unwrap();
        assert_eq!(execs.len(), 2);
        assert_eq!(execs[0].price, dec!(9_000_000));
    }

    #[tokio::test]
    async fn get_executions_respects_count() {
        let client = MockExchangeClient::with_fee(0.0);
        client.set_price(dec!(9_000_000));
        client.send_order(&buy_req(dec!(0.001))).await.unwrap();
        client.send_order(&buy_req(dec!(0.001))).await.unwrap();
        client.send_order(&buy_req(dec!(0.001))).await.unwrap();

        let execs = client.get_executions("BTC_JPY", Some(2), None, None).await.unwrap();
        assert_eq!(execs.len(), 2);
    }

    #[tokio::test]
    async fn get_trading_commission_equals_fee_pct() {
        let client = MockExchangeClient::with_fee(0.0005);
        let rate = client.get_trading_commission("BTC_JPY").await.unwrap();
        assert_eq!(rate, 0.0005);
    }

    #[tokio::test]
    async fn override_fee_ignores_volume() {
        // with_fee() で固定すると取引量に関わらず fee_pct が変わらない
        let client = MockExchangeClient::with_fee(0.0005);
        client.set_balances(vec![
            crate::types::balance::Balance {
                currency_code: "JPY".to_string(),
                amount: dec!(20_000_000),
                available: dec!(20_000_000),
            },
            crate::types::balance::Balance {
                currency_code: "BTC".to_string(),
                amount: dec!(0),
                available: dec!(0),
            },
        ]);
        client.set_price(dec!(10_000_000));
        client
            .send_order(&buy_req(dec!(1)))
            .await
            .unwrap();
        assert_eq!(client.fee_pct(), 0.0005);
    }

    #[tokio::test]
    async fn order_marked_as_completed() {
        let client = MockExchangeClient::new();
        client.set_price(dec!(9_000_000));

        let id = client.send_order(&buy_req(dec!(0.001))).await.unwrap();
        assert!(id.starts_with("MOCK-"));

        let orders = client.placed_orders();
        assert_eq!(orders.len(), 1);
        assert_eq!(orders[0].status, OrderStatus::Completed);
    }

    #[tokio::test]
    async fn mock_send_order_records_it() {
        let client = MockExchangeClient::new();
        client.set_price(dec!(9_000_000));
        let req = OrderRequest {
            product_code: "BTC_JPY".to_string(),
            side: OrderSide::Buy,
            order_type: OrderType::Limit,
            price: Some(dec!(9_000_000)),
            size: dec!(0.001),
            minute_to_expire: None,
            time_in_force: None,
        };
        let id = client.send_order(&req).await.unwrap();
        assert!(id.starts_with("MOCK-"));

        let orders = client.placed_orders();
        assert_eq!(orders.len(), 1);
        assert_eq!(orders[0].acceptance_id, id);
    }

    #[tokio::test]
    async fn mock_cancel_all_orders_cancels_active() {
        // cancel_all_orders はアクティブな注文をキャンセルするが、
        // send_order は即時Completedになるため、手動でActiveな注文を挿入してテスト
        let client = MockExchangeClient::new();
        {
            let mut orders = client.orders.write().unwrap();
            orders.push(Order {
                id: Some(99),
                acceptance_id: "MOCK-ACTIVE".to_string(),
                product_code: "BTC_JPY".to_string(),
                side: OrderSide::Buy,
                order_type: OrderType::Limit,
                price: Some(dec!(9_000_000)),
                size: dec!(0.001),
                status: OrderStatus::Active,
                created_at: Utc::now(),
                updated_at: Utc::now(),
            });
        }
        client.cancel_all_orders("BTC_JPY").await.unwrap();

        let orders = client.placed_orders();
        assert_eq!(orders[0].status, OrderStatus::Canceled);
    }
}
