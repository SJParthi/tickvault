//! Boot wiring for the daily 1-minute cross-verification (restored 2026-09-24).
//!
//! Authority: `no-rest-except-live-feed-2026-06-27.md` §12.15 and
//! `dhan-rest-only-noise-lock-2026-07-14.md` §2.5.
//!
//! Once per trading day, after the close, this task compares our own
//! `candles_1m` (`feed='dhan'`) against Dhan's own 1-minute tape
//! (`POST /v2/charts/intraday`, interval `"1"`) in integer paise. It is its own
//! task: the live lane NEVER waits for it, and no socket depends on it.
//!
//! When the comparison is MEASURED (at least one compared minute) and its rows
//! were persisted, it writes the day's marker under
//! [`CROSSVERIFY_MARKER_TASK`]. The daily S3 archive holds that day's
//! partitions until the marker exists (see
//! `tickvault_storage::partition_archive::VerifiedDayGate`). A vacuous or
//! failed run writes NO marker, so the archive keeps waiting — up to its own
//! bounded hold ceiling.
//!
//! Complexity: cold path, once a day. Target building is O(subscribed); the
//! comparison itself is documented in `dhan_live_crossverify`.

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use secrecy::{ExposeSecret, SecretString};
use tickvault_common::config::QuestDbConfig;
use tickvault_common::error_code::ErrorCode;
use tickvault_common::trading_calendar::{TradingCalendar, ist_offset};
use tickvault_common::types::ExchangeSegment;
use tickvault_core::auth::token_manager::global_token_manager;
use tickvault_core::websocket::pool_supervisor::SubscribeInstrument;
use tickvault_storage::dhan_live_crossverify_persistence::DhanLiveXverifyAuditWriter;
use tracing::{error, info, warn};

use crate::daily_task_marker::{
    MarkerDurability, daily_marker_exists, daily_marker_path, try_write_daily_marker_keeping,
};
use crate::dhan_live_crossverify::{
    DayComparison, DhanLiveCrossverifyConfig, RUN_SECS_OF_DAY_IST, RunReport,
    SESSION_CLOSE_SECS_OF_DAY_IST, XverifyTarget, daily_row, deterministic_run_ts_nanos,
    run_cross_verification,
};
use crate::shutdown_class::{
    SCHEDULED_STOP_WINDOW_END_SECS_OF_DAY_IST, SCHEDULED_STOP_WINDOW_START_SECS_OF_DAY_IST,
};
use crate::volume_leaderboard::OptionFamily;

/// Marker task name. The S3 archive gate in `main.rs` reads the same constant,
/// so the writer and the reader can never disagree about the file name.
pub const CROSSVERIFY_MARKER_TASK: &str = "dhan_live_crossverify";

/// Days a cross-verification marker is kept (2026-10-06, plan item 51a,
/// `no-rest` §12.15.7).
///
/// The S3 archive gate reads a day's marker when that day's partitions age
/// out of their hot window, which is up to the largest GATED window
/// (`retention_days`, 90 by default; `market_data_hot_days` 15; depth and
/// intraday 1, raised to their floors by `effective_hot_days`) plus
/// [`tickvault_storage::partition_archive::MAX_CROSSVERIFY_HOLD_DAYS`]. The
/// default 7-day sweep deleted markers long before that, so verified days read
/// as unverified and logged a false "archived WITHOUT a cross-verification"
/// line. The disk-pressure leg is not gated, so `pressure_hot_days` does not
/// count. Cost: about 400 files of about 60 bytes.
pub const CROSSVERIFY_MARKER_KEEP_DAYS: i64 = 400;

const _: () = assert!(
    CROSSVERIFY_MARKER_KEEP_DAYS
        > 90 + tickvault_storage::partition_archive::MAX_CROSSVERIFY_HOLD_DAYS,
    "the cross-verification marker keep must outlast the default gated hold lookback"
);

/// Days of slack the boot check wants between the gated hold lookback and the
/// marker keep.
const CROSSVERIFY_MARKER_KEEP_SLACK_DAYS: i64 = 3;

/// Whether the configured gated hot window (the largest of the gated
/// `[partition_retention]` windows, `pressure_hot_days` excluded) plus the
/// hold comes within [`CROSSVERIFY_MARKER_KEEP_SLACK_DAYS`] of
/// [`CROSSVERIFY_MARKER_KEEP_DAYS`]. When it does, a verified day's marker can
/// be swept before the archive gate reads it. Raw configured days are used:
/// the floors `effective_hot_days` applies (1 and 2 days) only raise tiny
/// values. Pure, O(1).
#[must_use]
pub fn crossverify_marker_keep_is_short(gated_hot_days_max: u32) -> bool {
    i64::from(gated_hot_days_max)
        .saturating_add(tickvault_storage::partition_archive::MAX_CROSSVERIFY_HOLD_DAYS)
        .saturating_add(CROSSVERIFY_MARKER_KEEP_SLACK_DAYS)
        >= CROSSVERIFY_MARKER_KEEP_DAYS
}

/// Runs counter, labelled by `outcome`
/// (`measured` / `vacuous` / `diverged` / `failed` / `no_token`). Re-exported
/// from the feed stack, which has owned the name since 2026-08-26, so one
/// metric has exactly one declaration.
pub use crate::dhan_feed_stack::XVERIFY_RUNS_COUNTER;
/// Rows persisted (findings + vendor tape + the daily row). Since 2026-10-06
/// the spot check counts only rows the database ACKed, never a discarded one
/// (discards count on `tv_dhan_live_xverify_audit_rows_discarded_total`). The
/// §12.15.6 option pass keeps its old shape on purpose (see
/// `persist_option_findings`): it still counts every row of a pass whose final
/// flush succeeded.
pub const XVERIFY_PERSIST_ROWS_COUNTER: &str = "tv_dhan_feed_xverify_rows_total";
/// Persist failures (the final flush was refused).
pub const XVERIFY_PERSIST_ERRORS_COUNTER: &str = "tv_dhan_feed_xverify_persist_errors_total";
/// Subscribed instruments the comparator cannot target (F&O etc.).
pub const XVERIFY_UNVERIFIABLE_COUNTER: &str = "tv_dhan_xverify_targets_unverifiable_total";

/// Run time, IST seconds of day.
pub const XVERIFY_RUN_AT_SECS_OF_DAY_IST: u64 = RUN_SECS_OF_DAY_IST as u64;

const _: () = assert!(
    RUN_SECS_OF_DAY_IST > SESSION_CLOSE_SECS_OF_DAY_IST,
    "the comparator must fire AFTER the last minute of the window it compares"
);

const SECS_PER_DAY: u64 = 24 * 3_600;

/// Rows buffered before a mid-run flush. The comparator can emit tens of
/// thousands of findings on a bad day; one unbounded ILP buffer would exceed
/// the client's maximum buffer and never flush.
const PERSIST_BATCH_ROWS: usize = 20_000;

/// Everything the task needs, built once in `main.rs`.
pub struct CrossverifyBootDeps {
    /// QuestDB HTTP `/exec` endpoint (the READ side).
    pub questdb_exec_url: String,
    /// Dhan intraday-candles endpoint.
    pub intraday_url: String,
    /// Comparator knobs.
    pub config: DhanLiveCrossverifyConfig,
    /// QuestDB ILP config (the WRITE side).
    pub questdb: QuestDbConfig,
    /// Trading calendar — the run is skipped on a non-trading day.
    pub calendar: Arc<TradingCalendar>,
}

/// Dhan's `instrument` string for a segment, or `None` when the segment alone
/// cannot determine it.
///
/// F&O returns `None` on purpose: `NSE_FNO` could be `FUTIDX`, `OPTIDX`,
/// `FUTSTK` or `OPTSTK`, and a wrong label fetches nothing while looking like
/// a failed fetch. An unverifiable target is counted, never guessed.
#[must_use]
pub fn dhan_intraday_instrument_for(segment: ExchangeSegment) -> Option<&'static str> {
    match segment {
        ExchangeSegment::IdxI => Some("INDEX"),
        ExchangeSegment::NseEquity | ExchangeSegment::BseEquity => Some("EQUITY"),
        ExchangeSegment::NseFno
        | ExchangeSegment::BseFno
        | ExchangeSegment::NseCurrency
        | ExchangeSegment::BseCurrency
        | ExchangeSegment::McxComm => None,
    }
}

/// Builds the target list from the subscribed main-feed set, so the check can
/// never verify a different universe than the lane captured. Returns the
/// targets and the count that cannot be targeted.
#[must_use]
pub fn crossverify_targets_with_skipped(
    main_feed: &[SubscribeInstrument],
) -> (Vec<XverifyTarget>, usize) {
    let mut targets = Vec::with_capacity(main_feed.len());
    let mut skipped = 0_usize;
    for i in main_feed {
        let (Some(instrument), Ok(security_id)) = (
            dhan_intraday_instrument_for(i.segment),
            i64::try_from(i.security_id),
        ) else {
            skipped = skipped.saturating_add(1);
            continue;
        };
        targets.push(XverifyTarget {
            security_id,
            segment: i.segment.as_str().to_string(),
            instrument: instrument.to_string(),
        });
    }
    (targets, skipped)
}

/// Seconds until the next run, from an IST seconds-of-day. Pure.
#[must_use]
pub const fn secs_until_next_run_ist(now_secs_of_day: u64) -> u64 {
    if now_secs_of_day < XVERIFY_RUN_AT_SECS_OF_DAY_IST {
        XVERIFY_RUN_AT_SECS_OF_DAY_IST - now_secs_of_day
    } else {
        SECS_PER_DAY - now_secs_of_day + XVERIFY_RUN_AT_SECS_OF_DAY_IST
    }
}

/// The run time as `HH:MM` IST, for log lines.
#[must_use]
pub fn run_at_ist_hhmm() -> String {
    let h = XVERIFY_RUN_AT_SECS_OF_DAY_IST / 3_600;
    let m = (XVERIFY_RUN_AT_SECS_OF_DAY_IST % 3_600) / 60;
    format!("{h:02}:{m:02}")
}

/// Whether a process that starts AFTER today's run time should run now.
///
/// A restart between the run time and the evening stop would otherwise sleep
/// until tomorrow and leave today unverified, holding today's S3 archive for
/// the full hold ceiling. Pure.
#[must_use]
pub const fn should_catch_up(now_secs_of_day: u64, marker_exists: bool) -> bool {
    now_secs_of_day >= XVERIFY_RUN_AT_SECS_OF_DAY_IST && !marker_exists
}

/// Whether this run may write the day's marker, which releases the S3
/// archive. Only a MEASURED comparison whose rows landed qualifies. A vacuous
/// run compared nothing, and an unpersisted one left no record to audit. Pure.
#[must_use]
pub fn should_write_marker(cmp: &DayComparison, persisted_ok: bool) -> bool {
    persisted_ok && !cmp.is_vacuous() && cmp.outcome.is_measured()
}

/// The largest share of targets, in percent, whose vendor fetch may fail
/// while the run still counts as complete enough to release the S3 hold.
pub const MAX_MARKER_REST_FAILURE_PERCENT: usize = 5;

/// Whether the run covered enough of the day to release the S3 hold.
///
/// A run cut short by its time budget, or whose live read was truncated, did
/// not look at the whole day. A run where more than
/// [`MAX_MARKER_REST_FAILURE_PERCENT`] of targets failed to fetch did not look
/// at most instruments. Either can still be `Partial`, which is measured, so
/// the outcome alone cannot catch it. Pure, O(1).
#[must_use]
pub const fn run_is_complete(
    budget_elapsed: bool,
    live_truncated: bool,
    rest_failures: usize,
    targets: usize,
) -> bool {
    !budget_elapsed
        && !live_truncated
        && rest_failures.saturating_mul(100)
            <= targets.saturating_mul(MAX_MARKER_REST_FAILURE_PERCENT)
}

/// Whether the divergence page fires: more than half of the compared price
/// fields disagree. Pure.
#[must_use]
pub fn is_catastrophic_divergence(cmp: &DayComparison) -> bool {
    let price_fields = cmp.minutes_compared.saturating_mul(4);
    !cmp.is_vacuous() && price_fields > 0 && cmp.cells_diverged.saturating_mul(2) > price_fields
}

/// Today's IST date and the IST-wall-as-epoch start of that day.
///
/// `and_utc()` on the IST date, deliberately. Both sides of the comparison
/// stamp IST wall-clock as though it were epoch; a true-UTC origin
/// (`and_local_timezone`) skews every minute by 19,800 s and dropped every
/// minute from 10:00 onward in the 2026-08 version of this check.
fn today_ist() -> (chrono::NaiveDate, i64) {
    let today = chrono::Utc::now().with_timezone(&ist_offset()).date_naive();
    let day_start = today
        .and_hms_opt(0, 0, 0)
        .and_then(|dt| dt.and_utc().timestamp_nanos_opt())
        .unwrap_or(0);
    (today, day_start)
}

fn now_ist_secs_of_day() -> u64 {
    let ist = chrono::Utc::now().with_timezone(&ist_offset());
    u64::from(chrono::Timelike::num_seconds_from_midnight(&ist.time()))
}

/// Seconds between token checks while the boot catch-up waits for login.
pub const TOKEN_WAIT_POLL_SECS: u64 = 5;

/// Sleeps between token checks before the run gives up: 60 × 5 s = 5 minutes.
///
/// There are `TOKEN_WAIT_MAX_POLLS + 1` checks (one before the first sleep)
/// and `TOKEN_WAIT_MAX_POLLS` sleeps.
///
/// 2026-09-24: the first live run was a boot catch-up at 18:32 IST. It started
/// before the token manager had loaded a token and failed at once, so the day
/// stayed unverified and its S3 archive stayed held. A normal boot has the
/// token within seconds; five minutes covers a slow mint with room to spare.
pub const TOKEN_WAIT_MAX_POLLS: u32 = 60;

/// Seconds between a failed attempt and the next same-day attempt.
///
/// 2026-09-24: before this, a failed 15:41 run slept until the next day. The
/// day then never got a marker, and its S3 archive waited the full hold
/// ceiling before archiving unverified. A same-day retry gives a transient
/// failure (slow token, QuestDB busy, vendor blip) three more chances.
///
/// 2026-10-06 (§12.15.8): 900 → 760 s, so four worst-case attempts still end
/// by [`XVERIFY_LAST_END_SECS_OF_DAY_IST`] (compile-time asserted below).
pub const XVERIFY_RETRY_INTERVAL_SECS: u64 = 760;

/// The most attempts one trading day gets, the first one included.
pub const XVERIFY_MAX_ATTEMPTS_PER_DAY: u32 = 4;

/// 17:30 IST — when the weekday stop cron fires. Kept for documentation and
/// the ordering assert only: since 2026-10-06 (§12.15.8) no attempt is bounded
/// by it, because the scheduled-stop window opens 5 minutes earlier, at
/// [`SCHEDULED_STOP_WINDOW_START_SECS_OF_DAY_IST`] (17:25), and an attempt
/// allowed to end at 17:30 could be killed half-way.
pub const EVENING_STOP_SECS_OF_DAY_IST: u64 = 17 * 3_600 + 30 * 60;

/// 17:24 IST — one minute before the scheduled-stop window opens. Derived from
/// the `shutdown_class` constant, never a literal (§12.15.8).
pub const XVERIFY_DEADLINE_SECS_OF_DAY_IST: u64 =
    SCHEDULED_STOP_WINDOW_START_SECS_OF_DAY_IST as u64 - 60;

/// 17:23 IST — the latest an attempt or the option pass may end, when it
/// started before the scheduled-stop window (§12.15.8).
pub const XVERIFY_LAST_END_SECS_OF_DAY_IST: u64 = XVERIFY_DEADLINE_SECS_OF_DAY_IST - 60;

/// The smallest run budget a late attempt is shrunk to. Below it the attempt
/// is skipped: a run that short compares too little to record the day.
pub const XVERIFY_MIN_ATTEMPT_BUDGET_SECS: u64 = 120;

/// Room left after the run budget for the audit flush and the marker write.
/// Not a promise the persist fits: the persist stops itself at the attempt's
/// deadline (§12.15.8, [`persist_report_into`]).
const PERSIST_MARGIN_SECS: u64 = 60;

/// The default `run_budget_secs`, mirrored here so the four-attempt fit can be
/// asserted at compile time. Pinned to `DhanLiveCrossverifyConfig::default()`
/// by `test_default_run_budget_mirror_matches_the_config_default`.
const XVERIFY_DEFAULT_RUN_BUDGET_SECS: u64 = 600;

const _: () = assert!(
    XVERIFY_RUN_AT_SECS_OF_DAY_IST
        + attempt_max_secs(XVERIFY_DEFAULT_RUN_BUDGET_SECS)
        + (XVERIFY_MAX_ATTEMPTS_PER_DAY as u64 - 1)
            * (XVERIFY_RETRY_INTERVAL_SECS + attempt_max_secs(XVERIFY_DEFAULT_RUN_BUDGET_SECS))
        <= XVERIFY_LAST_END_SECS_OF_DAY_IST,
    "four worst-case attempts with the default budget must end by 17:23 IST"
);

const _: () = assert!(
    XVERIFY_RUN_AT_SECS_OF_DAY_IST < XVERIFY_LAST_END_SECS_OF_DAY_IST
        && XVERIFY_LAST_END_SECS_OF_DAY_IST < XVERIFY_DEADLINE_SECS_OF_DAY_IST
        && XVERIFY_DEADLINE_SECS_OF_DAY_IST < SCHEDULED_STOP_WINDOW_START_SECS_OF_DAY_IST as u64
        && (SCHEDULED_STOP_WINDOW_START_SECS_OF_DAY_IST as u64) < EVENING_STOP_SECS_OF_DAY_IST
        && EVENING_STOP_SECS_OF_DAY_IST < SCHEDULED_STOP_WINDOW_END_SECS_OF_DAY_IST as u64
        && (SCHEDULED_STOP_WINDOW_END_SECS_OF_DAY_IST as u64) < SECS_PER_DAY,
    "15:41 < 17:23 < 17:24 < 17:25 < 17:30 < 17:45 < midnight"
);

/// Same-day retries, by the reason the previous attempt failed. Local
/// `/metrics` only; the final failure still pages through the existing
/// `xverify_failed` / `xverify_vacuous` log filters.
pub const XVERIFY_RETRIES_COUNTER: &str = "tv_dhan_xverify_retries_total";

/// Why one attempt did not record the day.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttemptFailure {
    /// No unexpired token after the full token wait.
    NoToken,
    /// The run itself returned an error (QuestDB read, request build, ...).
    RunFailed,
    /// The run compared zero minutes.
    Vacuous,
    /// The comparison ran but its audit rows did not land.
    NotPersisted,
    /// The comparison ran but did not cover the day (budget, truncation,
    /// too many vendor fetch failures, or a non-measured outcome).
    Incomplete,
    /// The comparison finished and every audit row was ACKed, but the day
    /// marker could not be saved to disk (2026-10-06, §12.15.7). The next
    /// attempt only writes the marker.
    MarkerNotWritten,
    /// The final flush and the daily row landed, but some cell or tape rows
    /// were discarded by a failed mid-run flush or refused at append
    /// (2026-10-06, §12.15.7). The day is not recorded on an incomplete audit.
    AuditRowsLost,
    /// No attempt ran in this process: the only one was skipped because too
    /// little time was left before 17:23 IST (§12.15.8). Logged, never paged.
    SkippedNoTime,
}

impl AttemptFailure {
    /// Stable label for logs and the retry counter.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NoToken => "no_token",
            Self::RunFailed => "run_failed",
            Self::Vacuous => "vacuous",
            Self::NotPersisted => "not_persisted",
            Self::Incomplete => "incomplete",
            Self::MarkerNotWritten => "marker_not_written",
            Self::AuditRowsLost => "audit_rows_lost",
            Self::SkippedNoTime => "skipped_no_time",
        }
    }
}

/// The longest one attempt can take: the token wait, the run budget, and the
/// persist margin. Pure, O(1).
#[must_use]
pub const fn attempt_max_secs(run_budget_secs: u64) -> u64 {
    TOKEN_WAIT_POLL_SECS
        .saturating_mul(TOKEN_WAIT_MAX_POLLS as u64)
        .saturating_add(run_budget_secs)
        .saturating_add(PERSIST_MARGIN_SECS)
}

/// Seconds to wait before the next same-day attempt, or `None` when there is
/// no next attempt: the day's attempts are used up, or the next attempt could
/// still be running at [`XVERIFY_LAST_END_SECS_OF_DAY_IST`] (17:23 IST; the
/// 17:30 bound before 2026-10-06 was a defect, §12.15.8). Pure, O(1).
#[must_use]
pub const fn retry_delay_secs(
    attempts_made: u32,
    now_secs_of_day: u64,
    attempt_max_secs: u64,
) -> Option<u64> {
    if attempts_made >= XVERIFY_MAX_ATTEMPTS_PER_DAY {
        return None;
    }
    let next_end = now_secs_of_day
        .saturating_add(XVERIFY_RETRY_INTERVAL_SECS)
        .saturating_add(attempt_max_secs);
    if next_end > XVERIFY_LAST_END_SECS_OF_DAY_IST {
        return None;
    }
    Some(XVERIFY_RETRY_INTERVAL_SECS)
}

/// Whether attempt number `attempt_number` (1-based), starting at
/// `start_secs_of_day`, is the day's last (§12.15.8).
///
/// Decided ONCE, before the attempt, from the LATEST end it can have
/// (`start + attempt_max_secs`), and never revised afterwards. An attempt that
/// is not the last therefore always leaves room for the next one, however
/// early it actually ends, and a decision made before the read (plan item
/// 51d) can never disagree with the retry loop. Pure, O(1).
#[must_use]
pub const fn attempt_is_last(
    attempt_number: u32,
    start_secs_of_day: u64,
    attempt_max_secs: u64,
) -> bool {
    retry_delay_secs(
        attempt_number,
        start_secs_of_day.saturating_add(attempt_max_secs),
        attempt_max_secs,
    )
    .is_none()
}

/// The run budget for a full attempt starting at `now_secs_of_day`, or `None`
/// when it should not start (§12.15.8).
///
/// - At or after the scheduled-stop window's end (17:45, a manual evening
///   boot after the weekday stop has fired) and before midnight: the
///   configured budget, unchanged.
/// - Otherwise the configured budget when the whole attempt (token wait,
///   budget, persist margin) ends by [`XVERIFY_LAST_END_SECS_OF_DAY_IST`];
///   else the room left, when that is at least
///   [`XVERIFY_MIN_ATTEMPT_BUDGET_SECS`]; else `None`.
/// - A clock reading of a day or more is nonsense and returns `None`.
///
/// A shrunk run that stops at its budget ends `budget_elapsed`, which
/// [`run_is_complete`] reads as incomplete, so it never writes the marker.
/// Pure, O(1).
#[must_use]
pub const fn attempt_budget_secs(now_secs_of_day: u64, config_budget_secs: u64) -> Option<u64> {
    if now_secs_of_day >= SECS_PER_DAY {
        return None;
    }
    if now_secs_of_day >= SCHEDULED_STOP_WINDOW_END_SECS_OF_DAY_IST as u64 {
        return Some(config_budget_secs);
    }
    // `attempt_max_secs(0)` is the fixed part: token wait plus persist margin.
    let Some(room) = XVERIFY_LAST_END_SECS_OF_DAY_IST
        .checked_sub(now_secs_of_day.saturating_add(attempt_max_secs(0)))
    else {
        return None;
    };
    if config_budget_secs <= room {
        Some(config_budget_secs)
    } else if room >= XVERIFY_MIN_ATTEMPT_BUDGET_SECS {
        Some(room)
    } else {
        None
    }
}

/// Whether one finished attempt recorded the day, and if not, why. The
/// checks run in a fixed order so the reason names the first thing that went
/// wrong: vacuous, then not measured, then the persist verdict (its own
/// variant, from [`persist_verdict`]), then an incomplete run. `Ok` is exactly
/// the condition under which the marker is written. Pure, O(1).
pub fn classify_attempt(
    vacuous: bool,
    measured: bool,
    persist: Result<(), AttemptFailure>,
    complete: bool,
) -> Result<(), AttemptFailure> {
    if vacuous {
        return Err(AttemptFailure::Vacuous);
    }
    if !measured {
        return Err(AttemptFailure::Incomplete);
    }
    persist?;
    if !complete {
        return Err(AttemptFailure::Incomplete);
    }
    Ok(())
}

/// What persisting one run actually did, counted per flush (2026-10-06,
/// §12.15.7).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PersistOutcome {
    /// The final flush was ACKed.
    pub final_flush_ok: bool,
    /// The daily summary row was appended to the buffer.
    pub daily_appended: bool,
    /// Rows a failed flush discarded (mid-run or final).
    pub rows_discarded: usize,
    /// Rows the database ACKed.
    pub rows_flushed: usize,
    /// Cell findings the buffer refused at append.
    pub cell_append_errors: usize,
    /// Vendor tape rows the buffer refused at append.
    pub tape_append_errors: usize,
    /// The persist stopped because its next flush could not end by the
    /// attempt's deadline (§12.15.8). `final_flush_ok` is then `false`.
    pub deadline_reached: bool,
    /// Rows never appended because the persist stopped at the deadline (the
    /// daily row included). Rows already buffered are in `rows_discarded`.
    pub rows_not_written_at_deadline: usize,
}

/// The persist half of an attempt's verdict. `NotPersisted` when the final
/// flush failed or the daily row never reached the buffer; otherwise
/// `AuditRowsLost` when any row was discarded or refused; otherwise `Ok`. A
/// database ACK means accepted into QuestDB's WAL, not applied. Pure, O(1).
pub fn persist_verdict(outcome: &PersistOutcome) -> Result<(), AttemptFailure> {
    if !outcome.final_flush_ok || !outcome.daily_appended {
        return Err(AttemptFailure::NotPersisted);
    }
    if outcome.rows_discarded > 0
        || outcome.cell_append_errors > 0
        || outcome.tape_append_errors > 0
    {
        return Err(AttemptFailure::AuditRowsLost);
    }
    Ok(())
}

/// What the next same-day attempt does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttemptKind {
    /// Token wait, vendor fetch, comparison, persist, marker.
    Full,
    /// Only the marker write: the previous attempt finished and persisted
    /// everything, and failed only to save the marker.
    MarkerOnly,
}

/// The next attempt's kind, from the immediately previous attempt's failure
/// in this process. Marker-only exactly after `MarkerNotWritten`, so a marker
/// is never written on the strength of any other earlier result. Pure, O(1).
#[must_use]
pub fn next_attempt_kind(prev: Option<AttemptFailure>) -> AttemptKind {
    if prev == Some(AttemptFailure::MarkerNotWritten) {
        AttemptKind::MarkerOnly
    } else {
        AttemptKind::Full
    }
}

/// Calls `read` until it returns `Some`, sleeping `poll_secs` between calls,
/// at most `max_polls` sleeps. Returns the value and the number of sleeps
/// taken. O(1) per call, bounded in total. Generic so the bound is testable
/// against a paused clock without a live token manager.
pub async fn poll_until<T>(
    mut read: impl FnMut() -> Option<T>,
    poll_secs: u64,
    max_polls: u32,
) -> Option<(T, u32)> {
    for poll in 0..=max_polls {
        if let Some(value) = read() {
            return Some((value, poll));
        }
        if poll < max_polls {
            tokio::time::sleep(Duration::from_secs(poll_secs)).await;
        }
    }
    None
}

/// Waits for the token manager to hold an unexpired token.
async fn wait_for_jwt() -> Option<SecretString> {
    let (jwt, polls) = poll_until(current_jwt, TOKEN_WAIT_POLL_SECS, TOKEN_WAIT_MAX_POLLS).await?;
    if polls > 0 {
        info!(
            waited_secs = TOKEN_WAIT_POLL_SECS * u64::from(polls),
            "Dhan 1-minute cross-verification: token became available"
        );
    }
    Some(jwt)
}

/// The current token, or `None` if there is none or it has expired. An expired
/// token loaded from the crash cache counts as absent, so the wait continues
/// until the fresh mint lands instead of sending a dead token to Dhan.
fn current_jwt() -> Option<SecretString> {
    let manager = global_token_manager()?;
    let guard = manager.token_handle().load();
    guard
        .as_ref()
        .as_ref()
        .filter(|state| state.is_valid())
        .map(|state| SecretString::from(state.access_token().expose_secret().to_string()))
}

/// Spawns the daily cross-verification task for the subscribed universe.
// TEST-EXEMPT: spawns a tokio task that waits for 15:41 IST and calls the vendor; its pure decisions (targets, schedule, catch-up, marker, divergence, same-day retry bound, attempt classification) are tested above and its emit contract by test_every_xverify_alarm_source_has_a_live_error_emit
pub fn spawn_dhan_live_crossverify(
    deps: CrossverifyBootDeps,
    main_feed: &[SubscribeInstrument],
) -> tokio::task::JoinHandle<()> {
    let (targets, skipped) = crossverify_targets_with_skipped(main_feed);
    if skipped > 0 {
        metrics::counter!(XVERIFY_UNVERIFIABLE_COUNTER).increment(skipped as u64);
        warn!(
            skipped,
            targeted = targets.len(),
            "cross-verification cannot target every subscribed instrument: an F&O \
             contract's Dhan instrument type is not derivable from its segment alone. \
             These instruments are CAPTURED but UNVERIFIED."
        );
    }
    tokio::spawn(async move {
        info!(
            targets = targets.len(),
            run_at_ist = %run_at_ist_hhmm(),
            "Dhan 1-minute cross-verification armed — it compares our 1-minute candles \
             against Dhan's own record after the close, and today's S3 archive waits for it"
        );
        let mut first = true;
        loop {
            let (today, _) = today_ist();
            let catch_up = first
                && should_catch_up(
                    now_ist_secs_of_day(),
                    daily_marker_exists(CROSSVERIFY_MARKER_TASK, today),
                );
            first = false;
            if !catch_up {
                let sleep_secs = secs_until_next_run_ist(now_ist_secs_of_day());
                tokio::time::sleep(Duration::from_secs(sleep_secs)).await;
            }
            let (today, day_start_ist_nanos) = today_ist();
            if !deps.calendar.is_trading_day(today) {
                info!(%today, "Dhan 1-minute cross-verification skipped — not a trading day");
                continue;
            }
            if daily_marker_exists(CROSSVERIFY_MARKER_TASK, today) {
                info!(%today, "Dhan 1-minute cross-verification already recorded for today");
                continue;
            }
            run_day(&deps, &targets, today, day_start_ist_nanos).await;
        }
    })
}

/// One attempt as the day loop plans it, before it starts (§12.15.8).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AttemptPlan {
    /// 1-based attempt number.
    number: u32,
    /// IST seconds of day when the attempt starts.
    start_secs_of_day: u64,
    /// Full run or marker-only (§12.15.7).
    kind: AttemptKind,
    /// The run budget of a full attempt, from [`attempt_budget_secs`]; `0`
    /// for a marker-only attempt.
    run_budget_secs: u64,
    /// Decided once, before the attempt, by [`attempt_is_last`].
    is_last: bool,
    /// When a full attempt's timeout fires: its start plus
    /// [`attempt_max_secs`] of its budget. The audit persist, which runs
    /// synchronously after the last `.await` and so cannot be cut by the
    /// timeout, stops itself here (§12.15.8). `None` for a marker-only
    /// attempt, which is one file write.
    deadline: Option<tokio::time::Instant>,
}

/// How the day loop ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DayResult {
    /// Attempts made, the first included.
    attempts: u32,
    /// `None` when an attempt recorded the day; otherwise the last failure.
    failure: Option<AttemptFailure>,
    /// The IST day changed during a retry sleep; nothing more is done today.
    day_changed: bool,
}

/// Runs `attempt` under `tokio::time::timeout_at(deadline)` (§12.15.8), where
/// `deadline` is `limit_secs` after the attempt started. An elapsed timeout is
/// an incomplete attempt: coded `warn!`, no marker, no page. The timeout can
/// stop an attempt only at an `.await`; the synchronous audit persist after
/// the last `.await` is bounded by the same `deadline` itself
/// ([`persist_report_into`]), so it never runs past it.
async fn bounded_attempt<F>(
    today: chrono::NaiveDate,
    attempt_number: u32,
    limit_secs: u64,
    deadline: tokio::time::Instant,
    attempt: F,
) -> Result<(), AttemptFailure>
where
    F: Future<Output = Result<(), AttemptFailure>>,
{
    match tokio::time::timeout_at(deadline, attempt).await {
        Ok(outcome) => outcome,
        Err(_elapsed) => {
            warn!(
                code = ErrorCode::WsGapConnectionState.code_str(),
                source = "xverify_attempt_timed_out",
                %today,
                attempt = attempt_number,
                limit_secs,
                last_end_ist_secs = XVERIFY_LAST_END_SECS_OF_DAY_IST,
                "Dhan 1-minute cross-verification attempt did not finish within its time \
                 limit and was stopped, so it ends before the evening stop; this attempt \
                 does not record today"
            );
            Err(AttemptFailure::Incomplete)
        }
    }
}

/// The same-day retry loop, with the clock, the day check and the attempt
/// injected so every permutation is testable against a paused clock.
///
/// Before each attempt it decides, once, whether the attempt is the last
/// ([`attempt_is_last`]) and, for a full attempt, its budget
/// ([`attempt_budget_secs`]); a full attempt runs under [`bounded_attempt`].
/// After a failed attempt the SAME `is_last` decides: the last returns the
/// failure for the one page, any other sleeps
/// [`XVERIFY_RETRY_INTERVAL_SECS`]. A marker-only attempt (§12.15.7) is one
/// synchronous file write: it gets the same `is_last` rule and no budget, and
/// runs under no timeout.
///
/// A full attempt with too little time left is skipped, never started. When
/// no attempt ran before it in this process, the day ends
/// [`AttemptFailure::SkippedNoTime`], which `run_day` logs and does not page:
/// a restart late in the window must not page a second time for a day an
/// earlier process already paged, and a page must not fire for a comparison
/// that never ran. When an earlier attempt in this process did run and fail,
/// the day ends with THAT failure, so it still pages (§12.15.8).
/// O(1) per attempt, at most [`XVERIFY_MAX_ATTEMPTS_PER_DAY`] attempts.
async fn drive_day<N, S, A, Fut>(
    today: chrono::NaiveDate,
    config_budget_secs: u64,
    mut now_secs_of_day: N,
    mut still_today: S,
    mut attempt: A,
) -> DayResult
where
    N: FnMut() -> u64,
    S: FnMut() -> bool,
    A: FnMut(AttemptPlan) -> Fut,
    Fut: Future<Output = Result<(), AttemptFailure>>,
{
    let max_attempt_secs = attempt_max_secs(config_budget_secs);
    let mut attempts: u32 = 0;
    let mut previous: Option<AttemptFailure> = None;
    loop {
        attempts = attempts.saturating_add(1);
        let start = now_secs_of_day();
        // §12.15.8: decided once, before the attempt, from the latest end it
        // can have; never revised from the attempt's actual end.
        let is_last = attempt_is_last(attempts, start, max_attempt_secs);
        let outcome = match next_attempt_kind(previous) {
            AttemptKind::MarkerOnly => {
                attempt(AttemptPlan {
                    number: attempts,
                    start_secs_of_day: start,
                    kind: AttemptKind::MarkerOnly,
                    run_budget_secs: 0,
                    is_last,
                    deadline: None,
                })
                .await
            }
            AttemptKind::Full => match attempt_budget_secs(start, config_budget_secs) {
                Some(budget) => {
                    // One instant for the timeout and the persist's own stop.
                    let limit_secs = attempt_max_secs(budget);
                    let deadline = tokio::time::Instant::now() + Duration::from_secs(limit_secs);
                    let plan = AttemptPlan {
                        number: attempts,
                        start_secs_of_day: start,
                        kind: AttemptKind::Full,
                        run_budget_secs: budget,
                        is_last,
                        deadline: Some(deadline),
                    };
                    bounded_attempt(today, attempts, limit_secs, deadline, attempt(plan)).await
                }
                None => {
                    warn!(
                        code = ErrorCode::WsGapConnectionState.code_str(),
                        source = "xverify_attempt_skipped_no_time",
                        %today,
                        attempt = attempts,
                        start_ist_secs = start,
                        last_end_ist_secs = XVERIFY_LAST_END_SECS_OF_DAY_IST,
                        config_budget_secs,
                        "Dhan 1-minute cross-verification attempt skipped: too little time \
                         left to run it before the evening stop; this attempt does not \
                         record today"
                    );
                    // §12.15.8: an earlier failure in this process still
                    // pages; a day on which nothing ran does not.
                    Err(previous.unwrap_or(AttemptFailure::SkippedNoTime))
                }
            },
        };
        let failure = match outcome {
            Ok(()) => {
                if attempts > 1 {
                    info!(%today, attempts, "Dhan 1-minute cross-verification recorded on a same-day retry");
                }
                return DayResult {
                    attempts,
                    failure: None,
                    day_changed: false,
                };
            }
            Err(failure) => failure,
        };
        previous = Some(failure);
        if is_last {
            return DayResult {
                attempts,
                failure: Some(failure),
                day_changed: false,
            };
        }
        metrics::counter!(XVERIFY_RETRIES_COUNTER, "reason" => failure.as_str()).increment(1);
        warn!(
            code = ErrorCode::WsGapConnectionState.code_str(),
            source = "xverify_retry",
            %today,
            reason = failure.as_str(),
            attempt = attempts,
            max_attempts = XVERIFY_MAX_ATTEMPTS_PER_DAY,
            retry_in_secs = XVERIFY_RETRY_INTERVAL_SECS,
            "Dhan 1-minute cross-verification did not record today — retrying later today"
        );
        tokio::time::sleep(Duration::from_secs(XVERIFY_RETRY_INTERVAL_SECS)).await;
        // The day can only change here if the process ran past midnight,
        // which the end bound rules out for an attempt that is not the last;
        // checked anyway so a retry can never verify the wrong day.
        if !still_today() {
            return DayResult {
                attempts,
                failure: Some(failure),
                day_changed: true,
            };
        }
    }
}

/// Runs today's check, retrying on the same day until it records the day or
/// the retry window closes, then the option pass. Each attempt is bounded by
/// its timeout and ends by 17:23 IST when it starts before the scheduled-stop
/// window (§12.15.8); the number of attempts is bounded by
/// [`XVERIFY_MAX_ATTEMPTS_PER_DAY`] and by that end bound.
async fn run_day(
    deps: &CrossverifyBootDeps,
    targets: &[XverifyTarget],
    today: chrono::NaiveDate,
    day_start_ist_nanos: i64,
) {
    let divergence_paged = AtomicBool::new(false);
    let paged = &divergence_paged;
    let result = drive_day(
        today,
        deps.config.run_budget_secs,
        now_ist_secs_of_day,
        || today_ist().0 == today,
        |plan: AttemptPlan| async move {
            match plan.kind {
                AttemptKind::MarkerOnly => {
                    // §12.15.7: the previous attempt compared and persisted
                    // everything and failed only to save the marker.
                    // Re-running the check would re-fetch the vendor tape for
                    // nothing, so this attempt only writes the marker.
                    record_day(today)
                }
                AttemptKind::Full => {
                    // §12.15.8: the budget may be shrunk for a late attempt.
                    let cfg = DhanLiveCrossverifyConfig {
                        run_budget_secs: plan.run_budget_secs,
                        ..deps.config
                    };
                    // A full plan always carries its deadline; `now` (stop at
                    // once, no marker) is the fail-safe if it ever did not.
                    let deadline = plan.deadline.unwrap_or_else(tokio::time::Instant::now);
                    let day = (today, day_start_ist_nanos);
                    run_once(deps, &cfg, targets, day, paged, deadline).await
                }
            }
        },
    )
    .await;
    if result.day_changed {
        return;
    }
    if let Some(failure) = result.failure {
        report_final_failure(failure, today, result.attempts, targets.len());
    }
    // §12.15.6: the depth-held option pass runs ONCE, after the spot check's
    // outcome is final. It never writes the day marker and never pages.
    // §12.15.8: it runs under its own timeout, so it too ends by 17:23.
    // The persist, which the timeout cannot cut, stops itself at the same
    // instant.
    let option_limit_secs = attempt_max_secs(XVERIFY_OPTION_PASS_BUDGET_SECS);
    let option_deadline = tokio::time::Instant::now() + Duration::from_secs(option_limit_secs);
    let option_pass = tokio::time::timeout_at(
        option_deadline,
        run_option_pass(deps, today, day_start_ist_nanos, option_deadline),
    )
    .await;
    if option_pass.is_err() {
        metrics::counter!(XVERIFY_OPTION_PASS_COUNTER, "outcome" => "timed_out").increment(1);
        warn!(
            code = ErrorCode::WsGapConnectionState.code_str(),
            source = "xverify_options_timed_out",
            %today,
            limit_secs = option_limit_secs,
            "Dhan option cross-check did not finish within its time limit and was stopped, \
             so it ends before the evening stop"
        );
    }
}

/// Pages once, after the last attempt of the day. Each arm is its own
/// `error!` so every alarmed `source` stays a literal the alarm filter can
/// match. The one exception is [`AttemptFailure::SkippedNoTime`]: no attempt
/// ran in this process, so there is nothing new to page about; it is a coded
/// `warn!` on a source no alarm filter matches (§12.15.8).
fn report_final_failure(
    failure: AttemptFailure,
    today: chrono::NaiveDate,
    attempts: u32,
    targets: usize,
) {
    let reason = failure.as_str();
    match failure {
        AttemptFailure::SkippedNoTime => warn!(
            code = ErrorCode::WsGapConnectionState.code_str(),
            source = "xverify_day_not_attempted",
            %today,
            attempts,
            targets,
            reason,
            last_end_ist_secs = XVERIFY_LAST_END_SECS_OF_DAY_IST,
            "Dhan 1-minute cross-verification did not run today in this process: it \
             started too late to finish before the evening stop. Today's candles are \
             UNVERIFIED and today's S3 archive stays held. Not paged: no check ran here, \
             and an earlier process that ran one has already paged"
        ),
        AttemptFailure::Vacuous => error!(
            code = ErrorCode::WsGapConnectionState.code_str(),
            source = "xverify_vacuous",
            %today,
            attempts,
            targets,
            reason,
            "Dhan 1-minute cross-verification compared ZERO minutes on every attempt \
             today — today's candles are UNVERIFIED and today's S3 archive stays held. \
             This is not a pass; it is no measurement at all."
        ),
        // §12.15.7: the same alarmed source; its meaning ("did not record
        // today after every attempt") is exactly what this day is.
        AttemptFailure::MarkerNotWritten => error!(
            code = ErrorCode::WsGapConnectionState.code_str(),
            source = "xverify_failed",
            %today,
            attempts,
            targets,
            reason,
            path = %daily_marker_path(CROSSVERIFY_MARKER_TASK, today).display(),
            "Dhan 1-minute cross-verification: comparison finished and every row was \
             accepted by the database, but the day marker could not be saved to disk; \
             today's S3 archive stays held; check disk space and permissions on the \
             state folder"
        ),
        AttemptFailure::NoToken
        | AttemptFailure::RunFailed
        | AttemptFailure::NotPersisted
        | AttemptFailure::Incomplete
        | AttemptFailure::AuditRowsLost => error!(
            code = ErrorCode::WsGapConnectionState.code_str(),
            source = "xverify_failed",
            %today,
            attempts,
            targets,
            reason,
            "Dhan 1-minute cross-verification did not record today after every \
             same-day attempt — today's candles are UNVERIFIED and today's S3 archive \
             stays held"
        ),
    }
}

/// One attempt. Returns `Ok` only when the day's marker was written.
///
/// Per-attempt problems log at `warn!` with sources no alarm filter matches;
/// the page fires once, from [`report_final_failure`], after the last attempt.
/// The divergence page is the exception: it is a finding about the data, not
/// about the attempt, so it fires on the first attempt that measures it and
/// `divergence_paged` stops a retry from paging it again.
///
/// `cfg` is the comparator config with this attempt's run budget, which
/// [`attempt_budget_secs`] may have shrunk (§12.15.8). `divergence_paged` is
/// an `AtomicBool` because each attempt is a separate future of the injected
/// day loop; one `swap` per catastrophic attempt, cold. `deadline` is the
/// instant this attempt's timeout fires: the audit persist, which runs
/// synchronously after the last `.await` and so cannot be cut by the timeout,
/// stops itself there, and the marker is not written past it (§12.15.8).
async fn run_once(
    deps: &CrossverifyBootDeps,
    cfg: &DhanLiveCrossverifyConfig,
    targets: &[XverifyTarget],
    (today, day_start_ist_nanos): (chrono::NaiveDate, i64),
    divergence_paged: &AtomicBool,
    deadline: tokio::time::Instant,
) -> Result<(), AttemptFailure> {
    let Some(jwt) = wait_for_jwt().await else {
        metrics::counter!(XVERIFY_RUNS_COUNTER, "outcome" => "no_token").increment(1);
        warn!(
            code = ErrorCode::WsGapConnectionState.code_str(),
            source = "xverify_attempt_no_token",
            waited_secs = TOKEN_WAIT_POLL_SECS * u64::from(TOKEN_WAIT_MAX_POLLS),
            "Dhan 1-minute cross-verification attempt could not run: no Dhan token \
             available"
        );
        return Err(AttemptFailure::NoToken);
    };
    let client = reqwest::Client::new();
    let result = run_cross_verification(
        &client,
        &deps.questdb_exec_url,
        &deps.intraday_url,
        jwt.expose_secret(),
        targets,
        today,
        day_start_ist_nanos,
        cfg,
    )
    .await;
    drop(jwt);
    match result {
        Ok(report) => {
            let c = &report.comparison;
            let label = if c.is_vacuous() {
                "vacuous"
            } else {
                "measured"
            };
            metrics::counter!(XVERIFY_RUNS_COUNTER, "outcome" => label).increment(1);
            info!(
                targets = targets.len(),
                outcome = c.outcome.as_str(),
                instruments = c.instruments,
                minutes_compared = c.minutes_compared,
                cells_diverged = c.cells_diverged,
                missing_live = c.missing_live,
                missing_live_traded = c.missing_live_traded,
                missing_rest = c.missing_rest,
                tail_unsealed = c.tail_unsealed,
                out_of_session = c.out_of_session,
                noise_p50_paise = c.noise_p50_paise,
                noise_p95_paise = c.noise_p95_paise,
                noise_max_paise = c.noise_max_paise,
                volume_cells = c.volume_cells,
                volume_exact = c.volume_exact,
                findings = c.findings.len(),
                rest_failures = report.rest_failures,
                rest_failure_reasons = %report.rest_failure_breakdown.summary(),
                malformed_rows = report.malformed_rows,
                budget_elapsed = report.budget_elapsed,
                live_truncated = report.live_truncated,
                degraded = report.degraded,
                vacuous = c.is_vacuous(),
                "Dhan 1-minute cross-verification finished"
            );
            let persisted = persist_report(
                &deps.questdb,
                &report,
                day_start_ist_nanos,
                cfg.tolerance_paise,
                deadline,
            );
            let persist = persist_verdict(&persisted);
            let persisted_ok = persist.is_ok();
            if is_catastrophic_divergence(c) && !divergence_paged.swap(true, Ordering::Relaxed) {
                metrics::counter!(XVERIFY_RUNS_COUNTER, "outcome" => "diverged").increment(1);
                error!(
                    code = ErrorCode::WsGapConnectionState.code_str(),
                    source = "xverify_diverged",
                    instruments = c.instruments,
                    minutes_compared = c.minutes_compared,
                    price_fields_compared = c.minutes_compared.saturating_mul(4),
                    cells_diverged = c.cells_diverged,
                    noise_p95_paise = c.noise_p95_paise,
                    noise_max_paise = c.noise_max_paise,
                    "Dhan 1-minute cross-verification found MORE THAN HALF of the compared \
                     price fields disagreeing with Dhan's own record. Treat today's candles \
                     as untrustworthy until this is explained."
                );
            }
            let complete = run_is_complete(
                report.budget_elapsed,
                report.live_truncated,
                report.rest_failures,
                targets.len(),
            );
            let verdict =
                classify_attempt(c.is_vacuous(), c.outcome.is_measured(), persist, complete);
            // The marker condition must stay exactly `should_write_marker`
            // plus a complete run; the two pure functions agree by test.
            debug_assert_eq!(
                verdict.is_ok(),
                should_write_marker(c, persisted_ok) && complete
            );
            match verdict {
                // §12.15.8: a persist that finished never started a flush it
                // could not end by the deadline, so this is a belt: past the
                // deadline the marker is not written and the attempt ends.
                Ok(()) if tokio::time::Instant::now() > deadline => {
                    warn!(
                        code = ErrorCode::WsGapConnectionState.code_str(),
                        source = "xverify_attempt_timed_out",
                        %today,
                        stage = "marker",
                        last_end_ist_secs = XVERIFY_LAST_END_SECS_OF_DAY_IST,
                        "Dhan 1-minute cross-verification attempt reached its time limit \
                         before writing the day marker, so it ends before the evening \
                         stop; this attempt does not record today"
                    );
                    Err(AttemptFailure::Incomplete)
                }
                Ok(()) => record_day(today),
                Err(failure) => {
                    warn!(
                        code = ErrorCode::WsGapConnectionState.code_str(),
                        source = "xverify_attempt_unrecorded",
                        reason = failure.as_str(),
                        targets = targets.len(),
                        minutes_compared = c.minutes_compared,
                        missing_live = c.missing_live,
                        missing_rest = c.missing_rest,
                        rest_failures = report.rest_failures,
                        budget_elapsed = report.budget_elapsed,
                        live_truncated = report.live_truncated,
                        persisted_ok,
                        rows_discarded = persisted.rows_discarded,
                        cell_append_errors = persisted.cell_append_errors,
                        tape_append_errors = persisted.tape_append_errors,
                        "Dhan 1-minute cross-verification attempt did not record today"
                    );
                    Err(failure)
                }
            }
        }
        Err(err) => {
            metrics::counter!(XVERIFY_RUNS_COUNTER, "outcome" => "failed").increment(1);
            warn!(
                code = ErrorCode::WsGapConnectionState.code_str(),
                source = "xverify_attempt_failed",
                %err,
                "Dhan 1-minute cross-verification attempt FAILED to run"
            );
            Err(AttemptFailure::RunFailed)
        }
    }
}

/// Writes today's marker: the ONLY marker-write site of the cross-verification
/// (2026-10-06, §12.15.7). Called from `run_once`'s `Ok` arm and from the
/// marker-only retry in `run_day`.
///
/// A folder-sync failure after the rename still counts as recorded: the marker
/// is in place and the strict reader accepts it, so the hold really is
/// released. A write error fails the attempt with `MarkerNotWritten`.
fn record_day(today: chrono::NaiveDate) -> Result<(), AttemptFailure> {
    match try_write_daily_marker_keeping(
        CROSSVERIFY_MARKER_TASK,
        today,
        CROSSVERIFY_MARKER_KEEP_DAYS,
    ) {
        Ok(MarkerDurability::Durable) => {
            info!(%today, "Dhan 1-minute cross-verification recorded — today's S3 archive may proceed");
            Ok(())
        }
        Ok(MarkerDurability::RenamedNotDirSynced) => {
            warn!(
                code = ErrorCode::WsGapConnectionState.code_str(),
                source = "xverify_marker_dir_sync_failed",
                %today,
                path = %daily_marker_path(CROSSVERIFY_MARKER_TASK, today).display(),
                "Dhan 1-minute cross-verification marker was saved, but its folder could \
                 not be synced to disk; the day is recorded, a host crash could lose it"
            );
            info!(%today, "Dhan 1-minute cross-verification recorded — today's S3 archive may proceed");
            Ok(())
        }
        Err(err) => {
            warn!(
                code = ErrorCode::WsGapConnectionState.code_str(),
                source = "xverify_attempt_marker_write_failed",
                %today,
                ?err,
                path = %daily_marker_path(CROSSVERIFY_MARKER_TASK, today).display(),
                "Dhan 1-minute cross-verification finished and persisted, but the day \
                 marker could not be saved; the next attempt only writes the marker"
            );
            Err(AttemptFailure::MarkerNotWritten)
        }
    }
}

/// Whether a flush of the writer's current buffer, started now, ends by
/// `deadline` even in the ILP client's worst case (§12.15.8). O(1).
fn flush_fits(
    writer: &DhanLiveXverifyAuditWriter,
    now: tokio::time::Instant,
    deadline: tokio::time::Instant,
) -> bool {
    now.checked_add(writer.flush_worst_case())
        .is_some_and(|end| end <= deadline)
}

/// Persists the run through the production writer, in batches of
/// [`PERSIST_BATCH_ROWS`], stopping at `deadline`. See [`persist_report_into`].
fn persist_report(
    questdb: &QuestDbConfig,
    report: &RunReport,
    day_start_ist_nanos: i64,
    tolerance_paise: i64,
    deadline: tokio::time::Instant,
) -> PersistOutcome {
    let mut writer = DhanLiveXverifyAuditWriter::new(questdb);
    persist_report_into(
        &mut writer,
        report,
        day_start_ist_nanos,
        tolerance_paise,
        PERSIST_BATCH_ROWS,
        deadline,
        tokio::time::Instant::now,
    )
}

/// Appends every finding, every vendor tape row and the daily row, flushing
/// every `batch_rows` rows, and counts what each flush did. A failed flush
/// discards its pending rows (the writer's poisoned-buffer defence), so those
/// rows are counted as discarded, never as persisted (2026-10-06, §12.15.7).
///
/// §12.15.8: the persist runs synchronously after the attempt's last
/// `.await`, so the attempt's timeout cannot stop it. It bounds itself:
/// before each row it checks that a flush of the buffer, started now, would
/// end by `deadline` in the ILP client's worst case
/// ([`DhanLiveXverifyAuditWriter::flush_worst_case`]). When it would not, the
/// persist stops: the buffered rows are discarded and counted, the rows not
/// yet appended are counted, and the outcome is `NotPersisted`, so no marker
/// is written. The check runs per row, not only before a flush, because a
/// buffer that cannot be flushed in time now cannot be later either: the
/// clock only advances and the final flush is still owed. So every flush this
/// starts ends by `deadline`.
/// O(findings + tape rows), once per attempt, cold.
fn persist_report_into(
    writer: &mut DhanLiveXverifyAuditWriter,
    report: &RunReport,
    day_start_ist_nanos: i64,
    tolerance_paise: i64,
    batch_rows: usize,
    deadline: tokio::time::Instant,
    mut now: impl FnMut() -> tokio::time::Instant,
) -> PersistOutcome {
    let c = &report.comparison;
    let mut out = PersistOutcome::default();
    let mut batch_errors = 0_usize;
    // Counts the rows a flush took with it: on `Err` they were discarded; on
    // `Ok` with an empty buffer they were ACKed. An `Ok` that left rows
    // pending flushed nothing.
    let account = |out: &mut PersistOutcome,
                   errs: &mut usize,
                   before: usize,
                   flushed: anyhow::Result<()>,
                   after: usize| {
        match flushed {
            Err(_) => {
                out.rows_discarded = out.rows_discarded.saturating_add(before);
                *errs = errs.saturating_add(1);
            }
            Ok(()) if after == 0 => {
                out.rows_flushed = out.rows_flushed.saturating_add(before);
            }
            Ok(()) => {}
        }
    };
    let flush_if_full =
        |w: &mut DhanLiveXverifyAuditWriter, out: &mut PersistOutcome, errs: &mut usize| {
            let before = w.pending();
            let flushed = if before >= batch_rows {
                tickvault_storage::off_worker::off_worker(|| w.flush())
            } else {
                tickvault_storage::off_worker::off_worker(|| w.flush_if_large())
            };
            account(out, errs, before, flushed, w.pending());
        };
    // §12.15.8: stop when the buffer could no longer be flushed by the
    // deadline. `remaining` counts the rows not yet appended, the daily row
    // included.
    let stop_at_deadline =
        |w: &mut DhanLiveXverifyAuditWriter, out: &mut PersistOutcome, remaining: usize| {
            out.rows_discarded = out.rows_discarded.saturating_add(w.discard_pending());
            out.rows_not_written_at_deadline = remaining;
            out.deadline_reached = true;
            out.final_flush_ok = false;
        };
    let total = c
        .findings
        .len()
        .saturating_add(report.rest_tape.len())
        .saturating_add(1);
    let mut appended = 0_usize;

    for finding in &c.findings {
        if !flush_fits(writer, now(), deadline) {
            stop_at_deadline(writer, &mut out, total - appended);
            return finish_persist(report, out, batch_errors, None);
        }
        if writer.append_cell(finding).is_err() {
            out.cell_append_errors = out.cell_append_errors.saturating_add(1);
        }
        appended += 1;
        flush_if_full(writer, &mut out, &mut batch_errors);
    }
    for row in &report.rest_tape {
        if !flush_fits(writer, now(), deadline) {
            stop_at_deadline(writer, &mut out, total - appended);
            return finish_persist(report, out, batch_errors, None);
        }
        if writer.append_rest_tape(row).is_err() {
            out.tape_append_errors = out.tape_append_errors.saturating_add(1);
        }
        appended += 1;
        flush_if_full(writer, &mut out, &mut batch_errors);
    }
    let daily = daily_row(
        c,
        day_start_ist_nanos,
        deterministic_run_ts_nanos(day_start_ist_nanos),
        tolerance_paise,
    );
    out.daily_appended = writer.append_daily(&daily).is_ok();
    if !flush_fits(writer, now(), deadline) {
        out.daily_appended = false;
        stop_at_deadline(writer, &mut out, 0);
        return finish_persist(report, out, batch_errors, None);
    }

    let before = writer.pending();
    let final_flush = tickvault_storage::off_worker::off_worker(|| writer.flush());
    out.final_flush_ok = final_flush.is_ok();
    let final_err = final_flush.as_ref().err().map(|e| format!("{e:#}"));
    account(
        &mut out,
        &mut batch_errors,
        before,
        final_flush,
        writer.pending(),
    );
    if final_err.is_some() {
        // `flush` already discarded on `Err` (poisoned-buffer defence);
        // this keeps the buffer empty even if that ever changes. The rows
        // were counted as discarded above, so nothing is added here.
        let _already_empty = writer.discard_pending();
    }
    finish_persist(report, out, batch_errors, final_err)
}

/// Publishes the persist's counters and its one log line. Split out of
/// [`persist_report_into`] so the deadline stop and the normal end log the
/// same way. O(1).
fn finish_persist(
    report: &RunReport,
    out: PersistOutcome,
    batch_errors: usize,
    final_err: Option<String>,
) -> PersistOutcome {
    let c = &report.comparison;
    metrics::counter!(XVERIFY_PERSIST_ROWS_COUNTER).increment(out.rows_flushed as u64);
    if out.deadline_reached {
        metrics::counter!(XVERIFY_PERSIST_ERRORS_COUNTER).increment(1);
        warn!(
            code = ErrorCode::WsGapConnectionState.code_str(),
            source = "xverify_persist_stopped_at_deadline",
            rows_flushed = out.rows_flushed,
            rows_discarded = out.rows_discarded,
            rows_not_written = out.rows_not_written_at_deadline,
            findings = c.findings.len(),
            tape_rows = report.rest_tape.len(),
            last_end_ist_secs = XVERIFY_LAST_END_SECS_OF_DAY_IST,
            "Dhan 1-minute cross-verification stopped saving its audit rows: the next \
             write could not finish within the attempt's time limit, so the attempt ends \
             before the evening stop; this attempt does not record today and today's S3 \
             archive stays held"
        );
        return out;
    }
    match final_err {
        None => {
            if out.cell_append_errors > 0
                || out.tape_append_errors > 0
                || batch_errors > 0
                || !out.daily_appended
            {
                error!(
                    code = ErrorCode::WsGapConnectionState.code_str(),
                    source = "xverify_persist_partial",
                    cell_errors = out.cell_append_errors,
                    tape_errors = out.tape_append_errors,
                    batch_errors,
                    rows_discarded = out.rows_discarded,
                    daily_failed = !out.daily_appended,
                    findings = c.findings.len(),
                    tape_rows = report.rest_tape.len(),
                    "Dhan 1-minute cross-verification persisted with gaps — the audit \
                     tables are incomplete for today, so today's S3 archive stays held; \
                     this attempt fails and is retried only if the day's window allows \
                     (see xverify_retry / xverify_failed)"
                );
            }
        }
        Some(err) => {
            metrics::counter!(XVERIFY_PERSIST_ERRORS_COUNTER).increment(1);
            error!(
                code = ErrorCode::WsGapConnectionState.code_str(),
                source = "xverify_persist_failed",
                %err,
                rows_discarded = out.rows_discarded,
                "Dhan 1-minute cross-verification could NOT be persisted — today's \
                 comparison exists only in this log stream; today's S3 archive stays \
                 held; this attempt fails and is retried only if the day's window \
                 allows (see xverify_retry / xverify_failed)"
            );
        }
    }
    out
}

// ── §12.15.6 — the day's depth-held OPTION contracts, checked separately ──

/// Most option contracts one day's option pass compares. Taken in
/// `security_id` order; the rest are counted as truncated, never guessed at.
pub const XVERIFY_MAX_OPTION_TARGETS: usize = 300;

/// Run budget of the option pass. 300 contracts at the 334 ms pacer is about
/// 100 s; the budget leaves room for the live-candle query.
pub const XVERIFY_OPTION_PASS_BUDGET_SECS: u64 = 150;

/// Option-pass outcomes, labelled by `outcome`. Local `/metrics` only — no EMF
/// name and no alarm (§12.15.6).
pub const XVERIFY_OPTION_PASS_COUNTER: &str = "tv_dhan_xverify_option_pass_total";

/// The option pass's targets and what was left out.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct OptionTargets {
    /// Contracts the pass will compare, sorted by `security_id`.
    pub targets: Vec<XverifyTarget>,
    /// Held contracts the contract map could not classify. Skipped, never
    /// guessed: a wrong Dhan instrument type returns another contract's tape.
    pub unresolved: usize,
    /// Contracts beyond [`XVERIFY_MAX_OPTION_TARGETS`].
    pub truncated: usize,
    /// Held keys on a segment other than `NSE_FNO`.
    pub not_fno: usize,
}

/// Builds the option pass's targets from the day's depth-held keys.
///
/// `family_of` answers the contract's option family from the daily master;
/// `OptionFamily::Index` becomes `OPTIDX`, `OptionFamily::Stock` becomes
/// `OPTSTK`. O(held · log held) for the sort, once a day, cold.
pub fn option_targets_from_depth_held<F>(
    held: &[(u64, u8)],
    family_of: F,
    cap: usize,
) -> OptionTargets
where
    F: Fn(u64, ExchangeSegment) -> Option<OptionFamily>,
{
    let fno = ExchangeSegment::NseFno.binary_code();
    let mut keys: Vec<u64> = Vec::with_capacity(held.len());
    let mut out = OptionTargets::default();
    for &(id, seg) in held {
        if seg == fno {
            keys.push(id);
        } else {
            out.not_fno = out.not_fno.saturating_add(1);
        }
    }
    keys.sort_unstable();
    keys.dedup();
    for id in keys {
        let family = family_of(id, ExchangeSegment::NseFno);
        let (Some(family), Ok(security_id)) = (family, i64::try_from(id)) else {
            out.unresolved = out.unresolved.saturating_add(1);
            continue;
        };
        if out.targets.len() >= cap {
            out.truncated = out.truncated.saturating_add(1);
            continue;
        }
        let instrument = match family {
            OptionFamily::Index => "OPTIDX",
            OptionFamily::Stock => "OPTSTK",
        };
        out.targets.push(XverifyTarget {
            security_id,
            segment: ExchangeSegment::NseFno.as_str().to_string(),
            instrument: instrument.to_string(),
        });
    }
    out
}

/// Whether the option pass can still finish by
/// [`XVERIFY_LAST_END_SECS_OF_DAY_IST`] (17:23 IST, §12.15.8), starting now.
/// Pure, O(1).
#[must_use]
pub const fn option_pass_fits(now_secs_of_day: u64) -> bool {
    now_secs_of_day.saturating_add(attempt_max_secs(XVERIFY_OPTION_PASS_BUDGET_SECS))
        <= XVERIFY_LAST_END_SECS_OF_DAY_IST
}

/// Every `outcome` label the option pass can publish. Seeded at zero at the
/// start of each pass so a label reads as a real zero on `/metrics` rather
/// than an absent series. `timed_out` is published by `run_day` when the
/// pass's timeout elapses (§12.15.8).
pub const XVERIFY_OPTION_PASS_OUTCOMES: [&str; 9] = [
    "timed_out",
    "skipped_late",
    "no_targets",
    "no_token",
    "vacuous",
    "measured",
    "partial",
    "diverged",
    "failed",
];

/// The label for a pass that ran. `vacuous` when nothing was compared;
/// `partial` when the run stopped at its budget or some vendor fetches failed,
/// so part of the target list was never compared; `measured` only when every
/// target was fetched. Pure, O(1).
#[must_use]
pub const fn option_pass_outcome(
    vacuous: bool,
    budget_elapsed: bool,
    rest_failures: usize,
) -> &'static str {
    if vacuous {
        "vacuous"
    } else if budget_elapsed || rest_failures > 0 {
        "partial"
    } else {
        "measured"
    }
}

/// The after-close check of the day's depth-held option contracts.
///
/// It never writes or blocks the day marker, never appends a daily row, and
/// never pages: a catastrophic divergence is one `warn!` (§12.15.6).
///
/// `deadline` is the instant its timeout fires (§12.15.8); the audit persist,
/// which runs synchronously and cannot be cut by that timeout, stops itself
/// there (`persist_option_findings`).
async fn run_option_pass(
    deps: &CrossverifyBootDeps,
    today: chrono::NaiveDate,
    day_start_ist_nanos: i64,
    deadline: tokio::time::Instant,
) {
    for label in XVERIFY_OPTION_PASS_OUTCOMES {
        metrics::counter!(XVERIFY_OPTION_PASS_COUNTER, "outcome" => label).increment(0);
    }
    if !option_pass_fits(now_ist_secs_of_day()) {
        metrics::counter!(XVERIFY_OPTION_PASS_COUNTER, "outcome" => "skipped_late").increment(1);
        info!(%today, "Dhan option cross-check skipped — it could not finish before the evening stop");
        return;
    }
    let held = crate::depth_subscription_view::global_depth_subscription_view()
        .held_today_snapshot(chrono::Utc::now().timestamp());
    let map = crate::contract_underlying_map::global_contract_underlying_map();
    let built = option_targets_from_depth_held(
        &held,
        |id, seg| map.owner_of(id, seg).map(|owner| owner.family),
        XVERIFY_MAX_OPTION_TARGETS,
    );
    if built.targets.is_empty() {
        metrics::counter!(XVERIFY_OPTION_PASS_COUNTER, "outcome" => "no_targets").increment(1);
        info!(
            %today,
            held = held.len(),
            unresolved = built.unresolved,
            not_fno = built.not_fno,
            "Dhan option cross-check had no option contracts to compare today"
        );
        return;
    }
    let Some(jwt) = wait_for_jwt().await else {
        metrics::counter!(XVERIFY_OPTION_PASS_COUNTER, "outcome" => "no_token").increment(1);
        warn!(
            code = ErrorCode::WsGapConnectionState.code_str(),
            source = "xverify_options_no_token",
            %today,
            "Dhan option cross-check could not run: no Dhan token available"
        );
        return;
    };
    let cfg = DhanLiveCrossverifyConfig {
        run_budget_secs: XVERIFY_OPTION_PASS_BUDGET_SECS,
        ..deps.config
    };
    let client = reqwest::Client::new();
    let result = run_cross_verification(
        &client,
        &deps.questdb_exec_url,
        &deps.intraday_url,
        jwt.expose_secret(),
        &built.targets,
        today,
        day_start_ist_nanos,
        &cfg,
    )
    .await;
    drop(jwt);
    match result {
        Ok(report) => {
            let c = &report.comparison;
            let persisted_ok = persist_option_findings(&deps.questdb, &report, deadline);
            let label =
                option_pass_outcome(c.is_vacuous(), report.budget_elapsed, report.rest_failures);
            metrics::counter!(XVERIFY_OPTION_PASS_COUNTER, "outcome" => label).increment(1);
            info!(
                %today,
                targets = built.targets.len(),
                unresolved = built.unresolved,
                truncated = built.truncated,
                not_fno = built.not_fno,
                outcome = c.outcome.as_str(),
                instruments = c.instruments,
                minutes_compared = c.minutes_compared,
                cells_diverged = c.cells_diverged,
                missing_live = c.missing_live,
                missing_rest = c.missing_rest,
                rest_failures = report.rest_failures,
                budget_elapsed = report.budget_elapsed,
                persisted_ok,
                "Dhan option cross-check finished"
            );
            if is_catastrophic_divergence(c) {
                metrics::counter!(XVERIFY_OPTION_PASS_COUNTER, "outcome" => "diverged")
                    .increment(1);
                warn!(
                    code = ErrorCode::WsGapConnectionState.code_str(),
                    source = "xverify_options_diverged",
                    %today,
                    minutes_compared = c.minutes_compared,
                    cells_diverged = c.cells_diverged,
                    "Dhan option cross-check found MORE THAN HALF of the compared price \
                     fields of the depth-held option contracts disagreeing with Dhan's \
                     own record"
                );
            }
        }
        Err(err) => {
            metrics::counter!(XVERIFY_OPTION_PASS_COUNTER, "outcome" => "failed").increment(1);
            warn!(
                code = ErrorCode::WsGapConnectionState.code_str(),
                source = "xverify_options_failed",
                %today,
                %err,
                "Dhan option cross-check FAILED to run"
            );
        }
    }
}

/// Persists the option pass's cell findings and vendor tape. NO daily row: the
/// daily DEDUP key `(ts, trading_date_ist, feed, outcome)` would collide with
/// the spot row. Returns `true` when the final flush succeeded.
///
/// It keeps its discard-then-continue shape ON PURPOSE (2026-10-06,
/// §12.15.7): a failed mid-run flush loses that chunk and the pass still
/// reports success once the final flush lands. Unlike the spot check
/// ([`persist_report_into`]) it writes no marker and never pages (§12.15.6),
/// so a partial write here holds nothing back and hides nothing alarmed; the
/// gap is logged on `xverify_options_persist_partial`.
///
/// §12.15.8: it stops at `deadline` the same way the spot persist does: before
/// each row it checks that a flush of the buffer would end by `deadline` in
/// the ILP client's worst case, and otherwise discards the buffer, logs
/// `xverify_options_persist_stopped_at_deadline` and returns `false`.
fn persist_option_findings(
    questdb: &QuestDbConfig,
    report: &RunReport,
    deadline: tokio::time::Instant,
) -> bool {
    let mut writer = DhanLiveXverifyAuditWriter::new(questdb);
    persist_option_findings_into(&mut writer, report, deadline, tokio::time::Instant::now)
        == OptionPersist::Flushed
}

/// What the option persist did (§12.15.8).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OptionPersist {
    /// The final flush was ACKed (mid-run chunks may still have been lost).
    Flushed,
    /// The final flush failed; its rows were discarded.
    Failed,
    /// The persist stopped before a flush it could not end by the deadline.
    StoppedAtDeadline,
}

/// The body of [`persist_option_findings`], with the writer and the clock
/// injected. O(findings + tape rows), once per day, cold.
fn persist_option_findings_into(
    writer: &mut DhanLiveXverifyAuditWriter,
    report: &RunReport,
    deadline: tokio::time::Instant,
    mut now: impl FnMut() -> tokio::time::Instant,
) -> OptionPersist {
    let c = &report.comparison;
    let mut row_errors = 0_usize;
    let mut batch_errors = 0_usize;
    let mut flush_if_full = |w: &mut DhanLiveXverifyAuditWriter| {
        let failed = if w.pending() >= PERSIST_BATCH_ROWS {
            tickvault_storage::off_worker::off_worker(|| w.flush()).is_err()
        } else {
            tickvault_storage::off_worker::off_worker(|| w.flush_if_large()).is_err()
        };
        if failed {
            batch_errors = batch_errors.saturating_add(1);
        }
    };
    let total = c.findings.len().saturating_add(report.rest_tape.len());
    let mut appended = 0_usize;
    let mut stopped = false;
    for finding in &c.findings {
        if !flush_fits(writer, now(), deadline) {
            stopped = true;
            break;
        }
        if writer.append_cell(finding).is_err() {
            row_errors = row_errors.saturating_add(1);
        }
        appended += 1;
        flush_if_full(writer);
    }
    if !stopped {
        for row in &report.rest_tape {
            if !flush_fits(writer, now(), deadline) {
                stopped = true;
                break;
            }
            if writer.append_rest_tape(row).is_err() {
                row_errors = row_errors.saturating_add(1);
            }
            appended += 1;
            flush_if_full(writer);
        }
    }
    if stopped || !flush_fits(writer, now(), deadline) {
        let discarded = writer.discard_pending();
        metrics::counter!(XVERIFY_PERSIST_ERRORS_COUNTER).increment(1);
        warn!(
            code = ErrorCode::WsGapConnectionState.code_str(),
            source = "xverify_options_persist_stopped_at_deadline",
            discarded,
            not_written = total - appended,
            "Dhan option cross-check stopped saving its audit rows: the next write could \
             not finish within the option check's time limit, so it ends before the \
             evening stop"
        );
        return OptionPersist::StoppedAtDeadline;
    }
    match tickvault_storage::off_worker::off_worker(|| writer.flush()) {
        Ok(()) => {
            metrics::counter!(XVERIFY_PERSIST_ROWS_COUNTER)
                .increment(c.findings.len() as u64 + report.rest_tape.len() as u64);
            if row_errors > 0 || batch_errors > 0 {
                error!(
                    code = ErrorCode::WsGapConnectionState.code_str(),
                    source = "xverify_options_persist_partial",
                    row_errors,
                    batch_errors,
                    "Dhan option cross-check persisted with gaps — the audit tables are \
                     incomplete for today's option contracts"
                );
            }
            OptionPersist::Flushed
        }
        Err(err) => {
            let discarded = writer.discard_pending();
            metrics::counter!(XVERIFY_PERSIST_ERRORS_COUNTER).increment(1);
            error!(
                code = ErrorCode::WsGapConnectionState.code_str(),
                source = "xverify_options_persist_failed",
                ?err,
                discarded,
                "Dhan option cross-check could NOT be persisted — today's option \
                 comparison exists only in this log stream"
            );
            OptionPersist::Failed
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tickvault_storage::dhan_live_crossverify_persistence::DhanLiveXverifyOutcome;

    fn instrument(security_id: u64, segment: ExchangeSegment) -> SubscribeInstrument {
        SubscribeInstrument {
            security_id,
            segment,
        }
    }

    fn comparison(outcome: DhanLiveXverifyOutcome, minutes: i64, diverged: i64) -> DayComparison {
        DayComparison {
            outcome,
            findings: Vec::new(),
            instruments: 1,
            minutes_compared: minutes,
            cells_diverged: diverged,
            missing_live: 0,
            missing_live_traded: 0,
            missing_live_zero_volume: 0,
            missing_rest: 0,
            tail_unsealed: 0,
            out_of_session: 0,
            noise_p50_paise: 0,
            noise_p95_paise: 0,
            noise_max_paise: 0,
            volume_cells: 0,
            volume_exact: 0,
            volume_capture_p50_pct: 0,
            volume_capture_p05_pct: 0,
            volume_capture_min_pct: 0,
        }
    }

    #[test]
    fn test_crossverify_targets_with_skipped_targets_index_and_equity_not_fno() {
        let feed = [
            instrument(13, ExchangeSegment::IdxI),
            instrument(2885, ExchangeSegment::NseEquity),
            instrument(500_325, ExchangeSegment::BseEquity),
            instrument(52_175, ExchangeSegment::NseFno),
            instrument(1, ExchangeSegment::McxComm),
        ];
        let (targets, skipped) = crossverify_targets_with_skipped(&feed);
        assert_eq!(skipped, 2);
        assert_eq!(targets.len(), 3);
        assert_eq!(targets[0].instrument, "INDEX");
        assert_eq!(targets[0].segment, "IDX_I");
        assert_eq!(targets[1].instrument, "EQUITY");
        assert_eq!(targets[2].instrument, "EQUITY");
    }

    #[test]
    fn test_dhan_intraday_instrument_for_maps_only_index_and_equity() {
        assert_eq!(
            dhan_intraday_instrument_for(ExchangeSegment::IdxI),
            Some("INDEX")
        );
        assert_eq!(
            dhan_intraday_instrument_for(ExchangeSegment::NseEquity),
            Some("EQUITY")
        );
        assert_eq!(
            dhan_intraday_instrument_for(ExchangeSegment::BseEquity),
            Some("EQUITY")
        );
        for skipped in [
            ExchangeSegment::NseFno,
            ExchangeSegment::BseFno,
            ExchangeSegment::NseCurrency,
            ExchangeSegment::BseCurrency,
            ExchangeSegment::McxComm,
        ] {
            assert_eq!(dhan_intraday_instrument_for(skipped), None);
        }
    }

    #[test]
    fn test_out_of_range_security_id_is_skipped_not_zeroed() {
        let feed = [instrument(u64::MAX, ExchangeSegment::IdxI)];
        let (targets, skipped) = crossverify_targets_with_skipped(&feed);
        assert!(targets.is_empty());
        assert_eq!(skipped, 1);
    }

    #[test]
    fn test_run_once_waits_for_the_token_before_failing() {
        // The 2026-09-24 boot catch-up failed because it read the token once,
        // before login finished. The wait must be long enough for a slow mint
        // and short enough to report a real login failure the same evening.
        let budget = TOKEN_WAIT_POLL_SECS * u64::from(TOKEN_WAIT_MAX_POLLS);
        assert!(budget >= 60, "wait too short to cover login: {budget}s");
        assert!(
            budget <= 600,
            "wait too long to report a dead login: {budget}s"
        );

        // run_once must go through the waiting read, never a single read.
        let src = include_str!("dhan_live_crossverify_boot.rs");
        let body = src
            .split("async fn run_once(")
            .nth(1)
            .and_then(|s| s.split("\n}\n").next())
            .unwrap_or("");
        assert!(
            body.contains("wait_for_jwt().await"),
            "run_once must wait for the token"
        );
        assert!(
            !body.contains("= current_jwt()"),
            "run_once reads the token once and fails on a slow login"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn test_poll_until_returns_once_the_value_appears() {
        let start = tokio::time::Instant::now();
        let mut calls = 0u32;
        let got = poll_until(
            || {
                calls += 1;
                (calls > 3).then_some(7u8)
            },
            TOKEN_WAIT_POLL_SECS,
            TOKEN_WAIT_MAX_POLLS,
        )
        .await;
        assert_eq!(got, Some((7, 3)));
        // Three sleeps before the fourth check finds the value.
        assert_eq!(
            start.elapsed(),
            Duration::from_secs(TOKEN_WAIT_POLL_SECS * 3)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn test_poll_until_gives_up_after_the_bound() {
        let start = tokio::time::Instant::now();
        let mut calls = 0u32;
        let got: Option<(u8, u32)> = poll_until(
            || {
                calls += 1;
                None
            },
            TOKEN_WAIT_POLL_SECS,
            TOKEN_WAIT_MAX_POLLS,
        )
        .await;
        assert_eq!(got, None);
        assert_eq!(
            calls,
            TOKEN_WAIT_MAX_POLLS + 1,
            "one check per sleep plus the first"
        );
        assert_eq!(
            start.elapsed(),
            Duration::from_secs(TOKEN_WAIT_POLL_SECS * u64::from(TOKEN_WAIT_MAX_POLLS)),
            "one sleep per poll"
        );
    }

    #[test]
    fn test_current_jwt_skips_an_expired_token() {
        let src = include_str!("dhan_live_crossverify_boot.rs");
        let body = src
            .split("fn current_jwt()")
            .nth(1)
            .and_then(|s| s.split("\n}\n").next())
            .unwrap_or("");
        assert!(
            body.contains(".filter(|state| state.is_valid())"),
            "an expired cached token must count as absent"
        );
    }

    #[test]
    fn test_run_is_complete_holds_s3_on_a_short_run() {
        assert!(run_is_complete(false, false, 0, 868));
        // 5% of 868 is 43.4, so 43 failures pass and 44 do not.
        assert!(run_is_complete(false, false, 43, 868));
        assert!(!run_is_complete(false, false, 44, 868));
        assert!(!run_is_complete(true, false, 0, 868));
        assert!(!run_is_complete(false, true, 0, 868));
        // No targets: nothing failed, nothing to hold on.
        assert!(run_is_complete(false, false, 0, 0));
        assert!(!run_is_complete(false, false, 1, 0));
    }

    /// §12.15.7: exactly one marker write in the production file, inside
    /// `record_day`; `record_day` is called only from `run_once`'s `Ok` arm and
    /// from `run_day`'s marker-only branch; a write error maps to
    /// `MarkerNotWritten`.
    #[test]
    fn test_marker_write_needs_a_complete_run() {
        let prod = prod_src();
        let body = fn_body(prod, "async fn run_once(");
        assert!(
            body.contains("classify_attempt("),
            "the marker decision must go through classify_attempt"
        );
        assert!(
            body.contains("run_is_complete("),
            "the marker must also require a complete run"
        );
        assert!(body.contains("persist_verdict("));
        assert_eq!(
            prod.matches("try_write_daily_marker_keeping(").count(),
            1,
            "exactly one marker write in the production file"
        );
        let record = fn_body(prod, "fn record_day(");
        assert!(record.contains("try_write_daily_marker_keeping("));
        assert!(record.contains("CROSSVERIFY_MARKER_KEEP_DAYS"));
        assert!(record.contains("Err(AttemptFailure::MarkerNotWritten)"));
        assert_eq!(
            prod.matches("record_day(").count(),
            3,
            "the definition plus exactly two call sites"
        );
        let ok_arm = body.find("Ok(()) => record_day(today)");
        assert!(ok_arm.is_some(), "run_once writes the marker on its Ok arm");
        let run_day = fn_body(prod, "async fn run_day(");
        assert!(run_day.contains("AttemptKind::MarkerOnly => {"));
        assert!(!prod.contains("write_daily_marker("));
    }

    /// `classify_attempt` returns `Ok` exactly when the marker rule holds:
    /// `should_write_marker(persist_verdict is Ok) && complete`. Checked over
    /// every outcome, both vacuous and measured minute counts, every persist
    /// verdict and both completeness values.
    #[test]
    fn test_classify_attempt_agrees_with_the_marker_rule_everywhere() {
        let outcomes = [
            DhanLiveXverifyOutcome::Clean,
            DhanLiveXverifyOutcome::Diverged,
            DhanLiveXverifyOutcome::Partial,
            DhanLiveXverifyOutcome::NoData,
            DhanLiveXverifyOutcome::Blind,
            DhanLiveXverifyOutcome::Degraded,
        ];
        let persists = [
            Ok(()),
            Err(AttemptFailure::NotPersisted),
            Err(AttemptFailure::AuditRowsLost),
        ];
        for outcome in outcomes {
            for minutes in [0_i64, 375] {
                let c = comparison(outcome, minutes, 0);
                for persist in persists {
                    for complete in [false, true] {
                        let verdict = classify_attempt(
                            c.is_vacuous(),
                            c.outcome.is_measured(),
                            persist,
                            complete,
                        );
                        assert_eq!(
                            verdict.is_ok(),
                            should_write_marker(&c, persist.is_ok()) && complete,
                            "{outcome:?} minutes={minutes} persist={persist:?} \
                             complete={complete}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn test_classify_attempt_names_the_first_problem() {
        use AttemptFailure::*;
        assert_eq!(classify_attempt(true, true, Ok(()), true), Err(Vacuous));
        assert_eq!(
            classify_attempt(true, false, Err(NotPersisted), false),
            Err(Vacuous)
        );
        // Vacuous beats a lost audit row.
        assert_eq!(
            classify_attempt(true, true, Err(AuditRowsLost), true),
            Err(Vacuous)
        );
        // Degraded with minutes compared: not vacuous, not measured.
        assert_eq!(
            classify_attempt(false, false, Ok(()), true),
            Err(Incomplete)
        );
        assert_eq!(
            classify_attempt(false, true, Err(NotPersisted), true),
            Err(NotPersisted)
        );
        assert_eq!(
            classify_attempt(false, true, Err(NotPersisted), false),
            Err(NotPersisted)
        );
        // A lost audit row beats an incomplete run.
        assert_eq!(
            classify_attempt(false, true, Err(AuditRowsLost), false),
            Err(AuditRowsLost)
        );
        assert_eq!(
            classify_attempt(false, true, Ok(()), false),
            Err(Incomplete)
        );
        assert_eq!(classify_attempt(false, true, Ok(()), true), Ok(()));
    }

    #[test]
    fn test_attempt_failure_labels_are_distinct() {
        let all = [
            AttemptFailure::NoToken,
            AttemptFailure::RunFailed,
            AttemptFailure::Vacuous,
            AttemptFailure::NotPersisted,
            AttemptFailure::Incomplete,
            AttemptFailure::MarkerNotWritten,
            AttemptFailure::AuditRowsLost,
            AttemptFailure::SkippedNoTime,
        ];
        let mut labels: Vec<&str> = all.iter().map(|f| f.as_str()).collect();
        labels.sort_unstable();
        labels.dedup();
        assert_eq!(labels.len(), all.len());
        assert_eq!(
            AttemptFailure::MarkerNotWritten.as_str(),
            "marker_not_written"
        );
        assert_eq!(AttemptFailure::AuditRowsLost.as_str(), "audit_rows_lost");
        assert_eq!(AttemptFailure::SkippedNoTime.as_str(), "skipped_no_time");
    }

    /// The keep outlasts every default gated hold lookback, and the boot
    /// check flags a configured window that comes within the slack. The
    /// disk-pressure window is not an input at all.
    #[test]
    fn test_crossverify_marker_keep_is_short_and_keep_exceeds_every_gated_hold_lookback() {
        use tickvault_storage::partition_archive::MAX_CROSSVERIFY_HOLD_DAYS;
        assert!(CROSSVERIFY_MARKER_KEEP_DAYS > 90 + MAX_CROSSVERIFY_HOLD_DAYS);
        assert!(CROSSVERIFY_MARKER_KEEP_DAYS > crate::daily_task_marker::DAILY_MARKER_SWEEP_DAYS);
        for days in [0_u32, 1, 15, 90, 300, 393] {
            assert!(
                !crossverify_marker_keep_is_short(days),
                "{days} gated days must fit the keep"
            );
        }
        for days in [394_u32, 397, 400, 10_000, u32::MAX] {
            assert!(
                crossverify_marker_keep_is_short(days),
                "{days} gated days must be flagged"
            );
        }
        let main_rs = include_str!("main.rs");
        let at = main_rs
            .find("crossverify_marker_keep_is_short(")
            .expect("main.rs must run the boot check");
        let start = main_rs[..at]
            .rfind("let _dhan_crossverify")
            .expect("the check sits in the cross-verification spawn block");
        let head = &main_rs[start..at];
        for gated in [
            "retention_days",
            "market_data_hot_days",
            "depth_hot_days",
            "intraday_hot_days",
        ] {
            assert!(head.contains(gated), "{gated} must feed the boot check");
        }
        assert!(
            !head.contains(".pressure_hot_days"),
            "the ungated disk-pressure window must not feed the boot check"
        );
        assert!(main_rs.contains("source = \"xverify_marker_keep_short\""));
    }

    proptest::proptest! {
        /// `persist_verdict` is `Ok` iff the final flush and the daily row
        /// landed and no row was discarded or refused; `NotPersisted` wins
        /// over `AuditRowsLost`.
        #[test]
        fn test_persist_verdict_any_lost_row_is_a_failed_attempt(
            final_flush_ok in proptest::bool::ANY,
            daily_appended in proptest::bool::ANY,
            rows_discarded in 0_usize..3,
            rows_flushed in 0_usize..100_000,
            cell_append_errors in 0_usize..3,
            tape_append_errors in 0_usize..3,
        ) {
            let outcome = PersistOutcome {
                final_flush_ok,
                daily_appended,
                rows_discarded,
                rows_flushed,
                cell_append_errors,
                tape_append_errors,
                deadline_reached: false,
                rows_not_written_at_deadline: 0,
            };
            let lost = rows_discarded > 0 || cell_append_errors > 0 || tape_append_errors > 0;
            let expected = if !final_flush_ok || !daily_appended {
                Err(AttemptFailure::NotPersisted)
            } else if lost {
                Err(AttemptFailure::AuditRowsLost)
            } else {
                Ok(())
            };
            proptest::prop_assert_eq!(persist_verdict(&outcome), expected);
        }
    }

    fn finding(
        i: i64,
    ) -> tickvault_storage::dhan_live_crossverify_persistence::DhanLiveXverifyCellFinding {
        tickvault_storage::dhan_live_crossverify_persistence::DhanLiveXverifyCellFinding {
            run_ts_ist_nanos: 1_000,
            trading_date_ist_nanos: 0,
            security_id: 13,
            segment: "IDX_I".to_string(),
            minute_ts_ist_nanos: i * 60_000_000_000,
            kind: tickvault_storage::dhan_live_crossverify_persistence::DhanLiveXverifyCellKind::Diverged,
            field: "open",
            live_value: 100.0,
            rest_value: 101.0,
            live_volume: 0,
            rest_volume: 0,
            diff_paise: 100,
        }
    }

    fn tape_row(i: i64) -> tickvault_storage::dhan_live_crossverify_persistence::DhanRestTapeRow {
        tickvault_storage::dhan_live_crossverify_persistence::DhanRestTapeRow {
            minute_ts_ist_nanos: i * 60_000_000_000,
            trading_date_ist_nanos: 0,
            security_id: 13,
            segment: "IDX_I".to_string(),
            instrument: "INDEX".to_string(),
            open: 100.0,
            high: 101.0,
            low: 99.0,
            close: 100.5,
            volume: 0,
            fetched_at_nanos: 2_000,
        }
    }

    /// With no sender every flush fails and discards, so every appended row
    /// (5 findings + 3 tape rows + the daily row) must be counted as
    /// discarded and none as flushed — rows, not batches. Coverage limit: the
    /// "final flush Ok after a mid-run discard" branch is pinned by the
    /// `persist_verdict` proptest, because the test writer has no sender.
    #[test]
    fn test_persist_report_into_counts_rows_discarded_not_batches() {
        let mut c = comparison(DhanLiveXverifyOutcome::Diverged, 375, 5);
        c.findings = (0..5).map(finding).collect();
        let report = RunReport {
            comparison: c,
            rest_failures: 0,
            rest_failure_breakdown: Default::default(),
            degraded: false,
            malformed_rows: 0,
            budget_elapsed: false,
            live_truncated: false,
            rest_tape: (0..3).map(tape_row).collect(),
        };
        let mut writer = DhanLiveXverifyAuditWriter::for_test();
        let t0 = tokio::time::Instant::now();
        let far = t0 + Duration::from_secs(86_400);
        let out = persist_report_into(&mut writer, &report, 0, 5, 2, far, || t0);
        assert!(!out.deadline_reached);
        assert_eq!(out.rows_not_written_at_deadline, 0);
        assert!(out.daily_appended);
        assert!(!out.final_flush_ok);
        assert_eq!(out.cell_append_errors, 0);
        assert_eq!(out.tape_append_errors, 0);
        assert_eq!(out.rows_discarded, 5 + 3 + 1, "{out:?}");
        assert_eq!(out.rows_flushed, 0);
        assert_eq!(writer.pending(), 0);
        assert_eq!(persist_verdict(&out), Err(AttemptFailure::NotPersisted));
    }

    fn report_with(findings: usize, tape: usize) -> RunReport {
        let mut c = comparison(DhanLiveXverifyOutcome::Diverged, 375, 5);
        let to_i64 = |n: usize| i64::try_from(n).unwrap_or(i64::MAX);
        c.findings = (0..findings).map(|i| finding(to_i64(i))).collect();
        RunReport {
            comparison: c,
            rest_failures: 0,
            rest_failure_breakdown: Default::default(),
            degraded: false,
            malformed_rows: 0,
            budget_elapsed: false,
            live_truncated: false,
            rest_tape: (0..tape).map(|i| tape_row(to_i64(i))).collect(),
        }
    }

    /// §12.15.8: with the deadline already passed, no row is appended and no
    /// flush starts; every row is counted as not written, and the attempt is
    /// `NotPersisted`, so no marker.
    #[test]
    fn test_persist_report_into_writes_nothing_past_the_deadline() {
        let report = report_with(5, 3);
        let mut writer = DhanLiveXverifyAuditWriter::for_test();
        let t0 = tokio::time::Instant::now();
        let out = persist_report_into(&mut writer, &report, 0, 5, 2, t0, || t0);
        assert!(out.deadline_reached);
        assert!(!out.final_flush_ok);
        assert!(!out.daily_appended);
        assert_eq!(out.rows_not_written_at_deadline, 5 + 3 + 1, "{out:?}");
        assert_eq!(out.rows_discarded, 0);
        assert_eq!(out.rows_flushed, 0);
        assert_eq!(writer.pending(), 0);
        assert_eq!(persist_verdict(&out), Err(AttemptFailure::NotPersisted));
    }

    /// §12.15.8: the persist stops at the first row whose buffer could not be
    /// flushed by the deadline in the ILP client's worst case (5 s plus the
    /// bytes at 100 KiB/s). The clock advances 1 s per reading; the deadline
    /// is 7 s out, so readings at 0, 1 and 2 s fit (2 s + 5 s with an empty
    /// buffer is exactly 7 s) and the reading at 3 s does not.
    #[test]
    fn test_persist_report_into_stops_at_the_first_flush_that_cannot_end_by_the_deadline() {
        let report = report_with(5, 3);
        let mut writer = DhanLiveXverifyAuditWriter::for_test();
        let t0 = tokio::time::Instant::now();
        let reads = std::cell::Cell::new(0_u64);
        let clock = || {
            let at = t0 + Duration::from_secs(reads.get());
            reads.set(reads.get() + 1);
            at
        };
        let deadline = t0 + Duration::from_secs(7);
        let out = persist_report_into(&mut writer, &report, 0, 5, 2, deadline, clock);
        assert_eq!(reads.get(), 4, "one reading per row until the stop");
        assert!(out.deadline_reached);
        // Rows 0 and 1 went in a failed batch flush (no sender), row 2 was
        // buffered and discarded at the stop; 2 findings, 3 tape rows and
        // the daily row were never appended.
        assert_eq!(out.rows_discarded, 3, "{out:?}");
        assert_eq!(out.rows_not_written_at_deadline, 6, "{out:?}");
        assert_eq!(writer.pending(), 0);
        assert_eq!(persist_verdict(&out), Err(AttemptFailure::NotPersisted));
    }

    proptest::proptest! {
        /// Whatever the clock and deadline: every row is accounted for once
        /// (flushed, discarded or not written), and every clock reading the
        /// persist acted on left at least the 5 s request timeout before the
        /// deadline, so no flush it started could end after it.
        #[test]
        fn test_persist_report_into_never_acts_without_room_before_the_deadline(
            findings in 0_usize..40,
            tape in 0_usize..40,
            batch in 1_usize..10,
            step_ms in 0_u64..3_000,
            deadline_ms in 0_u64..90_000,
        ) {
            let report = report_with(findings, tape);
            let mut writer = DhanLiveXverifyAuditWriter::for_test();
            let t0 = tokio::time::Instant::now();
            let readings = std::cell::RefCell::new(Vec::new());
            let clock = || {
                let n = u64::try_from(readings.borrow().len()).unwrap_or(u64::MAX);
                let at_ms = n.saturating_mul(step_ms);
                readings.borrow_mut().push(at_ms);
                t0 + Duration::from_millis(at_ms)
            };
            let deadline = t0 + Duration::from_millis(deadline_ms);
            let out = persist_report_into(&mut writer, &report, 0, 5, batch, deadline, clock);
            proptest::prop_assert_eq!(
                out.rows_flushed + out.rows_discarded + out.rows_not_written_at_deadline,
                findings + tape + 1
            );
            proptest::prop_assert_eq!(writer.pending(), 0);
            proptest::prop_assert!(persist_verdict(&out).is_err());
            let readings = readings.into_inner();
            let acted = if out.deadline_reached {
                &readings[..readings.len().saturating_sub(1)]
            } else {
                &readings[..]
            };
            for &at_ms in acted {
                proptest::prop_assert!(at_ms + 5_000 <= deadline_ms, "{at_ms} {deadline_ms}");
            }
            if out.deadline_reached {
                proptest::prop_assert!(!out.final_flush_ok);
            } else {
                proptest::prop_assert_eq!(out.rows_not_written_at_deadline, 0);
            }
        }
    }

    /// §12.15.8: the option persist stops the same way. An empty report with
    /// room flushes nothing and succeeds; the same report past the deadline
    /// stops before the final flush; rows with room reach the (absent)
    /// sender and fail.
    #[test]
    fn test_persist_option_findings_into_stops_at_the_deadline() {
        let t0 = tokio::time::Instant::now();
        let far = t0 + Duration::from_secs(86_400);
        let mut w = DhanLiveXverifyAuditWriter::for_test();
        let empty = report_with(0, 0);
        assert_eq!(
            persist_option_findings_into(&mut w, &empty, far, || t0),
            OptionPersist::Flushed
        );
        assert_eq!(
            persist_option_findings_into(&mut w, &empty, t0, || t0),
            OptionPersist::StoppedAtDeadline
        );
        let rows = report_with(4, 2);
        assert_eq!(
            persist_option_findings_into(&mut w, &rows, t0, || t0),
            OptionPersist::StoppedAtDeadline
        );
        assert_eq!(w.pending(), 0);
        assert_eq!(
            persist_option_findings_into(&mut w, &rows, far, || t0),
            OptionPersist::Failed
        );
        assert_eq!(w.pending(), 0);
    }

    #[test]
    fn test_persist_rows_counter_counts_only_flushed_rows() {
        let body = fn_body(prod_src(), "fn finish_persist(");
        assert!(fn_body(prod_src(), "fn persist_report_into(").contains("finish_persist("));
        assert!(
            body.contains(
                "metrics::counter!(XVERIFY_PERSIST_ROWS_COUNTER).increment(out.rows_flushed as u64)"
            ),
            "the rows counter must count ACKed rows only"
        );
        assert!(
            !body.contains("increment(c.findings.len()"),
            "the rows counter must never count appended rows"
        );
    }

    #[test]
    fn test_next_attempt_kind_after_marker_failure_is_marker_only() {
        assert_eq!(
            next_attempt_kind(Some(AttemptFailure::MarkerNotWritten)),
            AttemptKind::MarkerOnly
        );
        assert_eq!(next_attempt_kind(None), AttemptKind::Full);
        for other in [
            AttemptFailure::NoToken,
            AttemptFailure::RunFailed,
            AttemptFailure::Vacuous,
            AttemptFailure::NotPersisted,
            AttemptFailure::Incomplete,
            AttemptFailure::AuditRowsLost,
        ] {
            assert_eq!(next_attempt_kind(Some(other)), AttemptKind::Full);
        }
    }

    /// The marker-only branch writes the marker and nothing else.
    #[test]
    fn test_marker_only_attempt_never_reruns_the_check() {
        let drive = fn_body(prod_src(), "async fn drive_day<");
        assert!(drive.contains("next_attempt_kind(previous)"));
        assert!(drive.contains("previous = Some(failure);"));
        let run_day = fn_body(prod_src(), "async fn run_day(");
        let start = run_day
            .find("AttemptKind::MarkerOnly => {")
            .expect("run_day must branch on the attempt kind");
        let branch = &run_day[start..];
        let branch = &branch[..branch.find("AttemptKind::Full =>").unwrap_or(branch.len())];
        assert!(branch.contains("record_day("));
        for forbidden in [
            "run_once(",
            "run_cross_verification(",
            "wait_for_jwt(",
            "divergence_paged",
            "persist_report",
        ] {
            assert!(
                !branch.contains(forbidden),
                "the marker-only attempt must not call {forbidden}"
            );
        }
    }

    /// A day whose marker could not be saved on any attempt pages on the
    /// existing `xverify_failed` source, from its own `error!` arm.
    #[test]
    fn test_marker_not_written_final_failure_pages_on_existing_xverify_failed() {
        let body = fn_body(prod_src(), "fn report_final_failure(");
        let start = body
            .find("AttemptFailure::MarkerNotWritten => error!(")
            .expect("MarkerNotWritten must have its own error! arm");
        let arm = &body[start..];
        let arm = &arm[..arm.find("AttemptFailure::NoToken").unwrap_or(arm.len())];
        assert!(arm.contains("source = \"xverify_failed\""));
        assert!(arm.contains("code = ErrorCode::WsGapConnectionState.code_str()"));
        assert!(arm.contains("the day marker could not be saved to disk"));
        assert!(
            body.contains("| AttemptFailure::AuditRowsLost => error!("),
            "AuditRowsLost joins the existing xverify_failed arm"
        );
    }

    #[test]
    fn test_attempt_max_secs_is_token_wait_plus_budget_plus_margin() {
        let token_wait = TOKEN_WAIT_POLL_SECS * u64::from(TOKEN_WAIT_MAX_POLLS);
        assert_eq!(
            attempt_max_secs(600),
            token_wait + 600 + PERSIST_MARGIN_SECS
        );
        assert_eq!(attempt_max_secs(u64::MAX), u64::MAX);
    }

    #[test]
    fn test_retry_delay_secs_bounds() {
        let max = attempt_max_secs(600);
        let run = XVERIFY_RUN_AT_SECS_OF_DAY_IST;
        // A failed first attempt gets a retry.
        assert_eq!(
            retry_delay_secs(1, run, max),
            Some(XVERIFY_RETRY_INTERVAL_SECS)
        );
        // The day's attempts are used up.
        assert_eq!(
            retry_delay_secs(XVERIFY_MAX_ATTEMPTS_PER_DAY, run, max),
            None
        );
        assert_eq!(retry_delay_secs(u32::MAX, run, max), None);
        // §12.15.8: the next attempt must end by 17:23, not 17:30.
        let last_start = XVERIFY_LAST_END_SECS_OF_DAY_IST - XVERIFY_RETRY_INTERVAL_SECS - max;
        assert!(retry_delay_secs(1, last_start, max).is_some());
        assert_eq!(retry_delay_secs(1, last_start + 1, max), None);
        // The 17:30 bound would still allow this one; it must not.
        let old_bound_start = EVENING_STOP_SECS_OF_DAY_IST - XVERIFY_RETRY_INTERVAL_SECS - max;
        assert_eq!(retry_delay_secs(1, old_bound_start, max), None);
        // After the stop (a manual evening boot catch-up): no retry, no overflow.
        assert_eq!(
            retry_delay_secs(1, XVERIFY_LAST_END_SECS_OF_DAY_IST, max),
            None
        );
        assert_eq!(retry_delay_secs(1, EVENING_STOP_SECS_OF_DAY_IST, max), None);
        assert_eq!(retry_delay_secs(1, u64::MAX, max), None);
        assert_eq!(retry_delay_secs(1, run, u64::MAX), None);
    }

    #[test]
    fn test_deadline_and_last_end_derive_from_the_scheduled_stop_window() {
        let stop = u64::from(SCHEDULED_STOP_WINDOW_START_SECS_OF_DAY_IST);
        assert_eq!(stop, 17 * 3_600 + 25 * 60, "the window opens at 17:25");
        assert_eq!(XVERIFY_DEADLINE_SECS_OF_DAY_IST, stop - 60);
        assert_eq!(
            XVERIFY_LAST_END_SECS_OF_DAY_IST,
            XVERIFY_DEADLINE_SECS_OF_DAY_IST - 60
        );
        assert_eq!(XVERIFY_DEADLINE_SECS_OF_DAY_IST, 62_640, "17:24");
        assert_eq!(XVERIFY_LAST_END_SECS_OF_DAY_IST, 62_580, "17:23");
        assert!(XVERIFY_RUN_AT_SECS_OF_DAY_IST < XVERIFY_LAST_END_SECS_OF_DAY_IST);
        assert!(stop < EVENING_STOP_SECS_OF_DAY_IST);
        assert!(
            EVENING_STOP_SECS_OF_DAY_IST < u64::from(SCHEDULED_STOP_WINDOW_END_SECS_OF_DAY_IST)
        );
        // Neither bound is a literal in production code.
        let prod = prod_src();
        assert!(prod.contains("SCHEDULED_STOP_WINDOW_START_SECS_OF_DAY_IST as u64 - 60;"));
        assert!(prod.contains("XVERIFY_DEADLINE_SECS_OF_DAY_IST - 60;"));
        assert!(!prod.contains("= 62_580") && !prod.contains("= 62_640"));
    }

    #[test]
    fn test_default_run_budget_mirror_matches_the_config_default() {
        assert_eq!(
            DhanLiveCrossverifyConfig::default().run_budget_secs,
            XVERIFY_DEFAULT_RUN_BUDGET_SECS,
            "the four-attempt compile-time fit is asserted against this mirror"
        );
    }

    /// With the default budget, a 15:41 run whose every attempt takes the
    /// longest it can gets all its attempts in, the last one ending exactly at
    /// 17:23 (§12.15.8). The last-attempt decision is the conservative one.
    #[test]
    fn test_worst_case_day_fits_every_attempt_before_the_last_end() {
        let max = attempt_max_secs(600);
        assert_eq!(max, 960);
        let mut start = XVERIFY_RUN_AT_SECS_OF_DAY_IST;
        let mut starts = Vec::new();
        let mut attempts = 0_u32;
        let end = loop {
            attempts += 1;
            starts.push(start);
            assert_eq!(attempt_budget_secs(start, 600), Some(600), "never shrunk");
            let is_last = attempt_is_last(attempts, start, max);
            let end = start + max;
            assert!(end <= XVERIFY_LAST_END_SECS_OF_DAY_IST);
            if is_last {
                break end;
            }
            start = end + XVERIFY_RETRY_INTERVAL_SECS;
        };
        assert_eq!(attempts, XVERIFY_MAX_ATTEMPTS_PER_DAY);
        // 15:41:00, 16:09:40, 16:38:20, 17:07:00.
        assert_eq!(starts, vec![56_460, 58_180, 59_900, 61_620]);
        assert_eq!(
            end, XVERIFY_LAST_END_SECS_OF_DAY_IST,
            "ends exactly at 17:23"
        );
        // One more second of interval and the fourth attempt no longer fits.
        assert!(
            56_460 + max + 3 * (XVERIFY_RETRY_INTERVAL_SECS + 1 + max)
                > XVERIFY_LAST_END_SECS_OF_DAY_IST
        );
    }

    #[test]
    fn test_attempt_budget_secs_shrinks_then_refuses() {
        let last = XVERIFY_LAST_END_SECS_OF_DAY_IST;
        let fixed = attempt_max_secs(0);
        assert_eq!(fixed, 360, "token wait 300 s + persist margin 60 s");
        let run = XVERIFY_RUN_AT_SECS_OF_DAY_IST;
        assert_eq!(attempt_budget_secs(run, 600), Some(600));
        // Exactly the full budget fits, then one second less.
        assert_eq!(attempt_budget_secs(last - fixed - 600, 600), Some(600));
        assert_eq!(attempt_budget_secs(last - fixed - 599, 600), Some(599));
        assert_eq!(attempt_budget_secs(last - fixed - 200, 600), Some(200));
        // The floor, then one second past it.
        assert_eq!(
            attempt_budget_secs(last - fixed - XVERIFY_MIN_ATTEMPT_BUDGET_SECS, 600),
            Some(XVERIFY_MIN_ATTEMPT_BUDGET_SECS)
        );
        assert_eq!(
            attempt_budget_secs(last - fixed - XVERIFY_MIN_ATTEMPT_BUDGET_SECS + 1, 600),
            None
        );
        // A configured budget below the floor is never refused for its size,
        // only when it does not fit.
        assert_eq!(attempt_budget_secs(run, 60), Some(60));
        assert_eq!(attempt_budget_secs(last - fixed - 60, 60), Some(60));
        assert_eq!(attempt_budget_secs(last - fixed - 59, 60), None);
        assert_eq!(attempt_budget_secs(last - fixed - 110, 100), Some(100));
        assert_eq!(attempt_budget_secs(last - fixed - 119, 600), None);
        // At the end bound, inside the stop window, and up to its end: refused.
        for now in [last - fixed, last, XVERIFY_DEADLINE_SECS_OF_DAY_IST] {
            assert_eq!(attempt_budget_secs(now, 600), None, "now={now}");
        }
        let window_start = u64::from(SCHEDULED_STOP_WINDOW_START_SECS_OF_DAY_IST);
        let window_end = u64::from(SCHEDULED_STOP_WINDOW_END_SECS_OF_DAY_IST);
        assert_eq!(attempt_budget_secs(window_start, 600), None);
        assert_eq!(attempt_budget_secs(EVENING_STOP_SECS_OF_DAY_IST, 600), None);
        assert_eq!(attempt_budget_secs(window_end - 1, 600), None);
        // A manual evening boot after the stop window runs the configured
        // budget, as before §12.15.8.
        assert_eq!(attempt_budget_secs(window_end, 600), Some(600));
        assert_eq!(attempt_budget_secs(SECS_PER_DAY - 1, 600), Some(600));
        // A nonsense clock never runs, and nothing wraps.
        assert_eq!(attempt_budget_secs(SECS_PER_DAY, 600), None);
        assert_eq!(attempt_budget_secs(u64::MAX, 600), None);
        assert_eq!(attempt_budget_secs(run, u64::MAX), Some(last - run - fixed));
        assert_eq!(attempt_budget_secs(0, 600), Some(600));
    }

    /// Every second of the day, for budgets around every boundary: a granted
    /// budget never exceeds the configured one, an attempt that starts before
    /// the stop window always ends by 17:23, a budget below the floor is only
    /// ever the configured one, and a refusal happens only when the full
    /// configured attempt does not fit.
    #[test]
    fn test_attempt_budget_secs_every_second_of_the_day() {
        let window_start = u64::from(SCHEDULED_STOP_WINDOW_START_SECS_OF_DAY_IST);
        let window_end = u64::from(SCHEDULED_STOP_WINDOW_END_SECS_OF_DAY_IST);
        for config in [0_u64, 1, 60, 119, 120, 121, 599, 600, 601, 3_600, u64::MAX] {
            for now in 0..SECS_PER_DAY {
                match attempt_budget_secs(now, config) {
                    Some(budget) => {
                        assert!(budget <= config, "now={now} config={config}");
                        if now < window_start {
                            assert!(
                                now + attempt_max_secs(budget) <= XVERIFY_LAST_END_SECS_OF_DAY_IST,
                                "now={now} config={config} budget={budget}"
                            );
                        } else {
                            assert!(now >= window_end, "ran inside the stop window: {now}");
                            assert_eq!(budget, config);
                        }
                        if budget < XVERIFY_MIN_ATTEMPT_BUDGET_SECS {
                            assert_eq!(budget, config, "shrunk below the floor: {now}");
                        }
                    }
                    None => {
                        assert!(now < window_end, "an evening boot was refused: {now}");
                        assert!(
                            now.saturating_add(attempt_max_secs(config))
                                > XVERIFY_LAST_END_SECS_OF_DAY_IST,
                            "a fitting attempt was refused: now={now} config={config}"
                        );
                    }
                }
            }
        }
    }

    /// The last attempt is decided once, before it starts, from its latest
    /// possible end (§12.15.8). For every start second from 15:41 to
    /// midnight and every attempt number: an attempt that is not the last
    /// leaves room for the next one at its full budget, however early it
    /// ends; and the decision equals the old rule evaluated at the latest end.
    #[test]
    fn is_last_is_decided_once_from_start_plus_attempt_max() {
        for config in [60_u64, 120, 600, 900] {
            let max = attempt_max_secs(config);
            for start in XVERIFY_RUN_AT_SECS_OF_DAY_IST..SECS_PER_DAY {
                for n in [1_u32, 2, 3, 4, 5, u32::MAX] {
                    let is_last = attempt_is_last(n, start, max);
                    assert_eq!(is_last, retry_delay_secs(n, start + max, max).is_none());
                    if n >= XVERIFY_MAX_ATTEMPTS_PER_DAY {
                        assert!(is_last, "attempt {n} must be the last");
                    }
                    if is_last {
                        continue;
                    }
                    // Any actual end from the start to the latest end.
                    for end in [start, start + max / 2, start + max] {
                        assert!(retry_delay_secs(n, end, max).is_some());
                        let next = end + XVERIFY_RETRY_INTERVAL_SECS;
                        assert!(next + max <= XVERIFY_LAST_END_SECS_OF_DAY_IST);
                        assert_eq!(
                            attempt_budget_secs(next, config),
                            Some(config),
                            "a planned retry must never be shrunk or skipped"
                        );
                    }
                }
            }
        }
    }

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig::with_cases(4_096))]
        /// Random starts, attempt numbers and budgets: a not-last attempt
        /// always leaves a full next attempt that ends by 17:23.
        #[test]
        fn proptest_is_last_never_strands_a_retry(
            start in 0_u64..(2 * SECS_PER_DAY),
            n in 1_u32..=6,
            config in 1_u64..=4_000,
        ) {
            let max = attempt_max_secs(config);
            if !attempt_is_last(n, start, max) {
                proptest::prop_assert!(n < XVERIFY_MAX_ATTEMPTS_PER_DAY);
                let next = start + max + XVERIFY_RETRY_INTERVAL_SECS;
                proptest::prop_assert!(next + max <= XVERIFY_LAST_END_SECS_OF_DAY_IST);
                proptest::prop_assert_eq!(attempt_budget_secs(next, config), Some(config));
            }
        }
    }

    // ---- drive_day against a paused clock (§12.15.8) ----

    /// One simulated attempt: how long it takes (`None` = hangs until its
    /// timeout) and what it returns.
    type Step = (Option<u64>, Result<(), AttemptFailure>);

    /// What one simulated day did.
    struct Sim {
        result: DayResult,
        /// Plans of the attempts that were actually called, in order.
        plans: Vec<AttemptPlan>,
        /// `(attempt number, seconds of day)` for each attempt that finished on
        /// its own; an attempt cut by its timeout has no entry.
        ends: Vec<(u32, u64)>,
        /// Seconds of day when `drive_day` returned.
        finished: u64,
        /// The paused-clock instant of `first_start`.
        t0: tokio::time::Instant,
    }

    fn day() -> chrono::NaiveDate {
        chrono::NaiveDate::from_ymd_opt(2026, 10, 6).unwrap_or_default()
    }

    /// Runs `drive_day` from `first_start` on the paused clock, with attempt
    /// `n` following `script(n, plan)`, and `still_today` false after
    /// `day_changes_after` retry sleeps.
    async fn simulate(
        first_start: u64,
        config_budget: u64,
        script: impl Fn(u32, &AttemptPlan) -> Step,
        day_changes_after: Option<u32>,
    ) -> Sim {
        let t0 = tokio::time::Instant::now();
        let now = move || first_start + t0.elapsed().as_secs();
        let plans = std::cell::RefCell::new(Vec::new());
        let ends = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
        let sleeps = std::cell::Cell::new(0_u32);
        let result = drive_day(
            day(),
            config_budget,
            now,
            || {
                sleeps.set(sleeps.get() + 1);
                day_changes_after.is_none_or(|after| sleeps.get() <= after)
            },
            |plan: AttemptPlan| {
                plans.borrow_mut().push(plan);
                let (duration, outcome) = script(plan.number, &plan);
                let ends = std::rc::Rc::clone(&ends);
                let number = plan.number;
                async move {
                    match duration {
                        Some(secs) => tokio::time::sleep(Duration::from_secs(secs)).await,
                        None => std::future::pending::<()>().await,
                    }
                    ends.borrow_mut().push((number, now()));
                    outcome
                }
            },
        )
        .await;
        let finished = now();
        let ends = ends.borrow().clone();
        Sim {
            result,
            plans: plans.into_inner(),
            ends,
            finished,
            t0,
        }
    }

    /// A step that fails `failures` times, then succeeds instantly.
    fn fail_then_succeed(
        failures: u32,
        fail: impl Fn(u32) -> Step,
    ) -> impl Fn(u32, &AttemptPlan) -> Step {
        move |n, _plan| {
            if n <= failures {
                fail(n)
            } else {
                (Some(0), Ok(()))
            }
        }
    }

    /// When called attempt `i` (0-based) ended: its own record, else (cut by
    /// its timeout) one interval before the next attempt, else when the day
    /// returned (minus the interval when a skipped attempt followed it).
    fn attempt_end(sim: &Sim, i: usize) -> u64 {
        let number = u32::try_from(i).unwrap_or(u32::MAX) + 1;
        if let Some(&(_, end)) = sim.ends.iter().find(|(n, _)| *n == number) {
            return end;
        }
        if let Some(next) = sim.plans.get(i + 1) {
            return next.start_secs_of_day - XVERIFY_RETRY_INTERVAL_SECS;
        }
        if number == sim.result.attempts {
            sim.finished
        } else {
            sim.finished - XVERIFY_RETRY_INTERVAL_SECS
        }
    }

    /// Checks the §12.15.8 invariants on one simulated day.
    fn check_day(sim: &Sim, first_start: u64, config: u64, failures: u32) {
        let stop_window = u64::from(SCHEDULED_STOP_WINDOW_START_SECS_OF_DAY_IST);
        let r = sim.result;
        assert!(r.attempts >= 1 && r.attempts <= XVERIFY_MAX_ATTEMPTS_PER_DAY);
        assert!(!r.day_changed);
        // Every called attempt: budget, end bound, is_last decided once.
        let mut previous_end: Option<u64> = None;
        for (i, plan) in sim.plans.iter().enumerate() {
            let number = u32::try_from(i).unwrap_or(u32::MAX) + 1;
            assert_eq!(plan.number, number);
            if let Some(end) = previous_end {
                assert_eq!(
                    plan.start_secs_of_day,
                    end + XVERIFY_RETRY_INTERVAL_SECS,
                    "a retry starts one interval after the previous end"
                );
            }
            let end = attempt_end(sim, i);
            if !sim.ends.iter().any(|(n, _)| *n == number) {
                // Cut by its timeout: exactly at the attempt's own limit.
                assert_eq!(
                    end,
                    plan.start_secs_of_day + attempt_max_secs(plan.run_budget_secs)
                );
            }
            // §12.15.8: the persist's own stop is the timeout's instant;
            // a marker-only attempt carries none.
            match plan.kind {
                AttemptKind::Full => assert_eq!(
                    plan.deadline,
                    Some(
                        sim.t0
                            + Duration::from_secs(
                                plan.start_secs_of_day - first_start
                                    + attempt_max_secs(plan.run_budget_secs)
                            )
                    )
                ),
                AttemptKind::MarkerOnly => assert_eq!(plan.deadline, None),
            }
            if plan.kind == AttemptKind::Full && plan.start_secs_of_day < stop_window {
                assert!(
                    plan.start_secs_of_day + attempt_max_secs(plan.run_budget_secs)
                        <= XVERIFY_LAST_END_SECS_OF_DAY_IST
                );
                assert!(
                    end <= XVERIFY_LAST_END_SECS_OF_DAY_IST,
                    "attempt {number} from {first_start} ended at {end}"
                );
            }
            if number >= 2 && plan.kind == AttemptKind::Full {
                assert_eq!(plan.run_budget_secs, config, "a retry is never shrunk");
            }
            let final_attempt = number == r.attempts;
            if final_attempt && r.failure.is_some() {
                assert!(plan.is_last, "the paging attempt was not decided last");
            }
            if !final_attempt {
                assert!(
                    !plan.is_last,
                    "an attempt decided last was followed by another"
                );
            }
            previous_end = Some(end);
        }
        // A skipped attempt (no plan) is always the final, last one. With no
        // attempt before it, the day ends `SkippedNoTime` (not paged);
        // after a real failure, with that failure (paged).
        if sim.plans.len() < r.attempts as usize {
            assert_eq!(sim.plans.len() + 1, r.attempts as usize);
            if sim.plans.is_empty() {
                assert_eq!(r.failure, Some(AttemptFailure::SkippedNoTime));
            } else {
                assert_ne!(r.failure, Some(AttemptFailure::SkippedNoTime));
            }
        }
        // Recorded exactly when an attempt after the scripted failures ran.
        assert_eq!(r.failure.is_none(), r.attempts > failures);
        if r.failure.is_none() {
            assert_eq!(r.attempts, failures + 1);
        }
    }

    /// Every start second from 15:41 to 17:23, every failure count 1..=4,
    /// with attempts that hang to their timeout, fail at once, or alternate:
    /// no attempt ends after 17:23, `is_last` is decided once and agrees with
    /// what the loop did, and at most four attempts run.
    #[tokio::test(start_paused = true)]
    async fn test_drive_day_every_start_second_ends_by_the_last_end() {
        let patterns: [fn(u32) -> Step; 4] = [
            |_| (None, Err(AttemptFailure::RunFailed)),
            |_| (Some(0), Err(AttemptFailure::RunFailed)),
            |n| {
                if n % 2 == 1 {
                    (None, Err(AttemptFailure::RunFailed))
                } else {
                    (Some(5), Err(AttemptFailure::NotPersisted))
                }
            },
            // §12.15.7: a marker failure, then marker-only attempts.
            |_| (Some(0), Err(AttemptFailure::MarkerNotWritten)),
        ];
        let mut sims = 0_u32;
        for first_start in XVERIFY_RUN_AT_SECS_OF_DAY_IST..=XVERIFY_LAST_END_SECS_OF_DAY_IST {
            for failures in 1..=XVERIFY_MAX_ATTEMPTS_PER_DAY {
                for pattern in patterns {
                    let sim =
                        simulate(first_start, 600, fail_then_succeed(failures, pattern), None)
                            .await;
                    check_day(&sim, first_start, 600, failures);
                    sims += 1;
                }
            }
        }
        assert_eq!(sims, (62_580 - 56_460 + 1) * 4 * 4);
    }

    /// The worst case on the paused clock: four attempts that each hang to
    /// their timeout start at 15:41:00, 16:09:40, 16:38:20 and 17:07:00, and
    /// the day returns its failure exactly at 17:23.
    #[tokio::test(start_paused = true)]
    async fn test_drive_day_worst_case_hangs_end_exactly_at_the_last_end() {
        let sim = simulate(
            XVERIFY_RUN_AT_SECS_OF_DAY_IST,
            600,
            |_, _| (None, Err(AttemptFailure::RunFailed)),
            None,
        )
        .await;
        let starts: Vec<u64> = sim.plans.iter().map(|p| p.start_secs_of_day).collect();
        assert_eq!(starts, vec![56_460, 58_180, 59_900, 61_620]);
        assert_eq!(sim.result.attempts, 4);
        // A timed-out attempt is incomplete, whatever it would have returned.
        assert_eq!(sim.result.failure, Some(AttemptFailure::Incomplete));
        assert_eq!(sim.finished, XVERIFY_LAST_END_SECS_OF_DAY_IST);
        assert!(sim.ends.is_empty(), "every attempt was cut by its timeout");
        let lasts: Vec<bool> = sim.plans.iter().map(|p| p.is_last).collect();
        assert_eq!(lasts, vec![false, false, false, true]);
    }

    /// A marker-only attempt as the last: it gets the same once-decided
    /// `is_last`, no budget, and its failure is the day's final failure.
    #[tokio::test(start_paused = true)]
    async fn test_drive_day_marker_only_attempt_as_the_last() {
        let sim = simulate(
            XVERIFY_RUN_AT_SECS_OF_DAY_IST,
            600,
            |n, _| match n {
                1 => (None, Err(AttemptFailure::RunFailed)),
                _ => (Some(0), Err(AttemptFailure::MarkerNotWritten)),
            },
            None,
        )
        .await;
        let kinds: Vec<AttemptKind> = sim.plans.iter().map(|p| p.kind).collect();
        assert_eq!(
            kinds,
            vec![
                AttemptKind::Full,
                AttemptKind::Full,
                AttemptKind::MarkerOnly,
                AttemptKind::MarkerOnly
            ]
        );
        let last = sim.plans.last().copied();
        assert_eq!(last.map(|p| p.is_last), Some(true));
        assert_eq!(last.map(|p| p.run_budget_secs), Some(0));
        assert_eq!(sim.result.failure, Some(AttemptFailure::MarkerNotWritten));
        // The same day, the marker-only attempt succeeding records it.
        let ok = simulate(
            XVERIFY_RUN_AT_SECS_OF_DAY_IST,
            600,
            |n, _| match n {
                1 => (Some(0), Err(AttemptFailure::MarkerNotWritten)),
                _ => (Some(0), Ok(())),
            },
            None,
        )
        .await;
        assert_eq!(ok.result.failure, None);
        assert_eq!(ok.result.attempts, 2);
        assert_eq!(
            ok.plans.get(1).map(|p| p.kind),
            Some(AttemptKind::MarkerOnly)
        );
    }

    /// A late boot catch-up: too little time skips the attempt without
    /// calling it; a little more time shrinks the budget and the timeout
    /// follows the shrunk budget; a configured budget below the floor runs.
    #[tokio::test(start_paused = true)]
    async fn test_drive_day_late_start_shrinks_or_skips() {
        let fixed = attempt_max_secs(0);
        let last = XVERIFY_LAST_END_SECS_OF_DAY_IST;
        let skip_at = last - fixed - XVERIFY_MIN_ATTEMPT_BUDGET_SECS + 1;
        let skipped = simulate(skip_at, 600, |_, _| (Some(0), Ok(())), None).await;
        assert!(skipped.plans.is_empty(), "a skipped attempt must not run");
        assert_eq!(skipped.result.attempts, 1);
        assert_eq!(skipped.result.failure, Some(AttemptFailure::SkippedNoTime));
        assert_eq!(skipped.finished, skip_at);

        let shrink_at = last - fixed - 200;
        let shrunk = simulate(shrink_at, 600, |_, _| (None, Ok(())), None).await;
        assert_eq!(shrunk.plans.first().map(|p| p.run_budget_secs), Some(200));
        assert_eq!(shrunk.plans.first().map(|p| p.is_last), Some(true));
        assert_eq!(shrunk.result.failure, Some(AttemptFailure::Incomplete));
        assert_eq!(
            shrunk.finished, last,
            "the timeout follows the shrunk budget"
        );

        let small = simulate(
            XVERIFY_RUN_AT_SECS_OF_DAY_IST,
            60,
            |_, _| (Some(0), Ok(())),
            None,
        )
        .await;
        assert_eq!(small.plans.first().map(|p| p.run_budget_secs), Some(60));
        assert_eq!(small.result.failure, None);

        // An evening boot after the stop window runs once, unshrunk.
        let evening = u64::from(SCHEDULED_STOP_WINDOW_END_SECS_OF_DAY_IST) + 2_820;
        let late = simulate(
            evening,
            600,
            |_, _| (Some(1), Err(AttemptFailure::Vacuous)),
            None,
        )
        .await;
        assert_eq!(late.plans.first().map(|p| p.run_budget_secs), Some(600));
        assert_eq!(late.result.attempts, 1);
        assert_eq!(late.result.failure, Some(AttemptFailure::Vacuous));
    }

    /// §12.15.8: a skip with no attempt before it in this process is
    /// `SkippedNoTime` (logged, not paged: a restart at 17:24 must not page a
    /// day an earlier process already paged). A skip after a real failure in
    /// this process (here forced by a clock jump during the retry sleep) ends
    /// the day with THAT failure, so the page still fires.
    #[tokio::test(start_paused = true)]
    async fn test_drive_day_skip_pages_only_after_an_attempt_that_ran() {
        for start in [62_645_u64, 62_580, XVERIFY_LAST_END_SECS_OF_DAY_IST - 300] {
            let first = drive_day(
                day(),
                600,
                || start,
                || true,
                |_plan: AttemptPlan| async { Ok(()) },
            )
            .await;
            assert_eq!(first.attempts, 1);
            assert_eq!(
                first.failure,
                Some(AttemptFailure::SkippedNoTime),
                "start {start}"
            );
        }
        let readings = std::cell::Cell::new(0_u32);
        let jumping = || {
            readings.set(readings.get() + 1);
            if readings.get() == 1 { 56_460 } else { 62_500 }
        };
        let ran = std::cell::Cell::new(0_u32);
        let after_failure = drive_day(
            day(),
            600,
            jumping,
            || true,
            |_plan: AttemptPlan| {
                ran.set(ran.get() + 1);
                async { Err(AttemptFailure::RunFailed) }
            },
        )
        .await;
        assert_eq!(ran.get(), 1, "the second attempt was skipped, not run");
        assert_eq!(after_failure.attempts, 2);
        assert_eq!(after_failure.failure, Some(AttemptFailure::RunFailed));
    }

    /// The skipped day's final report is a coded `warn!`, not a page.
    #[test]
    fn test_skipped_day_is_logged_not_paged() {
        let body = fn_body(prod_src(), "fn report_final_failure(");
        let arm = body
            .split("AttemptFailure::SkippedNoTime => ")
            .nth(1)
            .expect("report_final_failure must name SkippedNoTime");
        let arm = &arm[..arm.find("AttemptFailure::Vacuous =>").unwrap_or(arm.len())];
        assert!(arm.starts_with("warn!("), "{arm}");
        assert!(arm.contains("source = \"xverify_day_not_attempted\""));
        assert!(!arm.contains("xverify_failed"));
    }

    /// The day changing during a retry sleep stops the loop without a page.
    #[tokio::test(start_paused = true)]
    async fn test_drive_day_day_change_stops_retrying() {
        let sim = simulate(
            XVERIFY_RUN_AT_SECS_OF_DAY_IST,
            600,
            |_, _| (Some(0), Err(AttemptFailure::RunFailed)),
            Some(0),
        )
        .await;
        assert!(sim.result.day_changed);
        assert_eq!(sim.result.attempts, 1);
    }

    /// The timeout can stop an attempt only at an `.await`. Work that runs
    /// synchronously past the limit finishes, and its result stands — success
    /// and failure alike. That is why the audit persist bounds itself to the
    /// same deadline (the `persist_report_into` deadline tests) rather than
    /// relying on this timeout.
    #[tokio::test]
    async fn test_bounded_attempt_keeps_a_result_finished_synchronously_past_the_limit() {
        for outcome in [Ok(()), Err(AttemptFailure::AuditRowsLost)] {
            let at = tokio::time::Instant::now();
            let got = bounded_attempt(day(), 1, 0, at, async move {
                // A persist that is still running when the limit passes.
                std::thread::sleep(Duration::from_millis(20));
                outcome
            })
            .await;
            assert_eq!(got, outcome);
        }
    }

    /// An attempt still waiting at its limit is stopped and is incomplete.
    #[tokio::test(start_paused = true)]
    async fn test_bounded_attempt_times_out_as_incomplete_at_the_limit() {
        let t0 = tokio::time::Instant::now();
        let got = bounded_attempt(day(), 1, 960, t0 + Duration::from_secs(960), async {
            tokio::time::sleep(Duration::from_secs(961)).await;
            Ok(())
        })
        .await;
        assert_eq!(got, Err(AttemptFailure::Incomplete));
        assert_eq!(t0.elapsed(), Duration::from_secs(960));
        // Work that completes exactly at the limit is kept: the attempt is
        // polled before its timer.
        let t1 = tokio::time::Instant::now();
        let at_limit = bounded_attempt(day(), 1, 960, t1 + Duration::from_secs(960), async {
            tokio::time::sleep(Duration::from_secs(960)).await;
            Ok(())
        })
        .await;
        assert_eq!(at_limit, Ok(()));
    }

    /// `run_day` runs every attempt through `drive_day`, which consults
    /// `attempt_budget_secs` and `attempt_is_last` before each attempt and
    /// wraps every full attempt in `tokio::time::timeout`; the option pass has
    /// its own timeout.
    #[test]
    fn test_every_attempt_start_consults_attempt_budget_secs_and_is_wrapped_in_timeout() {
        let prod = prod_src();
        let drive = fn_body(prod, "async fn drive_day<");
        let loop_at = drive.find("loop {").expect("drive_day loops");
        let body = &drive[loop_at..];
        let is_last_at = body.find("attempt_is_last(").expect("is_last decided");
        let kind_at = body
            .find("next_attempt_kind(previous)")
            .expect("kind decided");
        let budget_at = body
            .find("attempt_budget_secs(start")
            .expect("budget decided");
        let bounded_at = body.find("bounded_attempt(").expect("timeout wrapper");
        assert!(is_last_at < kind_at && kind_at < budget_at && budget_at < bounded_at);
        // Never re-decided from the attempt's end.
        let after = &body[bounded_at..];
        assert!(!after.contains("retry_delay_secs("));
        assert!(!after.contains("attempt_is_last("));
        assert!(after.contains("if is_last {"));
        let bounded = fn_body(prod, "async fn bounded_attempt<");
        assert!(bounded.contains("tokio::time::timeout_at(deadline, attempt)"));
        assert!(bounded.contains("source = \"xverify_attempt_timed_out\""));
        assert!(bounded.contains("Err(AttemptFailure::Incomplete)"));
        assert!(drive.contains("source = \"xverify_attempt_skipped_no_time\""));
        let run_day = fn_body(prod, "async fn run_day(");
        assert!(run_day.contains("drive_day("));
        assert!(run_day.contains("run_budget_secs: plan.run_budget_secs"));
        assert!(run_day.contains("tokio::time::timeout_at("));
        // The timeout and the persist's own stop are one instant.
        assert!(
            drive.contains("bounded_attempt(today, attempts, limit_secs, deadline, attempt(plan))")
        );
        assert!(drive.contains("deadline: Some(deadline)"));
        assert!(
            run_day.contains("run_option_pass(deps, today, day_start_ist_nanos, option_deadline)")
        );
        assert!(run_day.contains("source = \"xverify_options_timed_out\""));
        // run_once is reached only through the driver.
        assert_eq!(prod.matches("run_once(deps,").count(), 1);
        let run_once = fn_body(prod, "async fn run_once(");
        assert!(run_once.contains("cfg.tolerance_paise,\n                deadline,"));
        assert!(!run_day.contains("now_ist_secs_of_day()"));
    }

    /// The new lines are coded `warn!`s on sources no alarm filter matches.
    #[test]
    fn test_time_bound_sources_are_coded_and_match_no_alarm_filter() {
        let prod = prod_src();
        let tf = include_str!("../../../deploy/aws/terraform/error-code-alarms.tf");
        for source in [
            "xverify_attempt_timed_out",
            "xverify_attempt_skipped_no_time",
            "xverify_options_timed_out",
            "xverify_persist_stopped_at_deadline",
            "xverify_options_persist_stopped_at_deadline",
            "xverify_day_not_attempted",
        ] {
            let emit = format!("source = \"{source}\"");
            let at = prod
                .find(emit.as_str())
                .unwrap_or_else(|| panic!("no emit of {source}"));
            let head = &prod[..at];
            let warn_at = head.rfind("warn!(").unwrap_or(0);
            assert!(
                warn_at > head.rfind("error!(").unwrap_or(0),
                "{source} is not a warn!"
            );
            assert!(
                head[warn_at..].contains("code = ErrorCode::WsGapConnectionState.code_str()"),
                "{source} carries no code"
            );
            assert!(!tf.contains(source), "{source} must stay log-sink-only");
        }
    }

    /// The retry loop must use the pure bound and page only after it, and
    /// `run_once` must never emit an alarmed source itself except the
    /// once-per-day divergence page.
    #[test]
    fn test_run_day_retries_through_the_pure_bound() {
        let prod = prod_src();
        let drive = fn_body(prod, "async fn drive_day<");
        assert!(drive.contains("attempt_is_last("));
        assert!(drive.contains("XVERIFY_RETRY_INTERVAL_SECS"));
        assert!(!drive.contains("report_final_failure("));
        let body = fn_body(prod, "async fn run_day(");
        assert!(body.contains("report_final_failure("));
        assert!(body.contains("divergence_paged"));
        let run_once = fn_body(prod, "async fn run_once(");
        for alarmed in ["\"xverify_failed\"", "\"xverify_vacuous\""] {
            assert!(
                !run_once.contains(alarmed),
                "run_once must not page {alarmed} per attempt"
            );
        }
        assert!(run_once.contains("!divergence_paged.swap(true, Ordering::Relaxed)"));
        assert!(prod.contains("run_day(&deps, &targets, today, day_start_ist_nanos)"));
    }

    #[test]
    fn test_secs_until_next_run_ist_before_and_after() {
        let run = XVERIFY_RUN_AT_SECS_OF_DAY_IST;
        assert_eq!(secs_until_next_run_ist(run - 60), 60);
        assert_eq!(secs_until_next_run_ist(run), SECS_PER_DAY);
        assert_eq!(secs_until_next_run_ist(run + 60), SECS_PER_DAY - 60);
        assert_eq!(secs_until_next_run_ist(0), run);
    }

    #[test]
    fn test_run_at_ist_hhmm_is_after_the_session_close() {
        assert_eq!(run_at_ist_hhmm(), "15:41");
    }

    #[test]
    fn test_should_catch_up_only_after_run_time_and_without_marker() {
        let run = XVERIFY_RUN_AT_SECS_OF_DAY_IST;
        assert!(!should_catch_up(run - 1, false));
        assert!(should_catch_up(run, false));
        assert!(should_catch_up(run + 3_600, false));
        assert!(!should_catch_up(run + 3_600, true));
    }

    #[test]
    fn test_should_write_marker_needs_a_measured_persisted_comparison() {
        let clean = comparison(DhanLiveXverifyOutcome::Clean, 375, 0);
        assert!(should_write_marker(&clean, true));
        assert!(!should_write_marker(&clean, false));
        let diverged = comparison(DhanLiveXverifyOutcome::Diverged, 375, 12);
        assert!(should_write_marker(&diverged, true));
        let partial = comparison(DhanLiveXverifyOutcome::Partial, 375, 0);
        assert!(should_write_marker(&partial, true));
        for vacuous in [
            DhanLiveXverifyOutcome::NoData,
            DhanLiveXverifyOutcome::Blind,
            DhanLiveXverifyOutcome::Degraded,
        ] {
            assert!(!should_write_marker(&comparison(vacuous, 0, 0), true));
        }
        // A measured outcome with zero compared minutes is still vacuous.
        assert!(!should_write_marker(
            &comparison(DhanLiveXverifyOutcome::Clean, 0, 0),
            true
        ));
    }

    #[test]
    fn test_is_catastrophic_divergence_needs_more_than_half_the_price_fields() {
        // 100 minutes -> 400 price fields.
        assert!(!is_catastrophic_divergence(&comparison(
            DhanLiveXverifyOutcome::Diverged,
            100,
            200
        )));
        assert!(is_catastrophic_divergence(&comparison(
            DhanLiveXverifyOutcome::Diverged,
            100,
            201
        )));
        assert!(!is_catastrophic_divergence(&comparison(
            DhanLiveXverifyOutcome::NoData,
            0,
            0
        )));
    }

    #[test]
    fn test_today_ist_day_start_lands_on_a_day_boundary() {
        let (_, day_start) = today_ist();
        assert_eq!(day_start % (SECS_PER_DAY as i64 * 1_000_000_000), 0);
    }

    #[test]
    fn test_the_fetched_vendor_tape_is_persisted_not_discarded() {
        let src = include_str!("dhan_live_crossverify_boot.rs");
        let body = src
            .split("fn persist_report_into(")
            .nth(1)
            .expect("persist_report_into must exist");
        let end = body.find("\n}\n").unwrap_or(body.len());
        let body = &body[..end];
        assert!(body.contains("append_rest_tape("));
        assert!(body.contains("append_daily("));
        assert!(body.contains("discard_pending()"));
    }

    /// The three `xverify` CloudWatch filters match on `$.source`. If a
    /// source string here drifts from the terraform, that alarm can never
    /// fire again and nothing else would notice, so this pins both sides.
    /// The scan stops at the test module so this test's own literals cannot
    /// satisfy it.
    #[test]
    fn test_every_xverify_alarm_source_has_a_live_error_emit() {
        let src = include_str!("dhan_live_crossverify_boot.rs");
        let prod = src.split("#[cfg(test)]").next().unwrap_or("");
        let tf = include_str!("../../../deploy/aws/terraform/error-code-alarms.tf");
        for source in ["xverify_vacuous", "xverify_failed", "xverify_diverged"] {
            let emit = format!("source = \"{source}\"");
            let hits = prod.matches(emit.as_str()).count();
            assert!(hits >= 1, "no production emit carries {emit}");
            let filter = format!("$.source = \\\"{source}\\\"");
            assert!(
                tf.contains(filter.as_str()),
                "error-code-alarms.tf has no filter for {source}"
            );
            // Every emit of this source must be an `error!`: the filter also
            // requires `$.level = "ERROR"`, so a `warn!` emit is invisible.
            for (at, _) in prod.match_indices(emit.as_str()) {
                let head = &prod[..at];
                let nearest = |m: &str| head.rfind(m).map_or(0, |i| i + 1);
                let error_at = nearest("error!(");
                let other_at = nearest("warn!(")
                    .max(nearest("info!("))
                    .max(nearest("debug!("));
                assert!(
                    error_at > other_at,
                    "{emit} is not inside an error! call; the alarm filter \
                     requires $.level = \"ERROR\" and would never match it"
                );
            }
        }
    }
    // ---- §12.15.6 depth-held option pass ----

    const FNO: u8 = 2;

    fn family_by_parity(id: u64, _seg: ExchangeSegment) -> Option<OptionFamily> {
        match id % 3 {
            0 => Some(OptionFamily::Index),
            1 => Some(OptionFamily::Stock),
            _ => None,
        }
    }

    #[test]
    fn test_option_targets_from_depth_held_maps_family_to_instrument() {
        let held = [(3_u64, FNO), (4_u64, FNO)];
        let built = option_targets_from_depth_held(&held, family_by_parity, 300);
        assert_eq!(built.targets.len(), 2);
        assert_eq!(built.targets[0].security_id, 3);
        assert_eq!(built.targets[0].instrument, "OPTIDX");
        assert_eq!(built.targets[1].security_id, 4);
        assert_eq!(built.targets[1].instrument, "OPTSTK");
        for t in &built.targets {
            assert_eq!(t.segment, "NSE_FNO");
        }
    }

    #[test]
    fn test_option_targets_from_depth_held_skips_non_fno_segments() {
        // NSE_EQ (1) and IDX_I (0) spot keys held on depth-20 are never
        // option contracts and must not be sent as OPTIDX/OPTSTK.
        let held = [(3_u64, 1_u8), (3_u64, 0_u8), (6_u64, FNO)];
        let built = option_targets_from_depth_held(&held, family_by_parity, 300);
        assert_eq!(built.not_fno, 2);
        assert_eq!(built.targets.len(), 1);
        assert_eq!(built.targets[0].security_id, 6);
    }

    #[test]
    fn test_option_targets_from_depth_held_never_guesses_an_unresolved_contract() {
        let held = [(5_u64, FNO), (8_u64, FNO), (9_u64, FNO)];
        let built = option_targets_from_depth_held(&held, family_by_parity, 300);
        assert_eq!(built.unresolved, 2);
        assert_eq!(built.targets.len(), 1);
        assert_eq!(built.targets[0].security_id, 9);
    }

    #[test]
    fn test_option_targets_from_depth_held_refuses_ids_beyond_i64() {
        let held = [(u64::MAX, FNO)];
        let built = option_targets_from_depth_held(&held, |_, _| Some(OptionFamily::Stock), 300);
        assert!(built.targets.is_empty());
        assert_eq!(built.unresolved, 1);
    }

    #[test]
    fn test_option_targets_from_depth_held_sorts_dedups_and_caps() {
        let held = [
            (30_u64, FNO),
            (3, FNO),
            (30, FNO),
            (12, FNO),
            (21, FNO),
            (3, FNO),
        ];
        let built = option_targets_from_depth_held(&held, |_, _| Some(OptionFamily::Index), 2);
        let ids: Vec<i64> = built.targets.iter().map(|t| t.security_id).collect();
        assert_eq!(ids, vec![3, 12]);
        assert_eq!(built.truncated, 2, "21 and 30 are beyond the cap");
        assert_eq!(built.unresolved, 0);
    }

    #[test]
    fn test_option_targets_from_depth_held_empty_input() {
        let built = option_targets_from_depth_held(&[], family_by_parity, 300);
        assert_eq!(built, OptionTargets::default());
    }

    #[test]
    fn test_option_targets_from_depth_held_zero_cap_truncates_everything() {
        let held = [(3_u64, FNO), (6, FNO)];
        let built = option_targets_from_depth_held(&held, family_by_parity, 0);
        assert!(built.targets.is_empty());
        assert_eq!(built.truncated, 2);
    }

    #[test]
    fn test_option_pass_fits_bounds_on_last_end() {
        let need = attempt_max_secs(XVERIFY_OPTION_PASS_BUDGET_SECS);
        let last = XVERIFY_LAST_END_SECS_OF_DAY_IST;
        assert!(option_pass_fits(last - need));
        assert!(!option_pass_fits(last - need + 1));
        assert!(!option_pass_fits(last));
        // §12.15.8: the old 17:30 bound would still have started this one.
        assert!(!option_pass_fits(EVENING_STOP_SECS_OF_DAY_IST - need));
        assert!(!option_pass_fits(EVENING_STOP_SECS_OF_DAY_IST));
        assert!(!option_pass_fits(u64::MAX), "saturating add never wraps");
        assert!(option_pass_fits(0));
    }

    #[test]
    fn test_option_pass_budget_fits_its_target_cap_at_the_pacer() {
        // 300 contracts at the 334 ms REST pacer is ~100 s; the budget must
        // cover that or the pass times out before it compares the tail.
        let needed_ms = XVERIFY_MAX_OPTION_TARGETS as u64
            * crate::dhan_live_crossverify::XVERIFY_REST_MIN_GAP_MS;
        assert!(needed_ms <= XVERIFY_OPTION_PASS_BUDGET_SECS * 1_000);
    }

    fn prod_src() -> &'static str {
        include_str!("dhan_live_crossverify_boot.rs")
            .split("#[cfg(test)]")
            .next()
            .unwrap_or("")
    }

    fn fn_body<'a>(prod: &'a str, header: &str) -> &'a str {
        prod.split(header)
            .nth(1)
            .and_then(|s| s.split("\n}\n").next())
            .unwrap_or("")
    }

    #[test]
    fn test_run_option_pass_runs_after_the_spot_retry_loop_ends() {
        let body = fn_body(prod_src(), "async fn run_day(");
        let call = body.find("run_option_pass(deps, today, day_start_ist_nanos");
        let loop_end = body.find("report_final_failure(");
        assert!(call.is_some(), "run_day must call run_option_pass");
        assert!(loop_end.is_some() && body.contains("drive_day("));
        assert!(
            call > loop_end,
            "the option pass must run after the spot loop, never inside a retry"
        );
        let run_once = fn_body(prod_src(), "async fn run_once(");
        assert!(!run_once.contains("run_option_pass"));
    }

    #[test]
    fn test_run_option_pass_never_touches_the_marker_the_daily_row_or_a_page() {
        let prod = prod_src();
        for body in [
            fn_body(prod, "async fn run_option_pass("),
            fn_body(prod, "fn persist_option_findings("),
        ] {
            assert!(!body.is_empty());
            assert!(!body.contains("write_daily_marker"));
            assert!(!body.contains("record_day("));
            assert!(!body.contains("persist_report_into("));
            assert!(!body.contains("append_daily"));
            assert!(!body.contains("daily_row("));
            for alarmed in [
                "source = \"xverify_vacuous\"",
                "source = \"xverify_failed\"",
                "source = \"xverify_diverged\"",
            ] {
                assert!(
                    !body.contains(alarmed),
                    "option pass must not page {alarmed}"
                );
            }
            assert!(!body.contains("XVERIFY_RUNS_COUNTER"));
        }
    }

    #[test]
    fn test_run_option_pass_reads_the_held_today_snapshot_and_the_contract_map() {
        let body = fn_body(prod_src(), "async fn run_option_pass(");
        assert!(body.contains("held_today_snapshot("));
        assert!(body.contains("global_contract_underlying_map()"));
        assert!(body.contains("option_pass_fits("));
        assert!(body.contains("XVERIFY_OPTION_PASS_BUDGET_SECS"));
    }

    #[test]
    fn test_option_pass_outcome_marks_an_incomplete_run_partial() {
        assert_eq!(option_pass_outcome(true, false, 0), "vacuous");
        assert_eq!(option_pass_outcome(true, true, 9), "vacuous");
        assert_eq!(option_pass_outcome(false, false, 0), "measured");
        assert_eq!(option_pass_outcome(false, true, 0), "partial");
        assert_eq!(option_pass_outcome(false, false, 1), "partial");
    }

    #[test]
    fn test_every_option_pass_label_is_seeded_and_every_published_label_is_listed() {
        let body = fn_body(prod_src(), "async fn run_option_pass(");
        assert!(
            body.contains("for label in XVERIFY_OPTION_PASS_OUTCOMES"),
            "the pass must seed every label at zero"
        );
        assert!(body.contains("option_pass_outcome("));
        // Every literal label the pass publishes must be in the seeded list.
        let marker = "XVERIFY_OPTION_PASS_COUNTER, \"outcome\" => \"";
        let mut rest = body;
        let mut found = 0;
        while let Some(at) = rest.find(marker) {
            let after = &rest[at + marker.len()..];
            let end = after.find('"').expect("closing quote");
            let label = &after[..end];
            assert!(
                XVERIFY_OPTION_PASS_OUTCOMES.contains(&label),
                "published label {label} is not seeded"
            );
            found += 1;
            rest = &after[end..];
        }
        assert!(found >= 5, "scan found only {found} literal labels");
        // §12.15.8: `run_day` publishes `timed_out` for the pass.
        let run_day = fn_body(prod_src(), "async fn run_day(");
        let at = run_day
            .find(marker)
            .expect("run_day publishes the timeout label");
        let after = &run_day[at + marker.len()..];
        let label = &after[..after.find('"').unwrap_or(0)];
        assert_eq!(label, "timed_out");
        assert!(XVERIFY_OPTION_PASS_OUTCOMES.contains(&label));
        assert_eq!(run_day.matches(marker).count(), 1);
        for label in ["vacuous", "measured", "partial"] {
            assert!(XVERIFY_OPTION_PASS_OUTCOMES.contains(&label));
        }
    }
}
