//! Ratchets for the 2026-05-25 cross-verify scheduler + report contract
//! (PR #788).
//!
//! Pins:
//!   - 09:00:30 IST daily-1d trigger + 15:31:00 IST intraday trigger
//!   - Dhan yesterday/today fetch math (fromDate=yesterday, toDate=today)
//!   - IDX_I volume-check skip (operator-locked: indices have no volume)
//!   - Telegram suppressed on PASS (forensic report, not real-time alert)
//!   - JSONL + HTML report writer modules exist
//!   - `historical_fetch_enabled = true` in default config

use std::fs;
use std::path::{Path, PathBuf};

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .map(PathBuf::from)
        .expect("workspace root must exist above crates/common") // APPROVED: test
}

fn read(rel: &str) -> String {
    let p = workspace_root().join(rel);
    fs::read_to_string(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display())) // APPROVED: test
}

#[test]
fn test_cross_verify_scheduler_module_exists() {
    let path = workspace_root().join("crates/core/src/historical/cross_verify_scheduler.rs");
    assert!(
        path.exists(),
        "cross_verify_scheduler.rs is the canonical contract for the \
         09:00:30 IST daily + 15:31:00 IST intraday trigger math"
    );
}

#[test]
fn test_cross_verify_report_module_exists() {
    let path = workspace_root().join("crates/core/src/historical/cross_verify_report.rs");
    assert!(
        path.exists(),
        "cross_verify_report.rs is the canonical writer for the JSONL + HTML report"
    );
}

#[test]
fn test_scheduler_constants_match_operator_lock() {
    let body = read("crates/core/src/historical/cross_verify_scheduler.rs");
    assert!(
        body.contains("pub const DAILY_1D_FIRE_SECS_OF_DAY_IST: u32 = 9 * 3_600 + 30"),
        "daily 1d trigger MUST be 09:00:30 IST (32_430s)"
    );
    assert!(
        body.contains("pub const INTRADAY_FIRE_SECS_OF_DAY_IST: u32 = 15 * 3_600 + 31 * 60"),
        "intraday trigger MUST be 15:31:00 IST (55_860s)"
    );
}

#[test]
fn test_yesterday_today_window_is_one_day_wide_contract() {
    // Operator-confirmed 2026-05-25: to fetch yesterday's 1d candle this
    // morning, fromDate=yesterday and toDate=today (toDate non-inclusive
    // per Dhan docs). MUST NOT pass same-day range (= DH-905 error).
    let body = read("crates/core/src/historical/cross_verify_scheduler.rs");
    assert!(
        body.contains("pub fn yesterday_1d_window"),
        "yesterday_1d_window MUST be exported as pub fn"
    );
    assert!(
        body.contains("today_ist - Duration::days(1)"),
        "yesterday_1d_window MUST compute fromDate = today - 1 day"
    );
    // Documentation references the DH-905 trap so future readers know
    // why we don't pass fromDate == toDate.
    assert!(
        body.contains("DH-905"),
        "module docs MUST cite DH-905 (the same-day trap operator pointed out)"
    );
}

#[test]
fn test_volume_check_skipped_for_idx_i() {
    let body = read("crates/core/src/historical/cross_verify.rs");
    assert!(
        body.contains("pub fn is_volume_check_eligible"),
        "is_volume_check_eligible helper MUST be exported"
    );
    // The helper returns false for IDX_I, true for everything else.
    assert!(
        body.contains("segment != \"IDX_I\""),
        "is_volume_check_eligible MUST return false for IDX_I per data-integrity rule"
    );
    // classify_cross_match_row MUST use the helper.
    assert!(
        body.contains("is_volume_check_eligible(&ctx.segment)"),
        "classify_cross_match_row MUST gate volume_mismatch on is_volume_check_eligible"
    );
}

#[test]
fn test_main_rs_suppresses_telegram_on_cross_verify_pass() {
    let body = read("crates/app/src/main.rs");
    // The PR #788 marker comment must remain — pins the operator lock.
    assert!(
        body.contains("PR #788") && body.contains("Telegram suppressed per PR #788 operator lock"),
        "main.rs MUST suppress CandleCrossMatchPassed Telegram per PR #788"
    );
    // The report writer MUST be invoked on every cross-match outcome.
    assert!(
        body.contains("append_jsonl_row"),
        "main.rs MUST call cross_verify_report::append_jsonl_row"
    );
    assert!(
        body.contains("write_html_report"),
        "main.rs MUST call cross_verify_report::write_html_report"
    );
}

#[test]
fn test_historical_fetch_enabled_default_is_true() {
    let body = read("config/base.toml");
    // The line MUST be `historical_fetch_enabled = true` (post-2026-05-25 flip).
    // Without `true`, the entire cross-verify path is gated off and the
    // report writer never fires.
    assert!(
        body.lines().any(|l| l
            .trim_start()
            .starts_with("historical_fetch_enabled = true")),
        "config/base.toml MUST set historical_fetch_enabled = true"
    );
}

#[test]
fn test_report_paths_are_under_data_logs() {
    let body = read("crates/core/src/historical/cross_verify_report.rs");
    assert!(
        body.contains("data/logs/cross_verify."),
        "JSONL + HTML report paths MUST live under data/logs/"
    );
    assert!(
        body.contains(".jsonl") && body.contains(".html"),
        "Both .jsonl AND .html paths MUST be exported"
    );
}

#[test]
fn test_once_per_day_idempotency_via_marker_file() {
    // Operator-locked 2026-04-20 + re-confirmed 2026-05-25:
    // "until it is successful alone only it should happen dude okay not
    // more than once right dude?" — cross-verify runs UNTIL success,
    // then silently skips for the rest of the day. A marker file
    // (`data/cross_verify_success_marker.YYYY-MM-DD`) is written on
    // success; future boots check for it before re-firing.
    let main_body = read("crates/app/src/main.rs");
    assert!(
        main_body.contains("cross_verify_already_succeeded_today"),
        "main.rs MUST call cross_verify_already_succeeded_today before running cross-verify"
    );
    assert!(
        main_body.contains("SILENTLY skipped") && main_body.contains("already succeeded earlier"),
        "main.rs MUST silently skip when marker is present (no Telegram, no re-run)"
    );
    assert!(
        main_body.contains("mark_cross_verify_success"),
        "main.rs MUST write the success marker on PASS"
    );
    // The cross_verify module itself MUST export both helpers.
    let cv_body = read("crates/core/src/historical/cross_verify.rs");
    assert!(
        cv_body.contains("pub fn cross_verify_already_succeeded_today"),
        "cross_verify_already_succeeded_today MUST be exported"
    );
    assert!(
        cv_body.contains("pub fn mark_cross_verify_success"),
        "mark_cross_verify_success MUST be exported"
    );
}

#[test]
fn test_html_report_uses_chartjs_cdn() {
    let body = read("crates/core/src/historical/cross_verify_report.rs");
    assert!(
        body.contains("cdn.jsdelivr.net/npm/chart.js"),
        "HTML report MUST load Chart.js from the CDN — operator-visible visualization"
    );
}
