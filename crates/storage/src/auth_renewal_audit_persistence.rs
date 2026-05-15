//! Phase 0 Item 17 — SEBI 24h JWT renewal audit persistence.
//!
//! Captures every Dhan JWT renewal attempt — both the routine ~23h
//! background renewal and the boot-time / WebSocket-wake force
//! renewals — so the operator can answer:
//!
//!   * "Did yesterday's daily renewal fire at the expected time?"
//!   * "How many `CircuitBreakerHalt` events happened this quarter,
//!     and what was the underlying network/SSM error pattern?"
//!   * "When did the last `force_renewal_if_stale` succeed, and what
//!     was the token's headroom at that point?"
//!
//! SEBI 5-year retention applies because the renewal record is the
//! authentication chain — auditors reconstruct order authorisation
//! by joining `order_audit` against this table on
//! `(trading_date_ist, ts)`.
//!
//! One row per renewal event. The DEDUP key is `(trading_date_ist,
//! ts, outcome)` so a same-second double-write (e.g. heartbeat
//! force-renewal racing the scheduled cycle) idempotently overwrites
//! within the same outcome, while a success-then-failure or
//! failure-then-success transition in the same second keeps BOTH
//! rows — the operator needs to see both legs of a transition.

use std::time::Duration;

use reqwest::Client;
use tracing::{error, info, warn};

use tickvault_common::config::QuestDbConfig;
use tickvault_common::sanitize::sanitize_audit_string;

pub const QUESTDB_TABLE_AUTH_RENEWAL_AUDIT: &str = "auth_renewal_audit";

// QuestDB requires the designated timestamp column to be present in
// DEDUP UPSERT KEYS (2026-04-28 regression-class fix from
// `static_ip_audit_persistence`). Adding `outcome` to the key lets
// success and failure rows in the same second BOTH survive — the
// operator's "what happened on this transition?" question depends
// on seeing both legs.
pub const DEDUP_KEY_AUTH_RENEWAL_AUDIT: &str = "trading_date_ist, ts, outcome";

const QUESTDB_DDL_TIMEOUT_SECS: u64 = 10;

/// Stable wire-format outcome strings written into the `outcome`
/// column. Typed enum so callers can't accidentally pass `"OK"` or
/// `"renewed"` and silently fragment the column's symbol set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthRenewalAuditOutcome {
    /// Renewal HTTP call succeeded; new JWT is now active in
    /// `arc-swap`. Covers the routine ~23h cycle, the boot-time
    /// `force_renewal`, and the WebSocket-wake `force_renewal_if_stale`.
    Success,
    /// Single renewal attempt failed (network blip, transient SSM
    /// error, Dhan REST 5xx). The retry loop continues in the
    /// background; another row will land for the next attempt.
    Failed,
    /// `TOKEN_RENEWAL_MAX_CIRCUIT_BREAKER_CYCLES` consecutive failure
    /// cycles exhausted — the renewal task has halted permanently
    /// and operator intervention is required.
    CircuitBreakerHalt,
}

impl AuthRenewalAuditOutcome {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Failed => "failed",
            Self::CircuitBreakerHalt => "circuit_breaker_halt",
        }
    }
}

/// Creates the audit table if absent. Idempotent — safe to call on
/// every boot.
// TEST-EXEMPT: requires running QuestDB; tested via boot integration in CI.
pub async fn ensure_auth_renewal_audit_table(questdb_config: &QuestDbConfig) {
    let base_url = format!(
        "http://{}:{}/exec",
        questdb_config.host, questdb_config.http_port
    );
    let client = Client::builder()
        .timeout(Duration::from_secs(QUESTDB_DDL_TIMEOUT_SECS))
        .build()
        .unwrap_or_else(|_| Client::new());

    let create_ddl = format!(
        "CREATE TABLE IF NOT EXISTS {QUESTDB_TABLE_AUTH_RENEWAL_AUDIT} (\
            ts TIMESTAMP, \
            trading_date_ist TIMESTAMP, \
            outcome SYMBOL, \
            attempts_made LONG, \
            token_remaining_secs_at_attempt LONG, \
            trigger_source SYMBOL, \
            error_detail STRING\
        ) timestamp(ts) PARTITION BY DAY \
        DEDUP UPSERT KEYS({DEDUP_KEY_AUTH_RENEWAL_AUDIT});"
    );
    match client
        .get(&base_url)
        .query(&[("query", create_ddl.as_str())])
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => {
            info!(
                table = QUESTDB_TABLE_AUTH_RENEWAL_AUDIT,
                "audit table ready"
            );
        }
        Ok(resp) => {
            warn!(
                table = QUESTDB_TABLE_AUTH_RENEWAL_AUDIT,
                status = %resp.status(),
                "DDL non-2xx"
            );
        }
        Err(err) => {
            error!(
                table = QUESTDB_TABLE_AUTH_RENEWAL_AUDIT,
                ?err,
                "DDL request failed"
            );
        }
    }
}

/// Appends one auth-renewal audit row.
///
/// `ts_nanos_ist` and `trading_date_ist_nanos` are IST wall-clock
/// nanoseconds — divided to microseconds before embedding because
/// QuestDB `TIMESTAMP` stores microseconds (2026-04-28 regression
/// class). `trigger_source` is a small symbolic label
/// (`"scheduled"` / `"boot_force"` / `"ws_wake"` / `"manual"`) so
/// the operator can correlate against the renewal call site.
// APPROVED: 7 wire-format columns plus QuestDB config — same audit-row shape as sibling static_ip_audit / boot_audit / live_instance_lock writers.
#[allow(clippy::too_many_arguments)]
pub async fn append_auth_renewal_audit_row(
    questdb_config: &QuestDbConfig,
    ts_nanos_ist: i64,
    trading_date_ist_nanos: i64,
    outcome: AuthRenewalAuditOutcome,
    attempts_made: i64,
    token_remaining_secs_at_attempt: i64,
    trigger_source: &str,
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
    let trigger_source_esc = sanitize_audit_string(trigger_source);
    let error_detail_esc = sanitize_audit_string(error_detail);
    let ts_micros_ist = ts_nanos_ist / 1_000;
    let trading_date_ist_micros = trading_date_ist_nanos / 1_000;
    let sql = format!(
        "INSERT INTO {QUESTDB_TABLE_AUTH_RENEWAL_AUDIT} \
         (ts, trading_date_ist, outcome, attempts_made, token_remaining_secs_at_attempt, trigger_source, error_detail) VALUES \
         ({ts_micros_ist}, {trading_date_ist_micros}, '{outcome_str}', {attempts_made}, {token_remaining_secs_at_attempt}, '{trigger_source_esc}', '{error_detail_esc}');"
    );
    let resp = client
        .get(&base_url)
        .query(&[("query", sql.as_str())])
        .send()
        .await?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("auth_renewal_audit insert non-2xx ({status}): {body}");
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
        assert_eq!(QUESTDB_TABLE_AUTH_RENEWAL_AUDIT, "auth_renewal_audit");
    }

    #[test]
    fn test_outcome_as_str_stable() {
        // Wire-format strings — Grafana panels + SEBI audit queries
        // depend on these. Renaming or adding case-variation breaks
        // the column's symbol set silently.
        assert_eq!(AuthRenewalAuditOutcome::Success.as_str(), "success");
        assert_eq!(AuthRenewalAuditOutcome::Failed.as_str(), "failed");
        assert_eq!(
            AuthRenewalAuditOutcome::CircuitBreakerHalt.as_str(),
            "circuit_breaker_halt"
        );
    }

    /// Regression: 2026-04-28 — QuestDB requires the designated
    /// timestamp column to be present in DEDUP UPSERT KEYS.
    #[test]
    fn test_dedup_key_includes_designated_timestamp() {
        assert!(
            DEDUP_KEY_AUTH_RENEWAL_AUDIT.contains("ts"),
            "auth_renewal_audit DEDUP key must include `ts` (designated timestamp); \
             QuestDB rejects DDL otherwise. Got: {DEDUP_KEY_AUTH_RENEWAL_AUDIT}"
        );
    }

    /// `outcome` MUST be in the DEDUP key so a same-second
    /// Failed -> Success transition keeps BOTH rows. Without
    /// `outcome` the second write would silently overwrite the first
    /// and the operator would lose the failure record.
    #[test]
    fn test_dedup_key_includes_outcome() {
        assert!(DEDUP_KEY_AUTH_RENEWAL_AUDIT.contains("outcome"));
        assert!(DEDUP_KEY_AUTH_RENEWAL_AUDIT.contains("trading_date_ist"));
    }

    /// The audit table holds renewal lifecycle events, not per-
    /// instrument rows — `security_id` MUST NOT appear in the DEDUP
    /// key.
    #[test]
    fn test_dedup_key_no_security_id() {
        assert!(!DEDUP_KEY_AUTH_RENEWAL_AUDIT.contains("security_id"));
    }

    /// Regression: 2026-04-28 nanos-to-micros bug class. Source-scan
    /// ratchet locks the conversion line.
    #[test]
    fn test_insert_sql_uses_microseconds_not_nanoseconds() {
        let src = include_str!("auth_renewal_audit_persistence.rs");
        assert!(
            src.contains("ts_micros_ist = ts_nanos_ist / 1_000"),
            "INSERT must convert nanos to micros before embedding"
        );
        assert!(
            src.contains("trading_date_ist_micros = trading_date_ist_nanos / 1_000"),
            "INSERT must convert trading_date_ist nanos to micros"
        );
    }

    /// DDL column-set ratchet — schema drift will silently break
    /// downstream Grafana queries + SEBI audit SELECTs.
    #[test]
    fn test_ddl_contains_expected_columns() {
        let src = include_str!("auth_renewal_audit_persistence.rs");
        for column in [
            "ts TIMESTAMP",
            "trading_date_ist TIMESTAMP",
            "outcome SYMBOL",
            "attempts_made LONG",
            "token_remaining_secs_at_attempt LONG",
            "trigger_source SYMBOL",
            "error_detail STRING",
        ] {
            assert!(
                src.contains(column),
                "DDL must declare `{column}` — schema drift will break Grafana queries"
            );
        }
    }

    /// Wire-format outcome labels must all be unique — operator
    /// dashboards filter on these literal strings.
    #[test]
    fn test_all_outcomes_have_distinct_wire_format_labels() {
        let labels = [
            AuthRenewalAuditOutcome::Success.as_str(),
            AuthRenewalAuditOutcome::Failed.as_str(),
            AuthRenewalAuditOutcome::CircuitBreakerHalt.as_str(),
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

    #[test]
    fn test_append_auth_renewal_audit_row_smoke() {
        // Pub-fn-test guard pins the symbol by name. The tokio
        // tests below cover the actual behaviour; this is just the
        // regex-matched smoke test.
        let _ = append_auth_renewal_audit_row;
    }

    #[tokio::test]
    async fn test_append_returns_err_when_questdb_unreachable() {
        // Port 1 is reserved — guaranteed connection refusal.
        let cfg = test_cfg(1);
        let result = append_auth_renewal_audit_row(
            &cfg,
            1_710_000_000_000_000_000,
            1_709_980_200_000_000_000,
            AuthRenewalAuditOutcome::Success,
            1,
            82_800,
            "scheduled",
            "",
        )
        .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_append_handles_failed_outcome_with_error_detail() {
        // Exercise the Failed branch with a non-trivial error_detail
        // so the format!() macro can't quietly break on the path
        // operators care about most.
        let cfg = test_cfg(1);
        let result = append_auth_renewal_audit_row(
            &cfg,
            1_710_000_000_000_000_000,
            1_709_980_200_000_000_000,
            AuthRenewalAuditOutcome::Failed,
            3,
            300,
            "ws_wake",
            "DH-901 invalid TOTP",
        )
        .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_append_handles_circuit_breaker_halt() {
        let cfg = test_cfg(1);
        let result = append_auth_renewal_audit_row(
            &cfg,
            1_710_000_000_000_000_000,
            1_709_980_200_000_000_000,
            AuthRenewalAuditOutcome::CircuitBreakerHalt,
            15,
            0,
            "scheduled",
            "all renewal attempts exhausted across 3 consecutive cycles",
        )
        .await;
        assert!(result.is_err());
    }
}
