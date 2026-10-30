//! Coverage for `tick_conservation::spawn_tick_conservation` end to end,
//! against a QuestDB-shaped server on 127.0.0.1 and a paused tokio clock.
//!
//! The task reads the wall clock, so what it writes depends on when the test
//! runs. The expectation is computed from the same clock with the module's
//! own pure functions (`process_down_gap_row`, `secs_until_check`) and then
//! asserted:
//! - the downtime row (`feed_gap_audit`, reason `process_down`) is written
//!   exactly when a mid-session boot after a gap would write it;
//! - the daily `tick_conservation_audit` row is written exactly when today is
//!   a weekday (no-holiday calendar) and the 15:45 IST check is still ahead.
//!
//! One test in this binary: it changes the working directory to a temp folder
//! so nothing the writers might spill lands inside the repository.

mod cov_xvs_support;

use std::sync::Arc;
use std::time::Duration;

use cov_xvs_support::{FakeQuestDb, TempRoot, init_tracing, is_weekday, ist_now, weekday_calendar};
use tickvault_app::tick_conservation::{
    PROCESS_DOWN_REASON, process_down_gap_row, secs_until_check, spawn_tick_conservation,
};

const IST_OFFSET_NANOS: i64 = 19_800 * 1_000_000_000;
const TWO_HOURS_NANOS: i64 = 2 * 3_600 * 1_000_000_000;

fn lines_of(srv: &FakeQuestDb, table: &str) -> Vec<String> {
    srv.writes()
        .iter()
        .flat_map(|w| w.lines().map(str::to_owned).collect::<Vec<_>>())
        .filter(|l| l.starts_with(table))
        .collect()
}

#[tokio::test(start_paused = true)]
async fn test_tick_conservation_spawn_tick_conservation_writes_rows_the_clock_calls_for() {
    init_tracing();
    let original_cwd = std::env::current_dir().expect("cwd");
    let root = TempRoot::new("tick-conservation-spawn");
    std::env::set_current_dir(&root.path).expect("chdir temp");

    let srv = FakeQuestDb::start(Arc::new(|q: &str| {
        if q.contains("count(") {
            // A database count the empty ledger cannot match.
            (200, r#"{"columns":[],"dataset":[[5]]}"#.to_owned())
        } else {
            (200, r#"{"ddl":"OK"}"#.to_owned())
        }
    }));

    let now_utc = chrono::Utc::now().timestamp_nanos_opt().expect("now");
    let previous = now_utc - TWO_HOURS_NANOS;
    let (today, secs_of_day) = ist_now();
    let trading = is_weekday(today);
    let expect_gap_row = process_down_gap_row(Some(previous), now_utc, trading).is_some();
    let expect_daily_row = trading && secs_until_check(secs_of_day).is_some();
    // The process "started" at the IST instant of this boot.
    let process_start_ist = now_utc + IST_OFFSET_NANOS;

    let handle = spawn_tick_conservation(
        srv.questdb_config(),
        weekday_calendar(),
        process_start_ist,
        Some(previous),
    );

    let real_start = std::time::Instant::now();
    while real_start.elapsed() < Duration::from_secs(3) {
        tokio::time::sleep(Duration::from_secs(600)).await;
        let gap_done = !expect_gap_row || !lines_of(&srv, "feed_gap_audit").is_empty();
        let daily_done = !expect_daily_row || !lines_of(&srv, "tick_conservation_audit").is_empty();
        if gap_done && daily_done && (expect_gap_row || expect_daily_row) {
            break;
        }
    }
    assert!(
        !handle.is_finished(),
        "the daily loop never ends on its own"
    );
    handle.abort();
    let join = handle.await.expect_err("aborted task");
    assert!(join.is_cancelled(), "the task was cancelled, not panicked");

    std::env::set_current_dir(&original_cwd).expect("chdir back");
    root.cleanup();

    let (end_day, _) = ist_now();
    if end_day != today {
        return; // crossed IST midnight during the test
    }
    let gap_rows = lines_of(&srv, "feed_gap_audit");
    if expect_gap_row {
        assert_eq!(gap_rows.len(), 1, "one downtime row: {gap_rows:?}");
        assert!(gap_rows[0].contains(PROCESS_DOWN_REASON), "{}", gap_rows[0]);
    } else {
        assert!(
            gap_rows.is_empty(),
            "no downtime row outside the session: {gap_rows:?}"
        );
    }
    let daily = lines_of(&srv, "tick_conservation_audit");
    if expect_daily_row {
        assert_eq!(daily.len(), 1, "one daily row: {daily:?}");
        assert!(daily[0].contains("feed=dhan"), "{}", daily[0]);
        // The ledger saw no rows; whatever the count read returned, the day
        // can never be a match.
        assert!(!daily[0].contains("verdict=match"), "{}", daily[0]);
    } else {
        assert!(
            daily.is_empty(),
            "no daily row after the check time: {daily:?}"
        );
    }
}
