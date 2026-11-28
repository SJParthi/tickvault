//! Integration coverage for `tickvault_app::live_fill_report` (the daily
//! live paper fill calibration report, plan item LF-7).
//!
//! Covers, through the public API only:
//! - `live_fill_report` (mod.rs): the scheduling decisions (`attempt_plan`,
//!   `in_session`, `attempt_fits_outside_session`, `leftovers_allowed`,
//!   `today_step`, `wake_after_stop_window`), the catch-up decisions, the
//!   step budgets, the lookback, the readiness checks (`wal_settled`,
//!   `dir_has_no_files`), the summary row (`daily_row`,
//!   `daily_row_from_report_json`), `gated_row_write` and `register_metrics`.
//! - `live_fill_report::rows`: the day query, the cell parsers, the `/exec`
//!   body parser (column order, NULLs, skipped rows, truncation) and both
//!   bounded reads (`read_day`, `read_row_uploaded`) against a mock HTTP
//!   server on 127.0.0.1 port 0.
//! - a few `upload` / `render` helpers the report relies on.
//!
//! Why: CI's coverage report showed these paths unexecuted from the
//! production-only figure. Every test is deterministic: explicit times, no
//! wall clock, no AWS, temp folders under `std::env::temp_dir()`.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use chrono::NaiveDate;
use proptest::prelude::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use tickvault_app::live_fill_report::build::{
    Counts, DailyReport, FILL_SECONDS_BUCKETS, LatencyStats, RejectTotal, latency_stats,
    rows_day_start,
};
use tickvault_app::live_fill_report::render::{
    csv_field, render_files, render_json, report_file_names, summary_plain,
};
use tickvault_app::live_fill_report::rows::{
    EVENT_COLUMNS, REPORT_ROW_CAP, day_sql, json_i64, parse_exec_body, parse_exec_body_owned,
    parse_row_uploaded, parse_ts_nanos, read_day, read_row_uploaded, row_uploaded_sql,
};
use tickvault_app::live_fill_report::upload::{
    ROW_WRITTEN_RECORD, UploadSummary, copy_started, folder_complete, folder_copied,
    live_fill_report_key, markers_dir, row_written, unreported_day_folders, write_report_files,
    write_row_written,
};
use tickvault_app::live_fill_report::{
    DB_ROW_STEP_BOUND_SECS, LAST_DAY_GAUGE, ORDERS_COUNTER, REPORT_ATTEMPT_SECS,
    REPORT_ATTEMPT_TIMEOUT_SECS, REPORT_READINESS_WAIT_SECS, RUNS_COUNTER,
    TAIL_AFTER_READINESS_SECS, TAIL_AFTER_RECHECK_SECS, UPLOAD_COUNTER,
    attempt_fits_outside_session, attempt_plan, catch_up_probes_bucket, catch_up_repair_may_mark,
    catch_up_repairs_instead, copy_budget, daily_row, daily_row_from_report_json, dir_has_no_files,
    gated_row_write, in_session, leftovers_allowed, lookback_days, pending_days,
    probe_failure_may_build, readiness_budget, recheck_budget, register_metrics, today_step,
    wake_after_stop_window, wal_settled, ymd_number,
};
use tickvault_app::shutdown_class::{
    SCHEDULED_STOP_WINDOW_END_SECS_OF_DAY_IST, SCHEDULED_STOP_WINDOW_START_SECS_OF_DAY_IST,
};
use tickvault_common::constants::TICK_PERSIST_END_SECS_OF_DAY_IST;
use tickvault_storage::wal_suspension_watcher::WalTableRow;

const SESSION_OPEN: u32 = 9 * 3_600;
const STOP_START: u32 = SCHEDULED_STOP_WINDOW_START_SECS_OF_DAY_IST;
const STOP_END: u32 = SCHEDULED_STOP_WINDOW_END_SECS_OF_DAY_IST;
const SESSION_END: u32 = TICK_PERSIST_END_SECS_OF_DAY_IST;

fn date(y: i32, m: u32, d: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(y, m, d).unwrap_or(NaiveDate::MIN)
}

fn weekday(d: NaiveDate) -> bool {
    use chrono::Datelike;
    d.weekday().number_from_monday() <= 5
}

/// A unique temp folder for one test; removed by the guard.
struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        static SEQ: AtomicUsize = AtomicUsize::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let p = std::env::temp_dir().join(format!(
            "tv-cov-live-fill-report-{tag}-{}-{n}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap_or(());
        Self(p)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

// ---------------------------------------------------------------------------
// Scheduling decisions
// ---------------------------------------------------------------------------

/// `(now_sod, done, expected attempt_plan)`.
type PlanCase = (u32, Option<usize>, Option<(usize, u32)>);
/// `(now_sod, done, expected today_step)`.
type StepCase = (u32, Option<usize>, Option<(usize, u32, bool)>);

#[test]
fn test_attempt_plan_table() {
    let [a0, a1, a2] = REPORT_ATTEMPT_SECS;
    let cases: &[PlanCase] = &[
        (0, None, Some((0, a0))),
        (a0 - 1, None, Some((0, 1))),
        (a0, None, Some((0, 0))),
        (a0 + 1, None, Some((1, a1 - a0 - 1))),
        (a0, Some(0), Some((1, a1 - a0))),
        (a1, Some(0), Some((1, 0))),
        (a1, Some(1), Some((2, a2 - a1))),
        (a2, Some(1), Some((2, 0))),
        (a2, Some(2), None),
        (a2 + 1, None, None),
        (0, Some(2), None),
        (0, Some(7), None),
        (86_399, None, None),
        (u32::MAX, None, None),
    ];
    for &(now, done, want) in cases {
        assert_eq!(attempt_plan(now, done), want, "now={now} done={done:?}");
    }
}

#[test]
fn test_in_session_boundaries() {
    assert!(!in_session(0));
    assert!(!in_session(SESSION_OPEN - 1));
    assert!(in_session(SESSION_OPEN));
    assert!(in_session(12 * 3_600));
    assert!(in_session(SESSION_END - 1));
    assert!(!in_session(SESSION_END));
    assert!(!in_session(86_399));
    for a in REPORT_ATTEMPT_SECS {
        assert!(!in_session(a), "attempt {a} must be outside the session");
    }
}

#[test]
fn test_attempt_fits_outside_session_edges() {
    let t = REPORT_ATTEMPT_TIMEOUT_SECS as u32;
    // Ends exactly at 09:00 fits; one second later does not.
    assert!(attempt_fits_outside_session(0));
    assert!(attempt_fits_outside_session(SESSION_OPEN - t));
    assert!(!attempt_fits_outside_session(SESSION_OPEN - t + 1));
    // Inside the session never fits.
    assert!(!attempt_fits_outside_session(SESSION_OPEN));
    assert!(!attempt_fits_outside_session(SESSION_END - 1));
    // After the session: must end 60 s (margin) before the stop window.
    assert!(attempt_fits_outside_session(SESSION_END));
    let last_fit = STOP_START - t - 60 - 1;
    assert!(attempt_fits_outside_session(last_fit));
    assert!(!attempt_fits_outside_session(last_fit + 1));
    // Inside the stop window never fits; at its end it fits again.
    assert!(!attempt_fits_outside_session(STOP_START));
    assert!(!attempt_fits_outside_session(STOP_END - 1));
    assert!(attempt_fits_outside_session(STOP_END));
    assert!(attempt_fits_outside_session(86_399));
    // Every planned attempt fits.
    for a in REPORT_ATTEMPT_SECS {
        assert!(attempt_fits_outside_session(a), "attempt {a}");
        assert!(leftovers_allowed(a), "attempt {a}");
    }
}

#[test]
fn test_today_step_table() {
    let [a0, a1, a2] = REPORT_ATTEMPT_SECS;
    let t = REPORT_ATTEMPT_TIMEOUT_SECS as u32;
    let last_fit = STOP_START - t - 60 - 1;
    let cases: &[StepCase] = &[
        (0, None, Some((0, a0, false))),
        (a0, None, Some((0, 0, false))),
        (a0, Some(0), Some((1, a1 - a0, false))),
        (a1, Some(1), Some((2, a2 - a1, true))),
        (a2, None, Some((2, 0, true))),
        // After the last slot with none run: one final attempt at once.
        (a2 + 1, None, Some((2, 0, true))),
        (last_fit, None, Some((2, 0, true))),
        // ...but not once it no longer fits before the stop window.
        (last_fit + 1, None, None),
        (STOP_START, None, None),
        // After the stop window (now > last slot, fits) -> final try.
        (STOP_END, None, Some((2, 0, true))),
        // An attempt already ran today: no extra final try.
        (a2 + 1, Some(0), None),
        (a2 + 1, Some(2), None),
        (STOP_END, Some(1), None),
    ];
    for &(now, done, want) in cases {
        assert_eq!(today_step(now, done), want, "now={now} done={done:?}");
    }
}

#[test]
fn test_wake_after_stop_window_table() {
    let cases: &[(u32, bool, Option<u64>)] = &[
        (SESSION_END, true, Some(u64::from(STOP_END - SESSION_END))),
        (SESSION_END - 1, true, None),
        (STOP_START, true, Some(u64::from(STOP_END - STOP_START))),
        (STOP_END - 1, true, Some(1)),
        (STOP_END, true, None),
        (86_399, true, None),
        (0, true, None),
        (SESSION_END, false, None),
        (STOP_START, false, None),
    ];
    for &(now, owed, want) in cases {
        assert_eq!(
            wake_after_stop_window(now, owed),
            want,
            "now={now} owed={owed}"
        );
    }
}

proptest! {
    #[test]
    fn test_leftovers_allowed_equals_fits_and_never_overlaps_session(sod in 0u32..86_400) {
        let fits = attempt_fits_outside_session(sod);
        prop_assert_eq!(leftovers_allowed(sod), fits);
        if fits {
            let start = u64::from(sod);
            let end = start + REPORT_ATTEMPT_TIMEOUT_SECS;
            // Never overlaps [09:00, 15:40).
            prop_assert!(end <= u64::from(SESSION_OPEN) || start >= u64::from(SESSION_END));
            // Never overlaps [stop start, stop end).
            prop_assert!(
                end < u64::from(STOP_START) || start >= u64::from(STOP_END),
                "sod {} overlaps the stop window", sod
            );
            prop_assert!(!in_session(sod));
        }
    }

    #[test]
    fn test_today_step_never_starts_inside_session_or_stop_window(
        sod in 0u32..86_400,
        done in prop::option::of(0usize..3),
    ) {
        if let Some((idx, wait, final_try)) = today_step(sod, done) {
            prop_assert!(idx < REPORT_ATTEMPT_SECS.len());
            if let Some(d) = done {
                prop_assert!(idx > d);
            }
            prop_assert_eq!(final_try, idx == REPORT_ATTEMPT_SECS.len() - 1);
            let start = sod + wait;
            prop_assert!(!in_session(start));
            prop_assert!(attempt_fits_outside_session(start));
        }
    }

    #[test]
    fn test_wake_after_stop_window_lands_exactly_at_window_end(sod in 0u32..86_400) {
        match wake_after_stop_window(sod, true) {
            Some(secs) => {
                prop_assert_eq!(u64::from(sod) + secs, u64::from(STOP_END));
                prop_assert!(attempt_fits_outside_session(STOP_END));
            }
            None => prop_assert!(!(SESSION_END..STOP_END).contains(&sod)),
        }
        prop_assert_eq!(wake_after_stop_window(sod, false), None);
    }
}

// ---------------------------------------------------------------------------
// Catch-up decisions
// ---------------------------------------------------------------------------

#[test]
fn test_catch_up_decision_tables() {
    // catch_up_repairs_instead(files_in_bucket, row_uploaded)
    let repairs: &[(usize, Option<bool>, bool)] = &[
        (0, None, false),
        (0, Some(false), false),
        (0, Some(true), true),
        (1, None, true),
        (5, Some(false), true),
        (usize::MAX, None, true),
    ];
    for &(files, row, want) in repairs {
        assert_eq!(
            catch_up_repairs_instead(files, row),
            want,
            "{files} {row:?}"
        );
    }
    // catch_up_probes_bucket(catch_up, copy_started)
    assert!(catch_up_probes_bucket(true, false));
    assert!(!catch_up_probes_bucket(true, true));
    assert!(!catch_up_probes_bucket(false, false));
    assert!(!catch_up_probes_bucket(false, true));
    // catch_up_repair_may_mark(row_uploaded)
    assert!(catch_up_repair_may_mark(Some(true)));
    assert!(!catch_up_repair_may_mark(Some(false)));
    assert!(!catch_up_repair_may_mark(None));
    // probe_failure_may_build(folder_complete, row_read)
    let builds: &[(bool, Result<Option<bool>, String>, bool)] = &[
        (false, Ok(None), true),
        (true, Ok(None), false),
        (false, Ok(Some(false)), false),
        (false, Ok(Some(true)), false),
        (false, Err("timeout".to_owned()), false),
        (true, Err(String::new()), false),
    ];
    for (complete, read, want) in builds {
        assert_eq!(
            probe_failure_may_build(*complete, read),
            *want,
            "{complete} {read:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// Step budgets
// ---------------------------------------------------------------------------

#[test]
fn test_step_budget_constants_add_up() {
    assert_eq!(DB_ROW_STEP_BOUND_SECS, 40);
    assert_eq!(TAIL_AFTER_RECHECK_SECS, 180);
    assert_eq!(TAIL_AFTER_READINESS_SECS, 240);
    let fixed = [REPORT_READINESS_WAIT_SECS, TAIL_AFTER_READINESS_SECS];
    assert_eq!(fixed.iter().sum::<u64>(), 360);
    assert!(fixed.iter().sum::<u64>() < REPORT_ATTEMPT_TIMEOUT_SECS);
}

#[test]
fn test_readiness_recheck_copy_budget_tables() {
    let s = Duration::from_secs;
    // readiness: min(120, r - 240), saturating.
    let readiness: &[(u64, u64)] = &[
        (0, 0),
        (239, 0),
        (240, 0),
        (241, 1),
        (360, 120),
        (361, 120),
        (410, 120),
        (u64::MAX, 120),
    ];
    for &(r, want) in readiness {
        assert_eq!(readiness_budget(s(r)), s(want), "readiness {r}");
    }
    // recheck: min(120, r - (180 + 10)).
    let recheck: &[(u64, u64)] = &[(0, 0), (190, 0), (191, 1), (310, 120), (1_000, 120)];
    for &(r, want) in recheck {
        assert_eq!(recheck_budget(s(r)), s(want), "recheck {r}");
    }
    // copy: r - 40, saturating.
    let copy: &[(u64, u64)] = &[(0, 0), (40, 0), (41, 1), (300, 260)];
    for &(r, want) in copy {
        assert_eq!(copy_budget(s(r)), s(want), "copy {r}");
    }
    // Sub-second remainders are kept by copy_budget.
    assert_eq!(
        copy_budget(Duration::from_millis(40_500)),
        Duration::from_millis(500)
    );
}

proptest! {
    #[test]
    fn test_budgets_never_exceed_remaining_or_their_caps(ms in 0u64..2_000_000) {
        let r = Duration::from_millis(ms);
        let rb = readiness_budget(r);
        let cb = recheck_budget(r);
        let pb = copy_budget(r);
        prop_assert!(rb <= Duration::from_secs(REPORT_READINESS_WAIT_SECS));
        prop_assert!(cb <= Duration::from_secs(120));
        prop_assert!(rb <= r && cb <= r && pb <= r);
        // The readiness wait always leaves the tail it reserves.
        if rb > Duration::ZERO {
            prop_assert!(r - rb >= Duration::from_secs(TAIL_AFTER_READINESS_SECS));
        }
        prop_assert_eq!(pb, r.saturating_sub(Duration::from_secs(DB_ROW_STEP_BOUND_SECS)));
    }
}

// ---------------------------------------------------------------------------
// Lookback
// ---------------------------------------------------------------------------

#[test]
fn test_lookback_days_weekdays_and_holidays() {
    // Monday 2026-11-16: the three weekdays before it, oldest first.
    assert_eq!(
        lookback_days(date(2026, 11, 16), weekday),
        vec![date(2026, 11, 11), date(2026, 11, 12), date(2026, 11, 13)]
    );
    // A holiday on Thursday 12 Nov is skipped; the window reaches back.
    let holiday = date(2026, 11, 12);
    assert_eq!(
        lookback_days(date(2026, 11, 16), |d| weekday(d) && d != holiday),
        vec![date(2026, 11, 10), date(2026, 11, 11), date(2026, 11, 13)]
    );
    // Only one trading day inside the 10-calendar-day window.
    let only = date(2026, 11, 7);
    assert_eq!(lookback_days(date(2026, 11, 16), |d| d == only), vec![only]);
    // A day exactly 10 back is reached; 11 back is not.
    assert_eq!(
        lookback_days(date(2026, 11, 16), |d| d == date(2026, 11, 6)),
        vec![date(2026, 11, 6)]
    );
    assert!(lookback_days(date(2026, 11, 16), |d| d == date(2026, 11, 5)).is_empty());
    // Never today itself.
    assert!(lookback_days(date(2026, 11, 16), |d| d == date(2026, 11, 16)).is_empty());
    // At the earliest representable date nothing lies before it.
    assert!(lookback_days(NaiveDate::MIN, |_| true).is_empty());
    let after_min = NaiveDate::MIN.succ_opt().unwrap_or(NaiveDate::MIN);
    assert_eq!(lookback_days(after_min, |_| true), vec![NaiveDate::MIN]);
}

#[test]
fn test_pending_days_filters_marked_keeping_order() {
    let today = date(2026, 11, 16);
    let all = pending_days(today, |_| true, |_| false);
    assert_eq!(
        all,
        vec![date(2026, 11, 13), date(2026, 11, 14), date(2026, 11, 15)]
    );
    let marked = pending_days(today, |_| true, |d| d == date(2026, 11, 14));
    assert_eq!(marked, vec![date(2026, 11, 13), date(2026, 11, 15)]);
    assert!(pending_days(today, |_| true, |_| true).is_empty());
    // A marked day does not let the window reach further back.
    let none_extra = pending_days(today, weekday, |d| d == date(2026, 11, 11));
    assert_eq!(none_extra, vec![date(2026, 11, 12), date(2026, 11, 13)]);
}

// ---------------------------------------------------------------------------
// Readiness checks
// ---------------------------------------------------------------------------

fn wal(name: &str, w: Option<i64>, s: Option<i64>, suspended: bool) -> WalTableRow {
    WalTableRow {
        name: name.to_owned(),
        suspended,
        writer_txn: w,
        sequencer_txn: s,
        error_tag: None,
        error_message: None,
    }
}

#[test]
fn test_wal_settled_table() {
    let ok = |n: &str| wal(n, Some(10), Some(10), false);
    let cases: Vec<(Vec<WalTableRow>, bool, &str)> = vec![
        (vec![], true, "no rows: nothing to wait for"),
        (
            vec![ok("live_order_events"), ok("candles_1s")],
            true,
            "both applied",
        ),
        (
            vec![wal("live_order_events", Some(11), Some(10), false)],
            true,
            "writer ahead",
        ),
        (
            vec![wal("live_order_events", Some(9), Some(10), false)],
            false,
            "writer behind",
        ),
        (
            vec![wal("candles_1s", Some(10), Some(10), true)],
            false,
            "suspended",
        ),
        (
            vec![wal("candles_1s", None, Some(10), false)],
            false,
            "writer unknown",
        ),
        (
            vec![wal("candles_1s", Some(10), None, false)],
            false,
            "sequencer unknown",
        ),
        (
            vec![wal("ticks", Some(0), Some(999), true)],
            true,
            "unrelated table ignored",
        ),
        (
            vec![ok("candles_1s"), wal("candles_1s", Some(1), Some(2), false)],
            false,
            "any listed copy behind",
        ),
    ];
    for (rows, want, why) in cases {
        assert_eq!(wal_settled(&rows), want, "{why}");
    }
}

#[test]
fn test_dir_has_no_files_cases() {
    let t = TempDir::new("dirnofiles");
    // Missing folder holds none.
    assert!(dir_has_no_files(&t.0.join("missing")));
    // Empty folder.
    assert!(dir_has_no_files(&t.0));
    // A sub-folder alone is not a file.
    std::fs::create_dir_all(t.0.join("sub")).unwrap_or(());
    assert!(dir_has_no_files(&t.0));
    // A file in a sub-folder is not counted (only direct entries).
    std::fs::write(t.0.join("sub").join("inner.ndjson"), b"x").unwrap_or(());
    assert!(dir_has_no_files(&t.0));
    // A direct file is.
    let f = t.0.join("spill.ndjson");
    std::fs::write(&f, b"row").unwrap_or(());
    assert!(!dir_has_no_files(&t.0));
    // A path that is a file cannot be listed: not "no files".
    assert!(!dir_has_no_files(&f));
}

#[test]
fn test_ymd_number_table() {
    let cases: &[(&str, u32)] = &[
        ("2026-11-12", 20_261_112),
        ("2000-01-01", 20_000_101),
        ("", 0),
        ("abc", 0),
        ("2026-11-12x", 0),
        ("-", 0),
        ("99999-99-999", 0),
    ];
    for &(day, want) in cases {
        assert_eq!(ymd_number(day), want, "{day:?}");
    }
}

// ---------------------------------------------------------------------------
// Summary row
// ---------------------------------------------------------------------------

fn report(day: &str, counts: Counts, latency: Vec<LatencyStats>) -> DailyReport {
    DailyReport {
        day: day.to_owned(),
        status: "complete",
        status_reasons: Vec::new(),
        light: "green",
        no_orders_reason: None,
        fill_seconds_source: "recheck",
        counts,
        orders: Vec::new(),
        fill_seconds: vec![0; FILL_SECONDS_BUCKETS],
        latency,
        rejects: BTreeMap::new(),
    }
}

fn sample_counts() -> Counts {
    Counts {
        orders: 12,
        filled: 7,
        partial_cancelled: 2,
        unfilled: 1,
        skipped: 1,
        rejected: 1,
        worse_than_worst: 3,
        stored_worse: 2,
        better_than_worst: 4,
        recheck_skipped: 1,
        recheck_unavailable: 2,
        recheck_incomplete: 3,
        ..Counts::default()
    }
}

fn sample_latency() -> Vec<LatencyStats> {
    vec![
        latency_stats("decision_to_built_us", vec![10, 20, 30]),
        latency_stats("built_to_sent_us", vec![100, 200, -5]),
        latency_stats("feed_lag_us", vec![7, 8, 9, 1_000]),
        latency_stats("api_rtt_us", vec![50, 5_000, 60]),
    ]
}

#[test]
fn test_daily_row_maps_every_field() {
    let r = report("2026-11-12", sample_counts(), sample_latency());
    let row = daily_row(&r, 123, true);
    assert_eq!(row.reported_at_ist_nanos, 123);
    assert_eq!(Some(row.day_ist_nanos), rows_day_start("2026-11-12"));
    assert_eq!(row.feed, "dhan");
    assert_eq!(row.status, "complete");
    assert_eq!(row.light, "green");
    assert_eq!(
        (
            row.orders,
            row.filled,
            row.partial,
            row.unfilled,
            row.skipped,
            row.rejected
        ),
        (12, 7, 2, 1, 1, 1)
    );
    assert_eq!(row.worse_than_worst, 5, "worse + stored worse");
    assert_eq!(row.better_than_worst, 4);
    assert_eq!(row.recheck_skipped, 6, "skipped + unavailable + incomplete");
    // p99 decision->sent = p99(built_to_sent) + p99(decision_to_built).
    assert_eq!(row.p99_decision_to_sent_us, 200 + 30);
    assert_eq!(row.p99_feed_lag_us, 1_000);
    assert_eq!(row.max_api_rtt_us, 5_000);
    assert!(row.s3_uploaded);
}

#[test]
fn test_daily_row_missing_series_and_bad_day_and_huge_counts() {
    // No latency: every derived figure is -1.
    let r = report("2026-11-12", Counts::default(), Vec::new());
    let row = daily_row(&r, 9, false);
    assert_eq!(row.p99_decision_to_sent_us, -1);
    assert_eq!(row.p99_feed_lag_us, -1);
    assert_eq!(row.max_api_rtt_us, -1);
    assert!(!row.s3_uploaded);
    // Only one half of decision->sent: still -1.
    let r = report(
        "2026-11-12",
        Counts::default(),
        vec![latency_stats("built_to_sent_us", vec![5])],
    );
    assert_eq!(daily_row(&r, 9, false).p99_decision_to_sent_us, -1);
    // Both halves present but one has no non-negative sample: -1.
    let r = report(
        "2026-11-12",
        Counts::default(),
        vec![
            latency_stats("built_to_sent_us", vec![5]),
            latency_stats("decision_to_built_us", vec![-1, -2]),
        ],
    );
    assert_eq!(daily_row(&r, 9, false).p99_decision_to_sent_us, -1);
    // A day that is not a date falls back to the run instant.
    let r = report("not-a-day", Counts::default(), Vec::new());
    assert_eq!(daily_row(&r, 777, false).day_ist_nanos, 777);
    // Counts past i64::MAX saturate.
    let huge = Counts {
        orders: u64::MAX,
        filled: i64::MAX as u64 + 1,
        ..Counts::default()
    };
    let row = daily_row(&report("2026-11-12", huge, Vec::new()), 1, false);
    assert_eq!(row.orders, i64::MAX);
    assert_eq!(row.filled, i64::MAX);
}

#[test]
fn test_daily_row_from_report_json_round_trips_render_json() {
    for (day, counts, latency, uploaded) in [
        ("2026-11-12", sample_counts(), sample_latency(), true),
        ("2026-01-01", Counts::default(), Vec::new(), false),
    ] {
        let r = report(day, counts, latency);
        let json = render_json(&r);
        let back = daily_row_from_report_json(&json, 55, uploaded);
        assert_eq!(back, Some(daily_row(&r, 55, uploaded)), "{day}");
    }
    // Amber partial and red reports also round trip.
    let mut r = report("2026-11-12", sample_counts(), sample_latency());
    r.status = "partial";
    r.light = "amber";
    assert_eq!(
        daily_row_from_report_json(&render_json(&r), 1, false),
        Some(daily_row(&r, 1, false))
    );
    r.status = "no_orders";
    r.light = "red";
    let row = daily_row_from_report_json(&render_json(&r), 1, false);
    assert_eq!(row.map(|x| (x.status, x.light)), Some(("no_orders", "red")));
}

#[test]
fn test_daily_row_from_report_json_refuses_non_reports() {
    let good = render_json(&report("2026-11-12", sample_counts(), sample_latency()));
    let mut v: serde_json::Value = serde_json::from_str(&good).unwrap_or_default();
    assert!(daily_row_from_report_json(&good, 0, true).is_some());

    let edit = |f: &dyn Fn(&mut serde_json::Value)| {
        let mut c = v.clone();
        f(&mut c);
        daily_row_from_report_json(&c.to_string(), 0, true)
    };
    assert_eq!(daily_row_from_report_json("", 0, true), None);
    assert_eq!(daily_row_from_report_json("{", 0, true), None);
    assert_eq!(daily_row_from_report_json("[]", 0, true), None);
    assert_eq!(edit(&|c| c["status"] = "done".into()), None);
    assert_eq!(edit(&|c| c["light"] = "blue".into()), None);
    assert_eq!(edit(&|c| c["day"] = "2026-13-40".into()), None);
    assert_eq!(edit(&|c| c["day"] = 20_261_112.into()), None);
    assert_eq!(edit(&|c| c["status"] = serde_json::Value::Null), None);
    assert_eq!(
        edit(&|c| {
            if let Some(o) = c.as_object_mut() {
                o.remove("counts");
            }
        }),
        None
    );
    assert_eq!(
        edit(&|c| {
            if let Some(o) = c["counts"].as_object_mut() {
                o.remove("rejected");
            }
        }),
        None
    );
    // A count past i64 is not readable.
    assert_eq!(
        edit(&|c| c["counts"]["orders"] = serde_json::json!(u64::MAX)),
        None
    );
    // Missing latency is allowed: the figures read -1.
    let row = edit(&|c| {
        if let Some(o) = c.as_object_mut() {
            o.remove("latency");
        }
    });
    assert_eq!(
        row.map(|r| (
            r.p99_decision_to_sent_us,
            r.p99_feed_lag_us,
            r.max_api_rtt_us
        )),
        Some((-1, -1, -1))
    );
    // A latency entry with a null p99 reads -1 for that figure only.
    v["latency"] = serde_json::json!([
        {"series": "feed_lag_us", "p99": null, "max": 4},
        {"series": "api_rtt_us", "max": 99},
        {"series": "built_to_sent_us", "p99": 3},
        {"series": "decision_to_built_us", "p99": 4}
    ]);
    let row = daily_row_from_report_json(&v.to_string(), 0, false);
    assert_eq!(
        row.map(|r| (
            r.p99_decision_to_sent_us,
            r.p99_feed_lag_us,
            r.max_api_rtt_us
        )),
        Some((7, -1, 99))
    );
}

// ---------------------------------------------------------------------------
// gated_row_write
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_gated_row_write_already_ensured_skips_ensure() {
    let ensured = AtomicBool::new(true);
    let ensure_calls = AtomicUsize::new(0);
    let write_calls = AtomicUsize::new(0);
    let ok = gated_row_write(
        &ensured,
        || async {
            ensure_calls.fetch_add(1, Ordering::SeqCst);
            false
        },
        || async {
            write_calls.fetch_add(1, Ordering::SeqCst);
            true
        },
        "2026-11-12",
        1,
    )
    .await;
    assert!(ok);
    assert_eq!(ensure_calls.load(Ordering::SeqCst), 0);
    assert_eq!(write_calls.load(Ordering::SeqCst), 1);
    assert!(ensured.load(Ordering::SeqCst));
}

#[tokio::test]
async fn test_gated_row_write_reensures_then_writes_and_returns_write_result() {
    for write_result in [true, false] {
        let ensured = AtomicBool::new(false);
        let write_calls = AtomicUsize::new(0);
        let got = gated_row_write(
            &ensured,
            || async { true },
            || async {
                write_calls.fetch_add(1, Ordering::SeqCst);
                write_result
            },
            "2026-11-12",
            2,
        )
        .await;
        assert_eq!(got, write_result);
        assert!(
            ensured.load(Ordering::SeqCst),
            "ensure success is remembered"
        );
        assert_eq!(write_calls.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn test_gated_row_write_never_writes_an_unensured_table() {
    let ensured = AtomicBool::new(false);
    let ensure_calls = AtomicUsize::new(0);
    let write_calls = AtomicUsize::new(0);
    let got = gated_row_write(
        &ensured,
        || async {
            ensure_calls.fetch_add(1, Ordering::SeqCst);
            false
        },
        || async {
            write_calls.fetch_add(1, Ordering::SeqCst);
            true
        },
        "2026-11-12",
        3,
    )
    .await;
    assert!(!got);
    assert_eq!(ensure_calls.load(Ordering::SeqCst), 1);
    assert_eq!(write_calls.load(Ordering::SeqCst), 0);
    assert!(!ensured.load(Ordering::SeqCst));
}

// ---------------------------------------------------------------------------
// Metrics registration
// ---------------------------------------------------------------------------

#[test]
fn test_register_metrics_registers_every_series_at_zero() {
    let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
    let handle = recorder.handle();
    metrics::with_local_recorder(&recorder, register_metrics);
    let text = handle.render();
    for o in ["complete", "partial", "no_orders", "failed"] {
        assert!(
            text.contains(&format!("{RUNS_COUNTER}{{outcome=\"{o}\"}} 0")),
            "missing run outcome {o}"
        );
    }
    for v in [
        "match",
        "worse_than_worst",
        "better_than_worst",
        "stored_worse",
        "qty_differs",
        "recheck_incomplete",
        "recheck_unavailable",
        "recheck_skipped",
        "not_priced",
        "other",
    ] {
        assert!(
            text.contains(&format!("{ORDERS_COUNTER}{{verdict=\"{v}\"}} 0")),
            "missing verdict {v}"
        );
    }
    for o in ["verified", "not_verified", "no_bucket", "timed_out"] {
        assert!(
            text.contains(&format!("{UPLOAD_COUNTER}{{outcome=\"{o}\"}} 0")),
            "missing upload outcome {o}"
        );
    }
    assert!(text.contains(&format!("{LAST_DAY_GAUGE} 0")));
}

// ---------------------------------------------------------------------------
// rows: query, cell parsers, body parser
// ---------------------------------------------------------------------------

#[test]
fn test_day_sql_shape() {
    let sql = day_sql(date(2026, 11, 12));
    assert!(sql.starts_with(&format!(
        "SELECT {} FROM live_order_events",
        EVENT_COLUMNS.join(", ")
    )));
    assert!(sql.contains("WHERE ts IN '2026-11-12' AND feed = 'dhan'"));
    assert!(sql.contains("ORDER BY signal_id, seq, ts"));
    assert!(sql.ends_with(&format!("LIMIT {}", REPORT_ROW_CAP + 1)));
    assert_eq!(EVENT_COLUMNS.len(), 32);
    // Single-digit month and day are zero padded.
    assert!(day_sql(date(2027, 1, 5)).contains("'2027-01-05'"));
}

#[test]
fn test_parse_ts_nanos_table() {
    let cases: Vec<(serde_json::Value, Option<i64>)> = vec![
        (serde_json::json!("1970-01-01T00:00:00Z"), Some(0)),
        (
            serde_json::json!("1970-01-01T00:00:01.000001Z"),
            Some(1_000_001_000),
        ),
        (
            serde_json::json!("2026-11-12T10:00:05.123456Z"),
            Some(1_794_477_605_123_456_000),
        ),
        (serde_json::json!("1970-01-01T05:30:00+05:30"), Some(0)),
        (
            serde_json::json!("1969-12-31T23:59:59Z"),
            Some(-1_000_000_000),
        ),
        (serde_json::json!("2026-11-12"), None),
        (serde_json::json!(""), None),
        (serde_json::json!(12_345), None),
        (serde_json::Value::Null, None),
        (serde_json::json!("9999-12-31T23:59:59Z"), None),
    ];
    for (v, want) in cases {
        assert_eq!(parse_ts_nanos(&v), want, "{v}");
    }
}

#[test]
fn test_json_i64_table() {
    let cases: Vec<(serde_json::Value, Option<i64>)> = vec![
        (serde_json::json!(0), Some(0)),
        (serde_json::json!(-7), Some(-7)),
        (serde_json::json!(i64::MAX), Some(i64::MAX)),
        (serde_json::json!(i64::MIN), Some(i64::MIN)),
        (serde_json::json!(u64::MAX), None),
        (serde_json::json!(1.5), None),
        (serde_json::json!("42"), Some(42)),
        (serde_json::json!("  -3  "), Some(-3)),
        (serde_json::json!("4.2"), None),
        (serde_json::json!(""), None),
        (serde_json::json!("x"), None),
        (serde_json::json!(true), Some(1)),
        (serde_json::json!(false), Some(0)),
        (serde_json::Value::Null, None),
        (serde_json::json!([1]), None),
        (serde_json::json!({"a": 1}), None),
    ];
    for (v, want) in cases {
        assert_eq!(json_i64(&v), want, "{v}");
    }
}

fn columns_json(names: &[&str]) -> serde_json::Value {
    serde_json::Value::Array(
        names
            .iter()
            .map(|n| serde_json::json!({"name": n, "type": "x"}))
            .collect(),
    )
}

#[test]
fn test_parse_exec_body_errors() {
    assert_eq!(
        parse_exec_body("not json"),
        Err("response is not valid JSON".to_owned())
    );
    assert_eq!(
        parse_exec_body(r#"{"dataset": []}"#),
        Err("response has no `columns` array".to_owned())
    );
    assert_eq!(
        parse_exec_body(r#"{"columns": 3, "dataset": []}"#),
        Err("response has no `columns` array".to_owned())
    );
    assert_eq!(
        parse_exec_body(r#"{"columns": []}"#),
        Err("response has no `dataset` array".to_owned())
    );
    assert_eq!(
        parse_exec_body(r#"{"columns": [], "dataset": {"a": 1}}"#),
        Err("response has no `dataset` array".to_owned())
    );
    assert_eq!(
        parse_exec_body_owned("{".to_owned()),
        Err("response is not valid JSON".to_owned())
    );
    assert_eq!(
        parse_exec_body_owned(r#"{"columns": []}"#.to_owned()),
        Err("response has no `dataset` array".to_owned())
    );
}

#[test]
fn test_parse_exec_body_full_row_every_column() {
    let cols = columns_json(EVENT_COLUMNS);
    let row = serde_json::json!([
        "2026-11-12T10:00:05.000001Z", // ts
        "fill",                        // event
        "77",                          // signal_id as text
        3,                             // seq
        "orb",                         // recipe
        2,                             // recipe_version
        49_081,                        // security_id
        "NSE_FNO",                     // segment
        "NIFTY-Nov2026-24000-CE",      // symbol
        "BUY",                         // side
        75,                            // qty
        "2026-11-12T10:00:00Z",        // candle_end
        "2026-11-12T10:00:01Z",        // decision_ts
        10_000,                        // best_bid_paise
        150,                           // best_bid_qty
        10_005,                        // best_ask_paise
        300,                           // best_ask_qty
        10_002,                        // ltp_paise
        10_100,                        // limit_paise
        11,                            // queue_wait_us
        12,                            // feed_lag_us
        13,                            // api_rtt_us
        75,                            // fill_qty
        10_006,                        // avg_price_paise
        0,                             // remainder_qty
        10_006,                        // model_price_paise
        "not_worse",                   // worse_than_model
        "",                            // reason (empty -> None)
        5,                             // tick_paise
        75,                            // lot_size
        2_500,                         // participation_bps
        60                             // max_fill_seconds
    ]);
    let body = serde_json::json!({"columns": cols, "dataset": [row]}).to_string();
    let (rows, truncated) = parse_exec_body(&body).unwrap_or_default();
    assert!(!truncated);
    assert_eq!(rows.len(), 1);
    let r = &rows[0];
    assert_eq!(r.ts_ns, 1_794_477_605_000_001_000);
    assert_eq!(r.event, "fill");
    assert_eq!(r.signal_id, 77);
    assert_eq!(r.seq, 3);
    assert_eq!(r.recipe, "orb");
    assert_eq!(r.recipe_version, 2);
    assert_eq!(r.security_id, 49_081);
    assert_eq!(r.segment, "NSE_FNO");
    assert_eq!(r.symbol, "NIFTY-Nov2026-24000-CE");
    assert_eq!(r.side, "BUY");
    assert_eq!(r.qty, 75);
    assert_eq!(r.candle_end_ns, Some(1_794_477_600_000_000_000));
    assert_eq!(r.decision_ns, Some(1_794_477_601_000_000_000));
    assert_eq!(
        (
            r.best_bid_paise,
            r.best_bid_qty,
            r.best_ask_paise,
            r.best_ask_qty
        ),
        (Some(10_000), Some(150), Some(10_005), Some(300))
    );
    assert_eq!((r.ltp_paise, r.limit_paise), (Some(10_002), Some(10_100)));
    assert_eq!(
        (r.queue_wait_us, r.feed_lag_us, r.api_rtt_us),
        (Some(11), Some(12), Some(13))
    );
    assert_eq!(
        (
            r.fill_qty,
            r.avg_price_paise,
            r.remainder_qty,
            r.model_price_paise
        ),
        (Some(75), Some(10_006), Some(0), Some(10_006))
    );
    assert_eq!(r.worse_than_model.as_deref(), Some("not_worse"));
    assert_eq!(r.reason, None, "empty text reads as NULL");
    assert_eq!(
        (
            r.tick_paise,
            r.lot_size,
            r.participation_bps,
            r.max_fill_seconds
        ),
        (Some(5), Some(75), Some(2_500), Some(60))
    );
    // The owned form parses identically.
    assert_eq!(parse_exec_body_owned(body).unwrap_or_default().0, rows);
}

#[test]
fn test_parse_exec_body_by_name_defaults_and_skips() {
    // Only three columns, in a different order, plus an unknown one.
    let cols = columns_json(&["signal_id", "extra", "event", "ts"]);
    let dataset = serde_json::json!([
        [5, "zzz", "decision", "2026-11-12T09:30:00Z"], // kept
        [6, "zzz", "", "2026-11-12T09:30:00Z"],         // empty event: skipped
        [7, "zzz", "sent", "bad-ts"],                   // bad ts: skipped
        [null, "zzz", "sent", "2026-11-12T09:30:00Z"],  // no signal: skipped
        "not an array",                                 // skipped
        [8, "zzz", "ack"],                              // missing ts cell: skipped
        [9, "zzz", "ack", "2026-11-12T09:31:00Z", "x"]  // extra cell ignored: kept
    ]);
    let body = serde_json::json!({"columns": cols, "dataset": dataset}).to_string();
    let (rows, truncated) = parse_exec_body(&body).unwrap_or_default();
    assert!(!truncated);
    assert_eq!(
        rows.iter()
            .map(|r| (r.signal_id, r.event.as_str()))
            .collect::<Vec<_>>(),
        vec![(5, "decision"), (9, "ack")]
    );
    let r = &rows[0];
    // Missing columns read as NULL with the documented defaults.
    assert_eq!(r.seq, 0);
    assert_eq!(r.recipe, "");
    assert_eq!(r.recipe_version, 0);
    assert_eq!(r.security_id, -1);
    assert_eq!(r.qty, 0);
    assert_eq!(r.candle_end_ns, None);
    assert_eq!(r.limit_paise, None);
    assert_eq!(r.reason, None);
    assert_eq!(r.max_fill_seconds, None);
    // Column entries with no name are tolerated.
    let body = r#"{"columns":[{"type":"x"},{"name":"ts"},{"name":"event"},{"name":"signal_id"}],
        "dataset":[[1,"2026-11-12T09:30:00Z","built",4]]}"#;
    let (rows, _) = parse_exec_body(body).unwrap_or_default();
    assert_eq!(rows.len(), 1);
    assert_eq!((rows[0].signal_id, rows[0].event.as_str()), (4, "built"));
}

#[test]
fn test_parse_exec_body_truncates_past_the_cap() {
    let cols = columns_json(&["ts", "event", "signal_id"]);
    let make = |n: usize| {
        let mut s = String::with_capacity(n * 48 + 128);
        s.push_str(&format!(r#"{{"columns":{cols},"dataset":["#));
        for i in 0..n {
            if i > 0 {
                s.push(',');
            }
            s.push_str(&format!(r#"["2026-11-12T09:30:00Z","decision",{i}]"#));
        }
        s.push_str("]}");
        s
    };
    let (rows, truncated) = parse_exec_body(&make(REPORT_ROW_CAP)).unwrap_or_default();
    assert_eq!(rows.len(), REPORT_ROW_CAP);
    assert!(!truncated, "exactly the cap is not truncated");
    let (rows, truncated) = parse_exec_body_owned(make(REPORT_ROW_CAP + 1)).unwrap_or_default();
    assert_eq!(rows.len(), REPORT_ROW_CAP);
    assert!(truncated, "one past the cap is truncated");
    assert_eq!(
        rows.last().map(|r| r.signal_id),
        Some(REPORT_ROW_CAP as i64 - 1)
    );
}

#[test]
fn test_row_uploaded_sql_and_parse_table() {
    assert_eq!(
        row_uploaded_sql("2026-11-12").as_deref(),
        Some(
            "SELECT s3_uploaded FROM live_fill_report_daily WHERE ts IN '2026-11-12' \
             AND feed = 'dhan' ORDER BY reported_at DESC LIMIT 1"
        )
    );
    for bad in [
        "",
        "2026-13-01",
        "2026-11-12'; DROP TABLE x; --",
        "12-11-2026",
    ] {
        assert_eq!(row_uploaded_sql(bad), None, "{bad:?}");
    }
    let cases: &[(&str, Result<Option<bool>, String>)] = &[
        (r#"{"dataset":[[true]]}"#, Ok(Some(true))),
        (r#"{"dataset":[[false]]}"#, Ok(Some(false))),
        (r#"{"dataset":[[null]]}"#, Ok(Some(false))),
        (r#"{"dataset":[[]]}"#, Ok(Some(false))),
        (r#"{"dataset":["x"]}"#, Ok(Some(false))),
        (r#"{"dataset":[["true"]]}"#, Ok(Some(false))),
        (r#"{"dataset":[[true],[false]]}"#, Ok(Some(true))),
        (r#"{"dataset":[]}"#, Ok(None)),
        (
            r#"{"columns":[]}"#,
            Err("summary row reply has no dataset".to_owned()),
        ),
        (
            r#"{"dataset":{}}"#,
            Err("summary row reply has no dataset".to_owned()),
        ),
        ("nope", Err("summary row reply is not JSON".to_owned())),
    ];
    for (body, want) in cases {
        assert_eq!(&parse_row_uploaded(body), want, "{body}");
    }
}

proptest! {
    #[test]
    fn test_json_i64_reads_any_i64_as_number_and_text(v in any::<i64>()) {
        prop_assert_eq!(json_i64(&serde_json::json!(v)), Some(v));
        prop_assert_eq!(json_i64(&serde_json::json!(v.to_string())), Some(v));
        prop_assert_eq!(json_i64(&serde_json::json!(format!(" {v}\t"))), Some(v));
    }

    #[test]
    fn test_parse_exec_body_never_panics_on_arbitrary_text(s in ".{0,200}") {
        let _ = parse_exec_body(&s);
        prop_assert_eq!(parse_exec_body(&s), parse_exec_body_owned(s.clone()));
    }
}

// ---------------------------------------------------------------------------
// rows: bounded reads against a mock QuestDB
// ---------------------------------------------------------------------------

/// Serves exactly one HTTP request on 127.0.0.1 port 0 and returns the URL
/// and a handle yielding the request head it received.
async fn serve_once(
    status: &'static str,
    body: String,
) -> (String, tokio::task::JoinHandle<String>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap_or_else(|e| panic!("bind: {e}"));
    let addr = listener
        .local_addr()
        .unwrap_or_else(|e| panic!("addr: {e}"));
    let handle = tokio::spawn(async move {
        let Ok((mut sock, _)) = listener.accept().await else {
            return String::new();
        };
        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
            match sock.read(&mut chunk).await {
                Ok(0) | Err(_) => break,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
            }
        }
        let resp = format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
             Connection: close\r\n\r\n{body}",
            body.len()
        );
        let _ = sock.write_all(resp.as_bytes()).await;
        let _ = sock.shutdown().await;
        String::from_utf8_lossy(&buf).into_owned()
    });
    (format!("http://{addr}/exec"), handle)
}

/// A URL whose port has no listener.
async fn closed_url() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap_or_else(|e| panic!("bind: {e}"));
    let addr = listener
        .local_addr()
        .unwrap_or_else(|e| panic!("addr: {e}"));
    drop(listener);
    format!("http://{addr}/exec")
}

fn percent_decoded(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
                out.push(u8::from_str_radix(hex, 16).unwrap_or(b'?'));
                i += 3;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[tokio::test]
async fn test_read_day_parses_a_served_body_and_sends_the_day_query() {
    let body = serde_json::json!({
        "columns": columns_json(&["ts", "event", "signal_id", "qty"]),
        "dataset": [
            ["2026-11-12T09:30:00Z", "decision", 1, 75],
            ["2026-11-12T09:30:01Z", "sent", 1, 75]
        ]
    })
    .to_string();
    let (url, server) = serve_once("200 OK", body).await;
    let got = read_day(&url, date(2026, 11, 12)).await;
    let head = server.await.unwrap_or_default();
    let (rows, truncated) = got.unwrap_or_else(|e| panic!("read_day failed: {e}"));
    assert!(!truncated);
    assert_eq!(
        rows.iter()
            .map(|r| (r.event.as_str(), r.qty))
            .collect::<Vec<_>>(),
        vec![("decision", 75), ("sent", 75)]
    );
    assert!(head.starts_with("GET /exec?query="), "{head}");
    let first_line = head.lines().next().unwrap_or_default();
    assert!(
        percent_decoded(first_line).contains(&day_sql(date(2026, 11, 12))),
        "{first_line}"
    );
}

#[tokio::test]
async fn test_read_day_error_paths() {
    let (url, server) = serve_once("500 Internal Server Error", "boom".to_owned()).await;
    let got = read_day(&url, date(2026, 11, 12)).await;
    let _ = server.await;
    assert_eq!(got, Err("status 500 Internal Server Error".to_owned()));

    let (url, server) = serve_once("200 OK", "not json".to_owned()).await;
    let got = read_day(&url, date(2026, 11, 12)).await;
    let _ = server.await;
    assert_eq!(got, Err("response is not valid JSON".to_owned()));

    let (url, server) = serve_once("200 OK", r#"{"columns":[]}"#.to_owned()).await;
    let got = read_day(&url, date(2026, 11, 12)).await;
    let _ = server.await;
    assert_eq!(got, Err("response has no `dataset` array".to_owned()));

    let got = read_day(&closed_url().await, date(2026, 11, 12)).await;
    match got {
        Err(e) => assert!(e.starts_with("request: "), "{e}"),
        Ok(v) => panic!("closed port read succeeded: {v:?}"),
    }
}

#[tokio::test]
async fn test_read_row_uploaded_outcomes() {
    let (url, server) = serve_once("200 OK", r#"{"dataset":[[true]]}"#.to_owned()).await;
    let got = read_row_uploaded(&url, "2026-11-12").await;
    let head = server.await.unwrap_or_default();
    assert_eq!(got, Ok(Some(true)));
    assert!(
        percent_decoded(head.lines().next().unwrap_or_default())
            .contains("FROM live_fill_report_daily WHERE ts IN '2026-11-12'"),
        "{head}"
    );

    let (url, server) = serve_once("200 OK", r#"{"dataset":[]}"#.to_owned()).await;
    let got = read_row_uploaded(&url, "2026-11-12").await;
    let _ = server.await;
    assert_eq!(got, Ok(None), "no row is not a failure");

    let (url, server) = serve_once("503 Service Unavailable", String::new()).await;
    let got = read_row_uploaded(&url, "2026-11-12").await;
    let _ = server.await;
    assert_eq!(
        got,
        Err("summary row read status 503 Service Unavailable".to_owned())
    );

    let (url, server) = serve_once("200 OK", "<html>".to_owned()).await;
    let got = read_row_uploaded(&url, "2026-11-12").await;
    let _ = server.await;
    assert_eq!(got, Err("summary row reply is not JSON".to_owned()));

    // A bad day never reaches the network.
    assert_eq!(
        read_row_uploaded("http://127.0.0.1:1/exec", "nope").await,
        Err("bad day".to_owned())
    );

    match read_row_uploaded(&closed_url().await, "2026-11-12").await {
        Err(e) => assert!(e.starts_with("summary row read failed: "), "{e}"),
        Ok(v) => panic!("closed port read succeeded: {v:?}"),
    }
}

// ---------------------------------------------------------------------------
// upload + render helpers
// ---------------------------------------------------------------------------

#[test]
fn test_live_fill_report_key_table() {
    let cases: &[(&str, &str, Option<&str>)] = &[
        (
            "2026-11-12",
            "report-2026-11-12.json",
            Some("live-fill-logs/2026-11-12/report-2026-11-12.json.gz"),
        ),
        ("2026-11-12", "", None),
        ("2026-11-12", "a/b", None),
        ("2026-11-12", "a\\b", None),
        ("2026-11-12", "..", None),
        ("2026-11-12", "x..y", None),
        ("2026-1-2", "f", None),
        ("bad", "f", None),
        ("", "f", None),
    ];
    for &(day, name, want) in cases {
        assert_eq!(
            live_fill_report_key(day, name).as_deref(),
            want,
            "{day} {name}"
        );
    }
}

#[test]
fn test_folder_state_records_and_unreported_listing() {
    let t = TempDir::new("folders");
    let root = &t.0;
    let names = report_file_names("2026-11-03");
    // 2026-11-03: complete, never copied.
    let d3 = root.join("2026-11-03");
    let files: Vec<(String, String)> = names
        .iter()
        .map(|n| (n.clone(), format!("body of {n}")))
        .collect();
    assert!(write_report_files(&d3, &files).is_ok());
    assert!(folder_complete(&d3, "2026-11-03"));
    assert!(!copy_started(&d3));
    assert!(!folder_copied(&d3, "2026-11-03"));
    assert_eq!(
        std::fs::read_to_string(d3.join(&names[0])).unwrap_or_default(),
        format!("body of {}", names[0])
    );
    // 2026-11-02: incomplete (one file only).
    let d2 = root.join("2026-11-02");
    let one = vec![(report_file_names("2026-11-02")[0].clone(), "x".to_owned())];
    assert!(write_report_files(&d2, &one).is_ok());
    assert!(!folder_complete(&d2, "2026-11-02"));
    // 2026-11-04: copied, row missing.
    let d4 = root.join("2026-11-04");
    let n4 = report_file_names("2026-11-04");
    let f4: Vec<(String, String)> = n4.iter().map(|n| (n.clone(), String::new())).collect();
    assert!(write_report_files(&d4, &f4).is_ok());
    assert!(write_report_files(&markers_dir(&d4), &f4).is_ok());
    assert!(copy_started(&d4));
    assert!(folder_copied(&d4, "2026-11-04"));
    assert!(!row_written(&d4));
    // 2026-11-05: copied and row written -> finished, not listed.
    let d5 = root.join("2026-11-05");
    let n5 = report_file_names("2026-11-05");
    let f5: Vec<(String, String)> = n5.iter().map(|n| (n.clone(), String::new())).collect();
    assert!(write_report_files(&d5, &f5).is_ok());
    assert!(write_report_files(&markers_dir(&d5), &f5).is_ok());
    assert!(write_row_written(&d5, "2026-11-05").is_ok());
    assert!(row_written(&d5));
    assert_eq!(
        std::fs::read_to_string(markers_dir(&d5).join(ROW_WRITTEN_RECORD)).unwrap_or_default(),
        "2026-11-05"
    );
    // Noise: a non-date folder, a file named like a date, a day in lookback,
    // a day on/after today.
    std::fs::create_dir_all(root.join("notes")).unwrap_or(());
    std::fs::write(root.join("2026-11-01"), b"file").unwrap_or(());
    std::fs::create_dir_all(root.join("2026-11-09")).unwrap_or(());
    std::fs::create_dir_all(root.join("2026-11-10")).unwrap_or(());
    std::fs::create_dir_all(root.join("2026-11-11")).unwrap_or(());

    let today = date(2026, 11, 10);
    let listed = unreported_day_folders(root, today, |d| d == date(2026, 11, 9));
    let listed = listed.unwrap_or_default();
    let got: Vec<(NaiveDate, bool, bool)> = listed
        .iter()
        .map(|u| (u.day, u.complete, u.copied))
        .collect();
    assert_eq!(
        got,
        vec![
            (date(2026, 11, 2), false, false),
            (date(2026, 11, 3), true, false),
            (date(2026, 11, 4), true, true),
        ],
        "never-copied first, oldest first; finished, lookback and future days left out"
    );
    // Missing root holds none.
    let absent = unreported_day_folders(&root.join("absent"), today, |_| false);
    assert!(matches!(absent.as_deref(), Ok([])), "{absent:?}");
    // A root that is a file cannot be listed: an error, never "none".
    let file_root = root.join("2026-11-01");
    assert!(unreported_day_folders(&file_root, today, |_| false).is_err());
}

#[test]
fn test_upload_summary_all_verified_table() {
    let s = |outs: &[&str]| UploadSummary {
        files: outs
            .iter()
            .enumerate()
            .map(|(i, o)| (format!("f{i}"), (*o).to_owned()))
            .collect(),
    };
    assert!(
        s(&[
            "uploaded",
            "uploaded_sidecar",
            "reused",
            "uploaded",
            "reused"
        ])
        .all_verified()
    );
    assert!(
        !s(&["uploaded"; 4]).all_verified(),
        "four files are not five"
    );
    assert!(
        !s(&["uploaded"; 6]).all_verified(),
        "six files are not five"
    );
    assert!(!s(&["uploaded", "uploaded", "uploaded", "uploaded", "missing"]).all_verified());
    assert!(!s(&["uploaded", "failed:x", "uploaded", "uploaded", "uploaded"]).all_verified());
    assert!(!UploadSummary::default().all_verified());
}

#[test]
fn test_render_files_and_summary_plain() {
    let mut r = report("2026-11-12", sample_counts(), sample_latency());
    r.rejects.insert(
        "risk:max lots, cap".to_owned(),
        RejectTotal { count: 2, qty: 150 },
    );
    r.fill_seconds[0] = 75;
    r.fill_seconds[FILL_SECONDS_BUCKETS - 1] = 25;
    let files = render_files(&r);
    let names: Vec<&str> = files.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(
        names,
        report_file_names("2026-11-12")
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
    );
    assert!(files[0].1.ends_with("}\n"));
    assert!(files[2].1.starts_with("offset_seconds,shares\n0,75\n1,0\n"));
    assert!(files[2].1.ends_with("unfilled,25\n"));
    assert!(
        files[3]
            .1
            .contains("built_to_sent_us,2,100,200,200,200,1\n")
    );
    assert_eq!(
        files[4].1,
        "reason,count,qty\n\"risk:max lots, cap\",2,150\n"
    );
    assert_eq!(csv_field("a\"b"), "\"a\"\"b\"");
    assert_eq!(csv_field("line\nbreak"), "\"line\nbreak\"");
    assert_eq!(csv_field("plain"), "plain");

    let line = summary_plain(&r);
    assert_eq!(
        line,
        "live paper fill report 2026-11-12: green (complete), 12 orders: 7 filled, 2 part \
         filled, 1 not filled, 1 skipped, 1 rejected; worst-case check: 0 match, 5 worse, \
         4 better, 6 not checked"
    );
    r.no_orders_reason = Some("strategy ran, no signals");
    r.status_reasons = vec!["not_settled", "truncated"];
    assert!(summary_plain(&r).ends_with(
        "; no orders: strategy ran, no signals; partial because: not_settled, truncated"
    ));
}
