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
use tickvault_storage::dhan_live_crossverify_persistence::{
    DhanLiveXverifyAuditWriter, DhanLiveXverifyNotReady,
};
use tickvault_storage::seal_spill::SpillStaged;
use tickvault_storage::wal_suspension_watcher::{AppliedThrough, WalTableRow};
use tracing::{error, info, warn};

use crate::daily_task_marker::{
    MarkerDurability, daily_marker_exists, daily_marker_path, try_write_daily_marker,
    try_write_daily_marker_keeping,
};
use crate::dhan_live_crossverify::{
    AttemptStamp, DayComparison, DhanLiveCrossverifyConfig, LIVE_FINAL_SECS_OF_DAY_IST,
    LateWindowPolicy, MissingJudgeable, RUN_SECS_OF_DAY_IST, ReadPolicy, RunReport,
    SESSION_CLOSE_SECS_OF_DAY_IST, XverifyTarget, daily_row, deterministic_run_ts_nanos,
    fetched_at_ist_nanos_now, run_cross_verification,
};
use crate::shutdown_class::{
    SCHEDULED_STOP_WINDOW_END_SECS_OF_DAY_IST, SCHEDULED_STOP_WINDOW_START_SECS_OF_DAY_IST,
};
use crate::volume_leaderboard::OptionFamily;

/// Marker task name. The S3 archive gate in `main.rs` reads the same constant,
/// so the writer and the reader can never disagree about the file name.
pub const CROSSVERIFY_MARKER_TASK: &str = "dhan_live_crossverify";

/// Paged-day marker task name (2026-10-06, §12.15.8, 51b review). Written
/// after every page of a final failure, so the day is paged once across
/// processes and a day whose only attempt was skipped for time is paged
/// exactly when no page went out today. Kept the default 7 days: it is read
/// only on the day it names.
pub const CROSSVERIFY_PAGED_MARKER_TASK: &str = "dhan_live_crossverify_paged";

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
/// Persist failures (the final flush was refused). A §2.10 `audit_rows`
/// member: every increment pages `tv-<env>-audit-rows-lost`, so a deliberate
/// stop at the deadline never touches it (§12.15.8).
pub const XVERIFY_PERSIST_ERRORS_COUNTER: &str = "tv_dhan_feed_xverify_persist_errors_total";
/// Audit persists stopped on purpose at their deadline, by `pass` (`spot`,
/// `options`). Local `/metrics` only: no EMF name, filter or alarm reads it
/// (§12.15.8, 51b review). The day's last attempt still pages
/// `xverify_failed` (`reason = "not_persisted"`).
pub const XVERIFY_PERSIST_DEADLINE_STOPS_COUNTER: &str =
    "tv_dhan_xverify_persist_deadline_stops_total";
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

/// 17:45 IST — when the start watchdog's `stop_check` fires on weekdays
/// (`deploy/aws/terraform/start-watchdog-lambda.tf`,
/// `cron(15 12 ? * MON-FRI *)`). It stops any box launched before the 17:30
/// trigger and does NOT read the keep-alive override. Pinned to that cron by
/// `test_evening_start_follows_the_start_watchdog_stop_check_and_curfew`
/// (§12.15.8, second 51b review).
pub const START_WATCHDOG_STOP_CHECK_SECS_OF_DAY_IST: u64 = 17 * 3_600 + 45 * 60;

/// Room after the `stop_check` fires for its stop to land (EventBridge
/// delivery, the Lambda's StopInstances call, the guest shutdown) before the
/// evening attempt starts. That 300 s is enough is Assumed (§12.15.8).
pub const XVERIFY_STOP_CHECK_MARGIN_SECS: u64 = 300;

/// 17:50 IST — the earliest an unshrunk attempt starts after the 17:23 end
/// bound: the evening attempt of a day whose only attempt was skipped for
/// time, and a manual evening boot (§12.15.8). Before the second 51b review
/// this was 17:45 (`SCHEDULED_STOP_WINDOW_END_SECS_OF_DAY_IST`), the instant
/// the `stop_check` fires, so the attempt raced the stop. Derived, never a
/// literal.
pub const XVERIFY_EVENING_START_SECS_OF_DAY_IST: u64 =
    START_WATCHDOG_STOP_CHECK_SECS_OF_DAY_IST + XVERIFY_STOP_CHECK_MARGIN_SECS;

/// How many times the evening wait re-sleeps when the wall clock reads short
/// of [`XVERIFY_EVENING_START_SECS_OF_DAY_IST`] after the first sleep (a clock
/// stepped back during the wait; 51b review). Bounded so a clock that keeps
/// stepping back cannot hold the loop.
const XVERIFY_EVENING_RESLEEP_MAX: u32 = 3;

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
        && (SCHEDULED_STOP_WINDOW_END_SECS_OF_DAY_IST as u64)
            <= START_WATCHDOG_STOP_CHECK_SECS_OF_DAY_IST
        && START_WATCHDOG_STOP_CHECK_SECS_OF_DAY_IST < XVERIFY_EVENING_START_SECS_OF_DAY_IST
        && XVERIFY_EVENING_START_SECS_OF_DAY_IST < SECS_PER_DAY,
    "15:41 < 17:23 < 17:24 < 17:25 < 17:30 < 17:45 <= 17:45 < 17:50 < midnight"
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
    /// little time was left before 17:23 IST, and the one evening attempt at
    /// 17:50 could not start either (§12.15.8). Pages `xverify_failed` unless
    /// today's paged marker shows a page already went out.
    SkippedNoTime,
    /// The live side was not known final before the read: the catch-up floor
    /// was still moving below the close, or a strict read of an unknown
    /// completeness found late minutes missing (§12.15.10). Only the vendor
    /// tape was kept.
    LiveNotFinal,
    /// QuestDB had not applied `candles_1m` through the snapshot taken after
    /// the drain (§12.15.10). Nothing was read.
    LiveNotApplied,
    /// The seal writer had not drained after the last catch-up sweep, or a
    /// seal spill file for today was staged (§12.15.10). Nothing was read.
    SealsPending,
    /// A seal spill file the mid-session replay parked is still on disk
    /// (§12.15.10). Nothing was read.
    SealSpillParked,
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
            Self::LiveNotFinal => "live_not_final",
            Self::LiveNotApplied => "live_not_applied",
            Self::SealsPending => "seals_pending",
            Self::SealSpillParked => "seal_spill_parked",
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
/// - At or after [`XVERIFY_EVENING_START_SECS_OF_DAY_IST`] (17:50, five minutes
///   after the 17:45 start-watchdog `stop_check`; a manual evening boot) and
///   before midnight: the configured budget, unchanged.
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
    if now_secs_of_day >= XVERIFY_EVENING_START_SECS_OF_DAY_IST {
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
    /// daily row included).
    pub rows_not_written_at_deadline: usize,
    /// Rows already buffered when the persist stopped at the deadline,
    /// abandoned on purpose (`abandon_pending`), never counted as discarded.
    pub rows_abandoned_at_deadline: usize,
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

// ── §12.15.10 (plan item 51d) — read only after the live side is final ──

/// What the live side published about its catch-up seal: the floor cutoff of
/// the last COMPLETED sweep (fold seconds, the IST wall clock read as epoch)
/// and the UTC second it completed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SealProgress {
    /// Every bucket ending at or before this second was sealed by the sweep.
    pub floor_fold_secs: u32,
    /// When that sweep completed, UTC unix seconds.
    pub done_unix_secs: u32,
}

/// Whether the last session buckets are sealed (§12.15.10).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Completeness {
    /// A sweep completed at or after the live-final instant with a floor at
    /// or past the close: every session bucket is sealed.
    Final,
    /// The floor stayed below the close, unchanged from the first sweep after
    /// the live-final instant to the deadline: the feed's newest trade stamp
    /// stopped, so the buckets after it cannot seal before the shutdown seal.
    Frozen {
        /// The floor, as seconds of the IST day.
        sealed_through_secs_of_day: i64,
    },
    /// The floor was below the close and still moving at the deadline.
    Moving {
        /// The latest floor, as seconds of the IST day.
        sealed_through_secs_of_day: i64,
    },
    /// No sweep of today completed at or after the live-final instant.
    Unknown,
}

/// Whether the sealed bars are readable in QuestDB (§12.15.10).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Durability {
    /// The seal writer drained after the reference sweep, no spill file for
    /// today is staged, and `candles_1m` applied its WAL through a snapshot
    /// taken after both.
    Applied,
    /// One of the three barriers did not hold by the deadline.
    NotReady(DhanLiveXverifyNotReady),
}

/// What the readiness wait found (§12.15.10).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LiveReadiness {
    pub durability: Durability,
    pub completeness: Completeness,
}

impl LiveReadiness {
    /// For a day that is no longer today: nothing more can be folded into
    /// it, and the readiness wait reads only today's progress.
    pub const PAST_DAY: Self = Self {
        durability: Durability::Applied,
        completeness: Completeness::Final,
    };
}

/// How one attempt reads the live side, decided before the read (§12.15.10).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadPlan {
    /// Read and compare with `policy`. `check_late` is the not-last
    /// `Unknown` case: if the strict comparison finds a traded or index
    /// minute missing in the late window, keep only the vendor tape and
    /// retry `LiveNotFinal`.
    Read {
        policy: ReadPolicy,
        check_late: bool,
    },
    /// Do not read: retry later today for this reason. No vendor call, no
    /// persist, no marker, no page.
    Retry(AttemptFailure),
}

/// The retry reason for a durability barrier that did not hold. O(1).
const fn retry_reason(reason: DhanLiveXverifyNotReady) -> AttemptFailure {
    match reason {
        DhanLiveXverifyNotReady::SealsPending => AttemptFailure::SealsPending,
        DhanLiveXverifyNotReady::SealSpillParked => AttemptFailure::SealSpillParked,
        DhanLiveXverifyNotReady::NotApplied => AttemptFailure::LiveNotApplied,
        DhanLiveXverifyNotReady::CompletenessUnknown => AttemptFailure::LiveNotFinal,
    }
}

/// The read plan for one attempt (§12.15.10). Pure, total, O(1).
///
/// | Durability | Completeness | Not the last attempt | The last attempt |
/// |---|---|---|---|
/// | not ready | any | retry (its reason) | read, missing minutes unjudged |
/// | applied | `Final` | read `Strict` | read `Strict` |
/// | applied | `Frozen` | read, excuse after the floor | the same |
/// | applied | `Moving` | retry `live_not_final` | read, excuse after the latest floor |
/// | applied | `Unknown` | read `Strict`, then the late check | read, missing minutes unjudged |
///
/// The last attempt never retries: `is_last` is decided once, before the
/// attempt ([`attempt_is_last`]), and a not-ready read there judges prices
/// only. An excuse holds the day at `partial`, never `clean`.
#[must_use]
pub fn decide_read(readiness: LiveReadiness, is_last: bool) -> ReadPlan {
    let unjudged = |reason| ReadPlan::Read {
        policy: ReadPolicy {
            late: LateWindowPolicy::Strict,
            not_ready: Some(reason),
        },
        check_late: false,
    };
    let read = |late| ReadPlan::Read {
        policy: ReadPolicy {
            late,
            not_ready: None,
        },
        check_late: false,
    };
    match (readiness.durability, readiness.completeness) {
        (Durability::NotReady(reason), _) => {
            if is_last {
                unjudged(reason)
            } else {
                ReadPlan::Retry(retry_reason(reason))
            }
        }
        (Durability::Applied, Completeness::Final) => read(LateWindowPolicy::Strict),
        (
            Durability::Applied,
            Completeness::Frozen {
                sealed_through_secs_of_day,
            },
        ) => read(LateWindowPolicy::excuse_after(sealed_through_secs_of_day)),
        (
            Durability::Applied,
            Completeness::Moving {
                sealed_through_secs_of_day,
            },
        ) => {
            if is_last {
                read(LateWindowPolicy::excuse_after(sealed_through_secs_of_day))
            } else {
                ReadPlan::Retry(AttemptFailure::LiveNotFinal)
            }
        }
        (Durability::Applied, Completeness::Unknown) => {
            if is_last {
                unjudged(DhanLiveXverifyNotReady::CompletenessUnknown)
            } else {
                ReadPlan::Read {
                    policy: ReadPolicy::STRICT,
                    check_late: true,
                }
            }
        }
    }
}

/// The first durability barrier as the readiness wait applies it
/// (§12.15.10). Pure, O(1).
///
/// - `drained` must be STRICTLY after `reference_unix_secs` (`>= T + 1`): a
///   drained sample in the same second as the sweep may predate the seals
///   the sweep handed off.
/// - then no spill file for today may be staged; a parked replay file reads
///   as its own reason. A folder that cannot be listed reads as pending
///   (fail closed).
///
/// `Ok` means: take or check the WAL snapshot.
pub fn classify_durability(
    drained_unix_secs: Option<i64>,
    reference_unix_secs: i64,
    spill: &std::io::Result<SpillStaged>,
) -> Result<(), DhanLiveXverifyNotReady> {
    match drained_unix_secs {
        Some(drained) if drained > reference_unix_secs => {}
        _ => return Err(DhanLiveXverifyNotReady::SealsPending),
    }
    match spill {
        Ok(s) if s.staged == 0 => Ok(()),
        Ok(s) if s.parked > 0 => Err(DhanLiveXverifyNotReady::SealSpillParked),
        Ok(_) | Err(_) => Err(DhanLiveXverifyNotReady::SealsPending),
    }
}

/// The QuestDB table the comparison reads.
const LIVE_READ_TABLE: &str = "candles_1m";

/// Seconds between readiness samples.
pub const READINESS_POLL_SECS: u64 = 5;

/// The day the readiness wait is for, as the clocks it compares read it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ReadinessDay {
    /// IST midnight, as fold seconds.
    day_start_fold_secs: i64,
    /// The close, as fold seconds.
    close_fold_secs: i64,
    /// The live-final instant, as UTC unix seconds.
    live_final_unix_secs: i64,
}

impl ReadinessDay {
    fn new(day_start_ist_nanos: i64) -> Self {
        let day_start_fold_secs = day_start_ist_nanos.div_euclid(1_000_000_000);
        Self {
            day_start_fold_secs,
            close_fold_secs: day_start_fold_secs.saturating_add(SESSION_CLOSE_SECS_OF_DAY_IST),
            live_final_unix_secs: day_start_fold_secs
                .saturating_add(LIVE_FINAL_SECS_OF_DAY_IST)
                .saturating_sub(tickvault_common::constants::IST_UTC_OFFSET_SECONDS_I64),
        }
    }
}

/// The completeness the samples support, or `None` while it is not yet
/// decided (§12.15.10). `first` is the first sample whose sweep completed at
/// or after the live-final instant; `latest` the newest. Pure, O(1).
///
/// `Final` is decided as soon as it holds. Everything else waits for the
/// deadline: `Unknown` when there is no sample, the floor is not today's, or
/// no sweep completed at or after the live-final instant; else `Frozen` when
/// the floor did not change since `first`, else `Moving`.
fn classify_completeness(
    day: ReadinessDay,
    first: Option<SealProgress>,
    latest: Option<SealProgress>,
    at_deadline: bool,
) -> Option<Completeness> {
    let Some(latest) = latest else {
        return at_deadline.then_some(Completeness::Unknown);
    };
    let floor = i64::from(latest.floor_fold_secs);
    let done = i64::from(latest.done_unix_secs);
    let today = floor >= day.day_start_fold_secs
        && floor < day.day_start_fold_secs.saturating_add(SECS_PER_DAY as i64);
    if today && floor >= day.close_fold_secs && done >= day.live_final_unix_secs {
        return Some(Completeness::Final);
    }
    if !at_deadline {
        return None;
    }
    if !today || done < day.live_final_unix_secs {
        return Some(Completeness::Unknown);
    }
    let sealed_through_secs_of_day = floor.saturating_sub(day.day_start_fold_secs);
    Some(match first {
        Some(f) if f.floor_fold_secs == latest.floor_fold_secs => Completeness::Frozen {
            sealed_through_secs_of_day,
        },
        _ => Completeness::Moving {
            sealed_through_secs_of_day,
        },
    })
}

/// Everything the readiness wait reads, injected so it is testable against a
/// paused clock (§12.15.10).
pub trait ReadinessSource {
    /// The last completed catch-up sweep, if any.
    fn seal_progress(&self) -> Option<SealProgress>;
    /// [`tickvault_storage::seal_writer_loop::last_seal_drained_unix_secs`].
    fn last_drained_unix_secs(&self) -> Option<i64>;
    /// Seal spill files still staged for `date`. Lists a folder, so the
    /// production source runs it on the blocking pool, off the runtime's
    /// workers.
    fn staged_spill(
        &self,
        date: chrono::NaiveDate,
    ) -> impl Future<Output = std::io::Result<SpillStaged>> + Send;
    /// One `wal_tables()` read.
    fn wal_tables(&self) -> impl Future<Output = anyhow::Result<Vec<WalTableRow>>> + Send;
    /// The wall clock, UTC unix seconds.
    fn now_unix_secs(&self) -> i64;
}

/// The production [`ReadinessSource`].
struct ProductionReadiness<'a> {
    client: reqwest::Client,
    exec_url: &'a str,
}

impl ReadinessSource for ProductionReadiness<'_> {
    fn seal_progress(&self) -> Option<SealProgress> {
        crate::dhan_feed_stack::CATCHUP_PROGRESS
            .load()
            .map(|(floor_fold_secs, done_unix_secs)| SealProgress {
                floor_fold_secs,
                done_unix_secs,
            })
    }

    fn last_drained_unix_secs(&self) -> Option<i64> {
        tickvault_storage::seal_writer_loop::last_seal_drained_unix_secs()
    }

    async fn staged_spill(&self, date: chrono::NaiveDate) -> std::io::Result<SpillStaged> {
        tokio::task::spawn_blocking(move || {
            tickvault_storage::seal_spill::staged_production_spill_records_for_day(date)
        })
        .await
        .unwrap_or_else(|join| Err(std::io::Error::other(join)))
    }

    fn wal_tables(&self) -> impl Future<Output = anyhow::Result<Vec<WalTableRow>>> + Send {
        tickvault_storage::wal_suspension_watcher::fetch_wal_tables(&self.client, self.exec_url)
    }

    fn now_unix_secs(&self) -> i64 {
        chrono::Utc::now().timestamp()
    }
}

/// The readiness wait's running state (§12.15.10).
#[derive(Debug, Clone, Copy)]
struct ReadinessTracker {
    day: ReadinessDay,
    /// The first sample whose sweep completed at or after the live-final
    /// instant, kept across the day's attempts in this process.
    first: Option<SealProgress>,
    latest: Option<SealProgress>,
    /// The first sample carrying the latest floor value: its sweep sealed
    /// the newest bars, so the drain must come after it. Once a floor at or
    /// past the close is seen, it stays at that sample.
    floor_since: Option<SealProgress>,
    /// `candles_1m`'s `sequencerTxn`, snapshotted once after the drain and
    /// spill barriers held, and dropped whenever either fails again.
    snapshot_txn: Option<i64>,
    /// The last durability verdict.
    durability: Durability,
    /// The newest sample all three barriers held for: its bars are saved.
    applied_at: Option<SealProgress>,
}

impl ReadinessTracker {
    fn new(day: ReadinessDay, prior_first: Option<SealProgress>) -> Self {
        Self {
            day,
            first: prior_first,
            latest: None,
            floor_since: None,
            snapshot_txn: None,
            durability: Durability::NotReady(DhanLiveXverifyNotReady::SealsPending),
            applied_at: None,
        }
    }

    /// Whether `floor` is at or past today's close.
    fn past_close(&self, floor: u32) -> bool {
        let floor = i64::from(floor);
        floor >= self.day.close_fold_secs
            && floor
                < self
                    .day
                    .day_start_fold_secs
                    .saturating_add(SECS_PER_DAY as i64)
    }

    /// Records one progress sample. A new floor value re-arms the durability
    /// barriers, since its sweep sealed bars the earlier drain did not cover;
    /// a floor that moves on past the close does not, because every session
    /// bucket was already sealed by the sweep that crossed it.
    fn observe(&mut self, sample: Option<SealProgress>) {
        let Some(p) = sample else {
            return;
        };
        let crossed = self
            .floor_since
            .is_some_and(|s| self.past_close(s.floor_fold_secs));
        let moved = self.floor_since.map(|s| s.floor_fold_secs) != Some(p.floor_fold_secs);
        if moved && !(crossed && self.past_close(p.floor_fold_secs)) {
            self.floor_since = Some(p);
            self.snapshot_txn = None;
            self.durability = Durability::NotReady(DhanLiveXverifyNotReady::SealsPending);
        }
        if self.first.is_none() && i64::from(p.done_unix_secs) >= self.day.live_final_unix_secs {
            self.first = Some(p);
        }
        self.latest = Some(p);
    }

    /// The instant the drain must come after: the sweep that first reached
    /// the latest floor, never earlier than the live-final instant.
    fn reference_unix_secs(&self) -> i64 {
        self.floor_since
            .map_or(i64::MIN, |s| i64::from(s.done_unix_secs))
            .max(self.day.live_final_unix_secs)
    }

    /// The sample whose bars are known saved: `floor_since` while the
    /// barriers hold.
    fn mark_applied(&mut self) {
        self.applied_at = self.floor_since.or(self.latest);
    }
}

/// Waits until the live side is final or `deadline` passes, and reports what
/// it found (§12.15.10). Runs beside the token wait under the same deadline,
/// so an attempt's longest duration is unchanged.
///
/// Sleeps until the live-final instant (a no-op on a retry), then samples
/// every [`READINESS_POLL_SECS`]: the published catch-up progress, then the
/// three durability barriers in order (the seal writer drained after the
/// reference sweep; no spill file for today staged; one `candles_1m`
/// `sequencerTxn` snapshot, then `writerTxn` at or past it). Returns as soon
/// as completeness is decided and durability holds; otherwise at the deadline
/// with what it has. A `wal_tables()` error or timeout reads `NotApplied`.
///
/// At the deadline, when the barriers do not hold for the newest floor but
/// held for an earlier one, it reports that earlier floor as applied: its
/// bars are saved, and the minutes after it are judged as not yet sealed.
/// `prior_first` is the day's first sample after the live-final instant from
/// an earlier attempt; the returned one is kept for the next.
///
/// O(tables + spill files) per sample, at most one sample per
/// [`READINESS_POLL_SECS`], cold.
async fn wait_live_final<R: ReadinessSource>(
    src: &R,
    today: chrono::NaiveDate,
    day_start_ist_nanos: i64,
    deadline: tokio::time::Instant,
    prior_first: Option<SealProgress>,
) -> (LiveReadiness, Option<SealProgress>) {
    let day = ReadinessDay::new(day_start_ist_nanos);
    let early = day.live_final_unix_secs.saturating_sub(src.now_unix_secs());
    if early > 0 {
        let wake = tokio::time::Instant::now() + Duration::from_secs(early.unsigned_abs());
        tokio::time::sleep_until(wake.min(deadline)).await;
    }
    let mut t = ReadinessTracker::new(day, prior_first);
    loop {
        t.observe(src.seal_progress());
        if t.durability != Durability::Applied {
            // A listing that does not finish by the deadline reads pending.
            let spill = tokio::time::timeout_at(deadline, src.staged_spill(today))
                .await
                .unwrap_or_else(|_| Err(std::io::ErrorKind::TimedOut.into()));
            t.durability = match classify_durability(
                src.last_drained_unix_secs(),
                t.reference_unix_secs(),
                &spill,
            ) {
                Err(reason) => {
                    // A snapshot taken before this failure may predate seals
                    // written since; take a fresh one once it clears.
                    t.snapshot_txn = None;
                    Durability::NotReady(reason)
                }
                Ok(()) => match tokio::time::timeout_at(deadline, src.wal_tables()).await {
                    Ok(Ok(rows)) => {
                        let target = match t.snapshot_txn {
                            Some(txn) => Some(txn),
                            None => rows
                                .iter()
                                .find(|r| r.name == LIVE_READ_TABLE)
                                .and_then(|r| r.sequencer_txn),
                        };
                        t.snapshot_txn = target;
                        match target.map(|txn| {
                            tickvault_storage::wal_suspension_watcher::wal_applied_through(
                                &rows,
                                LIVE_READ_TABLE,
                                txn,
                            )
                        }) {
                            Some(AppliedThrough::Reached) => Durability::Applied,
                            _ => Durability::NotReady(DhanLiveXverifyNotReady::NotApplied),
                        }
                    }
                    Ok(Err(_)) | Err(_) => {
                        Durability::NotReady(DhanLiveXverifyNotReady::NotApplied)
                    }
                },
            };
            if t.durability == Durability::Applied {
                t.mark_applied();
            }
        }
        let now = tokio::time::Instant::now();
        let at_deadline = now >= deadline;
        let (durability, latest) = match (t.durability, t.applied_at) {
            (Durability::NotReady(_), Some(saved)) if at_deadline => {
                (Durability::Applied, Some(saved))
            }
            (d, _) => (d, t.latest),
        };
        if let Some(completeness) = classify_completeness(day, t.first, latest, at_deadline)
            && (durability == Durability::Applied || at_deadline)
        {
            return (
                LiveReadiness {
                    durability,
                    completeness,
                },
                t.first,
            );
        }
        if at_deadline {
            // Unreachable: `classify_completeness` decides at the deadline.
            return (
                LiveReadiness {
                    durability,
                    completeness: Completeness::Unknown,
                },
                t.first,
            );
        }
        tokio::time::sleep_until((now + Duration::from_secs(READINESS_POLL_SECS)).min(deadline))
            .await;
    }
}

/// The day's first catch-up sample after the live-final instant, kept across
/// the day's attempts in this process so `Frozen` means "unchanged since that
/// sample", not "unchanged during this attempt" (§12.15.10). Keyed by the
/// day's IST midnight in fold seconds. A restart starts it again.
static FIRST_AFTER_LIVE_FINAL: std::sync::Mutex<Option<(i64, SealProgress)>> =
    std::sync::Mutex::new(None);

/// The readiness for one attempt: the wait for today, or
/// [`LiveReadiness::PAST_DAY`] when the day checked is no longer today.
async fn attempt_readiness(
    deps: &CrossverifyBootDeps,
    today: chrono::NaiveDate,
    day_start_ist_nanos: i64,
    deadline: tokio::time::Instant,
) -> LiveReadiness {
    if today_ist().0 != today {
        return LiveReadiness::PAST_DAY;
    }
    // `Client::default()` panics when the client cannot be built, and the
    // process aborts on a panic, so a build failure reads not applied.
    let Ok(client) = reqwest::Client::builder()
        .timeout(Duration::from_secs(READINESS_PROBE_TIMEOUT_SECS))
        .build()
    else {
        return LiveReadiness {
            durability: Durability::NotReady(DhanLiveXverifyNotReady::NotApplied),
            completeness: Completeness::Unknown,
        };
    };
    let src = ProductionReadiness {
        client,
        exec_url: &deps.questdb_exec_url,
    };
    let day_key = day_start_ist_nanos.div_euclid(1_000_000_000);
    let prior = FIRST_AFTER_LIVE_FINAL
        .lock()
        .ok()
        .and_then(|slot| *slot)
        .and_then(|(key, p)| (key == day_key).then_some(p));
    let (readiness, first) =
        wait_live_final(&src, today, day_start_ist_nanos, deadline, prior).await;
    if let (Some(p), Ok(mut slot)) = (first, FIRST_AFTER_LIVE_FINAL.lock()) {
        *slot = Some((day_key, p));
    }
    readiness
}

/// One `wal_tables()` probe's own timeout during the readiness wait.
const READINESS_PROBE_TIMEOUT_SECS: u64 = 10;

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
/// an earlier attempt in this process did run and fail, the day ends with
/// THAT failure, so it pages as usual. When nothing ran before it in this
/// process (a start between about 17:15 and 17:50), the day does not end
/// there (§12.15.8, 51b review): `on_first_skip` reports the day at once,
/// BEFORE any wait, because the scheduled stop may end the process first
/// (`run_day` pages it unless today's paged marker shows a page already went
/// out); then the loop sleeps until
/// [`XVERIFY_EVENING_START_SECS_OF_DAY_IST`] (17:50) and, on the same IST
/// day, makes ONE full attempt with the configured budget. The skipped
/// attempt is not counted, so that attempt is number 1 and the last. Two
/// scheduled stops act on a weekday box before 17:50: the start watchdog's
/// 17:45 `stop_check` (any box launched before 17:30, keep-alive ignored) and
/// its hourly `curfew_check` (17:35: no keep-alive, past the launch grace).
/// The attempt therefore starts five minutes after the `stop_check`, never at
/// the same instant: a weekday box launched before 17:30 is stopped during
/// the wait and the attempt never starts (the day was already reported); a
/// box started by hand after 17:30, or a weekend session kept up by
/// keep-alive, runs it (second 51b review). The wait happens at most once; a
/// second skip ends the day [`AttemptFailure::SkippedNoTime`].
/// O(1) per attempt, at most [`XVERIFY_MAX_ATTEMPTS_PER_DAY`] attempts and
/// one evening wait.
async fn drive_day<N, S, K, A, Fut>(
    today: chrono::NaiveDate,
    config_budget_secs: u64,
    mut now_secs_of_day: N,
    mut still_today: S,
    mut on_first_skip: K,
    mut attempt: A,
) -> DayResult
where
    N: FnMut() -> u64,
    S: FnMut() -> bool,
    K: FnMut(),
    A: FnMut(AttemptPlan) -> Fut,
    Fut: Future<Output = Result<(), AttemptFailure>>,
{
    let max_attempt_secs = attempt_max_secs(config_budget_secs);
    let mut attempts: u32 = 0;
    let mut previous: Option<AttemptFailure> = None;
    let mut waited_for_evening = false;
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
                    // Capped at a day (51b review): an evening budget is not
                    // shrunk, and `Instant + Duration` panics on overflow,
                    // which with `panic = "abort"` would stop the whole
                    // process for a huge configured `run_budget_secs`.
                    let limit_secs = attempt_max_secs(budget).min(SECS_PER_DAY);
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
                    // §12.15.8: an earlier failure in this process ends the
                    // day with that failure, which pages as usual.
                    if previous.is_some() || waited_for_evening {
                        Err(previous.unwrap_or(AttemptFailure::SkippedNoTime))
                    } else {
                        // Nothing ran here: report the day now, then wait
                        // for 17:50, after the 17:45 start-watchdog
                        // stop_check, and try once more (§12.15.8, 51b review).
                        on_first_skip();
                        waited_for_evening = true;
                        let evening = XVERIFY_EVENING_START_SECS_OF_DAY_IST;
                        tokio::time::sleep(Duration::from_secs(evening.saturating_sub(start)))
                            .await;
                        // The sleep runs on the monotonic clock, the gate on
                        // the wall clock: a wall clock stepped back during
                        // the wait wakes short of 17:50 and would skip the
                        // one evening attempt (51b review). Sleep the
                        // shortfall, a bounded number of times.
                        for _ in 0..XVERIFY_EVENING_RESLEEP_MAX {
                            let shortfall = evening.saturating_sub(now_secs_of_day());
                            if shortfall == 0 {
                                break;
                            }
                            tokio::time::sleep(Duration::from_secs(shortfall)).await;
                        }
                        if !still_today() {
                            return DayResult {
                                attempts,
                                failure: Some(AttemptFailure::SkippedNoTime),
                                day_changed: true,
                            };
                        }
                        // The skipped attempt is not counted.
                        attempts = attempts.saturating_sub(1);
                        continue;
                    }
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
    // §12.15.10: the readiness of the latest spot attempt, for the option
    // pass. Locked only to store or take one `Copy` value, never across an
    // `.await`.
    let spot_readiness: std::sync::Mutex<Option<LiveReadiness>> = std::sync::Mutex::new(None);
    let readiness_out = &spot_readiness;
    let result = drive_day(
        today,
        deps.config.run_budget_secs,
        now_ist_secs_of_day,
        || today_ist().0 == today,
        // §12.15.8 (51b review): a day whose only attempt here was skipped
        // for time is reported before the evening wait, which the scheduled
        // stop may cut short.
        || page_final_failure_once(AttemptFailure::SkippedNoTime, today, 0, targets.len()),
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
                    run_once(
                        deps,
                        &cfg,
                        targets,
                        day,
                        paged,
                        (deadline, plan.is_last),
                        readiness_out,
                    )
                    .await
                }
            }
        },
    )
    .await;
    if result.day_changed {
        return;
    }
    if let Some(failure) = result.failure {
        page_final_failure_once(failure, today, result.attempts, targets.len());
    }
    // §12.15.6: the depth-held option pass runs ONCE, after the spot check's
    // outcome is final. It never writes the day marker and never pages.
    // §12.15.8: it runs under its own timeout, so it too ends by 17:23.
    // The persist, which the timeout cannot cut, stops itself at the same
    // instant.
    let option_limit_secs = attempt_max_secs(XVERIFY_OPTION_PASS_BUDGET_SECS);
    let option_deadline = tokio::time::Instant::now() + Duration::from_secs(option_limit_secs);
    // §12.15.10: the spot check's last readiness, so the pass reads with it.
    let spot = spot_readiness.lock().ok().and_then(|slot| *slot);
    let option_pass = tokio::time::timeout_at(
        option_deadline,
        run_option_pass(deps, today, day_start_ist_nanos, option_deadline, spot),
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

/// Reports the day's final failure once per IST day, across processes
/// (§12.15.8, 51b review): reads today's paged marker, reports through
/// [`report_final_failure`], and writes the marker when that paged. A marker
/// that cannot be written leaves the page standing and is a coded `warn!`; a
/// later failure the same day then pages again (loud, never silent). One file
/// stat, plus one small file write after a page; cold, at most twice a day.
fn page_final_failure_once(
    failure: AttemptFailure,
    today: chrono::NaiveDate,
    attempts: u32,
    targets: usize,
) {
    let paged_today = daily_marker_exists(CROSSVERIFY_PAGED_MARKER_TASK, today);
    if !report_final_failure(failure, today, attempts, targets, paged_today) {
        return;
    }
    if let Err(err) = try_write_daily_marker(CROSSVERIFY_PAGED_MARKER_TASK, today) {
        warn!(
            code = ErrorCode::WsGapConnectionState.code_str(),
            source = "xverify_paged_marker_write_failed",
            %today,
            ?err,
            path = %daily_marker_path(CROSSVERIFY_PAGED_MARKER_TASK, today).display(),
            "Dhan 1-minute cross-verification paged, but could not save the note that \
             today was paged; a later failure today may page again; check disk space \
             and permissions on the state folder"
        );
    }
}

/// Reports the day's final failure. Returns `true` when it paged.
///
/// When `paged_today` (today's paged marker exists), a page already went out
/// today, from this process or an earlier one, so nothing pages: a coded
/// `warn!` on a source no alarm filter matches (`xverify_day_not_attempted`
/// for a day nothing ran on here, `xverify_already_paged_today` otherwise).
/// Otherwise each arm is its own `error!` so every alarmed `source` stays a
/// literal the alarm filter can match; [`AttemptFailure::SkippedNoTime`] (no
/// attempt ran in this process) pages `xverify_failed`, because nobody has
/// been told today is unverified (§12.15.8, 51b review). Pure apart from the
/// log line. O(1).
fn report_final_failure(
    failure: AttemptFailure,
    today: chrono::NaiveDate,
    attempts: u32,
    targets: usize,
    paged_today: bool,
) -> bool {
    let reason = failure.as_str();
    if paged_today {
        let path = daily_marker_path(CROSSVERIFY_PAGED_MARKER_TASK, today);
        if failure == AttemptFailure::SkippedNoTime {
            warn!(
                code = ErrorCode::WsGapConnectionState.code_str(),
                source = "xverify_day_not_attempted",
                %today,
                attempts,
                targets,
                reason,
                path = %path.display(),
                last_end_ist_secs = XVERIFY_LAST_END_SECS_OF_DAY_IST,
                "Dhan 1-minute cross-verification did not run today in this process: it \
                 started too late to finish before the evening stop. Today's candles are \
                 UNVERIFIED and today's S3 archive stays held. Not paged again: the note at \
                 `path` shows a page already went out today. If the box is still up at \
                 17:50, one more attempt runs then"
            );
        } else {
            warn!(
                code = ErrorCode::WsGapConnectionState.code_str(),
                source = "xverify_already_paged_today",
                %today,
                attempts,
                targets,
                reason,
                path = %path.display(),
                "Dhan 1-minute cross-verification did not record today after every \
                 same-day attempt — today's candles are UNVERIFIED and today's S3 archive \
                 stays held. Not paged again: the note at `path` shows a page already \
                 went out today"
            );
        }
        return false;
    }
    match failure {
        AttemptFailure::SkippedNoTime => error!(
            code = ErrorCode::WsGapConnectionState.code_str(),
            source = "xverify_failed",
            %today,
            attempts,
            targets,
            reason,
            last_end_ist_secs = XVERIFY_LAST_END_SECS_OF_DAY_IST,
            "Dhan 1-minute cross-verification could not run today before the evening \
             stop (the box started or restarted too late) and no alert went out today — \
             today's candles are UNVERIFIED and today's S3 archive stays held. If the box \
             is still up at 17:50, one more attempt runs then"
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
        | AttemptFailure::AuditRowsLost
        // §12.15.10: never the outcome of the day's LAST attempt
        // (`decide_read` reads there), but an earlier attempt's readiness
        // reason ends the day when the next attempt is skipped for time.
        | AttemptFailure::LiveNotFinal
        | AttemptFailure::LiveNotApplied
        | AttemptFailure::SealsPending
        | AttemptFailure::SealSpillParked => error!(
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
    true
}

/// One attempt. Returns `Ok` only when the day's marker was written.
///
/// Per-attempt problems log at `warn!` with sources no alarm filter matches;
/// the page fires once per day, from [`report_final_failure`], after the last
/// attempt.
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
///
/// §12.15.10 (plan item 51d): the token wait and the readiness wait run
/// together under the token wait's own bound, so the attempt's longest
/// duration is unchanged; `decide_read` then decides, before any vendor call,
/// whether to read and how. `is_last` is the attempt plan's, decided once
/// before the attempt. The readiness found is left in `readiness_out` for the
/// option pass.
async fn run_once(
    deps: &CrossverifyBootDeps,
    cfg: &DhanLiveCrossverifyConfig,
    targets: &[XverifyTarget],
    (today, day_start_ist_nanos): (chrono::NaiveDate, i64),
    divergence_paged: &AtomicBool,
    (deadline, is_last): (tokio::time::Instant, bool),
    readiness_out: &std::sync::Mutex<Option<LiveReadiness>>,
) -> Result<(), AttemptFailure> {
    let ready_deadline = (tokio::time::Instant::now()
        + Duration::from_secs(TOKEN_WAIT_POLL_SECS * u64::from(TOKEN_WAIT_MAX_POLLS)))
    .min(deadline);
    let (jwt, readiness) = tokio::join!(
        wait_for_jwt(),
        attempt_readiness(deps, today, day_start_ist_nanos, ready_deadline)
    );
    if let Ok(mut slot) = readiness_out.lock() {
        *slot = Some(readiness);
    }
    let Some(jwt) = jwt else {
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
    let (read, check_late) = match decide_read(readiness, is_last) {
        // §12.15.10: before any vendor call. No persist, no marker, no page;
        // `drive_day` counts the retry by its reason.
        ReadPlan::Retry(reason) => {
            warn!(
                code = ErrorCode::WsGapConnectionState.code_str(),
                source = "xverify_attempt_not_ready",
                %today,
                reason = reason.as_str(),
                durability = ?readiness.durability,
                completeness = ?readiness.completeness,
                "Dhan 1-minute cross-verification waited for our last candles to be \
                 sealed and saved, and they were not ready yet; nothing was compared, \
                 and the check runs again later today"
            );
            return Err(reason);
        }
        ReadPlan::Read { policy, check_late } => (policy, check_late),
    };
    if let Some(reason) = read.not_ready {
        warn!(
            code = ErrorCode::WsGapConnectionState.code_str(),
            source = "xverify_unsettled_final",
            %today,
            reason = MissingJudgeable::NotReady(reason).as_str(),
            durability = ?readiness.durability,
            completeness = ?readiness.completeness,
            "Dhan 1-minute cross-verification: the day's last attempt found our last \
             candles not yet sealed and saved; prices are still compared, but no minute \
             missing from our side is judged today"
        );
    }
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
        read,
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
                late_excused = c.late_excused,
                missing_live_late = c.missing_live_late,
                missing_live_unjudged = c.missing_live_unjudged,
                missing_judgeable = c.missing_judgeable.as_str(),
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
            let complete = run_is_complete(
                report.budget_elapsed,
                report.live_truncated,
                report.rest_failures,
                targets.len(),
            );
            // §12.15.9 review: one wall-clock reading per attempt, written on
            // its daily row and every cell, so a reader can order the writes.
            // The stamps do not split the findings by attempt: a cell keeps
            // the stamp of the LAST attempt that wrote it.
            let rows = PersistRows {
                day_start_ist_nanos,
                tolerance_paise: cfg.tolerance_paise,
                attempt: AttemptStamp {
                    at_ist_nanos: fetched_at_ist_nanos_now(),
                    run_complete: complete,
                },
                scope: PersistScope::Full,
            };
            // §12.15.10: a strict read of an unknown completeness that finds
            // a late traded or index minute missing may have read too early.
            // Keep only the vendor tape (its DEDUP key has no outcome, so a
            // later attempt UPSERTs the same rows), page a price divergence
            // as usual, and retry: no daily row, no findings, no marker.
            if check_late && c.missing_live_late > 0 {
                page_divergence_once(c, divergence_paged);
                let tape_rows = PersistRows {
                    scope: PersistScope::TapeOnly,
                    ..rows
                };
                let kept = persist_report(&deps.questdb, &report, tape_rows, deadline);
                warn!(
                    code = ErrorCode::WsGapConnectionState.code_str(),
                    source = "xverify_attempt_not_ready",
                    %today,
                    reason = AttemptFailure::LiveNotFinal.as_str(),
                    missing_live_late = c.missing_live_late,
                    tape_rows = report.rest_tape.len(),
                    tape_rows_saved = kept.rows_flushed,
                    "Dhan 1-minute cross-verification could not tell whether our last \
                     candles were sealed, and minutes at the end of the session are missing \
                     on our side; Dhan's record was saved, nothing was judged, and the check \
                     runs again later today"
                );
                return Err(AttemptFailure::LiveNotFinal);
            }
            let persisted = persist_report(&deps.questdb, &report, rows, deadline);
            let persist = persist_verdict(&persisted);
            let persisted_ok = persist.is_ok();
            page_divergence_once(c, divergence_paged);
            let verdict =
                classify_attempt(c.is_vacuous(), c.outcome.is_measured(), persist, complete);
            // The marker condition must stay exactly `should_write_marker`
            // plus a complete run; the two pure functions agree by test.
            debug_assert_eq!(
                verdict.is_ok(),
                should_write_marker(c, persisted_ok) && complete
            );
            match marker_step(verdict, tokio::time::Instant::now(), deadline) {
                // §12.15.8: a persist that finished never started a flush it
                // could not end by the deadline, so this is a belt: past the
                // deadline the marker is not written and the attempt ends.
                MarkerStep::PastDeadline => {
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
                MarkerStep::Write => record_day(today),
                MarkerStep::Unrecorded(failure) => {
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

/// Pages a catastrophic price divergence once per day (§12.15.5). It is a
/// finding about the data, not about the attempt, so it fires on the first
/// attempt that measures it, the §12.15.10 tape-only retry included, and
/// `paged` stops a later attempt from paging it again. O(1).
fn page_divergence_once(c: &DayComparison, paged: &AtomicBool) {
    if is_catastrophic_divergence(c) && !paged.swap(true, Ordering::Relaxed) {
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
}

/// What `run_once` does with an attempt's verdict (§12.15.8).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MarkerStep {
    /// The verdict is `Ok` and the deadline has not passed: write the marker.
    Write,
    /// The verdict is `Ok` but the deadline has passed: no marker, the
    /// attempt fails `incomplete`.
    PastDeadline,
    /// The verdict is a failure: no marker.
    Unrecorded(AttemptFailure),
}

/// The marker decision, pure so its deadline arm is tested (§12.15.8, 51b
/// review): the attempt's timeout cannot stop a persist that returns `Ok`
/// late, so this arm is the only thing that keeps the marker from being
/// written past the deadline. `now == deadline` still writes. O(1).
fn marker_step(
    verdict: Result<(), AttemptFailure>,
    now: tokio::time::Instant,
    deadline: tokio::time::Instant,
) -> MarkerStep {
    match verdict {
        Ok(()) if now > deadline => MarkerStep::PastDeadline,
        Ok(()) => MarkerStep::Write,
        Err(failure) => MarkerStep::Unrecorded(failure),
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

/// What the spot persist stamps on its rows: the day, the applied tolerance,
/// and the attempt that wrote them (§12.15.9 review).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PersistRows {
    day_start_ist_nanos: i64,
    tolerance_paise: i64,
    attempt: AttemptStamp,
    scope: PersistScope,
}

/// Which rows the spot persist writes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PersistScope {
    /// Findings, vendor tape and the daily row.
    Full,
    /// Only the vendor tape: a read retried as `live_not_final` (§12.15.10)
    /// writes no daily row and no findings, so it can mark nothing.
    TapeOnly,
}

/// Persists the run through the production writer, in batches of
/// [`PERSIST_BATCH_ROWS`], stopping at `deadline`. See [`persist_report_into`].
fn persist_report(
    questdb: &QuestDbConfig,
    report: &RunReport,
    rows: PersistRows,
    deadline: tokio::time::Instant,
) -> PersistOutcome {
    let mut writer = DhanLiveXverifyAuditWriter::new(questdb);
    persist_report_into(
        &mut writer,
        report,
        rows,
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
/// `.await`, so the attempt's timeout cannot stop it. It bounds itself: for
/// each row it reads the clock once and checks, before the append and again
/// after it (on the buffer a batch flush would send), that a flush of the
/// buffer started at that reading would end by `deadline` in the ILP client's
/// worst case ([`DhanLiveXverifyAuditWriter::flush_worst_case`]). When it
/// would not, the
/// persist stops: the buffered rows are abandoned (counted locally, never on a
/// §2.10 loss-group counter, since a deliberate stop is not a lost row), the rows not
/// yet appended are counted, and the outcome is `NotPersisted`, so no marker
/// is written. The check runs per row, not only before a flush, because a
/// buffer that cannot be flushed in time now cannot be later either: the
/// clock only advances and the final flush is still owed. So every flush this
/// starts ends by `deadline`, plus the CPU time between the clock reading
/// and the flush call (one append, microseconds).
/// O(findings + tape rows), once per attempt, cold.
fn persist_report_into(
    writer: &mut DhanLiveXverifyAuditWriter,
    report: &RunReport,
    rows: PersistRows,
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
    // included. The buffer is ABANDONED, not discarded: a deliberate stop of
    // a recomputable write must not reach the §2.10 `audit_rows` page.
    let stop_at_deadline =
        |w: &mut DhanLiveXverifyAuditWriter, out: &mut PersistOutcome, remaining: usize| {
            out.rows_abandoned_at_deadline = w.abandon_pending();
            out.rows_not_written_at_deadline = remaining;
            out.deadline_reached = true;
            out.final_flush_ok = false;
        };
    // §12.15.10: a tape-only persist writes neither findings nor the daily
    // row.
    let full = rows.scope == PersistScope::Full;
    let findings: &[_] = if full { &c.findings } else { &[] };
    let total = findings
        .len()
        .saturating_add(report.rest_tape.len())
        .saturating_add(usize::from(full));
    let mut appended = 0_usize;

    // One clock reading per row, checked twice: before the append (so a
    // persist past its deadline appends nothing) and after it, on the buffer
    // `flush_if_full` would actually send (51b review: checking only before
    // the append let a batch flush start one row larger than the one
    // checked).
    for finding in findings {
        let at = now();
        if !flush_fits(writer, at, deadline) {
            stop_at_deadline(writer, &mut out, total - appended);
            return finish_persist(report, out, batch_errors, None, rows.scope);
        }
        if writer
            .append_cell(finding, rows.attempt.at_ist_nanos)
            .is_err()
        {
            out.cell_append_errors = out.cell_append_errors.saturating_add(1);
        }
        appended += 1;
        if !flush_fits(writer, at, deadline) {
            stop_at_deadline(writer, &mut out, total - appended);
            return finish_persist(report, out, batch_errors, None, rows.scope);
        }
        flush_if_full(writer, &mut out, &mut batch_errors);
    }
    for row in &report.rest_tape {
        let at = now();
        if !flush_fits(writer, at, deadline) {
            stop_at_deadline(writer, &mut out, total - appended);
            return finish_persist(report, out, batch_errors, None, rows.scope);
        }
        if writer.append_rest_tape(row).is_err() {
            out.tape_append_errors = out.tape_append_errors.saturating_add(1);
        }
        appended += 1;
        if !flush_fits(writer, at, deadline) {
            stop_at_deadline(writer, &mut out, total - appended);
            return finish_persist(report, out, batch_errors, None, rows.scope);
        }
        flush_if_full(writer, &mut out, &mut batch_errors);
    }
    if full {
        let daily = daily_row(
            c,
            rows.day_start_ist_nanos,
            deterministic_run_ts_nanos(rows.day_start_ist_nanos),
            rows.tolerance_paise,
            rows.attempt,
        );
        out.daily_appended = writer.append_daily(&daily).is_ok();
    }
    if !flush_fits(writer, now(), deadline) {
        out.daily_appended = false;
        stop_at_deadline(writer, &mut out, 0);
        return finish_persist(report, out, batch_errors, None, rows.scope);
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
    finish_persist(report, out, batch_errors, final_err, rows.scope)
}

/// Publishes the persist's counters and its one log line. Split out of
/// [`persist_report_into`] so the deadline stop and the normal end log the
/// same way. O(1).
fn finish_persist(
    report: &RunReport,
    out: PersistOutcome,
    batch_errors: usize,
    final_err: Option<String>,
    scope: PersistScope,
) -> PersistOutcome {
    let c = &report.comparison;
    metrics::counter!(XVERIFY_PERSIST_ROWS_COUNTER).increment(out.rows_flushed as u64);
    if out.deadline_reached {
        // §12.15.8 (51b review): a deliberate stop counts only locally. It
        // never touches `XVERIFY_PERSIST_ERRORS_COUNTER` or the writer's
        // discard counter, both §2.10 `audit_rows` members that page per
        // attempt. Rows an earlier batch flush really lost were already
        // counted there by the writer, and are named here (`rows_discarded`).
        metrics::counter!(XVERIFY_PERSIST_DEADLINE_STOPS_COUNTER, "pass" => "spot").increment(1);
        warn!(
            code = ErrorCode::WsGapConnectionState.code_str(),
            source = "xverify_persist_stopped_at_deadline",
            rows_flushed = out.rows_flushed,
            rows_abandoned = out.rows_abandoned_at_deadline,
            rows_not_written = out.rows_not_written_at_deadline,
            rows_discarded = out.rows_discarded,
            batch_errors,
            cell_errors = out.cell_append_errors,
            tape_errors = out.tape_append_errors,
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
                || (!out.daily_appended && scope == PersistScope::Full)
            {
                error!(
                    code = ErrorCode::WsGapConnectionState.code_str(),
                    source = "xverify_persist_partial",
                    cell_errors = out.cell_append_errors,
                    tape_errors = out.tape_append_errors,
                    batch_errors,
                    rows_discarded = out.rows_discarded,
                    daily_failed = !out.daily_appended && scope == PersistScope::Full,
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
pub const XVERIFY_OPTION_PASS_OUTCOMES: [&str; 10] = [
    "timed_out",
    "skipped_late",
    "skipped_not_ready",
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

/// How the option pass reads the live side, from the spot attempt's
/// readiness (§12.15.10), or `None` to skip it (`skipped_not_ready`): the pass
/// is never the day's verdict and never retries, so it reads only once the
/// sealed bars are saved. `Strict` when the live side was final; else the
/// excuse after the floor the spot check saw, or the derived window when no
/// floor was known. Pure, O(1).
#[must_use]
pub const fn option_pass_read(readiness: LiveReadiness) -> Option<ReadPolicy> {
    match (readiness.durability, readiness.completeness) {
        (Durability::NotReady(_), _) => None,
        (Durability::Applied, Completeness::Final) => Some(ReadPolicy::STRICT),
        (
            Durability::Applied,
            Completeness::Frozen {
                sealed_through_secs_of_day,
            }
            | Completeness::Moving {
                sealed_through_secs_of_day,
            },
        ) => Some(ReadPolicy {
            late: LateWindowPolicy::excuse_after(sealed_through_secs_of_day),
            not_ready: None,
        }),
        (Durability::Applied, Completeness::Unknown) => Some(ReadPolicy::DERIVED_WINDOW),
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
///
/// §12.15.10: it reads with [`option_pass_read`] of the last spot attempt's
/// readiness; with none (no spot attempt reached the wait), it runs its own
/// wait beside the token wait, bounded like the spot attempt's.
async fn run_option_pass(
    deps: &CrossverifyBootDeps,
    today: chrono::NaiveDate,
    day_start_ist_nanos: i64,
    deadline: tokio::time::Instant,
    spot_readiness: Option<LiveReadiness>,
) {
    for label in XVERIFY_OPTION_PASS_OUTCOMES {
        metrics::counter!(XVERIFY_OPTION_PASS_COUNTER, "outcome" => label).increment(0);
    }
    // 51b review: `option_pass_fits` reads seconds of day with no date, so an
    // evening attempt that ran past midnight would otherwise start the pass
    // for yesterday against today's (empty) held set.
    if today_ist().0 != today || !option_pass_fits(now_ist_secs_of_day()) {
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
    let (jwt, readiness) = match spot_readiness {
        Some(readiness) => (wait_for_jwt().await, readiness),
        None => {
            let ready_deadline = (tokio::time::Instant::now()
                + Duration::from_secs(TOKEN_WAIT_POLL_SECS * u64::from(TOKEN_WAIT_MAX_POLLS)))
            .min(deadline);
            tokio::join!(
                wait_for_jwt(),
                attempt_readiness(deps, today, day_start_ist_nanos, ready_deadline)
            )
        }
    };
    let Some(jwt) = jwt else {
        metrics::counter!(XVERIFY_OPTION_PASS_COUNTER, "outcome" => "no_token").increment(1);
        warn!(
            code = ErrorCode::WsGapConnectionState.code_str(),
            source = "xverify_options_no_token",
            %today,
            "Dhan option cross-check could not run: no Dhan token available"
        );
        return;
    };
    let Some(read) = option_pass_read(readiness) else {
        metrics::counter!(XVERIFY_OPTION_PASS_COUNTER, "outcome" => "skipped_not_ready")
            .increment(1);
        info!(
            %today,
            durability = ?readiness.durability,
            completeness = ?readiness.completeness,
            "Dhan option cross-check skipped: our last candles were not saved in time to \
             compare them today"
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
        read,
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
/// and after each append it checks that a flush of the buffer would end by `deadline` in
/// the ILP client's worst case, and otherwise abandons the buffer (counted
/// locally, never on a §2.10 loss-group counter), logs
/// `xverify_options_persist_stopped_at_deadline` and returns `false`.
fn persist_option_findings(
    questdb: &QuestDbConfig,
    report: &RunReport,
    deadline: tokio::time::Instant,
) -> bool {
    let mut writer = DhanLiveXverifyAuditWriter::new(questdb);
    // One reading for the pass (§12.15.9 review), as the spot persist does.
    let attempt_at_ist_nanos = fetched_at_ist_nanos_now();
    persist_option_findings_into(
        &mut writer,
        report,
        attempt_at_ist_nanos,
        deadline,
        tokio::time::Instant::now,
    ) == OptionPersist::Flushed
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
    attempt_at_ist_nanos: i64,
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
    // Checked before and after each append on one reading, as in
    // `persist_report_into` (51b review).
    for finding in &c.findings {
        let at = now();
        if !flush_fits(writer, at, deadline) {
            stopped = true;
            break;
        }
        if writer.append_cell(finding, attempt_at_ist_nanos).is_err() {
            row_errors = row_errors.saturating_add(1);
        }
        appended += 1;
        if !flush_fits(writer, at, deadline) {
            stopped = true;
            break;
        }
        flush_if_full(writer);
    }
    if !stopped {
        for row in &report.rest_tape {
            let at = now();
            if !flush_fits(writer, at, deadline) {
                stopped = true;
                break;
            }
            if writer.append_rest_tape(row).is_err() {
                row_errors = row_errors.saturating_add(1);
            }
            appended += 1;
            if !flush_fits(writer, at, deadline) {
                stopped = true;
                break;
            }
            flush_if_full(writer);
        }
    }
    // An empty buffer owes no flush (`flush` of nothing is a no-op `Ok`), so
    // it is never a deadline stop (51b review: it used to log one, a false
    // failure signal).
    if stopped || (writer.pending() > 0 && !flush_fits(writer, now(), deadline)) {
        // §12.15.8 (51b review): abandoned, counted locally only; the option
        // pass never pages (§12.15.6), so never a §2.10 loss-group counter.
        let abandoned = writer.abandon_pending();
        metrics::counter!(XVERIFY_PERSIST_DEADLINE_STOPS_COUNTER, "pass" => "options").increment(1);
        warn!(
            code = ErrorCode::WsGapConnectionState.code_str(),
            source = "xverify_options_persist_stopped_at_deadline",
            abandoned,
            not_written = total - appended,
            row_errors,
            batch_errors,
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

    /// The attempt stamp the persist tests write (§12.15.9 review).
    const TEST_ATTEMPT_AT: i64 = 9;

    fn test_rows() -> PersistRows {
        PersistRows {
            day_start_ist_nanos: 0,
            tolerance_paise: 5,
            attempt: AttemptStamp {
                at_ist_nanos: TEST_ATTEMPT_AT,
                run_complete: true,
            },
            scope: PersistScope::Full,
        }
    }

    /// §12.15.9 review: every attempt writes its daily row at the same
    /// deterministic `ts` and `outcome` is in the DEDUP key, so an incomplete
    /// attempt (say `diverged`, a sealed bar still unapplied) and its complete
    /// retry (`partial`) leave two rows. Each attempt reads the clock ONCE,
    /// before its persist, and stamps that reading on its daily row and on
    /// every cell; `run_complete` on the row is the same `run_is_complete`
    /// value the marker decision uses. The option pass stamps its cells too.
    #[test]
    fn test_every_persisted_row_carries_one_attempt_stamp_and_the_run_complete_flag() {
        let prod = prod_src();
        let run_once = fn_body(prod, "async fn run_once(");
        assert_eq!(
            run_once.matches("fetched_at_ist_nanos_now()").count(),
            1,
            "one clock reading per attempt"
        );
        assert_eq!(
            run_once.matches("run_is_complete(").count(),
            1,
            "the stamped flag and the marker decision must be one value"
        );
        let stamp = run_once
            .find("run_complete: complete")
            .expect("stamped flag");
        let persist = run_once.find("persist_report(").expect("persist call");
        let verdict = run_once.find("classify_attempt(").expect("marker decision");
        assert!(
            stamp < persist && persist < verdict,
            "stamp, persist, then decide"
        );
        assert!(run_once[verdict..].contains("persist, complete)"));

        let into = fn_body(prod, "fn persist_report_into(");
        assert!(into.contains("append_cell(finding, rows.attempt.at_ist_nanos)"));
        assert!(
            into.contains("rows.attempt,"),
            "the daily row takes the same stamp"
        );
        let options = fn_body(prod, "fn persist_option_findings(");
        assert_eq!(options.matches("fetched_at_ist_nanos_now()").count(), 1);
        let options_into = fn_body(prod, "fn persist_option_findings_into(");
        assert!(options_into.contains("append_cell(finding, attempt_at_ist_nanos)"));
    }
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
            missing_live_late: 0,
            late_excused: 0,
            missing_live_unjudged: 0,
            missing_judgeable: crate::dhan_live_crossverify::MissingJudgeable::Judged,
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
            body.contains("tokio::join!(\n        wait_for_jwt(),"),
            "run_once must wait for the token (beside the readiness wait, §12.15.10)"
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
        let ok_arm = body.find("MarkerStep::Write => record_day(today)");
        assert!(
            ok_arm.is_some(),
            "run_once writes the marker on its Write arm"
        );
        assert!(
            body.contains("match marker_step(verdict, tokio::time::Instant::now(), deadline) {"),
            "the marker decision reads the clock against the attempt's deadline"
        );
        assert!(body.contains("MarkerStep::PastDeadline => {"));
        let run_day = fn_body(prod, "async fn run_day(");
        assert!(run_day.contains("AttemptKind::MarkerOnly => {"));
        // The only other marker write is the paged-day marker (§12.15.8,
        // 51b review), a different task the S3 gate never reads.
        assert_eq!(prod.matches("write_daily_marker(").count(), 1);
        assert_eq!(
            prod.matches("try_write_daily_marker(CROSSVERIFY_PAGED_MARKER_TASK, today)")
                .count(),
            1
        );
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
                rows_abandoned_at_deadline: 0,
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
        let out = persist_report_into(&mut writer, &report, test_rows(), 2, far, || t0);
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
        let out = persist_report_into(&mut writer, &report, test_rows(), 2, t0, || t0);
        assert!(out.deadline_reached);
        assert!(!out.final_flush_ok);
        assert!(!out.daily_appended);
        assert_eq!(out.rows_not_written_at_deadline, 5 + 3 + 1, "{out:?}");
        assert_eq!(out.rows_discarded, 0);
        assert_eq!(out.rows_abandoned_at_deadline, 0);
        assert_eq!(out.rows_flushed, 0);
        assert_eq!(writer.pending(), 0);
        assert_eq!(persist_verdict(&out), Err(AttemptFailure::NotPersisted));
    }

    /// §12.15.8: the persist stops at the first row whose buffer could not be
    /// flushed by the deadline in the ILP client's worst case (5 s plus the
    /// bytes at 100 KiB/s). The clock advances 1 s per reading; the deadline
    /// is 7 s out. Readings at 0 and 1 s fit before and after their append.
    /// At 2 s the empty buffer fits (2 s + 5 s is exactly 7 s) but the buffer
    /// after the append does not (the row's bytes add a few milliseconds), so
    /// the persist stops there, after the append and before any flush (51b
    /// review: the check used to run only before the append).
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
        let out = persist_report_into(&mut writer, &report, test_rows(), 2, deadline, clock);
        assert_eq!(reads.get(), 3, "one reading per row until the stop");
        assert!(out.deadline_reached);
        // Rows 0 and 1 went in a failed batch flush (no sender), row 2 was
        // appended and abandoned at the stop (not discarded: §12.15.8); 2
        // findings, 3 tape rows and the daily row were never appended.
        assert_eq!(out.rows_discarded, 2, "{out:?}");
        assert_eq!(out.rows_abandoned_at_deadline, 1, "{out:?}");
        assert_eq!(out.rows_not_written_at_deadline, 6, "{out:?}");
        assert_eq!(writer.pending(), 0);
        assert_eq!(persist_verdict(&out), Err(AttemptFailure::NotPersisted));
    }

    /// §12.15.8 (51b review): the bytes term of the worst case bounds the
    /// persist, not only the 5 s request timeout. The batch is larger than the
    /// report, so no flush empties the buffer; the clock is frozen 6 s before
    /// the deadline. A writer that dropped `flush_worst_case`'s bytes term
    /// (`now + 5 s <= deadline`) would never stop here. The expected stop row
    /// is measured on a second writer fed the same rows: the first whose
    /// buffer's worst case exceeds the 6 s left.
    #[test]
    fn test_persist_report_into_stops_on_the_bytes_term_of_the_worst_case() {
        let report = report_with(4_000, 0);
        let room = Duration::from_secs(6);
        let mut probe = DhanLiveXverifyAuditWriter::for_test();
        let mut stop_row = None;
        for (i, f) in report.comparison.findings.iter().enumerate() {
            assert!(probe.append_cell(f, 0).is_ok());
            if probe.flush_worst_case() > room {
                stop_row = Some(i + 1);
                break;
            }
        }
        let stop_row = stop_row.expect("4,000 rows exceed 1 s at 100 KiB/s");
        assert!(stop_row > 1 && stop_row < 4_000, "{stop_row}");
        let mut writer = DhanLiveXverifyAuditWriter::for_test();
        let t0 = tokio::time::Instant::now();
        let out = persist_report_into(&mut writer, &report, test_rows(), 10_000, t0 + room, || t0);
        assert!(out.deadline_reached, "{out:?}");
        assert_eq!(out.rows_abandoned_at_deadline, stop_row, "{out:?}");
        assert_eq!(out.rows_not_written_at_deadline, 4_001 - stop_row);
        assert_eq!(out.rows_flushed + out.rows_discarded, 0);
        assert_eq!(writer.pending(), 0);
        // The same for the option persist.
        let mut writer = DhanLiveXverifyAuditWriter::for_test();
        assert_eq!(
            persist_option_findings_into(&mut writer, &report, TEST_ATTEMPT_AT, t0 + room, || t0),
            OptionPersist::StoppedAtDeadline
        );
        assert_eq!(writer.pending(), 0);
    }

    /// §12.15.8 (51b review): the marker is written only for an `Ok` verdict
    /// reached by the deadline. The timeout cannot stop a persist that
    /// returns `Ok` late, so this arm is the only thing that keeps the marker
    /// from being written past it.
    #[test]
    fn test_marker_step_refuses_the_marker_past_the_deadline() {
        let t0 = tokio::time::Instant::now();
        let later = t0 + Duration::from_millis(1);
        assert_eq!(
            marker_step(Ok(()), t0, t0),
            MarkerStep::Write,
            "at the deadline"
        );
        assert_eq!(marker_step(Ok(()), t0, later), MarkerStep::Write);
        assert_eq!(marker_step(Ok(()), later, t0), MarkerStep::PastDeadline);
        for failure in [AttemptFailure::NotPersisted, AttemptFailure::Vacuous] {
            assert_eq!(
                marker_step(Err(failure), later, t0),
                MarkerStep::Unrecorded(failure)
            );
            assert_eq!(
                marker_step(Err(failure), t0, later),
                MarkerStep::Unrecorded(failure)
            );
        }
    }

    proptest::proptest! {
        /// Whatever the clock and deadline: every row is accounted for once
        /// (flushed, discarded or not written), and every clock reading the
        /// persist acted on left at least the 5 s request timeout before the
        /// deadline. That is the floor of the bound, not all of it: the
        /// bytes term is pinned by
        /// `test_persist_report_into_stops_on_the_bytes_term_of_the_worst_case`.
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
            let out = persist_report_into(&mut writer, &report, test_rows(), batch, deadline, clock);
            proptest::prop_assert_eq!(
                out.rows_flushed
                    + out.rows_discarded
                    + out.rows_abandoned_at_deadline
                    + out.rows_not_written_at_deadline,
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

    /// §12.15.8: the option persist stops the same way. An empty report
    /// flushes nothing and succeeds, with room or without (51b review: an
    /// empty buffer owes no flush, so it is not a deadline stop); rows past
    /// the deadline stop before the final flush; rows with room reach the
    /// (absent) sender and fail.
    #[test]
    fn test_persist_option_findings_into_stops_at_the_deadline() {
        let t0 = tokio::time::Instant::now();
        let far = t0 + Duration::from_secs(86_400);
        let mut w = DhanLiveXverifyAuditWriter::for_test();
        let empty = report_with(0, 0);
        assert_eq!(
            persist_option_findings_into(&mut w, &empty, TEST_ATTEMPT_AT, far, || t0),
            OptionPersist::Flushed
        );
        assert_eq!(
            persist_option_findings_into(&mut w, &empty, TEST_ATTEMPT_AT, t0, || t0),
            OptionPersist::Flushed
        );
        let rows = report_with(4, 2);
        assert_eq!(
            persist_option_findings_into(&mut w, &rows, TEST_ATTEMPT_AT, t0, || t0),
            OptionPersist::StoppedAtDeadline
        );
        assert_eq!(w.pending(), 0);
        assert_eq!(
            persist_option_findings_into(&mut w, &rows, TEST_ATTEMPT_AT, far, || t0),
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
            body.contains("| AttemptFailure::AuditRowsLost\n"),
            "AuditRowsLost joins the existing xverify_failed arm"
        );
        // §12.15.10: the readiness reasons join the same arm.
        assert!(body.contains("| AttemptFailure::SealSpillParked => error!("));
        for r in ["LiveNotFinal", "LiveNotApplied", "SealsPending"] {
            assert!(body.contains(&format!("| AttemptFailure::{r}\n")), "{r}");
        }
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

    /// The `schedule_expression` cron fields of one terraform resource block,
    /// read from code lines only (comments skipped).
    fn tf_cron_fields(tf: &str, resource: &str) -> Vec<String> {
        let start = tf
            .find(resource)
            .unwrap_or_else(|| panic!("start-watchdog-lambda.tf lost `{resource}`"));
        let block = &tf[start..];
        let block = &block[..block.find("\n}").unwrap_or(block.len())];
        let line = block
            .lines()
            .map(str::trim_start)
            .find(|l| !l.starts_with('#') && l.starts_with("schedule_expression"))
            .unwrap_or_else(|| panic!("`{resource}` has no schedule_expression"));
        let open = line.find("cron(").expect("a cron() schedule") + 5;
        let close = line[open..].find(')').expect("a closed cron()") + open;
        line[open..close]
            .split_whitespace()
            .map(str::to_owned)
            .collect()
    }

    /// §12.15.8 (second 51b review): the evening attempt starts after BOTH
    /// start-watchdog stops that act on a weekday box after 17:30 have had
    /// their margin. The 17:45 `stop_check` (any box launched before 17:30,
    /// keep-alive ignored) is pinned to its terraform cron, and the hourly
    /// `curfew_check`'s latest firing at or before the evening start is at
    /// least the same margin earlier. Before this, the attempt started at
    /// 17:45, the stop_check's own instant.
    #[test]
    fn test_evening_start_follows_the_start_watchdog_stop_check_and_curfew() {
        const IST_OFFSET_SECS: u64 = 5 * 3_600 + 30 * 60;
        let tf = include_str!("../../../deploy/aws/terraform/start-watchdog-lambda.tf");

        let stop_check = tf_cron_fields(
            tf,
            "resource \"aws_cloudwatch_event_rule\" \"start_watchdog_stop_check\"",
        );
        assert_eq!(
            stop_check.get(4).map(String::as_str),
            Some("MON-FRI"),
            "{stop_check:?}"
        );
        let minute: u64 = stop_check[0].parse().expect("a fixed stop_check minute");
        let hour: u64 = stop_check[1].parse().expect("a fixed stop_check hour");
        let stop_check_ist = (hour * 3_600 + minute * 60 + IST_OFFSET_SECS) % SECS_PER_DAY;
        assert_eq!(
            stop_check_ist, START_WATCHDOG_STOP_CHECK_SECS_OF_DAY_IST,
            "the start-watchdog stop_check moved; move START_WATCHDOG_STOP_CHECK_SECS_OF_DAY_IST \
             and re-check §12.15.8's evening attempt with it"
        );
        assert_eq!(
            XVERIFY_EVENING_START_SECS_OF_DAY_IST,
            START_WATCHDOG_STOP_CHECK_SECS_OF_DAY_IST + XVERIFY_STOP_CHECK_MARGIN_SECS
        );
        assert!(XVERIFY_STOP_CHECK_MARGIN_SECS >= 300);
        assert!(
            u64::from(SCHEDULED_STOP_WINDOW_END_SECS_OF_DAY_IST)
                < XVERIFY_EVENING_START_SECS_OF_DAY_IST
        );

        let curfew = tf_cron_fields(
            tf,
            "resource \"aws_cloudwatch_event_rule\" \"start_watchdog_curfew_check\"",
        );
        assert_eq!(curfew.get(1).map(String::as_str), Some("*"), "{curfew:?}");
        let curfew_minute_utc: u64 = curfew[0].parse().expect("a fixed curfew minute");
        // IST is UTC + 5:30, so an hourly UTC minute lands at minute + 30 IST.
        let curfew_offset_ist = ((curfew_minute_utc + 30) % 60) * 60;
        let since_last_curfew =
            (XVERIFY_EVENING_START_SECS_OF_DAY_IST % 3_600 + 3_600 - curfew_offset_ist) % 3_600;
        assert!(
            since_last_curfew >= XVERIFY_STOP_CHECK_MARGIN_SECS,
            "the hourly curfew_check fires {since_last_curfew} s before the evening start; \
             it must have its margin too"
        );

        // The production loop sleeps to the evening start, never the window end.
        let prod = prod_src();
        assert!(prod.contains("let evening = XVERIFY_EVENING_START_SECS_OF_DAY_IST;"));
        assert!(prod.contains("if now_secs_of_day >= XVERIFY_EVENING_START_SECS_OF_DAY_IST {"));
        assert!(!prod.contains("let evening = u64::from(SCHEDULED_STOP_WINDOW_END"));
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
        let evening_start = XVERIFY_EVENING_START_SECS_OF_DAY_IST;
        assert_eq!(attempt_budget_secs(window_start, 600), None);
        assert_eq!(attempt_budget_secs(EVENING_STOP_SECS_OF_DAY_IST, 600), None);
        assert_eq!(attempt_budget_secs(evening_start - 1, 600), None);
        // §12.15.8 (second 51b review): the 17:45 window end is the instant the
        // start-watchdog stop_check fires, so an unshrunk attempt there would
        // race it. Refused until five minutes later.
        let window_end = u64::from(SCHEDULED_STOP_WINDOW_END_SECS_OF_DAY_IST);
        assert_eq!(attempt_budget_secs(window_end, 600), None);
        assert_eq!(
            attempt_budget_secs(START_WATCHDOG_STOP_CHECK_SECS_OF_DAY_IST, 600),
            None
        );
        // A manual evening boot from 17:50 runs the configured budget, as
        // before §12.15.8.
        assert_eq!(attempt_budget_secs(evening_start, 600), Some(600));
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
        // §12.15.8 (second 51b review): unshrunk attempts start only from
        // 17:50, five minutes after the start-watchdog stop_check.
        let window_end = XVERIFY_EVENING_START_SECS_OF_DAY_IST;
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
                            assert!(
                                now >= window_end,
                                "ran before the 17:50 evening start: {now}"
                            );
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
        /// Times `on_first_skip` was called (§12.15.8, 51b review).
        skip_notices: u32,
        /// Seconds of day of the last skip notice.
        notice_at: Option<u64>,
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
        let skip_notices = std::cell::Cell::new(0_u32);
        let notice_at = std::cell::Cell::new(None);
        let result = drive_day(
            day(),
            config_budget,
            now,
            || {
                sleeps.set(sleeps.get() + 1);
                day_changes_after.is_none_or(|after| sleeps.get() <= after)
            },
            || {
                skip_notices.set(skip_notices.get() + 1);
                notice_at.set(Some(now()));
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
            skip_notices: skip_notices.get(),
            notice_at: notice_at.get(),
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
        // §12.15.8 (51b review): a first attempt with no time is reported at
        // once, at the skip and before any wait, then retried ONCE at 17:50
        // with the configured budget, as attempt 1 and the last.
        let skipped_first = attempt_budget_secs(first_start, config).is_none();
        assert_eq!(sim.skip_notices, u32::from(skipped_first));
        if skipped_first {
            assert_eq!(sim.notice_at, Some(first_start), "reported before the wait");
            let evening = sim.plans.first().copied().expect("the 17:50 attempt ran");
            assert_eq!(
                evening.start_secs_of_day,
                XVERIFY_EVENING_START_SECS_OF_DAY_IST
            );
            assert_eq!(evening.number, 1);
            assert_eq!(evening.run_budget_secs, config);
            assert!(evening.is_last);
            assert_eq!(sim.plans.len(), 1);
        }
        // A skipped attempt (no plan) is always the final, last one. With no
        // attempt before it, the day ends `SkippedNoTime`; after a real
        // failure, with that failure.
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

    /// 51b review: the evening wait sleeps on the monotonic clock but the
    /// attempt is gated on the wall clock. A wall clock stepped back 2 s during
    /// the wait wakes at 17:49:58; the loop sleeps the shortfall and still
    /// makes the one evening attempt, instead of skipping it a second time
    /// and ending the day `skipped_no_time`.
    #[tokio::test(start_paused = true)]
    async fn test_drive_day_evening_attempt_survives_a_wall_clock_stepped_back() {
        let evening = XVERIFY_EVENING_START_SECS_OF_DAY_IST;
        let first_start = 63_000_u64;
        let t0 = tokio::time::Instant::now();
        let stepped = std::cell::Cell::new(false);
        let now = || {
            let wall = first_start + t0.elapsed().as_secs();
            if stepped.get() { wall - 2 } else { wall }
        };
        let starts = std::cell::RefCell::new(Vec::new());
        let result = drive_day(
            day(),
            600,
            now,
            || true,
            || stepped.set(true),
            |plan: AttemptPlan| {
                starts.borrow_mut().push(plan.start_secs_of_day);
                async { Ok(()) }
            },
        )
        .await;
        assert_eq!(result.failure, None, "{result:?}");
        assert_eq!(result.attempts, 1);
        assert_eq!(starts.into_inner(), vec![evening]);
        assert_eq!(now(), evening, "woke once more for the 2 s shortfall");
    }

    /// 51b review: an evening attempt keeps the configured budget unshrunk,
    /// and a huge one (`u64::MAX`, which `validate` accepts) must not overflow
    /// `Instant + Duration` (a panic, and `panic = "abort"` would stop the
    /// whole process). The limit is capped at a day.
    #[tokio::test(start_paused = true)]
    async fn test_drive_day_evening_attempt_with_a_huge_budget_does_not_panic() {
        let sim = simulate(
            XVERIFY_EVENING_START_SECS_OF_DAY_IST,
            u64::MAX,
            |_, _| (Some(1), Ok(())),
            None,
        )
        .await;
        assert_eq!(sim.result.failure, None);
        let plan = sim.plans.first().copied().expect("one attempt ran");
        assert_eq!(plan.run_budget_secs, u64::MAX);
        assert_eq!(
            plan.deadline,
            Some(sim.t0 + Duration::from_secs(SECS_PER_DAY)),
            "the limit is capped at a day"
        );
    }

    /// 51b review: the option pass refuses to start once the IST day has
    /// changed (an evening attempt that ran past midnight), so it never
    /// compares yesterday against today's empty held set.
    #[test]
    fn test_option_pass_refuses_a_changed_day() {
        let body = fn_body(prod_src(), "async fn run_option_pass(");
        assert!(
            body.contains(
                "if today_ist().0 != today || !option_pass_fits(now_ist_secs_of_day()) {"
            ),
            "the option pass checks the day before it checks the time"
        );
    }

    /// 51b review: the runbook's operator actions for a skipped day agree
    /// with §12.15.8 and the code: a start at or after 17:50 runs one full
    /// attempt (`attempt_budget_secs` returns the configured budget), so the
    /// runbook must never tell the operator today cannot be re-checked by a
    /// restart then, nor that a retry always runs at full budget.
    #[test]
    fn test_runbook_skip_rows_agree_with_the_evening_attempt() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../docs/error-runbooks/dhan-live-crossverify-error-codes.md");
        let runbook = std::fs::read_to_string(&path).unwrap_or_default();
        let row = |source: &str| {
            runbook
                .lines()
                .find(|l| l.starts_with(&format!("| `source = \"{source}\"`")))
                .unwrap_or("")
                .to_string()
        };
        let skipped = row("xverify_attempt_skipped_no_time");
        assert!(skipped.contains("17:50"), "{skipped}");
        let failed = runbook
            .lines()
            .find(|l| l.contains("`reason = \"skipped_no_time\"`") && l.starts_with("| `source"))
            .unwrap_or("");
        assert!(failed.contains("at or after 17:50"), "{failed}");
        let timed_out = row("xverify_attempt_timed_out");
        assert!(!timed_out.is_empty());
        for text in [skipped.as_str(), failed, timed_out.as_str()] {
            assert!(!text.contains("cannot be re-checked"), "{text}");
            assert!(!text.contains("cannot re-check"), "{text}");
            assert!(!text.contains("runs at full budget"), "{text}");
        }
        assert_eq!(
            attempt_budget_secs(XVERIFY_EVENING_START_SECS_OF_DAY_IST, 600),
            Some(600)
        );
        assert!(should_catch_up(
            XVERIFY_EVENING_START_SECS_OF_DAY_IST,
            false
        ));
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
    /// calling it, reports the day at once and retries once at 17:50; a
    /// little more time shrinks the budget and the timeout follows the shrunk
    /// budget; a configured budget below the floor runs.
    #[tokio::test(start_paused = true)]
    async fn test_drive_day_late_start_shrinks_or_skips() {
        let fixed = attempt_max_secs(0);
        let last = XVERIFY_LAST_END_SECS_OF_DAY_IST;
        let evening_start = XVERIFY_EVENING_START_SECS_OF_DAY_IST;
        let skip_at = last - fixed - XVERIFY_MIN_ATTEMPT_BUDGET_SECS + 1;
        let skipped = simulate(skip_at, 600, |_, _| (Some(0), Ok(())), None).await;
        assert_eq!(skipped.skip_notices, 1, "the day is reported at the skip");
        assert_eq!(skipped.notice_at, Some(skip_at));
        assert_eq!(skipped.plans.len(), 1, "only the 17:50 attempt runs");
        assert_eq!(
            skipped.plans.first().map(|p| p.start_secs_of_day),
            Some(evening_start)
        );
        assert_eq!(skipped.plans.first().map(|p| p.run_budget_secs), Some(600));
        assert_eq!(skipped.result.attempts, 1);
        assert_eq!(skipped.result.failure, None);
        assert_eq!(skipped.finished, evening_start);

        let shrink_at = last - fixed - 200;
        let shrunk = simulate(shrink_at, 600, |_, _| (None, Ok(())), None).await;
        assert_eq!(shrunk.plans.first().map(|p| p.run_budget_secs), Some(200));
        assert_eq!(shrunk.plans.first().map(|p| p.is_last), Some(true));
        assert_eq!(shrunk.result.failure, Some(AttemptFailure::Incomplete));
        assert_eq!(shrunk.skip_notices, 0);
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
        let evening = evening_start + 2_820;
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
        assert_eq!(late.skip_notices, 0);
    }

    /// §12.15.8 (51b review): the finding's scenario. A Saturday special
    /// session, the process restarted at 17:30 with no stop cron. The day is
    /// reported at once (before the wait, which a weekday stop would cut),
    /// then verified at 17:50 instead of being given up until the next day.
    /// Every skip start up to 17:50 behaves the same, the 17:45 instant the
    /// start-watchdog stop_check fires included (second 51b review: the
    /// attempt used to start at that same instant); the 17:50 attempt's own
    /// failure is the day's failure.
    #[tokio::test(start_paused = true)]
    async fn test_drive_day_skip_reports_at_once_then_retries_once_after_the_stop_check() {
        let evening = XVERIFY_EVENING_START_SECS_OF_DAY_IST;
        assert_eq!(evening, 17 * 3_600 + 50 * 60, "17:50 IST");
        for start in [
            XVERIFY_LAST_END_SECS_OF_DAY_IST - attempt_max_secs(0) - 119,
            XVERIFY_LAST_END_SECS_OF_DAY_IST - 300,
            62_580,
            62_645,
            63_000,
            u64::from(SCHEDULED_STOP_WINDOW_END_SECS_OF_DAY_IST),
            START_WATCHDOG_STOP_CHECK_SECS_OF_DAY_IST + 1,
            evening - 1,
        ] {
            let ok = simulate(start, 600, |_, _| (Some(30), Ok(())), None).await;
            assert_eq!(ok.skip_notices, 1, "start {start}");
            assert_eq!(ok.notice_at, Some(start), "start {start}");
            assert_eq!(ok.result.failure, None, "start {start}");
            assert_eq!(ok.result.attempts, 1);
            assert_eq!(ok.finished, evening + 30);
            let failed = simulate(
                start,
                600,
                |_, _| (Some(0), Err(AttemptFailure::Vacuous)),
                None,
            )
            .await;
            assert_eq!(failed.skip_notices, 1);
            assert_eq!(failed.result.failure, Some(AttemptFailure::Vacuous));
            assert_eq!(failed.plans.len(), 1, "one evening attempt, never more");
            assert!(failed.plans.iter().all(|p| p.is_last));
        }
        // A clock that never reaches 17:50 (nonsense, but bounded): the loop
        // waits once, skips again and ends `SkippedNoTime`; nothing ran.
        let ran = std::cell::Cell::new(0_u32);
        let notices = std::cell::Cell::new(0_u32);
        let stuck = drive_day(
            day(),
            600,
            || 62_645,
            || true,
            || notices.set(notices.get() + 1),
            |_plan: AttemptPlan| {
                ran.set(ran.get() + 1);
                async { Ok(()) }
            },
        )
        .await;
        assert_eq!(ran.get(), 0);
        assert_eq!(notices.get(), 1, "the evening wait happens at most once");
        assert_eq!(stuck.attempts, 1);
        assert_eq!(stuck.failure, Some(AttemptFailure::SkippedNoTime));
        assert!(!stuck.day_changed);
        // The day changes during the evening wait: no attempt, no report.
        let gone = simulate(62_645, 600, |_, _| (Some(0), Ok(())), Some(0)).await;
        assert!(gone.result.day_changed);
        assert!(gone.plans.is_empty());
        assert_eq!(gone.skip_notices, 1);
        // A skip after a real failure in this process (forced by a clock jump
        // during the retry sleep) ends the day with THAT failure: no notice,
        // no evening wait.
        let readings = std::cell::Cell::new(0_u32);
        let jumping = || {
            readings.set(readings.get() + 1);
            if readings.get() == 1 { 56_460 } else { 62_500 }
        };
        let after_failure_ran = std::cell::Cell::new(0_u32);
        let after_failure_notices = std::cell::Cell::new(0_u32);
        let after_failure = drive_day(
            day(),
            600,
            jumping,
            || true,
            || after_failure_notices.set(after_failure_notices.get() + 1),
            |_plan: AttemptPlan| {
                after_failure_ran.set(after_failure_ran.get() + 1);
                async { Err(AttemptFailure::RunFailed) }
            },
        )
        .await;
        assert_eq!(after_failure_ran.get(), 1, "the second attempt was skipped");
        assert_eq!(after_failure_notices.get(), 0);
        assert_eq!(after_failure.attempts, 2);
        assert_eq!(after_failure.failure, Some(AttemptFailure::RunFailed));
    }

    /// §12.15.8 (51b review): one page per IST day across processes. With no
    /// page today every final failure pages, `SkippedNoTime` included (before
    /// the review it only logged, on the unchecked belief that an earlier
    /// process had paged); with today's paged marker present none does.
    #[test]
    fn test_report_final_failure_pages_unless_today_was_already_paged() {
        for failure in [
            AttemptFailure::NoToken,
            AttemptFailure::RunFailed,
            AttemptFailure::Vacuous,
            AttemptFailure::NotPersisted,
            AttemptFailure::Incomplete,
            AttemptFailure::MarkerNotWritten,
            AttemptFailure::AuditRowsLost,
            AttemptFailure::SkippedNoTime,
        ] {
            assert!(
                report_final_failure(failure, day(), 1, 10, false),
                "{failure:?} must page when nobody was told today"
            );
            assert!(
                !report_final_failure(failure, day(), 1, 10, true),
                "{failure:?} must not page twice in a day"
            );
        }
    }

    /// The emit side of the rule above: the unpaged `SkippedNoTime` arm is an
    /// `error!` on the alarmed `xverify_failed` source; the already-paged
    /// branch is coded `warn!`s only; the wrapper reads and writes the paged
    /// marker around it; `run_day` reports only through the wrapper, at the
    /// skip and after the loop; the paged marker cannot collide with the
    /// day marker the S3 gate reads, nor be swept by its sweep.
    #[test]
    fn test_skipped_day_pages_unless_paged_and_the_paged_marker_is_wired() {
        let prod = prod_src();
        let body = fn_body(prod, "fn report_final_failure(");
        let paged_at = body.find("if paged_today {").expect("paged branch");
        let match_at = body.find("match failure {").expect("page arms");
        assert!(paged_at < match_at, "the paged check comes first");
        let paged_branch = &body[paged_at..match_at];
        assert!(!paged_branch.contains("error!("));
        assert!(paged_branch.contains("source = \"xverify_day_not_attempted\""));
        assert!(paged_branch.contains("source = \"xverify_already_paged_today\""));
        assert!(paged_branch.contains("return false;"));
        let arms = &body[match_at..];
        let arm = arms
            .split("AttemptFailure::SkippedNoTime => ")
            .nth(1)
            .expect("report_final_failure must name SkippedNoTime");
        let arm = &arm[..arm.find("AttemptFailure::Vacuous =>").unwrap_or(arm.len())];
        assert!(arm.starts_with("error!("), "{arm}");
        assert!(arm.contains("source = \"xverify_failed\""));
        assert!(arm.contains("code = ErrorCode::WsGapConnectionState.code_str()"));
        let wrapper = fn_body(prod, "fn page_final_failure_once(");
        let read_at = wrapper
            .find("daily_marker_exists(CROSSVERIFY_PAGED_MARKER_TASK, today)")
            .expect("reads today's paged marker");
        let report_at = wrapper
            .find("report_final_failure(failure, today, attempts, targets, paged_today)")
            .expect("reports with it");
        let write_at = wrapper
            .find("try_write_daily_marker(CROSSVERIFY_PAGED_MARKER_TASK, today)")
            .expect("writes the paged marker");
        assert!(read_at < report_at && report_at < write_at);
        assert!(wrapper.contains("source = \"xverify_paged_marker_write_failed\""));
        let run_day = fn_body(prod, "async fn run_day(");
        assert_eq!(run_day.matches("page_final_failure_once(").count(), 2);
        assert!(run_day.contains(
            "|| page_final_failure_once(AttemptFailure::SkippedNoTime, today, 0, targets.len())"
        ));
        assert_eq!(prod.matches("report_final_failure(failure").count(), 1);
        assert_ne!(CROSSVERIFY_PAGED_MARKER_TASK, CROSSVERIFY_MARKER_TASK);
        assert!(!CROSSVERIFY_PAGED_MARKER_TASK.starts_with(&format!("{CROSSVERIFY_MARKER_TASK}-")));
        assert!(!CROSSVERIFY_MARKER_TASK.starts_with(&format!("{CROSSVERIFY_PAGED_MARKER_TASK}-")));
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
            run_day.contains(
                "run_option_pass(deps, today, day_start_ist_nanos, option_deadline, spot)"
            )
        );
        assert!(run_day.contains("source = \"xverify_options_timed_out\""));
        // run_once is reached only through the driver: its definition and
        // one call.
        assert_eq!(prod.matches("run_once(").count(), 2);
        assert_eq!(prod.matches("async fn run_once(").count(), 1);
        let run_once = fn_body(prod, "async fn run_once(");
        assert!(run_once.contains("persist_report(&deps.questdb, &report, rows, deadline)"));
        assert!(!run_day.contains("now_ist_secs_of_day()"));
    }

    /// Every `.tf` file under `deploy/aws/terraform`, concatenated. Every
    /// CloudWatch alarm and log metric filter lives there (the error-code
    /// filters, the §2.9 and §2.10 `/metrics` loss filters, and the rest), so
    /// a name absent from all of them cannot page.
    fn all_terraform() -> String {
        let dir =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../deploy/aws/terraform");
        let mut paths: Vec<_> = std::fs::read_dir(&dir)
            .expect("terraform dir")
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|x| x == "tf"))
            .collect();
        paths.sort();
        assert!(
            paths.len() > 10,
            "terraform scan found only {} files",
            paths.len()
        );
        paths
            .iter()
            .map(|p| std::fs::read_to_string(p).expect("read tf"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The string value of `const <ident>: &str = "..."` in `src`.
    fn const_value<'a>(src: &'a str, ident: &str) -> &'a str {
        let decl = format!("const {ident}: &str =");
        let after = src
            .split(decl.as_str())
            .nth(1)
            .unwrap_or_else(|| panic!("no declaration of {ident}"));
        let open = after.find('"').expect("opening quote");
        let rest = &after[open + 1..];
        &rest[..rest.find('"').expect("closing quote")]
    }

    /// The new lines are coded `warn!`s on sources no alarm filter matches.
    /// Paging can also come from what the line's branch COUNTS (a §2.10
    /// `/metrics` filter turns a counter into a page), so for the two
    /// deliberate deadline stops this also checks every counter the branch
    /// increments, directly or through a writer method, against every
    /// terraform file (51b review: the first version read one file and gave a
    /// false OK while both stops paged `audit-rows-lost`).
    #[test]
    fn test_time_bound_sources_are_coded_and_match_no_alarm_filter() {
        let prod = prod_src();
        let tf = all_terraform();
        for source in [
            "xverify_attempt_timed_out",
            "xverify_attempt_skipped_no_time",
            "xverify_options_timed_out",
            "xverify_persist_stopped_at_deadline",
            "xverify_options_persist_stopped_at_deadline",
            "xverify_day_not_attempted",
            "xverify_already_paged_today",
            "xverify_paged_marker_write_failed",
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

        // The writer methods a branch may call, and the counter each bumps,
        // pinned against the storage source so the table cannot drift.
        let storage = include_str!("../../storage/src/dhan_live_crossverify_persistence.rs");
        let discard_body = storage
            .split("fn discard_pending(")
            .nth(1)
            .and_then(|s| s.split("\n    }\n").next())
            .expect("discard_pending");
        assert!(discard_body.contains("\"tv_dhan_live_xverify_audit_rows_discarded_total\""));
        let abandon_body = storage
            .split("fn abandon_pending(")
            .nth(1)
            .and_then(|s| s.split("\n    }\n").next())
            .expect("abandon_pending");
        assert!(abandon_body.contains("DHAN_LIVE_XVERIFY_AUDIT_ROWS_ABANDONED_COUNTER"));
        let abandoned = const_value(storage, "DHAN_LIVE_XVERIFY_AUDIT_ROWS_ABANDONED_COUNTER");

        // The scan must see the loss filters at all: the paging counters ARE
        // in them, so a broken scan cannot pass vacuously.
        for paging in [
            const_value(prod, "XVERIFY_PERSIST_ERRORS_COUNTER"),
            "tv_dhan_live_xverify_audit_rows_discarded_total",
        ] {
            assert!(tf.contains(paging), "{paging} is expected in a loss filter");
        }

        let report_into = fn_body(prod, "fn persist_report_into(");
        let finish = fn_body(prod, "fn finish_persist(");
        let options = fn_body(prod, "fn persist_option_findings_into(");
        let slice = |body: &'static str, from: &str, to: &str| -> &'static str {
            let at = body.find(from).unwrap_or_else(|| panic!("no {from}"));
            let rest = &body[at..];
            &rest[..rest.find(to).unwrap_or_else(|| panic!("no {to}")) + to.len()]
        };
        let branches = [
            slice(report_into, "let stop_at_deadline =", "};"),
            slice(finish, "if out.deadline_reached {", "return out;"),
            slice(
                options,
                "if stopped || (writer.pending() > 0 && !flush_fits(writer, now(), deadline)) {",
                "return OptionPersist::StoppedAtDeadline;",
            ),
        ];
        let mut counted = Vec::new();
        for branch in branches {
            for forbidden in ["discard_pending(", ".flush()", "flush_if_large("] {
                assert!(
                    !branch.contains(forbidden),
                    "a deadline stop must not call {forbidden}: it bumps a paging counter"
                );
            }
            if branch.contains("abandon_pending()") {
                counted.push(abandoned.to_string());
            }
            for (at, _) in branch.match_indices("metrics::counter!(") {
                let arg = branch[at + "metrics::counter!(".len()..].trim_start();
                let name = if let Some(lit) = arg.strip_prefix('"') {
                    lit[..lit.find('"').expect("closing quote")].to_string()
                } else {
                    let end = arg
                        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                        .unwrap_or(arg.len());
                    const_value(prod, &arg[..end]).to_string()
                };
                counted.push(name);
            }
        }
        assert!(counted.contains(&abandoned.to_string()));
        assert!(
            counted
                .contains(&const_value(prod, "XVERIFY_PERSIST_DEADLINE_STOPS_COUNTER").to_string())
        );
        for name in &counted {
            assert!(
                !tf.contains(name.as_str()),
                "a deliberate deadline stop counts on {name}, which a terraform \
                 alarm or filter reads: it would page per attempt"
            );
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
        assert!(!drive.contains("page_final_failure_once("));
        let body = fn_body(prod, "async fn run_day(");
        assert!(body.contains("page_final_failure_once("));
        assert!(body.contains("divergence_paged"));
        let run_once = fn_body(prod, "async fn run_once(");
        for alarmed in ["\"xverify_failed\"", "\"xverify_vacuous\""] {
            assert!(
                !run_once.contains(alarmed),
                "run_once must not page {alarmed} per attempt"
            );
        }
        assert!(run_once.contains("page_divergence_once(c, divergence_paged)"));
        assert!(
            fn_body(prod, "fn page_divergence_once(")
                .contains("!paged.swap(true, Ordering::Relaxed)")
        );
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
        let loop_end = body.rfind("page_final_failure_once(failure");
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

    // ---- §12.15.10 live-final readiness (plan item 51d) ----

    /// An IST midnight as fold seconds (the IST wall clock read as epoch).
    const DAY0: i64 = 20_000 * 86_400;

    fn day0() -> ReadinessDay {
        ReadinessDay::new(DAY0 * 1_000_000_000)
    }

    /// A sweep whose floor is `floor_sod` seconds into DAY0 and that completed
    /// `done_after_lf` seconds after the live-final instant.
    fn progress(floor_sod: i64, done_after_lf: i64) -> SealProgress {
        let d = day0();
        SealProgress {
            floor_fold_secs: u32::try_from(DAY0 + floor_sod).unwrap_or(0),
            done_unix_secs: u32::try_from(d.live_final_unix_secs + done_after_lf).unwrap_or(0),
        }
    }

    const CLOSE: i64 = SESSION_CLOSE_SECS_OF_DAY_IST;

    #[test]
    fn test_readiness_day_converts_the_fold_and_utc_clocks() {
        let d = day0();
        assert_eq!(d.day_start_fold_secs, DAY0);
        assert_eq!(d.close_fold_secs, DAY0 + CLOSE);
        assert_eq!(
            d.live_final_unix_secs,
            DAY0 + LIVE_FINAL_SECS_OF_DAY_IST - 19_800,
            "fold seconds are IST read as epoch; completion is UTC"
        );
    }

    #[test]
    fn test_classify_completeness_table() {
        let d = day0();
        let at = |first, latest, deadline| classify_completeness(d, first, latest, deadline);
        // No sample.
        assert_eq!(at(None, None, false), None);
        assert_eq!(at(None, None, true), Some(Completeness::Unknown));
        // Final is decided early, and needs both the floor and the instant.
        let fin = progress(CLOSE, 0);
        assert_eq!(at(Some(fin), Some(fin), false), Some(Completeness::Final));
        assert_eq!(
            at(None, Some(progress(CLOSE + 30, 9)), false),
            Some(Completeness::Final)
        );
        let early = progress(CLOSE, -1);
        assert_eq!(at(None, Some(early), false), None);
        assert_eq!(at(None, Some(early), true), Some(Completeness::Unknown));
        // A floor below the close waits for the deadline.
        let frozen = progress(CLOSE - 60, 1);
        assert_eq!(at(Some(frozen), Some(frozen), false), None);
        assert_eq!(
            at(Some(frozen), Some(frozen), true),
            Some(Completeness::Frozen {
                sealed_through_secs_of_day: CLOSE - 60
            })
        );
        let moved = progress(CLOSE - 30, 40);
        assert_eq!(
            at(Some(frozen), Some(moved), true),
            Some(Completeness::Moving {
                sealed_through_secs_of_day: CLOSE - 30
            })
        );
        assert_eq!(
            at(None, Some(moved), true),
            Some(Completeness::Moving {
                sealed_through_secs_of_day: CLOSE - 30
            })
        );
        // A floor that is not today's never reads Final or Frozen.
        for floor in [CLOSE - 86_400, 86_400 + 10, -1] {
            let p = progress(floor, 5);
            assert_eq!(at(Some(p), Some(p), false), None, "{floor}");
            assert_eq!(
                at(Some(p), Some(p), true),
                Some(Completeness::Unknown),
                "{floor}"
            );
        }
    }

    fn spill(staged: usize, parked: usize) -> std::io::Result<SpillStaged> {
        Ok(SpillStaged { staged, parked })
    }

    #[test]
    fn test_classify_durability_drain_must_be_strictly_after_the_reference() {
        let t = 1_000;
        let not_ready = |r| Err(r);
        assert_eq!(
            classify_durability(None, t, &spill(0, 0)),
            not_ready(DhanLiveXverifyNotReady::SealsPending)
        );
        assert_eq!(
            classify_durability(Some(t), t, &spill(0, 0)),
            not_ready(DhanLiveXverifyNotReady::SealsPending),
            "a drain in the same second may predate the sweep's hand-off"
        );
        assert_eq!(classify_durability(Some(t + 1), t, &spill(0, 0)), Ok(()));
        assert_eq!(
            classify_durability(Some(t + 1), t, &spill(2, 0)),
            not_ready(DhanLiveXverifyNotReady::SealsPending)
        );
        assert_eq!(
            classify_durability(Some(t + 1), t, &spill(1, 1)),
            not_ready(DhanLiveXverifyNotReady::SealSpillParked)
        );
        // A parked file of another day does not hold today's read.
        assert_eq!(classify_durability(Some(t + 1), t, &spill(0, 3)), Ok(()));
        // A folder that cannot be listed fails closed.
        let unreadable = Err(std::io::Error::other("unreadable"));
        assert_eq!(
            classify_durability(Some(t + 1), t, &unreadable),
            not_ready(DhanLiveXverifyNotReady::SealsPending)
        );
        // The drain is checked first.
        assert_eq!(
            classify_durability(Some(t), t, &spill(1, 1)),
            not_ready(DhanLiveXverifyNotReady::SealsPending)
        );
    }

    const NOT_READY: [DhanLiveXverifyNotReady; 4] = [
        DhanLiveXverifyNotReady::SealsPending,
        DhanLiveXverifyNotReady::SealSpillParked,
        DhanLiveXverifyNotReady::NotApplied,
        DhanLiveXverifyNotReady::CompletenessUnknown,
    ];

    fn every_readiness() -> Vec<LiveReadiness> {
        let mut durabilities = vec![Durability::Applied];
        durabilities.extend(NOT_READY.map(Durability::NotReady));
        let mut completeness = vec![Completeness::Final, Completeness::Unknown];
        for st in [0, CLOSE - 600, CLOSE - 300, CLOSE - 1, CLOSE, CLOSE + 60] {
            completeness.push(Completeness::Frozen {
                sealed_through_secs_of_day: st,
            });
            completeness.push(Completeness::Moving {
                sealed_through_secs_of_day: st,
            });
        }
        durabilities
            .iter()
            .flat_map(|&durability| {
                completeness.iter().map(move |&completeness| LiveReadiness {
                    durability,
                    completeness,
                })
            })
            .collect()
    }

    /// §12.15.10: the read plan is total and matches its table for every
    /// readiness, on the last attempt and before it.
    #[test]
    fn test_decide_read_is_total_and_matches_its_table() {
        let mut seen = 0;
        for r in every_readiness() {
            for is_last in [false, true] {
                seen += 1;
                let plan = decide_read(r, is_last);
                match plan {
                    ReadPlan::Retry(reason) => {
                        assert!(!is_last, "the last attempt never retries: {r:?}");
                        let expected = match r.durability {
                            Durability::NotReady(n) => retry_reason(n),
                            Durability::Applied => AttemptFailure::LiveNotFinal,
                        };
                        assert_eq!(reason, expected, "{r:?}");
                        if r.durability == Durability::Applied {
                            assert!(matches!(r.completeness, Completeness::Moving { .. }));
                        }
                    }
                    ReadPlan::Read { policy, check_late } => {
                        if check_late {
                            assert!(!is_last);
                            assert_eq!(r.durability, Durability::Applied);
                            assert_eq!(r.completeness, Completeness::Unknown);
                            assert_eq!(policy, ReadPolicy::STRICT);
                        }
                        if let Some(reason) = policy.not_ready {
                            assert!(is_last, "{r:?}");
                            assert!(!check_late);
                            assert_eq!(policy.late, LateWindowPolicy::Strict);
                            let expected = match r.durability {
                                Durability::NotReady(n) => n,
                                Durability::Applied => DhanLiveXverifyNotReady::CompletenessUnknown,
                            };
                            assert_eq!(reason, expected);
                        }
                        match (r.durability, r.completeness) {
                            (Durability::Applied, Completeness::Final) => {
                                assert_eq!(policy, ReadPolicy::STRICT);
                            }
                            (
                                Durability::Applied,
                                Completeness::Frozen {
                                    sealed_through_secs_of_day: st,
                                }
                                | Completeness::Moving {
                                    sealed_through_secs_of_day: st,
                                },
                            ) => {
                                assert_eq!(policy.late, LateWindowPolicy::excuse_after(st));
                                assert_eq!(policy.not_ready, None);
                            }
                            (Durability::Applied, Completeness::Unknown) => {
                                assert_eq!(check_late, !is_last);
                            }
                            (Durability::NotReady(_), _) => assert!(is_last),
                        }
                    }
                }
            }
        }
        assert_eq!(seen, 5 * 14 * 2);
    }

    #[test]
    fn test_every_attempt_failure_label_is_distinct() {
        let all = [
            AttemptFailure::NoToken,
            AttemptFailure::RunFailed,
            AttemptFailure::Vacuous,
            AttemptFailure::NotPersisted,
            AttemptFailure::Incomplete,
            AttemptFailure::MarkerNotWritten,
            AttemptFailure::AuditRowsLost,
            AttemptFailure::SkippedNoTime,
            AttemptFailure::LiveNotFinal,
            AttemptFailure::LiveNotApplied,
            AttemptFailure::SealsPending,
            AttemptFailure::SealSpillParked,
        ];
        // Exhaustive: a new variant fails to compile here until it is listed.
        for f in all {
            match f {
                AttemptFailure::NoToken
                | AttemptFailure::RunFailed
                | AttemptFailure::Vacuous
                | AttemptFailure::NotPersisted
                | AttemptFailure::Incomplete
                | AttemptFailure::MarkerNotWritten
                | AttemptFailure::AuditRowsLost
                | AttemptFailure::SkippedNoTime
                | AttemptFailure::LiveNotFinal
                | AttemptFailure::LiveNotApplied
                | AttemptFailure::SealsPending
                | AttemptFailure::SealSpillParked => {}
            }
        }
        let labels: std::collections::HashSet<_> = all.iter().map(|f| f.as_str()).collect();
        assert_eq!(labels.len(), all.len());
        // Each not-ready reason retries under its own label.
        let retry: std::collections::HashSet<_> = NOT_READY
            .iter()
            .map(|&n| retry_reason(n).as_str())
            .collect();
        assert_eq!(retry.len(), NOT_READY.len());
    }

    #[test]
    fn test_option_pass_read_table() {
        for r in every_readiness() {
            let read = option_pass_read(r);
            match (r.durability, r.completeness) {
                (Durability::NotReady(_), _) => assert_eq!(read, None),
                (Durability::Applied, Completeness::Final) => {
                    assert_eq!(read, Some(ReadPolicy::STRICT));
                }
                (
                    Durability::Applied,
                    Completeness::Frozen {
                        sealed_through_secs_of_day: st,
                    }
                    | Completeness::Moving {
                        sealed_through_secs_of_day: st,
                    },
                ) => assert_eq!(
                    read,
                    Some(ReadPolicy {
                        late: LateWindowPolicy::excuse_after(st),
                        not_ready: None,
                    })
                ),
                (Durability::Applied, Completeness::Unknown) => {
                    assert_eq!(read, Some(ReadPolicy::DERIVED_WINDOW));
                }
            }
            // The pass never reads unjudged: it is never the day's verdict.
            assert!(read.is_none_or(|p| p.not_ready.is_none()));
        }
    }

    /// §12.15.10: the attempt waits for the token and the readiness together,
    /// decides the read from the readiness, and returns a retry before any
    /// vendor call.
    #[test]
    fn test_run_once_joins_both_waits_and_retries_before_the_read() {
        let body = fn_body(prod_src(), "async fn run_once(");
        let join = body.find("tokio::join!(").expect("joined waits");
        let jwt = body.find("wait_for_jwt()").expect("token wait");
        let ready = body.find("attempt_readiness(").expect("readiness wait");
        let decide = body
            .find("decide_read(readiness, is_last)")
            .expect("decide");
        let retry = body.find("ReadPlan::Retry(reason)").expect("retry arm");
        let ret = body.find("return Err(reason);").expect("retry returns");
        let read = body.find("run_cross_verification(").expect("the read");
        assert!(join < jwt && jwt < ready && ready < decide, "{body}");
        assert!(decide < retry && retry < ret && ret < read);
        assert_eq!(body.matches("wait_for_jwt()").count(), 1);
        // The read takes the policy the readiness decided.
        assert!(body[read..].contains("        read,\n"));
        // The readiness deadline is bounded by the token wait and the attempt.
        assert!(body.contains(".min(deadline)"));
    }

    /// §12.15.10: the not-last unknown read that finds late minutes missing
    /// keeps only the vendor tape and retries `live_not_final` before the
    /// marker decision; it pages a price divergence as usual.
    #[test]
    fn test_run_once_late_check_keeps_only_the_tape_and_retries() {
        let body = fn_body(prod_src(), "async fn run_once(");
        let check = body
            .find("if check_late && c.missing_live_late > 0 {")
            .expect("late check");
        let branch = &body[check..];
        let end = branch
            .find("return Err(AttemptFailure::LiveNotFinal);")
            .expect("retry");
        let branch = &branch[..end];
        assert!(branch.contains("page_divergence_once(c, divergence_paged)"));
        assert!(branch.contains("scope: PersistScope::TapeOnly"));
        assert!(!branch.contains("record_day("));
        assert!(!branch.contains("classify_attempt("));
        let verdict = body.find("classify_attempt(").expect("marker decision");
        assert!(check < verdict);
        // The divergence page is the moved helper, called once on each path.
        assert_eq!(body.matches("page_divergence_once(").count(), 2);
        assert!(!body.contains("source = \"xverify_diverged\""));
        assert!(
            fn_body(prod_src(), "fn page_divergence_once(")
                .contains("source = \"xverify_diverged\"")
        );
    }

    /// §12.15.10: a tape-only persist writes the vendor tape and nothing
    /// else: no findings and no daily row, so it can never mark the day.
    #[test]
    fn test_tape_only_persist_writes_no_findings_and_no_daily_row() {
        let report = report_with(5, 3);
        let mut writer = DhanLiveXverifyAuditWriter::for_test();
        let t0 = tokio::time::Instant::now();
        let far = t0 + Duration::from_secs(86_400);
        let rows = PersistRows {
            scope: PersistScope::TapeOnly,
            ..test_rows()
        };
        let out = persist_report_into(&mut writer, &report, rows, 2, far, || t0);
        assert!(!out.daily_appended);
        assert_eq!(out.cell_append_errors, 0);
        // No sender, so every flush discards: only the 3 tape rows were
        // ever appended.
        assert_eq!(out.rows_discarded, 3, "{out:?}");
        assert_eq!(persist_verdict(&out), Err(AttemptFailure::NotPersisted));
        // Past the deadline it counts only the tape rows as not written.
        let mut writer = DhanLiveXverifyAuditWriter::for_test();
        let out = persist_report_into(&mut writer, &report, rows, 2, t0, || t0);
        assert_eq!(out.rows_not_written_at_deadline, 3, "{out:?}");
    }

    /// §12.15.10: the two new sources are warnings, and no alarm filter
    /// matches them (noise lock §2.5: no new page).
    #[test]
    fn test_readiness_sources_are_warnings_with_no_alarm_filter() {
        let prod = prod_src();
        let tf = include_str!("../../../deploy/aws/terraform/error-code-alarms.tf");
        for source in ["xverify_attempt_not_ready", "xverify_unsettled_final"] {
            let emit = format!("source = \"{source}\"");
            assert!(prod.contains(emit.as_str()), "{source} is not emitted");
            for (at, _) in prod.match_indices(emit.as_str()) {
                let head = &prod[..at];
                let warn_at = head.rfind("warn!(").map_or(0, |i| i + 1);
                let error_at = head.rfind("error!(").map_or(0, |i| i + 1);
                assert!(warn_at > error_at, "{source} must be a warn!");
            }
            assert!(!tf.contains(source), "{source} must match no alarm filter");
        }
    }

    /// §12.15.10: the option pass reads with the spot readiness, waits on its
    /// own beside the token only when it has none, and skips (never reads
    /// unjudged) when the sealed bars were not saved.
    #[test]
    fn test_run_option_pass_skips_when_not_ready_and_reads_with_the_policy() {
        let body = fn_body(prod_src(), "async fn run_option_pass(");
        assert!(body.contains("spot_readiness: Option<LiveReadiness>"));
        assert!(body.contains("tokio::join!("));
        let gate = body.find("option_pass_read(readiness)").expect("gate");
        let skip = body.find("\"skipped_not_ready\"").expect("skip label");
        let read = body.find("run_cross_verification(").expect("the read");
        assert!(gate < skip && skip < read);
        assert!(body[read..].contains("        read,\n"));
        assert!(XVERIFY_OPTION_PASS_OUTCOMES.contains(&"skipped_not_ready"));
        let run_day = fn_body(prod_src(), "async fn run_day(");
        assert!(run_day.contains("spot_readiness.lock()"));
    }

    // ---- wait_live_final against a paused clock ----

    struct FakeReadiness {
        t0: tokio::time::Instant,
        start_unix: i64,
        progress: Box<dyn Fn(i64) -> Option<SealProgress>>,
        drained: Box<dyn Fn(i64) -> Option<i64>>,
        staged: Box<dyn Fn(i64) -> usize>,
        wal: Box<dyn Fn(i64) -> Option<Vec<WalTableRow>>>,
        wal_reads: std::cell::Cell<u32>,
    }

    impl FakeReadiness {
        fn elapsed(&self) -> i64 {
            i64::try_from((tokio::time::Instant::now() - self.t0).as_secs()).unwrap_or(i64::MAX)
        }
    }

    impl ReadinessSource for FakeReadiness {
        fn seal_progress(&self) -> Option<SealProgress> {
            (self.progress)(self.elapsed())
        }
        fn last_drained_unix_secs(&self) -> Option<i64> {
            (self.drained)(self.now_unix_secs())
        }
        fn staged_spill(
            &self,
            _date: chrono::NaiveDate,
        ) -> impl Future<Output = std::io::Result<SpillStaged>> + Send {
            let staged = spill((self.staged)(self.elapsed()), 0);
            async move { staged }
        }
        fn wal_tables(&self) -> impl Future<Output = anyhow::Result<Vec<WalTableRow>>> + Send {
            self.wal_reads.set(self.wal_reads.get() + 1);
            let rows = (self.wal)(self.elapsed());
            async move { rows.ok_or_else(|| anyhow::anyhow!("questdb unreachable")) }
        }
        fn now_unix_secs(&self) -> i64 {
            self.start_unix + self.elapsed()
        }
    }

    fn wal_row(sequencer: i64, writer: i64) -> WalTableRow {
        WalTableRow {
            name: LIVE_READ_TABLE.to_string(),
            suspended: false,
            writer_txn: Some(writer),
            sequencer_txn: Some(sequencer),
            error_tag: None,
            error_message: None,
        }
    }

    fn fake(after_lf: i64) -> FakeReadiness {
        FakeReadiness {
            t0: tokio::time::Instant::now(),
            start_unix: day0().live_final_unix_secs + after_lf,
            progress: Box::new(|_| None),
            drained: Box::new(|now| Some(now)),
            staged: Box::new(|_| 0),
            wal: Box::new(|_| Some(vec![wal_row(7, 7)])),
            wal_reads: std::cell::Cell::new(0),
        }
    }

    fn today0() -> chrono::NaiveDate {
        chrono::NaiveDate::from_ymd_opt(2024, 10, 4).unwrap_or_default()
    }

    async fn wait(src: &FakeReadiness, secs: u64) -> (LiveReadiness, u64) {
        let deadline = src.t0 + Duration::from_secs(secs);
        let (r, _) = wait_live_final(src, today0(), DAY0 * 1_000_000_000, deadline, None).await;
        (r, (tokio::time::Instant::now() - src.t0).as_secs())
    }

    #[tokio::test(start_paused = true)]
    async fn test_wait_live_final_returns_at_once_when_final_and_applied() {
        let mut src = fake(10);
        src.progress = Box::new(|_| Some(progress(CLOSE, 5)));
        let (r, took) = wait(&src, 300).await;
        assert_eq!(took, 0);
        assert_eq!(
            r,
            LiveReadiness {
                durability: Durability::Applied,
                completeness: Completeness::Final,
            }
        );
    }

    #[tokio::test(start_paused = true)]
    async fn test_wait_live_final_is_bounded_by_its_deadline() {
        let mut src = fake(10);
        src.drained = Box::new(|_| None);
        let (r, took) = wait(&src, 300).await;
        assert_eq!(took, 300);
        assert_eq!(
            r,
            LiveReadiness {
                durability: Durability::NotReady(DhanLiveXverifyNotReady::SealsPending),
                completeness: Completeness::Unknown,
            }
        );
        assert_eq!(src.wal_reads.get(), 0, "no snapshot before the drain held");
    }

    #[tokio::test(start_paused = true)]
    async fn test_wait_live_final_sleeps_until_the_live_final_instant() {
        let mut src = fake(-100);
        src.progress = Box::new(|_| Some(progress(CLOSE, 0)));
        let (r, took) = wait(&src, 300).await;
        // It sleeps 100 s to the instant; the drain sampled there is in the
        // same second as the reference, so the next sample, 5 s on, holds.
        assert_eq!(took, 105);
        assert_eq!(r.completeness, Completeness::Final);
        assert_eq!(r.durability, Durability::Applied);
        // A deadline before the instant ends the wait at the deadline.
        let src = fake(-100);
        let (r, took) = wait(&src, 40).await;
        assert_eq!(took, 40);
        assert_eq!(r.completeness, Completeness::Unknown);
    }

    #[tokio::test(start_paused = true)]
    async fn test_wait_live_final_frozen_floor_and_unapplied_wal_at_the_deadline() {
        let mut src = fake(10);
        src.progress = Box::new(|_| Some(progress(CLOSE - 60, 1)));
        src.wal = Box::new(|_| Some(vec![wal_row(9, 8)]));
        let (r, took) = wait(&src, 120).await;
        assert_eq!(took, 120);
        assert_eq!(
            r,
            LiveReadiness {
                durability: Durability::NotReady(DhanLiveXverifyNotReady::NotApplied),
                completeness: Completeness::Frozen {
                    sealed_through_secs_of_day: CLOSE - 60
                },
            }
        );
        // An unreadable `wal_tables()` reads not applied too.
        let mut src = fake(10);
        src.progress = Box::new(|_| Some(progress(CLOSE, 0)));
        src.wal = Box::new(|_| None);
        let (r, _) = wait(&src, 60).await;
        assert_eq!(
            r.durability,
            Durability::NotReady(DhanLiveXverifyNotReady::NotApplied)
        );
        assert_eq!(r.completeness, Completeness::Final);
    }

    /// A new floor re-arms the barriers: the drain must come after the sweep
    /// that sealed the newest bars.
    #[tokio::test(start_paused = true)]
    async fn test_wait_live_final_rearms_when_the_floor_moves() {
        let mut src = fake(10);
        // Frozen below the close for 30 s, then a sweep at +40 s reaches it.
        src.progress = Box::new(|elapsed| {
            Some(if elapsed < 30 {
                progress(CLOSE - 120, 1)
            } else {
                progress(CLOSE, 40)
            })
        });
        // The drain lags the clock by 5 s.
        src.drained = Box::new(|now| Some(now - 5));
        let (r, took) = wait(&src, 300).await;
        assert_eq!(r.completeness, Completeness::Final);
        assert_eq!(r.durability, Durability::Applied);
        // Reference = live_final + 40, so the drain (now - 5) passes it at
        // elapsed > 35; the next 5 s sample after the floor moved is 40.
        assert_eq!(took, 40, "returned before the drain covered the new sweep");
    }

    /// Spill files staged for today hold the read.
    #[tokio::test(start_paused = true)]
    async fn test_wait_live_final_waits_for_staged_spill_files() {
        let mut src = fake(10);
        src.progress = Box::new(|_| Some(progress(CLOSE, 0)));
        src.staged = Box::new(|_| 1);
        let (r, took) = wait(&src, 30).await;
        assert_eq!(took, 30);
        assert_eq!(
            r.durability,
            Durability::NotReady(DhanLiveXverifyNotReady::SealsPending)
        );
    }

    /// 51d review: the spill listing is blocking file I/O, so production runs
    /// it on the blocking pool, and the wait bounds it by the deadline
    /// (a listing that does not finish reads pending).
    #[test]
    fn test_production_spill_listing_runs_off_the_workers_under_the_deadline() {
        let src = include_str!("dhan_live_crossverify_boot.rs");
        let prod = src.split("#[cfg(test)]\nmod tests").next().unwrap_or(src);
        assert!(prod.contains(
            "tokio::task::spawn_blocking(move || {\n            tickvault_storage::seal_spill::staged_production_spill_records_for_day(date)"
        ));
        assert!(
            prod.contains("tokio::time::timeout_at(deadline, src.staged_spill(today))"),
            "the listing is bounded by the deadline"
        );
        assert!(prod.contains("Err(std::io::ErrorKind::TimedOut.into())"));
    }

    /// 51d review: a floor that moved in the deadline sample re-arms the
    /// barriers, and the probe at the deadline cannot finish; the wait then
    /// reports the last floor whose bars it saw saved, as applied, so a last
    /// attempt excuses after that floor instead of reading unjudged.
    #[tokio::test(start_paused = true)]
    async fn test_wait_live_final_reports_the_last_applied_floor_at_the_deadline() {
        let mut src = fake(10);
        // The floor steps 10 s every 50 s, below the close; the step at 300
        // lands on the deadline sample.
        src.progress =
            Box::new(|elapsed| Some(progress(CLOSE - 240 + 10 * (elapsed / 50), elapsed)));
        src.drained = Box::new(|now| Some(now - 15));
        let (r, took) = wait(&src, 300).await;
        assert_eq!(took, 300);
        assert_eq!(
            r,
            LiveReadiness {
                durability: Durability::Applied,
                completeness: Completeness::Moving {
                    sealed_through_secs_of_day: CLOSE - 190
                },
            }
        );
    }

    /// 51d review: once a floor at or past the close is seen, later floors
    /// (post-close trades keep the watermark moving) do not re-arm the
    /// barriers; every session bucket was sealed by the sweep that crossed.
    #[tokio::test(start_paused = true)]
    async fn test_wait_live_final_does_not_rearm_past_the_close() {
        let mut src = fake(10);
        src.progress = Box::new(|elapsed| Some(progress(CLOSE + elapsed, elapsed)));
        src.drained = Box::new(|now| Some(now - 15));
        let (r, took) = wait(&src, 300).await;
        assert_eq!(r.completeness, Completeness::Final);
        assert_eq!(r.durability, Durability::Applied);
        // The reference stays at the crossing sweep (live-final + 0), which
        // the lagging drain passes at the third sample.
        assert_eq!(took, 10);
    }

    /// 51d review: a barrier that fails after the WAL snapshot drops it, so
    /// the next snapshot covers seals written since (a replayed spill file).
    #[tokio::test(start_paused = true)]
    async fn test_wait_live_final_takes_a_fresh_snapshot_after_a_barrier_fails() {
        let mut src = fake(10);
        src.progress = Box::new(|_| Some(progress(CLOSE - 60, 0)));
        src.staged = Box::new(|elapsed| usize::from((20..30).contains(&elapsed)));
        src.wal = Box::new(|elapsed| {
            Some(vec![if elapsed < 20 {
                wal_row(9, 8)
            } else {
                wal_row(12, 10)
            }])
        });
        let (r, _) = wait(&src, 60).await;
        // Against the old snapshot (9) writer 10 would read applied.
        assert_eq!(
            r.durability,
            Durability::NotReady(DhanLiveXverifyNotReady::NotApplied)
        );
    }

    /// 51d review: `Frozen` is judged from the day's first sample after the
    /// live-final instant, carried over from an earlier attempt, not from
    /// this attempt's own first sample.
    #[tokio::test(start_paused = true)]
    async fn test_wait_live_final_judges_frozen_against_the_days_first_sample() {
        let mut src = fake(10);
        src.progress = Box::new(|_| Some(progress(CLOSE - 60, 600)));
        let earlier = progress(CLOSE - 120, 1);
        let deadline = src.t0 + Duration::from_secs(60);
        let (r, first) = wait_live_final(
            &src,
            today0(),
            DAY0 * 1_000_000_000,
            deadline,
            Some(earlier),
        )
        .await;
        assert_eq!(first, Some(earlier));
        assert_eq!(
            r.completeness,
            Completeness::Moving {
                sealed_through_secs_of_day: CLOSE - 60
            }
        );
        // Without the earlier sample the same attempt reads it frozen.
        let (r, _) = wait(&src, 60).await;
        assert_eq!(
            r.completeness,
            Completeness::Frozen {
                sealed_through_secs_of_day: CLOSE - 60
            }
        );
    }
}
