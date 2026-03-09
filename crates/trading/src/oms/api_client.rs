//! Dhan REST API client for order management.
//!
//! Typed wrappers around the Dhan order REST endpoints.
//! Cold path — order submission is ~1-100/day. Allocations acceptable.
//!
//! # Endpoints (Phase 1 spec §9)
//! - `POST   /v2/orders`            — Place order
//! - `PUT    /v2/orders/{orderId}`   — Modify order
//! - `DELETE /v2/orders/{orderId}`   — Cancel order
//! - `GET    /v2/orders/{orderId}`   — Get order by ID
//! - `GET    /v2/orders`             — Get all orders (today)
//! - `GET    /v2/positions`          — Get positions

use reqwest::Client;
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
/// Holds a shared `reqwest::Client` (connection pool) and the base URL.
/// Methods accept `access_token` as a parameter — token management
/// is handled by the OMS engine layer above.
pub struct OrderApiClient {
    /// Shared HTTP client (connection pool).
    http: Client,
    /// Dhan REST API base URL (e.g., `https://api.dhan.co/v2`).
    base_url: String,
}

impl OrderApiClient {
    /// Creates a new API client.
    ///
    /// # Arguments
    /// * `http` — Shared reqwest client.
    /// * `base_url` — Dhan REST API base URL from config.
    pub fn new(http: Client, base_url: String) -> Self {
        Self { http, base_url }
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
            .http
            .post(&url)
            .header("Content-Type", "application/json")
            .bearer_auth(access_token)
            .json(request)
            .send()
            .await
            .map_err(|err| OmsError::HttpError(err.to_string()))?;

        let status = response.status().as_u16();
        if status == HTTP_TOO_MANY_REQUESTS {
            warn!("Dhan rate limited (HTTP 429) — SEBI violation risk, backing off");
            return Err(OmsError::DhanRateLimited);
        }

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
            .http
            .put(&url)
            .header("Content-Type", "application/json")
            .bearer_auth(access_token)
            .json(request)
            .send()
            .await
            .map_err(|err| OmsError::HttpError(err.to_string()))?;

        let status = response.status().as_u16();
        if status == HTTP_TOO_MANY_REQUESTS {
            warn!("Dhan rate limited (HTTP 429) on modify — backing off");
            return Err(OmsError::DhanRateLimited);
        }

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
            .http
            .delete(&url)
            .bearer_auth(access_token)
            .send()
            .await
            .map_err(|err| OmsError::HttpError(err.to_string()))?;

        let status = response.status().as_u16();
        if status == HTTP_TOO_MANY_REQUESTS {
            warn!("Dhan rate limited (HTTP 429) on cancel — backing off");
            return Err(OmsError::DhanRateLimited);
        }

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
            .http
            .get(&url)
            .bearer_auth(access_token)
            .send()
            .await
            .map_err(|err| OmsError::HttpError(err.to_string()))?;

        self.handle_get_response(response).await
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
            .http
            .get(&url)
            .bearer_auth(access_token)
            .send()
            .await
            .map_err(|err| OmsError::HttpError(err.to_string()))?;

        let status = response.status().as_u16();
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
            .http
            .get(&url)
            .bearer_auth(access_token)
            .send()
            .await
            .map_err(|err| OmsError::HttpError(err.to_string()))?;

        let status = response.status().as_u16();
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

    async fn handle_get_response(
        &self,
        response: reqwest::Response,
    ) -> Result<DhanOrderResponse, OmsError> {
        let status = response.status().as_u16();
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
        let client = OrderApiClient::new(http, "https://api.dhan.co/v2".to_owned());
        assert_eq!(client.base_url, "https://api.dhan.co/v2");
    }

    #[test]
    fn http_too_many_requests_constant() {
        assert_eq!(HTTP_TOO_MANY_REQUESTS, 429);
    }
}
