pub mod bitflyer;
pub mod mock;
pub mod rate_limiter;

use std::sync::Arc;

use async_trait::async_trait;

use crate::error::Result;
use crate::exchange::rate_limiter::RateLimiter;
use crate::storage::mock_state::MockStateRepository;
use crate::types::{
    balance::{Balance, Position},
    market::{MyExecution, Ticker},
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
    /// 個別注文をキャンセルする（`POST /v1/me/cancelchildorder`）。
    async fn cancel_order(&self, product_code: &str, acceptance_id: &str) -> Result<()>;
    /// 自分の約定履歴を取得する（`GET /v1/me/getexecutions`）。
    async fn get_executions(
        &self,
        product_code: &str,
        count: Option<u32>,
        before: Option<i64>,
        after: Option<i64>,
    ) -> Result<Vec<MyExecution>>;
    /// 取引手数料率を取得する（`GET /v1/me/gettradingcommission`）。
    async fn get_trading_commission(&self, product_code: &str) -> Result<f64>;
    /// 現在の手数料率を返す（bitFlyer Lightning 現物ティア制）。
    fn fee_pct(&self) -> f64;
}

/// API キーなしで使えるクライアント。
/// ticker など公開エンドポイントは実際の bitFlyer に問い合わせ、
/// 注文・残高など認証が必要な操作は MockExchangeClient に委譲する。
pub struct PublicBitFlyerClient {
    rest: bitflyer::rest::BitFlyerRestClient,
    mock: mock::MockExchangeClient,
    /// The single rate-limit budget both halves draw on. Held here so callers
    /// can read `granted()` / `waited()` to see how saturated paper trading is.
    rate_limiter: Arc<RateLimiter>,
}

impl PublicBitFlyerClient {
    /// Build the composite client with one shared rate-limit budget.
    ///
    /// NOTE: added 2026-09-13. Both halves previously had independent limiters
    /// — and `MockExchangeClient` in fact had none at all — so paper trading
    /// charged only `get_ticker` against a budget while `get_balance`,
    /// `get_positions` and `send_order` were free. Since `handle_indicator`
    /// issues all three on every evaluation, paper trading consumed one token
    /// where production consumes three, understating production's API load by
    /// 3x and making rate-limit saturation invisible in paper trading. bitFlyer
    /// applies its limit per account/IP rather than per client object, so a
    /// single shared bucket is the faithful model.
    fn build(rest_base_url: String, mock: mock::MockExchangeClient) -> Self {
        let rate_limiter = Arc::new(RateLimiter::new(
            bitflyer::rest::BITFLYER_MAX_REQ_PER_MIN,
        ));
        let rest = bitflyer::rest::BitFlyerRestClient::new_with_limiter(
            String::new(),
            String::new(),
            rest_base_url,
            Arc::clone(&rate_limiter),
        );
        Self {
            rest,
            mock: mock.with_rate_limiter(Arc::clone(&rate_limiter)),
            rate_limiter,
        }
    }

    pub fn new() -> Self {
        Self::build(
            bitflyer::rest::DEFAULT_BASE_URL.to_string(),
            mock::MockExchangeClient::new(),
        )
    }

    pub async fn new_with_db(repo: MockStateRepository) -> Result<Self> {
        Ok(Self::build(
            bitflyer::rest::DEFAULT_BASE_URL.to_string(),
            mock::MockExchangeClient::new_with_db(repo).await?,
        ))
    }

    /// The shared rate-limit budget, for observability.
    pub fn rate_limiter(&self) -> &Arc<RateLimiter> {
        &self.rate_limiter
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

    async fn cancel_order(&self, product_code: &str, acceptance_id: &str) -> Result<()> {
        self.mock.cancel_order(product_code, acceptance_id).await
    }

    async fn get_executions(
        &self,
        product_code: &str,
        count: Option<u32>,
        before: Option<i64>,
        after: Option<i64>,
    ) -> Result<Vec<MyExecution>> {
        self.mock.get_executions(product_code, count, before, after).await
    }

    async fn get_trading_commission(&self, product_code: &str) -> Result<f64> {
        self.mock.get_trading_commission(product_code).await
    }

    fn fee_pct(&self) -> f64 {
        self.mock.fee_pct()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `PublicBitFlyerClient` must charge its mock-delegated calls against the
    /// same budget its REST half uses. Until 2026-09-13 the mock half had no
    /// limiter at all, so paper trading consumed one token per
    /// `handle_indicator` cycle where production consumes three — it could not
    /// reproduce production's rate-limit saturation.
    ///
    /// Only the mock-backed methods are exercised here so the test needs no
    /// network; `get_ticker` is the REST half and already charged the same
    /// `Arc<RateLimiter>` by construction.
    #[tokio::test]
    async fn public_client_charges_mock_delegated_calls_to_the_shared_budget() {
        let client = PublicBitFlyerClient::new();
        assert_eq!(client.rate_limiter().granted(), 0);

        client.get_balance().await.unwrap();
        client.get_positions("BTC_JPY").await.unwrap();

        assert_eq!(
            client.rate_limiter().granted(),
            2,
            "mock-delegated calls must consume the shared rate-limit budget"
        );
    }
}
