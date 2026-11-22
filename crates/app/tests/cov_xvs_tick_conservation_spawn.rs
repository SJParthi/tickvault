//! Coverage for `tick_conservation::spawn_tick_conservation_with_clock` end to
//! end, against a QuestDB-shaped server on 127.0.0.1 and a paused tokio clock.
//!
//! B3 (2026-11-13, audit finding: the old version read the wall clock, so its
//! decisive checks ran only when CI happened to start on a weekday between
//! 09:15 and 15:45 IST, and were skipped on a weekend, after 15:45 or across
//! IST midnight). The task now takes its clock as an input. Each scenario
//! below starts the clock at a fixed IST instant and moves it with tokio's
//! paused clock, so every branch is asserted on every run, at any hour.
//! One limit: if the midnight-rollover sleep read the wall clock instead,
//! scenario 4 would fail only when CI ran between about 00:00 and 08:15 IST,
//! so that read is pinned by the source scan
//! `tick_conservation::tests::test_run_daily_checks_loop_reads_only_the_injected_clock`
//! (B3 review round 3).
//!
//!
//! | scenario | start (IST) | downtime row | daily row |
//! |---|---|---|---|
//! | in session | Tue 2026-11-10 11:00:00 | yes, 09:15 to 11:00 (6,300 s) | Tue, at 15:45 |
//! | after the check | Tue 2026-11-10 16:00:00 | no (after 15:40) | none on Tue; Wed at 15:45 |
//! | weekend | Sat 2026-11-14 11:00:00 | no (not a trading day) | none Sat/Sun; Mon at 15:45 |
//! | IST midnight | Tue 2026-11-10 23:59:58 | no | none on Tue; Wed at 15:45 |
//! | restart edge | Tue 09:15:05 / 09:15:06 | no at a 5 s gap; yes (6 s) at 6 s | not driven |
//! | Muhurat Sunday | Sun 2026-11-08 15:00:00 | yes, 14:05 to 15:00 (3,300 s) | Sun, at 15:45 |
//! | weekday holiday | Mon 2026-11-09 11:00:00 | no (a holiday) | none on Mon; Tue at 15:45 |
//!
//! The last two run on `session_calendar` (one Muhurat Sunday at the 2026
//! session times, one weekday holiday); the rest on `weekday_calendar`. B3
//! review (2026-11-13): the task gated on `is_trading_day`, false for a
//! Muhurat session on a weekend, so a Muhurat day got no row; a Muhurat
//! session is a session day (operator Quote 49) and these two pin both sides.
//!
//! The expectations are written out by hand from the module's documented rules
//! (never by calling the functions under test). The hand-written edges are
//! pinned to the exported constants below, so a moved check time or session
//! edge fails here by name instead of drifting silently.
//!
//! Every daily row reads `verdict=partial`, `reason=process_started_mid_day`
//! (the process start is never before 09:00 on the checked day) and
//! `db_tick_rows=10i` (5 + 5 over the two tables), read once, or twice when
//! the first read timed out under the paused clock (see `assert_daily_row`).
//!
//! One test in this binary: it changes the working directory to a temp folder
//! so nothing the writers might spill lands inside the repository.

mod cov_xvs_support;

use std::sync::Arc;
use std::time::Duration;

use chrono::{NaiveDate, NaiveDateTime};
use cov_xvs_support::{
    CwdGuard, FakeQuestDb, drive_paused, init_no_proxy, init_tracing, lock_cwd, session_calendar,
    weekday_calendar,
};
use tickvault_app::tick_conservation::{
    PROCESS_DOWN_MIN_GAP_NANOS, PROCESS_DOWN_REASON, TICK_CONSERVATION_CHECK_SECS_OF_DAY_IST,
    spawn_tick_conservation_with_clock,
};
use tickvault_common::constants::{MARKET_CLOSE_IST_NANOS, MARKET_OPEN_IST_NANOS};

const NANOS_PER_SEC: i64 = 1_000_000_000;
const IST_OFFSET_SECS: i64 = 19_800;
const TWO_HOURS_NANOS: i64 = 2 * 3_600 * NANOS_PER_SEC;
/// 09:15:00 IST, the session open.
const SESSION_OPEN_SECS: i64 = 33_300;
/// A gap of 5 s or less after the open is a restart, not downtime.
const RESTART_GAP_SECS: i64 = 5;
/// 15:40:00 IST, the session close.
const SESSION_CLOSE_SECS: i64 = 56_400;
/// 15:45:00 IST, the daily check.
const CHECK_SECS: i64 = 56_700;

fn ist(date: (i32, u32, u32), hms: (u32, u32, u32)) -> NaiveDateTime {
    NaiveDate::from_ymd_opt(date.0, date.1, date.2)
        .and_then(|d| d.and_hms_opt(hms.0, hms.1, hms.2))
        .expect("a valid IST instant")
}

/// UTC epoch nanos of an IST wall-clock instant.
fn utc_nanos_of(ist_instant: NaiveDateTime) -> i64 {
    (ist_instant.and_utc().timestamp() - IST_OFFSET_SECS) * NANOS_PER_SEC
}

/// IST-epoch nanos of an IST wall-clock instant (how the audit rows stamp it).
fn ist_nanos_of(ist_instant: NaiveDateTime) -> i64 {
    ist_instant.and_utc().timestamp() * NANOS_PER_SEC
}

/// A clock that reads `start` now and moves with tokio's (paused) clock.
fn clock_from(start: NaiveDateTime) -> impl Fn() -> i64 + Send + Sync + 'static {
    let base = utc_nanos_of(start);
    let anchor = tokio::time::Instant::now();
    move || base + i64::try_from(anchor.elapsed().as_nanos()).unwrap_or(i64::MAX)
}

/// An ILP timestamp field holding `ist_nanos`, in either unit the writer may
/// use (microseconds with `t`, nanoseconds with `n`).
fn has_ts_field(line: &str, field: &str, ist_nanos: i64) -> bool {
    line.contains(&format!("{field}={}t", ist_nanos / 1_000))
        || line.contains(&format!("{field}={ist_nanos}n"))
}

/// The designated timestamp (the row's `checked_at`, IST-epoch nanos) at the
/// end of an ILP line, in either spelling the writer may use (bare or `n`
/// nanos, `t` micros).
fn designated_ts(line: &str) -> Option<i64> {
    let last = line.trim_end().rsplit(' ').next()?;
    if let Some(micros) = last.strip_suffix('t') {
        return micros.parse::<i64>().ok().map(|m| m * 1_000);
    }
    last.strip_suffix('n').unwrap_or(last).parse::<i64>().ok()
}

/// One running task against its own fake server.
struct Scenario {
    srv: FakeQuestDb,
    handle: tokio::task::JoinHandle<()>,
    /// IST-epoch nanos the injected clock read at `t0`.
    start_ist: i64,
    /// The paused instant the injected clock is anchored at.
    t0: tokio::time::Instant,
}

impl Scenario {
    fn start(
        start: NaiveDateTime,
        previous_utc_nanos: Option<i64>,
        process_start: NaiveDateTime,
    ) -> Self {
        Self::start_on(weekday_calendar(), start, previous_utc_nanos, process_start)
    }

    fn start_on(
        calendar: Arc<tickvault_common::trading_calendar::TradingCalendar>,
        start: NaiveDateTime,
        previous_utc_nanos: Option<i64>,
        process_start: NaiveDateTime,
    ) -> Self {
        let srv = FakeQuestDb::start(Arc::new(|q: &str| {
            if q.contains("count(") {
                // Each of the two tables counts 5 rows.
                (200, r#"{"columns":[],"dataset":[[5]]}"#.to_owned())
            } else {
                (200, r#"{"ddl":"OK"}"#.to_owned())
            }
        }));
        // The clock below is anchored at this same paused instant, so
        // `start + (now - t0)` is exactly what it reads later.
        let t0 = tokio::time::Instant::now();
        let handle = spawn_tick_conservation_with_clock(
            srv.questdb_config(),
            calendar,
            ist_nanos_of(process_start),
            previous_utc_nanos,
            clock_from(start),
        );
        Self {
            srv,
            handle,
            start_ist: ist_nanos_of(start),
            t0,
        }
    }

    /// What the injected clock reads now, IST-epoch nanos.
    fn clock_now_ist(&self) -> i64 {
        let elapsed = tokio::time::Instant::now() - self.t0;
        self.start_ist + i64::try_from(elapsed.as_nanos()).expect("nanos")
    }

    fn gap(&self) -> Vec<String> {
        self.srv.ilp_lines("feed_gap_audit")
    }

    fn daily(&self) -> Vec<String> {
        self.srv.ilp_lines("tick_conservation_audit")
    }

    /// Drives the clock for `horizon` with no stop condition.
    async fn advance(&self, horizon: Duration) {
        let _ = drive_paused(&self.srv, horizon, || false).await;
    }

    /// Drives the clock until a daily row exists, at most `horizon`.
    async fn until_daily(&self, horizon: Duration) {
        let srv = &self.srv;
        let _ = drive_paused(srv, horizon, || {
            !srv.ilp_lines("tick_conservation_audit").is_empty()
        })
        .await;
    }

    /// Common end checks, then stops the task.
    async fn finish(self, label: &str) {
        let queries = self.srv.exec_queries();
        let requests = self.srv.request_lines();
        assert!(
            queries
                .iter()
                .any(|q| q.starts_with("CREATE TABLE IF NOT EXISTS tick_conservation_audit")),
            "{label}: the audit table DDL reached /exec: {queries:?}"
        );
        assert!(
            !requests.iter().any(|r| r.contains("/v2/charts/intraday")),
            "{label}: the vendor is never called: {requests:?}"
        );
        assert!(
            !self.handle.is_finished(),
            "{label}: the daily loop never ends on its own"
        );
        self.handle.abort();
        let join = self.handle.await.expect_err("aborted task");
        assert!(join.is_cancelled(), "{label}: cancelled, not panicked");
    }
}

/// The one daily row: `partial`, mid-day start, 5 + 5 rows, for `day`.
/// `clock_now_ist` is what the injected clock read when the row was seen.
fn assert_daily_row(label: &str, daily: &[String], day: NaiveDate, clock_now_ist: i64) {
    assert_eq!(daily.len(), 1, "{label}: exactly one daily row: {daily:?}");
    let line = &daily[0];
    assert!(line.contains("feed=dhan"), "{label}: {line}");
    assert!(line.contains("verdict=partial"), "{label}: {line}");
    assert!(
        line.contains("reason=process_started_mid_day"),
        "{label}: {line}"
    );
    assert!(line.contains("db_tick_rows=10i"), "{label}: {line}");
    let midnight = ist_nanos_of(day.and_hms_opt(0, 0, 0).expect("midnight"));
    assert!(
        has_ts_field(line, "trading_date_ist", midnight),
        "{label}: the row is for {day}: {line}"
    );
    // B3 review (2026-11-13): the row's `checked_at` follows the injected
    // clock too: no earlier than 15:45 IST on `day`, and no later than what
    // that clock read when the row was seen. (Not an exact instant: the
    // driver advances the paused clock in steps of up to half an hour, so a
    // sleeping task wakes up to one step after its deadline.) A
    // `run_one_check` that read the wall clock instead would stamp today's
    // real time and fail here.
    let check = midnight + CHECK_SECS * NANOS_PER_SEC;
    let checked_at = designated_ts(line).expect("a designated timestamp");
    assert!(
        (check..=clock_now_ist).contains(&checked_at),
        "{label}: checked_at {checked_at} is between 15:45 IST on {day} \
         ({check}) and the injected clock now ({clock_now_ist}), not the wall \
         clock: {line}"
    );
    // The count settles the verdict on the first read that answers, and the
    // task makes up to three reads 120 s apart. `drive_paused` waits for the
    // woken task before moving the paused clock, so one read is expected; a
    // second is allowed (a step on a loaded runner can still pass the read's
    // 30 s timeout before the fake server answers). A third would mean the
    // driver let the clock move under a live request.
    assert!(
        line.contains("count_reads=1i") || line.contains("count_reads=2i"),
        "{label}: {line}"
    );
}

/// The hand-written edges above equal the exported ones: a moved constant
/// fails here by name rather than turning the scenarios into the wrong test.
fn assert_edges_match_production() {
    assert_eq!(
        CHECK_SECS,
        i64::from(TICK_CONSERVATION_CHECK_SECS_OF_DAY_IST),
        "the daily check moved: re-derive the scenario times"
    );
    assert_eq!(SESSION_OPEN_SECS * NANOS_PER_SEC, MARKET_OPEN_IST_NANOS);
    assert_eq!(SESSION_CLOSE_SECS * NANOS_PER_SEC, MARKET_CLOSE_IST_NANOS);
    assert_eq!(RESTART_GAP_SECS * NANOS_PER_SEC, PROCESS_DOWN_MIN_GAP_NANOS);
}

#[tokio::test(start_paused = true)]
async fn test_tick_conservation_spawn_tick_conservation_writes_rows_the_clock_calls_for() {
    init_no_proxy();
    init_tracing();
    let _cwd_lock = lock_cwd().await;
    let cwd = CwdGuard::enter("tick-conservation-spawn");
    assert_edges_match_production();

    let tue = (2026, 11, 10);
    let tue_date = NaiveDate::from_ymd_opt(2026, 11, 10).expect("tue");
    let wed_date = NaiveDate::from_ymd_opt(2026, 11, 11).expect("wed");
    let mon_date = NaiveDate::from_ymd_opt(2026, 11, 16).expect("mon");
    assert_eq!(tue_date.format("%a").to_string(), "Tue");

    // ---- 1. In session: downtime row now, daily row at 15:45. ----
    {
        let start = ist(tue, (11, 0, 0));
        let previous = utc_nanos_of(start) - TWO_HOURS_NANOS; // 09:00 IST
        let s = Scenario::start(start, Some(previous), ist(tue, (10, 59, 59)));
        s.advance(Duration::from_secs(60)).await;
        let gap = s.gap();
        assert_eq!(gap.len(), 1, "in session: one downtime row: {gap:?}");
        let row = &gap[0];
        assert!(
            row.contains(&format!("reason={PROCESS_DOWN_REASON}")),
            "{row}"
        );
        assert!(row.contains("connection_index=-1i"), "{row}");
        // The gap starts at the open (the previous frame was before it).
        assert!(row.contains("down_secs=6300i"), "{row}");
        assert!(
            s.daily().is_empty(),
            "in session: no daily row before 15:45: {:?}",
            s.daily()
        );
        assert!(
            s.srv
                .exec_queries()
                .iter()
                .any(|q| q.starts_with("CREATE TABLE IF NOT EXISTS feed_gap_audit")),
            "in session: the downtime table DDL reached /exec"
        );
        // 11:00 -> 15:45 is 4 h 45 min; allow ten minutes for the reads.
        s.until_daily(Duration::from_secs(4 * 3_600 + 55 * 60))
            .await;
        assert_daily_row("in session", &s.daily(), tue_date, s.clock_now_ist());
        assert_eq!(s.gap().len(), 1, "the downtime row is written once");
        s.finish("in session").await;
    }

    // ---- 2. After the check time: nothing today, Wednesday's row tomorrow. ----
    {
        let start = ist(tue, (16, 0, 0));
        let previous = utc_nanos_of(start) - TWO_HOURS_NANOS;
        let s = Scenario::start(start, Some(previous), ist(tue, (15, 59, 59)));
        // To Wed 15:30: the whole rest of Tuesday plus Wednesday's morning.
        s.advance(Duration::from_secs(23 * 3_600 + 30 * 60)).await;
        assert!(
            s.gap().is_empty(),
            "after close: no downtime row: {:?}",
            s.gap()
        );
        assert!(
            s.daily().is_empty(),
            "after 15:45: no daily row for Tuesday, none for Wednesday yet: {:?}",
            s.daily()
        );
        s.until_daily(Duration::from_secs(3_600)).await;
        assert_daily_row("after the check", &s.daily(), wed_date, s.clock_now_ist());
        s.finish("after the check").await;
    }

    // ---- 3. Weekend: nothing Saturday or Sunday, Monday's row at 15:45. ----
    {
        let sat = ist((2026, 11, 14), (11, 0, 0));
        assert_eq!(sat.date().format("%a").to_string(), "Sat");
        let previous = utc_nanos_of(sat) - TWO_HOURS_NANOS;
        let s = Scenario::start(sat, Some(previous), ist((2026, 11, 14), (10, 59, 59)));
        // Sat 11:00 -> Mon 15:30 is 52 h 30 min.
        s.advance(Duration::from_secs(52 * 3_600 + 30 * 60)).await;
        assert!(
            s.gap().is_empty(),
            "weekend: no downtime row on a non-trading day: {:?}",
            s.gap()
        );
        assert!(
            s.daily().is_empty(),
            "weekend: no daily row on Saturday or Sunday: {:?}",
            s.daily()
        );
        s.until_daily(Duration::from_secs(3_600)).await;
        assert_daily_row("weekend", &s.daily(), mon_date, s.clock_now_ist());
        s.finish("weekend").await;
    }

    // ---- 4. Across IST midnight: the loop rolls to Wednesday. ----
    {
        let start = ist(tue, (23, 59, 58));
        let previous = utc_nanos_of(start) - TWO_HOURS_NANOS;
        let s = Scenario::start(start, Some(previous), ist(tue, (23, 59, 57)));
        // Past midnight (the loop sleeps to 00:00:05) and on to Wed 15:30.
        s.advance(Duration::from_secs(15 * 3_600 + 30 * 60)).await;
        assert!(
            s.gap().is_empty(),
            "midnight: no downtime row: {:?}",
            s.gap()
        );
        assert!(
            s.daily().is_empty(),
            "midnight: no row for Tuesday, none for Wednesday before 15:45: {:?}",
            s.daily()
        );
        s.until_daily(Duration::from_secs(3_600)).await;
        assert_daily_row("IST midnight", &s.daily(), wed_date, s.clock_now_ist());
        s.finish("IST midnight").await;
    }

    // ---- 5. The restart edge: 5 s after the open is a restart, 6 s is downtime. ----
    for (secs_after_open, expect_row) in [(RESTART_GAP_SECS, false), (RESTART_GAP_SECS + 1, true)] {
        let hms = u32::try_from(SESSION_OPEN_SECS + secs_after_open).expect("secs");
        let start = ist(tue, (hms / 3_600, hms % 3_600 / 60, hms % 60));
        // The previous frame was yesterday, so the gap starts at the open.
        let previous = utc_nanos_of(start) - 24 * 3_600 * NANOS_PER_SEC;
        let s = Scenario::start(start, Some(previous), start);
        s.advance(Duration::from_secs(60)).await;
        let gap = s.gap();
        if expect_row {
            assert_eq!(gap.len(), 1, "{secs_after_open} s after the open: {gap:?}");
            assert!(
                gap[0].contains(&format!("down_secs={secs_after_open}i")),
                "{}",
                gap[0]
            );
        } else {
            assert!(
                gap.is_empty(),
                "{secs_after_open} s after the open is a restart: {gap:?}"
            );
        }
        s.finish("restart edge").await;
    }

    // ---- 6. Muhurat Sunday: the Muhurat window, and a daily row that day. ----
    {
        let sun = (2026, 11, 8);
        let sun_date = NaiveDate::from_ymd_opt(2026, 11, 8).expect("sun");
        assert_eq!(sun_date.format("%a").to_string(), "Sun");
        let start = ist(sun, (15, 0, 0));
        let previous = utc_nanos_of(start) - TWO_HOURS_NANOS; // 13:00 IST
        let s = Scenario::start_on(
            session_calendar(),
            start,
            Some(previous),
            ist(sun, (14, 59, 59)),
        );
        s.advance(Duration::from_secs(60)).await;
        let gap = s.gap();
        assert_eq!(gap.len(), 1, "Muhurat: one downtime row: {gap:?}");
        // The gap starts at the 14:05 Muhurat open, never at 09:15.
        assert!(gap[0].contains("down_secs=3300i"), "{}", gap[0]);
        // 15:01 -> 15:45 is 44 min; allow ten minutes for the reads.
        s.until_daily(Duration::from_secs(55 * 60)).await;
        assert_daily_row("Muhurat Sunday", &s.daily(), sun_date, s.clock_now_ist());
        s.finish("Muhurat Sunday").await;
    }

    // ---- 7. Weekday holiday: nothing Monday, Tuesday's row at 15:45. ----
    {
        let mon = (2026, 11, 9);
        let start = ist(mon, (11, 0, 0));
        let previous = utc_nanos_of(start) - TWO_HOURS_NANOS;
        let s = Scenario::start_on(
            session_calendar(),
            start,
            Some(previous),
            ist(mon, (10, 59, 59)),
        );
        // Mon 11:00 -> Tue 15:30 is 28 h 30 min.
        s.advance(Duration::from_secs(28 * 3_600 + 30 * 60)).await;
        assert!(
            s.gap().is_empty(),
            "holiday: no downtime row: {:?}",
            s.gap()
        );
        assert!(
            s.daily().is_empty(),
            "holiday: no daily row on Monday: {:?}",
            s.daily()
        );
        s.until_daily(Duration::from_secs(3_600)).await;
        assert_daily_row("weekday holiday", &s.daily(), tue_date, s.clock_now_ist());
        s.finish("weekday holiday").await;
    }

    drop(cwd);
}
