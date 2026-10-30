//! Adversarial properties of the order-update WebSocket JSON decoder.
//!
//! Surface: `tickvault_core::parser::order_update::{parse_order_update,
//! is_hollow_order_update, unreadable_order_frame_type}`, the only place the
//! order-update socket's text enters the process.
//!
//! What the in-file tests in `order_update.rs` already cover is NOT repeated
//! here (they mutate one fixed sample: field subsets, casings, the coercion
//! grid, the size boundary, deep nesting). This file generates WHOLE random
//! orders and attacks the frame around them:
//!
//! 1. Every byte prefix of a valid frame is refused with the typed
//!    `JsonError` (an incomplete object is never a partial order), the full
//!    frame decodes to exactly the generated order, trailing whitespace
//!    changes nothing and trailing junk is refused.
//! 2. Any permutation of the JSON key order (inside `Data` and of the
//!    wrapper) decodes to the identical `OrderUpdate`.
//! 3. A valid frame wrapped in unknown keys (top level and inside `Data`,
//!    with arbitrary JSON values) decodes to the identical `OrderUpdate`.
//! 4. Numeric fields given extreme literals or strings (1e308, 1e309, -0,
//!    integers past i64/u64, `NaN`, `inf`, padded spellings) never panic,
//!    only ever fail with `JsonError`, and when they decode they decode to a
//!    value checked against a reference written here: an Int field holds
//!    exactly the integer spelled (never a neighbouring one), a Float field
//!    holds the finite number spelled.
//! 5. A fill-critical field (`Quantity`, `TradedQty`, `RemainingQuantity`,
//!    `Price`, `TradedPrice`, `AvgTradedPrice`) that is null, empty or
//!    whitespace, in either casing, is refused with `JsonError`; `OrderNo`
//!    and `Status` refuse null. The other fields read null as absent.
//! 6. `is_hollow_order_update` agrees with a reference (all three identity
//!    fields are only Unicode `White_Space`), and `unreadable_order_frame_type`
//!    agrees with a reference written from its doc comment.
//! 7. Arbitrary JSON-shaped frames that decode re-serialize to a frame that
//!    decodes to the same order (floats within one ULP: serde_json here is
//!    built without `float_roundtrip`, which the module documents).
//!
//! Honest limits: the field table (accepted key, other casing, type) is
//! private to the module, so it is restated below; a drift would show up as
//! a property failure, not silently. Generated floats are `paise / 100`
//! values, which serde_json reads back exactly; floats with 17 significant
//! digits are compared within one ULP (property 7). The decode-quirk
//! counters are not observed here (no metrics recorder is installed).

use proptest::prelude::*;
use proptest::test_runner::Config as ProptestConfig;
use serde_json::{Map, Value, json};
use tickvault_common::order_types::OrderUpdate;
use tickvault_core::parser::order_update::{
    ORDER_UPDATE_MAX_FRAME_BYTES, OrderUpdateParseError, is_hollow_order_update,
    parse_order_update, unreadable_order_frame_type,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Text,
    Int,
    Float,
}

/// `(accepted key, other casing, type)`, restated from the private table in
/// `order_update.rs`. Every `OrderUpdate` field appears once.
const FIELDS: &[(&str, &str, Kind)] = &[
    ("Exchange", "exchange", Kind::Text),
    ("Segment", "segment", Kind::Text),
    ("SecurityId", "securityId", Kind::Text),
    ("ClientId", "clientId", Kind::Text),
    ("OrderNo", "orderNo", Kind::Text),
    ("ExchOrderNo", "exchOrderNo", Kind::Text),
    ("Product", "product", Kind::Text),
    ("TxnType", "txnType", Kind::Text),
    ("OrderType", "orderType", Kind::Text),
    ("Validity", "validity", Kind::Text),
    ("Quantity", "quantity", Kind::Int),
    ("TradedQty", "tradedQty", Kind::Int),
    ("RemainingQuantity", "remainingQuantity", Kind::Int),
    ("Price", "price", Kind::Float),
    ("TriggerPrice", "triggerPrice", Kind::Float),
    ("TradedPrice", "tradedPrice", Kind::Float),
    ("AvgTradedPrice", "avgTradedPrice", Kind::Float),
    ("Status", "status", Kind::Text),
    ("Symbol", "symbol", Kind::Text),
    ("DisplayName", "displayName", Kind::Text),
    ("CorrelationId", "correlationId", Kind::Text),
    ("Remarks", "remarks", Kind::Text),
    ("ReasonDescription", "reasonDescription", Kind::Text),
    ("OrderDateTime", "orderDateTime", Kind::Text),
    ("ExchOrderTime", "exchOrderTime", Kind::Text),
    ("LastUpdatedTime", "lastUpdatedTime", Kind::Text),
    ("Instrument", "instrument", Kind::Text),
    ("LotSize", "lotSize", Kind::Int),
    ("StrikePrice", "strikePrice", Kind::Float),
    ("ExpiryDate", "expiryDate", Kind::Text),
    ("OptType", "optType", Kind::Text),
    ("Isin", "isin", Kind::Text),
    ("DiscQuantity", "discQuantity", Kind::Int),
    ("DiscQtyRem", "discQtyRem", Kind::Int),
    ("LegNo", "legNo", Kind::Int),
    ("ProductName", "productName", Kind::Text),
    ("refLtp", "RefLtp", Kind::Float),
    ("tickSize", "TickSize", Kind::Float),
    ("Source", "source", Kind::Text),
    ("OffMktFlag", "offMktFlag", Kind::Text),
    ("AlgoOrdNo", "algoOrdNo", Kind::Text),
    ("MktType", "mktType", Kind::Text),
    ("Series", "series", Kind::Text),
    ("GoodTillDaysDate", "goodTillDaysDate", Kind::Text),
    ("AlgoId", "algoId", Kind::Text),
    ("Multiplier", "multiplier", Kind::Int),
];

/// The fields whose null or blank value must fail the parse (read from the
/// module's `FILL_CRITICAL_FIELDS`, which is `#[cfg(test)]` there).
const FILL_CRITICAL: &[&str] = &[
    "Quantity",
    "TradedQty",
    "RemainingQuantity",
    "Price",
    "TradedPrice",
    "AvgTradedPrice",
];

/// Identity fields whose NULL also fails the parse (`fails_loud_when_blank`).
const NULL_FAILS_LOUD_TEXT: &[&str] = &["OrderNo", "Status"];

const MAX_ORDER_ALERT: &str = "order_alert";
const TAG_LOG_MAX_CHARS: usize = 64;

fn field_index(accepted: &str) -> usize {
    FIELDS
        .iter()
        .position(|(a, _, _)| *a == accepted)
        .unwrap_or_else(|| panic!("unknown field {accepted}"))
}

/// The struct with every field defaulted, built through serde (every field
/// is `#[serde(default)]`), never through the parser under test.
fn default_update() -> OrderUpdate {
    serde_json::from_value(json!({})).expect("all-default OrderUpdate")
}

/// The struct a `Data` object means, built directly by serde (no casing,
/// no coercion). The reference every decode is compared with.
fn reference_update(data: &Map<String, Value>) -> OrderUpdate {
    serde_json::from_value(Value::Object(data.clone())).expect("reference decode")
}

fn is_json_error(result: &Result<OrderUpdate, OrderUpdateParseError>) -> bool {
    matches!(result, Err(OrderUpdateParseError::JsonError(_)))
}

// ---------------------------------------------------------------------------
// Generators
// ---------------------------------------------------------------------------

/// A text value: printable ASCII, some Unicode, quotes and backslashes.
fn text_value() -> impl Strategy<Value = String> {
    prop_oneof![
        "[ -~]{0,16}",
        "[A-Z0-9]{1,12}",
        "[\"\\\\/₹é\u{3000}a-z]{0,8}",
    ]
}

/// A float that serde_json reads back exactly: `paise / 100`.
fn float_value() -> impl Strategy<Value = f64> {
    (-100_000_000_000i64..100_000_000_000).prop_map(|paise| paise as f64 / 100.0)
}

fn value_for(kind: Kind) -> BoxedStrategy<Value> {
    match kind {
        Kind::Text => text_value().prop_map(Value::String).boxed(),
        Kind::Int => (-1_000_000_000_000i64..1_000_000_000_000)
            .prop_map(Value::from)
            .boxed(),
        Kind::Float => float_value().prop_map(|f| json!(f)).boxed(),
    }
}

/// A full `Data` object: every field present in its accepted casing.
fn data_object() -> impl Strategy<Value = Map<String, Value>> {
    let per_field: Vec<BoxedStrategy<Value>> =
        FIELDS.iter().map(|&(_, _, kind)| value_for(kind)).collect();
    per_field.prop_map(|values| {
        FIELDS
            .iter()
            .zip(values)
            .map(|(&(accepted, _, _), value)| (accepted.to_string(), value))
            .collect()
    })
}

/// Serializes `(key, value)` pairs IN THE GIVEN ORDER (serde_json's map is
/// sorted, so order permutations must be written by hand).
fn object_text(pairs: &[(String, Value)]) -> String {
    let mut out = String::from("{");
    for (i, (key, value)) in pairs.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&serde_json::to_string(key).expect("key"));
        out.push(':');
        out.push_str(&value.to_string());
    }
    out.push('}');
    out
}

fn frame_text(data: &Map<String, Value>) -> String {
    let pairs: Vec<(String, Value)> = data.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
    format!(
        r#"{{"Data":{},"Type":"{MAX_ORDER_ALERT}"}}"#,
        object_text(&pairs)
    )
}

/// Arbitrary JSON values for unknown keys, nested a little.
fn junk_value() -> impl Strategy<Value = Value> {
    let leaf = prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::Bool),
        any::<i64>().prop_map(Value::from),
        float_value().prop_map(|f| json!(f)),
        "[ -~]{0,10}".prop_map(Value::String),
    ];
    leaf.prop_recursive(3, 16, 4, |inner| {
        prop_oneof![
            prop::collection::vec(inner.clone(), 0..4).prop_map(Value::Array),
            prop::collection::btree_map("x[a-z]{0,6}", inner, 0..4)
                .prop_map(|m| Value::Object(m.into_iter().collect())),
        ]
    })
}

/// Unknown key names: every real field and wrapper key starts with an
/// upper-case letter, `refLtp`/`tickSize`, a lower-case casing twin, or is
/// `instrumentType`/`data`/`type`; none starts with `x`.
fn unknown_key() -> impl Strategy<Value = String> {
    "x[A-Za-z0-9_]{0,10}"
}

// ---------------------------------------------------------------------------
// References for numeric fields
// ---------------------------------------------------------------------------

/// The integer a decimal spelling denotes EXACTLY (sign, digits, optional
/// fraction, optional exponent), or `None` when it is not an integer or is
/// outside `i64`. Written independently of the module: it normalises the
/// digit string and shifts it, with no early returns shared with the code
/// under test.
fn ref_exact_int(text: &str) -> Option<i64> {
    let (negative, body) = match text.as_bytes().first() {
        Some(b'-') => (true, &text[1..]),
        Some(b'+') => (false, &text[1..]),
        _ => (false, text),
    };
    let (mantissa, exponent) = match body.find(['e', 'E']) {
        Some(at) => {
            let exp_text = &body[at + 1..];
            let digits = exp_text.trim_start_matches(['+', '-']);
            if digits.is_empty()
                || !digits.bytes().all(|b| b.is_ascii_digit())
                || exp_text.len() - digits.len() > 1
            {
                return None;
            }
            let magnitude: i64 = digits.parse().unwrap_or(i64::MAX / 4).min(1_000_000);
            let signed = if exp_text.starts_with('-') {
                -magnitude
            } else {
                magnitude
            };
            (&body[..at], signed)
        }
        None => (body, 0),
    };
    let (int_part, frac_part) = match mantissa.split_once('.') {
        Some((i, f)) => (i, f),
        None => (mantissa, ""),
    };
    if !int_part.bytes().all(|b| b.is_ascii_digit())
        || !frac_part.bytes().all(|b| b.is_ascii_digit())
        || (int_part.is_empty() && frac_part.is_empty())
    {
        return None;
    }
    let mut digits: String = format!("{int_part}{frac_part}");
    let mut shift = exponent - frac_part.len() as i64;
    let stripped = digits.trim_start_matches('0').to_string();
    digits = stripped;
    if digits.is_empty() {
        return Some(0);
    }
    while digits.ends_with('0') {
        digits.pop();
        shift += 1;
    }
    if shift < 0 {
        return None;
    }
    if digits.len() as i64 + shift > 19 {
        return None;
    }
    let mut magnitude: i128 = digits.parse().ok()?;
    for _ in 0..shift {
        magnitude *= 10;
    }
    let signed = if negative { -magnitude } else { magnitude };
    i64::try_from(signed).ok()
}

/// `true` when `literal` is an integer JSON literal (optional `-`, digits).
fn is_integer_literal(literal: &str) -> bool {
    let body = literal.strip_prefix('-').unwrap_or(literal);
    !body.is_empty() && body.bytes().all(|b| b.is_ascii_digit())
}

/// 2^52, the coercion bound the module documents for float-typed Int values.
const INT_COERCE_LIMIT: i64 = 1 << 52;

/// What an Int field must decode to, for a numeric spelling inside the
/// controlled domain (at most 15 significant digits, exponent within ±22),
/// where serde_json's float read is exact.
fn expected_int_from_literal(literal: &str) -> Option<i64> {
    if is_integer_literal(literal) {
        return literal.parse::<i64>().ok();
    }
    ref_exact_int(literal).filter(|v| v.unsigned_abs() < INT_COERCE_LIMIT.unsigned_abs())
}

/// A controlled-domain decimal literal: up to 15 significant digits, an
/// optional point, an optional exponent, with the EFFECTIVE power of ten
/// (exponent minus fraction digits) inside `-22..=22`. There the value is
/// one exact integer times or divided by one exact power of ten, so a
/// single rounding gives the correctly rounded `f64` (measured: an
/// effective exponent of -23, `6179.110031e-17`, already reads one ULP off).
fn controlled_literal() -> impl Strategy<Value = String> {
    (
        any::<bool>(),
        "[0-9]{1,15}",
        prop::option::of(0usize..16),
        prop::option::of(-22i32..=22),
    )
        .prop_map(|(negative, digits, point, exponent)| {
            let mut text = String::new();
            if negative {
                text.push('-');
            }
            let mut frac_len = 0i32;
            match point {
                Some(at) if at > 0 && at < digits.len() => {
                    text.push_str(&digits[..at]);
                    text.push('.');
                    text.push_str(&digits[at..]);
                    frac_len = i32::try_from(digits.len() - at).unwrap_or(0);
                }
                _ => text.push_str(&digits),
            }
            let exponent = exponent.map(|exp| (exp - frac_len).clamp(-22, 22) + frac_len);
            // JSON forbids leading zeros on a multi-digit integer part.
            if let Some(rest) = text.strip_prefix('-') {
                text = format!("-{}", strip_json_leading_zeros(rest));
            } else {
                text = strip_json_leading_zeros(&text);
            }
            if let Some(exp) = exponent {
                text.push('e');
                text.push_str(&exp.to_string());
            }
            text
        })
}

fn strip_json_leading_zeros(text: &str) -> String {
    let (int_part, rest) = match text.find('.') {
        Some(at) => (&text[..at], &text[at..]),
        None => (text, ""),
    };
    let trimmed = int_part.trim_start_matches('0');
    let int_part = if trimmed.is_empty() { "0" } else { trimmed };
    format!("{int_part}{rest}")
}

/// Extreme spellings, each tried as a bare literal (when it is valid JSON)
/// and as a string.
const EXTREME_SPELLINGS: &[&str] = &[
    "1e308",
    "-1e308",
    "1e309",
    "-1e309",
    "1e-400",
    "-0",
    "-0.0",
    "0e0",
    "4.9e-324",
    "2.2250738585072014e-308",
    "1.7976931348623157e308",
    "9223372036854775807",
    "9223372036854775808",
    "-9223372036854775808",
    "-9223372036854775809",
    "18446744073709551615",
    "18446744073709551616",
    "123456789012345678901234567890",
    "4503599627370495.5",
    "4503599627370496.0",
    "9007199254740993",
    "9007199254740993.0",
    "1.00000000000000001",
    "0.99999999999999999",
    "1e18",
    "1e19",
    "NaN",
    "-NaN",
    "inf",
    "-inf",
    "Infinity",
    "+5",
    " 7 ",
    "\t8\n",
    "0x10",
    "1_000",
    "١٢",
    "",
    "   ",
    "1e",
    "e5",
    ".",
    "-",
    "5.",
    ".5",
];

fn frame_with_one_numeric(key: &str, raw_value: &str) -> String {
    format!(
        r#"{{"Data":{{"OrderNo":"7","Status":"TRADED","{key}":{raw_value}}},"Type":"order_alert"}}"#
    )
}

fn field_value(update: &OrderUpdate, accepted: &str) -> Value {
    serde_json::to_value(update).expect("serialize")[accepted].clone()
}

/// Outside [1 / EXTREME_MAGNITUDE, EXTREME_MAGNITUDE] serde_json (no
/// `float_roundtrip`) can re-read a shortest-rendered float more than one ULP
/// off (measured 2 ULPs at 9.3e257 and at 4.07e-171); no order price is
/// anywhere near either end.
const EXTREME_MAGNITUDE: f64 = 1e22;
/// Relative tolerance applied outside that band (about 45 ULPs).
const EXTREME_RELATIVE_TOLERANCE: f64 = 1e-14;

/// Two decoded updates are the same order, with float fields allowed one
/// ULP apart (serde_json without `float_roundtrip`).
fn same_order_within_one_ulp(a: &OrderUpdate, b: &OrderUpdate) -> Result<(), String> {
    let (va, vb) = (
        serde_json::to_value(a).expect("a"),
        serde_json::to_value(b).expect("b"),
    );
    for &(accepted, _, kind) in FIELDS {
        let (x, y) = (&va[accepted], &vb[accepted]);
        if kind == Kind::Float {
            let (fx, fy) = (
                x.as_f64().unwrap_or(f64::NAN),
                y.as_f64().unwrap_or(f64::NAN),
            );
            let ulps = ((fx.to_bits() as i64) - (fy.to_bits() as i64)).abs();
            // Measured: one ULP holds for prices; at extreme magnitudes the
            // re-read drifts further (2 ULPs at -9.345468904800974e257 and 4.07e-171).
            let ulp_apart = fx == fy
                || (fx.signum() == fy.signum()
                    && (ulps <= 1
                        || ((fx.abs() >= EXTREME_MAGNITUDE
                            || fx.abs() <= 1.0 / EXTREME_MAGNITUDE)
                            && ((fx - fy) / fx).abs() <= EXTREME_RELATIVE_TOLERANCE)));
            if !ulp_apart {
                return Err(format!(
                    "{accepted}: {fx} vs {fy} ({} ulps)",
                    ((fx.to_bits() as i64) - (fy.to_bits() as i64)).abs()
                ));
            }
        } else if x != y {
            return Err(format!("{accepted}: {x} vs {y}"));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Properties
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig { cases: 96, ..ProptestConfig::default() })]

    /// Property 1: every strict byte prefix of a valid frame is a typed
    /// `JsonError`; the whole frame decodes to the generated order; trailing
    /// whitespace changes nothing; trailing junk is refused.
    #[test]
    fn prop_every_prefix_of_a_valid_frame_is_refused_and_the_whole_decodes(
        data in data_object(),
        junk in "[a-z0-9,}\\]]{1,4}",
    ) {
        let frame = frame_text(&data);
        prop_assert!(frame.len() <= ORDER_UPDATE_MAX_FRAME_BYTES);
        let expected = reference_update(&data);
        for cut in 0..frame.len() {
            if !frame.is_char_boundary(cut) {
                continue;
            }
            let prefix = &frame[..cut];
            let result = parse_order_update(prefix);
            prop_assert!(
                is_json_error(&result),
                "prefix of {} bytes must be a JsonError, got {:?}", cut, result
            );
        }
        let whole = parse_order_update(&frame);
        prop_assert_eq!(whole.as_ref().ok(), Some(&expected));
        let padded = format!("  \n{frame}\t \r\n");
        prop_assert_eq!(parse_order_update(&padded).ok(), Some(expected));
        let with_junk = format!("{frame}{junk}");
        prop_assert!(is_json_error(&parse_order_update(&with_junk)));
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 512, ..ProptestConfig::default() })]

    /// Property 2: the order of keys inside `Data`, the casing chosen for
    /// each field, and the order of the wrapper keys never change the
    /// decoded order.
    #[test]
    fn prop_key_order_permutation_gives_identical_update(
        data in data_object(),
        order in Just((0..FIELDS.len()).collect::<Vec<usize>>()).prop_shuffle(),
        casing in prop::collection::vec(any::<bool>(), FIELDS.len()),
        type_first in any::<bool>(),
    ) {
        let expected = reference_update(&data);
        let pairs: Vec<(String, Value)> = order
            .iter()
            .map(|&i| {
                let (accepted, alt, _) = FIELDS[i];
                let key = if casing[i] { alt } else { accepted };
                (key.to_string(), data[accepted].clone())
            })
            .collect();
        let data_text = object_text(&pairs);
        let frame = if type_first {
            format!(r#"{{"Type":"order_alert","Data":{data_text}}}"#)
        } else {
            format!(r#"{{"Data":{data_text},"Type":"order_alert"}}"#)
        };
        prop_assert_eq!(parse_order_update(&frame).ok(), Some(expected));
    }

    /// Property 3: unknown keys at the top level and inside `Data`, with any
    /// JSON value, interleaved anywhere, never change the decoded order.
    #[test]
    fn prop_unknown_keys_anywhere_give_identical_update(
        data in data_object(),
        inner_junk in prop::collection::vec((unknown_key(), junk_value()), 0..8),
        outer_junk in prop::collection::vec((unknown_key(), junk_value()), 0..6),
        inner_positions in prop::collection::vec(any::<prop::sample::Index>(), 8),
        outer_first in any::<bool>(),
    ) {
        let expected = reference_update(&data);
        let mut pairs: Vec<(String, Value)> =
            data.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        for ((key, value), at) in inner_junk.into_iter().zip(&inner_positions) {
            let index = at.index(pairs.len() + 1);
            pairs.insert(index, (key, value));
        }
        let mut outer: Vec<(String, Value)> = Vec::new();
        let data_value_text = object_text(&pairs);
        let mut text = String::from("{");
        let outer_text: Vec<String> = outer_junk
            .into_iter()
            .map(|(k, v)| {
                outer.push((k.clone(), v.clone()));
                format!("{}:{}", serde_json::to_string(&k).expect("k"), v)
            })
            .collect();
        let mut members = vec![
            format!(r#""Data":{data_value_text}"#),
            r#""Type":"order_alert""#.to_string(),
        ];
        if outer_first {
            let mut all = outer_text;
            all.append(&mut members);
            members = all;
        } else {
            members.extend(outer_text);
        }
        text.push_str(&members.join(","));
        text.push('}');
        prop_assert_eq!(parse_order_update(&text).ok(), Some(expected));
        prop_assert!(outer.iter().all(|(k, _)| k.starts_with('x')));
    }

    /// Property 4a: a controlled-domain numeric literal or numeric string on
    /// an Int field decodes to exactly the integer it spells, or is refused
    /// with `JsonError`; never a neighbouring integer.
    #[test]
    fn prop_int_field_holds_exactly_the_spelled_integer(
        field in prop::sample::select(
            FIELDS.iter().filter(|f| f.2 == Kind::Int).copied().collect::<Vec<_>>()
        ),
        literal in controlled_literal(),
        as_string in any::<bool>(),
        alt_casing in any::<bool>(),
        pad in "[ \t]{0,2}",
    ) {
        let (accepted, alt, _) = field;
        let key = if alt_casing { alt } else { accepted };
        let raw = if as_string {
            serde_json::to_string(&format!("{pad}{literal}{pad}")).expect("s")
        } else {
            literal.clone()
        };
        let result = parse_order_update(&frame_with_one_numeric(key, &raw));
        let expected = if as_string {
            // A string is judged digit by digit (`exact_decimal_i64`), with
            // no 2^52 bound and no float step.
            ref_exact_int(literal.trim())
        } else {
            expected_int_from_literal(&literal)
        };
        match (expected, &result) {
            (Some(v), Ok(update)) => {
                prop_assert_eq!(field_value(update, accepted), json!(v), "{} = {}", key, raw);
            }
            (None, Err(OrderUpdateParseError::JsonError(_))) => {}
            (expected, result) => {
                prop_assert!(false, "{} = {}: expected {:?}, got {:?}", key, raw, expected, result);
            }
        }
    }

    /// Property 4b: a controlled-domain literal or numeric string on a Float
    /// field decodes to exactly the number it spells (the correctly rounded
    /// `f64`, which serde_json reads exactly in this domain).
    #[test]
    fn prop_float_field_holds_exactly_the_spelled_number(
        field in prop::sample::select(
            FIELDS.iter().filter(|f| f.2 == Kind::Float).copied().collect::<Vec<_>>()
        ),
        literal in controlled_literal(),
        as_string in any::<bool>(),
        alt_casing in any::<bool>(),
    ) {
        let (accepted, alt, _) = field;
        let key = if alt_casing { alt } else { accepted };
        let raw = if as_string {
            serde_json::to_string(&literal).expect("s")
        } else {
            literal.clone()
        };
        let update = parse_order_update(&frame_with_one_numeric(key, &raw));
        prop_assert!(update.is_ok(), "{} = {}: {:?}", key, raw, update);
        let got = field_value(&update.unwrap(), accepted).as_f64().unwrap_or(f64::NAN);
        let want: f64 = literal.parse().expect("controlled literal parses");
        prop_assert!(got == want, "{} = {}: got {}, want {}", key, raw, got, want);
    }
}

/// Property 4c: every extreme spelling, as a bare literal and as a string,
/// in both casings, on every numeric field: never a panic, only `Ok` or the
/// typed `JsonError`, never `FrameTooLarge`; an Int that decodes is within 1
/// of the value spelled (never a fabricated integer), a Float that decodes
/// is finite and within one ULP of the value spelled.
#[test]
fn test_extreme_numbers_on_every_numeric_field_never_panic_or_fabricate() {
    let mut decoded = 0usize;
    let mut refused = 0usize;
    for &(accepted, alt, kind) in FIELDS.iter().filter(|f| f.2 != Kind::Text) {
        for key in [accepted, alt] {
            for spelling in EXTREME_SPELLINGS {
                let as_string = serde_json::to_string(spelling).expect("s");
                for raw in [spelling.to_string(), as_string] {
                    let frame = frame_with_one_numeric(key, &raw);
                    let result = parse_order_update(&frame);
                    match &result {
                        Ok(update) => {
                            decoded += 1;
                            let got = field_value(update, accepted);
                            let trimmed = spelling.trim();
                            if trimmed.is_empty() {
                                assert!(
                                    !FILL_CRITICAL.contains(&accepted),
                                    "{key} = {raw}: a blank fill field must fail loud"
                                );
                                assert_eq!(got.as_f64(), Some(0.0), "{key} = {raw}");
                                continue;
                            }
                            let spelled: f64 = trimmed.parse().unwrap_or_else(|e| {
                                panic!("{key} = {raw} decoded but does not spell a number: {e}")
                            });
                            match kind {
                                Kind::Int => {
                                    let v = got.as_i64().expect("int field");
                                    assert!(
                                        (v as f64 - spelled).abs() <= 1.0,
                                        "{key} = {raw}: decoded {v}, spelled {spelled}"
                                    );
                                    if let Some(exact) = ref_exact_int(trimmed) {
                                        assert!(
                                            v == exact || !is_integer_literal(trimmed),
                                            "{key} = {raw}: decoded {v}, exactly {exact}"
                                        );
                                    }
                                }
                                Kind::Float => {
                                    let v = got.as_f64().expect("float field");
                                    assert!(v.is_finite(), "{key} = {raw}: {v}");
                                    let close = v == spelled
                                        || ((v.to_bits() as i64) - (spelled.to_bits() as i64))
                                            .abs()
                                            <= 1;
                                    assert!(close, "{key} = {raw}: decoded {v}, spelled {spelled}");
                                }
                                Kind::Text => unreachable!(),
                            }
                        }
                        Err(OrderUpdateParseError::JsonError(_)) => refused += 1,
                        Err(other) => panic!("{key} = {raw}: wrong error variant {other:?}"),
                    }
                }
            }
        }
    }
    // Both arms must actually be exercised, or the test proves nothing.
    assert!(decoded > 100, "decoded {decoded}");
    assert!(refused > 100, "refused {refused}");
}

/// Exact verdicts for the extreme integers on an Int field, from the
/// module's documented rules: an integer literal decodes iff it fits `i64`;
/// a float literal decodes only below 2^52 and only when integral; a string
/// is judged digit by digit.
#[test]
fn test_extreme_integers_on_int_field_exact_verdicts() {
    let cases: &[(&str, Option<i64>)] = &[
        ("9223372036854775807", Some(i64::MAX)),
        ("-9223372036854775808", Some(i64::MIN)),
        ("9223372036854775808", None),
        ("-9223372036854775809", None),
        ("18446744073709551615", None),
        ("18446744073709551616", None),
        ("123456789012345678901234567890", None),
        ("4503599627370496.0", None),
        ("4503599627370495.0", Some(4_503_599_627_370_495)),
        ("1e308", None),
        ("-0", Some(0)),
        ("-0.0", Some(0)),
        ("0e0", Some(0)),
        ("1e18", None),
        ("1e15", Some(1_000_000_000_000_000)),
    ];
    for &(literal, want) in cases {
        let result = parse_order_update(&frame_with_one_numeric("Quantity", literal));
        match (want, &result) {
            (Some(v), Ok(update)) => assert_eq!(update.quantity, v, "{literal}"),
            (None, Err(OrderUpdateParseError::JsonError(_))) => {}
            _ => panic!("{literal}: want {want:?}, got {result:?}"),
        }
        // As a string: digit by digit, so every in-range integer decodes.
        let quoted = serde_json::to_string(literal).expect("s");
        let result = parse_order_update(&frame_with_one_numeric("Quantity", &quoted));
        match (ref_exact_int(literal), &result) {
            (Some(v), Ok(update)) => assert_eq!(update.quantity, v, "string {literal}"),
            (None, Err(OrderUpdateParseError::JsonError(_))) => {}
            (exact, _) => panic!("string {literal}: want {exact:?}, got {result:?}"),
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 512, ..ProptestConfig::default() })]

    /// Property 5: on an otherwise complete random order, one fill-critical
    /// field made null, empty or whitespace (either casing, the other casing
    /// absent) is refused with `JsonError`, never defaulted to 0. `OrderNo`
    /// and `Status` refuse null. Any OTHER field made null decodes, with
    /// that field at its default and every other field unchanged.
    #[test]
    fn prop_blank_fill_critical_field_is_refused_others_default(
        data in data_object(),
        field_at in 0..FIELDS.len(),
        blank in prop_oneof![Just(Value::Null), Just(json!("")), "[ \t\n]{1,4}".prop_map(Value::String)],
        alt_casing in any::<bool>(),
    ) {
        let (accepted, alt, kind) = FIELDS[field_at];
        let mut frame_data = data.clone();
        frame_data.remove(accepted);
        let key = if alt_casing { alt } else { accepted };
        frame_data.insert(key.to_string(), blank.clone());
        let result = parse_order_update(&frame_text(&frame_data));
        let fails_loud = FILL_CRITICAL.contains(&accepted)
            || (blank.is_null() && NULL_FAILS_LOUD_TEXT.contains(&accepted));
        if fails_loud {
            prop_assert!(is_json_error(&result), "{} = {}: {:?}", key, blank, result);
        } else {
            let mut expected_data = data.clone();
            match (&blank, kind) {
                // A blank string on a Text field is kept verbatim.
                (Value::String(_), Kind::Text) => {
                    expected_data.insert(accepted.to_string(), blank.clone());
                }
                // Null anywhere, or a blank string on a non-fill number:
                // the field reads as absent.
                _ => {
                    expected_data.remove(accepted);
                }
            }
            prop_assert_eq!(
                result.ok(),
                Some(reference_update(&expected_data)),
                "{} = {}", key, blank
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Property 6: hollow and unreadable verdicts against references
// ---------------------------------------------------------------------------

/// Identity text: whitespace (some Unicode `White_Space`, plus a zero-width
/// space, which is NOT whitespace), letters, or empty.
fn identity_text() -> impl Strategy<Value = String> {
    prop_oneof![
        Just(String::new()),
        "[ \t\n\r\u{a0}\u{3000}\u{2028}\u{85}]{1,4}",
        "[\u{200b}\u{feff}]{1,2}",
        "[ ]{0,2}[A-Z0-9]{1,6}[ ]{0,2}",
    ]
}

fn ref_blank(text: &str) -> bool {
    text.chars().all(char::is_whitespace)
}

fn ref_hollow(update: &OrderUpdate) -> bool {
    ref_blank(&update.order_no) && ref_blank(&update.status) && ref_blank(&update.security_id)
}

/// The verdict `unreadable_order_frame_type` documents, written from its doc
/// comment: `None` when an order number is present; otherwise the frame's
/// string `Type` (or a string `type` when `Type` is absent or not a string),
/// trimmed, bounded to 64 characters, returned when the update is hollow or
/// the tag names something other than `order_alert` (ASCII case-insensitive).
fn ref_unreadable(update: &OrderUpdate, frame: &Value) -> Option<String> {
    if !ref_blank(&update.order_no) {
        return None;
    }
    let tag = match frame.get("Type") {
        Some(Value::String(s)) => s.as_str(),
        _ => match frame.get("type") {
            Some(Value::String(s)) => s.as_str(),
            _ => "",
        },
    }
    .trim();
    let foreign = !tag.is_empty() && !tag.eq_ignore_ascii_case(MAX_ORDER_ALERT);
    if ref_hollow(update) || foreign {
        Some(tag.chars().take(TAG_LOG_MAX_CHARS).collect())
    } else {
        None
    }
}

fn tag_value() -> impl Strategy<Value = Option<Value>> {
    prop_oneof![
        Just(None),
        Just(Some(json!("order_alert"))),
        Just(Some(json!("ORDER_Alert"))),
        Just(Some(json!("  order_alert  "))),
        Just(Some(Value::Null)),
        Just(Some(json!(5))),
        Just(Some(json!({}))),
        "[a-z_]{0,12}".prop_map(|s| Some(Value::String(s))),
        "[₹a]{60,70}".prop_map(|s| Some(Value::String(s))),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 2048, ..ProptestConfig::default() })]

    /// `is_hollow_order_update` equals the reference on every combination
    /// of the three identity fields (other fields random).
    #[test]
    fn prop_is_hollow_agrees_with_reference(
        order_no in identity_text(),
        status in identity_text(),
        security_id in identity_text(),
        symbol in "[ -~]{0,8}",
        quantity in any::<i64>(),
    ) {
        let mut update = default_update();
        update.order_no = order_no;
        update.status = status;
        update.security_id = security_id;
        update.symbol = symbol;
        update.quantity = quantity;
        prop_assert_eq!(is_hollow_order_update(&update), ref_hollow(&update));
    }

    /// Decode a random identity in a random casing with random `Type` /
    /// `type` tags, then check both verdicts against the references.
    #[test]
    fn prop_unreadable_order_frame_type_agrees_with_reference(
        order_no in prop::option::of(identity_text()),
        status in prop::option::of(identity_text()),
        security_id in prop::option::of(identity_text()),
        camel in prop::collection::vec(any::<bool>(), 3),
        upper_tag in tag_value(),
        lower_tag in tag_value(),
    ) {
        let mut data = Map::new();
        for ((accepted, alt, value), &use_alt) in [
            ("OrderNo", "orderNo", order_no),
            ("Status", "status", status),
            ("SecurityId", "securityId", security_id),
        ]
        .into_iter()
        .zip(&camel)
        {
            if let Some(text) = value {
                let key = if use_alt { alt } else { accepted };
                data.insert(key.to_string(), Value::String(text));
            }
        }
        let mut frame = Map::new();
        frame.insert("Data".to_string(), Value::Object(data));
        if let Some(tag) = upper_tag {
            frame.insert("Type".to_string(), tag);
        }
        if let Some(tag) = lower_tag {
            frame.insert("type".to_string(), tag);
        }
        let frame = Value::Object(frame);
        let text = frame.to_string();
        let update = parse_order_update(&text);
        prop_assert!(update.is_ok(), "{}: {:?}", text, update);
        let update = update.unwrap();
        prop_assert_eq!(is_hollow_order_update(&update), ref_hollow(&update));
        prop_assert_eq!(
            unreadable_order_frame_type(&update, &text),
            ref_unreadable(&update, &frame),
            "{}", text
        );
    }
}

// ---------------------------------------------------------------------------
// Property 7: decode is a fixed point under re-serialization
// ---------------------------------------------------------------------------

/// A JSON-shaped frame: a random subset of fields in random casings with
/// values of the right type, the wrong type, null or junk.
fn hostile_frame() -> impl Strategy<Value = String> {
    let entries = prop::collection::vec(
        (
            0..FIELDS.len(),
            any::<bool>(),
            prop_oneof![
                Just(Value::Null),
                "[ -~]{0,12}".prop_map(Value::String),
                any::<i64>().prop_map(Value::from),
                any::<f64>()
                    .prop_filter("finite", |f| f.is_finite())
                    .prop_map(|f| json!(f)),
                "-?[0-9]{1,20}(\\.[0-9]{0,6})?(e-?[0-9]{1,3})?".prop_map(Value::String),
                junk_value(),
            ],
        ),
        0..20,
    );
    (entries, tag_value()).prop_map(|(entries, tag)| {
        let mut data = Map::new();
        for (index, use_alt, value) in entries {
            let (accepted, alt, _) = FIELDS[index];
            let key = if use_alt { alt } else { accepted };
            data.insert(key.to_string(), value);
        }
        let mut frame = Map::new();
        frame.insert("Data".to_string(), Value::Object(data));
        if let Some(tag) = tag {
            frame.insert("Type".to_string(), tag);
        }
        Value::Object(frame).to_string()
    })
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 2048, ..ProptestConfig::default() })]

    /// Whatever a hostile frame decodes to, writing that order back out as
    /// a clean frame decodes to the same order (floats within one ULP). The
    /// decoder never invents a value that it would then read differently.
    /// Refusals are the typed `JsonError`.
    #[test]
    fn prop_decoded_order_is_a_fixed_point(frame in hostile_frame()) {
        match parse_order_update(&frame) {
            Ok(update) => {
                let clean = json!({ "Data": update, "Type": MAX_ORDER_ALERT }).to_string();
                let again = parse_order_update(&clean);
                prop_assert!(again.is_ok(), "{} -> {} -> {:?}", frame, clean, again);
                let again = again.unwrap();
                if let Err(diff) = same_order_within_one_ulp(&update, &again) {
                    prop_assert!(false, "{} -> {}: {}", frame, clean, diff);
                }
                // The verdict functions never panic on any decodable frame.
                let _ = unreadable_order_frame_type(&update, &frame);
                prop_assert_eq!(is_hollow_order_update(&update), ref_hollow(&update));
            }
            Err(OrderUpdateParseError::JsonError(_)) => {}
            Err(other) => prop_assert!(false, "{}: wrong variant {:?}", frame, other),
        }
    }

    /// Arbitrary text (not JSON-shaped) never panics, only yields `Ok` or
    /// the typed `JsonError` (all inputs here are under the size cap).
    #[test]
    fn prop_arbitrary_text_is_ok_or_json_error(text in "\\PC{0,200}") {
        let result = parse_order_update(&text);
        prop_assert!(result.is_ok() || is_json_error(&result), "{:?}", result);
        if let Ok(update) = &result {
            let _ = unreadable_order_frame_type(update, &text);
        }
    }
}

/// A frame one byte over the cap is `FrameTooLarge` with its exact length,
/// whatever it contains; the verdict function reads it as an empty tag.
#[test]
fn test_oversized_garbage_is_frame_too_large_with_exact_length() {
    for filler in ['x', '{', '"', '\u{0}'] {
        let text: String = std::iter::repeat_n(filler, ORDER_UPDATE_MAX_FRAME_BYTES + 1).collect();
        match parse_order_update(&text) {
            Err(OrderUpdateParseError::FrameTooLarge { len }) => {
                assert_eq!(len, ORDER_UPDATE_MAX_FRAME_BYTES + 1);
            }
            other => panic!("filler {filler:?}: {other:?}"),
        }
        assert_eq!(
            unreadable_order_frame_type(&default_update(), &text).as_deref(),
            Some("")
        );
    }
}

/// The restated field table matches the struct's own serialized key set,
/// so a drift in this file fails here and not silently in a property.
#[test]
fn test_restated_field_table_covers_every_order_update_field() {
    let keys: Vec<String> = serde_json::to_value(default_update())
        .expect("serialize")
        .as_object()
        .expect("object")
        .keys()
        .cloned()
        .collect();
    assert_eq!(keys.len(), FIELDS.len());
    for key in &keys {
        let at = field_index(key);
        let (_, alt, kind) = FIELDS[at];
        assert!(
            !keys.contains(&alt.to_string()),
            "{alt} must be the other casing"
        );
        let sample = serde_json::to_value(default_update()).expect("v")[key.as_str()].clone();
        let observed = if sample.is_string() {
            Kind::Text
        } else if sample.is_i64() {
            Kind::Int
        } else {
            Kind::Float
        };
        assert_eq!(observed, kind, "{key}");
    }
}

/// BUG (found 2026-10-30 by `test_extreme_integers_on_int_field_exact_verdicts`):
/// a FRACTIONAL JSON number on an Int field is decoded as an integer when
/// serde_json (built without `float_roundtrip`) misreads it by one ULP onto
/// an integral `f64`, although the correctly rounded `f64` is fractional.
/// The module documents only the narrower limit "a fractional wire number
/// whose NEAREST `f64` is integral reads as that integer"; in every case
/// below the nearest `f64` is NOT integral, so each should fail the typed
/// parse loudly, and today each decodes to a made-up integer quantity.
///
/// Minimal input: `{"Data":{"OrderNo":"7","Status":"TRADED","Quantity":100.00000000000001}}`
/// decodes with `quantity == 100` (correctly rounded value
/// 100.0000000000000142..., not integral).
///
/// Proposed fix: enable serde_json's `float_roundtrip` feature (needs owner
/// approval, it is a dependency feature change) so the `f64` is correctly
/// rounded; then only the documented nearest-is-integral limit remains.
/// Alternative with no dependency change: before the `Value` parse, refuse
/// an Int-typed field whose raw JSON number text has a non-zero fractional
/// digit (would need a raw-text scan or `RawValue`).
#[test]
#[ignore = "BUG: fractional JSON number on an Int field misread by serde_json onto an integer (100.00000000000001 -> 100)"]
fn bug_fractional_json_number_on_int_field_is_never_an_integer() {
    for literal in [
        "100.00000000000001",
        "4503599627370495.5",
        "2251799813685245.25",
    ] {
        // Rust's own parse is correctly rounded: the value really is fractional.
        let correctly_rounded: f64 = literal.parse().expect("a decimal literal");
        assert_ne!(correctly_rounded.fract(), 0.0, "{literal}");
        let result = parse_order_update(&frame_with_one_numeric("Quantity", literal));
        assert!(
            is_json_error(&result),
            "{literal}: a fractional quantity must fail loudly, got {:?}",
            result.map(|u| u.quantity)
        );
    }
}
