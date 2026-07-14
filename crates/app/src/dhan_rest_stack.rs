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
//!    `tickvault_core::instance_lock` exactly as the lane does). The two
//!    failure classes are handled DIFFERENTLY (2026-07-13 hostile-review
//!    HIGH — the lurk-and-steal fix; recorded in
//!    `dual-instance-lock-2026-07-04.md` §3.5):
//!    - **AlreadyHeld** (`stage = "already_held"`): bounded patience of
//!      [`DHAN_REST_STACK_ALREADYHELD_PATIENCE_SECS`] cumulative backoff
//!      (≫ the 90s lock TTL, so our OWN previous process's stale entry
//!      from a deploy/daily-restart clears inside the window and we
//!      acquire) — then the stack pages ONCE (`DualInstanceDetected`) and
//!      **PARKS permanently**: no further SSM polling, so a genuine live
//!      peer's lock can structurally NEVER be seized via the 90s
//!      stale-takeover the moment that peer restarts. Restart is the only
//!      retry. Holder machine identity is NOT reliably comparable (every
//!      `generate_host_id` call site passes `aws_instance_id = None`, so
//!      the machine component is the literal `local` everywhere) — hence
//!      the bounded window applies to ALL AlreadyHeld holders, same- and
//!      different-machine alike.
//!    - **SSM transport errors** (`stage = "ssm_transport"`): bounded
//!      exponential backoff (cap [`DHAN_REST_STACK_BACKOFF_CAP_SECS`]),
//!      retried forever in the background — an SSM outage proves nothing
//!      about peers, and fail-closed means no mint until the lock is held.
//!    Either way the process is NEVER halted and the Groww feed / shared
//!    infra are NEVER blocked by this task;
//! 2. `TokenManager::initialize` (SSM creds → TOTP → JWT) with the SAME
//!    `instance_lock_held` wiring (RESILIENCE-03 mint tripwire), then
//!    `set_global_token_manager` + `feed_runtime.set_live_token_manager` so
//!    the token gauges read this manager;
//! 3. the token renewal loop + the mid-session profile watchdog (SILENT
//!    since 2026-07-14 — coded errors/counters only; terminal re-mint
//!    failure pages the family-(c) `AuthenticationFailed` Critical), the
//!    GAP-02 900s stale-token sweep and the GAP-06 re-homed token-health
//!    gauge poller (`dhan-rest-only-noise-lock-2026-07-14.md`);
//! 4. the per-minute `spot_1m_rest` scheduler and the per-minute
//!    `option_chain_1m` scheduler / entitlement probe — mirroring
//!    `main.rs::spawn_post_market_tasks` (incl. the spot→chain sequencing
//!    watch channel and the existing `[spot_1m_rest]` /
//!    `[option_chain_1m]` config gates);
//! **Deliberately NOT spawned:** the WS pool, universe build / CSV
//! download, prev-day OHLCV, the SLO publisher, the 15:31 cross-verify,
//! the EOD digest, the orphan-position watchdog — AND, since 2026-07-14
//! (operator Dhan noise lock, `dhan-rest-only-noise-lock-2026-07-14.md` +
//! `websocket-connection-scope-lock.md` §A.1), the ORDER-UPDATE WS (the
//! PR-C1/Q4-i functional-dormant spawn is RETIRED: it opened a daily
//! socket to a demonstrably RST-flaky Dhan endpoint that protected
//! nothing while dry_run=true — events were counted-then-DISCARDED — and
//! was the stack's only HIGH-page noise source, WS-GAP-10; the core
//! module `order_update_connection.rs` stays DORMANT for the live-trading
//! re-wire, which needs a fresh dated quote in the scope-lock file first)
//! and the REST canary (retired the same day — the spot/chain legs
//! self-detect REST death via their own escalation edges).
//!
//! **Mutual exclusion by construction:** this stack is spawned ONLY from the
//! Dhan-OFF branch of main.rs (the `else` of `if config.feeds.dhan_enabled`)
//! AND only when the RAW boot TOML retires the lane
//! (`FeedRuntimeState::is_dhan_config_enabled() == false`, seeded
//! PRE-overlay — round-2 FIX A, 2026-07-13). The same raw value makes the
//! /api/feeds handler 409-refuse a Dhan enable and the runtime cold-start
//! supervisor refuse a lane start — so the lane's own lock / TokenManager /
//! post-market seam can never run alongside this stack. On a config-ON boot
//! whose runtime overlay left Dhan off, this stack does NOT spawn (the lane
//! is dormant, not retired — a runtime re-enable cold-starts the full lane,
//! which owns the REST surface via `spawn_post_market_tasks`). A
//! process-global once-guard additionally rejects a double spawn.
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
//! with backoff inside the bounded AlreadyHeld patience window and wins
//! once the TTL expires; the DualInstanceDetected Telegram is deferred
//! past the TTL window ([`DHAN_REST_STACK_PEER_PAGE_MIN_ATTEMPT`]) so a
//! quick restart never pages against its own stale entry. A holder that
//! SURVIVES the patience window is a genuine live peer (its heartbeat kept
//! renewing past the TTL) → one page + permanent park (see module item 1).

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
/// Bounded AlreadyHeld patience (2026-07-13 lurk-and-steal fix): total
/// cumulative backoff the stack is willing to wait on a held lock before
/// PARKING permanently. 5 minutes ≫ the 90s TTL, so a same-machine
/// previous process (deploy / daily restart / crash) always goes stale and
/// is taken over INSIDE the window; a holder that survives it has a live
/// heartbeat — a genuine peer we must never lurk against (a lurking loop
/// would seize the peer's lock via the 90s stale-takeover the instant the
/// peer restarts, then mint and kill the peer's token).
const DHAN_REST_STACK_ALREADYHELD_PATIENCE_SECS: u64 = 300;
/// Floor on the token-init retry ladder (2026-07-13 mint-cooldown fix):
/// every gap between `TokenManager::initialize` attempts must clear Dhan's
/// ~125s generateAccessToken cooldown — see
/// [`dhan_rest_token_backoff_secs`].
const DHAN_REST_STACK_TOKEN_RETRY_FLOOR_SECS: u64 = 130;
/// Process-global once-guard: the REST-only stack must never be brought up
/// twice (N stacks = N heartbeats + N renewal loops + N spot/chain
/// families). First caller wins; later calls log INFO and return `None`.
static DHAN_REST_STACK_SPAWNED: AtomicBool = AtomicBool::new(false);

/// Process-global once-guard for the Dhan-REST SCHEDULED TASK FAMILY —
/// SHARED between the lane path (`main.rs::spawn_post_market_tasks`: REST
/// spot_1m_rest + option_chain_1m + the lane-only orphan watchdog
/// / EOD digest / 1m cross-verify) and this REST-only stack's Phase 5
/// (spot + chain; the canary was retired 2026-07-14).
///
/// INVARIANT (2026-07-13 hostile-review MEDIUM): the family is spawned AT
/// MOST ONCE per process, WHICHEVER path claims first — so a future
/// relaxation of the runtime cold-start refusal (or any new path into
/// `run_dhan_lane_cold_start` → `spawn_post_market_tasks`) can never
/// double-spawn the spot/chain schedulers alongside this stack's
/// (double Data-API pulls per minute close, double Telegram). Mutual
/// exclusion by construction still holds today; this guard makes it
/// mechanical instead of situational.
static POST_MARKET_TASK_FAMILY_CLAIMED: AtomicBool = AtomicBool::new(false);

/// Claim the shared Dhan-REST task-family once-guard: `true` exactly once
/// per process (first caller wins). Called by BOTH spawn paths — the
/// lane's `spawn_post_market_tasks` (main.rs) and this stack's Phase 5.
#[must_use]
pub fn claim_post_market_task_family_once() -> bool {
    !POST_MARKET_TASK_FAMILY_CLAIMED.swap(true, Ordering::SeqCst)
}

/// Everything the bring-up task needs — Arc clones of the process-shared
/// infra main.rs already built (nothing here is lane-owned).
pub struct DhanRestStackParams {
    /// Immutable boot config snapshot (deep copy — no env re-parse).
    pub config: Arc<ApplicationConfig>,
    /// Telegram dispatcher (shared strict-init NotificationService).
    pub notifier: Arc<NotificationService>,
    /// Trading calendar for the spot/chain trading-day gates.
    pub calendar: Arc<TradingCalendar>,
    /// Runtime feed-state — receives `set_live_token_manager` so the token
    /// gauges read this stack's manager.
    pub feed_runtime: Arc<FeedRuntimeState>,
    /// The runtime's mark receiver, stashed by main.rs and taken ONCE at
    /// spawn (a `Receiver` is not `Clone`; the slot keeps the params struct
    /// constructible on every boot arm). `None` inside = already taken or
    /// runtime disabled.
    pub mark_rx_slot: Arc<
        std::sync::Mutex<Option<tokio::sync::mpsc::Receiver<crate::order_runtime::MarkUpdate>>>,
    >,
    /// Shared mark-gate flag (the Groww bridge's per-tick `Relaxed` load).
    pub marks_wanted: Arc<AtomicBool>,
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

/// PARK decision for the lock-acquire loop (2026-07-13 lurk-and-steal fix):
/// once the CUMULATIVE AlreadyHeld backoff reaches
/// [`DHAN_REST_STACK_ALREADYHELD_PATIENCE_SECS`], the holder has out-lived
/// the 90s TTL by a wide margin (its heartbeat is alive — a genuine peer)
/// and the stack must stop polling SSM entirely so it can never seize that
/// peer's lock via the stale-takeover. SSM transport errors do NOT feed
/// this counter (they prove nothing about peers). Pure — unit-tested.
#[must_use]
pub fn dhan_rest_lock_park_due(already_held_wait_secs: u64) -> bool {
    already_held_wait_secs >= DHAN_REST_STACK_ALREADYHELD_PATIENCE_SECS
}

/// Backoff for the `TokenManager::initialize` retry loop — floored at
/// [`DHAN_REST_STACK_TOKEN_RETRY_FLOOR_SECS`] so EVERY inter-attempt gap
/// clears Dhan's ~125s generateAccessToken mint cooldown from attempt 1
/// (130s → 260s → 300s cap; the generic 10/20/40/80s early rungs sat
/// INSIDE the cooldown). Pure — unit-tested.
#[must_use]
pub fn dhan_rest_token_backoff_secs(attempt: u32) -> u64 {
    // Shift clamped to 2: 130 << 2 = 520 already exceeds the 300s cap.
    let shift = attempt.saturating_sub(1).min(2);
    (DHAN_REST_STACK_TOKEN_RETRY_FLOOR_SECS << shift).min(DHAN_REST_STACK_BACKOFF_CAP_SECS)
}

/// Spawn the Dhan REST-only stack bring-up as ONE background task (plus a
/// tiny exit monitor so an unwind-build death is never silent). Returns
/// `None` when the once-guard rejects a duplicate spawn.
///
/// Called ONLY from main.rs's Dhan-OFF branch, and only when the RAW boot
/// TOML retires the lane — see the module docs for the mutual-exclusion
/// contract with the Dhan lane.
// The pure decisions (dhan_rest_stack_backoff_secs, dhan_rest_retry_should_log,
// dhan_rest_peer_page_due) are unit-tested below and the wiring is pinned by
// crates/app/tests/dhan_live_off_phase_a_guard.rs. The guard heuristic reads
// only the line directly above the fn, so the exemption token sits last:
// TEST-EXEMPT: orchestration spawn around live I/O (SSM lock + TOTP mint + task spawns)
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
                 Dhan REST surface (spot_1m_rest / option_chain_1m) may be absent \
                 this session; restart to re-run the bring-up"
            );
        }
    }))
}

/// The bring-up body: lock → token → renewal/watchdog/sweep/gauge → spot/chain.
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
         WS lane retired; REST retained surface: spot_1m_rest + option_chain_1m)"
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
        // 2026-07-13 lurk-and-steal fix: AlreadyHeld and SSM-transport
        // failures are SEPARATE arms with SEPARATE counters. AlreadyHeld
        // accumulates a bounded patience budget and then PARKS the stack
        // permanently (never lurks, never steals — see the module docs and
        // dual-instance-lock-2026-07-04.md §3.5); SSM transport errors
        // retry forever with bounded backoff (an outage proves nothing
        // about peers; fail-closed — no mint until the lock is held).
        let mut held_attempt: u32 = 0;
        let mut held_wait_secs: u64 = 0;
        let mut ssm_attempt: u32 = 0;
        let mut peer_paged = false;
        let mut last_holder = String::new();
        loop {
            if dhan_rest_lock_park_due(held_wait_secs) {
                // Patience exhausted: the holder's heartbeat kept renewing
                // far past the 90s TTL — a genuine live peer, not our own
                // stale entry. PARK: no further SSM polling from this
                // process, so the peer's lock can never be seized via the
                // stale-takeover the moment the peer restarts.
                //
                // Invariant (round-2 FIX B — the former defensive re-page
                // here was dead code): the DualInstanceDetected page has
                // ALWAYS fired before park — the ladder's page attempt (5,
                // 150s cumulative) strictly precedes the 300s park threshold
                // (first reachable at 310s, loop top), pinned by
                // test_dhan_rest_peer_page_due_genuine_peer_pages_once_then_parks.
                error!(
                    code = ErrorCode::Resilience01DualInstanceDetected.code_str(),
                    severity = ErrorCode::Resilience01DualInstanceDetected
                        .severity()
                        .as_str(),
                    stage = "already_held_parked",
                    peer = %last_holder,
                    lock_key = %lock_key,
                    waited_secs = held_wait_secs,
                    "Dhan REST-only stack PARKED: a live peer has held the \
                     dual-instance lock past the {}s patience window — refusing to \
                     lurk (a parked stack can never seize the peer's lock via the \
                     90s stale-takeover); the retained Dhan REST surface stays DOWN \
                     this process while Groww + shared infra keep running; RESTART \
                     is the only retry",
                    DHAN_REST_STACK_ALREADYHELD_PATIENCE_SECS
                );
                info!(
                    "Dhan REST-only stack parked — no further SSM lock polling this \
                     process (restart to re-attempt once the peer is stopped)"
                );
                return;
            }
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
                    held_attempt = held_attempt.saturating_add(1);
                    if dhan_rest_retry_should_log(held_attempt) {
                        error!(
                            code = ErrorCode::Resilience01DualInstanceDetected.code_str(),
                            severity = ErrorCode::Resilience01DualInstanceDetected
                                .severity()
                                .as_str(),
                            stage = "already_held",
                            peer = %holder,
                            lock_key = %lock_key,
                            attempt = held_attempt,
                            waited_secs = held_wait_secs,
                            "Dhan REST-only stack: another process holds the dual-instance \
                             lock — bounded patience in progress (parks at {}s; the peer's \
                             Dhan session is untouched — no mint before the lock)",
                            DHAN_REST_STACK_ALREADYHELD_PATIENCE_SECS
                        );
                    } else {
                        debug!(
                            attempt = held_attempt,
                            peer = %holder,
                            "dual-instance lock still held — bounded patience retry"
                        );
                    }
                    if dhan_rest_peer_page_due(held_attempt, peer_paged) {
                        // Past the 90s TTL self-stale window (see the const
                        // docs) — a genuine live peer. Page ONCE per process.
                        params
                            .notifier
                            .notify(NotificationEvent::DualInstanceDetected {
                                holder: holder.clone(),
                                lock_key: lock_key.clone(),
                            });
                        peer_paged = true;
                    }
                    last_holder = holder;
                    let wait = dhan_rest_stack_backoff_secs(held_attempt);
                    held_wait_secs = held_wait_secs.saturating_add(wait);
                    tokio::time::sleep(Duration::from_secs(wait)).await;
                }
                Err(err) => {
                    ssm_attempt = ssm_attempt.saturating_add(1);
                    if dhan_rest_retry_should_log(ssm_attempt) {
                        error!(
                            code = ErrorCode::Resilience01DualInstanceDetected.code_str(),
                            severity = ErrorCode::Resilience01DualInstanceDetected
                                .severity()
                                .as_str(),
                            stage = "ssm_transport",
                            error = %err,
                            lock_key = %lock_key,
                            attempt = ssm_attempt,
                            "Dhan REST-only stack: SSM lock acquire failed (transport) — \
                             retrying in background (cannot prove there is no peer, so \
                             no mint yet; transport errors never count toward the \
                             AlreadyHeld park patience)"
                        );
                    } else {
                        debug!(
                            attempt = ssm_attempt,
                            error = %err,
                            "SSM lock acquire failed (transport) — retrying"
                        );
                    }
                    tokio::time::sleep(Duration::from_secs(dhan_rest_stack_backoff_secs(
                        ssm_attempt,
                    )))
                    .await;
                }
            }
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
    // RESILIENCE-03 mint tripwire. Retry-forever with a ≥130s floor between
    // initialize attempts (`dhan_rest_token_backoff_secs`: 130 → 260 →
    // 300s cap) so EVERY retry clears Dhan's ~125s generateAccessToken
    // cooldown from attempt 1 (2026-07-13 fix — the generic 10/20/40/80s
    // early rungs sat INSIDE the cooldown). A failed/timed-out init logs
    // loudly (TokenManager's own internals page AUTH-GAP-04 /
    // AuthenticationFailed as today).
    //
    // Honest residual (same class as AUTH-GAP-05 AG5-R2-1): the
    // TOKEN_INIT_TIMEOUT_SECS arm abandons an IN-FLIGHT mint that may have
    // SUCCEEDED server-side — the next attempt's fresh mint invalidates
    // that orphaned token. Bounded and self-correcting (Dhan enforces one
    // active token; the retry loop converges on the newest mint), but it
    // is a real extra mint, not zero.
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
                             in background (no WS lane exists; spot/chain stay down \
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
                             unreachable; retrying in background (an in-flight mint that \
                             succeeded server-side is orphaned and superseded by the next \
                             attempt's fresh mint — bounded, self-correcting)"
                        );
                    } else {
                        debug!(attempt, "REST-stack auth timed out — retrying");
                    }
                }
            }
            // ≥130s floor — every gap clears the ~125s mint cooldown.
            tokio::time::sleep(Duration::from_secs(dhan_rest_token_backoff_secs(attempt))).await;
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
    // Phase 3: renewal loop + mid-session profile watchdog + (2026-07-14,
    // Dhan noise lock backstops) the GAP-02 stale-token sweep and the
    // GAP-06 re-homed token-health gauge poller. All need only the
    // TokenManager (+ notifier for the watchdog's terminal family-(3) arm).
    // -----------------------------------------------------------------------
    let _renewal_handle = token_manager.spawn_renewal_task();
    info!("Dhan REST-only stack: token renewal task started");

    let token_profile_valid = Arc::new(AtomicBool::new(true));
    let _mid_session_watchdog_handle =
        tickvault_core::auth::mid_session_watchdog::spawn_mid_session_profile_watchdog(
            Arc::clone(&token_manager),
            Some(Arc::clone(&params.notifier)),
            Arc::clone(&token_profile_valid),
        );
    info!(
        "Dhan REST-only stack: mid-session profile watchdog spawned \
         (15-min cadence; silent self-heal — only a terminal re-mint failure pages)"
    );

    // GAP-06 (2026-07-14): the token-health gauge poller is RE-HOMED here
    // (its lane/fast-arm spawn sites are deleted) so `tv_token_valid` +
    // the LIVE `tv_token_remaining_seconds` stay published on dhan-off
    // boots even after a renewal-loop circuit-breaker halt kills the
    // renewal loop's own mint-time snapshot writes — keeping the
    // `tv-<env>-token-remaining-low` CloudWatch alarm (family-(4))
    // sighted. Shares the SAME profile-truth flag as the watchdog above,
    // so a Dhan-killed (but locally unexpired) token reads 0 within one
    // watchdog cycle + one 15s poll.
    // L-fix (2026-07-14 fix round): supervised with the house respawn
    // pattern — a silent poller death would blind the family-(4) alarm
    // (the exact audited gap GAP-06 closed). Unwind-build self-heal only;
    // release panics abort the process (panic = "abort").
    let _token_health_gauge_supervisor = {
        let gauge_token_manager = Arc::clone(&token_manager);
        let gauge_profile_valid = Arc::clone(&token_profile_valid);
        spawn_supervised_stack_task(
            "token_health_gauge",
            STACK_TASK_RESPAWN_BACKOFF_SECS,
            move || {
                tickvault_core::auth::token_health_gauge::spawn_token_health_gauge_poller(
                    Arc::clone(&gauge_token_manager),
                    Arc::clone(&gauge_profile_valid),
                )
            },
        )
    };
    info!(
        poll_secs = tickvault_core::auth::token_health_gauge::TOKEN_HEALTH_GAUGE_POLL_SECS,
        "Dhan REST-only stack: live token-health gauge poller spawned (supervised; \
         tv_token_remaining_seconds + tv_token_valid)"
    );

    // GAP-02 (2026-07-14): stale-token sweep — the renewal-loop-halt
    // backstop the lane's 4h sweep used to be, at a 900s cadence matched
    // to how fast the spot/chain legs feel a dead token. SILENT on
    // no-op/success (debug!); `force_renewal_if_stale` renews only on
    // < 4h local headroom, honoring the RESILIENCE-03 lock tripwire; a
    // terminal mint failure pages via the token machinery's own
    // family-(3) paths. NOT market-hours gated.
    // L-fix (2026-07-14 fix round): supervised — a silent sweep death
    // would silently recreate the audited renewal-loop-halt gap GAP-02
    // exists to close.
    let _token_sweep_supervisor = {
        let sweep_token_manager = Arc::clone(&token_manager);
        spawn_supervised_stack_task("token_sweep", STACK_TASK_RESPAWN_BACKOFF_SECS, move || {
            let tm = Arc::clone(&sweep_token_manager);
            tokio::spawn(run_token_sweep_loop(tm))
        })
    };
    info!(
        interval_secs = tickvault_common::constants::DHAN_REST_STACK_TOKEN_SWEEP_INTERVAL_SECS,
        "Dhan REST-only stack: stale-token sweep spawned (silent renewal-loop backstop)"
    );

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
    // main.rs::spawn_post_market_tasks (spot / chain arms only;
    // orphan watchdog, EOD digest and cross-verify deliberately stay
    // lane-only per the Phase A scope).
    // -----------------------------------------------------------------------
    // Shared once-guard (2026-07-13 hostile-review MEDIUM): claim the
    // Dhan-REST task family BEFORE spawning it, the SAME guard the lane's
    // spawn_post_market_tasks claims — so a future relaxation of the
    // runtime cold-start refusal can never run spot/chain TWICE in
    // one process. Unreachable today (mutual exclusion by construction);
    // if it ever fires, the invariant is broken — stay down loudly.
    if !claim_post_market_task_family_once() {
        error!(
            "Dhan REST-only stack: the Dhan-REST scheduled task family is ALREADY \
             claimed this process (the lane's spawn_post_market_tasks ran first) — \
             refusing to double-spawn spot_1m_rest/option_chain_1m; the \
             lane/stack mutual-exclusion invariant is broken, investigate (the \
             stack stays DOWN: gauge remains 0)"
        );
        return;
    }
    let token_handle = token_manager.token_handle();
    let config = &params.config;

    // -----------------------------------------------------------------------
    // Phase 5a (the PR-C1/Q4-i functional-dormant ORDER-UPDATE WS spawn)
    // RETIRED 2026-07-14 — operator Dhan noise lock
    // (dhan-rest-only-noise-lock-2026-07-14.md; scope-lock §A.1 records the
    // superseding quote). The dormant socket protected nothing while
    // dry_run=true (events counted-then-DISCARDED, no WAL, no OMS) and was
    // this stack's only HIGH-page noise source (WS-GAP-10) against a
    // demonstrably RST-flaky Dhan endpoint. The core module
    // `order_update_connection.rs` stays DORMANT for the live-trading
    // re-wire (fresh dated quote required in the scope-lock file first).
    // -----------------------------------------------------------------------
    // Phase 5b (order-runtime dry-run PR, 2026-07-14 — SOCKET-FREE shape):
    // the config-gated DRY-RUN ORDER RUNTIME. Under the noise lock this
    // stack opens NO Dhan WebSocket and performs NO order-update frame
    // capture or boot drain — the runtime's order-update broadcast channel
    // is created locally with ZERO producers (paper fills are synthesized
    // INSIDE the runtime via `oms.handle_order_update`, never through this
    // channel), and its auth-notify seam never fires (the timer-driven
    // reconcile scheduler covers the boot reconcile). The live re-arm
    // follow-up — the socket spawn + durable frame capture/drain + the two
    // CloudWatch order-update alarms #1532 deleted — re-attaches producers
    // to EXACTLY this seam, and requires the operator's fresh dated quote
    // in dhan-rest-only-noise-lock-2026-07-14 §3 / scope-lock §A.1 FIRST.
    // Placed AFTER the family claim above so a broken lane/stack
    // mutual-exclusion invariant can never produce a SECOND runtime
    // (dual-OMS split-brain, design F8; the spawn-site guard pins this as
    // the sole call site). DISABLED (default): nothing spawns — the stack
    // is byte-identical to the post-#1532 noise-lock shape.
    // -----------------------------------------------------------------------
    if config.order_runtime.enabled {
        // Stack-local broadcast channel (capacity mirrors the legacy 256).
        // The construction receiver IS the runtime's first receiver — there
        // is no earlier producer to order against (no socket, no drain).
        let (order_update_sender, first_order_update_rx) =
            tokio::sync::broadcast::channel::<tickvault_common::order_types::OrderUpdate>(256);
        // The auth seam: never notified today (no socket) — the runtime's
        // one-shot reconcile-on-auth arm idles by construction; kept so the
        // gated live re-arm wires the socket's auth signal into the SAME
        // params field instead of growing a second seam.
        let auth_signal = Arc::new(tokio::sync::Notify::new());
        // Take the mark receiver ONCE from the boot slot (poisoning-safe
        // — the slot is written once by main.rs before this task spawns).
        let mark_rx = params
            .mark_rx_slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        let mark_rx = match mark_rx {
            Some(rx) => rx,
            None => {
                // Structurally unreachable (once-guarded spawn + the slot
                // is filled on every enabled boot): degrade to a mark-less
                // runtime — the leaked sender keeps the dummy channel open
                // so the mark arm just idles.
                error!(
                    "order runtime: mark receiver slot was EMPTY at spawn — running \
                     mark-less (paper fills defer until marks return; investigate the \
                     boot wiring)"
                );
                let (dummy_tx, rx) = tokio::sync::mpsc::channel(1);
                std::mem::forget(dummy_tx);
                rx
            }
        };
        let _order_runtime_supervisor =
            crate::order_runtime::spawn_order_runtime(crate::order_runtime::OrderRuntimeParams {
                config: Arc::clone(&params.config),
                notifier: params.notifier.clone(),
                calendar: Arc::clone(&params.calendar),
                order_update_sender,
                first_order_update_rx,
                mark_rx,
                marks_wanted: Arc::clone(&params.marks_wanted),
                token_handle: Arc::clone(&token_handle),
                client_id: client_id.clone(),
                auth_notify: auth_signal,
            });
        info!(
            "Dhan REST-only stack: DRY-RUN order runtime spawned (socket-free — \
             paper fills + Groww marks + daily-loss halt + reconcile heartbeat; \
             NO Dhan WebSocket and NO order-event frame capture, per the \
             2026-07-14 operator Dhan noise lock)"
        );
    }

    // REST-health canary (DHAN-REST-400) DELETED 2026-07-14 (operator Dhan
    // noise lock): the spot-1m + option-chain legs self-detect a dead REST
    // surface within ~3-4 minutes via their own escalation edges.

    // Spot→chain sequencing signal — created ONLY when BOTH halves are
    // enabled (byte-identical to the spawn_post_market_tasks wiring).
    let (spot_minute_done_tx, spot_minute_done_rx) =
        if config.spot_1m_rest.enabled && config.option_chain_1m.enabled {
            let (tx, rx) = tokio::sync::watch::channel::<Option<u32>>(None);
            (Some(tx), Some(rx))
        } else {
            (None, None)
        };

    // 2026-07-14 operator pacing directive: configure the shared Dhan
    // Data-API limiter cap from `[dhan_data_api] target_rps` BEFORE any
    // REST task spawns (idempotent; validate() already rejected an
    // out-of-range value at boot).
    crate::dhan_data_api_limiter::configure_shared_dhan_data_api_limiter(
        config.dhan_data_api.target_rps,
    );

    if config.spot_1m_rest.enabled {
        let _spot1m_supervisor = crate::spot_1m_rest_boot::spawn_supervised_spot_1m_rest(
            crate::spot_1m_rest_boot::Spot1mRestTaskParams {
                token_handle: Arc::clone(&token_handle),
                notifier: params.notifier.clone(),
                calendar: Arc::clone(&params.calendar),
                questdb: config.questdb.clone(),
                rest_api_base_url: config.dhan.rest_api_base_url.clone(),
                minute_done_tx: spot_minute_done_tx,
                diagnostics_enabled: config.spot_1m_rest.diagnostics,
                diagnostics_second_probe_secs_of_day_ist: config
                    .spot_1m_rest
                    .diagnostics_second_probe_secs_of_day_ist,
                fetch_mode: config.spot_1m_rest.fetch_mode,
                batch_interval_minutes: config.spot_1m_rest.batch_interval_minutes,
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
        "DHAN REST-ONLY STACK UP — lock + token + renewal + silent mid-session watchdog + \
         token sweep + token-health gauge + spot_1m_rest + option_chain_1m arms spawned \
         WITHOUT any Dhan WebSocket (operator directives 2026-07-13 + 2026-07-14)"
    );

    // M3 (2026-07-14 fix round): with BOTH per-minute legs disabled the
    // stack has ZERO leg-level pager coverage — a total Dhan REST death
    // would page nothing except the token family-(3) Critical + the
    // CloudWatch token-remaining-low early warning. Loud at boot so the
    // operator can never discover this from silence.
    if rest_stack_has_zero_pager_legs(config.spot_1m_rest.enabled, config.option_chain_1m.enabled) {
        warn!(
            spot_1m_rest_enabled = false,
            option_chain_1m_enabled = false,
            "Dhan REST stack up with ZERO legs enabled — no pager coverage: a total \
             Dhan REST death would fire NO leg alert (only the token family-(3) \
             Critical + the tv-token-remaining-low CloudWatch alarm remain); enable \
             [spot_1m_rest] / [option_chain_1m] if this is not intentional"
        );
    }
}

/// M3 (2026-07-14 fix round). Pure. True iff NEITHER per-minute Dhan REST
/// leg is enabled — the configuration under which the stack has zero
/// leg-level pager coverage (the boot-time warn above fires).
#[must_use]
pub(crate) fn rest_stack_has_zero_pager_legs(spot_enabled: bool, chain_enabled: bool) -> bool {
    !spot_enabled && !chain_enabled
}

/// GAP-02 sweep body (extracted for the supervisor): every
/// `DHAN_REST_STACK_TOKEN_SWEEP_INTERVAL_SECS`, renew the token iff its
/// local headroom is < `TOKEN_SWEEP_STALENESS_THRESHOLD_SECS`. SILENT on
/// no-op/success (debug!); the renewal machinery's own paths own paging.
async fn run_token_sweep_loop(token_manager: Arc<TokenManager>) {
    use tickvault_common::constants::{
        DHAN_REST_STACK_TOKEN_SWEEP_INTERVAL_SECS, TOKEN_SWEEP_STALENESS_THRESHOLD_SECS,
    };
    let interval = Duration::from_secs(DHAN_REST_STACK_TOKEN_SWEEP_INTERVAL_SECS);
    loop {
        tokio::time::sleep(interval).await;
        match token_manager
            .force_renewal_if_stale(TOKEN_SWEEP_STALENESS_THRESHOLD_SECS)
            .await
        {
            Ok(true) => {
                info!("Dhan REST-only stack token sweep: renewed stale token (< 4h headroom)");
                metrics::counter!("tv_token_sweep_renewals_total", "result" => "renewed")
                    .increment(1);
            }
            Ok(false) => {
                debug!("Dhan REST-only stack token sweep: token still fresh, no action");
                metrics::counter!("tv_token_sweep_renewals_total", "result" => "fresh")
                    .increment(1);
            }
            Err(err) => {
                // The AUTH-GAP-05 watchdog's GAP-04 re-arm + this sweep's own
                // next tick are the retry paths. L truth-fix (2026-07-14 fix
                // round): the ~23h renewal loop is NOT an independent retry —
                // it HALTS PERMANENTLY after its circuit-breaker cycles are
                // exhausted (that halt is exactly why this sweep exists).
                warn!(
                    error = %err,
                    "Dhan REST-only stack token sweep: force_renewal_if_stale failed — \
                     next sweep tick + the AUTH-GAP-05 watchdog retry (the ~23h renewal \
                     loop halts permanently after its circuit-breaker; it is NOT a backstop)"
                );
                metrics::counter!("tv_token_sweep_renewals_total", "result" => "failed")
                    .increment(1);
            }
        }
    }
}

/// L-fix (2026-07-14 fix round): respawn backoff for the supervised
/// stack background tasks (the disk_health_watcher / WS-GAP-05 house
/// cadence).
const STACK_TASK_RESPAWN_BACKOFF_SECS: u64 = 5;

/// L-fix (2026-07-14 fix round): minimal house respawn supervisor for the
/// stack's forever-tasks (GAP-02 sweep + GAP-06 gauge poller). The inner
/// task is an infinite loop, so ANY resolution of its `JoinHandle` is
/// abnormal: log + count + backoff + respawn. A CANCELLED inner task means
/// external teardown — the supervisor exits without respawn. Honest panic
/// envelope (the TICK-FLUSH-01 precedent): release builds run with
/// `panic = "abort"`, so the panic-respawn arm is an unwind-build/test
/// self-heal only; in production a panicking task aborts the process and
/// recovery is a restart.
fn spawn_supervised_stack_task<F>(
    task: &'static str,
    backoff_secs: u64,
    factory: F,
) -> tokio::task::JoinHandle<()>
where
    F: Fn() -> tokio::task::JoinHandle<()> + Send + 'static,
{
    tokio::spawn(async move {
        loop {
            let inner = factory();
            let reason = match inner.await {
                Ok(()) => "clean_exit",
                Err(join_err) if join_err.is_cancelled() => return,
                Err(_) => "panic",
            };
            error!(
                task,
                reason,
                backoff_secs,
                "Dhan REST-only stack background task died — respawning (silent death \
                 here would re-open the audited GAP-02/GAP-06 coverage gap)"
            );
            metrics::counter!(
                "tv_dhan_rest_stack_task_respawn_total",
                "task" => task,
                "reason" => reason
            )
            .increment(1);
            tokio::time::sleep(Duration::from_secs(backoff_secs)).await;
        }
    })
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

    /// Same-machine bounded patience (2026-07-13 lurk-and-steal fix,
    /// bounded-window FALLBACK — holder machine identity is NOT reliably
    /// comparable because every `generate_host_id` call site passes
    /// `aws_instance_id = None`, so the machine component is `local`
    /// everywhere): the patience window must comfortably cover the 90s
    /// lock TTL so our OWN previous process's stale entry (deploy / daily
    /// restart / crash) is always taken over INSIDE the window, and the
    /// park predicate must be false everywhere below the threshold.
    #[test]
    fn test_dhan_rest_lock_park_due_same_machine_patience_covers_ttl() {
        // ≥3x TTL margin: a same-machine restart can never be mislabeled a
        // genuine peer just because the TTL takeover was a beat late.
        assert!(
            DHAN_REST_STACK_ALREADYHELD_PATIENCE_SECS >= 3 * instance_lock::INSTANCE_LOCK_TTL_SECS,
            "patience {}s must be >= 3x the {}s lock TTL",
            DHAN_REST_STACK_ALREADYHELD_PATIENCE_SECS,
            instance_lock::INSTANCE_LOCK_TTL_SECS
        );
        assert!(!dhan_rest_lock_park_due(0));
        assert!(!dhan_rest_lock_park_due(
            instance_lock::INSTANCE_LOCK_TTL_SECS
        ));
        assert!(!dhan_rest_lock_park_due(
            DHAN_REST_STACK_ALREADYHELD_PATIENCE_SECS - 1
        ));
        assert!(dhan_rest_lock_park_due(
            DHAN_REST_STACK_ALREADYHELD_PATIENCE_SECS
        ));
        assert!(dhan_rest_lock_park_due(u64::MAX), "no overflow / saturates");
    }

    /// Foreign-machine (genuine live peer) arm: the page fires ONCE, only
    /// past the TTL self-stale window (a holder still fresh past the TTL
    /// has a live heartbeat = a real peer), and the loop then PARKS —
    /// cumulative backoff reaches the park threshold within one more
    /// attempt, so a lurking retry-forever loop is structurally impossible.
    #[test]
    fn test_dhan_rest_peer_page_due_genuine_peer_pages_once_then_parks() {
        // Page timing: cumulative backoff before the page attempt > TTL.
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
        // Latched: once paged, never again.
        assert!(!dhan_rest_peer_page_due(5, true));
        assert!(!dhan_rest_peer_page_due(500, true));
        // Page-before-park: the page attempt precedes the park instant...
        assert!(
            cumulative_before_page < DHAN_REST_STACK_ALREADYHELD_PATIENCE_SECS,
            "the peer page must fire BEFORE the park threshold"
        );
        // ...and the park is reached within ONE more backoff step after the
        // page (bounded lurk window; never retry-forever).
        let cumulative_through_page_attempt: u64 = (1..=DHAN_REST_STACK_PEER_PAGE_MIN_ATTEMPT)
            .map(dhan_rest_stack_backoff_secs)
            .sum();
        assert!(
            dhan_rest_lock_park_due(cumulative_through_page_attempt),
            "cumulative backoff through the page attempt ({}s) must reach the \
             {}s park threshold — AlreadyHeld can never lurk forever",
            cumulative_through_page_attempt,
            DHAN_REST_STACK_ALREADYHELD_PATIENCE_SECS
        );
    }

    /// SSM transport errors keep the bounded-backoff retry-FOREVER arm:
    /// the ladder stays capped (no overflow at extreme attempts), the
    /// coalesced-log discipline holds at any attempt count, and the park
    /// predicate consumes ONLY the AlreadyHeld cumulative wait (its sole
    /// parameter) — transport attempts can never park the stack.
    #[test]
    fn test_dhan_rest_lock_ssm_transport_retry_is_unbounded_and_never_parks() {
        // Ladder is safe at any transport-attempt count.
        assert_eq!(dhan_rest_stack_backoff_secs(6), 300);
        assert_eq!(dhan_rest_stack_backoff_secs(1_000_000), 300);
        assert_eq!(dhan_rest_stack_backoff_secs(u32::MAX), 300);
        // Coalescing holds forever (first + every 10th).
        assert!(dhan_rest_retry_should_log(1_000_000));
        assert!(!dhan_rest_retry_should_log(1_000_001));
        // Park is a function of the AlreadyHeld wait ONLY: with zero
        // AlreadyHeld wait accumulated, no number of transport retries can
        // trip it (the loop never feeds transport sleeps into it).
        assert!(!dhan_rest_lock_park_due(0));
    }

    /// `claim_post_market_task_family_once` — first caller wins, every
    /// later claim is refused (the shared lane/stack task-family guard;
    /// FIX 4 invariant: spot/chain can never spawn twice per
    /// process). NOTE: mutates the process-global static — no other test
    /// in this binary touches it.
    #[test]
    fn test_claim_post_market_task_family_once_first_caller_wins() {
        assert!(claim_post_market_task_family_once(), "first claim wins");
        assert!(!claim_post_market_task_family_once(), "second refused");
        assert!(!claim_post_market_task_family_once(), "latched forever");
    }

    /// `dhan_rest_token_backoff_secs` — every inter-attempt gap of the
    /// token-init loop clears Dhan's ~125s mint cooldown (130 → 260 →
    /// 300s cap), with no overflow at extreme attempts (2026-07-13
    /// mint-cooldown fix).
    #[test]
    fn test_dhan_rest_token_backoff_secs_floors_above_mint_cooldown() {
        assert_eq!(dhan_rest_token_backoff_secs(0), 130, "attempt 0 as 1");
        assert_eq!(dhan_rest_token_backoff_secs(1), 130);
        assert_eq!(dhan_rest_token_backoff_secs(2), 260);
        assert_eq!(dhan_rest_token_backoff_secs(3), 300, "cap engages");
        assert_eq!(dhan_rest_token_backoff_secs(4), 300);
        assert_eq!(dhan_rest_token_backoff_secs(u32::MAX), 300, "no overflow");
        // The floor itself clears the documented ~125s cooldown.
        for attempt in 0..64 {
            assert!(
                dhan_rest_token_backoff_secs(attempt) > 125,
                "attempt {attempt} gap must exceed the ~125s mint cooldown"
            );
        }
    }

    /// Exhaustive property sweep over the stack's whole pure decision
    /// surface (2026-07-13, post-#1499 merge hardening): every retry /
    /// page / park decision the bring-up loop can take is pinned across
    /// the full realistic attempt domain, not just the ladder's named
    /// rungs — a refactor that bends any monotonicity, cap, floor, or
    /// boundary fails here before it can reach the live SSM/mint loops.
    #[test]
    fn test_dhan_rest_stack_decision_surface_exhaustive_sweep() {
        // (a) Generic backoff: monotone non-decreasing, base-floored,
        //     hard-capped, and the cap is REACHED (never an unreachable
        //     asymptote) within the documented 6 rungs.
        let mut prev = 0u64;
        for attempt in 0..=64u32 {
            let gap = dhan_rest_stack_backoff_secs(attempt);
            assert!(gap >= DHAN_REST_STACK_BACKOFF_BASE_SECS, "base floor");
            assert!(gap <= DHAN_REST_STACK_BACKOFF_CAP_SECS, "hard cap");
            assert!(gap >= prev, "attempt {attempt}: backoff must never shrink");
            prev = gap;
        }
        assert_eq!(
            dhan_rest_stack_backoff_secs(6),
            DHAN_REST_STACK_BACKOFF_CAP_SECS,
            "cap reached at rung 6"
        );

        // (b) Token backoff: same shape AND strictly above the generic
        //     early rungs (the whole point of the 130s floor).
        let mut prev = 0u64;
        for attempt in 0..=64u32 {
            let gap = dhan_rest_token_backoff_secs(attempt);
            assert!(gap >= DHAN_REST_STACK_TOKEN_RETRY_FLOOR_SECS, "130s floor");
            assert!(gap <= DHAN_REST_STACK_BACKOFF_CAP_SECS, "hard cap");
            assert!(
                gap >= prev,
                "attempt {attempt}: token backoff must never shrink"
            );
            prev = gap;
        }

        // (c) Retry-log cadence: EXACT set membership over 0..=40 — first
        //     attempt, then every Nth; nothing else (log-storm bound).
        for attempt in 0..=40u32 {
            let expected = attempt == 1
                || (attempt > 0 && attempt % DHAN_REST_STACK_LOG_EVERY_N_ATTEMPTS == 0);
            assert_eq!(
                dhan_rest_retry_should_log(attempt),
                expected,
                "cadence mismatch at attempt {attempt}"
            );
        }
        assert!(!dhan_rest_retry_should_log(0), "attempt 0 never logs");

        // (d) Peer-page decision matrix: fires exactly once, only from the
        //     min-attempt boundary onward, and never re-fires once paged.
        let min = DHAN_REST_STACK_PEER_PAGE_MIN_ATTEMPT;
        assert!(!dhan_rest_peer_page_due(min - 1, false), "below boundary");
        assert!(dhan_rest_peer_page_due(min, false), "at boundary");
        assert!(dhan_rest_peer_page_due(min + 1, false), "above boundary");
        for attempt in 0..=64u32 {
            assert!(
                !dhan_rest_peer_page_due(attempt, true),
                "attempt {attempt}: already-paged must never re-page"
            );
        }

        // (e) Park boundary: exact >= threshold semantics (299/300/301)
        //     plus the domain extremes.
        let patience = DHAN_REST_STACK_ALREADYHELD_PATIENCE_SECS;
        assert!(!dhan_rest_lock_park_due(0), "fresh loop never parks");
        assert!(!dhan_rest_lock_park_due(patience - 1), "one second short");
        assert!(dhan_rest_lock_park_due(patience), "exactly at patience");
        assert!(dhan_rest_lock_park_due(patience + 1), "past patience");
        assert!(dhan_rest_lock_park_due(u64::MAX), "saturated domain end");
    }

    /// 2026-07-14 operator Dhan noise lock (NEGATIVE ratchet — replaces the
    /// PR-C1 `test_rest_stack_spawns_order_update_ws_functional_dormant`
    /// positive pins): the production region must NOT spawn the order-update
    /// WS nor the REST canary. Re-introducing either requires a fresh dated
    /// operator quote in `dhan-rest-only-noise-lock-2026-07-14.md` (and the
    /// scope-lock §A.1 for the order-update spawn) FIRST — this test makes
    /// the code half of that protocol build-failing. Production-region split
    /// at the test-module marker so these needle literals can never trip
    /// themselves.
    #[test]
    fn test_rest_stack_spawns_no_order_update_ws_and_no_canary() {
        let own_src = include_str!("dhan_rest_stack.rs");
        // H1 fix-round 2026-07-14: the canonical production-region helper
        // (blanks the test MODULE only — robust to future mid-file
        // #[cfg(test)] attrs, unlike a naive split_once).
        let prod = tickvault_common::source_scan::production_region(own_src)
            .expect("dhan_rest_stack.rs must keep its test module"); // APPROVED: test
        let prod = prod.as_str();
        for needle in [
            // The retired Q4-i order-update spawn (module stays dormant in
            // core; only the SPAWN is banned here).
            "run_order_update_connection(",
            "tv_order_update_dormant_events_total",
            "NotificationEvent::OrderUpdateAuthenticated",
            // The retired REST canary spawn.
            "rest_canary_boot::run_rest_canary(",
        ] {
            assert!(
                !prod.contains(needle),
                "dhan_rest_stack.rs production region REGAINED `{needle}` — the \
                 2026-07-14 operator Dhan noise lock retired this spawn; a \
                 re-introduction needs a fresh dated rule-file quote FIRST \
                 (dhan-rest-only-noise-lock-2026-07-14.md §3)"
            );
        }
    }

    /// Order-runtime dry-run PR (2026-07-14, SOCKET-FREE shape after the
    /// same-day operator Dhan noise lock): the stack wires the config-gated
    /// DRY-RUN ORDER RUNTIME in Phase 5b — WITHOUT any Dhan WebSocket and
    /// WITHOUT order-update frame capture/drain (both gated behind a fresh
    /// dated operator quote per dhan-rest-only-noise-lock-2026-07-14 §3 /
    /// scope-lock §A.1; the negative ratchet above owns the socket ban).
    /// Positive pins (production-region split so the needle literals can
    /// never satisfy themselves):
    ///   - the runtime spawn exists, EXACTLY once, inside the
    ///     `[order_runtime].enabled` gate (disabled boots stay byte-identical
    ///     to the post-#1532 noise-lock shape);
    ///   - ordering: family-claim (lane/stack exclusion tripwire) < the
    ///     enabled gate < the runtime spawn;
    ///   - the mark receiver is taken from the boot slot (the Groww-bridge
    ///     tap feeds the runtime through main.rs's take-once slot);
    ///   - WAL-free shape: no `wal_spill:` argument, no confirm/drain
    ///     helpers — the stack must stay out of the frame-capture business
    ///     until the live re-arm quote lands.
    #[test]
    fn test_rest_stack_wires_order_runtime() {
        let own_src = include_str!("dhan_rest_stack.rs");
        let prod = tickvault_common::source_scan::production_region(own_src)
            .expect("dhan_rest_stack.rs must keep its test module"); // APPROVED: test
        let prod = prod.as_str();
        // Positive pins.
        for needle in [
            // The config gate the spawn must live behind.
            "if config.order_runtime.enabled {",
            // Stack-local broadcast channel (the runtime's only order-update
            // seam; zero producers until the gated live re-arm).
            "broadcast::channel::<tickvault_common::order_types::OrderUpdate>",
            // The runtime spawn itself.
            "crate::order_runtime::spawn_order_runtime(",
            // The mark bridge: taken ONCE from main.rs's boot slot.
            ".mark_rx_slot",
        ] {
            assert!(
                prod.contains(needle),
                "dhan_rest_stack.rs production region lost `{needle}` — the \
                 socket-free order-runtime Phase 5b wiring regressed"
            );
        }
        // WAL-free shape pins: the stack must NOT touch the frame-capture /
        // drain / confirm surface (gated live re-arm territory).
        for banned in [
            "wal_spill:",
            "ws_frame_spill",
            "confirm_replayed(",
            "drain_replayed_order_updates_to_broadcast(",
        ] {
            assert!(
                !prod.contains(banned),
                "dhan_rest_stack.rs production region REGAINED `{banned}` — \
                 order-update frame capture/drain is gated behind a fresh dated \
                 operator quote (dhan-rest-only-noise-lock-2026-07-14 §3); the \
                 socket-free stack must stay WAL-free"
            );
        }
        // Runtime spawn is EXACTLY once (the spawn-site guard pins the rest
        // of the workspace; this pins the stack itself).
        let spawns = prod
            .matches("crate::order_runtime::spawn_order_runtime(")
            .count();
        assert_eq!(
            spawns, 1,
            "dhan_rest_stack.rs production region must contain EXACTLY ONE \
             spawn_order_runtime call (Phase 5b); found {spawns}"
        );
        // Ordering law: family-claim (lane/stack exclusion tripwire) < the
        // enabled gate < the runtime spawn — a runtime spawned before the
        // claim could double-run against the lane's OMS.
        let claim_call = prod
            .find("if !claim_post_market_task_family_once()")
            .expect("family-claim call present"); // APPROVED: test
        let gate = prod
            .find("if config.order_runtime.enabled {")
            .expect("enabled gate present (asserted above)"); // APPROVED: test
        let runtime_spawn = prod
            .find("crate::order_runtime::spawn_order_runtime(")
            .expect("order-runtime spawn present (asserted above)"); // APPROVED: test
        assert!(
            claim_call < gate && gate < runtime_spawn,
            "Phase 5b ordering law broken (claim@{claim_call} < gate@{gate} < \
             runtime@{runtime_spawn} required) — the runtime must spawn AFTER \
             the lane/stack family claim, inside the enabled gate"
        );
    }

    /// Source-scan pin of the #1499-mirrored spot→chain contract
    /// (2026-07-13 sequencing merge): PR #1499 rewrote the spot-1m fetch
    /// internals (day-window + backfill + the 15:31 post-session sweep)
    /// WITHOUT changing the spawn surface this REST-only stack mirrors
    /// from main.rs's `spawn_post_market_tasks`. The stack therefore
    /// inherits the sweep by construction — this test fails the build if
    /// either side of that contract drifts (a sweep moved OUT of the
    /// shared task, a params-field rename, or the stack dropping the
    /// spot→chain sequencing channel) so the mirror can never diverge
    /// silently again.
    #[test]
    fn test_dhan_rest_stack_mirrors_spot_chain_contract_post_1499() {
        // The shared spot-1m module still owns the sweep + sequencing
        // signal (the #1499 semantics both spawn paths inherit).
        let spot_src = include_str!("spot_1m_rest_boot.rs");
        for needle in [
            "run_post_session_sweep(",
            "pub minute_done_tx",
            "send_replace",
            // TEST-EXEMPT: string-literal ratchet needle (not a fn declaration) — pub-fn-test-guard grep false positive
            "pub fn spawn_supervised_spot_1m_rest",
        ] {
            assert!(
                spot_src.contains(needle),
                "spot_1m_rest_boot.rs lost `{needle}` — the #1499-mirrored \
                 contract drifted; re-check dhan_rest_stack's spawn mirror"
            );
        }

        // This module's PRODUCTION region (split at the test-module marker
        // so these assertion literals can never satisfy themselves) still
        // spawns the same supervised task and builds the sequencing
        // channel the lane path uses.
        let own_src = include_str!("dhan_rest_stack.rs");
        let (prod, _) = own_src
            .split_once("#[cfg(test)]")
            .expect("dhan_rest_stack.rs must keep its test module marker");
        for needle in [
            "spawn_supervised_spot_1m_rest(",
            "spawn_supervised_option_chain_1m(",
            "watch::channel::<Option<u32>>",
            "minute_done_tx",
        ] {
            assert!(
                prod.contains(needle),
                "dhan_rest_stack.rs production region lost `{needle}` — the \
                 REST-only stack no longer mirrors spawn_post_market_tasks"
            );
        }
    }

    /// M3 (2026-07-14 fix round): zero-pager-legs truth table + the boot
    /// warn stays wired in the production region.
    #[test]
    fn test_rest_stack_zero_pager_legs_truth_table_and_warn_wired() {
        assert!(rest_stack_has_zero_pager_legs(false, false));
        assert!(!rest_stack_has_zero_pager_legs(true, false));
        assert!(!rest_stack_has_zero_pager_legs(false, true));
        assert!(!rest_stack_has_zero_pager_legs(true, true));
        let own_src = include_str!("dhan_rest_stack.rs");
        let (prod, _) = own_src
            .split_once("#[cfg(test)]")
            .expect("dhan_rest_stack.rs must keep its test module marker");
        assert!(
            prod.contains("rest_stack_has_zero_pager_legs(config.spot_1m_rest.enabled"),
            "the boot-time zero-legs warn must stay wired — with both legs \
             disabled a total Dhan REST death is otherwise pageless (M3)"
        );
    }

    /// L-fix (2026-07-14 fix round): the supervisor respawns a dying inner
    /// task (clean-exit class) with the configured backoff. 0s backoff so
    /// the test is instant; the panic arm is unwind-build-only per the
    /// honest envelope on `spawn_supervised_stack_task`.
    #[tokio::test]
    async fn test_supervised_stack_task_respawns_on_clean_exit() {
        use std::sync::atomic::AtomicU32;
        let spawns = Arc::new(AtomicU32::new(0));
        let spawns_in_factory = Arc::clone(&spawns);
        let supervisor = spawn_supervised_stack_task("test_task", 0, move || {
            spawns_in_factory.fetch_add(1, Ordering::SeqCst);
            tokio::spawn(async {}) // resolves Ok(()) immediately = clean_exit
        });
        // Give the supervisor a few scheduler turns to cycle.
        for _ in 0..50 {
            tokio::task::yield_now().await;
            if spawns.load(Ordering::SeqCst) >= 3 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        supervisor.abort();
        assert!(
            spawns.load(Ordering::SeqCst) >= 2,
            "the supervisor must respawn a dying inner task — got {} spawn(s)",
            spawns.load(Ordering::SeqCst)
        );
    }

    /// GAP-02 + GAP-06 supervision wiring: BOTH stack background tasks go
    /// through the supervisor in the production region.
    #[test]
    fn test_sweep_and_gauge_poller_are_supervised() {
        let own_src = include_str!("dhan_rest_stack.rs");
        let (prod, _) = own_src
            .split_once("#[cfg(test)]")
            .expect("dhan_rest_stack.rs must keep its test module marker");
        // Whitespace-insensitive: rustfmt may fold/split the call, so the
        // needles are matched on a whitespace-stripped view of the source.
        let flattened: String = prod.split_whitespace().collect();
        for needle in [
            "spawn_supervised_stack_task(\"token_sweep\"",
            "spawn_supervised_stack_task(\"token_health_gauge\"",
            "run_token_sweep_loop(tm)",
        ] {
            let flat_needle: String = needle.split_whitespace().collect();
            assert!(
                flattened.contains(&flat_needle),
                "dhan_rest_stack.rs production region lost `{needle}` — the \
                 GAP-02 sweep / GAP-06 gauge poller must stay SUPERVISED (a \
                 silent death re-opens the audited coverage gap)"
            );
        }
        // Keep the original loop shape below vacuous-proof: assert the
        // supervisor helper itself exists in the production region.
        for needle in ["fn spawn_supervised_stack_task"] {
            assert!(
                prod.contains(needle),
                "dhan_rest_stack.rs production region lost `{needle}` — the \
                 GAP-02 sweep / GAP-06 gauge poller must stay SUPERVISED (a \
                 silent death re-opens the audited coverage gap)"
            );
        }
    }
}
