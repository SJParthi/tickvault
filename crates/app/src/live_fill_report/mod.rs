//! The daily live PAPER fill calibration report (plan item LF-7, owner Quote
//! 57, 2026-11-12).
//!
//! Once per trading day after the close, this task reads the day's
//! `live_order_events`, re-prices every paper order from the stored 1 s
//! candles with the chain's own pricing
//! ([`crate::live_fill::fill_window::price_held_window`]: the LIMIT volume
//! cut, the worst-case model on the best quote, the LIMIT share cut and the
//! whole-lot floor), and writes five files to `data/live-fill-logs/<D>/`:
//!
//! | File | What |
//! |---|---|
//! | `report-<D>.json` | everything below, plus the status and the traffic light |
//! | `orders-<D>.csv` | one line per would-be order: outcome and verdict |
//! | `fill-seconds-<D>.csv` | shares per fill second after the decision (re-priced) |
//! | `latency-<D>.csv` | decision → built → sent → ack → first fill, queue wait, feed lag, API time |
//! | `rejects-<D>.csv` | reject and skip reasons with counts |
//!
//! The files are copied to `s3://tv-<env>-cold/live-fill-logs/<D>/` (never an
//! overwrite, never a delete), a one-line summary goes to
//! `live_fill_report_daily`, and the day's marker is written only when all
//! five copies are verified. A day the box could not finish (bucket down,
//! restart) is picked up at the next boot, up to three trading days back.
//!
//! **Why not the paper rows' own verdict:** in paper mode the chain fills at
//! exactly the worst-case price and writes `worse_than_model = not_worse`
//! every time, so a report reading only that column passes every day without
//! testing anything. The re-pricing compares the chain against the stored
//! data instead. A difference is a finding to read (a late tick the chain
//! never saw can widen a stored second), not proof of a defect.
//!
//! **Paper only.** Nothing here places, modifies or cancels an order, or
//! calls a broker. No Telegram page: both codes (LIVE-FILL-01/02) are
//! log-only (noise lock §2.22).
//!
//! # Schedule and cost
//!
//! Attempts at 15:47, 15:54 and 16:01 IST on trading days only, never inside
//! 09:00–15:40 IST, each bounded at [`REPORT_ATTEMPT_TIMEOUT_SECS`] (420 s)
//! with every step inside budgeted against it (readiness, day query,
//! re-pricing, file write, the summary row before the copy, the copy, the
//! row after it; const-asserted); the last ends by 16:08, before the 16:25
//! stop window. A process that starts after 16:01 with today unreported runs
//! one final attempt at once when a whole one fits outside the session and
//! the stop window. Leftover days run only AFTER today's attempts, and only
//! while a whole bounded run stays outside 09:00–15:40 and clear of the stop
//! window.
//! Cold, on its own task, never on the frame drain, never per tick: one day
//! query (a reply of at most 50,000 rows; QuestDB sorts the whole day's
//! `live_order_events` before the limit), at most 500 re-pricings of the
//! orders the report compares, each a reply of at most `max_fill_seconds + 3`
//! rows but a server scan of EVERY instrument's `candles_1s` rows in the
//! order's window (`security_id` is not indexed: O(instruments traded in the
//! window × window seconds) per order, Assumed 1e5 to 3e5 rows for a 62 s
//! window, not measured; the client timeout does not cancel the server
//! query), all under a wall budget of at most 120 s, one sort per latency
//! series (O(k log k)), then O(bytes) to write and copy. Flagged: not O(1)
//! (LF-7 review finding 8).

pub mod build;
pub mod recheck;
pub mod render;
pub mod rows;
pub mod upload;

#[cfg(test)]
mod build_tests;

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use chrono::NaiveDate;
use tracing::{error, info, warn};

use tickvault_common::config::QuestDbConfig;
use tickvault_common::constants::{IST_UTC_OFFSET_NANOS, TICK_PERSIST_END_SECS_OF_DAY_IST};
use tickvault_common::error_code::ErrorCode;
use tickvault_common::trading_calendar::TradingCalendar;
use tickvault_storage::live_fill_report_persistence::{
    LiveFillReportDailyRow, LiveFillReportDailyWriter, ensure_live_fill_report_daily_table,
};
use tickvault_storage::off_worker::off_worker;
use tickvault_storage::wal_suspension_watcher::{WalTableRow, fetch_wal_tables};

use build::{DailyReport, OUTSIDE_WORST_CASE_LOG_CAP, ReportContext, build_report};
use recheck::{
    OrderSeconds, RECHECK_MAX_ORDERS, RECHECK_QUERY_TIMEOUT_SECS, RECHECK_WALL_BUDGET_SECS,
    Recheck, RecheckKind, read_seconds,
};
use render::{render_files, report_file_names, summary_plain};
use rows::REPORT_QUERY_TIMEOUT_SECS;
use upload::{
    LIVE_FILL_LOGS_DIR, UPLOAD_SPACING_SECS, UPLOAD_TRIES, copy_started, day_dir, folder_complete,
    upload_day_with_retries, write_report_files,
};

use crate::shutdown_class::{
    SCHEDULED_STOP_WINDOW_END_SECS_OF_DAY_IST, SCHEDULED_STOP_WINDOW_START_SECS_OF_DAY_IST,
};

/// The daily marker task name.
pub const LIVE_FILL_REPORT_TASK: &str = "live_fill_report";

/// Attempt times, IST seconds of day: 15:47, 15:54, 16:01.
pub const REPORT_ATTEMPT_SECS: [u32; 3] = [56_820, 57_240, 57_660];

/// One attempt's whole bound (the outer timeout). Every step inside it has
/// its own bound and the steps are budgeted against it (LF-7 review finding
/// 4), so the outer timeout is a backstop, not the normal end of a slow run.
pub const REPORT_ATTEMPT_TIMEOUT_SECS: u64 = 420;

/// Kept between the steps' own deadline and the outer bound.
pub const ATTEMPT_GRACE_SECS: u64 = 10;

/// How long one attempt waits for the readiness barriers at most (less when
/// the steps after it need the time).
pub const REPORT_READINESS_WAIT_SECS: u64 = 120;

/// Past trading days a boot looks back for an unfinished report.
pub const REPORT_LOOKBACK_TRADING_DAYS: usize = 3;

/// Calendar days the look-back scans for those trading days.
pub const REPORT_LOOKBACK_CALENDAR_DAYS: i64 = 10;

/// Day folders older than the lookback, never copied, that one pass copies
/// at most (copy only, never a rebuild; LF-7 review round 3, finding 2). The
/// rest wait for a later pass or boot; nothing is ever dropped.
pub const OLD_DAY_COPIES_PER_PASS: usize = 3;

/// One `wal_tables()` probe's bound during the readiness wait.
pub const WAL_PROBE_TIMEOUT_SECS: u64 = 10;

/// Spacing of the readiness polls.
pub const READINESS_POLL_SECS: u64 = 10;

/// The cold-bucket client load's bound.
pub const BUCKET_LOAD_TIMEOUT_SECS: u64 = 30;

/// Tries of one summary-row write.
pub const DB_ROW_TRIES: u64 = 3;

/// Spacing of the summary row's write retries.
pub const DB_ROW_RETRY_SPACING_SECS: u64 = 5;

/// One row flush's bound (the writer's `request_timeout`).
pub const DB_ROW_FLUSH_BOUND_SECS: u64 = 5;

/// One ensure retry's bound before a row write.
pub const ENSURE_RETRY_TIMEOUT_SECS: u64 = 15;

/// One summary-row step: one ensure retry, the tries and the spacing between
/// them (no sleep after the last).
pub const DB_ROW_STEP_BOUND_SECS: u64 = ENSURE_RETRY_TIMEOUT_SECS
    + DB_ROW_TRIES * DB_ROW_FLUSH_BOUND_SECS
    + (DB_ROW_TRIES - 1) * DB_ROW_RETRY_SPACING_SECS;

/// Kept for the five local file writes.
pub const FILE_WRITE_RESERVE_SECS: u64 = 10;

/// The least the copy step is given: the bucket load and one pass.
pub const COPY_MIN_SECS: u64 = BUCKET_LOAD_TIMEOUT_SECS + 60;

/// What the steps after the re-pricing need: the file write, the row before
/// the copy, the copy, the row after it.
pub const TAIL_AFTER_RECHECK_SECS: u64 =
    FILE_WRITE_RESERVE_SECS + DB_ROW_STEP_BOUND_SECS + COPY_MIN_SECS + DB_ROW_STEP_BOUND_SECS;

/// What the steps after the readiness wait need, re-pricing aside (it takes
/// what is left, up to its own budget).
pub const TAIL_AFTER_READINESS_SECS: u64 = REPORT_QUERY_TIMEOUT_SECS + TAIL_AFTER_RECHECK_SECS;

/// Margin kept before the scheduled stop window.
const STOP_MARGIN_SECS: u64 = 60;

/// 09:00 IST, seconds of day.
const SESSION_OPEN_SECS_OF_DAY: u32 = 9 * 3_600;

const _: () = assert!(REPORT_ATTEMPT_SECS[0] > TICK_PERSIST_END_SECS_OF_DAY_IST);
const _: () = assert!(REPORT_ATTEMPT_SECS[0] < REPORT_ATTEMPT_SECS[1]);
const _: () = assert!(REPORT_ATTEMPT_SECS[1] < REPORT_ATTEMPT_SECS[2]);
// One attempt never runs into the next attempt's slot.
const _: () = assert!(
    REPORT_ATTEMPT_TIMEOUT_SECS <= (REPORT_ATTEMPT_SECS[1] - REPORT_ATTEMPT_SECS[0]) as u64
        && REPORT_ATTEMPT_TIMEOUT_SECS <= (REPORT_ATTEMPT_SECS[2] - REPORT_ATTEMPT_SECS[1]) as u64
);
const _: () = assert!(
    REPORT_ATTEMPT_SECS[2] as u64 + REPORT_ATTEMPT_TIMEOUT_SECS + STOP_MARGIN_SECS
        < SCHEDULED_STOP_WINDOW_START_SECS_OF_DAY_IST as u64
);
// Every fixed step's bound fits the attempt (LF-7 review finding 4): the
// readiness wait, the day query, the file write, both summary rows and the
// least copy. The re-pricing takes only what is left (`recheck_budget`).
const _: () = assert!(
    REPORT_READINESS_WAIT_SECS + TAIL_AFTER_READINESS_SECS + ATTEMPT_GRACE_SECS
        <= REPORT_ATTEMPT_TIMEOUT_SECS
);

const NANOS_PER_SEC: i64 = 1_000_000_000;
const NANOS_PER_DAY: i64 = 86_400 * NANOS_PER_SEC;

/// Runs per outcome.
pub const RUNS_COUNTER: &str = "tv_live_fill_report_runs_total";
/// Orders per verdict.
pub const ORDERS_COUNTER: &str = "tv_live_fill_report_orders_total";
/// Copy passes per outcome.
pub const UPLOAD_COUNTER: &str = "tv_live_fill_report_upload_total";
/// The last reported day as `YYYYMMDD`.
pub const LAST_DAY_GAUGE: &str = "tv_live_fill_report_last_day_ymd";

const RUN_OUTCOMES: [&str; 4] = ["complete", "partial", "no_orders", "failed"];
const VERDICTS: [&str; 10] = [
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
];

/// Registers every series at 0 so an absent series never reads as healthy.
/// Local `/metrics` only. Cold.
pub fn register_metrics() {
    for o in RUN_OUTCOMES {
        metrics::counter!(RUNS_COUNTER, "outcome" => o).increment(0);
    }
    for v in VERDICTS {
        metrics::counter!(ORDERS_COUNTER, "verdict" => v).increment(0);
    }
    for o in ["verified", "not_verified", "no_bucket", "timed_out"] {
        metrics::counter!(UPLOAD_COUNTER, "outcome" => o).increment(0);
    }
    metrics::gauge!(LAST_DAY_GAUGE).set(0.0);
}

/// The next attempt at or after `now_sod` (IST seconds of day) whose index is
/// above `done` (the last attempt already run, if any): `(index, seconds to
/// wait)`, or `None` when today's attempts are over. Pure, O(3).
#[must_use]
pub const fn attempt_plan(now_sod: u32, done: Option<usize>) -> Option<(usize, u32)> {
    let mut i = match done {
        Some(d) => d + 1,
        None => 0,
    };
    while i < REPORT_ATTEMPT_SECS.len() {
        let at = REPORT_ATTEMPT_SECS[i];
        if at >= now_sod {
            return Some((i, at - now_sod));
        }
        i += 1;
    }
    None
}

/// Whether an IST second of day is inside 09:00–15:40, when the report never
/// runs. Pure.
#[must_use]
pub const fn in_session(sod: u32) -> bool {
    sod >= SESSION_OPEN_SECS_OF_DAY && sod < TICK_PERSIST_END_SECS_OF_DAY_IST
}

/// Whether a whole attempt started at `sod` stays out of the session and
/// clear of the scheduled stop window: it ends by 09:00, or it starts after
/// the session and ends a margin before the stop window, or it starts after
/// the stop window. Pure, O(1).
#[must_use]
pub const fn attempt_fits_outside_session(sod: u32) -> bool {
    let end = sod as u64 + REPORT_ATTEMPT_TIMEOUT_SECS;
    end <= SESSION_OPEN_SECS_OF_DAY as u64
        || (sod >= TICK_PERSIST_END_SECS_OF_DAY_IST
            && end + STOP_MARGIN_SECS < SCHEDULED_STOP_WINDOW_START_SECS_OF_DAY_IST as u64)
        || sod >= SCHEDULED_STOP_WINDOW_END_SECS_OF_DAY_IST
}

/// Whether a leftover (catch-up) day may start at `sod`: only when the whole
/// bounded run stays outside 09:00–15:40 and clear of the stop window
/// (LF-7 review findings 3, 6, 9, 12). The loop runs leftovers only after
/// today's attempts, so they never take today's slots. Pure, O(1).
#[must_use]
pub const fn leftovers_allowed(sod: u32) -> bool {
    attempt_fits_outside_session(sod)
}

/// What today's loop does next at `now_sod`, `done` being the last attempt
/// run: `(index, seconds to wait, final try)`. The three planned attempts
/// first; a process that reaches the loop after the last one with none run
/// (a start after 16:01) runs ONE final attempt at once when it fits outside
/// the session and the stop window (LF-7 review finding 3). `None` when
/// today is over. Pure, O(3).
#[must_use]
pub const fn today_step(now_sod: u32, done: Option<usize>) -> Option<(usize, u32, bool)> {
    let last = REPORT_ATTEMPT_SECS.len() - 1;
    match attempt_plan(now_sod, done) {
        Some((i, wait)) => Some((i, wait, i == last)),
        None => {
            if done.is_none()
                && now_sod > REPORT_ATTEMPT_SECS[last]
                && attempt_fits_outside_session(now_sod)
            {
                Some((last, 0, true))
            } else {
                None
            }
        }
    }
}

/// Whether a catch-up of a day with no local state repairs its record
/// instead of rebuilding it (LF-9 finding 7): the bucket already holds some
/// of its report files, or its summary row says the copy was verified. A
/// rebuild would upsert a `catch_up_rebuild` row over the original and put a
/// second build beside the first in the bucket. Pure, O(1).
#[must_use]
pub const fn catch_up_repairs_instead(files_in_bucket: usize, row_uploaded: Option<bool>) -> bool {
    files_in_bucket > 0 || matches!(row_uploaded, Some(true))
}

/// Whether a catch-up asks the bucket before it builds or copies (LF-9
/// findings 7 and review): every catch-up whose copy never started, with a
/// complete local folder or none. A complete folder may be a build made
/// while the bucket could not be checked; it is copied only once the bucket
/// is checked. Pure, O(1).
#[must_use]
pub const fn catch_up_probes_bucket(catch_up: bool, copy_started: bool) -> bool {
    catch_up && !copy_started
}

/// Whether a catch-up whose bucket probe FAILED may build the day locally
/// (without the copy): only a day with no complete local folder whose summary
/// row read succeeded and found NO row (LF-9 review, findings 6 and 10). A
/// failed row read is not "never reported": the day may have been reported
/// green and copied, and a rebuild would upsert an Amber `s3_uploaded =
/// false` row over it. Pure, O(1).
#[must_use]
pub const fn probe_failure_may_build(
    folder_complete: bool,
    row_read: &Result<Option<bool>, String>,
) -> bool {
    !folder_complete && matches!(row_read, Ok(None))
}

/// Whether a catch-up that found the day already in the bucket may mark the
/// day done (LF-9 review): only when the summary row already says the copy
/// was verified. A row that says otherwise, or none, cannot be repaired here
/// (the bucket is never read back), so the day is left unmarked and reported
/// rather than marked done with a summary row that disagrees with the
/// bucket. Pure, O(1).
#[must_use]
pub const fn catch_up_repair_may_mark(row_uploaded: Option<bool>) -> bool {
    matches!(row_uploaded, Some(true))
}

/// Whether a run builds the day's files afresh (LF-7 review finding 5). Never
/// once a copy of the day has started (`copy_started`): a rebuild after a
/// partial or unrecorded copy would leave the bucket holding some files from
/// one build and some (or sidecars) from another, with the summary row
/// matching only the second. A catch-up with the folder complete also only
/// copies. Pure, O(1).
const fn rebuild_wanted(catch_up: bool, folder_complete: bool, copy_started: bool) -> bool {
    !copy_started && (!catch_up || !folder_complete)
}

/// The readiness wait an attempt with `remaining` left may take: at most
/// [`REPORT_READINESS_WAIT_SECS`], less when the later steps need the time.
/// Pure.
#[must_use]
pub fn readiness_budget(remaining: Duration) -> Duration {
    Duration::from_secs(REPORT_READINESS_WAIT_SECS)
        .min(remaining.saturating_sub(Duration::from_secs(TAIL_AFTER_READINESS_SECS)))
}

/// The re-pricing budget with `remaining` left: at most
/// [`RECHECK_WALL_BUDGET_SECS`], less the steps after it and one read past
/// the budget's end. Pure.
#[must_use]
pub fn recheck_budget(remaining: Duration) -> Duration {
    Duration::from_secs(RECHECK_WALL_BUDGET_SECS).min(remaining.saturating_sub(
        Duration::from_secs(TAIL_AFTER_RECHECK_SECS + RECHECK_QUERY_TIMEOUT_SECS),
    ))
}

/// The copy step's bound with `remaining` left: all of it but the summary
/// row written after it. Pure.
#[must_use]
pub fn copy_budget(remaining: Duration) -> Duration {
    remaining.saturating_sub(Duration::from_secs(DB_ROW_STEP_BOUND_SECS))
}

/// Past trading days (before `today`, at most
/// [`REPORT_LOOKBACK_TRADING_DAYS`] within
/// [`REPORT_LOOKBACK_CALENDAR_DAYS`]) with no marker, oldest first.
/// O(lookback).
#[must_use]
pub fn pending_days(
    today: NaiveDate,
    is_trading: impl Fn(NaiveDate) -> bool,
    marked: impl Fn(NaiveDate) -> bool,
) -> Vec<NaiveDate> {
    lookback_days(today, is_trading)
        .into_iter()
        .filter(|d| !marked(*d))
        .collect()
}

/// The past trading days the boot's lookback reaches (at most
/// [`REPORT_LOOKBACK_TRADING_DAYS`] within [`REPORT_LOOKBACK_CALENDAR_DAYS`]),
/// oldest first. A day folder older than these is found by
/// [`upload::unreported_day_folders`] instead (LF-7 review round 3, finding
/// 2). O(lookback).
#[must_use]
pub fn lookback_days(today: NaiveDate, is_trading: impl Fn(NaiveDate) -> bool) -> Vec<NaiveDate> {
    let mut out = Vec::new();
    for back in 1..=REPORT_LOOKBACK_CALENDAR_DAYS {
        let Some(d) = today.checked_sub_signed(chrono::Duration::days(back)) else {
            break;
        };
        if !is_trading(d) {
            continue;
        }
        out.push(d);
        if out.len() >= REPORT_LOOKBACK_TRADING_DAYS {
            break;
        }
    }
    out.reverse();
    out
}

/// Seconds the loop sleeps before it looks again TODAY, when a run is still
/// owed (today unreported, or a leftover or old day deferred) and the loop
/// ended after the session but before the end of the scheduled stop window:
/// it wakes at the window's end, where a whole attempt fits (LF-7 review
/// round 3, finding 3). `None` sleeps to the next IST day as before. Pure,
/// O(1).
#[must_use]
pub const fn wake_after_stop_window(now_sod: u32, run_owed: bool) -> Option<u64> {
    if run_owed
        && now_sod >= TICK_PERSIST_END_SECS_OF_DAY_IST
        && now_sod < SCHEDULED_STOP_WINDOW_END_SECS_OF_DAY_IST
    {
        Some((SCHEDULED_STOP_WINDOW_END_SECS_OF_DAY_IST - now_sod) as u64)
    } else {
        None
    }
}

/// Whether the WAL of `live_order_events` and `candles_1s` is applied (writer
/// caught up with the sequencer, not suspended). A table the database does not
/// list holds nothing to wait for. Pure, O(rows).
#[must_use]
pub fn wal_settled(rows: &[WalTableRow]) -> bool {
    ["live_order_events", "candles_1s"].iter().all(|t| {
        rows.iter().filter(|r| r.name == *t).all(|r| {
            !r.suspended && matches!((r.writer_txn, r.sequencer_txn), (Some(w), Some(s)) if w >= s)
        })
    })
}

/// Whether a spill folder holds no file (a missing folder holds none).
/// O(entries).
#[must_use]
pub fn dir_has_no_files(dir: &Path) -> bool {
    match std::fs::read_dir(dir) {
        Ok(entries) => entries
            .filter_map(Result::ok)
            .all(|e| e.file_type().map(|t| !t.is_file()).unwrap_or(true)),
        Err(e) => e.kind() == std::io::ErrorKind::NotFound,
    }
}

/// `YYYYMMDD` of a `YYYY-MM-DD` day, or 0. Pure.
#[must_use]
pub fn ymd_number(day: &str) -> u32 {
    day.replace('-', "").parse().unwrap_or(0)
}

/// The day's database summary row. Pure.
#[must_use]
pub fn daily_row(r: &DailyReport, at_ist_ns: i64, s3_uploaded: bool) -> LiveFillReportDailyRow {
    let c = &r.counts;
    let i = |v: u64| i64::try_from(v).unwrap_or(i64::MAX);
    let series = |name: &str| r.latency.iter().find(|l| l.series == name);
    LiveFillReportDailyRow {
        reported_at_ist_nanos: at_ist_ns,
        day_ist_nanos: build::rows_day_start(&r.day).unwrap_or(at_ist_ns),
        feed: "dhan",
        status: r.status,
        light: r.light,
        orders: i(c.orders),
        filled: i(c.filled),
        partial: i(c.partial_cancelled),
        unfilled: i(c.unfilled),
        skipped: i(c.skipped),
        rejected: i(c.rejected),
        worse_than_worst: i(c.worse_than_worst + c.stored_worse),
        better_than_worst: i(c.better_than_worst),
        recheck_skipped: i(c.recheck_skipped + c.recheck_unavailable + c.recheck_incomplete),
        p99_decision_to_sent_us: series("built_to_sent_us")
            .zip(series("decision_to_built_us"))
            .and_then(|(a, b)| Some(a.p99? + b.p99?))
            .unwrap_or(-1),
        p99_feed_lag_us: series("feed_lag_us").and_then(|l| l.p99).unwrap_or(-1),
        max_api_rtt_us: series("api_rtt_us").and_then(|l| l.max).unwrap_or(-1),
        s3_uploaded,
    }
}

/// The summary row of a day whose report was built by an earlier run, read
/// back from its `report-<D>.json` (a catch-up that only copies, LF-7 review
/// finding 7). `None` when the file is not a report (unknown status or
/// light, missing day or counts). Pure, O(bytes).
#[must_use]
pub fn daily_row_from_report_json(
    body: &str,
    at_ist_ns: i64,
    s3_uploaded: bool,
) -> Option<LiveFillReportDailyRow> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    let day = v.get("day")?.as_str()?;
    let day_ist_nanos = build::rows_day_start(day)?;
    let status = match v.get("status")?.as_str()? {
        "complete" => "complete",
        "partial" => "partial",
        "no_orders" => "no_orders",
        _ => return None,
    };
    let light = match v.get("light")?.as_str()? {
        "green" => "green",
        "amber" => "amber",
        "red" => "red",
        _ => return None,
    };
    let c = v.get("counts")?;
    let n = |k: &str| c.get(k).and_then(serde_json::Value::as_i64);
    let latency = v.get("latency").and_then(serde_json::Value::as_array);
    let stat = |name: &str, field: &str| {
        latency?
            .iter()
            .find(|l| l.get("series").and_then(serde_json::Value::as_str) == Some(name))?
            .get(field)?
            .as_i64()
    };
    Some(LiveFillReportDailyRow {
        reported_at_ist_nanos: at_ist_ns,
        day_ist_nanos,
        feed: "dhan",
        status,
        light,
        orders: n("orders")?,
        filled: n("filled")?,
        partial: n("partial_cancelled")?,
        unfilled: n("unfilled")?,
        skipped: n("skipped")?,
        rejected: n("rejected")?,
        worse_than_worst: n("worse_than_worst")?.saturating_add(n("stored_worse")?),
        better_than_worst: n("better_than_worst")?,
        recheck_skipped: n("recheck_skipped")?
            .saturating_add(n("recheck_unavailable")?)
            .saturating_add(n("recheck_incomplete")?),
        p99_decision_to_sent_us: stat("built_to_sent_us", "p99")
            .zip(stat("decision_to_built_us", "p99"))
            .map_or(-1, |(a, b)| a + b),
        p99_feed_lag_us: stat("feed_lag_us", "p99").unwrap_or(-1),
        max_api_rtt_us: stat("api_rtt_us", "max").unwrap_or(-1),
        s3_uploaded,
    })
}

/// Writes one summary row only when the table is ensured: a row that is not
/// ensured is re-ensured once (`ensure`), and when that fails too the row is
/// NOT written (LF-7 review finding 8): an ILP write must never be what
/// creates `live_fill_report_daily`, because QuestDB would make it itself
/// with the designated timestamp `timestamp` and no DEDUP key, and every
/// later `DEDUP ENABLE` would fail (the LH-01 class). Returns whether `write`
/// ran and succeeded. O(1).
///
/// LF-9 review (second pass): since LF-9 finding 6 the day's marker waits for
/// this row, so a table that can never be ensured leaves every day
/// unmarked: each boot copies the lookback days again and today's loop runs
/// all its attempts (each copy is idempotent: verified files are reused, one
/// HeadObject each), and each attempt is reported here. The files alone are
/// no longer the record; the row is.
pub async fn gated_row_write<E, EF, W, WF>(
    ensured: &AtomicBool,
    ensure: E,
    write: W,
    day: &str,
    attempt: usize,
) -> bool
where
    E: FnOnce() -> EF,
    EF: std::future::Future<Output = bool>,
    W: FnOnce() -> WF,
    WF: std::future::Future<Output = bool>,
{
    if !ensured.load(Ordering::Acquire) && ensure().await {
        ensured.store(true, Ordering::Release);
    }
    if !ensured.load(Ordering::Acquire) {
        report_failed(
            "db_row_not_ensured",
            day,
            attempt,
            "the table could not be created or checked; the summary row is not written, \
             so the day stays unmarked and is copied again (idempotent) at the next \
             attempt and boot; its files stay on disk",
        );
        return false;
    }
    write().await
}

fn now_ist_ns() -> i64 {
    chrono::Utc::now()
        .timestamp_nanos_opt()
        .unwrap_or(0)
        .saturating_add(IST_UTC_OFFSET_NANOS)
}

fn ist_date_of(ist_ns: i64) -> Option<NaiveDate> {
    chrono::DateTime::from_timestamp(ist_ns.div_euclid(NANOS_PER_SEC), 0).map(|d| d.date_naive())
}

fn sod_of(ist_ns: i64) -> u32 {
    u32::try_from(ist_ns.rem_euclid(NANOS_PER_DAY) / NANOS_PER_SEC).unwrap_or(0)
}

fn report_failed(stage: &'static str, day: &str, attempt: usize, detail: &str) {
    error!(
        code = ErrorCode::LiveFill01ReportFailed.code_str(),
        stage,
        day,
        attempt,
        detail,
        "LIVE-FILL-01: the daily live paper fill report could not finish a step; its \
         local files (if written) are kept and the copy is retried"
    );
}

/// Everything one run needs.
struct ReportRun<'a> {
    questdb: &'a QuestDbConfig,
    bucket: &'a str,
    process_start_ist_ns: i64,
    /// `live_fill_report_daily` was ensured (CREATE and DEDUP accepted).
    ensured: AtomicBool,
}

/// Time left before `deadline`.
fn left(deadline: tokio::time::Instant) -> Duration {
    deadline.saturating_duration_since(tokio::time::Instant::now())
}

impl ReportRun<'_> {
    fn exec_url(&self) -> String {
        format!(
            "http://{}:{}/exec",
            self.questdb.host, self.questdb.http_port
        )
    }

    /// Waits for the barriers: today's candles sealed and applied, both
    /// tables' WAL applied, the event spill folder empty. Bounded by
    /// `deadline`.
    async fn wait_settled(&self, day: NaiveDate, deadline: tokio::time::Instant) -> bool {
        let url = self.exec_url();
        let spill = tickvault_storage::audit_spill::AuditSpillTable::LiveOrderEvents.default_dir();
        loop {
            let day_start = build::rows_day_start(&day.format("%Y-%m-%d").to_string()).unwrap_or(0);
            let r = crate::dhan_live_crossverify_boot::live_side_readiness(
                &url, day, day_start, deadline,
            )
            .await;
            let seals = crate::expiry_scope_check::readiness_reason(r).is_none();
            let wal = match tickvault_storage::http_client::shared_probe_client() {
                Ok(client) => matches!(
                    tokio::time::timeout(Duration::from_secs(WAL_PROBE_TIMEOUT_SECS), fetch_wal_tables(client, &url))
                        .await,
                    Ok(Ok(rows)) if wal_settled(&rows)
                ),
                Err(_) => false,
            };
            let spill_empty = off_worker(|| dir_has_no_files(&spill));
            if seals && wal && spill_empty {
                return true;
            }
            if tokio::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_secs(READINESS_POLL_SECS).min(left(deadline))).await;
        }
    }

    /// Re-prices the Decision row of every order the report will compare
    /// ([`build::priced_signal_ids`]), at most [`RECHECK_MAX_ORDERS`] of them
    /// (LF-7 review finding 3: unplaced refusals, skips and runtime
    /// rejections are never compared and never use up the cap or the budget).
    /// `catch_up` skips the reads: the stored seconds of a past day are
    /// usually archived and gone. No read starts at or after `deadline`.
    ///
    /// LF-9 finding 3: the orders the chain held on one instrument share each
    /// second's participation cap, earlier orders first, so the re-pricing
    /// walks every HELD order of an instrument in signal order
    /// ([`build::held_share_known`]) with one map of the volume taken per
    /// second. A held order that is not compared but whose window overlaps a
    /// later held order of the same instrument is read too (its share is
    /// needed; at most [`RECHECK_MAX_ORDERS`] such reads). An order whose
    /// window overlaps one whose share is unknown (finished unpriced by a
    /// taint or a late price, or not read) reads
    /// `recheck_unavailable:shared_volume_unknown`, and so does every later
    /// overlapping one. Cold, once a day: O(rows log rows) plus one bounded
    /// read per order read.
    async fn recheck_all(
        &self,
        rows: &[rows::EventRow],
        catch_up: bool,
        day: &str,
        deadline: tokio::time::Instant,
    ) -> Vec<(i64, Recheck)> {
        let url = self.exec_url();
        let mut read_failures = 0u64;
        let items = off_worker(|| {
            let targets = build::priced_signal_ids(rows);
            let held = build::held_share_known(rows);
            recheck::walk_items(rows, &targets, &held)
        });
        // Phase 1: the reads (bounded, in walk order).
        let mut got: std::collections::BTreeMap<i64, OrderSeconds> =
            std::collections::BTreeMap::new();
        let mut targeted = 0usize;
        let mut helper_reads = 0usize;
        for item in &items {
            if !item.wants_seconds() && !item.is_target {
                continue;
            }
            let sid = item.row.signal_id;
            if item.is_target {
                targeted += 1;
                let not_run = if targeted > RECHECK_MAX_ORDERS {
                    Some(RecheckKind::Skipped("order_cap"))
                } else if tokio::time::Instant::now() >= deadline {
                    Some(RecheckKind::Skipped("wall_budget"))
                } else if catch_up {
                    Some(RecheckKind::Unavailable("catch_up_rebuild"))
                } else {
                    None
                };
                if let Some(kind) = not_run {
                    got.insert(sid, OrderSeconds::NotRun(Recheck::not_run(kind)));
                    continue;
                }
            } else {
                // A held order not compared: read only for its share.
                if catch_up
                    || helper_reads >= RECHECK_MAX_ORDERS
                    || tokio::time::Instant::now() >= deadline
                {
                    continue;
                }
                helper_reads += 1;
            }
            let Some(inp) = item.inputs.as_ref() else {
                continue;
            };
            let read = match read_seconds(&url, inp).await {
                Ok(all) => OrderSeconds::Seconds(all),
                Err(_) => {
                    read_failures += 1;
                    OrderSeconds::ReadFailed
                }
            };
            got.insert(sid, read);
        }
        // Phase 2: the shared walk (pure, off the runtime worker).
        let out = off_worker(|| recheck::recheck_walk(&items, &mut got));
        if read_failures > 0 {
            report_failed(
                "recheck",
                day,
                0,
                &format!("{read_failures} order second reads failed"),
            );
        }
        out
    }

    /// One report of `day`: build (unless a copy of the day already started,
    /// or the folder is complete and this is a catch-up, or the catch-up of a
    /// day with no local state finds it already reported and copied, LF-9
    /// finding 7), write, the summary row (`s3_uploaded = false`), copy, the
    /// summary row again after a verified copy, then the marker only once
    /// that row is written (LF-9 finding 6). Every step
    /// is budgeted against `deadline` (LF-7 review findings 4, 7, 10), so the
    /// row and the counters are never left behind by a slow copy. `final_try`
    /// builds even when unsettled. Returns `true` when the day is done
    /// (marked or the last attempt ran).
    async fn run_day(
        &self,
        day: NaiveDate,
        attempt: usize,
        final_try: bool,
        catch_up: bool,
        deadline: tokio::time::Instant,
    ) -> bool {
        let day_s = day.format("%Y-%m-%d").to_string();
        let dir = day_dir(Path::new(LIVE_FILL_LOGS_DIR), &day_s);
        let mut report: Option<DailyReport> = None;
        let (complete, started) =
            off_worker(|| (folder_complete(&dir, &day_s), copy_started(&dir)));
        if started && !complete {
            report_failed(
                "rebuild_after_partial_copy",
                &day_s,
                attempt,
                "a copy of this day already started but a file is missing; not rebuilt, \
                 so the bucket never holds two builds",
            );
        }
        // LF-9 finding 7: a catch-up of a day with no local state (a box
        // rebuild or a wiped data folder) first asks the bucket (HeadObject
        // only) and the summary table whether the day was already reported
        // and copied; if so it repairs the local record (the marker) and
        // never builds the day a second time. LF-9 review: a complete local
        // folder not yet copied is asked about too (it may be a build made
        // while the bucket could not be checked), and a probe that fails
        // still builds a never-reported day locally, without the copy.
        let mut copy_blocked = false;
        if catch_up_probes_bucket(catch_up, started) {
            match self.already_reported_and_copied(&day_s).await {
                Err(e) => {
                    let row = rows::read_row_uploaded(&self.exec_url(), &day_s).await;
                    if let Err(re) = &row {
                        // LF-9 review, findings 6 and 10: neither the bucket
                        // nor the summary row could be read, so whether the
                        // day was already reported is unknown; it is never
                        // rebuilt over a row that may say green.
                        report_failed(
                            "summary_row_read",
                            &day_s,
                            attempt,
                            &format!(
                                "the bucket could not be checked ({e}) and the summary row \
                                 could not be read ({re}); the day is neither rebuilt nor \
                                 copied now"
                            ),
                        );
                        return false;
                    }
                    if !probe_failure_may_build(complete, &row) {
                        report_failed(
                            "bucket_probe",
                            &day_s,
                            attempt,
                            &format!(
                                "the bucket could not be checked, so the day is neither \
                                 rebuilt nor copied now: {e}"
                            ),
                        );
                        return false;
                    }
                    report_failed(
                        "bucket_probe",
                        &day_s,
                        attempt,
                        &format!(
                            "the bucket could not be checked; the day is built locally and \
                             copied only once the bucket is checked: {e}"
                        ),
                    );
                    copy_blocked = true;
                }
                Ok(Some((files, row))) => {
                    if files < report_file_names(&day_s).len() {
                        report_failed(
                            "copied_day_without_local_files",
                            &day_s,
                            attempt,
                            &format!(
                                "the bucket holds {files} of the day's report files and no local \
                                 copy exists; not rebuilt, so the bucket never holds two builds"
                            ),
                        );
                    }
                    if !catch_up_repair_may_mark(row) {
                        report_failed(
                            "copied_day_row_not_verified",
                            &day_s,
                            attempt,
                            "the bucket holds the day's report files but its summary row does \
                             not say the copy was verified; the day is left unmarked",
                        );
                        return false;
                    }
                    let marked = off_worker(|| {
                        crate::daily_task_marker::try_write_daily_marker(LIVE_FILL_REPORT_TASK, day)
                    });
                    if let Err(e) = marked {
                        report_failed("marker", &day_s, attempt, &e.to_string());
                        return false;
                    }
                    info!(day = %day_s, files, "live fill report: the day was already reported and copied; local record repaired, not rebuilt");
                    return true;
                }
                Ok(None) => {}
            }
        }
        if rebuild_wanted(catch_up, complete, started) {
            // A past day's rows were applied long before; its report reads
            // Amber (`catch_up_rebuild`) anyway, and today's WAL and spill say
            // nothing about it (LF-7 review finding 9).
            let settled = if catch_up {
                true
            } else {
                let wait = readiness_budget(left(deadline));
                self.wait_settled(day, tokio::time::Instant::now() + wait)
                    .await
            };
            if !settled && !final_try {
                info!(day = %day_s, attempt, "live fill report: data not settled yet; retried at the next attempt");
                return false;
            }
            let read = tokio::time::timeout(
                Duration::from_secs(REPORT_QUERY_TIMEOUT_SECS),
                rows::read_day(&self.exec_url(), day),
            )
            .await
            .unwrap_or_else(|_| Err("timeout".to_owned()));
            let (rows, truncated) = match read {
                Ok(v) => v,
                Err(e) => {
                    report_failed("query", &day_s, attempt, &e);
                    metrics::counter!(RUNS_COUNTER, "outcome" => "failed").increment(1);
                    return final_try;
                }
            };
            if truncated {
                report_failed(
                    "truncated",
                    &day_s,
                    attempt,
                    "the day held more rows than the read takes",
                );
            }
            let recheck_deadline = tokio::time::Instant::now() + recheck_budget(left(deadline));
            let rechecks = self
                .recheck_all(&rows, catch_up, &day_s, recheck_deadline)
                .await;
            let ctx = ReportContext {
                boot: crate::live_fill::last_boot_outcome(),
                process_started_ist_ns: self.process_start_ist_ns,
                settled,
                truncated,
                catch_up,
            };
            // The sort and grouping run off the runtime worker (finding 14).
            let built = off_worker(|| build_report(&day_s, &rows, &rechecks, ctx));
            drop(rows);
            if let Err(e) = off_worker(|| write_report_files(&dir, &render_files(&built))) {
                report_failed("write", &day_s, attempt, &e.to_string());
                metrics::counter!(RUNS_COUNTER, "outcome" => "failed").increment(1);
                return final_try;
            }
            log_findings(&built);
            metrics::counter!(RUNS_COUNTER, "outcome" => built.status).increment(1);
            metrics::gauge!(LAST_DAY_GAUGE).set(f64::from(ymd_number(&built.day)));
            for o in &built.orders {
                let v = if VERDICTS.contains(&o.verdict) {
                    o.verdict
                } else {
                    "other"
                };
                metrics::counter!(ORDERS_COUNTER, "verdict" => v).increment(1);
            }
            // The row before the copy: a copy cut off by the bound never
            // leaves the day without a row.
            self.write_db_row(daily_row(&built, now_ist_ns(), false), &day_s, attempt)
                .await;
            report = Some(built);
        }
        if copy_blocked {
            // Built (or not) locally; the copy waits for a bucket check, so
            // the day stays unmarked and a later catch-up or the old-day pass
            // copies it once the bucket answers.
            if let Some(r) = &report {
                info!(uploaded = false, "{}", summary_plain(r));
            }
            return false;
        }
        let uploaded = match tokio::time::timeout(
            copy_budget(left(deadline)),
            self.copy_verified(&day_s, &dir, attempt),
        )
        .await
        {
            Ok(v) => v,
            Err(_) => {
                metrics::counter!(UPLOAD_COUNTER, "outcome" => "timed_out").increment(1);
                report_failed(
                    "copy_timeout",
                    &day_s,
                    attempt,
                    "the copy ran past what the attempt had left; files kept, retried",
                );
                false
            }
        };
        if uploaded {
            let row = match &report {
                Some(r) => Some(daily_row(r, now_ist_ns(), true)),
                None => {
                    let path = dir.join(format!("report-{day_s}.json"));
                    off_worker(|| std::fs::read_to_string(&path))
                        .ok()
                        .and_then(|body| daily_row_from_report_json(&body, now_ist_ns(), true))
                }
            };
            // LF-9 finding 6: copy, then the row, then the marker. The day is
            // marked done only once its summary row says the copy is
            // verified; a row that cannot be written leaves the day unmarked,
            // so a later attempt or the next boot copies again (idempotent)
            // and writes the row before marking.
            let row_written = match row {
                Some(row) => self.write_db_row(row, &day_s, attempt).await,
                None => {
                    report_failed(
                        "db_row_from_files",
                        &day_s,
                        attempt,
                        "the copied report file could not be read back for its summary row",
                    );
                    false
                }
            };
            // LF-9 review (second pass): the row's own local record, beside
            // the five copy records (which are written as each file is
            // verified, before the row). The old-day scan treats a day as
            // finished only with it, so a copied day whose row failed is
            // revisited and its row rewritten from its report file.
            let row_written = row_written && self.record_row_written(&dir, &day_s, attempt);
            let marked = row_written && self.write_marker(day, &day_s, attempt);
            if let Some(r) = &report {
                info!(uploaded, marked, "{}", summary_plain(r));
            }
            return marked || final_try;
        }
        if let Some(r) = &report {
            info!(uploaded, "{}", summary_plain(r));
        }
        final_try
    }

    /// Whether day `day_s` was already reported and copied (LF-9 finding 7):
    /// `Some((files in the bucket, the summary row's copy flag))` when the
    /// bucket holds any of its report files or the summary row says the copy
    /// was verified, `None` when neither. HeadObject and one bounded summary read only; never a write.
    ///
    /// # Errors
    /// No bucket, or a bucket probe that failed.
    async fn already_reported_and_copied(
        &self,
        day_s: &str,
    ) -> Result<Option<(usize, Option<bool>)>, String> {
        let store = self
            .load_store()
            .await
            .ok_or_else(|| "no cold bucket".to_owned())?;
        let files = tokio::time::timeout(
            Duration::from_secs(BUCKET_LOAD_TIMEOUT_SECS),
            upload::day_files_in_bucket(&store, day_s),
        )
        .await
        .map_err(|_| "bucket probe timed out".to_owned())??;
        // A failed row read (LF-9 review, findings 6 and 10): with files in
        // the bucket the day is reported with no verified row (never
        // marked); with none, whether it was reported is unknown, so the
        // caller's failure arm handles it (it reads the row again and never
        // builds over a row it cannot read).
        let row = match rows::read_row_uploaded(&self.exec_url(), day_s).await {
            Ok(row) => row,
            Err(e) if files == 0 => return Err(e),
            Err(_) => None,
        };
        Ok(catch_up_repairs_instead(files, row).then_some((files, row)))
    }

    /// Loads the cold bucket, bounded. Cold.
    async fn load_store(&self) -> Option<tickvault_storage::s3_cold::S3Cold> {
        tokio::time::timeout(
            Duration::from_secs(BUCKET_LOAD_TIMEOUT_SECS),
            tickvault_storage::s3_cold::S3Cold::load(self.bucket),
        )
        .await
        .ok()
        .flatten()
    }

    /// Writes the day's row record (`upload::write_row_written`); `false`
    /// (reported) when it fails, which leaves the day unmarked. Cold.
    fn record_row_written(&self, dir: &Path, day_s: &str, attempt: usize) -> bool {
        match off_worker(|| upload::write_row_written(dir, day_s)) {
            Ok(()) => true,
            Err(e) => {
                report_failed("row_record", day_s, attempt, &e.to_string());
                false
            }
        }
    }

    /// Writes the day's marker; `false` (reported) when it fails. Cold.
    fn write_marker(&self, day: NaiveDate, day_s: &str, attempt: usize) -> bool {
        match off_worker(|| {
            crate::daily_task_marker::try_write_daily_marker(LIVE_FILL_REPORT_TASK, day)
        }) {
            Ok(_) => true,
            Err(e) => {
                report_failed("marker", day_s, attempt, &e.to_string());
                false
            }
        }
    }

    /// Day folders older than the lookback that were never copied (LF-7
    /// review round 3, finding 2), or copied with their summary row never
    /// written: complete ones are copied again (a copy-only catch-up, never a
    /// rebuild), each only while a whole run stays outside the session and
    /// the stop window; each incomplete one is reported once per process as
    /// LIVE-FILL-01 `stage="outside_lookback_incomplete"` naming the folder.
    /// A folder that cannot be listed is reported as `stage="old_day_scan"`.
    /// See [`Self::walk_old_days`] for the order and the per-pass cap.
    /// Returns whether a copy is still owed this pass (deferred for room or
    /// by the cap). Cold, once per pass, O(folders) through `off_worker`.
    async fn copy_old_days(
        &self,
        today: NaiveDate,
        lookback: &[NaiveDate],
        reported_incomplete: &mut BTreeSet<NaiveDate>,
        unfinished: &mut BTreeSet<NaiveDate>,
    ) -> bool {
        let root = Path::new(LIVE_FILL_LOGS_DIR);
        let found = match off_worker(|| {
            upload::unreported_day_folders(root, today, |d| lookback.contains(&d))
        }) {
            Ok(v) => v,
            Err(e) => {
                report_failed(
                    "old_day_scan",
                    &today.format("%Y-%m-%d").to_string(),
                    0,
                    &e.to_string(),
                );
                return false;
            }
        };
        Self::walk_old_days(
            found,
            reported_incomplete,
            unfinished,
            || leftovers_allowed(sod_of(now_ist_ns())),
            |day, was_copied| async move {
                let _done = bounded_run(self, day, 0, true, true).await;
                let day_s = day.format("%Y-%m-%d").to_string();
                let dir = root.join(&day_s);
                off_worker(|| {
                    let now_copied = upload::folder_copied(&dir, &day_s);
                    now_copied && (!was_copied || upload::row_written(&dir))
                })
            },
        )
        .await
    }

    /// One pass over the old day folders `found` (never-copied ones first,
    /// then copied ones missing only their row, oldest first within each).
    /// `run(day, was_copied)` copies one and says whether it made PROGRESS
    /// (finished, or copied when it was not).
    ///
    /// LF-9 hardening: only a run that made progress uses one of the pass's
    /// [`OLD_DAY_COPIES_PER_PASS`] slots, and a folder run once by this
    /// process with no progress (`unfinished`) is not run again by it: its
    /// own run already reported why (its row cannot be written, or the
    /// bucket holds its files with no verified row). Before, such folders
    /// took every slot, oldest first, on every pass forever, and a newer
    /// folder that was never copied waited behind them, its report on the
    /// local disk only. A pass therefore runs at most one bounded run per
    /// folder, each started only while `allowed()` (a whole run fits outside
    /// the session and the stop window). Returns whether a copy is still
    /// owed this pass. Cold, O(folders).
    async fn walk_old_days<R, F>(
        found: Vec<upload::UnreportedDay>,
        reported_incomplete: &mut BTreeSet<NaiveDate>,
        unfinished: &mut BTreeSet<NaiveDate>,
        mut allowed: impl FnMut() -> bool,
        mut run: R,
    ) -> bool
    where
        R: FnMut(NaiveDate, bool) -> F,
        F: std::future::Future<Output = bool>,
    {
        let mut progressed = 0;
        for old in found {
            let day_s = old.day.format("%Y-%m-%d").to_string();
            if !old.complete {
                if reported_incomplete.insert(old.day) {
                    report_failed(
                        "outside_lookback_incomplete",
                        &day_s,
                        0,
                        "a day folder older than the lookback was never copied and is \
                         missing a file; it is not rebuilt (copy only); its files stay \
                         in data/live-fill-logs/",
                    );
                }
                continue;
            }
            if unfinished.contains(&old.day) {
                continue;
            }
            if progressed >= OLD_DAY_COPIES_PER_PASS || !allowed() {
                info!(day = %day_s, "live fill report: old uncopied day deferred to a later pass");
                return true;
            }
            warn!(day = %day_s, copied = old.copied, "live fill report: copying a day older than the lookback that is not finished");
            if run(old.day, old.copied).await {
                progressed += 1;
            } else {
                unfinished.insert(old.day);
                info!(day = %day_s, "live fill report: old day made no progress; not retried by this process");
            }
        }
        false
    }

    /// Copies the day's files; whether all five are verified in the bucket.
    /// The marker is written by the caller only after the summary row (LF-9
    /// finding 6).
    async fn copy_verified(&self, day_s: &str, dir: &Path, attempt: usize) -> bool {
        let Some(store) = self.load_store().await else {
            metrics::counter!(UPLOAD_COUNTER, "outcome" => "no_bucket").increment(1);
            report_failed(
                "no_bucket",
                day_s,
                attempt,
                "no cold bucket resolved in time",
            );
            return false;
        };
        let (summary, tries) = upload_day_with_retries(
            &store,
            dir,
            day_s,
            UPLOAD_TRIES,
            Duration::from_secs(UPLOAD_SPACING_SECS),
        )
        .await;
        if !summary.all_verified() {
            metrics::counter!(UPLOAD_COUNTER, "outcome" => "not_verified").increment(1);
            // A verified copy whose local record could not be written is
            // reported here only (LF-7 review findings 4 and 6), never on the
            // raw-frame uploader's paged counter.
            let marker_failed = summary
                .files
                .iter()
                .any(|(_, o)| o.starts_with("marker_failed:"));
            report_failed(
                if marker_failed { "marker" } else { "upload" },
                day_s,
                attempt,
                &format!("{tries} tries: {:?}", summary.files),
            );
            return false;
        }
        metrics::counter!(UPLOAD_COUNTER, "outcome" => "verified").increment(1);
        true
    }

    /// One summary row, gated on the table being ensured, at most
    /// [`DB_ROW_TRIES`] tries [`DB_ROW_RETRY_SPACING_SECS`] apart (no sleep
    /// after the last). The designated timestamp is the day, so this row
    /// upserts any earlier row of the same day.
    async fn write_db_row(&self, row: LiveFillReportDailyRow, day: &str, attempt: usize) -> bool {
        let questdb = self.questdb;
        let ensure = || async move {
            tokio::time::timeout(
                Duration::from_secs(ENSURE_RETRY_TIMEOUT_SECS),
                ensure_live_fill_report_daily_table(questdb),
            )
            .await
            .unwrap_or(false)
        };
        let write = || async move {
            let mut last = String::new();
            for t in 0..DB_ROW_TRIES {
                if t > 0 {
                    tokio::time::sleep(Duration::from_secs(DB_ROW_RETRY_SPACING_SECS)).await;
                }
                let mut writer = LiveFillReportDailyWriter::new(questdb);
                let written = writer
                    .append_row(&row)
                    .and_then(|()| crate::order_observability::blocking_flush(|| writer.flush()));
                match written {
                    Ok(()) => return true,
                    Err(e) => last = e.to_string(),
                }
            }
            report_failed("db_row", day, attempt, &last);
            false
        };
        gated_row_write(&self.ensured, ensure, write, day, attempt).await
    }
}

/// One LIVE-FILL-02 line per order outside the re-priced worst case, at most
/// [`OUTSIDE_WORST_CASE_LOG_CAP`], then one total line.
fn log_findings(r: &DailyReport) {
    let outside = r.outside_worst_case();
    for o in outside.iter().take(OUTSIDE_WORST_CASE_LOG_CAP) {
        error!(
            code = ErrorCode::LiveFill02FillOutsideWorstCase.code_str(),
            day = %r.day,
            signal_id = o.signal_id,
            security_id = o.security_id,
            segment = %o.segment,
            symbol = %o.symbol,
            side = %o.side,
            qty = o.qty,
            fill_qty = o.fill_qty,
            avg_paise = ?o.avg_paise,
            recheck_qty = ?o.recheck_qty,
            recheck_avg_paise = ?o.recheck_avg_paise,
            gap_ticks = ?o.gap_ticks,
            verdict = o.verdict,
            "LIVE-FILL-02: a paper fill is outside the worst case re-priced from the stored \
             1 s candles (paper only; read the day's report)"
        );
    }
    if outside.len() > OUTSIDE_WORST_CASE_LOG_CAP {
        error!(
            code = ErrorCode::LiveFill02FillOutsideWorstCase.code_str(),
            day = %r.day,
            total = outside.len(),
            logged = OUTSIDE_WORST_CASE_LOG_CAP,
            "LIVE-FILL-02: more paper fills outside the re-priced worst case than logged; \
             every one is in the day's orders file"
        );
    }
    if r.light != "green" && outside.is_empty() {
        warn!(day = %r.day, light = r.light, status = r.status, "live fill report: not green");
    }
}

/// Starts the daily report when `[live_fill] enabled` (the paper chain is
/// on), else does nothing. Called once from `main`. Cold.
pub fn start(
    config: &tickvault_common::config::ApplicationConfig,
    calendar: &Arc<TradingCalendar>,
    process_start_ist_ns: i64,
) -> Option<tokio::task::JoinHandle<()>> {
    config.live_fill.enabled.then(|| {
        spawn_live_fill_report(
            config.questdb.clone(),
            config.partition_retention.archive_bucket.clone(),
            Arc::clone(calendar),
            process_start_ist_ns,
        )
    })
}

/// One run of `day` under the attempt bound (the backstop; the steps budget
/// themselves against the same deadline less [`ATTEMPT_GRACE_SECS`]).
async fn bounded_run(
    run: &ReportRun<'_>,
    day: NaiveDate,
    attempt: usize,
    final_try: bool,
    catch_up: bool,
) -> Option<bool> {
    let started = tokio::time::Instant::now();
    let deadline = started + Duration::from_secs(REPORT_ATTEMPT_TIMEOUT_SECS - ATTEMPT_GRACE_SECS);
    let outcome = tokio::time::timeout(
        Duration::from_secs(REPORT_ATTEMPT_TIMEOUT_SECS),
        run.run_day(day, attempt, final_try, catch_up, deadline),
    )
    .await;
    if outcome.is_err() {
        report_failed(
            "attempt_timeout",
            &day.format("%Y-%m-%d").to_string(),
            attempt,
            "the attempt ran past its bound",
        );
    }
    outcome.ok()
}

/// Spawns the daily report on its own task (never the frame drain). Off with
/// `[live_fill] enabled = false` (the caller spawns it only when enabled).
/// Today's attempts run first; leftover days run after them, only while a
/// whole bounded run stays outside 09:00–15:40 and clear of the stop window.
// TEST-EXEMPT: tokio::spawn wrapper over the unit-tested parts (attempt_plan, today_step, leftovers_allowed, attempt_fits_outside_session, readiness_budget, recheck_budget, copy_budget, gated_row_write, in_session, pending_days, lookback_days, wake_after_stop_window, unreported_day_folders, folder_copied, wal_settled, dir_has_no_files, daily_row, daily_row_from_report_json, build_report, render_files, upload_day_with_retries); spawn site pinned by test_main_start_spawn_live_fill_report_only_under_live_fill_enabled.
pub fn spawn_live_fill_report(
    questdb: QuestDbConfig,
    bucket: String,
    calendar: Arc<TradingCalendar>,
    process_start_ist_ns: i64,
) -> tokio::task::JoinHandle<()> {
    register_metrics();
    tokio::spawn(async move {
        let ensured = ensure_live_fill_report_daily_table(&questdb).await;
        let run = ReportRun {
            questdb: &questdb,
            bucket: &bucket,
            process_start_ist_ns,
            ensured: AtomicBool::new(ensured),
        };
        // Incomplete old folders already reported by this process (one line
        // per folder per process). Bounded by the folder count.
        let mut reported_incomplete = BTreeSet::new();
        // Old complete folders this process ran with no progress (LF-9
        // hardening): not run again by it. Bounded by the folder count.
        let mut unfinished_old = BTreeSet::new();
        loop {
            let now = now_ist_ns();
            if let Some(today) = ist_date_of(now).filter(|d| calendar.is_trading_day(*d)) {
                let sod = sod_of(now);
                if in_session(sod) {
                    let wait = u64::from(REPORT_ATTEMPT_SECS[0].saturating_sub(sod));
                    tokio::time::sleep(Duration::from_secs(wait)).await;
                }
                // Today first (LF-7 review findings 3, 6, 9, 12).
                let mut done = None;
                while let Some((idx, wait, final_try)) = today_step(sod_of(now_ist_ns()), done) {
                    if off_worker(|| {
                        crate::daily_task_marker::daily_marker_exists(LIVE_FILL_REPORT_TASK, today)
                    }) {
                        break;
                    }
                    tokio::time::sleep(Duration::from_secs(u64::from(wait))).await;
                    done = Some(idx);
                    if bounded_run(&run, today, idx + 1, final_try, false).await == Some(true) {
                        break;
                    }
                }
                // Then the leftovers, while the clock allows a whole run.
                let leftovers = pending_days(
                    today,
                    |d| calendar.is_trading_day(d),
                    |d| {
                        off_worker(|| {
                            crate::daily_task_marker::daily_marker_exists(LIVE_FILL_REPORT_TASK, d)
                        })
                    },
                );
                let mut deferred = false;
                for d in leftovers {
                    if !leftovers_allowed(sod_of(now_ist_ns())) {
                        info!(day = %d, "live fill report: leftover day deferred (no room outside the session and the stop window); retried after the stop window or at the next boot");
                        deferred = true;
                        break;
                    }
                    let _done = bounded_run(&run, d, 0, true, true).await;
                }
                // Then day folders older than the lookback that were never
                // copied (LF-7 review round 3, finding 2): copy only, never a
                // rebuild; an incomplete one is reported, not rebuilt.
                if !deferred {
                    let lookback = lookback_days(today, |d| calendar.is_trading_day(d));
                    deferred = run
                        .copy_old_days(
                            today,
                            &lookback,
                            &mut reported_incomplete,
                            &mut unfinished_old,
                        )
                        .await;
                }
                // LF-7 review round 3, finding 3: a start or an end inside the
                // stop window with a run still owed wakes at the window's end
                // today instead of sleeping to the next day.
                let today_owed = !off_worker(|| {
                    crate::daily_task_marker::daily_marker_exists(LIVE_FILL_REPORT_TASK, today)
                });
                if let Some(secs) =
                    wake_after_stop_window(sod_of(now_ist_ns()), today_owed || deferred)
                {
                    info!(
                        secs,
                        today_owed,
                        deferred,
                        "live fill report: run owed; looking again after the stop window"
                    );
                    tokio::time::sleep(Duration::from_secs(secs)).await;
                    continue;
                }
            }
            let after = now_ist_ns();
            let secs = crate::expiry_scope_check::secs_until_after_checked_day(now, after);
            tokio::time::sleep(Duration::from_secs(secs)).await;
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// This module's own source, read once for the two source scans.
    const MOD_SRC: &str = include_str!("mod.rs");

    #[test]
    fn test_attempt_plan_never_in_session_and_ends_before_stop() {
        assert_eq!(attempt_plan(0, None), Some((0, 56_820)));
        assert_eq!(attempt_plan(56_820, None), Some((0, 0)));
        assert_eq!(attempt_plan(56_820, Some(0)), Some((1, 420)));
        assert_eq!(attempt_plan(57_000, None), Some((1, 240)));
        assert_eq!(attempt_plan(57_660, Some(1)), Some((2, 0)));
        assert_eq!(attempt_plan(57_660, Some(2)), None);
        assert_eq!(attempt_plan(57_661, None), None);
        for a in REPORT_ATTEMPT_SECS {
            assert!(!in_session(a));
            assert!(
                u64::from(a) + REPORT_ATTEMPT_TIMEOUT_SECS + STOP_MARGIN_SECS
                    < u64::from(SCHEDULED_STOP_WINDOW_START_SECS_OF_DAY_IST)
            );
        }
        assert!(in_session(9 * 3_600));
        assert!(in_session(TICK_PERSIST_END_SECS_OF_DAY_IST - 1));
        assert!(!in_session(TICK_PERSIST_END_SECS_OF_DAY_IST));
        assert!(!in_session(9 * 3_600 - 1));
    }

    #[test]
    fn test_pending_days_oldest_first_trading_only_and_capped() {
        let today = NaiveDate::from_ymd_opt(2026, 11, 16).unwrap(); // Monday
        let weekday = |d: NaiveDate| {
            use chrono::Datelike;
            d.weekday().number_from_monday() <= 5
        };
        let none = pending_days(today, weekday, |_| false);
        assert_eq!(
            none,
            vec![
                NaiveDate::from_ymd_opt(2026, 11, 11).unwrap(),
                NaiveDate::from_ymd_opt(2026, 11, 12).unwrap(),
                NaiveDate::from_ymd_opt(2026, 11, 13).unwrap(),
            ]
        );
        let marked_13 = NaiveDate::from_ymd_opt(2026, 11, 13).unwrap();
        let some = pending_days(today, weekday, |d| d == marked_13);
        assert_eq!(some.len(), 2);
        assert!(!some.contains(&marked_13));
        assert!(pending_days(today, |_| false, |_| false).is_empty());
    }

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
    fn test_wal_settled_needs_both_tables_applied_and_not_suspended() {
        assert!(wal_settled(&[]));
        assert!(wal_settled(&[
            wal("live_order_events", Some(5), Some(5), false),
            wal("candles_1s", Some(9), Some(9), false),
            wal("ticks", Some(1), Some(9), false),
        ]));
        assert!(!wal_settled(&[wal("candles_1s", Some(8), Some(9), false)]));
        assert!(!wal_settled(&[wal(
            "live_order_events",
            Some(9),
            Some(9),
            true
        )]));
        assert!(!wal_settled(&[wal("candles_1s", None, Some(9), false)]));
    }

    #[test]
    fn test_dir_has_no_files_reads_missing_as_empty() {
        let base = std::env::temp_dir().join(format!(
            "tv_lfr_spill_{}_{}",
            std::process::id(),
            now_ist_ns()
        ));
        assert!(dir_has_no_files(&base));
        std::fs::create_dir_all(base.join("quarantine")).unwrap();
        assert!(dir_has_no_files(&base));
        std::fs::write(base.join("batch.ilp"), b"x").unwrap();
        assert!(!dir_has_no_files(&base));
    }

    #[test]
    fn test_ymd_number_and_register_metrics() {
        assert_eq!(ymd_number("2026-11-12"), 20_261_112);
        assert_eq!(ymd_number("bad"), 0);
        register_metrics();
    }

    #[test]
    fn test_daily_row_maps_counts_and_unknown_latency_as_minus_one() {
        let ctx = ReportContext {
            boot: Some(crate::live_fill::BootOutcome::IdleNoEngine),
            process_started_ist_ns: 0,
            settled: true,
            truncated: false,
            catch_up: false,
        };
        let r = build_report("2026-11-12", &[], &[], ctx);
        let row = daily_row(&r, 7, true);
        assert_eq!(row.status, "no_orders");
        assert_eq!(row.light, "green");
        assert_eq!(row.orders, 0);
        assert_eq!(row.p99_feed_lag_us, -1);
        assert_eq!(row.max_api_rtt_us, -1);
        assert_eq!(row.p99_decision_to_sent_us, -1);
        assert!(row.s3_uploaded);
        assert_eq!(row.day_ist_nanos, 1_794_441_600_000_000_000);
    }

    /// The report module places no order, calls no broker and deletes no
    /// object (source scan of every file in the module).
    #[test]
    fn test_report_module_places_no_order() {
        let sources = [
            MOD_SRC,
            include_str!("rows.rs"),
            include_str!("recheck.rs"),
            include_str!("build.rs"),
            include_str!("render.rs"),
            include_str!("upload.rs"),
        ];
        let banned = [
            concat!("place", "_order"),
            concat!("dry", "_run"),
            concat!("oms", "::"),
            concat!("dhan", ".co"),
            concat!("delete", "_object"),
            concat!("remove", "_file("),
            concat!("cancel", "_order"),
        ];
        for src in sources {
            for b in banned {
                assert!(!src.contains(b), "{b} found in the report module");
            }
        }
    }

    /// The two LIVE-FILL codes never page: no alarm list names them.
    #[test]
    fn test_report_codes_not_in_error_code_alerts() {
        let alarms = include_str!("../../../../deploy/aws/terraform/error-code-alarms.tf");
        assert!(!alarms.contains("LIVE-FILL-01"));
        assert!(!alarms.contains("LIVE-FILL-02"));
    }

    /// `start` spawns nothing with `[live_fill] enabled = false` (the
    /// shipped default in `config/base.toml`).
    #[test]
    fn test_start_is_none_when_live_fill_is_off() {
        use figment::Figment;
        use figment::providers::{Format, Toml};
        let path = if Path::new("config/base.toml").exists() {
            "config/base.toml"
        } else {
            "../../config/base.toml"
        };
        let mut config: tickvault_common::config::ApplicationConfig = Figment::new()
            .merge(Toml::file(path))
            .extract()
            .expect("config/base.toml must parse"); // APPROVED: test
        config.live_fill.enabled = false;
        let calendar = Arc::new(
            TradingCalendar::from_config(&config.trading).expect("calendar builds"), // APPROVED: test
        );
        assert!(start(&config, &calendar, 0).is_none());
    }

    /// `main` starts the report, and `start` spawns it only when
    /// `[live_fill] enabled`.
    #[test]
    fn test_main_start_spawn_live_fill_report_only_under_live_fill_enabled() {
        let main = include_str!("../main.rs");
        assert!(main.contains(
            "live_fill_report::start(&config, &trading_calendar, process_start_ist_nanos)"
        ));
        // `start` itself: the behaviour test `test_start_is_none_when_live_fill_is_off`.
    }

    /// Review findings 3, 6, 9, 12 (LF-7): today's planned attempts come
    /// first; a start after 16:01 with today unreported gets ONE final attempt
    /// at once when it fits; never inside the session or into the stop window.
    #[test]
    fn test_today_step_attempts_first_then_one_late_final_attempt() {
        assert_eq!(today_step(0, None), Some((0, 56_820, false)));
        assert_eq!(today_step(57_000, None), Some((1, 240, false)));
        assert_eq!(today_step(57_660, Some(1)), Some((2, 0, true)));
        // After the last planned slot with nothing run: one final attempt now.
        let late = 57_700;
        assert_eq!(today_step(late, None), Some((2, 0, true)));
        // ... only once.
        assert_eq!(today_step(late + 400, Some(2)), None);
        // Not when it would run into the stop window (16:20 + 7 min > 16:24).
        assert_eq!(today_step(16 * 3_600 + 20 * 60, None), None);
        // After the stop window (a drain-held evening box): allowed.
        assert_eq!(
            today_step(SCHEDULED_STOP_WINDOW_END_SECS_OF_DAY_IST + 1, None),
            Some((2, 0, true))
        );
        // Done attempts never re-run as a late attempt.
        assert_eq!(today_step(late, Some(0)), None);
    }

    /// LF-7 review round 3, finding 3: a start (or an end) in the
    /// 16:17–16:45 gap with today unreported, or a leftover deferred, wakes at
    /// the stop window's end TODAY, where today_step plans one final attempt
    /// and leftovers_allowed lets the leftovers run; nothing owed, before the
    /// session's end, or after the window sleeps to the next day.
    #[test]
    fn test_wake_after_stop_window_plans_a_run_at_the_window_end() {
        let end = SCHEDULED_STOP_WINDOW_END_SECS_OF_DAY_IST;
        for sod in [
            16 * 3_600 + 17 * 60,
            16 * 3_600 + 20 * 60,
            SCHEDULED_STOP_WINDOW_START_SECS_OF_DAY_IST,
            end - 1,
        ] {
            // The dead zone: no attempt and no leftover may start here.
            assert_eq!(today_step(sod, None), None, "{sod}");
            assert!(!leftovers_allowed(sod), "{sod}");
            let wait = wake_after_stop_window(sod, true).expect("woken today");
            assert_eq!(u64::from(sod) + wait, u64::from(end), "{sod}");
            // At the wake: a whole run fits, so today and leftovers run.
            let at = sod + u32::try_from(wait).unwrap();
            assert_eq!(today_step(at, None), Some((2, 0, true)), "{sod}");
            assert!(leftovers_allowed(at), "{sod}");
            assert_eq!(wake_after_stop_window(sod, false), None, "{sod}");
        }
        // Inside the session, at or after the window's end: the next day.
        assert_eq!(wake_after_stop_window(10 * 3_600, true), None);
        assert_eq!(wake_after_stop_window(end, true), None);
        assert_eq!(wake_after_stop_window(end + 600, true), None);
        // The spawn loop wakes there and re-enters instead of sleeping to the
        // next day.
        let spawn = &MOD_SRC[MOD_SRC
            .find("pub fn spawn_live_fill_report(")
            .expect("spawn")..];
        let wake = spawn
            .find("wake_after_stop_window(sod_of(now_ist_ns())")
            .expect("wake wired");
        let next_day = spawn
            .find("secs_until_after_checked_day(now, after)")
            .expect("next day");
        assert!(wake < next_day);
        assert!(spawn[wake..next_day].contains("continue;"));
    }

    /// LF-7 review round 3, finding 2: the lookback is the last three trading
    /// days; pending_days is its unmarked subset, and the spawn loop then
    /// copies the older uncopied folders (copy_old_days) instead of dropping
    /// them.
    #[test]
    fn test_lookback_days_is_the_three_trading_days_and_old_days_are_copied() {
        let today = NaiveDate::from_ymd_opt(2026, 11, 16).unwrap(); // Monday
        let weekday = |d: NaiveDate| {
            use chrono::Datelike;
            d.weekday().number_from_monday() <= 5
        };
        let lb = lookback_days(today, weekday);
        assert_eq!(
            lb,
            vec![
                NaiveDate::from_ymd_opt(2026, 11, 11).unwrap(),
                NaiveDate::from_ymd_opt(2026, 11, 12).unwrap(),
                NaiveDate::from_ymd_opt(2026, 11, 13).unwrap(),
            ]
        );
        assert_eq!(pending_days(today, weekday, |_| false), lb);
        assert!(lookback_days(today, |_| false).is_empty());
        let spawn = &MOD_SRC[MOD_SRC
            .find("pub fn spawn_live_fill_report(")
            .expect("spawn")..];
        let leftovers = spawn.find("pending_days(").expect("leftovers");
        let old = spawn.find(".copy_old_days(").expect("old days wired");
        assert!(leftovers < old);
        let copy_old = &MOD_SRC[MOD_SRC
            .find("    async fn copy_old_days(")
            .expect("copy_old_days")..];
        let copy_old = &copy_old[..copy_old.find("    async fn copy_verified(").expect("end")];
        assert!(copy_old.contains("upload::unreported_day_folders("));
        // Copy only, never a rebuild: a catch-up run with the folder complete.
        assert!(copy_old.contains("bounded_run(self, day, 0, true, true)"));
        assert!(copy_old.contains("\"outside_lookback_incomplete\""));
        assert!(copy_old.contains("OLD_DAY_COPIES_PER_PASS"));
    }

    /// Review findings 6, 9, 12: a leftover starts only when its whole
    /// bounded run stays outside 09:00–15:40 and clear of the stop window.
    #[test]
    fn test_leftovers_allowed_attempt_fits_outside_session_refuses_0856_session_and_1622() {
        let t = |h: u32, m: u32| h * 3_600 + m * 60;
        assert!(leftovers_allowed(t(8, 50)));
        assert!(!leftovers_allowed(t(8, 56)), "would run into 09:00");
        assert!(!leftovers_allowed(t(10, 0)), "in session");
        assert!(!leftovers_allowed(t(15, 39)), "in session");
        assert!(leftovers_allowed(t(15, 50)));
        assert!(leftovers_allowed(t(16, 10)));
        assert!(
            !leftovers_allowed(t(16, 22)),
            "would run into the stop window"
        );
        assert!(!leftovers_allowed(t(16, 30)), "inside the stop window");
        assert!(leftovers_allowed(SCHEDULED_STOP_WINDOW_END_SECS_OF_DAY_IST));
        for sod in (0..86_400).step_by(60) {
            if leftovers_allowed(sod) {
                let end = u64::from(sod) + REPORT_ATTEMPT_TIMEOUT_SECS;
                assert!(
                    end <= u64::from(SESSION_OPEN_SECS_OF_DAY)
                        || sod >= TICK_PERSIST_END_SECS_OF_DAY_IST,
                    "{sod} would cross into the session"
                );
                assert!(
                    !(SCHEDULED_STOP_WINDOW_START_SECS_OF_DAY_IST
                        ..SCHEDULED_STOP_WINDOW_END_SECS_OF_DAY_IST)
                        .contains(&sod)
                );
            }
        }
        assert!(attempt_fits_outside_session(t(15, 47)));
    }

    /// Review findings 4 and 10: whatever the readiness wait and the query
    /// take, the re-pricing budget leaves the file write, both summary rows
    /// and the least copy inside the attempt, and the copy leaves the last
    /// row its bound.
    #[test]
    fn test_readiness_budget_recheck_budget_copy_budget_fit_the_attempt() {
        let total = Duration::from_secs(REPORT_ATTEMPT_TIMEOUT_SECS - ATTEMPT_GRACE_SECS);
        let ready = readiness_budget(total);
        assert_eq!(ready, Duration::from_secs(REPORT_READINESS_WAIT_SECS));
        for waited in (0..=ready.as_secs()).step_by(5) {
            for query in (0..=REPORT_QUERY_TIMEOUT_SECS).step_by(5) {
                let remaining = total - Duration::from_secs(waited + query);
                let recheck = recheck_budget(remaining);
                assert!(recheck <= Duration::from_secs(RECHECK_WALL_BUDGET_SECS));
                let after = remaining - recheck - Duration::from_secs(RECHECK_QUERY_TIMEOUT_SECS);
                assert!(
                    after >= Duration::from_secs(TAIL_AFTER_RECHECK_SECS),
                    "waited {waited} query {query}: {after:?} left"
                );
                let copy_remaining =
                    after - Duration::from_secs(FILE_WRITE_RESERVE_SECS + DB_ROW_STEP_BOUND_SECS);
                let copy = copy_budget(copy_remaining);
                assert!(copy >= Duration::from_secs(COPY_MIN_SECS));
                assert_eq!(
                    copy_remaining - copy,
                    Duration::from_secs(DB_ROW_STEP_BOUND_SECS)
                );
            }
        }
        // A late start shrinks the readiness wait so the later steps fit.
        let short = Duration::from_secs(TAIL_AFTER_READINESS_SECS + 30);
        assert_eq!(readiness_budget(short), Duration::from_secs(30));
        assert_eq!(readiness_budget(Duration::ZERO), Duration::ZERO);
        assert_eq!(recheck_budget(Duration::ZERO), Duration::ZERO);
        assert_eq!(copy_budget(Duration::ZERO), Duration::ZERO);
    }

    /// Review finding 8: an ILP row write must never create the table. Not
    /// ensured and the re-ensure fails: the write never runs. Re-ensure
    /// works: it runs, and later rows skip the ensure.
    #[tokio::test]
    async fn test_gated_row_write_never_writes_when_the_table_is_not_ensured() {
        use std::sync::atomic::AtomicUsize;
        let ensures = AtomicUsize::new(0);
        let writes = AtomicUsize::new(0);
        let ensured = AtomicBool::new(false);
        let ok = gated_row_write(
            &ensured,
            || {
                ensures.fetch_add(1, Ordering::SeqCst);
                async { false }
            },
            || {
                writes.fetch_add(1, Ordering::SeqCst);
                async { true }
            },
            "2026-11-12",
            1,
        )
        .await;
        assert!(!ok);
        assert_eq!(
            writes.load(Ordering::SeqCst),
            0,
            "no ILP write may create the table"
        );
        assert_eq!(ensures.load(Ordering::SeqCst), 1);
        let ok = gated_row_write(
            &ensured,
            || {
                ensures.fetch_add(1, Ordering::SeqCst);
                async { true }
            },
            || {
                writes.fetch_add(1, Ordering::SeqCst);
                async { true }
            },
            "2026-11-12",
            1,
        )
        .await;
        assert!(ok);
        assert_eq!(writes.load(Ordering::SeqCst), 1);
        assert!(ensured.load(Ordering::SeqCst));
        let _ = gated_row_write(
            &ensured,
            || {
                ensures.fetch_add(1, Ordering::SeqCst);
                async { false }
            },
            || {
                writes.fetch_add(1, Ordering::SeqCst);
                async { true }
            },
            "2026-11-12",
            1,
        )
        .await;
        assert_eq!(
            ensures.load(Ordering::SeqCst),
            2,
            "an ensured table is not re-ensured"
        );
        assert_eq!(writes.load(Ordering::SeqCst), 2);
    }

    /// Review finding 7: a catch-up that only copies rebuilds the summary row
    /// from the copied report file, equal to the row of the built report.
    #[test]
    fn test_daily_row_from_report_json_equals_daily_row_of_the_report() {
        let ctx = ReportContext {
            boot: Some(crate::live_fill::BootOutcome::Running),
            process_started_ist_ns: 0,
            settled: false,
            truncated: false,
            catch_up: false,
        };
        let r = build_report("2026-11-12", &[], &[], ctx);
        let json = render::render_json(&r);
        assert_eq!(
            daily_row_from_report_json(&json, 9, true),
            Some(daily_row(&r, 9, true))
        );
        assert_eq!(daily_row_from_report_json("{}", 9, true), None);
        assert_eq!(daily_row_from_report_json("not json", 9, true), None);
        let odd = json.replace("\"amber\"", "\"purple\"");
        assert_eq!(daily_row_from_report_json(&odd, 9, true), None);
    }

    /// Review findings 7, 9, 10 (wiring): the row and the counters are written
    /// before the copy, the copy has its own deadline, and today's attempts
    /// run before the leftovers.
    #[test]
    fn test_run_day_writes_row_before_copy_and_spawn_runs_today_before_leftovers() {
        let src = MOD_SRC;
        let body = |start: &str| {
            let at = src.find(start).expect("fn present");
            &src[at..]
        };
        let run_day = body("    async fn run_day(");
        let row_false = run_day
            .find("daily_row(&built, now_ist_ns(), false)")
            .expect("row before copy");
        let counter = run_day
            .find("RUNS_COUNTER, \"outcome\" => built.status")
            .expect("counter");
        let copy = run_day
            .find("copy_budget(left(deadline))")
            .expect("copy budget");
        let row_true = run_day
            .find("daily_row(r, now_ist_ns(), true)")
            .expect("row after copy");
        assert!(counter < copy && row_false < copy && copy < row_true);
        let spawn = body("pub fn spawn_live_fill_report(");
        let today = spawn.find("today_step(").expect("today first");
        let leftovers = spawn.find("pending_days(").expect("leftovers");
        let gate = spawn.find("leftovers_allowed(").expect("leftover gate");
        assert!(today < leftovers && leftovers < gate);
        assert!(spawn.contains("AtomicBool::new(ensured)"));
    }

    /// LF-9 finding 6: the day's marker is written only after the summary
    /// row that says the copy is verified (copy, row, marker), and only when
    /// that row was written: a row that fails leaves the day unmarked, so a
    /// later attempt or the next boot copies again (idempotent) and writes
    /// it. The copy itself writes no marker.
    #[test]
    fn test_regression_marker_only_after_the_row_after_a_verified_copy() {
        let run_day = &MOD_SRC[MOD_SRC.find("    async fn run_day(").expect("run_day")..];
        let run_day = &run_day[..run_day
            .find("    async fn already_reported_and_copied(")
            .expect("end")];
        let copy = run_day.find("self.copy_verified(").expect("copy");
        let row = run_day
            .find("daily_row(r, now_ist_ns(), true)")
            .expect("row after copy");
        let record = run_day
            .find("let row_written = row_written && self.record_row_written(")
            .expect("the row record gated on the row");
        let marker = run_day
            .find("let marked = row_written && self.write_marker(")
            .expect("marker gated on the row");
        assert!(copy < row && row < marker);
        assert!(
            row < record && record < marker,
            "row, its record, then the marker"
        );
        let copy_fn = &MOD_SRC[MOD_SRC
            .find("    async fn copy_verified(")
            .expect("copy fn")..];
        let copy_fn = &copy_fn[..copy_fn.find("    async fn write_db_row(").expect("end")];
        assert!(
            !copy_fn.contains("try_write_daily_marker"),
            "the copy writes no marker"
        );
        assert!(run_day.contains("return marked || final_try;"));
    }

    /// LF-9 finding 7: a catch-up of a day with no local state asks the
    /// bucket and the summary row first, and repairs (never rebuilds) a day
    /// already reported and copied.
    #[test]
    fn test_regression_catch_up_never_rebuilds_a_day_already_copied() {
        assert!(catch_up_repairs_instead(5, None), "files in the bucket");
        assert!(catch_up_repairs_instead(1, Some(false)), "a part copy");
        assert!(
            catch_up_repairs_instead(0, Some(true)),
            "the row says copied"
        );
        assert!(!catch_up_repairs_instead(0, Some(false)));
        assert!(!catch_up_repairs_instead(0, None), "never copied: rebuild");
        let run_day = &MOD_SRC[MOD_SRC.find("    async fn run_day(").expect("run_day")..];
        let probe = run_day
            .find("if catch_up_probes_bucket(catch_up, started) {")
            .expect("the probe for a day whose copy never started");
        let ask = run_day
            .find("self.already_reported_and_copied(&day_s)")
            .expect("asks the bucket and the row");
        let rebuild = run_day
            .find("if rebuild_wanted(catch_up, complete, started) {")
            .expect("rebuild");
        assert!(probe < ask && ask < rebuild);
    }

    /// LF-9 review: (1) a catch-up whose copy never started asks the bucket
    /// even with a complete local folder; a probe that fails still builds a
    /// never-reported day locally (files and a `s3_uploaded = false` row) and
    /// never copies or marks it, so a later catch-up or the old-day pass
    /// finishes it once the bucket answers; (2) a day found in the bucket is
    /// marked done only when its summary row already says the copy was
    /// verified.
    #[test]
    fn test_regression_probe_failure_builds_locally_and_repair_needs_a_verified_row() {
        assert!(catch_up_probes_bucket(true, false));
        assert!(!catch_up_probes_bucket(true, true), "a started copy");
        assert!(!catch_up_probes_bucket(false, false), "today's run");
        assert!(catch_up_repair_may_mark(Some(true)));
        assert!(
            !catch_up_repair_may_mark(Some(false)),
            "row says not copied"
        );
        assert!(!catch_up_repair_may_mark(None), "no row");
        let run_day = &MOD_SRC[MOD_SRC.find("    async fn run_day(").expect("run_day")..];
        let run_day = &run_day[..run_day
            .find("    async fn already_reported_and_copied(")
            .expect("end")];
        // (1) A probe error does not return before the build unless the day
        // is complete or has a row; it blocks the copy instead.
        let err = run_day
            .find("                Err(e) => {")
            .expect("probe error arm");
        let err_arm = &run_day[err..run_day.find("Ok(Some((files, row))) =>").expect("repair")];
        assert!(err_arm.contains("if !probe_failure_may_build(complete, &row) {"));
        assert!(err_arm.contains("copy_blocked = true;"));
        let build = run_day
            .find("if rebuild_wanted(catch_up, complete, started) {")
            .expect("build");
        let blocked = run_day.find("if copy_blocked {").expect("copy gate");
        let copy = run_day.find("self.copy_verified(").expect("copy");
        assert!(err < build && build < blocked && blocked < copy);
        let gate = &run_day[blocked..copy];
        assert!(gate.contains("return false;"), "no copy, no marker");
        assert!(!gate.contains("write_marker"));
        // (2) The repair arm checks the row before the marker.
        let repair = &run_day[run_day.find("Ok(Some((files, row))) =>").expect("repair")..];
        let check = repair
            .find("if !catch_up_repair_may_mark(row) {")
            .expect("row check");
        let mark = repair.find("try_write_daily_marker").expect("marker");
        assert!(check < mark);
    }

    /// LF-9 review, findings 6 and 10: when the bucket probe fails, a day is
    /// built locally only when its summary row read SUCCEEDED and found no
    /// row. A failed row read (a timeout, a bad status, an unreadable body)
    /// used to read as "no row", so a day already reported green and copied
    /// was rebuilt Amber and its row overwritten with `s3_uploaded = false`.
    /// The failure arm now returns, reported as `summary_row_read`, before
    /// any build or row write.
    #[test]
    fn test_regression_a_failed_row_read_never_builds_probe_failure_may_build() {
        assert!(probe_failure_may_build(false, &Ok(None)), "never reported");
        assert!(!probe_failure_may_build(true, &Ok(None)), "complete folder");
        assert!(!probe_failure_may_build(false, &Ok(Some(true))), "a row");
        assert!(!probe_failure_may_build(false, &Ok(Some(false))), "a row");
        assert!(
            !probe_failure_may_build(false, &Err("timed out".to_owned())),
            "a failed read is not no row"
        );
        let run_day = &MOD_SRC[MOD_SRC.find("    async fn run_day(").expect("run_day")..];
        let err = run_day
            .find("                Err(e) => {")
            .expect("probe error arm");
        let err_arm = &run_day[err..run_day.find("Ok(Some((files, row))) =>").expect("repair")];
        let read_fail = err_arm
            .find("if let Err(re) = &row {")
            .expect("failed read handled");
        let returns = err_arm[read_fail..].find("return false;").expect("returns") + read_fail;
        let build = run_day
            .find("if rebuild_wanted(catch_up, complete, started) {")
            .expect("build");
        assert!(err + returns < build, "returns before any build");
        assert!(err_arm[read_fail..returns].contains("\"summary_row_read\""));
        // The bucket probe itself: a failed row read with no file in the
        // bucket is an error (the failure arm decides), never "no row".
        let probe = &MOD_SRC[MOD_SRC
            .find("    async fn already_reported_and_copied(")
            .expect("probe")..];
        assert!(probe.contains("Err(e) if files == 0 => return Err(e),"));
    }

    /// LF-7 review finding 5: once a copy of the day started, no attempt
    /// (today's retry, a same-day restart's late attempt, or a catch-up)
    /// rebuilds the files; only a day never copied is built afresh.
    #[test]
    fn test_rebuild_wanted_never_after_a_copy_started() {
        for catch_up in [false, true] {
            for complete in [false, true] {
                assert!(
                    !rebuild_wanted(catch_up, complete, true),
                    "catch_up={catch_up} complete={complete}"
                );
            }
        }
        assert!(rebuild_wanted(false, false, false));
        assert!(rebuild_wanted(false, true, false), "today, never copied");
        assert!(rebuild_wanted(true, false, false));
        assert!(!rebuild_wanted(true, true, false), "catch-up only copies");
        let run_day = &MOD_SRC[MOD_SRC.find("    async fn run_day(").expect("run_day")..];
        assert!(run_day.contains("if rebuild_wanted(catch_up, complete, started) {"));
        assert!(run_day.contains("copy_started(&dir)"));
    }

    fn event(signal: i64, seq: i64, ev: &str) -> rows::EventRow {
        rows::EventRow {
            ts_ns: seq,
            event: ev.to_owned(),
            signal_id: signal,
            seq,
            side: "BUY".to_owned(),
            qty: 500,
            ..rows::EventRow::default()
        }
    }

    /// LF-9 finding 3: the chain held two orders on one instrument with
    /// overlapping windows; the first was refused by a taint after it had
    /// taken part of a shared second, so the second's share is unknown and it
    /// reads `shared_volume_unknown` (never a read, never a guessed fill). A
    /// third order on another instrument is unaffected (here it reads
    /// `seconds_read_failed`: the database is unreachable).
    #[tokio::test]
    async fn test_recheck_all_marks_an_order_sharing_with_a_tainted_one_unknown_held_share_known() {
        let decision = |sid: i64, sec_id: i64, end_s: i64| {
            let mut d = event(sid, 0, "decision");
            d.security_id = sec_id;
            d.segment = "NSE_FNO".to_owned();
            d.qty = 100;
            d.candle_end_ns = Some(end_s * 1_000_000_000);
            d.ltp_paise = Some(10_000);
            d.limit_paise = Some(10_100);
            d.tick_paise = Some(5);
            d.lot_size = Some(1);
            d.participation_bps = Some(2_500);
            d.max_fill_seconds = Some(60);
            d
        };
        let t0 = 1_794_441_600 + 36_000;
        let mut rows = vec![
            decision(1, 2885, t0),
            event(1, 1, "built"),
            decision(2, 2885, t0 + 10),
            event(2, 1, "built"),
            event(2, 2, "sent"),
            event(2, 3, "cancel"),
            decision(3, 7, t0),
            event(3, 1, "built"),
            event(3, 2, "sent"),
            event(3, 3, "cancel"),
        ];
        let mut tainted = event(1, 2, "reject");
        tainted.reason = Some("tick_dropped".to_owned());
        rows.push(tainted);
        let known = build::held_share_known(&rows);
        assert_eq!(known.get(&1), Some(&false), "finished unpriced");
        assert_eq!(known.get(&2), Some(&true));
        let questdb = QuestDbConfig {
            host: "127.0.0.1".to_owned(),
            http_port: 1,
            pg_port: 1,
            ilp_port: 1,
        };
        let run = ReportRun {
            questdb: &questdb,
            bucket: "tv-test-cold",
            process_start_ist_ns: 0,
            ensured: AtomicBool::new(false),
        };
        let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
        let mut out = run.recheck_all(&rows, false, "2026-11-12", deadline).await;
        out.sort_by_key(|(sid, _)| *sid);
        assert_eq!(out.len(), 2, "{out:?}");
        assert_eq!(
            out[0],
            (
                2,
                Recheck::not_run(RecheckKind::Unavailable("shared_volume_unknown"))
            )
        );
        assert_eq!(
            out[1],
            (
                3,
                Recheck::not_run(RecheckKind::Unavailable("seconds_read_failed"))
            )
        );
    }

    /// LF-7 review finding 3: the 500-order cap counts only the orders the
    /// report will compare. 600 unplaced refusals ahead of three placed
    /// orders used to take every slot and mark the placed ones
    /// `recheck_skipped`; now the refusals are never read and the placed
    /// orders are the only re-pricings. `catch_up` keeps the test off the
    /// database (each read is `catch_up_rebuild` instead).
    #[tokio::test]
    async fn test_recheck_all_cap_counts_only_priced_orders_via_priced_signal_ids() {
        let mut rows = Vec::new();
        for sid in 1..=600 {
            let mut d = event(sid, 0, "decision");
            d.qty = 0;
            rows.push(d);
            let mut r = event(sid, 1, "reject");
            r.reason = Some("exit_without_position".to_owned());
            rows.push(r);
        }
        for sid in 1_001..=1_003 {
            rows.push(event(sid, 0, "decision"));
            rows.push(event(sid, 1, "built"));
            rows.push(event(sid, 2, "sent"));
            rows.push(event(sid, 3, "cancel"));
        }
        assert_eq!(
            build::priced_signal_ids(&rows)
                .into_iter()
                .collect::<Vec<_>>(),
            [1_001, 1_002, 1_003]
        );
        let questdb = QuestDbConfig {
            host: "127.0.0.1".to_owned(),
            http_port: 1,
            pg_port: 1,
            ilp_port: 1,
        };
        let run = ReportRun {
            questdb: &questdb,
            bucket: "tv-test-cold",
            process_start_ist_ns: 0,
            ensured: AtomicBool::new(false),
        };
        let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
        let out = run.recheck_all(&rows, true, "2026-11-12", deadline).await;
        assert_eq!(out.len(), 3, "unplaced refusals are never re-priced");
        for (sid, r) in &out {
            assert!((1_001..=1_003).contains(sid));
            assert_eq!(r.kind, RecheckKind::Unavailable("catch_up_rebuild"));
        }
    }

    /// LF-9 hardening: old day folders that cannot finish never starve a
    /// newer one that was never copied. Four complete folders older than the
    /// lookback: the three oldest are stuck (case 1: copied, their summary
    /// row can never be written; case 2: never copied locally, the bucket
    /// already holds their files with no verified row), the newest is complete
    /// and was never copied. The newest is copied in the FIRST pass, and a
    /// second pass of the same process does not run the stuck ones again.
    /// Before, the three oldest took every slot of every pass, oldest first,
    /// and the newest was never copied.
    #[tokio::test]
    async fn test_regression_stuck_old_days_never_starve_a_never_copied_one() {
        for stuck_copied in [true, false] {
            let base = std::env::temp_dir().join(format!(
                "tv_lfr_old_{}_{}_{stuck_copied}",
                std::process::id(),
                now_ist_ns()
            ));
            let days = ["2026-10-01", "2026-10-02", "2026-10-05", "2026-10-06"];
            for (i, d) in days.iter().enumerate() {
                let dir = upload::day_dir(&base, d);
                std::fs::create_dir_all(upload::markers_dir(&dir)).expect("dirs");
                for n in report_file_names(d) {
                    std::fs::write(dir.join(&n), b"x").expect("file");
                    if i < 3 && stuck_copied {
                        std::fs::write(upload::markers_dir(&dir).join(&n), b"x").expect("rec");
                    }
                }
            }
            let today = NaiveDate::from_ymd_opt(2026, 11, 13).expect("day");
            let newest = NaiveDate::from_ymd_opt(2026, 10, 6).expect("day");
            let found = upload::unreported_day_folders(&base, today, |_| false).expect("scan");
            assert_eq!(found.len(), 4);
            let mut reported = BTreeSet::new();
            let mut unfinished = BTreeSet::new();
            let ran = std::cell::RefCell::new(Vec::new());
            let run = |day: NaiveDate, _copied: bool| {
                ran.borrow_mut().push(day);
                std::future::ready(day == newest)
            };
            let owed = ReportRun::walk_old_days(
                found.clone(),
                &mut reported,
                &mut unfinished,
                || true,
                run,
            )
            .await;
            assert!(
                ran.borrow().contains(&newest),
                "stuck_copied={stuck_copied}: the newest is copied in one pass: {:?}",
                ran.borrow()
            );
            assert!(!owed);
            assert_eq!(unfinished.len(), 3);
            // A second pass of the process runs none of the stuck ones again.
            ran.borrow_mut().clear();
            let rest: Vec<_> = found.into_iter().filter(|u| u.day != newest).collect();
            let run = |day: NaiveDate, _copied: bool| {
                ran.borrow_mut().push(day);
                std::future::ready(false)
            };
            let owed =
                ReportRun::walk_old_days(rest, &mut reported, &mut unfinished, || true, run).await;
            assert!(!owed);
            assert!(ran.borrow().is_empty(), "{:?}", ran.borrow());
            std::fs::remove_dir_all(&base).ok();
        }
    }

    /// The per-pass cap counts only runs that made progress, and a pass with
    /// no room left defers the rest.
    #[tokio::test]
    async fn test_walk_old_days_caps_progress_and_defers_without_room() {
        let day = |d: u32| NaiveDate::from_ymd_opt(2026, 10, d).expect("day");
        let found: Vec<upload::UnreportedDay> = (1..=5)
            .map(|d| upload::UnreportedDay {
                day: day(d),
                complete: true,
                copied: false,
            })
            .collect();
        let mut reported = BTreeSet::new();
        let mut unfinished = BTreeSet::new();
        let mut ran = 0usize;
        let owed = ReportRun::walk_old_days(
            found.clone(),
            &mut reported,
            &mut unfinished,
            || true,
            |_, _| {
                ran += 1;
                std::future::ready(true)
            },
        )
        .await;
        assert!(owed, "deferred by the cap");
        assert_eq!(ran, OLD_DAY_COPIES_PER_PASS);
        let owed = ReportRun::walk_old_days(
            found,
            &mut reported,
            &mut unfinished,
            || false,
            |_, _| std::future::ready(true),
        )
        .await;
        assert!(owed, "deferred for room");
    }
}
