//! Wall-clock steps this process has seen (PR AA review, finding 2,
//! 2026-11-08).
//!
//! # Why
//!
//! The archive's in-flight check maps an hour to frame sequences
//! (`partition_archive::snapshot_gate::hour_frame_seq_span`), assuming a
//! row's receipt time and its frame sequence differ by about 2 s. A wall
//! clock step breaks that:
//!
//! * Back by S: a frame sequence is `max(previous + 1, wall)`, so it stays
//!   at the old time until the wall catches up, while the receipt anchor
//!   takes the step at its next 30 s refresh. Rows land in the earlier hour
//!   with sequences of the later one.
//! * Forward by S: the sequence jumps at once (it reads the wall per frame);
//!   the receipt follows at the next refresh. For up to 30 s rows land in
//!   the earlier hour with sequences S later.
//!
//! A shed or deferred mark on such a row lies outside the hour's span, so
//! the hour read clear and could be dropped while a row of it was still on
//! its way. `ClockAhead` only compares the newest sequence with the wall at
//! archive time, which has cleared by then, and never sees a forward step.
//!
//! # What this records
//!
//! One window `[lo, hi]` of wall nanos (UTC): the union of every step seen,
//! each widened by its size plus one anchor refresh interval
//! ([`CLOCK_STEP_MARGIN_NANOS`]) on both sides. Sources: a receipt anchor
//! refresh that moves the anchor more than [`CLOCK_STEP_MIN_NANOS`] either
//! way, and a segment rotation that finds the newest segment name or the
//! frame counter ahead of the wall (a backward step, or a restart after
//! one: the counter is seeded past the disk before the first segment
//! opens). An hour whose span overlaps the window is held
//! (`InFlightVerdict::ClockStepped`).
//!
//! # Honest limits
//!
//! RAM only: a restart forgets the window, like the other in-process marks.
//! Two steps far apart hold every hour between them (the union keeps more,
//! never less). A step smaller than 1 s is not recorded; the span's own 2 s
//! receipt slop covers it.
//!
//! # Complexity
//!
//! O(1): two atomic read-modify-writes per recorded step (rare: a refresh
//! every 30 s or a segment rotation), two loads per archive verdict. Never
//! on the frame path.

use std::sync::atomic::{AtomicU64, Ordering};

use tracing::info;

/// How far past a step its effect can reach: one receipt anchor refresh
/// interval (the refresh runs every 30 s), since the anchor takes a step only
/// at its next refresh.
pub(crate) const CLOCK_STEP_MARGIN_NANOS: u64 = 30_000_000_000;

/// A move of the clock this size or smaller is not a step: the span's 2 s
/// receipt slop already covers it, and NTP slew over one 30 s refresh is
/// about 15 ms.
pub(crate) const CLOCK_STEP_MIN_NANOS: u64 = 1_000_000_000;

/// Low end of the recorded window; `u64::MAX` while nothing is recorded.
static STEP_LO: AtomicU64 = AtomicU64::new(u64::MAX);

/// High end of the recorded window; `0` while nothing is recorded.
static STEP_HI: AtomicU64 = AtomicU64::new(0);

/// The window one step of `magnitude_nanos` at wall `at_wall_nanos` can
/// affect, or `None` when the move is too small to be a step. Pure, O(1).
#[must_use]
pub(crate) const fn step_window(at_wall_nanos: u64, magnitude_nanos: u64) -> Option<(u64, u64)> {
    if magnitude_nanos <= CLOCK_STEP_MIN_NANOS {
        return None;
    }
    let reach = magnitude_nanos.saturating_add(CLOCK_STEP_MARGIN_NANOS);
    Some((
        at_wall_nanos.saturating_sub(reach),
        at_wall_nanos.saturating_add(reach),
    ))
}

/// Records a clock step of `magnitude_nanos` seen at wall `at_wall_nanos`
/// (UTC nanos), widening the process window. A move of 1 s or less is
/// ignored. O(1), two atomics; the high end is written first so a reader
/// that sees the new low end also sees the new high end.
pub(crate) fn note_clock_step(at_wall_nanos: u64, magnitude_nanos: u64) {
    let Some((lo, hi)) = step_window(at_wall_nanos, magnitude_nanos) else {
        return;
    };
    STEP_HI.fetch_max(hi, Ordering::SeqCst);
    STEP_LO.fetch_min(lo, Ordering::SeqCst);
    info!(
        step_ms = magnitude_nanos / 1_000_000,
        window_lo_nanos = lo,
        window_hi_nanos = hi,
        "wall clock step recorded: archive hours that overlap this window are kept until \
         the next restart, because their frame sequences no longer map to receipt hours"
    );
}

/// The recorded window `[lo, hi]`, or `None` when no step was seen. Low end
/// read first (see [`note_clock_step`]). O(1).
#[must_use]
pub(crate) fn clock_step_window() -> Option<(u64, u64)> {
    let lo = STEP_LO.load(Ordering::SeqCst);
    let hi = STEP_HI.load(Ordering::SeqCst);
    (lo <= hi).then_some((lo, hi))
}

/// Whether `window` overlaps the inclusive span `[lo, hi]`. Pure, O(1).
#[must_use]
pub(crate) const fn window_overlaps(window: Option<(u64, u64)>, lo: u64, hi: u64) -> bool {
    match window {
        Some((w_lo, w_hi)) => w_lo <= hi && lo <= w_hi,
        None => false,
    }
}

/// How far ahead of the wall `now_nanos` a rotation finds the clock-derived
/// state: the new segment's name (above the wall only after a clamp) or the
/// frame counter. `0` when neither is ahead. Pure, O(1).
#[must_use]
pub(crate) fn rotation_lead_nanos(now_nanos: u128, named_nanos: u128, frame_seq: u64) -> u64 {
    let ahead = named_nanos.max(u128::from(frame_seq));
    u64::try_from(ahead.saturating_sub(now_nanos)).unwrap_or(u64::MAX)
}

/// Called at every WAL segment rotation (cold, writer thread): records a
/// step when the name or the frame counter is ahead of the wall. O(1).
pub(crate) fn note_segment_rotation(now_nanos: u128, named_nanos: u128) {
    let lead = rotation_lead_nanos(
        now_nanos,
        named_nanos,
        crate::ws_frame_spill::current_frame_seq(),
    );
    note_clock_step(u64::try_from(now_nanos).unwrap_or(u64::MAX), lead);
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEC: u64 = 1_000_000_000;

    #[test]
    fn test_step_window_ignores_slew_and_widens_a_step_both_ways() {
        assert_eq!(step_window(100 * SEC, SEC), None);
        assert_eq!(step_window(100 * SEC, 0), None);
        assert_eq!(
            step_window(1_000 * SEC, 600 * SEC),
            Some((370 * SEC, 1_630 * SEC))
        );
        // Saturates at both ends instead of wrapping.
        assert_eq!(step_window(0, 10 * SEC), Some((0, 40 * SEC)));
        assert_eq!(
            step_window(u64::MAX, 10 * SEC),
            Some((u64::MAX - 40 * SEC, u64::MAX))
        );
    }

    #[test]
    fn test_window_overlaps_is_inclusive_and_none_never_overlaps() {
        assert!(!window_overlaps(None, 0, u64::MAX));
        let w = Some((10, 20));
        assert!(window_overlaps(w, 0, 10));
        assert!(window_overlaps(w, 20, 30));
        assert!(window_overlaps(w, 12, 14));
        assert!(window_overlaps(w, 0, 100));
        assert!(!window_overlaps(w, 0, 9));
        assert!(!window_overlaps(w, 21, 30));
    }

    #[test]
    fn test_rotation_lead_nanos_reads_the_name_or_the_counter_ahead_of_the_wall() {
        let now = u128::from(1_000 * SEC);
        assert_eq!(rotation_lead_nanos(now, now, 999 * SEC), 0);
        assert_eq!(rotation_lead_nanos(now, now + 5, 0), 5);
        assert_eq!(rotation_lead_nanos(now, now, 1_600 * SEC), 600 * SEC);
        assert_eq!(rotation_lead_nanos(now, now + 7, 1_000 * SEC + 3), 7);
    }

    /// The process window is global to the test binary, so this only asserts
    /// that a recorded step is inside the window afterwards (other tests may
    /// have widened it).
    #[test]
    fn test_note_clock_step_and_note_segment_rotation_widen_the_clock_step_window() {
        // A far-past instant no real hour test uses (1971).
        let at = 40_000_000 * SEC;
        note_clock_step(at, SEC);
        note_clock_step(at, 600 * SEC);
        let window = clock_step_window().expect("a step was recorded");
        assert!(window_overlaps(
            Some(window),
            at - 600 * SEC,
            at + 600 * SEC
        ));
        assert!(window.0 <= at - 630 * SEC && window.1 >= at + 630 * SEC);
        // A rotation with nothing ahead of the wall records nothing new: the
        // window is unchanged.
        note_segment_rotation(u128::from(u64::MAX / 2), u128::from(u64::MAX / 2));
        assert_eq!(clock_step_window(), Some(window));
    }
}
