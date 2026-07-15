//! Feed-toggle full-lifecycle guard (PR-4 of feed-toggle-full-lifecycle plan).
//!
//! Operator lock chain: 2026-06-23 ("until the feed is ON, the entire
//! architecture for that feed must never be touched") + 2026-06-24 ("when I
//! enable on/off the entire mechanism should run entirely for that feed").
//!
//! PR-1/PR-2 shipped the dormant, level-triggered activation watchers whose
//! decisions are PURE: `reconcile_lane_action`, `is_dead_activation`.
//! PR-1/PR-2 unit tests cover those decisions POINT-WISE (single inputs). The
//! remaining regression class is BEHAVIOURAL-OVER-A-SEQUENCE: a rapid toggle
//! storm, a crash-on-start re-Start, a double toggle. This module drives the
//! SAME pure decisions through hostile SEQUENCES and asserts the watcher
//! converges + is idempotent — proving the real failure modes (missed edge,
//! double-spawn, dead-lane-never-restarted) cannot regress.
//!
//! ## PR-C2 retirement (2026-07-13 — Dhan live-WS lane deletion, operator
//! retirement directive per websocket-connection-scope-lock.md "2026-07-13
//! Amendment" §B)
//! The Dhan-side halves of this suite (`step_dhan`,
//! `dhan_toggle_storm_converges_with_gate_open`,
//! `dhan_toggle_storm_no_double_start`,
//! `dhan_stale_flag_reconverges_then_idles`,
//! `dhan_double_enable_is_idempotent`, `dhan_double_disable_is_idempotent`,
//! `off_dhan_feed_takes_no_start_action_even_with_gate_states`,
//! `dhan_disable_storm_never_tears_down_while_live`,
//! `dhan_disable_storm_tears_down_once_gate_opens`, and the Dhan arms of
//! `dead_activation_never_restarts_a_running_lane`) are RETIRED: they pinned
//! `dhan_activation.rs` (`reconcile_dhan_lane_action_with_gate`,
//! `is_dead_activation`, `start_marks_running`), which died with the lane —
//! the runtime Dhan toggle's ON-half is REVOKED (the /feeds POST answers 409),
//! so NO Dhan reconciler exists to sequence-test. The FeedRuntimeState
//! dhan-lane FLAG round-trip test is KEPT (the flag surface survives for the
//! /feeds page + the 409 refusal path). The Groww watcher tests are UNCHANGED.
//!
//! ## Honest envelope (no illusion)
//! These are PURE-function sequence guards — they model ONE watcher loop
//! iteration WITHOUT I/O (no live QuestDB / Groww / tokio supervisor). That is
//! the correct altitude: the decision logic is what regresses; the live
//! supervisor is exercised by the app's own boot path. The #1192 source-scan
//! guard (`per_feed_boot_isolation_guard.rs`) pins the spawn SHAPE; PR-4 adds
//! the behavioural SEQUENCE proofs on top.

use tickvault_api::feed_state::{Feed, FeedRuntimeState};
use tickvault_app::groww_activation::{
    self, is_dead_activation as groww_is_dead, reconcile_lane_action as groww_reconcile,
};

// ───────────────────────── deterministic watcher-step models ─────────────────
// Each helper models ONE reconcile iteration WITHOUT I/O. `Start` would spawn the
// lane task → we model "now activated = true". `Stop` → activated = false.
// `None` → activated unchanged. A `dead`-activation pre-step (the panic / stale
// watchdog) clears `activated` first, exactly as the real loops do.

/// One Groww watcher step. `task_finished` + `lane_running` model the dead-task
/// watchdog inputs; returns `(action, next_activated)`.
fn step_groww(
    desired: bool,
    activated: bool,
    task_finished: bool,
    lane_running: bool,
) -> (groww_activation::LaneAction, bool) {
    // Dead-task watchdog: if desired-ON + task finished without bringing the lane
    // up, the watcher clears its handle so THIS tick re-Starts it.
    let activated = if activated && groww_is_dead(desired, task_finished, lane_running) {
        false
    } else {
        activated
    };
    let action = groww_reconcile(desired, activated);
    let next = match action {
        groww_activation::LaneAction::Start => true,
        groww_activation::LaneAction::Stop => false,
        groww_activation::LaneAction::None => activated,
    };
    (action, next)
}

// ───────────────────────────── toggle-storm convergence ──────────────────────

#[test]
fn groww_toggle_storm_converges_to_final_on() {
    // A flap where the watcher observes each level value at successive polls. The
    // reconciler is level-triggered, so the lane ends activated iff the FINAL
    // observed value is ON — no oscillation, no missed edge.
    let storm = [true, false, true, false, true]; // final = ON
    let mut activated = false;
    let mut starts = 0u32;
    for &desired in &storm {
        // Healthy task (not finished / lane running tracks activated).
        let (action, next) = step_groww(desired, activated, false, activated);
        if action == groww_activation::LaneAction::Start {
            starts += 1;
        }
        activated = next;
    }
    assert!(activated, "terminal ON must leave the lane activated");
    // ON,OFF,ON,OFF,ON observed as distinct polls = 3 rising edges = 3 starts; the
    // point is each Start is paired with an intervening Stop — never a double-Start.
    assert_eq!(
        starts, 3,
        "each ON-after-OFF is exactly one Start, no double-Start"
    );
}

#[test]
fn groww_toggle_storm_converges_to_final_off() {
    let storm = [true, false, true, false]; // final = OFF
    let mut activated = false;
    for &desired in &storm {
        let (_, next) = step_groww(desired, activated, false, activated);
        activated = next;
    }
    assert!(!activated, "terminal OFF must leave the lane NOT activated");
}

// ───────────────────────── crash-on-start / dead-activation ──────────────────

#[test]
fn groww_dead_activation_triggers_exactly_one_restart() {
    // Crash-on-start: the activation task finished WITHOUT bringing the lane up
    // (panic / early return). The watchdog must re-Start exactly once, then — once
    // the re-Start marks the lane running — settle to None (no infinite re-Start).
    let mut activated = true; // a task was spawned for this ON period…
    // Poll 1: desired ON, task finished, lane NOT running → dead → re-Start.
    let (a1, n1) = step_groww(
        true, activated, /*finished*/ true, /*running*/ false,
    );
    assert_eq!(
        a1,
        groww_activation::LaneAction::Start,
        "dead lane must re-Start"
    );
    activated = n1;
    assert!(activated, "re-Start marks the lane activated again");
    // Poll 2: the re-Started task is now healthy + running → None (no second Start).
    let (a2, n2) = step_groww(
        true, activated, /*finished*/ false, /*running*/ true,
    );
    assert_eq!(
        a2,
        groww_activation::LaneAction::None,
        "healthy lane must not re-Start"
    );
    assert!(n2, "lane stays activated");
}

#[test]
fn dead_activation_never_restarts_a_running_lane() {
    // Success path: a task that finished AFTER marking the lane running is NOT
    // dead — re-starting it would needlessly rebuild a live lane.
    // (PR-C2, 2026-07-13: the Dhan arm retired with dhan_activation.rs — see the
    // module-header retirement note; the Groww arm carries the contract.)
    assert!(!groww_is_dead(
        true, /*finished*/ true, /*running*/ true
    ));
    // And the watcher step confirms: desired ON, finished, running → None.
    let (g_action, _) = step_groww(true, true, true, true);
    assert_eq!(g_action, groww_activation::LaneAction::None);
}

// ───────────────────────────── double-toggle idempotency ─────────────────────

#[test]
fn groww_double_enable_is_idempotent() {
    let mut activated = false;
    let (a1, n1) = step_groww(true, activated, false, activated);
    assert_eq!(
        a1,
        groww_activation::LaneAction::Start,
        "first enable Starts"
    );
    activated = n1;
    let (a2, n2) = step_groww(true, activated, false, activated);
    assert_eq!(
        a2,
        groww_activation::LaneAction::None,
        "second identical enable is a no-op"
    );
    assert!(n2, "still activated, no second Start");
}

#[test]
fn groww_double_disable_is_idempotent() {
    let mut activated = true;
    let (a1, n1) = step_groww(false, activated, false, true);
    assert_eq!(
        a1,
        groww_activation::LaneAction::Stop,
        "first disable Stops"
    );
    activated = n1;
    let (a2, n2) = step_groww(false, activated, false, false);
    assert_eq!(
        a2,
        groww_activation::LaneAction::None,
        "second identical disable is a no-op"
    );
    assert!(!n2, "still stopped, no second Stop");
}

#[test]
fn feed_runtime_lane_flag_round_trips_idempotently() {
    // The UI flag setters must round-trip and be idempotent on repeat — setting the
    // same value twice has no extra effect (the feed page never lies after a repeat).
    // PR-C2 (2026-07-13): the Dhan lane-running FLAG surface survives the lane
    // deletion (the /feeds page + the 409 Dhan-enable refusal still read it), so
    // BOTH feeds' flag round-trips stay pinned here.
    let state = FeedRuntimeState::default();
    for feed in [Feed::Dhan, Feed::Groww] {
        // Start NOT running (default mirrors prod for both lanes).
        assert!(!state.lane_running(feed));
    }
    // Groww: true, true (idempotent), false, false (idempotent).
    state.set_groww_lane_running(true);
    assert!(state.is_groww_lane_running());
    state.set_groww_lane_running(true);
    assert!(state.is_groww_lane_running(), "repeat-true is idempotent");
    state.set_groww_lane_running(false);
    assert!(!state.is_groww_lane_running());
    state.set_groww_lane_running(false);
    assert!(!state.is_groww_lane_running(), "repeat-false is idempotent");
    // Dhan: same round-trip (flag only — no reconciler exists since PR-C2).
    state.set_dhan_lane_running(true);
    assert!(state.is_dhan_lane_running());
    state.set_dhan_lane_running(true);
    assert!(state.is_dhan_lane_running(), "repeat-true is idempotent");
    state.set_dhan_lane_running(false);
    assert!(!state.is_dhan_lane_running());
    state.set_dhan_lane_running(false);
    assert!(!state.is_dhan_lane_running(), "repeat-false is idempotent");
}

// ───────────────────────── no-work-while-OFF invariant (#1192) ────────────────

#[test]
fn off_groww_feed_takes_no_start_action_in_any_state() {
    // An OFF Groww feed must NEVER emit Start, regardless of the activated/running
    // bits the watcher could legally be in while OFF — so the dormant watcher takes
    // ZERO cold-start action while the feed is OFF (operator lock 2026-06-23).
    for &activated in &[false, true] {
        for &task_finished in &[false, true] {
            for &lane_running in &[false, true] {
                let (action, _) = step_groww(false, activated, task_finished, lane_running);
                assert_ne!(
                    action,
                    groww_activation::LaneAction::Start,
                    "OFF Groww feed must never Start (activated={activated}, \
                     finished={task_finished}, running={lane_running})"
                );
            }
        }
    }
}
