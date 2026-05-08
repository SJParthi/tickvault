//! Wave-5 §K-L13 — pure primitives for stamping Bar's frozen-per-day
//! % fields at SEAL time.
//!
//! Ships the contract:
//!
//! - [`PrevDayRefs`] — Copy struct carrying the 3 prev-day baselines
//!   (`prev_day_close`, `prev_day_oi`, `prev_day_total_volume`).
//! - Pure compute functions ([`compute_close_pct`],
//!   [`compute_oi_pct`], [`compute_volume_pct`]) — handle div-by-zero
//!   without panic, return `0.0` when the baseline is zero.
//! - [`stamp_bar_pct_fields`] — mutates a `Bar` in place, populating
//!   `close_pct_from_prev_day`, `oi_pct_from_prev_day`, and
//!   `volume_pct_from_prev_day`.
//!
//! ## Integration
//!
//! The actual seal-site wiring (calling `stamp_bar_pct_fields` after
//! every sealed bar in `CandleEngineMap::on_tick` /
//! `CandleEngineMap::on_sealed_bar` returns) lands as a small
//! follow-up PR alongside the boot-time loader that populates a
//! [`crate::in_mem::PrevDayCache`] from the existing `previous_close`
//! cache (PR #466) + `prev_oi_loader` (PR #454). #504e ships the
//! contract + tests so that follow-up is mechanical wiring, not
//! design work.
//!
//! ## Div-by-zero policy
//!
//! When the baseline is `0.0` (or `0` for integer-typed fields):
//!
//! - For an instrument that has no prior trading history (newly
//!   listed contract, T+0 listing day), `prev_day_close` is `0.0`. We
//!   return `0.0` for the % so the field is not `NaN` (which would
//!   serialize as `null` in some JSON encoders and confuse readers).
//! - For non-derivative instruments (equities), `prev_day_oi` is `0`.
//!   Returning `0.0` for `oi_pct_from_prev_day` is correct — the
//!   field is not meaningful for that instrument class.
//!
//! Pinned by `test_compute_*_pct_handles_zero_baseline_without_nan`.

use crate::candles::engine::Bar;

/// Frozen-per-day reference values for one `(security_id,
/// exchange_segment)` instrument. Copy so the lookup hot path is
/// alloc-free.
///
/// **Sources** (per L12 + plan):
/// - `prev_day_close` ← `previous_close` cache (PR #466)
/// - `prev_day_oi` ← `prev_oi_loader` cache (PR #454)
/// - `prev_day_total_volume` ← `previous_close` cache (same PR)
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PrevDayRefs {
    /// Previous trading day's close price (rupees). `0.0` if the
    /// instrument has no prior trading history.
    pub prev_day_close: f64,
    /// Previous trading day's last open interest. `0` for non-F&O.
    pub prev_day_oi: i64,
    /// Previous trading day's total cumulative volume. `0` if no
    /// prior trading.
    pub prev_day_total_volume: i64,
}

impl Default for PrevDayRefs {
    fn default() -> Self {
        Self {
            prev_day_close: 0.0,
            prev_day_oi: 0,
            prev_day_total_volume: 0,
        }
    }
}

/// Compute close-vs-prev-day % change.
///
/// Formula: `((close - prev_day_close) / prev_day_close) * 100`.
///
/// Returns `0.0` if `prev_day_close == 0.0` (newly-listed instrument
/// with no history) — never NaN.
#[must_use]
#[inline]
pub fn compute_close_pct(close: f64, prev_day_close: f64) -> f64 {
    if prev_day_close == 0.0 {
        0.0
    } else {
        ((close - prev_day_close) / prev_day_close) * 100.0
    }
}

/// Compute OI-vs-prev-day % change.
///
/// Formula: `((oi - prev_day_oi) / prev_day_oi) * 100`.
///
/// Returns `0.0` if `prev_day_oi == 0` (non-F&O instrument or first
/// listing day) — never NaN. The integer arithmetic is widened to
/// `f64` before division to preserve fractional precision.
#[must_use]
#[inline]
pub fn compute_oi_pct(oi: i64, prev_day_oi: i64) -> f64 {
    if prev_day_oi == 0 {
        0.0
    } else {
        // APPROVED: integer-to-f64 widening is precise for the OI
        // value range (typical OI < 1e10, well within f64's 2^53
        // mantissa). NOT subject to the f32-price IEEE-754 widening
        // bug that data-integrity.md guards against.
        let oi_f = oi as f64;
        let prev_f = prev_day_oi as f64;
        ((oi_f - prev_f) / prev_f) * 100.0
    }
}

/// Compute cumulative-volume-vs-prev-day % change.
///
/// Formula: `((volume_cum_day_at_end - prev_day_total_volume) /
/// prev_day_total_volume) * 100`.
///
/// Returns `0.0` if `prev_day_total_volume == 0` — never NaN. Uses
/// the same i64-to-f64 widening as `compute_oi_pct`.
#[must_use]
#[inline]
pub fn compute_volume_pct(cum_volume: i64, prev_day_total_volume: i64) -> f64 {
    if prev_day_total_volume == 0 {
        0.0
    } else {
        // APPROVED: see `compute_oi_pct` for widening rationale.
        let cum_f = cum_volume as f64;
        let prev_f = prev_day_total_volume as f64;
        ((cum_f - prev_f) / prev_f) * 100.0
    }
}

/// Stamp the 3 % fields on a sealed Bar using the supplied prev-day
/// reference values. Mutates the bar in place.
///
/// Idempotent: calling twice with the same `refs` produces the same
/// fields (the formula is deterministic). Calling with different
/// `refs` overwrites the previous stamping.
///
/// **Hot-path cost**: 3 floating-point divisions + 3 multiplications
/// + 3 stores = ~15 ns. Called on the SEAL path (not per-tick), so
/// the 200 ns/tick budget is unaffected.
#[inline]
pub fn stamp_bar_pct_fields(bar: &mut Bar, refs: PrevDayRefs) {
    bar.prev_day_close = refs.prev_day_close;
    bar.prev_day_oi = refs.prev_day_oi;
    bar.close_pct_from_prev_day = compute_close_pct(bar.close, refs.prev_day_close);
    bar.oi_pct_from_prev_day = compute_oi_pct(bar.oi, refs.prev_day_oi);
    bar.volume_pct_from_prev_day =
        compute_volume_pct(bar.volume_cum_day_at_end, refs.prev_day_total_volume);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_bar(close: f64, oi: i64, cum_vol: i64) -> Bar {
        Bar {
            bucket_start_ist_secs: 0,
            bucket_end_ist_secs: 60,
            open: close,
            high: close,
            low: close,
            close,
            volume: 0,
            volume_cum_day_at_end: cum_vol,
            oi,
            tick_count: 1,
            security_id: 1234,
            exchange_segment_code: 1,
            sealed: true,
            prev_day_close: 0.0,
            prev_day_oi: 0,
            close_pct_from_prev_day: 0.0,
            oi_pct_from_prev_day: 0.0,
            volume_pct_from_prev_day: 0.0,
        }
    }

    #[test]
    fn test_compute_close_pct_happy_path() {
        // 100 → 105 = +5%
        assert_eq!(compute_close_pct(105.0, 100.0), 5.0);
        // 100 → 95 = -5%
        assert_eq!(compute_close_pct(95.0, 100.0), -5.0);
        // No change
        assert_eq!(compute_close_pct(100.0, 100.0), 0.0);
    }

    #[test]
    fn test_compute_close_pct_handles_zero_baseline_without_nan() {
        // L13 div-by-zero policy: 0 baseline returns 0.0 (not NaN).
        let result = compute_close_pct(123.45, 0.0);
        assert_eq!(result, 0.0);
        assert!(
            !result.is_nan(),
            "MUST NOT return NaN — JSON serialization breaks"
        );
    }

    #[test]
    fn test_compute_oi_pct_happy_path() {
        // 1_000_000 → 1_500_000 = +50%
        assert_eq!(compute_oi_pct(1_500_000, 1_000_000), 50.0);
        // 1_000_000 → 500_000 = -50%
        assert_eq!(compute_oi_pct(500_000, 1_000_000), -50.0);
    }

    #[test]
    fn test_compute_oi_pct_handles_zero_baseline_without_nan() {
        let result = compute_oi_pct(1_000_000, 0);
        assert_eq!(result, 0.0);
        assert!(!result.is_nan());
    }

    #[test]
    fn test_compute_volume_pct_happy_path() {
        // 10M → 25M = +150%
        assert_eq!(compute_volume_pct(25_000_000, 10_000_000), 150.0);
    }

    #[test]
    fn test_compute_volume_pct_handles_zero_baseline_without_nan() {
        let result = compute_volume_pct(123_456, 0);
        assert_eq!(result, 0.0);
        assert!(!result.is_nan());
    }

    #[test]
    fn test_stamp_bar_pct_fields_populates_all_five_fields() {
        // Verify the stamping function writes all 5 fields per L12.
        let mut bar = empty_bar(105.0, 1_500_000, 25_000_000);
        let refs = PrevDayRefs {
            prev_day_close: 100.0,
            prev_day_oi: 1_000_000,
            prev_day_total_volume: 10_000_000,
        };
        stamp_bar_pct_fields(&mut bar, refs);
        assert_eq!(bar.prev_day_close, 100.0);
        assert_eq!(bar.prev_day_oi, 1_000_000);
        assert_eq!(bar.close_pct_from_prev_day, 5.0);
        assert_eq!(bar.oi_pct_from_prev_day, 50.0);
        assert_eq!(bar.volume_pct_from_prev_day, 150.0);
    }

    #[test]
    fn test_stamp_is_idempotent() {
        // L13: idempotent stamping — same refs, same output.
        let mut bar = empty_bar(105.0, 1_500_000, 25_000_000);
        let refs = PrevDayRefs {
            prev_day_close: 100.0,
            prev_day_oi: 1_000_000,
            prev_day_total_volume: 10_000_000,
        };
        stamp_bar_pct_fields(&mut bar, refs);
        let snapshot = bar;
        stamp_bar_pct_fields(&mut bar, refs);
        assert_eq!(bar, snapshot);
    }

    #[test]
    fn test_stamp_with_default_refs_zeroes_pct_fields() {
        // Edge case: PrevDayRefs::default() (all zeros) → all % fields
        // become 0.0, prev_day_close + prev_day_oi stamped as 0.
        // Mirrors #504b's "ships fields with 0.0/0 default" semantics.
        let mut bar = empty_bar(123.45, 999, 99_999);
        // Pre-set the fields to non-zero so we can verify they get reset.
        bar.close_pct_from_prev_day = 99.0;
        stamp_bar_pct_fields(&mut bar, PrevDayRefs::default());
        assert_eq!(bar.prev_day_close, 0.0);
        assert_eq!(bar.prev_day_oi, 0);
        assert_eq!(bar.close_pct_from_prev_day, 0.0);
        assert_eq!(bar.oi_pct_from_prev_day, 0.0);
        assert_eq!(bar.volume_pct_from_prev_day, 0.0);
    }

    #[test]
    fn test_prev_day_refs_default_is_all_zero() {
        let refs = PrevDayRefs::default();
        assert_eq!(refs.prev_day_close, 0.0);
        assert_eq!(refs.prev_day_oi, 0);
        assert_eq!(refs.prev_day_total_volume, 0);
    }

    #[test]
    fn test_stamp_bar_pct_fields_preserves_unrelated_bar_state() {
        // Stamping must NOT touch open / high / low / close / volume
        // / tick_count / security_id / etc. — only the 5 % fields.
        let mut bar = empty_bar(105.0, 1_500_000, 25_000_000);
        bar.open = 99.0;
        bar.high = 110.0;
        bar.low = 90.0;
        bar.tick_count = 42;
        let refs = PrevDayRefs {
            prev_day_close: 100.0,
            prev_day_oi: 1_000_000,
            prev_day_total_volume: 10_000_000,
        };
        stamp_bar_pct_fields(&mut bar, refs);
        // Unchanged:
        assert_eq!(bar.open, 99.0);
        assert_eq!(bar.high, 110.0);
        assert_eq!(bar.low, 90.0);
        assert_eq!(bar.close, 105.0);
        assert_eq!(bar.tick_count, 42);
        assert_eq!(bar.security_id, 1234);
    }

    #[test]
    fn test_negative_pct_change_works() {
        // A bar that closes BELOW the prev day close → negative %.
        let mut bar = empty_bar(80.0, 0, 0);
        let refs = PrevDayRefs {
            prev_day_close: 100.0,
            prev_day_oi: 0,
            prev_day_total_volume: 0,
        };
        stamp_bar_pct_fields(&mut bar, refs);
        assert_eq!(bar.close_pct_from_prev_day, -20.0);
    }

    #[test]
    fn test_compute_close_pct_handles_negative_baseline_without_panic() {
        // Negative prev_close shouldn't happen in production but the
        // function must not panic on it.
        let result = compute_close_pct(50.0, -100.0);
        assert!(result.is_finite());
    }
}
