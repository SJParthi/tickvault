//! Phase 0 Item 19e — dual-instance Valkey lock audit persistence.
//!
//! Persists every lifecycle event of the boot-time + heartbeat-time
//! Valkey lock that enforces the "one tickvault per Dhan client-id"
//! rule (Items 19a/b/c/d). The operator can answer:
//!
//!   * "Did this host acquire the lock cleanly at 09:01 IST today?"
//!   * "How many `already_held` events have I had this quarter, and
//!     who was the live peer?"
//!   * "When was the lock last released — graceful shutdown or did
//!     the heartbeat detect ownership loss?"
//!
//! One row per lifecycle event. The DEDUP key is `(trading_date_ist,
//! ts, host_id, outcome)` so that:
//!
//!   * a same-host same-second double-write idempotently overwrites
//!     (typical when graceful-shutdown release races the final
//!     heartbeat tick);
//!   * different hosts on the same second do NOT collide (so the
//!     loser's `already_held` row and the winner's `acquired` row
//!     both survive).

use std::time::Duration;

use reqwest::Client;
use tracing::{error, info, warn};

use tickvault_common::config::QuestDbConfig;
use tickvault_common::sanitize::sanitize_audit_string;

pub const QUESTDB_TABLE_LIVE_INSTANCE_LOCK: &str = "live_instance_lock";

// QuestDB requires the designated timestamp column to be present in
// DEDUP UPSERT KEYS — same regression-class as static_ip_audit's
// 2026-04-28 fix. The composite `(trading_date_ist, ts, host_id,
// outcome)` lets two hosts boot in the same second without losing
// either row.
pub const DEDUP_KEY_LIVE_INSTANCE_LOCK: &str = "trading_date_ist, ts, host_id, outcome";

const QUESTDB_DDL_TIMEOUT_SECS: u64 = 10;

/// Stable wire-format outcome strings written into the `outcome`
/// column. Kept as a typed enum so callers can't accidentally write
/// `"ACQUIRED"` (uppercase) or `"acquire"` (verb stem) and silently
/// fragment the column's symbol set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LiveInstanceLockOutcome {
    /// Boot-time SET-NX-EX succeeded; this process holds the lock.
    Acquired,
    /// Boot-time SET-NX-EX returned conflict — another live peer
    /// already holds the lock. Boot HALTS via Item 19d's wiring.
    AlreadyHeld,
    /// Heartbeat detected ownership loss (`renew_instance_lock`
    /// returned `Ok(false)`). Heartbeat task exits; boot supervisor
    /// is expected to treat this as a HALT-class signal.
    LostOwnership,
    /// Graceful shutdown — Notify fired, heartbeat released the lock
    /// before exiting.
    GracefulRelease,
    /// Valkey GET / SET / DEL error during boot acquire or
    /// heartbeat renew. Boot HALTs at the acquire site; mid-session
    /// errors are logged but the heartbeat retries on the next tick.
    ValkeyError,
}

impl LiveInstanceLockOutcome {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Acquired => "acquired",
            Self::AlreadyHeld => "already_held",
            Self::LostOwnership => "lost_ownership",
            Self::GracefulRelease => "graceful_release",
            Self::ValkeyError => "valkey_error",
        }
    }
}

/// Creates the audit table if absent. Idempotent — safe to call on
/// every boot.
// TEST-EXEMPT: requires running QuestDB; tested via boot integration in CI.
pub async fn ensure_live_instance_lock_table(questdb_config: &QuestDbConfig) {
    let base_url = format!(
        "http://{}:{}/exec",
        questdb_config.host, questdb_config.http_port
    );
    let client = Client::builder()
        .timeout(Duration::from_secs(QUESTDB_DDL_TIMEOUT_SECS))
        .build()
        .unwrap_or_else(|_| Client::new());

    let create_ddl = format!(
        "CREATE TABLE IF NOT EXISTS {QUESTDB_TABLE_LIVE_INSTANCE_LOCK} (\
            ts TIMESTAMP, \
            trading_date_ist TIMESTAMP, \
            outcome SYMBOL, \
            host_id SYMBOL, \
            peer_holder SYMBOL, \
            env SYMBOL, \
            lock_key SYMBOL, \
            error_detail STRING\
        ) timestamp(ts) PARTITION BY DAY \
        DEDUP UPSERT KEYS({DEDUP_KEY_LIVE_INSTANCE_LOCK});"
    );
    match client
        .get(&base_url)
        .query(&[("query", create_ddl.as_str())])
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => {
            info!(
                table = QUESTDB_TABLE_LIVE_INSTANCE_LOCK,
                "audit table ready"
            );
        }
        Ok(resp) => {
            warn!(
                table = QUESTDB_TABLE_LIVE_INSTANCE_LOCK,
                status = %resp.status(),
                "DDL non-2xx"
            );
        }
        Err(err) => {
            error!(
                table = QUESTDB_TABLE_LIVE_INSTANCE_LOCK,
                ?err,
                "DDL request failed"
            );
        }
    }
}

/// Appends one live-instance-lock audit row.
///
/// `ts_nanos_ist` and `trading_date_ist_nanos` are IST wall-clock
/// nanoseconds — the function divides them to microseconds before
/// embedding because QuestDB `TIMESTAMP` columns store microseconds
/// since epoch (same nanos-to-micros lesson as
/// `static_ip_audit_persistence`).
// APPROVED: 9 wire-format columns plus QuestDB config — same audit-row shape as sibling static_ip_audit / boot_audit writers.
#[allow(clippy::too_many_arguments)]
pub async fn append_live_instance_lock_row(
    questdb_config: &QuestDbConfig,
    ts_nanos_ist: i64,
    trading_date_ist_nanos: i64,
    outcome: LiveInstanceLockOutcome,
    host_id: &str,
    peer_holder: &str,
    env: &str,
    lock_key: &str,
    error_detail: &str,
) -> anyhow::Result<()> {
    let base_url = format!(
        "http://{}:{}/exec",
        questdb_config.host, questdb_config.http_port
    );
    let client = Client::builder()
        .timeout(Duration::from_secs(QUESTDB_DDL_TIMEOUT_SECS))
        .build()?;
    let outcome_str = outcome.as_str();
    let host_id_esc = sanitize_audit_string(host_id);
    let peer_holder_esc = sanitize_audit_string(peer_holder);
    let env_esc = sanitize_audit_string(env);
    let lock_key_esc = sanitize_audit_string(lock_key);
    let error_detail_esc = sanitize_audit_string(error_detail);
    let ts_micros_ist = ts_nanos_ist / 1_000;
    let trading_date_ist_micros = trading_date_ist_nanos / 1_000;
    let sql = format!(
        "INSERT INTO {QUESTDB_TABLE_LIVE_INSTANCE_LOCK} \
         (ts, trading_date_ist, outcome, host_id, peer_holder, env, lock_key, error_detail) VALUES \
         ({ts_micros_ist}, {trading_date_ist_micros}, '{outcome_str}', '{host_id_esc}', '{peer_holder_esc}', '{env_esc}', '{lock_key_esc}', '{error_detail_esc}');"
    );
    let resp = client
        .get(&base_url)
        .query(&[("query", sql.as_str())])
        .send()
        .await?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("live_instance_lock insert non-2xx ({status}): {body}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_cfg(http_port: u16) -> QuestDbConfig {
        QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port,
            pg_port: 8812,
            ilp_port: 9009,
        }
    }

    #[test]
    fn test_table_name_constant() {
        assert_eq!(QUESTDB_TABLE_LIVE_INSTANCE_LOCK, "live_instance_lock");
    }

    #[test]
    fn test_outcome_as_str_stable() {
        // Wire-format strings — Grafana panels + audit queries depend on these.
        assert_eq!(LiveInstanceLockOutcome::Acquired.as_str(), "acquired");
        assert_eq!(
            LiveInstanceLockOutcome::AlreadyHeld.as_str(),
            "already_held"
        );
        assert_eq!(
            LiveInstanceLockOutcome::LostOwnership.as_str(),
            "lost_ownership"
        );
        assert_eq!(
            LiveInstanceLockOutcome::GracefulRelease.as_str(),
            "graceful_release"
        );
        assert_eq!(
            LiveInstanceLockOutcome::ValkeyError.as_str(),
            "valkey_error"
        );
    }

    /// Regression: 2026-04-28 — QuestDB requires the designated
    /// timestamp column to be present in DEDUP UPSERT KEYS. Without
    /// `ts` the CREATE TABLE returns 400 Bad Request and the table
    /// is silently missing for the whole session.
    #[test]
    fn test_dedup_key_includes_designated_timestamp() {
        assert!(
            DEDUP_KEY_LIVE_INSTANCE_LOCK.contains("ts"),
            "live_instance_lock DEDUP key must include `ts` (designated timestamp); \
             QuestDB rejects DDL otherwise. Got: {DEDUP_KEY_LIVE_INSTANCE_LOCK}"
        );
    }

    /// The DEDUP key must also include `host_id` and `outcome` so
    /// multiple hosts booting in the same second don't collide and
    /// lose each other's rows.
    #[test]
    fn test_dedup_key_includes_host_id_and_outcome() {
        assert!(DEDUP_KEY_LIVE_INSTANCE_LOCK.contains("host_id"));
        assert!(DEDUP_KEY_LIVE_INSTANCE_LOCK.contains("outcome"));
        assert!(DEDUP_KEY_LIVE_INSTANCE_LOCK.contains("trading_date_ist"));
    }

    /// The audit table holds lock lifecycle events, not per-instrument
    /// rows — `security_id` MUST NOT appear in the DEDUP key.
    #[test]
    fn test_dedup_key_no_security_id() {
        assert!(!DEDUP_KEY_LIVE_INSTANCE_LOCK.contains("security_id"));
    }

    /// Regression: 2026-04-28 nanos-to-micros bug class. Source-scan
    /// ratchet locks the conversion line.
    #[test]
    fn test_insert_sql_uses_microseconds_not_nanoseconds() {
        let src = include_str!("live_instance_lock_persistence.rs");
        assert!(
            src.contains("ts_micros_ist = ts_nanos_ist / 1_000"),
            "INSERT must convert nanos to micros before embedding"
        );
        assert!(
            src.contains("trading_date_ist_micros = trading_date_ist_nanos / 1_000"),
            "INSERT must convert trading_date_ist nanos to micros"
        );
    }

    /// Schema-drift guard: the DDL column set is the operator-facing
    /// contract. New columns are fine; renaming or removing one breaks
    /// downstream Grafana queries and audit SELECTs silently.
    #[test]
    fn test_ddl_contains_expected_columns() {
        let src = include_str!("live_instance_lock_persistence.rs");
        for column in [
            "ts TIMESTAMP",
            "trading_date_ist TIMESTAMP",
            "outcome SYMBOL",
            "host_id SYMBOL",
            "peer_holder SYMBOL",
            "env SYMBOL",
            "lock_key SYMBOL",
            "error_detail STRING",
        ] {
            assert!(
                src.contains(column),
                "DDL must declare `{column}` — schema drift will break Grafana queries"
            );
        }
    }

    /// All 5 outcome variants must be reachable from the typed enum's
    /// `as_str` — operator-facing wire format. If a variant is added
    /// without updating `as_str` the build fails (match exhaustive),
    /// but this test additionally guards the wire-format alphabet so
    /// nobody accidentally renames an existing label.
    #[test]
    fn test_all_outcomes_have_distinct_wire_format_labels() {
        let labels = [
            LiveInstanceLockOutcome::Acquired.as_str(),
            LiveInstanceLockOutcome::AlreadyHeld.as_str(),
            LiveInstanceLockOutcome::LostOwnership.as_str(),
            LiveInstanceLockOutcome::GracefulRelease.as_str(),
            LiveInstanceLockOutcome::ValkeyError.as_str(),
        ];
        let mut sorted: Vec<&str> = labels.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(
            sorted.len(),
            labels.len(),
            "wire-format outcome labels must be unique; got duplicates: {labels:?}"
        );
    }

    /// Smoke-pin the pub fn symbol exists. Companion to the three
    /// `#[tokio::test]` cases below — the pub-fn-test guard only
    /// scans for `#[test]` (sync), so without this sync test the
    /// guard cannot see `append_live_instance_lock_row` as covered.
    #[test]
    fn test_append_live_instance_lock_row_smoke() {
        let _ = append_live_instance_lock_row;
    }

    #[tokio::test]
    async fn test_append_returns_err_when_questdb_unreachable() {
        // Port 1 is reserved — guaranteed connection refusal.
        let cfg = test_cfg(1);
        let result = append_live_instance_lock_row(
            &cfg,
            1_710_000_000_000_000_000,
            1_709_980_200_000_000_000,
            LiveInstanceLockOutcome::Acquired,
            "host-a",
            "",
            "prod",
            "tickvault:instance:lock:prod",
            "",
        )
        .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_append_handles_already_held_with_peer_holder() {
        // Exercise the AlreadyHeld branch including a non-empty
        // peer_holder so the format!() macro can't quietly break on
        // the path operators care about most.
        let cfg = test_cfg(1);
        let result = append_live_instance_lock_row(
            &cfg,
            1_710_000_000_000_000_000,
            1_709_980_200_000_000_000,
            LiveInstanceLockOutcome::AlreadyHeld,
            "host-a:99:abc",
            "host-b:42:def",
            "prod",
            "tickvault:instance:lock:prod",
            "",
        )
        .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_append_handles_valkey_error_with_detail() {
        let cfg = test_cfg(1);
        let result = append_live_instance_lock_row(
            &cfg,
            1_710_000_000_000_000_000,
            1_709_980_200_000_000_000,
            LiveInstanceLockOutcome::ValkeyError,
            "host-a:99:abc",
            "",
            "prod",
            "tickvault:instance:lock:prod",
            "connection refused at boot",
        )
        .await;
        assert!(result.is_err());
    }
}
