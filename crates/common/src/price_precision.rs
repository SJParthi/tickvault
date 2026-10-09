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

/// Powers of ten tried by [`f32_to_f64_clean`]'s arithmetic path, in
/// order: decimal places 0 through 9. Every entry is exact in `f64`.
const DECIMAL_SCALES: [f64; 10] = [1.0, 1e1, 1e2, 1e3, 1e4, 1e5, 1e6, 1e7, 1e8, 1e9];

/// Magnitude bound of the arithmetic path: 2^24. Below it every integer
/// is an `f32`, so the shortest decimal of a value never ends in an
/// integer-part zero the arithmetic could miss; at or above it the
/// shortest form can round the integer part (`123456792_f32` displays
/// as `123456790`), and the text path handles it.
const ARITHMETIC_PATH_MAX_MAGNITUDE: f32 = 16_777_216.0;

/// Converts `f32` → `f64` without IEEE-754 widening artifacts.
///
/// The result is the `f64` nearest to the shortest decimal that
/// round-trips through `f32` (what `{v}` prints). Zero allocation.
///
/// # How
///
/// For `0 < |v| < 2^24` it searches decimal places 0..=9: at each scale
/// `s` it takes the nearest integer `m` to `v·s` (then its other
/// neighbour) and accepts the first `m / s` that rounds back to `v` as
/// an `f32`. `m` and `s` are exact in `f64`, so the IEEE division gives
/// the nearest `f64` to the decimal `m / s`, which is what parsing the
/// printed text gives. Anything else (zero, non-finite, larger values,
/// or no hit within nine places) takes the format-and-parse path.
///
/// Bit-identical to the format-and-parse path for all 2^32 inputs,
/// checked by `test_f32_to_f64_clean_matches_text_path_for_every_f32`
/// (`#[ignore]`d: about 8 minutes on 4 cores, release).
///
/// # Performance
///
/// O(1): at most 10 scales × 2 candidates, then the text path. Measured
/// 2026-10-09 (release, x86 dev container, 1,000 price-like inputs):
/// about 17 ns per call against 129 ns for format-and-parse. Used per
/// tick in `tick_persistence::append_tick` and the candle fold.
#[inline]
#[must_use]
pub fn f32_to_f64_clean(v: f32) -> f64 {
    let magnitude = v.abs();
    if magnitude > 0.0 && magnitude < ARITHMETIC_PATH_MAX_MAGNITUDE {
        // APPROVED: f64::from(f32) is exact; only the scaled candidates
        // below are returned, never this widened value.
        let widened = f64::from(v);
        for scale in DECIMAL_SCALES {
            let scaled = widened * scale;
            let nearest = scaled.round();
            let candidate = nearest / scale;
            #[allow(clippy::cast_possible_truncation)]
            // APPROVED: narrowing is the round-trip test itself
            if candidate as f32 == v {
                return candidate;
            }
            let other = if scaled > nearest {
                nearest + 1.0
            } else {
                nearest - 1.0
            };
            let candidate = other / scale;
            #[allow(clippy::cast_possible_truncation)]
            // APPROVED: narrowing is the round-trip test itself
            if candidate as f32 == v {
                return candidate;
            }
        }
    }
    f32_to_f64_clean_via_text(v)
}

/// The format-and-parse conversion: print the shortest round-trip
/// decimal (`{v}`, ryu) and parse it as `f64`. The reference the
/// arithmetic path in [`f32_to_f64_clean`] must match, and its fallback.
fn f32_to_f64_clean_via_text(v: f32) -> f64 {
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

/// Rounds a finite `f64` to exactly 2 decimal places (half-away-from-zero).
///
/// Operator rule (2026-05-29): persisted DOUBLE columns for percentage /
/// price values carry at most two digits after the decimal point. This is
/// the canonical rounding primitive for that contract — write sites round
/// through THIS function so a 17-digit artifact like
/// `0.10142836229792751` becomes the 2-dp value `0.1`.
///
/// Non-finite inputs (`NaN` / `±Inf`) are returned unchanged. `-0.0`
/// normalises to `0.0` so QuestDB never stores a signed zero.
///
/// # Performance
/// O(1): two multiplies + one `round` + one divide. No allocation. Safe on
/// the seal path (called per sealed candle, not per tick).
#[inline]
#[must_use]
pub fn round_to_2dp(value: f64) -> f64 {
    if !value.is_finite() {
        return value;
    }
    let rounded = (value * 100.0).round() / 100.0;
    if rounded == 0.0 { 0.0 } else { rounded }
}

// ============================================================================
// Display-layer 2-decimal formatters (Sub-PR #4.5)
// ============================================================================

/// Sentinel for non-finite display values. NaN / +inf / -inf render as
/// an em-dash (operator-locked 2026-05-27) rather than "NaN" / "inf"
/// which are confusing in a trading context. Callers MUST use these
/// helpers — direct `format!("{:.2}", value)` is banned in
/// operator-facing string paths per Sub-PR #4.6 (future hook addition).
const NON_FINITE_SENTINEL: &str = "—";

/// Format a price value for operator display — exactly 2 decimal places,
/// non-finite values rendered as em-dash sentinel.
///
/// Operator-locked 2026-05-27: every price emitted via Telegram, UI, or
/// REST response MUST go through this helper. Storage + RAM keep full
/// f64 precision; only the operator-facing boundary rounds to 2 decimals.
///
/// # Examples
///
/// ```
/// use tickvault_common::price_precision::format_price_2_decimals;
/// assert_eq!(format_price_2_decimals(2500.55), "2500.55");
/// assert_eq!(format_price_2_decimals(2520.5500488), "2520.55");
/// assert_eq!(format_price_2_decimals(f64::NAN), "—");
/// ```
#[must_use]
pub fn format_price_2_decimals(value: f64) -> String {
    if !value.is_finite() {
        return NON_FINITE_SENTINEL.to_string();
    }
    format!("{value:.2}")
}

/// Format a percentage value for operator display — exactly 2 decimal
/// places, leading sign ('+' or '-'), trailing '%'. Zero renders without
/// a sign. Non-finite values render as em-dash sentinel.
///
/// Operator-locked 2026-05-27: every percentage emitted via Telegram,
/// UI, or REST response MUST go through this helper. Standard NSE
/// display convention.
///
/// # Examples
///
/// ```
/// use tickvault_common::price_precision::format_pct_2_decimals;
/// assert_eq!(format_pct_2_decimals(5.0), "+5.00%");
/// assert_eq!(format_pct_2_decimals(-0.81), "-0.81%");
/// assert_eq!(format_pct_2_decimals(0.0), "0.00%");
/// assert_eq!(format_pct_2_decimals(f64::NAN), "—");
/// ```
#[must_use]
pub fn format_pct_2_decimals(value: f64) -> String {
    if !value.is_finite() {
        return NON_FINITE_SENTINEL.to_string();
    }
    // Zero (including -0.0) renders without a sign per operator
    // convention. Use `format!("{:.2}", value)` to apply rounding first,
    // then check the rounded value — avoids "+0.00%" when the raw value
    // is a tiny negative that rounds to zero.
    let rounded = format!("{value:.2}");
    if rounded == "0.00" || rounded == "-0.00" {
        return "0.00%".to_string();
    }
    if value > 0.0 {
        format!("+{rounded}%")
    } else {
        format!("{rounded}%")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_round_to_2dp_truncates_long_fraction() {
        assert_eq!(format!("{}", round_to_2dp(0.101_428_362_297_927_51)), "0.1");
        assert_eq!(format!("{}", round_to_2dp(2.613_407_964_243_586_3)), "2.61");
        assert_eq!(format!("{}", round_to_2dp(-0.531_771_429_404_51)), "-0.53");
    }

    #[test]
    fn test_round_to_2dp_already_two_or_fewer() {
        assert_eq!(round_to_2dp(5.0), 5.0);
        assert_eq!(format!("{}", round_to_2dp(5.12)), "5.12");
    }

    #[test]
    fn test_round_to_2dp_normalises_negative_zero() {
        let r = round_to_2dp(-0.0001);
        assert_eq!(r, 0.0);
        assert!(r.is_sign_positive(), "must normalise -0.0 -> 0.0");
    }

    #[test]
    fn test_round_to_2dp_passes_through_non_finite() {
        assert!(round_to_2dp(f64::NAN).is_nan());
        assert!(round_to_2dp(f64::INFINITY).is_infinite());
    }

    #[test]
    fn test_f32_to_f64_clean_preserves_simple_prices() {
        // Operator-observed corruption: 23925.65_f32 was landing as
        // 23925.650390625_f64 in candles_1m via f64::from().
        assert_eq!(f32_to_f64_clean(23925.65_f32), 23925.65_f64);
        assert_eq!(f32_to_f64_clean(23_937.3_f32), 23937.30_f64);
        assert_eq!(f32_to_f64_clean(23_924.4_f32), 23924.40_f64);
        assert_eq!(f32_to_f64_clean(10.20_f32), 10.20_f64);
        assert_eq!(f32_to_f64_clean(21004.95_f32), 21004.95_f64);
    }

    /// Bit comparison that treats every NaN as equal to every NaN.
    fn same_bits(left: f64, right: f64) -> bool {
        left.to_bits() == right.to_bits() || (left.is_nan() && right.is_nan())
    }

    #[test]
    fn test_f32_to_f64_clean_matches_text_path_for_every_paise_price() {
        // Every price in paise from 0 to 2,00,000.00 rupees, both signs:
        // the values the feed actually carries.
        for paise in 0_u32..=20_000_000 {
            #[allow(clippy::cast_precision_loss)]
            // APPROVED: test input generation, paise < 2^25
            let price = paise as f32 / 100.0;
            for v in [price, -price] {
                let (fast, text) = (f32_to_f64_clean(v), f32_to_f64_clean_via_text(v));
                assert!(same_bits(fast, text), "{v}: {fast} != {text}");
            }
        }
    }

    #[test]
    fn test_f32_to_f64_clean_matches_text_path_on_a_stride_of_all_f32() {
        // Every 4,099th bit pattern (about one million inputs) across
        // the whole f32 range: subnormals, tiny and huge values, the
        // 2^24 boundary, both signs, infinities and NaNs.
        let mut bits: u64 = 0;
        while bits <= u64::from(u32::MAX) {
            #[allow(clippy::cast_possible_truncation)]
            // APPROVED: loop bound keeps bits within u32
            let v = f32::from_bits(bits as u32);
            let (fast, text) = (f32_to_f64_clean(v), f32_to_f64_clean_via_text(v));
            assert!(same_bits(fast, text), "{v:e}: {fast} != {text}");
            bits += 4_099;
        }
    }

    #[test]
    fn test_f32_to_f64_clean_boundary_values_match_text_path() {
        let boundaries = [
            ARITHMETIC_PATH_MAX_MAGNITUDE,
            f32::from_bits(ARITHMETIC_PATH_MAX_MAGNITUDE.to_bits() - 1),
            f32::from_bits(ARITHMETIC_PATH_MAX_MAGNITUDE.to_bits() + 1),
            123_456_792.0,
            f32::MIN_POSITIVE,
            f32::from_bits(1),
            f32::MAX,
            0.1,
            0.3,
            1e-9,
            -0.0,
        ];
        for v in boundaries {
            for v in [v, -v] {
                let (fast, text) = (f32_to_f64_clean(v), f32_to_f64_clean_via_text(v));
                assert!(same_bits(fast, text), "{v:e}: {fast} != {text}");
            }
        }
    }

    /// The full proof: all 2^32 inputs, split across the machine's
    /// threads. Measured 2026-10-09: 0 differences, 382 s on 4
    /// cores (release). Run with
    /// `cargo test -p tickvault-common --release --lib -- --ignored every_f32`.
    #[test]
    #[ignore = "exhaustive: minutes in release, far longer in debug"]
    fn test_f32_to_f64_clean_matches_text_path_for_every_f32() {
        let threads = std::thread::available_parallelism().map_or(4, std::num::NonZeroUsize::get);
        let threads = u64::try_from(threads).unwrap_or(4);
        let span = 1_u64 << 32;
        let handles: Vec<_> = (0..threads)
            .map(|t| {
                std::thread::spawn(move || {
                    let (low, high) = (span * t / threads, span * (t + 1) / threads);
                    let mut first_difference = None;
                    let mut differences = 0_u64;
                    for bits in low..high {
                        #[allow(clippy::cast_possible_truncation)]
                        // APPROVED: bits < 2^32
                        let v = f32::from_bits(bits as u32);
                        let (fast, text) = (f32_to_f64_clean(v), f32_to_f64_clean_via_text(v));
                        if !same_bits(fast, text) {
                            differences += 1;
                            first_difference.get_or_insert((v, fast, text));
                        }
                    }
                    (differences, first_difference)
                })
            })
            .collect();
        let mut differences = 0;
        let mut first = None;
        for handle in handles {
            let (count, example) = handle.join().expect("worker thread panicked");
            differences += count;
            first = first.or(example);
        }
        assert_eq!(differences, 0, "first difference: {first:?}");
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
    fn test_f32_to_f64_clean_round_trips_through_f32_display() {
        // The contract of f32_to_f64_clean is: the f64 output equals
        // the shortest decimal that round-trips through f32 — i.e.
        // `clean.to_string() == raw.to_string()`. This is the actual
        // guarantee Dhan callers depend on: whatever decimal the f32
        // parser produces (whether that's "23925.65" for a Dhan tick
        // or "0.45000002" for a synthetic test value), the f64 output
        // displays identically.
        //
        // Note: NOT every synthetic `(i as f32) * 0.05` value
        // round-trips cleanly because 0.05 isn't exactly representable
        // in f32 — but Dhan-sourced f32 values DO round-trip cleanly
        // because Dhan's wire format already snapped them to the
        // shortest-decimal form before transmission.
        for cents in 0_i32..1_000 {
            let raw = (cents as f32) * 0.05;
            let clean = f32_to_f64_clean(raw);
            assert_eq!(
                clean.to_string(),
                raw.to_string(),
                "f32_to_f64_clean({raw}) = {clean} broke shortest-decimal round-trip",
            );
        }
    }

    // ====================================================================
    // Sub-PR #4.5 — display-layer 2-decimal formatters (operator-locked
    // 2026-05-27): every Telegram message, every UI string, every REST
    // response that renders a PRICE or a PERCENTAGE must go through one
    // of the helpers below. Storage / RAM keeps full f64 precision; only
    // the operator-facing string boundary rounds to 2 decimals.
    // ====================================================================

    #[test]
    fn test_format_price_2_decimals_basic() {
        assert_eq!(format_price_2_decimals(2500.55), "2500.55");
        assert_eq!(format_price_2_decimals(10.20), "10.20");
        assert_eq!(format_price_2_decimals(0.05), "0.05");
        assert_eq!(format_price_2_decimals(100.0), "100.00");
    }

    #[test]
    fn test_format_price_2_decimals_rounds_extra_digits() {
        // Math results (e.g. weighted averages, P&L) can produce extra
        // digits — operator-facing display rounds to 2 decimals.
        assert_eq!(format_price_2_decimals(2520.5500488), "2520.55");
        assert_eq!(format_price_2_decimals(2520.5599), "2520.56");
        assert_eq!(format_price_2_decimals(2520.554), "2520.55");
        // 2520.555 in f64 is not exactly representable — defer to Rust's
        // built-in rounding (banker's, but the f64 input may already be
        // slightly off from 2520.555 exact). Just verify it produces a
        // valid 2-decimal string.
        let s = format_price_2_decimals(2520.555);
        assert!(s == "2520.55" || s == "2520.56", "got {s}");
    }

    #[test]
    fn test_format_price_2_decimals_negative() {
        // Greeks deltas, P&L drawdowns — must render with sign.
        assert_eq!(format_price_2_decimals(-150.55), "-150.55");
        assert_eq!(format_price_2_decimals(-0.05), "-0.05");
    }

    #[test]
    fn test_format_price_2_decimals_zero() {
        assert_eq!(format_price_2_decimals(0.0), "0.00");
        assert_eq!(format_price_2_decimals(-0.0), "-0.00");
    }

    #[test]
    fn test_format_price_2_decimals_non_finite_safe() {
        // Defensive — NaN / Inf should NOT propagate to operator-facing
        // strings as "NaN" / "inf" because that's confusing. Use a
        // sentinel that the operator can spot.
        assert_eq!(format_price_2_decimals(f64::NAN), "—");
        assert_eq!(format_price_2_decimals(f64::INFINITY), "—");
        assert_eq!(format_price_2_decimals(f64::NEG_INFINITY), "—");
    }

    #[test]
    fn test_format_pct_2_decimals_positive() {
        assert_eq!(format_pct_2_decimals(5.0), "+5.00%");
        assert_eq!(format_pct_2_decimals(4.166_666_666_666_667), "+4.17%");
        assert_eq!(format_pct_2_decimals(0.05), "+0.05%");
    }

    #[test]
    fn test_format_pct_2_decimals_negative() {
        // "+" sign omitted for negative — standard NSE display convention.
        assert_eq!(format_pct_2_decimals(-5.0), "-5.00%");
        assert_eq!(format_pct_2_decimals(-0.813_008_130_081_3), "-0.81%");
    }

    #[test]
    fn test_format_pct_2_decimals_zero_uses_neutral_sign() {
        // Zero gets neither + nor - — operator can immediately see
        // "no change" without the +0.00% confusion.
        assert_eq!(format_pct_2_decimals(0.0), "0.00%");
        assert_eq!(format_pct_2_decimals(-0.0), "0.00%");
    }

    #[test]
    fn test_format_pct_2_decimals_non_finite_safe() {
        // Pre-market / newly-listed instrument has no prev_close → pct
        // is None → caller passes f64::NAN. Render as em-dash, NOT
        // "NaN%".
        assert_eq!(format_pct_2_decimals(f64::NAN), "—");
        assert_eq!(format_pct_2_decimals(f64::INFINITY), "—");
    }

    #[test]
    fn test_format_pct_2_decimals_small_values_round_correctly() {
        // < 0.005% rounds to 0.00% — operator sees "essentially zero".
        // Tiny positive that rounds to 0.00 must show as "0.00%" (no
        // sign), NOT "+0.00%" — per zero-render convention.
        assert_eq!(format_pct_2_decimals(0.004), "0.00%");
        // 0.005 boundary is f64-representation-dependent; just verify
        // the output is a valid % string with 2 decimals.
        let s = format_pct_2_decimals(0.005);
        assert!(s == "+0.01%" || s == "0.00%", "got {s}");
    }
}
