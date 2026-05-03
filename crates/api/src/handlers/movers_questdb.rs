//! QuestDB-backed query helpers for movers REST handlers.
//!
//! Replaces the in-memory `TopMoversTracker` / `MoversTrackerV2`
//! snapshots that were retired by PR #446 (Steps 4 + 5 of the movers
//! cleanup wave). All movers reads now go through QuestDB SELECTs
//! against the `movers_5s` materialized view (5-second granularity —
//! refresh-aligned to the `movers_1s` base table cadence).
//!
//! # Design
//!
//! - Single HTTP `/exec` call per handler, JSON dataset response.
//! - SQL string builder is pure (no allocations beyond the `String`
//!   itself), parameterised by `instrument_type` + sort metric + LIMIT.
//! - Response parsing uses the same column ordering pattern as
//!   `depth_dynamic_pipeline_v2::parse_cohort_dataset`.
//! - Cold path — REST handler latency budget is ~100ms typical;
//!   QuestDB query + JSON parse fits comfortably.

use anyhow::{Context, Result, anyhow};
use serde_json::Value;
use std::time::Duration;

use tickvault_common::config::QuestDbConfig;
use tickvault_core::pipeline::top_movers::MoverEntry;

/// HTTP timeout for movers QuestDB queries — matches the depth
/// selector (cold path, generous).
const QUESTDB_QUERY_TIMEOUT_SECS: u64 = 10;

/// Default LIMIT for movers REST queries — matches the legacy
/// `TopMoversTracker::TOP_N` value so the API contract is unchanged.
pub const DEFAULT_MOVERS_LIMIT: usize = 20;

/// Sort metric for the movers SELECT.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MoversSortMetric {
    /// Ranks by `change_pct DESC` — gainers list.
    GainersByChangePct,
    /// Ranks by `change_pct ASC` — losers list.
    LosersByChangePct,
    /// Ranks by `volume DESC` — most-active list.
    MostActiveByVolume,
}

impl MoversSortMetric {
    /// Returns the SQL `ORDER BY` clause body (no `ORDER BY` prefix).
    #[must_use]
    pub const fn order_by_clause(self) -> &'static str {
        match self {
            Self::GainersByChangePct => "change_pct DESC",
            Self::LosersByChangePct => "change_pct ASC",
            Self::MostActiveByVolume => "volume DESC",
        }
    }
}

/// Builds the SELECT for a single movers list.
///
/// Reads the most recent 5-second `movers_5s` snapshot row per
/// instrument (the view's `last()` aggregate ensures one row per
/// instrument per 5s bucket). Filters by `instrument_type` (e.g.
/// `'EQUITY'`, `'INDEX'`) and ranks by the requested sort metric.
///
/// # Why `movers_5s`
///
/// - Fresh enough for a REST endpoint (5s lag is acceptable).
/// - Server-aggregated by QuestDB so we don't ship per-tick rows.
/// - Already populated by the `movers_base_pipeline` writer.
///
/// # Panics
///
/// Panics if `instrument_type` contains a single quote (defensive —
/// the only legal values are operator-controlled enum-like tokens
/// `'INDEX'`, `'EQUITY'`, `'OPTSTK'`, `'OPTIDX'`, `'FUTSTK'`,
/// `'FUTIDX'`).
#[must_use]
pub fn build_movers_query(
    instrument_type: &str,
    sort_metric: MoversSortMetric,
    limit: usize,
) -> String {
    assert!(
        !instrument_type.contains('\''),
        "instrument_type must not contain single quotes (defensive — operator-controlled enum)"
    );
    assert!(limit > 0, "limit must be > 0");

    // Most recent ts in movers_5s aggregated to one row per security_id.
    // We use `LATEST ON (ts) PARTITION BY security_id` semantics via
    // SAMPLE BY in the view — but simpler: filter the most recent
    // 30-second window so we capture at least 6 5-second buckets,
    // then group by security_id + take latest.
    format!(
        "SELECT security_id, segment, last_price, prev_close, change_pct, volume \
         FROM movers_5s \
         WHERE ts > dateadd('s', -30, now()) \
           AND instrument_type = '{instrument_type}' \
         LATEST ON ts PARTITION BY security_id \
         ORDER BY {order_by} \
         LIMIT {limit}",
        order_by = sort_metric.order_by_clause(),
    )
}

/// Executes a movers query against QuestDB and parses the JSON
/// dataset response into `Vec<MoverEntry>`.
///
/// Returns an empty Vec on parse failure (handler still serves a
/// `available: true` response with empty lists — operator sees
/// "no movers right now" rather than a 5xx).
pub async fn query_movers(
    questdb: &QuestDbConfig,
    instrument_type: &str,
    sort_metric: MoversSortMetric,
    limit: usize,
) -> Result<Vec<MoverEntry>> {
    let sql = build_movers_query(instrument_type, sort_metric, limit);
    let url = format!("http://{}:{}/exec", questdb.host, questdb.http_port);

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(QUESTDB_QUERY_TIMEOUT_SECS))
        .build()
        .context("build reqwest client for movers QuestDB query")?;

    let resp = client
        .get(&url)
        .query(&[("query", sql.as_str())])
        .send()
        .await
        .context("movers QuestDB /exec request")?;

    if !resp.status().is_success() {
        return Err(anyhow!("QuestDB movers query non-2xx: {}", resp.status()));
    }

    let json: Value = resp.json().await.context("parse QuestDB JSON response")?;
    parse_movers_dataset(&json)
}

/// Parses the QuestDB `/exec` JSON response into `Vec<MoverEntry>`.
///
/// Expected dataset column order (matches `build_movers_query`):
/// `security_id, segment, last_price, prev_close, change_pct, volume`.
///
/// Rows with missing or invalid fields are SKIPPED (defensive — a
/// single malformed row should not poison the entire response).
fn parse_movers_dataset(json: &Value) -> Result<Vec<MoverEntry>> {
    let dataset = json
        .get("dataset")
        .and_then(|d| d.as_array())
        .ok_or_else(|| anyhow!("QuestDB response missing dataset"))?;

    let mut out = Vec::with_capacity(dataset.len());
    for row in dataset {
        let row_array = match row.as_array() {
            Some(a) if a.len() >= 6 => a,
            _ => continue, // skip short / non-array rows
        };
        let Some(security_id) = row_array[0].as_u64().and_then(|v| u32::try_from(v).ok()) else {
            continue;
        };
        let segment_str = row_array[1].as_str().unwrap_or("");
        let exchange_segment_code = segment_string_to_code(segment_str);
        let last_traded_price = row_array[2].as_f64().unwrap_or(0.0) as f32;
        let prev_close = row_array[3].as_f64().unwrap_or(0.0) as f32;
        let change_pct = row_array[4].as_f64().unwrap_or(0.0) as f32;
        let volume = row_array[5]
            .as_u64()
            .and_then(|v| u32::try_from(v).ok())
            .unwrap_or(0);

        out.push(MoverEntry {
            security_id,
            exchange_segment_code,
            last_traded_price,
            prev_close,
            change_pct,
            volume,
        });
    }
    Ok(out)
}

/// Maps Dhan single-char segment ('I'/'E'/'D'/'C'/'M') to the
/// numeric `exchange_segment_code` used by `MoverEntry`. Returns 0
/// (IDX_I) on unknown — defensive default for a non-trading display.
fn segment_string_to_code(segment: &str) -> u8 {
    match segment.chars().next() {
        Some('I') => 0, // IDX_I
        Some('E') => 1, // NSE_EQ / BSE_EQ both use 'E'
        Some('D') => 2, // NSE_FNO / BSE_FNO both use 'D'
        Some('C') => 3, // NSE_CURRENCY / BSE_CURRENCY
        Some('M') => 5, // MCX_COMM
        _ => 0,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sort_metric_order_by_clauses_pinned() {
        assert_eq!(
            MoversSortMetric::GainersByChangePct.order_by_clause(),
            "change_pct DESC"
        );
        assert_eq!(
            MoversSortMetric::LosersByChangePct.order_by_clause(),
            "change_pct ASC"
        );
        assert_eq!(
            MoversSortMetric::MostActiveByVolume.order_by_clause(),
            "volume DESC"
        );
    }

    #[test]
    fn test_default_movers_limit_pinned_at_20() {
        // Pin matches legacy TopMoversTracker::TOP_N for API contract stability.
        assert_eq!(DEFAULT_MOVERS_LIMIT, 20);
    }

    #[test]
    fn test_build_movers_query_targets_movers_5s_view() {
        let sql = build_movers_query("EQUITY", MoversSortMetric::GainersByChangePct, 20);
        assert!(sql.contains("FROM movers_5s"));
    }

    #[test]
    fn test_build_movers_query_filters_by_instrument_type() {
        let sql = build_movers_query("EQUITY", MoversSortMetric::GainersByChangePct, 20);
        assert!(sql.contains("instrument_type = 'EQUITY'"));
    }

    #[test]
    fn test_build_movers_query_includes_order_by_and_limit() {
        let sql = build_movers_query("INDEX", MoversSortMetric::LosersByChangePct, 5);
        assert!(sql.contains("ORDER BY change_pct ASC"));
        assert!(sql.contains("LIMIT 5"));
    }

    #[test]
    fn test_build_movers_query_uses_30s_freshness_window() {
        let sql = build_movers_query("EQUITY", MoversSortMetric::GainersByChangePct, 20);
        assert!(sql.contains("dateadd('s', -30, now())"));
    }

    #[test]
    fn test_build_movers_query_uses_latest_on_ts_partition_by_security_id() {
        let sql = build_movers_query("EQUITY", MoversSortMetric::GainersByChangePct, 20);
        assert!(sql.contains("LATEST ON ts PARTITION BY security_id"));
    }

    #[test]
    #[should_panic(expected = "must not contain single quotes")]
    fn test_build_movers_query_panics_on_quote_injection() {
        let _ = build_movers_query(
            "EQUITY' OR 1=1 --",
            MoversSortMetric::GainersByChangePct,
            20,
        );
    }

    #[test]
    #[should_panic(expected = "limit must be > 0")]
    fn test_build_movers_query_panics_on_zero_limit() {
        let _ = build_movers_query("EQUITY", MoversSortMetric::GainersByChangePct, 0);
    }

    #[test]
    fn test_parse_movers_dataset_extracts_all_fields() {
        let json: Value = serde_json::json!({
            "dataset": [
                [42, "E", 100.5, 95.0, 5.78, 10000],
                [99, "I", 22000.0, 21800.0, 0.92, 0],
            ]
        });
        let entries = parse_movers_dataset(&json).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].security_id, 42);
        assert_eq!(entries[0].exchange_segment_code, 1);
        assert!((entries[0].last_traded_price - 100.5_f32).abs() < f32::EPSILON);
        assert!((entries[0].change_pct - 5.78_f32).abs() < 0.01_f32);
        assert_eq!(entries[0].volume, 10000);
        assert_eq!(entries[1].security_id, 99);
        assert_eq!(entries[1].exchange_segment_code, 0); // IDX_I
    }

    #[test]
    fn test_parse_movers_dataset_skips_short_rows() {
        let json: Value = serde_json::json!({
            "dataset": [
                [42, "E", 100.5, 95.0, 5.78, 10000],
                [99],  // too short, should skip
                [100, "E", 50.0, 49.0, 2.0, 5000],
            ]
        });
        let entries = parse_movers_dataset(&json).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].security_id, 42);
        assert_eq!(entries[1].security_id, 100);
    }

    #[test]
    fn test_parse_movers_dataset_handles_empty() {
        let json: Value = serde_json::json!({ "dataset": [] });
        let entries = parse_movers_dataset(&json).unwrap();
        assert_eq!(entries.len(), 0);
    }

    #[test]
    fn test_parse_movers_dataset_errors_on_missing_dataset_key() {
        let json: Value = serde_json::json!({});
        let result = parse_movers_dataset(&json);
        assert!(result.is_err());
    }

    #[test]
    fn test_segment_string_to_code_maps_dhan_chars() {
        assert_eq!(segment_string_to_code("I"), 0);
        assert_eq!(segment_string_to_code("E"), 1);
        assert_eq!(segment_string_to_code("D"), 2);
        assert_eq!(segment_string_to_code("C"), 3);
        assert_eq!(segment_string_to_code("M"), 5);
        assert_eq!(segment_string_to_code(""), 0); // unknown -> defensive 0
        assert_eq!(segment_string_to_code("Z"), 0); // unknown
    }
}
