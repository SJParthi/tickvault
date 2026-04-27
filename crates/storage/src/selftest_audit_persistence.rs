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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_dedup_key_no_security_id() {
        assert!(!DEDUP_KEY_SELFTEST_AUDIT.contains("security_id"));
        assert!(DEDUP_KEY_SELFTEST_AUDIT.contains("check_name"));
    }

    #[test]
    fn test_table_name_constant() {
        assert_eq!(QUESTDB_TABLE_SELFTEST_AUDIT, "selftest_audit");
    }
}
