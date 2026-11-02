//! Fuzz target: order-update WebSocket JSON (`wss://api-order-update.dhan.co`).
//!
//! Feeds arbitrary bytes (as UTF-8 text) to the order-update decoder and its
//! two verdict helpers. Properties beyond "no panic":
//!
//! * a refusal is only ever the typed `JsonError`;
//! * an accepted order, written back out as a clean frame, decodes again
//!   (the decoder never produces an order it would then refuse), with every
//!   integer and text field identical.
//!
//! Floats are compared only for "decodes again": serde_json without
//! `float_roundtrip` can re-read a float a few ULPs off (pinned in
//! `crates/core/tests/adversarial_order_update_properties.rs`).

#![no_main]

use libfuzzer_sys::fuzz_target;
use tickvault_core::parser::order_update::{
    is_hollow_order_update, parse_order_update, unreadable_order_frame_type,
};

/// Real frames are a few KiB; the decoder's own cap refuses far larger ones.
const MAX_INPUT: usize = 64 * 1024;

fuzz_target!(|data: &[u8]| {
    if data.len() > MAX_INPUT {
        return;
    }
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    match parse_order_update(text) {
        Ok(update) => {
            let _ = is_hollow_order_update(&update);
            let _ = unreadable_order_frame_type(&update, text);
            let clean = serde_json::json!({ "Data": &update, "Type": "order_alert" }).to_string();
            let again = match parse_order_update(&clean) {
                Ok(again) => again,
                Err(err) => panic!("a decoded order refused on re-read: {err:?} {clean}"),
            };
            let Ok(serde_json::Value::Object(expected)) = serde_json::to_value(&update) else {
                panic!("accepted order must serialize as an object");
            };
            let Ok(serde_json::Value::Object(actual)) = serde_json::to_value(&again) else {
                panic!("re-read order must serialize as an object");
            };
            assert_eq!(actual.len(), expected.len());
            // Select by the serialized field type, so every integer and text
            // field is covered, including fields added to OrderUpdate later.
            // Number::is_i64/is_u64 do not accept floating-point variants.
            for (field, value) in &expected {
                if value.is_string() || value.is_i64() || value.is_u64() {
                    assert_eq!(actual.get(field), Some(value), "changed field {field}");
                }
            }
        }
        Err(tickvault_core::parser::order_update::OrderUpdateParseError::JsonError(_)) => {}
        Err(other) => panic!("untyped refusal: {other:?}"),
    }
});
