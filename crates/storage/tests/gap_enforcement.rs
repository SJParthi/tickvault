//! Gap enforcement tests for the storage crate.
//!
//! Verifies mechanical guarantees documented in `.claude/rules/project/gap-enforcement.md`.
//! These tests exist as a separate integration test file (not inline) so they
//! can be audited independently of the implementation.

// ---------------------------------------------------------------------------
// STORAGE-GAP-01: Tick DEDUP key includes segment — RETIRED
// ---------------------------------------------------------------------------

// RETIRED (stage-2 dead-WS sweep, 2026-07-17):
// `test_storage_gap_01_tick_dedup_key_includes_segment` pinned
// `tick_persistence::tick_dedup_key()` — the tick writer (and with it the
// in-repo `DEDUP_KEY_TICKS` constant) was DELETED with the dead Dhan tick
// chain; nothing writes the `ticks` table anymore, so there is no write-side
// key to pin. The `ticks` TABLE (already DEDUP-keyed on
// `security_id, segment, capture_seq, feed` on the live box) is retained
// read-only (SEBI). Any future tick writer must restore the constant AND
// this gap test (gap-enforcement.md STORAGE-GAP-01 stands as the contract).

// ---------------------------------------------------------------------------
// STORAGE-GAP-02: f32→f64 precision (canonical primitive in common)
// ---------------------------------------------------------------------------
// Stage-2 dead-WS sweep (2026-07-17): re-pointed from the deleted
// `tick_persistence::f32_to_f64_clean` WRAPPER to the canonical primitive
// `tickvault_common::price_precision::f32_to_f64_clean` (which the surviving
// REST/seal write paths consume). The gap contract itself is unchanged.

use tickvault_common::price_precision::f32_to_f64_clean;

/// `f32_to_f64_clean` must prevent IEEE 754 widening artifacts.
/// 21004.95_f32 naively widened gives 21004.94921875.
#[test]
fn test_storage_gap_02_f32_to_f64_clean_prevents_widening_artifacts() {
    let val: f32 = 21004.95;
    let naive = f64::from(val);
    let cleaned = f32_to_f64_clean(val);

    // f32_to_f64_clean rounds to 2 decimal places (tick price precision).
    assert!(
        (cleaned - 21004.95_f64).abs() < 1e-9,
        "f32_to_f64_clean(21004.95_f32) should produce 21004.95"
    );
    assert!(
        (naive - cleaned).abs() > 0.0,
        "f32_to_f64_clean should differ from naive cast"
    );
}

/// f32_to_f64_clean handles zero.
#[test]
fn test_storage_gap_02_f32_to_f64_clean_zero() {
    assert_eq!(f32_to_f64_clean(0.0_f32), 0.0_f64);
}

/// f32_to_f64_clean handles NaN (returns NaN).
#[test]
fn test_storage_gap_02_f32_to_f64_clean_nan() {
    assert!(f32_to_f64_clean(f32::NAN).is_nan());
}

/// f32_to_f64_clean handles infinity.
#[test]
fn test_storage_gap_02_f32_to_f64_clean_infinity() {
    assert!(f32_to_f64_clean(f32::INFINITY).is_infinite());
    assert!(f32_to_f64_clean(f32::NEG_INFINITY).is_infinite());
}

/// f32_to_f64_clean handles typical Dhan prices.
#[test]
fn test_storage_gap_02_f32_to_f64_clean_typical_prices() {
    for price in [23_146.45_f32, 51_234.7_f32, 100.05_f32, 0.05_f32] {
        let cleaned = f32_to_f64_clean(price);
        assert!(cleaned.is_finite(), "cleaned price must be finite");
        assert!(cleaned > 0.0, "cleaned price must stay positive");
    }
}
