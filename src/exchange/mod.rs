pub mod bitflyer;
pub mod mock;
pub mod rate_limiter;

use async_trait::async_trait;

use crate::error::Result;
use crate::storage::mock_state::MockStateRepository;
use crate::types::{
    balance::{Balance, Position},
    market::Ticker,
    order::{Order, OrderRequest},
};

#[async_trait]
pub trait ExchangeClient: Send + Sync + 'static {
    async fn get_ticker(&self, product_code: &str) -> Result<Ticker>;
    async fn get_balance(&self) -> Result<Vec<Balance>>;
    async fn get_positions(&self, product_code: &str) -> Result<Vec<Position>>;
    async fn send_order(&self, req: &OrderRequest) -> Result<String>;
    async fn cancel_all_orders(&self, product_code: &str) -> Result<()>;
    async fn get_orders(
        &self,
        product_code: &str,
        status: Option<&str>,
        count: Option<u32>,
    ) -> Result<Vec<Order>>;
}

/// API キーなしで使えるクライアント。
/// ticker など公開エンドポイントは実際の bitFlyer に問い合わせ、
/// 注文・残高など認証が必要な操作は MockExchangeClient に委譲する。
pub struct PublicBitFlyerClient {
    rest: bitflyer::rest::BitFlyerRestClient,
    mock: mock::MockExchangeClient,
}

impl PublicBitFlyerClient {
    pub fn new() -> Self {
        Self {
            rest: bitflyer::rest::BitFlyerRestClient::new(String::new(), String::new()),
            mock: mock::MockExchangeClient::new(),
        }
    }

    pub async fn new_with_db(repo: MockStateRepository, fee_pct: f64) -> Result<Self> {
        Ok(Self {
            rest: bitflyer::rest::BitFlyerRestClient::new(String::new(), String::new()),
            mock: mock::MockExchangeClient::new_with_db(repo, fee_pct).await?,
        })
    }
}

impl Default for PublicBitFlyerClient {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ExchangeClient for PublicBitFlyerClient {
    async fn get_ticker(&self, product_code: &str) -> Result<Ticker> {
        // 公開 API — API キー不要
        self.rest.get_ticker(product_code).await
    }

    async fn get_balance(&self) -> Result<Vec<Balance>> {
        self.mock.get_balance().await
    }

    async fn get_positions(&self, product_code: &str) -> Result<Vec<Position>> {
        self.mock.get_positions(product_code).await
    }

    async fn send_order(&self, req: &OrderRequest) -> Result<String> {
        self.mock.send_order(req).await
    }

    async fn cancel_all_orders(&self, product_code: &str) -> Result<()> {
        self.mock.cancel_all_orders(product_code).await
    }

    async fn get_orders(
        &self,
        product_code: &str,
        status: Option<&str>,
        count: Option<u32>,
    ) -> Result<Vec<Order>> {
        self.mock.get_orders(product_code, status, count).await
    }
}
