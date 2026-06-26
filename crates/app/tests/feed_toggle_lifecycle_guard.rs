//! Feed-toggle full-lifecycle guard (PR-4 of feed-toggle-full-lifecycle plan).
//!
//! Operator lock chain: 2026-06-23 ("until the feed is ON, the entire
//! architecture for that feed must never be touched") + 2026-06-24 ("when I
//! enable on/off the entire mechanism should run entirely for that feed").
//!
//! PR-1/PR-2 shipped the dormant, level-triggered activation watchers
//! (`groww_activation` / `dhan_activation`) whose decisions are PURE:
//! `reconcile_lane_action`, `reconcile_dhan_lane_action_with_gate`,
//! `is_dead_activation`, `start_marks_running`. PR-1/PR-2 unit tests cover those
//! decisions POINT-WISE (single inputs). The remaining regression class is
//! BEHAVIOURAL-OVER-A-SEQUENCE: a rapid toggle storm, a crash-on-start re-Start,
//! a double toggle, a disable storm under the live-trading safety gate. This
//! module drives the SAME pure decisions through hostile SEQUENCES and asserts
//! the watcher converges + is idempotent — proving the four real failure modes
//! (missed edge, double-spawn, dead-lane-never-restarted, torn-down-mid-trade)
//! cannot regress.
//!
//! ## Honest envelope (no illusion)
//! These are PURE-function sequence guards — they model ONE watcher loop
//! iteration WITHOUT I/O (no live QuestDB / Dhan / Groww / tokio supervisor).
//! That is the correct altitude: the decision logic is what regresses; the live
//! supervisor is exercised by the app's own boot path. The #1192 source-scan
//! guard (`per_feed_boot_isolation_guard.rs`) pins the spawn SHAPE; PR-4 adds
//! the behavioural SEQUENCE proofs on top.

use tickvault_api::feed_state::{Feed, FeedRuntimeState};
use tickvault_app::dhan_activation::{
    self, is_dead_activation as dhan_is_dead, reconcile_dhan_lane_action_with_gate,
    start_marks_running,
};
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

/// One Dhan watcher step WITH the disable safety gate. `disable_allowed` mirrors
/// `can_disable_dhan()`; `lane_running` drives the defensive stale-flag watchdog.
/// `pool_present` gates whether a `Start` actually marks the lane running
/// (boot-OFF → false-OK closed). Returns `(action, next_activated)`.
fn step_dhan(
    desired: bool,
    activated: bool,
    disable_allowed: bool,
    lane_running: bool,
    pool_present: bool,
) -> (dhan_activation::LaneAction, bool) {
    let activated = if dhan_is_dead(desired, activated, lane_running) {
        false
    } else {
        activated
    };
    let action = reconcile_dhan_lane_action_with_gate(desired, activated, disable_allowed);
    let next = match action {
        // A Start only marks running when a real pool exists (boot-ON); otherwise
        // the desired flag is recorded but the lane stays NOT activated (no false-OK).
        dhan_activation::LaneAction::Start => start_marks_running(pool_present),
        dhan_activation::LaneAction::Stop => false,
        dhan_activation::LaneAction::None => activated,
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

#[test]
fn dhan_toggle_storm_converges_with_gate_open() {
    // Gate OPEN (no-orders phase) → disable is permitted, so the storm behaves
    // exactly like Groww's: converges to the final desired value.
    let storm = [true, false, true]; // final = ON, gate open, pool present
    let mut activated = false;
    for &desired in &storm {
        let (_, next) = step_dhan(desired, activated, true, activated, true);
        activated = next;
    }
    assert!(
        activated,
        "terminal ON with gate open leaves the lane activated"
    );
}

#[test]
fn dhan_toggle_storm_no_double_start() {
    // Repeated ON polls after activation must NOT re-emit Start (the watcher does
    // not re-touch a running lane). Boot-ON (pool present), gate open.
    let mut activated = false;
    let mut starts = 0u32;
    for _ in 0..5 {
        let (action, next) = step_dhan(true, activated, true, activated, true);
        if action == dhan_activation::LaneAction::Start {
            starts += 1;
        }
        activated = next;
    }
    assert_eq!(
        starts, 1,
        "a sustained-ON Dhan lane Starts exactly once, never re-Starts"
    );
    assert!(activated);
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
fn dhan_stale_flag_reconverges_then_idles() {
    // Dhan defensive stale-flag: desired ON, was activated, but the running flag
    // went false from some OTHER actor → treat as not-activated → Start once →
    // then settle to None. Boot-ON (pool present), gate open.
    let mut activated = true;
    let (a1, n1) = step_dhan(true, activated, true, /*running*/ false, true);
    assert_eq!(
        a1,
        dhan_activation::LaneAction::Start,
        "stale flag must re-converge via Start"
    );
    activated = n1;
    assert!(activated);
    let (a2, _) = step_dhan(true, activated, true, /*running*/ true, true);
    assert_eq!(
        a2,
        dhan_activation::LaneAction::None,
        "re-converged lane idles"
    );
}

#[test]
fn dead_activation_never_restarts_a_running_lane() {
    // Success path for BOTH feeds: a task that finished AFTER marking the lane
    // running is NOT dead — re-starting it would needlessly rebuild a live lane.
    assert!(!groww_is_dead(
        true, /*finished*/ true, /*running*/ true
    ));
    assert!(!dhan_is_dead(
        true, /*was_activated*/ true, /*running*/ true
    ));
    // And the watcher step confirms: desired ON, finished, running → None.
    let (g_action, _) = step_groww(true, true, true, true);
    assert_eq!(g_action, groww_activation::LaneAction::None);
    let (d_action, _) = step_dhan(true, true, true, true, true);
    assert_eq!(d_action, dhan_activation::LaneAction::None);
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
fn dhan_double_enable_is_idempotent() {
    let mut activated = false;
    let (a1, n1) = step_dhan(true, activated, true, activated, true);
    assert_eq!(
        a1,
        dhan_activation::LaneAction::Start,
        "first enable Starts"
    );
    activated = n1;
    let (a2, n2) = step_dhan(true, activated, true, activated, true);
    assert_eq!(
        a2,
        dhan_activation::LaneAction::None,
        "second identical enable is a no-op"
    );
    assert!(n2);
}

#[test]
fn dhan_double_disable_is_idempotent() {
    // Gate OPEN so the disable is permitted.
    let mut activated = true;
    let (a1, n1) = step_dhan(false, activated, true, true, true);
    assert_eq!(a1, dhan_activation::LaneAction::Stop, "first disable Stops");
    activated = n1;
    let (a2, n2) = step_dhan(false, activated, true, false, true);
    assert_eq!(
        a2,
        dhan_activation::LaneAction::None,
        "second identical disable is a no-op"
    );
    assert!(!n2);
}

#[test]
fn feed_runtime_lane_flag_round_trips_idempotently() {
    // The UI flag setters must round-trip and be idempotent on repeat — setting the
    // same value twice has no extra effect (the feed page never lies after a repeat).
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
    // Dhan: same round-trip.
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

#[test]
fn off_dhan_feed_takes_no_start_action_even_with_gate_states() {
    // An OFF Dhan feed must NEVER emit Start across every gate / activated / running
    // combination. Disabling may yield None (gate closed) or Stop (gate open) — but
    // a Start (cold-start work) while OFF is structurally impossible.
    for &disable_allowed in &[false, true] {
        for &activated in &[false, true] {
            for &lane_running in &[false, true] {
                let (action, _) = step_dhan(false, activated, disable_allowed, lane_running, true);
                assert_ne!(
                    action,
                    dhan_activation::LaneAction::Start,
                    "OFF Dhan feed must never Start (gate={disable_allowed}, \
                     activated={activated}, running={lane_running})"
                );
            }
        }
    }
}

// ───────────────────────── Dhan disable-storm safety gate ─────────────────────

#[test]
fn dhan_disable_storm_never_tears_down_while_live() {
    // Live trading on (disable_allowed=false): a disable storm must NEVER tear the
    // lane down — every Stop is downgraded to None, so a running Dhan lane stays
    // running across the whole flap (the feed can never be blinded mid-trade).
    let mut activated = true;
    for _ in 0..5 {
        let (action, next) =
            step_dhan(false, activated, /*disable_allowed*/ false, true, true);
        assert_eq!(
            action,
            dhan_activation::LaneAction::None,
            "disable while live trading is on must downgrade Stop to None"
        );
        activated = next;
        assert!(activated, "the live Dhan lane is never torn down mid-trade");
    }
}

#[test]
fn dhan_disable_storm_tears_down_once_gate_opens() {
    // The same desired-OFF storm: refused while the gate is CLOSED, then the moment
    // the gate re-opens (back to the no-orders phase) the teardown DOES fire once.
    let mut activated = true;
    // Three polls with the gate closed — no teardown.
    for _ in 0..3 {
        let (action, next) = step_dhan(false, activated, false, true, true);
        assert_eq!(action, dhan_activation::LaneAction::None);
        activated = next;
    }
    assert!(activated, "still running while the gate was closed");
    // Gate opens → Stop fires once.
    let (action, next) = step_dhan(false, activated, /*disable_allowed*/ true, true, true);
    assert_eq!(
        action,
        dhan_activation::LaneAction::Stop,
        "teardown fires once the gate opens"
    );
    assert!(!next, "lane torn down");
}
