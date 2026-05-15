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
//! Item 19a (this PR) ships the pure-logic primitives — host_id
//! composition, lock-key formatting, constants. Item 19b will wire
//! the async acquire/renew/release calls over `ValkeyCache` plus the
//! boot-sequence integration. Item 19c will add the
//! `live_instance_lock` audit table.

use tickvault_common::sanitize::sanitize_ilp_symbol;

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

#[cfg(test)]
mod tests {
    use super::*;

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
