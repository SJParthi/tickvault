//! How long depth rows from a frame the WAL refused have waited in RAM.
//!
//! Such a row exists only in the depth buffer: no WAL segment can replay it
//! after a crash. ITEM 57b kept these rows on the 500 ms hand-off, but only in
//! the drain's timer arm. The drain's `select!` is `biased` with the frame arm
//! first, so while the ring stays non-empty (the 09:15 burst, a catch-up after
//! a disk stall) the timer arm is not polled and the rows could wait until the
//! 8 MiB size trigger, seconds later. The frame arm now asks this clock once
//! per frame and hands the rows on itself once they have waited the limit.
//!
//! O(1) per frame, no allocation, no lock: one `Option` compare while no
//! unbacked rows exist, and one clock read per frame while they do.

use std::time::Duration;

use tokio::time::Instant;

/// When the oldest unbacked depth row still in the buffer started waiting.
#[derive(Debug, Default)]
pub(super) struct UnbackedDepthClock {
    since: Option<Instant>,
}

impl UnbackedDepthClock {
    /// True when unbacked rows have waited `limit` or longer; the clock then
    /// restarts, so a hand-off that leaves the rows behind (offload queue
    /// full) is retried after another `limit`, never once per frame.
    pub(super) fn due(&mut self, unbacked: bool, limit: Duration) -> bool {
        if !unbacked {
            self.since = None;
            return false;
        }
        self.due_at(true, Instant::now(), limit)
    }

    /// Forgets the wait after a flush made elsewhere (the timer arm, or the
    /// size trigger in the same frame), so the frame arm does not hand the
    /// same buffer on a second time inside one limit.
    pub(super) fn reset(&mut self) {
        self.since = None;
    }

    /// [`Self::due`] at a given instant, for tests.
    fn due_at(&mut self, unbacked: bool, now: Instant, limit: Duration) -> bool {
        if !unbacked {
            self.since = None;
            return false;
        }
        let since = *self.since.get_or_insert(now);
        if now.saturating_duration_since(since) >= limit {
            self.since = None;
            return true;
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    const LIMIT: Duration = Duration::from_millis(500);

    #[test]
    fn test_unbacked_clock_never_due_without_unbacked_rows() {
        let mut clock = UnbackedDepthClock::default();
        let t0 = Instant::now();
        for step in 0..10_u64 {
            assert!(!clock.due_at(false, t0 + Duration::from_secs(step), LIMIT));
        }
        assert!(clock.since.is_none());
        assert!(!clock.due(false, LIMIT));
    }

    #[test]
    fn test_unbacked_clock_due_once_the_limit_has_passed() {
        let mut clock = UnbackedDepthClock::default();
        let t0 = Instant::now();
        assert!(
            !clock.due_at(true, t0, LIMIT),
            "the first sight only starts the clock"
        );
        assert!(!clock.due_at(true, t0 + Duration::from_millis(499), LIMIT));
        assert!(
            clock.due_at(true, t0 + LIMIT, LIMIT),
            "due at exactly the limit"
        );
    }

    #[test]
    fn test_unbacked_clock_restarts_after_it_fires() {
        let mut clock = UnbackedDepthClock::default();
        let t0 = Instant::now();
        assert!(!clock.due_at(true, t0, LIMIT));
        assert!(clock.due_at(true, t0 + LIMIT, LIMIT));
        // The hand-off left the rows behind: the next attempt waits a full limit.
        assert!(!clock.due_at(true, t0 + LIMIT + Duration::from_millis(1), LIMIT));
        assert!(!clock.due_at(true, t0 + LIMIT * 2, LIMIT));
        assert!(clock.due_at(true, t0 + LIMIT * 2 + Duration::from_millis(1), LIMIT));
    }

    #[test]
    fn test_unbacked_clock_resets_when_the_rows_leave() {
        let mut clock = UnbackedDepthClock::default();
        let t0 = Instant::now();
        assert!(!clock.due_at(true, t0, LIMIT));
        // The timer arm flushed them in between: the clock forgets t0.
        assert!(!clock.due_at(false, t0 + Duration::from_millis(400), LIMIT));
        assert!(!clock.due_at(true, t0 + Duration::from_millis(600), LIMIT));
        assert!(!clock.due_at(true, t0 + Duration::from_millis(1_099), LIMIT));
        assert!(clock.due_at(true, t0 + Duration::from_millis(1_100), LIMIT));
    }

    #[test]
    fn test_unbacked_clock_reset_after_another_flush_waits_a_full_limit() {
        let mut clock = UnbackedDepthClock::default();
        let t0 = Instant::now();
        assert!(!clock.due_at(true, t0, LIMIT));
        // The timer arm flushed at t0 + 300 ms.
        clock.reset();
        assert!(!clock.due_at(true, t0 + Duration::from_millis(300), LIMIT));
        assert!(
            !clock.due_at(true, t0 + LIMIT, LIMIT),
            "no second hand-off 200 ms after the timer arm's"
        );
        assert!(clock.due_at(true, t0 + Duration::from_millis(800), LIMIT));
    }

    #[test]
    fn test_unbacked_clock_due_reads_the_wall_only_while_unbacked() {
        let mut clock = UnbackedDepthClock::default();
        assert!(!clock.due(true, Duration::from_secs(3_600)));
        assert!(clock.since.is_some(), "unbacked rows start the clock");
        assert!(!clock.due(false, Duration::from_secs(3_600)));
        assert!(clock.since.is_none(), "no unbacked rows clears it");
        assert!(
            clock.due(true, Duration::ZERO),
            "a zero limit is due at once"
        );
    }

    proptest! {
        /// Over any sequence of frames, a run of unbacked rows is handed on
        /// no later than the first frame at or past `limit` after it started,
        /// and never twice inside one `limit`.
        #[test]
        fn proptest_unbacked_clock_fires_at_most_once_per_limit_and_never_late(
            steps in prop::collection::vec((0_u64..400, any::<bool>()), 1..400),
        ) {
            let mut clock = UnbackedDepthClock::default();
            let t0 = Instant::now();
            let mut now = t0;
            let mut run_start: Option<Instant> = None;
            let mut last_fire: Option<Instant> = None;
            for (gap_ms, unbacked) in steps {
                now += Duration::from_millis(gap_ms);
                let fired = clock.due_at(unbacked, now, LIMIT);
                if !unbacked {
                    prop_assert!(!fired);
                    run_start = None;
                    continue;
                }
                let start = *run_start.get_or_insert(now);
                if fired {
                    if let Some(prev) = last_fire {
                        prop_assert!(now.duration_since(prev) >= LIMIT);
                    }
                    prop_assert!(now.duration_since(start) >= LIMIT);
                    last_fire = Some(now);
                    run_start = None;
                } else {
                    prop_assert!(now.duration_since(start) < LIMIT, "late: waited past the limit");
                }
            }
        }
    }
}
