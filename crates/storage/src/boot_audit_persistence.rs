//! Wave 2 Item 9 — boot audit persistence.
//!
//! Every boot step emits one row so the operator can reconstruct the
//! exact boot timeline. Useful for "why did boot take 47s today vs 12s
//! yesterday" forensics.

use std::time::Duration;

use reqwest::Client;
use tracing::{error, info, warn};

use tickvault_common::config::QuestDbConfig;
use tickvault_common::sanitize::sanitize_audit_string;

pub const QUESTDB_TABLE_BOOT_AUDIT: &str = "boot_audit";
// QuestDB requires the designated timestamp column to be present in
// DEDUP UPSERT KEYS — without `ts` the CREATE TABLE returned 400 Bad Request
// on 2026-04-28 boot, leaving the table missing for the entire session.
pub const DEDUP_KEY_BOOT_AUDIT: &str = "boot_id, step, ts";

const QUESTDB_DDL_TIMEOUT_SECS: u64 = 10;

// TEST-EXEMPT: requires running QuestDB; tested via boot integration in CI.
pub async fn ensure_boot_audit_table(questdb_config: &QuestDbConfig) {
    let base_url = format!(
        "http://{}:{}/exec",
        questdb_config.host, questdb_config.http_port
    );
    let client = Client::builder()
        .timeout(Duration::from_secs(QUESTDB_DDL_TIMEOUT_SECS))
        .build()
        .unwrap_or_else(|_| Client::new());

    let create_ddl = format!(
        "CREATE TABLE IF NOT EXISTS {QUESTDB_TABLE_BOOT_AUDIT} (\
            ts TIMESTAMP, \
            boot_id SYMBOL, \
            step SYMBOL, \
            outcome SYMBOL, \
            elapsed_ms LONG, \
            detail STRING\
        ) timestamp(ts) PARTITION BY DAY \
        DEDUP UPSERT KEYS({DEDUP_KEY_BOOT_AUDIT});"
    );
    match client
        .get(&base_url)
        .query(&[("query", create_ddl.as_str())])
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => {
            info!(table = QUESTDB_TABLE_BOOT_AUDIT, "audit table ready");
        }
        Ok(resp) => {
            warn!(
                table = QUESTDB_TABLE_BOOT_AUDIT,
                status = %resp.status(),
                "DDL non-2xx"
            );
        }
        Err(err) => {
            error!(table = QUESTDB_TABLE_BOOT_AUDIT, ?err, "DDL request failed");
        }
    }
}

/// Append one boot-step audit row.
pub async fn append_boot_audit_row(
    questdb_config: &QuestDbConfig,
    ts_nanos_ist: i64,
    boot_id: &str,
    step: &str,
    outcome: &str,
    elapsed_ms: i64,
    detail: &str,
) -> anyhow::Result<()> {
    let base_url = format!(
        "http://{}:{}/exec",
        questdb_config.host, questdb_config.http_port
    );
    let client = Client::builder()
        .timeout(Duration::from_secs(QUESTDB_DDL_TIMEOUT_SECS))
        .build()?;
    let boot_id_esc = sanitize_audit_string(boot_id);
    let step_esc = sanitize_audit_string(step);
    let outcome_esc = sanitize_audit_string(outcome);
    let detail_esc = sanitize_audit_string(detail);
    // QuestDB TIMESTAMP columns store microseconds since epoch. The
    // caller passes IST wall-clock nanoseconds; divide by 1_000 before
    // embedding so the value stays in the QuestDB year-9999 range.
    let ts_micros_ist = ts_nanos_ist / 1_000;
    let sql = format!(
        "INSERT INTO {QUESTDB_TABLE_BOOT_AUDIT} (ts, boot_id, step, outcome, elapsed_ms, detail) VALUES \
         ({ts_micros_ist}, '{boot_id_esc}', '{step_esc}', '{outcome_esc}', {elapsed_ms}, '{detail_esc}');"
    );
    let resp = client
        .get(&base_url)
        .query(&[("query", sql.as_str())])
        .send()
        .await?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("boot audit insert non-2xx ({status}): {body}");
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
    fn test_dedup_key_no_security_id() {
        assert!(!DEDUP_KEY_BOOT_AUDIT.contains("security_id"));
        assert!(DEDUP_KEY_BOOT_AUDIT.contains("boot_id"));
        assert!(DEDUP_KEY_BOOT_AUDIT.contains("step"));
    }

    /// Regression: 2026-04-28 — QuestDB requires the designated timestamp
    /// column to be present in DEDUP UPSERT KEYS. Without `ts` the CREATE
    /// TABLE returned 400 Bad Request and the table was missing for the
    /// entire session, breaking every subsequent `boot_audit` insert.
    #[test]
    fn test_dedup_key_includes_designated_timestamp() {
        assert!(
            DEDUP_KEY_BOOT_AUDIT.contains("ts"),
            "boot_audit DEDUP key must include `ts` (designated timestamp); \
             QuestDB rejects DDL otherwise. Got: {DEDUP_KEY_BOOT_AUDIT}"
        );
    }

    /// Regression: 2026-04-28 — the function used to embed `ts_nanos_ist`
    /// directly into the SQL VALUES clause, but QuestDB TIMESTAMP columns
    /// store microseconds since epoch, so a nanosecond value overflowed
    /// year 9999 and the insert returned 400. The fix divides by 1_000.
    /// This source-scan ratchet locks the conversion in place.
    #[test]
    fn test_insert_sql_uses_microseconds_not_nanoseconds() {
        let src = include_str!("boot_audit_persistence.rs");
        assert!(
            src.contains("ts_micros_ist = ts_nanos_ist / 1_000"),
            "INSERT must convert nanos to micros before embedding"
        );
        assert!(
            !src.contains("VALUES \\\n         ({ts_nanos_ist}"),
            "INSERT must NOT embed raw nanoseconds in TIMESTAMP column"
        );
    }

    #[test]
    fn test_table_name_constant() {
        assert_eq!(QUESTDB_TABLE_BOOT_AUDIT, "boot_audit");
    }

    #[tokio::test]
    async fn test_append_boot_audit_row_returns_err_when_questdb_unreachable() {
        let cfg = test_cfg(1);
        let result = append_boot_audit_row(
            &cfg,
            1_710_000_000_000_000_000,
            "boot-2026-04-27-1426",
            "questdb_ready",
            "ok",
            12_345,
            "QuestDB ready in 12.3s",
        )
        .await;
        assert!(result.is_err());
    }
}
