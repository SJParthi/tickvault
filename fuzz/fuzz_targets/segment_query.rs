//! Fuzz target: the `?segment=` value on `GET /api/quote/{security_id}`.
//!
//! Property: any text maps to a segment code only if it is exactly that
//! code's canonical name, so nothing but a fixed name reaches the SQL.

#![no_main]

use libfuzzer_sys::fuzz_target;
use tickvault_common::segment::{segment_code_to_str, segment_str_to_code};

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    if let Some(code) = segment_str_to_code(text) {
        assert_eq!(segment_code_to_str(code), text);
    }
});
