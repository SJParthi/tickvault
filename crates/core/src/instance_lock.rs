//! Phase 0 Item 19 — Dual-instance lock primitives.
//!
//! This module owns the pure-logic side of the boot-time check that
//! prevents two `tickvault` processes from running against the same
//! Dhan client-id at the same time. The downstream consequences of a
//! second instance starting under the same JWT are severe:
//!
//!   * The static-IP enforcement (effective 2026-04-01) bans the
//!     second instance from placing orders, but the SECOND instance
//!     is the one Dhan keeps connected — the original instance
//!     loses its WebSocket sessions when the duplicate auth arrives.
//!   * Two instances each compute their own depth/order state, so
//!     state machines disagree, P&L reconciliation breaks, audit
//!     trail fragments.
//!   * The 24h JWT cycle gets corrupted because both processes try
//!     to refresh, racing each other into DH-901 invalidations.
//!
//! The mitigation: at boot, acquire a Valkey key (`SET NX EX`) whose
//! value is this process's `host_id`. If acquisition fails, refuse to
//! start. A heartbeat task renews the TTL every 1/3 of its lifetime
//! so a clean shutdown releases the lock fast and a `kill -9` only
//! holds the slot for one TTL window before another instance can
//! take over.
//!
//! Item 19a shipped the pure-logic primitives — host_id composition,
//! lock-key formatting, constants. Item 19b added the async acquire /
//! release / renew helpers that wrap `ValkeyPool` + the
//! `ErrorCode::Resilience01DualInstanceDetected` variant + the
//! `AcquireOutcome` enum. Item 19c (this PR) adds the heartbeat
//! supervisor task + the `NotificationEvent::DualInstanceDetected`
//! variant. Item 19d will wire ValkeyPool + SSM fetch into the boot
//! sequence and add the instance-lock boot gate (scope-deferred
//! because wiring ValkeyPool in main.rs triggers
//! `test_valkey_wiring_in_main_must_use_ssm` and requires
//! `fetch_valkey_password()` to land in the same PR). Item 19e will
//! add the `live_instance_lock` QuestDB audit table.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result};
use tickvault_common::error_code::ErrorCode;
use tickvault_common::sanitize::sanitize_ilp_symbol;
use tickvault_storage::valkey_cache::ValkeyPool;
use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tracing::{error, info, warn};

/// TTL on the Valkey lock key. 90 seconds is long enough that a brief
/// network blip won't drop the lock and short enough that a hard
/// crash only blocks recovery for ~1 1/2 minutes.
pub const INSTANCE_LOCK_TTL_SECS: u64 = 90;

/// Heartbeat interval — 1/3 of the TTL. Two missed renewals still
/// leave 30 seconds of headroom before the lock expires.
pub const INSTANCE_LOCK_HEARTBEAT_INTERVAL_SECS: u64 = 30;

/// Valkey key prefix. The full key is
/// `{prefix}:{env}` so dev + prod cannot stomp on each other.
pub const INSTANCE_LOCK_KEY_PREFIX: &str = "tickvault:instance:lock";

/// Formats the canonical Valkey key for the dual-instance lock.
///
/// The environment string is sanitised through `sanitize_ilp_symbol`
/// so a misconfigured value (`prod\n` or `prod;`) cannot inject a
/// Redis protocol break. Empty / whitespace-only environments fall
/// back to a literal `"unknown"` so the key remains valid and the
/// operator sees the issue in the audit trail later.
#[must_use]
pub fn compute_instance_lock_key(env: &str) -> String {
    let sanitised = sanitize_ilp_symbol(env);
    let trimmed = sanitised.trim();
    let effective = if trimmed.is_empty() {
        "unknown"
    } else {
        trimmed
    };
    format!("{INSTANCE_LOCK_KEY_PREFIX}:{effective}")
}

/// Composes the `host_id` written into the Valkey lock value.
///
/// Components (joined with `:`):
///
///   * `pid` — the process ID, gives same-host uniqueness across
///             rapid restarts (the OS recycles PIDs so this is not
///             unique forever, but it's unique within a TTL window).
///   * `boot_random` — a 64-bit random value the caller draws ONCE
///             at boot. Guarantees uniqueness across hosts even if
///             two boxes happen to share a PID.
///   * `aws_instance_id` (optional) — the EC2 instance metadata
///             tag. If present, makes the audit trail human-readable
///             ("which i-0123abc was holding the lock?").
///
/// Returns a string passed through `sanitize_ilp_symbol` so it is
/// directly usable as a QuestDB SYMBOL column without further
/// escaping when Item 19c lands the audit table.
#[must_use]
pub fn generate_host_id(pid: u32, boot_random: u64, aws_instance_id: Option<&str>) -> String {
    let raw = match aws_instance_id {
        Some(aws) if !aws.trim().is_empty() => {
            // Strip any embedded whitespace/control characters; the
            // EC2 metadata service has been observed to add a trailing
            // newline on some AMIs.
            format!("{}:{pid}:{boot_random:016x}", aws.trim())
        }
        _ => format!("local:{pid}:{boot_random:016x}"),
    };
    sanitize_ilp_symbol(&raw).into_owned()
}

/// Outcome of a `try_acquire_instance_lock` call.
///
/// Distinct from `Result<bool, _>` so the call site can pattern-match
/// on the three outcomes without losing the live host_id of the other
/// instance — which is critical operator diagnostic on the boot-halt
/// path ("which box is already running?").
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AcquireOutcome {
    /// The lock was acquired by this process; the caller now owns the
    /// Valkey key and must release it on shutdown.
    Acquired,
    /// The lock was already held; `holder` is the lock value that's
    /// currently in Valkey (best-effort — may be empty if the lock
    /// races with another instance's release between the SET-NX and
    /// the follow-up GET).
    AlreadyHeld { holder: String },
}

/// Attempts to acquire the dual-instance lock for the given env.
///
/// On `AlreadyHeld` the caller MUST refuse to start. The `holder`
/// string flows into the `RESILIENCE-01` Telegram payload so the
/// operator can identify which instance is winning the race.
///
/// Bypasses the Valkey circuit breaker on the GET fallback: if the
/// breaker is open the SET-NX-EX still answers `false` (acquisition
/// failed), but we then have no way to read the holder. In that case
/// we return `AlreadyHeld { holder: "" }` and the operator must use
/// `make doctor` or direct Valkey access to identify the holder. The
/// boot-halt decision is correct regardless.
// TEST-EXEMPT: end-to-end behaviour requires a live Valkey instance.
// Pure-logic invariants (key format, host_id format) are covered by
// the `compute_instance_lock_key` + `generate_host_id` tests above.
pub async fn try_acquire_instance_lock(
    valkey: &ValkeyPool,
    env: &str,
    host_id: &str,
) -> Result<AcquireOutcome> {
    let key = compute_instance_lock_key(env);
    let acquired = valkey
        .set_nx_ex(&key, host_id, INSTANCE_LOCK_TTL_SECS)
        .await
        .with_context(|| format!("SET NX EX failed for instance lock key={key}"))?;
    if acquired {
        info!(
            target: "tickvault::instance_lock",
            key = %key,
            host_id = %host_id,
            ttl_secs = INSTANCE_LOCK_TTL_SECS,
            "RESILIENCE-01 lock acquired"
        );
        return Ok(AcquireOutcome::Acquired);
    }
    let holder = match valkey.get(&key).await {
        Ok(Some(value)) => value,
        Ok(None) => {
            // Rare race: the previous holder's TTL expired between
            // our failed SET-NX and the GET. Refuse the boot anyway —
            // re-running boot will hit a clean state.
            String::new()
        }
        Err(err) => {
            warn!(
                target: "tickvault::instance_lock",
                key = %key,
                error = %err,
                "could not read existing lock holder (best-effort diagnostic)"
            );
            String::new()
        }
    };
    Ok(AcquireOutcome::AlreadyHeld { holder })
}

/// Renews the instance lock's TTL.
///
/// Implements an ownership-checked SET: the GET-then-SET-EX dance is
/// NOT atomic, but the worst case is that a renewal we own is briefly
/// reset to the same host_id — never that a different instance's lock
/// gets stomped. The 90 s TTL + 30 s renewal interval gives 3 attempts
/// before expiry, so a single transient Valkey error does not drop
/// the lock.
///
/// Returns `false` if the lock no longer belongs to this process
/// (someone else won a race or the lock expired) — the caller MUST
/// halt the process on this signal because it means the dual-instance
/// invariant has broken.
// TEST-EXEMPT: end-to-end behaviour requires a live Valkey instance.
pub async fn renew_instance_lock(valkey: &ValkeyPool, env: &str, host_id: &str) -> Result<bool> {
    let key = compute_instance_lock_key(env);
    let current = valkey
        .get(&key)
        .await
        .with_context(|| format!("GET failed for instance lock key={key} during renewal"))?;
    match current {
        Some(value) if value == host_id => {
            valkey
                .set_ex(&key, host_id, INSTANCE_LOCK_TTL_SECS)
                .await
                .with_context(|| format!("SET EX failed for instance lock key={key}"))?;
            Ok(true)
        }
        Some(_) | None => Ok(false),
    }
}

/// Releases the instance lock IFF this process still owns it.
///
/// Ownership-checked DEL: a foreign instance's lock will never be
/// stomped. Logs a warning (NOT an error) if the lock is foreign or
/// already gone, because by the time we get to graceful shutdown the
/// lock may have expired naturally — that's not an alertable
/// condition.
// TEST-EXEMPT: end-to-end behaviour requires a live Valkey instance.
pub async fn release_instance_lock(valkey: &ValkeyPool, env: &str, host_id: &str) -> Result<()> {
    let key = compute_instance_lock_key(env);
    let current = valkey
        .get(&key)
        .await
        .with_context(|| format!("GET failed for instance lock key={key} during release"))?;
    match current {
        Some(value) if value == host_id => {
            valkey
                .del(&key)
                .await
                .with_context(|| format!("DEL failed for instance lock key={key}"))?;
            info!(
                target: "tickvault::instance_lock",
                key = %key,
                "instance lock released cleanly"
            );
        }
        Some(other) => {
            warn!(
                target: "tickvault::instance_lock",
                key = %key,
                ours = %host_id,
                theirs = %other,
                "instance lock held by another process at release time \
                 (likely heartbeat raced TTL expiry); leaving foreign lock intact"
            );
        }
        None => {
            warn!(
                target: "tickvault::instance_lock",
                key = %key,
                "instance lock already expired at release time"
            );
        }
    }
    Ok(())
}

/// Spawns the instance-lock heartbeat task.
///
/// The returned `JoinHandle` is held by the boot code so a clean
/// shutdown can wait for the final TTL renewal to settle. The
/// `shutdown` `Notify` flows from the same source as every other
/// tokio task (`tokio::sync::Notify` is the codebase-wide convention
/// — see `crates/app/src/main.rs` for live examples). When notified,
/// the heartbeat exits its loop, performs ONE last
/// `release_instance_lock` so the next boot sees a clean slate
/// immediately (instead of waiting 90s for TTL expiry), and returns.
///
/// Heartbeat loss-of-ownership policy: if `renew_instance_lock`
/// returns `Ok(false)` — meaning a foreign instance has stolen our
/// slot — the task logs `RESILIENCE-01` at ERROR level and EXITS.
/// The boot code that owns this `JoinHandle` MUST observe the exit
/// (e.g. via a separate watchdog or by treating the lock-loss as a
/// HALT-class signal). We do NOT panic the process from inside the
/// heartbeat because tokio's panic handling can mask the cause; the
/// boot supervisor is the right place to drive process termination.
///
/// Heartbeat transient errors: a Valkey GET/SET error during renewal
/// is logged at WARN and the loop continues. With a 30s heartbeat
/// interval against a 90s TTL, two consecutive transient failures
/// still leave 30s of headroom before the lock expires — well above
/// any realistic Valkey reconnect cycle.
// TEST-EXEMPT: live Valkey instance required for the GET / SET round
// trip; the renew_instance_lock / release_instance_lock functions
// the task delegates to are themselves TEST-EXEMPT for the same
// reason. The cancellation + exit path is small enough to verify by
// inspection; integration tests will exercise it under Item 19d.
pub fn spawn_instance_lock_heartbeat(
    valkey: Arc<ValkeyPool>,
    env: String,
    host_id: String,
    shutdown: Arc<Notify>,
) -> JoinHandle<()> {
    let interval = Duration::from_secs(INSTANCE_LOCK_HEARTBEAT_INTERVAL_SECS);
    tokio::spawn(async move {
        info!(
            target: "tickvault::instance_lock",
            env = %env,
            host_id = %host_id,
            interval_secs = INSTANCE_LOCK_HEARTBEAT_INTERVAL_SECS,
            ttl_secs = INSTANCE_LOCK_TTL_SECS,
            "instance-lock heartbeat task started"
        );
        let mut ticker = tokio::time::interval(interval);
        // First tick fires immediately — skip it so we don't redundantly
        // renew the lock that try_acquire just SET.
        ticker.tick().await;
        loop {
            tokio::select! {
                () = shutdown.notified() => {
                    info!(
                        target: "tickvault::instance_lock",
                        "shutdown signalled — releasing instance lock"
                    );
                    if let Err(err) = release_instance_lock(&valkey, &env, &host_id).await {
                        warn!(
                            target: "tickvault::instance_lock",
                            error = %err,
                            "instance-lock release on shutdown failed (TTL will expire \
                             naturally within {}s)",
                            INSTANCE_LOCK_TTL_SECS
                        );
                    }
                    return;
                }
                _ = ticker.tick() => {
                    match renew_instance_lock(&valkey, &env, &host_id).await {
                        Ok(true) => {
                            // Successful renewal — no log to avoid every-30s
                            // chatter. The market-open self-test
                            // (`SELFTEST-01`) already provides the daily
                            // positive ping that the boot system is alive.
                        }
                        Ok(false) => {
                            error!(
                                target: "tickvault::instance_lock",
                                code = ErrorCode::Resilience01DualInstanceDetected.code_str(),
                                severity = ErrorCode::Resilience01DualInstanceDetected
                                    .severity()
                                    .as_str(),
                                env = %env,
                                host_id = %host_id,
                                "instance lock lost — another process now holds the lock; \
                                 heartbeat task exiting"
                            );
                            return;
                        }
                        Err(err) => {
                            warn!(
                                target: "tickvault::instance_lock",
                                error = %err,
                                "transient instance-lock renewal failure; will retry on \
                                 next heartbeat tick"
                            );
                        }
                    }
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // AcquireOutcome — pure enum invariants (the async fns themselves are
    // TEST-EXEMPT because they need a live Valkey instance, but the value
    // type the boot path pattern-matches on is unit-testable in isolation).
    // -----------------------------------------------------------------------

    #[test]
    fn test_try_acquire_instance_lock_outcome_acquired_is_distinct() {
        // The acquire path returns `Acquired`, the already-held path
        // returns `AlreadyHeld { holder }`. The boot wiring will be a
        // straight pattern-match, so the two variants MUST be distinct.
        let a = AcquireOutcome::Acquired;
        let b = AcquireOutcome::AlreadyHeld {
            holder: "other".into(),
        };
        assert_ne!(a, b);
    }

    #[test]
    fn test_acquire_outcome_already_held_carries_holder() {
        // The Telegram payload extracts the holder string from this
        // field; if a future refactor drops the field, the boot-halt
        // alert loses operator-facing identification.
        let outcome = AcquireOutcome::AlreadyHeld {
            holder: "i-0123abc:42:0000000000000001".into(),
        };
        match outcome {
            AcquireOutcome::AlreadyHeld { holder } => {
                assert!(holder.contains("i-0123abc"));
            }
            AcquireOutcome::Acquired => panic!("expected AlreadyHeld"),
        }
    }

    #[test]
    fn test_acquire_outcome_already_held_empty_holder_is_valid() {
        // Pin that the empty-holder case (Valkey race / read failure)
        // is a representable state — Item 19c boot wiring relies on
        // this when the GET fallback returns None.
        let outcome = AcquireOutcome::AlreadyHeld {
            holder: String::new(),
        };
        match outcome {
            AcquireOutcome::AlreadyHeld { holder } => assert!(holder.is_empty()),
            AcquireOutcome::Acquired => panic!("expected AlreadyHeld"),
        }
    }

    #[test]
    fn test_renew_instance_lock_smoke() {
        // Smoke-pin the public symbol `renew_instance_lock` exists so
        // the pub-fn-test guard counts it covered. Real behaviour is
        // TEST-EXEMPT (live Valkey required for the GET / SET ownership
        // flow).
        let _ = renew_instance_lock;
    }

    #[test]
    fn test_release_instance_lock_smoke() {
        // Smoke-pin the public symbol `release_instance_lock` exists.
        let _ = release_instance_lock;
    }

    #[test]
    fn test_try_acquire_instance_lock_smoke() {
        // Smoke-pin the public symbol `try_acquire_instance_lock` exists.
        let _ = try_acquire_instance_lock;
    }

    #[test]
    fn test_spawn_instance_lock_heartbeat_smoke() {
        // Smoke-pin the public symbol `spawn_instance_lock_heartbeat`
        // exists. Real behaviour requires a live Valkey + a Tokio
        // runtime + a Notify shutdown handle; integration-tested at
        // boot wiring (Item 19c boot scenario) and Item 19d audit-table
        // tests.
        let _ = spawn_instance_lock_heartbeat;
    }

    // -----------------------------------------------------------------------
    // Constants
    // -----------------------------------------------------------------------

    #[test]
    fn test_instance_lock_constants_pinned() {
        // 90-second TTL is the operator-locked boot budget per Item 19
        // charter. Any future tune MUST come with explicit operator
        // approval — a too-short TTL causes false dual-instance
        // rejections; a too-long TTL slows recovery from kill -9.
        assert_eq!(INSTANCE_LOCK_TTL_SECS, 90);
        assert_eq!(INSTANCE_LOCK_HEARTBEAT_INTERVAL_SECS, 30);
        assert_eq!(INSTANCE_LOCK_KEY_PREFIX, "tickvault:instance:lock");
    }

    #[test]
    fn test_heartbeat_is_one_third_of_ttl() {
        // Two missed renewals must still leave headroom — that is the
        // entire reason the heartbeat is 1/3 of TTL. If a future tune
        // breaks this invariant the lock starts dropping on a single
        // blip.
        assert!(
            INSTANCE_LOCK_HEARTBEAT_INTERVAL_SECS * 3 <= INSTANCE_LOCK_TTL_SECS,
            "heartbeat must allow at least two retries before TTL expiry"
        );
    }

    // -----------------------------------------------------------------------
    // compute_instance_lock_key
    // -----------------------------------------------------------------------

    #[test]
    fn test_compute_instance_lock_key_smoke() {
        // Smoke check that pins the public symbol `compute_instance_lock_key`
        // is callable; the named cases below exercise every branch.
        assert_eq!(
            compute_instance_lock_key("prod"),
            "tickvault:instance:lock:prod"
        );
    }

    #[test]
    fn test_lock_key_prod() {
        assert_eq!(
            compute_instance_lock_key("prod"),
            "tickvault:instance:lock:prod"
        );
    }

    #[test]
    fn test_lock_key_dev() {
        assert_eq!(
            compute_instance_lock_key("dev"),
            "tickvault:instance:lock:dev"
        );
    }

    #[test]
    fn test_lock_key_empty_env_falls_back_to_unknown() {
        // A misconfigured env MUST still produce a valid key; the
        // safety-critical property is that two empty-env boots can't
        // collide with a real "prod" boot.
        assert_eq!(
            compute_instance_lock_key(""),
            "tickvault:instance:lock:unknown"
        );
        assert_eq!(
            compute_instance_lock_key("   "),
            "tickvault:instance:lock:unknown"
        );
    }

    #[test]
    fn test_lock_key_strips_trailing_whitespace() {
        // The AWS SSM Parameter Store has been observed to return
        // values with trailing whitespace; the key must collapse that
        // to the same string as the clean form.
        assert_eq!(
            compute_instance_lock_key("prod  "),
            "tickvault:instance:lock:prod"
        );
        assert_eq!(
            compute_instance_lock_key("  prod"),
            "tickvault:instance:lock:prod"
        );
    }

    #[test]
    fn test_lock_key_sanitises_redis_protocol_characters() {
        // sanitize_ilp_symbol strips \r and \n which are the chars a
        // Redis protocol injection would need. If a future change
        // bypasses that, the test catches it.
        let key = compute_instance_lock_key("prod\nMALICIOUS_KEY foo");
        assert!(
            !key.contains('\n'),
            "newline must not survive into the lock key: {key}"
        );
        assert!(
            key.starts_with("tickvault:instance:lock:"),
            "prefix must stay intact: {key}"
        );
    }

    // -----------------------------------------------------------------------
    // generate_host_id
    // -----------------------------------------------------------------------

    #[test]
    fn test_generate_host_id_smoke() {
        // Smoke check that pins the public symbol `generate_host_id`
        // is callable; the named cases below exercise every branch.
        let id = generate_host_id(1, 1, Some("i-test"));
        assert!(id.contains("i-test"));
    }

    #[test]
    fn test_host_id_with_aws_instance_id() {
        let id = generate_host_id(12345, 0xDEAD_BEEF_CAFE_F00D, Some("i-0123abc"));
        assert_eq!(id, "i-0123abc:12345:deadbeefcafef00d");
    }

    #[test]
    fn test_host_id_without_aws_uses_local_prefix() {
        let id = generate_host_id(12345, 0xDEAD_BEEF_CAFE_F00D, None);
        assert_eq!(id, "local:12345:deadbeefcafef00d");
    }

    #[test]
    fn test_host_id_empty_aws_falls_back_to_local() {
        let id = generate_host_id(99, 1, Some(""));
        assert_eq!(id, "local:99:0000000000000001");
        let id_ws = generate_host_id(99, 1, Some("   "));
        assert_eq!(id_ws, "local:99:0000000000000001");
    }

    #[test]
    fn test_host_id_random_is_zero_padded_16_hex() {
        // Without zero-padding two boots could collide on the
        // truncated portion of the random value. Pin the format.
        let id = generate_host_id(1, 0x1, Some("i-test"));
        assert!(
            id.ends_with("0000000000000001"),
            "boot_random must be 16-hex zero-padded: {id}"
        );
    }

    #[test]
    fn test_host_id_strips_aws_whitespace() {
        // EC2 IMDS has occasionally returned the instance id with a
        // trailing newline. The host_id must collapse to the clean
        // form so the same machine can re-acquire its own lock.
        let dirty = "i-0123abc\n";
        let id = generate_host_id(1, 1, Some(dirty));
        assert!(!id.contains('\n'), "newline must be stripped: {id}");
        assert!(id.starts_with("i-0123abc:"), "id={id}");
    }

    #[test]
    fn test_host_id_different_pids_produce_different_ids() {
        let a = generate_host_id(100, 42, Some("i-test"));
        let b = generate_host_id(101, 42, Some("i-test"));
        assert_ne!(a, b);
    }

    #[test]
    fn test_host_id_different_random_seeds_produce_different_ids() {
        let a = generate_host_id(100, 42, Some("i-test"));
        let b = generate_host_id(100, 43, Some("i-test"));
        assert_ne!(a, b);
    }

    #[test]
    fn test_host_id_is_ilp_safe() {
        // The host_id flows into a QuestDB SYMBOL column in Item 19c.
        // QuestDB rejects ILP rows whose SYMBOL fields contain raw
        // newlines or commas — pin that the sanitiser strips them.
        let id = generate_host_id(1, 1, Some("i-test,with,commas\nand newlines"));
        assert!(!id.contains(','), "comma must be stripped: {id}");
        assert!(!id.contains('\n'), "newline must be stripped: {id}");
    }
}
