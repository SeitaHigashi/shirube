use async_trait::async_trait;
use serde::{de::DeserializeOwned, Serialize};
use tracing::debug;

use super::auth::build_auth_headers;
use super::models::{
    ApiErrorBody, CancelAllOrdersRequest, RawBalance, RawOrder, RawPosition, RawTicker,
    SendOrderRequest, SendOrderResponse,
};
use crate::error::{Error, Result};
use crate::exchange::ExchangeClient;
use crate::types::{
    balance::{Balance, Position},
    market::Ticker,
    order::{Order, OrderRequest, OrderSide, OrderType},
};

const DEFAULT_BASE_URL: &str = "https://api.bitflyer.com";

pub struct BitFlyerRestClient {
    http: reqwest::Client,
    base_url: String,
    api_key: String,
    api_secret: String,
}

impl BitFlyerRestClient {
    pub fn new(api_key: String, api_secret: String) -> Self {
        Self::new_with_base_url(api_key, api_secret, DEFAULT_BASE_URL.to_string())
    }

    pub fn new_with_base_url(api_key: String, api_secret: String, base_url: String) -> Self {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .expect("Failed to build HTTP client");
        Self {
            http,
            base_url,
            api_key,
            api_secret,
        }
    }

    async fn get_public<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        let url = format!("{}{}", self.base_url, path);
        debug!("GET {}", url);
        let resp = self.http.get(&url).send().await?;
        self.parse_response(resp).await
    }

    async fn get_authenticated<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        let headers = build_auth_headers(&self.api_key, &self.api_secret, "GET", path, "");
        let url = format!("{}{}", self.base_url, path);
        debug!("GET (auth) {}", url);
        let resp = self
            .http
            .get(&url)
            .header("ACCESS-KEY", &headers.access_key)
            .header("ACCESS-TIMESTAMP", &headers.access_timestamp)
            .header("ACCESS-SIGN", &headers.access_sign)
            .header("Content-Type", "application/json")
            .send()
            .await?;
        self.parse_response(resp).await
    }

    async fn post_authenticated<B: Serialize, T: DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<T> {
        let body_str = serde_json::to_string(body)?;
        let headers =
            build_auth_headers(&self.api_key, &self.api_secret, "POST", path, &body_str);
        let url = format!("{}{}", self.base_url, path);
        debug!("POST (auth) {}", url);
        let resp = self
            .http
            .post(&url)
            .header("ACCESS-KEY", &headers.access_key)
            .header("ACCESS-TIMESTAMP", &headers.access_timestamp)
            .header("ACCESS-SIGN", &headers.access_sign)
            .header("Content-Type", "application/json")
            .body(body_str)
            .send()
            .await?;
        self.parse_response(resp).await
    }

    async fn delete_authenticated<B: Serialize>(&self, path: &str, body: &B) -> Result<()> {
        let body_str = serde_json::to_string(body)?;
        let headers =
            build_auth_headers(&self.api_key, &self.api_secret, "DELETE", path, &body_str);
        let url = format!("{}{}", self.base_url, path);
        debug!("DELETE (auth) {}", url);
        let resp = self
            .http
            .delete(&url)
            .header("ACCESS-KEY", &headers.access_key)
            .header("ACCESS-TIMESTAMP", &headers.access_timestamp)
            .header("ACCESS-SIGN", &headers.access_sign)
            .header("Content-Type", "application/json")
            .body(body_str)
            .send()
            .await?;
        if resp.status().is_success() {
            return Ok(());
        }
        let status_code = resp.status().as_u16() as i32;
        let text = resp.text().await.unwrap_or_default();
        if let Ok(err) = serde_json::from_str::<ApiErrorBody>(&text) {
            return Err(Error::ApiError {
                code: err.status.unwrap_or(status_code),
                message: err.error_message.unwrap_or_else(|| text.clone()),
            });
        }
        Err(Error::ApiError {
            code: status_code,
            message: text,
        })
    }

    async fn parse_response<T: DeserializeOwned>(&self, resp: reqwest::Response) -> Result<T> {
        if resp.status().is_success() {
            let value = resp.json::<T>().await?;
            return Ok(value);
        }
        let status_code = resp.status().as_u16() as i32;
        let text = resp.text().await.unwrap_or_default();
        if let Ok(err) = serde_json::from_str::<ApiErrorBody>(&text) {
            return Err(Error::ApiError {
                code: err.status.unwrap_or(status_code),
                message: err.error_message.unwrap_or_else(|| text.clone()),
            });
        }
        Err(Error::ApiError {
            code: status_code,
            message: text,
        })
    }
}

#[async_trait]
impl ExchangeClient for BitFlyerRestClient {
    async fn get_ticker(&self, product_code: &str) -> Result<Ticker> {
        let path = format!("/v1/ticker?product_code={}", product_code);
        let raw: RawTicker = self.get_public(&path).await?;
        Ok(raw.into())
    }

    async fn get_balance(&self) -> Result<Vec<Balance>> {
        let raw: Vec<RawBalance> = self.get_authenticated("/v1/me/getbalance").await?;
        Ok(raw.into_iter().map(Into::into).collect())
    }

    async fn get_positions(&self, product_code: &str) -> Result<Vec<Position>> {
        let path = format!(
            "/v1/me/getpositions?product_code={}",
            product_code
        );
        let raw: Vec<RawPosition> = self.get_authenticated(&path).await?;
        Ok(raw.into_iter().map(Into::into).collect())
    }

    async fn send_order(&self, req: &OrderRequest) -> Result<String> {
        let body = SendOrderRequest {
            product_code: req.product_code.clone(),
            child_order_type: match req.order_type {
                OrderType::Limit => "LIMIT".to_string(),
                OrderType::Market => "MARKET".to_string(),
            },
            side: match req.side {
                OrderSide::Buy => "BUY".to_string(),
                OrderSide::Sell => "SELL".to_string(),
            },
            price: req.price,
            size: req.size,
            minute_to_expire: req.minute_to_expire,
            time_in_force: req.time_in_force.clone(),
        };
        let resp: SendOrderResponse = self
            .post_authenticated("/v1/me/sendchildorder", &body)
            .await?;
        Ok(resp.child_order_acceptance_id)
    }

    async fn cancel_all_orders(&self, product_code: &str) -> Result<()> {
        let body = CancelAllOrdersRequest {
            product_code: product_code.to_string(),
        };
        self.delete_authenticated("/v1/me/cancelallchildorders", &body)
            .await
    }

    async fn get_orders(
        &self,
        product_code: &str,
        status: Option<&str>,
        count: Option<u32>,
    ) -> Result<Vec<Order>> {
        let mut path = format!(
            "/v1/me/getchildorders?product_code={}",
            product_code
        );
        if let Some(s) = status {
            path.push_str(&format!("&child_order_state={}", s));
        }
        if let Some(c) = count {
            path.push_str(&format!("&count={}", c));
        }
        let raw: Vec<RawOrder> = self.get_authenticated(&path).await?;
        Ok(raw.into_iter().map(Into::into).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;
    use serde_json::json;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    async fn make_client(server: &MockServer) -> BitFlyerRestClient {
        BitFlyerRestClient::new_with_base_url(
            "testkey".to_string(),
            "testsecret".to_string(),
            server.uri(),
        )
    }

    #[tokio::test]
    async fn get_ticker_parses_correctly() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/ticker"))
            .and(query_param("product_code", "BTC_JPY"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "product_code": "BTC_JPY",
                "timestamp": "2024-01-01T00:00:00.000Z",
                "best_bid": "9000000",
                "best_ask": "9001000",
                "best_bid_size": "0.1",
                "best_ask_size": "0.2",
                "ltp": "9000500",
                "volume": "1234.5",
                "volume_by_product": "1234.5"
            })))
            .mount(&server)
            .await;

        let client = make_client(&server).await;
        let ticker = client.get_ticker("BTC_JPY").await.unwrap();
        assert_eq!(ticker.product_code, "BTC_JPY");
        assert_eq!(ticker.best_bid, dec!(9000000));
        assert_eq!(ticker.ltp, dec!(9000500));
    }

    #[tokio::test]
    async fn get_ticker_api_error_returns_err() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/ticker"))
            .respond_with(ResponseTemplate::new(400).set_body_json(json!({
                "status": -1,
                "error_message": "Bad Request",
                "data": null
            })))
            .mount(&server)
            .await;

        let client = make_client(&server).await;
        let result = client.get_ticker("BTC_JPY").await;
        assert!(result.is_err());
        match result.unwrap_err() {
            Error::ApiError { code, message } => {
                assert_eq!(code, -1);
                assert_eq!(message, "Bad Request");
            }
            e => panic!("unexpected error: {:?}", e),
        }
    }

    #[tokio::test]
    async fn send_order_returns_acceptance_id() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/me/sendchildorder"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "child_order_acceptance_id": "JRF20240101-000000-123456"
            })))
            .mount(&server)
            .await;

        let client = make_client(&server).await;
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
        assert_eq!(id, "JRF20240101-000000-123456");
    }

    #[tokio::test]
    async fn get_balance_parses_correctly() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/me/getbalance"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([
                { "currency_code": "JPY", "amount": "1000000", "available": "900000" },
                { "currency_code": "BTC", "amount": "0.1", "available": "0.1" }
            ])))
            .mount(&server)
            .await;

        let client = make_client(&server).await;
        let balances = client.get_balance().await.unwrap();
        assert_eq!(balances.len(), 2);
        assert_eq!(balances[0].currency_code, "JPY");
        assert_eq!(balances[0].amount, dec!(1000000));
    }
}
