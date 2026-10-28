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
/// missing/null/non-object `Data`, a top-level array) keep failing; an
/// array `Data` or top-level array is refused before the typed parse.
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
    refuse_array_frame(&value)?;
    let message: OrderUpdateMessage = serde_json::from_value(value)?;
    Ok(message.data)
}

/// Refuses a top-level array or an array `Data` (after the wrapper casing
/// pass) as a structural error. Without this, serde's derived sequence
/// visitor decodes `{"Data":[...]}` POSITIONALLY (or as an all-default
/// update for `[]`) and `[{"Data":{...}}]` as a hollow update, bypassing
/// the casing and coercion pass. Scalar and null `Data` already fail the
/// typed parse. Pure, O(1), no allocation on the accept path.
fn refuse_array_frame(value: &serde_json::Value) -> Result<(), OrderUpdateParseError> {
    let is_array = value.is_array() || value.get("Data").is_some_and(serde_json::Value::is_array);
    if is_array {
        return Err(OrderUpdateParseError::JsonError(
            <serde_json::Error as serde::de::Error>::invalid_type(
                serde::de::Unexpected::Seq,
                &"an object",
            ),
        ));
    }
    Ok(())
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
        if fails_loud_when_blank(accepted)
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
#[cfg(test)]
const FILL_CRITICAL_FIELDS: &[&str] = &[
    "Quantity",
    "TradedQty",
    "RemainingQuantity",
    "Price",
    "TradedPrice",
    "AvgTradedPrice",
];

/// Whether `key` is a fill-critical field (the list above): one fixed `match`, no
/// list walk. Kept in step with the list by
/// `test_is_fill_critical_matches_the_list`.
fn is_fill_critical(key: &str) -> bool {
    matches!(
        key,
        "Quantity" | "TradedQty" | "RemainingQuantity" | "Price" | "TradedPrice" | "AvgTradedPrice"
    )
}

/// The fields whose `null` must keep failing the typed parse loudly
/// (ORDER-EVT-02 `typed_parse_failed`, the order-path loss page) instead of
/// reading as absent: the fill fields, plus `OrderNo` and `Status`, which
/// decide whether the OMS applies a fill at all. A null `Status` read as
/// absent parses to `""`, which the OMS skips as an unknown status with no
/// page; a null `OrderNo` skips the update as an unknown order. One fixed
/// `match`, no list walk.
fn fails_loud_when_blank(key: &str) -> bool {
    is_fill_critical(key) || matches!(key, "OrderNo" | "Status")
}

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
/// - `null` or an empty string in a [`is_fill_critical`] field → keep,
///   so the typed parse fails loud (a defaulted 0 would lose the fill).
/// - `null` in any other field → remove: null means absent, and every
///   `OrderUpdate` field defaults when absent. Before 2026-10-28 one null
///   field (say `"CorrelationId": null`) failed the whole typed parse and
///   the order event was dropped.
/// - Text field holding a number → its exact string rendering (integral
///   values without `.0`). `AlgoOrdNo` keeps its own quirk kind
///   (`numeric_algo_ord_no`, the live doc table types it `float`). A
///   [`INTEGRAL_ID_TEXT_FIELDS`] field accepts integral numbers only.
/// - Int field holding a string → the integer it spells EXACTLY (`"50"`,
///   `"50.0"`, `"1e3"`; see [`exact_decimal_i64`]); a fractional or
///   out-of-range spelling is kept (loud), never rounded; an empty string →
///   remove.
/// - Int field holding an integral float (`50.0`) with magnitude below 2^53
///   → that integer. At or past 2^53 the `f64` serde read may stand for a
///   different wire integer, so it is kept (loud). Honest limit: serde has
///   already rounded a JSON number to `f64` before this runs, so a
///   fractional wire number whose nearest `f64` is integral
///   (`1.00000000000000001`) reads as that integer; only a string spelling
///   is judged digit by digit.
/// - Float field holding a string → the finite number it spells; an empty
///   string → remove.
/// - Anything else (a fractional quantity, `"ten"`, an object, an array, a
///   bool, a non-finite number) → keep, so the typed parse fails loud.
fn fix_for_field(key: &str, kind: FieldKind, value: &serde_json::Value) -> FieldFix {
    use serde_json::Value;
    if is_blank_value(value) && fails_loud_when_blank(key) {
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
            match exact_decimal_i64(trimmed) {
                Some(integral) => {
                    FieldFix::Replace(Value::from(integral), "numeric_string_coerced")
                }
                None => FieldFix::Keep,
            }
        }
        (FieldKind::Int, Value::Number(number)) if number.as_i64().is_none() => {
            // Only below 2^53: every integer there is an exact f64, so the
            // f64 serde read cannot stand for a different integer. At or
            // past 2^53 two wire integers share one f64
            // (9007199254740993.0 reads as 2^53), so it is refused.
            match number
                .as_f64()
                .filter(|float| float.abs() < F64_EXACT_INTEGER_LIMIT)
                .and_then(exact_i64)
            {
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

/// 2^53. Below it every integer is an exact `f64`, so an integral `f64`
/// there names exactly one wire integer; at or past it two integers can
/// share one `f64` (`9007199254740993.0` reads as 2^53).
const F64_EXACT_INTEGER_LIMIT: f64 = 9_007_199_254_740_992.0;

/// The `i64` a decimal string spells EXACTLY, never rounded: an optional
/// sign, digits with an optional `.` fraction, an optional `e`/`E`
/// exponent (the `f64` grammar without `inf`/`nan`). `None` when the value
/// has a non-zero fractional part (`"50.5"`, `"5e-1"`), lies outside the
/// `i64` range, or the text is not such a decimal. Unlike a parse through
/// `f64`, `"9007199254740993.0"` is 9007199254740993, never 2^53.
///
/// Pure, O(text length), no allocation. Cold: the order-update read task.
fn exact_decimal_i64(text: &str) -> Option<i64> {
    let bytes = text.as_bytes();
    let (negative, rest) = match bytes.split_first() {
        Some((b'-', after)) => (true, after),
        Some((b'+', after)) => (false, after),
        _ => (false, bytes),
    };
    let int_len = rest.iter().take_while(|b| b.is_ascii_digit()).count();
    let (int_digits, rest) = rest.split_at(int_len);
    let (frac_digits, rest) = match rest.split_first() {
        Some((b'.', after)) => {
            let frac_len = after.iter().take_while(|b| b.is_ascii_digit()).count();
            after.split_at(frac_len)
        }
        _ => rest.split_at(0),
    };
    if int_digits.is_empty() && frac_digits.is_empty() {
        return None;
    }
    let exponent: i64 = match rest.split_first() {
        None => 0,
        Some((b'e' | b'E', after)) => {
            let (exp_negative, exp_digits) = match after.split_first() {
                Some((b'-', digits)) => (true, digits),
                Some((b'+', digits)) => (false, digits),
                _ => (false, after),
            };
            if exp_digits.is_empty() || !exp_digits.iter().all(u8::is_ascii_digit) {
                return None;
            }
            let magnitude = exp_digits.iter().fold(0i64, |acc, digit| {
                acc.saturating_mul(10)
                    .saturating_add(i64::from(digit.saturating_sub(b'0')))
            });
            if exp_negative {
                0i64.saturating_sub(magnitude)
            } else {
                magnitude
            }
        }
        Some(_) => return None,
    };
    let digits = || int_digits.iter().chain(frac_digits);
    let total = int_digits.len().saturating_add(frac_digits.len());
    let trailing_zeros = digits().rev().take_while(|&&digit| digit == b'0').count();
    if trailing_zeros == total {
        return Some(0);
    }
    // Value = (all digits as an integer) x 10^shift.
    let shift = exponent.saturating_sub(i64::try_from(frac_digits.len()).ok()?);
    let (keep, scale) = if shift < 0 {
        let drop = usize::try_from(shift.unsigned_abs()).ok()?;
        if drop > trailing_zeros {
            return None; // a non-zero digit sits after the decimal point
        }
        (total.saturating_sub(drop), 0)
    } else {
        (total, shift.unsigned_abs())
    };
    // 2^63 - 1, or 2^63 for a negative value (i64::MIN).
    let limit = u128::from(i64::MAX.unsigned_abs()).saturating_add(u128::from(negative));
    let mut magnitude: u128 = 0;
    for digit in digits().take(keep) {
        magnitude = magnitude
            .saturating_mul(10)
            .saturating_add(u128::from(digit.saturating_sub(b'0')));
        if magnitude > limit {
            return None;
        }
    }
    // `magnitude` holds a non-zero digit here, so this ends within 20 steps.
    for _ in 0..scale {
        magnitude = magnitude.saturating_mul(10);
        if magnitude > limit {
            return None;
        }
    }
    let signed = i128::try_from(magnitude).ok()?;
    i64::try_from(if negative {
        signed.checked_neg()?
    } else {
        signed
    })
    .ok()
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
/// `order_no`, `status` and `security_id` are all empty after trimming
/// (a whitespace-only value is blank, as in `is_blank_value`). Pure, no
/// allocation, O(length of the three fields).
///
/// `OrderUpdate` defaults every field, so a frame whose `Data` keys match
/// nothing the struct knows still parses `Ok` and decodes to this shape.
/// A real partial update (say only `OrderNo`) is NOT hollow.
pub fn is_hollow_order_update(update: &OrderUpdate) -> bool {
    update.order_no.trim().is_empty()
        && update.status.trim().is_empty()
        && update.security_id.trim().is_empty()
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
/// The returned tag is the frame's string `Type` (or a drifted string
/// `type` when `Type` is absent or not a string), trimmed and bounded to
/// `MESSAGE_TYPE_LOG_MAX_CHARS` characters; empty when neither casing holds
/// a string, or the frame is oversized or does not re-parse. An order
/// number, status or security id made only of whitespace counts as absent.
///
/// # Complexity
/// Allocation-free for a frame that carries an order number (the
/// normal case). Otherwise one more `serde_json::Value` parse of the frame,
/// O(frame bytes) bounded by [`ORDER_UPDATE_MAX_FRAME_BYTES`]. Cold: the
/// order-update read task, 1 to 100 orders a day.
pub fn unreadable_order_frame_type(update: &OrderUpdate, json_str: &str) -> Option<String> {
    if !update.order_no.trim().is_empty() {
        return None;
    }
    let parsed = (json_str.len() <= ORDER_UPDATE_MAX_FRAME_BYTES)
        .then(|| serde_json::from_str::<serde_json::Value>(json_str).ok())
        .flatten();
    let tag = parsed
        .as_ref()
        .and_then(|value| {
            // A `Type` that is present but not a string (null, a number, an
            // object) is as unreadable as an absent one, so the drifted
            // lowercase `type` is read in both cases.
            value
                .get("Type")
                .and_then(serde_json::Value::as_str)
                .or_else(|| value.get("type").and_then(serde_json::Value::as_str))
        })
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

    /// Regression (PR #2134 review): a null `Status` or `OrderNo` on a TRADED
    /// frame read as absent parsed to `""`, and the OMS skipped the fill as
    /// an unknown status or unknown order with no page. Both must keep
    /// failing the typed parse loudly, in either casing.
    #[test]
    fn test_fails_loud_when_blank_null_status_or_order_no_never_parses() {
        let frames = [
            r#"{"Data":{"OrderNo":"1124","Status":null,"TradedQty":75,"Quantity":75,"AvgTradedPrice":245.4},"Type":"order_alert"}"#,
            r#"{"Data":{"OrderNo":"1124","status":null,"TradedQty":75,"Quantity":75},"Type":"order_alert"}"#,
            r#"{"Data":{"OrderNo":null,"Status":"TRADED","TradedQty":75,"Quantity":75},"Type":"order_alert"}"#,
            r#"{"Data":{"orderNo":null,"Status":"TRADED","TradedQty":75,"Quantity":75},"Type":"order_alert"}"#,
        ];
        for frame in frames {
            assert!(
                parse_order_update(frame).is_err(),
                "must fail loud: {frame}"
            );
        }
        assert!(fails_loud_when_blank("Status"));
        assert!(fails_loud_when_blank("OrderNo"));
        assert!(fails_loud_when_blank("TradedQty"));
        assert!(!fails_loud_when_blank("Series"));
        // Any other null still reads as absent.
        let ok = r#"{"Data":{"OrderNo":"1124","Status":"TRADED","CorrelationId":null},"Type":"order_alert"}"#;
        assert_eq!(parse_order_update(ok).unwrap().order_no, "1124");
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
            if fails_loud_when_blank(accepted) {
                assert!(
                    parse_order_update(&null.to_string()).is_err(),
                    "{accepted} null must fail loud"
                );
                // An empty string fails the typed parse only in a numeric
                // fill field; `""` is a valid text value (as before 10-28).
                if FILL_CRITICAL_FIELDS.contains(&accepted) {
                    let mut blank = full_sample_value();
                    blank["Data"][accepted] = serde_json::Value::from("");
                    assert!(
                        parse_order_update(&blank.to_string()).is_err(),
                        "{accepted} empty must fail loud"
                    );
                }
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
    fn test_is_fill_critical_matches_the_list() {
        for &field in FILL_CRITICAL_FIELDS {
            assert!(is_fill_critical(field), "{field} is in the list");
        }
        for &(accepted, alt, _) in ORDER_UPDATE_FIELDS {
            assert_eq!(
                is_fill_critical(accepted),
                FILL_CRITICAL_FIELDS.contains(&accepted),
                "{accepted}"
            );
            assert_eq!(
                is_fill_critical(alt),
                FILL_CRITICAL_FIELDS.contains(&alt),
                "{alt}"
            );
        }
        assert!(!is_fill_critical(""));
        assert!(!is_fill_critical("quantity"), "casing matters");
    }

    #[test]
    fn test_every_field_missing_decodes_hollow() {
        let update = parse_order_update(r#"{"Data": {}, "Type": "order_alert"}"#).unwrap();
        assert!(is_hollow_order_update(&update));
        // Every other field null (the fail-loud fields absent): hollow.
        let mut all_null = full_sample_value();
        let data = all_null["Data"].as_object_mut().unwrap();
        data.retain(|key, _| !fails_loud_when_blank(key));
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
    fn test_exact_decimal_i64_never_rounds() {
        let rows: &[(&str, Option<i64>)] = &[
            ("0", Some(0)),
            ("-0.000", Some(0)),
            ("0e99999999999999999999", Some(0)),
            ("50", Some(50)),
            ("+50.00", Some(50)),
            ("5.", Some(5)),
            ("50e-1", Some(5)),
            ("1.5e1", Some(15)),
            ("0.0000000000000000000001e22", Some(1)),
            ("00000000000000000000000000000042", Some(42)),
            ("9007199254740993.0", Some(9_007_199_254_740_993)),
            ("18014398509481985.0", Some(18_014_398_509_481_985)),
            ("9223372036854775807.0", Some(i64::MAX)),
            ("-9223372036854775808.0", Some(i64::MIN)),
            ("9223372036854775808.0", None),
            ("-9223372036854775809", None),
            ("1e19", None),
            ("1e99999999999999999999", None),
            ("1.00000000000000001", None),
            ("4503599627370496.5", None),
            ("5e-1", None),
            ("1e-99999999999999999999", None),
            (".5", None),
            ("", None),
            (".", None),
            ("+", None),
            ("e5", None),
            ("5e", None),
            ("5e+", None),
            ("5ee1", None),
            ("1.2.3", None),
            ("--5", None),
            ("0x10", None),
            ("inf", None),
            ("NaN", None),
            ("５０", None),
            ("1_000", None),
            ("50 ", None),
        ];
        for (text, expected) in rows {
            assert_eq!(exact_decimal_i64(text), *expected, "{text:?}");
        }
    }

    #[test]
    fn test_refuse_array_frame() {
        use serde_json::json;
        assert!(refuse_array_frame(&json!({"Data": {}})).is_ok());
        assert!(refuse_array_frame(&json!({"Data": null})).is_ok());
        assert!(refuse_array_frame(&json!({"OrderNo": "1"})).is_ok());
        assert!(
            refuse_array_frame(&json!("x")).is_ok(),
            "the typed parse refuses it"
        );
        for bad in [json!([]), json!([{"Data": {}}]), json!({"Data": []})] {
            assert!(
                matches!(
                    refuse_array_frame(&bad),
                    Err(OrderUpdateParseError::JsonError(_))
                ),
                "{bad}"
            );
        }
        // A JSON number past 2^53 in an Int field is refused, never rounded.
        assert_eq!(
            fix_for_field("LotSize", FieldKind::Int, &json!(9_007_199_254_740_992.0)),
            FieldFix::Keep
        );
        assert_eq!(
            fix_for_field("LotSize", FieldKind::Int, &json!(9_007_199_254_740_991.0)),
            FieldFix::Replace(json!(9_007_199_254_740_991i64), "integral_float_coerced")
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
                    _ if fails_loud_when_blank(accepted) => {
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
    // -----------------------------------------------------------------------
    // Gap audit 2026-10-28: exhaustive grids over the normalizer, the field
    // fixer, the quirk counters and the ORDER-EVT-02 verdict. Each grid is
    // checked against an independent oracle written from the documented
    // rules, never against the code under test.
    // -----------------------------------------------------------------------
    mod gap_grid {
        use super::*;
        use proptest::prelude::*;
        use serde_json::{Value, json};
        use std::collections::BTreeMap;
        use std::sync::{Arc, Mutex};

        /// `Type` values that are present but not a string.
        const JUNK_TYPE_TAGS: [&str; 5] = ["null", "5", "true", "{}", "[]"];

        fn verdict(json: &str) -> Option<String> {
            let update = parse_order_update(json).unwrap_or_else(|e| panic!("{json}: {e}"));
            unreadable_order_frame_type(&update, json)
        }

        fn frame_with_data(data: serde_json::Map<String, Value>) -> String {
            json!({ "Data": Value::Object(data), "Type": "order_alert" }).to_string()
        }

        /// The full sample with `accepted` and `alt` both removed.
        fn sample_data_without(accepted: &str, alt: &str) -> serde_json::Map<String, Value> {
            let value = full_sample_value();
            let mut data = value["Data"].as_object().unwrap().clone();
            data.remove(accepted);
            data.remove(alt);
            data
        }

        /// The full sample's struct with `accepted` set to `field` (built
        /// through serde, not the parser).
        fn expected_with(accepted: &str, field: Value) -> OrderUpdate {
            let full = parse_order_update(FULL_ACCEPTED_SAMPLE).unwrap();
            let mut value = serde_json::to_value(&full).unwrap();
            value[accepted] = field;
            serde_json::from_value(value).unwrap()
        }

        // ---------------------------------------------------------------
        // Gap 1: a junk `Type` next to a lowercase `type`.
        // ---------------------------------------------------------------

        #[test]
        fn test_unreadable_order_frame_type_junk_type_tag_with_order_alert_or_no_lowercase_tag() {
            // Rows where today's behaviour is already right: no lowercase
            // tag, or a lowercase `order_alert`, next to a junk `Type`.
            for junk in JUNK_TYPE_TAGS {
                for lower in ["", r#","type":"order_alert""#, r#","type":" ORDER_ALERT ""#] {
                    let json = format!(r#"{{"Data":{{"status":"success"}},"Type":{junk}{lower}}}"#);
                    let update = parse_order_update(&json).unwrap();
                    assert_eq!(update.status, "success", "{json}");
                    assert_eq!(update.order_no, "", "{json}");
                    assert_eq!(unreadable_order_frame_type(&update, &json), None, "{json}");
                }
            }
            // Control: with `Type` ABSENT the lowercase tag is moved onto
            // `Type` and a foreign tag pages, as documented.
            assert_eq!(
                verdict(r#"{"Data":{"status":"success"},"type":"login_ack"}"#).as_deref(),
                Some("login_ack")
            );
        }

        #[test]
        fn test_unreadable_order_frame_type_junk_type_falls_back_to_foreign_lowercase_type() {
            let mut misses = Vec::new();
            for junk in JUNK_TYPE_TAGS {
                let json = format!(
                    r#"{{"Data":{{"status":"success"}},"Type":{junk},"type":"login_ack"}}"#
                );
                if verdict(&json).as_deref() != Some("login_ack") {
                    misses.push(json);
                }
            }
            assert!(misses.is_empty(), "foreign frames not paged: {misses:?}");
        }

        // ---------------------------------------------------------------
        // Gap 2: whitespace-only identity fields.
        // ---------------------------------------------------------------

        /// The three identity fields with a real value each.
        const IDENTITY: [(&str, &str, &str); 3] = [
            ("OrderNo", "orderNo", "1124"),
            ("Status", "status", "TRADED"),
            ("SecurityId", "securityId", "52432"),
        ];

        /// One identity slot: absent, or a value text under one casing.
        /// `None` text = absent.
        fn identity_options(real: &'static str) -> Vec<Option<(&'static str, bool)>> {
            let mut options = vec![None];
            for text in ["", "  ", "\t", real] {
                for alt in [false, true] {
                    options.push(Some((text, alt)));
                }
            }
            options
        }

        /// Every one of the 9 x 9 x 9 identity combinations: the frame,
        /// the trimmed-empty oracle and the exactly-empty (today's) rule.
        fn identity_grid() -> Vec<(String, bool, bool, [Option<&'static str>; 3])> {
            let per_field: Vec<_> = IDENTITY.iter().map(|f| identity_options(f.2)).collect();
            let mut rows = Vec::new();
            for a in &per_field[0] {
                for b in &per_field[1] {
                    for c in &per_field[2] {
                        let mut data = serde_json::Map::new();
                        let mut sent = [None; 3];
                        for (i, slot) in [a, b, c].into_iter().enumerate() {
                            if let Some((text, alt)) = slot {
                                let key = if *alt { IDENTITY[i].1 } else { IDENTITY[i].0 };
                                data.insert(key.to_string(), json!(text));
                                sent[i] = Some(*text);
                            }
                        }
                        let trimmed_blank =
                            sent.iter().all(|s| s.is_none_or(|t| t.trim().is_empty()));
                        let exactly_blank = sent.iter().all(|s| s.is_none_or(str::is_empty));
                        rows.push((frame_with_data(data), trimmed_blank, exactly_blank, sent));
                    }
                }
            }
            rows
        }

        #[test]
        fn test_is_hollow_order_update_identity_grid_without_whitespace_rows() {
            let rows = identity_grid();
            assert_eq!(rows.len(), 729);
            let mut checked = 0;
            for (json, trimmed_blank, exactly_blank, sent) in rows {
                let update = parse_order_update(&json).unwrap_or_else(|e| panic!("{json}: {e}"));
                // Each field decodes to exactly what was sent, in either casing.
                assert_eq!(update.order_no, sent[0].unwrap_or(""), "{json}");
                assert_eq!(update.status, sent[1].unwrap_or(""), "{json}");
                assert_eq!(update.security_id, sent[2].unwrap_or(""), "{json}");
                // Hollow means blank after trimming (whitespace-only is blank).
                assert_eq!(is_hollow_order_update(&update), trimmed_blank, "{json}");
                // Rows with no whitespace-only identity: both rules agree.
                if trimmed_blank == exactly_blank {
                    let expected = trimmed_blank.then(|| "order_alert".to_string());
                    assert_eq!(
                        unreadable_order_frame_type(&update, &json),
                        expected,
                        "{json}"
                    );
                    checked += 1;
                }
            }
            // 5^3 rows have no whitespace-only identity at all, and some
            // whitespace rows still carry a real value elsewhere.
            assert!(checked >= 125, "only {checked} rows checked");
        }

        #[test]
        fn test_unreadable_order_frame_type_whitespace_only_identity_is_hollow() {
            let mut misses = Vec::new();
            for (json, trimmed_blank, _, _) in identity_grid() {
                let expected = trimmed_blank.then(|| "order_alert".to_string());
                if verdict(&json) != expected {
                    misses.push(json);
                }
            }
            assert!(
                misses.is_empty(),
                "{} rows disagree, e.g. {:?}",
                misses.len(),
                misses.first()
            );
        }

        // ---------------------------------------------------------------
        // Gap 3: the eight fail-loud fields, accepted x alt state matrix.
        // ---------------------------------------------------------------

        #[derive(Clone, Copy, Debug, PartialEq)]
        enum Slot {
            Absent,
            Null,
            Empty,
            Spaces,
            Valid,
        }

        const SLOTS: [Slot; 5] = [
            Slot::Absent,
            Slot::Null,
            Slot::Empty,
            Slot::Spaces,
            Slot::Valid,
        ];

        /// Distinct valid values for the accepted and the alt casing, so the
        /// winner is visible.
        fn valid_pair(kind: FieldKind) -> (Value, Value) {
            match kind {
                FieldKind::Text => (json!("ACC"), json!("ALT")),
                FieldKind::Int => (json!(50), json!(60)),
                FieldKind::Float => (json!(245.5), json!(246.5)),
            }
        }

        fn slot_value(slot: Slot, valid: &Value) -> Option<Value> {
            match slot {
                Slot::Absent => None,
                Slot::Null => Some(Value::Null),
                Slot::Empty => Some(json!("")),
                Slot::Spaces => Some(json!("  ")),
                Slot::Valid => Some(valid.clone()),
            }
        }

        /// Oracle: which slot survives the merge, and from which casing.
        /// A null accepted key is absent; a blank (non-null) accepted key is
        /// a value and wins; a blank alt with no accepted key is moved onto
        /// the accepted key so it fails loud.
        fn survivor(accepted: Slot, alt: Slot) -> (Slot, bool) {
            match (accepted, alt) {
                (Slot::Absent, Slot::Absent) => (Slot::Absent, false),
                (Slot::Absent, b) => (b, true),
                (Slot::Null, Slot::Absent | Slot::Null) => (Slot::Null, false),
                (Slot::Null, b) => (b, true),
                (a, _) => (a, false),
            }
        }

        #[test]
        fn test_normalize_order_update_frame_fail_loud_fields_accepted_by_alt_matrix() {
            let fields: Vec<_> = ORDER_UPDATE_FIELDS
                .iter()
                .filter(|f| fails_loud_when_blank(f.0))
                .collect();
            assert_eq!(fields.len(), 8);
            let mut rows = 0;
            for &&(accepted, alt, kind) in &fields {
                let (acc_valid, alt_valid) = valid_pair(kind);
                for a in SLOTS {
                    for b in SLOTS {
                        let mut data = sample_data_without(accepted, alt);
                        if let Some(v) = slot_value(a, &acc_valid) {
                            data.insert(accepted.to_string(), v);
                        }
                        if let Some(v) = slot_value(b, &alt_valid) {
                            data.insert(alt.to_string(), v);
                        }
                        let json = frame_with_data(data);
                        let (slot, from_alt) = survivor(a, b);
                        let expected = match slot {
                            Slot::Absent => Some(default_for(kind)),
                            Slot::Null => None,
                            Slot::Empty | Slot::Spaces if kind != FieldKind::Text => None,
                            Slot::Empty => Some(json!("")),
                            Slot::Spaces => Some(json!("  ")),
                            Slot::Valid if from_alt => Some(alt_valid.clone()),
                            Slot::Valid => Some(acc_valid.clone()),
                        };
                        let note = match (a, b) {
                            (Slot::Null, Slot::Valid) => {
                                " (asymmetry: a null accepted key yields to a valid alt)"
                            }
                            (Slot::Empty | Slot::Spaces, Slot::Valid) => {
                                " (asymmetry: a blank accepted key is kept over a valid alt)"
                            }
                            _ => "",
                        };
                        let got = parse_order_update(&json);
                        match expected {
                            None => assert!(
                                got.is_err(),
                                "{accepted} {a:?} / {alt} {b:?}{note}: {got:?}"
                            ),
                            Some(field) => {
                                let got = got.unwrap_or_else(|e| {
                                    panic!("{accepted} {a:?} / {alt} {b:?}{note}: {e}")
                                });
                                assert_eq!(
                                    got,
                                    expected_with(accepted, field),
                                    "{accepted} {a:?} / {alt} {b:?}{note}"
                                );
                            }
                        }
                        rows += 1;
                    }
                }
            }
            assert_eq!(rows, 200);
        }

        // ---------------------------------------------------------------
        // Gap 4: every field x both casings x 12 value classes.
        // ---------------------------------------------------------------

        const VALUE_CLASSES: [&str; 12] = [
            "null", r#""""#, r#""  ""#, "7", "7.0", "7.5", r#""7""#, r#""7.0""#, r#""x""#, "true",
            "{}", "[]",
        ];

        /// Independent oracle: the decoded field value, or `None` for a
        /// loud parse failure.
        fn field_oracle(key: &str, kind: FieldKind, raw: &str) -> Option<Value> {
            let fail_loud = fails_loud_when_blank(key);
            let fill = is_fill_critical(key);
            let is_id = INTEGRAL_ID_TEXT_FIELDS.contains(&key);
            match (raw, kind) {
                ("null", _) if fail_loud => None,
                ("null", _) => Some(default_for(kind)),
                (r#""""#, FieldKind::Text) => Some(json!("")),
                (r#""  ""#, FieldKind::Text) => Some(json!("  ")),
                (r#""""# | r#""  ""#, _) if fill => None,
                (r#""""# | r#""  ""#, _) => Some(default_for(kind)),
                ("7" | r#""7""#, FieldKind::Text) => Some(json!("7")),
                ("7.0", FieldKind::Text) if is_id => None,
                ("7.0", FieldKind::Text) => Some(json!("7")),
                ("7.5", FieldKind::Text) if is_id => None,
                ("7.5", FieldKind::Text) => Some(json!("7.5")),
                (r#""7.0""#, FieldKind::Text) => Some(json!("7.0")),
                (r#""x""#, FieldKind::Text) => Some(json!("x")),
                ("7" | "7.0" | r#""7""# | r#""7.0""#, FieldKind::Int) => Some(json!(7)),
                ("7" | "7.0" | r#""7""# | r#""7.0""#, FieldKind::Float) => Some(json!(7.0)),
                ("7.5", FieldKind::Float) => Some(json!(7.5)),
                _ => None,
            }
        }

        #[test]
        fn test_fix_for_field_every_field_both_casings_every_value_class() {
            let mut cases = 0;
            for &(accepted, alt, kind) in ORDER_UPDATE_FIELDS {
                for use_alt in [false, true] {
                    for raw in VALUE_CLASSES {
                        let mut data = sample_data_without(accepted, alt);
                        let key = if use_alt { alt } else { accepted };
                        data.insert(key.to_string(), serde_json::from_str(raw).unwrap());
                        let json = frame_with_data(data);
                        let got = parse_order_update(&json);
                        match field_oracle(accepted, kind, raw) {
                            None => assert!(got.is_err(), "{key}={raw}: {got:?}"),
                            Some(field) => assert_eq!(
                                got.unwrap_or_else(|e| panic!("{key}={raw}: {e}")),
                                expected_with(accepted, field),
                                "{key}={raw}"
                            ),
                        }
                        cases += 1;
                    }
                }
            }
            assert_eq!(cases, 1_104);
        }

        // ---------------------------------------------------------------
        // Gap 5: the decode-quirk counters.
        // ---------------------------------------------------------------

        type CounterMap = Arc<Mutex<BTreeMap<String, u64>>>;

        #[derive(Default)]
        struct QuirkRecorder {
            counters: CounterMap,
        }

        struct QuirkCounter {
            key: String,
            counters: CounterMap,
        }

        impl metrics::CounterFn for QuirkCounter {
            fn increment(&self, value: u64) {
                if let Ok(mut m) = self.counters.lock() {
                    let slot = m.entry(self.key.clone()).or_insert(0);
                    *slot = slot.saturating_add(value);
                }
            }
            fn absolute(&self, value: u64) {
                if let Ok(mut m) = self.counters.lock() {
                    m.insert(self.key.clone(), value);
                }
            }
        }

        impl metrics::Recorder for QuirkRecorder {
            fn describe_counter(
                &self,
                _: metrics::KeyName,
                _: Option<metrics::Unit>,
                _: metrics::SharedString,
            ) {
            }
            fn describe_gauge(
                &self,
                _: metrics::KeyName,
                _: Option<metrics::Unit>,
                _: metrics::SharedString,
            ) {
            }
            fn describe_histogram(
                &self,
                _: metrics::KeyName,
                _: Option<metrics::Unit>,
                _: metrics::SharedString,
            ) {
            }
            fn register_counter(
                &self,
                key: &metrics::Key,
                _: &metrics::Metadata<'_>,
            ) -> metrics::Counter {
                // `name` alone for any other counter, `kind` value for ours.
                let rendered = if key.name() == DECODE_QUIRKS_COUNTER {
                    key.labels()
                        .find(|l| l.key() == "kind")
                        .map(|l| l.value().to_string())
                        .unwrap_or_else(|| "<no kind label>".to_string())
                } else {
                    format!("other:{}", key.name())
                };
                metrics::Counter::from_arc(Arc::new(QuirkCounter {
                    key: rendered,
                    counters: Arc::clone(&self.counters),
                }))
            }
            fn register_gauge(
                &self,
                _: &metrics::Key,
                _: &metrics::Metadata<'_>,
            ) -> metrics::Gauge {
                metrics::Gauge::noop()
            }
            fn register_histogram(
                &self,
                _: &metrics::Key,
                _: &metrics::Metadata<'_>,
            ) -> metrics::Histogram {
                metrics::Histogram::noop()
            }
        }

        fn quirks_of(json: &str) -> BTreeMap<String, u64> {
            let recorder = QuirkRecorder::default();
            metrics::with_local_recorder(&recorder, || {
                let _ = parse_order_update(json);
            });
            recorder.counters.lock().unwrap().clone()
        }

        #[test]
        fn test_normalize_order_update_frame_quirk_counters_exact_counts() {
            let rows: &[(&str, &[(&str, u64)])] = &[
                (FULL_ACCEPTED_SAMPLE, &[]),
                (
                    r#"{"data":{"OrderNo":"1"},"type":"order_alert"}"#,
                    &[("wrapper_casing", 2)],
                ),
                (
                    r#"{"data":{"OrderNo":"1"},"type":5}"#,
                    &[("wrapper_casing", 2), ("type_not_string", 1)],
                ),
                (
                    r#"{"Data":{"OrderNo":"1"},"Type":5}"#,
                    &[("type_not_string", 1)],
                ),
                (
                    r#"{"Data":{"OrderNo":"1"},"Type":null,"type":"order_alert"}"#,
                    &[("type_not_string", 1)],
                ),
                (r#"{"Data":{"orderNo":"1"}}"#, &[("casing_fallback", 1)]),
                (
                    r#"{"Data":{"OrderNo":"1","orderNo":"2"}}"#,
                    &[("casing_conflict", 1)],
                ),
                (r#"{"Data":{"OrderNo":"1","orderNo":"1"}}"#, &[]),
                (
                    r#"{"Data":{"OrderNo":"1","Multiplier":1,"multiplier":1.0}}"#,
                    &[],
                ),
                (
                    r#"{"Data":{"OrderNo":"1","exchange":null}}"#,
                    &[("null_alt_dropped", 1)],
                ),
                (
                    r#"{"Data":{"OrderNo":"1","Exchange":null,"exchange":null}}"#,
                    &[("null_alt_dropped", 1), ("null_field_dropped", 1)],
                ),
                (
                    r#"{"Data":{"OrderNo":"1","instrumentType":"EQ"}}"#,
                    &[("assumed_twin", 1)],
                ),
                (
                    r#"{"Data":{"OrderNo":"1","instrumentType":5}}"#,
                    &[("assumed_twin", 1), ("numeric_text_coerced", 1)],
                ),
                (
                    r#"{"Data":{"OrderNo":"1","Instrument":null,"instrumentType":"EQ"}}"#,
                    &[("null_field_dropped", 1), ("assumed_twin", 1)],
                ),
                (
                    r#"{"Data":{"OrderNo":"1","Remarks":null}}"#,
                    &[("null_field_dropped", 1)],
                ),
                (r#"{"Data":{"OrderNo":5}}"#, &[("numeric_text_coerced", 1)]),
                (
                    r#"{"Data":{"OrderNo":"1","AlgoOrdNo":5}}"#,
                    &[("numeric_algo_ord_no", 1)],
                ),
                (
                    r#"{"Data":{"OrderNo":"1","LotSize":"75"}}"#,
                    &[("numeric_string_coerced", 1)],
                ),
                (
                    r#"{"Data":{"OrderNo":"1","LotSize":75.0}}"#,
                    &[("integral_float_coerced", 1)],
                ),
                (
                    r#"{"Data":{"OrderNo":"1","LotSize":""}}"#,
                    &[("empty_number_dropped", 1)],
                ),
                // The fail-loud pre-move counts nothing (and the parse fails).
                (r#"{"Data":{"OrderNo":"1","tradedQty":null}}"#, &[]),
                (r#"{"Data":{"OrderNo":"1","tradedQty":""}}"#, &[]),
                (r#"{"Data":{"OrderNo":"1","TradedQty":null}}"#, &[]),
                (
                    r#"{"Data":{"OrderNo":"1","TradedQty":null,"tradedQty":50}}"#,
                    &[("casing_fallback", 1)],
                ),
                (
                    r#"{"Data":{"OrderNo":"1","tradedQty":"50"}}"#,
                    &[("casing_fallback", 1), ("numeric_string_coerced", 1)],
                ),
                // No Data object: no field pass at all.
                (r#"{"Data":null}"#, &[]),
                (r#"{"Data":{}}"#, &[]),
            ];
            for (json, expected) in rows {
                let expected: BTreeMap<String, u64> = expected
                    .iter()
                    .map(|(k, n)| ((*k).to_string(), *n))
                    .collect();
                assert_eq!(quirks_of(json), expected, "{json}");
            }
        }

        // ---------------------------------------------------------------
        // Gap 6: idempotence over coerced and drifted frames.
        // ---------------------------------------------------------------

        const TWO_POW_53: i64 = 1 << 53;

        fn generated_value(kind: FieldKind, form: u8, n: i64, x: f64, s: &str) -> Value {
            // Below 2^49, so serde_json's lossy float parse reads `<n>.0`
            // back exactly (see the float BUG test for larger values).
            let small = n % (1 << 49);
            match (kind, form) {
                (FieldKind::Text, 0 | 3) => json!(s),
                (FieldKind::Text, 1) => json!(n),
                (FieldKind::Text, _) => json!(n.unsigned_abs()),
                (FieldKind::Int, 0) => json!(n),
                (FieldKind::Int, 1) => json!(n.to_string()),
                (FieldKind::Int, 2) => json!(small as f64),
                (FieldKind::Int, _) => json!(format!("{small}.0")),
                (FieldKind::Float, 0) => json!(x),
                (FieldKind::Float, 1) => json!(format!("{x}")),
                (FieldKind::Float, 2) => json!(n % 1_000_000),
                (FieldKind::Float, _) => json!(format!("{}", n % 1_000_000)),
            }
        }

        /// Field-by-field equality, allowing a float to differ by one ULP.
        fn same_up_to_one_ulp(a: &OrderUpdate, b: &OrderUpdate) -> Result<(), String> {
            let (a, b) = (
                serde_json::to_value(a).unwrap(),
                serde_json::to_value(b).unwrap(),
            );
            for (key, left) in a.as_object().unwrap() {
                let right = &b[key];
                let same = match (left.as_f64(), right.as_f64(), left.is_f64()) {
                    (Some(x), Some(y), true) => x.to_bits().abs_diff(y.to_bits()) <= 1,
                    _ => left == right,
                };
                if !same {
                    return Err(format!("{key}: {left} != {right}"));
                }
            }
            Ok(())
        }

        #[test]
        #[ignore = "needs serde_json float_roundtrip feature: owner approval"]
        fn test_parse_order_update_float_number_and_string_forms_agree() {
            let mut misses = Vec::new();
            for text in ["204443773.92190543", "204443773.92190546"] {
                let number = parse_order_update(&format!(r#"{{"Data":{{"refLtp":{text}}}}}"#))
                    .unwrap()
                    .ref_ltp;
                let string = parse_order_update(&format!(r#"{{"Data":{{"refLtp":"{text}"}}}}"#))
                    .unwrap()
                    .ref_ltp;
                assert_eq!(string.to_bits(), text.parse::<f64>().unwrap().to_bits());
                if number.to_bits() != string.to_bits() {
                    misses.push(format!("{text}: number {number} vs string {string}"));
                }
            }
            // The same lossy read makes an exactly representable integral
            // quantity fractional, so it fails the typed parse.
            let lot =
                parse_order_update(r#"{"Data":{"OrderNo":"1","LotSize":4085276730493842.0}}"#);
            if lot.as_ref().map(|u| u.lot_size).ok() != Some(4_085_276_730_493_842) {
                misses.push(format!("LotSize 4085276730493842.0: {lot:?}"));
            }
            assert!(misses.is_empty(), "{misses:?}");
        }

        #[test]
        fn test_parse_order_update_array_data_is_a_structural_error() {
            for json in [
                r#"{"Data":[]}"#,
                r#"{"data":[]}"#,
                r#"{"Data":[],"data":{"OrderNo":"2"}}"#,
                r#"{"Data":["NSE","D","52432","c","1124"]}"#,
                r#"[{"Data":{"OrderNo":"1"}}]"#,
            ] {
                let got = parse_order_update(json);
                assert!(got.is_err(), "{json}: {got:?}");
            }
        }

        proptest! {
            #[test]
            fn proptest_parse_order_update_coerced_frames_are_idempotent(
                choices in proptest::collection::vec(
                    (0u8..3, 0u8..4, any::<i64>(), -1e9f64..1e9, "[ -~]{0,8}"),
                    ORDER_UPDATE_FIELDS.len(),
                ),
            ) {
                let mut data = serde_json::Map::new();
                for (&(accepted, alt, kind), (casing, form, n, x, s)) in
                    ORDER_UPDATE_FIELDS.iter().zip(&choices)
                {
                    let value = generated_value(kind, *form, *n, *x, s);
                    if let FieldFix::Replace(fixed, _) = fix_for_field(accepted, kind, &value) {
                        prop_assert_eq!(
                            fix_for_field(accepted, kind, &fixed),
                            FieldFix::Keep,
                            "{} {:?} -> {:?}", accepted, value, fixed
                        );
                    }
                    if *casing != 1 {
                        data.insert(accepted.to_string(), value.clone());
                    }
                    if *casing != 0 {
                        data.insert(alt.to_string(), value);
                    }
                }
                let raw = json!({ "Data": Value::Object(data), "Type": "order_alert" });
                let first = parse_order_update(&raw.to_string());
                prop_assert!(first.is_ok(), "{:?}", first);
                let first = first.unwrap();
                // A replay of the same raw frame decodes identically.
                prop_assert_eq!(&parse_order_update(&raw.to_string()).unwrap(), &first);
                // Re-serializing the decode and decoding again is a no-op,
                // up to the one-ULP float re-parse limit pinned by
                // `test_parse_order_update_float_number_and_string_forms_agree`.
                let again = json!({ "Data": serde_json::to_value(&first).unwrap(), "Type": "order_alert" });
                let second = parse_order_update(&again.to_string()).unwrap();
                if let Err(diff) = same_up_to_one_ulp(&second, &first) {
                    prop_assert!(false, "{}", diff);
                }
                // Normalizing an already-normalized frame changes nothing.
                let mut once = raw.clone();
                normalize_order_update_frame(&mut once);
                let mut twice = once.clone();
                normalize_order_update_frame(&mut twice);
                prop_assert_eq!(twice, once);
            }
        }

        // ---------------------------------------------------------------
        // Gap 7: precision and identity-id edges.
        // ---------------------------------------------------------------

        #[test]
        fn test_fix_for_field_precision_and_id_edges() {
            let coerced = |v: Value| FieldFix::Replace(v, "numeric_string_coerced");
            let rows: Vec<(&str, FieldKind, Value, FieldFix)> = vec![
                // An integral float in an identity id is refused (loud), not
                // rendered: pinned, the safe direction.
                (
                    "OrderNo",
                    FieldKind::Text,
                    json!(1_124_091_136_546.0),
                    FieldFix::Keep,
                ),
                (
                    "SecurityId",
                    FieldKind::Text,
                    json!(52_432.0),
                    FieldFix::Keep,
                ),
                // Negative and zero integer ids are rendered as sent.
                (
                    "OrderNo",
                    FieldKind::Text,
                    json!(-5),
                    FieldFix::Replace(json!("-5"), "numeric_text_coerced"),
                ),
                (
                    "SecurityId",
                    FieldKind::Text,
                    json!(0),
                    FieldFix::Replace(json!("0"), "numeric_text_coerced"),
                ),
                (
                    "ExchOrderNo",
                    FieldKind::Text,
                    json!(u64::MAX),
                    FieldFix::Replace(json!(u64::MAX.to_string()), "numeric_text_coerced"),
                ),
                (
                    "Quantity",
                    FieldKind::Int,
                    json!("9223372036854775807"),
                    coerced(json!(i64::MAX)),
                ),
                (
                    "Quantity",
                    FieldKind::Int,
                    json!("9223372036854775808"),
                    FieldFix::Keep,
                ),
                (
                    "Quantity",
                    FieldKind::Int,
                    json!("-9223372036854775808"),
                    coerced(json!(i64::MIN)),
                ),
                (
                    "Quantity",
                    FieldKind::Int,
                    json!("9223372036854775807.0"),
                    coerced(json!(i64::MAX)),
                ),
                (
                    "Quantity",
                    FieldKind::Int,
                    json!("9223372036854775808.0"),
                    FieldFix::Keep,
                ),
                (
                    "Quantity",
                    FieldKind::Int,
                    json!("-9223372036854775808.0"),
                    coerced(json!(i64::MIN)),
                ),
                (
                    "Quantity",
                    FieldKind::Int,
                    json!("9007199254740993.0"),
                    coerced(json!(9_007_199_254_740_993i64)),
                ),
                (
                    "Quantity",
                    FieldKind::Int,
                    json!("9007199254740992.0"),
                    coerced(json!(TWO_POW_53)),
                ),
                ("Quantity", FieldKind::Int, json!(i64::MAX), FieldFix::Keep),
                ("Quantity", FieldKind::Int, json!(i64::MIN), FieldFix::Keep),
                (
                    "Quantity",
                    FieldKind::Int,
                    json!(9_223_372_036_854_775_808u64),
                    FieldFix::Keep,
                ),
                ("Quantity", FieldKind::Int, json!(u64::MAX), FieldFix::Keep),
            ];
            for (key, kind, value, expected) in rows {
                assert_eq!(fix_for_field(key, kind, &value), expected, "{key} {value}");
            }
            // Through the parser: the refused integral-float id fails loud.
            assert!(parse_order_update(r#"{"Data":{"OrderNo":1124091136546.0}}"#).is_err());
            assert_eq!(
                parse_order_update(r#"{"Data":{"OrderNo":-5}}"#)
                    .unwrap()
                    .order_no,
                "-5"
            );
        }

        #[test]
        fn test_fix_for_field_int_never_rounds_an_inexact_integral_value() {
            let rounded = [
                ("9007199254740993.0", 9_007_199_254_740_992i64),
                ("-9223372036854775809", i64::MIN),
                ("18014398509481985.0", 18_014_398_509_481_984),
            ];
            for (text, wrong) in rounded {
                assert_ne!(
                    fix_for_field("Quantity", FieldKind::Int, &json!(text)),
                    FieldFix::Replace(json!(wrong), "numeric_string_coerced"),
                    "{text} rounded to {wrong}"
                );
            }
            let decoded =
                parse_order_update(r#"{"Data":{"OrderNo":"1","LotSize":9007199254740993.0}}"#);
            assert!(
                decoded
                    .as_ref()
                    .map_or(true, |u| u.lot_size != 9_007_199_254_740_992),
                "a JSON number 9007199254740993.0 decoded as 9007199254740992"
            );
        }

        proptest! {
            #[test]
            fn proptest_fix_for_field_int_float_string_at_or_below_2_pow_53_is_exact(
                n in -TWO_POW_53..=TWO_POW_53,
            ) {
                prop_assert_eq!(
                    fix_for_field("Quantity", FieldKind::Int, &json!(format!("{n}.0"))),
                    FieldFix::Replace(json!(n), "numeric_string_coerced")
                );
            }

            #[test]
            fn proptest_fix_for_field_int_float_string_above_2_pow_53_is_exact_or_refused(
                n in (1u64 << 53) + 1..=u64::MAX,
            ) {
                let fix = fix_for_field("Quantity", FieldKind::Int, &json!(format!("{n}.0")));
                let exact = i64::try_from(n)
                    .map(|m| FieldFix::Replace(json!(m), "numeric_string_coerced"));
                prop_assert!(
                    fix == FieldFix::Keep || Ok(fix.clone()) == exact,
                    "{}.0 -> {:?}", n, fix
                );
            }
        }

        // ---------------------------------------------------------------
        // Gap 8: size and depth boundaries.
        // ---------------------------------------------------------------

        /// `prefix + pad + suffix`, `target` bytes long, padded with `fill`
        /// characters then ASCII `x` to land on the exact byte count.
        fn padded(prefix: &str, suffix: &str, target: usize, fill: char) -> String {
            let room = target
                .saturating_sub(prefix.len())
                .saturating_sub(suffix.len());
            let wide = room.checked_div(fill.len_utf8()).unwrap();
            let rest = room.saturating_sub(wide.saturating_mul(fill.len_utf8()));
            let frame = format!(
                "{prefix}{}{}{suffix}",
                fill.to_string().repeat(wide),
                "x".repeat(rest)
            );
            assert_eq!(frame.len(), target);
            frame
        }

        #[test]
        fn test_parse_order_update_and_unreadable_order_frame_type_size_boundary() {
            let cap = ORDER_UPDATE_MAX_FRAME_BYTES;
            let hollow = parse_order_update(r#"{"Data":{}}"#).unwrap();
            for fill in ['x', '₹'] {
                for target in [cap - 1, cap] {
                    let readable = padded(
                        r#"{"Data":{"OrderNo":"1","Remarks":""#,
                        r#""},"Type":"order_alert"}"#,
                        target,
                        fill,
                    );
                    let update = parse_order_update(&readable).unwrap();
                    assert_eq!(update.order_no, "1");
                    assert_eq!(unreadable_order_frame_type(&update, &readable), None);
                    let foreign = padded(
                        r#"{"Data":{"Remarks":""#,
                        r#""},"Type":"login_ack"}"#,
                        target,
                        fill,
                    );
                    assert_eq!(
                        verdict(&foreign).as_deref(),
                        Some("login_ack"),
                        "{target} {fill}"
                    );
                }
                let over = padded(
                    r#"{"Data":{"Remarks":""#,
                    r#""},"Type":"login_ack"}"#,
                    cap + 1,
                    fill,
                );
                assert!(matches!(
                    parse_order_update(&over),
                    Err(OrderUpdateParseError::FrameTooLarge { len }) if len == cap + 1
                ));
                assert_eq!(
                    unreadable_order_frame_type(&hollow, &over).as_deref(),
                    Some("")
                );
            }
            // Bytes, not characters: under the cap in characters, over it in bytes.
            let wide = format!(r#"{{"Data":{{"Remarks":"{}"}}}}"#, "₹".repeat(cap / 3 + 10));
            assert!(wide.chars().count() < cap && wide.len() > cap);
            assert!(matches!(
                parse_order_update(&wide),
                Err(OrderUpdateParseError::FrameTooLarge { len }) if len == wide.len()
            ));
            assert_eq!(
                unreadable_order_frame_type(&hollow, &wide).as_deref(),
                Some("")
            );
        }

        #[test]
        fn test_parse_order_update_deep_nesting_is_an_error_not_a_crash() {
            let hollow = parse_order_update(r#"{"Data":{}}"#).unwrap();
            for depth in [200usize, 5_000] {
                for (open, close) in [("[", "]"), (r#"{"a":"#, "}")] {
                    let json = format!(
                        r#"{{"Data":{{"OrderNo":"1","Remarks":{}0{}}},"Type":"x"}}"#,
                        open.repeat(depth),
                        close.repeat(depth)
                    );
                    assert!(json.len() <= ORDER_UPDATE_MAX_FRAME_BYTES);
                    assert!(
                        matches!(
                            parse_order_update(&json),
                            Err(OrderUpdateParseError::JsonError(_))
                        ),
                        "depth {depth} {open}"
                    );
                    assert_eq!(
                        unreadable_order_frame_type(&hollow, &json).as_deref(),
                        Some("")
                    );
                }
            }
            // Under serde_json's recursion limit, deep junk outside `Data`
            // is ignored and the event still decodes.
            let json = format!(
                r#"{{"Data":{{"OrderNo":"1"}},"Pad":{}0{}}}"#,
                "[".repeat(100),
                "]".repeat(100)
            );
            assert_eq!(parse_order_update(&json).unwrap().order_no, "1");
        }

        #[test]
        fn test_parse_order_update_twenty_thousand_junk_keys_and_both_casings() {
            let mut value = full_sample_value();
            let data = value["Data"].as_object_mut().unwrap();
            for &(accepted, alt, _) in ORDER_UPDATE_FIELDS {
                let copy = data[accepted].clone();
                data.insert(alt.to_string(), copy);
            }
            for i in 0..20_000 {
                data.insert(format!("j{i}"), json!(0));
            }
            let json = value.to_string();
            assert!(
                json.len() <= ORDER_UPDATE_MAX_FRAME_BYTES,
                "{} bytes",
                json.len()
            );
            assert_eq!(
                parse_order_update(&json).unwrap(),
                parse_order_update(FULL_ACCEPTED_SAMPLE).unwrap()
            );
        }

        // ---------------------------------------------------------------
        // Gap 9: wrapper casing.
        // ---------------------------------------------------------------

        #[test]
        fn test_normalize_order_update_frame_wrapper_casing_table() {
            let rows: &[(&str, Option<&str>)] = &[
                // Pinned decision: a present null `Data` blocks the lowercase
                // fallback (a null wrapper is an error, not "absent").
                (r#"{"Data":null,"data":{"OrderNo":"1"}}"#, None),
                (r#"{"Data":{"OrderNo":"1"},"data":null}"#, Some("1")),
                (r#"{"Data":"x","data":{"OrderNo":"2"}}"#, None),
                (
                    r#"{"Type":null,"type":"order_alert","Data":{"OrderNo":"1"}}"#,
                    Some("1"),
                ),
                (
                    r#"{"Data":{"OrderNo":"1"},"data":{"OrderNo":"2"},"type":5}"#,
                    Some("1"),
                ),
                (r#"{"data":{"OrderNo":"1"},"Type":"x"}"#, Some("1")),
                (r#"{"DATA":{"OrderNo":"1"}}"#, None),
                (r#"{"data":"x"}"#, None),
                (r#"{}"#, None),
                (r#""Data""#, None),
                ("5", None),
                ("null", None),
                ("true", None),
            ];
            for (json, expected) in rows {
                let got = parse_order_update(json);
                match expected {
                    Some(order_no) => assert_eq!(got.unwrap().order_no, *order_no, "{json}"),
                    None => assert!(
                        matches!(got, Err(OrderUpdateParseError::JsonError(_))),
                        "{json}: {got:?}"
                    ),
                }
            }
            // A junk `Type` is removed and the lowercase tag is left behind.
            let mut value: Value =
                serde_json::from_str(r#"{"Type":null,"type":"order_alert","Data":{}}"#).unwrap();
            normalize_order_update_frame(&mut value);
            assert!(value.get("Type").is_none());
            assert_eq!(value["type"], json!("order_alert"));
            // Non-object frames are left untouched.
            for raw in ["[1]", "5", "null", r#""x""#] {
                let mut value: Value = serde_json::from_str(raw).unwrap();
                let before = value.clone();
                normalize_order_update_frame(&mut value);
                assert_eq!(value, before, "{raw}");
            }
        }

        // ---------------------------------------------------------------
        // Gap 10: literal duplicate keys x casing.
        // ---------------------------------------------------------------

        #[test]
        fn test_normalize_order_update_frame_literal_duplicates_with_casing() {
            // `Ok(traded_qty)` or `Err`; every frame carries OrderNo "1"
            // unless the row is about OrderNo.
            let order_no_rows: &[(&str, &str)] = &[
                (
                    r#"{"Data":{"OrderNo":"1","orderNo":"2","OrderNo":"3"}}"#,
                    "3",
                ),
                (r#"{"Data":{"orderNo":"2","orderNo":"4"}}"#, "4"),
                (
                    r#"{"Data":{"OrderNo":null,"OrderNo":"5","orderNo":"6"}}"#,
                    "5",
                ),
            ];
            for (json, expected) in order_no_rows {
                assert_eq!(
                    parse_order_update(json).unwrap().order_no,
                    *expected,
                    "{json}"
                );
            }
            let qty_rows: &[(&str, Option<i64>)] = &[
                (
                    r#"{"Data":{"OrderNo":"1","TradedQty":50,"TradedQty":null}}"#,
                    None,
                ),
                (
                    r#"{"Data":{"OrderNo":"1","tradedQty":50,"tradedQty":""}}"#,
                    None,
                ),
                (
                    r#"{"Data":{"OrderNo":"1","tradedQty":50,"tradedQty":"  "}}"#,
                    None,
                ),
                (
                    r#"{"Data":{"OrderNo":"1","TradedQty":50,"TradedQty":"","tradedQty":7}}"#,
                    None,
                ),
                (
                    r#"{"Data":{"OrderNo":"1","TradedQty":null,"TradedQty":50,"tradedQty":null}}"#,
                    Some(50),
                ),
                (
                    r#"{"Data":{"OrderNo":"1","TradedQty":"","TradedQty":50}}"#,
                    Some(50),
                ),
                (
                    r#"{"Data":{"OrderNo":"1","tradedQty":"","tradedQty":50}}"#,
                    Some(50),
                ),
                (
                    r#"{"Data":{"OrderNo":"1","TradedQty":50,"tradedQty":null,"tradedQty":7}}"#,
                    Some(50),
                ),
                (
                    r#"{"Data":{"OrderNo":"1","TradedQty":null,"tradedQty":null,"tradedQty":9}}"#,
                    Some(9),
                ),
            ];
            for (json, expected) in qty_rows {
                let got = parse_order_update(json);
                match expected {
                    Some(qty) => assert_eq!(got.unwrap().traded_qty, *qty, "{json}"),
                    None => assert!(got.is_err(), "{json} must fail loud: {got:?}"),
                }
            }
        }

        // ---------------------------------------------------------------
        // Gap 11: the Instrument assumed twin.
        // ---------------------------------------------------------------

        #[test]
        fn test_normalize_order_update_frame_instrument_assumed_twin_table() {
            let rows: &[(&str, Option<&str>)] = &[
                (
                    r#""instrument":"OPTIDX","instrumentType":"EQ""#,
                    Some("OPTIDX"),
                ),
                (
                    r#""Instrument":"FUTIDX","instrument":"OPTIDX","instrumentType":"EQ""#,
                    Some("FUTIDX"),
                ),
                (r#""Instrument":null,"instrumentType":"EQ""#, Some("EQ")),
                (r#""instrument":null,"instrumentType":"EQ""#, Some("EQ")),
                (r#""Instrument":"","instrumentType":"EQ""#, Some("")),
                (r#""instrumentType":"EQ""#, Some("EQ")),
                (r#""instrumentType":5"#, Some("5")),
                (r#""instrumentType":7.5"#, Some("7.5")),
                (r#""instrumentType":null"#, Some("")),
                (r#""instrumentType":{}"#, None),
                (r#""instrumentType":[]"#, None),
                (r#""instrumentType":true"#, None),
                (r#""Instrument":"EQ","instrumentType":{}"#, Some("EQ")),
            ];
            for (fields, expected) in rows {
                let json = format!(r#"{{"Data":{{"OrderNo":"1",{fields}}}}}"#);
                let got = parse_order_update(&json);
                match expected {
                    Some(instrument) => {
                        assert_eq!(got.unwrap().instrument, *instrument, "{json}")
                    }
                    None => assert!(got.is_err(), "{json}: {got:?}"),
                }
            }
        }

        // ---------------------------------------------------------------
        // Gap 12: numeric string forms.
        // ---------------------------------------------------------------

        #[test]
        fn test_fix_for_field_numeric_string_forms() {
            let int = |text: &str| fix_for_field("Quantity", FieldKind::Int, &json!(text));
            let int_ok = |n: i64| FieldFix::Replace(json!(n), "numeric_string_coerced");
            let int_rows: &[(&str, Option<i64>)] = &[
                ("+50", Some(50)),
                // Pinned choice: an exponent spelling is an exact integer.
                ("1e3", Some(1_000)),
                ("1E3", Some(1_000)),
                ("1e18", Some(1_000_000_000_000_000_000)),
                ("5.", Some(5)),
                ("-0", Some(0)),
                ("\u{a0}50\u{a0}", Some(50)),
                ("\n50\t", Some(50)),
                ("5e-1", None),
                (".5", None),
                ("0x10", None),
                ("50 50", None),
                ("５０", None),
                ("1_000", None),
                ("1,000", None),
                ("inf", None),
                ("NaN", None),
                ("1e19", None),
                ("--5", None),
            ];
            for (text, expected) in int_rows {
                let want = expected.map_or(FieldFix::Keep, int_ok);
                assert_eq!(int(text), want, "Int {text:?}");
            }
            let float = |text: &str| fix_for_field("Price", FieldKind::Float, &json!(text));
            let float_rows: &[(&str, Option<f64>)] = &[
                (".5", Some(0.5)),
                ("5.", Some(5.0)),
                ("+5", Some(5.0)),
                ("1e3", Some(1_000.0)),
                ("-0", Some(-0.0)),
                ("\u{a0}245.5", Some(245.5)),
                ("1e400", None),
                ("-1e400", None),
                ("245,50", None),
                ("+inf", None),
                ("infinity", None),
                ("-Infinity", None),
                ("nan", None),
                ("0x10", None),
                ("1_000", None),
            ];
            for (text, expected) in float_rows {
                match (float(text), expected) {
                    (FieldFix::Replace(Value::Number(n), "numeric_string_coerced"), Some(x)) => {
                        assert_eq!(n.as_f64().unwrap().to_bits(), x.to_bits(), "Float {text:?}");
                    }
                    (FieldFix::Keep, None) => {}
                    (got, _) => panic!("Float {text:?}: {got:?}, expected {expected:?}"),
                }
            }
        }

        proptest! {
            #[test]
            fn proptest_fix_for_field_int_parses_every_i64_string(n in any::<i64>()) {
                prop_assert_eq!(
                    fix_for_field("Quantity", FieldKind::Int, &json!(n.to_string())),
                    FieldFix::Replace(json!(n), "numeric_string_coerced")
                );
            }

            #[test]
            fn proptest_fix_for_field_float_parses_every_finite_f64_display(
                x in proptest::num::f64::ANY.prop_filter("finite", |x| x.is_finite()),
            ) {
                match fix_for_field("Price", FieldKind::Float, &json!(format!("{x}"))) {
                    FieldFix::Replace(Value::Number(n), "numeric_string_coerced") => {
                        prop_assert_eq!(n.as_f64().unwrap().to_bits(), x.to_bits());
                    }
                    other => prop_assert!(false, "{} -> {:?}", x, other),
                }
            }

            #[test]
            fn proptest_fix_for_field_non_numeric_strings_are_kept(s in "[^0-9eE.+-]+") {
                prop_assert_eq!(fix_for_field("Quantity", FieldKind::Int, &json!(s)), FieldFix::Keep);
                prop_assert_eq!(fix_for_field("Price", FieldKind::Float, &json!(s)), FieldFix::Keep);
            }
        }
    }
}
