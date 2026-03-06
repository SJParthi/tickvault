//! Fuzz target: TOML config parser.
//!
//! Feeds random bytes (interpreted as UTF-8 TOML) into ApplicationConfig
//! deserialization + validation. Must NEVER panic — only return errors.

#![no_main]

use libfuzzer_sys::fuzz_target;

use dhan_live_trader_common::config::ApplicationConfig;

/// Maximum config input size (4 KiB). Real config files are ~2 KiB.
/// Tight limit to prevent stack overflow from deeply nested TOML
/// under AddressSanitizer (which reduces default stack size to ~128 KB).
const MAX_CONFIG_INPUT_SIZE: usize = 4_096;

/// Maximum TOML nesting depth. Prevents recursive parser stack overflow.
/// Real configs have ~3 levels; 32 is extremely generous.
const MAX_NESTING_DEPTH: usize = 32;

/// Quick check for excessive bracket nesting in TOML input.
/// Counts maximum `[` depth (including inline tables `{`).
fn exceeds_nesting_depth(input: &str, max_depth: usize) -> bool {
    let mut depth: usize = 0;
    for byte in input.bytes() {
        match byte {
            b'[' | b'{' => {
                depth += 1;
                if depth > max_depth {
                    return true;
                }
            }
            b']' | b'}' => {
                depth = depth.saturating_sub(1);
            }
            _ => {}
        }
    }
    false
}

fuzz_target!(|data: &[u8]| {
    // Reject oversized input
    if data.len() > MAX_CONFIG_INPUT_SIZE {
        return;
    }

    // Treat fuzz input as TOML string
    let Ok(toml_str) = std::str::from_utf8(data) else {
        return; // Not valid UTF-8 — skip
    };

    // Reject deeply nested input — prevents stack overflow under ASan
    if exceeds_nesting_depth(toml_str, MAX_NESTING_DEPTH) {
        return;
    }

    // Attempt to deserialize as ApplicationConfig
    // This must NEVER panic — only return Ok or Err.
    if let Ok(config) = toml::from_str::<ApplicationConfig>(toml_str) {
        // If deserialization succeeded, validation must also not panic
        let _ = config.validate();
    }
});
