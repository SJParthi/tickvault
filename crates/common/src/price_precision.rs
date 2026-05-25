//! Price precision helpers shared between `tickvault-storage` and
//! `tickvault-trading`.
//!
//! Dhan WebSocket sends prices as `f32`. Naive `f64::from(f32)` widens
//! the IEEE-754 bit pattern, introducing artifacts:
//!
//! ```text
//! 10.20_f32       → f64::from() → 10.19999980926514_f64  (WRONG — 12 decimals)
//! 23925.65_f32    → f64::from() → 23925.650390625_f64    (WRONG — 9 decimals)
//! 23937.30_f32    → f64::from() → 23937.30078125_f64     (WRONG — 8 decimals)
//! ```
//!
//! [`f32_to_f64_clean`] avoids this by going through the shortest
//! decimal representation that round-trips through `f32` (ryu's
//! `Display` impl), then parsing that string back as `f64`. The
//! result is the operator-visible value with no IEEE-754 widening.
//!
//! ## Authority
//!
//! `.claude/rules/project/data-integrity.md` "Price Precision
//! Preservation" demands this for ALL f32→f64 conversions that land
//! in QuestDB. The companion banned-pattern hook
//! (`.claude/hooks/banned-pattern-scanner.sh`) blocks bare
//! `f64::from(` in any crate that writes prices to storage.
//!
//! ## Why this lives in `tickvault-common`
//!
//! Both `tickvault-storage::tick_persistence` (writes the `ticks`
//! table) and `tickvault-trading::candles::aggregator_cell` (drives
//! the `candles_*` tables) need it. Common is the only crate both
//! depend on. Storage previously held the impl; the candle aggregator
//! used `f64::from(f32)` instead — which corrupted every sealed
//! candle row until this module was introduced (operator-spotted
//! 2026-05-25: `23937.30078125` in `candles_1m`).

use std::io::Write;

/// Maximum buffer needed for any f32's shortest decimal representation
/// (ryu's f32 buffer is 16; +8 of slack for safety).
const F32_DECIMAL_BUF_SIZE: usize = 24;

/// Converts `f32` → `f64` without IEEE-754 widening artifacts.
///
/// Zero allocation. O(1) — one ryu format + one parse.
///
/// # Performance
///
/// Hot path safe — no heap allocation; ~50ns per call on the bench
/// machine. Used in `tick_persistence::append_tick` (per-tick) and
/// `aggregator_cell::fold_in_bucket` (per-tick) — both budgeted at
/// <100ns end-to-end.
#[inline]
#[must_use]
pub fn f32_to_f64_clean(v: f32) -> f64 {
    if v == 0.0 || !v.is_finite() {
        // APPROVED: f64::from(f32) is correct for zero/inf/NaN — no
        // precision loss for these values, only for ordinary decimals.
        return f64::from(v);
    }
    let mut buf = [0u8; F32_DECIMAL_BUF_SIZE];
    let mut cursor = std::io::Cursor::new(&mut buf[..]);
    // f32 Display uses ryu internally — produces the shortest decimal
    // representation that round-trips through f32. Zero allocation.
    drop(write!(cursor, "{v}"));
    #[allow(clippy::arithmetic_side_effects)]
    // APPROVED: cursor position of a 24-byte buf always fits usize
    let n = cursor.position() as usize;
    // f32 Display only produces ASCII digits, '.', '-', 'e', '+'.
    std::str::from_utf8(&buf[..n])
        .ok()
        .and_then(|s| s.parse::<f64>().ok())
        // APPROVED: f64::from(f32) fallback — only reached if
        // ryu/parse fails (never in practice for finite f32).
        .unwrap_or(f64::from(v))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_f32_to_f64_clean_preserves_simple_prices() {
        // Operator-observed corruption: 23925.65_f32 was landing as
        // 23925.650390625_f64 in candles_1m via f64::from().
        assert_eq!(f32_to_f64_clean(23925.65_f32), 23925.65_f64);
        assert_eq!(f32_to_f64_clean(23937.30_f32), 23937.30_f64);
        assert_eq!(f32_to_f64_clean(23924.40_f32), 23924.40_f64);
        assert_eq!(f32_to_f64_clean(10.20_f32), 10.20_f64);
        assert_eq!(f32_to_f64_clean(21004.95_f32), 21004.95_f64);
    }

    #[test]
    fn test_f32_to_f64_clean_naive_conversion_differs() {
        // Sanity check that the naive path IS broken — proves we're
        // not just shadowing a no-op.
        assert_ne!(f64::from(23925.65_f32), 23925.65_f64);
        assert_ne!(f64::from(10.20_f32), 10.20_f64);
    }

    #[test]
    fn test_f32_to_f64_clean_zero_passthrough() {
        assert_eq!(f32_to_f64_clean(0.0_f32), 0.0_f64);
    }

    #[test]
    fn test_f32_to_f64_clean_negative_prices() {
        // Greeks / OI deltas can be negative; aggregator does not use
        // them but the helper must handle them correctly.
        assert_eq!(f32_to_f64_clean(-23925.65_f32), -23925.65_f64);
        assert_eq!(f32_to_f64_clean(-0.05_f32), -0.05_f64);
    }

    #[test]
    fn test_f32_to_f64_clean_infinity_and_nan_safe() {
        // Should not panic on edge inputs. NaN is excluded from PartialEq.
        assert!(f32_to_f64_clean(f32::INFINITY).is_infinite());
        assert!(f32_to_f64_clean(f32::NEG_INFINITY).is_infinite());
        assert!(f32_to_f64_clean(f32::NAN).is_nan());
    }

    #[test]
    fn test_f32_to_f64_clean_small_index_increments() {
        // NSE tick size for indices = 0.05; the aggregator sees these
        // values on every NIFTY/BANKNIFTY tick.
        for cents in 0_i32..1_000 {
            let raw = (cents as f32) * 0.05;
            let clean = f32_to_f64_clean(raw);
            // Round-trip through f32 is lossy; we only assert the
            // clean value has at most 2 decimal places (no IEEE
            // widening artifacts).
            let scaled = (clean * 100.0).round() / 100.0;
            assert!(
                (clean - scaled).abs() < 1e-9,
                "f32_to_f64_clean({raw}) = {clean} did not quantize to 2dp",
            );
        }
    }
}
