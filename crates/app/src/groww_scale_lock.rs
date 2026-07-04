//! Groww scale-FLEET dual-instance lock gate (Session-B fix #1, operator go
//! 2026-07-04 — "fixing and working?" go-ahead on the Session-B fix plan).
//!
//! The multi-connection Groww scale fleet (`make scale-test` / `scale-smoke`
//! / any boot with `feeds.groww.scale.enabled = true`) previously had NO
//! dual-instance protection: the RESILIENCE-01 SSM lock is acquired only
//! inside `start_dhan_lane`, and scale-test boots run `dhan_enabled=false`,
//! so NOTHING stopped two tickvault instances (Mac + AWS, or two Macs) from
//! scaling the SAME Groww account simultaneously. The failure masqueraded as
//! provider throttle (repeating close/auth rejects) with triage pointing at
//! Groww, never at a peer instance.
//!
//! This module gates the FLEET SPAWN (smallest safe scope — the single-conn
//! Groww path is untouched) behind a NAMED SSM dual-instance lock, reusing
//! the `tickvault_core::instance_lock` machinery:
//!
//!   * Path: `/tickvault/<env>/instance-lock-groww-scale` — deliberately
//!     OUTSIDE the banned `/tickvault/<env>/groww/*` namespace (token-minter
//!     lock 2026-07-02: TickVault never writes any `groww/*` parameter).
//!   * AlreadyHeld → fleet SKIPPED (single-conn fallback, same semantics as
//!     a failed scale PREFLIGHT) + `error!(code = "GROWW-SCALE-05")` Critical
//!     via the log-sink chain (stdout / app.log / errors.log / errors.jsonl →
//!     CloudWatch-log-derived alerting). HONEST delivery boundary: no typed
//!     Telegram `NotificationEvent` exists for this code yet — the deferred
//!     notifier slot is unfilled at this boot stage, so Telegram delivery
//!     depends on the generic ERROR→CloudWatch routing until a typed event
//!     lands (documented follow-up). The rest of the app keeps running.
//!   * SSM transport error → bounded 3-attempt retry (2s/4s exponential
//!     backoff — the same budget as the Dhan Step 6a-prime lock) then
//!     FAIL-CLOSED (fleet skipped + same Critical page). Never silently
//!     proceeds to a fleet it cannot prove is unique.
//!   * Acquired → the named-lock heartbeat renews every 30s. On GRACEFUL
//!     shutdown (SIGINT/SIGTERM path) `run_process_runloop` calls
//!     [`GrowwScaleFleetLockGuard::release_on_shutdown`], which triggers the
//!     heartbeat's clean SSM release so a same-host restart can re-acquire
//!     IMMEDIATELY (2026-07-04 adversarial-review HIGH fix — previously the
//!     slot stayed fresh for up to 90s and a quick restart saw its OWN dead
//!     slot as a peer). On a hard crash (kill -9 / panic-abort) the 90s TTL
//!     remains the self-heal backstop: a restart within that window sees
//!     AlreadyHeld and falls back to single-conn — wait ~90s and restart.
//!
//! Runbook: `.claude/rules/project/groww-scale-error-codes.md` §4b.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use tickvault_common::error_code::ErrorCode;
use tickvault_core::auth::secret_manager::{create_ssm_client_public, resolve_environment};
use tickvault_core::instance_lock::{
    AcquireOutcome, GROWW_SCALE_FLEET_LOCK_NAME, INSTANCE_LOCK_TTL_SECS, compute_named_lock_path,
    generate_host_id, spawn_named_lock_heartbeat, try_acquire_named_lock,
};
use tokio::sync::Notify;
use tracing::{error, info, warn};

/// Bounded SSM transport-retry budget — mirrors the Dhan Step 6a-prime
/// dual-instance lock acquire (3 attempts, exponential backoff).
pub const GROWW_SCALE_LOCK_ACQUIRE_MAX_ATTEMPTS: u32 = 3;

/// Bounded wait for the heartbeat's clean SSM release at graceful shutdown
/// (one DeleteParameter round-trip). On timeout shutdown proceeds anyway —
/// the 90s TTL remains the self-heal backstop; the bound only exists so a
/// black-holed SSM endpoint can never hang process exit.
pub const GROWW_SCALE_LOCK_RELEASE_TIMEOUT_SECS: u64 = 5;

/// Exponential backoff before retry attempt N+1 after failed attempt
/// `attempt` (1-based): 2s after attempt 1, 4s after attempt 2 — the same
/// `2^attempt` ladder the Dhan Step 6a-prime lock uses.
#[must_use]
pub const fn groww_scale_lock_backoff_secs(attempt: u32) -> u64 {
    2u64.saturating_pow(attempt)
}

/// Pure gate decision over an acquire outcome: ONLY a genuine `Acquired`
/// allows the multi-connection fleet to spawn. `AlreadyHeld` — a fresh peer
/// instance is already scaling this Groww account — refuses fail-closed.
#[must_use]
pub const fn fleet_gate_allows(outcome: &AcquireOutcome) -> bool {
    matches!(outcome, AcquireOutcome::Acquired)
}

/// Holds the fleet lock alive for the life of `main`.
///
/// Dropping the guard aborts nothing by itself — the heartbeat task keeps
/// renewing until the process exits. Graceful shutdown MUST route through
/// [`Self::release_on_shutdown`] (wired into `run_process_runloop`,
/// 2026-07-04 adversarial-review HIGH fix) so the SSM slot is freed for an
/// immediate same-host restart; a hard crash falls back to the 90s TTL.
pub struct GrowwScaleFleetLockGuard {
    /// The named-lock heartbeat task (30s renewals).
    pub heartbeat: tokio::task::JoinHandle<()>,
    /// Notify that triggers the heartbeat's clean release path.
    pub shutdown: Arc<Notify>,
    /// Flips to `false` if the lock is lost to a foreign takeover mid-run.
    /// HONEST boundary: no runtime code reads this flag yet — it exists for
    /// the PR-2 ladder-side wiring (freeze/teardown on lock loss); today the
    /// heartbeat's GROWW-SCALE-05 page is the only mid-run loss signal.
    pub held: Arc<AtomicBool>,
}

impl GrowwScaleFleetLockGuard {
    /// Releases the fleet lock at graceful shutdown.
    ///
    /// Signals the heartbeat's clean-release arm (`notify_one` stores a
    /// permit — Rule 16 lost-wake safe even if the heartbeat is currently
    /// mid-renewal instead of parked on `notified()`), then awaits the
    /// heartbeat task for up to [`GROWW_SCALE_LOCK_RELEASE_TIMEOUT_SECS`]
    /// so the SSM DeleteParameter round-trip lands BEFORE the process
    /// exits. Without this, every clean shutdown left the slot fresh for
    /// up to 90s and a same-host restart (the dominant Mac scale-test
    /// iteration workflow) saw its OWN dead slot as a fresh peer: fleet
    /// refused for the whole session + a false-positive GROWW-SCALE-05
    /// page naming the operator's own previous pid.
    pub async fn release_on_shutdown(self) {
        self.shutdown.notify_one();
        match tokio::time::timeout(
            std::time::Duration::from_secs(GROWW_SCALE_LOCK_RELEASE_TIMEOUT_SECS),
            self.heartbeat,
        )
        .await
        {
            Ok(_) => info!(
                "groww scale-fleet lock released on shutdown — the SSM slot is \
                 free for an immediate same-host restart"
            ),
            Err(_) => warn!(
                timeout_secs = GROWW_SCALE_LOCK_RELEASE_TIMEOUT_SECS,
                "groww scale-fleet lock release did not settle within the bound — \
                 the 90s TTL self-heals the slot; a same-host restart inside that \
                 window sees AlreadyHeld and falls back to the single-conn path"
            ),
        }
    }
}

/// Outcome of the fleet-lock gate.
pub enum GrowwScaleFleetLockOutcome {
    /// Lock held — the fleet may spawn; keep the guard alive for the life
    /// of the process.
    Acquired(GrowwScaleFleetLockGuard),
    /// Fleet REFUSED (peer holds the lock, or SSM was unavailable after the
    /// bounded retry budget, or the environment could not be resolved).
    /// The GROWW-SCALE-05 Critical page has already been emitted; the boot
    /// falls back to the single-connection Groww path.
    SkipFleet,
}

/// Acquires the Groww scale-fleet dual-instance SSM lock, fail-closed.
///
/// Called from `main.rs` as the third stage of the `groww_scale_enabled`
/// gate (cfg enabled → preflight pass → THIS lock), so a default boot
/// (`scale.enabled = false`) performs ZERO SSM calls.
// TEST-EXEMPT: end-to-end behaviour requires a real AWS SSM endpoint (same
// class as the Dhan Step 6a-prime acquire in main.rs). The pure decision
// primitives (backoff ladder, max-attempts pin, gate decision) are
// unit-tested below and a smoke test pins the symbol.
pub async fn acquire_groww_scale_fleet_lock() -> GrowwScaleFleetLockOutcome {
    let code = ErrorCode::GrowwScale05DualFleetDetected;
    let env = match resolve_environment() {
        Ok(env) => env,
        Err(err) => {
            // Cannot even compute the lock path → cannot prove there is no
            // peer → fail-closed (skip the fleet, keep the app running).
            error!(
                code = code.code_str(),
                severity = code.severity().as_str(),
                error = %err,
                "GROWW-SCALE-05: cannot resolve environment for the Groww \
                 scale-fleet dual-instance lock — fleet spawn REFUSED \
                 fail-closed; falling back to the single-connection Groww path"
            );
            return GrowwScaleFleetLockOutcome::SkipFleet;
        }
    };
    let ssm = Arc::new(create_ssm_client_public().await);
    let host_id = generate_host_id(
        std::process::id(),
        // Boot-once 64-bit value derived from nanos-since-UNIX-EPOCH —
        // same cross-host-uniqueness rationale as the Dhan lock host_id
        // (rand is not a workspace dep; uniqueness only needs to hold
        // within the 90s TTL window).
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0),
        None,
    );
    let lock_key = compute_named_lock_path(&env, GROWW_SCALE_FLEET_LOCK_NAME);

    // Transport (SSM) errors retry on the bounded budget; AlreadyHeld is a
    // DEFINITIVE answer and is never retried.
    let acquire_result = {
        let mut attempt: u32 = 0;
        loop {
            attempt = attempt.saturating_add(1);
            match try_acquire_named_lock(&ssm, &env, GROWW_SCALE_FLEET_LOCK_NAME, &host_id).await {
                Ok(outcome) => break Ok(outcome),
                Err(err) if attempt >= GROWW_SCALE_LOCK_ACQUIRE_MAX_ATTEMPTS => break Err(err),
                Err(err) => {
                    warn!(
                        attempt,
                        max_attempts = GROWW_SCALE_LOCK_ACQUIRE_MAX_ATTEMPTS,
                        error = %err,
                        "groww scale-fleet lock: SSM acquire failed (transient) — retrying"
                    );
                    tokio::time::sleep(std::time::Duration::from_secs(
                        groww_scale_lock_backoff_secs(attempt),
                    ))
                    .await;
                }
            }
        }
    };

    match acquire_result {
        Ok(outcome) if fleet_gate_allows(&outcome) => {
            info!(
                env = %env,
                host_id = %host_id,
                lock_key = %lock_key,
                ttl_secs = INSTANCE_LOCK_TTL_SECS,
                "groww scale-fleet dual-instance lock ACQUIRED — fleet spawn allowed \
                 (Session-B fix #1, operator go 2026-07-04)"
            );
            let shutdown = Arc::new(Notify::new());
            let held = Arc::new(AtomicBool::new(true));
            let heartbeat = spawn_named_lock_heartbeat(
                ssm,
                env,
                GROWW_SCALE_FLEET_LOCK_NAME,
                host_id,
                Arc::clone(&shutdown),
                Arc::clone(&held),
                // A mid-run foreign takeover pages with the same code the
                // boot-time refusal uses — the collision is loud, never
                // silent.
                code,
            );
            GrowwScaleFleetLockOutcome::Acquired(GrowwScaleFleetLockGuard {
                heartbeat,
                shutdown,
                held,
            })
        }
        Ok(outcome) => {
            // fleet_gate_allows refused — the only refusing variant today is
            // AlreadyHeld; extract the peer's host_id for the operator page.
            let peer = match outcome {
                AcquireOutcome::AlreadyHeld { holder } => holder,
                // Unreachable (Acquired passes the gate above) — kept
                // exhaustive without a wildcard so a future AcquireOutcome
                // variant forces a deliberate decision here.
                AcquireOutcome::Acquired => String::new(),
            };
            // Log-injection defense (2026-07-04 adversarial review LOW): the
            // corrupt-JSON acquire arm embeds the RAW SSM value in `holder`,
            // and this Critical line flows through the whole log-sink chain.
            // sanitize_ilp_symbol strips newlines/control chars and caps at
            // 256 bytes, so a poisoned parameter cannot forge log lines or
            // flood the page.
            let peer = tickvault_common::sanitize::sanitize_ilp_symbol(&peer);
            error!(
                code = code.code_str(),
                severity = code.severity().as_str(),
                env = %env,
                lock_key = %lock_key,
                peer = %peer,
                "GROWW-SCALE-05: another tickvault instance already holds the Groww \
                 scale-fleet lock — fleet spawn REFUSED (a second fleet against the \
                 same Groww account masquerades as provider throttle); falling back \
                 to the single-connection Groww path"
            );
            GrowwScaleFleetLockOutcome::SkipFleet
        }
        Err(err) => {
            error!(
                code = code.code_str(),
                severity = code.severity().as_str(),
                error = %err,
                env = %env,
                lock_key = %lock_key,
                max_attempts = GROWW_SCALE_LOCK_ACQUIRE_MAX_ATTEMPTS,
                "GROWW-SCALE-05: SSM unavailable after the bounded retry budget — \
                 cannot prove there is no peer fleet; fleet spawn REFUSED \
                 fail-closed; falling back to the single-connection Groww path"
            );
            GrowwScaleFleetLockOutcome::SkipFleet
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_groww_scale_lock_backoff_secs_mirrors_dhan_policy() {
        // 2s after attempt 1, 4s after attempt 2 — the Dhan Step 6a-prime
        // ladder (2^attempt).
        assert_eq!(groww_scale_lock_backoff_secs(1), 2);
        assert_eq!(groww_scale_lock_backoff_secs(2), 4);
        // Saturates instead of overflowing on garbage input.
        assert_eq!(groww_scale_lock_backoff_secs(u32::MAX), u64::MAX);
    }

    #[test]
    fn test_groww_scale_lock_max_attempts_pinned() {
        // Mirror of the Dhan INSTANCE_LOCK_ACQUIRE_MAX_ATTEMPTS budget.
        assert_eq!(GROWW_SCALE_LOCK_ACQUIRE_MAX_ATTEMPTS, 3);
    }

    #[test]
    fn test_fleet_gate_allows_only_acquired() {
        assert!(fleet_gate_allows(&AcquireOutcome::Acquired));
        assert!(!fleet_gate_allows(&AcquireOutcome::AlreadyHeld {
            holder: "i-peer:1:0000000000000001".into()
        }));
        assert!(!fleet_gate_allows(&AcquireOutcome::AlreadyHeld {
            holder: String::new()
        }));
    }

    #[test]
    fn test_acquire_groww_scale_fleet_lock_smoke() {
        // End-to-end is TEST-EXEMPT (real SSM endpoint); pin the symbol.
        let _ = acquire_groww_scale_fleet_lock;
    }

    #[test]
    fn test_release_timeout_is_bounded_and_below_ttl() {
        // The release wait must be a small bound (never hangs shutdown) and
        // far below the 90s TTL it is a fast-path alternative to.
        assert!(GROWW_SCALE_LOCK_RELEASE_TIMEOUT_SECS >= 1);
        assert!(
            GROWW_SCALE_LOCK_RELEASE_TIMEOUT_SECS
                < tickvault_core::instance_lock::INSTANCE_LOCK_TTL_SECS
        );
    }

    #[tokio::test]
    async fn test_release_on_shutdown_completes_within_bound() {
        // The heartbeat stand-in parks on the SAME Notify the guard signals;
        // notify_one stores a permit, so the release completes even if the
        // task has not yet reached `notified()` (Rule 16 lost-wake trap).
        let shutdown = Arc::new(Notify::new());
        let held = Arc::new(AtomicBool::new(true));
        let hb_shutdown = Arc::clone(&shutdown);
        let heartbeat = tokio::spawn(async move {
            hb_shutdown.notified().await;
        });
        let guard = GrowwScaleFleetLockGuard {
            heartbeat,
            shutdown,
            held,
        };
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            guard.release_on_shutdown(),
        )
        .await
        .expect("release_on_shutdown must complete promptly when the heartbeat exits");
    }

    #[tokio::test]
    async fn test_release_on_shutdown_returns_even_if_heartbeat_already_exited() {
        // A mid-run lock loss makes the heartbeat exit BEFORE shutdown; the
        // release path must still return cleanly (JoinHandle resolves
        // immediately for a finished task).
        let shutdown = Arc::new(Notify::new());
        let held = Arc::new(AtomicBool::new(false));
        let heartbeat = tokio::spawn(async {});
        let _ = heartbeat.is_finished();
        let guard = GrowwScaleFleetLockGuard {
            heartbeat,
            shutdown,
            held,
        };
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            guard.release_on_shutdown(),
        )
        .await
        .expect("release_on_shutdown must not hang on an already-exited heartbeat");
    }
}
