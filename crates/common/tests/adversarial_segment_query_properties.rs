//! Adversarial properties of the `?segment=` query value on
//! `GET /api/quote/{security_id}`.
//!
//! The handler maps the caller's text with `segment::segment_str_to_code`
//! and puts ONLY `segment_code_to_str(code)` (a fixed name) into its SQL, so
//! these two functions are the whole input surface:
//!
//! 1. Any text maps to a code only if that code's canonical name is exactly
//!    the text (so no other text, however crafted, reaches the SQL).
//! 2. Every one of the 256 codes either has a canonical name that maps back
//!    to it, or is "UNKNOWN", which maps to nothing.
//! 3. Case, whitespace, NUL, quote, comment and Unicode look-alike variants
//!    of every real name are refused.

use proptest::prelude::*;
use proptest::test_runner::Config as ProptestConfig;
use tickvault_common::segment::{segment_code_to_str, segment_str_to_code};

/// Every canonical name, read from the code space rather than restated.
fn canonical_names() -> Vec<&'static str> {
    (0..=u8::MAX)
        .map(segment_code_to_str)
        .filter(|n| *n != "UNKNOWN")
        .collect()
}

#[test]
fn test_every_code_round_trips_or_is_unknown() {
    let mut known = 0;
    for code in 0..=u8::MAX {
        let name = segment_code_to_str(code);
        if name == "UNKNOWN" {
            assert_eq!(segment_str_to_code(name), None);
        } else {
            known += 1;
            assert_eq!(segment_str_to_code(name), Some(code), "{name}");
        }
    }
    assert_eq!(known, 8, "eight named segments");
}

#[test]
fn test_hostile_variants_of_every_name_are_refused() {
    for name in canonical_names() {
        let variants = [
            name.to_ascii_lowercase(),
            format!(" {name}"),
            format!("{name} "),
            format!("{name}\0"),
            format!("{name}'"),
            format!("{name}' OR '1'='1"),
            format!("{name};--"),
            format!("{name}\n"),
            format!("\u{feff}{name}"),
            name.replace('_', "\u{2010}"),
            name.replace('I', "\u{0399}"),
            name.replace('_', ""),
            format!("{name},{name}"),
        ];
        for variant in variants {
            if variant == name {
                continue;
            }
            assert_eq!(segment_str_to_code(&variant), None, "{variant:?}");
        }
    }
    for empty in ["", " ", "UNKNOWN", "null", "%20NSE_EQ"] {
        assert_eq!(segment_str_to_code(empty), None, "{empty:?}");
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 4096, ..ProptestConfig::default() })]

    /// Property 1 over arbitrary text and near-miss text.
    #[test]
    fn prop_only_a_canonical_name_maps_to_a_code(text in prop_oneof![
        "\\PC{0,24}",
        "(?s).{0,24}",
        "[A-Z_]{0,14}",
        "(IDX|NSE|BSE|MCX)_(I|EQ|FNO|CURRENCY|COMM)[ -~]{0,2}",
    ]) {
        if let Some(code) = segment_str_to_code(&text) {
            prop_assert_eq!(segment_code_to_str(code), text.as_str());
        }
    }
}
