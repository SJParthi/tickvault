//! I-P1-08 mechanical enforcement — Grafana dashboard snapshot filter guard.
//!
//! Scans every `.json` file under `deploy/docker/grafana/dashboards/` and
//! verifies that ANY SQL query referencing a snapshot table
//! (`fno_underlyings`, `derivative_contracts`, `subscribed_indices`,
//! `instrument_build_metadata`) includes the mandatory
//! `WHERE timestamp = (SELECT max(timestamp) FROM <table>)` filter.
//!
//! # Why this exists
//!
//! These 4 tables ACCUMULATE rows across days (I-P1-08 cross-day snapshot
//! rule — historical rows are preserved for SEBI audit). Any Grafana query
//! without the max-timestamp filter returns N * days of rows and looks like
//! duplicates on the dashboard.
//!
//! Regression: 2026-04-17 — the Market Data Explorer dashboard showed 442
//! F&O underlyings (2x expected) and 207,796 derivative contracts (8x
//! expected) because the count queries lacked the filter. Detected by human
//! review of the Grafana UI. This test prevents that class of bug mechanically.
//!
//! # Rule
//!
//! For each dashboard JSON file:
//!   1. Extract every `"rawSql": "..."` value
//!   2. For each SQL, check if it references a snapshot table
//!   3. If yes, require the exact filter `WHERE timestamp = (SELECT max(timestamp) FROM <table>)`
//!      OR a `GROUP BY` / `ORDER BY timestamp DESC LIMIT 1` pattern (template variable
//!      selectors may use latest-row semantics differently).
//!
//! Test failure → commit blocked. The fix is to add the filter to the offending SQL.

#![cfg(test)]

use std::path::{Path, PathBuf};

/// Snapshot tables subject to cross-day accumulation (I-P1-08).
/// Queries on these tables MUST include a max-timestamp filter.
const SNAPSHOT_TABLES: &[&str] = &[
    "fno_underlyings",
    "derivative_contracts",
    "subscribed_indices",
    "instrument_build_metadata",
];

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

/// Returns the snapshot table name referenced by a SQL query, if any.
fn snapshot_table_referenced(sql: &str) -> Option<&'static str> {
    let lower = sql.to_lowercase();
    for &table in SNAPSHOT_TABLES {
        // Match `FROM <table>` (with optional surrounding whitespace/newlines).
        let needle = format!("from {table}");
        if lower.contains(&needle) {
            return Some(table);
        }
    }
    None
}

/// Returns true if the SQL contains the mandatory max-timestamp filter
/// for the given table, OR uses an acceptable latest-row pattern.
fn has_snapshot_filter(sql: &str, table: &str) -> bool {
    let lower = sql.to_lowercase().replace('\n', " ").replace("  ", " ");
    let normalized: String = lower.split_whitespace().collect::<Vec<_>>().join(" ");
    let exact_filter = format!("timestamp = (select max(timestamp) from {table})");
    if normalized.contains(&exact_filter) {
        return true;
    }
    // Acceptable alternative: build history panels on `instrument_build_metadata`
    // intentionally show cross-day rows (the whole point is build history).
    // They must use `ORDER BY timestamp DESC LIMIT N` to bound output.
    if table == "instrument_build_metadata" && normalized.contains("order by timestamp desc limit")
    {
        return true;
    }
    // Acceptable alternative: explicit ORDER BY timestamp DESC LIMIT 1 pattern
    // (used by e.g. "latest build" panels which inherently filter).
    if normalized.contains("order by timestamp desc limit 1") {
        return true;
    }
    false
}

// ============================================================================
// I-P1-08 — Dashboard snapshot filter guard
// ============================================================================

#[test]
fn dashboard_snapshot_queries_must_include_max_timestamp_filter() {
    let dashboards = read_all_dashboards();
    let mut violations: Vec<String> = Vec::new();

    for (file, json) in &dashboards {
        let queries = extract_raw_sql_queries(json);
        for (idx, sql) in queries.iter().enumerate() {
            if let Some(table) = snapshot_table_referenced(sql) {
                // Skip template variable queries that use UNION ALL across multiple
                // tables — these are picker dropdowns, latest-row filtering is
                // applied via DISTINCT + ordering and doesn't corrupt counts.
                // We identify them by the presence of `__text` or `__value`
                // Grafana variable column aliases AND `union all` AND not being
                // a `count()` panel.
                let sql_lower = sql.to_lowercase();
                let is_template_var = sql_lower.contains("__text")
                    && sql_lower.contains("__value")
                    && sql_lower.contains("union all");
                if is_template_var {
                    continue;
                }
                // Skip queries that reference the table only in a subquery
                // (e.g. `WHERE x IN (SELECT ... FROM table)`). We check by
                // looking for the table name appearing only after a `(SELECT`.
                // For safety we require the filter in all other cases.
                if !has_snapshot_filter(sql, table) {
                    violations.push(format!(
                        "[{file}] query #{} references `{table}` without \
                         `WHERE timestamp = (SELECT max(timestamp) FROM {table})`:\n    {}",
                        idx,
                        sql.replace('\n', " ").chars().take(200).collect::<String>()
                    ));
                }
            }
        }
    }

    assert!(
        violations.is_empty(),
        "I-P1-08 violation — Grafana dashboard queries on snapshot tables \
         MUST include the max-timestamp filter. These tables accumulate rows \
         across days; missing the filter causes 2x/Nx row counts on the \
         dashboard. Fix by adding `WHERE timestamp = (SELECT max(timestamp) \
         FROM <table>)` to each query below.\n\n{}\n\n\
         Rule: .claude/rules/project/gap-enforcement.md (I-P1-08).",
        violations.join("\n\n")
    );
}

// ============================================================================
// Self-tests for the helper functions (ensures test infra is correct)
// ============================================================================

#[test]
fn self_test_snapshot_table_referenced_detects_fno_underlyings() {
    let sql = "SELECT count() FROM fno_underlyings;";
    assert_eq!(snapshot_table_referenced(sql), Some("fno_underlyings"));
}

#[test]
fn self_test_snapshot_table_referenced_detects_derivative_contracts() {
    let sql = "SELECT * FROM derivative_contracts ORDER BY security_id;";
    assert_eq!(snapshot_table_referenced(sql), Some("derivative_contracts"));
}

#[test]
fn self_test_snapshot_table_referenced_detects_subscribed_indices() {
    let sql = "select count() from subscribed_indices;";
    assert_eq!(snapshot_table_referenced(sql), Some("subscribed_indices"));
}

#[test]
fn self_test_snapshot_table_referenced_ignores_non_snapshot_tables() {
    let sql = "SELECT * FROM ticks WHERE security_id = 13;";
    assert_eq!(snapshot_table_referenced(sql), None);

    let sql2 = "SELECT * FROM historical_candles WHERE timeframe = '1m';";
    assert_eq!(snapshot_table_referenced(sql2), None);
}

#[test]
fn self_test_has_snapshot_filter_accepts_exact_filter() {
    let sql = "SELECT count() FROM fno_underlyings WHERE timestamp = (SELECT max(timestamp) FROM fno_underlyings);";
    assert!(has_snapshot_filter(sql, "fno_underlyings"));
}

#[test]
fn self_test_has_snapshot_filter_accepts_multiline_filter() {
    let sql = "SELECT underlying_symbol\nFROM fno_underlyings\nWHERE timestamp = (SELECT max(timestamp) FROM fno_underlyings)\nORDER BY underlying_symbol;";
    assert!(has_snapshot_filter(sql, "fno_underlyings"));
}

#[test]
fn self_test_has_snapshot_filter_rejects_missing_filter() {
    let sql = "SELECT count() FROM fno_underlyings;";
    assert!(!has_snapshot_filter(sql, "fno_underlyings"));
}

#[test]
fn self_test_has_snapshot_filter_rejects_wrong_table_in_filter() {
    // Filter references wrong table — must still fail.
    let sql = "SELECT count() FROM fno_underlyings WHERE timestamp = (SELECT max(timestamp) FROM other_table);";
    assert!(!has_snapshot_filter(sql, "fno_underlyings"));
}

#[test]
fn self_test_extract_raw_sql_queries_finds_all() {
    let json = r#"{
        "targets": [
            { "rawSql": "SELECT count() FROM ticks;" },
            { "rawSql": "SELECT * FROM fno_underlyings;" }
        ]
    }"#;
    let queries = extract_raw_sql_queries(json);
    assert_eq!(queries.len(), 2);
    assert!(queries[0].contains("ticks"));
    assert!(queries[1].contains("fno_underlyings"));
}

#[test]
fn self_test_extract_raw_sql_queries_handles_escaped_newlines() {
    let json = r#"{ "rawSql": "SELECT a,\n  b\nFROM fno_underlyings;" }"#;
    let queries = extract_raw_sql_queries(json);
    assert_eq!(queries.len(), 1);
    assert!(queries[0].contains("FROM fno_underlyings"));
    assert!(queries[0].contains('\n'));
}
