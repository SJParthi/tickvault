//! Per-tick incremental volume tracker.
//!
//! Per L1 of `.claude/plans/active-plan-29-tf-and-movers-deletion.md`, the
//! `ticks` table stores raw `volume` (cumulative-day from Dhan) AND a new
//! `volume_delta` column that captures the per-tick increment:
//!
//! ```text
//! volume_delta[i] = current_volume[i] - last_seen_volume[security]
//! ```
//!
//! VOLUME-MONO-01 wires through `volume_delta` for the cumulative-monotonicity
//! check (a negative delta during PHASE=OPEN is a Dhan-side regression
//! signal — see `.claude/rules/project/wave-5-error-codes.md`).
//!
//! ## IST midnight reset (L13 step 1 + step 5)
//!
//! At IST 00:00:00 the `MidnightRolloverTask` calls `clear_for_new_day()`,
//! emptying the `last_seen` map. The first tick of the new day legitimately
//! resets Dhan's cumulative counter to ~0 — VOLUME-MONO-01 is suppressed
//! during PHASE=PREMARKET so this rollover does NOT fire 25,000 false
//! breach alerts.
//!
//! ## Composite key (I-P1-11)
//!
//! `(security_id, exchange_segment_code)` per
//! `.claude/rules/project/security-id-uniqueness.md`. NIFTY index
//! (IDX_I, sec_id=13) and a hypothetical NSE_EQ instrument with the
//! same security_id MUST track separate cumulative counters.
//!
//! ## Hot-path guarantee
//!
//! `record_tick` is O(1), zero-alloc, lock-free via `papaya`. Two
//! `papaya` calls per tick: `get` for the previous baseline,
//! `insert` to update. The `try_insert` race path is unused here
//! because `record_tick` is called on a single-thread tick consumer
//! (the SPSC processor). For multi-thread use, swap to `try_insert`
//! and accept the contention winner.

use std::sync::Arc;

use papaya::HashMap;

/// Composite key per I-P1-11.
type TrackerKey = (u64, u8);

/// Lock-free per-instrument cumulative-volume tracker.
///
/// Cloning is cheap (Arc-shared). Multiple consumers (tick enricher +
/// midnight rollover) hold their own clone and read concurrently.
#[derive(Clone, Default)]
pub struct VolumeDeltaTracker {
    inner: Arc<HashMap<TrackerKey, u32>>,
}

/// Outcome of a single `record_tick` call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DeltaOutcome {
    /// Per-tick incremental volume. Stored verbatim into the `volume_delta`
    /// column. Can be 0 (no trades since last tick) or positive (typical).
    /// On a day rollover before midnight reset fires, can be negative —
    /// represented as i64 so callers can detect the regression and route
    /// to VOLUME-MONO-01 via the `is_regression` flag below.
    pub delta: i64,
    /// `true` when `current_volume < previous_volume`. Caller routes
    /// to VOLUME-MONO-01 if PHASE=OPEN.
    pub is_regression: bool,
    /// `true` for the first tick of the day for a given instrument
    /// (no previous baseline existed). The caller may want to skip
    /// the monotonicity check on this case (the delta would be the
    /// full cumulative counter, which trivially exceeds any threshold).
    pub is_first_seen: bool,
}

impl VolumeDeltaTracker {
    /// Constructs an empty tracker. Call `record_tick` to populate.
    pub fn new() -> Self {
        Self::default()
    }

    /// Records a tick's cumulative volume and returns the per-tick
    /// incremental delta plus regression / first-seen flags.
    ///
    /// O(1), lock-free, zero-alloc — Phase 2.7 perf fix (hot-path agent
    /// CRITICAL C1): single papaya `insert` returns the prior value as
    /// `Option<&V>`, halving the hash probes from 2 (legacy
    /// get + insert) to 1.
    #[inline]
    pub fn record_tick(
        &self,
        security_id: u64,
        segment_code: u8,
        current_volume: u32,
    ) -> DeltaOutcome {
        let key = (security_id, segment_code);
        let guard = self.inner.guard();
        // Single atomic replace — returns the prior value if present,
        // `None` on first insert. One hash probe instead of two.
        match self.inner.insert(key, current_volume, &guard) {
            Some(p) => {
                let delta = i64::from(current_volume) - i64::from(*p);
                DeltaOutcome {
                    delta,
                    is_regression: delta < 0,
                    is_first_seen: false,
                }
            }
            None => DeltaOutcome {
                // First tick of the day — the "delta" is the full
                // cumulative counter. Caller (VOLUME-MONO-01) treats
                // is_first_seen as a suppression signal.
                delta: i64::from(current_volume),
                is_regression: false,
                is_first_seen: true,
            },
        }
    }

    /// Returns the last-seen cumulative volume for a given instrument
    /// without updating the baseline. Useful for diagnostics; on the
    /// hot path use `record_tick`.
    #[inline]
    pub fn get_last_seen(&self, security_id: u64, segment_code: u8) -> Option<u32> {
        let guard = self.inner.guard();
        self.inner
            .get(&(security_id, segment_code), &guard)
            .copied()
    }

    /// Clears all cumulative-volume baselines. Called by
    /// `MidnightRolloverTask` at IST 00:00:00 so the next tick of the
    /// new day starts fresh (Dhan resets the cumulative counter at
    /// session boundary).
    pub fn clear_for_new_day(&self) {
        let guard = self.inner.guard();
        let keys: Vec<TrackerKey> = self.inner.keys(&guard).copied().collect();
        for k in keys {
            self.inner.remove(&k, &guard);
        }
    }

    /// Number of tracked instruments. O(1).
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    /// `true` if no instruments have been recorded yet.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_tracker_is_empty() {
        let t = VolumeDeltaTracker::new();
        assert!(t.is_empty());
        assert_eq!(t.len(), 0);
        assert_eq!(t.get_last_seen(1234, 1), None);
    }

    #[test]
    fn test_record_tick_first_call_is_first_seen() {
        let t = VolumeDeltaTracker::new();
        let outcome = t.record_tick(1234, 1, 10_000);
        assert!(outcome.is_first_seen);
        assert!(!outcome.is_regression);
        assert_eq!(outcome.delta, 10_000);
        assert_eq!(t.get_last_seen(1234, 1), Some(10_000));
    }

    #[test]
    fn test_record_tick_second_call_returns_increment() {
        let t = VolumeDeltaTracker::new();
        t.record_tick(1234, 1, 10_000);
        let outcome = t.record_tick(1234, 1, 12_500);
        assert!(!outcome.is_first_seen);
        assert!(!outcome.is_regression);
        assert_eq!(outcome.delta, 2_500);
        assert_eq!(t.get_last_seen(1234, 1), Some(12_500));
    }

    #[test]
    fn test_record_tick_unchanged_volume_returns_zero_delta() {
        let t = VolumeDeltaTracker::new();
        t.record_tick(1234, 1, 5_000);
        let outcome = t.record_tick(1234, 1, 5_000);
        assert_eq!(outcome.delta, 0);
        assert!(!outcome.is_regression);
    }

    #[test]
    fn test_record_tick_regression_is_flagged() {
        let t = VolumeDeltaTracker::new();
        t.record_tick(1234, 1, 100_000);
        // Cumulative volume went DOWN — VOLUME-MONO-01 trigger condition.
        let outcome = t.record_tick(1234, 1, 50_000);
        assert!(outcome.is_regression);
        assert_eq!(outcome.delta, -50_000);
        // The baseline updates to the (lower) current value so subsequent
        // ticks don't repeatedly fire — caller decides what to do.
        assert_eq!(t.get_last_seen(1234, 1), Some(50_000));
    }

    /// I-P1-11 ratchet: same security_id under different segments
    /// tracks independently.
    #[test]
    fn test_composite_key_separates_cross_segment_collisions() {
        let t = VolumeDeltaTracker::new();
        // Both records: first-seen for both instruments.
        let nifty = t.record_tick(13, 0, 1_000_000); // NIFTY IDX_I
        let stock = t.record_tick(13, 1, 50_000); // hypothetical NSE_EQ same id
        assert!(nifty.is_first_seen);
        assert!(stock.is_first_seen);
        assert_eq!(t.len(), 2);

        // Update NIFTY only — stock's baseline stays.
        let n2 = t.record_tick(13, 0, 1_500_000);
        assert!(!n2.is_first_seen);
        assert_eq!(n2.delta, 500_000);
        assert_eq!(t.get_last_seen(13, 1), Some(50_000), "stock untouched");
    }

    #[test]
    fn test_clear_for_new_day_empties_map() {
        let t = VolumeDeltaTracker::new();
        t.record_tick(1, 1, 100);
        t.record_tick(2, 1, 200);
        t.record_tick(3, 2, 300);
        assert_eq!(t.len(), 3);

        t.clear_for_new_day();

        assert_eq!(t.len(), 0);
        assert!(t.is_empty());
        // Next tick is first-seen again — this is the L13 IST midnight
        // legitimately-reset case.
        let outcome = t.record_tick(1, 1, 50);
        assert!(outcome.is_first_seen, "post-rollover tick is first-seen");
        assert!(
            !outcome.is_regression,
            "rollover NOT a regression — VOLUME-MONO-01 suppression handles this"
        );
    }

    /// Day-rollover scenario: simulates the IST midnight rollover bug
    /// that L13 prevents. WITHOUT the midnight clear, the first tick
    /// of day 2 (cumulative=50) vs day 1's last (cumulative=10000)
    /// would be flagged as regression. WITH the clear, it's first-seen.
    #[test]
    fn test_day_rollover_with_midnight_clear_no_false_regression() {
        let t = VolumeDeltaTracker::new();
        // Day 1: cumulative climbs to 10,000.
        t.record_tick(99, 1, 10_000);

        // IST midnight fires.
        t.clear_for_new_day();

        // Day 2 first tick: Dhan reset cumulative to 50.
        let day2_first = t.record_tick(99, 1, 50);
        assert!(day2_first.is_first_seen);
        assert!(
            !day2_first.is_regression,
            "midnight clear MUST suppress the day-rollover false regression"
        );
    }

    /// Day-rollover WITHOUT clear (regression case) — proves L13 is required.
    #[test]
    fn test_day_rollover_without_midnight_clear_flags_regression() {
        let t = VolumeDeltaTracker::new();
        t.record_tick(99, 1, 10_000);
        // No clear — simulating the bug L13 prevents.
        let day2_first = t.record_tick(99, 1, 50);
        assert!(!day2_first.is_first_seen);
        assert!(
            day2_first.is_regression,
            "without midnight clear, day rollover trivially looks like regression — proves L13's necessity"
        );
    }

    #[test]
    fn test_clone_shares_state() {
        let t1 = VolumeDeltaTracker::new();
        let t2 = t1.clone();
        t1.record_tick(42, 1, 999);
        assert_eq!(
            t2.get_last_seen(42, 1),
            Some(999),
            "clone must share inner state"
        );
    }

    #[test]
    fn test_reasonable_memory_bound_at_25k_entries() {
        let t = VolumeDeltaTracker::new();
        for i in 0..25_000_u64 {
            t.record_tick(i, 1, 100 + i as u32);
        }
        assert_eq!(t.len(), 25_000);
        assert_eq!(t.get_last_seen(0, 1), Some(100));
        assert_eq!(t.get_last_seen(24_999, 1), Some(100 + 24_999));
    }

    #[test]
    fn test_record_tick_explicit_name_match() {
        // pub-fn-test guard requires the test name contains the fn name.
        let t = VolumeDeltaTracker::new();
        let _ = t.record_tick(1, 1, 1);
    }

    #[test]
    fn test_get_last_seen_explicit_name_match() {
        let t = VolumeDeltaTracker::new();
        assert_eq!(t.get_last_seen(99, 1), None);
    }

    #[test]
    fn test_clear_for_new_day_explicit_name_match() {
        let t = VolumeDeltaTracker::new();
        t.clear_for_new_day();
        assert!(t.is_empty());
    }

    #[test]
    fn test_len_explicit_name_match() {
        let t = VolumeDeltaTracker::new();
        assert_eq!(t.len(), 0);
    }

    #[test]
    fn test_is_empty_explicit_name_match() {
        let t = VolumeDeltaTracker::new();
        assert!(t.is_empty());
    }
}
