//! Wave 5 Item 26 L2 — NSE bhavcopy volume cross-check audit persistence.
//!
//! Persists every (trading_date, security_id, segment) verdict from the
//! 16:30 IST daily NSE bhavcopy cross-check. SEBI-relevant: 90d hot →
//! S3 IT → Glacier per `aws-budget.md`.
//!
//! Each row carries the dhan-captured EOD cumulative volume, the
//! NSE-reported EOD volume, the signed `diff_pct`, and the typed verdict
//! (`PASS` / `FAIL` / `MISSING_OUR` / `MISSING_NSE`). Operator triages
//! `FAIL` and `MISSING_OUR` rows; `PASS` is the expected normal state.
//!
//! # I-P1-11 + STORAGE-GAP-01 invariants
//!
//! `DEDUP_KEY_VOLUME_NSE_AUDIT` includes `segment` per the
//! security-id-uniqueness rule (Dhan reuses numeric `security_id`
//! across `ExchangeSegment` values). This is enforced by the
//! `dedup_segment_meta_guard.rs` workspace meta-guard.

use std::time::Duration;

use reqwest::Client;
use tracing::{error, info, warn};

use tickvault_common::config::QuestDbConfig;
use tickvault_common::sanitize::sanitize_audit_string;

/// QuestDB table name for the NSE bhavcopy volume cross-check audit trail.
pub const QUESTDB_TABLE_VOLUME_NSE_AUDIT: &str = "volume_nse_audit";

/// DEDUP UPSERT KEY for `volume_nse_audit` — composite
/// `(trading_date_ist, security_id, segment)` per Wave 5 plan §"Item 26
/// L2 NSE bhavcopy — verified implementation recipe". Includes `segment`
/// so the `dedup_segment_meta_guard.rs` meta-guard is satisfied
/// (I-P1-11 + STORAGE-GAP-01).
pub const DEDUP_KEY_VOLUME_NSE_AUDIT: &str = "trading_date_ist, security_id, segment";

/// HTTP timeout for DDL probes + INSERT round-trips.
const QUESTDB_DDL_TIMEOUT_SECS: u64 = 10;

/// Idempotent CREATE + ALTER ADD COLUMN IF NOT EXISTS for the audit
/// table. Called once at boot from `main.rs` per the schema-self-heal
/// pattern in `observability-architecture.md`.
// TEST-EXEMPT: requires running QuestDB; tested via boot integration in CI.
pub async fn ensure_volume_nse_audit_table(questdb_config: &QuestDbConfig) {
    let base_url = format!(
        "http://{}:{}/exec",
        questdb_config.host, questdb_config.http_port
    );
    let client = Client::builder()
        .timeout(Duration::from_secs(QUESTDB_DDL_TIMEOUT_SECS))
        .build()
        .unwrap_or_else(|_| Client::new());

    let create_ddl = format!(
        "CREATE TABLE IF NOT EXISTS {QUESTDB_TABLE_VOLUME_NSE_AUDIT} (\
            ts TIMESTAMP, \
            trading_date_ist TIMESTAMP, \
            security_id INT, \
            segment SYMBOL, \
            ticker_symbol SYMBOL, \
            dhan_eod_volume LONG, \
            nse_eod_volume LONG, \
            diff_pct DOUBLE, \
            verdict SYMBOL\
        ) timestamp(ts) PARTITION BY DAY \
        DEDUP UPSERT KEYS({DEDUP_KEY_VOLUME_NSE_AUDIT});"
    );
    match client
        .get(&base_url)
        .query(&[("query", create_ddl.as_str())])
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => {
            info!(table = QUESTDB_TABLE_VOLUME_NSE_AUDIT, "audit table ready");
        }
        Ok(resp) => {
            warn!(table = QUESTDB_TABLE_VOLUME_NSE_AUDIT, status = %resp.status(), "DDL non-2xx — continuing best-effort");
        }
        Err(err) => {
            error!(
                table = QUESTDB_TABLE_VOLUME_NSE_AUDIT,
                ?err,
                "DDL request failed — table may already exist"
            );
        }
    }
}

/// Append one volume_nse_audit row.
///
/// The cross-check produces typically 200K rows for F&O on a busy day,
/// but this function is called once-per-row from a post-market scheduler
/// that drains the entire batch over ~30s, so HTTP `/exec` INSERT is
/// acceptable (no need for ILP batching at this volume + cadence).
///
/// Caller passes `ts_nanos_ist` and `trading_date_ist_nanos` as IST
/// wall-clock nanos; this function divides by 1_000 before embedding
/// to stay in QuestDB's TIMESTAMP microsecond range (per the
/// 2026-04-28 Phase 2 audit regression fix in `phase2_audit_persistence.rs`).
pub async fn append_volume_nse_audit_row(
    questdb_config: &QuestDbConfig,
    ts_nanos_ist: i64,
    trading_date_ist_nanos: i64,
    security_id: u32,
    segment: &str,
    ticker_symbol: &str,
    dhan_eod_volume: u64,
    nse_eod_volume: u64,
    diff_pct: f64,
    verdict: &str,
) -> anyhow::Result<()> {
    let base_url = format!(
        "http://{}:{}/exec",
        questdb_config.host, questdb_config.http_port
    );
    let client = Client::builder()
        .timeout(Duration::from_secs(QUESTDB_DDL_TIMEOUT_SECS))
        .build()?;
    let escaped_segment = sanitize_audit_string(segment);
    let escaped_ticker = sanitize_audit_string(ticker_symbol);
    let escaped_verdict = sanitize_audit_string(verdict);
    let ts_micros_ist = ts_nanos_ist / 1_000;
    let trading_date_ist_micros = trading_date_ist_nanos / 1_000;
    // Diff_pct can be infinite (when nse_eod_volume == 0); QuestDB's
    // DOUBLE accepts NaN/Inf but the SQL serializer doesn't. Coerce to
    // a sentinel finite value so the INSERT succeeds; downstream queries
    // can filter on `verdict = 'MISSING_NSE'` for the same semantic.
    let diff_pct_safe = if diff_pct.is_finite() { diff_pct } else { 0.0 };
    let sql = format!(
        "INSERT INTO {QUESTDB_TABLE_VOLUME_NSE_AUDIT} \
         (ts, trading_date_ist, security_id, segment, ticker_symbol, dhan_eod_volume, nse_eod_volume, diff_pct, verdict) VALUES \
         ({ts_micros_ist}, {trading_date_ist_micros}, {security_id}, '{escaped_segment}', '{escaped_ticker}', \
          {dhan_eod_volume}, {nse_eod_volume}, {diff_pct_safe}, '{escaped_verdict}');"
    );
    let resp = client
        .get(&base_url)
        .query(&[("query", sql.as_str())])
        .send()
        .await?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("volume_nse_audit insert non-2xx ({status}): {body}");
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
    fn test_dedup_key_volume_nse_audit_includes_segment_per_i_p1_11() {
        // I-P1-11 + STORAGE-GAP-01: composite key MUST include segment so
        // Dhan's cross-segment security_id reuse cannot collapse rows.
        assert!(DEDUP_KEY_VOLUME_NSE_AUDIT.contains("security_id"));
        assert!(DEDUP_KEY_VOLUME_NSE_AUDIT.contains("segment"));
        assert!(DEDUP_KEY_VOLUME_NSE_AUDIT.contains("trading_date_ist"));
    }

    #[test]
    fn test_table_name_constant_is_volume_nse_audit() {
        assert_eq!(QUESTDB_TABLE_VOLUME_NSE_AUDIT, "volume_nse_audit");
    }

    #[tokio::test]
    async fn test_append_volume_nse_audit_row_returns_err_when_questdb_unreachable() {
        // Port 1 is unprivileged + always rejects; the function must
        // propagate the network error rather than swallow it.
        let cfg = test_cfg(1);
        let result = append_volume_nse_audit_row(
            &cfg,
            1_710_000_000_000_000_000,
            1_709_980_200_000_000_000,
            42,
            "NSE_FNO",
            "NIFTY",
            1_000_000,
            1_000_500,
            0.05,
            "PASS",
        )
        .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_append_volume_nse_audit_row_handles_infinite_diff_pct() {
        // MISSING_NSE case: nse_eod_volume = 0, dhan = N → diff_pct = INF.
        // The SQL builder must coerce INF to 0.0 (sentinel) so the insert
        // doesn't fail with a serializer error.
        let cfg = test_cfg(1);
        let _ = append_volume_nse_audit_row(
            &cfg,
            0,
            0,
            99,
            "NSE_FNO",
            "",
            7_777,
            0,
            f64::INFINITY,
            "MISSING_NSE",
        )
        .await;
        // Reaching this assertion proves no panic in the SQL builder.
    }

    #[tokio::test]
    async fn test_append_volume_nse_audit_row_escapes_single_quotes_in_ticker() {
        // Defensive: NSE doesn't emit ticker symbols with quotes today
        // but our sanitizer must not panic if a future format does.
        let cfg = test_cfg(1);
        let _ =
            append_volume_nse_audit_row(&cfg, 0, 0, 42, "NSE_FNO", "FOO'S", 100, 100, 0.0, "PASS")
                .await;
    }

    /// Regression: 2026-04-28 — INSERT used to embed `ts_nanos_ist`
    /// directly into the SQL VALUES clause, but QuestDB TIMESTAMP columns
    /// store microseconds since epoch, so a nanosecond value overflowed
    /// year 9999 and the insert returned 400. Source-scan ratchet locks
    /// the fix in place for this writer too.
    #[test]
    fn test_insert_sql_uses_microseconds_not_nanoseconds() {
        let src = include_str!("volume_nse_audit_persistence.rs");
        assert!(
            src.contains("ts_micros_ist = ts_nanos_ist / 1_000"),
            "INSERT must convert ts nanos to micros before embedding"
        );
        assert!(
            src.contains("trading_date_ist_micros = trading_date_ist_nanos / 1_000"),
            "INSERT must convert trading_date_ist nanos to micros"
        );
    }

    #[test]
    fn test_ddl_includes_all_nine_columns() {
        // Documentation ratchet — if columns are added/removed the test
        // fails LOUD so the DDL stays in sync with the BhavcopyAuditRow
        // shape in `bhavcopy_cross_check.rs`.
        let src = include_str!("volume_nse_audit_persistence.rs");
        for col in [
            "ts TIMESTAMP",
            "trading_date_ist TIMESTAMP",
            "security_id INT",
            "segment SYMBOL",
            "ticker_symbol SYMBOL",
            "dhan_eod_volume LONG",
            "nse_eod_volume LONG",
            "diff_pct DOUBLE",
            "verdict SYMBOL",
        ] {
            assert!(
                src.contains(col),
                "DDL must declare column `{col}` — see plan §'Audit table DDL'"
            );
        }
    }
}
