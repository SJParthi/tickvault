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

/// Poll cadence while waiting for the lazily-filled notifier slot (boot
/// ordering: the Groww lane spawns BEFORE the `NotificationService` exists).
const NOTIFIER_READY_POLL_MS: u64 = 500;
const NOTIFIER_READY_POLL: Duration = Duration::from_millis(NOTIFIER_READY_POLL_MS);
/// Max flusher polls before the deferred-ping queue is cleared (500ms × 240 =
/// 2 minutes — far beyond any healthy notification boot). The budget bounds
/// the flusher task's lifetime + the queue's memory; it never blocks or
/// fails activation.
const NOTIFIER_READY_MAX_POLLS: u32 = 240;

/// Capacity of the deferred boot-ping queue. Boot emits a handful of
/// feed-stage milestones (auth OK, instruments loaded, connected, sidecar
/// reject) — 16 is generous headroom while keeping memory strictly bounded.
pub const MAX_PENDING_BOOT_PINGS: usize = 16;

/// Outcome of [`PendingBootPings::notify_or_queue`] — unit-testable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BootPingDelivery {
    /// Notifier present — delivered immediately (any stale queued pings were
    /// flushed FIRST, so boot-stage order is preserved).
    Sent,
    /// Notifier absent — held in the bounded queue for the flusher.
    Queued,
    /// Queued, and the OLDEST pending ping was dropped to stay bounded.
    QueuedDroppedOldest,
}

/// Queue-and-flush for boot-stage feed Telegrams (operator 2026-07-05 —
/// feed-Telegram parity: "why dhan messages and groww messages are not
/// same"). Replaces the former give-up poll (`notify_when_ready`): a ping
/// emitted BEFORE the lazily-filled notifier slot is populated is no longer
/// skipped after a wait budget — it is HELD in this bounded FIFO and flushed
/// the moment the notifier arrives (order preserved, `info!` per late
/// flush). No busy-wait (the flusher sleeps between bounded polls), no
/// unbounded memory (capacity [`MAX_PENDING_BOOT_PINGS`], drop-oldest).
#[derive(Default)]
pub struct PendingBootPings {
    pending: std::sync::Mutex<
        std::collections::VecDeque<tickvault_core::notification::NotificationEvent>,
    >,
}

impl PendingBootPings {
    /// Lock the pending queue, recovering from a poisoned lock (cold path;
    /// the queue holds plain event data — no invariant a panicking peer
    /// could have corrupted).
    fn lock_pending(
        &self,
    ) -> std::sync::MutexGuard<
        '_,
        std::collections::VecDeque<tickvault_core::notification::NotificationEvent>,
    > {
        self.pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Deliver `event` now if the notifier slot is filled (flushing any
    /// stale queued pings FIRST so order is preserved); otherwise hold it in
    /// the bounded queue. Non-blocking, O(1) — safe to call inline from the
    /// activation task.
    pub fn notify_or_queue(
        &self,
        notifier_slot: &arc_swap::ArcSwapOption<tickvault_core::notification::NotificationService>,
        event: tickvault_core::notification::NotificationEvent,
    ) -> BootPingDelivery {
        if let Some(notifier) = notifier_slot.load_full() {
            // Drain anything still queued BEFORE the fresh event so the
            // operator sees boot milestones in emission order.
            let _ = self.flush(&notifier);
            notifier.notify(event);
            return BootPingDelivery::Sent;
        }
        let dropped_oldest = {
            let mut queue = self.lock_pending();
            let dropped = if queue.len() >= MAX_PENDING_BOOT_PINGS {
                if let Some(oldest) = queue.pop_front() {
                    warn!(
                        dropped_topic = oldest.topic(),
                        "[feeds] deferred boot-ping queue full — dropped the \
                         OLDEST pending Telegram to stay bounded (newest boot \
                         milestones win)"
                    );
                }
                true
            } else {
                false
            };
            queue.push_back(event);
            dropped
        };
        // Close the store race: if the notifier landed while we were
        // queuing, flush immediately — no ping is ever stranded behind a
        // just-filled slot.
        if let Some(notifier) = notifier_slot.load_full() {
            let _ = self.flush(&notifier);
        }
        if dropped_oldest {
            BootPingDelivery::QueuedDroppedOldest
        } else {
            BootPingDelivery::Queued
        }
    }

    /// Drain every queued ping into `notifier` in FIFO (emission) order,
    /// logging an `info!` per late-flushed ping. Returns the flushed topics
    /// in send order — unit-testable proof of order preservation.
    pub fn flush(
        &self,
        notifier: &Arc<tickvault_core::notification::NotificationService>,
    ) -> Vec<&'static str> {
        let drained: Vec<tickvault_core::notification::NotificationEvent> = {
            let mut queue = self.lock_pending();
            queue.drain(..).collect()
        };
        let mut topics = Vec::with_capacity(drained.len());
        for event in drained {
            let topic = event.topic();
            info!(
                topic,
                "[feeds] deferred boot-stage Telegram flushed late — the \
                 notifier arrived after this milestone fired; sending now"
            );
            notifier.notify(event);
            topics.push(topic);
        }
        topics
    }

    /// Empty the queue WITHOUT sending (flusher budget expiry — the notifier
    /// never arrived). Returns how many pings were discarded so the caller
    /// can log loudly. Keeps memory bounded on the regression path.
    pub fn clear(&self) -> usize {
        let mut queue = self.lock_pending();
        let cleared = queue.len();
        queue.clear();
        cleared
    }
}

/// Flush the deferred boot-ping queue as soon as the lazily-filled notifier
/// slot is populated. Bounded: polls up to `max_polls` times (sleeping
/// `poll` between checks — no busy-wait), then clears whatever is still
/// queued with a loud `warn!` so memory stays bounded even if the notifier
/// never arrives (regression class). Returns whether the notifier was
/// observed within the budget — unit-testable with tiny budgets.
pub async fn flush_when_ready(
    pings: Arc<PendingBootPings>,
    notifier_slot: Arc<arc_swap::ArcSwapOption<tickvault_core::notification::NotificationService>>,
    max_polls: u32,
    poll: Duration,
) -> bool {
    for _ in 0..max_polls {
        if let Some(notifier) = notifier_slot.load_full() {
            let _ = pings.flush(&notifier);
            return true;
        }
        tokio::time::sleep(poll).await;
    }
    let cleared = pings.clear();
    if cleared > 0 {
        warn!(
            cleared,
            "[feeds] boot-stage Telegrams discarded — notifier slot never \
             filled within the wait budget (the structured logs for these \
             milestones already recorded them; boot wiring must store the \
             notifier into the slot on every boot path)"
        );
    } else {
        info!(
            "[feeds] deferred boot-ping flusher retiring — notifier slot \
             never filled within the wait budget and nothing was queued"
        );
    }
    false
}

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
    // §34 auto-scale (PR-2): when the scale ladder is active, the watcher
    // publishes each built daily subscribe set here so the ladder can cut
    // per-conn shards. `None` = single-conn mode, byte-identical behavior.
    scale_entries_slot: Option<
        Arc<arc_swap::ArcSwapOption<Vec<tickvault_core::feed::groww::instruments::WatchEntry>>>,
    >,
) {
    // Deferred boot-ping queue (2026-07-05 feed-Telegram parity): boot-stage
    // feed Telegrams emitted BEFORE the notifier slot fills are HELD here and
    // flushed by the single bounded flusher below the moment the notifier
    // arrives — order preserved, never silently skipped. Shared with every
    // activation task this watcher spawns.
    let boot_pings = Arc::new(PendingBootPings::default());
    tokio::spawn(flush_when_ready(
        Arc::clone(&boot_pings),
        Arc::clone(&notifier_slot),
        NOTIFIER_READY_MAX_POLLS,
        NOTIFIER_READY_POLL,
    ));
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
                    Arc::clone(&boot_pings),
                    scale_entries_slot.clone(),
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
#[allow(clippy::too_many_arguments)] // APPROVED: activation carries the lane's full boot context
async fn activate_groww_lane(
    feed_runtime: Arc<FeedRuntimeState>,
    feed_health: Arc<FeedHealthRegistry>,
    questdb: QuestDbConfig,
    max_subscribe: Option<usize>,
    auth_timeout_ms: u64,
    notifier_slot: Arc<arc_swap::ArcSwapOption<tickvault_core::notification::NotificationService>>,
    // 2026-07-05 feed-Telegram parity: the watcher's deferred boot-ping
    // queue — boot-stage pings emitted before the notifier slot fills are
    // queued here and flushed in order, never skipped.
    boot_pings: Arc<PendingBootPings>,
    // §34: scale-mode subscribe-set publication slot (None = single-conn).
    scale_entries_slot: Option<
        Arc<arc_swap::ArcSwapOption<Vec<tickvault_core::feed::groww::instruments::WatchEntry>>>,
    >,
) {
    // Not live until the watch-list is built; clear any stale true from a prior
    // ON period so the feed page reports honestly during activation.
    feed_runtime.set_groww_lane_running(false);

    // Compute the watch date NOW (activation time), not at boot — an overnight
    // re-enable must use today's date, never the boot-day's stale date.
    // Hostile-review round 3 (2026-07-08): this entry-time value is ONLY the
    // fail-fast validity probe below — the pull-until-success loop re-derives
    // the date PER ATTEMPT (mirror of the Dhan `today_ist_fn` R2 fix), so an
    // activation straddling IST midnight (T-0→T+1 expiry crossing) builds
    // and names the watch file with the NEW date, never a frozen stale one.
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

    // Human-readable analyst console views (ticks_named / candles_named) —
    // review round 1 fix: WITHOUT this call site a Groww-only boot
    // (`feeds.dhan_enabled = false`, the documented scale-test / groww-only
    // lab mode) never created the views (both main.rs call sites are
    // Dhan-gated), so `SELECT * FROM ticks_named` failed with "table does
    // not exist" while ticks/candles_1m filled with feed='groww' rows.
    // Sequential AFTER the base-table ensures above so `ticks` +
    // `candles_1m` exist before CREATE VIEW validates its column
    // references. Fail-soft + convergent (CREATE OR REPLACE VIEW) — double
    // execution on a dual-feed boot is harmless.
    tickvault_storage::console_views::ensure_named_views(&questdb).await;

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
            // 2026-07-04 Groww boot-visibility parity (operator: "i need the
            // same view display everything even for groww"): mirror Dhan's
            // "Auth OK" boot ping. 2026-07-05 hardening: queue-and-flush —
            // a not-yet-filled notifier slot HOLDS the ping (bounded FIFO)
            // and the watcher's flusher sends it the moment the notifier
            // arrives; never skipped, activation never blocked. Once per
            // activation (edge-latched by construction: one activation task
            // per ON period), so a genuine disable → re-enable re-announces
            // while poll turns never do.
            let _ = boot_pings.notify_or_queue(
                &notifier_slot,
                tickvault_core::notification::NotificationEvent::FeedAuthOk {
                    feed: Feed::Groww.display_name().to_string(),
                },
            );
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
        // Hostile-review round 3 (2026-07-08): the trading date is re-derived
        // PER ATTEMPT — a retry loop that crosses IST midnight must select
        // FUTIDX (and name the watch file) with the NEW date, never keep the
        // just-expired front month for the whole next session. The entry-time
        // validity probe above already bailed on a permanently-broken clock;
        // a per-attempt invalid date falls back to the (validated) entry
        // value so the loop never spins on a transient clock glitch.
        let attempt_watch_date = {
            let d = today_ist_date();
            if tickvault_core::instrument::instrument_snapshot::is_valid_trading_date(&d) {
                d
            } else {
                watch_date.clone()
            }
        };
        match tickvault_core::feed::groww::instruments::build_and_write_groww_watch(
            &cache_dir,
            &attempt_watch_date,
            max_subscribe,
        )
        .await
        {
            Ok(set) => {
                // §34 auto-scale: publish the daily subscribe set for the
                // ladder's shard cutter (scale mode only; cheap Arc swap).
                if let Some(slot) = &scale_entries_slot {
                    slot.store(Some(std::sync::Arc::new(set.entries.clone())));
                }
                // §36/§36.7 (2026-07-10): the all-months index futures are
                // the FNO entries of the assembled set (~12 typical).
                let index_futures = set
                    .entries
                    .iter()
                    .filter(|e| e.segment.eq_ignore_ascii_case("FNO"))
                    .count();
                info!(
                    entries = set.entries.len(),
                    master_entries = set.master_entries.len(),
                    indices = set.indices,
                    resolved_stocks = set.resolved_stocks,
                    index_futures,
                    unresolved = set.unresolved_stocks.len(),
                    "[feeds] Groww watch-list ready"
                );
                // FIX 13c (2026-07-04): record the watch-set counts in the
                // shared feed-health registry AT ACTIVATION time — the mirror
                // of the Dhan subscription-plan `set_subscribed` call in
                // main.rs — so the boot/readiness Telegram feed lines and the
                // /feeds page show REAL Groww counts instead of unknown until
                // the sidecar's first status report. Idempotent overwrite: the
                // bridge's later sidecar-status set stays authoritative.
                // Hostile-review round 3 (2026-07-08), §36.7 AM-r2 F3 reword
                // (2026-07-10): the non-index bucket is the LIVE set minus
                // indices (mirror of the Dhan call in main.rs:
                // `stocks = total - indices`) so it INCLUDES the §36.7
                // futures (~12 typical, ≤24 by the monthly-serial envelope)
                // — the /feeds page and the FeedInstrumentsLoaded Telegram
                // (`subscribed = entries.len()`) agree
                // (subscribed == stocks_bucket + indices, no futures-sized
                // off-by-N). The arithmetic is count-agnostic.
                feed_health.set_subscribed(
                    Feed::Groww,
                    set.entries.len().saturating_sub(set.indices) as u64,
                    set.indices as u64,
                );
                // 2026-07-03 Telegram feed parity (operator: "whatever we
                // have provided for dhan the same should be provided for
                // groww also — instruments load message"). Mirror of the
                // Dhan `InstrumentBuildSuccess` ping: one Info Telegram when
                // the Groww watch-set resolves. 2026-07-05 hardening: a slot
                // not yet filled (boot ordering) queues the ping in the
                // watcher's bounded FIFO and the flusher sends it the moment
                // the notifier arrives — delayed, never lost, order
                // preserved; the info! line above still records the counts,
                // and activation is never blocked.
                let _ = boot_pings.notify_or_queue(
                    &notifier_slot,
                    tickvault_core::notification::events::NotificationEvent::FeedInstrumentsLoaded {
                        feed: Feed::Groww.display_name().to_string(),
                        subscribed: set.entries.len(),
                        indices: set.indices,
                        stocks: set.resolved_stocks,
                        skipped: set.unresolved_stocks.len(),
                    },
                );
                // Scoreboard PR-D: register the Groww watch set into the
                // per-instrument presence registry (cross-feed coverage
                // slots — ISIN for stocks, canonical index name for
                // indices, contract identity for futures). BEFORE the
                // persist spawn below moves `set`. Cold path, idempotent.
                register_groww_presence_from_watch(&set.entries, &attempt_watch_date);
                // PR-A: persist the Groww instrument set into the SHARED
                // `instrument_lifecycle` (+ `index_constituency`) master tables
                // tagged `feed='groww'`. Fire-and-forget + degrade-safe — a
                // persist failure logs GROWW-MASTER-01 and returns; it never
                // blocks lane activation or the live feed (cold-path forensic
                // master write). The Groww lane has no dry-run universe, so
                // `dry_run=false`.
                let persist_questdb = questdb.clone();
                // Round 3: persist under the date the set was BUILT for
                // (the per-attempt date), not the activation-entry date.
                let persist_date = attempt_watch_date.clone();
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

/// Scoreboard PR-D: pure Groww watch-entry → presence-registration
/// derivation. Mirrors the Dhan side (`presence_registration.rs`):
/// - segment label via the SAME `groww_segment_label` mapping the master
///   writer uses (the labels ticks carry), mapped to the shared binary
///   segment codes;
/// - pairing: futures by contract identity (canonical underlying +
///   expiry), indices by DUAL-FIELD canonical resolution (PR-D review
///   round 1, HIGH — the 2026-06-28 `groww_indices_absent_vs_dhan`
///   token-only lesson repeated: the short `groww_symbol` token
///   ("NSE-NIFTYAUTO" → "NIFTYAUTO") only covers the trading-symbol
///   allowlist entries, while the display `symbol_name` ("NIFTY Auto" →
///   "NIFTY AUTO") covers the descriptive ones — NEITHER field alone
///   canonicalizes to every Dhan-registered index name, so ~20 of ~25
///   co-listed indices split into two singleton slots. Canonicalize BOTH
///   and pick whichever is an `NSE_INDEX_ALLOWLIST` member, falling back
///   to the stripped token — e.g. BSE SENSEX, not an NSE-allowlist
///   member, still pairs on the token), stocks by ISIN; anything else
///   registers as a feed-local singleton (reported at drain, never
///   dropped — Rule 11).
///
/// O(1) EXEMPT: cold-path activation derivation, once per watch build.
#[must_use]
pub(crate) fn groww_presence_registrations(
    entries: &[tickvault_core::feed::groww::instruments::WatchEntry],
) -> Vec<tickvault_core::pipeline::feed_presence::PresenceRegistration> {
    use tickvault_core::feed::groww::instruments::WatchKind;
    use tickvault_core::feed::groww::shared_master_writer::groww_segment_label;
    use tickvault_core::instrument::index_extractor::{
        NSE_INDEX_ALLOWLIST, canonicalize_index_symbol,
    };
    use tickvault_core::pipeline::feed_presence::{PairingKey, PresenceRegistration};

    // Canonicalized allowlist — the exact value space the Dhan side
    // registers its Index pairing keys in (built once, cold path).
    let allowlist_canon: std::collections::HashSet<String> = NSE_INDEX_ALLOWLIST
        .iter()
        .map(|a| canonicalize_index_symbol(a))
        .collect();
    let mut out = Vec::with_capacity(entries.len());
    for e in entries {
        // validate_groww_tick rejects non-positive ids at the fold, so a
        // non-positive watch id could never match a tick anyway.
        let Ok(security_id) = u64::try_from(e.security_id) else {
            continue;
        };
        let segment_label = groww_segment_label(e);
        let segment_code = match segment_label {
            "IDX_I" => 0u8,
            "NSE_EQ" => 1,
            "NSE_FNO" => 2,
            "BSE_EQ" => 4,
            "BSE_FNO" => 8,
            // Unknown label — defensive skip (groww_segment_label is a
            // closed 5-label map today).
            _ => continue,
        };
        let pairing = if let (Some(underlying), Some(expiry)) =
            (e.underlying_symbol.as_deref(), e.expiry_date.as_deref())
        {
            // §36 index future: contract identity, never native ids (the
            // Groww exchange_token is a different id space from the Dhan
            // FUTIDX SID — record_index_future_selection precedent).
            Some(PairingKey::Future {
                underlying: underlying.to_string(),
                expiry: expiry.to_string(),
            })
        } else if matches!(e.kind, WatchKind::IndexValue) {
            // Dual-field resolution (mirror `groww_indices_absent_vs_dhan`,
            // instruments.rs): try the stripped token first
            // (`NSE-NIFTY` → `NIFTY`), then the display `symbol_name`
            // (`NIFTY Auto` → `NIFTY AUTO`) — whichever canonicalizes into
            // the allowlist is the value the Dhan registration carries.
            // Fallback: the stripped token (BSE SENSEX pairs here).
            let name = e.index_name.as_deref().unwrap_or(&e.exchange_token);
            let stripped = name
                .strip_prefix("NSE-")
                .or_else(|| name.strip_prefix("BSE-"))
                .unwrap_or(name);
            let canon_token = canonicalize_index_symbol(stripped);
            let canon = if allowlist_canon.contains(&canon_token) {
                canon_token
            } else {
                match e.symbol_name.as_deref().map(canonicalize_index_symbol) {
                    Some(canon_name) if allowlist_canon.contains(&canon_name) => canon_name,
                    _ => canon_token,
                }
            };
            Some(PairingKey::Index(canon))
        } else {
            e.isin
                .as_deref()
                .map(str::trim)
                .filter(|i| !i.is_empty())
                .map(|i| PairingKey::Isin(i.to_string()))
        };
        let symbol = e
            .symbol_name
            .clone()
            .or_else(|| e.index_name.clone())
            .unwrap_or_else(|| e.exchange_token.clone());
        out.push(PresenceRegistration {
            security_id,
            segment_code,
            segment_label,
            symbol,
            pairing,
        });
    }
    out
}

/// Registers the Groww watch set into the process-global presence registry
/// for the watch date. Thin wrapper — no-op (debug log) when the registry
/// is uninitialized (presence fold disabled).
// TEST-EXEMPT: thin wrapper over groww_presence_registrations (tested) + feed_presence::register_instruments (tested); call site pinned by test_groww_presence_registration_site_wired.
fn register_groww_presence_from_watch(
    entries: &[tickvault_core::feed::groww::instruments::WatchEntry],
    watch_date: &str,
) {
    let Ok(date) = chrono::NaiveDate::parse_from_str(watch_date, "%Y-%m-%d") else {
        warn!(
            watch_date,
            "feed_presence: unparsable watch date — Groww registration skipped"
        );
        return;
    };
    let ist_day = tickvault_core::instrument::presence_registration::ist_day_from_date(date);
    let regs = groww_presence_registrations(entries);
    match tickvault_core::pipeline::feed_presence::register_instruments(Feed::Groww, ist_day, &regs)
    {
        Some(summary) => {
            if summary.overflow_dropped > 0 {
                error!(
                    code = tickvault_common::error_code::ErrorCode::Scoreboard01AggregationDegraded
                        .code_str(),
                    stage = "presence_register_overflow",
                    overflow_dropped = summary.overflow_dropped,
                    "SCOREBOARD-01: presence slot table overflowed — coverage \
                     rows for the dropped instruments are missing today"
                );
            }
            info!(
                feed = "groww",
                registered = summary.registered,
                paired = summary.paired,
                ist_day,
                "feed_presence: Groww watch set registered for per-instrument \
                 coverage tracking"
            );
        }
        None => {
            tracing::debug!(
                "feed_presence: registry uninitialized (presence fold \
                 disabled) — Groww registration skipped"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tickvault_core::feed::groww::instruments::WatchKind;
    use tickvault_core::instrument::index_extractor::canonicalize_index_symbol;
    use tickvault_core::pipeline::feed_presence::{PairingKey, PresenceRegistration};

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

    fn empty_slot()
    -> Arc<arc_swap::ArcSwapOption<tickvault_core::notification::NotificationService>> {
        Arc::new(arc_swap::ArcSwapOption::empty())
    }

    fn auth_ok_event(feed: &str) -> tickvault_core::notification::NotificationEvent {
        tickvault_core::notification::NotificationEvent::FeedAuthOk {
            feed: feed.to_string(),
        }
    }

    #[test]
    fn test_notify_or_queue_queues_before_notifier_then_flushes_in_order() {
        // The 2026-07-05 root-cause hardening: pings emitted BEFORE the
        // notifier slot fills are held, then flushed in FIFO emission order
        // the moment the notifier arrives — never skipped.
        let slot = empty_slot();
        let pings = PendingBootPings::default();
        assert_eq!(
            pings.notify_or_queue(&slot, auth_ok_event("Groww")),
            BootPingDelivery::Queued
        );
        assert_eq!(
            pings.notify_or_queue(
                &slot,
                tickvault_core::notification::NotificationEvent::FeedInstrumentsLoaded {
                    feed: "Groww".to_string(),
                    subscribed: 767,
                    indices: 25,
                    stocks: 742,
                    skipped: 0,
                },
            ),
            BootPingDelivery::Queued
        );
        // Notifier arrives (no-op service — delivery side effects disabled).
        let notifier = tickvault_core::notification::NotificationService::disabled();
        let flushed = pings.flush(&notifier);
        assert_eq!(
            flushed,
            vec!["FeedAuthOk", "FeedInstrumentsLoaded"],
            "queued pings must flush in FIFO emission order"
        );
        // Queue is now empty — a second flush sends nothing.
        assert!(pings.flush(&notifier).is_empty());
    }

    #[test]
    fn test_notify_or_queue_sends_directly_when_notifier_present() {
        // Filled slot → immediate send, nothing queued.
        let slot = empty_slot();
        slot.store(Some(
            tickvault_core::notification::NotificationService::disabled(),
        ));
        let pings = PendingBootPings::default();
        assert_eq!(
            pings.notify_or_queue(&slot, auth_ok_event("Groww")),
            BootPingDelivery::Sent
        );
        assert_eq!(pings.clear(), 0, "direct send must leave nothing queued");
    }

    #[test]
    fn test_notify_or_queue_drops_oldest_at_capacity() {
        // Bounded memory: at capacity the OLDEST ping is dropped (newest
        // boot milestones win) and the queue length is pinned at the cap.
        let slot = empty_slot();
        let pings = PendingBootPings::default();
        for i in 0..MAX_PENDING_BOOT_PINGS {
            assert_eq!(
                pings.notify_or_queue(&slot, auth_ok_event(&format!("Feed{i}"))),
                BootPingDelivery::Queued
            );
        }
        assert_eq!(
            pings.notify_or_queue(
                &slot,
                tickvault_core::notification::NotificationEvent::FeedInstrumentsLoaded {
                    feed: "Groww".to_string(),
                    subscribed: 1,
                    indices: 1,
                    stocks: 0,
                    skipped: 0,
                },
            ),
            BootPingDelivery::QueuedDroppedOldest
        );
        // Still exactly at capacity, and the NEWEST event survived (flush
        // order ends with it).
        let notifier = tickvault_core::notification::NotificationService::disabled();
        let flushed = pings.flush(&notifier);
        assert_eq!(flushed.len(), MAX_PENDING_BOOT_PINGS);
        assert_eq!(flushed.last().copied(), Some("FeedInstrumentsLoaded"));
    }

    #[test]
    fn test_notify_or_queue_recheck_flushes_after_racing_store() {
        // Store-race closure: if the notifier lands between the empty-slot
        // check and the queue push, the post-queue re-check flushes — the
        // ping is never stranded behind a just-filled slot.
        let slot = empty_slot();
        let pings = PendingBootPings::default();
        // Simulate the race by pre-queuing while empty, then storing, then
        // sending a fresh ping: the Sent path must drain the stale queue
        // FIRST (order preserved), leaving nothing behind.
        assert_eq!(
            pings.notify_or_queue(&slot, auth_ok_event("Groww")),
            BootPingDelivery::Queued
        );
        slot.store(Some(
            tickvault_core::notification::NotificationService::disabled(),
        ));
        assert_eq!(
            pings.notify_or_queue(&slot, auth_ok_event("Groww")),
            BootPingDelivery::Sent
        );
        assert_eq!(pings.clear(), 0, "stale queue must drain on the Sent path");
    }

    #[tokio::test]
    async fn test_flush_when_ready_flushes_after_slot_fills() {
        // Boot-ordering race: the slot fills AFTER pings queue — the bounded
        // flusher must deliver them (late, with order preserved).
        let slot = empty_slot();
        let pings = Arc::new(PendingBootPings::default());
        assert_eq!(
            pings.notify_or_queue(&slot, auth_ok_event("Groww")),
            BootPingDelivery::Queued
        );
        let filler = Arc::clone(&slot);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            filler.store(Some(
                tickvault_core::notification::NotificationService::disabled(),
            ));
        });
        let observed =
            flush_when_ready(Arc::clone(&pings), slot, 50, Duration::from_millis(10)).await;
        assert!(observed, "flusher must observe the slot fill and flush");
        assert_eq!(pings.clear(), 0, "flusher must have drained the queue");
    }

    #[tokio::test]
    async fn test_flush_when_ready_budget_expiry_clears_queue_bounded() {
        // A never-filled slot must NOT hang forever or hold memory: bounded
        // give-up clears the queue loudly (the milestone info! logs already
        // recorded the events).
        let slot = empty_slot();
        let pings = Arc::new(PendingBootPings::default());
        assert_eq!(
            pings.notify_or_queue(&slot, auth_ok_event("Groww")),
            BootPingDelivery::Queued
        );
        let observed =
            flush_when_ready(Arc::clone(&pings), slot, 3, Duration::from_millis(1)).await;
        assert!(!observed, "never-filled slot must give up after the budget");
        assert_eq!(
            pings.clear(),
            0,
            "budget expiry must leave the queue empty (bounded memory)"
        );
    }

    #[test]
    fn test_notifier_ready_budget_is_bounded_boot_scale() {
        // ~2 minutes of 500ms polls — far beyond any healthy notification
        // boot, yet strictly bounded (never an infinite background poll).
        assert_eq!(NOTIFIER_READY_POLL_MS, 500);
        assert_eq!(
            NOTIFIER_READY_POLL,
            Duration::from_millis(NOTIFIER_READY_POLL_MS)
        );
        assert_eq!(NOTIFIER_READY_MAX_POLLS, 240);
    }

    #[test]
    fn test_is_dead_activation_false_when_disabled() {
        // Desired OFF → the Stop path owns teardown; the dead-task watchdog must
        // never fire (it only re-starts a DESIRED-ON lane).
        assert!(!is_dead_activation(false, true, false));
        assert!(!is_dead_activation(false, true, true));
        assert!(!is_dead_activation(false, false, false));
    }

    /// Hostile-review round 3 (2026-07-08) ratchet (source-order scan,
    /// mirror of `runner_rederives_trading_date_per_build_attempt` on the
    /// Dhan side): the watch TRADING DATE must be re-derived INSIDE the
    /// pull-until-success loop (per attempt, before the build call) — a
    /// frozen entry-time date selects the just-expired contract when the
    /// retry loop crosses IST midnight on the T-0→T+1 expiry boundary.
    #[test]
    fn ratchet_watch_date_rederived_per_attempt_inside_loop() {
        let src: String = include_str!("groww_activation.rs")
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect();
        // Needles split via concat! so this test's own source never
        // satisfies the scan vacuously.
        let loop_marker = concat!("attempt=attempt.", "saturating_add(1);");
        let derive_marker = concat!("letattempt_watch_date", "=");
        let build_marker = concat!("build_and_write_", "groww_watch(");
        let loop_pos = src.find(loop_marker).expect("loop head present");
        let derive_pos = src
            .find(derive_marker)
            .expect("per-attempt date derivation present");
        let build_pos = src.find(build_marker).expect("build call present");
        assert!(
            loop_pos < derive_pos && derive_pos < build_pos,
            "the watch date must be derived PER ATTEMPT: loop head -> date \
             derivation -> build call, in source order"
        );
        // And the build must consume the per-attempt date, not the frozen one.
        let consume = concat!("&attempt_", "watch_date,");
        assert!(
            src.contains(consume),
            "build_and_write_groww_watch must take the per-attempt date"
        );
    }

    /// FIX 13c ratchet (source-scan, groww_bridge pattern): the watch-set
    /// `Ok(set)` arm must record the Groww subscribe counts in feed-health at
    /// ACTIVATION time — the mirror of Dhan's subscription-plan
    /// `set_subscribed` call — so the boot/readiness Telegram never shows an
    /// unknown Groww count while the sidecar is still coming up.
    #[test]
    fn ratchet_activation_sets_groww_subscribed_counts() {
        // Whitespace-normalized scan so rustfmt line-breaking can't break the pin.
        let src: String = include_str!("groww_activation.rs")
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect();
        // Needle split via concat! so the test's own source can never satisfy
        // the scan vacuously.
        let needle = concat!("feed_health.set_subscribed(", "Feed::Groww,");
        assert!(
            src.contains(needle),
            "activation must record the Groww subscribe counts in feed-health \
             in the watch-set Ok(set) arm (FIX 13c)"
        );
    }

    // ---- Scoreboard PR-D: Groww presence registration ----

    fn watch_entry(
        kind: WatchKind,
        segment: &str,
        exchange: &str,
        security_id: i64,
    ) -> tickvault_core::feed::groww::instruments::WatchEntry {
        tickvault_core::feed::groww::instruments::WatchEntry {
            exchange: exchange.to_string(),
            segment: segment.to_string(),
            exchange_token: security_id.to_string(),
            kind,
            security_id,
            isin: None,
            symbol_name: None,
            index_name: None,
            expiry_date: None,
            underlying_symbol: None,
        }
    }

    #[test]
    fn test_groww_presence_registrations_index_stock_future_keys() {
        let mut index = watch_entry(WatchKind::IndexValue, "CASH", "NSE", 0x4000_0000_0000_0001);
        index.index_name = Some("NSE-NIFTY".to_string());
        let mut stock = watch_entry(WatchKind::Ltp, "CASH", "NSE", 1333);
        stock.isin = Some("INE002A01018".to_string());
        stock.symbol_name = Some("RELIANCE".to_string());
        let mut fut = watch_entry(WatchKind::Ltp, "FNO", "BSE", 999_777);
        fut.underlying_symbol = Some("SENSEX".to_string());
        fut.expiry_date = Some("2026-07-30".to_string());
        let mut orphan = watch_entry(WatchKind::Ltp, "CASH", "NSE", 555);
        orphan.symbol_name = Some("ODDSTOCK".to_string());

        let regs = groww_presence_registrations(&[index, stock, fut, orphan]);
        assert_eq!(regs.len(), 4);
        // Index: IDX_I code 0, exchange prefix stripped + canonicalized.
        assert_eq!(regs[0].segment_code, 0);
        assert_eq!(regs[0].segment_label, "IDX_I");
        assert_eq!(
            regs[0].pairing,
            Some(PairingKey::Index("NIFTY".to_string()))
        );
        // The canonicalizer must be the SHARED one (alias-drift lesson).
        assert_eq!(canonicalize_index_symbol("NIFTY"), "NIFTY");
        // Stock: NSE_EQ code 1 + ISIN pairing (the by_isin precedent).
        assert_eq!(regs[1].segment_code, 1);
        assert_eq!(regs[1].segment_label, "NSE_EQ");
        assert_eq!(
            regs[1].pairing,
            Some(PairingKey::Isin("INE002A01018".to_string()))
        );
        // Future: BSE_FNO code 8 + contract-identity pairing.
        assert_eq!(regs[2].segment_code, 8);
        assert_eq!(
            regs[2].pairing,
            Some(PairingKey::Future {
                underlying: "SENSEX".to_string(),
                expiry: "2026-07-30".to_string(),
            })
        );
        // ISIN-less stock: feed-local singleton, never dropped (Rule 11).
        assert_eq!(regs[3].pairing, None);
        assert_eq!(regs[3].symbol, "ODDSTOCK");
    }

    #[test]
    fn test_groww_presence_registrations_skip_non_positive_ids() {
        let bad = watch_entry(WatchKind::Ltp, "CASH", "NSE", -5);
        assert!(groww_presence_registrations(&[bad]).is_empty());
    }

    /// The REAL 24 Groww NSE index rows (short `groww_symbol` token +
    /// display `name`) — the same fixture pinned in
    /// `instruments.rs::REAL_GROWW_NSE_INDICES` (2026-06-28 operator
    /// verification).
    const REAL_GROWW_NSE_INDICES: &[(&str, &str)] = &[
        ("NIFTY", "NIFTY 50"),
        ("BANKNIFTY", "NIFTY Bank"),
        ("FINNIFTY", "Nifty Financial Services"),
        ("INDIAVIX", "India Vix"),
        ("NIFTYJR", "Nifty Next 50"),
        ("MIDCAP50", "NIFTY MIDCAP 50"),
        ("NIFTY100", "NIFTY 100"),
        ("NIFTY500", "NIFTY 500"),
        ("NIFTYAUTO", "NIFTY Auto"),
        ("NIFTYCDTY", "NIFTY Commodities"),
        ("NIFTYFMCG", "NIFTY FMCG"),
        ("NIFTYIT", "NIFTY IT"),
        ("NIFTYMEDIA", "NIFTY Media"),
        ("NIFTYMETAL", "NIFTY Metal"),
        ("NIFTYMIDCAP", "NIFTY Midcap 100"),
        ("NIFTYMIDCAP150", "NIFTY Midcap 150"),
        ("NIFTYMIDSELECT", "Nifty Midcap Select"),
        ("NIFTYPHARMA", "NIFTY Pharma"),
        ("NIFTYPSUBANK", "NIFTY PSU Bank"),
        ("NIFTYPVTBANK", "NIFTY Pvt Bank"),
        ("NIFTYREALTY", "NIFTY Realty"),
        ("NIFTYSMALL", "NIFTY Smallcap 100"),
        ("NIFTYSMALLCAP250", "NIFTY Smallcap 250"),
        ("NIFTYTOTALMCAP", "Nifty Total Market"),
    ];

    /// Regression (PR-D review round 1, HIGH — the token-only pairing bug):
    /// against the REAL 24 Groww NSE index rows, the dual-field resolution
    /// must land 22 of them (= the 32-entry allowlist minus the 10
    /// known-absent-on-Groww) on the EXACT canonical value the Dhan side
    /// registers — not the buggy 4 that the token field alone resolves.
    #[test]
    fn test_groww_index_pairing_dual_field_matches_dhan_canonicals() {
        use tickvault_core::instrument::index_extractor::NSE_INDEX_ALLOWLIST;

        let entries: Vec<_> = REAL_GROWW_NSE_INDICES
            .iter()
            .enumerate()
            .map(|(i, (token, name))| {
                let mut e = watch_entry(
                    WatchKind::IndexValue,
                    "CASH",
                    "NSE",
                    0x4000_0000_0000_0000_i64 + i64::try_from(i).unwrap_or(0) + 1,
                );
                e.exchange_token = (*token).to_string();
                e.index_name = Some(format!("NSE-{token}"));
                e.symbol_name = Some((*name).to_string());
                e
            })
            .collect();
        let regs = groww_presence_registrations(&entries);
        assert_eq!(regs.len(), 24);
        let allowlist_canon: std::collections::HashSet<String> = NSE_INDEX_ALLOWLIST
            .iter()
            .map(|a| canonicalize_index_symbol(a))
            .collect();
        let keys: Vec<String> = regs
            .iter()
            .filter_map(|r| match &r.pairing {
                Some(PairingKey::Index(name)) => Some(name.clone()),
                _ => None,
            })
            .collect();
        let paired = keys.iter().filter(|k| allowlist_canon.contains(*k)).count();
        assert_eq!(
            paired, 22,
            "22 of the real 24 Groww NSE indices must resolve to a Dhan \
             allowlist canonical (only NIFTY Commodities + NIFTY Midcap 100 \
             are legitimately Dhan-untracked); token-only pairing resolved \
             just 4 — keys: {keys:?}"
        );
        // The previously-split spellings now pair on the Dhan canonical.
        for expected in [
            "NIFTY AUTO",
            "NIFTY NEXT 50",
            "INDIA VIX",
            "MIDCPNIFTY",
            "NIFTYMCAP50",
            "NIFTY TOTAL MKT",
        ] {
            assert!(
                keys.iter().any(|k| k == expected),
                "expected canonical {expected} missing from {keys:?}"
            );
        }
        // BSE SENSEX: not an NSE-allowlist member — pairs on the stripped
        // token fallback, exactly what the Dhan side registers.
        let mut sensex = watch_entry(WatchKind::IndexValue, "CASH", "BSE", 0x4000_0000_0000_0100);
        sensex.index_name = Some("BSE-SENSEX".to_string());
        sensex.symbol_name = Some("SENSEX".to_string());
        let regs = groww_presence_registrations(&[sensex]);
        assert_eq!(
            regs[0].pairing,
            Some(PairingKey::Index("SENSEX".to_string()))
        );
    }

    #[test]
    fn test_groww_presence_registration_reg_type_matches_registry_input() {
        // Compile-time shape pin: the derivation output feeds
        // feed_presence::register_instruments directly.
        let regs: Vec<PresenceRegistration> = groww_presence_registrations(&[]);
        assert!(regs.is_empty());
    }

    /// Wiring ratchet (source-scan): the watch-set `Ok(set)` arm must
    /// register the Groww presence slots BEFORE the master-persist spawn
    /// moves `set` — a registration wired nowhere leaves every Groww
    /// instrument an unregistered fold at 15:45.
    #[test]
    fn test_groww_presence_registration_site_wired() {
        let src: String = include_str!("groww_activation.rs")
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect();
        let needle = concat!("register_groww_presence_from_watch(", "&set.entries,");
        assert!(
            src.contains(needle),
            "groww_activation must register the watch set into the presence \
             registry in the Ok(set) arm"
        );
    }
}
