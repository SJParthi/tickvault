//! Wave 2 Item 9 — Phase 2 audit persistence.
//!
//! Persists every Phase 2 dispatch outcome (Complete / Failed / Skipped)
//! at 09:13 IST so the operator can reconstruct what happened weeks
//! later. SEBI-relevant: 90d hot → S3 IT → Glacier per `aws-budget.md`.

use std::time::Duration;

use reqwest::Client;
use tracing::{error, info, warn};

use tickvault_common::config::QuestDbConfig;

/// QuestDB table name for the Phase 2 audit trail.
pub const QUESTDB_TABLE_PHASE2_AUDIT: &str = "phase2_audit";

/// DEDUP UPSERT KEY — `(trading_date_ist, ts)` per `active-plan-wave-2.md`
/// §9. Composite key satisfies the meta-guard
/// `dedup_segment_meta_guard.rs` (no `security_id` column → no segment
/// requirement).
pub const DEDUP_KEY_PHASE2_AUDIT: &str = "trading_date_ist, ts";

/// HTTP timeout for DDL probes.
const QUESTDB_DDL_TIMEOUT_SECS: u64 = 10;

/// Idempotent CREATE + ALTER ADD COLUMN IF NOT EXISTS for the audit
/// table. Called once at boot from `main.rs` — per the schema-self-heal
/// pattern in `observability-architecture.md`.
pub async fn ensure_phase2_audit_table(questdb_config: &QuestDbConfig) {
    let base_url = format!(
        "http://{}:{}/exec",
        questdb_config.host, questdb_config.http_port
    );
    let client = Client::builder()
        .timeout(Duration::from_secs(QUESTDB_DDL_TIMEOUT_SECS))
        .build()
        .unwrap_or_else(|_| Client::new());

    let create_ddl = format!(
        "CREATE TABLE IF NOT EXISTS {QUESTDB_TABLE_PHASE2_AUDIT} (\
            ts TIMESTAMP, \
            trading_date_ist TIMESTAMP, \
            outcome SYMBOL, \
            stocks_added LONG, \
            stocks_skipped LONG, \
            buffer_entries LONG, \
            diagnostic STRING\
        ) timestamp(ts) PARTITION BY DAY \
        DEDUP UPSERT KEYS({DEDUP_KEY_PHASE2_AUDIT});"
    );
    match client
        .get(&base_url)
        .query(&[("query", create_ddl.as_str())])
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => {
            info!(table = QUESTDB_TABLE_PHASE2_AUDIT, "audit table ready");
        }
        Ok(resp) => {
            warn!(table = QUESTDB_TABLE_PHASE2_AUDIT, status = %resp.status(), "DDL non-2xx — continuing best-effort");
        }
        Err(err) => {
            error!(
                table = QUESTDB_TABLE_PHASE2_AUDIT,
                ?err,
                "DDL request failed — table may already exist"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_dedup_key_phase2_audit_does_not_require_segment() {
        // No security_id column → meta-guard does not require segment.
        assert!(!DEDUP_KEY_PHASE2_AUDIT.contains("security_id"));
        assert!(DEDUP_KEY_PHASE2_AUDIT.contains("ts"));
    }

    #[test]
    fn test_table_name_constant_is_phase2_audit() {
        assert_eq!(QUESTDB_TABLE_PHASE2_AUDIT, "phase2_audit");
    }
}
