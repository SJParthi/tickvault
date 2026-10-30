//! Coverage for `tickvault_app::drain_hold::measure_day_work` and the pure
//! decisions its supervisor feeds (`ProgressTracker`, `HoldState`).
//!
//! `measure_day_work` reads the production spill folders (relative to the
//! working directory), the deferred-depth marks, the seal spill and a few
//! process globals. In a fresh test process the globals are at rest, so the
//! fields this test asserts exactly are the ones those globals decide; the
//! spill-file count depends on what earlier runs left on disk and is only
//! checked for consistency.

use tickvault_app::drain_hold::{
    DRAIN_HOLD_DONE_POLLS, DRAIN_HOLD_REFRESH_SECS, DRAIN_HOLD_STALL_SECS, DayWork, HoldAction,
    HoldState, ProgressTracker, measure_day_work,
};

fn day(y: i32, m: u32, d: u32) -> chrono::NaiveDate {
    chrono::NaiveDate::from_ymd_opt(y, m, d).expect("valid date")
}

#[test]
fn test_drain_hold_measure_day_work_fresh_process_reports_no_deferred_marks_and_no_wal_hold() {
    let work = measure_day_work(day(2026, 10, 29));
    // Nothing in this process marked a deferred depth bucket.
    assert_eq!(work.deferred_marks, 0);
    assert!(!work.deferred_overflowed);
    // The supervisor decides `wal_holding`; the measurement never sets it.
    assert!(!work.wal_holding);
    // The summary names every part it measured.
    let text = work.left_text();
    assert!(text.starts_with("depth_buckets=0 "), "{text}");
    assert!(
        text.contains(&format!("spill_files={}", work.spill_files)),
        "{text}"
    );
    assert!(!text.contains("+overflow"), "{text}");
    assert!(!text.contains("+replaying"), "{text}");
}

#[test]
fn test_drain_hold_measure_day_work_is_stable_across_days_and_extreme_dates() {
    // A day with no seal spill at all, the earliest and latest representable
    // dates: the measurement never panics and the globals read the same.
    let a = measure_day_work(day(2026, 10, 29));
    for d in [
        chrono::NaiveDate::MIN,
        chrono::NaiveDate::MAX,
        day(1970, 1, 1),
    ] {
        let b = measure_day_work(d);
        assert_eq!(b.deferred_marks, a.deferred_marks);
        assert_eq!(b.deferred_overflowed, a.deferred_overflowed);
        assert_eq!(b.wal_holding, a.wal_holding);
        assert_eq!(b.spill_files, a.spill_files, "spill folders are not dated");
    }
}

#[test]
fn test_drain_hold_measure_day_work_feeds_tracker_and_hold_state() {
    let work = measure_day_work(day(2026, 10, 29));
    let mut tracker = ProgressTracker::default();
    // The first reading only anchors.
    assert!(!tracker.observe(&work, 0, 1_000));
    // Nothing moved, not yet an hour: not stalled.
    assert!(!tracker.observe(&work, 0, 1_000 + DRAIN_HOLD_STALL_SECS - 1));
    // Nothing moved for an hour: stalled.
    assert!(tracker.observe(&work, 0, 1_000 + DRAIN_HOLD_STALL_SECS));
    // The pass writing rows is progress again.
    assert!(!tracker.observe(&work, 1, 1_000 + DRAIN_HOLD_STALL_SECS + 1));

    let mut state = HoldState::default();
    let first = state.step(work.is_done(), 0);
    if work.is_done() {
        assert_eq!(first, HoldAction::None);
    } else {
        assert_eq!(first, HoldAction::Hold);
        assert_eq!(
            state.step(false, DRAIN_HOLD_REFRESH_SECS - 1),
            HoldAction::None
        );
        assert_eq!(state.step(false, DRAIN_HOLD_REFRESH_SECS), HoldAction::Hold);
    }
}

#[test]
fn test_drain_hold_hold_state_releases_once_after_done_streak() {
    let mut state = HoldState::default();
    assert_eq!(state.step(false, 0), HoldAction::Hold);
    let mut actions = Vec::new();
    for i in 0..(DRAIN_HOLD_DONE_POLLS + 3) {
        actions.push(state.step(true, i64::from(i) + 1));
    }
    let releases = actions
        .iter()
        .filter(|a| **a == HoldAction::Release)
        .count();
    assert_eq!(releases, 1, "{actions:?}");
    assert!(state.released);
    assert_eq!(state.last_hold_write, None);
    // New work after a release holds again at once.
    assert_eq!(state.step(false, 10_000), HoldAction::Hold);
    assert!(!state.released);
}

#[test]
fn test_drain_hold_progress_tracker_unknown_parts_never_count_as_progress() {
    let unknown = DayWork::default();
    let mut tracker = ProgressTracker::default();
    assert!(!tracker.observe(&unknown, 0, 0));
    // A part going from unknown to known is a fall (MAX -> value).
    let known = DayWork {
        apply_lag: Some(5),
        ..DayWork::default()
    };
    assert!(!tracker.observe(&known, 0, DRAIN_HOLD_STALL_SECS * 2));
    // Back to unknown is not progress; an hour later it is a stall.
    assert!(!tracker.observe(&unknown, 0, DRAIN_HOLD_STALL_SECS * 2 + 10));
    assert!(tracker.observe(&unknown, 0, DRAIN_HOLD_STALL_SECS * 3));
}
