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

use std::sync::Arc;
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

use crate::daily_task_marker::{daily_marker_exists, write_daily_marker};
use crate::dhan_live_crossverify::{
    DayComparison, DhanLiveCrossverifyConfig, RUN_SECS_OF_DAY_IST, RunReport,
    SESSION_CLOSE_SECS_OF_DAY_IST, XverifyTarget, daily_row, deterministic_run_ts_nanos,
    run_cross_verification,
};
use crate::volume_leaderboard::OptionFamily;

/// Marker task name. The S3 archive gate in `main.rs` reads the same constant,
/// so the writer and the reader can never disagree about the file name.
pub const CROSSVERIFY_MARKER_TASK: &str = "dhan_live_crossverify";

/// Runs counter, labelled by `outcome`
/// (`measured` / `vacuous` / `diverged` / `failed` / `no_token`). Re-exported
/// from the feed stack, which has owned the name since 2026-08-26, so one
/// metric has exactly one declaration.
pub use crate::dhan_feed_stack::XVERIFY_RUNS_COUNTER;
/// Rows the run persisted (findings + vendor tape + the daily row).
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
pub const XVERIFY_RETRY_INTERVAL_SECS: u64 = 900;

/// The most attempts one trading day gets, the first one included.
pub const XVERIFY_MAX_ATTEMPTS_PER_DAY: u32 = 4;

/// 17:30 IST — the scheduled evening stop of the box. An attempt that could
/// still be running at this time is not started, because the stop would kill
/// it half-way.
pub const EVENING_STOP_SECS_OF_DAY_IST: u64 = 17 * 3_600 + 30 * 60;

/// Room left after the run budget for the audit flush and the marker write.
const PERSIST_MARGIN_SECS: u64 = 60;

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
/// still be running at the evening stop. Pure, O(1).
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
    if next_end > EVENING_STOP_SECS_OF_DAY_IST {
        return None;
    }
    Some(XVERIFY_RETRY_INTERVAL_SECS)
}

/// Whether one finished attempt recorded the day, and if not, why. The
/// checks run in a fixed order so the reason names the first thing that went
/// wrong. `Ok` is exactly the condition under which the marker is written.
/// Pure, O(1).
pub fn classify_attempt(
    vacuous: bool,
    measured: bool,
    persisted_ok: bool,
    complete: bool,
) -> Result<(), AttemptFailure> {
    if vacuous {
        return Err(AttemptFailure::Vacuous);
    }
    if !measured {
        return Err(AttemptFailure::Incomplete);
    }
    if !persisted_ok {
        return Err(AttemptFailure::NotPersisted);
    }
    if !complete {
        return Err(AttemptFailure::Incomplete);
    }
    Ok(())
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

/// Runs today's check, retrying on the same day until it records the day or
/// the retry window closes. Each attempt is bounded by the token wait and the
/// run budget; the number of attempts is bounded by
/// [`XVERIFY_MAX_ATTEMPTS_PER_DAY`] and by the evening stop.
async fn run_day(
    deps: &CrossverifyBootDeps,
    targets: &[XverifyTarget],
    today: chrono::NaiveDate,
    day_start_ist_nanos: i64,
) {
    let max_attempt_secs = attempt_max_secs(deps.config.run_budget_secs);
    let mut attempts: u32 = 0;
    let mut divergence_paged = false;
    loop {
        attempts = attempts.saturating_add(1);
        let outcome = run_once(
            deps,
            targets,
            today,
            day_start_ist_nanos,
            &mut divergence_paged,
        )
        .await;
        let failure = match outcome {
            Ok(()) => {
                if attempts > 1 {
                    info!(%today, attempts, "Dhan 1-minute cross-verification recorded on a same-day retry");
                }
                break;
            }
            Err(failure) => failure,
        };
        match retry_delay_secs(attempts, now_ist_secs_of_day(), max_attempt_secs) {
            Some(delay) => {
                metrics::counter!(XVERIFY_RETRIES_COUNTER, "reason" => failure.as_str())
                    .increment(1);
                warn!(
                    code = ErrorCode::WsGapConnectionState.code_str(),
                    source = "xverify_retry",
                    %today,
                    reason = failure.as_str(),
                    attempt = attempts,
                    max_attempts = XVERIFY_MAX_ATTEMPTS_PER_DAY,
                    retry_in_secs = delay,
                    "Dhan 1-minute cross-verification did not record today — retrying later today"
                );
                tokio::time::sleep(Duration::from_secs(delay)).await;
                // The day can only change here if the process ran past
                // midnight, which the evening-stop bound rules out; checked
                // anyway so a retry can never verify the wrong day.
                if today_ist().0 != today {
                    return;
                }
            }
            None => {
                report_final_failure(failure, today, attempts, targets.len());
                break;
            }
        }
    }
    // §12.15.6: the depth-held option pass runs ONCE, after the spot check's
    // outcome is final. It never writes the day marker and never pages.
    run_option_pass(deps, today, day_start_ist_nanos).await;
}

/// Pages once, after the last attempt of the day. Each arm is its own
/// `error!` so every alarmed `source` stays a literal the alarm filter can
/// match.
fn report_final_failure(
    failure: AttemptFailure,
    today: chrono::NaiveDate,
    attempts: u32,
    targets: usize,
) {
    let reason = failure.as_str();
    match failure {
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
        AttemptFailure::NoToken
        | AttemptFailure::RunFailed
        | AttemptFailure::NotPersisted
        | AttemptFailure::Incomplete => error!(
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
async fn run_once(
    deps: &CrossverifyBootDeps,
    targets: &[XverifyTarget],
    today: chrono::NaiveDate,
    day_start_ist_nanos: i64,
    divergence_paged: &mut bool,
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
        &deps.config,
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
            let persisted_ok = persist_report(
                &deps.questdb,
                &report,
                day_start_ist_nanos,
                deps.config.tolerance_paise,
            );
            if is_catastrophic_divergence(c) && !*divergence_paged {
                *divergence_paged = true;
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
            let verdict = classify_attempt(
                c.is_vacuous(),
                c.outcome.is_measured(),
                persisted_ok,
                complete,
            );
            // The marker condition must stay exactly `should_write_marker`
            // plus a complete run; the two pure functions agree by test.
            debug_assert_eq!(
                verdict.is_ok(),
                should_write_marker(c, persisted_ok) && complete
            );
            match verdict {
                Ok(()) => {
                    write_daily_marker(CROSSVERIFY_MARKER_TASK, today);
                    info!(%today, "Dhan 1-minute cross-verification recorded — today's S3 archive may proceed");
                }
                Err(failure) => warn!(
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
                    "Dhan 1-minute cross-verification attempt did not record today"
                ),
            }
            verdict
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

/// Persists the run. Returns `true` only when the final flush succeeded AND the
/// daily row was appended — that is what the marker depends on.
fn persist_report(
    questdb: &QuestDbConfig,
    report: &RunReport,
    day_start_ist_nanos: i64,
    tolerance_paise: i64,
) -> bool {
    let c = &report.comparison;
    let mut writer = DhanLiveXverifyAuditWriter::new(questdb);
    let mut batch_errors = 0_usize;
    let flush_if_full = |w: &mut DhanLiveXverifyAuditWriter, errs: &mut usize| {
        let failed = if w.pending() >= PERSIST_BATCH_ROWS {
            w.flush().is_err()
        } else {
            w.flush_if_large().is_err()
        };
        if failed {
            *errs = errs.saturating_add(1);
        }
    };

    let mut cell_errors = 0_usize;
    for finding in &c.findings {
        if writer.append_cell(finding).is_err() {
            cell_errors = cell_errors.saturating_add(1);
        }
        flush_if_full(&mut writer, &mut batch_errors);
    }
    let mut tape_errors = 0_usize;
    for row in &report.rest_tape {
        if writer.append_rest_tape(row).is_err() {
            tape_errors = tape_errors.saturating_add(1);
        }
        flush_if_full(&mut writer, &mut batch_errors);
    }
    let daily = daily_row(
        c,
        day_start_ist_nanos,
        deterministic_run_ts_nanos(day_start_ist_nanos),
        tolerance_paise,
    );
    let daily_failed = writer.append_daily(&daily).is_err();

    match writer.flush() {
        Ok(()) => {
            metrics::counter!(XVERIFY_PERSIST_ROWS_COUNTER)
                .increment(c.findings.len() as u64 + report.rest_tape.len() as u64 + 1);
            if cell_errors > 0 || tape_errors > 0 || batch_errors > 0 || daily_failed {
                error!(
                    code = ErrorCode::WsGapConnectionState.code_str(),
                    source = "xverify_persist_partial",
                    cell_errors,
                    tape_errors,
                    batch_errors,
                    daily_failed,
                    findings = c.findings.len(),
                    tape_rows = report.rest_tape.len(),
                    "Dhan 1-minute cross-verification persisted with gaps — the audit \
                     tables are incomplete for today"
                );
            }
            !daily_failed
        }
        Err(err) => {
            let discarded = writer.discard_pending();
            metrics::counter!(XVERIFY_PERSIST_ERRORS_COUNTER).increment(1);
            error!(
                code = ErrorCode::WsGapConnectionState.code_str(),
                source = "xverify_persist_failed",
                ?err,
                discarded,
                "Dhan 1-minute cross-verification could NOT be persisted — today's \
                 comparison exists only in this log stream and today's S3 archive stays held"
            );
            false
        }
    }
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

/// Whether the option pass can still finish before the evening stop, starting
/// now. Pure, O(1).
#[must_use]
pub const fn option_pass_fits(now_secs_of_day: u64) -> bool {
    now_secs_of_day.saturating_add(attempt_max_secs(XVERIFY_OPTION_PASS_BUDGET_SECS))
        <= EVENING_STOP_SECS_OF_DAY_IST
}

/// Every `outcome` label the option pass can publish. Seeded at zero at the
/// start of each pass so a label reads as a real zero on `/metrics` rather
/// than an absent series.
pub const XVERIFY_OPTION_PASS_OUTCOMES: [&str; 8] = [
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
async fn run_option_pass(
    deps: &CrossverifyBootDeps,
    today: chrono::NaiveDate,
    day_start_ist_nanos: i64,
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
            let persisted_ok = persist_option_findings(&deps.questdb, &report);
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
fn persist_option_findings(questdb: &QuestDbConfig, report: &RunReport) -> bool {
    let c = &report.comparison;
    let mut writer = DhanLiveXverifyAuditWriter::new(questdb);
    let mut row_errors = 0_usize;
    let mut batch_errors = 0_usize;
    let mut flush_if_full = |w: &mut DhanLiveXverifyAuditWriter| {
        let failed = if w.pending() >= PERSIST_BATCH_ROWS {
            w.flush().is_err()
        } else {
            w.flush_if_large().is_err()
        };
        if failed {
            batch_errors = batch_errors.saturating_add(1);
        }
    };
    for finding in &c.findings {
        if writer.append_cell(finding).is_err() {
            row_errors = row_errors.saturating_add(1);
        }
        flush_if_full(&mut writer);
    }
    for row in &report.rest_tape {
        if writer.append_rest_tape(row).is_err() {
            row_errors = row_errors.saturating_add(1);
        }
        flush_if_full(&mut writer);
    }
    match writer.flush() {
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
            true
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
            false
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

    #[test]
    fn test_marker_write_needs_a_complete_run() {
        let src = include_str!("dhan_live_crossverify_boot.rs");
        let body = src
            .split("async fn run_once(")
            .nth(1)
            .and_then(|s| s.split("\n}\n").next())
            .unwrap_or("");
        assert!(
            body.contains("classify_attempt("),
            "the marker decision must go through classify_attempt"
        );
        assert!(
            body.contains("run_is_complete("),
            "the marker must also require a complete run"
        );
        assert_eq!(
            body.matches("write_daily_marker(").count(),
            1,
            "exactly one marker write, on the Ok arm"
        );
        let ok_arm = body.find("Ok(()) => {").unwrap_or(usize::MAX);
        assert_ne!(ok_arm, usize::MAX, "an Ok arm must exist");
        let write = body.find("write_daily_marker(").unwrap_or(0);
        assert!(
            write > ok_arm,
            "the marker write must sit inside the Ok arm"
        );
    }

    /// `classify_attempt` returns `Ok` exactly when the old marker rule held:
    /// `should_write_marker && complete`. Checked over every outcome, both
    /// vacuous and measured minute counts, and every persisted/complete pair.
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
        for outcome in outcomes {
            for minutes in [0_i64, 375] {
                let c = comparison(outcome, minutes, 0);
                for persisted_ok in [false, true] {
                    for complete in [false, true] {
                        let verdict = classify_attempt(
                            c.is_vacuous(),
                            c.outcome.is_measured(),
                            persisted_ok,
                            complete,
                        );
                        assert_eq!(
                            verdict.is_ok(),
                            should_write_marker(&c, persisted_ok) && complete,
                            "{outcome:?} minutes={minutes} persisted={persisted_ok} \
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
        assert_eq!(classify_attempt(true, true, true, true), Err(Vacuous));
        assert_eq!(classify_attempt(true, false, false, false), Err(Vacuous));
        // Degraded with minutes compared: not vacuous, not measured.
        assert_eq!(classify_attempt(false, false, true, true), Err(Incomplete));
        assert_eq!(
            classify_attempt(false, true, false, true),
            Err(NotPersisted)
        );
        assert_eq!(
            classify_attempt(false, true, false, false),
            Err(NotPersisted)
        );
        assert_eq!(classify_attempt(false, true, true, false), Err(Incomplete));
        assert_eq!(classify_attempt(false, true, true, true), Ok(()));
    }

    #[test]
    fn test_attempt_failure_labels_are_distinct() {
        let all = [
            AttemptFailure::NoToken,
            AttemptFailure::RunFailed,
            AttemptFailure::Vacuous,
            AttemptFailure::NotPersisted,
            AttemptFailure::Incomplete,
        ];
        let mut labels: Vec<&str> = all.iter().map(|f| f.as_str()).collect();
        labels.sort_unstable();
        labels.dedup();
        assert_eq!(labels.len(), all.len());
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
        // The next attempt would still be running at the evening stop.
        let last_start = EVENING_STOP_SECS_OF_DAY_IST - XVERIFY_RETRY_INTERVAL_SECS - max;
        assert!(retry_delay_secs(1, last_start, max).is_some());
        assert_eq!(retry_delay_secs(1, last_start + 1, max), None);
        // After the stop (a manual evening boot catch-up): no retry, no overflow.
        assert_eq!(retry_delay_secs(1, EVENING_STOP_SECS_OF_DAY_IST, max), None);
        assert_eq!(retry_delay_secs(1, u64::MAX, max), None);
        assert_eq!(retry_delay_secs(1, run, u64::MAX), None);
    }

    /// With the default budget, a 15:41 run whose every attempt takes the
    /// longest it can still gets all its attempts in before 17:30.
    #[test]
    fn test_worst_case_day_fits_every_attempt_before_the_evening_stop() {
        let max = attempt_max_secs(600);
        let mut now = XVERIFY_RUN_AT_SECS_OF_DAY_IST;
        let mut attempts = 0_u32;
        loop {
            attempts += 1;
            now += max;
            assert!(now <= EVENING_STOP_SECS_OF_DAY_IST);
            match retry_delay_secs(attempts, now, max) {
                Some(delay) => now += delay,
                None => break,
            }
        }
        assert_eq!(attempts, XVERIFY_MAX_ATTEMPTS_PER_DAY);
    }

    /// The retry loop must use the pure bound and page only after it, and
    /// `run_once` must never emit an alarmed source itself except the
    /// once-per-day divergence page.
    #[test]
    fn test_run_day_retries_through_the_pure_bound() {
        let src = include_str!("dhan_live_crossverify_boot.rs");
        let prod = src.split("#[cfg(test)]").next().unwrap_or("");
        let body = prod
            .split("async fn run_day(")
            .nth(1)
            .and_then(|s| s.split("\n}\n").next())
            .unwrap_or("");
        assert!(body.contains("retry_delay_secs("));
        assert!(body.contains("report_final_failure("));
        assert!(body.contains("divergence_paged"));
        let run_once = prod
            .split("async fn run_once(")
            .nth(1)
            .and_then(|s| s.split("\n}\n").next())
            .unwrap_or("");
        for alarmed in ["\"xverify_failed\"", "\"xverify_vacuous\""] {
            assert!(
                !run_once.contains(alarmed),
                "run_once must not page {alarmed} per attempt"
            );
        }
        assert!(run_once.contains("!*divergence_paged"));
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
            .split("fn persist_report(")
            .nth(1)
            .expect("persist_report must exist");
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
    fn test_option_pass_fits_respects_the_evening_stop() {
        let need = attempt_max_secs(XVERIFY_OPTION_PASS_BUDGET_SECS);
        assert!(option_pass_fits(EVENING_STOP_SECS_OF_DAY_IST - need));
        assert!(!option_pass_fits(EVENING_STOP_SECS_OF_DAY_IST - need + 1));
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
        let call = body.find("run_option_pass(deps, today, day_start_ist_nanos)");
        let last_break = body.rfind("break;");
        assert!(call.is_some(), "run_day must call run_option_pass");
        assert!(
            call > last_break,
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
        for label in ["vacuous", "measured", "partial"] {
            assert!(XVERIFY_OPTION_PASS_OUTCOMES.contains(&label));
        }
    }
}
