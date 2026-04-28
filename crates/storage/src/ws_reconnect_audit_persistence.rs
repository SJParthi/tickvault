//! Wave 2 Item 9 — WS reconnect audit persistence.
//!
//! Every WebSocket reconnect (main, depth-20, depth-200, order-update)
//! emits one row so the operator can reconstruct connection churn.

use std::time::Duration;

use reqwest::Client;
use tracing::{error, info, warn};

use tickvault_common::config::QuestDbConfig;
use tickvault_common::sanitize::sanitize_audit_string;

pub const QUESTDB_TABLE_WS_RECONNECT_AUDIT: &str = "ws_reconnect_audit";
pub const DEDUP_KEY_WS_RECONNECT_AUDIT: &str = "connection_id, ts";

const QUESTDB_DDL_TIMEOUT_SECS: u64 = 10;

// TEST-EXEMPT: requires running QuestDB; tested via boot integration in CI.
pub async fn ensure_ws_reconnect_audit_table(questdb_config: &QuestDbConfig) {
    let base_url = format!(
        "http://{}:{}/exec",
        questdb_config.host, questdb_config.http_port
    );
    let client = Client::builder()
        .timeout(Duration::from_secs(QUESTDB_DDL_TIMEOUT_SECS))
        .build()
        .unwrap_or_else(|_| Client::new());

    let create_ddl = format!(
        "CREATE TABLE IF NOT EXISTS {QUESTDB_TABLE_WS_RECONNECT_AUDIT} (\
            ts TIMESTAMP, \
            feed SYMBOL, \
            connection_id INT, \
            attempt LONG, \
            outcome SYMBOL, \
            disconnect_code SHORT, \
            reason STRING\
        ) timestamp(ts) PARTITION BY DAY \
        DEDUP UPSERT KEYS({DEDUP_KEY_WS_RECONNECT_AUDIT});"
    );
    match client
        .get(&base_url)
        .query(&[("query", create_ddl.as_str())])
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => {
            info!(
                table = QUESTDB_TABLE_WS_RECONNECT_AUDIT,
                "audit table ready"
            );
        }
        Ok(resp) => {
            warn!(
                table = QUESTDB_TABLE_WS_RECONNECT_AUDIT,
                status = %resp.status(),
                "DDL non-2xx"
            );
        }
        Err(err) => {
            error!(
                table = QUESTDB_TABLE_WS_RECONNECT_AUDIT,
                ?err,
                "DDL request failed"
            );
        }
    }
}

/// Append one WS-reconnect audit row.
///
/// `feed` ∈ {"main", "depth20", "depth200", "order_update"};
/// `outcome` ∈ {"success", "failed", "exhausted"}.
#[allow(clippy::too_many_arguments)] // APPROVED: audit row schema requires every column
pub async fn append_ws_reconnect_audit_row(
    questdb_config: &QuestDbConfig,
    ts_nanos_ist: i64,
    feed: &str,
    connection_id: i32,
    attempt: i64,
    outcome: &str,
    disconnect_code: Option<i16>,
    reason: &str,
) -> anyhow::Result<()> {
    let base_url = format!(
        "http://{}:{}/exec",
        questdb_config.host, questdb_config.http_port
    );
    let client = Client::builder()
        .timeout(Duration::from_secs(QUESTDB_DDL_TIMEOUT_SECS))
        .build()?;
    let feed_esc = sanitize_audit_string(feed);
    let outcome_esc = sanitize_audit_string(outcome);
    let reason_esc = sanitize_audit_string(reason);
    // SECURITY (Wave-2-D adversarial review MEDIUM): disconnect_code
    // is written verbatim as an i16. We DELIBERATELY do not reject
    // codes outside the known Dhan enum (800..814) — SEBI audit
    // completeness requires recording what actually happened on the
    // wire, including unknown future codes. The column type is
    // SHORT (signed i16), which bounds the wire format itself.
    // The `reason` free-text column carries any human-readable
    // context; that one IS sanitized via `sanitize_audit_string` to
    // strip control chars and Unicode bidi-overrides per the same
    // adversarial-review finding.
    let dc = disconnect_code
        .map(|v| v.to_string())
        .unwrap_or_else(|| "NULL".to_string());
    let sql = format!(
        "INSERT INTO {QUESTDB_TABLE_WS_RECONNECT_AUDIT} (ts, feed, connection_id, attempt, outcome, disconnect_code, reason) VALUES \
         ({ts_nanos_ist}, '{feed_esc}', {connection_id}, {attempt}, '{outcome_esc}', {dc}, '{reason_esc}');"
    );
    let resp = client
        .get(&base_url)
        .query(&[("query", sql.as_str())])
        .send()
        .await?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("ws_reconnect audit insert non-2xx ({status}): {body}");
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
        assert!(!DEDUP_KEY_WS_RECONNECT_AUDIT.contains("security_id"));
        assert!(DEDUP_KEY_WS_RECONNECT_AUDIT.contains("connection_id"));
    }

    #[test]
    fn test_table_name_constant() {
        assert_eq!(QUESTDB_TABLE_WS_RECONNECT_AUDIT, "ws_reconnect_audit");
    }

    #[tokio::test]
    async fn test_append_ws_reconnect_audit_row_returns_err_when_questdb_unreachable() {
        let cfg = test_cfg(1);
        let result = append_ws_reconnect_audit_row(
            &cfg,
            1_710_000_000_000_000_000,
            "main",
            0,
            5,
            "success",
            None,
            "TCP RST recovered",
        )
        .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_append_ws_reconnect_audit_row_handles_optional_disconnect_code() {
        let cfg = test_cfg(1);
        let _ = append_ws_reconnect_audit_row(
            &cfg,
            0,
            "main",
            0,
            1,
            "failed",
            Some(807),
            "DataAPI-807 token expired",
        )
        .await;
    }
}
