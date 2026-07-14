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

use tickvault_common::constants::ORDER_UPDATE_LOGIN_MSG_CODE;
use tickvault_common::order_types::{OrderUpdate, OrderUpdateMessage};

/// Error type for order update parsing.
#[derive(Debug, thiserror::Error)]
pub enum OrderUpdateParseError {
    /// JSON deserialization failed.
    #[error("failed to parse order update JSON: {0}")]
    JsonError(#[from] serde_json::Error),
}

/// Dual-casing merge pairs: `(accepted-by-struct key, alternate-casing key)`.
///
/// The live Dhan order-update sample (2026-07-14 runner crawl of
/// `docs/dhan-ref/10-live-order-update-websocket.md`) carries a camelCase
/// DUPLICATE cluster ahead of the PascalCase fields — real wire frames may
/// carry EITHER or BOTH casings. Per-pair rule: accepted-key-wins (identical
/// to today's derive behavior for every frame that parses today), with the
/// alternate casing accepted as a fallback when the struct's key is absent.
const CASING_PAIRS: &[(&str, &str)] = &[
    ("refLtp", "RefLtp"),
    ("tickSize", "TickSize"),
    ("Series", "series"),
    ("GoodTillDaysDate", "goodTillDaysDate"),
    ("AlgoId", "algoId"),
    ("Multiplier", "multiplier"),
    ("AlgoOrdNo", "algoOrdNo"),
];

/// The docs' "likely twin" of `Instrument` — Assumed (no parameter-table row
/// upstream), fallback-only, never overrides a present `Instrument`.
const ASSUMED_TWIN_PAIR: (&str, &str) = ("Instrument", "instrumentType");

/// Quirk counter name — 4 static `kind` label values:
/// `casing_fallback`, `casing_conflict`, `numeric_algo_ord_no`, `assumed_twin`.
const DECODE_QUIRKS_COUNTER: &str = "tv_order_update_decode_quirks_total";

/// Parses a JSON order update message from the Dhan order update WebSocket.
///
/// Expects JSON with a top-level `"Data"` key containing order fields.
///
/// Two-stage parse: the frame is first read into a `serde_json::Value`
/// (literal same-key duplicates collapse last-wins at this stage), the
/// `"Data"` object is normalized for the known dual-casing drift pairs and
/// the numeric `AlgoOrdNo` typing quirk, then deserialized into the derived
/// `OrderUpdateMessage` struct. Structural errors (malformed JSON,
/// missing/null/non-object `Data`) keep failing exactly as before.
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
    let mut value: serde_json::Value = serde_json::from_str(json_str)?;
    normalize_order_update_data(&mut value);
    let message: OrderUpdateMessage = serde_json::from_value(value)?;
    Ok(message.data)
}

/// Normalizes the `"Data"` object of a raw order-update frame in place.
///
/// Operates ONLY on the `"Data"` member if it is present AND an object —
/// absent/null/non-object `Data` is left untouched so the wrapper's
/// missing-`Data` / `Data: null` error contract is preserved unchanged.
fn normalize_order_update_data(value: &mut serde_json::Value) {
    let Some(data) = value
        .get_mut("Data")
        .and_then(serde_json::Value::as_object_mut)
    else {
        return;
    };
    for &(accepted, alt) in CASING_PAIRS {
        merge_casing_pair(data, accepted, alt, "casing_fallback");
    }
    merge_casing_pair(
        data,
        ASSUMED_TWIN_PAIR.0,
        ASSUMED_TWIN_PAIR.1,
        "assumed_twin",
    );
    coerce_numeric_algo_ord_no(data);
}

/// Applies the accepted-key-wins rule for one casing pair.
///
/// - alt present + accepted absent → rename alt→accepted (quirk `fallback_kind`)
/// - BOTH present → drop alt (accepted wins; if the values differ, quirk
///   `casing_conflict`)
/// - accepted only / neither → no-op
fn merge_casing_pair(
    data: &mut serde_json::Map<String, serde_json::Value>,
    accepted: &str,
    alt: &str,
    fallback_kind: &'static str,
) {
    if !data.contains_key(alt) {
        return;
    }
    if data.contains_key(accepted) {
        let removed = data.remove(alt);
        if removed.as_ref() != data.get(accepted) {
            metrics::counter!(DECODE_QUIRKS_COUNTER, "kind" => "casing_conflict").increment(1);
        }
    } else if let Some(alt_value) = data.remove(alt) {
        // O(1) EXEMPT: begin — cold-path key rename on the order-update frame
        data.insert(accepted.to_string(), alt_value);
        // O(1) EXEMPT: end
        metrics::counter!(DECODE_QUIRKS_COUNTER, "kind" => fallback_kind).increment(1);
    }
}

/// Coerces a JSON Number `AlgoOrdNo` (the live doc table types it `float`)
/// into its string rendering so the `String`-typed struct field accepts it.
///
/// Integral values render without a `.0` suffix; other numbers use the f64
/// `Display` rendering. String/null values are untouched (null hits
/// `#[serde(default)]` as today); object/array values are left in place so
/// the typed parse error stays loud and the WAL retains the frame.
fn coerce_numeric_algo_ord_no(data: &mut serde_json::Map<String, serde_json::Value>) {
    let Some(algo_ord_no) = data.get_mut("AlgoOrdNo") else {
        return;
    };
    if let serde_json::Value::Number(number) = algo_ord_no {
        // O(1) EXEMPT: begin — cold-path number→string rendering
        let rendered = if let Some(integral) = number.as_i64() {
            integral.to_string()
        } else if let Some(unsigned) = number.as_u64() {
            unsigned.to_string()
        } else if let Some(float) = number.as_f64() {
            float.to_string()
        } else {
            return;
        };
        // O(1) EXEMPT: end
        *algo_ord_no = serde_json::Value::String(rendered);
        metrics::counter!(DECODE_QUIRKS_COUNTER, "kind" => "numeric_algo_ord_no").increment(1);
    }
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

    // -----------------------------------------------------------------------
    // Dual-casing decode hardening (2026-07-14 upstream drift)
    // -----------------------------------------------------------------------

    #[test]
    fn test_ref_ltp_tick_size_pascal_only_now_parses() {
        // PascalCase-only forms (the live parameter table casing) previously
        // degraded to #[serde(default)] zeros — now accepted as fallback.
        let json = r#"{"Data": {"RefLtp": 245.0, "TickSize": 0.05, "OrderNo": "1"}}"#;
        let update = parse_order_update(json).unwrap();
        assert!((update.ref_ltp - 245.0).abs() < f64::EPSILON);
        assert!((update.tick_size - 0.05).abs() < f64::EPSILON);
        assert_eq!(update.order_no, "1");
    }

    #[test]
    fn test_dual_casing_cluster_live_doc_sample_shape() {
        // Mirrors the 2026-07-14 live doc sample: camelCase duplicate cluster
        // AHEAD of the PascalCase forms, equal values.
        let json = r#"{
            "Data": {
                "series": "EQ",
                "goodTillDaysDate": "2024-09-11",
                "instrumentType": "EQ",
                "refLtp": 13.21,
                "tickSize": 0.01,
                "algoId": "0",
                "multiplier": 1,
                "OrderNo": "112111182198",
                "Status": "PENDING",
                "Series": "EQ",
                "GoodTillDaysDate": "2024-09-11",
                "Instrument": "EQ",
                "RefLtp": 13.21,
                "TickSize": 0.01,
                "AlgoId": "0",
                "Multiplier": 1
            }
        }"#;
        let update = parse_order_update(json).unwrap();
        assert_eq!(update.series, "EQ");
        assert_eq!(update.good_till_days_date, "2024-09-11");
        assert_eq!(update.instrument, "EQ");
        assert!((update.ref_ltp - 13.21).abs() < f64::EPSILON);
        assert!((update.tick_size - 0.01).abs() < f64::EPSILON);
        assert_eq!(update.algo_id, "0");
        assert_eq!(update.multiplier, 1);
        assert_eq!(update.order_no, "112111182198");
        assert_eq!(update.status, "PENDING");
    }

    #[test]
    fn test_dual_casing_conflict_accepted_key_wins_and_counts() {
        // Both casings present with DIFFERING values: the accepted-by-struct
        // key wins deterministically (camel for refLtp/tickSize — the
        // explicit renames; Pascal for the rename_all fields).
        let json = r#"{
            "Data": {
                "refLtp": 10.0,
                "RefLtp": 99.0,
                "tickSize": 0.05,
                "TickSize": 9.99,
                "Series": "EQ",
                "series": "XX",
                "Multiplier": 1,
                "multiplier": 7
            }
        }"#;
        let update = parse_order_update(json).unwrap();
        assert!((update.ref_ltp - 10.0).abs() < f64::EPSILON);
        assert!((update.tick_size - 0.05).abs() < f64::EPSILON);
        assert_eq!(update.series, "EQ");
        assert_eq!(update.multiplier, 1);
    }

    #[test]
    fn test_dual_casing_drifting_cluster_all_fields() {
        // Alternate-casing-only frames for every drift pair now parse into
        // the struct fields instead of degrading to defaults.
        let json = r#"{
            "Data": {
                "series": "EQ",
                "goodTillDaysDate": "2024-09-11",
                "algoId": "7",
                "multiplier": 2,
                "algoOrdNo": "555"
            }
        }"#;
        let update = parse_order_update(json).unwrap();
        assert_eq!(update.series, "EQ");
        assert_eq!(update.good_till_days_date, "2024-09-11");
        assert_eq!(update.algo_id, "7");
        assert_eq!(update.multiplier, 2);
        assert_eq!(update.algo_ord_no, "555");
    }

    #[test]
    fn test_same_key_duplicate_last_wins_no_error() {
        // Pins serde_json Value semantics: literal same-key duplicates
        // collapse LAST-wins at the Value stage (library-drift ratchet).
        let json = r#"{"Data": {"OrderNo": "first", "OrderNo": "second"}}"#;
        let update = parse_order_update(json).unwrap();
        assert_eq!(update.order_no, "second");
    }

    #[test]
    fn test_algo_ord_no_numeric_float_coerced_to_string() {
        // The live doc table types AlgoOrdNo as float — coerce Number→String.
        let json = r#"{"Data": {"AlgoOrdNo": 12345.0}}"#;
        let update = parse_order_update(json).unwrap();
        assert_eq!(
            update.algo_ord_no, "12345",
            "integral float renders without .0"
        );

        let json = r#"{"Data": {"AlgoOrdNo": 12345.5}}"#;
        let update = parse_order_update(json).unwrap();
        assert_eq!(update.algo_ord_no, "12345.5");
    }

    #[test]
    fn test_algo_ord_no_int_string_null_variants() {
        // Integer
        let update = parse_order_update(r#"{"Data": {"AlgoOrdNo": 42}}"#).unwrap();
        assert_eq!(update.algo_ord_no, "42");

        // Boundary values (financial-test-guard)
        let update = parse_order_update(r#"{"Data": {"AlgoOrdNo": 0}}"#).unwrap();
        assert_eq!(update.algo_ord_no, "0");
        let update = parse_order_update(r#"{"Data": {"AlgoOrdNo": 9223372036854775807}}"#).unwrap();
        assert_eq!(update.algo_ord_no, "9223372036854775807");
        let update =
            parse_order_update(r#"{"Data": {"AlgoOrdNo": 18446744073709551615}}"#).unwrap();
        assert_eq!(update.algo_ord_no, "18446744073709551615");
        let update = parse_order_update(r#"{"Data": {"AlgoOrdNo": -1}}"#).unwrap();
        assert_eq!(update.algo_ord_no, "-1");

        // String untouched
        let update = parse_order_update(r#"{"Data": {"AlgoOrdNo": "ABC123"}}"#).unwrap();
        assert_eq!(update.algo_ord_no, "ABC123");

        // Null untouched by the normalizer — #[serde(default)] covers only a
        // MISSING field, so an explicit null errored before this change too
        // (behavior unchanged); a missing field still defaults to "".
        let err = parse_order_update(r#"{"Data": {"AlgoOrdNo": null}}"#).unwrap_err();
        assert!(matches!(err, OrderUpdateParseError::JsonError(_)));
        let update = parse_order_update(r#"{"Data": {"OrderNo": "1"}}"#).unwrap();
        assert_eq!(update.algo_ord_no, "");

        // Object/array left in place → typed parse error, loud
        let err = parse_order_update(r#"{"Data": {"AlgoOrdNo": {"x": 1}}}"#).unwrap_err();
        assert!(matches!(err, OrderUpdateParseError::JsonError(_)));
        let err = parse_order_update(r#"{"Data": {"AlgoOrdNo": [1]}}"#).unwrap_err();
        assert!(matches!(err, OrderUpdateParseError::JsonError(_)));
    }

    #[test]
    fn test_instrument_type_camel_fallback_only_never_overrides_pascal() {
        // Assumed twin: fallback-only when Instrument is absent...
        let update = parse_order_update(r#"{"Data": {"instrumentType": "EQ"}}"#).unwrap();
        assert_eq!(update.instrument, "EQ");

        // ...and NEVER overrides a present Instrument.
        let update =
            parse_order_update(r#"{"Data": {"Instrument": "OPTIDX", "instrumentType": "EQ"}}"#)
                .unwrap();
        assert_eq!(update.instrument, "OPTIDX");
    }

    #[test]
    fn test_upstream_malformed_sample_is_json_error_not_panic() {
        // Dhan's own live-sample missing comma after "multiplier": 1.
        let json = r#"{"Data": {"multiplier": 1 "Series": "EQ"}}"#;
        let err = parse_order_update(json).unwrap_err();
        assert!(matches!(err, OrderUpdateParseError::JsonError(_)));
    }

    #[test]
    fn test_auth_ack_and_heartbeat_still_fail_parse() {
        // Login ACK (no Data member) must still Err so the caller's
        // classify_auth_response routing is preserved.
        let err = parse_order_update(
            r#"{"LoginResp": {"MsgCode": 42, "Status": "Ok"}, "Type": "login"}"#,
        )
        .unwrap_err();
        assert!(matches!(err, OrderUpdateParseError::JsonError(_)));

        // Heartbeat-shaped frame must still Err.
        let err = parse_order_update(r#"{"Type": "heartbeat"}"#).unwrap_err();
        assert!(matches!(err, OrderUpdateParseError::JsonError(_)));

        // Non-object Data must still Err (normalizer leaves it untouched).
        let err = parse_order_update(r#"{"Data": "not-an-object"}"#).unwrap_err();
        assert!(matches!(err, OrderUpdateParseError::JsonError(_)));
    }

    #[test]
    fn test_serialized_roundtrip_through_normalizer() {
        // Our own Serialize output must survive the normalizer unchanged
        // (WAL replay pin — replayed frames re-parse to the same struct).
        let json = r#"{
            "Data": {
                "Exchange": "NSE", "Segment": "D", "SecurityId": "52432",
                "OrderNo": "1234567890", "TxnType": "B", "Status": "TRADED",
                "Quantity": 50, "Price": 245.50, "Symbol": "NIFTY",
                "Instrument": "OPTIDX", "OptType": "CE",
                "refLtp": 245.0, "tickSize": 0.05,
                "AlgoOrdNo": "777", "Series": "EQ",
                "GoodTillDaysDate": "2024-09-11", "AlgoId": "0", "Multiplier": 1
            }
        }"#;
        let original = parse_order_update(json).unwrap();
        let message = OrderUpdateMessage {
            data: original.clone(),
            r#type: "order_alert".to_string(),
        };
        let serialized = serde_json::to_string(&message).unwrap();
        let reparsed = parse_order_update(&serialized).unwrap();
        assert_eq!(reparsed, original);
    }

    proptest::proptest! {
        #[test]
        fn prop_parse_order_update_never_panics_on_arbitrary_json(input in ".*") {
            // Total: arbitrary input must yield Ok or Err, never panic.
            let _ = parse_order_update(&input);
        }

        #[test]
        fn prop_parse_order_update_never_panics_on_json_shaped_data(
            key in "[a-zA-Z]{1,20}",
            num in proptest::num::f64::ANY,
            s in ".{0,40}",
        ) {
            let value = serde_json::json!({ "Data": { key.clone(): num, "AlgoOrdNo": num } });
            let _ = parse_order_update(&value.to_string());
            let value = serde_json::json!({ "Data": { key: s } });
            let _ = parse_order_update(&value.to_string());
        }
    }
}
