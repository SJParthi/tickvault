//! Groww runtime activation watcher — makes the feed toggle start/stop the
//! ENTIRE Groww lane LIVE (operator 2026-06-24: "when I enable on/off the entire
//! mechanism and its architecture should run entirely for that feed").
//!
//! ## Why this exists
//! Before this, the Groww lane (tables + watch-list + bridge + Python sidecar)
//! was spawned ONLY inside `if feeds.groww_enabled` at boot. So a feed that was
//! OFF at boot could never be cold-started by the webpage toggle — the toggle
//! flipped a flag with no lane behind it ("DEGRADED · not started at boot").
//!
//! ## The design (level-triggered reconciler, single owned task)
//! The bridge + sidecar supervisor are spawned UNCONDITIONALLY at boot; both
//! already self-idle on `is_enabled(Feed::Groww)` (the sidecar's loop `sleep`s
//! without provisioning a venv / spawning Python when OFF; the bridge tails
//! nothing when OFF) — so an OFF Groww feed still touches NOTHING (no auth, no
//! instruments, no connection), preserving the operator's OFF-feed-isolation
//! guarantee, just as a *dormant poll* rather than *not-spawned*.
//!
//! This watcher owns the remaining ONE-SHOT, Groww-server-touching work that
//! must run while Groww is enabled: ensure the Groww tables, run the auth
//! smoke-check, build + write the watch-list the sidecar reads, and ONLY THEN
//! set `groww_lane_running = true` (so the flag is never a false-OK).
//!
//! ### Lifecycle ownership (fixes the leak + false-OK + ungated-after-OFF trio)
//! The watcher is **level-triggered**, not edge-classified, so a sub-poll
//! ON→OFF→ON flap converges to the final desired state instead of missing an
//! edge. It holds a SINGLE `JoinHandle` for the activation task:
//!   * desired ON & not yet activated  → spawn the activation task (Start).
//!   * desired OFF & activated         → `abort()` the task + clear the flag (Stop).
//! Because the auth-check + watch-list build run INLINE inside that one task
//! (no detached children), the `abort()` on the disable transition cancels ALL
//! in-flight Groww work at the next await point — no leaked build loops, no
//! work continuing after OFF. `running` is set true only after the watch-list
//! actually builds, so the webpage never shows "running" for an empty lane.

use std::sync::Arc;
use std::time::Duration;

use tickvault_api::feed_state::{Feed, FeedRuntimeState};
use tickvault_common::config::QuestDbConfig;
use tickvault_common::feed_health::FeedHealthRegistry;
use tokio::task::JoinHandle;
use tracing::{error, info, warn};

/// Poll cadence for the enable flag. This is the COLD control-plane (a feed
/// toggle), NOT the hot tick path — a 2s observation latency on an operator
/// toggle is irrelevant, and the loop does zero work while OFF. Being
/// level-triggered, a flap faster than this window simply converges to the
/// final state (no missed-edge bug).
const GROWW_ACTIVATION_POLL_SECS: u64 = 2;
const GROWW_ACTIVATION_POLL: Duration = Duration::from_secs(GROWW_ACTIVATION_POLL_SECS);

/// Today's IST date as `YYYY-MM-DD`. Computed at ACTIVATION time (not boot) so a
/// runtime re-enable past IST midnight uses today's watch date, never a stale
/// boot-day date (security-review MEDIUM: a date frozen at boot would build the
/// wrong day's watch-list after an overnight re-enable). Pure wall-clock read.
fn today_ist_date() -> String {
    (chrono::Utc::now()
        + chrono::TimeDelta::seconds(tickvault_common::constants::IST_UTC_OFFSET_SECONDS_I64))
    .format("%Y-%m-%d")
    .to_string()
}

/// What the reconciler decides to do this tick, given the desired enable state
/// vs whether the lane is currently activated. Pure (unit-tested) so the watcher
/// loop stays a trivial poll around this decision. Level-triggered: it compares
/// DESIRED vs CURRENT, never PREV vs NOW — so a sub-poll ON→OFF→ON flap cannot
/// drop an activation (it converges to the final desired state).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LaneAction {
    /// Desired state already matches the lane state — do nothing.
    None,
    /// Lane should run but isn't activated yet — start the one-shot activation.
    Start,
    /// Lane is activated but should be off — abort activation + mark stopped.
    Stop,
}

/// Decide the reconcile action. `desired_enabled` = the live feed flag;
/// `currently_activated` = whether the watcher has an activation task running
/// for the current ON period. Pure + total.
#[must_use]
pub fn reconcile_lane_action(desired_enabled: bool, currently_activated: bool) -> LaneAction {
    match (desired_enabled, currently_activated) {
        (true, false) => LaneAction::Start,
        (false, true) => LaneAction::Stop,
        _ => LaneAction::None,
    }
}

/// Detect a DEAD activation task: the lane is desired ON, the activation task
/// has finished, yet the lane never came up. This is the panic / early-return
/// blind-spot — the task ended (panicked, or returned before marking running)
/// but `set_groww_lane_running(false)` was its last word, so `reconcile` (which
/// reads `active_task.is_some()`) would see an activated lane forever and never
/// re-Start it → silently dead lane (desired ON, running=false, no alert).
///
/// Pure + total so the watcher loop stays a trivial poll around this decision.
/// Returns true ONLY for `(desired=ON, task finished, lane NOT running)` — a
/// task that finished AFTER marking the lane running (the success path) is NOT
/// a dead lane and must NOT be re-started.
#[must_use]
pub fn is_dead_activation(desired_enabled: bool, task_finished: bool, lane_running: bool) -> bool {
    desired_enabled && task_finished && !lane_running
}

// Run the activation watcher for the process lifetime. Spawned UNCONDITIONALLY
// at boot. Level-triggered: it reconciles the live `is_enabled(Feed::Groww)`
// flag against the lane's activated state every poll, so a feed ENABLED at boot
// is started on the first tick exactly like a runtime enable (one code path for
// both), and a quick ON→OFF→ON flap converges to the final state.
// It drives live QuestDB/HTTP I/O via spawn+abort; the pure decision it loops
// around (reconcile_lane_action) is fully unit-tested below and the enable gate
// (is_enabled) is unit-tested in the api crate.
// TEST-EXEMPT: infinite control-plane poll loop driving live I/O; pure decision (reconcile_lane_action) unit-tested below.
pub async fn run_groww_activation_watcher(
    feed_runtime: Arc<FeedRuntimeState>,
    feed_health: Arc<FeedHealthRegistry>,
    questdb: QuestDbConfig,
    max_subscribe: Option<usize>,
    auth_timeout_ms: u64,
    // 2026-07-03 Telegram feed parity: slot filled once the
    // NotificationService boots (same slot the sidecar supervisor uses) —
    // the watch-set-ready arm emits the Groww instruments-load Info ping
    // through it. A not-yet-filled slot skips the Telegram, never blocks.
    notifier_slot: Arc<arc_swap::ArcSwapOption<tickvault_core::notification::NotificationService>>,
) {
    // The SINGLE owned activation task for the current ON period. `Some` iff the
    // lane is activated. Holding the handle lets us `abort()` ALL in-flight Groww
    // work (auth-check + watch-list build, which run inline inside it) the moment
    // the feed is disabled — no leaked loops, no work after OFF.
    let mut active_task: Option<JoinHandle<()>> = None;
    loop {
        let desired = feed_runtime.is_enabled(Feed::Groww);

        // Dead-task watchdog (panic blind-spot): if the activation task finished
        // WITHOUT bringing the lane up (a panic, or an early return), clear the
        // handle so this same tick's reconcile re-Starts it (bounded: one re-spawn
        // per poll, each doing real async work — not a tight loop). A task that
        // finished AFTER marking the lane running is the success path and is left
        // as-is. Mirrors the WS-GAP-05 pool-supervisor respawn pattern.
        let dead = active_task.as_ref().is_some_and(|h| {
            is_dead_activation(
                desired,
                h.is_finished(),
                feed_runtime.is_groww_lane_running(),
            )
        });
        if dead {
            error!(
                "[feeds] Groww activation task ended without bringing the lane up \
                 (panic or early return) — re-starting activation so the desired-ON \
                 feed is not left silently dead"
            );
            active_task = None;
        }

        match reconcile_lane_action(desired, active_task.is_some()) {
            LaneAction::Start => {
                info!(
                    "[feeds] Groww enabled — activating lane (tables + auth + watch-list); \
                     bridge + sidecar (spawned dormant at boot) will stream once the \
                     watch-list is ready"
                );
                // Defensive: abort any stale handle before replacing it (cannot
                // happen given the `is_some()` gate, but keeps the invariant local).
                if let Some(handle) = active_task.take() {
                    handle.abort();
                }
                let task = tokio::spawn(activate_groww_lane(
                    Arc::clone(&feed_runtime),
                    Arc::clone(&feed_health),
                    questdb.clone(),
                    max_subscribe,
                    auth_timeout_ms,
                    Arc::clone(&notifier_slot),
                ));
                active_task = Some(task);
            }
            LaneAction::Stop => {
                info!(
                    "[feeds] Groww disabled — aborting in-flight activation, lane marked \
                     stopped; bridge + sidecar self-idle, Python producer killed by the \
                     sidecar supervisor"
                );
                if let Some(handle) = active_task.take() {
                    // Cancels the inline auth-check / watch-list build at its next
                    // await point → no Groww server is touched after OFF.
                    handle.abort();
                }
                feed_runtime.set_groww_lane_running(false);
            }
            LaneAction::None => {}
        }
        tokio::time::sleep(GROWW_ACTIVATION_POLL).await;
    }
}

/// The one-shot Groww-server-touching work for an ON period, run as ONE task the
/// watcher owns (so a disable `abort()`s the whole thing). Order: ensure the
/// Groww tables (idempotent CREATE/ALTER) → auth smoke-check (diagnostic) →
/// build + write the watch-list (pull-until-success) → mark the lane running.
///
/// `running` is set true ONLY after the watch-list build succeeds — the lane is
/// not actually live until the sidecar has a watch-list to subscribe, so marking
/// it earlier would be a false-OK on the feed page (operator 2026-06-24: "no
/// illusion"). All steps run INLINE (no detached children) so the watcher's
/// `abort()` on a disable cancels every in-flight step.
///
/// The watch date is computed HERE (at activation time) via `today_ist_date()`,
/// never frozen at boot — so a runtime re-enable past IST midnight builds today's
/// watch-list, not a stale boot-day one (security-review MEDIUM).
async fn activate_groww_lane(
    feed_runtime: Arc<FeedRuntimeState>,
    feed_health: Arc<FeedHealthRegistry>,
    questdb: QuestDbConfig,
    max_subscribe: Option<usize>,
    auth_timeout_ms: u64,
    notifier_slot: Arc<arc_swap::ArcSwapOption<tickvault_core::notification::NotificationService>>,
) {
    // Not live until the watch-list is built; clear any stale true from a prior
    // ON period so the feed page reports honestly during activation.
    feed_runtime.set_groww_lane_running(false);

    // Compute the watch date NOW (activation time), not at boot — an overnight
    // re-enable must use today's date, never the boot-day's stale date.
    let watch_date = today_ist_date();

    // Fail-closed date guard (defense-in-depth, operator "cover all worst cases").
    // `watch_date` is system-generated (`%Y-%m-%d`) and never attacker-controlled,
    // and the watch-list writer ALSO validates it before any file op — but a bad
    // date is a PERMANENT error, so the pull-until-success loop below would spin
    // forever on it. Validate once here and bail cleanly instead of looping.
    if !tickvault_core::instrument::instrument_snapshot::is_valid_trading_date(&watch_date) {
        error!(
            watch_date = %watch_date,
            "[feeds] Groww activation aborted — computed watch date is not a valid \
             YYYY-MM-DD; refusing to enter the watch-list build loop (would never \
             succeed). Lane stays not-running."
        );
        return;
    }

    // Groww shares Dhan's `ticks` + 21 `candles_<tf>` tables (operator
    // 2026-06-19); these ensures DELEGATE to the canonical shared DDL. SP5: the
    // 1m parity audit is now the ONE unified `feed_parity_1m_audit` table (both
    // feeds write it, `feed` in the DEDUP key) — Groww ensures the same table the
    // Dhan path does (idempotent).
    tickvault_storage::groww_persistence::ensure_groww_live_ticks_table(&questdb).await;
    tickvault_storage::groww_candle_persistence::ensure_groww_candles_1m_table(&questdb).await;
    tickvault_storage::feed_parity_1m_audit_persistence::ensure_feed_parity_1m_audit_table(
        &questdb,
    )
    .await;

    // Auth smoke-check (diagnostic; inline so a disable aborts it). A failure is
    // logged but does NOT abort activation — the watch-list build + sidecar may
    // still succeed if the token recovers; the smoke-check is an early warning.
    // The hint names the RESOLVED environment + the EXACT SSM paths read (never
    // the values) so the failure is self-diagnosing — no literal `<env>`.
    use tickvault_core::auth::secret_manager::{build_ssm_path, resolve_environment};
    use tickvault_core::feed::groww::auth::GrowwAuthSmokeError;
    let env = resolve_environment().unwrap_or_else(|_| "<unresolved>".to_string());
    let token_param_path = build_ssm_path(
        &env,
        tickvault_common::constants::SSM_GROWW_SERVICE,
        tickvault_common::constants::GROWW_ACCESS_TOKEN_SECRET,
    );
    match tickvault_core::feed::groww::auth::run_groww_auth_smoke_check(auth_timeout_ms).await {
        Ok(()) => {
            // Token present + plausible — clear any stale auth-rejected flag so
            // the Feed Control page resolves back to a normal verdict.
            feed_health.set_auth_rejected(Feed::Groww, false);
        }
        Err(GrowwAuthSmokeError::Rejected { status: _, body }) => {
            // The token parameter was READ but its value is unusable (empty /
            // placeholder) — the actionable "the bruteX groww-token-minter
            // Lambda has not populated the token" case. Surface it on the Feed
            // Control page. TickVault NEVER mints a replacement (shared
            // token-minter lock 2026-07-02).
            feed_health.set_auth_rejected(Feed::Groww, true);
            error!(
                env = %env,
                token_param_path = %token_param_path,
                body = %body,
                "Groww shared access token UNUSABLE — check the bruteX \
                 groww-token-minter Lambda's last daily mint (~06:05 IST); \
                 TickVault is a read-only consumer and never mints"
            );
        }
        Err(GrowwAuthSmokeError::Transport(reason)) => {
            // Retained for API compatibility; the SSM-read smoke check does
            // not construct this variant.
            error!(
                env = %env,
                token_param_path = %token_param_path,
                reason = %reason,
                "Groww token smoke-check transport failure (transient) — the \
                 token is NOT marked rejected"
            );
        }
        Err(GrowwAuthSmokeError::Credentials(reason)) => {
            // The SSM read itself failed (parameter missing / IAM / network) —
            // an infra fault, NOT a token-value problem; do NOT mark rejected.
            error!(
                env = %env,
                token_param_path = %token_param_path,
                reason = %reason,
                "Groww access-token SSM read failed — verify the reader role \
                 (groww-token-minter-reader-tickvault) can GetParameter+Decrypt \
                 the token param above; TickVault never mints a fallback"
            );
        }
    }

    // Watch-list build — pull-until-success, inline. The watcher's `abort()` on a
    // disable drops this future at its next await (the build call or the backoff
    // sleep), so it stops touching Groww/niftyindices the moment the feed is OFF.
    let cache_dir = std::path::PathBuf::from("data/groww");
    let mut attempt: u32 = 0;
    loop {
        attempt = attempt.saturating_add(1);
        match tickvault_core::feed::groww::instruments::build_and_write_groww_watch(
            &cache_dir,
            &watch_date,
            max_subscribe,
        )
        .await
        {
            Ok(set) => {
                info!(
                    entries = set.entries.len(),
                    master_entries = set.master_entries.len(),
                    indices = set.indices,
                    resolved_stocks = set.resolved_stocks,
                    unresolved = set.unresolved_stocks.len(),
                    "[feeds] Groww watch-list ready"
                );
                // 2026-07-03 Telegram feed parity (operator: "whatever we
                // have provided for dhan the same should be provided for
                // groww also — instruments load message"). Mirror of the
                // Dhan `InstrumentBuildSuccess` ping: one Info Telegram when
                // the Groww watch-set resolves. Slot not yet filled (boot
                // ordering) → skipped; the info! line above still records
                // the counts, and activation is never blocked.
                if let Some(notifier) = notifier_slot.load_full() {
                    notifier.notify(
                        tickvault_core::notification::events::NotificationEvent::FeedInstrumentsLoaded {
                            feed: Feed::Groww.display_name().to_string(),
                            subscribed: set.entries.len(),
                            indices: set.indices,
                            stocks: set.resolved_stocks,
                            skipped: set.unresolved_stocks.len(),
                        },
                    );
                }
                // PR-A: persist the Groww instrument set into the SHARED
                // `instrument_lifecycle` (+ `index_constituency`) master tables
                // tagged `feed='groww'`. Fire-and-forget + degrade-safe — a
                // persist failure logs GROWW-MASTER-01 and returns; it never
                // blocks lane activation or the live feed (cold-path forensic
                // master write). The Groww lane has no dry-run universe, so
                // `dry_run=false`.
                let persist_questdb = questdb.clone();
                let persist_date = watch_date.clone();
                tokio::spawn(async move {
                    tickvault_core::feed::groww::shared_master_writer::persist_groww_instruments(
                        &persist_questdb,
                        &set,
                        &persist_date,
                        false,
                    )
                    .await;
                });
                break;
            }
            Err(err) => {
                let backoff = std::cmp::min(10u64.saturating_mul(1u64 << attempt.min(5)), 300);
                if attempt <= 3 {
                    warn!(
                        ?err,
                        attempt,
                        backoff_secs = backoff,
                        "[feeds] Groww watch-list build failed — retrying"
                    );
                } else {
                    error!(
                        ?err,
                        attempt,
                        backoff_secs = backoff,
                        "[feeds] Groww watch-list build still failing — pull-until-success"
                    );
                }
                tokio::time::sleep(Duration::from_secs(backoff)).await;
            }
        }
    }

    // Watch-list is ready → the lane is genuinely live (bridge tails the sidecar
    // tick file, sidecar subscribes the watch-list). Mark running — no false-OK.
    feed_runtime.mark_groww_lane_running();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_reconcile_lane_action_start_when_desired_and_not_activated() {
        assert_eq!(reconcile_lane_action(true, false), LaneAction::Start);
    }

    #[test]
    fn test_reconcile_lane_action_stop_when_not_desired_and_activated() {
        assert_eq!(reconcile_lane_action(false, true), LaneAction::Stop);
    }

    #[test]
    fn test_reconcile_lane_action_none_when_already_converged() {
        // Running + desired-on, and stopped + desired-off both need no action.
        assert_eq!(reconcile_lane_action(true, true), LaneAction::None);
        assert_eq!(reconcile_lane_action(false, false), LaneAction::None);
    }

    #[test]
    fn test_reconcile_lane_action_boot_enabled_starts_once_then_idles() {
        // Boot with Groww enabled: first tick has activated=false → Start, then
        // activated=true → None (the one-shots do not re-run while it stays ON).
        let mut activated = false;
        assert_eq!(reconcile_lane_action(true, activated), LaneAction::Start);
        activated = true;
        assert_eq!(reconcile_lane_action(true, activated), LaneAction::None);
    }

    #[test]
    fn test_reconcile_lane_action_sub_poll_flap_converges_no_missed_edge() {
        // The reconciler reads DESIRED vs ACTIVATED, never PREV vs NOW — so an
        // ON→OFF→ON flap within one poll window is seen only as its FINAL value.
        // Final ON while not activated → Start (correct). Final OFF while not
        // activated → None (correct; a sub-poll blip never started anything).
        assert_eq!(reconcile_lane_action(true, false), LaneAction::Start);
        assert_eq!(reconcile_lane_action(false, false), LaneAction::None);
    }

    #[test]
    fn test_poll_cadence_is_cold_control_plane() {
        // A feed toggle is cold control-plane; 2s observation latency is fine and
        // the loop does zero Groww work while OFF.
        assert_eq!(GROWW_ACTIVATION_POLL, Duration::from_secs(2));
    }

    #[test]
    fn test_is_dead_activation_true_when_finished_and_lane_not_running() {
        // Panic / early-return blind-spot: desired ON, task finished, lane never
        // came up → dead lane, must re-start.
        assert!(is_dead_activation(true, true, false));
    }

    #[test]
    fn test_is_dead_activation_false_on_success_path() {
        // Task finished AFTER marking the lane running → success, NOT dead;
        // re-starting would needlessly rebuild a live lane.
        assert!(!is_dead_activation(true, true, true));
    }

    #[test]
    fn test_is_dead_activation_false_while_still_running() {
        // Task still in-flight (not finished) → not dead, regardless of the
        // running flag (running is false until the watch-list builds).
        assert!(!is_dead_activation(true, false, false));
        assert!(!is_dead_activation(true, false, true));
    }

    #[test]
    fn test_is_dead_activation_false_when_disabled() {
        // Desired OFF → the Stop path owns teardown; the dead-task watchdog must
        // never fire (it only re-starts a DESIRED-ON lane).
        assert!(!is_dead_activation(false, true, false));
        assert!(!is_dead_activation(false, true, true));
        assert!(!is_dead_activation(false, false, false));
    }
}
