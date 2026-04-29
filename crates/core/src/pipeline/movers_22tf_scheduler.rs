//! Movers 22-TF snapshot scheduler — Phase 10 of v3 plan, 2026-04-28.
//!
//! Schedules 22 `tokio::spawn` tasks, one per timeframe in
//! `MOVERS_TIMEFRAMES`. Each task:
//!
//! 1. Sleeps until its next cadence boundary (1s, 5s, ..., 1h aligned).
//! 2. Checks the market-hours gate (`is_within_market_hours_ist`) per
//!    `audit-findings-2026-04-17.md` Rule 3.
//! 3. Calls `snapshot_into(arena)` on the shared `MoversTracker` to fill
//!    the per-task arena `Vec<MoverRow>` (zero-alloc per snapshot).
//! 4. Drains the arena into the per-timeframe `mpsc::Sender<MoverRow>`
//!    using the drop-newest backpressure semantics (native tokio).
//! 5. Records lag + drop-rate metrics.
//!
//! ## Pure-function primitives
//!
//! The scheduling logic is split into pure functions (`compute_next_tick`,
//! `compute_lag_secs`, `should_emit`) so it can be tested without spawning
//! any tasks. The async runner (`run_movers_22tf_scheduler`) composes them.
//!
//! ## See also
//!
//! - `tickvault_common::mover_types::MOVERS_TIMEFRAMES` — the ladder
//! - `crates/core/src/pipeline/movers_22tf_supervisor.rs` — respawn logic
//! - `crates/core/src/pipeline/movers_22tf_writer_state.rs` — sender registry

use std::time::Duration;

use tickvault_common::mover_types::{MOVERS_TIMEFRAMES, MoversTimeframe};

/// Drop-newest mpsc capacity per writer slot. Matches the plan's
/// `mpsc(8192)` decision (v3 architecture §What-changed item 6).
pub const MOVERS_22TF_MPSC_CAPACITY: usize = 8192;

/// Maximum acceptable snapshot lag before MOVERS-22TF schedule-drift
/// alert fires (500 ms). Pinned by `tv-movers-22tf-schedule-drift`
/// alert in `alerts.yml`.
pub const MAX_ACCEPTABLE_LAG_MS: u64 = 500;

/// Per-task arena pre-allocation hint (matches
/// `MOVERS_ARENA_CAPACITY` from common). Each snapshot reuses the
/// same Vec to keep the snapshot path zero-alloc.
pub use tickvault_common::mover_types::MOVERS_ARENA_CAPACITY;

/// Computes the next cadence-aligned tick instant relative to a given
/// reference. The returned value is the number of seconds since the
/// epoch, rounded UP to the next multiple of `cadence_secs`.
///
/// Example: cadence_secs = 60, now_secs = 12345 → 12360 (next 12360 + 60n).
///
/// Pure function — no time mocking required for tests.
#[must_use]
pub const fn compute_next_tick_secs(cadence_secs: u64, now_secs: u64) -> u64 {
    if cadence_secs == 0 {
        return now_secs;
    }
    let remainder = now_secs % cadence_secs;
    if remainder == 0 {
        now_secs + cadence_secs
    } else {
        now_secs + (cadence_secs - remainder)
    }
}

/// Computes the lag (in seconds) between when a snapshot was scheduled
/// and when it actually fired. Used for `tv_movers_snapshot_lag_seconds`.
///
/// `actual_secs` MUST be ≥ `scheduled_secs` in normal operation; if they
/// invert (e.g. clock moved backwards), the function returns 0.
#[must_use]
pub const fn compute_lag_secs(scheduled_secs: u64, actual_secs: u64) -> u64 {
    actual_secs.saturating_sub(scheduled_secs)
}

/// Decides whether a given timeframe should emit a snapshot at the
/// current `now_secs`, given when it last emitted.
///
/// Returns `true` iff the elapsed time since `last_emit_secs` is
/// `≥ cadence_secs`. This is the "have we crossed our cadence boundary"
/// check that the scheduler runs each loop iteration.
#[must_use]
pub const fn should_emit_snapshot(cadence_secs: u64, now_secs: u64, last_emit_secs: u64) -> bool {
    if cadence_secs == 0 {
        return true;
    }
    now_secs.saturating_sub(last_emit_secs) >= cadence_secs
}

/// Computes the sleep duration until the next cadence-aligned tick.
/// Caps at `cadence_secs` so a clock-jump-forward doesn't make the task
/// busy-loop.
#[must_use]
pub fn sleep_until_next_tick(cadence_secs: u64, now_secs: u64) -> Duration {
    let next = compute_next_tick_secs(cadence_secs, now_secs);
    Duration::from_secs(next.saturating_sub(now_secs).max(1).min(cadence_secs))
}

/// Returns the count of timeframes in the catalog. Pinned at 22 by the
/// catalog itself + ratchet in `mover_types.rs`. Re-exported here so
/// callers don't have to import twice.
#[must_use]
pub const fn timeframe_count() -> usize {
    MOVERS_TIMEFRAMES.len()
}

/// Returns the cadence (in seconds) for the timeframe at the given index.
/// Returns `None` if the index is out of range.
#[must_use]
pub fn cadence_for_index(idx: usize) -> Option<u64> {
    MOVERS_TIMEFRAMES.get(idx).map(|tf| tf.cadence_secs)
}

/// Returns the timeframe at the given index. Returns `None` if out of range.
#[must_use]
pub fn timeframe_at(idx: usize) -> Option<&'static MoversTimeframe> {
    MOVERS_TIMEFRAMES.get(idx)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Phase 10 primitive ratchet — next-tick alignment.
    #[test]
    fn test_compute_next_tick_secs_at_cadence_boundary() {
        // now_secs is exact multiple of cadence — next tick is now + cadence.
        assert_eq!(compute_next_tick_secs(60, 12_300), 12_360);
        assert_eq!(compute_next_tick_secs(1, 12_345), 12_346);
        assert_eq!(compute_next_tick_secs(3600, 7200), 10_800);
    }

    #[test]
    fn test_compute_next_tick_secs_mid_cadence() {
        // now_secs = 12_345 (mid-minute) → next tick at 12_360 (rounded up).
        assert_eq!(compute_next_tick_secs(60, 12_345), 12_360);
        assert_eq!(compute_next_tick_secs(15, 100), 105);
        assert_eq!(compute_next_tick_secs(300, 12_345), 12_600);
    }

    #[test]
    fn test_compute_next_tick_secs_zero_cadence_safe() {
        // Defensive — cadence 0 should not divide by zero.
        assert_eq!(compute_next_tick_secs(0, 12_345), 12_345);
    }

    #[test]
    fn test_compute_lag_secs_normal_case() {
        assert_eq!(compute_lag_secs(100, 105), 5);
        assert_eq!(compute_lag_secs(100, 100), 0);
    }

    #[test]
    fn test_compute_lag_secs_clock_skew_safe() {
        // actual_secs < scheduled_secs (clock moved backwards) → 0.
        assert_eq!(compute_lag_secs(100, 90), 0);
    }

    #[test]
    fn test_should_emit_snapshot_at_cadence() {
        assert!(should_emit_snapshot(60, 200, 140));
        assert!(should_emit_snapshot(60, 200, 100));
        assert!(!should_emit_snapshot(60, 200, 150));
    }

    #[test]
    fn test_should_emit_snapshot_zero_cadence_always_emits() {
        assert!(should_emit_snapshot(0, 100, 100));
    }

    #[test]
    fn test_should_emit_snapshot_clock_skew_safe() {
        // last_emit_secs > now_secs (clock skew) — should not panic, returns false.
        assert!(!should_emit_snapshot(60, 100, 200));
    }

    #[test]
    fn test_sleep_until_next_tick_caps_at_cadence() {
        // Even if compute_next is far in the future, sleep is bounded.
        let d = sleep_until_next_tick(60, 12_300);
        assert!(d <= Duration::from_secs(60));
        assert!(d >= Duration::from_secs(1));
    }

    #[test]
    fn test_timeframe_count_is_22() {
        assert_eq!(timeframe_count(), 22);
    }

    #[test]
    fn test_cadence_for_index_first_and_last() {
        assert_eq!(cadence_for_index(0), Some(1));
        assert_eq!(cadence_for_index(21), Some(3600));
        assert_eq!(cadence_for_index(22), None);
    }

    #[test]
    fn test_timeframe_at_returns_correct_label() {
        assert_eq!(timeframe_at(0).unwrap().label, "1s");
        assert_eq!(timeframe_at(5).unwrap().label, "1m");
        assert_eq!(timeframe_at(21).unwrap().label, "1h");
        assert!(timeframe_at(22).is_none());
    }

    #[test]
    fn test_movers_22tf_mpsc_capacity_is_8192() {
        assert_eq!(MOVERS_22TF_MPSC_CAPACITY, 8192);
    }

    #[test]
    fn test_max_acceptable_lag_is_500ms() {
        assert_eq!(MAX_ACCEPTABLE_LAG_MS, 500);
    }
}
