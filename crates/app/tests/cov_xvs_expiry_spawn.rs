//! Coverage for `expiry_scope_check::spawn_expiry_scope_check` end to end,
//! against a QuestDB-shaped server on 127.0.0.1 and a paused tokio clock.
//!
//! The task is driven through `spawn_expiry_scope_check_with_clock` with a
//! fixed clock (B3 review, 2026-11-13), so a weekday run (one
//! `expiry_scope_audit` row over ILP/HTTP), a Muhurat Sunday (one row), a
//! weekday holiday (nothing) and a weekend run (nothing) are all exercised on
//! every test run, whatever day CI runs on.
//!
//! One test in this binary: it changes the process working directory to a
//! temp folder so the relative contract-artifact read finds the file written
//! here and nothing is written inside the repository.

mod cov_xvs_support;

use std::sync::Arc;
use std::time::Duration;

use chrono::{NaiveDate, NaiveDateTime};
use cov_xvs_support::{
    CwdGuard, FakeQuestDb, drive_paused, init_no_proxy, init_tracing, lock_cwd, session_calendar,
    weekday_calendar,
};
use tickvault_app::expiry_scope_check::spawn_expiry_scope_check_with_clock;
use tickvault_storage::expiry_scope_audit_persistence::EXPIRY_SCOPE_AUDIT_TABLE;

fn responder() -> cov_xvs_support::ExecResponder {
    Arc::new(|q: &str| {
        if q.contains("wal_tables") {
            (200, r#"{"columns":[],"dataset":[]}"#.to_owned())
        } else if q.contains("candles_1m") {
            (
                200,
                r#"{"columns":[],"dataset":[[1001,"NSE_FNO",null],["x","NSE_FNO","BAD"],[]]}"#
                    .to_owned(),
            )
        } else {
            (200, r#"{"ddl":"OK"}"#.to_owned())
        }
    })
}

fn write_artifact(root: &std::path::Path, day: chrono::NaiveDate) {
    let dir = root.join("data/instrument-cache");
    std::fs::create_dir_all(&dir).expect("artifact dir");
    // One stock option on a far expiry: never expired, and with a single leg
    // its underlying has no real expiry, so the day can never read as a
    // violation and never as clean.
    let rows =
        r#"[{"i":1001,"x":"NSE","c":"OPTSTK","e":20991231,"s":100000,"l":"CE","u":"ABC","z":1}]"#;
    for d in [day, day.succ_opt().unwrap_or(day)] {
        let name = format!("dhan-contracts-{}.json", d.format("%Y-%m-%d"));
        std::fs::write(dir.join(name), rows).expect("artifact write");
    }
}

/// The `partial` reason the weekday run lands on: this binary never drains the
/// seal writer after a catch-up sweep, so the first readiness barrier (seals
/// sealed and saved) never holds. `classify_expiry_scope` returns it only
/// after the artifact was read and the candle read succeeded.
const WEEKDAY_READINESS_REASON: &str = "seals_pending";

const NANOS_PER_SEC: i64 = 1_000_000_000;
const IST_OFFSET_SECS: i64 = 19_800;

fn ist(date: (i32, u32, u32), hms: (u32, u32, u32)) -> NaiveDateTime {
    NaiveDate::from_ymd_opt(date.0, date.1, date.2)
        .and_then(|d| d.and_hms_opt(hms.0, hms.1, hms.2))
        .expect("a valid IST instant")
}

/// IST-epoch nanos of an IST wall-clock instant (how the audit row stamps
/// `checked_at`).
fn ist_nanos_of(ist_instant: NaiveDateTime) -> i64 {
    ist_instant.and_utc().timestamp() * NANOS_PER_SEC
}

/// The designated timestamp (the row's `checked_at`, IST-epoch nanos) at the end
/// of an ILP line, in either spelling the writer may use (bare or `n` nanos,
/// `t` micros).
fn designated_ts(line: &str) -> Option<i64> {
    let last = line.trim_end().rsplit(' ').next()?;
    if let Some(micros) = last.strip_suffix('t') {
        return micros.parse::<i64>().ok().map(|m| m * 1_000);
    }
    last.strip_suffix('n').unwrap_or(last).parse::<i64>().ok()
}

/// A clock that reads `start` (IST wall time) now and moves with tokio's
/// paused clock.
fn clock_from(start: NaiveDateTime) -> impl Fn() -> i64 + Send + Sync + 'static {
    let base = (start.and_utc().timestamp() - IST_OFFSET_SECS) * NANOS_PER_SEC;
    let anchor = tokio::time::Instant::now();
    move || base + i64::try_from(anchor.elapsed().as_nanos()).unwrap_or(i64::MAX)
}

/// One task on its own fake server, started at `start` IST and driven for
/// `horizon` of virtual time (or until it writes a row). Returns the audit
/// rows, the `/exec` queries, the request lines, and what the injected clock
/// read (IST-epoch nanos) when the drive stopped.
async fn run_from(
    start: NaiveDateTime,
    horizon: Duration,
) -> (Vec<String>, Vec<String>, Vec<String>, i64) {
    run_on(weekday_calendar(), start, horizon).await
}

/// [`run_from`] on a chosen calendar.
async fn run_on(
    calendar: Arc<tickvault_common::trading_calendar::TradingCalendar>,
    start: NaiveDateTime,
    horizon: Duration,
) -> (Vec<String>, Vec<String>, Vec<String>, i64) {
    let srv = FakeQuestDb::start(responder());
    // The clock below is anchored at this same paused instant, so
    // `start + (now - t0)` is exactly what it reads later.
    let t0 = tokio::time::Instant::now();
    let handle =
        spawn_expiry_scope_check_with_clock(srv.questdb_config(), calendar, clock_from(start));
    let rows = || srv.ilp_lines(EXPIRY_SCOPE_AUDIT_TABLE);
    drive_paused(&srv, horizon, || !rows().is_empty()).await;
    let elapsed = i64::try_from((tokio::time::Instant::now() - t0).as_nanos()).expect("nanos");
    let end_ist = ist_nanos_of(start) + elapsed;
    assert!(
        !handle.is_finished(),
        "the daily loop never ends on its own"
    );
    handle.abort();
    let join = handle.await.expect_err("aborted task");
    assert!(join.is_cancelled(), "the task was cancelled, not panicked");
    (rows(), srv.exec_queries(), srv.request_lines(), end_ist)
}

fn assert_common(queries: &[String], requests: &[String]) {
    assert!(
        queries
            .iter()
            .any(|q| q.starts_with("CREATE TABLE IF NOT EXISTS expiry_scope_audit")),
        "the audit table DDL reached /exec: {queries:?}"
    );
    assert!(
        !requests.iter().any(|r| r.contains("/v2/charts/intraday")),
        "the vendor is never called: {requests:?}"
    );
}

/// B3 review (2026-11-13): the task used to read the wall clock, so the run
/// path (`run_one_check`, the candle read, the row write) was covered only
/// when CI ran on an IST weekday, and the production coverage floor moved
/// with the day of the week. With the clock passed in, both a weekday and a
/// weekend run on every test run.
#[tokio::test(start_paused = true)]
async fn test_expiry_scope_check_spawn_runs_a_weekday_and_skips_a_weekend_whatever_the_wall_clock()
{
    init_no_proxy();
    init_tracing();
    let _cwd_lock = lock_cwd().await;
    let weekday = NaiveDate::from_ymd_opt(2026, 11, 11).expect("Wednesday");
    let cwd = CwdGuard::enter("expiry-spawn");
    write_artifact(cwd.path(), weekday);
    let muhurat = NaiveDate::from_ymd_opt(2026, 11, 8).expect("Muhurat Sunday");
    write_artifact(cwd.path(), muhurat);

    // A Wednesday at 10:00 IST: the task waits to 15:50 and writes one row
    // for that day, reading that day's candles.
    let (found, queries, requests, end_ist) = run_from(
        ist((2026, 11, 11), (10, 0, 0)),
        Duration::from_secs(17 * 3_600),
    )
    .await;
    assert_common(&queries, &requests);
    assert_eq!(found.len(), 1, "a weekday run writes one row: {found:?}");
    let line = &found[0];
    assert!(line.contains("feed=dhan"), "{line}");
    assert!(
        line.contains("verdict=partial"),
        "an under-specified day is never clean nor a violation: {line}"
    );
    assert!(
        queries
            .iter()
            .any(|q| q.contains("from candles_1m where ts in '2026-11-11'")),
        "the injected day's candle read reached /exec: {queries:?}"
    );
    // B3 review (2026-11-13): `verdict=partial` alone proves little, since a
    // missing artifact and a failed candle read are partial too. The reason
    // must be the readiness one: `classify_expiry_scope` returns a readiness
    // reason only after the artifact was read and the candle read succeeded.
    // The artifact exists only for the injected day and the day after, so an
    // artifact read by any other date (the wall clock's) lands
    // `artifact_missing` and fails here.
    assert!(
        line.contains(&format!("reason={WEEKDAY_READINESS_REASON}")),
        "the artifact and the candles were both read for the injected day: {line}"
    );
    assert!(
        !line.contains("rows_read=-1i"),
        "the candle read succeeded: {line}"
    );
    // The row's `checked_at` follows the injected clock: no earlier than
    // 15:50 IST on the injected day, and no later than what that clock read
    // when the drive stopped. (Not an exact instant: the driver advances the
    // paused clock in steps of up to half an hour, so a sleeping task wakes
    // up to one step after its deadline.) A row stamped by the wall clock
    // falls outside this window.
    let check = ist_nanos_of(ist((2026, 11, 11), (15, 50, 0)));
    let checked_at = designated_ts(line).expect("a designated timestamp");
    assert!(
        (check..=end_ist).contains(&checked_at),
        "checked_at {checked_at} is between 15:50 IST on the injected day \
         ({check}) and the injected clock at the end ({end_ist}), not the wall \
         clock: {line}"
    );

    // B3 review (2026-11-13): a Muhurat session is a session day (operator
    // Quote 49). On the Muhurat Sunday the task writes that day's row at
    // 15:50; before, it gated on `is_trading_day` and skipped it.
    let (found, queries, requests, end_ist) = run_on(
        session_calendar(),
        ist((2026, 11, 8), (10, 0, 0)),
        Duration::from_secs(13 * 3_600),
    )
    .await;
    assert_common(&queries, &requests);
    assert_eq!(found.len(), 1, "a Muhurat Sunday writes one row: {found:?}");
    assert!(
        queries
            .iter()
            .any(|q| q.contains("from candles_1m where ts in '2026-11-08'")),
        "the Muhurat day's candle read reached /exec: {queries:?}"
    );
    let check = ist_nanos_of(ist((2026, 11, 8), (15, 50, 0)));
    let checked_at = designated_ts(&found[0]).expect("a designated timestamp");
    assert!(
        (check..=end_ist).contains(&checked_at),
        "Muhurat checked_at {checked_at} not in [{check}, {end_ist}]: {}",
        found[0]
    );

    // A weekday holiday (Monday 2026-11-09) at 10:00 IST: nothing that day.
    let (found, queries, requests, _) = run_on(
        session_calendar(),
        ist((2026, 11, 9), (10, 0, 0)),
        Duration::from_secs(13 * 3_600),
    )
    .await;
    assert_common(&queries, &requests);
    assert!(
        found.is_empty(),
        "a weekday holiday writes no row: {found:?}"
    );
    assert!(
        !queries.iter().any(|q| q.contains("candles_1m")),
        "a weekday holiday reads no candles: {queries:?}"
    );

    // A Saturday at 10:00 IST: nothing is read or written through Sunday.
    let (found, queries, requests, _) = run_from(
        ist((2026, 11, 14), (10, 0, 0)),
        Duration::from_secs(37 * 3_600),
    )
    .await;
    drop(cwd);
    assert_common(&queries, &requests);
    assert!(found.is_empty(), "a weekend writes no row: {found:?}");
    assert!(
        !queries.iter().any(|q| q.contains("candles_1m")),
        "a weekend reads no candles: {queries:?}"
    );
}
