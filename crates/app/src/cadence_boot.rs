//! Cadence scheduler boot wiring (judge-locked design, 2026-07-14).
//!
//! Config-gated (`[cadence] enabled`, ships `false`), once-per-process
//! guarded (the `tf_consistency_boot` dual-spawn pattern: main.rs calls
//! this from BOTH boot paths — the FAST crash-recovery arm returns before
//! the process-global prefix ever runs, so both must own the spawn).
//!
//! Day 1 BOTH lanes run the [`DryRunLoggingExecutor`]: every fire is a
//! structured `info!` returning `Empty` — the timing machinery is proven
//! end-to-end in the logs, decisions honest-skip, and NO REST call is
//! made. The real broker executors (and the dated rule-file
//! re-authorization for the cadence decision-path fires) land in a LATER
//! PR (`dhan_cadence_executor.rs`). Runbook:
//! `.claude/rules/project/cadence-error-codes.md`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

use tickvault_api::feed_state::FeedRuntimeState;
use tickvault_common::config::ApplicationConfig;
use tickvault_common::trading_calendar::TradingCalendar;
use tickvault_core::cadence::{
    CadenceRunnerDeps, DryRunLoggingExecutor, global_expiry_store, init_global_dhan_gates,
    spawn_supervised_cadence_runner,
};
use tickvault_core::notification::NotificationService;
use tokio::sync::Notify;
use tracing::info;

/// Once-per-process guard: the fast crash-recovery arm and the
/// process-global prefix both call [`spawn_cadence_scheduler`]; only the
/// first spawns.
static CADENCE_SPAWNED: AtomicBool = AtomicBool::new(false);

/// The spawned runner's shutdown handle (verifier F2, dated 2026-07-15):
/// the pre-fix spawn returned the `Notify` to a `let _cadence_shutdown`
/// binding at both main.rs spawn sites — NOTHING ever notified it, so
/// graceful teardown never reached the runner (it died with the runtime
/// instead of exiting its `LoopExit::Shutdown` arm). The handle now
/// parks HERE and [`notify_cadence_shutdown`] fires it from the
/// process-teardown path (the sibling supervised-task house pattern).
static CADENCE_SHUTDOWN: OnceLock<Arc<Notify>> = OnceLock::new();

/// Notify the cadence runner's graceful-shutdown signal (no-op when the
/// scheduler never spawned — a disabled boot has nothing to tear down).
/// Called from `run_process_runloop`'s teardown (F2, 2026-07-15).
pub fn notify_cadence_shutdown() {
    if let Some(shutdown) = CADENCE_SHUTDOWN.get() {
        info!("cadence: graceful shutdown notified");
        shutdown.notify_waiters();
    }
}

/// Spawn the supervised cadence runner (dry-run executors, both lanes).
/// Disabled config = one `info!` + return — a disabled boot is
/// byte-identical to today. Returns the shutdown handle the caller may
/// notify at graceful teardown (`None` when disabled or already spawned);
/// the SAME handle is parked process-globally so
/// [`notify_cadence_shutdown`] reaches it from the teardown path (F2).
// TEST-EXEMPT: thin tokio wiring over the unit-tested core runner; the dual spawn sites + config gate + shutdown wiring are pinned by crates/app/tests/cadence_boot_wiring_guard.rs.
pub fn spawn_cadence_scheduler(
    config: &ApplicationConfig,
    trading_calendar: &Arc<TradingCalendar>,
    feed_runtime: &Arc<FeedRuntimeState>,
    notifier: &Arc<NotificationService>,
) -> Option<Arc<Notify>> {
    if !config.cadence.enabled {
        info!("cadence: disabled by [cadence] config — nothing spawned");
        return None;
    }
    if CADENCE_SPAWNED.swap(true, Ordering::SeqCst) {
        // The other boot path already spawned it this process.
        return None;
    }
    let shutdown = Arc::new(Notify::new());
    // Park the handle for the teardown path (F2, 2026-07-15).
    drop(CADENCE_SHUTDOWN.set(Arc::clone(&shutdown)));
    // The PROCESS-GLOBAL Dhan gate registry (F1(ii), 2026-07-15): every
    // future Dhan-firing composition shares this one budget; the runner
    // receives a clone of the same Arc.
    let gates = Arc::clone(init_global_dhan_gates(
        config.cadence.chain_min_spacing_ms,
        config.cadence.spot_window_cap,
    ));
    // The PROCESS-GLOBAL day-locked expiry store (Workstream A,
    // 2026-07-15): the runner's resolution loop WRITES it; the SAME
    // store is the ExpiryResolver read facade stamping every chain
    // request (Dhan-wins keying; day-locked, respawn-proof).
    let expiry_store = Arc::clone(global_expiry_store());
    let deps = CadenceRunnerDeps {
        config: config.cadence.clone(),
        calendar: Arc::clone(trading_calendar),
        // Day-1 dry-run: both lanes log fires and return Empty — prices
        // are NEVER synthesized (judge ruling, design §0).
        dhan_executor: Arc::new(DryRunLoggingExecutor),
        groww_executor: Arc::new(DryRunLoggingExecutor),
        // Level-triggered lane gates: the SAME atomics the /api/feeds
        // toggle flips (dhan_flag/groww_flag), read per cycle per lane.
        dhan_enabled: feed_runtime.dhan_flag(),
        groww_enabled: feed_runtime.groww_flag(),
        // ExpiryResolver seam (2026-07-15): the day-locked store IS the
        // production read facade — chains are stamped from the WINNING
        // (Dhan-preferred) policy date; unresolved days carry the
        // coalesced `expiry_unresolved` stage (the scheduler never
        // guesses). Under the dry-run executors every expiry-list fetch
        // returns Empty, so the store stays honestly unresolved.
        expiry_resolver: Arc::clone(&expiry_store)
            as Arc<dyn tickvault_core::cadence::ExpiryResolver>,
        expiry_store: Some(expiry_store),
        gates,
        // F10 (2026-07-15): the wired executors ARE the dry-run loggers —
        // Empty-shaped skips/degrades log at info! (dry_run=true), never
        // the High coded error! storm. Flip to false with the REAL
        // executor PR.
        dry_run: true,
        // R6 (2026-07-16): the typed Telegram sink for the expiry
        // cross-broker disagreement page (`CadenceExpiryDisagreement`,
        // edge-latched once per underlying per day).
        notifier: Some(Arc::clone(notifier)),
        shutdown: Arc::clone(&shutdown),
    };
    // Fire-and-forget: the supervisor owns respawn; graceful teardown
    // reaches it via the parked Notify (notify_cadence_shutdown).
    drop(spawn_supervised_cadence_runner(deps));
    info!(
        "cadence: supervised runner spawned (dry-run executors both lanes; \
         post-close all-7 burst at T+1s — 3 chains + 4 spots concurrent, \
         shape/concurrency-laddered on rate limits; Groww all-7 at T+0, \
         wave shape-laddered)"
    );
    Some(shutdown)
}
