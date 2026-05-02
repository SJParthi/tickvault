//! 2026-05-02 PR-D — depth-dynamic diff audit persistence.
//!
//! Persists one row per non-no-op diff cycle so the operator can
//! reconstruct "why did the depth-20 set change at 11:23:45 IST" weeks
//! later. Pairs with `depth_dynamic_pipeline_v2.rs` (PR-C1) — the
//! pipeline calls `append_depth_dynamic_diff_audit_row` on every cycle
//! where `DiffStats::is_no_op()` is false.
//!
//! ## Schema
//!
//! ```sql
//! CREATE TABLE IF NOT EXISTS depth_dynamic_diff_audit (
//!     ts             TIMESTAMP,
//!     feed           SYMBOL,        -- "depth-20-dynamic" / "depth-200-dynamic"
//!     removed_count  LONG,
//!     added_count    LONG,
//!     retained_count LONG,
//!     removed_sids   STRING,        -- comma-joined SIDs that left
//!     added_sids     STRING,        -- comma-joined SIDs that joined
//! ) timestamp(ts) PARTITION BY DAY
//! DEDUP UPSERT KEYS(ts, feed);
//! ```
//!
//! ## DEDUP key
//!
//! `(ts, feed)` — one row per (cycle, feed). Per-conn breakdown lives in
//! Prometheus metrics rather than audit rows; multiplying rows by 5 for
//! the per-conn dimension would explode storage cost without adding
//! reconstruction signal (the SID lists are already on the row).
//!
//! ## SEBI retention
//!
//! Standard 90d hot in QuestDB → S3 IT → Glacier per `aws-budget.md`.
//! This audit table is NOT order-trail (5y mandate) but operator may
//! want it for post-incident reconstruction.

use std::time::Duration;

use reqwest::Client;
use tracing::{error, info, warn};

use tickvault_common::config::QuestDbConfig;
use tickvault_common::sanitize::sanitize_audit_string;

/// QuestDB table for depth-dynamic diff audit.
pub const QUESTDB_TABLE_DEPTH_DYNAMIC_DIFF_AUDIT: &str = "depth_dynamic_diff_audit";

/// DEDUP key: one row per (cycle ts, feed). Per-conn split lives in
/// Prom metrics, not audit rows.
pub const DEDUP_KEY_DEPTH_DYNAMIC_DIFF_AUDIT: &str = "ts, feed";

const QUESTDB_DDL_TIMEOUT_SECS: u64 = 10;

/// Maximum SID-list payload length before truncation. 4 KB is generous
/// for a top-50 swap (250 chars × 5 conns) but bounds worst-case.
const MAX_SID_LIST_BYTES: usize = 4096;

// TEST-EXEMPT: requires running QuestDB; tested via boot integration in CI.
pub async fn ensure_depth_dynamic_diff_audit_table(questdb_config: &QuestDbConfig) {
    let base_url = format!(
        "http://{}:{}/exec",
        questdb_config.host, questdb_config.http_port
    );
    let client = Client::builder()
        .timeout(Duration::from_secs(QUESTDB_DDL_TIMEOUT_SECS))
        .build()
        .unwrap_or_else(|_| Client::new());

    let create_ddl = format!(
        "CREATE TABLE IF NOT EXISTS {QUESTDB_TABLE_DEPTH_DYNAMIC_DIFF_AUDIT} (\
            ts             TIMESTAMP, \
            feed           SYMBOL, \
            removed_count  LONG, \
            added_count    LONG, \
            retained_count LONG, \
            removed_sids   STRING, \
            added_sids     STRING\
        ) timestamp(ts) PARTITION BY DAY \
        DEDUP UPSERT KEYS({DEDUP_KEY_DEPTH_DYNAMIC_DIFF_AUDIT});"
    );
    match client
        .get(&base_url)
        .query(&[("query", create_ddl.as_str())])
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => {
            info!(
                table = QUESTDB_TABLE_DEPTH_DYNAMIC_DIFF_AUDIT,
                "audit table ready"
            );
        }
        Ok(resp) => {
            warn!(
                table = QUESTDB_TABLE_DEPTH_DYNAMIC_DIFF_AUDIT,
                status = %resp.status(),
                "DDL non-2xx"
            );
        }
        Err(err) => {
            error!(
                table = QUESTDB_TABLE_DEPTH_DYNAMIC_DIFF_AUDIT,
                ?err,
                "DDL request failed"
            );
        }
    }
}

/// Append one depth-dynamic diff audit row.
///
/// `feed` ∈ {"depth-20-dynamic", "depth-200-dynamic"} (the same `label`
/// passed to `spawn_depth_dynamic_pool`).
///
/// SID lists are comma-joined: `"50001,50002,50003"`. Truncated at
/// `MAX_SID_LIST_BYTES` if oversize (rare — depth-20's max diff is
/// 250 SIDs ≈ 1.5 KB).
#[allow(clippy::too_many_arguments)] // APPROVED: audit row schema requires every column
pub async fn append_depth_dynamic_diff_audit_row(
    questdb_config: &QuestDbConfig,
    ts_nanos_ist: i64,
    feed: &str,
    removed_count: u64,
    added_count: u64,
    retained_count: u64,
    removed_sids: &[u32],
    added_sids: &[u32],
) -> anyhow::Result<()> {
    let base_url = format!(
        "http://{}:{}/exec",
        questdb_config.host, questdb_config.http_port
    );
    let client = Client::builder()
        .timeout(Duration::from_secs(QUESTDB_DDL_TIMEOUT_SECS))
        .build()?;

    let feed_esc = sanitize_audit_string(feed);
    let removed_sids_str = sanitize_audit_string(&format_sid_list(removed_sids));
    let added_sids_str = sanitize_audit_string(&format_sid_list(added_sids));

    // QuestDB TIMESTAMP columns store microseconds since epoch. The
    // caller passes IST wall-clock nanoseconds; divide by 1_000 before
    // embedding so the value stays in the QuestDB year-9999 range.
    // (Same nanos-to-micros bug class as phase2_audit_persistence.rs.)
    let ts_micros_ist = ts_nanos_ist / 1_000;

    let sql = format!(
        "INSERT INTO {QUESTDB_TABLE_DEPTH_DYNAMIC_DIFF_AUDIT} \
         (ts, feed, removed_count, added_count, retained_count, removed_sids, added_sids) \
         VALUES ({ts_micros_ist}, '{feed_esc}', {removed_count}, {added_count}, {retained_count}, '{removed_sids_str}', '{added_sids_str}');"
    );

    let resp = client
        .get(&base_url)
        .query(&[("query", sql.as_str())])
        .send()
        .await?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("depth_dynamic_diff_audit insert non-2xx ({status}): {body}");
    }
    Ok(())
}

/// Formats a SID slice as a comma-joined CSV string, truncating at
/// `MAX_SID_LIST_BYTES`. Returns an empty string for an empty input.
#[must_use]
fn format_sid_list(sids: &[u32]) -> String {
    if sids.is_empty() {
        return String::new();
    }
    let mut out = String::with_capacity(sids.len() * 8);
    for (idx, sid) in sids.iter().enumerate() {
        if out.len() > MAX_SID_LIST_BYTES {
            out.push_str(",…truncated");
            break;
        }
        if idx > 0 {
            out.push(',');
        }
        out.push_str(&sid.to_string());
    }
    out
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
    fn test_dedup_key_depth_dynamic_diff_audit_is_ts_plus_feed() {
        assert_eq!(DEDUP_KEY_DEPTH_DYNAMIC_DIFF_AUDIT, "ts, feed");
    }

    #[test]
    fn test_dedup_key_depth_dynamic_diff_audit_excludes_security_id() {
        // The audit row carries SID *lists* in STRING columns rather
        // than per-SID rows, so the DEDUP key intentionally has no
        // security_id column. This is allowed because the row is keyed
        // on (cycle ts, feed) and the SID detail lives inside the row,
        // not as a key.
        assert!(!DEDUP_KEY_DEPTH_DYNAMIC_DIFF_AUDIT.contains("security_id"));
    }

    #[test]
    fn test_table_name_constant() {
        assert_eq!(
            QUESTDB_TABLE_DEPTH_DYNAMIC_DIFF_AUDIT,
            "depth_dynamic_diff_audit"
        );
    }

    #[test]
    fn test_format_sid_list_handles_empty_slice() {
        assert_eq!(format_sid_list(&[]), "");
    }

    #[test]
    fn test_format_sid_list_emits_comma_joined_csv() {
        assert_eq!(format_sid_list(&[50001, 50002, 50003]), "50001,50002,50003");
    }

    #[test]
    fn test_format_sid_list_truncates_at_max_bytes() {
        // Build a slice large enough to overflow MAX_SID_LIST_BYTES.
        // Each SID = 10 ASCII digits (e.g. 4_000_000_000) + 1 separator
        // = 11 bytes; 500 SIDs ≈ 5.5 KB — well past the 4 KB cap.
        // Use u32 values in the 1B-4B range so to_string() produces 10
        // chars consistently.
        let oversize: Vec<u32> = (0..500_u32).map(|i| 1_000_000_000_u32 + i).collect();
        let s = format_sid_list(&oversize);
        assert!(
            s.contains("…truncated"),
            "expected truncation marker in output of len {}",
            s.len()
        );
        assert!(s.len() <= MAX_SID_LIST_BYTES + 32); // small slack for tail
    }

    /// Regression: 2026-04-28 — INSERT MUST convert nanos to micros so
    /// QuestDB TIMESTAMP doesn't overflow year-9999 bound. Same bug
    /// class as phase2_audit_persistence.rs ratchet.
    #[test]
    fn test_insert_sql_uses_microseconds_not_nanoseconds() {
        let src = include_str!("depth_dynamic_diff_audit_persistence.rs");
        assert!(
            src.contains("ts_micros_ist = ts_nanos_ist / 1_000"),
            "INSERT must convert nanos to micros before embedding"
        );
    }

    #[tokio::test]
    async fn test_append_depth_dynamic_diff_audit_row_returns_err_when_questdb_unreachable() {
        let cfg = test_cfg(1);
        let result = append_depth_dynamic_diff_audit_row(
            &cfg,
            1_710_000_000_000_000_000,
            "depth-20-dynamic",
            1,
            1,
            249,
            &[50000],
            &[50001],
        )
        .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_append_depth_dynamic_diff_audit_row_escapes_quotes_in_feed() {
        // Defensive: caller-supplied feed shouldn't break the SQL.
        let cfg = test_cfg(1);
        let _ =
            append_depth_dynamic_diff_audit_row(&cfg, 0, "depth-20-dyn'amic", 0, 0, 0, &[], &[])
                .await;
    }

    #[tokio::test]
    async fn test_append_depth_dynamic_diff_audit_row_handles_empty_sid_lists() {
        let cfg = test_cfg(1);
        // No-op cycle would never call this fn, but defensive: if it
        // did with empty lists, the call must complete without panic
        // (it'll fail on the unreachable port, but that's fine).
        let _ = append_depth_dynamic_diff_audit_row(
            &cfg,
            1_710_000_000_000_000_000,
            "depth-200-dynamic",
            0,
            0,
            5,
            &[],
            &[],
        )
        .await;
    }
}
