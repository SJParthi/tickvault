//! Panic safety tests for common crate.
//!
//! Verifies that public API functions never panic on arbitrary/garbage input.
//! Part of the 22 test types: Type 13 (Panic Safety).

use figment::providers::Format;
use tickvault_common::types::ExchangeSegment;

// ---------------------------------------------------------------------------
// ExchangeSegment::from_byte — must never panic on any u8 input
// ---------------------------------------------------------------------------

#[test]
fn exchange_segment_from_byte_all_256_values_no_panic() {
    for byte in 0..=u8::MAX {
        let result = ExchangeSegment::from_byte(byte);
        match byte {
            0 | 1 | 2 | 3 | 4 | 5 | 7 | 8 => {
                assert!(result.is_some(), "byte {byte} should be valid");
            }
            _ => {
                assert!(result.is_none(), "byte {byte} should be None (unknown)");
            }
        }
    }
}

#[test]
fn exchange_segment_gap_at_6_returns_none() {
    assert!(
        ExchangeSegment::from_byte(6).is_none(),
        "byte 6 is a gap in the ExchangeSegment enum — must be None"
    );
}

// ---------------------------------------------------------------------------
// Config parsing — garbage TOML must not panic
// ---------------------------------------------------------------------------

#[test]
fn config_from_empty_string_does_not_panic() {
    let result = std::panic::catch_unwind(|| {
        let _: Result<tickvault_common::config::ApplicationConfig, _> = figment::Figment::new()
            .merge(figment::providers::Toml::string(""))
            .extract();
    });
    assert!(result.is_ok(), "empty TOML must not panic");
}

#[test]
fn config_from_garbage_toml_does_not_panic() {
    let result = std::panic::catch_unwind(|| {
        let _: Result<tickvault_common::config::ApplicationConfig, _> = figment::Figment::new()
            .merge(figment::providers::Toml::string("{{{invalid toml!!!"))
            .extract();
    });
    assert!(result.is_ok(), "garbage TOML must not panic");
}

// ---------------------------------------------------------------------------
// segment_code_to_str — must never panic
// ---------------------------------------------------------------------------

#[test]
fn segment_code_to_str_all_256_values_no_panic() {
    for code in 0..=u8::MAX {
        let _ = tickvault_common::segment::segment_code_to_str(code);
    }
}
