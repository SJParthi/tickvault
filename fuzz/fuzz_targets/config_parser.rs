//! Fuzz target: TOML config parser.
//!
//! Feeds random bytes (interpreted as UTF-8 TOML) into ApplicationConfig
//! deserialization + validation. Must NEVER panic — only return errors.

#![no_main]

use libfuzzer_sys::fuzz_target;

use dhan_live_trader_common::config::ApplicationConfig;

fuzz_target!(|data: &[u8]| {
    // Treat fuzz input as TOML string
    let Ok(toml_str) = std::str::from_utf8(data) else {
        return; // Not valid UTF-8 — skip
    };

    // Attempt to deserialize as ApplicationConfig
    // This must NEVER panic — only return Ok or Err.
    if let Ok(config) = toml::from_str::<ApplicationConfig>(toml_str) {
        // If deserialization succeeded, validation must also not panic
        let _ = config.validate();
    }
});
