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
            assert_eq!(again.order_no, update.order_no);
            assert_eq!(again.status, update.status);
            assert_eq!(again.quantity, update.quantity);
            assert_eq!(again.traded_qty, update.traded_qty);
            assert_eq!(again.security_id, update.security_id);
        }
        Err(tickvault_core::parser::order_update::OrderUpdateParseError::JsonError(_)) => {}
        Err(other) => panic!("untyped refusal: {other:?}"),
    }
});
