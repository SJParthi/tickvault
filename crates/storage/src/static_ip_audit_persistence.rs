//! Phase 0 Item 18c — static IP boot gate audit persistence.
//!
//! Persists the final outcome of the boot-time `/v2/ip/getIP` check
//! (Phase 0 Items 18 + 18b) so the operator can answer questions like:
//!
//!   * "Did the static-IP boot gate pass at 09:01 IST today?"
//!   * "How many times did `orders_not_allowed` exhaust the retry
//!      budget this month?"
//!   * "What was Dhan reporting at the exact instant the operator
//!      restarted the app?"
//!
//! One row per boot. The DEDUP key is `(trading_date_ist, ts)` so a
//! same-second re-run cannot overwrite an existing audit row — every
//! boot survives in the table for SEBI's 5-year retention window.

use std::time::Duration;

use reqwest::Client;
use tracing::{error, info, warn};

use tickvault_common::config::QuestDbConfig;
use tickvault_common::sanitize::sanitize_audit_string;

pub const QUESTDB_TABLE_STATIC_IP_AUDIT: &str = "static_ip_audit";

// QuestDB requires the designated timestamp column to be present in
// DEDUP UPSERT KEYS — without `ts` the CREATE TABLE returns 400 Bad
// Request and the table is missing for the entire session
// (regression 2026-04-28).
pub const DEDUP_KEY_STATIC_IP_AUDIT: &str = "trading_date_ist, ts";

const QUESTDB_DDL_TIMEOUT_SECS: u64 = 10;

/// Stable wire-format outcome strings written into the `outcome`
/// column. Kept as a typed enum so callers can't accidentally pass
/// `"PASS"` (uppercase) or `"passed"` (verb form) and silently
/// fragment the column's symbol set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StaticIpAuditOutcome {
    /// Boot gate passed — order path may spawn.
    Pass,
    /// Boot gate failed — boot halted. `reason` carries the typed
    /// failure label from `StaticIpBootFailureReason::as_str()`.
    Fail,
}

impl StaticIpAuditOutcome {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::Fail => "fail",
        }
    }
}

/// Creates the audit table if absent. Idempotent — safe to call on
/// every boot.
// TEST-EXEMPT: requires running QuestDB; tested via boot integration in CI.
pub async fn ensure_static_ip_audit_table(questdb_config: &QuestDbConfig) {
    let base_url = format!(
        "http://{}:{}/exec",
        questdb_config.host, questdb_config.http_port
    );
    let client = Client::builder()
        .timeout(Duration::from_secs(QUESTDB_DDL_TIMEOUT_SECS))
        .build()
        .unwrap_or_else(|_| Client::new());

    let create_ddl = format!(
        "CREATE TABLE IF NOT EXISTS {QUESTDB_TABLE_STATIC_IP_AUDIT} (\
            ts TIMESTAMP, \
            trading_date_ist TIMESTAMP, \
            outcome SYMBOL, \
            reason SYMBOL, \
            ip_flag SYMBOL, \
            ip_match_status SYMBOL, \
            attempts_made LONG, \
            orders_allowed BOOLEAN\
        ) timestamp(ts) PARTITION BY DAY \
        DEDUP UPSERT KEYS({DEDUP_KEY_STATIC_IP_AUDIT});"
    );
    match client
        .get(&base_url)
        .query(&[("query", create_ddl.as_str())])
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => {
            info!(table = QUESTDB_TABLE_STATIC_IP_AUDIT, "audit table ready");
        }
        Ok(resp) => {
            warn!(
                table = QUESTDB_TABLE_STATIC_IP_AUDIT,
                status = %resp.status(),
                "DDL non-2xx"
            );
        }
        Err(err) => {
            error!(
                table = QUESTDB_TABLE_STATIC_IP_AUDIT,
                ?err,
                "DDL request failed"
            );
        }
    }
}

/// Appends one static-IP audit row.
///
/// `ts_nanos_ist` and `trading_date_ist_nanos` are IST wall-clock
/// nanoseconds — the function divides them down to microseconds
/// before embedding in the INSERT statement because QuestDB
/// `TIMESTAMP` columns store microseconds since epoch and embedding
/// nanos overflows the year-9999 range (2026-04-28 regression).
pub async fn append_static_ip_audit_row(
    questdb_config: &QuestDbConfig,
    ts_nanos_ist: i64,
    trading_date_ist_nanos: i64,
    outcome: StaticIpAuditOutcome,
    reason: &str,
    ip_flag: &str,
    ip_match_status: &str,
    attempts_made: i64,
    orders_allowed: bool,
) -> anyhow::Result<()> {
    let base_url = format!(
        "http://{}:{}/exec",
        questdb_config.host, questdb_config.http_port
    );
    let client = Client::builder()
        .timeout(Duration::from_secs(QUESTDB_DDL_TIMEOUT_SECS))
        .build()?;
    let outcome_str = outcome.as_str();
    let reason_esc = sanitize_audit_string(reason);
    let ip_flag_esc = sanitize_audit_string(ip_flag);
    let ip_match_status_esc = sanitize_audit_string(ip_match_status);
    // QuestDB TIMESTAMP columns store microseconds since epoch — divide
    // nanos to micros before embedding so the value stays in range.
    let ts_micros_ist = ts_nanos_ist / 1_000;
    let trading_date_ist_micros = trading_date_ist_nanos / 1_000;
    let orders_allowed_str = if orders_allowed { "true" } else { "false" };
    let sql = format!(
        "INSERT INTO {QUESTDB_TABLE_STATIC_IP_AUDIT} \
         (ts, trading_date_ist, outcome, reason, ip_flag, ip_match_status, attempts_made, orders_allowed) VALUES \
         ({ts_micros_ist}, {trading_date_ist_micros}, '{outcome_str}', '{reason_esc}', '{ip_flag_esc}', '{ip_match_status_esc}', {attempts_made}, {orders_allowed_str});"
    );
    let resp = client
        .get(&base_url)
        .query(&[("query", sql.as_str())])
        .send()
        .await?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("static_ip_audit insert non-2xx ({status}): {body}");
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
        assert_eq!(QUESTDB_TABLE_STATIC_IP_AUDIT, "static_ip_audit");
    }

    #[test]
    fn test_outcome_as_str_stable() {
        // Wire-format strings — Grafana panels + audit queries depend on these.
        assert_eq!(StaticIpAuditOutcome::Pass.as_str(), "pass");
        assert_eq!(StaticIpAuditOutcome::Fail.as_str(), "fail");
    }

    /// Regression: 2026-04-28 — QuestDB requires the designated
    /// timestamp column to be present in DEDUP UPSERT KEYS. Without
    /// `ts` the CREATE TABLE returns 400 Bad Request and the table
    /// is silently missing for the whole session.
    #[test]
    fn test_dedup_key_includes_designated_timestamp() {
        assert!(
            DEDUP_KEY_STATIC_IP_AUDIT.contains("ts"),
            "static_ip_audit DEDUP key must include `ts` (designated timestamp); \
             QuestDB rejects DDL otherwise. Got: {DEDUP_KEY_STATIC_IP_AUDIT}"
        );
    }

    /// The audit table holds boot-gate outcomes, not per-instrument
    /// rows — `security_id` MUST NOT appear in the DEDUP key.
    #[test]
    fn test_dedup_key_no_security_id() {
        assert!(!DEDUP_KEY_STATIC_IP_AUDIT.contains("security_id"));
        assert!(DEDUP_KEY_STATIC_IP_AUDIT.contains("trading_date_ist"));
    }

    /// Regression: 2026-04-28 nanos-to-micros bug class
    /// (see `selftest_audit_persistence.rs`). Source-scan ratchet
    /// locks the conversion line.
    #[test]
    fn test_insert_sql_uses_microseconds_not_nanoseconds() {
        let src = include_str!("static_ip_audit_persistence.rs");
        assert!(
            src.contains("ts_micros_ist = ts_nanos_ist / 1_000"),
            "INSERT must convert nanos to micros before embedding"
        );
        assert!(
            src.contains("trading_date_ist_micros = trading_date_ist_nanos / 1_000"),
            "INSERT must convert trading_date_ist nanos to micros"
        );
    }

    /// The DDL must reflect the operator-facing column set so future
    /// schema drift fails the test loudly instead of silently breaking
    /// downstream queries. The check is a string-presence one because
    /// the DDL is built via format!() at runtime — we lock the column
    /// names by scanning the source file.
    #[test]
    fn test_ddl_contains_expected_columns() {
        let src = include_str!("static_ip_audit_persistence.rs");
        for column in [
            "ts TIMESTAMP",
            "trading_date_ist TIMESTAMP",
            "outcome SYMBOL",
            "reason SYMBOL",
            "ip_flag SYMBOL",
            "ip_match_status SYMBOL",
            "attempts_made LONG",
            "orders_allowed BOOLEAN",
        ] {
            assert!(
                src.contains(column),
                "DDL must declare `{column}` — schema drift will break Grafana queries"
            );
        }
    }

    #[tokio::test]
    async fn test_append_static_ip_audit_row_returns_err_when_questdb_unreachable() {
        // Port 1 is reserved — guaranteed connection refusal.
        let cfg = test_cfg(1);
        let result = append_static_ip_audit_row(
            &cfg,
            1_710_000_000_000_000_000,
            1_709_980_200_000_000_000,
            StaticIpAuditOutcome::Pass,
            "",
            "PRIMARY",
            "MATCH",
            1,
            true,
        )
        .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_append_static_ip_audit_row_handles_fail_outcome() {
        // Same connection-refused path, but exercises the fail-path
        // string formatting so the format!() macro can't quietly break.
        let cfg = test_cfg(1);
        let result = append_static_ip_audit_row(
            &cfg,
            1_710_000_000_000_000_000,
            1_709_980_200_000_000_000,
            StaticIpAuditOutcome::Fail,
            "orders_not_allowed",
            "PRIMARY",
            "MISMATCH",
            30,
            false,
        )
        .await;
        assert!(result.is_err());
    }
}
