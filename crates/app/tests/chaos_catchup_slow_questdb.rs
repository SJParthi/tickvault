//! PR TG-2 (2026-11-06 live incident): the boot WAL catch-up drain against a
//! QuestDB that stalls.
//!
//! On 2026-11-06 QuestDB's single HTTP worker was held by archiver exports,
//! so ILP writes waited and apply lag grew. The catch-up drain must then
//! (a) never archive a WAL segment whose rows the writers have not acked,
//! and say so within a bounded wait; (b) pause while apply lag grows and
//! stand down, never push more replay into a backlog that is not shrinking;
//! (c) never keep the sockets dark past its clock. These tests drive the
//! decision logic the lane exposes (`AppliedWatermark` ack accounting,
//! `wal_catchup_lag_step`, `wal_catchup_budget_secs`) rather than standing up
//! the whole lane; a source pin keeps the pause-loop model below identical
//! to the loop in `dhan_feed_stack.rs`.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tickvault_app::dhan_feed_stack::{
    CatchupLagStep, WAL_CATCHUP_BUDGET_SECS, WAL_CATCHUP_IN_SESSION_BUDGET_SECS,
    WAL_CATCHUP_LAG_PAUSE_MAX_SECS, WAL_CATCHUP_LAG_PAUSE_POLL_SECS,
    WAL_CATCHUP_PREOPEN_STOP_SECS_OF_DAY_IST, wal_catchup_budget_secs, wal_catchup_lag_step,
};
use tickvault_common::constants::{
    TICK_PERSIST_END_SECS_OF_DAY_IST, TICK_PERSIST_START_SECS_OF_DAY_IST,
};
use tickvault_storage::wal_applied_watermark::AppliedWatermark;

// ---------------------------------------------------------------------------
// (a) Ack timeout: a writer stalled on QuestDB never lets a segment archive.
// ---------------------------------------------------------------------------

/// Slack on a wall-clock bound for a loaded CI host.
const WAIT_SLACK: Duration = Duration::from_secs(2);

/// A tick batch handed to a writer that is stuck on QuestDB never completes:
/// the ack wait answers `false` (segments stay staged) and returns at its
/// timeout, not later.
#[test]
fn test_regression_tg2_ack_wait_times_out_on_a_writer_stalled_by_questdb() {
    let wm = AppliedWatermark::new();
    let before = wm.unlanded_total();
    wm.note_ticks_handed_off();
    let timeout = Duration::from_millis(300);
    let started = Instant::now();
    assert!(
        !wm.wait_for_offload_drained(timeout, before),
        "an unacked batch must not read as landed"
    );
    let elapsed = started.elapsed();
    assert!(elapsed >= timeout, "it waited the whole bound: {elapsed:?}");
    assert!(
        elapsed < timeout + WAIT_SLACK,
        "it returned at its bound, not later: {elapsed:?}"
    );
}

/// The same with the DEPTH writer stalled while the tick writer is fine: one
/// stalled sink is enough to refuse the archive.
#[test]
fn test_regression_tg2_ack_wait_refuses_when_only_the_depth_writer_is_stalled() {
    let wm = AppliedWatermark::new();
    let before = wm.unlanded_total();
    wm.note_ticks_handed_off();
    wm.note_ticks_completed();
    wm.note_depth_handed_off();
    assert!(!wm.wait_for_offload_drained(Duration::from_millis(100), before));
    wm.note_depth_completed();
    assert!(wm.wait_for_offload_drained(Duration::from_millis(100), before));
}

/// QuestDB recovers inside the wait: the writer completes the batch after a
/// delay shorter than the bound, and the wait answers `true` as soon as it
/// does, without spending the rest of the bound.
#[test]
fn test_regression_tg2_ack_wait_succeeds_when_questdb_recovers_inside_the_bound() {
    let wm = Arc::new(AppliedWatermark::new());
    let before = wm.unlanded_total();
    wm.note_ticks_handed_off();
    let writer = {
        let wm = Arc::clone(&wm);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(150));
            wm.note_ticks_completed();
        })
    };
    let bound = Duration::from_secs(10);
    let started = Instant::now();
    assert!(wm.wait_for_offload_drained(bound, before));
    assert!(started.elapsed() < bound / 2, "returned when the ack came");
    writer.join().expect("writer thread");
}

/// The stalled write finally fails AND its rescue to disk fails: the batch
/// completes, but it landed nowhere. The wait refuses at once, without
/// spending its bound, so the segment is never archived unread.
#[test]
fn test_regression_tg2_ack_wait_refuses_a_batch_that_landed_nowhere_without_waiting_out_the_bound()
{
    let wm = Arc::new(AppliedWatermark::new());
    let before = wm.unlanded_total();
    wm.note_ticks_handed_off();
    let writer = {
        let wm = Arc::clone(&wm);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            wm.note_unlanded();
            wm.note_ticks_completed();
        })
    };
    let bound = Duration::from_secs(10);
    let started = Instant::now();
    assert!(!wm.wait_for_offload_drained(bound, before));
    assert!(
        started.elapsed() < bound / 2,
        "the refusal is immediate once the loss is known"
    );
    writer.join().expect("writer thread");
}

/// While QuestDB's probe is unclean (suspended or lagging tables keep ACKing
/// writes they do not apply), acks park instead of raising the skip
/// watermark, and the sink reads SUSPECT, which the replay confirm refuses
/// on. A clean probe ends the episode.
#[test]
fn test_regression_tg2_acks_during_a_questdb_stall_do_not_raise_the_skip_watermark() {
    let wm = AppliedWatermark::new();
    let seq = 5_u64 << 40;
    wm.note_ticks_acked(seq);
    assert_eq!(wm.probe_basis().0, seq);
    assert!(!wm.is_sink_suspect());

    wm.note_questdb_probe(false);
    assert!(
        wm.is_sink_suspect(),
        "an unclean probe makes the sink suspect"
    );
    wm.note_ticks_acked(seq * 2);
    wm.note_depth_acked(seq * 2);
    assert_eq!(
        wm.probe_basis(),
        (seq, 0),
        "an ack taken while QuestDB is stalled must not move the watermark the \
         next boot skips by"
    );

    wm.note_questdb_probe(true);
    assert!(!wm.is_sink_suspect(), "a clean probe ends the episode");
}

// ---------------------------------------------------------------------------
// (b) + (c) Apply-lag pause and clock run-out.
// ---------------------------------------------------------------------------

/// How one pause ended, and how many seconds it took.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PauseEnd {
    step: CatchupLagStep,
    elapsed_secs: u64,
}

/// The catch-up drain's pause loop, step for step: `growing(paused_secs)` is
/// the number of tables whose apply lag is growing at that point. Each pause
/// step sleeps `POLL.min(secs_left.max(1))`, exactly as the loop in
/// `dhan_feed_stack.rs` (pinned by the source test below).
fn simulate_pause(budget_secs: u64, mut growing: impl FnMut(u64) -> u32) -> PauseEnd {
    let mut elapsed = 0_u64;
    let mut paused = 0_u64;
    // Bounded: every Pause advances the clock by at least one second and the
    // budget is finite, so this ends; the cap only guards a regression.
    for _ in 0..10_000 {
        let secs_left = budget_secs.saturating_sub(elapsed);
        match wal_catchup_lag_step(growing(paused), paused, secs_left) {
            CatchupLagStep::Pause => {
                let step = WAL_CATCHUP_LAG_PAUSE_POLL_SECS.min(secs_left.max(1));
                paused = paused.saturating_add(step);
                elapsed = elapsed.saturating_add(step);
            }
            end => {
                return PauseEnd {
                    step: end,
                    elapsed_secs: elapsed,
                };
            }
        }
    }
    panic!("the pause loop did not end");
}

fn in_session(t: u64) -> bool {
    t >= u64::from(TICK_PERSIST_START_SECS_OF_DAY_IST)
        && t < u64::from(TICK_PERSIST_END_SECS_OF_DAY_IST)
}

/// A QuestDB whose apply lag grows forever, at every boot minute of the day:
/// the pause always ends, never outlives the drain's clock or the longest
/// pause, and is reported by its real cause.
#[test]
fn test_regression_tg2_lag_pause_against_a_stalled_questdb_ends_within_its_bounds_all_day() {
    for boot in (0..86_400_u64).step_by(60) {
        let budget = wal_catchup_budget_secs(boot);
        let end = simulate_pause(budget, |_| 3);
        assert!(
            end.elapsed_secs <= budget.min(WAL_CATCHUP_LAG_PAUSE_MAX_SECS),
            "boot {boot}: paused {}s, budget {budget}s",
            end.elapsed_secs
        );
        assert_ne!(
            end.step,
            CatchupLagStep::Proceed,
            "boot {boot}: lag never cleared"
        );
        if in_session(boot) {
            // The sockets are dark while the drain runs: an in-session boot
            // spends its short budget and stands down for the CLOCK, never
            // reported as "apply lag kept growing" (the 2026-10-02 split).
            assert_eq!(budget, WAL_CATCHUP_IN_SESSION_BUDGET_SECS);
            assert_eq!(
                end,
                PauseEnd {
                    step: CatchupLagStep::OutOfTime,
                    elapsed_secs: WAL_CATCHUP_IN_SESSION_BUDGET_SECS
                },
                "boot {boot}"
            );
        }
        if boot < u64::from(TICK_PERSIST_START_SECS_OF_DAY_IST) {
            // A pre-open boot never pauses past 08:58 unless it got no more
            // than the in-session budget.
            assert!(
                boot + end.elapsed_secs
                    <= WAL_CATCHUP_PREOPEN_STOP_SECS_OF_DAY_IST
                        .max(boot + WAL_CATCHUP_IN_SESSION_BUDGET_SECS),
                "boot {boot}: paused until {}",
                boot + end.elapsed_secs
            );
        }
        if budget == WAL_CATCHUP_BUDGET_SECS {
            // A full budget outlasts the longest pause: the drain stands down
            // for APPLY LAG after exactly the longest pause.
            assert_eq!(
                end,
                PauseEnd {
                    step: CatchupLagStep::Stop,
                    elapsed_secs: WAL_CATCHUP_LAG_PAUSE_MAX_SECS
                },
                "boot {boot}"
            );
        }
    }
}

/// QuestDB recovers mid-pause: the drain resumes on the first poll that sees
/// the lag stop growing, having waited only those polls.
#[test]
fn test_regression_tg2_lag_pause_resumes_on_the_first_poll_after_questdb_recovers() {
    let evening = u64::from(TICK_PERSIST_END_SECS_OF_DAY_IST) + 3_600;
    let budget = wal_catchup_budget_secs(evening);
    for polls in 0..(WAL_CATCHUP_LAG_PAUSE_MAX_SECS / WAL_CATCHUP_LAG_PAUSE_POLL_SECS) {
        let clears_at = polls * WAL_CATCHUP_LAG_PAUSE_POLL_SECS;
        let end = simulate_pause(budget, |paused| u32::from(paused < clears_at));
        assert_eq!(
            end,
            PauseEnd {
                step: CatchupLagStep::Proceed,
                elapsed_secs: clears_at
            },
            "lag cleared after {polls} polls"
        );
    }
}

/// The model above is the production loop: the pause step, the clock it
/// reads and the decision call are the exact expressions in
/// `dhan_feed_stack.rs`. If the loop changes, this fails and the model must
/// change with it.
#[test]
fn test_regression_tg2_pause_model_matches_the_catch_up_loop_source() {
    let src = include_str!("../src/dhan_feed_stack.rs");
    let at = src
        .find("match wal_catchup_lag_step(growing, lag_paused_secs, secs_left)")
        .expect("the catch-up loop decides through wal_catchup_lag_step");
    let window = &src[at.saturating_sub(800)..(at + 2_500).min(src.len())];
    for needle in [
        "catchup_deadline\n                    .saturating_duration_since(tokio::time::Instant::now())\n                    .as_secs()",
        "let step = WAL_CATCHUP_LAG_PAUSE_POLL_SECS.min(secs_left.max(1));",
        "lag_paused_secs = lag_paused_secs.saturating_add(step);",
        "CatchupLagStep::OutOfTime => {",
        "CatchupLagStep::Stop => {",
    ] {
        assert!(
            window.contains(needle),
            "the catch-up loop no longer has: {needle}"
        );
    }
}
