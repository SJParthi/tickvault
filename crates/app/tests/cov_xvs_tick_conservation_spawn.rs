//! Coverage for `tick_conservation::spawn_tick_conservation` end to end,
//! against a QuestDB-shaped server on 127.0.0.1 and a paused tokio clock.
//!
//! The task reads the wall clock, so what it writes depends on when the test
//! runs. The expectations are written out by hand here from the module's
//! documented rules (never by calling the functions under test), with fixed
//! inputs:
//! - the previous receipt is two hours before now, the process start is
//!   12:00:00 IST today (so the day is never fully covered);
//! - the downtime row (`feed_gap_audit`, reason `process_down`) is written
//!   when today is a weekday (no-holiday calendar) and now is inside
//!   09:15:00..15:40:00 IST and more than 5 s after 09:15 (the gap is
//!   `now - max(previous, 09:15)`, and a gap of 5 s or less is a restart);
//! - the daily `tick_conservation_audit` row is written when today is a
//!   weekday and now is before the 15:45:00 IST check, and it reads
//!   `verdict=partial`, `reason=process_started_mid_day`, with the database
//!   count 5 + 5 = 10 (two tables) read once.
//!
//! A clock reading within a few seconds of one of those edges is not asserted
//! (the task reads the wall clock a moment after the test does).
//!
//! One test in this binary: it changes the working directory to a temp folder
//! so nothing the writers might spill lands inside the repository.

mod cov_xvs_support;

use std::sync::Arc;
use std::time::Duration;

use cov_xvs_support::{
    CwdGuard, FakeQuestDb, drive_paused, init_no_proxy, init_tracing, is_weekday, ist_now,
    lock_cwd, near, weekday_calendar,
};
use tickvault_app::tick_conservation::{PROCESS_DOWN_REASON, spawn_tick_conservation};

const NANOS_PER_SEC: i64 = 1_000_000_000;
const IST_OFFSET_NANOS: i64 = 19_800 * NANOS_PER_SEC;
const TWO_HOURS_NANOS: i64 = 2 * 3_600 * NANOS_PER_SEC;
/// 09:15:00 IST, the session open.
const SESSION_OPEN_SECS: u32 = 33_300;
/// A gap of 5 s or less after the open is a restart, not downtime.
const RESTART_GAP_SECS: u32 = 5;
/// 15:40:00 IST, the session close.
const SESSION_CLOSE_SECS: u32 = 56_400;
/// 15:45:00 IST, the daily check.
const CHECK_SECS: u32 = 56_700;
/// 12:00:00 IST, the fixed process start: after 09:00, so never full coverage.
const PROCESS_START_SECS: i64 = 43_200;
/// Seconds of slack around an edge, for the gap between the two clock reads.
const EDGE_MARGIN_SECS: u32 = 10;

#[tokio::test(start_paused = true)]
async fn test_tick_conservation_spawn_tick_conservation_writes_rows_the_clock_calls_for() {
    init_no_proxy();
    init_tracing();
    let _cwd_lock = lock_cwd().await;
    let cwd = CwdGuard::enter("tick-conservation-spawn");

    let srv = FakeQuestDb::start(Arc::new(|q: &str| {
        if q.contains("count(") {
            // Each of the two tables counts 5 rows.
            (200, r#"{"columns":[],"dataset":[[5]]}"#.to_owned())
        } else {
            (200, r#"{"ddl":"OK"}"#.to_owned())
        }
    }));

    let now_utc = chrono::Utc::now().timestamp_nanos_opt().expect("now");
    let previous = now_utc - TWO_HOURS_NANOS;
    let (today, secs) = ist_now();
    let trading = is_weekday(today);
    let in_session = secs > SESSION_OPEN_SECS + RESTART_GAP_SECS && secs < SESSION_CLOSE_SECS;
    let gap_decided = !near(secs, SESSION_OPEN_SECS + RESTART_GAP_SECS, EDGE_MARGIN_SECS)
        && !near(secs, SESSION_CLOSE_SECS, EDGE_MARGIN_SECS);
    let expect_gap_row = trading && in_session;
    let expect_daily_row = trading && secs < CHECK_SECS;
    let daily_decided = !near(secs, CHECK_SECS, EDGE_MARGIN_SECS);
    let now_ist = now_utc + IST_OFFSET_NANOS;
    let day_start_ist = now_ist - now_ist.rem_euclid(86_400 * NANOS_PER_SEC);
    let process_start_ist = day_start_ist + PROCESS_START_SECS * NANOS_PER_SEC;

    let handle = spawn_tick_conservation(
        srv.questdb_config(),
        weekday_calendar(),
        process_start_ist,
        Some(previous),
    );

    let gap_rows = || srv.ilp_lines("feed_gap_audit");
    let daily_rows = || srv.ilp_lines("tick_conservation_audit");
    // 17 h of virtual time reaches the 15:45 check from any start. The task's
    // next run after a row is past the following IST midnight (at least 8 h
    // 15 min later), and the loop stops at the first settle that sees the
    // rows, before that.
    drive_paused(&srv, Duration::from_secs(17 * 3_600), || {
        let gap_done = !expect_gap_row || !gap_rows().is_empty();
        let daily_done = !expect_daily_row || !daily_rows().is_empty();
        gap_done && daily_done && (expect_gap_row || expect_daily_row)
    })
    .await;
    assert!(
        !handle.is_finished(),
        "the daily loop never ends on its own"
    );
    handle.abort();
    let join = handle.await.expect_err("aborted task");
    assert!(join.is_cancelled(), "the task was cancelled, not panicked");

    let gap = gap_rows();
    let daily = daily_rows();
    let queries = srv.exec_queries();
    let requests = srv.request_lines();
    drop(cwd);

    // Every day: the daily-check table is ensured first, Dhan never called.
    assert!(
        queries
            .iter()
            .any(|q| q.starts_with("CREATE TABLE IF NOT EXISTS tick_conservation_audit")),
        "the audit table DDL reached /exec: {queries:?}"
    );
    assert!(
        !requests.iter().any(|r| r.contains("/v2/charts/intraday")),
        "the vendor is never called: {requests:?}"
    );
    // Exactly one of each, never more: the downtime row is written once at
    // start; the daily row once per run, and the next run is hours of virtual
    // time after the loop above stopped. A count re-read (120 s apart) adds no
    // row: the task writes one row after its last read.
    assert!(gap.len() <= 1, "{gap:?}");
    assert!(daily.len() <= 1, "{daily:?}");

    let (end_day, _) = ist_now();
    if end_day != today {
        return; // crossed IST midnight during the test
    }
    if gap_decided {
        if expect_gap_row {
            assert_eq!(gap.len(), 1, "one downtime row: {gap:?}");
            assert!(
                gap[0].contains(&format!("reason={PROCESS_DOWN_REASON}")),
                "{}",
                gap[0]
            );
            assert!(gap[0].contains("connection_index=-1i"), "{}", gap[0]);
            assert!(
                queries
                    .iter()
                    .any(|q| q.starts_with("CREATE TABLE IF NOT EXISTS feed_gap_audit")),
                "the downtime table DDL reached /exec: {queries:?}"
            );
        } else {
            assert!(
                gap.is_empty(),
                "no downtime row outside the session: {gap:?}"
            );
        }
    }
    if daily_decided {
        if expect_daily_row {
            assert_eq!(daily.len(), 1, "one daily row: {daily:?}");
            let line = &daily[0];
            assert!(line.contains("feed=dhan"), "{line}");
            // Started at 12:00 IST: never full coverage, so `partial` whatever
            // the count, and the count (5 + 5) settles it on the first read.
            assert!(line.contains("verdict=partial"), "{line}");
            assert!(line.contains("reason=process_started_mid_day"), "{line}");
            assert!(line.contains("db_tick_rows=10i"), "{line}");
            assert!(line.contains("count_reads=1i"), "{line}");
        } else {
            assert!(
                daily.is_empty(),
                "no daily row after the check time: {daily:?}"
            );
        }
    }
}
