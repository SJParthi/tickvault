//! I-P1-08 + I-P1-11 mechanical enforcement — Grafana dashboard filter guards.
//!
//! Two distinct guards ride this file:
//!
//! * **I-P1-08** — any query on a lifecycle snapshot table
//!   (`fno_underlyings`, `derivative_contracts`, `subscribed_indices`) must
//!   include an explicit `status` filter.
//! * **I-P1-11 (2026-04-17)** — any `count_distinct(security_id)` aggregation
//!   on a cross-segment table (`ticks`, `market_depth`, `deep_market_depth`,
//!   live candles, `historical_candles`) MUST also qualify by segment.
//!   Without the segment qualifier, a collision like NIFTY IDX_I id=13 and
//!   an NSE_EQ stock id=13 collapses into ONE distinct id in the count,
//!   silently hiding missing-subscription bugs on the operator dashboard.
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

use std::collections::BTreeSet;
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

/// I-P1-11: cross-segment data tables where `count_distinct(security_id)`
/// alone under-counts when Dhan reuses a numeric id across segments.
/// Any aggregation over one of these tables MUST include `segment` (or
/// `exchange_segment`) alongside `security_id` in the distinct clause.
/// Audit finding #5 (2026-04-17): critical metrics that MUST appear on
/// at least one dashboard panel so the operator sees them in Grafana.
/// Every entry here must be a prefix (or exact name) that we scan the
/// Grafana dashboard JSON for. A missing entry = silent dashboard gap
/// = operator can't triage without SSH into Prometheus.
///
/// The rule is deliberately PREFIX-based so derived label variants
/// (e.g. `tv_depth_20lvl_packets_total` matches `tv_depth_`) don't
/// force per-variant updates.
const REQUIRED_DASHBOARD_METRIC_PREFIXES: &[&str] = &[
    // Depth feed health — 20-level, 200-level, rebalancer.
    "tv_depth_",
    // QuestDB connectivity (commits e05e66c, earlier).
    "tv_questdb_",
    // Main feed WebSocket pool health.
    "tv_pool_",
    // Tick persistence write path (catches the WARN→ERROR fixes
    // shipped in audit finding #2 — need a panel to see flush errors).
    "tv_tick_flush_errors_total",
    // I-P1-11 cross-segment collision detector (commit d049bd6).
    "tv_instrument_registry_cross_segment_collisions",
    // Plan item M (2026-04-22): Phase 2 daily scheduler health panels
    // on operator-health dashboard. Prefix matches both
    // tv_phase2_runs_total and tv_phase2_preopen_buffer_entries and
    // tv_phase2_trigger_latency_ms.
    "tv_phase2_",
    // Plan item F2 (2026-04-22): 6-bucket movers dashboard panels on
    // market-movers.json. Prefix matches tv_movers_snapshot_duration_ms
    // and tv_movers_tracked_total.
    "tv_movers_",
];

const CROSS_SEGMENT_TABLES: &[&str] = &[
    "ticks",
    "market_depth",
    "deep_market_depth",
    "candles_1m",
    "candles_5m",
    "candles_15m",
    "candles_60m",
    "candles_1d",
    "historical_candles",
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

/// I-P1-11: returns the cross-segment table referenced by this SQL if any.
fn cross_segment_table_referenced(sql: &str) -> Option<&'static str> {
    let lower = sql.to_lowercase();
    for &table in CROSS_SEGMENT_TABLES {
        let needle = format!("from {table}");
        if lower.contains(&needle) {
            return Some(table);
        }
    }
    None
}

/// I-P1-11: returns true if the SQL uses `count_distinct(security_id)` or
/// `count(DISTINCT security_id)` without pairing with `segment` /
/// `exchange_segment` inside the same distinct expression.
///
/// Catches the exact dashboard bug-class from 2026-04-17: the Grafana
/// panel `SELECT count_distinct(security_id) AS total FROM ticks` would
/// report 1 instead of 2 when NIFTY IDX_I id=13 and some NSE_EQ id=13
/// both have ticks.
fn distinct_security_id_missing_segment(sql: &str) -> bool {
    let normalized = sql
        .to_lowercase()
        .replace('\n', " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");

    // Find any `count_distinct(...)` or `count(distinct ...)` expression
    // and check its body for security_id without segment.
    let mut offending = false;
    for pattern in ["count_distinct(", "count(distinct "] {
        let mut search_from = 0;
        while let Some(idx) = normalized[search_from..].find(pattern) {
            let body_start = search_from + idx + pattern.len();
            // Find matching close-paren — assumes no nested parens.
            let rest = &normalized[body_start..];
            let close = match rest.find(')') {
                Some(p) => p,
                None => break,
            };
            let body = &rest[..close];
            if body.contains("security_id") && !body.contains("segment") {
                offending = true;
            }
            search_from = body_start + close;
        }
    }
    offending
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
// I-P1-11 — Dashboard segment-aware distinct-security_id guard
// ============================================================================

#[test]
fn dashboard_distinct_security_id_must_include_segment() {
    let dashboards = read_all_dashboards();
    let mut violations: Vec<String> = Vec::new();

    for (file, json) in &dashboards {
        let queries = extract_raw_sql_queries(json);
        for (idx, sql) in queries.iter().enumerate() {
            if let Some(table) = cross_segment_table_referenced(sql)
                && distinct_security_id_missing_segment(sql)
            {
                violations.push(format!(
                    "[{file}] query #{} on cross-segment table `{table}` uses \
                     `count_distinct(security_id)` without including `segment` in \
                     the distinct expression. When Dhan reuses a numeric id across \
                     segments (e.g. NIFTY IDX_I id=13 and NSE_EQ id=13), the count \
                     collapses the two into one. Fix by using \
                     `count(DISTINCT security_id || '|' || segment)` or \
                     `count(*) FROM (SELECT DISTINCT security_id, segment FROM {table})`.\n    {}",
                    idx,
                    sql.replace('\n', " ").chars().take(200).collect::<String>()
                ));
            }
        }
    }

    assert!(
        violations.is_empty(),
        "I-P1-11 violation — Grafana dashboard `count_distinct(security_id)` \
         aggregations on cross-segment tables MUST also include `segment` in the \
         distinct clause. Without it, cross-segment id collisions silently \
         under-count instruments.\n\n{}\n\n\
         Rule: .claude/rules/project/security-id-uniqueness.md (I-P1-11).",
        violations.join("\n\n")
    );
}

// ============================================================================
// Audit finding #5 — critical metrics must appear on at least one dashboard
// ============================================================================

/// Ratchet allow-list: metrics KNOWN to be missing from dashboards.
/// The test FAILS if this list grows — adding a new required metric
/// without a dashboard panel is blocked. Items should be REMOVED
/// from this list as operator adds dashboard panels; each removal is
/// a one-way ratchet improvement.
///
/// Empty as of 2026-04-17 commit that shipped `tv-health.json` —
/// all 4 previously-missing prefixes now have panels on that
/// dashboard. Adding a new required metric requires adding a
/// panel (or re-populating this list if tech debt is acceptable).
const KNOWN_DASHBOARD_GAPS: &[&str] = &[];

#[test]
fn critical_metric_prefixes_appear_on_at_least_one_dashboard() {
    let dashboards = read_all_dashboards();
    // Concatenate every dashboard's raw JSON so we can scan for substring
    // matches. This matches metric names referenced anywhere in PromQL
    // expressions, panel titles, legend formats, etc.
    let all_content: String = dashboards
        .iter()
        .map(|(_, content)| content.as_str())
        .collect::<Vec<_>>()
        .join("\n");

    let mut missing: Vec<&str> = Vec::new();
    for &prefix in REQUIRED_DASHBOARD_METRIC_PREFIXES {
        if !all_content.contains(prefix) {
            // Allow the known-gap ratchet; any NEW gap blocks the build.
            if KNOWN_DASHBOARD_GAPS.contains(&prefix) {
                continue;
            }
            missing.push(prefix);
        }
    }

    assert!(
        missing.is_empty(),
        "Audit finding #5 violation — the following critical metric \
         prefix(es) are emitted by the app but have NO Grafana dashboard \
         panel referencing them, AND they are not in the known-gap \
         ratchet. Operators cannot triage the underlying subsystem from \
         the dashboards. Fix by adding at least one panel per prefix to \
         `deploy/docker/grafana/dashboards/*.json`.\n\n\
         Missing prefixes: {:?}\n\n\
         (Or, if the prefix is intentionally retired, remove it from \
         REQUIRED_DASHBOARD_METRIC_PREFIXES in this test.)",
        missing
    );
}

/// Guard that the known-gap list can only shrink (ratchet). If an operator
/// adds a panel for a known gap, they MUST remove it from the list — this
/// test enforces that the list doesn't accumulate stale entries.
#[test]
fn known_dashboard_gaps_must_actually_be_missing() {
    let dashboards = read_all_dashboards();
    let all_content: String = dashboards
        .iter()
        .map(|(_, content)| content.as_str())
        .collect::<Vec<_>>()
        .join("\n");

    let mut stale_gaps: Vec<&str> = Vec::new();
    for &prefix in KNOWN_DASHBOARD_GAPS {
        if all_content.contains(prefix) {
            stale_gaps.push(prefix);
        }
    }

    assert!(
        stale_gaps.is_empty(),
        "Ratchet violation — the following metric prefix(es) are in \
         KNOWN_DASHBOARD_GAPS but actually DO appear on a dashboard. \
         Remove them from the list to tighten the ratchet:\n\n\
         Stale gaps: {:?}",
        stale_gaps
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
fn self_test_cross_segment_table_detects_ticks() {
    let sql = "SELECT count_distinct(security_id) FROM ticks;";
    assert_eq!(cross_segment_table_referenced(sql), Some("ticks"));
}

#[test]
fn self_test_cross_segment_table_detects_historical_candles() {
    let sql = "SELECT count() FROM historical_candles WHERE timeframe='1m';";
    assert_eq!(
        cross_segment_table_referenced(sql),
        Some("historical_candles")
    );
}

#[test]
fn self_test_cross_segment_table_ignores_lifecycle_tables() {
    let sql = "SELECT count() FROM fno_underlyings WHERE status = 'active';";
    assert_eq!(cross_segment_table_referenced(sql), None);
}

#[test]
fn self_test_distinct_security_id_flags_bare_count_distinct() {
    let sql = "SELECT count_distinct(security_id) AS total FROM ticks;";
    assert!(
        distinct_security_id_missing_segment(sql),
        "bare count_distinct(security_id) must be flagged"
    );
}

#[test]
fn self_test_distinct_security_id_flags_sql_distinct_form() {
    let sql = "SELECT count(DISTINCT security_id) AS total FROM ticks;";
    assert!(distinct_security_id_missing_segment(sql));
}

#[test]
fn self_test_distinct_security_id_accepts_segment_qualified() {
    let sql = "SELECT count_distinct(security_id || '|' || segment) FROM ticks;";
    assert!(!distinct_security_id_missing_segment(sql));
}

#[test]
fn self_test_distinct_security_id_accepts_wrapped_distinct() {
    let sql = "SELECT count(*) FROM (SELECT DISTINCT security_id, segment FROM ticks);";
    // No count_distinct on top level — our regex doesn't fire; this is fine
    // because the inner SELECT DISTINCT includes segment.
    assert!(!distinct_security_id_missing_segment(sql));
}

#[test]
fn self_test_distinct_security_id_ignores_unrelated_aggregations() {
    let sql = "SELECT count() FROM ticks;";
    assert!(!distinct_security_id_missing_segment(sql));
    let sql2 = "SELECT max(ltp) FROM ticks;";
    assert!(!distinct_security_id_missing_segment(sql2));
}

#[test]
fn self_test_extract_raw_sql_queries_handles_escaped_newlines() {
    let json = r#"{ "rawSql": "SELECT a,\n  b\nFROM fno_underlyings\nWHERE status = 'active';" }"#;
    let queries = extract_raw_sql_queries(json);
    assert_eq!(queries.len(), 1);
    assert!(queries[0].contains("FROM fno_underlyings"));
    assert!(queries[0].contains('\n'));
}

// ============================================================================
// Rule 12 (audit-findings-2026-04-17.md) — Dashboard counter wrapper guard
// ============================================================================
//
// Any Prometheus *counter* shown as a raw value (not `rate()`/`increase()`/
// `delta()`/`irate()`) lies to the operator after the process restarts:
// the counter resets to 0 and the panel reads "0" while drops are actively
// happening. The 2026-04-24 audit finding #5 (PR #346) wrapped two counters
// (`tv_ticks_dropped_total`, `tv_ticks_spilled_total`) in `increase(...[5m])`.
// This guard prevents the regression class.
//
// Strategy:
//   1. Auto-discover counter names from the source tree by scanning
//      `metrics::counter!("tv_xxx")` and `counter!("tv_xxx")` invocations
//      across `crates/**/src/**/*.rs`.  Source is single source of truth —
//      gauges (which legitimately appear bare) are never picked up.
//   2. For each Grafana dashboard JSON, extract every `"expr": "..."`
//      string (Prometheus expressions, distinct from `rawSql` SQL strings).
//   3. For each occurrence of a discovered counter name in an expr,
//      check whether its IMMEDIATELY enclosing function call is one of
//      `rate` / `increase` / `delta` / `irate`. Aggregations like
//      `sum(...)` or `topk(...)` are NOT enough on their own — they
//      aggregate raw counter values which still go to 0 after restart.
//   4. Bare counter usage = violation. KNOWN_BARE_COUNTERS is a ratchet
//      allowlist that can only shrink (each removal = one panel fixed).
//
// Why a ratchet: dashboards have ~30 historical bare-counter panels. We
// fix them in waves and tighten the allowlist each PR. The companion
// `known_bare_counter_entries_must_actually_be_bare` test ensures stale
// entries cannot accumulate — the moment a dashboard is fixed, the
// matching allowlist entry MUST be removed in the same PR.

/// Allowed wrapper functions whose immediate body sanitises a counter
/// against process restarts.  `rate()`/`irate()`/`increase()`/`delta()`
/// all treat counter resets as zero and emit a per-second / per-window
/// value that survives restart.
const COUNTER_RESET_SAFE_WRAPPERS: &[&str] = &["rate", "irate", "increase", "delta"];

/// Ratchet allowlist of `(dashboard_file, counter_metric)` pairs that are
/// currently rendered as bare counters. Each entry is a known papercut.
/// **The list can only shrink.** Adding a new bare counter is blocked.
/// Removing an entry must be paired with an actual dashboard fix in the
/// same commit (enforced by `known_bare_counter_entries_must_actually_be_bare`).
///
/// Format: `(dashboard_filename, counter_metric_name)`. A single entry
/// covers ALL panels in that file using that counter — operator may have
/// multiple panels in one dashboard for the same counter, all need wrapping
/// at once.
const KNOWN_BARE_COUNTERS: &[(&str, &str)] = &[
    // depth-flow.json — FIXED in this commit, all entries removed.
    // (orphan `tv_depth_stale_spot_total` is a dashboard-only reference
    // with no source emission, so the discoverer never sees it.)
    // tv-health.json — FIXED in this commit, all 9 counter panels wrapped
    // in `increase(...[5m])`.
    // trading-pipeline.json — FIXED in this commit. All 14 raw counters
    // wrapped in `increase(...[5m])`. The 3 `sum(metric)` panels rewritten
    // to `sum(increase(metric[5m]))` (sum() alone is still bare because it
    // does not handle counter resets, only rate/increase/delta/irate do).
    // trading-flow.json — orphan: dashboard panel with no source emission.
    // ("trading-flow.json", "tv_oms_reconciliation_mismatches_total"),
    // auth-health.json — orphan: dashboard panel with no source emission.
    // ("auth-health.json", "tv_ip_mismatch_total"),
];

/// Discovered counter names from source code. Cached at first call.
/// Pure function — caller passes the workspace root.
fn discover_counter_metric_names(workspace_root: &Path) -> BTreeSet<String> {
    let mut out: BTreeSet<String> = BTreeSet::new();
    for crate_dir in &["common", "core", "trading", "storage", "api", "app"] {
        let src = workspace_root.join("crates").join(crate_dir).join("src");
        if !src.is_dir() {
            continue;
        }
        scan_dir_for_counter_names(&src, &mut out);
    }
    out
}

fn scan_dir_for_counter_names(dir: &Path, out: &mut BTreeSet<String>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            scan_dir_for_counter_names(&path, out);
        } else if path.extension().and_then(|s| s.to_str()) == Some("rs") {
            if let Ok(content) = std::fs::read_to_string(&path) {
                extract_counter_names_from_source(&content, out);
            }
        }
    }
}

/// Pure: pull metric names from `metrics::counter!("tv_xxx")` and
/// `counter!("tv_xxx")` invocations. The `metrics` crate also exposes
/// `register_counter!` (deprecated) and `describe_counter!`; both handled.
///
/// Tolerates whitespace and newlines between `(` and the opening quote
/// (the codebase uses multi-line macro invocations for labelled counters).
fn extract_counter_names_from_source(content: &str, out: &mut BTreeSet<String>) {
    for needle in &["counter!(", "register_counter!(", "describe_counter!("] {
        let mut rest = content;
        while let Some(idx) = rest.find(needle) {
            let after = &rest[idx + needle.len()..];
            // Skip whitespace (incl. newlines + indentation) until the
            // opening quote of the metric name. Bail if we hit a non-
            // whitespace, non-quote byte first — that's a non-literal
            // call (e.g. variable name passed as identifier).
            let bytes = after.as_bytes();
            let mut i = 0;
            while i < bytes.len() && (bytes[i] as char).is_whitespace() {
                i += 1;
            }
            if i >= bytes.len() || bytes[i] != b'"' {
                rest = &after[i..];
                continue;
            }
            i += 1;
            let name_start = i;
            while i < bytes.len() && bytes[i] != b'"' {
                i += 1;
            }
            if i >= bytes.len() {
                break;
            }
            let name = &after[name_start..i];
            if name.starts_with("tv_") {
                out.insert(name.to_owned());
            }
            rest = &after[i..];
        }
    }
}

/// Pure: extract every `"expr": "..."` Prometheus expression from a
/// dashboard JSON. Excludes `"rawSql": "..."` SQL queries (handled by
/// the lifecycle guard above). Returns the unescaped expression strings.
fn extract_promql_exprs(json: &str) -> Vec<String> {
    let mut out = Vec::new();
    let needle = "\"expr\":";
    let mut rest = json;
    while let Some(idx) = rest.find(needle) {
        let after = &rest[idx + needle.len()..];
        let start_quote = match after.find('"') {
            Some(p) => p + 1,
            None => break,
        };
        let body = &after[start_quote..];
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
        let expr = raw
            .replace("\\n", "\n")
            .replace("\\\"", "\"")
            .replace("\\\\", "\\");
        out.push(expr);
        rest = &body[end..];
    }
    out
}

/// Pure: returns true if `expr` contains a bare (unwrapped) reference to
/// `counter`. A reference is "bare" iff at least one occurrence of the
/// counter name is NOT immediately enclosed by one of the
/// COUNTER_RESET_SAFE_WRAPPERS.
///
/// Reference enclosure detection: walk left from the metric position and
/// count parens. The first `(` we encounter at `depth = -1` (i.e. the
/// opening paren of the immediate enclosing call) is preceded by an
/// identifier — if that identifier is one of the safe wrappers, the
/// occurrence is wrapped.
///
/// Multiple occurrences of the same counter in one expr: ANY bare
/// occurrence flags the expr.
fn expr_uses_counter_bare(expr: &str, counter: &str) -> bool {
    let bytes = expr.as_bytes();
    let counter_bytes = counter.as_bytes();
    let mut search_from = 0;
    while let Some(idx) = expr[search_from..].find(counter) {
        let pos = search_from + idx;
        // Word boundary check: the byte before pos must not be an identifier
        // char, and the byte at pos+counter.len() must not be one either.
        // Otherwise `tv_foo_total` would match inside `tv_foo_total_extended`.
        let left_ok = pos == 0 || !is_ident_byte(bytes[pos - 1]);
        let right_idx = pos + counter_bytes.len();
        let right_ok = right_idx >= bytes.len() || !is_ident_byte(bytes[right_idx]);
        if !left_ok || !right_ok {
            search_from = pos + 1;
            continue;
        }

        // Walk left from pos. Track paren depth. depth starts at 0; each
        // ')' before pos increases depth (we are inside it), each '('
        // before pos decreases depth. When depth becomes -1 we have
        // found the immediate enclosing '(' — read the identifier
        // immediately to its left.
        let mut depth: i32 = 0;
        let mut wrapper: Option<String> = None;
        let mut i = pos;
        while i > 0 {
            i -= 1;
            let c = bytes[i];
            if c == b')' {
                depth += 1;
            } else if c == b'(' {
                depth -= 1;
                if depth < 0 {
                    // Read identifier immediately before this '('.
                    let mut j = i;
                    while j > 0 && (bytes[j - 1] == b' ' || bytes[j - 1] == b'\t') {
                        j -= 1;
                    }
                    let id_end = j;
                    while j > 0 && is_ident_byte(bytes[j - 1]) {
                        j -= 1;
                    }
                    if j < id_end {
                        wrapper = Some(expr[j..id_end].to_owned());
                    }
                    break;
                }
            }
        }

        let is_wrapped = wrapper
            .as_deref()
            .is_some_and(|w| COUNTER_RESET_SAFE_WRAPPERS.contains(&w));
        if !is_wrapped {
            return true;
        }
        search_from = pos + counter_bytes.len();
    }
    false
}

fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// Resolve workspace root from this crate's manifest dir.
fn workspace_root() -> PathBuf {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest_dir
        .parent()
        .and_then(Path::parent)
        .expect("workspace root must exist")
        .to_path_buf()
}

#[test]
fn dashboard_counters_must_be_wrapped_in_increase_or_rate() {
    let dashboards = read_all_dashboards();
    let counters = discover_counter_metric_names(&workspace_root());
    assert!(
        !counters.is_empty(),
        "auto-discovery returned zero counter names — scanner regressed"
    );

    let allow: BTreeSet<(&str, &str)> = KNOWN_BARE_COUNTERS.iter().copied().collect();
    let mut violations: Vec<String> = Vec::new();

    for (file, json) in &dashboards {
        let exprs = extract_promql_exprs(json);
        for expr in &exprs {
            for counter in &counters {
                if !expr_uses_counter_bare(expr, counter) {
                    continue;
                }
                if allow.contains(&(file.as_str(), counter.as_str())) {
                    continue;
                }
                violations.push(format!(
                    "[{file}] counter `{counter}` rendered bare in PromQL \
                     expression — wrap it in `increase({counter}[5m])` or \
                     `rate({counter}[5m])`. Bare counters reset to 0 after \
                     restart and silently lie to the operator about ongoing \
                     activity. (See audit-findings-2026-04-17.md Rule 12 + \
                     PR #346 for the reference fix.)\n  expr: {}",
                    expr.replace('\n', " ")
                        .chars()
                        .take(200)
                        .collect::<String>()
                ));
            }
        }
    }

    assert!(
        violations.is_empty(),
        "Rule 12 violation — Grafana dashboard panels render bare \
         Prometheus counters. After process restart these panels read 0 \
         even when drops are happening. Wrap each in `increase(...[5m])` \
         or `rate(...[5m])`.\n\n{}\n\n\
         If the metric is intentionally a gauge (despite the `_total` \
         suffix), confirm it is registered with `metrics::gauge!` (NOT \
         `metrics::counter!`); only counter-registered metrics are flagged.",
        violations.join("\n\n")
    );
}

/// Ratchet integrity: if a dashboard is FIXED to wrap a previously-bare
/// counter, the corresponding `KNOWN_BARE_COUNTERS` entry MUST be removed
/// in the same commit. Otherwise the allowlist accumulates stale entries
/// and the guard silently weakens.
#[test]
fn known_bare_counter_entries_must_actually_be_bare() {
    let dashboards = read_all_dashboards();
    let counters = discover_counter_metric_names(&workspace_root());

    // Build a quick lookup: (file, counter) -> bare?
    let mut still_bare: BTreeSet<(String, String)> = BTreeSet::new();
    for (file, json) in &dashboards {
        let exprs = extract_promql_exprs(json);
        for expr in &exprs {
            for counter in &counters {
                if expr_uses_counter_bare(expr, counter) {
                    still_bare.insert((file.clone(), counter.clone()));
                }
            }
        }
    }

    let mut stale: Vec<(&str, &str)> = Vec::new();
    for &(file, counter) in KNOWN_BARE_COUNTERS {
        if !still_bare.contains(&(file.to_owned(), counter.to_owned())) {
            stale.push((file, counter));
        }
    }

    assert!(
        stale.is_empty(),
        "Rule 12 ratchet violation — KNOWN_BARE_COUNTERS contains \
         entries that are no longer bare on the dashboard. The dashboard \
         was fixed but the allowlist entry was not removed. Remove these \
         entries from KNOWN_BARE_COUNTERS in the SAME commit that fixed \
         the dashboard:\n\n{}",
        stale
            .iter()
            .map(|(f, c)| format!("  ({f:?}, {c:?}),"))
            .collect::<Vec<_>>()
            .join("\n")
    );
}

#[test]
fn rule12_self_test_extract_promql_exprs_finds_simple_expr() {
    let json = r#"{ "expr": "tv_foo_total" }"#;
    let exprs = extract_promql_exprs(json);
    assert_eq!(exprs, vec!["tv_foo_total".to_string()]);
}

#[test]
fn rule12_self_test_extract_promql_exprs_skips_rawsql() {
    // `rawSql` strings must not be extracted by the PromQL extractor —
    // they are SQL queries handled by the lifecycle guard above.
    let json = r#"{ "rawSql": "SELECT 1 FROM ticks" }"#;
    let exprs = extract_promql_exprs(json);
    assert!(exprs.is_empty());
}

#[test]
fn rule12_self_test_bare_counter_detected() {
    assert!(expr_uses_counter_bare("tv_foo_total", "tv_foo_total"));
}

#[test]
fn rule12_self_test_increase_wrap_accepted() {
    assert!(!expr_uses_counter_bare(
        "increase(tv_foo_total[5m])",
        "tv_foo_total"
    ));
}

#[test]
fn rule12_self_test_rate_wrap_accepted() {
    assert!(!expr_uses_counter_bare(
        "rate(tv_foo_total[1m])",
        "tv_foo_total"
    ));
}

#[test]
fn rule12_self_test_delta_wrap_accepted() {
    assert!(!expr_uses_counter_bare(
        "delta(tv_foo_total[10m])",
        "tv_foo_total"
    ));
}

#[test]
fn rule12_self_test_irate_wrap_accepted() {
    assert!(!expr_uses_counter_bare(
        "irate(tv_foo_total[30s])",
        "tv_foo_total"
    ));
}

#[test]
fn rule12_self_test_sum_alone_rejected() {
    // sum() is not enough — it aggregates raw counter values that still
    // reset to 0 on restart.
    assert!(expr_uses_counter_bare("sum(tv_foo_total)", "tv_foo_total"));
}

#[test]
fn rule12_self_test_sum_of_increase_accepted() {
    assert!(!expr_uses_counter_bare(
        "sum(increase(tv_foo_total[5m]))",
        "tv_foo_total"
    ));
}

#[test]
fn rule12_self_test_word_boundary_avoids_false_positive() {
    // `tv_foo_total` should NOT match inside `tv_foo_total_extended`.
    assert!(!expr_uses_counter_bare(
        "tv_foo_total_extended",
        "tv_foo_total"
    ));
}

#[test]
fn rule12_self_test_label_filter_treated_as_bare() {
    // `tv_foo_total{instance="x"}` is bare — label selectors are not a
    // wrapper; they restrict series, not handle counter resets.
    assert!(expr_uses_counter_bare(
        "tv_foo_total{instance=\"x\"}",
        "tv_foo_total"
    ));
}

#[test]
fn rule12_self_test_multiple_occurrences_any_bare_flags() {
    // First occurrence is wrapped, second is bare → must flag.
    let expr = "increase(tv_foo_total[5m]) - tv_foo_total";
    assert!(expr_uses_counter_bare(expr, "tv_foo_total"));
}

#[test]
fn rule12_self_test_discover_picks_up_counter_macro() {
    let mut out = BTreeSet::new();
    extract_counter_names_from_source("let _ = metrics::counter!(\"tv_xyz_total\");", &mut out);
    assert!(out.contains("tv_xyz_total"));
}

#[test]
fn rule12_self_test_discover_ignores_gauge_macro() {
    let mut out = BTreeSet::new();
    // Gauges legitimately appear bare on dashboards; the discoverer
    // must not pick them up.
    extract_counter_names_from_source("let _ = metrics::gauge!(\"tv_some_gauge\");", &mut out);
    assert!(out.is_empty());
}

#[test]
fn rule12_self_test_discover_ignores_non_tv_prefix() {
    let mut out = BTreeSet::new();
    extract_counter_names_from_source("let _ = metrics::counter!(\"some_other_total\");", &mut out);
    assert!(out.is_empty());
}

#[test]
fn rule12_self_test_known_bare_counter_pairs_unique() {
    // Catch accidental duplicate entries in the ratchet.
    let mut seen: BTreeSet<(&str, &str)> = BTreeSet::new();
    for &pair in KNOWN_BARE_COUNTERS {
        assert!(
            seen.insert(pair),
            "duplicate KNOWN_BARE_COUNTERS entry: {pair:?}"
        );
    }
}
