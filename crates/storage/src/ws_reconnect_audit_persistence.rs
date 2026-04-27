//! Wave 2 Item 9 — WS reconnect audit persistence.
//!
//! Every WebSocket reconnect (main, depth-20, depth-200, order-update)
//! emits one row so the operator can reconstruct connection churn.

use std::time::Duration;

use reqwest::Client;
use tracing::{error, info, warn};

use tickvault_common::config::QuestDbConfig;

pub const QUESTDB_TABLE_WS_RECONNECT_AUDIT: &str = "ws_reconnect_audit";
pub const DEDUP_KEY_WS_RECONNECT_AUDIT: &str = "connection_id, ts";

const QUESTDB_DDL_TIMEOUT_SECS: u64 = 10;

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_dedup_key_no_security_id() {
        assert!(!DEDUP_KEY_WS_RECONNECT_AUDIT.contains("security_id"));
        assert!(DEDUP_KEY_WS_RECONNECT_AUDIT.contains("connection_id"));
    }

    #[test]
    fn test_table_name_constant() {
        assert_eq!(QUESTDB_TABLE_WS_RECONNECT_AUDIT, "ws_reconnect_audit");
    }
}
