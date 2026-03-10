//! Dhan REST API client for order management.
//!
//! Typed wrappers around the Dhan order REST endpoints.
//! Cold path — order submission is ~1-100/day. Allocations acceptable.
//!
//! # Endpoints
//! - `POST   /orders`            — Place order
//! - `PUT    /orders/{orderId}`   — Modify order
//! - `DELETE /orders/{orderId}`   — Cancel order
//! - `GET    /orders/{orderId}`   — Get order by ID
//! - `GET    /orders`             — Get all orders (today)
//! - `GET    /positions`          — Get positions
//!
//! # Authentication
//! Dhan uses custom headers, NOT `Authorization: Bearer`:
//! - `access-token: <JWT>`
//! - `client-id: <Dhan client ID>`

use reqwest::{Client, RequestBuilder};
use tracing::{debug, warn};

use super::types::{
    DhanModifyOrderRequest, DhanOrderResponse, DhanPlaceOrderRequest, DhanPlaceOrderResponse,
    DhanPositionResponse, OmsError,
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
}
