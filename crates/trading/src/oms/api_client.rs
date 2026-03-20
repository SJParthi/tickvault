//! Dhan REST API client for order management.
//!
//! Typed wrappers around the Dhan order REST endpoints.
//! Cold path — order submission is ~1-100/day. Allocations acceptable.
//!
//! # Endpoints
//! - `POST   /orders`              — Place order
//! - `PUT    /orders/{orderId}`     — Modify order
//! - `DELETE /orders/{orderId}`     — Cancel order
//! - `GET    /orders/{orderId}`     — Get order by ID
//! - `GET    /orders`               — Get all orders (today)
//! - `GET    /positions`            — Get positions
//! - `GET    /holdings`             — Get holdings
//! - `POST   /positions/convert`    — Convert position product type
//! - `DELETE /positions`            — Exit all positions + cancel orders
//! - `POST   /margincalculator`     — Single margin calculation
//! - `POST   /margincalculator/multi` — Multi margin calculation
//! - `GET    /fundlimit`            — Get fund/balance limits
//!
//! # Authentication
//! Dhan uses custom headers, NOT `Authorization: Bearer`:
//! - `access-token: <JWT>`
//! - `client-id: <Dhan client ID>`

use reqwest::{Client, RequestBuilder};
use tracing::{debug, warn};

use dhan_live_trader_common::constants;

use super::types::{
    DhanConvertPositionRequest, DhanExitAllResponse, DhanHoldingResponse, DhanModifyOrderRequest,
    DhanOrderResponse, DhanPlaceOrderRequest, DhanPlaceOrderResponse, DhanPositionResponse,
    FundLimitResponse, MarginCalculatorRequest, MarginCalculatorResponse, MultiMarginRequest,
    MultiMarginResponse, OmsError,
};

// ---------------------------------------------------------------------------
// HTTP status codes
// ---------------------------------------------------------------------------

/// HTTP 429 Too Many Requests (SEBI rate limit at broker).
const HTTP_TOO_MANY_REQUESTS: u16 = 429;

// ---------------------------------------------------------------------------
// OrderApiClient
// ---------------------------------------------------------------------------

/// HTTP client for Dhan order management REST API.
///
/// Holds a shared `reqwest::Client` (connection pool), the base URL,
/// and the Dhan client ID for authentication headers.
pub struct OrderApiClient {
    /// Shared HTTP client (connection pool).
    http: Client,
    /// Dhan REST API base URL (e.g., `https://api.dhan.co/v2`).
    base_url: String,
    /// Dhan client ID for the `client-id` header.
    client_id: String,
}

impl OrderApiClient {
    /// Creates a new API client.
    ///
    /// # Arguments
    /// * `http` — Shared reqwest client.
    /// * `base_url` — Dhan REST API base URL from config.
    /// * `client_id` — Dhan client ID for authentication.
    pub fn new(http: Client, base_url: String, client_id: String) -> Self {
        Self {
            http,
            base_url,
            client_id,
        }
    }

    /// Places a new order.
    ///
    /// # Errors
    /// - `OmsError::DhanRateLimited` on HTTP 429
    /// - `OmsError::DhanApiError` on other non-2xx responses
    /// - `OmsError::HttpError` on transport failure
    pub async fn place_order(
        &self,
        access_token: &str,
        request: &DhanPlaceOrderRequest,
    ) -> Result<DhanPlaceOrderResponse, OmsError> {
        let url = format!("{}/orders", self.base_url);

        debug!(
            security_id = %request.security_id,
            transaction_type = %request.transaction_type,
            order_type = %request.order_type,
            quantity = request.quantity,
            price = request.price,
            "placing order"
        );

        let response = self
            .auth_headers(self.http.post(&url), access_token)
            .json(request)
            .send()
            .await
            .map_err(|err| OmsError::HttpError(err.to_string()))?;

        let status = response.status().as_u16();
        self.check_rate_limit(status, "place")?;

        let body = response
            .text()
            .await
            .map_err(|err| OmsError::HttpError(err.to_string()))?;

        if !(200..300).contains(&status) {
            return Err(OmsError::DhanApiError {
                status_code: status,
                message: body,
            });
        }

        serde_json::from_str(&body).map_err(|err| OmsError::JsonError(err.to_string()))
    }

    /// Modifies an existing open order.
    ///
    /// # Errors
    /// Same as `place_order`.
    pub async fn modify_order(
        &self,
        access_token: &str,
        order_id: &str,
        request: &DhanModifyOrderRequest,
    ) -> Result<(), OmsError> {
        let url = format!("{}/orders/{}", self.base_url, order_id);

        debug!(order_id = %order_id, "modifying order");

        let response = self
            .auth_headers(self.http.put(&url), access_token)
            .json(request)
            .send()
            .await
            .map_err(|err| OmsError::HttpError(err.to_string()))?;

        let status = response.status().as_u16();
        self.check_rate_limit(status, "modify")?;

        if !(200..300).contains(&status) {
            let body = response
                .text()
                .await
                .map_err(|err| OmsError::HttpError(err.to_string()))?;
            return Err(OmsError::DhanApiError {
                status_code: status,
                message: body,
            });
        }

        Ok(())
    }

    /// Cancels an order.
    ///
    /// # Errors
    /// Same as `place_order`.
    pub async fn cancel_order(&self, access_token: &str, order_id: &str) -> Result<(), OmsError> {
        let url = format!("{}/orders/{}", self.base_url, order_id);

        debug!(order_id = %order_id, "cancelling order");

        let response = self
            .auth_headers(self.http.delete(&url), access_token)
            .send()
            .await
            .map_err(|err| OmsError::HttpError(err.to_string()))?;

        let status = response.status().as_u16();
        self.check_rate_limit(status, "cancel")?;

        if !(200..300).contains(&status) {
            let body = response
                .text()
                .await
                .map_err(|err| OmsError::HttpError(err.to_string()))?;
            return Err(OmsError::DhanApiError {
                status_code: status,
                message: body,
            });
        }

        Ok(())
    }

    /// Gets a single order by ID.
    ///
    /// # Errors
    /// Same as `place_order`.
    pub async fn get_order(
        &self,
        access_token: &str,
        order_id: &str,
    ) -> Result<DhanOrderResponse, OmsError> {
        let url = format!("{}/orders/{}", self.base_url, order_id);

        let response = self
            .auth_headers(self.http.get(&url), access_token)
            .send()
            .await
            .map_err(|err| OmsError::HttpError(err.to_string()))?;

        self.handle_json_response(response, "get_order").await
    }

    /// Gets all orders for today.
    ///
    /// Used for reconciliation after WebSocket reconnect.
    ///
    /// # Errors
    /// Same as `place_order`.
    pub async fn get_all_orders(
        &self,
        access_token: &str,
    ) -> Result<Vec<DhanOrderResponse>, OmsError> {
        let url = format!("{}/orders", self.base_url);

        let response = self
            .auth_headers(self.http.get(&url), access_token)
            .send()
            .await
            .map_err(|err| OmsError::HttpError(err.to_string()))?;

        let status = response.status().as_u16();
        self.check_rate_limit(status, "get_all_orders")?;

        let body = response
            .text()
            .await
            .map_err(|err| OmsError::HttpError(err.to_string()))?;

        if !(200..300).contains(&status) {
            return Err(OmsError::DhanApiError {
                status_code: status,
                message: body,
            });
        }

        serde_json::from_str(&body).map_err(|err| OmsError::JsonError(err.to_string()))
    }

    /// Gets all positions for today.
    ///
    /// Used for startup reconciliation and position seeding.
    ///
    /// # Errors
    /// Same as `place_order`.
    pub async fn get_positions(
        &self,
        access_token: &str,
    ) -> Result<Vec<DhanPositionResponse>, OmsError> {
        let url = format!("{}/positions", self.base_url);

        let response = self
            .auth_headers(self.http.get(&url), access_token)
            .send()
            .await
            .map_err(|err| OmsError::HttpError(err.to_string()))?;

        let status = response.status().as_u16();
        self.check_rate_limit(status, "get_positions")?;

        let body = response
            .text()
            .await
            .map_err(|err| OmsError::HttpError(err.to_string()))?;

        if !(200..300).contains(&status) {
            return Err(OmsError::DhanApiError {
                status_code: status,
                message: body,
            });
        }

        serde_json::from_str(&body).map_err(|err| OmsError::JsonError(err.to_string()))
    }

    // -----------------------------------------------------------------------
    // Portfolio — Holdings, Convert Position, Exit All
    // -----------------------------------------------------------------------

    /// Gets all holdings.
    ///
    /// `GET /v2/holdings` — response is a flat array of holding objects.
    ///
    /// # Errors
    /// - `OmsError::DhanRateLimited` on HTTP 429
    /// - `OmsError::DhanApiError` on non-2xx
    /// - `OmsError::HttpError` on transport failure
    pub async fn get_holdings(
        &self,
        access_token: &str,
    ) -> Result<Vec<DhanHoldingResponse>, OmsError> {
        let url = format!("{}{}", self.base_url, constants::DHAN_HOLDINGS_PATH);

        let response = self
            .auth_headers(self.http.get(&url), access_token)
            .send()
            .await
            .map_err(|err| OmsError::HttpError(err.to_string()))?;

        let status = response.status().as_u16();
        self.check_rate_limit(status, "get_holdings")?;

        let body = response
            .text()
            .await
            .map_err(|err| OmsError::HttpError(err.to_string()))?;

        if !(200..300).contains(&status) {
            return Err(OmsError::DhanApiError {
                status_code: status,
                message: body,
            });
        }

        serde_json::from_str(&body).map_err(|err| OmsError::JsonError(err.to_string()))
    }

    /// Converts a position between product types.
    ///
    /// `POST /v2/positions/convert` — e.g., INTRADAY → CNC.
    /// Note: `convertQty` is a STRING, not integer.
    /// Response is `202 Accepted`.
    ///
    /// # Errors
    /// Same as `place_order`.
    pub async fn convert_position(
        &self,
        access_token: &str,
        request: &DhanConvertPositionRequest,
    ) -> Result<(), OmsError> {
        let url = format!(
            "{}{}",
            self.base_url,
            constants::DHAN_POSITIONS_CONVERT_PATH
        );

        debug!(
            security_id = %request.security_id,
            from = %request.from_product_type,
            to = %request.to_product_type,
            "converting position"
        );

        let response = self
            .auth_headers(self.http.post(&url), access_token)
            .json(request)
            .send()
            .await
            .map_err(|err| OmsError::HttpError(err.to_string()))?;

        let status = response.status().as_u16();
        self.check_rate_limit(status, "convert_position")?;

        // 202 Accepted is success for convert
        if !(200..300).contains(&status) {
            let body = response
                .text()
                .await
                .map_err(|err| OmsError::HttpError(err.to_string()))?;
            return Err(OmsError::DhanApiError {
                status_code: status,
                message: body,
            });
        }

        Ok(())
    }

    /// Exits all open positions AND cancels all pending orders.
    ///
    /// `DELETE /v2/positions` — emergency stop. Use alongside kill switch.
    ///
    /// # Errors
    /// Same as `place_order`.
    pub async fn exit_all_positions(
        &self,
        access_token: &str,
    ) -> Result<DhanExitAllResponse, OmsError> {
        let url = format!("{}{}", self.base_url, constants::DHAN_POSITIONS_PATH);

        warn!("EXIT ALL — closing all positions and cancelling all pending orders");

        let response = self
            .auth_headers(self.http.delete(&url), access_token)
            .send()
            .await
            .map_err(|err| OmsError::HttpError(err.to_string()))?;

        let status = response.status().as_u16();
        self.check_rate_limit(status, "exit_all_positions")?;

        let body = response
            .text()
            .await
            .map_err(|err| OmsError::HttpError(err.to_string()))?;

        if !(200..300).contains(&status) {
            return Err(OmsError::DhanApiError {
                status_code: status,
                message: body,
            });
        }

        serde_json::from_str(&body).map_err(|err| OmsError::JsonError(err.to_string()))
    }

    // -----------------------------------------------------------------------
    // Funds & Margin
    // -----------------------------------------------------------------------

    /// Calculates margin for a single order.
    ///
    /// `POST /v2/margincalculator` — uses same fields as order placement.
    /// Note: `leverage` in response is a STRING, not float.
    ///
    /// # Errors
    /// Same as `place_order`.
    pub async fn calculate_margin(
        &self,
        access_token: &str,
        request: &MarginCalculatorRequest,
    ) -> Result<MarginCalculatorResponse, OmsError> {
        let url = format!(
            "{}{}",
            self.base_url,
            constants::DHAN_MARGIN_CALCULATOR_PATH
        );

        let response = self
            .auth_headers(self.http.post(&url), access_token)
            .json(request)
            .send()
            .await
            .map_err(|err| OmsError::HttpError(err.to_string()))?;

        let status = response.status().as_u16();
        self.check_rate_limit(status, "calculate_margin")?;

        let body = response
            .text()
            .await
            .map_err(|err| OmsError::HttpError(err.to_string()))?;

        if !(200..300).contains(&status) {
            return Err(OmsError::DhanApiError {
                status_code: status,
                message: body,
            });
        }

        serde_json::from_str(&body).map_err(|err| OmsError::JsonError(err.to_string()))
    }

    /// Calculates margin for multiple orders (portfolio margin).
    ///
    /// `POST /v2/margincalculator/multi` — includes position/order context
    /// and hedge benefit calculation. All response values are STRINGS.
    ///
    /// # Errors
    /// Same as `place_order`.
    pub async fn calculate_multi_margin(
        &self,
        access_token: &str,
        request: &MultiMarginRequest,
    ) -> Result<MultiMarginResponse, OmsError> {
        let url = format!(
            "{}{}",
            self.base_url,
            constants::DHAN_MARGIN_CALCULATOR_MULTI_PATH
        );

        let response = self
            .auth_headers(self.http.post(&url), access_token)
            .json(request)
            .send()
            .await
            .map_err(|err| OmsError::HttpError(err.to_string()))?;

        let status = response.status().as_u16();
        self.check_rate_limit(status, "calculate_multi_margin")?;

        let body = response
            .text()
            .await
            .map_err(|err| OmsError::HttpError(err.to_string()))?;

        if !(200..300).contains(&status) {
            return Err(OmsError::DhanApiError {
                status_code: status,
                message: body,
            });
        }

        serde_json::from_str(&body).map_err(|err| OmsError::JsonError(err.to_string()))
    }

    /// Gets fund/balance limits for the account.
    ///
    /// `GET /v2/fundlimit` — Note: Dhan's response uses `availabelBalance`
    /// (typo — missing 'l'). We preserve this exact field name.
    ///
    /// # Errors
    /// Same as `place_order`.
    pub async fn get_fund_limit(&self, access_token: &str) -> Result<FundLimitResponse, OmsError> {
        let url = format!("{}{}", self.base_url, constants::DHAN_FUND_LIMIT_PATH);

        let response = self
            .auth_headers(self.http.get(&url), access_token)
            .send()
            .await
            .map_err(|err| OmsError::HttpError(err.to_string()))?;

        let status = response.status().as_u16();
        self.check_rate_limit(status, "get_fund_limit")?;

        let body = response
            .text()
            .await
            .map_err(|err| OmsError::HttpError(err.to_string()))?;

        if !(200..300).contains(&status) {
            return Err(OmsError::DhanApiError {
                status_code: status,
                message: body,
            });
        }

        serde_json::from_str(&body).map_err(|err| OmsError::JsonError(err.to_string()))
    }

    // -----------------------------------------------------------------------
    // Internal helpers
    // -----------------------------------------------------------------------

    /// Sets Dhan authentication headers on a request builder.
    ///
    /// Dhan uses custom headers, NOT `Authorization: Bearer`:
    /// - `access-token` — JWT access token
    /// - `client-id` — Dhan client identifier
    /// - `Content-Type` — application/json
    /// - `Accept` — application/json
    fn auth_headers(&self, builder: RequestBuilder, access_token: &str) -> RequestBuilder {
        builder
            .header("access-token", access_token)
            .header("client-id", &self.client_id)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json")
    }

    /// Checks for HTTP 429 and returns `OmsError::DhanRateLimited`.
    fn check_rate_limit(&self, status: u16, operation: &str) -> Result<(), OmsError> {
        if status == HTTP_TOO_MANY_REQUESTS {
            warn!(
                operation = %operation,
                "Dhan rate limited (HTTP 429) — SEBI violation risk, backing off"
            );
            return Err(OmsError::DhanRateLimited);
        }
        Ok(())
    }

    /// Handles a JSON response with status check and 429 handling.
    async fn handle_json_response(
        &self,
        response: reqwest::Response,
        operation: &str,
    ) -> Result<DhanOrderResponse, OmsError> {
        let status = response.status().as_u16();
        self.check_rate_limit(status, operation)?;

        let body = response
            .text()
            .await
            .map_err(|err| OmsError::HttpError(err.to_string()))?;

        if !(200..300).contains(&status) {
            return Err(OmsError::DhanApiError {
                status_code: status,
                message: body,
            });
        }

        serde_json::from_str(&body).map_err(|err| OmsError::JsonError(err.to_string()))
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_client_construction() {
        let http = Client::new();
        let client =
            OrderApiClient::new(http, "https://api.dhan.co/v2".to_owned(), "100".to_owned());
        assert_eq!(client.base_url, "https://api.dhan.co/v2");
        assert_eq!(client.client_id, "100");
    }

    #[test]
    fn http_too_many_requests_constant() {
        assert_eq!(HTTP_TOO_MANY_REQUESTS, 429);
    }

    #[test]
    fn check_rate_limit_ok_on_200() {
        let client = OrderApiClient::new(
            Client::new(),
            "https://api.dhan.co/v2".to_owned(),
            "100".to_owned(),
        );
        assert!(client.check_rate_limit(200, "test").is_ok());
        assert!(client.check_rate_limit(201, "test").is_ok());
    }

    #[test]
    fn check_rate_limit_err_on_429() {
        let client = OrderApiClient::new(
            Client::new(),
            "https://api.dhan.co/v2".to_owned(),
            "100".to_owned(),
        );
        let result = client.check_rate_limit(429, "test");
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), OmsError::DhanRateLimited));
    }

    // -----------------------------------------------------------------------
    // HTTP mock tests — async tests using a local TCP mock server
    // -----------------------------------------------------------------------

    use std::time::Duration;

    /// Starts a one-shot TCP mock server that returns the given status and body.
    #[allow(clippy::arithmetic_side_effects)] // APPROVED: test-only content-length arithmetic
    async fn start_mock_server(status: u16, body: &str) -> (String, tokio::task::JoinHandle<()>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base_url = format!("http://127.0.0.1:{}", addr.port());
        let body = body.to_string();

        let handle = tokio::spawn(async move {
            if let Ok((mut stream, _)) = listener.accept().await {
                let mut buf = vec![0u8; 8192];
                let _ = stream.read(&mut buf).await;
                let response = format!(
                    "HTTP/1.1 {} Status\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    status,
                    body.len(),
                    body
                );
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.shutdown().await;
            }
        });

        (base_url, handle)
    }

    /// Builds a test `OrderApiClient` pointing at the given mock base URL.
    fn make_test_client(base_url: &str) -> OrderApiClient {
        let http = Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap();
        OrderApiClient::new(http, base_url.to_owned(), "TEST-100".to_owned())
    }

    /// Builds a minimal `DhanPlaceOrderRequest` for test use.
    fn make_test_place_request() -> DhanPlaceOrderRequest {
        DhanPlaceOrderRequest {
            dhan_client_id: "100".to_owned(),
            transaction_type: "BUY".to_owned(),
            exchange_segment: "NSE_FNO".to_owned(),
            product_type: "INTRADAY".to_owned(),
            order_type: "LIMIT".to_owned(),
            validity: "DAY".to_owned(),
            security_id: "52432".to_owned(),
            quantity: 50,
            price: 245.50,
            trigger_price: 0.0,
            disclosed_quantity: 0,
            after_market_order: false,
            correlation_id: "test-uuid-1".to_owned(),
        }
    }

    /// Builds a minimal `DhanModifyOrderRequest` for test use.
    fn make_test_modify_request() -> DhanModifyOrderRequest {
        DhanModifyOrderRequest {
            dhan_client_id: "100".to_owned(),
            order_id: "ORD-1".to_owned(),
            order_type: "LIMIT".to_owned(),
            leg_name: String::new(),
            quantity: 50,
            price: 250.00,
            trigger_price: 0.0,
            validity: "DAY".to_owned(),
            disclosed_quantity: 0,
        }
    }

    // -- 1. place_order success -----------------------------------------------

    #[tokio::test]
    async fn test_place_order_success_200() {
        let body = r#"{"orderId":"ORD-1","orderStatus":"TRANSIT","correlationId":"uuid-1"}"#;
        let (base_url, handle) = start_mock_server(200, body).await;
        let client = make_test_client(&base_url);

        let result = client
            .place_order("fake-token", &make_test_place_request())
            .await;

        let resp = result.unwrap();
        assert_eq!(resp.order_id, "ORD-1");
        assert_eq!(resp.order_status, "TRANSIT");
        assert_eq!(resp.correlation_id, "uuid-1");

        handle.abort();
    }

    // -- 2. place_order rate limited ------------------------------------------

    #[tokio::test]
    async fn test_place_order_rate_limited_429() {
        let (base_url, handle) = start_mock_server(429, "{}").await;
        let client = make_test_client(&base_url);

        let result = client
            .place_order("fake-token", &make_test_place_request())
            .await;

        assert!(matches!(result.unwrap_err(), OmsError::DhanRateLimited));

        handle.abort();
    }

    // -- 3. place_order API error 500 -----------------------------------------

    #[tokio::test]
    async fn test_place_order_api_error_500() {
        let body = r#"{"errorCode":"DH-908","errorMessage":"internal"}"#;
        let (base_url, handle) = start_mock_server(500, body).await;
        let client = make_test_client(&base_url);

        let result = client
            .place_order("fake-token", &make_test_place_request())
            .await;

        match result.unwrap_err() {
            OmsError::DhanApiError {
                status_code,
                message,
            } => {
                assert_eq!(status_code, 500);
                assert!(message.contains("DH-908"));
            }
            other => panic!("expected DhanApiError, got: {other:?}"),
        }

        handle.abort();
    }

    // -- 4. place_order malformed JSON ----------------------------------------

    #[tokio::test]
    async fn test_place_order_malformed_json_200() {
        let (base_url, handle) = start_mock_server(200, "not json").await;
        let client = make_test_client(&base_url);

        let result = client
            .place_order("fake-token", &make_test_place_request())
            .await;

        assert!(matches!(result.unwrap_err(), OmsError::JsonError(_)));

        handle.abort();
    }

    // -- 5. modify_order success ----------------------------------------------

    #[tokio::test]
    async fn test_modify_order_success_200() {
        let body = r#"{"orderId":"ORD-1","orderStatus":"PENDING"}"#;
        let (base_url, handle) = start_mock_server(200, body).await;
        let client = make_test_client(&base_url);

        let result = client
            .modify_order("fake-token", "ORD-1", &make_test_modify_request())
            .await;

        assert!(result.is_ok());

        handle.abort();
    }

    // -- 6. modify_order rate limited -----------------------------------------

    #[tokio::test]
    async fn test_modify_order_rate_limited_429() {
        let (base_url, handle) = start_mock_server(429, "{}").await;
        let client = make_test_client(&base_url);

        let result = client
            .modify_order("fake-token", "ORD-1", &make_test_modify_request())
            .await;

        assert!(matches!(result.unwrap_err(), OmsError::DhanRateLimited));

        handle.abort();
    }

    // -- 7. cancel_order success ----------------------------------------------

    #[tokio::test]
    async fn test_cancel_order_success_200() {
        let (base_url, handle) = start_mock_server(200, "").await;
        let client = make_test_client(&base_url);

        let result = client.cancel_order("fake-token", "ORD-1").await;

        assert!(result.is_ok());

        handle.abort();
    }

    // -- 8. cancel_order API error 403 ----------------------------------------

    #[tokio::test]
    async fn test_cancel_order_api_error_403() {
        let body = r#"{"errorCode":"DH-901","errorMessage":"forbidden"}"#;
        let (base_url, handle) = start_mock_server(403, body).await;
        let client = make_test_client(&base_url);

        let result = client.cancel_order("fake-token", "ORD-1").await;

        match result.unwrap_err() {
            OmsError::DhanApiError {
                status_code,
                message,
            } => {
                assert_eq!(status_code, 403);
                assert!(message.contains("DH-901"));
            }
            other => panic!("expected DhanApiError, got: {other:?}"),
        }

        handle.abort();
    }

    // -- 9. get_order success -------------------------------------------------

    #[tokio::test]
    async fn test_get_order_success() {
        let body = r#"{
            "orderId": "ORD-99",
            "orderStatus": "TRADED",
            "transactionType": "BUY",
            "exchangeSegment": "NSE_FNO",
            "productType": "INTRADAY",
            "orderType": "LIMIT",
            "securityId": "52432",
            "quantity": 50,
            "price": 245.50,
            "tradedQuantity": 50,
            "tradedPrice": 245.50,
            "correlationId": "corr-99"
        }"#;
        let (base_url, handle) = start_mock_server(200, body).await;
        let client = make_test_client(&base_url);

        let result = client.get_order("fake-token", "ORD-99").await;

        let resp = result.unwrap();
        assert_eq!(resp.order_id, "ORD-99");
        assert_eq!(resp.order_status, "TRADED");
        assert_eq!(resp.transaction_type, "BUY");
        assert_eq!(resp.security_id, "52432");
        assert_eq!(resp.quantity, 50);
        assert_eq!(resp.correlation_id, "corr-99");

        handle.abort();
    }

    // -- 10. get_all_orders success -------------------------------------------

    #[tokio::test]
    async fn test_get_all_orders_success() {
        let body = r#"[
            {"orderId": "ORD-1", "orderStatus": "TRADED", "quantity": 25},
            {"orderId": "ORD-2", "orderStatus": "PENDING", "quantity": 50}
        ]"#;
        let (base_url, handle) = start_mock_server(200, body).await;
        let client = make_test_client(&base_url);

        let result = client.get_all_orders("fake-token").await;

        let orders = result.unwrap();
        assert_eq!(orders.len(), 2);
        assert_eq!(orders[0].order_id, "ORD-1");
        assert_eq!(orders[0].order_status, "TRADED");
        assert_eq!(orders[1].order_id, "ORD-2");
        assert_eq!(orders[1].order_status, "PENDING");

        handle.abort();
    }

    // -- 11. get_all_orders empty array ---------------------------------------

    #[tokio::test]
    async fn test_get_all_orders_empty_array() {
        let (base_url, handle) = start_mock_server(200, "[]").await;
        let client = make_test_client(&base_url);

        let result = client.get_all_orders("fake-token").await;

        let orders = result.unwrap();
        assert!(orders.is_empty());

        handle.abort();
    }

    // -- 12. get_positions success --------------------------------------------

    #[tokio::test]
    async fn test_get_positions_success() {
        let body = r#"[
            {
                "securityId": "52432",
                "exchangeSegment": "NSE_FNO",
                "productType": "INTRADAY",
                "positionType": "LONG",
                "buyQty": 50,
                "sellQty": 0,
                "netQty": 50,
                "realizedProfit": 0.0,
                "unrealizedProfit": 125.50
            }
        ]"#;
        let (base_url, handle) = start_mock_server(200, body).await;
        let client = make_test_client(&base_url);

        let result = client.get_positions("fake-token").await;

        let positions = result.unwrap();
        assert_eq!(positions.len(), 1);
        assert_eq!(positions[0].security_id, "52432");
        assert_eq!(positions[0].position_type, "LONG");
        assert_eq!(positions[0].net_qty, 50);
        assert_eq!(positions[0].unrealized_profit, 125.50);

        handle.abort();
    }

    // -- 13. get_positions empty ----------------------------------------------

    #[tokio::test]
    async fn test_get_positions_empty() {
        let (base_url, handle) = start_mock_server(200, "[]").await;
        let client = make_test_client(&base_url);

        let result = client.get_positions("fake-token").await;

        let positions = result.unwrap();
        assert!(positions.is_empty());

        handle.abort();
    }

    // -- 14. get_holdings success ----------------------------------------------

    #[tokio::test]
    async fn test_get_holdings_success() {
        let body = r#"[
            {
                "exchange": "NSE",
                "tradingSymbol": "RELIANCE",
                "securityId": "2885",
                "isin": "INE002A01018",
                "totalQty": 10,
                "dpQty": 10,
                "t1Qty": 0,
                "availableQty": 10,
                "collateralQty": 0,
                "avgCostPrice": 2450.50
            }
        ]"#;
        let (base_url, handle) = start_mock_server(200, body).await;
        let client = make_test_client(&base_url);

        let result = client.get_holdings("fake-token").await;
        let holdings = result.unwrap();
        assert_eq!(holdings.len(), 1);
        assert_eq!(holdings[0].security_id, "2885");
        assert_eq!(holdings[0].trading_symbol, "RELIANCE");
        assert_eq!(holdings[0].total_qty, 10);
        assert_eq!(holdings[0].available_qty, 10);

        handle.abort();
    }

    // -- 15. get_holdings empty -----------------------------------------------

    #[tokio::test]
    async fn test_get_holdings_empty() {
        let (base_url, handle) = start_mock_server(200, "[]").await;
        let client = make_test_client(&base_url);

        let result = client.get_holdings("fake-token").await;
        assert!(result.unwrap().is_empty());

        handle.abort();
    }

    // -- 16. convert_position success -----------------------------------------

    #[tokio::test]
    async fn test_convert_position_success_202() {
        let (base_url, handle) = start_mock_server(202, "").await;
        let client = make_test_client(&base_url);

        let request = DhanConvertPositionRequest {
            dhan_client_id: "100".to_owned(),
            from_product_type: "INTRADAY".to_owned(),
            to_product_type: "CNC".to_owned(),
            exchange_segment: "NSE_EQ".to_owned(),
            position_type: "LONG".to_owned(),
            security_id: "2885".to_owned(),
            convert_qty: "10".to_owned(),
            trading_symbol: "RELIANCE".to_owned(),
        };

        let result = client.convert_position("fake-token", &request).await;
        assert!(result.is_ok());

        handle.abort();
    }

    // -- 17. convert_position rate limited ------------------------------------

    #[tokio::test]
    async fn test_convert_position_rate_limited_429() {
        let (base_url, handle) = start_mock_server(429, "{}").await;
        let client = make_test_client(&base_url);

        let request = DhanConvertPositionRequest {
            dhan_client_id: "100".to_owned(),
            from_product_type: "INTRADAY".to_owned(),
            to_product_type: "CNC".to_owned(),
            exchange_segment: "NSE_EQ".to_owned(),
            position_type: "LONG".to_owned(),
            security_id: "2885".to_owned(),
            convert_qty: "10".to_owned(),
            trading_symbol: "RELIANCE".to_owned(),
        };

        let result = client.convert_position("fake-token", &request).await;
        assert!(matches!(result.unwrap_err(), OmsError::DhanRateLimited));

        handle.abort();
    }

    // -- 18. exit_all_positions success ----------------------------------------

    #[tokio::test]
    async fn test_exit_all_positions_success() {
        let body = r#"{"status":"success","message":"All positions exited"}"#;
        let (base_url, handle) = start_mock_server(200, body).await;
        let client = make_test_client(&base_url);

        let result = client.exit_all_positions("fake-token").await;
        let resp = result.unwrap();
        assert_eq!(resp.status, "success");

        handle.abort();
    }

    // -- 19. calculate_margin success -----------------------------------------

    #[tokio::test]
    async fn test_calculate_margin_success() {
        let body = r#"{
            "totalMargin": 12500.50,
            "spanMargin": 10000.00,
            "exposureMargin": 2500.50,
            "availableBalance": 50000.00,
            "insufficientBalance": 0.0,
            "leverage": "4.00"
        }"#;
        let (base_url, handle) = start_mock_server(200, body).await;
        let client = make_test_client(&base_url);

        let request = MarginCalculatorRequest {
            dhan_client_id: "100".to_owned(),
            exchange_segment: "NSE_FNO".to_owned(),
            transaction_type: "BUY".to_owned(),
            quantity: 50,
            product_type: "INTRADAY".to_owned(),
            security_id: "52432".to_owned(),
            price: 245.50,
            trigger_price: 0.0,
        };

        let result = client.calculate_margin("fake-token", &request).await;
        let resp = result.unwrap();
        assert_eq!(resp.total_margin, 12500.50);
        assert_eq!(resp.leverage, "4.00");
        assert_eq!(resp.insufficient_balance, 0.0);

        handle.abort();
    }

    // -- 20. calculate_multi_margin success ------------------------------------

    #[tokio::test]
    async fn test_calculate_multi_margin_success() {
        let body = r#"{
            "total_margin": "25000.50",
            "span_margin": "20000.00",
            "exposure_margin": "5000.50",
            "equity_margin": "15000.00",
            "fo_margin": "10000.50",
            "commodity_margin": "0.00",
            "currency": "0.00",
            "hedge_benefit": "3500.00"
        }"#;
        let (base_url, handle) = start_mock_server(200, body).await;
        let client = make_test_client(&base_url);

        let request = MultiMarginRequest {
            include_position: true,
            include_orders: false,
            scripts: vec![],
        };

        let result = client.calculate_multi_margin("fake-token", &request).await;
        let resp = result.unwrap();
        assert_eq!(resp.total_margin, "25000.50");
        assert_eq!(resp.hedge_benefit, "3500.00");

        handle.abort();
    }

    // -- 21. get_fund_limit success -------------------------------------------

    #[tokio::test]
    async fn test_get_fund_limit_success() {
        let body = r#"{
            "availabelBalance": 150000.50,
            "sodLimit": 200000.00,
            "collateralAmount": 50000.00,
            "receiveableAmount": 10000.00,
            "utilizedAmount": 60000.00,
            "blockedPayoutAmount": 0.00,
            "withdrawableBalance": 140000.50
        }"#;
        let (base_url, handle) = start_mock_server(200, body).await;
        let client = make_test_client(&base_url);

        let result = client.get_fund_limit("fake-token").await;
        let resp = result.unwrap();
        // Verify the typo field name works correctly
        assert_eq!(resp.availabel_balance, 150000.50);
        assert_eq!(resp.sod_limit, 200000.00);
        assert_eq!(resp.collateral_amount, 50000.00);
        assert_eq!(resp.withdrawable_balance, 140000.50);

        handle.abort();
    }

    // -- 22. get_fund_limit rate limited --------------------------------------

    #[tokio::test]
    async fn test_get_fund_limit_rate_limited() {
        let (base_url, handle) = start_mock_server(429, "{}").await;
        let client = make_test_client(&base_url);

        let result = client.get_fund_limit("fake-token").await;
        assert!(matches!(result.unwrap_err(), OmsError::DhanRateLimited));

        handle.abort();
    }

    // -- 23. get_holdings api error 401 ---------------------------------------

    #[tokio::test]
    async fn test_get_holdings_api_error_401() {
        let body = r#"{"errorCode":"DH-901","errorMessage":"auth failed"}"#;
        let (base_url, handle) = start_mock_server(401, body).await;
        let client = make_test_client(&base_url);

        let result = client.get_holdings("fake-token").await;
        match result.unwrap_err() {
            OmsError::DhanApiError {
                status_code,
                message,
            } => {
                assert_eq!(status_code, 401);
                assert!(message.contains("DH-901"));
            }
            other => panic!("expected DhanApiError, got: {other:?}"),
        }

        handle.abort();
    }

    // -- 24. calculate_margin api error 400 -----------------------------------

    #[tokio::test]
    async fn test_calculate_margin_api_error_400() {
        let body = r#"{"errorCode":"DH-905","errorMessage":"invalid input"}"#;
        let (base_url, handle) = start_mock_server(400, body).await;
        let client = make_test_client(&base_url);

        let request = MarginCalculatorRequest {
            dhan_client_id: "100".to_owned(),
            exchange_segment: "NSE_FNO".to_owned(),
            transaction_type: "BUY".to_owned(),
            quantity: 50,
            product_type: "INTRADAY".to_owned(),
            security_id: "52432".to_owned(),
            price: 245.50,
            trigger_price: 0.0,
        };

        let result = client.calculate_margin("fake-token", &request).await;
        match result.unwrap_err() {
            OmsError::DhanApiError {
                status_code,
                message,
            } => {
                assert_eq!(status_code, 400);
                assert!(message.contains("DH-905"));
            }
            other => panic!("expected DhanApiError, got: {other:?}"),
        }

        handle.abort();
    }

    // -- 25. place_order HTTP 400 bad request ----------------------------------

    #[tokio::test]
    async fn test_place_order_api_error_400() {
        let body = r#"{"errorCode":"DH-905","errorMessage":"invalid input"}"#;
        let (base_url, handle) = start_mock_server(400, body).await;
        let client = make_test_client(&base_url);

        let result = client
            .place_order("fake-token", &make_test_place_request())
            .await;

        match result.unwrap_err() {
            OmsError::DhanApiError {
                status_code,
                message,
            } => {
                assert_eq!(status_code, 400);
                assert!(message.contains("DH-905"));
            }
            other => panic!("expected DhanApiError, got: {other:?}"),
        }

        handle.abort();
    }

    // -- 26. place_order HTTP 401 unauthorized --------------------------------

    #[tokio::test]
    async fn test_place_order_api_error_401() {
        let body = r#"{"errorCode":"DH-901","errorMessage":"auth failed"}"#;
        let (base_url, handle) = start_mock_server(401, body).await;
        let client = make_test_client(&base_url);

        let result = client
            .place_order("fake-token", &make_test_place_request())
            .await;

        match result.unwrap_err() {
            OmsError::DhanApiError {
                status_code,
                message,
            } => {
                assert_eq!(status_code, 401);
                assert!(message.contains("DH-901"));
            }
            other => panic!("expected DhanApiError, got: {other:?}"),
        }

        handle.abort();
    }

    // -- 27. modify_order HTTP 400 bad request --------------------------------

    #[tokio::test]
    async fn test_modify_order_api_error_400() {
        let body = r#"{"errorCode":"DH-905","errorMessage":"bad field"}"#;
        let (base_url, handle) = start_mock_server(400, body).await;
        let client = make_test_client(&base_url);

        let result = client
            .modify_order("fake-token", "ORD-1", &make_test_modify_request())
            .await;

        match result.unwrap_err() {
            OmsError::DhanApiError {
                status_code,
                message,
            } => {
                assert_eq!(status_code, 400);
                assert!(message.contains("DH-905"));
            }
            other => panic!("expected DhanApiError, got: {other:?}"),
        }

        handle.abort();
    }

    // -- 28. modify_order HTTP 401 unauthorized -------------------------------

    #[tokio::test]
    async fn test_modify_order_api_error_401() {
        let body = r#"{"errorCode":"DH-901","errorMessage":"auth failed"}"#;
        let (base_url, handle) = start_mock_server(401, body).await;
        let client = make_test_client(&base_url);

        let result = client
            .modify_order("fake-token", "ORD-1", &make_test_modify_request())
            .await;

        match result.unwrap_err() {
            OmsError::DhanApiError {
                status_code,
                message,
            } => {
                assert_eq!(status_code, 401);
                assert!(message.contains("DH-901"));
            }
            other => panic!("expected DhanApiError, got: {other:?}"),
        }

        handle.abort();
    }

    // -- 29. modify_order HTTP 500 internal server error ----------------------

    #[tokio::test]
    async fn test_modify_order_api_error_500() {
        let body = r#"{"errorCode":"DH-908","errorMessage":"internal error"}"#;
        let (base_url, handle) = start_mock_server(500, body).await;
        let client = make_test_client(&base_url);

        let result = client
            .modify_order("fake-token", "ORD-1", &make_test_modify_request())
            .await;

        match result.unwrap_err() {
            OmsError::DhanApiError {
                status_code,
                message,
            } => {
                assert_eq!(status_code, 500);
                assert!(message.contains("DH-908"));
            }
            other => panic!("expected DhanApiError, got: {other:?}"),
        }

        handle.abort();
    }

    // -- 30. cancel_order HTTP 400 bad request --------------------------------

    #[tokio::test]
    async fn test_cancel_order_api_error_400() {
        let body = r#"{"errorCode":"DH-906","errorMessage":"order error"}"#;
        let (base_url, handle) = start_mock_server(400, body).await;
        let client = make_test_client(&base_url);

        let result = client.cancel_order("fake-token", "ORD-1").await;

        match result.unwrap_err() {
            OmsError::DhanApiError {
                status_code,
                message,
            } => {
                assert_eq!(status_code, 400);
                assert!(message.contains("DH-906"));
            }
            other => panic!("expected DhanApiError, got: {other:?}"),
        }

        handle.abort();
    }

    // -- 31. cancel_order HTTP 401 unauthorized -------------------------------

    #[tokio::test]
    async fn test_cancel_order_api_error_401() {
        let body = r#"{"errorCode":"DH-901","errorMessage":"unauthorized"}"#;
        let (base_url, handle) = start_mock_server(401, body).await;
        let client = make_test_client(&base_url);

        let result = client.cancel_order("fake-token", "ORD-1").await;

        match result.unwrap_err() {
            OmsError::DhanApiError {
                status_code,
                message,
            } => {
                assert_eq!(status_code, 401);
                assert!(message.contains("DH-901"));
            }
            other => panic!("expected DhanApiError, got: {other:?}"),
        }

        handle.abort();
    }

    // -- 32. cancel_order rate limited 429 ------------------------------------

    #[tokio::test]
    async fn test_cancel_order_rate_limited_429() {
        let (base_url, handle) = start_mock_server(429, "{}").await;
        let client = make_test_client(&base_url);

        let result = client.cancel_order("fake-token", "ORD-1").await;

        assert!(matches!(result.unwrap_err(), OmsError::DhanRateLimited));

        handle.abort();
    }

    // -- 33. cancel_order HTTP 500 internal server error ----------------------

    #[tokio::test]
    async fn test_cancel_order_api_error_500() {
        let body = r#"{"errorCode":"DH-908","errorMessage":"server down"}"#;
        let (base_url, handle) = start_mock_server(500, body).await;
        let client = make_test_client(&base_url);

        let result = client.cancel_order("fake-token", "ORD-1").await;

        match result.unwrap_err() {
            OmsError::DhanApiError {
                status_code,
                message,
            } => {
                assert_eq!(status_code, 500);
                assert!(message.contains("DH-908"));
            }
            other => panic!("expected DhanApiError, got: {other:?}"),
        }

        handle.abort();
    }

    // -- 34. place_order malformed JSON on 200 — alternative body ------------

    #[tokio::test]
    async fn test_place_order_malformed_json_empty_body() {
        let (base_url, handle) = start_mock_server(200, "").await;
        let client = make_test_client(&base_url);

        let result = client
            .place_order("fake-token", &make_test_place_request())
            .await;

        assert!(matches!(result.unwrap_err(), OmsError::JsonError(_)));

        handle.abort();
    }

    // -- 35. transport/network error — connection refused ---------------------

    #[tokio::test]
    async fn test_place_order_transport_error_connection_refused() {
        // Point at a port where nobody is listening
        let client = make_test_client("http://127.0.0.1:1");

        let result = client
            .place_order("fake-token", &make_test_place_request())
            .await;

        assert!(
            matches!(result.unwrap_err(), OmsError::HttpError(_)),
            "connection refused must return HttpError"
        );
    }

    // -- 36. modify_order transport error — connection refused ----------------

    #[tokio::test]
    async fn test_modify_order_transport_error_connection_refused() {
        let client = make_test_client("http://127.0.0.1:1");

        let result = client
            .modify_order("fake-token", "ORD-1", &make_test_modify_request())
            .await;

        assert!(
            matches!(result.unwrap_err(), OmsError::HttpError(_)),
            "connection refused must return HttpError"
        );
    }

    // -- 37. cancel_order transport error — connection refused ----------------

    #[tokio::test]
    async fn test_cancel_order_transport_error_connection_refused() {
        let client = make_test_client("http://127.0.0.1:1");

        let result = client.cancel_order("fake-token", "ORD-1").await;

        assert!(
            matches!(result.unwrap_err(), OmsError::HttpError(_)),
            "connection refused must return HttpError"
        );
    }

    // -- 38. get_all_orders malformed JSON on 200 ----------------------------

    #[tokio::test]
    async fn test_get_all_orders_malformed_json() {
        let (base_url, handle) = start_mock_server(200, "not-json").await;
        let client = make_test_client(&base_url);

        let result = client.get_all_orders("fake-token").await;

        assert!(matches!(result.unwrap_err(), OmsError::JsonError(_)));

        handle.abort();
    }

    // -- 39. get_positions malformed JSON on 200 -----------------------------

    #[tokio::test]
    async fn test_get_positions_malformed_json() {
        let (base_url, handle) = start_mock_server(200, "{invalid}").await;
        let client = make_test_client(&base_url);

        let result = client.get_positions("fake-token").await;

        assert!(matches!(result.unwrap_err(), OmsError::JsonError(_)));

        handle.abort();
    }

    // -- 40. auth headers set correctly ---------------------------------------

    #[test]
    fn test_auth_headers_set_correctly() {
        let http = Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap();
        let client = OrderApiClient::new(
            http.clone(),
            "https://api.dhan.co/v2".to_owned(),
            "MY-CLIENT-ID".to_owned(),
        );

        // Build a dummy request and apply auth_headers
        let builder = http.get("https://api.dhan.co/v2/orders");
        let builder = client.auth_headers(builder, "my-jwt-token");

        // Build the request to inspect headers
        let request = builder.build().unwrap();
        let headers = request.headers();

        assert_eq!(
            headers.get("access-token").unwrap().to_str().unwrap(),
            "my-jwt-token"
        );
        assert_eq!(
            headers.get("client-id").unwrap().to_str().unwrap(),
            "MY-CLIENT-ID"
        );
        assert_eq!(
            headers.get("Content-Type").unwrap().to_str().unwrap(),
            "application/json"
        );
        assert_eq!(
            headers.get("Accept").unwrap().to_str().unwrap(),
            "application/json"
        );
    }
}
