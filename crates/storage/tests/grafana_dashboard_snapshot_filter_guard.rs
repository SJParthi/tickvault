//! I-P1-08 mechanical enforcement — Grafana dashboard lifecycle filter guard.
//!
//! Scans every `.json` file under `deploy/docker/grafana/dashboards/` and
//! verifies that ANY SQL query referencing a lifecycle snapshot table
//! (`fno_underlyings`, `derivative_contracts`, `subscribed_indices`)
//! includes the mandatory `WHERE status = 'active'` filter (or an explicit
//! `status IN (...)` / `status = 'expired'` for lifecycle-history panels).
//!
//! # Why this exists
//!
//! As of the 2026-04-17 redesign, these 3 tables hold a single lifecycle row
//! per instrument forever — `status` ∈ {`active`, `expired`}. Operational
//! dashboard panels (counts, universe views) MUST filter on status or they
//! will include expired contracts from years past and show inflated counts.
//!
//! `instrument_build_metadata` is exempt — it intentionally accumulates one
//! row per daily build and has its own timestamp-based filter rules.
//!
//! # Rule
//!
//! For each dashboard JSON file:
//!   1. Extract every `"rawSql": "..."` value.
//!   2. For each SQL, check if it references a lifecycle table.
//!   3. If yes, require a `status` clause in the query.
//!
//! Test failure → commit blocked. The fix is to add `WHERE status = 'active'`
//! (or an explicit status filter) to the offending query.

#![cfg(test)]

use std::path::{Path, PathBuf};

/// Lifecycle tables — queries on these must filter on `status`.
const LIFECYCLE_TABLES: &[&str] = &[
    "fno_underlyings",
    "derivative_contracts",
    "subscribed_indices",
];

/// Build-history table — cross-day accumulation by design, exempt from the
/// lifecycle filter rule. Historical build panels use `ORDER BY timestamp DESC
/// LIMIT N` instead.
const BUILD_HISTORY_TABLE: &str = "instrument_build_metadata";

/// Absolute path to the Grafana dashboards directory, rooted at workspace root.
fn dashboards_dir() -> PathBuf {
    // CARGO_MANIFEST_DIR = crates/storage, workspace root = two levels up.
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest_dir
        .parent()
        .and_then(Path::parent)
        .expect("workspace root must exist")
        .join("deploy/docker/grafana/dashboards")
}

/// Read every `.json` file under the dashboards directory and return
/// `(file_name, content)` tuples.
fn read_all_dashboards() -> Vec<(String, String)> {
    let dir = dashboards_dir();
    assert!(
        dir.is_dir(),
        "dashboards directory not found: {}",
        dir.display()
    );

    let mut out = Vec::new();
    let entries = std::fs::read_dir(&dir).expect("failed to read dashboards dir");
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) == Some("json") {
            let file_name = path
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("<unknown>")
                .to_owned();
            let content = std::fs::read_to_string(&path)
                .unwrap_or_else(|err| panic!("failed to read {}: {err}", path.display()));
            out.push((file_name, content));
        }
    }
    assert!(!out.is_empty(), "no dashboard JSON files found");
    out
}

/// Extract every `"rawSql": "<sql>"` value from a dashboard JSON.
/// Returns the SQL strings (JSON-unescaped).
fn extract_raw_sql_queries(json: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = json;
    let needle = "\"rawSql\":";
    while let Some(idx) = rest.find(needle) {
        let after = &rest[idx + needle.len()..];
        // Skip whitespace to the opening quote.
        let start_quote = match after.find('"') {
            Some(p) => p + 1,
            None => break,
        };
        let body = &after[start_quote..];
        // Find the closing quote, handling backslash escapes.
        let mut end = 0;
        let bytes = body.as_bytes();
        while end < bytes.len() {
            if bytes[end] == b'\\' && end + 1 < bytes.len() {
                end += 2;
                continue;
            }
            if bytes[end] == b'"' {
                break;
            }
            end += 1;
        }
        if end >= bytes.len() {
            break;
        }
        let raw = &body[..end];
        // JSON-unescape the common sequences we care about.
        let sql = raw
            .replace("\\n", "\n")
            .replace("\\\"", "\"")
            .replace("\\\\", "\\");
        out.push(sql);
        rest = &body[end..];
    }
    out
}

/// Returns the lifecycle table name referenced by a SQL query, if any.
fn lifecycle_table_referenced(sql: &str) -> Option<&'static str> {
    let lower = sql.to_lowercase();
    for &table in LIFECYCLE_TABLES {
        let needle = format!("from {table}");
        if lower.contains(&needle) {
            return Some(table);
        }
    }
    None
}

/// Returns true if the SQL contains an explicit `status` predicate.
fn has_status_filter(sql: &str) -> bool {
    // Normalise whitespace. Accept any of:
    //   status = 'active'
    //   status='active'
    //   status = 'expired'
    //   status in (...)
    //   status <> 'expired'
    let lower = sql.to_lowercase().replace('\n', " ");
    let normalized: String = lower.split_whitespace().collect::<Vec<_>>().join(" ");
    normalized.contains("status =")
        || normalized.contains("status=")
        || normalized.contains("status in ")
        || normalized.contains("status in(")
        || normalized.contains("status <>")
        || normalized.contains("status!=")
        || normalized.contains("status !=")
}

// ============================================================================
// I-P1-08 — Dashboard lifecycle filter guard
// ============================================================================

#[test]
fn dashboard_lifecycle_queries_must_include_status_filter() {
    let dashboards = read_all_dashboards();
    let mut violations: Vec<String> = Vec::new();

    for (file, json) in &dashboards {
        let queries = extract_raw_sql_queries(json);
        for (idx, sql) in queries.iter().enumerate() {
            if let Some(table) = lifecycle_table_referenced(sql) {
                // Skip template variable queries that use UNION ALL across tables.
                let sql_lower = sql.to_lowercase();
                let is_template_var = sql_lower.contains("__text")
                    && sql_lower.contains("__value")
                    && sql_lower.contains("union all");
                if is_template_var {
                    continue;
                }
                if !has_status_filter(sql) {
                    violations.push(format!(
                        "[{file}] query #{} references `{table}` without \
                         an explicit `status` filter (e.g. `WHERE status = 'active'`):\n    {}",
                        idx,
                        sql.replace('\n', " ").chars().take(200).collect::<String>()
                    ));
                }
            }
        }
    }

    assert!(
        violations.is_empty(),
        "I-P1-08 violation — Grafana dashboard queries on lifecycle tables \
         MUST include an explicit `status` filter (`status = 'active'` for \
         operational views). Without it, expired contracts from years past \
         inflate the result. Fix by adding the filter to each query below.\n\n{}\n\n\
         Rule: .claude/rules/project/gap-enforcement.md (I-P1-08).",
        violations.join("\n\n")
    );
}

// ============================================================================
// Self-tests for the helper functions
// ============================================================================

#[test]
fn self_test_lifecycle_table_referenced_detects_fno_underlyings() {
    let sql = "SELECT count() FROM fno_underlyings;";
    assert_eq!(lifecycle_table_referenced(sql), Some("fno_underlyings"));
}

#[test]
fn self_test_lifecycle_table_referenced_detects_derivative_contracts() {
    let sql = "SELECT * FROM derivative_contracts ORDER BY security_id;";
    assert_eq!(
        lifecycle_table_referenced(sql),
        Some("derivative_contracts")
    );
}

#[test]
fn self_test_lifecycle_table_referenced_detects_subscribed_indices() {
    let sql = "select count() from subscribed_indices;";
    assert_eq!(lifecycle_table_referenced(sql), Some("subscribed_indices"));
}

#[test]
fn self_test_lifecycle_table_referenced_ignores_non_lifecycle_tables() {
    let sql = "SELECT * FROM ticks WHERE security_id = 13;";
    assert_eq!(lifecycle_table_referenced(sql), None);
    let sql2 = "SELECT * FROM historical_candles WHERE timeframe = '1m';";
    assert_eq!(lifecycle_table_referenced(sql2), None);
}

#[test]
fn self_test_lifecycle_table_referenced_ignores_build_metadata() {
    // instrument_build_metadata is exempt — build history is cross-day by design.
    let sql = format!("SELECT * FROM {BUILD_HISTORY_TABLE};");
    assert_eq!(lifecycle_table_referenced(&sql), None);
}

#[test]
fn self_test_has_status_filter_accepts_equals_active() {
    let sql = "SELECT count() FROM fno_underlyings WHERE status = 'active';";
    assert!(has_status_filter(sql));
}

#[test]
fn self_test_has_status_filter_accepts_no_space_equals() {
    let sql = "SELECT count() FROM fno_underlyings WHERE status='active';";
    assert!(has_status_filter(sql));
}

#[test]
fn self_test_has_status_filter_accepts_in_clause() {
    let sql = "SELECT * FROM derivative_contracts WHERE status IN ('active', 'expired');";
    assert!(has_status_filter(sql));
}

#[test]
fn self_test_has_status_filter_rejects_missing_filter() {
    let sql = "SELECT count() FROM fno_underlyings;";
    assert!(!has_status_filter(sql));
}

#[test]
fn self_test_extract_raw_sql_queries_finds_all() {
    let json = r#"{
        "targets": [
            { "rawSql": "SELECT count() FROM ticks;" },
            { "rawSql": "SELECT * FROM fno_underlyings WHERE status = 'active';" }
        ]
    }"#;
    let queries = extract_raw_sql_queries(json);
    assert_eq!(queries.len(), 2);
    assert!(queries[0].contains("ticks"));
    assert!(queries[1].contains("fno_underlyings"));
}

#[test]
fn self_test_extract_raw_sql_queries_handles_escaped_newlines() {
    let json = r#"{ "rawSql": "SELECT a,\n  b\nFROM fno_underlyings\nWHERE status = 'active';" }"#;
    let queries = extract_raw_sql_queries(json);
    assert_eq!(queries.len(), 1);
    assert!(queries[0].contains("FROM fno_underlyings"));
    assert!(queries[0].contains('\n'));
}
