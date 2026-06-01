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
//! - [`stamp_seal_pct_fields`] — mutates a [`LiveCandleState`] in
//!   place, populating the 3 pct fields at seal time.
//!
//! ## Integration
//!
//! `stamp_seal_pct_fields` is called on Engine B's seal path (in the
//! `spawn_engine_b_aggregator` helper in `crates/app/src/main.rs`)
//! after every sealed [`LiveCandleState`], using prev-day refs looked
//! up from a [`crate::in_mem::PrevDayCache`].
//!
//! Candle-engine re-architecture #T1b: the legacy `stamp_bar_pct_fields`
//! (which mutated the Engine-A/C `Bar` type) was DELETED alongside
//! Engine C — Engine B's `LiveCandleState` is the only candle state.
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

use crate::candles::aggregator_cell::LiveCandleState;

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
        // 2-decimal contract (operator rule 2026-05-29): persisted % columns
        // carry <=2 digits after the point. Round via the canonical primitive.
        tickvault_common::price_precision::round_to_2dp(
            ((close - prev_day_close) / prev_day_close) * 100.0,
        )
    }
}

/// Compute close-vs-session-open % change (operator lock 2026-06-01 §31,
/// Option 2 — the NSE official 09:15 open from the Quote `day_open` field).
///
/// Formula: `((close - session_open) / session_open) * 100`.
///
/// Returns `0.0` if `session_open == 0.0` (pre-open / blank) — never NaN.
#[must_use]
#[inline]
pub fn compute_open_pct(close: f64, session_open: f64) -> f64 {
    if session_open == 0.0 {
        0.0
    } else {
        // 2-decimal contract (operator rule 2026-05-29).
        tickvault_common::price_precision::round_to_2dp(
            ((close - session_open) / session_open) * 100.0,
        )
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
        // 2-decimal contract (operator rule 2026-05-29).
        tickvault_common::price_precision::round_to_2dp(((oi_f - prev_f) / prev_f) * 100.0)
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
        // 2-decimal contract (operator rule 2026-05-29).
        tickvault_common::price_precision::round_to_2dp(((cum_f - prev_f) / prev_f) * 100.0)
    }
}

/// Wave 6 Sub-PR #1 item 1.5 — stamp the 3 % fields on a sealed
/// [`LiveCandleState`] using the supplied prev-day refs. The
/// [`LiveCandleState`] type does NOT carry `prev_day_close` /
/// `prev_day_oi` baseline fields (only `Bar` does); only the 3
/// pct fields get populated.
///
/// Volume-pct uses **cumulative-at-bucket-end** = `bucket_start_cumulative
/// + volume`. The `u64 → i64` cast saturates at `i64::MAX` to defend
/// against rogue upstream values that could otherwise wrap negative
/// (mirrors the saturating cast in
/// `tickvault_storage::shadow_seal_columns::ShadowSealRow::from_buffered_seal`).
///
/// **Hot-path cost**: 1 add + 1 saturating cast + 3 fp divisions + 3
/// fp multiplications + 3 stores = ~20 ns. Called on the SEAL path
/// (not per-tick), so the per-tick budget is unaffected.
///
/// Idempotent — calling twice with the same `refs` produces the same
/// fields.
#[inline]
pub fn stamp_seal_pct_fields(state: &mut LiveCandleState, refs: PrevDayRefs) {
    let cum_volume_at_end_i64 =
        i64::try_from(state.bucket_start_cumulative.saturating_add(state.volume))
            .unwrap_or(i64::MAX);
    state.close_pct_from_prev_day = compute_close_pct(state.close, refs.prev_day_close);
    state.oi_pct_from_prev_day = compute_oi_pct(state.oi, refs.prev_day_oi);
    state.volume_pct_from_prev_day =
        compute_volume_pct(cum_volume_at_end_i64, refs.prev_day_total_volume);
    // Operator lock 2026-06-01 §31 (Option 2): % vs the official 09:15
    // session open, captured live on `state.session_open` from the Quote
    // `day_open` field (no refs / cache needed — it's on the state).
    state.open_pct = compute_open_pct(state.close, state.session_open);
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn test_compute_open_pct_vs_session_open() {
        // §31 Option 2: % change vs the official 09:15 open.
        // Open 23400, close 23540 → +0.6%.
        assert_eq!(compute_open_pct(23_540.0, 23_400.0), 0.6);
        // Below open → negative.
        assert_eq!(compute_open_pct(99.0, 100.0), -1.0);
        // At open → 0.
        assert_eq!(compute_open_pct(100.0, 100.0), 0.0);
        // Div-by-zero guard (pre-open / blank session_open) → 0.0, never NaN.
        let z = compute_open_pct(123.45, 0.0);
        assert_eq!(z, 0.0);
        assert!(!z.is_nan());
    }

    #[test]
    fn test_stamp_seal_sets_open_pct_from_session_open() {
        let mut state = LiveCandleState::empty();
        state.bucket_start_ist_secs = 1_716_000_900;
        state.session_open = 100.0;
        state.close = 102.5;
        stamp_seal_pct_fields(&mut state, PrevDayRefs::default());
        assert_eq!(state.open_pct, 2.5, "close 102.5 vs open 100 = +2.5%");
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
    fn test_prev_day_refs_default_is_all_zero() {
        let refs = PrevDayRefs::default();
        assert_eq!(refs.prev_day_close, 0.0);
        assert_eq!(refs.prev_day_oi, 0);
        assert_eq!(refs.prev_day_total_volume, 0);
    }

    #[test]
    fn test_compute_close_pct_handles_negative_baseline_without_panic() {
        // Negative prev_close shouldn't happen in production but the
        // function must not panic on it.
        let result = compute_close_pct(50.0, -100.0);
        assert!(result.is_finite());
    }

    // -----------------------------------------------------------------------
    // Wave 6 Sub-PR #1 item 1.5 — stamp_seal_pct_fields tests
    // -----------------------------------------------------------------------

    fn mk_live_candle_state(
        close: f64,
        oi: i64,
        volume: u64,
        cum_at_start: u64,
    ) -> LiveCandleState {
        let mut s = LiveCandleState::empty();
        s.bucket_start_ist_secs = 1_716_000_900;
        s.open = close;
        s.high = close;
        s.low = close;
        s.close = close;
        s.volume = volume;
        s.bucket_start_cumulative = cum_at_start;
        s.oi = oi;
        s.tick_count = 1;
        s
    }

    // Operator 2-decimal contract (2026-05-29): close_pct must never
    // persist more than 2 digits after the point. Before the fix this
    // produced values like 0.10534002950099992 in QuestDB.
    #[test]
    fn test_compute_close_pct_rounds_to_two_decimals() {
        // 23932.75 vs 23908.5 → 0.1014...% → rounds to 0.10
        let pct = compute_close_pct(23932.75, 23908.5);
        assert_eq!(pct, 0.10);
        // a value that rounds up
        let pct_up = compute_close_pct(29662.15, 28907.0);
        assert_eq!(pct_up, 2.61);
        // verify no long fraction survives
        assert_eq!((pct_up * 100.0).fract(), 0.0);
    }

    #[test]
    fn test_stamp_seal_pct_fields_happy_path() {
        let mut state = mk_live_candle_state(105.0, 1_500_000, 5_000_000, 20_000_000);
        // cum_at_end = 20M + 5M = 25M
        let refs = PrevDayRefs {
            prev_day_close: 100.0,
            prev_day_oi: 1_000_000,
            prev_day_total_volume: 10_000_000,
        };
        stamp_seal_pct_fields(&mut state, refs);
        assert_eq!(state.close_pct_from_prev_day, 5.0);
        assert_eq!(state.oi_pct_from_prev_day, 50.0);
        assert_eq!(state.volume_pct_from_prev_day, 150.0);
    }

    #[test]
    fn test_stamp_seal_pct_fields_zero_baseline_yields_zero_no_nan() {
        // L13 div-by-zero policy applies via the underlying compute_* fns.
        let mut state = mk_live_candle_state(123.45, 999, 99_999, 0);
        let refs = PrevDayRefs::default();
        stamp_seal_pct_fields(&mut state, refs);
        assert_eq!(state.close_pct_from_prev_day, 0.0);
        assert_eq!(state.oi_pct_from_prev_day, 0.0);
        assert_eq!(state.volume_pct_from_prev_day, 0.0);
        assert!(!state.close_pct_from_prev_day.is_nan());
        assert!(!state.oi_pct_from_prev_day.is_nan());
        assert!(!state.volume_pct_from_prev_day.is_nan());
    }

    #[test]
    fn test_stamp_seal_pct_fields_negative_red_day() {
        let mut state = mk_live_candle_state(80.0, 500_000, 1_000_000, 4_000_000);
        // cum_at_end = 5M
        let refs = PrevDayRefs {
            prev_day_close: 100.0,
            prev_day_oi: 1_000_000,
            prev_day_total_volume: 10_000_000,
        };
        stamp_seal_pct_fields(&mut state, refs);
        assert_eq!(state.close_pct_from_prev_day, -20.0);
        assert_eq!(state.oi_pct_from_prev_day, -50.0);
        assert_eq!(state.volume_pct_from_prev_day, -50.0);
    }

    #[test]
    fn test_stamp_seal_pct_fields_is_idempotent() {
        let mut state = mk_live_candle_state(105.0, 1_500_000, 5_000_000, 20_000_000);
        let refs = PrevDayRefs {
            prev_day_close: 100.0,
            prev_day_oi: 1_000_000,
            prev_day_total_volume: 10_000_000,
        };
        stamp_seal_pct_fields(&mut state, refs);
        let snapshot = state;
        stamp_seal_pct_fields(&mut state, refs);
        assert_eq!(state, snapshot);
    }

    #[test]
    fn test_stamp_seal_pct_fields_preserves_unrelated_state() {
        // Stamping must NOT touch open / high / low / close / volume / oi
        // / tick_count / bucket_start_ist_secs — only the 3 pct fields.
        let mut state = mk_live_candle_state(105.0, 1_500_000, 5_000_000, 20_000_000);
        state.open = 99.0;
        state.high = 110.0;
        state.low = 90.0;
        state.tick_count = 42;
        let refs = PrevDayRefs {
            prev_day_close: 100.0,
            prev_day_oi: 1_000_000,
            prev_day_total_volume: 10_000_000,
        };
        stamp_seal_pct_fields(&mut state, refs);
        assert_eq!(state.open, 99.0);
        assert_eq!(state.high, 110.0);
        assert_eq!(state.low, 90.0);
        assert_eq!(state.close, 105.0);
        assert_eq!(state.volume, 5_000_000);
        assert_eq!(state.bucket_start_cumulative, 20_000_000);
        assert_eq!(state.oi, 1_500_000);
        assert_eq!(state.tick_count, 42);
        assert_eq!(state.bucket_start_ist_secs, 1_716_000_900);
    }

    #[test]
    fn test_stamp_seal_pct_fields_uses_cum_at_end_for_volume_pct() {
        // The volume_pct must be computed from bucket_start_cumulative
        // + volume (cum-at-end), NOT just `volume` (incremental). A
        // common bug class is to use the incremental-only value, which
        // would massively under-report % vs prev day.
        let mut state = mk_live_candle_state(100.0, 0, 1_000_000, 9_000_000);
        // cum_at_end = 10M; prev_day_total_volume = 5M → +100%
        let refs = PrevDayRefs {
            prev_day_close: 0.0,
            prev_day_oi: 0,
            prev_day_total_volume: 5_000_000,
        };
        stamp_seal_pct_fields(&mut state, refs);
        // If the bug existed (only `volume` used), result would be
        // ((1M - 5M) / 5M) * 100 = -80% — wrong!
        // Correct: ((10M - 5M) / 5M) * 100 = +100%.
        assert_eq!(state.volume_pct_from_prev_day, 100.0);
    }

    #[test]
    fn test_stamp_seal_pct_fields_volume_saturates_at_i64_max_safely() {
        // Defensive: u64::MAX volumes saturate to i64::MAX rather than
        // wrapping to a negative i64 (which would give a nonsense pct).
        let mut state = mk_live_candle_state(100.0, 0, u64::MAX, u64::MAX);
        let refs = PrevDayRefs {
            prev_day_close: 0.0,
            prev_day_oi: 0,
            prev_day_total_volume: 1,
        };
        stamp_seal_pct_fields(&mut state, refs);
        // Result must be finite + positive (saturated, not wrapped).
        assert!(state.volume_pct_from_prev_day.is_finite());
        assert!(state.volume_pct_from_prev_day > 0.0);
    }

    #[test]
    fn test_stamp_seal_pct_fields_empty_state_zeros_pct_fields() {
        // LiveCandleState::empty() + PrevDayRefs::default() must
        // produce all-zero pct fields without panic.
        let mut state = LiveCandleState::empty();
        stamp_seal_pct_fields(&mut state, PrevDayRefs::default());
        assert_eq!(state.close_pct_from_prev_day, 0.0);
        assert_eq!(state.oi_pct_from_prev_day, 0.0);
        assert_eq!(state.volume_pct_from_prev_day, 0.0);
    }

    #[test]
    fn test_stamp_seal_pct_fields_overwrites_pre_existing_pct_values() {
        // If the state already has stale pct values (from a previous
        // seal), stamping with new refs must REPLACE them (not add
        // to / merge).
        let mut state = mk_live_candle_state(105.0, 1_500_000, 5_000_000, 20_000_000);
        state.close_pct_from_prev_day = 999.99;
        state.oi_pct_from_prev_day = -88.88;
        state.volume_pct_from_prev_day = 77.77;
        let refs = PrevDayRefs {
            prev_day_close: 100.0,
            prev_day_oi: 1_000_000,
            prev_day_total_volume: 10_000_000,
        };
        stamp_seal_pct_fields(&mut state, refs);
        assert_eq!(state.close_pct_from_prev_day, 5.0);
        assert_eq!(state.oi_pct_from_prev_day, 50.0);
        assert_eq!(state.volume_pct_from_prev_day, 150.0);
    }
}
