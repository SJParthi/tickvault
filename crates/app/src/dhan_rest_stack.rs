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
//! 3. the token renewal loop + the mid-session profile watchdog;
//! 4. the REST canary (`rest_canary_boot`), the per-minute `spot_1m_rest`
//!    scheduler and the per-minute `option_chain_1m` scheduler /
//!    entitlement probe — mirroring `main.rs::spawn_post_market_tasks`
//!    exactly (incl. the spot→chain sequencing watch channel and the
//!    existing `[spot_1m_rest]` / `[option_chain_1m]` config gates);
//! 5. **(PR-C1, 2026-07-13 — operator ruling Q4-i "agreed dude")** the Dhan
//!    ORDER-UPDATE WS, rewired from the retired lane into this stack —
//!    FUNCTIONAL-DORMANT: connected + authenticated (MsgCode-42 JSON login),
//!    receiving order events for any order placed via the retained REST
//!    surface; it carries NO market data and is NOT a live price feed. Its
//!    reconnect / post-close-sleep machinery (WS-GAP-04/10) is
//!    self-contained; it runs as its own tokio task and can never block or
//!    kill this stack's bring-up.
//!    **Dormancy honesty (2026-07-13 hostile-review M1/M2):** while
//!    functionally dormant, incoming order-update frames are parsed,
//!    COUNTED (`tv_order_update_dormant_events_total` — a stack-held DRAIN
//!    task consumes the broadcast; the positive "order activity observed
//!    while dormant" signal) and DISCARDED — no WAL capture, no OMS
//!    consumer; durable order-event capture returns with live trading (the
//!    OMS wiring). Boot-staged order-update WAL segments remain UNDRAINED
//!    on dhan-off boots (pre-existing Phase A residual — C2 settles the
//!    replay topology).
//!    **Paging honesty (2026-07-13 hostile-review L2):** there is NO
//!    CloudWatch dead-socket alarm for this stack's order-update WS —
//!    `tv_order_update_ws_active` is written ONLY by the (dead) lane spawn
//!    sites, so the `tv-<env>-order-update-ws-inactive` alarm is
//!    missing-data-silent both ways on a dhan-off boot; the WS-GAP-10
//!    in-loop outage page (notifier wired) is the SOLE pager. Re-homing
//!    the gauge into the connection loop is a C2 target.
//!
//! **Deliberately NOT spawned (stay lane-only; deletion is a later phase):**
//! the WS pool, universe build / CSV download, prev-day OHLCV, the SLO
//! publisher, the 15:31 cross-verify, the EOD digest, and the
//! orphan-position watchdog. (The order-update WS moved OFF this list in
//! PR-C1 per Q4-i — see item 5 above.)
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
/// Boot-staged order-update WAL burst warning threshold (order-runtime
/// dry-run PR, 2026-07-14): the stack broadcast holds 256 events — a
/// replay burst beyond ~200 can lag the runtime's receiver mid-drain
/// (counted there); this fires ONE coalesced warn + counter so the
/// envelope crossing is visible.
const ORDER_UPDATE_WAL_REPLAY_BURST_WARN: usize = 200;

/// Process-global once-guard: the REST-only stack must never be brought up
/// twice (N stacks = N heartbeats + N renewal loops + N canary/spot/chain
/// families). First caller wins; later calls log INFO and return `None`.
static DHAN_REST_STACK_SPAWNED: AtomicBool = AtomicBool::new(false);

/// Process-global once-guard for the Dhan-REST SCHEDULED TASK FAMILY —
/// SHARED between the lane path (`main.rs::spawn_post_market_tasks`: REST
/// canary + spot_1m_rest + option_chain_1m + the lane-only orphan watchdog
/// / EOD digest / 1m cross-verify) and this REST-only stack's Phase 5
/// (canary + spot + chain).
///
/// INVARIANT (2026-07-13 hostile-review MEDIUM): the family is spawned AT
/// MOST ONCE per process, WHICHEVER path claims first — so a future
/// relaxation of the runtime cold-start refusal (or any new path into
/// `run_dhan_lane_cold_start` → `spawn_post_market_tasks`) can never
/// double-spawn the canary/spot/chain schedulers alongside this stack's
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
    /// Trading calendar for the canary/spot/chain trading-day gates.
    pub calendar: Arc<TradingCalendar>,
    /// Runtime feed-state — receives `set_live_token_manager` so the token
    /// gauges read this stack's manager.
    pub feed_runtime: Arc<FeedRuntimeState>,
    /// The process-shared WS frame WAL (order-runtime dry-run PR,
    /// 2026-07-14): with `[order_runtime].enabled` the order-update WS
    /// restores durable frame capture (`wal_spill = Some(..)`); disabled
    /// keeps the Phase-A dormant `None`. `Option` mirrors main.rs's
    /// fail-closed init (always `Some` on a real boot — init exits(1)
    /// on failure).
    pub ws_frame_spill: Option<Arc<tickvault_storage::ws_frame_spill::WsFrameSpill>>,
    /// Boot-staged order-update WAL frames (main.rs STAGE-C replay) —
    /// drained into the stack broadcast BEFORE the WS spawns (FIFO law F4)
    /// when the runtime is enabled. Disabled: left staged in `replaying/`
    /// (byte-identical Phase-A residual).
    pub ws_wal_replay_order_update: Vec<Vec<u8>>,
    /// Count of boot-staged LIVE-FEED frames (dhan-off = never re-injected).
    /// Gates the conditional `confirm_replayed`: a whole-dir confirm while
    /// stale live-feed frames sit staged would archive them un-reinjected
    /// (silent tick loss, F6) — so confirm fires ONLY when this is 0.
    pub livefeed_frames_replayed: usize,
    /// The WAL directory (`ws_wal_dir()`) — the conditional confirm target.
    pub wal_dir: std::path::PathBuf,
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
async fn run_dhan_rest_stack(mut params: DhanRestStackParams) {
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
    // Shared once-guard (2026-07-13 hostile-review MEDIUM): claim the
    // Dhan-REST task family BEFORE spawning it, the SAME guard the lane's
    // spawn_post_market_tasks claims — so a future relaxation of the
    // runtime cold-start refusal can never run canary/spot/chain TWICE in
    // one process. Unreachable today (mutual exclusion by construction);
    // if it ever fires, the invariant is broken — stay down loudly.
    if !claim_post_market_task_family_once() {
        error!(
            "Dhan REST-only stack: the Dhan-REST scheduled task family is ALREADY \
             claimed this process (the lane's spawn_post_market_tasks ran first) — \
             refusing to double-spawn canary/spot_1m_rest/option_chain_1m; the \
             lane/stack mutual-exclusion invariant is broken, investigate (the \
             stack stays DOWN: gauge remains 0)"
        );
        return;
    }
    let token_handle = token_manager.token_handle();
    let config = &params.config;

    // -----------------------------------------------------------------------
    // Phase 5a (PR-C1, 2026-07-13 — operator ruling Q4-i "agreed dude"): the
    // Dhan ORDER-UPDATE WS, rewired from the retired lane into this stack —
    // FUNCTIONAL-DORMANT (module docs item 5). Placed AFTER the family claim
    // above so a broken lane/stack mutual-exclusion invariant can never
    // produce a SECOND order-update WS (the lane spawns its own). After the
    // Phase C2 deletion of the legacy fast-arm + lane spawn sites, THIS is
    // the SOLE `run_order_update_connection` call site.
    //
    // Parameter honesty:
    //   - `order_sender`: a STACK-LOCAL broadcast channel (capacity mirrors
    //     the legacy 256). Consumer depends on `[order_runtime].enabled`:
    //       * DISABLED (Phase-A dormant shape, byte-identical): a stack-held
    //         DRAIN task consumes every event — counted
    //         (`tv_order_update_dormant_events_total`, static label) and
    //         DISCARDED: no WAL capture, no OMS consumer.
    //       * ENABLED (order-runtime dry-run PR, 2026-07-14): the dry-run
    //         ORDER RUNTIME subscribes BEFORE the boot-staged WAL frames are
    //         drained into the broadcast (ordering law F5 — replayed fills
    //         must reach the runtime), replacing the discard drain. The
    //         runtime holds its receiver for the process lifetime, so the
    //         connection's dropped-receiver error arm
    //         (`tv_order_update_broadcast_drops_total`) stays dormant BY
    //         CONSTRUCTION exactly as the M1 drain guaranteed.
    //   - `wal_spill`: `None` when the runtime is disabled (Phase-A dormant
    //     shape — Verified 2026-07-13); `Some(ws_frame_spill)` when enabled —
    //     durable order-event frame CAPTURE is restored (the upstream
    //     append-before-broadcast code always existed; this re-arms it), and
    //     the boot-staged order-update WAL segments are drained + CONDITIONALLY
    //     confirmed below (confirm iff parse-clean AND zero stale live-feed
    //     frames staged — F6; else defer with ONE coalesced WS-REINJECT-01
    //     `warn!` + counter, never a page).
    //   - `dhan_feed_flag = None`: always-on within this stack — the stack
    //     exists only when the raw boot TOML retires the lane, and the
    //     /api/feeds handler 409-refuses a runtime Dhan enable.
    // -----------------------------------------------------------------------
    {
        let (order_update_sender, order_update_receiver) =
            tokio::sync::broadcast::channel::<tickvault_common::order_types::OrderUpdate>(256);
        // OrderUpdateAuthenticated Telegram once Dhan accepts the token
        // (the fast-arm listener mirror; this stack is process-lifetime, so
        // a plain spawn is correct — no lane teardown can orphan it).
        // Hoisted ABOVE the consumer branch: the order runtime shares the
        // auth signal (its one-shot reconcile arm fires on first WS auth).
        let auth_signal = Arc::new(tokio::sync::Notify::new());
        let auth_latch = Arc::new(AtomicBool::new(false));
        // wal_spill for the WS spawn below: capture restored ONLY with the
        // runtime enabled (disabled keeps the Phase-A dormant `None`).
        let ou_wal_spill = if config.order_runtime.enabled {
            params.ws_frame_spill.clone()
        } else {
            None
        };
        if config.order_runtime.enabled {
            // The channel-construction receiver is replaced by an explicit
            // subscribe so the ordering law is visible + ratchetable:
            // subscribe() BEFORE spawn_order_runtime BEFORE the WAL drain
            // BEFORE the WS spawn.
            drop(order_update_receiver);
            let runtime_rx = order_update_sender.subscribe();
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
                    // runtime — order updates still flow; the leaked sender
                    // keeps the dummy channel open so the mark arm just idles.
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
            let _order_runtime_supervisor = crate::order_runtime::spawn_order_runtime(
                crate::order_runtime::OrderRuntimeParams {
                    config: Arc::clone(&params.config),
                    notifier: params.notifier.clone(),
                    calendar: Arc::clone(&params.calendar),
                    order_update_sender: order_update_sender.clone(),
                    first_order_update_rx: runtime_rx,
                    mark_rx,
                    marks_wanted: Arc::clone(&params.marks_wanted),
                    token_handle: Arc::clone(&token_handle),
                    client_id: client_id.clone(),
                    auth_notify: Arc::clone(&auth_signal),
                },
            );
            // F4 FIFO: drain the boot-staged order-update WAL frames into the
            // broadcast BEFORE the WS spawns, so replayed fills precede fresh
            // live events on the runtime's receiver.
            let frames = std::mem::take(&mut params.ws_wal_replay_order_update);
            let staged_count = frames.len();
            if staged_count > ORDER_UPDATE_WAL_REPLAY_BURST_WARN {
                // Broadcast(256) envelope honesty: a burst larger than the
                // channel would lag the runtime receiver mid-drain (counted
                // there); loud here so the operator knows the day started
                // from an unusually deep residual.
                warn!(
                    staged_frames = staged_count,
                    "order runtime: boot-staged order-update WAL burst exceeds the \
                     broadcast envelope — replay proceeds; receiver lag is counted"
                );
                metrics::counter!("tv_wal_replay_burst_total").increment(1);
            }
            let (replay_parsed, replay_broadcast, replay_parse_errors) =
                crate::boot_helpers::drain_replayed_order_updates_to_broadcast(
                    frames,
                    &order_update_sender,
                );
            if staged_count > 0 {
                info!(
                    parsed = replay_parsed,
                    broadcast = replay_broadcast,
                    parse_errors = replay_parse_errors,
                    "order runtime: boot-staged order-update WAL frames drained into the \
                     stack broadcast (before the WS spawn — FIFO preserved)"
                );
            }
            // F6/F7 conditional confirm: whole-dir `confirm_replayed` archives
            // EVERYTHING staged in `replaying/` — safe ONLY when this drain
            // was parse-clean AND no stale live-feed frames sit staged
            // (dhan-off boots never re-inject live-feed frames).
            match crate::order_runtime::confirm_decision(
                replay_parse_errors,
                params.livefeed_frames_replayed,
            ) {
                crate::order_runtime::ConfirmVerdict::Confirm => {
                    tickvault_storage::ws_frame_spill::confirm_replayed(&params.wal_dir);
                    info!(
                        confirmed_frames = staged_count,
                        "order runtime: WAL order-update replay confirmed — staged segments \
                         archived (replaying/ clean)"
                    );
                }
                crate::order_runtime::ConfirmVerdict::Defer { live_feed_frames } => {
                    // `warn!` DELIBERATELY (not `error!`): WS-REINJECT-01 has a
                    // CloudWatch ERROR-level log-filter alarm, and a stale
                    // live-feed residual on a dhan-off boot is EXPECTED (the
                    // Phase-A class) — a per-boot page would be pager noise.
                    // The frames stay staged (re-replayed next boot, never
                    // lost); the runbook's one-time archive procedure clears
                    // the stale live-feed segments.
                    warn!(
                        code = ErrorCode::WsReinject01Aborted.code_str(),
                        reason = "confirm_deferred_stale_livefeed",
                        live_feed_frames,
                        parse_errors = replay_parse_errors,
                        "order runtime: WAL confirm DEFERRED — staged segments left in \
                         replaying/ (zero loss; cleared by the operator archive procedure \
                         or the next dhan-on boot's live-feed re-injection)"
                    );
                    metrics::counter!("tv_wal_confirm_deferred_total").increment(1);
                }
            }
            info!(
                "order runtime: dry-run order runtime spawned (replaces the dormant \
                 discard drain; order-update WAL capture restored)"
            );
        } else {
            // DISABLED: byte-identical Phase-A dormant shape.
            // Dormant DRAIN task (2026-07-13 hostile-review M1): the receiver
            // is HELD and drained — never dropped. Events are parsed upstream,
            // counted here, and DISCARDED (module docs item 5 dormancy
            // honesty); `Lagged` is impossible in practice at this event rate
            // but is skipped defensively; `Closed` (sender gone = connection
            // task dead) ends the drain — the WS-GAP-10 unreachable-exit error
            // at the spawn below is the loud signal for that case.
            let mut order_update_receiver = order_update_receiver;
            tokio::spawn(async move {
                loop {
                    match order_update_receiver.recv().await {
                        Ok(_event) => {
                            metrics::counter!("tv_order_update_dormant_events_total").increment(1);
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }
            });
        }
        let ou_url = config.dhan.order_update_websocket_url.clone();
        let ou_client_id = client_id.clone();
        let ou_token = Arc::clone(&token_handle);
        let ou_calendar = Arc::clone(&params.calendar);
        {
            let listener_signal = Arc::clone(&auth_signal);
            let listener_notifier = params.notifier.clone();
            tokio::spawn(async move {
                listener_signal.notified().await;
                listener_notifier.notify(NotificationEvent::OrderUpdateAuthenticated);
            });
        }
        let run_signal = Some(Arc::clone(&auth_signal));
        let run_latch = Some(Arc::clone(&auth_latch));
        let ou_reconnect_notifier = Some(params.notifier.clone());
        // Order-update lifecycle events → ws_event_audit (its own consumer,
        // the SAME relocated helper the main.rs spawn sites use).
        let ou_ws_audit_tx = Some(crate::ws_audit_consumer::spawn_ws_event_audit_consumer(
            config.questdb.clone(),
        ));
        let _order_update_handle = tokio::spawn(async move {
            tickvault_core::websocket::order_update_connection::run_order_update_connection(
                ou_url,
                ou_client_id,
                ou_token,
                order_update_sender,
                ou_calendar,
                // wal_spill: Some(..) with the runtime enabled (durable frame
                // capture restored); None keeps the Phase-A dormant shape.
                ou_wal_spill,
                run_signal,
                run_latch,
                ou_reconnect_notifier,
                ou_ws_audit_tx,
                None,
            )
            .await;
            // Defensive only (the lane-site mirror): run_order_update_connection
            // is an infinite never-give-up loop (WS-GAP-04) and structurally
            // cannot return. If this line ever executes, a future refactor
            // broke the loop contract — surface it loudly, never silently.
            error!(
                code = ErrorCode::WsGap10OrderUpdateOutage.code_str(),
                reason = "task_exited_unreachable",
                "order update WebSocket task exited — unreachable by design; investigate immediately"
            );
        });
        if config.order_runtime.enabled {
            info!(
                "Dhan REST-only stack: order-update WS spawned (Q4-i — connects at market \
                 open via the off-hours gate; incoming order events feed the DRY-RUN order \
                 runtime and are durably WAL-captured)"
            );
        } else {
            info!(
                "Dhan REST-only stack: order-update WS spawned (functional-dormant, Q4-i — \
                 connects at market open via the off-hours gate; incoming order events are \
                 drained + counted, NOT persisted, until live trading returns)"
            );
        }
    }

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
                diagnostics_enabled: config.spot_1m_rest.diagnostics,
                diagnostics_second_probe_secs_of_day_ist: config
                    .spot_1m_rest
                    .diagnostics_second_probe_secs_of_day_ist,
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
         canary + spot_1m_rest + option_chain_1m arms + the functional-dormant \
         order-update WS (Q4-i, PR-C1) spawned WITHOUT the market-data WS lane \
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
    /// FIX 4 invariant: canary/spot/chain can never spawn twice per
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

    /// Order-runtime dry-run PR (2026-07-14) — REPLACES
    /// `test_rest_stack_spawns_order_update_ws_functional_dormant`: the stack
    /// now wires the dry-run ORDER RUNTIME in its Phase 5a (config-gated;
    /// the disabled branch keeps the PR-C1 dormant shape byte-identical).
    /// Production-region source pins (the house production-region split so
    /// these assertion literals can never satisfy themselves) + the ordering
    /// laws:
    ///   family-claim < subscribe < spawn_order_runtime < WAL drain
    ///   < confirm arm < run_order_update_connection.
    #[test]
    fn test_rest_stack_wires_order_runtime() {
        let own_src = include_str!("dhan_rest_stack.rs");
        let (prod, _) = own_src
            .split_once("#[cfg(test)]")
            .expect("dhan_rest_stack.rs must keep its test module marker");
        for needle in [
            // The rewired WS spawn itself (Q4-i — kept).
            "run_order_update_connection(",
            // Stack-local broadcast channel.
            "broadcast::channel::<tickvault_common::order_types::OrderUpdate>",
            // ENABLED branch: the runtime's early subscribe (ordering law F5).
            "order_update_sender.subscribe()",
            // ENABLED branch: the runtime spawn replacing the discard drain.
            "crate::order_runtime::spawn_order_runtime(",
            // ENABLED branch: boot-staged WAL frames drained BEFORE the WS
            // spawns (F4 FIFO).
            "crate::boot_helpers::drain_replayed_order_updates_to_broadcast(",
            // ENABLED branch: the CONDITIONAL confirm (F6 — parse-clean AND
            // zero stale live-feed frames), never an unconditional confirm.
            "crate::order_runtime::confirm_decision(",
            "tickvault_storage::ws_frame_spill::confirm_replayed(",
            // Defer arm loudness: the coalesced counter (warn-level by design).
            "metrics::counter!(\"tv_wal_confirm_deferred_total\")",
            // ENABLED branch: durable order-update frame capture restored.
            "params.ws_frame_spill.clone()",
            // DISABLED branch: the PR-C1 dormant shape survives byte-identical
            // — receiver HELD + drained (hostile-review M1) and counted.
            "order_update_receiver.recv()",
            "metrics::counter!(\"tv_order_update_dormant_events_total\")",
            // Lifecycle rows keep flowing to ws_event_audit.
            "ws_audit_consumer::spawn_ws_event_audit_consumer(",
            // Operator visibility: the authenticated Telegram listener.
            "NotificationEvent::OrderUpdateAuthenticated",
        ] {
            assert!(
                prod.contains(needle),
                "dhan_rest_stack.rs production region lost `{needle}` — the \
                 order-runtime Phase 5a wiring (or the PR-C1 dormant fallback) \
                 regressed"
            );
        }
        // Ordering law: family-claim (lane/stack exclusion tripwire) <
        // subscribe (F5) < spawn_order_runtime < WAL drain (F4) < confirm arm
        // (F6) < the WS spawn.
        let claim_call = prod
            .find("if !claim_post_market_task_family_once()")
            .expect("family-claim call present"); // APPROVED: test
        let subscribe = prod
            .find("order_update_sender.subscribe()")
            .expect("early subscribe present (asserted above)"); // APPROVED: test
        let runtime_spawn = prod
            .find("crate::order_runtime::spawn_order_runtime(")
            .expect("order-runtime spawn present (asserted above)"); // APPROVED: test
        let wal_drain = prod
            .find("crate::boot_helpers::drain_replayed_order_updates_to_broadcast(")
            .expect("WAL drain present (asserted above)"); // APPROVED: test
        let confirm_arm = prod
            .find("crate::order_runtime::confirm_decision(")
            .expect("confirm decision present (asserted above)"); // APPROVED: test
        let ou_spawn = prod
            .find("order_update_connection::run_order_update_connection(")
            .expect("order-update spawn present (asserted above)"); // APPROVED: test
        assert!(
            claim_call < subscribe
                && subscribe < runtime_spawn
                && runtime_spawn < wal_drain
                && wal_drain < confirm_arm
                && confirm_arm < ou_spawn,
            "Phase 5a ordering law broken (claim@{claim_call} < subscribe@{subscribe} \
             < runtime@{runtime_spawn} < drain@{wal_drain} < confirm@{confirm_arm} \
             < ws@{ou_spawn} required) — a runtime subscribed AFTER the drain misses \
             replayed fills (F5); a drain AFTER the WS spawn breaks FIFO (F4); an \
             unconditionally-early confirm archives un-reinjected frames (F6)"
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
}
