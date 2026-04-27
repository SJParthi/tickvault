//! Wave 2 Item 9 — order audit persistence.
//!
//! Every order lifecycle event (Place / Modify / Cancel / Fill / Reject)
//! is persisted with full context. **SEBI 5-year retention applies** —
//! S3 lifecycle policy must keep order rows for 1825 days minimum.

use std::time::Duration;

use reqwest::Client;
use tracing::{error, info, warn};

use tickvault_common::config::QuestDbConfig;

pub const QUESTDB_TABLE_ORDER_AUDIT: &str = "order_audit";
pub const DEDUP_KEY_ORDER_AUDIT: &str = "order_id, ts, leg";

const QUESTDB_DDL_TIMEOUT_SECS: u64 = 10;

pub async fn ensure_order_audit_table(questdb_config: &QuestDbConfig) {
    let base_url = format!(
        "http://{}:{}/exec",
        questdb_config.host, questdb_config.http_port
    );
    let client = Client::builder()
        .timeout(Duration::from_secs(QUESTDB_DDL_TIMEOUT_SECS))
        .build()
        .unwrap_or_else(|_| Client::new());

    let create_ddl = format!(
        "CREATE TABLE IF NOT EXISTS {QUESTDB_TABLE_ORDER_AUDIT} (\
            ts TIMESTAMP, \
            order_id SYMBOL, \
            correlation_id SYMBOL, \
            leg SYMBOL, \
            event SYMBOL, \
            security_id INT, \
            segment SYMBOL, \
            transaction_type SYMBOL, \
            quantity LONG, \
            price DOUBLE, \
            order_status SYMBOL, \
            outcome SYMBOL, \
            detail STRING\
        ) timestamp(ts) PARTITION BY DAY \
        DEDUP UPSERT KEYS({DEDUP_KEY_ORDER_AUDIT});"
    );
    match client
        .get(&base_url)
        .query(&[("query", create_ddl.as_str())])
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => {
            info!(
                table = QUESTDB_TABLE_ORDER_AUDIT,
                "audit table ready (SEBI 5y retention)"
            );
        }
        Ok(resp) => {
            warn!(
                table = QUESTDB_TABLE_ORDER_AUDIT,
                status = %resp.status(),
                "DDL non-2xx"
            );
        }
        Err(err) => {
            error!(
                table = QUESTDB_TABLE_ORDER_AUDIT,
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
    fn test_dedup_key_uses_order_id_not_security_id() {
        // Order rows are per-order, not per-instrument; the natural
        // identity is `(order_id, ts, leg)`. `security_id` is in the
        // schema for join-friendliness but NOT in the DEDUP key — so
        // the meta-guard's "if security_id in key, segment must be too"
        // rule does not apply.
        assert!(DEDUP_KEY_ORDER_AUDIT.contains("order_id"));
        assert!(DEDUP_KEY_ORDER_AUDIT.contains("leg"));
        assert!(!DEDUP_KEY_ORDER_AUDIT.contains("security_id"));
    }

    #[test]
    fn test_table_name_constant() {
        assert_eq!(QUESTDB_TABLE_ORDER_AUDIT, "order_audit");
    }
}
