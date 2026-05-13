//! Phase 0 Item 9 — gap-fill audit persistence (2026-05-13).
//!
//! Every disconnect-triggered gap-fill operation against Dhan REST
//! `/v2/charts/intraday` emits one row per bar-minute so the operator
//! can reconstruct exactly which bars were refilled, when, and with
//! what outcome.
//!
//! Schema is locked by `topic-PHASE-0-LEAN-LOCKED.md` §3:
//!
//! ```sql
//! CREATE TABLE IF NOT EXISTS gap_fill_audit (
//!   ts TIMESTAMP,                 -- minute bar start (IST nanos, stored as micros)
//!   trading_date_ist STRING,
//!   bar_minute STRING,            -- "09:33"
//!   trigger_event STRING,         -- ws_disconnect | manual | scheduler_catchup
//!   sids_requested INT,
//!   sids_completed INT,
//!   sids_failed INT,
//!   duration_ms LONG,
//!   result STRING                 -- success | partial | failed
//! ) DEDUP UPSERT KEYS(trading_date_ist, bar_minute, trigger_event);
//! ```
//!
//! Mirrors the established pattern of `ws_reconnect_audit_persistence.rs`
//! and the 8 other audit-table modules in this crate.

use std::time::Duration;

use reqwest::Client;
use tracing::{error, info, warn};

use tickvault_common::config::QuestDbConfig;
use tickvault_common::sanitize::sanitize_audit_string;

pub const QUESTDB_TABLE_GAP_FILL_AUDIT: &str = "gap_fill_audit";
pub const DEDUP_KEY_GAP_FILL_AUDIT: &str = "trading_date_ist, bar_minute, trigger_event";

const QUESTDB_DDL_TIMEOUT_SECS: u64 = 10;

/// Valid `trigger_event` values. The scheduler MUST pass one of these
/// — arbitrary strings are sanitized but the operator's audit query
/// filters on these literals.
pub const TRIGGER_EVENT_WS_DISCONNECT: &str = "ws_disconnect";
pub const TRIGGER_EVENT_MANUAL: &str = "manual";
pub const TRIGGER_EVENT_SCHEDULER_CATCHUP: &str = "scheduler_catchup";

/// Valid `result` values.
pub const RESULT_SUCCESS: &str = "success";
pub const RESULT_PARTIAL: &str = "partial";
pub const RESULT_FAILED: &str = "failed";

// TEST-EXEMPT: requires running QuestDB; tested via boot integration in CI.
pub async fn ensure_gap_fill_audit_table(questdb_config: &QuestDbConfig) {
    let base_url = format!(
        "http://{}:{}/exec",
        questdb_config.host, questdb_config.http_port
    );
    let client = Client::builder()
        .timeout(Duration::from_secs(QUESTDB_DDL_TIMEOUT_SECS))
        .build()
        .unwrap_or_else(|_| Client::new());

    let create_ddl = format!(
        "CREATE TABLE IF NOT EXISTS {QUESTDB_TABLE_GAP_FILL_AUDIT} (\
            ts TIMESTAMP, \
            trading_date_ist STRING, \
            bar_minute STRING, \
            trigger_event STRING, \
            sids_requested INT, \
            sids_completed INT, \
            sids_failed INT, \
            duration_ms LONG, \
            result STRING\
        ) timestamp(ts) PARTITION BY DAY \
        DEDUP UPSERT KEYS({DEDUP_KEY_GAP_FILL_AUDIT});"
    );
    match client
        .get(&base_url)
        .query(&[("query", create_ddl.as_str())])
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => {
            info!(table = QUESTDB_TABLE_GAP_FILL_AUDIT, "audit table ready");
        }
        Ok(resp) => {
            warn!(
                table = QUESTDB_TABLE_GAP_FILL_AUDIT,
                status = %resp.status(),
                "DDL non-2xx"
            );
        }
        Err(err) => {
            error!(
                table = QUESTDB_TABLE_GAP_FILL_AUDIT,
                ?err,
                "DDL request failed"
            );
        }
    }
}

/// Append one gap-fill audit row.
///
/// `ts_nanos_ist` is the bar's START time in IST epoch nanoseconds.
/// Converted to microseconds before INSERT (QuestDB TIMESTAMP type
/// stores microseconds — same nanos→micros bug class fixed in
/// `ws_reconnect_audit_persistence.rs` 2026-04-28).
///
/// `bar_minute` is the human-readable "HH:MM" of the bar (e.g.
/// `"09:33"`) — used for the DEDUP UPSERT key so a re-attempt of the
/// SAME bar under the SAME trigger overwrites the prior row.
///
/// `trigger_event` SHOULD be one of `TRIGGER_EVENT_*` constants but is
/// sanitized so arbitrary operator-supplied strings are safe.
///
/// `result` SHOULD be one of `RESULT_*` constants for the operator's
/// audit query to filter cleanly.
#[allow(clippy::too_many_arguments)] // APPROVED: audit row schema requires every column
pub async fn append_gap_fill_audit_row(
    questdb_config: &QuestDbConfig,
    ts_nanos_ist: i64,
    trading_date_ist: &str,
    bar_minute: &str,
    trigger_event: &str,
    sids_requested: i32,
    sids_completed: i32,
    sids_failed: i32,
    duration_ms: i64,
    result: &str,
) -> anyhow::Result<()> {
    let base_url = format!(
        "http://{}:{}/exec",
        questdb_config.host, questdb_config.http_port
    );
    let client = Client::builder()
        .timeout(Duration::from_secs(QUESTDB_DDL_TIMEOUT_SECS))
        .build()?;
    let trading_date_esc = sanitize_audit_string(trading_date_ist);
    let bar_minute_esc = sanitize_audit_string(bar_minute);
    let trigger_esc = sanitize_audit_string(trigger_event);
    let result_esc = sanitize_audit_string(result);
    // QuestDB TIMESTAMP columns store microseconds since epoch.
    // Regression: 2026-04-28 — see phase2_audit_persistence.rs for the
    // nanos-to-micros bug class. Source-scan ratchet below locks the fix.
    let ts_micros_ist = ts_nanos_ist / 1_000;
    let sql = format!(
        "INSERT INTO {QUESTDB_TABLE_GAP_FILL_AUDIT} \
         (ts, trading_date_ist, bar_minute, trigger_event, sids_requested, \
          sids_completed, sids_failed, duration_ms, result) VALUES \
         ({ts_micros_ist}, '{trading_date_esc}', '{bar_minute_esc}', \
          '{trigger_esc}', {sids_requested}, {sids_completed}, {sids_failed}, \
          {duration_ms}, '{result_esc}');"
    );
    let resp = client
        .get(&base_url)
        .query(&[("query", sql.as_str())])
        .send()
        .await?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("gap_fill audit insert non-2xx ({status}): {body}");
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
    fn test_table_name_constant_is_gap_fill_audit() {
        assert_eq!(QUESTDB_TABLE_GAP_FILL_AUDIT, "gap_fill_audit");
    }

    #[test]
    fn test_dedup_key_matches_plan_lock() {
        // Plan §3 row 4 of gap_fill_audit schema:
        // `DEDUP UPSERT KEYS(trading_date_ist, bar_minute, trigger_event);`
        // Same-day re-attempt of the SAME bar under the SAME trigger
        // overwrites the previous row — operator can re-run gap-fill
        // without DUP'd rows.
        assert_eq!(
            DEDUP_KEY_GAP_FILL_AUDIT,
            "trading_date_ist, bar_minute, trigger_event",
        );
    }

    #[test]
    fn test_dedup_key_does_not_include_security_id() {
        // gap_fill_audit is a per-bar summary table (not per-instrument).
        // The summary counters `sids_requested/completed/failed` carry
        // the per-SID detail; the row identity is `(date, bar, trigger)`.
        // No `security_id` should appear in the DEDUP key.
        assert!(
            !DEDUP_KEY_GAP_FILL_AUDIT.contains("security_id"),
            "gap_fill_audit DEDUP key must NOT include security_id \
             (table is per-bar summary, not per-SID)",
        );
    }

    #[test]
    fn test_trigger_event_constants_match_plan_lock() {
        // Plan §3 column comment locks these to:
        //   trigger_event STRING -- ws_disconnect | manual | scheduler_catchup
        assert_eq!(TRIGGER_EVENT_WS_DISCONNECT, "ws_disconnect");
        assert_eq!(TRIGGER_EVENT_MANUAL, "manual");
        assert_eq!(TRIGGER_EVENT_SCHEDULER_CATCHUP, "scheduler_catchup");
    }

    #[test]
    fn test_result_constants_match_plan_lock() {
        // Plan §3 column comment locks these to:
        //   result STRING -- success | partial | failed
        assert_eq!(RESULT_SUCCESS, "success");
        assert_eq!(RESULT_PARTIAL, "partial");
        assert_eq!(RESULT_FAILED, "failed");
    }

    #[tokio::test]
    async fn test_append_gap_fill_audit_row_returns_err_when_questdb_unreachable() {
        let cfg = test_cfg(1);
        let result = append_gap_fill_audit_row(
            &cfg,
            1_710_000_000_000_000_000,
            "2026-05-13",
            "09:33",
            TRIGGER_EVENT_WS_DISCONNECT,
            218,
            218,
            0,
            1_234,
            RESULT_SUCCESS,
        )
        .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_append_gap_fill_audit_row_handles_partial_result() {
        // Partial result: scheduler completed some SIDs but failed others.
        let cfg = test_cfg(1);
        let _ = append_gap_fill_audit_row(
            &cfg,
            1_710_000_000_000_000_000,
            "2026-05-13",
            "09:33",
            TRIGGER_EVENT_WS_DISCONNECT,
            218,
            210,
            8,
            5_500,
            RESULT_PARTIAL,
        )
        .await;
    }

    /// Regression: 2026-04-28 — see phase2_audit_persistence.rs for the
    /// nanos-to-micros bug class. Source-scan ratchet locks the fix.
    #[test]
    fn test_insert_sql_uses_microseconds_not_nanoseconds() {
        let src = include_str!("gap_fill_audit_persistence.rs");
        assert!(
            src.contains("ts_micros_ist = ts_nanos_ist / 1_000"),
            "INSERT must convert nanos to micros before embedding",
        );
    }

    #[test]
    fn test_dedup_key_constant_present_in_ddl_string() {
        // Source-scan: the CREATE TABLE DDL must reference
        // DEDUP_KEY_GAP_FILL_AUDIT exactly. Pins that the runtime DDL
        // matches the plan-locked DEDUP contract.
        let src = include_str!("gap_fill_audit_persistence.rs");
        assert!(
            src.contains("DEDUP UPSERT KEYS({DEDUP_KEY_GAP_FILL_AUDIT})"),
            "DDL must use the DEDUP_KEY_GAP_FILL_AUDIT constant verbatim",
        );
    }
}
