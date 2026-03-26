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

    // -----------------------------------------------------------------------
    // Product code mapping tests (WS abbreviations)
    // -----------------------------------------------------------------------

    #[test]
    fn test_product_code_c_is_cnc() {
        let json = r#"{"Data": {"Product": "C", "ProductName": "CNC"}}"#;
        let update = parse_order_update(json).unwrap();
        assert_eq!(update.product, "C");
        assert_eq!(update.product_name, "CNC");
    }

    #[test]
    fn test_product_code_i_is_intraday() {
        let json = r#"{"Data": {"Product": "I", "ProductName": "INTRADAY"}}"#;
        let update = parse_order_update(json).unwrap();
        assert_eq!(update.product, "I");
        assert_eq!(update.product_name, "INTRADAY");
    }

    #[test]
    fn test_txn_type_b_is_buy() {
        let json = r#"{"Data": {"TxnType": "B"}}"#;
        let update = parse_order_update(json).unwrap();
        assert_eq!(update.txn_type, "B");
    }

    #[test]
    fn test_txn_type_s_is_sell() {
        let json = r#"{"Data": {"TxnType": "S"}}"#;
        let update = parse_order_update(json).unwrap();
        assert_eq!(update.txn_type, "S");
    }

    #[test]
    fn test_source_p_is_api() {
        // Source field is not in OrderUpdate struct — verify unknown fields
        // don't break deserialization (serde default behavior)
        let json = r#"{"Data": {"Source": "P", "OrderNo": "123"}}"#;
        let update = parse_order_update(json).unwrap();
        assert_eq!(update.order_no, "123");
    }

    #[test]
    fn test_opt_type_xx_is_non_option() {
        let json = r#"{"Data": {"OptType": "XX"}}"#;
        let update = parse_order_update(json).unwrap();
        assert_eq!(update.opt_type, "XX");
    }

    #[test]
    fn test_opt_type_ce_is_call() {
        let json = r#"{"Data": {"OptType": "CE"}}"#;
        let update = parse_order_update(json).unwrap();
        assert_eq!(update.opt_type, "CE");
    }

    #[test]
    fn test_opt_type_pe_is_put() {
        let json = r#"{"Data": {"OptType": "PE"}}"#;
        let update = parse_order_update(json).unwrap();
        assert_eq!(update.opt_type, "PE");
    }

    // -----------------------------------------------------------------------
    // AMO and leg number mapping
    // -----------------------------------------------------------------------

    #[test]
    fn test_leg_number_mapping() {
        let json = r#"{"Data": {"LegNo": 1}}"#;
        let update = parse_order_update(json).unwrap();
        assert_eq!(update.leg_no, 1, "LegNo 1 = Entry leg");

        let json2 = r#"{"Data": {"LegNo": 2}}"#;
        let update2 = parse_order_update(json2).unwrap();
        assert_eq!(update2.leg_no, 2, "LegNo 2 = Stop Loss leg");

        let json3 = r#"{"Data": {"LegNo": 3}}"#;
        let update3 = parse_order_update(json3).unwrap();
        assert_eq!(update3.leg_no, 3, "LegNo 3 = Target leg");
    }

    #[test]
    fn test_super_order_remark() {
        let json = r#"{"Data": {"Remarks": "Super Order", "OrderNo": "789"}}"#;
        let update = parse_order_update(json).unwrap();
        assert_eq!(update.remarks, "Super Order");
    }

    #[test]
    fn test_correlation_id_roundtrip() {
        let correlation = "my-corr-id-abc-123";
        let json = format!(
            r#"{{"Data": {{"CorrelationId": "{}", "OrderNo": "111"}}}}"#,
            correlation
        );
        let update = parse_order_update(&json).unwrap();
        assert_eq!(update.correlation_id, correlation);
    }

    // -----------------------------------------------------------------------
    // Extra unknown fields — serde should ignore them
    // -----------------------------------------------------------------------

    #[test]
    fn test_extra_unknown_fields_ignored() {
        let json = r#"{
            "Data": {
                "OrderNo": "999",
                "Status": "TRADED",
                "SomeNewField": "value",
                "AnotherFuture": 42,
                "Nested": {"deep": true}
            }
        }"#;
        let update = parse_order_update(json).unwrap();
        assert_eq!(update.order_no, "999");
        assert_eq!(update.status, "TRADED");
    }

    // -----------------------------------------------------------------------
    // Partial fill fields
    // -----------------------------------------------------------------------

    #[test]
    fn test_partial_fill_fields() {
        let json = r#"{
            "Data": {
                "OrderNo": "444",
                "Status": "PENDING",
                "Quantity": 100,
                "TradedQty": 40,
                "RemainingQuantity": 60,
                "AvgTradedPrice": 250.25,
                "TradedPrice": 250.50
            }
        }"#;
        let update = parse_order_update(json).unwrap();
        assert_eq!(update.quantity, 100);
        assert_eq!(update.traded_qty, 40);
        assert_eq!(update.remaining_quantity, 60);
        assert!((update.avg_traded_price - 250.25).abs() < f64::EPSILON);
        assert!((update.traded_price - 250.50).abs() < f64::EPSILON);
    }

    // -----------------------------------------------------------------------
    // OrderUpdateParseError coverage
    // -----------------------------------------------------------------------

    #[test]
    fn test_order_update_parse_error_debug() {
        let err = parse_order_update("invalid json").unwrap_err();
        let debug = format!("{err:?}");
        assert!(debug.contains("JsonError"));
    }

    #[test]
    fn test_order_update_parse_error_source() {
        let err = parse_order_update("not json").unwrap_err();
        // thiserror generates source from #[from] attribute
        assert!(std::error::Error::source(&err).is_some());
    }

    #[test]
    fn test_order_update_parse_error_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<OrderUpdateParseError>();
    }

    // -----------------------------------------------------------------------
    // build_order_update_login edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn test_build_order_update_login_msg_code_is_42() {
        let msg = build_order_update_login("client", "token");
        let parsed: serde_json::Value = serde_json::from_str(&msg).unwrap();
        assert_eq!(parsed["LoginReq"]["MsgCode"], 42);
    }

    #[test]
    fn test_build_order_update_login_user_type_is_self() {
        let msg = build_order_update_login("client", "token");
        let parsed: serde_json::Value = serde_json::from_str(&msg).unwrap();
        assert_eq!(parsed["UserType"], "SELF");
    }

    #[test]
    fn test_build_order_update_login_special_chars_in_token() {
        let msg = build_order_update_login("client", "eyJ.token+with/special=chars");
        let parsed: serde_json::Value = serde_json::from_str(&msg).unwrap();
        assert_eq!(parsed["LoginReq"]["Token"], "eyJ.token+with/special=chars");
    }

    // -----------------------------------------------------------------------
    // Additional order update field coverage
    // -----------------------------------------------------------------------

    #[test]
    fn test_parse_order_update_all_ws_statuses() {
        for status in [
            "TRANSIT",
            "PENDING",
            "REJECTED",
            "CANCELLED",
            "TRADED",
            "EXPIRED",
        ] {
            let json = format!(r#"{{"Data": {{"Status": "{status}"}}}}"#);
            let update = parse_order_update(&json).unwrap();
            assert_eq!(update.status, status);
        }
    }

    #[test]
    fn test_parse_order_update_segment_codes() {
        for (segment, desc) in [
            ("E", "Equity"),
            ("D", "Derivatives"),
            ("C", "Currency"),
            ("M", "Commodity"),
        ] {
            let json = format!(r#"{{"Data": {{"Segment": "{segment}"}}}}"#);
            let update = parse_order_update(&json).unwrap();
            assert_eq!(update.segment, segment, "segment code for {desc}");
        }
    }

    #[test]
    fn test_parse_order_update_off_mkt_flag() {
        let json = r#"{"Data": {"OffMktFlag": "1"}}"#;
        let update = parse_order_update(json).unwrap();
        assert_eq!(update.off_mkt_flag, "1");

        let json = r#"{"Data": {"OffMktFlag": "0"}}"#;
        let update = parse_order_update(json).unwrap();
        assert_eq!(update.off_mkt_flag, "0");
    }

    #[test]
    fn test_parse_order_update_disc_quantity() {
        let json = r#"{"Data": {"DiscQuantity": 100, "DiscQtyRem": 50}}"#;
        let update = parse_order_update(json).unwrap();
        assert_eq!(update.disc_quantity, 100);
        assert_eq!(update.disc_qty_rem, 50);
    }

    #[test]
    fn test_parse_order_update_strike_price_and_expiry() {
        let json = r#"{"Data": {"StrikePrice": 24500.0, "ExpiryDate": "2026-03-27"}}"#;
        let update = parse_order_update(json).unwrap();
        assert!((update.strike_price - 24500.0).abs() < f64::EPSILON);
        assert_eq!(update.expiry_date, "2026-03-27");
    }

    #[test]
    fn test_parse_order_update_lot_size() {
        let json = r#"{"Data": {"LotSize": 50}}"#;
        let update = parse_order_update(json).unwrap();
        assert_eq!(update.lot_size, 50);
    }

    #[test]
    fn test_parse_order_update_ref_ltp_and_tick_size() {
        // refLtp and tickSize use camelCase, NOT PascalCase
        let json = r#"{"Data": {"refLtp": 245.0, "tickSize": 0.05}}"#;
        let update = parse_order_update(json).unwrap();
        assert!((update.ref_ltp - 245.0).abs() < f64::EPSILON);
        assert!((update.tick_size - 0.05).abs() < f64::EPSILON);
    }

    #[test]
    fn test_parse_order_update_empty_string_fields() {
        let json = r#"{"Data": {"OrderNo": "", "Status": "", "Symbol": ""}}"#;
        let update = parse_order_update(json).unwrap();
        assert_eq!(update.order_no, "");
        assert_eq!(update.status, "");
        assert_eq!(update.symbol, "");
    }

    #[test]
    fn test_parse_order_update_null_data_fails() {
        let err = parse_order_update(r#"{"Data": null}"#).unwrap_err();
        assert!(matches!(err, OrderUpdateParseError::JsonError(_)));
    }
}
