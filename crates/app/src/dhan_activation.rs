//! Dhan runtime activation watcher — the dormant supervisor that keeps the Dhan
//! lane's `dhan_lane_running` UI flag HONEST across runtime toggles, and enforces
//! the Dhan-disable safety gate at the supervisor layer (operator 2026-06-21 /
//! 2026-06-24: "if I want to switch off or on dhan also it should be accepted",
//! "when I enable on/off the entire mechanism and its architecture should run
//! entirely for that feed").
//!
//! ## Why this exists (and the HONEST boundary — no illusion)
//! PR-E (#1170) already made Dhan runtime-toggleable for the **boot-ON** case via
//! *in-loop dormancy*: each Dhan `WebSocketConnection` carries the shared enable
//! flag (`with_feed_enable_flag` in `connection.rs`) and, when the flag flips
//! `false`, the `run()` loop CLOSES the socket and parks, reconnecting when it
//! flips back `true`. So a Dhan feed ENABLED at boot fully disconnects on OFF and
//! reconnects on ON — live, no restart. What PR-E did NOT do is keep the
//! `dhan_lane_running` UI flag (read by the `/feeds` page) truthful across those
//! runtime toggles, and it did NOT enforce the disable safety gate anywhere but
//! the HTTP handler.
//!
//! This watcher fills BOTH gaps with the SAME level-triggered-reconciler shape as
//! the Groww watcher (`groww_activation.rs`):
//!   * desired ON  & lane not marked running → `start_dhan_lane` (mark running;
//!     the shared flag the WS loop reads is already ON, so the pool reconnects).
//!   * desired OFF & lane marked running     → `stop_dhan_lane` (mark stopped) —
//!     BUT ONLY IF the Dhan-disable safety gate permits it. While live trading is
//!     on (`can_disable_dhan() == false`) a `Stop` is DOWNGRADED to `None`, so the
//!     supervisor refuses to tear the lane's UI state down mid-trade. This is a
//!     SECOND layer behind the handler's `CONFLICT` rejection — defence in depth.
//!
//! ## The deferred residual (documented honestly — NOT a skeleton, false-OK CLOSED)
//! The FULL boot-OFF → cold-start of the ~4000-line inline Dhan boot spine
//! (auth → daily-universe build → WS-pool spawn, which lives inline in
//! `crates/app/src/main.rs`) is NOT lifted out of `main()` by this PR. That lift
//! is a multi-day refactor on the SEBI-critical trading spine and is the explicit
//! residual of the feed-toggle-full-lifecycle plan. Until it lands, a Dhan feed
//! that was OFF *at boot* has no pool to start.
//!
//! **The boot-OFF false-OK is now CLOSED in code (PR-2 review fix), not just
//! prose.** `FeedRuntimeState` carries a `dhan_pool_present` sentinel set TRUE
//! only by the inline boot spine when it actually spawns the pool (boot-ON). On a
//! boot-OFF run it stays false, and this watcher's `Start` path REFUSES to mark
//! the lane running when it is false — so `dhan_lane_running` can never report
//! `true` with zero connections / zero ticks. Instead it logs an honest INFO that
//! a restart with `dhan_enabled=true` is required (the `feeds.rs` handler + the
//! `/feeds` page meta text say the same). What this watcher DOES guarantee today
//! (real work, real call site — anti-pattern Rule 14 respected) is: (a) the
//! `dhan_lane_running` flag is truthful both ways on every runtime toggle of a
//! boot-ON Dhan feed (and never falsely true on a boot-OFF feed), and (b) the
//! disable safety gate is enforced in the supervisor, not just the handler.
//!
//! ## Enabled-default is byte-identical
//! The inline Dhan boot block in `main()` is UNCHANGED. When `dhan_enabled=true`
//! at boot (the production default) the inline spine runs exactly as before and
//! calls `mark_dhan_lane_running()`. This watcher runs ALONGSIDE it; on its first
//! tick it observes desired-ON + already-running → `LaneAction::None` → it does
//! NOTHING to the boot (no re-auth, no second pool, no reordering). The watcher
//! only ACTS on a *runtime* toggle.

use std::sync::Arc;
use std::time::Duration;

use tickvault_api::feed_state::{Feed, FeedRuntimeState};
use tracing::info;

/// Poll cadence for the enable flag. This is the COLD control-plane (a feed
/// toggle), NOT the hot tick path — a 2s observation latency on an operator
/// toggle is irrelevant, and the loop does zero feed work while idle. Being
/// level-triggered, a flap faster than this window simply converges to the
/// final state (no missed-edge bug). Mirrors `GROWW_ACTIVATION_POLL`.
const DHAN_ACTIVATION_POLL_SECS: u64 = 2;
const DHAN_ACTIVATION_POLL: Duration = Duration::from_secs(DHAN_ACTIVATION_POLL_SECS);

/// What the reconciler decides to do this tick, given the desired enable state vs
/// whether the lane is currently marked running. Pure (unit-tested) so the watcher
/// loop stays a trivial poll around this decision. Level-triggered: it compares
/// DESIRED vs CURRENT, never PREV vs NOW — so a sub-poll ON→OFF→ON flap cannot
/// drop an activation (it converges to the final desired state).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LaneAction {
    /// Desired state already matches the lane state — do nothing.
    None,
    /// Lane should run but isn't marked running yet — start the lane.
    Start,
    /// Lane is marked running but should be off — stop the lane.
    Stop,
}

/// Decide the reconcile action WITHOUT the safety gate. `desired_enabled` = the
/// live Dhan flag; `currently_activated` = whether the watcher has marked the
/// lane running for the current ON period. Pure + total. Identical semantics to
/// the Groww reconciler — kept separate (not the gated one) so the gate-free
/// decision is independently testable and the gate is an explicit overlay.
#[must_use]
pub fn reconcile_lane_action(desired_enabled: bool, currently_activated: bool) -> LaneAction {
    match (desired_enabled, currently_activated) {
        (true, false) => LaneAction::Start,
        (false, true) => LaneAction::Stop,
        _ => LaneAction::None,
    }
}

/// Decide the reconcile action WITH the Dhan-disable safety gate
/// (websocket-connection-scope-lock PR-E, operator-authorized 2026-06-21).
///
/// The ONLY dangerous direction is DISABLING Dhan (it blinds the system). When
/// `disable_allowed == false` (live trading is on — orders/positions open) a
/// `Stop` is DOWNGRADED to `None`, so the supervisor refuses to tear the lane
/// down mid-trade. ENABLING Dhan is NEVER gated (a `Start` is always allowed).
/// This is the SECOND layer behind the handler's `CONFLICT` rejection.
///
/// Pure + total so the watcher loop stays a trivial poll around this decision.
#[must_use]
pub fn reconcile_dhan_lane_action_with_gate(
    desired_enabled: bool,
    currently_activated: bool,
    disable_allowed: bool,
) -> LaneAction {
    match reconcile_lane_action(desired_enabled, currently_activated) {
        // The teardown direction is the safety-critical one: refuse it while live
        // trading is on so the feed can never be blinded mid-trade.
        LaneAction::Stop if !disable_allowed => LaneAction::None,
        other => other,
    }
}

/// Detect a STALE running flag: desired ON, the lane was marked running, yet the
/// running flag has since gone false WITHOUT this reconciler's `Stop` having
/// cleared it. Pure + total. (Same SHAPE as `groww_activation::is_dead_activation`,
/// but the threat model differs: the Groww watcher owns an abortable JoinHandle
/// whose task can panic/early-return out from under it, so its check guards a REAL
/// blind-spot. THIS watcher spawns no task and only mutates the `FeedRuntimeState`
/// atomics — the ONLY in-process writer of `dhan_lane_running=false` is this
/// reconciler's own `Stop` arm (which already clears `activated`). So here this is
/// DEFENSIVE-ONLY: it re-converges if some OTHER actor (a future code path, or an
/// external `set_dhan_lane_running(false)`) clears the flag while desired-ON. It is
/// NOT guarding a panic/early-return — there is no task to die.)
///
/// Returns true ONLY for `(desired=ON, was_activated, lane NOT running)` — a lane
/// that is running (the success path) is NOT dead and must NOT be re-started.
#[must_use]
pub fn is_dead_activation(desired_enabled: bool, was_activated: bool, lane_running: bool) -> bool {
    desired_enabled && was_activated && !lane_running
}

/// PR-2 false-OK gate (pure + total, unit-tested): may a `Start` action mark the
/// Dhan lane running THIS process? ONLY if a real main-feed pool was spawned at
/// boot (`pool_present`). On a boot-OFF run no pool exists — PR-E's in-loop
/// dormancy can RESUME a pool but cannot CREATE one — so marking the lane running
/// would lie (`dhan_lane_running=true` with zero connections). `false` here means
/// "record the desired flag only; lane stays NOT running; restart required".
#[must_use]
pub fn start_marks_running(pool_present: bool) -> bool {
    pool_present
}

/// Mark the Dhan lane running. Idempotent. The shared enable flag the core WS loop
/// reads is already ON at this point (the API/runtime toggle flipped it before
/// this reconcile saw `desired=ON`), so the pool reconnects via PR-E's in-loop
/// dormancy; this call only makes the `/feeds` page report the truth. Caller MUST
/// have checked [`start_marks_running`] first (boot-ON only).
fn start_dhan_lane(feed_runtime: &FeedRuntimeState) {
    feed_runtime.set_dhan_lane_running(true);
}

/// Mark the Dhan lane stopped. Idempotent. Called ONLY when the safety gate has
/// already permitted the teardown (the reconciler downgraded `Stop`→`None`
/// otherwise). The shared enable flag is already OFF, so the WS loop has parked;
/// this call clears the stale "running" so the `/feeds` page reports honestly.
fn stop_dhan_lane(feed_runtime: &FeedRuntimeState) {
    feed_runtime.set_dhan_lane_running(false);
}

/// Run the Dhan activation watcher for the process lifetime. Spawned
/// UNCONDITIONALLY at boot. Level-triggered: it reconciles the live
/// `is_enabled(Feed::Dhan)` flag (gated by `can_disable_dhan()` on the disable
/// direction) against the lane's marked-running state every poll, so a Dhan feed
/// ENABLED at boot is observed already-running on the first tick (the inline boot
/// marked it) → `None` → the boot is untouched; a runtime toggle is reflected on
/// the next tick; a quick ON→OFF→ON flap converges to the final state; and the
/// dead-lane watchdog re-marks a desired-ON lane whose flag silently went false.
///
/// It mutates ONLY the `FeedRuntimeState` UI/safety atomics — it adds NO Dhan
/// WebSocket connection or endpoint (the 2-WS Dhan lock is untouched). The pure
/// decisions it loops around (`reconcile_dhan_lane_action_with_gate`,
/// `is_dead_activation`) are fully unit-tested below.
// TEST-EXEMPT: infinite control-plane poll loop; pure decisions (reconcile_dhan_lane_action_with_gate, is_dead_activation) unit-tested below.
pub async fn run_dhan_activation_watcher(feed_runtime: Arc<FeedRuntimeState>) {
    // Tracks whether THIS watcher has marked the lane running for the current ON
    // period, so the reconcile compares desired vs activated (level-triggered).
    let mut activated = feed_runtime.is_dhan_lane_running();
    // Edge-triggers the boot-OFF "no pool, restart required" INFO so a desired-ON
    // run that has no pool logs ONCE per ON period, not every 2s poll. Reset on
    // any teardown / desired-OFF so a later re-enable logs again.
    let mut warned_boot_off = false;
    loop {
        let desired = feed_runtime.is_enabled(Feed::Dhan);
        let disable_allowed = feed_runtime.can_disable_dhan();

        // Stale-flag watchdog (DEFENSIVE-ONLY — see is_dead_activation docs): the
        // only in-process writer of dhan_lane_running=false is THIS reconciler's
        // own Stop arm, which already clears `activated`; there is NO task here
        // that can panic out from under us (unlike the Groww watcher). This guard
        // simply re-converges if some OTHER actor clears the flag while desired-ON
        // by treating it as not-activated so this tick re-Starts it. A lane still
        // running (success path) is left as-is.
        if is_dead_activation(desired, activated, feed_runtime.is_dhan_lane_running()) {
            activated = false;
        }

        match reconcile_dhan_lane_action_with_gate(desired, activated, disable_allowed) {
            LaneAction::Start => {
                // PR-2 false-OK fix: only mark the lane running if a REAL Dhan
                // main-feed pool was spawned at boot (boot-ON). PR-E's in-loop
                // dormancy can only RESUME a pool that exists — it cannot create
                // one. On a boot-OFF run (Dhan disabled at boot, e.g. Groww-only)
                // there is no pool, so marking the lane running would lie
                // (`dhan_lane_running=true` with zero connections / zero ticks).
                // Keep the flag false and tell the operator honestly that a
                // restart with `dhan_enabled=true` is the documented path — this
                // is the deferred-residual boundary, now enforced in code.
                if start_marks_running(feed_runtime.is_dhan_pool_present()) {
                    info!(
                        "[feeds] Dhan enabled — marking lane running; the main-feed pool \
                         reconnects via the shared enable flag (PR-E in-loop dormancy)"
                    );
                    start_dhan_lane(&feed_runtime);
                    activated = true;
                } else {
                    // Edge-triggered: log once per ON period (activated stays
                    // false so this arm is re-entered each poll while desired-ON;
                    // gate the log on the not-yet-warned transition). The desired
                    // flag is already ON (the API toggle recorded it), so the page
                    // shows Dhan enabled — but lane_running stays false (honest),
                    // and the /feeds page meta text already says "set it on in
                    // config and restart to actually stream".
                    if !warned_boot_off {
                        info!(
                            "[feeds] Dhan enabled at runtime, but no main-feed pool was \
                             spawned at boot (Dhan was OFF at boot) — runtime cold-start \
                             from cold is the deferred residual of feed-toggle-full-lifecycle. \
                             The desired flag is recorded, but the lane stays NOT running \
                             (no false-OK); a restart with dhan_enabled=true is required to \
                             actually stream."
                        );
                        warned_boot_off = true;
                    }
                }
            }
            LaneAction::Stop => {
                info!(
                    "[feeds] Dhan disabled (safety gate permits) — marking lane \
                     stopped; the main-feed pool parks via the shared enable flag"
                );
                stop_dhan_lane(&feed_runtime);
                activated = false;
                warned_boot_off = false;
            }
            LaneAction::None => {
                // Reset the boot-OFF warn latch whenever the feed is desired-OFF
                // so a later runtime re-enable on a (still pool-less) boot-OFF run
                // logs the honest "restart required" line again, edge-triggered.
                if !desired {
                    warned_boot_off = false;
                }
            }
        }
        tokio::time::sleep(DHAN_ACTIVATION_POLL).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reconcile_dhan_lane_action_start_when_desired_and_not_activated() {
        assert_eq!(reconcile_lane_action(true, false), LaneAction::Start);
    }

    #[test]
    fn reconcile_dhan_lane_action_stop_when_not_desired_and_activated() {
        assert_eq!(reconcile_lane_action(false, true), LaneAction::Stop);
    }

    #[test]
    fn reconcile_dhan_lane_action_none_when_already_converged() {
        assert_eq!(reconcile_lane_action(true, true), LaneAction::None);
        assert_eq!(reconcile_lane_action(false, false), LaneAction::None);
    }

    #[test]
    fn reconcile_dhan_lane_action_boot_enabled_starts_once_then_idles() {
        // Boot with Dhan enabled: first tick has activated=false → Start, then
        // activated=true → None (the watcher does not re-touch a running lane).
        let mut activated = false;
        assert_eq!(reconcile_lane_action(true, activated), LaneAction::Start);
        activated = true;
        assert_eq!(reconcile_lane_action(true, activated), LaneAction::None);
    }

    #[test]
    fn reconcile_dhan_lane_action_sub_poll_flap_converges_no_missed_edge() {
        // The reconciler reads DESIRED vs ACTIVATED, never PREV vs NOW — so an
        // ON→OFF→ON flap within one poll window is seen only as its FINAL value.
        assert_eq!(reconcile_lane_action(true, false), LaneAction::Start);
        assert_eq!(reconcile_lane_action(false, false), LaneAction::None);
    }

    #[test]
    fn reconcile_dhan_lane_action_with_gate_stop_allowed_when_disable_permitted() {
        // No-orders data-pull phase: disabling Dhan is allowed → Stop passes through.
        assert_eq!(
            reconcile_dhan_lane_action_with_gate(false, true, true),
            LaneAction::Stop
        );
    }

    #[test]
    fn reconcile_dhan_lane_action_with_gate_stop_refused_when_live_trading() {
        // Live trading on (disable_allowed=false): the supervisor REFUSES the
        // teardown — Stop is downgraded to None so the feed is never blinded.
        assert_eq!(
            reconcile_dhan_lane_action_with_gate(false, true, false),
            LaneAction::None
        );
    }

    #[test]
    fn reconcile_dhan_lane_action_with_gate_start_never_gated() {
        // Enabling Dhan is the safe direction — never gated, even with the
        // disable gate closed.
        assert_eq!(
            reconcile_dhan_lane_action_with_gate(true, false, false),
            LaneAction::Start
        );
        assert_eq!(
            reconcile_dhan_lane_action_with_gate(true, false, true),
            LaneAction::Start
        );
    }

    #[test]
    fn dhan_disable_refused_while_orders_live() {
        // Named per the PR-2 plan: the one decision that protects a live trade —
        // a desired-OFF, currently-running Dhan lane with the disable gate CLOSED
        // resolves to None (no teardown). This is the supervisor-layer half of the
        // two-layer safety gate (the handler returns CONFLICT for the other half).
        assert_eq!(
            reconcile_dhan_lane_action_with_gate(false, true, false),
            LaneAction::None,
            "disabling Dhan while live trading is on must NOT tear the lane down"
        );
        // And the moment the gate re-opens (back to the no-orders phase) the same
        // desired-OFF state DOES resolve to Stop.
        assert_eq!(
            reconcile_dhan_lane_action_with_gate(false, true, true),
            LaneAction::Stop
        );
    }

    #[test]
    fn is_dead_activation_true_when_was_activated_and_lane_not_running() {
        // Panic / early-return blind-spot: desired ON, was activated, running flag
        // went false → dead lane, must re-start.
        assert!(is_dead_activation(true, true, false));
    }

    #[test]
    fn is_dead_activation_false_on_success_path() {
        // Lane still running → success, NOT dead; re-starting is needless.
        assert!(!is_dead_activation(true, true, true));
    }

    #[test]
    fn is_dead_activation_false_when_not_yet_activated() {
        // Never marked running this ON period → not "dead", just not started yet.
        assert!(!is_dead_activation(true, false, false));
        assert!(!is_dead_activation(true, false, true));
    }

    #[test]
    fn is_dead_activation_false_when_disabled() {
        // Desired OFF → the Stop path (gated) owns teardown; the dead-lane
        // watchdog must never fire (it only re-starts a DESIRED-ON lane).
        assert!(!is_dead_activation(false, true, false));
        assert!(!is_dead_activation(false, true, true));
        assert!(!is_dead_activation(false, false, false));
    }

    #[test]
    fn start_marks_running_is_the_pool_present_sentinel() {
        // Pure gate: a Start may mark the lane running iff a real pool exists.
        assert!(
            start_marks_running(true),
            "boot-ON: pool exists → mark running"
        );
        assert!(
            !start_marks_running(false),
            "boot-OFF: no pool → must NOT mark running (no false-OK)"
        );
    }

    #[test]
    fn test_boot_off_toggle_on_does_not_falsely_mark_running() {
        // FIX 1 regression: on a boot-OFF run the inline Dhan spine never spawned a
        // pool, so `dhan_pool_present` stays false. A runtime enable (operator flips
        // Dhan ON via /api/feeds) makes the reconciler emit `Start` — but the gate
        // MUST refuse to mark the lane running, because there is no pool to stream.
        // Reporting `dhan_lane_running=true` here would be a lie (zero connections /
        // zero ticks). This test exercises the EXACT gate the watcher loop applies.
        let state = FeedRuntimeState::default();
        // boot-OFF: pool was never marked present at boot.
        assert!(
            !state.is_dhan_pool_present(),
            "default/boot-OFF: no pool present"
        );
        // Operator toggles Dhan ON at runtime → desired enabled.
        state.set_enabled(Feed::Dhan, true);
        // Reconciler decides Start (desired ON, not yet activated, gate open).
        assert_eq!(
            reconcile_dhan_lane_action_with_gate(true, false, true),
            LaneAction::Start
        );
        // The watcher's gate: Start does NOT mark running when no pool exists.
        assert!(
            !start_marks_running(state.is_dhan_pool_present()),
            "boot-OFF Start must NOT mark the lane running"
        );
        // Apply the gate exactly as the loop does: because the gate is false, the
        // lane flag is left untouched.
        if start_marks_running(state.is_dhan_pool_present()) {
            state.set_dhan_lane_running(true);
        }
        assert!(
            !state.is_dhan_lane_running(),
            "false-OK CLOSED: lane stays NOT running without a real pool"
        );

        // Contrast — boot-ON: the inline spine marked the pool present, so the SAME
        // Start DOES mark the lane running (byte-identical to the pre-fix boot-ON).
        let booted = FeedRuntimeState::default();
        booted.mark_dhan_pool_present();
        assert!(
            start_marks_running(booted.is_dhan_pool_present()),
            "boot-ON Start marks the lane running (a real pool exists)"
        );
        if start_marks_running(booted.is_dhan_pool_present()) {
            booted.set_dhan_lane_running(true);
        }
        assert!(
            booted.is_dhan_lane_running(),
            "boot-ON behaviour preserved: lane runs when a pool exists"
        );
    }

    #[test]
    fn dhan_activation_poll_is_cold_control_plane() {
        // A feed toggle is cold control-plane; 2s observation latency is fine and
        // the loop does zero Dhan work while idle.
        assert_eq!(DHAN_ACTIVATION_POLL, Duration::from_secs(2));
    }
}
