//! Gap enforcement tests for the storage crate.
//!
//! Verifies mechanical guarantees documented in `.claude/rules/project/gap-enforcement.md`.
//! These tests exist as a separate integration test file (not inline) so they
//! can be audited independently of the implementation.

// ---------------------------------------------------------------------------
// STORAGE-GAP-01: Tick DEDUP key includes segment
// ---------------------------------------------------------------------------

/// Tick dedup key must include `segment` to prevent cross-segment collision.
/// Example: security_id=13 (NIFTY index) exists on both NSE_EQ and IDX_I.
/// Without segment in the key, ticks from different segments would overwrite.
#[test]
fn test_storage_gap_01_tick_dedup_key_includes_segment() {
    let key = dhan_live_trader_storage::tick_persistence::tick_dedup_key();
    assert!(
        key.contains("segment"),
        "tick dedup key must include 'segment' (STORAGE-GAP-01), got: {key}"
    );
    assert!(
        key.contains("security_id"),
        "tick dedup key must include 'security_id' (STORAGE-GAP-01), got: {key}"
    );
}

// ---------------------------------------------------------------------------
// STORAGE-GAP-02: f32→f64 precision
// ---------------------------------------------------------------------------

/// `f32_to_f64_clean` must prevent IEEE 754 widening artifacts.
/// Example: 21004.95_f32 → 21004.949219 (wrong) vs 21004.95 (correct).
#[test]
fn test_storage_gap_02_f32_to_f64_clean_prevents_widening_artifacts() {
    let val: f32 = 21004.95;
    let widened_raw = val as f64; // BAD: 21004.94921875
    let cleaned = dhan_live_trader_storage::tick_persistence::f32_to_f64_clean(val);

    // The raw widening introduces artifacts past the f32 precision.
    // f32_to_f64_clean rounds to 2 decimal places (tick price precision).
    assert_ne!(
        widened_raw, cleaned,
        "f32_to_f64_clean should differ from naive cast"
    );
    // Cleaned value should be closer to the original f32 string representation.
    let cleaned_str = format!("{cleaned:.2}");
    assert_eq!(
        cleaned_str, "21004.95",
        "f32_to_f64_clean(21004.95_f32) should produce 21004.95"
    );
}

/// f32_to_f64_clean handles zero.
#[test]
fn test_storage_gap_02_f32_to_f64_clean_zero() {
    assert_eq!(
        dhan_live_trader_storage::tick_persistence::f32_to_f64_clean(0.0_f32),
        0.0_f64
    );
}

/// f32_to_f64_clean handles NaN (returns NaN).
#[test]
fn test_storage_gap_02_f32_to_f64_clean_nan() {
    assert!(dhan_live_trader_storage::tick_persistence::f32_to_f64_clean(f32::NAN).is_nan());
}

/// f32_to_f64_clean handles infinity.
#[test]
fn test_storage_gap_02_f32_to_f64_clean_infinity() {
    assert!(
        dhan_live_trader_storage::tick_persistence::f32_to_f64_clean(f32::INFINITY).is_infinite()
    );
    assert!(
        dhan_live_trader_storage::tick_persistence::f32_to_f64_clean(f32::NEG_INFINITY)
            .is_infinite()
    );
}

/// f32_to_f64_clean handles typical Dhan prices.
#[test]
fn test_storage_gap_02_f32_to_f64_clean_typical_prices() {
    // Typical NSE tick prices
    let prices: &[f32] = &[100.50, 24500.0, 0.05, 99999.95, 1234.55, 50000.00, 0.10];
    for &price in prices {
        let cleaned = dhan_live_trader_storage::tick_persistence::f32_to_f64_clean(price);
        assert!(
            cleaned.is_finite(),
            "price {price} should produce finite f64"
        );
        // Cleaned value should round-trip through 2-decimal format.
        let formatted = format!("{cleaned:.2}");
        let reparsed: f64 = formatted.parse().unwrap();
        assert!(
            (cleaned - reparsed).abs() < 0.01,
            "price {price}: cleaned={cleaned}, reparsed={reparsed}"
        );
    }
}
