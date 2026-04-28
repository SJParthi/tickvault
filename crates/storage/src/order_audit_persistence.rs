//! Wave 2 Item 9 — order audit persistence.
//!
//! Every order lifecycle event (Place / Modify / Cancel / Fill / Reject)
//! is persisted with full context. **SEBI 5-year retention applies** —
//! S3 lifecycle policy must keep order rows for 1825 days minimum.

use std::time::Duration;

use reqwest::Client;
use tracing::{error, info, warn};

use tickvault_common::config::QuestDbConfig;
use tickvault_common::sanitize::sanitize_audit_string;

pub const QUESTDB_TABLE_ORDER_AUDIT: &str = "order_audit";
pub const DEDUP_KEY_ORDER_AUDIT: &str = "order_id, ts, leg";

const QUESTDB_DDL_TIMEOUT_SECS: u64 = 10;

// TEST-EXEMPT: requires running QuestDB; tested via boot integration in CI.
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

/// Append one order-lifecycle audit row. SEBI 5y retention applies.
#[allow(clippy::too_many_arguments)] // APPROVED: order audit row schema requires every column
pub async fn append_order_audit_row(
    questdb_config: &QuestDbConfig,
    ts_nanos_ist: i64,
    order_id: &str,
    correlation_id: &str,
    leg: &str,
    event: &str,
    security_id: i32,
    segment: &str,
    transaction_type: &str,
    quantity: i64,
    price: f64,
    order_status: &str,
    outcome: &str,
    detail: &str,
) -> anyhow::Result<()> {
    let base_url = format!(
        "http://{}:{}/exec",
        questdb_config.host, questdb_config.http_port
    );
    let client = Client::builder()
        .timeout(Duration::from_secs(QUESTDB_DDL_TIMEOUT_SECS))
        .build()?;
    let order_id_esc = sanitize_audit_string(order_id);
    let corr_esc = sanitize_audit_string(correlation_id);
    let leg_esc = sanitize_audit_string(leg);
    let event_esc = sanitize_audit_string(event);
    let seg_esc = sanitize_audit_string(segment);
    let txn_esc = sanitize_audit_string(transaction_type);
    let status_esc = sanitize_audit_string(order_status);
    let outcome_esc = sanitize_audit_string(outcome);
    let detail_esc = sanitize_audit_string(detail);
    // Wave-2-D adversarial review (HIGH) — `price` is `f64`; a malformed
    // Dhan response could produce NaN/Infinity. QuestDB rejects both,
    // which would fail every subsequent audit insert for that order.
    // Clamp to a finite fallback (0.0) and log; the audit row still
    // captures the order with the lifecycle event, just without a
    // bogus price.
    let safe_price: f64 = if price.is_finite() {
        price
    } else {
        tracing::warn!(
            order_id = %order_id_esc,
            raw_price = price,
            "order_audit: non-finite price clamped to 0.0 to avoid QuestDB reject"
        );
        0.0
    };
    let sql = format!(
        "INSERT INTO {QUESTDB_TABLE_ORDER_AUDIT} (ts, order_id, correlation_id, leg, event, security_id, segment, transaction_type, quantity, price, order_status, outcome, detail) VALUES \
         ({ts_nanos_ist}, '{order_id_esc}', '{corr_esc}', '{leg_esc}', '{event_esc}', {security_id}, '{seg_esc}', '{txn_esc}', {quantity}, {safe_price}, '{status_esc}', '{outcome_esc}', '{detail_esc}');"
    );
    let resp = client
        .get(&base_url)
        .query(&[("query", sql.as_str())])
        .send()
        .await?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("order audit insert non-2xx ({status}): {body}");
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

    #[tokio::test]
    async fn test_append_order_audit_row_returns_err_when_questdb_unreachable() {
        let cfg = test_cfg(1);
        let result = append_order_audit_row(
            &cfg,
            1_710_000_000_000_000_000,
            "ORD-2026-001",
            "corr-001",
            "ENTRY_LEG",
            "place",
            123_456,
            "NSE_FNO",
            "BUY",
            75,
            247.5,
            "PENDING",
            "ok",
            "limit order placed",
        )
        .await;
        assert!(result.is_err());
    }
}
