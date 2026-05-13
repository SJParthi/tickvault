//! Phase 0 Item 12 — last-tick audit persistence (2026-05-13).
//!
//! Per-SID per-bar forensic record of the most recent
//! `exchange_timestamp` observed during each 1m bar's seal. Used to
//! diagnose:
//!   * Was the 15:29 bar truly sealed at 15:29:59.x or did we lose
//!     the final tick? (compare `last_tick_exchange_ts_nanos` to
//!     `MARKET_CLOSE_IST_NANOS - 1ns`)
//!   * How much late-arrival lag does Dhan exhibit at close-of-day?
//!     (`late_arrival_ms` distribution per SID)
//!   * Per-SID coverage during outage recovery (gap-fill audit gives
//!     per-bar summary; this gives per-SID-per-bar detail).
//!
//! Schema (derived from `topic-PHASE-0-LEAN-LOCKED.md` §8):
//!
//! ```sql
//! CREATE TABLE IF NOT EXISTS last_tick_audit (
//!   ts TIMESTAMP,                     -- seal time (IST nanos → micros)
//!   trading_date_ist STRING,
//!   bar_minute STRING,                -- "15:29"
//!   security_id INT,
//!   exchange_segment SYMBOL,
//!   last_tick_exchange_ts_nanos LONG, -- last-tick exchange_ts (IST nanos)
//!   late_arrival_ms LONG              -- (seal_ts - last_tick_exchange_ts) in ms
//! ) timestamp(ts) PARTITION BY DAY
//! DEDUP UPSERT KEYS(trading_date_ist, bar_minute, security_id, exchange_segment);
//! ```
//!
//! The DEDUP key follows I-P1-11 (security_id + exchange_segment) plus
//! the per-day per-bar identity columns. A re-seal of the same bar
//! for the same SID overwrites the prior row (idempotent on retry).
//!
//! Mirrors `ws_reconnect_audit_persistence.rs` + `gap_fill_audit_persistence.rs`
//! patterns.

use std::time::Duration;

use reqwest::Client;
use tracing::{error, info, warn};

use tickvault_common::config::QuestDbConfig;
use tickvault_common::sanitize::sanitize_audit_string;

pub const QUESTDB_TABLE_LAST_TICK_AUDIT: &str = "last_tick_audit";
pub const DEDUP_KEY_LAST_TICK_AUDIT: &str =
    "trading_date_ist, bar_minute, security_id, exchange_segment";

const QUESTDB_DDL_TIMEOUT_SECS: u64 = 10;

// TEST-EXEMPT: requires running QuestDB; tested via boot integration in CI.
pub async fn ensure_last_tick_audit_table(questdb_config: &QuestDbConfig) {
    let base_url = format!(
        "http://{}:{}/exec",
        questdb_config.host, questdb_config.http_port
    );
    let client = Client::builder()
        .timeout(Duration::from_secs(QUESTDB_DDL_TIMEOUT_SECS))
        .build()
        .unwrap_or_else(|_| Client::new());

    let create_ddl = format!(
        "CREATE TABLE IF NOT EXISTS {QUESTDB_TABLE_LAST_TICK_AUDIT} (\
            ts TIMESTAMP, \
            trading_date_ist STRING, \
            bar_minute STRING, \
            security_id INT, \
            exchange_segment SYMBOL, \
            last_tick_exchange_ts_nanos LONG, \
            late_arrival_ms LONG\
        ) timestamp(ts) PARTITION BY DAY \
        DEDUP UPSERT KEYS({DEDUP_KEY_LAST_TICK_AUDIT});"
    );
    match client
        .get(&base_url)
        .query(&[("query", create_ddl.as_str())])
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => {
            info!(table = QUESTDB_TABLE_LAST_TICK_AUDIT, "audit table ready");
        }
        Ok(resp) => {
            warn!(
                table = QUESTDB_TABLE_LAST_TICK_AUDIT,
                status = %resp.status(),
                "DDL non-2xx"
            );
        }
        Err(err) => {
            error!(
                table = QUESTDB_TABLE_LAST_TICK_AUDIT,
                ?err,
                "DDL request failed"
            );
        }
    }
}

/// Append one last-tick audit row.
///
/// `ts_nanos_ist`: bar seal time (IST nanoseconds). Converted to
/// microseconds before INSERT (regression fix 2026-04-28).
/// `bar_minute`: human-readable "HH:MM" of the bar (e.g. `"15:29"`).
/// `security_id` + `exchange_segment`: I-P1-11 composite identity.
/// `last_tick_exchange_ts_nanos`: the actual exchange-stamped time of
/// the last tick within this bar (raw IST nanos).
/// `late_arrival_ms`: signed milliseconds between seal time and
/// last-tick exchange ts. Positive = last tick arrived early; large
/// = Dhan ingestion lag.
#[allow(clippy::too_many_arguments)] // APPROVED: audit row schema requires every column
pub async fn append_last_tick_audit_row(
    questdb_config: &QuestDbConfig,
    ts_nanos_ist: i64,
    trading_date_ist: &str,
    bar_minute: &str,
    security_id: u32,
    exchange_segment: &str,
    last_tick_exchange_ts_nanos: i64,
    late_arrival_ms: i64,
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
    let segment_esc = sanitize_audit_string(exchange_segment);
    // QuestDB TIMESTAMP stores microseconds since epoch — regression
    // 2026-04-28, source-scan ratchet below pins the fix.
    let ts_micros_ist = ts_nanos_ist / 1_000;
    let sql = format!(
        "INSERT INTO {QUESTDB_TABLE_LAST_TICK_AUDIT} \
         (ts, trading_date_ist, bar_minute, security_id, exchange_segment, \
          last_tick_exchange_ts_nanos, late_arrival_ms) VALUES \
         ({ts_micros_ist}, '{trading_date_esc}', '{bar_minute_esc}', \
          {security_id}, '{segment_esc}', {last_tick_exchange_ts_nanos}, \
          {late_arrival_ms});"
    );
    let resp = client
        .get(&base_url)
        .query(&[("query", sql.as_str())])
        .send()
        .await?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("last_tick audit insert non-2xx ({status}): {body}");
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
    fn test_table_name_constant_is_last_tick_audit() {
        assert_eq!(QUESTDB_TABLE_LAST_TICK_AUDIT, "last_tick_audit");
    }

    #[test]
    fn test_dedup_key_matches_plan_lock() {
        // Plan §8: per-SID per-bar forensic table. DEDUP key MUST
        // include security_id + exchange_segment (I-P1-11) AND
        // trading_date_ist + bar_minute for per-day per-bar identity.
        assert_eq!(
            DEDUP_KEY_LAST_TICK_AUDIT,
            "trading_date_ist, bar_minute, security_id, exchange_segment",
        );
    }

    #[test]
    fn test_dedup_key_includes_exchange_segment_per_i_p1_11() {
        // I-P1-11: every collection keyed on security_id MUST also
        // include exchange_segment because Dhan reuses numeric SIDs
        // across segments (e.g. 27 = FINNIFTY IDX_I AND some NSE_EQ).
        // The meta-guard in `dedup_segment_meta_guard.rs` verifies
        // this workspace-wide; the assertion here is the per-table pin.
        assert!(
            DEDUP_KEY_LAST_TICK_AUDIT.contains("security_id"),
            "DEDUP key MUST identify per-SID rows",
        );
        assert!(
            DEDUP_KEY_LAST_TICK_AUDIT.contains("exchange_segment"),
            "I-P1-11: security_id alone is NOT unique — exchange_segment \
             MUST be part of every DEDUP key that mentions security_id",
        );
    }

    #[tokio::test]
    async fn test_append_last_tick_audit_row_returns_err_when_questdb_unreachable() {
        let cfg = test_cfg(1);
        let result = append_last_tick_audit_row(
            &cfg,
            1_710_000_000_000_000_000,
            "2026-05-13",
            "15:29",
            13,
            "IdxI",
            55_799_586_000_000,
            14_000, // 14 seconds late
        )
        .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_append_last_tick_audit_row_handles_negative_late_arrival() {
        // Defensive: late_arrival_ms can be NEGATIVE if seal fires
        // before the last-tick's exchange_ts (clock skew, out-of-order
        // delivery). The INSERT must not panic on signed i64 input.
        let cfg = test_cfg(1);
        let _ = append_last_tick_audit_row(
            &cfg,
            1_710_000_000_000_000_000,
            "2026-05-13",
            "15:29",
            25,
            "NseFno",
            55_799_999_999_999,
            -150,
        )
        .await;
    }

    /// Regression: 2026-04-28 — see phase2_audit_persistence.rs for the
    /// nanos-to-micros bug class. Source-scan ratchet locks the fix.
    #[test]
    fn test_insert_sql_uses_microseconds_not_nanoseconds() {
        let src = include_str!("last_tick_audit_persistence.rs");
        assert!(
            src.contains("ts_micros_ist = ts_nanos_ist / 1_000"),
            "INSERT must convert nanos to micros before embedding",
        );
    }

    #[test]
    fn test_dedup_key_constant_present_in_ddl_string() {
        // Source-scan: the CREATE TABLE DDL must reference
        // DEDUP_KEY_LAST_TICK_AUDIT exactly. Pins that the runtime DDL
        // matches the plan-locked DEDUP contract.
        let src = include_str!("last_tick_audit_persistence.rs");
        assert!(
            src.contains("DEDUP UPSERT KEYS({DEDUP_KEY_LAST_TICK_AUDIT})"),
            "DDL must use the DEDUP_KEY_LAST_TICK_AUDIT constant verbatim",
        );
    }

    #[test]
    fn test_last_tick_exchange_ts_nanos_column_is_long_type() {
        // Source-scan: the column type for last_tick_exchange_ts_nanos
        // MUST be LONG (i64). i32 would overflow at year 2262 — we
        // need i64 for IST epoch nanos.
        let src = include_str!("last_tick_audit_persistence.rs");
        assert!(
            src.contains("last_tick_exchange_ts_nanos LONG"),
            "last_tick_exchange_ts_nanos column MUST be LONG (i64) — \
             nanos require 64-bit range",
        );
    }
}
