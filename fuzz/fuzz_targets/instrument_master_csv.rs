//! Fuzz target: the Dhan instrument master CSV.
//!
//! Feeds arbitrary bytes (as UTF-8 text) to `parse_master_csv` and every
//! cell parser. Properties beyond "no panic":
//!
//! * a lot size, when accepted, is a positive whole number whose decimal
//!   text has no non-zero fraction digit (the 2026-10-30 fix);
//! * `parse_master_csv` refuses or returns rows; it never panics on short
//!   rows, quotes, a BOM or CRLF.

#![no_main]

use libfuzzer_sys::fuzz_target;
use tickvault_core::instrument::master_csv::{
    parse_expiry_ymd, parse_lot_size, parse_master_csv, parse_strike_paise,
};

/// Large enough for many rows; the real file is parsed once a day.
const MAX_INPUT: usize = 256 * 1024;

fuzz_target!(|data: &[u8]| {
    if data.len() > MAX_INPUT {
        return;
    }
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    let _ = parse_master_csv(text);
    for cell in text.split([',', '\n']).take(64) {
        let _ = parse_expiry_ymd(cell);
        let _ = parse_strike_paise(cell);
        let lot = parse_lot_size(cell);
        if lot != 0 {
            let trimmed = cell.trim();
            let mantissa = trimmed.split(['e', 'E']).next().unwrap_or(trimmed);
            if !trimmed.contains(['e', 'E']) {
                if let Some((_, fraction)) = mantissa.split_once('.') {
                    assert!(
                        fraction.bytes().all(|b| b == b'0'),
                        "lot {lot} accepted from fractional text {cell:?}"
                    );
                }
            }
        }
    }
});
