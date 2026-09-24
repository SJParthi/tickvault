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

/// Token checks before the run gives up: 60 × 5 s = 5 minutes.
///
/// 2026-09-24: the first live run was a boot catch-up at 18:32 IST. It started
/// before the token manager had loaded a token and failed at once, so the day
/// stayed unverified and its S3 archive stayed held. A normal boot has the
/// token within seconds; five minutes covers a slow mint with room to spare.
pub const TOKEN_WAIT_MAX_POLLS: u32 = 60;

/// Waits for the token manager to hold a token. O(1) per poll, bounded.
async fn wait_for_jwt() -> Option<SecretString> {
    for poll in 0..=TOKEN_WAIT_MAX_POLLS {
        if let Some(jwt) = current_jwt() {
            if poll > 0 {
                info!(
                    waited_secs = TOKEN_WAIT_POLL_SECS * u64::from(poll),
                    "Dhan 1-minute cross-verification: token became available"
                );
            }
            return Some(jwt);
        }
        if poll < TOKEN_WAIT_MAX_POLLS {
            tokio::time::sleep(Duration::from_secs(TOKEN_WAIT_POLL_SECS)).await;
        }
    }
    None
}

fn current_jwt() -> Option<SecretString> {
    let manager = global_token_manager()?;
    let guard = manager.token_handle().load();
    guard
        .as_ref()
        .as_ref()
        .map(|state| SecretString::from(state.access_token().expose_secret().to_string()))
}

/// Spawns the daily cross-verification task for the subscribed universe.
// TEST-EXEMPT: spawns a tokio task that waits for 15:41 IST and calls the vendor; its pure decisions (targets, schedule, catch-up, marker, divergence) are tested above and its emit contract by test_every_xverify_alarm_source_has_a_live_error_emit
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
            run_once(&deps, &targets, today, day_start_ist_nanos).await;
        }
    })
}

async fn run_once(
    deps: &CrossverifyBootDeps,
    targets: &[XverifyTarget],
    today: chrono::NaiveDate,
    day_start_ist_nanos: i64,
) {
    let Some(jwt) = wait_for_jwt().await else {
        metrics::counter!(XVERIFY_RUNS_COUNTER, "outcome" => "no_token").increment(1);
        error!(
            code = ErrorCode::WsGapConnectionState.code_str(),
            source = "xverify_failed",
            waited_secs = TOKEN_WAIT_POLL_SECS * u64::from(TOKEN_WAIT_MAX_POLLS),
            "Dhan 1-minute cross-verification could not run: no Dhan token available. \
             Today's candles are UNVERIFIED and today's S3 archive stays held."
        );
        return;
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
            if is_catastrophic_divergence(c) {
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
            if c.is_vacuous() {
                error!(
                    code = ErrorCode::WsGapConnectionState.code_str(),
                    source = "xverify_vacuous",
                    targets = targets.len(),
                    missing_live = c.missing_live,
                    missing_rest = c.missing_rest,
                    "Dhan 1-minute cross-verification compared ZERO minutes — today's \
                     candles are UNVERIFIED and today's S3 archive stays held. This is not \
                     a pass; it is no measurement at all."
                );
            }
            if should_write_marker(c, persisted_ok) {
                write_daily_marker(CROSSVERIFY_MARKER_TASK, today);
                info!(%today, "Dhan 1-minute cross-verification recorded — today's S3 archive may proceed");
            }
        }
        Err(err) => {
            metrics::counter!(XVERIFY_RUNS_COUNTER, "outcome" => "failed").increment(1);
            error!(
                code = ErrorCode::WsGapConnectionState.code_str(),
                source = "xverify_failed",
                %err,
                "Dhan 1-minute cross-verification FAILED to run — today's candles are \
                 UNVERIFIED and today's S3 archive stays held"
            );
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
                source = "xverify_failed",
                ?err,
                discarded,
                "Dhan 1-minute cross-verification could NOT be persisted — today's \
                 comparison exists only in this log stream and today's S3 archive stays held"
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
            .and_then(|s| s.split("\nasync fn ").next())
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
}
