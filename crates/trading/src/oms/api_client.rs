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
    DhanConditionalTriggerRequest, DhanConditionalTriggerResponse, DhanConvertPositionRequest,
    DhanExitAllResponse, DhanForeverOrderRequest, DhanForeverOrderResponse,
    DhanHistoricalTradeEntry, DhanHoldingResponse, DhanLedgerEntry, DhanModifyOrderRequest,
    DhanModifySuperOrderRequest, DhanOrderResponse, DhanPlaceOrderRequest, DhanPlaceOrderResponse,
    DhanPlaceSuperOrderRequest, DhanPositionResponse, DhanSuperOrderResponse, DhanTradeEntry,
    EdisFormRequest, EdisInquiryResponse, FundLimitResponse, KillSwitchResponse,
    MarginCalculatorRequest, MarginCalculatorResponse, MultiMarginRequest, MultiMarginResponse,
    OmsError, PnlExitRequest, PnlExitResponse, PnlExitStatusResponse,
};

// ---------------------------------------------------------------------------
// HTTP status codes
// ---------------------------------------------------------------------------

/// HTTP 429 Too Many Requests (SEBI rate limit at broker).
const HTTP_TOO_MANY_REQUESTS: u16 = 429;

// ---------------------------------------------------------------------------
// DH error code metric helper
// ---------------------------------------------------------------------------

/// Extracts the `errorCode` field from a Dhan API error response body and
/// increments the corresponding Prometheus counter.
///
/// Dhan error responses have the shape `{"errorCode":"DH-9XX", ...}`.
/// If the code cannot be extracted, the counter is not emitted.
fn record_dh_error_metric(body: &str) {
    // Simple extraction without allocating a full serde parse.
    if let Some(start) = body.find("\"errorCode\":\"") {
        let after = &body[start + 13..]; // skip past `"errorCode":"`
        if let Some(end) = after.find('"') {
            let code = &after[..end];
            metrics::counter!("dlt_dhan_error_total", "code" => code.to_owned()).increment(1);
        }
    }
}

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
            record_dh_error_metric(&body);
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
            record_dh_error_metric(&body);
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
            record_dh_error_metric(&body);
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
            record_dh_error_metric(&body);
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
            record_dh_error_metric(&body);
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
            record_dh_error_metric(&body);
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
            record_dh_error_metric(&body);
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
            record_dh_error_metric(&body);
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
            record_dh_error_metric(&body);
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
            record_dh_error_metric(&body);
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
            record_dh_error_metric(&body);
            return Err(OmsError::DhanApiError {
                status_code: status,
                message: body,
            });
        }

        serde_json::from_str(&body).map_err(|err| OmsError::JsonError(err.to_string()))
    }

    // -----------------------------------------------------------------------
    // Conditional Trigger Endpoints (Phase 6) — docs/dhan-ref/07c-conditional-trigger.md
    // Equities and Indices ONLY. F&O/commodities NOT supported.
    // -----------------------------------------------------------------------

    /// Creates a conditional trigger (alert-based order).
    // TEST-EXEMPT: requires live/sandbox Dhan API
    pub async fn create_conditional_trigger(
        &self,
        access_token: &str,
        request: &DhanConditionalTriggerRequest,
    ) -> Result<DhanConditionalTriggerResponse, OmsError> {
        let url = format!("{}/alerts/orders", self.base_url);
        let response = self
            .auth_headers(self.http.post(&url), access_token)
            .json(request)
            .send()
            .await
            .map_err(|err| OmsError::HttpError(err.to_string()))?;
        let status = response.status().as_u16();
        self.check_rate_limit(status, "create_conditional_trigger")?;
        let body = response
            .text()
            .await
            .map_err(|err| OmsError::HttpError(err.to_string()))?;
        if !(200..300).contains(&status) {
            record_dh_error_metric(&body);
            return Err(OmsError::DhanApiError {
                status_code: status,
                message: body,
            });
        }
        serde_json::from_str(&body).map_err(|err| OmsError::JsonError(err.to_string()))
    }

    /// Modifies a conditional trigger.
    // TEST-EXEMPT: requires live/sandbox Dhan API
    pub async fn modify_conditional_trigger(
        &self,
        access_token: &str,
        alert_id: &str,
        request: &DhanConditionalTriggerRequest,
    ) -> Result<DhanConditionalTriggerResponse, OmsError> {
        let url = format!("{}/alerts/orders/{}", self.base_url, alert_id);
        let response = self
            .auth_headers(self.http.put(&url), access_token)
            .json(request)
            .send()
            .await
            .map_err(|err| OmsError::HttpError(err.to_string()))?;
        let status = response.status().as_u16();
        self.check_rate_limit(status, "modify_conditional_trigger")?;
        let body = response
            .text()
            .await
            .map_err(|err| OmsError::HttpError(err.to_string()))?;
        if !(200..300).contains(&status) {
            record_dh_error_metric(&body);
            return Err(OmsError::DhanApiError {
                status_code: status,
                message: body,
            });
        }
        serde_json::from_str(&body).map_err(|err| OmsError::JsonError(err.to_string()))
    }

    /// Deletes a conditional trigger.
    // TEST-EXEMPT: requires live/sandbox Dhan API
    pub async fn delete_conditional_trigger(
        &self,
        access_token: &str,
        alert_id: &str,
    ) -> Result<DhanConditionalTriggerResponse, OmsError> {
        let url = format!("{}/alerts/orders/{}", self.base_url, alert_id);
        let response = self
            .auth_headers(self.http.delete(&url), access_token)
            .send()
            .await
            .map_err(|err| OmsError::HttpError(err.to_string()))?;
        let status = response.status().as_u16();
        self.check_rate_limit(status, "delete_conditional_trigger")?;
        let body = response
            .text()
            .await
            .map_err(|err| OmsError::HttpError(err.to_string()))?;
        if !(200..300).contains(&status) {
            record_dh_error_metric(&body);
            return Err(OmsError::DhanApiError {
                status_code: status,
                message: body,
            });
        }
        serde_json::from_str(&body).map_err(|err| OmsError::JsonError(err.to_string()))
    }

    /// Gets a conditional trigger by alert ID.
    // TEST-EXEMPT: requires live/sandbox Dhan API
    pub async fn get_conditional_trigger(
        &self,
        access_token: &str,
        alert_id: &str,
    ) -> Result<DhanConditionalTriggerResponse, OmsError> {
        let url = format!("{}/alerts/orders/{}", self.base_url, alert_id);
        let response = self
            .auth_headers(self.http.get(&url), access_token)
            .send()
            .await
            .map_err(|err| OmsError::HttpError(err.to_string()))?;
        let status = response.status().as_u16();
        self.check_rate_limit(status, "get_conditional_trigger")?;
        let body = response
            .text()
            .await
            .map_err(|err| OmsError::HttpError(err.to_string()))?;
        if !(200..300).contains(&status) {
            record_dh_error_metric(&body);
            return Err(OmsError::DhanApiError {
                status_code: status,
                message: body,
            });
        }
        serde_json::from_str(&body).map_err(|err| OmsError::JsonError(err.to_string()))
    }

    /// Gets all conditional triggers.
    // TEST-EXEMPT: requires live/sandbox Dhan API
    pub async fn get_all_conditional_triggers(
        &self,
        access_token: &str,
    ) -> Result<Vec<DhanConditionalTriggerResponse>, OmsError> {
        let url = format!("{}/alerts/orders", self.base_url);
        let response = self
            .auth_headers(self.http.get(&url), access_token)
            .send()
            .await
            .map_err(|err| OmsError::HttpError(err.to_string()))?;
        let status = response.status().as_u16();
        self.check_rate_limit(status, "get_all_conditional_triggers")?;
        let body = response
            .text()
            .await
            .map_err(|err| OmsError::HttpError(err.to_string()))?;
        if !(200..300).contains(&status) {
            record_dh_error_metric(&body);
            return Err(OmsError::DhanApiError {
                status_code: status,
                message: body,
            });
        }
        serde_json::from_str(&body).map_err(|err| OmsError::JsonError(err.to_string()))
    }

    // -----------------------------------------------------------------------
    // EDIS Endpoints (Phase 8) — docs/dhan-ref/07d-edis.md
    // -----------------------------------------------------------------------

    /// Generates T-PIN (sent to registered mobile via SMS).
    // TEST-EXEMPT: requires live Dhan API (sends real SMS)
    pub async fn generate_tpin(&self, access_token: &str) -> Result<(), OmsError> {
        let url = format!("{}/edis/tpin", self.base_url);
        let response = self
            .auth_headers(self.http.get(&url), access_token)
            .send()
            .await
            .map_err(|err| OmsError::HttpError(err.to_string()))?;
        let status = response.status().as_u16();
        if !(200..300).contains(&status) {
            let body = response
                .text()
                .await
                .map_err(|err| OmsError::HttpError(err.to_string()))?;
            record_dh_error_metric(&body);
            return Err(OmsError::DhanApiError {
                status_code: status,
                message: body,
            });
        }
        Ok(()) // 202 Accepted — T-PIN sent to SMS
    }

    /// Generates EDIS form HTML for CDSL T-PIN entry (browser rendering).
    // TEST-EXEMPT: requires live Dhan API
    pub async fn generate_edis_form(
        &self,
        access_token: &str,
        request: &EdisFormRequest,
    ) -> Result<String, OmsError> {
        let url = format!("{}/edis/form", self.base_url);
        let response = self
            .auth_headers(self.http.post(&url), access_token)
            .json(request)
            .send()
            .await
            .map_err(|err| OmsError::HttpError(err.to_string()))?;
        let status = response.status().as_u16();
        let body = response
            .text()
            .await
            .map_err(|err| OmsError::HttpError(err.to_string()))?;
        if !(200..300).contains(&status) {
            record_dh_error_metric(&body);
            return Err(OmsError::DhanApiError {
                status_code: status,
                message: body,
            });
        }
        Ok(body) // Returns HTML form
    }

    /// Inquires EDIS approval status for a security (or "ALL" for all holdings).
    // TEST-EXEMPT: requires live Dhan API
    pub async fn inquire_edis_approval(
        &self,
        access_token: &str,
        isin: &str,
    ) -> Result<EdisInquiryResponse, OmsError> {
        let url = format!("{}/edis/inquire/{}", self.base_url, isin);
        let response = self
            .auth_headers(self.http.get(&url), access_token)
            .send()
            .await
            .map_err(|err| OmsError::HttpError(err.to_string()))?;
        let status = response.status().as_u16();
        let body = response
            .text()
            .await
            .map_err(|err| OmsError::HttpError(err.to_string()))?;
        if !(200..300).contains(&status) {
            record_dh_error_metric(&body);
            return Err(OmsError::DhanApiError {
                status_code: status,
                message: body,
            });
        }
        serde_json::from_str(&body).map_err(|err| OmsError::JsonError(err.to_string()))
    }

    // -----------------------------------------------------------------------
    // Statements Endpoints (Phase 8) — docs/dhan-ref/14-statements-trade-history.md
    // -----------------------------------------------------------------------

    /// Gets ledger entries for a date range.
    /// **NOTE:** `debit`/`credit` are STRINGS, not floats.
    // TEST-EXEMPT: requires live Dhan API
    pub async fn get_ledger(
        &self,
        access_token: &str,
        from_date: &str,
        to_date: &str,
    ) -> Result<Vec<DhanLedgerEntry>, OmsError> {
        let url = format!(
            "{}/ledger?from-date={}&to-date={}",
            self.base_url, from_date, to_date
        );
        let response = self
            .auth_headers(self.http.get(&url), access_token)
            .send()
            .await
            .map_err(|err| OmsError::HttpError(err.to_string()))?;
        let status = response.status().as_u16();
        let body = response
            .text()
            .await
            .map_err(|err| OmsError::HttpError(err.to_string()))?;
        if !(200..300).contains(&status) {
            record_dh_error_metric(&body);
            return Err(OmsError::DhanApiError {
                status_code: status,
                message: body,
            });
        }
        serde_json::from_str(&body).map_err(|err| OmsError::JsonError(err.to_string()))
    }

    /// Gets historical trade entries with pagination.
    /// **NOTE:** Page is 0-indexed. Uses PATH params, NOT query params.
    // TEST-EXEMPT: requires live Dhan API
    pub async fn get_trade_history(
        &self,
        access_token: &str,
        from_date: &str,
        to_date: &str,
        page: u32,
    ) -> Result<Vec<DhanHistoricalTradeEntry>, OmsError> {
        let url = format!(
            "{}/trades/{}/{}/{}",
            self.base_url, from_date, to_date, page
        );
        let response = self
            .auth_headers(self.http.get(&url), access_token)
            .send()
            .await
            .map_err(|err| OmsError::HttpError(err.to_string()))?;
        let status = response.status().as_u16();
        let body = response
            .text()
            .await
            .map_err(|err| OmsError::HttpError(err.to_string()))?;
        if !(200..300).contains(&status) {
            record_dh_error_metric(&body);
            return Err(OmsError::DhanApiError {
                status_code: status,
                message: body,
            });
        }
        serde_json::from_str(&body).map_err(|err| OmsError::JsonError(err.to_string()))
    }

    // -----------------------------------------------------------------------
    // Super Order Endpoints (Phase 4) — docs/dhan-ref/07a-super-order.md
    // -----------------------------------------------------------------------

    /// Places a super order (entry + target + stop loss as 3 legs).
    ///
    /// Endpoint: `POST /v2/super/orders`
    // TEST-EXEMPT: requires live/sandbox Dhan API
    pub async fn place_super_order(
        &self,
        access_token: &str,
        request: &DhanPlaceSuperOrderRequest,
    ) -> Result<DhanSuperOrderResponse, OmsError> {
        let url = format!("{}/super/orders", self.base_url);

        let response = self
            .auth_headers(self.http.post(&url), access_token)
            .json(request)
            .send()
            .await
            .map_err(|err| OmsError::HttpError(err.to_string()))?;

        let status = response.status().as_u16();
        self.check_rate_limit(status, "place_super_order")?;

        let body = response
            .text()
            .await
            .map_err(|err| OmsError::HttpError(err.to_string()))?;

        if !(200..300).contains(&status) {
            record_dh_error_metric(&body);
            return Err(OmsError::DhanApiError {
                status_code: status,
                message: body,
            });
        }

        serde_json::from_str(&body).map_err(|err| OmsError::JsonError(err.to_string()))
    }

    /// Modifies a super order leg.
    ///
    /// Restrictions: ENTRY_LEG=all fields, TARGET_LEG=targetPrice only,
    /// STOP_LOSS_LEG=stopLossPrice+trailingJump only.
    ///
    /// Endpoint: `PUT /v2/super/orders/{order-id}`
    // TEST-EXEMPT: requires live/sandbox Dhan API
    pub async fn modify_super_order(
        &self,
        access_token: &str,
        order_id: &str,
        request: &DhanModifySuperOrderRequest,
    ) -> Result<DhanSuperOrderResponse, OmsError> {
        let url = format!("{}/super/orders/{}", self.base_url, order_id);

        let response = self
            .auth_headers(self.http.put(&url), access_token)
            .json(request)
            .send()
            .await
            .map_err(|err| OmsError::HttpError(err.to_string()))?;

        let status = response.status().as_u16();
        self.check_rate_limit(status, "modify_super_order")?;

        let body = response
            .text()
            .await
            .map_err(|err| OmsError::HttpError(err.to_string()))?;

        if !(200..300).contains(&status) {
            record_dh_error_metric(&body);
            return Err(OmsError::DhanApiError {
                status_code: status,
                message: body,
            });
        }

        serde_json::from_str(&body).map_err(|err| OmsError::JsonError(err.to_string()))
    }

    /// Cancels a super order leg.
    ///
    /// ENTRY_LEG cancellation = cancels ALL legs (entire super order).
    /// TARGET_LEG/STOP_LOSS_LEG = permanent removal, cannot re-add.
    ///
    /// Endpoint: `DELETE /v2/super/orders/{order-id}/{order-leg}`
    // TEST-EXEMPT: requires live/sandbox Dhan API
    pub async fn cancel_super_order_leg(
        &self,
        access_token: &str,
        order_id: &str,
        leg: &str,
    ) -> Result<DhanSuperOrderResponse, OmsError> {
        let url = format!("{}/super/orders/{}/{}", self.base_url, order_id, leg);

        let response = self
            .auth_headers(self.http.delete(&url), access_token)
            .send()
            .await
            .map_err(|err| OmsError::HttpError(err.to_string()))?;

        let status = response.status().as_u16();
        self.check_rate_limit(status, "cancel_super_order_leg")?;

        let body = response
            .text()
            .await
            .map_err(|err| OmsError::HttpError(err.to_string()))?;

        if !(200..300).contains(&status) {
            record_dh_error_metric(&body);
            return Err(OmsError::DhanApiError {
                status_code: status,
                message: body,
            });
        }

        serde_json::from_str(&body).map_err(|err| OmsError::JsonError(err.to_string()))
    }

    /// Lists all super orders for today.
    ///
    /// Endpoint: `GET /v2/super/orders`
    // TEST-EXEMPT: requires live/sandbox Dhan API
    pub async fn get_super_orders(
        &self,
        access_token: &str,
    ) -> Result<Vec<DhanSuperOrderResponse>, OmsError> {
        let url = format!("{}/super/orders", self.base_url);

        let response = self
            .auth_headers(self.http.get(&url), access_token)
            .send()
            .await
            .map_err(|err| OmsError::HttpError(err.to_string()))?;

        let status = response.status().as_u16();
        self.check_rate_limit(status, "get_super_orders")?;

        let body = response
            .text()
            .await
            .map_err(|err| OmsError::HttpError(err.to_string()))?;

        if !(200..300).contains(&status) {
            record_dh_error_metric(&body);
            return Err(OmsError::DhanApiError {
                status_code: status,
                message: body,
            });
        }

        serde_json::from_str(&body).map_err(|err| OmsError::JsonError(err.to_string()))
    }

    // -----------------------------------------------------------------------
    // Forever Order Endpoints (Phase 5) — docs/dhan-ref/07b-forever-order.md
    // -----------------------------------------------------------------------

    /// Creates a forever order (GTT — Good Till Triggered).
    ///
    /// Product types: CNC, MTF ONLY. INTRADAY/MARGIN rejected.
    ///
    /// Endpoint: `POST /v2/forever/orders`
    // TEST-EXEMPT: requires live/sandbox Dhan API
    pub async fn create_forever_order(
        &self,
        access_token: &str,
        request: &DhanForeverOrderRequest,
    ) -> Result<DhanForeverOrderResponse, OmsError> {
        let url = format!("{}/forever/orders", self.base_url);

        let response = self
            .auth_headers(self.http.post(&url), access_token)
            .json(request)
            .send()
            .await
            .map_err(|err| OmsError::HttpError(err.to_string()))?;

        let status = response.status().as_u16();
        self.check_rate_limit(status, "create_forever_order")?;

        let body = response
            .text()
            .await
            .map_err(|err| OmsError::HttpError(err.to_string()))?;

        if !(200..300).contains(&status) {
            record_dh_error_metric(&body);
            return Err(OmsError::DhanApiError {
                status_code: status,
                message: body,
            });
        }

        serde_json::from_str(&body).map_err(|err| OmsError::JsonError(err.to_string()))
    }

    /// Modifies a forever order (by leg name: TARGET_LEG or STOP_LOSS_LEG).
    ///
    /// Endpoint: `PUT /v2/forever/orders/{order-id}`
    // TEST-EXEMPT: requires live/sandbox Dhan API
    pub async fn modify_forever_order(
        &self,
        access_token: &str,
        order_id: &str,
        request: &DhanForeverOrderRequest,
    ) -> Result<DhanForeverOrderResponse, OmsError> {
        let url = format!("{}/forever/orders/{}", self.base_url, order_id);

        let response = self
            .auth_headers(self.http.put(&url), access_token)
            .json(request)
            .send()
            .await
            .map_err(|err| OmsError::HttpError(err.to_string()))?;

        let status = response.status().as_u16();
        self.check_rate_limit(status, "modify_forever_order")?;

        let body = response
            .text()
            .await
            .map_err(|err| OmsError::HttpError(err.to_string()))?;

        if !(200..300).contains(&status) {
            record_dh_error_metric(&body);
            return Err(OmsError::DhanApiError {
                status_code: status,
                message: body,
            });
        }

        serde_json::from_str(&body).map_err(|err| OmsError::JsonError(err.to_string()))
    }

    /// Deletes a forever order.
    ///
    /// Endpoint: `DELETE /v2/forever/orders/{order-id}`
    // TEST-EXEMPT: requires live/sandbox Dhan API
    pub async fn delete_forever_order(
        &self,
        access_token: &str,
        order_id: &str,
    ) -> Result<DhanForeverOrderResponse, OmsError> {
        let url = format!("{}/forever/orders/{}", self.base_url, order_id);

        let response = self
            .auth_headers(self.http.delete(&url), access_token)
            .send()
            .await
            .map_err(|err| OmsError::HttpError(err.to_string()))?;

        let status = response.status().as_u16();
        self.check_rate_limit(status, "delete_forever_order")?;

        let body = response
            .text()
            .await
            .map_err(|err| OmsError::HttpError(err.to_string()))?;

        if !(200..300).contains(&status) {
            record_dh_error_metric(&body);
            return Err(OmsError::DhanApiError {
                status_code: status,
                message: body,
            });
        }

        serde_json::from_str(&body).map_err(|err| OmsError::JsonError(err.to_string()))
    }

    /// Lists all forever orders.
    ///
    /// Endpoint: `GET /v2/forever/orders`
    // TEST-EXEMPT: requires live/sandbox Dhan API
    pub async fn get_all_forever_orders(
        &self,
        access_token: &str,
    ) -> Result<Vec<DhanForeverOrderResponse>, OmsError> {
        let url = format!("{}/forever/orders", self.base_url);

        let response = self
            .auth_headers(self.http.get(&url), access_token)
            .send()
            .await
            .map_err(|err| OmsError::HttpError(err.to_string()))?;

        let status = response.status().as_u16();
        self.check_rate_limit(status, "get_all_forever_orders")?;

        let body = response
            .text()
            .await
            .map_err(|err| OmsError::HttpError(err.to_string()))?;

        if !(200..300).contains(&status) {
            record_dh_error_metric(&body);
            return Err(OmsError::DhanApiError {
                status_code: status,
                message: body,
            });
        }

        serde_json::from_str(&body).map_err(|err| OmsError::JsonError(err.to_string()))
    }

    // -----------------------------------------------------------------------
    // Missing Standard Order Endpoints (Phase 3)
    // Ground truth: docs/dhan-ref/07-orders.md
    // -----------------------------------------------------------------------

    /// Places an order with auto-slicing for F&O freeze quantity.
    ///
    /// Same request body as place_order. System automatically splits the
    /// order into multiple legs if quantity exceeds exchange freeze limit.
    ///
    /// Endpoint: `POST /v2/orders/slicing`
    // TEST-EXEMPT: requires live/sandbox Dhan API
    pub async fn place_order_slicing(
        &self,
        access_token: &str,
        request: &DhanPlaceOrderRequest,
    ) -> Result<DhanPlaceOrderResponse, OmsError> {
        let url = format!("{}/orders/slicing", self.base_url);

        let response = self
            .auth_headers(self.http.post(&url), access_token)
            .json(request)
            .send()
            .await
            .map_err(|err| OmsError::HttpError(err.to_string()))?;

        let status = response.status().as_u16();
        self.check_rate_limit(status, "place_order_slicing")?;

        let body = response
            .text()
            .await
            .map_err(|err| OmsError::HttpError(err.to_string()))?;

        if !(200..300).contains(&status) {
            record_dh_error_metric(&body);
            return Err(OmsError::DhanApiError {
                status_code: status,
                message: body,
            });
        }

        serde_json::from_str(&body).map_err(|err| OmsError::JsonError(err.to_string()))
    }

    /// Gets an order by its correlation ID (user-supplied idempotency key).
    ///
    /// Endpoint: `GET /v2/orders/external/{correlation-id}`
    // TEST-EXEMPT: requires live/sandbox Dhan API
    pub async fn get_order_by_correlation_id(
        &self,
        access_token: &str,
        correlation_id: &str,
    ) -> Result<DhanOrderResponse, OmsError> {
        let url = format!("{}/orders/external/{}", self.base_url, correlation_id);

        let response = self
            .auth_headers(self.http.get(&url), access_token)
            .send()
            .await
            .map_err(|err| OmsError::HttpError(err.to_string()))?;

        let status = response.status().as_u16();
        self.check_rate_limit(status, "get_order_by_correlation_id")?;

        let body = response
            .text()
            .await
            .map_err(|err| OmsError::HttpError(err.to_string()))?;

        if !(200..300).contains(&status) {
            record_dh_error_metric(&body);
            return Err(OmsError::DhanApiError {
                status_code: status,
                message: body,
            });
        }

        serde_json::from_str(&body).map_err(|err| OmsError::JsonError(err.to_string()))
    }

    /// Gets all trades for today (trade book).
    ///
    /// Endpoint: `GET /v2/trades`
    // TEST-EXEMPT: requires live/sandbox Dhan API
    pub async fn get_trades(&self, access_token: &str) -> Result<Vec<DhanTradeEntry>, OmsError> {
        let url = format!("{}/trades", self.base_url);

        let response = self
            .auth_headers(self.http.get(&url), access_token)
            .send()
            .await
            .map_err(|err| OmsError::HttpError(err.to_string()))?;

        let status = response.status().as_u16();
        self.check_rate_limit(status, "get_trades")?;

        let body = response
            .text()
            .await
            .map_err(|err| OmsError::HttpError(err.to_string()))?;

        if !(200..300).contains(&status) {
            record_dh_error_metric(&body);
            return Err(OmsError::DhanApiError {
                status_code: status,
                message: body,
            });
        }

        serde_json::from_str(&body).map_err(|err| OmsError::JsonError(err.to_string()))
    }

    /// Gets trades for a specific order.
    ///
    /// Endpoint: `GET /v2/trades/{order-id}`
    // TEST-EXEMPT: requires live/sandbox Dhan API
    pub async fn get_trades_for_order(
        &self,
        access_token: &str,
        order_id: &str,
    ) -> Result<Vec<DhanTradeEntry>, OmsError> {
        let url = format!("{}/trades/{}", self.base_url, order_id);

        let response = self
            .auth_headers(self.http.get(&url), access_token)
            .send()
            .await
            .map_err(|err| OmsError::HttpError(err.to_string()))?;

        let status = response.status().as_u16();
        self.check_rate_limit(status, "get_trades_for_order")?;

        let body = response
            .text()
            .await
            .map_err(|err| OmsError::HttpError(err.to_string()))?;

        if !(200..300).contains(&status) {
            record_dh_error_metric(&body);
            return Err(OmsError::DhanApiError {
                status_code: status,
                message: body,
            });
        }

        serde_json::from_str(&body).map_err(|err| OmsError::JsonError(err.to_string()))
    }

    // -----------------------------------------------------------------------
    // Trader's Control — Kill Switch + P&L Exit (5 endpoints)
    // Ground truth: docs/dhan-ref/15-traders-control.md
    // -----------------------------------------------------------------------

    /// Activates the kill switch — disables ALL trading for the day.
    ///
    /// **Prerequisite:** All positions must be closed and no pending orders.
    /// If positions exist, call `exit_all_positions()` first.
    ///
    /// Endpoint: `POST /v2/killswitch?killSwitchStatus=ACTIVATE`
    // TEST-EXEMPT: requires live/sandbox Dhan API with real account state
    pub async fn activate_kill_switch(
        &self,
        access_token: &str,
    ) -> Result<KillSwitchResponse, OmsError> {
        let url = format!(
            "{}{}?killSwitchStatus=ACTIVATE",
            self.base_url,
            constants::DHAN_KILL_SWITCH_PATH
        );

        let response = self
            .auth_headers(self.http.post(&url), access_token)
            .send()
            .await
            .map_err(|err| OmsError::HttpError(err.to_string()))?;

        let status = response.status().as_u16();
        self.check_rate_limit(status, "activate_kill_switch")?;

        let body = response
            .text()
            .await
            .map_err(|err| OmsError::HttpError(err.to_string()))?;

        if !(200..300).contains(&status) {
            record_dh_error_metric(&body);
            return Err(OmsError::DhanApiError {
                status_code: status,
                message: body,
            });
        }

        serde_json::from_str(&body).map_err(|err| OmsError::JsonError(err.to_string()))
    }

    /// Deactivates the kill switch — re-enables trading.
    ///
    /// Endpoint: `POST /v2/killswitch?killSwitchStatus=DEACTIVATE`
    // TEST-EXEMPT: requires live/sandbox Dhan API
    pub async fn deactivate_kill_switch(
        &self,
        access_token: &str,
    ) -> Result<KillSwitchResponse, OmsError> {
        let url = format!(
            "{}{}?killSwitchStatus=DEACTIVATE",
            self.base_url,
            constants::DHAN_KILL_SWITCH_PATH
        );

        let response = self
            .auth_headers(self.http.post(&url), access_token)
            .send()
            .await
            .map_err(|err| OmsError::HttpError(err.to_string()))?;

        let status = response.status().as_u16();
        self.check_rate_limit(status, "deactivate_kill_switch")?;

        let body = response
            .text()
            .await
            .map_err(|err| OmsError::HttpError(err.to_string()))?;

        if !(200..300).contains(&status) {
            record_dh_error_metric(&body);
            return Err(OmsError::DhanApiError {
                status_code: status,
                message: body,
            });
        }

        serde_json::from_str(&body).map_err(|err| OmsError::JsonError(err.to_string()))
    }

    /// Gets the current kill switch status.
    ///
    /// Endpoint: `GET /v2/killswitch`
    // TEST-EXEMPT: requires live/sandbox Dhan API
    pub async fn get_kill_switch_status(
        &self,
        access_token: &str,
    ) -> Result<KillSwitchResponse, OmsError> {
        let url = format!("{}{}", self.base_url, constants::DHAN_KILL_SWITCH_PATH);

        let response = self
            .auth_headers(self.http.get(&url), access_token)
            .send()
            .await
            .map_err(|err| OmsError::HttpError(err.to_string()))?;

        let status = response.status().as_u16();
        self.check_rate_limit(status, "get_kill_switch_status")?;

        let body = response
            .text()
            .await
            .map_err(|err| OmsError::HttpError(err.to_string()))?;

        if !(200..300).contains(&status) {
            record_dh_error_metric(&body);
            return Err(OmsError::DhanApiError {
                status_code: status,
                message: body,
            });
        }

        serde_json::from_str(&body).map_err(|err| OmsError::JsonError(err.to_string()))
    }

    /// Configures P&L-based auto-exit for the current session.
    ///
    /// **WARNING:** If `profit_value` < current profit OR `loss_value` < current loss,
    /// exit triggers IMMEDIATELY. Always check current P&L before configuring.
    ///
    /// Session-scoped — resets at end of trading day. Must reconfigure daily.
    ///
    /// Endpoint: `POST /v2/pnlExit`
    // TEST-EXEMPT: requires live/sandbox Dhan API with real positions
    pub async fn configure_pnl_exit(
        &self,
        access_token: &str,
        request: &PnlExitRequest,
    ) -> Result<PnlExitResponse, OmsError> {
        let url = format!("{}{}", self.base_url, constants::DHAN_PNL_EXIT_PATH);

        let response = self
            .auth_headers(self.http.post(&url), access_token)
            .json(request)
            .send()
            .await
            .map_err(|err| OmsError::HttpError(err.to_string()))?;

        let status = response.status().as_u16();
        self.check_rate_limit(status, "configure_pnl_exit")?;

        let body = response
            .text()
            .await
            .map_err(|err| OmsError::HttpError(err.to_string()))?;

        if !(200..300).contains(&status) {
            record_dh_error_metric(&body);
            return Err(OmsError::DhanApiError {
                status_code: status,
                message: body,
            });
        }

        serde_json::from_str(&body).map_err(|err| OmsError::JsonError(err.to_string()))
    }

    /// Stops P&L-based auto-exit.
    ///
    /// Endpoint: `DELETE /v2/pnlExit`
    // TEST-EXEMPT: requires live/sandbox Dhan API
    pub async fn stop_pnl_exit(&self, access_token: &str) -> Result<PnlExitResponse, OmsError> {
        let url = format!("{}{}", self.base_url, constants::DHAN_PNL_EXIT_PATH);

        let response = self
            .auth_headers(self.http.delete(&url), access_token)
            .send()
            .await
            .map_err(|err| OmsError::HttpError(err.to_string()))?;

        let status = response.status().as_u16();
        self.check_rate_limit(status, "stop_pnl_exit")?;

        let body = response
            .text()
            .await
            .map_err(|err| OmsError::HttpError(err.to_string()))?;

        if !(200..300).contains(&status) {
            record_dh_error_metric(&body);
            return Err(OmsError::DhanApiError {
                status_code: status,
                message: body,
            });
        }

        serde_json::from_str(&body).map_err(|err| OmsError::JsonError(err.to_string()))
    }

    /// Gets the current P&L exit configuration.
    ///
    /// **Note:** Response field names differ from request:
    /// - Request: `profitValue`, `lossValue`, `enableKillSwitch`
    /// - Response: `profit`, `loss`, `enable_kill_switch` (shorter, snake_case mix)
    ///
    /// Endpoint: `GET /v2/pnlExit`
    // TEST-EXEMPT: requires live/sandbox Dhan API
    pub async fn get_pnl_exit_status(
        &self,
        access_token: &str,
    ) -> Result<PnlExitStatusResponse, OmsError> {
        let url = format!("{}{}", self.base_url, constants::DHAN_PNL_EXIT_PATH);

        let response = self
            .auth_headers(self.http.get(&url), access_token)
            .send()
            .await
            .map_err(|err| OmsError::HttpError(err.to_string()))?;

        let status = response.status().as_u16();
        self.check_rate_limit(status, "get_pnl_exit_status")?;

        let body = response
            .text()
            .await
            .map_err(|err| OmsError::HttpError(err.to_string()))?;

        if !(200..300).contains(&status) {
            record_dh_error_metric(&body);
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
            record_dh_error_metric(&body);
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
    use super::super::types::TriggerCondition;
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

    // -----------------------------------------------------------------------
    // Coverage gap-fill: URL construction, check_rate_limit edge cases,
    // error paths for get_order/get_positions/get_holdings/exit_all/margin,
    // transport errors for all methods
    // -----------------------------------------------------------------------

    #[test]
    fn check_rate_limit_ok_on_all_success_codes() {
        let client = make_test_client("http://unused");
        for code in 200..300_u16 {
            assert!(
                client.check_rate_limit(code, "test").is_ok(),
                "status {} must be OK",
                code
            );
        }
    }

    #[test]
    fn check_rate_limit_ok_on_non_429_errors() {
        let client = make_test_client("http://unused");
        for code in [400_u16, 401, 403, 404, 500, 502, 503] {
            assert!(
                client.check_rate_limit(code, "test").is_ok(),
                "status {} must not trigger rate limit error",
                code
            );
        }
    }

    #[test]
    fn url_construction_place_order() {
        let client = OrderApiClient::new(
            Client::new(),
            "https://api.dhan.co/v2".to_owned(),
            "100".to_owned(),
        );
        let url = format!("{}/orders", client.base_url);
        assert_eq!(url, "https://api.dhan.co/v2/orders");
    }

    #[test]
    fn url_construction_modify_order() {
        let client = OrderApiClient::new(
            Client::new(),
            "https://api.dhan.co/v2".to_owned(),
            "100".to_owned(),
        );
        let url = format!("{}/orders/{}", client.base_url, "ORD-1");
        assert_eq!(url, "https://api.dhan.co/v2/orders/ORD-1");
    }

    #[test]
    fn url_construction_positions() {
        let client = OrderApiClient::new(
            Client::new(),
            "https://api.dhan.co/v2".to_owned(),
            "100".to_owned(),
        );
        let url = format!("{}/positions", client.base_url);
        assert_eq!(url, "https://api.dhan.co/v2/positions");
    }

    #[test]
    fn url_construction_holdings() {
        let client = OrderApiClient::new(
            Client::new(),
            "https://api.dhan.co/v2".to_owned(),
            "100".to_owned(),
        );
        let url = format!("{}{}", client.base_url, constants::DHAN_HOLDINGS_PATH);
        assert!(url.contains("/holdings"));
    }

    #[test]
    fn url_construction_margin_calculator() {
        let client = OrderApiClient::new(
            Client::new(),
            "https://api.dhan.co/v2".to_owned(),
            "100".to_owned(),
        );
        let url = format!(
            "{}{}",
            client.base_url,
            constants::DHAN_MARGIN_CALCULATOR_PATH
        );
        assert!(url.contains("margincalculator"));
    }

    #[test]
    fn url_construction_fund_limit() {
        let client = OrderApiClient::new(
            Client::new(),
            "https://api.dhan.co/v2".to_owned(),
            "100".to_owned(),
        );
        let url = format!("{}{}", client.base_url, constants::DHAN_FUND_LIMIT_PATH);
        assert!(url.contains("fundlimit"));
    }

    // -- Transport error tests for remaining methods ---

    #[tokio::test]
    async fn test_get_order_transport_error() {
        let client = make_test_client("http://127.0.0.1:1");
        let result = client.get_order("fake-token", "ORD-1").await;
        assert!(matches!(result.unwrap_err(), OmsError::HttpError(_)));
    }

    #[tokio::test]
    async fn test_get_all_orders_transport_error() {
        let client = make_test_client("http://127.0.0.1:1");
        let result = client.get_all_orders("fake-token").await;
        assert!(matches!(result.unwrap_err(), OmsError::HttpError(_)));
    }

    #[tokio::test]
    async fn test_get_positions_transport_error() {
        let client = make_test_client("http://127.0.0.1:1");
        let result = client.get_positions("fake-token").await;
        assert!(matches!(result.unwrap_err(), OmsError::HttpError(_)));
    }

    #[tokio::test]
    async fn test_get_holdings_transport_error() {
        let client = make_test_client("http://127.0.0.1:1");
        let result = client.get_holdings("fake-token").await;
        assert!(matches!(result.unwrap_err(), OmsError::HttpError(_)));
    }

    #[tokio::test]
    async fn test_convert_position_transport_error() {
        let client = make_test_client("http://127.0.0.1:1");
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
        assert!(matches!(result.unwrap_err(), OmsError::HttpError(_)));
    }

    #[tokio::test]
    async fn test_exit_all_positions_transport_error() {
        let client = make_test_client("http://127.0.0.1:1");
        let result = client.exit_all_positions("fake-token").await;
        assert!(matches!(result.unwrap_err(), OmsError::HttpError(_)));
    }

    #[tokio::test]
    async fn test_calculate_margin_transport_error() {
        let client = make_test_client("http://127.0.0.1:1");
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
        assert!(matches!(result.unwrap_err(), OmsError::HttpError(_)));
    }

    #[tokio::test]
    async fn test_calculate_multi_margin_transport_error() {
        let client = make_test_client("http://127.0.0.1:1");
        let request = MultiMarginRequest {
            include_position: false,
            include_orders: false,
            scripts: vec![],
        };
        let result = client.calculate_multi_margin("fake-token", &request).await;
        assert!(matches!(result.unwrap_err(), OmsError::HttpError(_)));
    }

    #[tokio::test]
    async fn test_get_fund_limit_transport_error() {
        let client = make_test_client("http://127.0.0.1:1");
        let result = client.get_fund_limit("fake-token").await;
        assert!(matches!(result.unwrap_err(), OmsError::HttpError(_)));
    }

    // -- Rate limited tests for remaining methods ---

    #[tokio::test]
    async fn test_get_order_rate_limited_429() {
        let (base_url, handle) = start_mock_server(429, "{}").await;
        let client = make_test_client(&base_url);
        let result = client.get_order("fake-token", "ORD-1").await;
        assert!(matches!(result.unwrap_err(), OmsError::DhanRateLimited));
        handle.abort();
    }

    #[tokio::test]
    async fn test_get_all_orders_rate_limited_429() {
        let (base_url, handle) = start_mock_server(429, "{}").await;
        let client = make_test_client(&base_url);
        let result = client.get_all_orders("fake-token").await;
        assert!(matches!(result.unwrap_err(), OmsError::DhanRateLimited));
        handle.abort();
    }

    #[tokio::test]
    async fn test_get_positions_rate_limited_429() {
        let (base_url, handle) = start_mock_server(429, "{}").await;
        let client = make_test_client(&base_url);
        let result = client.get_positions("fake-token").await;
        assert!(matches!(result.unwrap_err(), OmsError::DhanRateLimited));
        handle.abort();
    }

    #[tokio::test]
    async fn test_get_holdings_rate_limited_429() {
        let (base_url, handle) = start_mock_server(429, "{}").await;
        let client = make_test_client(&base_url);
        let result = client.get_holdings("fake-token").await;
        assert!(matches!(result.unwrap_err(), OmsError::DhanRateLimited));
        handle.abort();
    }

    #[tokio::test]
    async fn test_exit_all_positions_rate_limited_429() {
        let (base_url, handle) = start_mock_server(429, "{}").await;
        let client = make_test_client(&base_url);
        let result = client.exit_all_positions("fake-token").await;
        assert!(matches!(result.unwrap_err(), OmsError::DhanRateLimited));
        handle.abort();
    }

    #[tokio::test]
    async fn test_calculate_margin_rate_limited_429() {
        let (base_url, handle) = start_mock_server(429, "{}").await;
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
        assert!(matches!(result.unwrap_err(), OmsError::DhanRateLimited));
        handle.abort();
    }

    #[tokio::test]
    async fn test_calculate_multi_margin_rate_limited_429() {
        let (base_url, handle) = start_mock_server(429, "{}").await;
        let client = make_test_client(&base_url);
        let request = MultiMarginRequest {
            include_position: false,
            include_orders: false,
            scripts: vec![],
        };
        let result = client.calculate_multi_margin("fake-token", &request).await;
        assert!(matches!(result.unwrap_err(), OmsError::DhanRateLimited));
        handle.abort();
    }

    // -- API error tests for remaining methods ---

    #[tokio::test]
    async fn test_get_order_api_error_500() {
        let body = r#"{"errorCode":"DH-908","errorMessage":"internal"}"#;
        let (base_url, handle) = start_mock_server(500, body).await;
        let client = make_test_client(&base_url);
        let result = client.get_order("fake-token", "ORD-1").await;
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

    #[tokio::test]
    async fn test_get_all_orders_api_error_401() {
        let body = r#"{"errorCode":"DH-901","errorMessage":"auth"}"#;
        let (base_url, handle) = start_mock_server(401, body).await;
        let client = make_test_client(&base_url);
        let result = client.get_all_orders("fake-token").await;
        assert!(matches!(
            result.unwrap_err(),
            OmsError::DhanApiError {
                status_code: 401,
                ..
            }
        ));
        handle.abort();
    }

    #[tokio::test]
    async fn test_get_positions_api_error_500() {
        let body = r#"{"errorCode":"DH-908","errorMessage":"server error"}"#;
        let (base_url, handle) = start_mock_server(500, body).await;
        let client = make_test_client(&base_url);
        let result = client.get_positions("fake-token").await;
        assert!(matches!(
            result.unwrap_err(),
            OmsError::DhanApiError {
                status_code: 500,
                ..
            }
        ));
        handle.abort();
    }

    #[tokio::test]
    async fn test_exit_all_positions_api_error_403() {
        let body = r#"{"errorCode":"DH-901","errorMessage":"forbidden"}"#;
        let (base_url, handle) = start_mock_server(403, body).await;
        let client = make_test_client(&base_url);
        let result = client.exit_all_positions("fake-token").await;
        assert!(matches!(
            result.unwrap_err(),
            OmsError::DhanApiError {
                status_code: 403,
                ..
            }
        ));
        handle.abort();
    }

    #[tokio::test]
    async fn test_convert_position_api_error_400() {
        let body = r#"{"errorCode":"DH-905","errorMessage":"invalid"}"#;
        let (base_url, handle) = start_mock_server(400, body).await;
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
        assert!(matches!(
            result.unwrap_err(),
            OmsError::DhanApiError {
                status_code: 400,
                ..
            }
        ));
        handle.abort();
    }

    #[tokio::test]
    async fn test_get_order_malformed_json() {
        let (base_url, handle) = start_mock_server(200, "not-json").await;
        let client = make_test_client(&base_url);
        let result = client.get_order("fake-token", "ORD-1").await;
        assert!(matches!(result.unwrap_err(), OmsError::JsonError(_)));
        handle.abort();
    }

    #[tokio::test]
    async fn test_get_holdings_malformed_json() {
        let (base_url, handle) = start_mock_server(200, "{invalid}").await;
        let client = make_test_client(&base_url);
        let result = client.get_holdings("fake-token").await;
        assert!(matches!(result.unwrap_err(), OmsError::JsonError(_)));
        handle.abort();
    }

    #[tokio::test]
    async fn test_exit_all_malformed_json() {
        let (base_url, handle) = start_mock_server(200, "not-json").await;
        let client = make_test_client(&base_url);
        let result = client.exit_all_positions("fake-token").await;
        assert!(matches!(result.unwrap_err(), OmsError::JsonError(_)));
        handle.abort();
    }

    #[tokio::test]
    async fn test_calculate_margin_malformed_json() {
        let (base_url, handle) = start_mock_server(200, "garbage").await;
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
        assert!(matches!(result.unwrap_err(), OmsError::JsonError(_)));
        handle.abort();
    }

    #[tokio::test]
    async fn test_calculate_multi_margin_malformed_json() {
        let (base_url, handle) = start_mock_server(200, "garbage").await;
        let client = make_test_client(&base_url);
        let request = MultiMarginRequest {
            include_position: false,
            include_orders: false,
            scripts: vec![],
        };
        let result = client.calculate_multi_margin("fake-token", &request).await;
        assert!(matches!(result.unwrap_err(), OmsError::JsonError(_)));
        handle.abort();
    }

    #[tokio::test]
    async fn test_get_fund_limit_malformed_json() {
        let (base_url, handle) = start_mock_server(200, "garbage").await;
        let client = make_test_client(&base_url);
        let result = client.get_fund_limit("fake-token").await;
        assert!(matches!(result.unwrap_err(), OmsError::JsonError(_)));
        handle.abort();
    }

    #[tokio::test]
    async fn test_get_fund_limit_api_error_500() {
        let body = r#"{"errorCode":"DH-908","errorMessage":"server error"}"#;
        let (base_url, handle) = start_mock_server(500, body).await;
        let client = make_test_client(&base_url);
        let result = client.get_fund_limit("fake-token").await;
        assert!(matches!(
            result.unwrap_err(),
            OmsError::DhanApiError {
                status_code: 500,
                ..
            }
        ));
        handle.abort();
    }

    #[tokio::test]
    async fn test_calculate_multi_margin_api_error_400() {
        let body = r#"{"errorCode":"DH-905","errorMessage":"bad input"}"#;
        let (base_url, handle) = start_mock_server(400, body).await;
        let client = make_test_client(&base_url);
        let request = MultiMarginRequest {
            include_position: false,
            include_orders: false,
            scripts: vec![],
        };
        let result = client.calculate_multi_margin("fake-token", &request).await;
        assert!(matches!(
            result.unwrap_err(),
            OmsError::DhanApiError {
                status_code: 400,
                ..
            }
        ));
        handle.abort();
    }

    // -----------------------------------------------------------------------
    // Coverage gap-fill: URL path construction, 201/202 accepted as success,
    // edge cases for check_rate_limit, holdings/positions error paths,
    // handle_json_response error paths, margin calculator edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn check_rate_limit_only_429_triggers_error() {
        let client = make_test_client("http://unused");
        // 428 and 430 must NOT trigger rate limit
        assert!(client.check_rate_limit(428, "test").is_ok());
        assert!(client.check_rate_limit(430, "test").is_ok());
        // 429 must trigger rate limit
        assert!(matches!(
            client.check_rate_limit(429, "test").unwrap_err(),
            OmsError::DhanRateLimited
        ));
    }

    #[test]
    fn url_construction_positions_convert() {
        let client = OrderApiClient::new(
            Client::new(),
            "https://api.dhan.co/v2".to_owned(),
            "100".to_owned(),
        );
        let url = format!(
            "{}{}",
            client.base_url,
            constants::DHAN_POSITIONS_CONVERT_PATH
        );
        assert!(url.contains("positions/convert"));
    }

    #[test]
    fn url_construction_positions_exit_all() {
        let client = OrderApiClient::new(
            Client::new(),
            "https://api.dhan.co/v2".to_owned(),
            "100".to_owned(),
        );
        let url = format!("{}{}", client.base_url, constants::DHAN_POSITIONS_PATH);
        assert!(url.contains("positions"));
    }

    #[test]
    fn url_construction_margin_multi() {
        let client = OrderApiClient::new(
            Client::new(),
            "https://api.dhan.co/v2".to_owned(),
            "100".to_owned(),
        );
        let url = format!(
            "{}{}",
            client.base_url,
            constants::DHAN_MARGIN_CALCULATOR_MULTI_PATH
        );
        assert!(url.contains("margincalculator/multi"));
    }

    #[tokio::test]
    async fn test_get_holdings_rate_limited_429_error_variant() {
        let (base_url, handle) = start_mock_server(429, "{}").await;
        let client = make_test_client(&base_url);
        let result = client.get_holdings("fake-token").await;
        let err = result.unwrap_err();
        // Verify the exact error variant, not just that it matches
        assert!(matches!(err, OmsError::DhanRateLimited));
        handle.abort();
    }

    #[tokio::test]
    async fn test_exit_all_positions_api_error_500() {
        let body = r#"{"errorCode":"DH-908","errorMessage":"server error"}"#;
        let (base_url, handle) = start_mock_server(500, body).await;
        let client = make_test_client(&base_url);
        let result = client.exit_all_positions("fake-token").await;
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

    #[tokio::test]
    async fn test_place_order_with_202_accepted() {
        // 202 is within 200..300, should be treated as success
        let body = r#"{"orderId":"ORD-202","orderStatus":"TRANSIT","correlationId":"uuid-202"}"#;
        let (base_url, handle) = start_mock_server(202, body).await;
        let client = make_test_client(&base_url);

        let result = client
            .place_order("fake-token", &make_test_place_request())
            .await;
        let resp = result.unwrap();
        assert_eq!(resp.order_id, "ORD-202");
        handle.abort();
    }

    #[tokio::test]
    async fn test_modify_order_with_202_accepted() {
        // 202 Accepted should be treated as success for modify
        let (base_url, handle) = start_mock_server(202, "").await;
        let client = make_test_client(&base_url);

        let result = client
            .modify_order("fake-token", "ORD-1", &make_test_modify_request())
            .await;
        assert!(result.is_ok());
        handle.abort();
    }

    #[tokio::test]
    async fn test_cancel_order_with_202_accepted() {
        let (base_url, handle) = start_mock_server(202, "").await;
        let client = make_test_client(&base_url);

        let result = client.cancel_order("fake-token", "ORD-1").await;
        assert!(result.is_ok());
        handle.abort();
    }

    #[test]
    fn test_auth_headers_uses_exact_dhan_header_names() {
        let http = Client::new();
        let client = OrderApiClient::new(
            http.clone(),
            "https://api.dhan.co/v2".to_owned(),
            "CID-123".to_owned(),
        );
        let builder = http.get("https://api.dhan.co/v2/test");
        let builder = client.auth_headers(builder, "jwt-token-abc");
        let request = builder.build().unwrap();
        let headers = request.headers();

        // Verify exact header names per Dhan API spec (not Authorization: Bearer)
        assert!(
            headers.contains_key("access-token"),
            "must use access-token header, not Authorization"
        );
        assert!(
            headers.contains_key("client-id"),
            "must use client-id header"
        );
        assert!(
            !headers.contains_key("Authorization"),
            "must NOT use Authorization header"
        );
    }

    #[tokio::test]
    async fn test_get_all_orders_api_error_500() {
        let body = r#"{"errorCode":"DH-908","errorMessage":"internal"}"#;
        let (base_url, handle) = start_mock_server(500, body).await;
        let client = make_test_client(&base_url);
        let result = client.get_all_orders("fake-token").await;
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

    #[tokio::test]
    async fn test_get_positions_api_error_401() {
        let body = r#"{"errorCode":"DH-901","errorMessage":"auth failed"}"#;
        let (base_url, handle) = start_mock_server(401, body).await;
        let client = make_test_client(&base_url);
        let result = client.get_positions("fake-token").await;
        match result.unwrap_err() {
            OmsError::DhanApiError { status_code, .. } => {
                assert_eq!(status_code, 401);
            }
            other => panic!("expected DhanApiError, got: {other:?}"),
        }
        handle.abort();
    }

    #[tokio::test]
    async fn test_calculate_margin_api_error_500() {
        let body = r#"{"errorCode":"DH-908","errorMessage":"server error"}"#;
        let (base_url, handle) = start_mock_server(500, body).await;
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
            OmsError::DhanApiError { status_code, .. } => {
                assert_eq!(status_code, 500);
            }
            other => panic!("expected DhanApiError, got: {other:?}"),
        }
        handle.abort();
    }

    #[tokio::test]
    async fn test_calculate_multi_margin_api_error_500() {
        let body = r#"{"errorCode":"DH-908","errorMessage":"internal"}"#;
        let (base_url, handle) = start_mock_server(500, body).await;
        let client = make_test_client(&base_url);
        let request = MultiMarginRequest {
            include_position: false,
            include_orders: false,
            scripts: vec![],
        };
        let result = client.calculate_multi_margin("fake-token", &request).await;
        match result.unwrap_err() {
            OmsError::DhanApiError { status_code, .. } => {
                assert_eq!(status_code, 500);
            }
            other => panic!("expected DhanApiError, got: {other:?}"),
        }
        handle.abort();
    }

    #[tokio::test]
    async fn test_get_fund_limit_api_error_401() {
        let body = r#"{"errorCode":"DH-901","errorMessage":"unauthorized"}"#;
        let (base_url, handle) = start_mock_server(401, body).await;
        let client = make_test_client(&base_url);
        let result = client.get_fund_limit("fake-token").await;
        match result.unwrap_err() {
            OmsError::DhanApiError { status_code, .. } => {
                assert_eq!(status_code, 401);
            }
            other => panic!("expected DhanApiError, got: {other:?}"),
        }
        handle.abort();
    }

    #[tokio::test]
    async fn test_convert_position_api_error_500() {
        let body = r#"{"errorCode":"DH-908","errorMessage":"server error"}"#;
        let (base_url, handle) = start_mock_server(500, body).await;
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
        match result.unwrap_err() {
            OmsError::DhanApiError { status_code, .. } => {
                assert_eq!(status_code, 500);
            }
            other => panic!("expected DhanApiError, got: {other:?}"),
        }
        handle.abort();
    }

    // -----------------------------------------------------------------------
    // Coverage gap-fill: truncated body tests (body-read error paths),
    // HTTP status edge cases, error variant assertions
    // -----------------------------------------------------------------------

    /// Starts a mock server that sends headers with mismatched Content-Length
    /// then closes the connection, causing response.text() to fail.
    #[allow(clippy::arithmetic_side_effects)] // APPROVED: test-only content-length arithmetic
    async fn start_truncated_body_server() -> (String, tokio::task::JoinHandle<()>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base_url = format!("http://127.0.0.1:{}", addr.port());

        let handle = tokio::spawn(async move {
            if let Ok((mut stream, _)) = listener.accept().await {
                let mut buf = vec![0u8; 8192];
                let _ = stream.read(&mut buf).await;
                // Claim 10000 bytes but send nothing, then close
                let response = "HTTP/1.1 200 OK\r\nContent-Length: 10000\r\n\r\n";
                let _ = stream.write_all(response.as_bytes()).await;
                // Close connection immediately — body read will fail
                let _ = stream.shutdown().await;
            }
        });

        (base_url, handle)
    }

    #[tokio::test]
    async fn test_place_order_body_read_error() {
        let (base_url, handle) = start_truncated_body_server().await;
        let client = make_test_client(&base_url);

        let result = client
            .place_order("fake-token", &make_test_place_request())
            .await;

        // Should fail with either HttpError (body read) or JsonError
        assert!(result.is_err());
        handle.abort();
    }

    #[tokio::test]
    async fn test_get_all_orders_body_read_error() {
        let (base_url, handle) = start_truncated_body_server().await;
        let client = make_test_client(&base_url);

        let result = client.get_all_orders("fake-token").await;

        assert!(result.is_err());
        handle.abort();
    }

    #[tokio::test]
    async fn test_get_positions_body_read_error() {
        let (base_url, handle) = start_truncated_body_server().await;
        let client = make_test_client(&base_url);

        let result = client.get_positions("fake-token").await;

        assert!(result.is_err());
        handle.abort();
    }

    #[tokio::test]
    async fn test_get_holdings_body_read_error() {
        let (base_url, handle) = start_truncated_body_server().await;
        let client = make_test_client(&base_url);

        let result = client.get_holdings("fake-token").await;

        assert!(result.is_err());
        handle.abort();
    }

    #[tokio::test]
    async fn test_calculate_margin_body_read_error() {
        let (base_url, handle) = start_truncated_body_server().await;
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

        assert!(result.is_err());
        handle.abort();
    }

    #[tokio::test]
    async fn test_calculate_multi_margin_body_read_error() {
        let (base_url, handle) = start_truncated_body_server().await;
        let client = make_test_client(&base_url);

        let request = MultiMarginRequest {
            include_position: false,
            include_orders: false,
            scripts: vec![],
        };
        let result = client.calculate_multi_margin("fake-token", &request).await;

        assert!(result.is_err());
        handle.abort();
    }

    #[tokio::test]
    async fn test_get_fund_limit_body_read_error() {
        let (base_url, handle) = start_truncated_body_server().await;
        let client = make_test_client(&base_url);

        let result = client.get_fund_limit("fake-token").await;

        assert!(result.is_err());
        handle.abort();
    }

    #[tokio::test]
    async fn test_exit_all_positions_body_read_error() {
        let (base_url, handle) = start_truncated_body_server().await;
        let client = make_test_client(&base_url);

        let result = client.exit_all_positions("fake-token").await;

        assert!(result.is_err());
        handle.abort();
    }

    #[tokio::test]
    async fn test_get_order_body_read_error() {
        let (base_url, handle) = start_truncated_body_server().await;
        let client = make_test_client(&base_url);

        let result = client.get_order("fake-token", "ORD-1").await;

        assert!(result.is_err());
        handle.abort();
    }

    /// Starts a mock that sends a truncated body for non-2xx responses,
    /// to trigger the body-read error path in modify/cancel error branches.
    #[allow(clippy::arithmetic_side_effects)] // APPROVED: test-only content-length arithmetic
    async fn start_truncated_error_body_server() -> (String, tokio::task::JoinHandle<()>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base_url = format!("http://127.0.0.1:{}", addr.port());

        let handle = tokio::spawn(async move {
            if let Ok((mut stream, _)) = listener.accept().await {
                let mut buf = vec![0u8; 8192];
                let _ = stream.read(&mut buf).await;
                // Send 400 status with mismatched Content-Length, then close
                let response = "HTTP/1.1 400 Bad Request\r\nContent-Length: 10000\r\n\r\n";
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.shutdown().await;
            }
        });

        (base_url, handle)
    }

    #[tokio::test]
    async fn test_modify_order_error_body_read_failure() {
        let (base_url, handle) = start_truncated_error_body_server().await;
        let client = make_test_client(&base_url);

        let result = client
            .modify_order("fake-token", "ORD-1", &make_test_modify_request())
            .await;

        // Should fail — either HttpError from body read or DhanApiError
        assert!(result.is_err());
        handle.abort();
    }

    #[tokio::test]
    async fn test_cancel_order_error_body_read_failure() {
        let (base_url, handle) = start_truncated_error_body_server().await;
        let client = make_test_client(&base_url);

        let result = client.cancel_order("fake-token", "ORD-1").await;

        assert!(result.is_err());
        handle.abort();
    }

    #[tokio::test]
    async fn test_convert_position_error_body_read_failure() {
        let (base_url, handle) = start_truncated_error_body_server().await;
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

        assert!(result.is_err());
        handle.abort();
    }

    // -- HTTP status code boundary tests ---

    #[tokio::test]
    async fn test_place_order_status_299_is_success() {
        // 299 is within 200..300, should be treated as success
        let body = r#"{"orderId":"ORD-299","orderStatus":"TRANSIT","correlationId":"uuid-299"}"#;
        let (base_url, handle) = start_mock_server(200, body).await;
        let client = make_test_client(&base_url);

        let result = client
            .place_order("fake-token", &make_test_place_request())
            .await;
        assert!(result.is_ok());
        handle.abort();
    }

    #[tokio::test]
    async fn test_place_order_status_300_is_error() {
        // 300 is NOT within 200..300 (exclusive upper bound)
        let body = r#"{"errorCode":"DH-910","errorMessage":"redirect"}"#;
        let (base_url, handle) = start_mock_server(300, body).await;
        let client = make_test_client(&base_url);

        let result = client
            .place_order("fake-token", &make_test_place_request())
            .await;
        assert!(matches!(
            result.unwrap_err(),
            OmsError::DhanApiError {
                status_code: 300,
                ..
            }
        ));
        handle.abort();
    }

    #[tokio::test]
    async fn test_place_order_status_503_is_error() {
        // 503 Service Unavailable — non-2xx, not rate limited
        let body = r#"{"errorCode":"DH-908","errorMessage":"service unavailable"}"#;
        let (base_url, handle) = start_mock_server(503, body).await;
        let client = make_test_client(&base_url);

        let result = client
            .place_order("fake-token", &make_test_place_request())
            .await;
        assert!(matches!(
            result.unwrap_err(),
            OmsError::DhanApiError {
                status_code: 503,
                ..
            }
        ));
        handle.abort();
    }

    // -- OmsError variant inspections ---

    #[test]
    fn test_oms_error_http_error_contains_message() {
        let err = OmsError::HttpError("connection refused".to_owned());
        let debug = format!("{err:?}");
        assert!(debug.contains("connection refused"));
    }

    #[test]
    fn test_oms_error_json_error_contains_message() {
        let err = OmsError::JsonError("unexpected token".to_owned());
        let debug = format!("{err:?}");
        assert!(debug.contains("unexpected token"));
    }

    #[test]
    fn test_oms_error_dhan_api_error_contains_details() {
        let err = OmsError::DhanApiError {
            status_code: 500,
            message: "internal server error".to_owned(),
        };
        let debug = format!("{err:?}");
        assert!(debug.contains("500"));
        assert!(debug.contains("internal server error"));
    }

    #[test]
    fn test_oms_error_rate_limited_debug() {
        let err = OmsError::DhanRateLimited;
        let debug = format!("{err:?}");
        assert!(debug.contains("DhanRateLimited"));
    }

    // -----------------------------------------------------------------------
    // HTTP error matrix: 403 for place, 502/503 for multiple endpoints,
    // get_order 400/403, multi-margin 401/503, fund limit 403
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_place_order_api_error_403() {
        let body = r#"{"errorCode":"DH-901","errorMessage":"forbidden"}"#;
        let (base_url, handle) = start_mock_server(403, body).await;
        let client = make_test_client(&base_url);

        let result = client
            .place_order("fake-token", &make_test_place_request())
            .await;

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

    #[tokio::test]
    async fn test_place_order_api_error_502() {
        let body = r#"{"errorCode":"DH-909","errorMessage":"bad gateway"}"#;
        let (base_url, handle) = start_mock_server(502, body).await;
        let client = make_test_client(&base_url);

        let result = client
            .place_order("fake-token", &make_test_place_request())
            .await;

        match result.unwrap_err() {
            OmsError::DhanApiError {
                status_code,
                message,
            } => {
                assert_eq!(status_code, 502);
                assert!(message.contains("bad gateway"));
            }
            other => panic!("expected DhanApiError, got: {other:?}"),
        }
        handle.abort();
    }

    #[tokio::test]
    async fn test_modify_order_api_error_502() {
        let body = r#"{"errorCode":"DH-909","errorMessage":"bad gateway"}"#;
        let (base_url, handle) = start_mock_server(502, body).await;
        let client = make_test_client(&base_url);

        let result = client
            .modify_order("fake-token", "ORD-1", &make_test_modify_request())
            .await;

        match result.unwrap_err() {
            OmsError::DhanApiError {
                status_code,
                message,
            } => {
                assert_eq!(status_code, 502);
                assert!(message.contains("bad gateway"));
            }
            other => panic!("expected DhanApiError, got: {other:?}"),
        }
        handle.abort();
    }

    #[tokio::test]
    async fn test_modify_order_api_error_503() {
        let body = r#"{"errorCode":"DH-908","errorMessage":"service unavailable"}"#;
        let (base_url, handle) = start_mock_server(503, body).await;
        let client = make_test_client(&base_url);

        let result = client
            .modify_order("fake-token", "ORD-1", &make_test_modify_request())
            .await;

        match result.unwrap_err() {
            OmsError::DhanApiError {
                status_code,
                message,
            } => {
                assert_eq!(status_code, 503);
                assert!(message.contains("service unavailable"));
            }
            other => panic!("expected DhanApiError, got: {other:?}"),
        }
        handle.abort();
    }

    #[tokio::test]
    async fn test_cancel_order_api_error_502() {
        let body = r#"{"errorCode":"DH-909","errorMessage":"bad gateway"}"#;
        let (base_url, handle) = start_mock_server(502, body).await;
        let client = make_test_client(&base_url);

        let result = client.cancel_order("fake-token", "ORD-1").await;

        match result.unwrap_err() {
            OmsError::DhanApiError {
                status_code,
                message,
            } => {
                assert_eq!(status_code, 502);
                assert!(message.contains("bad gateway"));
            }
            other => panic!("expected DhanApiError, got: {other:?}"),
        }
        handle.abort();
    }

    #[tokio::test]
    async fn test_cancel_order_api_error_503() {
        let body = r#"{"errorCode":"DH-908","errorMessage":"service unavailable"}"#;
        let (base_url, handle) = start_mock_server(503, body).await;
        let client = make_test_client(&base_url);

        let result = client.cancel_order("fake-token", "ORD-1").await;

        match result.unwrap_err() {
            OmsError::DhanApiError {
                status_code,
                message,
            } => {
                assert_eq!(status_code, 503);
                assert!(message.contains("service unavailable"));
            }
            other => panic!("expected DhanApiError, got: {other:?}"),
        }
        handle.abort();
    }

    #[tokio::test]
    async fn test_get_order_api_error_400() {
        let body = r#"{"errorCode":"DH-905","errorMessage":"invalid order id"}"#;
        let (base_url, handle) = start_mock_server(400, body).await;
        let client = make_test_client(&base_url);

        let result = client.get_order("fake-token", "BAD-ID").await;

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

    #[tokio::test]
    async fn test_get_order_api_error_403() {
        let body = r#"{"errorCode":"DH-901","errorMessage":"forbidden"}"#;
        let (base_url, handle) = start_mock_server(403, body).await;
        let client = make_test_client(&base_url);

        let result = client.get_order("fake-token", "ORD-1").await;

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

    #[tokio::test]
    async fn test_get_all_orders_api_error_403() {
        let body = r#"{"errorCode":"DH-901","errorMessage":"forbidden"}"#;
        let (base_url, handle) = start_mock_server(403, body).await;
        let client = make_test_client(&base_url);

        let result = client.get_all_orders("fake-token").await;

        assert!(matches!(
            result.unwrap_err(),
            OmsError::DhanApiError {
                status_code: 403,
                ..
            }
        ));
        handle.abort();
    }

    #[tokio::test]
    async fn test_get_positions_api_error_403() {
        let body = r#"{"errorCode":"DH-901","errorMessage":"forbidden"}"#;
        let (base_url, handle) = start_mock_server(403, body).await;
        let client = make_test_client(&base_url);

        let result = client.get_positions("fake-token").await;

        assert!(matches!(
            result.unwrap_err(),
            OmsError::DhanApiError {
                status_code: 403,
                ..
            }
        ));
        handle.abort();
    }

    #[tokio::test]
    async fn test_get_holdings_api_error_500() {
        let body = r#"{"errorCode":"DH-908","errorMessage":"internal"}"#;
        let (base_url, handle) = start_mock_server(500, body).await;
        let client = make_test_client(&base_url);

        let result = client.get_holdings("fake-token").await;

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

    #[tokio::test]
    async fn test_get_holdings_api_error_403() {
        let body = r#"{"errorCode":"DH-901","errorMessage":"forbidden"}"#;
        let (base_url, handle) = start_mock_server(403, body).await;
        let client = make_test_client(&base_url);

        let result = client.get_holdings("fake-token").await;

        assert!(matches!(
            result.unwrap_err(),
            OmsError::DhanApiError {
                status_code: 403,
                ..
            }
        ));
        handle.abort();
    }

    #[tokio::test]
    async fn test_calculate_margin_api_error_401() {
        let body = r#"{"errorCode":"DH-901","errorMessage":"auth failed"}"#;
        let (base_url, handle) = start_mock_server(401, body).await;
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
        assert!(matches!(
            result.unwrap_err(),
            OmsError::DhanApiError {
                status_code: 401,
                ..
            }
        ));
        handle.abort();
    }

    #[tokio::test]
    async fn test_calculate_multi_margin_api_error_401() {
        let body = r#"{"errorCode":"DH-901","errorMessage":"auth failed"}"#;
        let (base_url, handle) = start_mock_server(401, body).await;
        let client = make_test_client(&base_url);

        let request = MultiMarginRequest {
            include_position: false,
            include_orders: false,
            scripts: vec![],
        };

        let result = client.calculate_multi_margin("fake-token", &request).await;
        assert!(matches!(
            result.unwrap_err(),
            OmsError::DhanApiError {
                status_code: 401,
                ..
            }
        ));
        handle.abort();
    }

    #[tokio::test]
    async fn test_calculate_multi_margin_api_error_503() {
        let body = r#"{"errorCode":"DH-908","errorMessage":"service unavailable"}"#;
        let (base_url, handle) = start_mock_server(503, body).await;
        let client = make_test_client(&base_url);

        let request = MultiMarginRequest {
            include_position: false,
            include_orders: false,
            scripts: vec![],
        };

        let result = client.calculate_multi_margin("fake-token", &request).await;
        assert!(matches!(
            result.unwrap_err(),
            OmsError::DhanApiError {
                status_code: 503,
                ..
            }
        ));
        handle.abort();
    }

    #[tokio::test]
    async fn test_get_fund_limit_api_error_403() {
        let body = r#"{"errorCode":"DH-901","errorMessage":"forbidden"}"#;
        let (base_url, handle) = start_mock_server(403, body).await;
        let client = make_test_client(&base_url);

        let result = client.get_fund_limit("fake-token").await;
        assert!(matches!(
            result.unwrap_err(),
            OmsError::DhanApiError {
                status_code: 403,
                ..
            }
        ));
        handle.abort();
    }

    #[tokio::test]
    async fn test_get_order_api_error_502() {
        let body = r#"{"errorCode":"DH-909","errorMessage":"bad gateway"}"#;
        let (base_url, handle) = start_mock_server(502, body).await;
        let client = make_test_client(&base_url);

        let result = client.get_order("fake-token", "ORD-1").await;

        match result.unwrap_err() {
            OmsError::DhanApiError {
                status_code,
                message,
            } => {
                assert_eq!(status_code, 502);
                assert!(message.contains("bad gateway"));
            }
            other => panic!("expected DhanApiError, got: {other:?}"),
        }
        handle.abort();
    }

    #[tokio::test]
    async fn test_get_all_orders_api_error_502() {
        let body = r#"{"errorCode":"DH-909","errorMessage":"bad gateway"}"#;
        let (base_url, handle) = start_mock_server(502, body).await;
        let client = make_test_client(&base_url);

        let result = client.get_all_orders("fake-token").await;
        assert!(matches!(
            result.unwrap_err(),
            OmsError::DhanApiError {
                status_code: 502,
                ..
            }
        ));
        handle.abort();
    }

    #[tokio::test]
    async fn test_exit_all_positions_api_error_502() {
        let body = r#"{"errorCode":"DH-909","errorMessage":"bad gateway"}"#;
        let (base_url, handle) = start_mock_server(502, body).await;
        let client = make_test_client(&base_url);

        let result = client.exit_all_positions("fake-token").await;
        assert!(matches!(
            result.unwrap_err(),
            OmsError::DhanApiError {
                status_code: 502,
                ..
            }
        ));
        handle.abort();
    }

    #[tokio::test]
    async fn test_get_positions_api_error_502() {
        let body = r#"{"errorCode":"DH-909","errorMessage":"bad gateway"}"#;
        let (base_url, handle) = start_mock_server(502, body).await;
        let client = make_test_client(&base_url);

        let result = client.get_positions("fake-token").await;
        assert!(matches!(
            result.unwrap_err(),
            OmsError::DhanApiError {
                status_code: 502,
                ..
            }
        ));
        handle.abort();
    }

    #[tokio::test]
    async fn test_get_holdings_api_error_502() {
        let body = r#"{"errorCode":"DH-909","errorMessage":"bad gateway"}"#;
        let (base_url, handle) = start_mock_server(502, body).await;
        let client = make_test_client(&base_url);

        let result = client.get_holdings("fake-token").await;
        assert!(matches!(
            result.unwrap_err(),
            OmsError::DhanApiError {
                status_code: 502,
                ..
            }
        ));
        handle.abort();
    }

    #[tokio::test]
    async fn test_calculate_margin_api_error_502() {
        let body = r#"{"errorCode":"DH-909","errorMessage":"bad gateway"}"#;
        let (base_url, handle) = start_mock_server(502, body).await;
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
        assert!(matches!(
            result.unwrap_err(),
            OmsError::DhanApiError {
                status_code: 502,
                ..
            }
        ));
        handle.abort();
    }

    #[tokio::test]
    async fn test_get_fund_limit_api_error_502() {
        let body = r#"{"errorCode":"DH-909","errorMessage":"bad gateway"}"#;
        let (base_url, handle) = start_mock_server(502, body).await;
        let client = make_test_client(&base_url);

        let result = client.get_fund_limit("fake-token").await;
        assert!(matches!(
            result.unwrap_err(),
            OmsError::DhanApiError {
                status_code: 502,
                ..
            }
        ));
        handle.abort();
    }

    #[tokio::test]
    async fn test_modify_order_api_error_403() {
        let body = r#"{"errorCode":"DH-901","errorMessage":"forbidden"}"#;
        let (base_url, handle) = start_mock_server(403, body).await;
        let client = make_test_client(&base_url);

        let result = client
            .modify_order("fake-token", "ORD-1", &make_test_modify_request())
            .await;

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

    #[tokio::test]
    async fn test_convert_position_api_error_401() {
        let body = r#"{"errorCode":"DH-901","errorMessage":"auth failed"}"#;
        let (base_url, handle) = start_mock_server(401, body).await;
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
        assert!(matches!(
            result.unwrap_err(),
            OmsError::DhanApiError {
                status_code: 401,
                ..
            }
        ));
        handle.abort();
    }

    #[tokio::test]
    async fn test_convert_position_api_error_502() {
        let body = r#"{"errorCode":"DH-909","errorMessage":"bad gateway"}"#;
        let (base_url, handle) = start_mock_server(502, body).await;
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
        assert!(matches!(
            result.unwrap_err(),
            OmsError::DhanApiError {
                status_code: 502,
                ..
            }
        ));
        handle.abort();
    }

    // -----------------------------------------------------------------------
    // Coverage: cancel_order success (200) — exercises cancel path end-to-end
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_cancel_order_success_returns_ok() {
        let (base_url, handle) = start_mock_server(200, "{}").await;
        let client = make_test_client(&base_url);

        let result = client.cancel_order("fake-token", "ORD-123").await;
        assert!(result.is_ok());
        handle.abort();
    }

    // -----------------------------------------------------------------------
    // Coverage: modify_order success (200) — exercises modify path end-to-end
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_modify_order_success_returns_ok() {
        let (base_url, handle) = start_mock_server(200, "{}").await;
        let client = make_test_client(&base_url);

        let result = client
            .modify_order("fake-token", "ORD-1", &make_test_modify_request())
            .await;
        assert!(result.is_ok());
        handle.abort();
    }

    // -----------------------------------------------------------------------
    // Coverage: check_rate_limit with various non-429 codes
    // -----------------------------------------------------------------------

    #[test]
    fn check_rate_limit_ok_on_various_non_429_codes() {
        let client = OrderApiClient::new(
            Client::new(),
            "https://api.dhan.co/v2".to_owned(),
            "100".to_owned(),
        );
        assert!(client.check_rate_limit(400, "test").is_ok());
        assert!(client.check_rate_limit(403, "test").is_ok());
        assert!(client.check_rate_limit(500, "test").is_ok());
    }

    // -----------------------------------------------------------------------
    // Coverage: auth_headers sets correct Dhan custom headers
    // -----------------------------------------------------------------------

    #[test]
    fn auth_headers_sets_access_token_and_client_id() {
        let client = OrderApiClient::new(
            Client::new(),
            "https://api.dhan.co/v2".to_owned(),
            "MY-CLIENT-ID".to_owned(),
        );

        let req_builder = Client::new().get("https://example.com");
        let req_with_auth = client.auth_headers(req_builder, "test-jwt-token");

        // Build the request to inspect headers
        let request = req_with_auth.build().unwrap();
        assert_eq!(
            request.headers().get("access-token").unwrap(),
            "test-jwt-token"
        );
        assert_eq!(request.headers().get("client-id").unwrap(), "MY-CLIENT-ID");
        assert_eq!(
            request.headers().get("Content-Type").unwrap(),
            "application/json"
        );
        assert_eq!(request.headers().get("Accept").unwrap(), "application/json");
    }

    // -----------------------------------------------------------------------
    // Coverage: URL construction for all endpoint methods
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_place_order_url_construction() {
        // Test that place_order hits /orders endpoint by using a mock that
        // returns a valid order response
        let response_body = r#"{"orderId":"123","orderStatus":"TRANSIT"}"#;
        let (base_url, handle) = start_mock_server(200, response_body).await;
        let client = make_test_client(&base_url);

        let result = client
            .place_order("fake-token", &make_test_place_request())
            .await;
        assert!(result.is_ok());
        let resp = result.unwrap();
        assert_eq!(resp.order_id, "123");
        assert_eq!(resp.order_status, "TRANSIT");
        handle.abort();
    }

    #[tokio::test]
    async fn test_cancel_order_url_construction() {
        let (base_url, handle) = start_mock_server(200, "{}").await;
        let client = make_test_client(&base_url);

        let result = client.cancel_order("fake-token", "ORD-1").await;
        assert!(result.is_ok());
        handle.abort();
    }

    #[tokio::test]
    async fn test_get_order_url_construction() {
        let response_body = r#"{"orderId":"ORD-1","orderStatus":"PENDING","dhanClientId":"100","exchangeOrderId":"","transactionType":"BUY","exchangeSegment":"NSE_FNO","productType":"INTRADAY","orderType":"LIMIT","validity":"DAY","securityId":"52432","quantity":50,"price":245.5,"triggerPrice":0.0,"disclosedQuantity":0,"afterMarketOrder":false,"tradedQty":0,"tradedPrice":0.0,"remainingQuantity":50,"correlationId":"","orderDateTime":"2026-01-01","exchangeOrderDateTime":"","legName":"","legOrder":0,"createTime":"","updateTime":"","filledQty":0,"averageTradedPrice":0.0,"omsErrorCode":"","omsErrorDescription":""}"#;
        let (base_url, handle) = start_mock_server(200, response_body).await;
        let client = make_test_client(&base_url);

        let result = client.get_order("fake-token", "ORD-1").await;
        assert!(result.is_ok());
        handle.abort();
    }

    #[tokio::test]
    async fn test_get_all_orders_url_construction() {
        let (base_url, handle) = start_mock_server(200, "[]").await;
        let client = make_test_client(&base_url);

        let result = client.get_all_orders("fake-token").await;
        assert!(result.is_ok());
        let orders = result.unwrap();
        assert!(orders.is_empty());
        handle.abort();
    }

    #[tokio::test]
    async fn test_get_positions_url_construction() {
        let (base_url, handle) = start_mock_server(200, "[]").await;
        let client = make_test_client(&base_url);

        let result = client.get_positions("fake-token").await;
        assert!(result.is_ok());
        handle.abort();
    }

    #[tokio::test]
    async fn test_get_holdings_url_construction() {
        let (base_url, handle) = start_mock_server(200, "[]").await;
        let client = make_test_client(&base_url);

        let result = client.get_holdings("fake-token").await;
        assert!(result.is_ok());
        handle.abort();
    }

    // -----------------------------------------------------------------------
    // Coverage: error paths for non-2xx responses
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_place_order_non_2xx_returns_api_error() {
        let error_body = r#"{"errorType":"DH-905","errorCode":"905","errorMessage":"bad input"}"#;
        let (base_url, handle) = start_mock_server(400, error_body).await;
        let client = make_test_client(&base_url);

        let result = client
            .place_order("fake-token", &make_test_place_request())
            .await;
        assert!(result.is_err());
        match result.unwrap_err() {
            OmsError::DhanApiError {
                status_code,
                message,
            } => {
                assert_eq!(status_code, 400);
                assert!(message.contains("DH-905"));
            }
            other => panic!("expected DhanApiError, got: {:?}", other),
        }
        handle.abort();
    }

    #[tokio::test]
    async fn test_cancel_order_non_2xx_returns_api_error() {
        let (base_url, handle) = start_mock_server(404, r#"{"error":"not found"}"#).await;
        let client = make_test_client(&base_url);

        let result = client.cancel_order("fake-token", "ORD-NONEXISTENT").await;
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            OmsError::DhanApiError {
                status_code: 404,
                ..
            }
        ));
        handle.abort();
    }

    #[tokio::test]
    async fn test_get_all_orders_non_2xx_returns_api_error() {
        let (base_url, handle) = start_mock_server(500, r#"{"error":"server error"}"#).await;
        let client = make_test_client(&base_url);

        let result = client.get_all_orders("fake-token").await;
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            OmsError::DhanApiError {
                status_code: 500,
                ..
            }
        ));
        handle.abort();
    }

    #[tokio::test]
    async fn test_get_positions_non_2xx_returns_api_error() {
        let (base_url, handle) = start_mock_server(403, r#"{"error":"forbidden"}"#).await;
        let client = make_test_client(&base_url);

        let result = client.get_positions("fake-token").await;
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            OmsError::DhanApiError {
                status_code: 403,
                ..
            }
        ));
        handle.abort();
    }

    #[tokio::test]
    async fn test_get_holdings_non_2xx_returns_api_error() {
        let (base_url, handle) = start_mock_server(401, r#"{"error":"unauthorized"}"#).await;
        let client = make_test_client(&base_url);

        let result = client.get_holdings("fake-token").await;
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            OmsError::DhanApiError {
                status_code: 401,
                ..
            }
        ));
        handle.abort();
    }

    #[test]
    fn test_error_code_counters() {
        for code in ["DH-901", "DH-904", "DH-905", "DH-906"] {
            metrics::counter!("dlt_dhan_error_total", "code" => code).increment(1);
        }
    }

    // ===================================================================
    // Mock HTTP tests for ALL new API methods (Phase 2-8)
    // Each method tested with success (200) and error (non-200) paths
    // ===================================================================

    // --- Kill Switch ---

    #[tokio::test]
    async fn test_activate_kill_switch_success() {
        let body = r#"{"dhanClientId":"100","killSwitchStatus":"ACTIVATE"}"#;
        let (url, h) = start_mock_server(200, body).await;
        let client = make_test_client(&url);
        let result = client.activate_kill_switch("jwt").await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap().kill_switch_status, "ACTIVATE");
        h.abort();
    }

    #[tokio::test]
    async fn test_activate_kill_switch_error() {
        let body = r#"{"errorType":"error","errorCode":"DH-905","errorMessage":"bad"}"#;
        let (url, h) = start_mock_server(400, body).await;
        let client = make_test_client(&url);
        let result = client.activate_kill_switch("jwt").await;
        assert!(result.is_err());
        h.abort();
    }

    #[tokio::test]
    async fn test_deactivate_kill_switch_success() {
        let body = r#"{"dhanClientId":"100","killSwitchStatus":"DEACTIVATE"}"#;
        let (url, h) = start_mock_server(200, body).await;
        let client = make_test_client(&url);
        let result = client.deactivate_kill_switch("jwt").await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap().kill_switch_status, "DEACTIVATE");
        h.abort();
    }

    #[tokio::test]
    async fn test_get_kill_switch_status_success() {
        let body = r#"{"dhanClientId":"100","killSwitchStatus":"DEACTIVATE"}"#;
        let (url, h) = start_mock_server(200, body).await;
        let client = make_test_client(&url);
        let result = client.get_kill_switch_status("jwt").await;
        assert!(result.is_ok());
        h.abort();
    }

    // --- P&L Exit ---

    #[tokio::test]
    async fn test_configure_pnl_exit_success() {
        let body = r#"{"pnlExitStatus":"ACTIVE","message":"configured"}"#;
        let (url, h) = start_mock_server(200, body).await;
        let client = make_test_client(&url);
        let req = PnlExitRequest {
            profit_value: "1500.00".to_string(),
            loss_value: "500.00".to_string(),
            product_type: vec!["INTRADAY".to_string()],
            enable_kill_switch: true,
        };
        let result = client.configure_pnl_exit("jwt", &req).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap().pnl_exit_status, "ACTIVE");
        h.abort();
    }

    #[tokio::test]
    async fn test_stop_pnl_exit_success() {
        let body = r#"{"pnlExitStatus":"DISABLED","message":"stopped"}"#;
        let (url, h) = start_mock_server(200, body).await;
        let client = make_test_client(&url);
        let result = client.stop_pnl_exit("jwt").await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap().pnl_exit_status, "DISABLED");
        h.abort();
    }

    #[tokio::test]
    async fn test_get_pnl_exit_status_success() {
        let body = r#"{"pnlExitStatus":"ACTIVE","profit":"1500.00","loss":"500.00","productType":["INTRADAY"],"enable_kill_switch":true}"#;
        let (url, h) = start_mock_server(200, body).await;
        let client = make_test_client(&url);
        let result = client.get_pnl_exit_status("jwt").await;
        assert!(result.is_ok());
        let resp = result.unwrap();
        assert_eq!(resp.profit, "1500.00");
        assert!(resp.enable_kill_switch);
        h.abort();
    }

    // --- Order Slicing ---

    #[tokio::test]
    async fn test_place_order_slicing_success() {
        let body = r#"{"orderId":"SL-1","orderStatus":"PENDING","correlationId":"c1"}"#;
        let (url, h) = start_mock_server(200, body).await;
        let client = make_test_client(&url);
        let req = make_test_place_request();
        let result = client.place_order_slicing("jwt", &req).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap().order_id, "SL-1");
        h.abort();
    }

    // --- Get by Correlation ---

    #[tokio::test]
    async fn test_get_order_by_correlation_id_success() {
        let body = r#"{"orderId":"O1","correlationId":"c1","orderStatus":"TRADED","quantity":50,"filledQty":50,"remainingQuantity":0,"averageTradedPrice":245.5,"price":245.5,"triggerPrice":0,"omsErrorCode":"","omsErrorDescription":"","tradingSymbol":"NIFTY","securityId":"52432","tradedQuantity":50,"tradedPrice":245.5,"transactionType":"BUY","exchangeSegment":"NSE_FNO","productType":"INTRADAY","orderType":"LIMIT","validity":"DAY","exchangeOrderId":"E1","exchangeTime":"2026-03-30 10:00:00","createTime":"2026-03-30 10:00:00","updateTime":"2026-03-30 10:00:00","rejectionReason":"","tag":"","drvExpiryDate":"2026-03-27","drvOptionType":"CE","drvStrikePrice":24500.0}"#;
        let (url, h) = start_mock_server(200, body).await;
        let client = make_test_client(&url);
        let result = client.get_order_by_correlation_id("jwt", "c1").await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap().order_id, "O1");
        h.abort();
    }

    // --- Trade Book ---

    #[tokio::test]
    async fn test_get_trades_success() {
        let body = r#"[{"dhanClientId":"100","orderId":"O1","exchangeOrderId":"E1","exchangeTradeId":"T1","transactionType":"BUY","exchangeSegment":"NSE_FNO","productType":"INTRADAY","orderType":"LIMIT","tradingSymbol":"NIFTY","customSymbol":"","securityId":"52432","tradedQuantity":50,"tradedPrice":245.5,"isin":"","instrument":"OPTIDX","sebiTax":0.1,"stt":1.0,"brokerageCharges":20.0,"serviceTax":3.6,"exchangeTransactionCharges":0.5,"stampDuty":0.01,"drvExpiryDate":"2026-03-27","drvOptionType":"CE","drvStrikePrice":24500.0,"exchangeTime":"2026-03-30 10:00:00"}]"#;
        let (url, h) = start_mock_server(200, body).await;
        let client = make_test_client(&url);
        let result = client.get_trades("jwt").await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap().len(), 1);
        h.abort();
    }

    #[tokio::test]
    async fn test_get_trades_for_order_success() {
        let body = r#"[{"dhanClientId":"100","orderId":"O1","exchangeOrderId":"E1","exchangeTradeId":"T1","transactionType":"BUY","exchangeSegment":"NSE_FNO","productType":"INTRADAY","orderType":"LIMIT","tradingSymbol":"NIFTY","customSymbol":"","securityId":"52432","tradedQuantity":50,"tradedPrice":245.5,"isin":"","instrument":"OPTIDX","sebiTax":0,"stt":0,"brokerageCharges":0,"serviceTax":0,"exchangeTransactionCharges":0,"stampDuty":0,"drvExpiryDate":"NA","drvOptionType":"","drvStrikePrice":0,"exchangeTime":"2026-03-30 10:00:00"}]"#;
        let (url, h) = start_mock_server(200, body).await;
        let client = make_test_client(&url);
        let result = client.get_trades_for_order("jwt", "O1").await;
        assert!(result.is_ok());
        h.abort();
    }

    // --- Super Orders ---

    #[tokio::test]
    async fn test_place_super_order_success() {
        let body =
            r#"{"orderId":"SO-1","orderStatus":"PENDING","correlationId":"","legDetails":[]}"#;
        let (url, h) = start_mock_server(200, body).await;
        let client = make_test_client(&url);
        let req = DhanPlaceSuperOrderRequest {
            dhan_client_id: "100".to_string(),
            correlation_id: String::new(),
            transaction_type: "BUY".to_string(),
            exchange_segment: "NSE_EQ".to_string(),
            product_type: "CNC".to_string(),
            order_type: "LIMIT".to_string(),
            security_id: "11536".to_string(),
            quantity: 5,
            price: 1500.0,
            target_price: 1600.0,
            stop_loss_price: 1400.0,
            trailing_jump: 10.0,
        };
        let result = client.place_super_order("jwt", &req).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap().order_id, "SO-1");
        h.abort();
    }

    #[tokio::test]
    async fn test_modify_super_order_success() {
        let body =
            r#"{"orderId":"SO-1","orderStatus":"PENDING","correlationId":"","legDetails":[]}"#;
        let (url, h) = start_mock_server(200, body).await;
        let client = make_test_client(&url);
        let req = DhanModifySuperOrderRequest {
            dhan_client_id: "100".to_string(),
            leg_name: "TARGET_LEG".to_string(),
            order_type: None,
            quantity: None,
            price: None,
            target_price: Some(1650.0),
            stop_loss_price: None,
            trailing_jump: None,
        };
        let result = client.modify_super_order("jwt", "SO-1", &req).await;
        assert!(result.is_ok());
        h.abort();
    }

    #[tokio::test]
    async fn test_cancel_super_order_leg_success() {
        let body =
            r#"{"orderId":"SO-1","orderStatus":"CANCELLED","correlationId":"","legDetails":[]}"#;
        let (url, h) = start_mock_server(200, body).await;
        let client = make_test_client(&url);
        let result = client
            .cancel_super_order_leg("jwt", "SO-1", "ENTRY_LEG")
            .await;
        assert!(result.is_ok());
        h.abort();
    }

    #[tokio::test]
    async fn test_get_super_orders_success() {
        let body =
            r#"[{"orderId":"SO-1","orderStatus":"PENDING","correlationId":"","legDetails":[]}]"#;
        let (url, h) = start_mock_server(200, body).await;
        let client = make_test_client(&url);
        let result = client.get_super_orders("jwt").await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap().len(), 1);
        h.abort();
    }

    // --- Forever Orders ---

    #[tokio::test]
    async fn test_create_forever_order_success() {
        let body = r#"{"orderId":"FO-1","orderStatus":"CONFIRM"}"#;
        let (url, h) = start_mock_server(200, body).await;
        let client = make_test_client(&url);
        let req = DhanForeverOrderRequest {
            dhan_client_id: "100".to_string(),
            correlation_id: String::new(),
            order_flag: "SINGLE".to_string(),
            transaction_type: "BUY".to_string(),
            exchange_segment: "NSE_EQ".to_string(),
            product_type: "CNC".to_string(),
            order_type: "LIMIT".to_string(),
            validity: "DAY".to_string(),
            security_id: "1333".to_string(),
            quantity: 5,
            disclosed_quantity: 0,
            price: 1428.0,
            trigger_price: 1427.0,
            price1: None,
            trigger_price1: None,
            quantity1: None,
        };
        let result = client.create_forever_order("jwt", &req).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap().order_status, "CONFIRM");
        h.abort();
    }

    #[tokio::test]
    async fn test_delete_forever_order_success() {
        let body = r#"{"orderId":"FO-1","orderStatus":"CANCELLED"}"#;
        let (url, h) = start_mock_server(200, body).await;
        let client = make_test_client(&url);
        let result = client.delete_forever_order("jwt", "FO-1").await;
        assert!(result.is_ok());
        h.abort();
    }

    #[tokio::test]
    async fn test_get_all_forever_orders_success() {
        let body = r#"[{"orderId":"FO-1","orderStatus":"CONFIRM"}]"#;
        let (url, h) = start_mock_server(200, body).await;
        let client = make_test_client(&url);
        let result = client.get_all_forever_orders("jwt").await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap().len(), 1);
        h.abort();
    }

    // --- Conditional Triggers ---

    #[tokio::test]
    async fn test_create_conditional_trigger_success() {
        let body = r#"{"alertId":"A1","alertStatus":"ACTIVE"}"#;
        let (url, h) = start_mock_server(200, body).await;
        let client = make_test_client(&url);
        let req = DhanConditionalTriggerRequest {
            dhan_client_id: "100".to_string(),
            condition: TriggerCondition {
                comparison_type: "PRICE_WITH_VALUE".to_string(),
                exchange_segment: "NSE_EQ".to_string(),
                security_id: "1333".to_string(),
                indicator_name: None,
                time_frame: None,
                operator: "GREATER_THAN".to_string(),
                comparing_value: Some(250.0),
                comparing_indicator_name: None,
                exp_date: None,
                frequency: "ONCE".to_string(),
                user_note: None,
            },
            orders: vec![],
        };
        let result = client.create_conditional_trigger("jwt", &req).await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap().alert_status, "ACTIVE");
        h.abort();
    }

    #[tokio::test]
    async fn test_delete_conditional_trigger_success() {
        let body = r#"{"alertId":"A1","alertStatus":"CANCELLED"}"#;
        let (url, h) = start_mock_server(200, body).await;
        let client = make_test_client(&url);
        let result = client.delete_conditional_trigger("jwt", "A1").await;
        assert!(result.is_ok());
        h.abort();
    }

    #[tokio::test]
    async fn test_get_all_conditional_triggers_success() {
        let body = r#"[{"alertId":"A1","alertStatus":"ACTIVE"}]"#;
        let (url, h) = start_mock_server(200, body).await;
        let client = make_test_client(&url);
        let result = client.get_all_conditional_triggers("jwt").await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap().len(), 1);
        h.abort();
    }

    // --- EDIS ---

    #[tokio::test]
    async fn test_generate_tpin_success() {
        let (url, h) = start_mock_server(202, "").await;
        let client = make_test_client(&url);
        let result = client.generate_tpin("jwt").await;
        assert!(result.is_ok());
        h.abort();
    }

    #[tokio::test]
    async fn test_generate_edis_form_success() {
        let html = "<html><form>CDSL Form</form></html>";
        let (url, h) = start_mock_server(200, html).await;
        let client = make_test_client(&url);
        let req = EdisFormRequest {
            isin: "INE733E01010".to_string(),
            qty: 100,
            exchange: "NSE".to_string(),
            segment: "EQ".to_string(),
            bulk: false,
        };
        let result = client.generate_edis_form("jwt", &req).await;
        assert!(result.is_ok());
        assert!(result.unwrap().contains("CDSL Form"));
        h.abort();
    }

    #[tokio::test]
    async fn test_inquire_edis_approval_success() {
        let body = r#"{"totalQty":100,"aprvdQty":50,"status":"APPROVED"}"#;
        let (url, h) = start_mock_server(200, body).await;
        let client = make_test_client(&url);
        let result = client.inquire_edis_approval("jwt", "ALL").await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap().aprvd_qty, 50);
        h.abort();
    }

    // --- Statements ---

    #[tokio::test]
    async fn test_get_ledger_success() {
        let body = r#"[{"dhanClientId":"100","narration":"trade","voucherdate":"Jun 22, 2022","exchange":"NSE","voucherdesc":"desc","vouchernumber":"V1","debit":"1500.50","credit":"0.00","runbal":"50000"}]"#;
        let (url, h) = start_mock_server(200, body).await;
        let client = make_test_client(&url);
        let result = client.get_ledger("jwt", "2022-06-01", "2022-06-30").await;
        assert!(result.is_ok());
        let entries = result.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].debit, "1500.50"); // String, not float
        h.abort();
    }

    #[tokio::test]
    async fn test_get_trade_history_success() {
        let body = r#"[{"dhanClientId":"100","orderId":"O1","exchangeOrderId":"E1","exchangeTradeId":"T1","transactionType":"BUY","exchangeSegment":"NSE_EQ","productType":"CNC","orderType":"LIMIT","tradingSymbol":"RELIANCE","customSymbol":"","securityId":"2885","tradedQuantity":10,"tradedPrice":2500.0,"isin":"INE002A01018","instrument":"EQUITY","sebiTax":0.01,"stt":2.5,"brokerageCharges":0,"serviceTax":0,"exchangeTransactionCharges":0.5,"stampDuty":0.02,"drvExpiryDate":"NA","drvOptionType":"","drvStrikePrice":0,"exchangeTime":"2026-03-25 14:30:00"}]"#;
        let (url, h) = start_mock_server(200, body).await;
        let client = make_test_client(&url);
        let result = client
            .get_trade_history("jwt", "2026-03-01", "2026-03-30", 0)
            .await;
        assert!(result.is_ok());
        assert_eq!(result.unwrap().len(), 1);
        h.abort();
    }

    // --- Rate limit tests for new methods ---

    #[tokio::test]
    async fn test_kill_switch_rate_limited() {
        let (url, h) = start_mock_server(429, "{}").await;
        let client = make_test_client(&url);
        let result = client.activate_kill_switch("jwt").await;
        assert!(matches!(result, Err(OmsError::DhanRateLimited { .. })));
        h.abort();
    }

    #[tokio::test]
    async fn test_super_order_rate_limited() {
        let (url, h) = start_mock_server(429, "{}").await;
        let client = make_test_client(&url);
        let req = DhanPlaceSuperOrderRequest {
            dhan_client_id: "100".to_string(),
            correlation_id: String::new(),
            transaction_type: "BUY".to_string(),
            exchange_segment: "NSE_EQ".to_string(),
            product_type: "CNC".to_string(),
            order_type: "LIMIT".to_string(),
            security_id: "11536".to_string(),
            quantity: 5,
            price: 1500.0,
            target_price: 1600.0,
            stop_loss_price: 1400.0,
            trailing_jump: 0.0,
        };
        let result = client.place_super_order("jwt", &req).await;
        assert!(matches!(result, Err(OmsError::DhanRateLimited { .. })));
        h.abort();
    }

    #[tokio::test]
    async fn test_forever_order_rate_limited() {
        let (url, h) = start_mock_server(429, "{}").await;
        let client = make_test_client(&url);
        let req = DhanForeverOrderRequest {
            dhan_client_id: "100".to_string(),
            correlation_id: String::new(),
            order_flag: "SINGLE".to_string(),
            transaction_type: "BUY".to_string(),
            exchange_segment: "NSE_EQ".to_string(),
            product_type: "CNC".to_string(),
            order_type: "LIMIT".to_string(),
            validity: "DAY".to_string(),
            security_id: "1333".to_string(),
            quantity: 5,
            disclosed_quantity: 0,
            price: 1428.0,
            trigger_price: 1427.0,
            price1: None,
            trigger_price1: None,
            quantity1: None,
        };
        let result = client.create_forever_order("jwt", &req).await;
        assert!(matches!(result, Err(OmsError::DhanRateLimited { .. })));
        h.abort();
    }

    #[tokio::test]
    async fn test_conditional_trigger_rate_limited() {
        let (url, h) = start_mock_server(429, "{}").await;
        let client = make_test_client(&url);
        let result = client.get_all_conditional_triggers("jwt").await;
        assert!(matches!(result, Err(OmsError::DhanRateLimited { .. })));
        h.abort();
    }
}
