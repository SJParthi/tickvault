//! Cadence scheduler boot wiring (judge-locked design, 2026-07-14).
//!
//! Config-gated (`[cadence] enabled`, ships `false`), once-per-process
//! guarded (the `tf_consistency_boot` dual-spawn pattern: main.rs calls
//! this from BOTH boot paths — the FAST crash-recovery arm returns before
//! the process-global prefix ever runs, so both must own the spawn).
//!
//! Since 2026-07-17 BOTH lanes run the REAL broker executors
//! ([`crate::dhan_cadence_executor::DhanCadenceExecutor`] +
//! [`crate::groww_cadence_executor::GrowwCadenceExecutor`]) — one bounded
//! request per fire, runner-owned pacing/retry/ladder, persist-then-fold
//! spot bars, RAM chain-snapshot publish. The RS3 mutual exclusion
//! (config.rs) guarantees the legacy per-minute legs are OFF whenever the
//! scheduler is ON, so the executors are the SOLE authors of the
//! `spot_1m_rest` / `option_chain_1m` rows — which is why THIS spawn owns
//! the ensure-DDL for those tables + `rest_fetch_audit` (previously the
//! legacy legs' boot duty). Fire-time-token safety: the Dhan executor
//! resolves JWT + client-id from the global `TokenManager` AT FIRE TIME
//! (registered by `dhan_rest_stack` Phase 2, which may complete AFTER
//! this spawn — a pre-registration fire is an honest `Auth` error, never
//! a blocked boot); the Groww executor reads the shared-minter SSM token
//! at fire time (never minted). Runbook:
//! `.claude/rules/project/cadence-error-codes.md`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

use tickvault_api::feed_state::FeedRuntimeState;
use tickvault_common::config::ApplicationConfig;
use tickvault_common::error_code::ErrorCode;
use tickvault_common::trading_calendar::TradingCalendar;
use tickvault_core::cadence::{
    CadenceRunnerDeps, global_expiry_store, init_global_dhan_gates, spawn_supervised_cadence_runner,
};
use tickvault_core::notification::NotificationService;
use tokio::sync::Notify;
use tracing::{error, info};

use crate::dhan_cadence_executor::DhanCadenceExecutor;
use crate::groww_cadence_executor::GrowwCadenceExecutor;

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

/// Spawn the supervised cadence runner (REAL broker executors, both lanes).
/// Disabled config = one `info!` + return — a disabled boot is
/// byte-identical to today. Returns the shutdown handle the caller may
/// notify at graceful teardown (`None` when disabled or already spawned);
/// the SAME handle is parked process-globally so
/// [`notify_cadence_shutdown`] reaches it from the teardown path (F2).
// TEST-EXEMPT: thin tokio wiring over the unit-tested core runner; the dual spawn sites + config gate + shutdown wiring are pinned by crates/app/tests/cadence_boot_wiring_guard.rs.
pub fn spawn_cadence_scheduler(
    config: &ApplicationConfig,
    trading_calendar: &Arc<TradingCalendar>,
    // Retained in the signature for call-site stability; the cadence
    // lanes deliberately no longer gate on the retired live-WS feed
    // flags (fix round 2026-07-17) — see the deps wiring below.
    _feed_runtime: &Arc<FeedRuntimeState>,
    notifier: &Arc<NotificationService>,
    // Order-runtime mark tap (2026-07-18): threaded into the GROWW
    // executor ONLY — the Dhan executor must NEVER carry it (Dhan sids
    // 13/25/51 are a different id space than the Groww-native u64s the
    // paper book keys on; cross-feeding would double-key instruments
    // invisibly to the first-seen-segment tripwire). `None` when
    // `[order_runtime]` is disabled.
    groww_mark_forwarder: Option<crate::order_runtime::MarkForwarder>,
) -> Option<Arc<Notify>> {
    if !config.cadence.enabled {
        info!("cadence: disabled by [cadence] config — nothing spawned");
        return None;
    }
    // Build the REAL broker executors BEFORE the once-guard so a client
    // build failure (HTTP-CLIENT-01 class) leaves the guard un-tripped —
    // the OTHER boot path can still succeed. Fail loud, never a
    // `Client::new()` panic fallback.
    let dhan_executor = match DhanCadenceExecutor::new(
        &config.dhan.rest_api_base_url,
        &config.questdb,
        // Escalation/recovery Telegram sink (fix round 2026-07-17): the
        // executors own the SPOT1M-01/CHAIN-02 escalation edges now that
        // the legacy per-minute loops stand down.
        Some(Arc::clone(notifier)),
    ) {
        Ok(exec) => Arc::new(exec),
        Err(err) => {
            metrics::counter!("tv_http_client_build_failed_total", "site" => "cadence_dhan_executor")
                .increment(1);
            error!(
                code = ErrorCode::HttpClient01BuildFailed.code_str(),
                site = "cadence_dhan_executor",
                %err,
                "HTTP-CLIENT-01: Dhan cadence executor client build failed — cadence scheduler NOT spawned this attempt"
            );
            return None;
        }
    };
    let groww_executor = match GrowwCadenceExecutor::new(
        &config.questdb,
        Some(Arc::clone(notifier)),
        groww_mark_forwarder,
    ) {
        Ok(exec) => Arc::new(exec),
        Err(err) => {
            metrics::counter!("tv_http_client_build_failed_total", "site" => "cadence_groww_executor")
                .increment(1);
            error!(
                code = ErrorCode::HttpClient01BuildFailed.code_str(),
                site = "cadence_groww_executor",
                %err,
                "HTTP-CLIENT-01: Groww cadence executor client build failed — cadence scheduler NOT spawned this attempt"
            );
            return None;
        }
    };
    if CADENCE_SPAWNED.swap(true, Ordering::SeqCst) {
        // The other boot path already spawned it this process.
        return None;
    }
    // The scheduler's executors are the SOLE authors of these tables under
    // the RS3 mutual exclusion (legacy legs OFF) — the ensure-DDL duty
    // moves here (idempotent CREATE + ALTER ADD COLUMN IF NOT EXISTS; a
    // failed ensure degrades per HTTP-CLIENT-01's documented
    // duplicate-row-window envelope, never blocks the spawn).
    {
        let questdb = config.questdb.clone();
        drop(tokio::spawn(async move {
            tickvault_storage::spot_1m_rest_persistence::ensure_spot_1m_rest_table(&questdb).await;
            tickvault_storage::option_chain_1m_persistence::migrate_drop_moneyness_depth_column(
                &questdb,
            )
            .await;
            tickvault_storage::option_chain_1m_persistence::ensure_option_chain_1m_table(&questdb)
                .await;
            tickvault_storage::rest_fetch_audit_persistence::ensure_rest_fetch_audit_table(
                &questdb,
            )
            .await;
        }));
    }
    // 2026-07-17 (review fix S7): the legacy 15:33:30 IST post-session
    // repair sweep died with the leg loops — without it a cadence
    // per-minute miss is a PERMANENT spot_1m_rest gap. One-shot Dhan-lane
    // sweep task reusing the legacy sweep body + PACED fetch (post-session
    // — limiter pacing is fine). Groww residual: the Groww sweep needs the
    // leg's target resolution + token cache; not wired here.
    if config.cadence.dhan_lane {
        drop(tokio::spawn(
            crate::spot_1m_rest_boot::run_cadence_post_session_sweep(
                Arc::clone(trading_calendar),
                config.questdb.clone(),
                config.dhan.rest_api_base_url.clone(),
            ),
        ));
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
        // REAL broker executors both lanes (2026-07-17): one bounded
        // request per fire, runner-owned pacing/retry/ladder.
        dhan_executor,
        groww_executor,
        // Lane gates seeded from `[cadence] dhan_lane`/`groww_lane`
        // (fix round 2026-07-17, CRITICAL): the cadence REST lanes are
        // deliberately INDEPENDENT of the RETIRED live-WS feed flags
        // (feeds.dhan_enabled/groww_enabled are FALSE in shipped config
        // and runtime enable is 409'd — gating on feed_runtime parked
        // both lanes forever = zero market-data capture). Config +
        // restart to change; no runtime toggle.
        dhan_enabled: Arc::new(AtomicBool::new(config.cadence.dhan_lane)),
        groww_enabled: Arc::new(AtomicBool::new(config.cadence.groww_lane)),
        // ExpiryResolver seam (2026-07-15): the day-locked store IS the
        // production read facade — chains are stamped from the WINNING
        // (Dhan-preferred) policy date; unresolved days carry the
        // coalesced `expiry_unresolved` stage (the scheduler never
        // guesses).
        expiry_resolver: Arc::clone(&expiry_store)
            as Arc<dyn tickvault_core::cadence::ExpiryResolver>,
        expiry_store: Some(expiry_store),
        gates,
        // F10 (2026-07-15) semantics: false since the REAL executor PR
        // (2026-07-17) — skips/degrades keep their coded error! levels.
        dry_run: false,
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
        "cadence: supervised runner spawned (REAL broker executors both \
         lanes; post-close all-7 burst at T+1s — 3 chains + 4 spots \
         concurrent, shape/concurrency-laddered on rate limits; Groww \
         all-7 at T+0, wave shape-laddered)"
    );
    Some(shutdown)
}
