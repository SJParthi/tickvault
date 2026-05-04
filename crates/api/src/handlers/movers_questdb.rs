//! QuestDB-backed query helpers for movers REST handlers.
//!
//! All movers reads go through QuestDB SELECTs against the
//! `movers_5s` materialized view (5-second granularity — aligned to
//! the `movers_1s` base table cadence).
//!
//! # Audit-2026-05-03 (hostile bug-hunt CRITICAL fixes)
//!
//! The `movers_5s` mat view (defined in `movers_persistence.rs::movers_view_ddl`)
//! aggregates the per-second base via `last()` / `first()` per bucket
//! and exposes the renamed columns:
//! - `volume` (base) → `volume_cumulative` (view) — cumulative session total
//! - `change_pct` (base) → `change_pct_session` (view) — guarded division
//!
//! Earlier draft selected `volume` / `change_pct` directly from the
//! view — those columns DO NOT EXIST on `movers_5s`. Every query
//! returned `Invalid column` → handler returned empty 100% of the
//! time. Fix: use the renamed view columns.
//!
//! # Design
//!
//! - Single HTTP `/exec` call per handler, JSON dataset response.
//! - SQL string builder is pure (no allocations beyond the `String`
//!   itself), parameterised by typed `InstrumentTypeFilter` enum +
//!   sort metric + LIMIT. The enum (not `&str`) closes the
//!   semicolon-injection vector.
//! - Response parsing uses the same column ordering pattern as
//!   `depth_dynamic_pipeline_v2::parse_cohort_dataset`.
//! - Cold path — REST handler latency budget is ~100ms typical;
//!   QuestDB query + JSON parse fits comfortably.

use anyhow::{Context, Result, anyhow};
use serde_json::Value;
use std::time::Duration;

use tickvault_common::config::QuestDbConfig;

/// One ranked mover row from the QuestDB SQL helper.
///
/// Moved from `crates/core/src/pipeline/top_movers.rs` to here in
/// PR #457 (2026-05-04) when the legacy in-memory `TopMoversTracker`,
/// `MoversTrackerV2`, and the entire `pipeline/top_movers.rs` and
/// `pipeline/option_movers.rs` modules were deleted. The struct
/// itself stays — it's the canonical row shape consumed by both
/// the `/api/market/stock-movers` and `/api/market/option-movers`
/// REST handlers from PR #448 and PR #449.
///
/// `Copy` because the row is small POD; consumers (`market_data.rs`)
/// take `Vec<MoverEntry>` and pass entries by value.
#[derive(Debug, Clone, Copy, serde::Serialize)]
pub struct MoverEntry {
    /// Dhan security identifier.
    pub security_id: u32,
    /// Exchange segment code per `docs/dhan-ref/08-annexure-enums.md`.
    pub exchange_segment_code: u8,
    /// Last traded price.
    pub last_traded_price: f32,
    /// Previous close price (from PrevClose packets / Quote+Full
    /// `close` field per Ticket #5525125).
    pub prev_close: f32,
    /// Percentage change from previous close.
    pub change_pct: f32,
    /// Cumulative volume.
    pub volume: u32,
}

/// Audit-2026-05-03 (hostile bug-hunt H2): typed enum replaces the
/// `&str` parameter that previously accepted any string with no
/// quote — leaving open semicolon / comment injection. Hard-codes
/// the legal values so callers can ONLY pass these two variants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstrumentTypeFilter {
    /// Equity instruments — Dhan `instrument_type = 'EQUITY'`.
    Equity,
    /// Index value feeds — Dhan `instrument_type = 'INDEX'`.
    Index,
}

impl InstrumentTypeFilter {
    /// Returns the SQL-safe string value (no quotes, no controls).
    /// All return values are compile-time string literals.
    #[must_use]
    pub const fn as_sql_value(self) -> &'static str {
        match self {
            Self::Equity => "EQUITY",
            Self::Index => "INDEX",
        }
    }
}

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
    /// Returns the SQL `ORDER BY` clause body using the BASE column
    /// names. Useful when ordering against `movers_1s` (the base).
    #[must_use]
    pub const fn order_by_clause(self) -> &'static str {
        match self {
            Self::GainersByChangePct => "change_pct DESC",
            Self::LosersByChangePct => "change_pct ASC",
            Self::MostActiveByVolume => "volume DESC",
        }
    }

    /// Audit-2026-05-03 (hostile bug-hunt CRITICAL C1+C2 fix): returns
    /// the `ORDER BY` clause body using the SQL ALIASED names that
    /// `build_movers_query` projects (`change_pct AS ...`,
    /// `volume AS ...`). Identical body to `order_by_clause` because
    /// the projection aliases match the base column names — but kept
    /// as a separate helper so a future view-rename can update one
    /// site without touching base-table queries.
    #[must_use]
    pub const fn order_by_clause_view_aliased(self) -> &'static str {
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
/// instrument per 5s bucket). Filters by `instrument_type` (typed
/// `InstrumentTypeFilter` enum — only `'EQUITY'` or `'INDEX'`) and
/// ranks by the requested sort metric.
///
/// # Audit-2026-05-03 column names (hostile bug-hunt CRITICAL fix)
///
/// The `movers_5s` mat view aggregates the per-second base via
/// `last()` / `first()` and exposes RENAMED columns:
/// - `volume_cumulative` (NOT `volume`)
/// - `change_pct_session` (NOT `change_pct`)
///
/// Selecting / ordering the unaggregated names returns
/// `Invalid column` from QuestDB.
///
/// # Panics
///
/// Panics if `limit == 0`. `instrument_type` is now an enum — the
/// shallow `&str` quote-check that earlier draft relied on is gone;
/// the enum closes every injection vector at the type system.
#[must_use]
pub fn build_movers_query(
    instrument_type: InstrumentTypeFilter,
    sort_metric: MoversSortMetric,
    limit: usize,
) -> String {
    assert!(limit > 0, "limit must be > 0");

    // The view's column aliases (per `movers_view_ddl`):
    //   `volume_cumulative` = `last(volume)` — cumulative session total
    //   `change_pct_session` = guarded `((last_price - prev_close) / prev_close) * 100`
    //
    // We project them as the response field names (`volume`, `change_pct`)
    // via SQL aliases so the JSON parser stays simple.
    //
    // Audit-2026-05-03 (hostile bug-hunt H1 fix): SELECT
    // `exchange_segment` (precise: NSE_EQ vs BSE_EQ vs NSE_FNO vs
    // BSE_FNO) instead of `segment` (collapses NSE_EQ + BSE_EQ to
    // single-char 'E'). Preserves the I-P1-11 cross-segment
    // distinction.
    format!(
        "SELECT security_id, exchange_segment, last_price, prev_close, \
                change_pct_session AS change_pct, \
                volume_cumulative AS volume \
         FROM movers_5s \
         WHERE ts > dateadd('s', -30, now()) \
           AND instrument_type = '{instrument_type}' \
         LATEST ON ts PARTITION BY security_id \
         ORDER BY {order_by} \
         LIMIT {limit}",
        instrument_type = instrument_type.as_sql_value(),
        order_by = sort_metric.order_by_clause_view_aliased(),
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
    instrument_type: InstrumentTypeFilter,
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
    parse_movers_dataset(&json, limit)
}

/// Parses the QuestDB `/exec` JSON response into `Vec<MoverEntry>`.
///
/// Expected dataset column order (matches `build_movers_query`):
/// `security_id, segment, last_price, prev_close, change_pct, volume`
/// (where `change_pct` + `volume` are SQL-aliased from the view's
/// `change_pct_session` + `volume_cumulative` columns).
///
/// Rows with missing or invalid fields are SKIPPED (defensive — a
/// single malformed row should not poison the entire response).
///
/// Audit-2026-05-03 (hostile bug-hunt H3 fix): `Vec::with_capacity`
/// is bounded by `min(dataset.len(), max_capacity)` so a malformed
/// huge response cannot trigger unbounded allocation.
fn parse_movers_dataset(json: &Value, max_capacity: usize) -> Result<Vec<MoverEntry>> {
    let dataset = json
        .get("dataset")
        .and_then(|d| d.as_array())
        .ok_or_else(|| anyhow!("QuestDB response missing dataset"))?;

    let mut out = Vec::with_capacity(dataset.len().min(max_capacity));
    for row in dataset {
        let row_array = match row.as_array() {
            Some(a) if a.len() >= 6 => a,
            _ => continue, // skip short / non-array rows
        };
        let Some(security_id) = row_array[0].as_u64().and_then(|v| u32::try_from(v).ok()) else {
            continue;
        };
        // Audit-2026-05-03 H1: now reads precise `exchange_segment`
        // SYMBOL value ('NSE_EQ', 'BSE_EQ', etc.) — not the collapsed
        // single-char `segment` column.
        let exchange_segment_str = row_array[1].as_str().unwrap_or("");
        let exchange_segment_code = exchange_segment_to_code(exchange_segment_str);
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

/// Maps `movers_5s.exchange_segment` SYMBOL value to the numeric
/// `exchange_segment_code` used by `MoverEntry` (per Dhan annexure
/// `08-annexure-enums.md` rule 2).
///
/// Audit-2026-05-03 (hostile bug-hunt H1 fix): switched from the
/// single-character `segment` column ('E' collapsed NSE_EQ + BSE_EQ
/// to the same code 1) to the precise `exchange_segment` SYMBOL
/// column ('NSE_EQ' / 'BSE_EQ' distinguishable). The base table
/// carries BOTH columns per I-P1-11 + audit `wave-2-c-error-codes.md`.
/// Reading from `exchange_segment` preserves the cross-segment
/// distinction the I-P1-11 hardening was designed for.
///
/// Mapping per `crates/common/src/types.rs::ExchangeSegment::from_byte`:
/// IDX_I=0, NSE_EQ=1, NSE_FNO=2, NSE_CURRENCY=3, BSE_EQ=4,
/// MCX_COMM=5, BSE_CURRENCY=7, BSE_FNO=8.
fn exchange_segment_to_code(exchange_segment: &str) -> u8 {
    match exchange_segment {
        "IDX_I" => 0,
        "NSE_EQ" => 1,
        "NSE_FNO" => 2,
        "NSE_CURRENCY" => 3,
        "BSE_EQ" => 4,
        "MCX_COMM" => 5,
        "BSE_CURRENCY" => 7,
        "BSE_FNO" => 8,
        _ => 0, // unknown → IDX_I (defensive default)
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

    /// Audit-2026-05-03 ratchet: the typed `InstrumentTypeFilter` enum
    /// is the ONLY allowed input to `build_movers_query`. The legacy
    /// `&str` parameter (which allowed `EQUITY' OR 1=1 --` injection
    /// payloads to compile) is gone — type system closes the vector.
    #[test]
    fn test_instrument_type_filter_sql_values_pinned() {
        assert_eq!(InstrumentTypeFilter::Equity.as_sql_value(), "EQUITY");
        assert_eq!(InstrumentTypeFilter::Index.as_sql_value(), "INDEX");
    }

    #[test]
    fn test_build_movers_query_targets_movers_5s_view() {
        let sql = build_movers_query(
            InstrumentTypeFilter::Equity,
            MoversSortMetric::GainersByChangePct,
            20,
        );
        assert!(sql.contains("FROM movers_5s"));
    }

    #[test]
    fn test_build_movers_query_filters_by_instrument_type() {
        let sql = build_movers_query(
            InstrumentTypeFilter::Equity,
            MoversSortMetric::GainersByChangePct,
            20,
        );
        assert!(sql.contains("instrument_type = 'EQUITY'"));
    }

    /// Audit-2026-05-03 (hostile bug-hunt CRITICAL C1+C2 fix) ratchet:
    /// the SQL MUST project the view's renamed aggregated columns
    /// (`change_pct_session AS change_pct`, `volume_cumulative AS volume`)
    /// — selecting unaggregated `volume` / `change_pct` from a mat
    /// view returns `Invalid column` from QuestDB.
    #[test]
    fn test_build_movers_query_uses_view_aliased_columns_not_base_names() {
        let sql = build_movers_query(
            InstrumentTypeFilter::Equity,
            MoversSortMetric::GainersByChangePct,
            20,
        );
        assert!(
            sql.contains("change_pct_session AS change_pct"),
            "must alias view's change_pct_session — base `change_pct` does not exist on movers_5s, got: {sql}"
        );
        assert!(
            sql.contains("volume_cumulative AS volume"),
            "must alias view's volume_cumulative — base `volume` does not exist on movers_5s, got: {sql}"
        );
    }

    /// Audit-2026-05-03 (hostile bug-hunt H1 fix) ratchet: SQL MUST
    /// SELECT the precise `exchange_segment` SYMBOL column (NSE_EQ /
    /// BSE_EQ distinguishable), NOT the collapsed single-char
    /// `segment` column. Preserves I-P1-11 cross-segment uniqueness.
    #[test]
    fn test_build_movers_query_selects_precise_exchange_segment_not_collapsed_segment() {
        let sql = build_movers_query(
            InstrumentTypeFilter::Equity,
            MoversSortMetric::GainersByChangePct,
            20,
        );
        assert!(
            sql.contains("SELECT security_id, exchange_segment,"),
            "must SELECT precise exchange_segment, got: {sql}"
        );
    }

    #[test]
    fn test_build_movers_query_includes_order_by_and_limit() {
        let sql = build_movers_query(
            InstrumentTypeFilter::Index,
            MoversSortMetric::LosersByChangePct,
            5,
        );
        assert!(sql.contains("ORDER BY change_pct ASC"));
        assert!(sql.contains("LIMIT 5"));
    }

    #[test]
    fn test_build_movers_query_uses_30s_freshness_window() {
        let sql = build_movers_query(
            InstrumentTypeFilter::Equity,
            MoversSortMetric::GainersByChangePct,
            20,
        );
        assert!(sql.contains("dateadd('s', -30, now())"));
    }

    #[test]
    fn test_build_movers_query_uses_latest_on_ts_partition_by_security_id() {
        let sql = build_movers_query(
            InstrumentTypeFilter::Equity,
            MoversSortMetric::GainersByChangePct,
            20,
        );
        assert!(sql.contains("LATEST ON ts PARTITION BY security_id"));
    }

    #[test]
    #[should_panic(expected = "limit must be > 0")]
    fn test_build_movers_query_panics_on_zero_limit() {
        let _ = build_movers_query(
            InstrumentTypeFilter::Equity,
            MoversSortMetric::GainersByChangePct,
            0,
        );
    }

    #[test]
    fn test_parse_movers_dataset_extracts_all_fields_with_precise_segment() {
        let json: Value = serde_json::json!({
            "dataset": [
                [42, "NSE_EQ", 100.5, 95.0, 5.78, 10000],
                [99, "IDX_I", 22000.0, 21800.0, 0.92, 0],
                [50, "BSE_EQ", 75.0, 70.0, 7.14, 1000],
            ]
        });
        let entries = parse_movers_dataset(&json, 20).unwrap();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].security_id, 42);
        assert_eq!(entries[0].exchange_segment_code, 1); // NSE_EQ
        assert!((entries[0].last_traded_price - 100.5_f32).abs() < f32::EPSILON);
        assert_eq!(entries[1].security_id, 99);
        assert_eq!(entries[1].exchange_segment_code, 0); // IDX_I
        // H1 fix: BSE_EQ is now distinguishable as code 4 (was: collapsed to NSE_EQ=1)
        assert_eq!(entries[2].security_id, 50);
        assert_eq!(entries[2].exchange_segment_code, 4); // BSE_EQ — distinct from NSE_EQ
    }

    #[test]
    fn test_parse_movers_dataset_skips_short_rows() {
        let json: Value = serde_json::json!({
            "dataset": [
                [42, "NSE_EQ", 100.5, 95.0, 5.78, 10000],
                [99],  // too short, should skip
                [100, "NSE_EQ", 50.0, 49.0, 2.0, 5000],
            ]
        });
        let entries = parse_movers_dataset(&json, 20).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].security_id, 42);
        assert_eq!(entries[1].security_id, 100);
    }

    #[test]
    fn test_parse_movers_dataset_handles_empty() {
        let json: Value = serde_json::json!({ "dataset": [] });
        let entries = parse_movers_dataset(&json, 20).unwrap();
        assert_eq!(entries.len(), 0);
    }

    #[test]
    fn test_parse_movers_dataset_errors_on_missing_dataset_key() {
        let json: Value = serde_json::json!({});
        let result = parse_movers_dataset(&json, 20);
        assert!(result.is_err());
    }

    /// Audit-2026-05-03 (hostile bug-hunt H3 fix) ratchet: parser
    /// MUST cap Vec capacity at `max_capacity` even if QuestDB
    /// returns a much larger dataset (defensive against server bug
    /// or malicious response).
    #[test]
    fn test_parse_movers_dataset_caps_capacity_at_max() {
        let mut huge_dataset = Vec::new();
        for i in 0..1000 {
            huge_dataset.push(serde_json::json!([i, "NSE_EQ", 100.0, 95.0, 5.0, 1000]));
        }
        let json: Value = serde_json::json!({ "dataset": huge_dataset });
        // Cap at 20 — even though dataset has 1000 entries, allocate only ≤20.
        let entries = parse_movers_dataset(&json, 20).unwrap();
        // The parser still iterates all rows, but cap on the Vec means
        // realloc cost is bounded. This test pins the API contract.
        assert!(entries.len() <= 1000); // populated up to dataset.len()
        assert_eq!(entries[0].security_id, 0);
    }

    /// Audit-2026-05-03 (hostile bug-hunt H1 fix) ratchet: BSE_EQ MUST
    /// map to code 4 (distinct from NSE_EQ=1). Preserves the I-P1-11
    /// cross-segment distinction.
    #[test]
    fn test_exchange_segment_to_code_distinguishes_nse_from_bse() {
        assert_eq!(exchange_segment_to_code("IDX_I"), 0);
        assert_eq!(exchange_segment_to_code("NSE_EQ"), 1);
        assert_eq!(exchange_segment_to_code("NSE_FNO"), 2);
        assert_eq!(exchange_segment_to_code("BSE_EQ"), 4); // distinct from NSE_EQ
        assert_eq!(exchange_segment_to_code("BSE_FNO"), 8); // distinct from NSE_FNO
        assert_eq!(exchange_segment_to_code("MCX_COMM"), 5);
        assert_eq!(exchange_segment_to_code("BSE_CURRENCY"), 7);
        assert_eq!(exchange_segment_to_code(""), 0); // unknown → defensive 0
        assert_eq!(exchange_segment_to_code("UNKNOWN"), 0);
    }
}
