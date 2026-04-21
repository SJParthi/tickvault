//! Ratchet: cross-match must be exhaustive across 09:15→15:30 IST for
//! indices + stock equities.
//!
//! # Why (2026-04-21)
//!
//! Live 15:47 IST alert reported "OK — 116,162 candles match" despite the
//! app restarting mid-day and leaving huge gaps in live materialized
//! views. Root causes in `crates/core/src/historical/cross_verify.rs`:
//!
//! 1. `determine_cross_match_passed` ignored `missing_live`/`missing_historical`
//! 2. Pass-B (missing_historical) was never actually run
//! 3. Detail query had `LIMIT` truncating the real mismatch count
//! 4. No scope filter — F&O (which Dhan doesn't even serve historically)
//!    was implicitly in the expected grid
//! 5. Window end was `<=` instead of `<` (15:30 is exclusive)
//!
//! This ratchet file source-scans cross_verify.rs so those regressions
//! cannot be silently reintroduced.

use std::fs;
use std::path::PathBuf;

fn read(rel: &str) -> String {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop();
    p.pop();
    p.push(rel);
    fs::read_to_string(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display()))
}

#[test]
fn cross_verify_source_has_pass_b_for_missing_historical() {
    let src = read("crates/core/src/historical/cross_verify.rs");
    assert!(
        src.contains("parse_missing_historical_rows"),
        "Pass-B query helper `parse_missing_historical_rows` must exist so \
         missing_historical is actually populated (was silently zero before I12)."
    );
    assert!(
        src.contains("missing_historical"),
        "cross_verify.rs MUST track missing_historical as a first-class \
         category — Parthiban directive 2026-04-21."
    );
}

#[test]
fn cross_verify_source_has_scope_filter_to_idx_i_and_nse_eq() {
    let src = read("crates/core/src/historical/cross_verify.rs");
    assert!(
        src.contains("CROSS_MATCH_SCOPE_SQL"),
        "Scope constant `CROSS_MATCH_SCOPE_SQL` must exist."
    );
    assert!(
        src.contains("NSE_EQ") && src.contains("IDX_I"),
        "Scope filter must include both NSE_EQ and IDX_I."
    );
    assert!(
        src.contains("cross_match_scope_aliased"),
        "Aliased scope helper must exist for `h.segment IN (...)` / `m.segment IN (...)`."
    );
}

#[test]
fn cross_verify_window_end_is_exclusive() {
    let src = read("crates/core/src/historical/cross_verify.rs");
    // The where_clause MUST build `ts < ...` — there's no 15:30:00 candle
    // (last 1m candle is 15:29:00).
    assert!(
        src.contains("\"ts >= {start} AND ts < {end}\"")
            || src.contains("ts >= {start} AND ts < {end}"),
        "where_clause must emit `ts < end` (exclusive), not `ts <= end`."
    );
}

#[test]
fn cross_verify_pass_a_detail_query_has_no_limit() {
    // Parthiban directive 2026-04-21: the cross-match Pass-A detail query
    // (historical LEFT JOIN live, fetching value_diff + missing_live rows)
    // must NOT be LIMIT-capped — we need every violation for the exhaustive
    // Telegram report. Regression = a day with >N gaps shows <N in Telegram.
    //
    // Narrow scan: extract the region containing the Pass-A query and
    // assert no `LIMIT {}` substring inside it. The candle-integrity
    // verifier uses LIMIT for a different purpose (OHLC/data scans) — we
    // don't touch those.
    let src = read("crates/core/src/historical/cross_verify.rs");
    let marker = "// Pass-A: historical LEFT JOIN live";
    let start = src
        .find(marker)
        .unwrap_or_else(|| panic!("Pass-A marker comment missing — refactor broke the ratchet"));
    let end_marker = "// Pass-B:";
    let end = src[start..]
        .find(end_marker)
        .map(|rel| start + rel)
        .unwrap_or_else(|| panic!("Pass-B marker comment missing — refactor broke the ratchet"));
    let pass_a_region = &src[start..end];
    assert!(
        !pass_a_region.contains("LIMIT {}"),
        "Pass-A detail query must not be LIMIT-capped — full list required. \
         Region scanned: {pass_a_region}"
    );
    assert!(
        !pass_a_region.contains("LIMIT 500"),
        "Pass-A detail query must not hardcode LIMIT 500. Region: {pass_a_region}"
    );
}

#[test]
fn cross_verify_determine_passed_requires_missing_counters_zero() {
    let src = read("crates/core/src/historical/cross_verify.rs");
    // The fixed `determine_cross_match_passed` signature must take
    // missing_live AND missing_historical — both must appear in its body.
    assert!(
        src.contains("total_missing_live: usize"),
        "determine_cross_match_passed must accept total_missing_live"
    );
    assert!(
        src.contains("total_missing_historical: usize"),
        "determine_cross_match_passed must accept total_missing_historical"
    );
    assert!(
        src.contains("total_missing_live == 0") && src.contains("total_missing_historical == 0"),
        "determine_cross_match_passed body MUST check both missing counters == 0. \
         Regression = OK fires despite live gaps."
    );
}
