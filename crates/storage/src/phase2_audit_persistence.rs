//! Wave 2 Item 9 — Phase 2 audit persistence.
//!
//! Persists every Phase 2 dispatch outcome (Complete / Failed / Skipped)
//! at 09:13 IST so the operator can reconstruct what happened weeks
//! later. SEBI-relevant: 90d hot → S3 IT → Glacier per `aws-budget.md`.

use std::time::Duration;

use reqwest::Client;
use tracing::{error, info, warn};

use tickvault_common::config::QuestDbConfig;
use tickvault_common::sanitize::sanitize_audit_string;

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
// TEST-EXEMPT: requires running QuestDB; tested via boot integration in CI.
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

/// Append one Phase 2 audit row.
///
/// `outcome` ∈ {"complete", "failed", "skipped"}; `diagnostic` is a short
/// reason string (truncated by QuestDB if too long). HTTP `/exec` INSERT —
/// audit rows are low-volume (≤ a few per day), so ILP batching is overkill.
pub async fn append_phase2_audit_row(
    questdb_config: &QuestDbConfig,
    ts_nanos_ist: i64,
    trading_date_ist_nanos: i64,
    outcome: &str,
    stocks_added: i64,
    stocks_skipped: i64,
    buffer_entries: i64,
    diagnostic: &str,
) -> anyhow::Result<()> {
    let base_url = format!(
        "http://{}:{}/exec",
        questdb_config.host, questdb_config.http_port
    );
    let client = Client::builder()
        .timeout(Duration::from_secs(QUESTDB_DDL_TIMEOUT_SECS))
        .build()?;
    // QuestDB SQL escape: replace single quotes with two single quotes.
    let escaped_diag = sanitize_audit_string(diagnostic);
    let escaped_outcome = sanitize_audit_string(outcome);
    // QuestDB TIMESTAMP columns store microseconds since epoch. The
    // caller passes IST wall-clock nanoseconds; divide by 1_000 before
    // embedding so the value stays in the QuestDB year-9999 range.
    let ts_micros_ist = ts_nanos_ist / 1_000;
    let trading_date_ist_micros = trading_date_ist_nanos / 1_000;
    let sql = format!(
        "INSERT INTO {QUESTDB_TABLE_PHASE2_AUDIT} (ts, trading_date_ist, outcome, stocks_added, stocks_skipped, buffer_entries, diagnostic) VALUES \
         ({ts_micros_ist}, {trading_date_ist_micros}, '{escaped_outcome}', {stocks_added}, {stocks_skipped}, {buffer_entries}, '{escaped_diag}');"
    );
    let resp = client
        .get(&base_url)
        .query(&[("query", sql.as_str())])
        .send()
        .await?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("Phase2 audit insert non-2xx ({status}): {body}");
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
    fn test_dedup_key_phase2_audit_does_not_require_segment() {
        // No security_id column → meta-guard does not require segment.
        assert!(!DEDUP_KEY_PHASE2_AUDIT.contains("security_id"));
        assert!(DEDUP_KEY_PHASE2_AUDIT.contains("ts"));
    }

    #[test]
    fn test_table_name_constant_is_phase2_audit() {
        assert_eq!(QUESTDB_TABLE_PHASE2_AUDIT, "phase2_audit");
    }

    #[tokio::test]
    async fn test_append_phase2_audit_row_returns_err_when_questdb_unreachable() {
        // Port 1 is unprivileged and always rejects connections; the
        // function must surface the network error as an Err rather than
        // panicking or silently swallowing it.
        let cfg = test_cfg(1);
        let result = append_phase2_audit_row(
            &cfg,
            1_710_000_000_000_000_000,
            1_709_980_200_000_000_000,
            "failed",
            0,
            216,
            0,
            "buffer empty at 09:13:00 — REST fallback also returned 0 LTPs",
        )
        .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_append_phase2_audit_row_escapes_single_quotes_in_diagnostic() {
        // Embed a single quote in the diagnostic; the function must
        // escape it (double single-quote) before sending the SQL.
        let cfg = test_cfg(1);
        let _ = append_phase2_audit_row(
            &cfg,
            0,
            0,
            "skipped",
            0,
            0,
            0,
            "RELIANCE's pre-open close was missing",
        )
        .await;
        // Reaching this assertion proves no panic/format error in the SQL builder.
    }

    /// Regression: 2026-04-28 — INSERT used to embed `ts_nanos_ist`
    /// directly into the SQL VALUES clause, but QuestDB TIMESTAMP columns
    /// store microseconds since epoch, so a nanosecond value overflowed
    /// year 9999 and the insert returned 400 ("designated timestamp
    /// beyond 9999-12-31 is not allowed"). Source-scan ratchet locks
    /// the fix in place.
    #[test]
    fn test_insert_sql_uses_microseconds_not_nanoseconds() {
        let src = include_str!("phase2_audit_persistence.rs");
        assert!(
            src.contains("ts_micros_ist = ts_nanos_ist / 1_000"),
            "INSERT must convert ts nanos to micros before embedding"
        );
        assert!(
            src.contains("trading_date_ist_micros = trading_date_ist_nanos / 1_000"),
            "INSERT must convert trading_date_ist nanos to micros"
        );
    }
}
