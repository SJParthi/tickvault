//! Daily post-close QuestDB partition archive — the SCHEDULER.
//!
//! # The defect this module exists to fix (2026-09-01)
//!
//! The daily archive→verify→drop sweep used to run INLINE inside the
//! `market_close` branch of `main.rs`'s single `tokio::select!`. Two things
//! followed from that shape, and both were silent:
//!
//! 1. `compute_market_close_sleep` returns `Duration::ZERO` once the wall
//!    clock passes 15:30 IST (`boot_helpers.rs`), and the select arm is
//!    guarded `if market_close_sleep > Duration::ZERO`. So an app **start or
//!    restart after 15:30 disabled the arm entirely** and no archive ran that
//!    day at all.
//! 2. A `SIGTERM` — which is exactly how the 17:30 IST box stop arrives —
//!    takes the *other* arm, so `shutdown_reason != "market_close"` and the
//!    sweep was skipped.
//!
//! Neither case reported anything. `tv_partition_archive_failed_total` stays
//! flat at zero when the pass never runs, which reads as healthy, so a day
//! with no archival was indistinguishable from a day with nothing to archive.
//!
//! The fix is a supervised polling loop that owns the sweep outright. The
//! inline block is DELETED rather than kept alongside, so there is exactly one
//! path and the two cannot drift.
//!
//! # Why the obvious version of this is wrong
//!
//! An adversarial review of the design before it was written found twelve
//! ways a naive scheduler reports success while archiving nothing. The ones
//! that shaped this code:
//!
//! - **`archive_and_drop_old_partitions` never returns `Err`.** It returns a
//!   summary, and a run where every table was WAL-suspended returns an
//!   all-zero summary that looks identical to a run with nothing to do.
//!   Latching the day on "the call returned" would seal a completely failed
//!   day as done. So the latch is driven by [`pass_verdict`], which reads the
//!   summary, and by an explicit [`PassOutcome`] for the cases where the
//!   archiver was never even constructed.
//! - **A budget-exhausted pass is not a finished pass.** The per-run cap is
//!   200 partitions; after a multi-day outage the backlog exceeds it, and a
//!   scheduler that latched on the first pass would leave the disk full until
//!   tomorrow with five unused attempts in hand.
//! - **The day must be captured BEFORE the pass, not after.** A sweep that
//!   starts at 23:58 and ends at 00:03 would otherwise latch *tomorrow*,
//!   opening tomorrow's window already sealed.
//! - **In-process state does not survive `Restart=always`.** The systemd unit
//!   restarts in 3 seconds, so a crash loop would re-run the full sweep every
//!   few seconds. The latch is therefore also written to the durable
//!   [`crate::daily_task_marker`], the idiom this repo already uses for
//!   exactly this.
//! - **A malformed `market_close_time` must refuse to schedule.** Reusing
//!   `compute_market_close_sleep`'s parse would silently place the window at
//!   00:00:02 and archive during the pre-open. This module parses once at
//!   spawn and returns `None` instead — the `thresholds_are_sane` precedent
//!   from `disk_pressure_boot`.
//!
//! # Concurrency with the disk-pressure leg
//!
//! Both legs call the same entry point, and they are now awake in the same
//! window. Mutual exclusion lives in `partition_archive` itself — at the
//! function, not at the call sites — so neither leg has to know about the
//! other and no future caller can forget. See `ARCHIVE_PASS_LOCK` there.

use std::time::Duration;

use chrono::{Datelike, NaiveDate, NaiveTime, Timelike};
use tickvault_common::config::{PartitionRetentionConfig, QuestDbConfig};
use tickvault_common::constants::MARKET_CLOSE_DRAIN_BUFFER_SECS;
use tickvault_common::error_code::ErrorCode;
use tickvault_common::trading_calendar::ist_offset;
use tickvault_storage::partition_archive::{ArchiveRunSummary, PartitionArchiver};
use tracing::{debug, error, info, warn};

/// How often the loop asks whether the sweep is due.
///
/// Five minutes, not one: the sweep is a daily housekeeping job with a
/// two-hour window (15:30 → 17:30 IST box stop), so a five-minute granularity
/// costs at most five minutes of latency on a job that had been missing
/// entire days. It is also comfortably longer than the disk-pressure loop's
/// 60-second poll, which keeps the two legs from contending on every tick.
pub const DAILY_ARCHIVE_POLL_INTERVAL_SECS: u64 = 300;

/// The poll cadence as a `Duration`, derived from the named constant above
/// so no bare literal appears at a call site.
const DAILY_ARCHIVE_POLL: Duration = Duration::from_secs(DAILY_ARCHIVE_POLL_INTERVAL_SECS);

/// Attempts allowed per IST day before the loop stops retrying.
///
/// Six, against a ~two-hour window at a five-minute poll. The cap exists so a
/// persistently broken QuestDB is not hammered for the whole evening; it is
/// deliberately larger than one so a single transient failure at 15:35 does
/// not cost the day's archival, which is what latching on first attempt would
/// have done.
pub const MAX_DAILY_ARCHIVE_ATTEMPTS: u32 = 6;

/// Durable marker task name — see [`crate::daily_task_marker`].
pub const DAILY_ARCHIVE_MARKER_TASK: &str = "daily_archive";

/// What the loop should do on one tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveTick {
    /// Run a sweep now.
    Run,
    /// The post-close window has not opened yet today.
    BeforeWindow,
    /// A sweep already COMPLETED today — nothing further is due.
    AlreadyCompleted,
    /// Today's attempt budget is spent. Deliberately distinct from
    /// `AlreadyCompleted`: this day did NOT finish, and reporting the two as
    /// one state is how a failed day would read as a successful one.
    AttemptsExhausted,
}

impl ArchiveTick {
    /// Stable label for counters and logs.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Run => "run",
            Self::BeforeWindow => "before_window",
            Self::AlreadyCompleted => "already_completed",
            Self::AttemptsExhausted => "attempts_exhausted",
        }
    }
}

/// Whether a completed pass actually finished the job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PassVerdict {
    /// Every table swept, every partition resolved, budget not exhausted.
    Complete,
    /// The pass ran but did not finish. Carries the reason so the retry is
    /// explainable rather than a bare "try again".
    Incomplete(&'static str),
}

/// Outcome of one attempt, including the cases where no pass ran at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PassOutcome {
    /// A pass ran; the verdict says whether it finished.
    Ran(PassVerdict),
    /// The archiver refused to construct (no bucket resolved). A deliberate
    /// fail-closed no-op — and emphatically NOT a completed day.
    Skipped,
    /// Construction errored. Also not a completed day.
    Failed,
}

impl PassOutcome {
    /// Stable label for the attempts counter.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ran(PassVerdict::Complete) => "complete",
            Self::Ran(PassVerdict::Incomplete(reason)) => reason,
            Self::Skipped => "skipped",
            Self::Failed => "failed",
        }
    }

    /// ONLY a complete pass may latch the day.
    #[must_use]
    pub const fn latches_the_day(self) -> bool {
        matches!(self, Self::Ran(PassVerdict::Complete))
    }
}

/// Reads a pass summary and decides whether it finished the job. Pure, O(1).
///
/// The three incomplete cases are each a real day-losing bug if treated as
/// success:
/// - `tables_list_failed` — a table contributed nothing to the worklist and
///   is indistinguishable from a table with no eligible partitions in every
///   other field of the summary.
/// - `failed` — at least one partition was kept for a reason that will recur
///   unless something changes.
/// - budget exhaustion — the worklist was truncated, so there is provably
///   more to do.
#[must_use]
pub const fn pass_verdict(summary: &ArchiveRunSummary, max_partitions_per_run: u32) -> PassVerdict {
    // FIRST, and it is the one that matters most. Two early returns inside
    // the pass hand back an all-zero summary — the concurrency-guard skip and
    // the WAL-probe fail-closed skip — and every count in a zero summary is
    // identical to a pass that ran and found nothing to do. Checking the
    // counts first would read "nothing happened" as "nothing needed to
    // happen" and latch the day on a sweep that never started.
    // A lock skip is reported FIRST and under its own name, because it is the
    // one "did not run" that is not this scheduler's problem: the other driver
    // is doing the work right now. The caller refunds the attempt on this
    // reason and only on this reason — see `AttemptLedger::refund`.
    if summary.contended {
        return PassVerdict::Incomplete("contended");
    }
    if !summary.pass_ran {
        return PassVerdict::Incomplete("did_not_run");
    }
    if summary.tables_wal_suspended > 0 {
        // The pass ran, but these tables were skipped whole and left no trace
        // in any other counter. Retrying is right: `RESUME WAL` may land
        // before the day is out.
        return PassVerdict::Incomplete("wal_suspended");
    }
    if summary.tables_list_failed > 0 {
        return PassVerdict::Incomplete("table_list_failed");
    }
    if summary.failed > 0 {
        return PassVerdict::Incomplete("partition_failed");
    }
    // `>=` and not `>`: hitting the cap exactly means the worklist was
    // truncated at it, which is the same "there is more" as exceeding it.
    if max_partitions_per_run > 0 && summary.partitions_considered >= max_partitions_per_run {
        return PassVerdict::Incomplete("budget_exhausted");
    }
    PassVerdict::Complete
}

/// Per-day attempt bookkeeping.
///
/// One value rather than three loose fields so `attempts` can never be read
/// against a day it was not counted for — the desync an adversarial review
/// flagged in the three-field shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AttemptLedger {
    day: Option<i32>,
    attempts: u32,
}

impl AttemptLedger {
    /// Attempts recorded for `day` — zero for any other day, which is what
    /// makes the rollover automatic instead of needing a reset call.
    #[must_use]
    pub const fn attempts_on(&self, day: i32) -> u32 {
        match self.day {
            Some(d) if d == day => self.attempts,
            _ => 0,
        }
    }

    /// Records one attempt on `day`, resetting the count on a day change.
    pub const fn record(&mut self, day: i32) {
        match self.day {
            Some(d) if d == day => self.attempts = self.attempts.saturating_add(1),
            _ => {
                self.day = Some(day);
                self.attempts = 1;
            }
        }
    }

    /// Gives back one attempt on `day` — used ONLY when the pass never
    /// started because another pass already held the lock.
    ///
    /// # Why a refund and not "record later"
    ///
    /// The attempt is recorded BEFORE the pass deliberately, so a panic or a
    /// cancellation still costs an attempt and the loop cannot retry forever.
    /// That is the right default and it is kept. But a lock skip is not a
    /// failed attempt — it means the OTHER driver (the disk-pressure leg) is
    /// doing this exact work right now, so charging the daily budget for it
    /// is charging for someone else's success.
    ///
    /// Found by adversarial review 2026-09-01, and it was a real defect: the
    /// pressure leg polls every 60s and a pass can export, upload, verify and
    /// drop up to `max_partitions_per_run` partitions, so six daily ticks
    /// landing inside one long pressure pass exhausted the whole day's budget.
    /// The consequences were both wrong and opposite: a page saying the
    /// archive did not complete on a box where it was completing, AND the
    /// daily sweep permanently abandoned for that day once the lock freed.
    ///
    /// Saturating at zero: a refund without a matching record is a no-op
    /// rather than an underflow, so this can never manufacture attempts.
    pub const fn refund(&mut self, day: i32) {
        if let Some(d) = self.day
            && d == day
        {
            self.attempts = self.attempts.saturating_sub(1);
        }
    }
}

/// The tick decision. Pure, O(1) — three integer comparisons, no allocation.
///
/// `completed_today` is checked FIRST, before the window, so a day already
/// finished stays reported as finished even when the clock has rolled past
/// midnight into a new day's pre-window hours.
#[must_use]
pub const fn archive_tick(
    now_secs_of_day: u32,
    window_open_secs_of_day: u32,
    completed_today: bool,
    attempts_today: u32,
    max_attempts: u32,
) -> ArchiveTick {
    if completed_today {
        return ArchiveTick::AlreadyCompleted;
    }
    if now_secs_of_day < window_open_secs_of_day {
        return ArchiveTick::BeforeWindow;
    }
    if attempts_today >= max_attempts {
        return ArchiveTick::AttemptsExhausted;
    }
    ArchiveTick::Run
}

/// Seconds-of-day at which the post-close window opens, or `None` when
/// `market_close_time` cannot be parsed.
///
/// `None` is a REFUSAL TO SCHEDULE, not a default. `compute_market_close_sleep`
/// answers a malformed value with `Duration::ZERO` plus a `warn!`, which is
/// right for "do not sleep" and catastrophic here: it would place the window
/// at 00:00:02 and run the sweep at the 08:30 boot, dropping partitions during
/// the pre-open.
#[must_use]
pub fn window_open_secs_of_day(market_close_time: &str) -> Option<u32> {
    let close = NaiveTime::parse_from_str(market_close_time, "%H:%M:%S").ok()?;
    let secs = close.num_seconds_from_midnight();
    // The drain buffer is the same settle time the retired inline block used,
    // so the sweep still starts after in-flight ticks land.
    let open = secs.checked_add(u32::try_from(MARKET_CLOSE_DRAIN_BUFFER_SECS).ok()?)?;
    // A close time within the drain buffer of midnight would wrap; refuse it
    // rather than schedule a window that opens yesterday.
    (open < 86_400).then_some(open)
}

/// Today's IST date and seconds-of-day, read once per tick.
fn now_ist() -> (NaiveDate, u32) {
    let now = chrono::Utc::now().with_timezone(&ist_offset());
    (now.date_naive(), now.time().num_seconds_from_midnight())
}

/// Runs ONE sweep and classifies it. Never panics; never returns `Err`.
async fn run_one_pass(
    questdb: &QuestDbConfig,
    cfg: &PartitionRetentionConfig,
) -> (PassOutcome, ArchiveRunSummary) {
    match PartitionArchiver::new(questdb, cfg).await {
        Ok(Some(mut archiver)) => {
            let summary = archiver.archive_and_drop_old_partitions().await;
            let verdict = pass_verdict(&summary, cfg.max_partitions_per_run);
            info!(
                tables_scanned = summary.tables_scanned,
                partitions_considered = summary.partitions_considered,
                verified = summary.verified,
                dropped = summary.dropped,
                failed = summary.failed,
                tables_list_failed = summary.tables_list_failed,
                rows_archived = summary.rows_archived,
                gzip_bytes_uploaded = summary.gzip_bytes_uploaded,
                csv_bytes_exported = summary.csv_bytes_exported,
                verdict = ?verdict,
                "daily partition archive pass complete (verified S3 copy before every drop)"
            );
            (PassOutcome::Ran(verdict), summary)
        }
        // No explicit archive bucket and no resolvable environment — archival
        // skipped rather than guessing the prod bucket. The constructor
        // already logged the actionable warn. Never latches the day.
        Ok(None) => (PassOutcome::Skipped, ArchiveRunSummary::default()),
        Err(err) => {
            error!(
                ?err,
                code = ErrorCode::StorageGap04S3ArchiveFailed.code_str(),
                "partition archiver construction failed — this attempt is a loud no-op \
                 and the day stays UNLATCHED so the next tick retries"
            );
            (PassOutcome::Failed, ArchiveRunSummary::default())
        }
    }
}

/// Spawns the supervised daily-archive loop, or `None` when it must not run.
///
/// Returning `None` is always a deliberate refusal with a log line naming the
/// reason — never a silent no-op.
pub fn spawn_supervised_daily_archive_loop(
    questdb: QuestDbConfig,
    cfg: PartitionRetentionConfig,
    market_close_time: String,
) -> Option<tokio::task::JoinHandle<()>> {
    if !cfg.archive_enabled {
        info!("daily partition archive disabled by config — no sweep will be scheduled");
        return None;
    }
    let Some(window_open) = window_open_secs_of_day(&market_close_time) else {
        // Refuse rather than default. A window silently placed at 00:00 would
        // archive during the pre-open, which is worse than not archiving.
        error!(
            code = ErrorCode::StorageGap04S3ArchiveFailed.code_str(),
            market_close_time = %market_close_time,
            "daily partition archive REFUSED to schedule: market_close_time is not \
             parseable as HH:MM:SS, and defaulting the window would place the sweep \
             at midnight — fix the config to enable it"
        );
        return None;
    };

    info!(
        window_open_secs_of_day = window_open,
        poll_secs = DAILY_ARCHIVE_POLL.as_secs(),
        max_attempts = MAX_DAILY_ARCHIVE_ATTEMPTS,
        max_partitions_per_run = cfg.max_partitions_per_run,
        "daily partition archive ARMED (polling scheduler — survives a restart after \
         market close, which the retired inline block did not)"
    );

    Some(tokio::spawn(async move {
        loop {
            let q = questdb.clone();
            let c = cfg.clone();
            let handle =
                tokio::spawn(async move { run_daily_archive_loop(q, c, window_open).await });
            match handle.await {
                Ok(()) => {
                    info!("daily archive loop returned cleanly — not respawning");
                    return;
                }
                Err(err) => {
                    error!(
                        ?err,
                        code = ErrorCode::StorageGap04S3ArchiveFailed.code_str(),
                        "daily archive loop PANICKED — respawning so the sweep is not \
                         silently lost for the rest of the process lifetime"
                    );
                    metrics::counter!("tv_daily_archive_respawn_total").increment(1);
                    tokio::time::sleep(DAILY_ARCHIVE_POLL).await;
                }
            }
        }
    }))
}

/// The loop body. The pass is awaited INLINE — never spawned — so a sweep that
/// outlives the poll interval cannot have a second sweep started on top of it.
async fn run_daily_archive_loop(
    questdb: QuestDbConfig,
    cfg: PartitionRetentionConfig,
    window_open: u32,
) {
    let mut ledger = AttemptLedger::default();
    let mut ticker = tokio::time::interval(DAILY_ARCHIVE_POLL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    // Pre-register both series at zero so the CloudWatch agent's counter-delta
    // baseline is the harmless zero rather than the first real event. The
    // repo has been bitten by an absent series reading as a healthy one.
    for outcome in [
        "complete",
        "table_list_failed",
        "partition_failed",
        "budget_exhausted",
        "skipped",
        "failed",
    ] {
        metrics::counter!("tv_daily_archive_attempts_total", "outcome" => outcome).increment(0);
    }
    let completed_gauge = metrics::gauge!("tv_daily_archive_completed_today");

    loop {
        ticker.tick().await;

        // Read the day ONCE, before anything else, and use this captured value
        // for the whole tick. A sweep that crosses midnight must latch the day
        // it STARTED, or tomorrow opens already sealed.
        let (today_date, now_secs) = now_ist();
        let today = today_date.num_days_from_ce();

        let completed_today =
            crate::daily_task_marker::daily_marker_exists(DAILY_ARCHIVE_MARKER_TASK, today_date);
        completed_gauge.set(if completed_today { 1.0 } else { 0.0 });

        let decision = archive_tick(
            now_secs,
            window_open,
            completed_today,
            ledger.attempts_on(today),
            MAX_DAILY_ARCHIVE_ATTEMPTS,
        );

        match decision {
            ArchiveTick::BeforeWindow | ArchiveTick::AlreadyCompleted => {
                debug!(
                    decision = decision.as_str(),
                    "daily archive tick: nothing due"
                );
                continue;
            }
            ArchiveTick::AttemptsExhausted => {
                // Loud ONCE per day, not per tick: the ledger only crosses the
                // cap on one tick, and every tick after it re-enters this arm.
                // Logging each time would be ~24 identical lines an evening.
                if ledger.attempts_on(today) == MAX_DAILY_ARCHIVE_ATTEMPTS {
                    ledger.record(today); // push past the cap so this fires once
                    error!(
                        code = ErrorCode::StorageGap04S3ArchiveFailed.code_str(),
                        attempts = MAX_DAILY_ARCHIVE_ATTEMPTS,
                        "daily partition archive did NOT complete today after \
                         {MAX_DAILY_ARCHIVE_ATTEMPTS} attempts — disk will not be \
                         reclaimed until the next session unless this is fixed"
                    );
                }
                continue;
            }
            ArchiveTick::Run => {}
        }

        ledger.record(today);
        let attempt = ledger.attempts_on(today);
        info!(
            attempt,
            max_attempts = MAX_DAILY_ARCHIVE_ATTEMPTS,
            "daily partition archive sweep starting"
        );

        let (outcome, _summary) = run_one_pass(&questdb, &cfg).await;
        metrics::counter!("tv_daily_archive_attempts_total", "outcome" => outcome.as_str())
            .increment(1);

        // REFUND before anything else reads the ledger. A contended pass never
        // started — the disk-pressure leg holds the lock and is doing exactly
        // this work — so charging the day's six attempts for it would exhaust
        // the budget on someone else's success and then abandon the daily
        // sweep once the lock freed. Found by adversarial review 2026-09-01.
        if matches!(
            outcome,
            PassOutcome::Ran(PassVerdict::Incomplete("contended"))
        ) {
            ledger.refund(today);
            debug!(
                attempt,
                "daily archive tick stood aside — the disk-pressure leg holds the \
                 archive lock and is doing this work; attempt refunded, not spent"
            );
            continue;
        }

        if outcome.latches_the_day() {
            // Durable, so a crash-restart three seconds later does not re-run
            // a sweep that already finished. `write_daily_marker` is fail-open
            // by design: an unwritable marker costs a redundant sweep, never a
            // skipped one.
            crate::daily_task_marker::write_daily_marker(DAILY_ARCHIVE_MARKER_TASK, today_date);
            completed_gauge.set(1.0);
            info!(
                attempt,
                "daily partition archive COMPLETE for today — latched (durable marker \
                 written; a restart will not repeat it)"
            );
        } else {
            warn!(
                attempt,
                outcome = outcome.as_str(),
                remaining = MAX_DAILY_ARCHIVE_ATTEMPTS.saturating_sub(attempt),
                "daily partition archive attempt did not finish the job — the day stays \
                 UNLATCHED and the next tick retries"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A summary for a pass that GENUINELY RAN.
    ///
    /// `pass_ran: true` is not incidental here: every verdict these tests
    /// exercise is downstream of the pass having started, and `pass_verdict`
    /// short-circuits on `!pass_ran` before it reads any count. A helper that
    /// left it `false` would make every test below pass for the wrong reason —
    /// which is exactly what happened when the field was introduced.
    fn summary(considered: u32, failed: u32, list_failed: u32) -> ArchiveRunSummary {
        ArchiveRunSummary {
            pass_ran: true,
            partitions_considered: considered,
            failed,
            tables_list_failed: list_failed,
            ..ArchiveRunSummary::default()
        }
    }

    // ---- the tick decision -------------------------------------------------

    #[test]
    fn before_the_window_nothing_is_due() {
        assert_eq!(
            archive_tick(50_000, 55_802, false, 0, MAX_DAILY_ARCHIVE_ATTEMPTS),
            ArchiveTick::BeforeWindow
        );
    }

    #[test]
    fn archive_tick_runs_inside_the_window_with_budget() {
        assert_eq!(
            archive_tick(56_000, 55_802, false, 0, MAX_DAILY_ARCHIVE_ATTEMPTS),
            ArchiveTick::Run
        );
    }

    /// The whole point of the fix: a process that STARTS after the close must
    /// still sweep. The retired inline block could not, because its select arm
    /// was disabled once `compute_market_close_sleep` returned zero.
    #[test]
    fn a_process_started_after_market_close_still_sweeps_today() {
        // 16:45 IST — well past the 15:30 close, inside the 17:30 box stop.
        let sixteen_forty_five = 16 * 3600 + 45 * 60;
        assert_eq!(
            archive_tick(
                sixteen_forty_five,
                55_802,
                false,
                0,
                MAX_DAILY_ARCHIVE_ATTEMPTS
            ),
            ArchiveTick::Run,
            "a restart after the close is exactly the case the inline block \
             could not handle — it must run here"
        );
    }

    /// Completion is checked BEFORE the window so a finished day stays
    /// finished after midnight rolls the clock back under the window.
    #[test]
    fn a_completed_day_reads_completed_even_before_the_window() {
        assert_eq!(
            archive_tick(1, 55_802, true, 0, MAX_DAILY_ARCHIVE_ATTEMPTS),
            ArchiveTick::AlreadyCompleted
        );
    }

    /// Exhaustion must NOT be reported as completion — a day that failed six
    /// times did not archive, and collapsing the two states is how a failed
    /// day would read as a successful one.
    #[test]
    fn exhausted_attempts_are_distinct_from_completion() {
        let decision = archive_tick(
            56_000,
            55_802,
            false,
            MAX_DAILY_ARCHIVE_ATTEMPTS,
            MAX_DAILY_ARCHIVE_ATTEMPTS,
        );
        assert_eq!(decision, ArchiveTick::AttemptsExhausted);
        assert_ne!(
            decision,
            ArchiveTick::AlreadyCompleted,
            "a day that ran out of attempts must never be indistinguishable \
             from a day that finished"
        );
    }

    // ---- the pass verdict --------------------------------------------------

    #[test]
    fn pass_verdict_reports_complete_for_a_clean_pass() {
        assert_eq!(pass_verdict(&summary(10, 0, 0), 200), PassVerdict::Complete);
    }

    /// The CRITICAL one. `archive_and_drop_old_partitions` never returns
    /// `Err`, and a run where every table was WAL-suspended returns an
    /// all-zero summary that is byte-identical to a run with nothing to do.
    /// Latching on "the call returned" would seal a totally failed day.
    #[test]
    fn a_table_that_could_not_be_listed_is_not_a_complete_day() {
        assert_eq!(
            pass_verdict(&summary(0, 0, 1), 200),
            PassVerdict::Incomplete("table_list_failed")
        );
    }

    #[test]
    fn a_failed_partition_is_not_a_complete_day() {
        assert_eq!(
            pass_verdict(&summary(10, 1, 0), 200),
            PassVerdict::Incomplete("partition_failed")
        );
    }

    /// After a multi-day outage the backlog exceeds the per-run cap. A pass
    /// that cleared exactly the cap provably has more to do, so latching it
    /// would strand the remaining backlog until tomorrow with unused attempts
    /// still in hand.
    #[test]
    fn a_budget_exhausted_pass_is_not_a_complete_day() {
        assert_eq!(
            pass_verdict(&summary(200, 0, 0), 200),
            PassVerdict::Incomplete("budget_exhausted")
        );
    }

    /// A zero cap means "no cap configured" and must not make every pass read
    /// as budget-exhausted — that would latch nothing, ever.
    #[test]
    fn a_zero_cap_does_not_make_every_pass_look_exhausted() {
        assert_eq!(
            pass_verdict(&summary(5_000, 0, 0), 0),
            PassVerdict::Complete
        );
    }

    // ---- what may latch the day -------------------------------------------

    #[test]
    fn latches_the_day_is_true_only_for_a_complete_pass() {
        assert!(PassOutcome::Ran(PassVerdict::Complete).latches_the_day());
        for non_latching in [
            PassOutcome::Ran(PassVerdict::Incomplete("partition_failed")),
            PassOutcome::Ran(PassVerdict::Incomplete("table_list_failed")),
            PassOutcome::Ran(PassVerdict::Incomplete("budget_exhausted")),
            PassOutcome::Skipped,
            PassOutcome::Failed,
        ] {
            assert!(
                !non_latching.latches_the_day(),
                "{non_latching:?} must NOT seal the day — every one of these \
                 leaves real work undone, and latching would skip it until \
                 tomorrow with no signal that anything was missed"
            );
        }
    }

    #[test]
    fn every_outcome_has_a_distinct_stable_label() {
        let labels = [
            PassOutcome::Ran(PassVerdict::Complete).as_str(),
            PassOutcome::Ran(PassVerdict::Incomplete("table_list_failed")).as_str(),
            PassOutcome::Ran(PassVerdict::Incomplete("partition_failed")).as_str(),
            PassOutcome::Ran(PassVerdict::Incomplete("budget_exhausted")).as_str(),
            PassOutcome::Skipped.as_str(),
            PassOutcome::Failed.as_str(),
        ];
        let mut seen: Vec<&str> = labels.to_vec();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(
            seen.len(),
            labels.len(),
            "each outcome needs its own counter label or the dashboard cannot \
             tell a skipped day from a failed one"
        );
    }

    // ---- the attempt ledger ------------------------------------------------

    #[test]
    fn attempts_on_counts_per_day_and_resets_on_rollover() {
        let mut ledger = AttemptLedger::default();
        assert_eq!(ledger.attempts_on(100), 0);
        ledger.record(100);
        ledger.record(100);
        assert_eq!(ledger.attempts_on(100), 2);
        // A different day reads zero WITHOUT an explicit reset call — which is
        // what makes the midnight rollover automatic instead of a thing that
        // can be forgotten.
        assert_eq!(ledger.attempts_on(101), 0);
        ledger.record(101);
        assert_eq!(ledger.attempts_on(101), 1);
        assert_eq!(
            ledger.attempts_on(100),
            0,
            "yesterday's count must not survive the rollover"
        );
    }

    /// The HIGH defect found by adversarial review 2026-09-01: a pass that
    /// never started because the disk-pressure leg held the lock was charging
    /// the day's attempt budget. Six overlaps exhausted it, paged that the
    /// archive had not completed on a box where it WAS completing, and then
    /// abandoned the daily sweep for the rest of that day.
    #[test]
    fn refund_gives_back_only_the_contended_attempt() {
        let mut ledger = AttemptLedger::default();
        ledger.record(100);
        ledger.record(100);
        assert_eq!(ledger.attempts_on(100), 2);

        ledger.refund(100);
        assert_eq!(
            ledger.attempts_on(100),
            1,
            "a contended tick must cost nothing — the other driver is doing \
             this work right now"
        );

        // Saturating: a refund without a matching record can never manufacture
        // attempts, and can never underflow.
        ledger.refund(100);
        ledger.refund(100);
        ledger.refund(100);
        assert_eq!(ledger.attempts_on(100), 0);

        // A refund aimed at a different day must not touch today's count —
        // otherwise a midnight rollover mid-pass would silently hand back an
        // attempt that belonged to the previous day.
        ledger.record(101);
        ledger.refund(100);
        assert_eq!(
            ledger.attempts_on(101),
            1,
            "a refund for another day must be a no-op"
        );
    }

    /// The two `did not run` causes mean OPPOSITE things and were reported
    /// identically until 2026-09-01: a WAL-probe fail-closed means NOBODY is
    /// archiving, a lock skip means someone else IS.
    #[test]
    fn pass_verdict_tells_a_lock_skip_apart_from_a_pass_that_never_ran() {
        let contended = ArchiveRunSummary {
            contended: true,
            ..ArchiveRunSummary::default()
        };
        assert!(
            matches!(
                pass_verdict(&contended, 200),
                PassVerdict::Incomplete("contended")
            ),
            "a lock skip must be named, so the caller can refund it"
        );

        let never_ran = ArchiveRunSummary::default();
        assert!(
            matches!(
                pass_verdict(&never_ran, 200),
                PassVerdict::Incomplete("did_not_run")
            ),
            "a fail-closed skip must stay `did_not_run` — it is a real failed \
             attempt and must keep costing one"
        );
    }

    // ---- the window parse --------------------------------------------------

    #[test]
    fn window_open_secs_of_day_opens_one_drain_buffer_after_the_close() {
        assert_eq!(
            window_open_secs_of_day("15:30:00"),
            Some(15 * 3600 + 30 * 60 + MARKET_CLOSE_DRAIN_BUFFER_SECS as u32)
        );
    }

    /// A malformed close time must REFUSE to schedule, never default. The
    /// sibling `compute_market_close_sleep` answers a bad parse with
    /// `Duration::ZERO`, which is right for "do not sleep" and catastrophic
    /// here: it would place the window at 00:00:02 and drop partitions during
    /// the pre-open.
    #[test]
    fn a_malformed_close_time_refuses_to_schedule() {
        for bad in ["", "not-a-time", "15:30", "25:00:00", "15:70:00"] {
            assert_eq!(
                window_open_secs_of_day(bad),
                None,
                "{bad:?} must refuse rather than default the window to midnight"
            );
        }
    }

    /// A close time within the drain buffer of midnight would wrap to a window
    /// that opens "yesterday".
    #[test]
    fn a_close_time_at_the_edge_of_midnight_refuses_rather_than_wrapping() {
        assert_eq!(window_open_secs_of_day("23:59:59"), None);
    }

    #[test]
    fn the_tick_labels_are_stable_and_distinct() {
        let labels = [
            ArchiveTick::Run.as_str(),
            ArchiveTick::BeforeWindow.as_str(),
            ArchiveTick::AlreadyCompleted.as_str(),
            ArchiveTick::AttemptsExhausted.as_str(),
        ];
        let mut seen: Vec<&str> = labels.to_vec();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), labels.len());
    }

    /// The poll cadence must leave room for the attempt budget inside the
    /// window the box is actually alive for (15:30 close → 17:30 stop).
    #[test]
    fn the_attempt_budget_fits_inside_the_evening_window() {
        let window_secs = 2 * 3600_u64; // 15:30 → 17:30 IST
        let needed = DAILY_ARCHIVE_POLL.as_secs() * u64::from(MAX_DAILY_ARCHIVE_ATTEMPTS);
        assert!(
            needed <= window_secs,
            "{MAX_DAILY_ARCHIVE_ATTEMPTS} attempts at {}s apart needs {needed}s, \
             which does not fit the ~{window_secs}s the box is alive after the \
             close — the later attempts would never be reached",
            DAILY_ARCHIVE_POLL.as_secs()
        );
    }
}

#[cfg(test)]
mod pass_ran_tests {
    use super::*;

    /// The bug an adversarial verification pass caught in the FIRST draft of
    /// this module, and the most important test in the file.
    ///
    /// `archive_and_drop_old_partitions` has two early returns that hand back
    /// an all-zero `ArchiveRunSummary`: the concurrency-guard skip and the
    /// WAL-probe fail-closed skip. Every COUNT in a zero summary is identical
    /// to a pass that ran and found nothing to do — so the first draft read
    /// "nothing happened" as "nothing needed to happen", returned `Complete`,
    /// and wrote the durable day-latch. A day on which no sweep ever started
    /// would have been sealed as finished, and the restart that would
    /// otherwise have retried it would skip it too.
    ///
    /// That is precisely the false-OK class this whole module exists to close,
    /// reintroduced by the fix for it.
    #[test]
    fn a_pass_that_never_ran_is_never_complete() {
        let never_ran = ArchiveRunSummary::default();
        assert!(
            !never_ran.pass_ran,
            "the default summary must report that it did not run — both early \
             returns rely on that being the default rather than on remembering \
             to set it"
        );
        assert_eq!(
            pass_verdict(&never_ran, 200),
            PassVerdict::Incomplete("did_not_run"),
            "an all-zero summary must NEVER read as a complete day"
        );
        assert!(
            !PassOutcome::Ran(pass_verdict(&never_ran, 200)).latches_the_day(),
            "and it must never latch the durable marker"
        );
    }

    /// The same zero counts, but the pass genuinely ran: that IS a complete
    /// day, and must still be latched. Without this the fix above would swing
    /// too far and never latch anything.
    #[test]
    fn a_pass_that_ran_and_found_nothing_is_a_complete_day() {
        let ran_clean = ArchiveRunSummary {
            pass_ran: true,
            ..ArchiveRunSummary::default()
        };
        assert_eq!(pass_verdict(&ran_clean, 200), PassVerdict::Complete);
        assert!(PassOutcome::Ran(pass_verdict(&ran_clean, 200)).latches_the_day());
    }

    /// A WAL-suspended table leaves NO trace in any other counter — it is
    /// skipped before `tables_scanned` or `tables_list_failed` are touched —
    /// so without its own field a run that skipped every table for suspension
    /// is byte-identical to a clean run.
    #[test]
    fn wal_suspended_tables_prevent_a_complete_day() {
        let suspended = ArchiveRunSummary {
            pass_ran: true,
            tables_wal_suspended: 1,
            ..ArchiveRunSummary::default()
        };
        assert_eq!(
            pass_verdict(&suspended, 200),
            PassVerdict::Incomplete("wal_suspended"),
            "a suspended table archived nothing; retrying is right because \
             RESUME WAL may land before the day is out"
        );
    }

    /// Every incomplete reason must be its own counter label, or the operator
    /// cannot tell "the guard skipped it" from "a partition failed".
    #[test]
    fn did_not_run_and_wal_suspended_have_distinct_labels() {
        let labels = [
            PassOutcome::Ran(PassVerdict::Incomplete("did_not_run")).as_str(),
            PassOutcome::Ran(PassVerdict::Incomplete("wal_suspended")).as_str(),
            PassOutcome::Ran(PassVerdict::Incomplete("partition_failed")).as_str(),
            PassOutcome::Ran(PassVerdict::Complete).as_str(),
        ];
        let mut seen: Vec<&str> = labels.to_vec();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), labels.len());
    }
}

#[cfg(test)]
mod spawn_refusal_tests {
    use super::*;

    fn test_questdb() -> QuestDbConfig {
        QuestDbConfig {
            host: "127.0.0.1".to_string(),
            http_port: 9000,
            pg_port: 8812,
            ilp_port: 9009,
        }
    }

    fn enabled_cfg() -> PartitionRetentionConfig {
        PartitionRetentionConfig {
            archive_enabled: true,
            ..PartitionRetentionConfig::default()
        }
    }

    /// A disabled build must not even spawn the task — the sibling
    /// `spawn_supervised_disk_pressure_loop_returns_none_when_disabled`
    /// precedent. These refusal paths are the ones worth pinning: they decide
    /// whether anything runs at all.
    #[test]
    fn a_disabled_archive_config_spawns_nothing() {
        let cfg = PartitionRetentionConfig {
            archive_enabled: false,
            ..PartitionRetentionConfig::default()
        };
        assert!(
            spawn_supervised_daily_archive_loop(test_questdb(), cfg, "15:30:00".to_string())
                .is_none(),
            "archive_enabled = false must spawn no task at all"
        );
    }

    /// The refusal that matters most. `compute_market_close_sleep` answers a
    /// malformed close time with `Duration::ZERO` plus a warn, which is right
    /// for "do not sleep" and catastrophic here — it would place the window at
    /// 00:00:02 and drop partitions during the pre-open. Refusing to schedule
    /// is the only safe answer.
    #[test]
    fn spawn_supervised_daily_archive_loop_refuses_a_malformed_close_time() {
        for bad in ["", "half past three", "15:30", "99:99:99"] {
            assert!(
                spawn_supervised_daily_archive_loop(test_questdb(), enabled_cfg(), bad.to_string())
                    .is_none(),
                "close time {bad:?} must REFUSE to schedule rather than \
                 defaulting the window to midnight"
            );
        }
    }

    /// And the positive case, so the two refusals above cannot pass by the
    /// function simply always returning `None`.
    #[tokio::test]
    async fn a_valid_config_does_spawn_the_loop() {
        let handle = spawn_supervised_daily_archive_loop(
            test_questdb(),
            enabled_cfg(),
            "15:30:00".to_string(),
        );
        assert!(
            handle.is_some(),
            "an enabled config with a parseable close time must spawn the loop \
             — without this the refusal tests above would pass vacuously"
        );
        // Nothing is asserted about the loop's behaviour here; it polls on a
        // five-minute interval and its decisions are unit-tested above as pure
        // functions. Abort so the test process does not leave it running.
        if let Some(h) = handle {
            h.abort();
        }
    }
}
