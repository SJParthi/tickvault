//! Order update parser for the Dhan live order update WebSocket.
//!
//! The order update WebSocket (`wss://api-order-update.dhan.co`) sends JSON messages
//! (NOT binary) with a `"Data"` wrapper containing order fields in PascalCase.
//!
//! This module provides:
//! - `parse_order_update` — Deserializes a JSON string into an `OrderUpdate`.
//! - `build_order_update_login` — Builds the login JSON message.
//!
//! # Protocol
//! After connecting, the client sends a login message:
//! ```json
//! {
//!   "LoginReq": { "MsgCode": 42, "ClientId": "..." , "Token": "..." },
//!   "UserType": "SELF"
//! }
//! ```
//! Then order updates arrive automatically as JSON with a `"Data"` key.

use dhan_live_trader_common::constants::ORDER_UPDATE_LOGIN_MSG_CODE;
use dhan_live_trader_common::order_types::{OrderUpdate, OrderUpdateMessage};

/// Error type for order update parsing.
#[derive(Debug, thiserror::Error)]
pub enum OrderUpdateParseError {
    /// JSON deserialization failed.
    #[error("failed to parse order update JSON: {0}")]
    JsonError(#[from] serde_json::Error),
}

/// Parses a JSON order update message from the Dhan order update WebSocket.
///
/// Expects JSON with a top-level `"Data"` key containing order fields.
///
/// # Arguments
/// * `json_str` — Raw JSON string from the WebSocket.
///
/// # Returns
/// The parsed `OrderUpdate` struct.
///
/// # Errors
/// Returns `OrderUpdateParseError::JsonError` if deserialization fails.
pub fn parse_order_update(json_str: &str) -> Result<OrderUpdate, OrderUpdateParseError> {
    let message: OrderUpdateMessage = serde_json::from_str(json_str)?;
    Ok(message.data)
}

/// Builds the login JSON message for the order update WebSocket.
///
/// Must be sent immediately after WebSocket connection is established.
///
/// # Arguments
/// * `client_id` — Dhan client identifier.
/// * `access_token` — JWT access token.
///
/// # Returns
/// Serialized JSON string ready to send over WebSocket.
pub fn build_order_update_login(client_id: &str, access_token: &str) -> String {
    // O(1) EXEMPT: begin — login message built once at connect time
    serde_json::json!({
        "LoginReq": {
            "MsgCode": ORDER_UPDATE_LOGIN_MSG_CODE,
            "ClientId": client_id,
            "Token": access_token
        },
        "UserType": "SELF"
    })
    .to_string()
    // O(1) EXEMPT: end
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_order_update_full_message() {
        let json = r#"{
            "Data": {
                "Exchange": "NSE",
                "Segment": "D",
                "SecurityId": "52432",
                "ClientId": "1000000001",
                "OrderNo": "1234567890",
                "ExchOrderNo": "NSE-1234567",
                "Product": "I",
                "TxnType": "B",
                "OrderType": "LMT",
                "Validity": "DAY",
                "Quantity": 50,
                "TradedQty": 50,
                "RemainingQuantity": 0,
                "Price": 245.50,
                "TriggerPrice": 0.0,
                "TradedPrice": 245.50,
                "AvgTradedPrice": 245.50,
                "Status": "TRADED",
                "Symbol": "NIFTY",
                "DisplayName": "NIFTY 27MAR 24500 CE",
                "CorrelationId": "uuid-123",
                "Remarks": "",
                "ReasonDescription": "CONFIRMED",
                "OrderDateTime": "2026-03-15 10:30:45",
                "ExchOrderTime": "2026-03-15 10:30:45",
                "LastUpdatedTime": "2026-03-15 10:30:46",
                "Instrument": "OPTIDX",
                "LotSize": 50,
                "StrikePrice": 24500.0,
                "ExpiryDate": "2026-03-27",
                "OptType": "CE",
                "Isin": "",
                "DiscQuantity": 0,
                "DiscQtyRem": 0,
                "LegNo": 0,
                "ProductName": "INTRADAY",
                "refLtp": 245.0,
                "tickSize": 0.05
            }
        }"#;

        let update = parse_order_update(json).unwrap();
        assert_eq!(update.exchange, "NSE");
        assert_eq!(update.order_no, "1234567890");
        assert_eq!(update.txn_type, "B");
        assert_eq!(update.status, "TRADED");
        assert_eq!(update.quantity, 50);
        assert!((update.price - 245.50).abs() < f64::EPSILON);
        assert_eq!(update.symbol, "NIFTY");
        assert_eq!(update.opt_type, "CE");
    }

    #[test]
    fn test_parse_order_update_minimal() {
        let json = r#"{"Data": {"OrderNo": "999", "Status": "PENDING"}}"#;
        let update = parse_order_update(json).unwrap();
        assert_eq!(update.order_no, "999");
        assert_eq!(update.status, "PENDING");
        assert_eq!(update.exchange, "");
        assert_eq!(update.quantity, 0);
    }

    #[test]
    fn test_parse_order_update_invalid_json() {
        let err = parse_order_update("not json").unwrap_err();
        assert!(matches!(err, OrderUpdateParseError::JsonError(_)));
    }

    #[test]
    fn test_parse_order_update_missing_data_key() {
        let err = parse_order_update(r#"{"OrderNo": "123"}"#).unwrap_err();
        assert!(matches!(err, OrderUpdateParseError::JsonError(_)));
    }

    #[test]
    fn test_parse_order_update_empty_data() {
        let update = parse_order_update(r#"{"Data": {}}"#).unwrap();
        assert_eq!(update.order_no, "");
        assert_eq!(update.status, "");
    }

    #[test]
    fn test_build_order_update_login() {
        let msg = build_order_update_login("1000000001", "jwt-token-123");
        let parsed: serde_json::Value = serde_json::from_str(&msg).unwrap();

        assert_eq!(parsed["LoginReq"]["MsgCode"], 42);
        assert_eq!(parsed["LoginReq"]["ClientId"], "1000000001");
        assert_eq!(parsed["LoginReq"]["Token"], "jwt-token-123");
        assert_eq!(parsed["UserType"], "SELF");
    }

    #[test]
    fn test_build_order_update_login_empty_credentials() {
        let msg = build_order_update_login("", "");
        let parsed: serde_json::Value = serde_json::from_str(&msg).unwrap();
        assert_eq!(parsed["LoginReq"]["ClientId"], "");
        assert_eq!(parsed["LoginReq"]["Token"], "");
    }

    #[test]
    fn test_parse_order_update_rejected_order() {
        let json = r#"{
            "Data": {
                "OrderNo": "555",
                "Status": "REJECTED",
                "ReasonDescription": "Insufficient margin",
                "TxnType": "B",
                "Quantity": 100,
                "Price": 500.0,
                "TradedQty": 0,
                "RemainingQuantity": 100
            }
        }"#;

        let update = parse_order_update(json).unwrap();
        assert_eq!(update.status, "REJECTED");
        assert_eq!(update.reason_description, "Insufficient margin");
        assert_eq!(update.traded_qty, 0);
        assert_eq!(update.remaining_quantity, 100);
    }

    #[test]
    fn test_parse_order_update_cancelled_order() {
        let json = r#"{
            "Data": {
                "OrderNo": "666",
                "Status": "Cancelled",
                "Quantity": 50,
                "TradedQty": 25,
                "RemainingQuantity": 25
            }
        }"#;

        let update = parse_order_update(json).unwrap();
        assert_eq!(update.status, "Cancelled");
        assert_eq!(update.traded_qty, 25);
        assert_eq!(update.remaining_quantity, 25);
    }

    #[test]
    fn test_order_update_parse_error_display() {
        let err = parse_order_update("invalid").unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("failed to parse order update JSON"));
    }
}
