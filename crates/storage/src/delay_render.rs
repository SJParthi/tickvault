//! The human-readable delay renderer shared by the candle writer.
//!
//! **Relocated 2026-10-22 (Quote 34, plan item 62).** It lived in
//! `top_volume_rank_persistence`, whose writer rendered the same delay
//! columns. That module was deleted with the four `top_volume_<tf>` tables;
//! the shadow candle writer still renders its delay columns through this
//! function, so it moved here unchanged (code and tests verbatim).

/// The four bands of the delay renderer, in nanoseconds.
///
/// Each band stops SHORT of the round number above it so a value never renders
/// as `1000 microseconds` when `1 millisecond` is the true reading: the
/// microsecond band ends at 999,499 ns (which rounds to 999 µs) and the
/// millisecond band begins at 999,500 ns (which rounds to 1 ms).
const DELAY_MICRO_FLOOR_NANOS: i64 = 1_000;
const DELAY_MILLI_FLOOR_NANOS: i64 = 999_500;
const DELAY_SECOND_FLOOR_NANOS: i64 = 999_500_000;

/// Renders a nanosecond delay as whole units of ONE band, into `out`.
///
/// # The contract (operator 2026-09-18, §19 §4)
///
/// > "if it is below microseconds I need to know it took 100 or 1000
/// > microseconds, or if it is in milliseconds then the data should be like
/// > this 100 or 200 or 123 milliseconds, or if it is in seconds then I want to
/// > see this as 1 second or 2 seconds"
///
/// Whole units, never a decimal point, and one band per value:
///
/// | nanoseconds | renders as |
/// |---|---|
/// | `< 1,000` | whole **nanoseconds** — `4 nanoseconds` |
/// | `1,000 .. 999,499` | whole **microseconds** — `100 microseconds` |
/// | `999,500 .. 999,499,999` | whole **milliseconds** — `123 milliseconds` |
/// | `>= 999,500,000` | whole **seconds** — `1 second` |
///
/// Rounding is to NEAREST within the band, and singular/plural follows the
/// number (`1 second`, `2 seconds`).
///
/// # Why this is lossy on purpose, and why the `_ns` twin exists
///
/// `1 second` covers 999,500,000 .. 1,499,999,999 ns. That loss is the
/// operator's own instruction and is confined to the SENTENCE: every delay is
/// stored twice, and the `_ns` column always carries the exact signed figure.
/// Nothing is lost from the table — only from the phrase.
///
/// **And the `_ns` twin is not a convenience.** Text sorted descending
/// compares the first character and stops, so four real delays — 1 second,
/// 2 milliseconds, 3 microseconds, 4 nanoseconds — sort to `4, 3, 2, 1`: the
/// exact REVERSE of their true order, and it looks entirely plausible. Any
/// `ORDER BY` must use the `_ns` column; the rendered string is for a human to
/// read, never for a query to sort.
///
/// # Negative delays are real and are rendered with a sign
///
/// The drain back-dates `received_at_nanos` by ring dwell
/// (`Utc::now() - frame.received_at.elapsed()`), so a tick can carry a receipt
/// instant BEFORE the window it lands in. `unsigned_abs` takes the magnitude —
/// never `abs()`, which panics on `i64::MIN` under the release profile's
/// `overflow-checks = true` — and the sign is carried as a leading `-` so the
/// sentence cannot silently claim a negative delay was positive.
///
/// # Allocation and cost
///
/// Writes into a caller-owned buffer, which the caller CLEARS first. The naive
/// `format!` shape would be three fresh `String`s per row — at the measured
/// 20,220-contract ceiling that is 60,660 allocations per sweep, on the frame
/// drain, in a codebase whose first principle is zero allocation on the hot
/// path. One reused buffer is the whole difference.
///
/// The digits are written by hand into a 20-byte stack array (2026-10-30,
/// owner directive: per-seal work under 1 µs at p99): the earlier
/// `write!(out, "{value} {unit}")` went through the `fmt` machinery for every
/// one of the three delay columns of every sealed bar. The text is byte for
/// byte what that `write!` produced, pinned against a copy of the old body by
/// `proptest_render_delay_into_matches_the_fmt_version_byte_for_byte`.
/// O(digits) (at most 20), no allocation once `out` holds a short string.
pub fn render_delay_into(out: &mut String, nanos: i64) {
    out.clear();
    let magnitude = nanos.unsigned_abs();
    if nanos < 0 {
        out.push('-');
    }
    // Every band divides by its own unit with round-half-up, which is what
    // makes 999,500 ns read as `1 millisecond` rather than `0 milliseconds`.
    let (value, unit) = if magnitude < DELAY_MICRO_FLOOR_NANOS.unsigned_abs() {
        (magnitude, "nanosecond")
    } else if magnitude < DELAY_MILLI_FLOOR_NANOS.unsigned_abs() {
        ((magnitude + 500) / 1_000, "microsecond")
    } else if magnitude < DELAY_SECOND_FLOOR_NANOS.unsigned_abs() {
        ((magnitude + 500_000) / 1_000_000, "millisecond")
    } else {
        ((magnitude + 500_000_000) / 1_000_000_000, "second")
    };
    push_decimal(out, value);
    out.push(' ');
    out.push_str(unit);
    if value != 1 {
        out.push('s');
    }
}

/// Appends `value` in base 10, as `{value}` would. `u64::MAX` has 20 digits.
fn push_decimal(out: &mut String, mut value: u64) {
    let mut digits = [0u8; 20];
    let mut start = digits.len();
    loop {
        start -= 1;
        // `value % 10` is below 10, so the cast cannot truncate.
        digits[start] = b'0' + (value % 10) as u8;
        value /= 10;
        if value == 0 {
            break;
        }
    }
    for &digit in &digits[start..] {
        out.push(char::from(digit));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every band boundary, from BOTH sides, plus the two rounding hinges.
    ///
    /// The hinges are the point: 999,499 ns must read `999 microseconds` and
    /// 999,500 ns must read `1 millisecond`. A band that ended on the round
    /// number instead would render the second one as `1000 microseconds`,
    /// which is exactly the shape the operator's instruction rules out.
    #[test]
    fn render_delay_into_uses_whole_units_and_never_the_round_number_above_a_band() {
        let mut out = String::new();
        for (nanos, expected) in [
            (0_i64, "0 nanoseconds"),
            (1, "1 nanosecond"),
            (4, "4 nanoseconds"),
            (999, "999 nanoseconds"),
            (1_000, "1 microsecond"),
            (100_000, "100 microseconds"),
            (999_499, "999 microseconds"),
            (999_500, "1 millisecond"),
            (123_000_000, "123 milliseconds"),
            (999_499_999, "999 milliseconds"),
            (999_500_000, "1 second"),
            (2_000_000_000, "2 seconds"),
            (60_000_000_000, "60 seconds"),
        ] {
            render_delay_into(&mut out, nanos);
            assert_eq!(out, expected, "{nanos} ns");
        }
    }

    /// A negative delay renders with its sign and its magnitude, and does not
    /// panic on `i64::MIN`.
    ///
    /// Both halves matter. The drain back-dates a receipt by ring dwell, so a
    /// tick can legitimately carry an instant a hair before the boundary it
    /// lands in — a clamp to zero would hide that. And `abs()` on `i64::MIN`
    /// PANICS under this workspace's release profile (`overflow-checks = true`),
    /// on the frame-drain task, so the renderer uses `unsigned_abs`.
    #[test]
    fn a_negative_delay_keeps_its_sign_and_i64_min_does_not_panic() {
        let mut out = String::new();
        render_delay_into(&mut out, -1_500_000);
        assert_eq!(out, "-2 milliseconds");
        render_delay_into(&mut out, -1);
        assert_eq!(out, "-1 nanosecond");
        // The whole point: this line panics if anyone swaps in `abs()`.
        render_delay_into(&mut out, i64::MIN);
        assert!(out.starts_with('-'), "sign lost on i64::MIN: {out}");
        assert!(out.ends_with(" seconds"), "band wrong on i64::MIN: {out}");
    }

    /// The buffer is REUSED, so a longer previous value must never bleed into
    /// a shorter next one.
    ///
    /// `render_delay_into` clears first. Without that, rendering `999
    /// milliseconds` and then `1 second` would produce `1 secondiseconds`.
    #[test]
    fn the_reused_buffer_is_cleared_so_a_short_value_cannot_inherit_a_long_one() {
        let mut out = String::new();
        render_delay_into(&mut out, 999_499_999);
        assert_eq!(out, "999 milliseconds");
        render_delay_into(&mut out, 1);
        assert_eq!(out, "1 nanosecond");
    }

    /// THE reason the `_ns` twin exists, demonstrated rather than asserted in
    /// a comment.
    ///
    /// Four real delays, an order of magnitude apart each time. Sorted as TEXT
    /// they come out in the EXACT REVERSE of their true order, and the result
    /// looks entirely plausible — which is what makes it dangerous. Sorted on
    /// the nanosecond twin they come out right. Any `ORDER BY` must use the
    /// `_ns` column; this test fails if someone ever decides one column was
    /// enough.
    #[test]
    fn sorting_the_readable_column_reverses_the_true_order_which_is_why_the_ns_twin_exists() {
        let mut out = String::new();
        let mut rendered: Vec<(i64, String)> = Vec::new();
        for nanos in [1_000_000_000_i64, 2_000_000, 3_000, 4] {
            render_delay_into(&mut out, nanos);
            rendered.push((nanos, out.clone()));
        }

        let mut by_text = rendered.clone();
        by_text.sort_by(|a, b| b.1.cmp(&a.1));
        assert_eq!(
            by_text.iter().map(|(n, _)| *n).collect::<Vec<_>>(),
            vec![4, 3_000, 2_000_000, 1_000_000_000],
            "text descending should be wrong — if this now matches the true \
             order the fixture stopped demonstrating the hazard"
        );

        let mut by_nanos = rendered;
        by_nanos.sort_unstable_by_key(|(n, _)| std::cmp::Reverse(*n));
        assert_eq!(
            by_nanos.iter().map(|(n, _)| *n).collect::<Vec<_>>(),
            vec![1_000_000_000, 2_000_000, 3_000, 4],
        );
    }

    /// The body `render_delay_into` had until 2026-10-30, kept verbatim as the
    /// reference the hand-written digits are checked against.
    fn render_delay_into_fmt_reference(out: &mut String, nanos: i64) {
        use std::fmt::Write as _;

        out.clear();
        let magnitude = nanos.unsigned_abs();
        if nanos < 0 {
            out.push('-');
        }
        let (value, unit) = if magnitude < DELAY_MICRO_FLOOR_NANOS.unsigned_abs() {
            (magnitude, "nanosecond")
        } else if magnitude < DELAY_MILLI_FLOOR_NANOS.unsigned_abs() {
            ((magnitude + 500) / 1_000, "microsecond")
        } else if magnitude < DELAY_SECOND_FLOOR_NANOS.unsigned_abs() {
            ((magnitude + 500_000) / 1_000_000, "millisecond")
        } else {
            ((magnitude + 500_000_000) / 1_000_000_000, "second")
        };
        _ = write!(out, "{value} {unit}");
        if value != 1 {
            out.push('s');
        }
    }

    #[test]
    fn test_render_delay_into_matches_the_fmt_version_at_every_edge() {
        let (mut new, mut old) = (String::new(), String::new());
        let mut edges = vec![i64::MIN, i64::MIN + 1, i64::MAX, i64::MAX - 1, 0];
        for base in [
            1_i64,
            9,
            10,
            99,
            100,
            999,
            1_000,
            999_499,
            999_500,
            999_499_999,
            999_500_000,
            1_499_999_999,
            1_500_000_000,
            9_999_999_999,
            10_000_000_000,
        ] {
            for d in -1..=1 {
                edges.push(base + d);
                edges.push(-(base + d));
            }
        }
        for nanos in edges {
            render_delay_into(&mut new, nanos);
            render_delay_into_fmt_reference(&mut old, nanos);
            assert_eq!(new, old, "{nanos} ns");
        }
    }

    #[test]
    fn test_push_decimal_writes_every_digit_count_up_to_u64_max() {
        let mut out = String::new();
        let mut value = 0_u64;
        for digits in 1..=19 {
            value = if digits == 1 { 7 } else { value * 10 + 3 };
            out.clear();
            push_decimal(&mut out, value);
            assert_eq!(out, value.to_string());
        }
        out.clear();
        push_decimal(&mut out, u64::MAX);
        assert_eq!(out, u64::MAX.to_string());
    }

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig::with_cases(4_096))]
        #[test]
        fn proptest_render_delay_into_matches_the_fmt_version_byte_for_byte(nanos in proptest::prelude::any::<i64>()) {
            let (mut new, mut old) = (String::from("stale text"), String::new());
            render_delay_into(&mut new, nanos);
            render_delay_into_fmt_reference(&mut old, nanos);
            proptest::prop_assert_eq!(new, old);
        }

        #[test]
        fn proptest_render_delay_into_matches_in_the_realistic_range(nanos in -300_000_000_000_i64..300_000_000_000) {
            let (mut new, mut old) = (String::new(), String::new());
            render_delay_into(&mut new, nanos);
            render_delay_into_fmt_reference(&mut old, nanos);
            proptest::prop_assert_eq!(new, old);
        }
    }
}
