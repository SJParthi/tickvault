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
    /// Frame exceeded [`ORDER_UPDATE_MAX_FRAME_BYTES`] — rejected before any
    /// `Value` parse to cap allocation amplification from a hostile frame.
    #[error(
        "order update frame too large: {len} bytes exceeds the \
         {ORDER_UPDATE_MAX_FRAME_BYTES}-byte cap"
    )]
    FrameTooLarge {
        /// Byte length of the rejected frame.
        len: usize,
    },
}

/// Maximum accepted order-update frame size (256 KiB).
///
/// Real Dhan order events are ~1-2 KB, so 256 KiB is a generous ceiling; the
/// guard bounds the `serde_json::Value`-tree allocation amplification of an
/// oversized/hostile frame BEFORE the two-stage parse allocates anything.
pub const ORDER_UPDATE_MAX_FRAME_BYTES: usize = 262_144;

/// How the `OrderUpdate` field behind an accepted key is typed. Drives the
/// tolerant coercion in [`fix_for_field`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FieldKind {
    /// `String` field.
    Text,
    /// `i64` field.
    Int,
    /// `f64` field.
    Float,
}

/// Every `OrderUpdate` field: `(key the struct accepts, other casing, type)`.
///
/// Per-pair rule (unchanged since 2026-07-14): accepted-key-wins. The live
/// Dhan sample (2026-07-14 runner crawl of
/// `docs/dhan-ref/10-live-order-update-websocket.md`) carries a camelCase
/// DUPLICATE cluster ahead of the PascalCase fields, and the reference's
/// SDK note reads the order number as `data["orderNo"]` (camelCase), so a
/// real wire frame may carry EITHER or BOTH casings for ANY field.
///
/// ORDER-EVT-02 fix (2026-10-28): this table used to cover only seven
/// fields (`refLtp`, `tickSize`, `Series`, `GoodTillDaysDate`, `AlgoId`,
/// `Multiplier`, `AlgoOrdNo`). A frame whose core keys arrived in camelCase
/// (`orderNo`, `status`, `securityId`, ...) therefore decoded with every
/// core field empty and ONLY those seven populated, which is exactly the
/// shape of the 2026-08-25 hollow rows (`ref_ltp`, `tick_size`,
/// `multiplier`, `good_till_days_date`, `algo_ord_no` set, identity blank).
/// It now covers every field. `test_field_table_covers_every_order_update_field`
/// pins it against the struct's own serialized key set, so a field added to
/// `OrderUpdate` without a row here fails the build.
///
/// `refLtp` / `tickSize` are the two fields whose struct key is camelCase
/// (explicit `#[serde(rename)]`), so their "other casing" is PascalCase.
const ORDER_UPDATE_FIELDS: &[(&str, &str, FieldKind)] = &[
    ("Exchange", "exchange", FieldKind::Text),
    ("Segment", "segment", FieldKind::Text),
    ("SecurityId", "securityId", FieldKind::Text),
    ("ClientId", "clientId", FieldKind::Text),
    ("OrderNo", "orderNo", FieldKind::Text),
    ("ExchOrderNo", "exchOrderNo", FieldKind::Text),
    ("Product", "product", FieldKind::Text),
    ("TxnType", "txnType", FieldKind::Text),
    ("OrderType", "orderType", FieldKind::Text),
    ("Validity", "validity", FieldKind::Text),
    ("Quantity", "quantity", FieldKind::Int),
    ("TradedQty", "tradedQty", FieldKind::Int),
    ("RemainingQuantity", "remainingQuantity", FieldKind::Int),
    ("Price", "price", FieldKind::Float),
    ("TriggerPrice", "triggerPrice", FieldKind::Float),
    ("TradedPrice", "tradedPrice", FieldKind::Float),
    ("AvgTradedPrice", "avgTradedPrice", FieldKind::Float),
    ("Status", "status", FieldKind::Text),
    ("Symbol", "symbol", FieldKind::Text),
    ("DisplayName", "displayName", FieldKind::Text),
    ("CorrelationId", "correlationId", FieldKind::Text),
    ("Remarks", "remarks", FieldKind::Text),
    ("ReasonDescription", "reasonDescription", FieldKind::Text),
    ("OrderDateTime", "orderDateTime", FieldKind::Text),
    ("ExchOrderTime", "exchOrderTime", FieldKind::Text),
    ("LastUpdatedTime", "lastUpdatedTime", FieldKind::Text),
    ("Instrument", "instrument", FieldKind::Text),
    ("LotSize", "lotSize", FieldKind::Int),
    ("StrikePrice", "strikePrice", FieldKind::Float),
    ("ExpiryDate", "expiryDate", FieldKind::Text),
    ("OptType", "optType", FieldKind::Text),
    ("Isin", "isin", FieldKind::Text),
    ("DiscQuantity", "discQuantity", FieldKind::Int),
    ("DiscQtyRem", "discQtyRem", FieldKind::Int),
    ("LegNo", "legNo", FieldKind::Int),
    ("ProductName", "productName", FieldKind::Text),
    ("refLtp", "RefLtp", FieldKind::Float),
    ("tickSize", "TickSize", FieldKind::Float),
    ("Source", "source", FieldKind::Text),
    ("OffMktFlag", "offMktFlag", FieldKind::Text),
    ("AlgoOrdNo", "algoOrdNo", FieldKind::Text),
    ("MktType", "mktType", FieldKind::Text),
    ("Series", "series", FieldKind::Text),
    ("GoodTillDaysDate", "goodTillDaysDate", FieldKind::Text),
    ("AlgoId", "algoId", FieldKind::Text),
    ("Multiplier", "multiplier", FieldKind::Int),
];

/// The docs' "likely twin" of `Instrument` — Assumed (no parameter-table row
/// upstream), fallback-only, never overrides a present `Instrument`.
const ASSUMED_TWIN_PAIR: (&str, &str) = ("Instrument", "instrumentType");

/// Wrapper keys: `(accepted, other casing)`. The typed parse needs `Data`
/// and reads `Type`; a lowercase wrapper is moved onto them when the
/// accepted key is absent (accepted-key-wins, as for the fields).
const WRAPPER_CASING_PAIRS: &[(&str, &str)] = &[("Data", "data"), ("Type", "type")];

/// The documented message type of an order event (`"Type": "order_alert"`).
const ORDER_ALERT_MESSAGE_TYPE: &str = "order_alert";

/// Longest message-type string [`unreadable_order_frame_type`] keeps for a
/// log line. A vendor tag is a short word; the bound only stops a garbage
/// frame from putting an arbitrarily long string into a log field.
const MESSAGE_TYPE_LOG_MAX_CHARS: usize = 64;

/// Quirk counter name — static `kind` label values:
/// `casing_fallback`, `casing_conflict`, `numeric_algo_ord_no`,
/// `assumed_twin`, `null_alt_dropped`, and since 2026-10-28
/// `wrapper_casing`, `type_not_string`, `null_field_dropped`,
/// `numeric_text_coerced`, `numeric_string_coerced`,
/// `integral_float_coerced`, `empty_number_dropped`.
const DECODE_QUIRKS_COUNTER: &str = "tv_order_update_decode_quirks_total";

/// Parses a JSON order update message from the Dhan order update WebSocket.
///
/// Expects JSON with a top-level `"Data"` key containing order fields.
///
/// Two-stage parse: the frame is first read into a `serde_json::Value`
/// (literal same-key duplicates collapse last-wins at this stage), the
/// frame is normalized (both casings of every field and of the wrapper, null
/// fields as absent, exact number/string coercion; see
/// `normalize_order_update_frame`), then deserialized into the derived
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
    if json_str.len() > ORDER_UPDATE_MAX_FRAME_BYTES {
        return Err(OrderUpdateParseError::FrameTooLarge {
            len: json_str.len(),
        });
    }
    let mut value: serde_json::Value = serde_json::from_str(json_str)?;
    normalize_order_update_frame(&mut value);
    let message: OrderUpdateMessage = serde_json::from_value(value)?;
    Ok(message.data)
}

/// Normalizes a raw order-update frame in place before the typed parse.
///
/// Wrapper: a lowercase `data` / `type` is moved onto `Data` / `Type` when
/// the accepted key is absent; a `Type` that is null or not a string is
/// dropped (the struct defaults it), because the tag is metadata and a bad
/// tag must never cost the order event its decode.
///
/// `Data`: operates ONLY when `Data` is present AND an object, so the
/// missing-`Data` / `Data: null` / non-object `Data` error contract is
/// preserved unchanged. For every field in [`ORDER_UPDATE_FIELDS`] it merges
/// the two casings (accepted-key-wins), then applies [`fix_for_field`]:
/// a null field is dropped (the struct default applies, as for an absent
/// one), and a number or string in the wrong JSON type is coerced when the
/// value converts exactly; anything else is left so the typed parse fails
/// loud (ORDER-EVT-02 `typed_parse_failed`) and the WAL keeps the frame.
///
/// # Complexity
/// O(`ORDER_UPDATE_FIELDS`) = 46 map probes per frame, a fixed constant,
/// on the order-update read task (orders are sparse: 1 to 100 a day). Not
/// on the market-data hot path.
fn normalize_order_update_frame(value: &mut serde_json::Value) {
    let Some(top) = value.as_object_mut() else {
        return;
    };
    for &(accepted, alt) in WRAPPER_CASING_PAIRS {
        if !top.contains_key(accepted)
            && let Some(moved) = top.remove(alt)
        {
            // O(1) EXEMPT: begin — cold-path key rename on the order-update frame
            top.insert(accepted.to_string(), moved);
            // O(1) EXEMPT: end
            metrics::counter!(DECODE_QUIRKS_COUNTER, "kind" => "wrapper_casing").increment(1);
        }
    }
    if top.get("Type").is_some_and(|tag| !tag.is_string()) {
        top.remove("Type");
        metrics::counter!(DECODE_QUIRKS_COUNTER, "kind" => "type_not_string").increment(1);
    }
    let Some(data) = top
        .get_mut("Data")
        .and_then(serde_json::Value::as_object_mut)
    else {
        return;
    };
    for &(accepted, alt, kind) in ORDER_UPDATE_FIELDS {
        // A blank fill field in the OTHER casing, with no accepted key, is
        // moved onto the accepted key so the typed parse refuses it loudly
        // (see `FILL_CRITICAL_FIELDS`); the generic merge would drop a null
        // alt and let the field default to 0.
        if FILL_CRITICAL_FIELDS.contains(&accepted)
            && !data.contains_key(accepted)
            && data.get(alt).is_some_and(is_blank_value)
            && let Some(moved) = data.remove(alt)
        {
            // O(1) EXEMPT: begin — cold-path key rename on the order-update frame
            data.insert(accepted.to_string(), moved);
            // O(1) EXEMPT: end
        }
        merge_casing_pair(data, accepted, alt, "casing_fallback");
        apply_field_fix(data, accepted, kind);
    }
    merge_casing_pair(
        data,
        ASSUMED_TWIN_PAIR.0,
        ASSUMED_TWIN_PAIR.1,
        "assumed_twin",
    );
    apply_field_fix(data, ASSUMED_TWIN_PAIR.0, FieldKind::Text);
}

/// Applies the accepted-key-wins rule for one casing pair, treating JSON
/// `null` as ABSENT on both sides:
///
/// - alt is `null` → drop alt, no rename (quirk `null_alt_dropped`)
/// - alt non-null + accepted absent → rename alt→accepted (quirk `fallback_kind`)
/// - alt non-null + accepted is `null` → alt REPLACES the null accepted value
///   (quirk `fallback_kind` — the null accepted key counts as absent)
/// - BOTH present + non-null → drop alt (accepted wins; if the values differ
///   beyond numeric equivalence, quirk `casing_conflict`)
/// - accepted only / neither → no-op
fn merge_casing_pair(
    data: &mut serde_json::Map<String, serde_json::Value>,
    accepted: &str,
    alt: &str,
    fallback_kind: &'static str,
) {
    match data.get(alt) {
        None => return,
        Some(serde_json::Value::Null) => {
            // Null alt is treated as absent — drop it, never rename.
            data.remove(alt);
            metrics::counter!(DECODE_QUIRKS_COUNTER, "kind" => "null_alt_dropped").increment(1);
            return;
        }
        Some(_) => {}
    }
    let accepted_is_null = data.get(accepted).map(serde_json::Value::is_null);
    match accepted_is_null {
        // Accepted absent, or present-but-null (treated as absent): the
        // non-null alt value lands under the accepted key.
        None | Some(true) => {
            if let Some(alt_value) = data.remove(alt) {
                // O(1) EXEMPT: begin — cold-path key rename on the order-update frame
                data.insert(accepted.to_string(), alt_value);
                // O(1) EXEMPT: end
                metrics::counter!(DECODE_QUIRKS_COUNTER, "kind" => fallback_kind).increment(1);
            }
        }
        // Both present and non-null: accepted wins; differing values beyond
        // numeric equivalence count one conflict quirk.
        Some(false) => {
            let removed = data.remove(alt);
            if let (Some(removed), Some(kept)) = (removed.as_ref(), data.get(accepted))
                && !json_values_equivalent(removed, kept)
            {
                metrics::counter!(DECODE_QUIRKS_COUNTER, "kind" => "casing_conflict").increment(1);
            }
        }
    }
}

/// Structural equality with numeric tolerance: two JSON Numbers whose f64
/// renderings are equal (e.g. `1` vs `1.0`) are the SAME wire value, not a
/// casing conflict.
fn json_values_equivalent(a: &serde_json::Value, b: &serde_json::Value) -> bool {
    if a == b {
        return true;
    }
    if let (serde_json::Value::Number(x), serde_json::Value::Number(y)) = (a, b)
        && let (Some(xf), Some(yf)) = (x.as_f64(), y.as_f64())
    {
        // Exact f64 equality is intended: same wire value, two renderings.
        return xf == yf;
    }
    false
}

/// What [`fix_for_field`] decided for one accepted key.
#[derive(Debug, Clone, PartialEq)]
enum FieldFix {
    /// The value already fits the struct, or cannot be converted exactly
    /// (left in place so the typed parse fails loud).
    Keep,
    /// Remove the key so `#[serde(default)]` applies; carries the quirk kind.
    Remove(&'static str),
    /// Replace the value; carries the quirk kind.
    Replace(serde_json::Value, &'static str),
}

/// Applies [`fix_for_field`] to `key` in `data` and counts the quirk.
fn apply_field_fix(
    data: &mut serde_json::Map<String, serde_json::Value>,
    key: &str,
    kind: FieldKind,
) {
    let fix = match data.get(key) {
        None => return,
        Some(value) => fix_for_field(key, kind, value),
    };
    match fix {
        FieldFix::Keep => {}
        FieldFix::Remove(quirk) => {
            data.remove(key);
            metrics::counter!(DECODE_QUIRKS_COUNTER, "kind" => quirk).increment(1);
        }
        FieldFix::Replace(fixed, quirk) => {
            if let Some(slot) = data.get_mut(key) {
                *slot = fixed;
            }
            metrics::counter!(DECODE_QUIRKS_COUNTER, "kind" => quirk).increment(1);
        }
    }
}

/// The fields the order book reads to compute a fill (`extract_fill_delta`
/// in the OMS engine): quantity, filled quantity and prices. A `null` or an
/// empty string in any of them is NEVER read as "absent": a defaulted 0
/// there turns a TRADED update into a terminal order with no fill, so the
/// position and P&L go wrong with nothing logged. Such a frame keeps
/// failing the typed parse loudly (ORDER-EVT-02 `typed_parse_failed`,
/// `tv_order_update_frames_dropped_total{reason="unparseable_order"}`),
/// exactly as before 2026-10-28; the WAL keeps the raw frame.
const FILL_CRITICAL_FIELDS: &[&str] = &[
    "Quantity",
    "TradedQty",
    "RemainingQuantity",
    "Price",
    "TradedPrice",
    "AvgTradedPrice",
];

/// Identity text fields that are numbers on the wire when they are numbers
/// at all. A non-integral number here (`12.5`, or one past `u64::MAX`,
/// which serde reads as a rounded float) is not converted: it would become
/// a made-up order or instrument id, so it is left for the typed parse to
/// refuse loudly. `AlgoOrdNo` is NOT in this list: the live doc table types
/// it `float`, and its fractional rendering predates 2026-10-28.
const INTEGRAL_ID_TEXT_FIELDS: &[&str] = &["OrderNo", "ExchOrderNo", "SecurityId"];

/// `null`, or a string that is empty after trimming. Pure, no allocation.
fn is_blank_value(value: &serde_json::Value) -> bool {
    value.is_null() || value.as_str().is_some_and(|text| text.trim().is_empty())
}

/// Decides how one field value is made to fit its struct type. Pure.
///
/// - `null` or an empty string in a [`FILL_CRITICAL_FIELDS`] field → keep,
///   so the typed parse fails loud (a defaulted 0 would lose the fill).
/// - `null` in any other field → remove: null means absent, and every
///   `OrderUpdate` field defaults when absent. Before 2026-10-28 one null
///   field (say `"CorrelationId": null`) failed the whole typed parse and
///   the order event was dropped.
/// - Text field holding a number → its exact string rendering (integral
///   values without `.0`). `AlgoOrdNo` keeps its own quirk kind
///   (`numeric_algo_ord_no`, the live doc table types it `float`). A
///   [`INTEGRAL_ID_TEXT_FIELDS`] field accepts integral numbers only.
/// - Int field holding a string → the integer it spells (`"50"`, `"50.0"`);
///   an empty string → remove.
/// - Int field holding an integral float (`50.0`) inside the `i64` range →
///   that integer.
/// - Float field holding a string → the finite number it spells; an empty
///   string → remove.
/// - Anything else (a fractional quantity, `"ten"`, an object, an array, a
///   bool, a non-finite number) → keep, so the typed parse fails loud.
fn fix_for_field(key: &str, kind: FieldKind, value: &serde_json::Value) -> FieldFix {
    use serde_json::Value;
    if is_blank_value(value) && FILL_CRITICAL_FIELDS.contains(&key) {
        return FieldFix::Keep;
    }
    match (kind, value) {
        (_, Value::Null) => FieldFix::Remove("null_field_dropped"),
        (FieldKind::Text, Value::Number(number)) => {
            // O(1) EXEMPT: begin — cold-path number→string rendering
            let rendered = if let Some(integral) = number.as_i64() {
                integral.to_string()
            } else if let Some(unsigned) = number.as_u64() {
                unsigned.to_string()
            } else if INTEGRAL_ID_TEXT_FIELDS.contains(&key) {
                return FieldFix::Keep;
            } else if let Some(float) = number.as_f64() {
                float.to_string()
            } else {
                return FieldFix::Keep;
            };
            // O(1) EXEMPT: end
            let quirk = if key == "AlgoOrdNo" {
                "numeric_algo_ord_no"
            } else {
                "numeric_text_coerced"
            };
            FieldFix::Replace(Value::String(rendered), quirk)
        }
        (FieldKind::Int, Value::String(text)) => {
            let trimmed = text.trim();
            if trimmed.is_empty() {
                return FieldFix::Remove("empty_number_dropped");
            }
            if let Ok(integral) = trimmed.parse::<i64>() {
                return FieldFix::Replace(Value::from(integral), "numeric_string_coerced");
            }
            match trimmed.parse::<f64>().ok().and_then(exact_i64) {
                Some(integral) => {
                    FieldFix::Replace(Value::from(integral), "numeric_string_coerced")
                }
                None => FieldFix::Keep,
            }
        }
        (FieldKind::Int, Value::Number(number)) if number.as_i64().is_none() => {
            match number.as_f64().and_then(exact_i64) {
                Some(integral) => {
                    FieldFix::Replace(Value::from(integral), "integral_float_coerced")
                }
                None => FieldFix::Keep,
            }
        }
        (FieldKind::Float, Value::String(text)) => {
            let trimmed = text.trim();
            if trimmed.is_empty() {
                return FieldFix::Remove("empty_number_dropped");
            }
            match trimmed
                .parse::<f64>()
                .ok()
                .and_then(serde_json::Number::from_f64)
            {
                Some(number) => FieldFix::Replace(Value::Number(number), "numeric_string_coerced"),
                None => FieldFix::Keep,
            }
        }
        _ => FieldFix::Keep,
    }
}

/// The `i64` an `f64` holds exactly: finite, no fractional part, inside the
/// `i64` range (`i64::MAX as f64` is 2^63, which is out of range, hence `<`).
fn exact_i64(float: f64) -> Option<i64> {
    // `i64::MIN as f64` is exactly -2^63 and `i64::MAX as f64` rounds up to
    // 2^63, so `[lo, hi)` is exactly the range an `i64` can hold.
    let (lo, hi) = (i64::MIN as f64, i64::MAX as f64);
    if float.is_finite() && float.fract() == 0.0 && float >= lo && float < hi {
        Some(float as i64)
    } else {
        None
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

/// True when a decoded update carries none of the three identity fields:
/// `order_no`, `status` and `security_id` are all empty. Pure, O(1), no
/// allocation.
///
/// `OrderUpdate` defaults every field, so a frame whose `Data` keys match
/// nothing the struct knows still parses `Ok` and decodes to this shape.
/// A real partial update (say only `OrderNo`) is NOT hollow.
pub fn is_hollow_order_update(update: &OrderUpdate) -> bool {
    update.order_no.is_empty() && update.status.is_empty() && update.security_id.is_empty()
}

/// The ORDER-EVT-02 verdict for a frame that [`parse_order_update`] decoded:
/// `Some(message type)` when the frame must be logged as an order event we
/// cannot read, `None` for a readable one. Two shapes qualify:
///
/// 1. Hollow ([`is_hollow_order_update`]): no order number, status or
///    security id in either casing, whatever the frame's `Type`.
/// 2. No order number AND a `Type` naming something other than
///    `order_alert`. Since every field is read in both casings, an
///    undocumented frame carrying a camelCase `status` (say
///    `{"Data":{"status":"success"},"Type":"login_ack"}`) is no longer
///    hollow; without this arm it would reach the order book as an order
///    with no number and log nothing, where before 2026-10-28 it paged.
///
/// Deliberately NOT a filter: nothing here keeps a frame from the order
/// book or the audit tables (the caller broadcasts every frame). Dhan
/// documents a single message type on this socket (`order_alert`) and no
/// captured frame shows another one, so an unknown tag is exactly the
/// vendor-format-change case ORDER-EVT-02 exists to page on.
///
/// The returned tag is the frame's `Type` (or a drifted `type`), trimmed
/// and bounded to `MESSAGE_TYPE_LOG_MAX_CHARS` characters; empty when the
/// tag is absent, not a string, or the frame is oversized or does not
/// re-parse.
///
/// # Complexity
/// O(1) and allocation-free for a frame that carries an order number (the
/// normal case). Otherwise one more `serde_json::Value` parse of the frame,
/// O(frame bytes) bounded by [`ORDER_UPDATE_MAX_FRAME_BYTES`]. Cold: the
/// order-update read task, 1 to 100 orders a day.
pub fn unreadable_order_frame_type(update: &OrderUpdate, json_str: &str) -> Option<String> {
    if !update.order_no.is_empty() {
        return None;
    }
    let parsed = (json_str.len() <= ORDER_UPDATE_MAX_FRAME_BYTES)
        .then(|| serde_json::from_str::<serde_json::Value>(json_str).ok())
        .flatten();
    let tag = parsed
        .as_ref()
        .and_then(|value| value.get("Type").or_else(|| value.get("type")))
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .unwrap_or_default();
    let foreign = !tag.is_empty() && !tag.eq_ignore_ascii_case(ORDER_ALERT_MESSAGE_TYPE);
    if !(is_hollow_order_update(update) || foreign) {
        return None;
    }
    // O(1) EXEMPT: begin — cold-path bounded copy of the tag for the log
    Some(tag.chars().take(MESSAGE_TYPE_LOG_MAX_CHARS).collect())
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

        // 2026-10-28 (ORDER-EVT-02 fix): an explicit null now reads as
        // ABSENT and defaults to "" — before, one null field failed the whole
        // typed parse and the order event was dropped. A missing field still
        // defaults to "".
        let update = parse_order_update(r#"{"Data": {"AlgoOrdNo": null}}"#).unwrap();
        assert_eq!(update.algo_ord_no, "");
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

    // -----------------------------------------------------------------------
    // Review round 1: null-merge semantics, numeric tolerance, frame size cap
    // -----------------------------------------------------------------------

    #[test]
    fn test_alt_null_dropped_frame_still_parses() {
        // M1/E2: a null alternate-casing key is treated as ABSENT — dropped,
        // never renamed onto the accepted key (which would fail the typed
        // parse). The frame parses with the struct default.
        let json = r#"{"Data": {"series": null, "OrderNo": "1"}}"#;
        let update = parse_order_update(json).unwrap();
        assert_eq!(update.series, "");
        assert_eq!(update.order_no, "1");

        // Null alt for the assumed twin behaves identically.
        let json = r#"{"Data": {"instrumentType": null, "OrderNo": "2"}}"#;
        let update = parse_order_update(json).unwrap();
        assert_eq!(update.instrument, "");
        assert_eq!(update.order_no, "2");
    }

    #[test]
    fn test_accepted_null_recovered_from_non_null_alt() {
        // M1/E2: a null accepted key is treated as ABSENT — the non-null
        // alternate-casing value lands under the accepted key instead of the
        // null poisoning the typed parse.
        let json = r#"{"Data": {"Series": null, "series": "EQ", "OrderNo": "3"}}"#;
        let update = parse_order_update(json).unwrap();
        assert_eq!(update.series, "EQ");
        assert_eq!(update.order_no, "3");
    }

    #[test]
    fn test_both_null_drops_alt_and_accepted_null_reads_as_absent() {
        // alt null → dropped; accepted null → dropped too (2026-10-28: null
        // means absent, so the struct default applies). Before, an explicit
        // null on a String field failed the typed parse and lost the event.
        let json = r#"{"Data": {"Series": null, "series": null, "OrderNo": "8"}}"#;
        let update = parse_order_update(json).unwrap();
        assert_eq!(update.series, "");
        assert_eq!(update.order_no, "8");
    }

    #[test]
    fn test_numeric_int_vs_float_equal_is_not_a_conflict() {
        // E6/L2: 1 vs 1.0 are the same wire value in two renderings —
        // accepted wins, no casing_conflict is counted, and the frame parses.
        let json = r#"{"Data": {"Multiplier": 1, "multiplier": 1.0, "OrderNo": "4"}}"#;
        let update = parse_order_update(json).unwrap();
        assert_eq!(update.multiplier, 1);
        assert_eq!(update.order_no, "4");

        // The pure helper pins the equivalence rule directly.
        assert!(json_values_equivalent(
            &serde_json::json!(1),
            &serde_json::json!(1.0)
        ));
        assert!(json_values_equivalent(
            &serde_json::json!(0.05),
            &serde_json::json!(0.05)
        ));
        assert!(!json_values_equivalent(
            &serde_json::json!(1),
            &serde_json::json!(2.0)
        ));
        assert!(!json_values_equivalent(
            &serde_json::json!("1"),
            &serde_json::json!(1)
        ));
    }

    #[test]
    fn test_oversize_frame_rejected_before_parse() {
        // Sec-MEDIUM: frames beyond ORDER_UPDATE_MAX_FRAME_BYTES are refused
        // with a typed error BEFORE any Value-tree allocation.
        let padding = "x".repeat(ORDER_UPDATE_MAX_FRAME_BYTES);
        let json = format!(r#"{{"Data": {{"Remarks": "{padding}"}}}}"#);
        assert!(json.len() > ORDER_UPDATE_MAX_FRAME_BYTES);
        let err = parse_order_update(&json).unwrap_err();
        assert!(matches!(
            err,
            OrderUpdateParseError::FrameTooLarge { len } if len == json.len()
        ));
        assert!(err.to_string().contains("frame too large"));

        // A frame AT or below the cap passes the size guard (parse outcome
        // then depends on content as before).
        let small = r#"{"Data": {"OrderNo": "5"}}"#;
        assert!(small.len() <= ORDER_UPDATE_MAX_FRAME_BYTES);
        assert_eq!(parse_order_update(small).unwrap().order_no, "5");
    }

    // -----------------------------------------------------------------------
    // ORDER-EVT-02 fix (2026-10-28): every field in both casings, null as
    // absent, exact coercion, and the hollow-frame classification.
    // -----------------------------------------------------------------------

    /// A real Dhan order event (the reference's §3 sample, one `Remarks`)
    /// with every `OrderUpdate` field present, keyed as the struct accepts.
    const FULL_ACCEPTED_SAMPLE: &str = r#"{
        "Data": {
            "Exchange": "NSE", "Segment": "D", "Source": "P",
            "SecurityId": "52432", "ClientId": "1000000001",
            "ExchOrderNo": "1400000000404591", "OrderNo": "1124091136546",
            "Product": "I", "TxnType": "B", "OrderType": "LMT", "Validity": "DAY",
            "DiscQuantity": 5, "DiscQtyRem": 4, "RemainingQuantity": 25,
            "Quantity": 75, "TradedQty": 50, "Price": 245.5, "TriggerPrice": 240.25,
            "TradedPrice": 245.45, "AvgTradedPrice": 245.4, "AlgoOrdNo": "1124091136500",
            "OffMktFlag": "0", "OrderDateTime": "2026-10-28 10:30:45",
            "ExchOrderTime": "2026-10-28 10:30:45", "LastUpdatedTime": "2026-10-28 10:30:46",
            "Remarks": "Super Order", "MktType": "NL", "ReasonDescription": "CONFIRMED",
            "LegNo": 2, "Instrument": "OPTIDX", "Symbol": "NIFTY",
            "ProductName": "INTRADAY", "Status": "PART_TRADED", "LotSize": 75,
            "StrikePrice": 24500.0, "ExpiryDate": "2026-11-04", "OptType": "CE",
            "DisplayName": "NIFTY 04 NOV 24500 CALL", "Isin": "XX0000000000",
            "Series": "XX", "GoodTillDaysDate": "2026-10-28", "refLtp": 245.55,
            "tickSize": 0.05, "AlgoId": "7", "Multiplier": 3,
            "CorrelationId": "corr-abc_123"
        },
        "Type": "order_alert"
    }"#;

    fn full_sample_value() -> serde_json::Value {
        serde_json::from_str(FULL_ACCEPTED_SAMPLE).unwrap()
    }

    fn default_for(kind: FieldKind) -> serde_json::Value {
        match kind {
            FieldKind::Text => serde_json::json!(""),
            FieldKind::Int => serde_json::json!(0),
            FieldKind::Float => serde_json::json!(0.0),
        }
    }

    /// The full sample's struct with one field reset to its default, built
    /// through serde (not the parser) so it is an independent expectation.
    fn expected_without(accepted: &str, kind: FieldKind) -> OrderUpdate {
        let full = parse_order_update(FULL_ACCEPTED_SAMPLE).unwrap();
        let mut value = serde_json::to_value(&full).unwrap();
        value[accepted] = default_for(kind);
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn test_field_table_covers_every_order_update_field() {
        // The struct's own serialized key set IS the accepted key set; a
        // field added to OrderUpdate without a table row fails here.
        let empty = parse_order_update(r#"{"Data": {}}"#).unwrap();
        let value = serde_json::to_value(&empty).unwrap();
        let object = value.as_object().unwrap();
        assert_eq!(object.len(), ORDER_UPDATE_FIELDS.len());
        for &(accepted, alt, kind) in ORDER_UPDATE_FIELDS {
            let field = object
                .get(accepted)
                .unwrap_or_else(|| panic!("{accepted} is not an OrderUpdate key"));
            let actual = if field.is_string() {
                FieldKind::Text
            } else if field.is_i64() {
                FieldKind::Int
            } else {
                FieldKind::Float
            };
            assert_eq!(actual, kind, "{accepted} is typed {actual:?}");
            // The other casing differs only in the first letter's case.
            let mut a = accepted.chars();
            let mut b = alt.chars();
            let (fa, fb) = (a.next().unwrap(), b.next().unwrap());
            assert_eq!(a.as_str(), b.as_str(), "{accepted} / {alt}");
            assert_ne!(fa, fb, "{accepted} / {alt}");
            assert_eq!(fa.to_ascii_lowercase(), fb.to_ascii_lowercase());
        }
    }

    #[test]
    fn test_full_accepted_sample_populates_every_field() {
        let update = parse_order_update(FULL_ACCEPTED_SAMPLE).unwrap();
        let value = serde_json::to_value(&update).unwrap();
        for &(accepted, _, kind) in ORDER_UPDATE_FIELDS {
            assert_ne!(
                value[accepted],
                default_for(kind),
                "{accepted} must carry a non-default value in the sample"
            );
        }
        assert!(!is_hollow_order_update(&update));
    }

    #[test]
    fn test_parse_order_update_all_camel_case_frame_decodes_like_pascal() {
        // The 2026-08-25 / 2026-10-28 hollow shape: every key in its other
        // casing. Before the fix only seven fields survived; now all do.
        let mut value = full_sample_value();
        let data = value["Data"].as_object_mut().unwrap();
        for &(accepted, alt, _) in ORDER_UPDATE_FIELDS {
            let moved = data.remove(accepted).unwrap();
            data.insert(alt.to_string(), moved);
        }
        let camel = parse_order_update(&value.to_string()).unwrap();
        let pascal = parse_order_update(FULL_ACCEPTED_SAMPLE).unwrap();
        assert_eq!(camel, pascal);
        assert_eq!(camel.order_no, "1124091136546");
        assert_eq!(camel.status, "PART_TRADED");
        assert_eq!(camel.security_id, "52432");
        assert_eq!(camel.quantity, 75);
        assert!(!is_hollow_order_update(&camel));
    }

    #[test]
    fn test_parse_order_update_camel_case_wrapper_and_core_fields() {
        let json = r#"{"data": {"orderNo": "77", "status": "TRADED", "securityId": "14366",
            "txnType": "S", "tradedQty": 10, "avgTradedPrice": 13.2}, "type": "order_alert"}"#;
        let update = parse_order_update(json).unwrap();
        assert_eq!(update.order_no, "77");
        assert_eq!(update.status, "TRADED");
        assert_eq!(update.security_id, "14366");
        assert_eq!(update.txn_type, "S");
        assert_eq!(update.traded_qty, 10);
        assert!((update.avg_traded_price - 13.2).abs() < f64::EPSILON);
        // Accepted wrapper wins when both are present.
        let both = r#"{"Data": {"OrderNo": "1"}, "data": {"OrderNo": "2"}}"#;
        assert_eq!(parse_order_update(both).unwrap().order_no, "1");
        // A lowercase null wrapper still fails like `Data: null`.
        assert!(parse_order_update(r#"{"data": null}"#).is_err());
    }

    #[test]
    fn test_every_single_field_missing_or_null_still_parses() {
        // Every field-missing permutation of one field, and the same field
        // sent as null: the event still decodes, only that field defaults.
        // A null fill field (quantity, filled quantity, a price) is the one
        // exception: it fails the parse loudly instead of reading as 0.
        for &(accepted, _, kind) in ORDER_UPDATE_FIELDS {
            let expected = expected_without(accepted, kind);
            let mut missing = full_sample_value();
            missing["Data"].as_object_mut().unwrap().remove(accepted);
            assert_eq!(
                parse_order_update(&missing.to_string()).unwrap(),
                expected,
                "{accepted} missing"
            );
            let mut null = full_sample_value();
            null["Data"][accepted] = serde_json::Value::Null;
            if FILL_CRITICAL_FIELDS.contains(&accepted) {
                assert!(
                    parse_order_update(&null.to_string()).is_err(),
                    "{accepted} null must fail loud"
                );
                let mut blank = full_sample_value();
                blank["Data"][accepted] = serde_json::Value::from("");
                assert!(
                    parse_order_update(&blank.to_string()).is_err(),
                    "{accepted} empty must fail loud"
                );
            } else {
                assert_eq!(
                    parse_order_update(&null.to_string()).unwrap(),
                    expected,
                    "{accepted} null"
                );
            }
        }
    }

    /// Hostile review 2026-10-28: a TRADED update whose filled quantity or
    /// price is null or empty must never decode with a defaulted 0 (the
    /// order book would mark the order Traded with no fill and refuse every
    /// later re-send). It fails the parse, which counts it on
    /// `unparseable_order` and logs ORDER-EVT-02, as before 2026-10-28.
    #[test]
    fn test_traded_update_with_blank_fill_fields_fails_loud() {
        for (field, blank) in [
            ("TradedQty", "null"),
            ("TradedQty", r#""""#),
            ("tradedQty", "null"),
            ("AvgTradedPrice", "null"),
            ("AvgTradedPrice", r#"" ""#),
            ("TradedPrice", "null"),
            ("Quantity", "null"),
            ("RemainingQuantity", r#""""#),
            ("Price", "null"),
        ] {
            let json = format!(
                r#"{{"Data":{{"OrderNo":"1","Status":"TRADED","SecurityId":"52432","Exchange":"NSE","Segment":"D","{field}":{blank}}},"Type":"order_alert"}}"#
            );
            assert!(
                parse_order_update(&json).is_err(),
                "{field}={blank} must fail loud, not default to 0"
            );
        }
        // A missing fill key is a partial update, as it always was.
        let partial = r#"{"Data":{"OrderNo":"1","Status":"PENDING"},"Type":"order_alert"}"#;
        assert_eq!(parse_order_update(partial).unwrap().traded_qty, 0);
    }

    #[test]
    fn test_every_field_missing_decodes_hollow() {
        let update = parse_order_update(r#"{"Data": {}, "Type": "order_alert"}"#).unwrap();
        assert!(is_hollow_order_update(&update));
        // Every non-fill field null (the fill fields absent): hollow.
        let mut all_null = full_sample_value();
        let data = all_null["Data"].as_object_mut().unwrap();
        for fill in FILL_CRITICAL_FIELDS {
            data.remove(*fill);
        }
        for value in data.values_mut() {
            *value = serde_json::Value::Null;
        }
        let update = parse_order_update(&all_null.to_string()).unwrap();
        assert!(is_hollow_order_update(&update));
    }

    #[test]
    fn test_is_hollow_order_update_all_identity_permutations() {
        // 8 permutations of the three identity fields: hollow iff all three
        // are missing.
        for mask in 0u8..8 {
            let mut value = full_sample_value();
            let data = value["Data"].as_object_mut().unwrap();
            for (bit, key) in ["OrderNo", "Status", "SecurityId"].iter().enumerate() {
                if mask & (1 << bit) != 0 {
                    data.remove(*key);
                }
            }
            let update = parse_order_update(&value.to_string()).unwrap();
            assert_eq!(is_hollow_order_update(&update), mask == 7, "mask {mask}");
        }
    }

    #[test]
    fn test_fix_for_field_coercions() {
        use serde_json::json;
        let int = |v: serde_json::Value| fix_for_field("Quantity", FieldKind::Int, &v);
        assert_eq!(
            int(json!("50")),
            FieldFix::Replace(json!(50), "numeric_string_coerced")
        );
        assert_eq!(
            int(json!(" 50.0 ")),
            FieldFix::Replace(json!(50), "numeric_string_coerced")
        );
        assert_eq!(
            int(json!("-1")),
            FieldFix::Replace(json!(-1), "numeric_string_coerced")
        );
        // A blank fill field is never read as absent: it fails loud.
        assert_eq!(int(json!("  ")), FieldFix::Keep);
        let lot = |v: serde_json::Value| fix_for_field("LotSize", FieldKind::Int, &v);
        assert_eq!(lot(json!("  ")), FieldFix::Remove("empty_number_dropped"));
        assert_eq!(lot(json!(null)), FieldFix::Remove("null_field_dropped"));
        assert_eq!(int(json!("ten")), FieldFix::Keep);
        assert_eq!(int(json!("50.5")), FieldFix::Keep);
        assert_eq!(
            int(json!(50.0)),
            FieldFix::Replace(json!(50), "integral_float_coerced")
        );
        assert_eq!(int(json!(50.5)), FieldFix::Keep);
        assert_eq!(int(json!(1e20)), FieldFix::Keep);
        assert_eq!(int(json!(18_446_744_073_709_551_615u64)), FieldFix::Keep);
        assert_eq!(int(json!(50)), FieldFix::Keep);
        assert_eq!(int(json!(null)), FieldFix::Keep);
        assert_eq!(int(json!(true)), FieldFix::Keep);
        for fill in FILL_CRITICAL_FIELDS {
            for blank in [json!(null), json!(""), json!("  ")] {
                assert_eq!(
                    fix_for_field(fill, FieldKind::Int, &blank),
                    FieldFix::Keep,
                    "{fill} {blank}"
                );
            }
        }

        let float = |v: serde_json::Value| fix_for_field("Price", FieldKind::Float, &v);
        assert_eq!(
            float(json!("245.5")),
            FieldFix::Replace(json!(245.5), "numeric_string_coerced")
        );
        assert_eq!(float(json!("NaN")), FieldFix::Keep);
        assert_eq!(float(json!("inf")), FieldFix::Keep);
        assert_eq!(float(json!("")), FieldFix::Keep);
        assert_eq!(
            fix_for_field("StrikePrice", FieldKind::Float, &json!("")),
            FieldFix::Remove("empty_number_dropped")
        );
        assert_eq!(float(json!("abc")), FieldFix::Keep);
        assert_eq!(float(json!(245.5)), FieldFix::Keep);
        assert_eq!(float(json!([1])), FieldFix::Keep);

        let text = |v: serde_json::Value| fix_for_field("OrderNo", FieldKind::Text, &v);
        assert_eq!(
            text(json!(1_124_091_136_546i64)),
            FieldFix::Replace(json!("1124091136546"), "numeric_text_coerced")
        );
        // An identity id accepts integral numbers only.
        assert_eq!(text(json!(12.5)), FieldFix::Keep);
        assert_eq!(text(json!(1e30)), FieldFix::Keep);
        assert_eq!(
            text(json!(18_446_744_073_709_551_615u64)),
            FieldFix::Replace(json!("18446744073709551615"), "numeric_text_coerced")
        );
        for id in INTEGRAL_ID_TEXT_FIELDS {
            assert_eq!(
                fix_for_field(id, FieldKind::Text, &json!(1.5)),
                FieldFix::Keep
            );
        }
        assert_eq!(
            fix_for_field("Remarks", FieldKind::Text, &json!(12.5)),
            FieldFix::Replace(json!("12.5"), "numeric_text_coerced")
        );
        assert_eq!(text(json!("x")), FieldFix::Keep);
        assert_eq!(text(json!(true)), FieldFix::Keep);
        assert_eq!(text(json!({"a": 1})), FieldFix::Keep);
        assert_eq!(
            fix_for_field("AlgoOrdNo", FieldKind::Text, &json!(5)),
            FieldFix::Replace(json!("5"), "numeric_algo_ord_no")
        );
    }

    #[test]
    fn test_exact_i64_boundaries() {
        assert_eq!(exact_i64(i64::MIN as f64), Some(i64::MIN));
        assert_eq!(
            exact_i64(9_223_372_036_854_775_807.0),
            None,
            "2^63 is out of range"
        );
        assert_eq!(exact_i64(0.0), Some(0));
        assert_eq!(exact_i64(-0.0), Some(0));
        assert_eq!(exact_i64(0.5), None);
        assert_eq!(exact_i64(f64::NAN), None);
        assert_eq!(exact_i64(f64::INFINITY), None);
        assert_eq!(
            exact_i64(9_007_199_254_740_992.0),
            Some(9_007_199_254_740_992)
        );
    }

    #[test]
    fn test_parse_order_update_numbers_and_strings_swapped() {
        let json = r#"{"Data": {"OrderNo": 1124091136546, "SecurityId": 14366,
            "ExchOrderNo": 1400000000404591, "Quantity": "75", "TradedQty": 50.0,
            "Price": "245.5", "StrikePrice": "", "LegNo": " 1 ", "Status": "TRADED"}}"#;
        let update = parse_order_update(json).unwrap();
        assert_eq!(update.order_no, "1124091136546");
        assert_eq!(update.security_id, "14366");
        assert_eq!(update.exch_order_no, "1400000000404591");
        assert_eq!(update.quantity, 75);
        assert_eq!(update.traded_qty, 50);
        assert!((update.price - 245.5).abs() < f64::EPSILON);
        assert!(update.strike_price.abs() < f64::EPSILON);
        assert_eq!(update.leg_no, 1);
        // A value that does not convert exactly still fails loud.
        assert!(parse_order_update(r#"{"Data": {"OrderNo": "1", "Quantity": 1.5}}"#).is_err());
        assert!(parse_order_update(r#"{"Data": {"OrderNo": "1", "Status": true}}"#).is_err());
    }

    #[test]
    fn test_parse_order_update_bad_type_tag_never_costs_the_event() {
        for tag in ["null", "5", "true", "{}", "[]"] {
            let json = format!(r#"{{"Data": {{"OrderNo": "9"}}, "Type": {tag}}}"#);
            let update = parse_order_update(&json).unwrap();
            assert_eq!(update.order_no, "9", "Type {tag}");
        }
    }

    #[test]
    fn test_unreadable_order_frame_type_verdicts() {
        let verdict = |json: &str| {
            let update = parse_order_update(json).unwrap();
            unreadable_order_frame_type(&update, json)
        };
        // Readable order events: no verdict, whatever the tag.
        assert_eq!(
            verdict(r#"{"Data":{"OrderNo":"1","Status":"TRADED"},"Type":"order_alert"}"#),
            None
        );
        assert_eq!(
            verdict(r#"{"Data":{"orderNo":"1","status":"TRADED"},"Type":"heartbeat"}"#),
            None,
            "an order number makes the frame readable"
        );
        assert_eq!(
            verdict(r#"{"Data":{"Status":"PENDING","SecurityId":"5"}}"#),
            None
        );
        assert_eq!(
            verdict(r#"{"Data":{"Status":"PENDING"},"Type":"order_alert"}"#),
            None
        );
        // Hollow: always a verdict, with the tag for the log.
        assert_eq!(
            verdict(r#"{"Data":{"MktType":"XX"},"Type":"order_alert"}"#).as_deref(),
            Some("order_alert")
        );
        assert_eq!(
            verdict(r#"{"Data":{"MktType":"XX","RefLtp":11.15}}"#).as_deref(),
            Some("")
        );
        assert_eq!(
            verdict(r#"{"Data":{},"Type":" ORDER_ALERT "}"#).as_deref(),
            Some("ORDER_ALERT")
        );
        assert_eq!(
            verdict(r#"{"data":{},"type":"trade_alert"}"#).as_deref(),
            Some("trade_alert"),
            "an unknown tag is never suppressed"
        );
        assert_eq!(verdict(r#"{"Data":{},"Type":"   "}"#).as_deref(), Some(""));
        assert_eq!(verdict(r#"{"Data":{},"Type":5}"#).as_deref(), Some(""));
        assert_eq!(verdict(r#"{"Data":{},"Type":null}"#).as_deref(), Some(""));
        // Hostile review MEDIUM: a foreign-typed frame that now decodes a
        // camelCase status but carries no order number still pages.
        assert_eq!(
            verdict(r#"{"Data":{"status":"success","message":"ok"},"Type":"login_ack"}"#)
                .as_deref(),
            Some("login_ack")
        );
        // A frame that does not re-parse or is oversized reads an empty tag.
        let hollow = parse_order_update(r#"{"Data":{}}"#).unwrap();
        assert_eq!(
            unreadable_order_frame_type(&hollow, "not json").as_deref(),
            Some("")
        );
        assert_eq!(
            unreadable_order_frame_type(&hollow, "[1,2]").as_deref(),
            Some("")
        );
        let oversized = format!(
            r#"{{"Data": {{}}, "Type": "heartbeat", "Pad": "{}"}}"#,
            "x".repeat(ORDER_UPDATE_MAX_FRAME_BYTES)
        );
        assert_eq!(
            unreadable_order_frame_type(&hollow, &oversized).as_deref(),
            Some("")
        );
        // The tag is bounded for the log.
        let long = format!(r#"{{"Data": {{}}, "Type": "{}"}}"#, "₹".repeat(500));
        assert_eq!(
            unreadable_order_frame_type(&hollow, &long).map(|tag| tag.chars().count()),
            Some(MESSAGE_TYPE_LOG_MAX_CHARS)
        );
    }

    proptest::proptest! {
        /// Any subset of fields removed or nulled, each remaining field in a
        /// random casing: the frame always decodes, every kept field equals
        /// the full sample's value, and every dropped one is its default.
        #[test]
        fn prop_any_field_subset_and_casing_decodes(
            drop_mask in proptest::collection::vec(0u8..4, ORDER_UPDATE_FIELDS.len()),
        ) {
            let full = parse_order_update(FULL_ACCEPTED_SAMPLE).unwrap();
            let mut expected = serde_json::to_value(&full).unwrap();
            let mut frame = full_sample_value();
            let data = frame["Data"].as_object_mut().unwrap();
            for (&(accepted, alt, kind), &choice) in ORDER_UPDATE_FIELDS.iter().zip(&drop_mask) {
                match choice {
                    0 => {}
                    1 => {
                        let moved = data.remove(accepted).unwrap();
                        data.insert(alt.to_string(), moved);
                    }
                    2 => {
                        data.remove(accepted);
                        expected[accepted] = default_for(kind);
                    }
                    // A null fill field fails loud (pinned by
                    // `test_every_single_field_missing_or_null_still_parses`),
                    // so this decode-always property removes it instead.
                    _ if FILL_CRITICAL_FIELDS.contains(&accepted) => {
                        data.remove(accepted);
                        expected[accepted] = default_for(kind);
                    }
                    _ => {
                        data.insert(accepted.to_string(), serde_json::Value::Null);
                        expected[accepted] = default_for(kind);
                    }
                }
            }
            let expected: OrderUpdate = serde_json::from_value(expected).unwrap();
            let decoded = parse_order_update(&frame.to_string()).unwrap();
            proptest::prop_assert_eq!(decoded, expected);
        }
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
