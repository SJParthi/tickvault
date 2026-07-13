//! Dhan REST-only auth bootstrap — Phase A of the Dhan-live-feed removal.
//!
//! **Operator directive (2026-07-13, verbatim):** "now remove this entire
//! Dhan live websocket feed instruments subscription even entire live
//! websocket feed itself... As of now only Groww and Dhan historical api
//! pull as we discussed last night along with option chain."
//!
//! With `feeds.dhan_enabled = false` the Dhan lane (`start_dhan_lane`) never
//! runs — and before Phase A that killed EVERY Dhan-REST subsystem as a side
//! effect, because the TokenManager and the `spawn_post_market_tasks` seam
//! live exclusively inside the lane / fast-boot arm. This module brings up
//! the RETAINED Dhan REST surface WITHOUT any WebSocket:
//!
//! 1. dual-instance SSM lock BEFORE any mint (lock-before-mint,
//!    `dual-instance-lock-2026-07-04.md` §2 — reuses
//!    `tickvault_core::instance_lock` exactly as the lane does), retried
//!    forever in the background on AlreadyHeld / SSM failure (bounded
//!    exponential backoff, cap [`DHAN_REST_STACK_BACKOFF_CAP_SECS`]) — the
//!    process is NEVER halted and the Groww feed / shared infra are NEVER
//!    blocked by this task;
//! 2. `TokenManager::initialize` (SSM creds → TOTP → JWT) with the SAME
//!    `instance_lock_held` wiring (RESILIENCE-03 mint tripwire), then
//!    `set_global_token_manager` + `feed_runtime.set_live_token_manager` so
//!    the token gauges read this manager;
//! 3. the token renewal loop + the mid-session profile watchdog;
//! 4. the REST canary (`rest_canary_boot`), the per-minute `spot_1m_rest`
//!    scheduler and the per-minute `option_chain_1m` scheduler /
//!    entitlement probe — mirroring `main.rs::spawn_post_market_tasks`
//!    exactly (incl. the spot→chain sequencing watch channel and the
//!    existing `[spot_1m_rest]` / `[option_chain_1m]` config gates).
//!
//! **Deliberately NOT spawned (stay lane-only; deletion is a later phase):**
//! the WS pool, universe build / CSV download, prev-day OHLCV, the SLO
//! publisher, the 15:31 cross-verify, the EOD digest, the orphan-position
//! watchdog, and the order-update WS.
//!
//! **Mutual exclusion by construction:** this stack is spawned ONLY from the
//! Dhan-OFF branch of main.rs (the `else` of `if config.feeds.dhan_enabled`)
//! — the same flag gates the fast crash-recovery arm off AND makes the
//! runtime cold-start supervisor REFUSE a lane start — so the lane's own
//! lock / TokenManager / post-market seam can never run alongside this
//! stack. A process-global once-guard additionally rejects a double spawn.
//!
//! **Panic honesty (the TICK-FLUSH-01 precedent):** the release profile sets
//! `panic = "abort"`, so a panicked bring-up task aborts the PROCESS in
//! prod — the monitor wrapper's died-log is an unwind-build (dev/test)
//! visibility path only; no in-process respawn is claimed. Robustness
//! against TRANSIENT failures comes from the internal retry-forever loops
//! (every failure arm is a typed-error `match`, no unwrap/expect).
//!
//! **Graceful-shutdown honest envelope:** the SSM lock heartbeat here has no
//! shutdown-notify chain (the lane's Item 19f bridge is lane-scoped), so a
//! process stop leaves the lock parameter in SSM until the 90s TTL clears
//! it. The next boot's stack absorbs that window: its acquire loop retries
//! with backoff and wins once the TTL expires; the DualInstanceDetected
//! Telegram is deferred past the TTL window
//! ([`DHAN_REST_STACK_PEER_PAGE_MIN_ATTEMPT`]) so a quick restart never
//! pages against its own stale entry.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use secrecy::ExposeSecret;
use tracing::{debug, error, info, warn};

use tickvault_api::feed_state::FeedRuntimeState;
use tickvault_common::config::ApplicationConfig;
use tickvault_common::constants::TOKEN_INIT_TIMEOUT_SECS;
use tickvault_common::error_code::ErrorCode;
use tickvault_common::trading_calendar::TradingCalendar;
use tickvault_core::auth::secret_manager;
use tickvault_core::auth::token_manager::TokenManager;
use tickvault_core::instance_lock;
use tickvault_core::notification::{NotificationEvent, NotificationService};

/// First-retry backoff of every retry-forever loop in this stack (seconds).
const DHAN_REST_STACK_BACKOFF_BASE_SECS: u64 = 10;
/// Backoff ceiling — retries never space out further than this (seconds).
const DHAN_REST_STACK_BACKOFF_CAP_SECS: u64 = 300;
/// Coalesced-logging cadence: `error!` on the FIRST failed attempt and every
/// Nth thereafter; the attempts in between log at `debug!` (audit Rule 4 —
/// no per-attempt error spam from a retry-forever loop).
const DHAN_REST_STACK_LOG_EVERY_N_ATTEMPTS: u32 = 10;
/// The DualInstanceDetected Telegram fires only from this attempt onward.
/// Rationale: a quick restart (<90s after a stop) sees its OWN previous
/// process's lock entry as AlreadyHeld until the
/// [`instance_lock::INSTANCE_LOCK_TTL_SECS`] (90s) TTL clears it. Cumulative
/// backoff before attempt 5 = 10+20+40+80 = 150s > 90s TTL, so a holder that
/// STILL exists at attempt 5 is a genuine live peer (its heartbeat kept
/// renewing past the TTL) — page-worthy; the self-stale window is not.
const DHAN_REST_STACK_PEER_PAGE_MIN_ATTEMPT: u32 = 5;

/// Process-global once-guard: the REST-only stack must never be brought up
/// twice (N stacks = N heartbeats + N renewal loops + N canary/spot/chain
/// families). First caller wins; later calls log INFO and return `None`.
static DHAN_REST_STACK_SPAWNED: AtomicBool = AtomicBool::new(false);

/// Everything the bring-up task needs — Arc clones of the process-shared
/// infra main.rs already built (nothing here is lane-owned).
pub struct DhanRestStackParams {
    /// Immutable boot config snapshot (deep copy — no env re-parse).
    pub config: Arc<ApplicationConfig>,
    /// Telegram dispatcher (shared strict-init NotificationService).
    pub notifier: Arc<NotificationService>,
    /// Trading calendar for the canary/spot/chain trading-day gates.
    pub calendar: Arc<TradingCalendar>,
    /// Runtime feed-state — receives `set_live_token_manager` so the token
    /// gauges read this stack's manager.
    pub feed_runtime: Arc<FeedRuntimeState>,
}

/// Bounded exponential backoff for the stack's retry-forever loops:
/// 10s → 20s → 40s → 80s → 160s → 300s cap (attempt is 1-based; 0 is
/// treated as 1). Pure — unit-tested below.
#[must_use]
pub fn dhan_rest_stack_backoff_secs(attempt: u32) -> u64 {
    // Shift is clamped so `10 << shift` can never overflow; ≥ shift 5 the
    // cap wins anyway.
    let shift = attempt.saturating_sub(1).min(6);
    (DHAN_REST_STACK_BACKOFF_BASE_SECS << shift).min(DHAN_REST_STACK_BACKOFF_CAP_SECS)
}

/// Coalesced-logging decision: `true` on the first attempt and every
/// [`DHAN_REST_STACK_LOG_EVERY_N_ATTEMPTS`]th thereafter. Pure — unit-tested.
#[must_use]
pub fn dhan_rest_retry_should_log(attempt: u32) -> bool {
    // `attempt > 0` guard: 0 is a multiple of N, but attempt 0 means "no
    // failure yet" — total-fn correctness (the callers always increment
    // before asking; this keeps the pure fn honest on its whole domain).
    attempt == 1 || (attempt > 0 && attempt.is_multiple_of(DHAN_REST_STACK_LOG_EVERY_N_ATTEMPTS))
}

/// Edge-latched Telegram decision for a persisting AlreadyHeld peer: fires
/// once per process, and only from
/// [`DHAN_REST_STACK_PEER_PAGE_MIN_ATTEMPT`] onward (past the 90s TTL
/// self-stale window of a quick restart). Pure — unit-tested.
#[must_use]
pub fn dhan_rest_peer_page_due(attempt: u32, already_paged: bool) -> bool {
    !already_paged && attempt >= DHAN_REST_STACK_PEER_PAGE_MIN_ATTEMPT
}

/// Spawn the Dhan REST-only stack bring-up as ONE background task (plus a
/// tiny exit monitor so an unwind-build death is never silent). Returns
/// `None` when the once-guard rejects a duplicate spawn.
///
/// Called ONLY from main.rs's Dhan-OFF branch — see the module docs for the
/// mutual-exclusion contract with the Dhan lane.
// TEST-EXEMPT: orchestration spawn around live I/O (SSM lock + TOTP mint +
// task spawns); the pure decisions (dhan_rest_stack_backoff_secs,
// dhan_rest_retry_should_log, dhan_rest_peer_page_due) are unit-tested
// below and the wiring is pinned by
// crates/app/tests/dhan_live_off_phase_a_guard.rs.
pub fn spawn_dhan_rest_stack(params: DhanRestStackParams) -> Option<tokio::task::JoinHandle<()>> {
    if DHAN_REST_STACK_SPAWNED.swap(true, Ordering::SeqCst) {
        info!(
            "Dhan REST-only stack already spawned this process — skipping duplicate \
             (once-guard; mutual exclusion with the lane is by construction)"
        );
        return None;
    }
    let inner = tokio::spawn(run_dhan_rest_stack(params));
    Some(tokio::spawn(async move {
        // Exit monitor: the bring-up task normally COMPLETES after spawning
        // its children (they keep running on their own) — only an abnormal
        // exit is logged. Release builds abort the process on panic
        // (`panic = "abort"`), so this arm is unwind-build visibility only.
        if let Err(join_err) = inner.await
            && !join_err.is_cancelled()
        {
            error!(
                ?join_err,
                "Dhan REST-only stack bring-up task died before completing — the retained \
                 Dhan REST surface (canary / spot_1m_rest / option_chain_1m) may be absent \
                 this session; restart to re-run the bring-up"
            );
        }
    }))
}

/// The bring-up body: lock → token → renewal/watchdog → canary/spot/chain.
/// Every phase is a retry-forever loop with bounded exponential backoff —
/// this task never halts the process and never blocks boot (it runs
/// entirely in the background off the cold path).
// TEST-EXEMPT: live-I/O orchestration (see spawn_dhan_rest_stack); exercised
// by the live boot-deploy follow, with the pure helpers unit-tested below.
async fn run_dhan_rest_stack(params: DhanRestStackParams) {
    // Rule-11 discipline: the gauge reads 0 for the whole bring-up window so
    // "stack not up yet" is never presented as up.
    metrics::gauge!("tv_dhan_rest_stack_up").set(0.0);
    info!(
        "Dhan REST-only stack bring-up starting (operator directive 2026-07-13 — Dhan live \
         WS lane retired; REST retained surface: canary + spot_1m_rest + option_chain_1m)"
    );

    // -----------------------------------------------------------------------
    // Phase 1: dual-instance SSM lock — LOCK BEFORE MINT
    // (dual-instance-lock-2026-07-04.md §2). Same machinery as the lane's
    // Step 6a-prime, but retry-forever instead of halt: a REST-only stack
    // that cannot acquire the lock simply stays down (loud, coalesced) while
    // Groww + shared infra keep running.
    // -----------------------------------------------------------------------
    let instance_lock_held = Arc::new(AtomicBool::new(false));
    let ssm_client = Arc::new(secret_manager::create_ssm_client_public().await);

    let env = {
        let mut attempt: u32 = 0;
        loop {
            match secret_manager::resolve_environment() {
                Ok(env) => break env,
                Err(err) => {
                    attempt = attempt.saturating_add(1);
                    if dhan_rest_retry_should_log(attempt) {
                        error!(
                            attempt,
                            error = %err,
                            "Dhan REST-only stack: cannot resolve environment for the \
                             dual-instance lock path — retrying in background"
                        );
                    }
                    tokio::time::sleep(Duration::from_secs(dhan_rest_stack_backoff_secs(attempt)))
                        .await;
                }
            }
        }
    };
    let host_id = instance_lock::generate_host_id(
        std::process::id(),
        // Boot-once uniqueness value — same rationale as the lane's
        // Step 6a-prime (cross-host uniqueness within the 90s TTL window).
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0),
        None,
    );
    let lock_key = instance_lock::compute_instance_lock_path(&env);

    {
        let mut attempt: u32 = 0;
        let mut peer_paged = false;
        loop {
            match instance_lock::try_acquire_instance_lock(&ssm_client, &env, &host_id).await {
                Ok(instance_lock::AcquireOutcome::Acquired) => {
                    instance_lock_held.store(true, Ordering::Release);
                    info!(
                        env = %env,
                        host_id = %host_id,
                        lock_key = %lock_key,
                        "Dhan REST-only stack: dual-instance lock acquired (pre-mint)"
                    );
                    break;
                }
                Ok(instance_lock::AcquireOutcome::AlreadyHeld { holder }) => {
                    attempt = attempt.saturating_add(1);
                    if dhan_rest_retry_should_log(attempt) {
                        error!(
                            code = ErrorCode::Resilience01DualInstanceDetected.code_str(),
                            severity = ErrorCode::Resilience01DualInstanceDetected
                                .severity()
                                .as_str(),
                            peer = %holder,
                            lock_key = %lock_key,
                            attempt,
                            "Dhan REST-only stack: another process holds the dual-instance \
                             lock — retrying in background (never halts; the peer's Dhan \
                             session is untouched — no mint before the lock)"
                        );
                    } else {
                        debug!(attempt, peer = %holder, "dual-instance lock still held — retrying");
                    }
                    if dhan_rest_peer_page_due(attempt, peer_paged) {
                        // Past the 90s TTL self-stale window (see the const
                        // docs) — a genuine live peer. Page ONCE per process.
                        params
                            .notifier
                            .notify(NotificationEvent::DualInstanceDetected {
                                holder,
                                lock_key: lock_key.clone(),
                            });
                        peer_paged = true;
                    }
                }
                Err(err) => {
                    attempt = attempt.saturating_add(1);
                    if dhan_rest_retry_should_log(attempt) {
                        error!(
                            code = ErrorCode::Resilience01DualInstanceDetected.code_str(),
                            severity = ErrorCode::Resilience01DualInstanceDetected
                                .severity()
                                .as_str(),
                            error = %err,
                            lock_key = %lock_key,
                            attempt,
                            "Dhan REST-only stack: SSM lock acquire failed — retrying in \
                             background (cannot prove there is no peer, so no mint yet)"
                        );
                    } else {
                        debug!(attempt, error = %err, "SSM lock acquire failed — retrying");
                    }
                }
            }
            tokio::time::sleep(Duration::from_secs(dhan_rest_stack_backoff_secs(attempt))).await;
        }
    }

    // Lock held — spawn the heartbeat. The shutdown Notify is deliberately
    // never notified (this stack lives for the process lifetime; see the
    // module-docs graceful-shutdown honest envelope: the 90s TTL clears the
    // lock for the next boot).
    let heartbeat_shutdown = Arc::new(tokio::sync::Notify::new());
    let _heartbeat_handle = instance_lock::spawn_instance_lock_heartbeat(
        Arc::clone(&ssm_client),
        env.clone(),
        host_id.clone(),
        heartbeat_shutdown,
        Arc::clone(&instance_lock_held),
    );

    // -----------------------------------------------------------------------
    // Phase 2: TokenManager (SSM creds → TOTP → JWT), the SAME wiring the
    // lane uses — the `instance_lock_held` flag rides in as the
    // RESILIENCE-03 mint tripwire. Retry-forever: a failed/timed-out init
    // logs loudly (TokenManager's own internals page AUTH-GAP-04 /
    // AuthenticationFailed as today) and re-attempts at the backoff cap —
    // the 300s cap also respects Dhan's ~125s mint cooldown.
    // -----------------------------------------------------------------------
    let token_manager = {
        let mut attempt: u32 = 0;
        loop {
            match tokio::time::timeout(
                Duration::from_secs(TOKEN_INIT_TIMEOUT_SECS),
                TokenManager::initialize(
                    &params.config.dhan,
                    &params.config.token,
                    &params.config.network,
                    &params.notifier,
                    Some(Arc::clone(&instance_lock_held)),
                ),
            )
            .await
            {
                Ok(Ok(manager)) => break manager,
                Ok(Err(err)) => {
                    attempt = attempt.saturating_add(1);
                    if dhan_rest_retry_should_log(attempt) {
                        error!(
                            code = ErrorCode::Dh901InvalidAuth.code_str(),
                            severity = ErrorCode::Dh901InvalidAuth.severity().as_str(),
                            error = %err,
                            attempt,
                            "DH-901: Dhan REST-only stack authentication failed — retrying \
                             in background (no WS lane exists; canary/spot/chain stay down \
                             until auth succeeds)"
                        );
                    } else {
                        debug!(attempt, error = %err, "REST-stack auth failed — retrying");
                    }
                }
                Err(_elapsed) => {
                    attempt = attempt.saturating_add(1);
                    if dhan_rest_retry_should_log(attempt) {
                        error!(
                            timeout_secs = TOKEN_INIT_TIMEOUT_SECS,
                            attempt,
                            "Dhan REST-only stack authentication timed out — Dhan API may be \
                             unreachable; retrying in background"
                        );
                    } else {
                        debug!(attempt, "REST-stack auth timed out — retrying");
                    }
                }
            }
            tokio::time::sleep(Duration::from_secs(dhan_rest_stack_backoff_secs(attempt))).await;
        }
    };

    // Install the manager exactly as the lane does (main.rs ~7172/7183): the
    // global OnceLock for the force-renewal paths + the live slot so the
    // token health/headroom gauges read THIS manager. With the lane retired
    // nothing ever calls `clear_live_token_manager`, so the slot stays ours.
    if !tickvault_core::auth::token_manager::set_global_token_manager(token_manager.clone()) {
        warn!("global TokenManager already installed — skipping (Dhan REST-only stack)");
    }
    params
        .feed_runtime
        .set_live_token_manager(token_manager.clone());

    // -----------------------------------------------------------------------
    // Phase 3: renewal loop + mid-session profile watchdog (both need only
    // the TokenManager + notifier; the profile-valid flag has no other
    // consumer here — the token-health gauge poller / writer stay lane-only
    // in Phase A).
    // -----------------------------------------------------------------------
    let _renewal_handle = token_manager.spawn_renewal_task();
    info!("Dhan REST-only stack: token renewal task started");

    let token_profile_valid = Arc::new(AtomicBool::new(true));
    let _mid_session_watchdog_handle =
        tickvault_core::auth::mid_session_watchdog::spawn_mid_session_profile_watchdog(
            Arc::clone(&token_manager),
            Some(Arc::clone(&params.notifier)),
            token_profile_valid,
        );
    info!("Dhan REST-only stack: mid-session profile watchdog spawned (15-min cadence)");

    // -----------------------------------------------------------------------
    // Phase 4: Dhan client-id (the option-chain endpoints need the extra
    // `client-id` header — option-chain.md rule 3). Retry-forever like the
    // phases above.
    // -----------------------------------------------------------------------
    let client_id = {
        let mut attempt: u32 = 0;
        loop {
            match secret_manager::fetch_dhan_credentials().await {
                Ok(credentials) => break credentials.client_id.expose_secret().to_string(),
                Err(err) => {
                    attempt = attempt.saturating_add(1);
                    if dhan_rest_retry_should_log(attempt) {
                        error!(
                            attempt,
                            error = %err,
                            "Dhan REST-only stack: client-id fetch failed — retrying in \
                             background (the option-chain leg needs it)"
                        );
                    }
                    tokio::time::sleep(Duration::from_secs(dhan_rest_stack_backoff_secs(attempt)))
                        .await;
                }
            }
        }
    };

    // -----------------------------------------------------------------------
    // Phase 5: the retained REST subsystems — the EXACT spawn shapes of
    // main.rs::spawn_post_market_tasks (canary / spot / chain arms only;
    // orphan watchdog, EOD digest and cross-verify deliberately stay
    // lane-only per the Phase A scope).
    // -----------------------------------------------------------------------
    let token_handle = token_manager.token_handle();
    let config = &params.config;

    // REST-health canary (DHAN-REST-400): 09:05 / 12:00 / 15:25 IST probes.
    {
        let canary_token = Arc::clone(&token_handle);
        let canary_base = config.dhan.rest_api_base_url.clone();
        let canary_calendar = Arc::clone(&params.calendar);
        let _canary_handle = tokio::spawn(async move {
            use chrono::{FixedOffset, TimeZone, Timelike, Utc};
            use tickvault_common::constants::IST_UTC_OFFSET_SECONDS;
            let Some(ist_offset) = FixedOffset::east_opt(IST_UTC_OFFSET_SECONDS) else {
                return;
            };
            let now_ist = ist_offset.from_utc_datetime(&Utc::now().naive_utc());
            let is_trading_day = canary_calendar.is_trading_day(now_ist.date_naive());
            crate::rest_canary_boot::run_rest_canary(
                canary_token,
                canary_base,
                is_trading_day,
                now_ist.time().num_seconds_from_midnight(),
            )
            .await;
        });
        info!("rest_canary: REST-health probe task spawned (09:05 / 12:00 / 15:25 IST)");
    }

    // Spot→chain sequencing signal — created ONLY when BOTH halves are
    // enabled (byte-identical to the spawn_post_market_tasks wiring).
    let (spot_minute_done_tx, spot_minute_done_rx) =
        if config.spot_1m_rest.enabled && config.option_chain_1m.enabled {
            let (tx, rx) = tokio::sync::watch::channel::<Option<u32>>(None);
            (Some(tx), Some(rx))
        } else {
            (None, None)
        };

    if config.spot_1m_rest.enabled {
        let _spot1m_supervisor = crate::spot_1m_rest_boot::spawn_supervised_spot_1m_rest(
            crate::spot_1m_rest_boot::Spot1mRestTaskParams {
                token_handle: Arc::clone(&token_handle),
                notifier: params.notifier.clone(),
                calendar: Arc::clone(&params.calendar),
                questdb: config.questdb.clone(),
                rest_api_base_url: config.dhan.rest_api_base_url.clone(),
                minute_done_tx: spot_minute_done_tx,
            },
        );
        info!(
            "spot_1m_rest: per-minute spot 1m REST pipeline spawned \
             (fires each minute close 09:16:00–15:30:00 IST)"
        );
    } else {
        info!("spot_1m_rest: disabled by config — per-minute spot fetch not spawned");
    }

    {
        let chain_params = crate::option_chain_1m_boot::OptionChain1mTaskParams {
            token_handle: Arc::clone(&token_handle),
            notifier: params.notifier.clone(),
            calendar: Arc::clone(&params.calendar),
            questdb: config.questdb.clone(),
            rest_api_base_url: config.dhan.rest_api_base_url.clone(),
            client_id: client_id.clone(),
            spot_minute_done: spot_minute_done_rx,
        };
        if config.option_chain_1m.enabled {
            let _chain1m_supervisor =
                crate::option_chain_1m_boot::spawn_supervised_option_chain_1m(chain_params);
            info!(
                "option_chain_1m: per-minute option-chain REST pipeline spawned \
                 (expirylist warmup, then each minute close right after the spot leg)"
            );
        } else if config.option_chain_1m.probe_and_report {
            let probe_handle = tokio::spawn(
                crate::option_chain_1m_boot::run_option_chain_1m_probe(chain_params),
            );
            let _probe_monitor = tokio::spawn(async move {
                if let Err(join_err) = probe_handle.await
                    && !join_err.is_cancelled()
                {
                    error!(
                        code = ErrorCode::Chain04ExpirylistFailed.code_str(),
                        stage = "probe_task_exit",
                        ?join_err,
                        "CHAIN-04: the option-chain entitlement probe task died (panic) — \
                         no verdict today; tomorrow's boot re-probes"
                    );
                }
            });
            info!(
                "option_chain_1m: pipeline disabled by config — boot-time entitlement \
                 probe spawned (verdict via Telegram)"
            );
        } else {
            info!(
                "option_chain_1m: disabled by config (probe_and_report off) — no \
                 option-chain REST activity"
            );
        }
    }

    metrics::gauge!("tv_dhan_rest_stack_up").set(1.0);
    info!(
        spot_1m_rest_enabled = config.spot_1m_rest.enabled,
        option_chain_1m_enabled = config.option_chain_1m.enabled,
        option_chain_1m_probe = config.option_chain_1m.probe_and_report,
        "DHAN REST-ONLY STACK UP — lock + token + renewal + mid-session watchdog + REST \
         canary + spot_1m_rest + option_chain_1m arms spawned WITHOUT the WS lane \
         (operator directive 2026-07-13)"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `dhan_rest_stack_backoff_secs` — the documented 10→20→40→80→160→300
    /// ladder with a hard 300s cap and no overflow at extreme attempts.
    #[test]
    fn test_dhan_rest_stack_backoff_secs_ladder_and_cap() {
        assert_eq!(
            dhan_rest_stack_backoff_secs(0),
            10,
            "attempt 0 treated as 1"
        );
        assert_eq!(dhan_rest_stack_backoff_secs(1), 10);
        assert_eq!(dhan_rest_stack_backoff_secs(2), 20);
        assert_eq!(dhan_rest_stack_backoff_secs(3), 40);
        assert_eq!(dhan_rest_stack_backoff_secs(4), 80);
        assert_eq!(dhan_rest_stack_backoff_secs(5), 160);
        assert_eq!(dhan_rest_stack_backoff_secs(6), 300, "cap engages");
        assert_eq!(dhan_rest_stack_backoff_secs(7), 300);
        assert_eq!(dhan_rest_stack_backoff_secs(u32::MAX), 300, "no overflow");
    }

    /// `dhan_rest_retry_should_log` — first attempt + every 10th, nothing
    /// in between (coalesced error discipline, audit Rule 4).
    #[test]
    fn test_dhan_rest_retry_should_log_first_and_every_tenth() {
        assert!(dhan_rest_retry_should_log(1));
        assert!(!dhan_rest_retry_should_log(2));
        assert!(!dhan_rest_retry_should_log(9));
        assert!(dhan_rest_retry_should_log(10));
        assert!(!dhan_rest_retry_should_log(11));
        assert!(dhan_rest_retry_should_log(20));
        assert!(dhan_rest_retry_should_log(100));
        assert!(!dhan_rest_retry_should_log(0), "attempt 0 never logs");
    }

    /// `dhan_rest_peer_page_due` — the Telegram fires once, and only past
    /// the 90s-TTL self-stale window (cumulative backoff before attempt 5 =
    /// 10+20+40+80 = 150s > INSTANCE_LOCK_TTL_SECS), so a quick restart
    /// never pages against its own stale lock entry.
    #[test]
    fn test_dhan_rest_peer_page_due_defers_past_ttl_and_latches() {
        // Prove the deferral really clears the TTL window.
        let cumulative_before_page: u64 = (1..DHAN_REST_STACK_PEER_PAGE_MIN_ATTEMPT)
            .map(dhan_rest_stack_backoff_secs)
            .sum();
        assert!(
            cumulative_before_page > instance_lock::INSTANCE_LOCK_TTL_SECS,
            "the page threshold must sit past the {}s lock TTL (self-stale \
             quick-restart window); cumulative backoff before the page = {}s",
            instance_lock::INSTANCE_LOCK_TTL_SECS,
            cumulative_before_page
        );
        assert!(!dhan_rest_peer_page_due(1, false));
        assert!(!dhan_rest_peer_page_due(4, false));
        assert!(dhan_rest_peer_page_due(5, false));
        assert!(dhan_rest_peer_page_due(50, false));
        // Latched: once paged, never again.
        assert!(!dhan_rest_peer_page_due(5, true));
        assert!(!dhan_rest_peer_page_due(500, true));
    }
}
