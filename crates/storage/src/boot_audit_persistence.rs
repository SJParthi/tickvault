//! Wave 2 Item 9 — boot audit persistence.
//!
//! Every boot step emits one row so the operator can reconstruct the
//! exact boot timeline. Useful for "why did boot take 47s today vs 12s
//! yesterday" forensics.

use std::time::Duration;

use reqwest::Client;
use tracing::{error, info, warn};

use tickvault_common::config::QuestDbConfig;

pub const QUESTDB_TABLE_BOOT_AUDIT: &str = "boot_audit";
pub const DEDUP_KEY_BOOT_AUDIT: &str = "boot_id, step";

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_dedup_key_no_security_id() {
        assert!(!DEDUP_KEY_BOOT_AUDIT.contains("security_id"));
        assert!(DEDUP_KEY_BOOT_AUDIT.contains("boot_id"));
        assert!(DEDUP_KEY_BOOT_AUDIT.contains("step"));
    }

    #[test]
    fn test_table_name_constant() {
        assert_eq!(QUESTDB_TABLE_BOOT_AUDIT, "boot_audit");
    }
}
