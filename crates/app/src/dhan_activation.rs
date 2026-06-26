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
//! ## The deferred residual (documented honestly — NOT a skeleton)
//! The FULL boot-OFF → cold-start of the ~4000-line inline Dhan boot spine
//! (auth → daily-universe build → WS-pool spawn, which lives inline in
//! `crates/app/src/main.rs`) is NOT lifted out of `main()` by this PR. That lift
//! is a multi-day refactor on the SEBI-critical trading spine and is the explicit
//! residual of the feed-toggle-full-lifecycle plan. Until it lands, a Dhan feed
//! that was OFF *at boot* has no pool to start — the `feeds.rs` handler reports
//! this honestly. What this watcher DOES guarantee today (real work, real call
//! site — anti-pattern Rule 14 respected) is: (a) the `dhan_lane_running` flag is
//! truthful both ways on every runtime toggle of a boot-ON Dhan feed, and (b) the
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

/// Detect a DEAD lane: desired ON, the lane was marked running, yet the running
/// flag has since gone false WITHOUT a `Stop` having cleared it — the panic /
/// early-return blind-spot. Pure + total. Mirrors `groww_activation::is_dead_activation`.
///
/// Returns true ONLY for `(desired=ON, was_activated, lane NOT running)` — a lane
/// that is running (the success path) is NOT dead and must NOT be re-started.
#[must_use]
pub fn is_dead_activation(desired_enabled: bool, was_activated: bool, lane_running: bool) -> bool {
    desired_enabled && was_activated && !lane_running
}

/// Mark the Dhan lane running. Idempotent. The shared enable flag the core WS loop
/// reads is already ON at this point (the API/runtime toggle flipped it before
/// this reconcile saw `desired=ON`), so the pool reconnects via PR-E's in-loop
/// dormancy; this call only makes the `/feeds` page report the truth.
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
    loop {
        let desired = feed_runtime.is_enabled(Feed::Dhan);
        let disable_allowed = feed_runtime.can_disable_dhan();

        // Dead-lane watchdog (panic / early-return blind-spot): if the lane was
        // marked running but the running flag has gone false while still desired
        // ON, re-converge by treating it as not-activated so this tick re-Starts
        // it. A lane still running (success path) is left as-is.
        if is_dead_activation(desired, activated, feed_runtime.is_dhan_lane_running()) {
            activated = false;
        }

        match reconcile_dhan_lane_action_with_gate(desired, activated, disable_allowed) {
            LaneAction::Start => {
                info!(
                    "[feeds] Dhan enabled — marking lane running; the main-feed pool \
                     reconnects via the shared enable flag (PR-E in-loop dormancy)"
                );
                start_dhan_lane(&feed_runtime);
                activated = true;
            }
            LaneAction::Stop => {
                info!(
                    "[feeds] Dhan disabled (safety gate permits) — marking lane \
                     stopped; the main-feed pool parks via the shared enable flag"
                );
                stop_dhan_lane(&feed_runtime);
                activated = false;
            }
            LaneAction::None => {}
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
    fn dhan_activation_poll_is_cold_control_plane() {
        // A feed toggle is cold control-plane; 2s observation latency is fine and
        // the loop does zero Dhan work while idle.
        assert_eq!(DHAN_ACTIVATION_POLL, Duration::from_secs(2));
    }
}
