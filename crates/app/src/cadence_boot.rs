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

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tickvault_api::feed_state::FeedRuntimeState;
use tickvault_common::config::ApplicationConfig;
use tickvault_common::trading_calendar::TradingCalendar;
use tickvault_core::cadence::{
    CadenceRunnerDeps, DryRunLoggingExecutor, StubExpiryResolver, spawn_supervised_cadence_runner,
};
use tokio::sync::Notify;
use tracing::info;

/// Once-per-process guard: the fast crash-recovery arm and the
/// process-global prefix both call [`spawn_cadence_scheduler`]; only the
/// first spawns.
static CADENCE_SPAWNED: AtomicBool = AtomicBool::new(false);

/// Spawn the supervised cadence runner (dry-run executors, both lanes).
/// Disabled config = one `info!` + return — a disabled boot is
/// byte-identical to today. Returns the shutdown handle the caller may
/// notify at graceful teardown (`None` when disabled or already spawned).
// TEST-EXEMPT: thin tokio wiring over the unit-tested core runner; the dual spawn sites + config gate are pinned by crates/app/tests/cadence_boot_wiring_guard.rs.
pub fn spawn_cadence_scheduler(
    config: &ApplicationConfig,
    trading_calendar: &Arc<TradingCalendar>,
    feed_runtime: &Arc<FeedRuntimeState>,
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
        // ExpiryResolver seam (2026-07-15): day-1 stub — always
        // unresolved (`None`); the scheduler NEVER guesses an expiry and
        // the lanes carry the coalesced `expiry_unresolved` stage. The
        // real resolution boot phase is a SEPARATE follow-up increment.
        expiry_resolver: Arc::new(StubExpiryResolver),
        shutdown: Arc::clone(&shutdown),
    };
    // Fire-and-forget: the supervisor owns respawn; graceful teardown
    // reaches it via the returned Notify.
    drop(spawn_supervised_cadence_runner(deps));
    info!(
        "cadence: supervised runner spawned (dry-run executors both lanes; \
         chains pre-fire T-5s/T-2s + post T+2s, spots concurrency-laddered \
         from T+3s, Groww waves shape-laddered from T+0)"
    );
    Some(shutdown)
}
