//! Fuzz target: binary tick parser.
//!
//! Feeds random bytes into `dispatch_frame` to find panics/crashes.
//! If this target crashes, the parser has undefined behavior on malformed input.

#![no_main]

use libfuzzer_sys::fuzz_target;

use dhan_live_trader_core::parser::dispatcher::dispatch_frame;

fuzz_target!(|data: &[u8]| {
    // dispatch_frame must NEVER panic — only return Ok or Err.
    // Any panic here = fuzz crash = bug.
    let _ = dispatch_frame(data, 0);
});
