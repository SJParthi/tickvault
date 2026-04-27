//! Wave 2 Item 9 — selftest audit persistence.
//!
//! Persists the outcome of each `make doctor` / `make validate-automation`
//! self-test check so the operator can answer "was the system green
//! at 14:32 IST yesterday".

use std::time::Duration;

use reqwest::Client;
use tracing::{error, info, warn};

use tickvault_common::config::QuestDbConfig;

pub const QUESTDB_TABLE_SELFTEST_AUDIT: &str = "selftest_audit";
pub const DEDUP_KEY_SELFTEST_AUDIT: &str = "trading_date_ist, check_name";

const QUESTDB_DDL_TIMEOUT_SECS: u64 = 10;

// TEST-EXEMPT: requires running QuestDB; tested via boot integration in CI.
pub async fn ensure_selftest_audit_table(questdb_config: &QuestDbConfig) {
    let base_url = format!(
        "http://{}:{}/exec",
        questdb_config.host, questdb_config.http_port
    );
    let client = Client::builder()
        .timeout(Duration::from_secs(QUESTDB_DDL_TIMEOUT_SECS))
        .build()
        .unwrap_or_else(|_| Client::new());

    let create_ddl = format!(
        "CREATE TABLE IF NOT EXISTS {QUESTDB_TABLE_SELFTEST_AUDIT} (\
            ts TIMESTAMP, \
            trading_date_ist TIMESTAMP, \
            check_name SYMBOL, \
            outcome SYMBOL, \
            duration_ms LONG, \
            detail STRING\
        ) timestamp(ts) PARTITION BY DAY \
        DEDUP UPSERT KEYS({DEDUP_KEY_SELFTEST_AUDIT});"
    );
    match client
        .get(&base_url)
        .query(&[("query", create_ddl.as_str())])
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => {
            info!(table = QUESTDB_TABLE_SELFTEST_AUDIT, "audit table ready");
        }
        Ok(resp) => {
            warn!(
                table = QUESTDB_TABLE_SELFTEST_AUDIT,
                status = %resp.status(),
                "DDL non-2xx"
            );
        }
        Err(err) => {
            error!(
                table = QUESTDB_TABLE_SELFTEST_AUDIT,
                ?err,
                "DDL request failed"
            );
        }
    }
}

/// Append one selftest audit row.
pub async fn append_selftest_audit_row(
    questdb_config: &QuestDbConfig,
    ts_nanos_ist: i64,
    trading_date_ist_nanos: i64,
    check_name: &str,
    outcome: &str,
    duration_ms: i64,
    detail: &str,
) -> anyhow::Result<()> {
    let base_url = format!(
        "http://{}:{}/exec",
        questdb_config.host, questdb_config.http_port
    );
    let client = Client::builder()
        .timeout(Duration::from_secs(QUESTDB_DDL_TIMEOUT_SECS))
        .build()?;
    let check_esc = check_name.replace('\'', "''");
    let outcome_esc = outcome.replace('\'', "''");
    let detail_esc = detail.replace('\'', "''");
    let sql = format!(
        "INSERT INTO {QUESTDB_TABLE_SELFTEST_AUDIT} (ts, trading_date_ist, check_name, outcome, duration_ms, detail) VALUES \
         ({ts_nanos_ist}, {trading_date_ist_nanos}, '{check_esc}', '{outcome_esc}', {duration_ms}, '{detail_esc}');"
    );
    let resp = client
        .get(&base_url)
        .query(&[("query", sql.as_str())])
        .send()
        .await?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("selftest audit insert non-2xx ({status}): {body}");
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
        assert!(!DEDUP_KEY_SELFTEST_AUDIT.contains("security_id"));
        assert!(DEDUP_KEY_SELFTEST_AUDIT.contains("check_name"));
    }

    #[test]
    fn test_table_name_constant() {
        assert_eq!(QUESTDB_TABLE_SELFTEST_AUDIT, "selftest_audit");
    }

    #[tokio::test]
    async fn test_append_selftest_audit_row_returns_err_when_questdb_unreachable() {
        let cfg = test_cfg(1);
        let result = append_selftest_audit_row(
            &cfg,
            1_710_000_000_000_000_000,
            1_709_980_200_000_000_000,
            "make-doctor:section-4-questdb",
            "green",
            512,
            "all checks green",
        )
        .await;
        assert!(result.is_err());
    }
}
