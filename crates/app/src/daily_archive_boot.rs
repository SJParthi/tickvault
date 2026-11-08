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
//!
//! # Until done, not until 15:47 (PR AA step 5, owner Quote 51, 2026-11-07)
//!
//! Measured 3–7 Nov: the leg latched the day at about 15:47, while later
//! hours only became old enough afterwards, and the box stopped with the
//! disk still filling. Three changes:
//!
//! - **The window runs to 07:30 IST.** [`archive_session_day`] reads a clock
//!   before 07:30 as the previous session day, so the loop no longer stops at
//!   midnight; 07:30 is the drain hold's cap.
//! - **The day latches only when it is done.** [`daily_latch_decision`]:
//!   a clean pass that dropped nothing AND free space at or above the target
//!   (`archive_free_target`, `max(1.5 × p95 session bytes, 30% of disk)`).
//!   A pass that dropped something, or a clean pass below the target, gives
//!   its attempt back, so the six-attempt cap counts only passes that failed
//!   without progress.
//! - **Below the target the pass uses the hour path** ([`daily_pass_config`]):
//!   the Quote 51 tables are archived by the hour, each hour more than four
//!   hours past its end, with every gate of the snapshot path. Day windows of
//!   every other table are unchanged.
//!
//! The session size is read once a day after 15:40, before this process
//! drops any partition of that day ([`should_measure_session`]).

use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::{Datelike, NaiveDate, NaiveTime, Timelike};
use tickvault_common::config::{PartitionRetentionConfig, QuestDbConfig};
use tickvault_common::constants::{
    MARKET_CLOSE_DRAIN_BUFFER_SECS, TICK_PERSIST_END_SECS_OF_DAY_IST,
};
use tickvault_common::error_code::ErrorCode;
use tickvault_common::trading_calendar::ist_offset;
use tickvault_storage::disk_health_watcher::{DiskHealthOutcome, probe_disk_free_bytes};
use tickvault_storage::partition_archive::{
    ArchivePassLimits, ArchiveRunSummary, PartitionArchiver, VerifiedDayGate,
};
use tracing::{debug, error, info, warn};

use crate::archive_free_target::{
    SessionBytesRing, free_target_bytes, load_session_bytes, save_session_bytes, session_bytes_path,
};

/// How often the loop asks whether the sweep is due.
///
/// Five minutes, not one: the sweep is a daily housekeeping job with a
/// one-hour window (15:30 → 16:30 IST box stop since 2026-10-22, plan item
/// 64; two hours to 17:30 before), so a five-minute granularity
/// costs at most five minutes of latency on a job that had been missing
/// entire days. It is also comfortably longer than the disk-pressure loop's
/// 60-second poll, which keeps the two legs from contending on every tick.
pub const DAILY_ARCHIVE_POLL_INTERVAL_SECS: u64 = 300;

/// The poll cadence as a `Duration`, derived from the named constant above
/// so no bare literal appears at a call site.
const DAILY_ARCHIVE_POLL: Duration = Duration::from_secs(DAILY_ARCHIVE_POLL_INTERVAL_SECS);

/// Attempts allowed per IST day before the loop stops retrying.
///
/// Six attempts that fail WITHOUT progress (PR AA review: since step 5 the
/// window runs from the close to 07:30 IST, and a pass that dropped
/// something, was stood off by another job or only waited on QuestDB's WAL
/// is given back, so only a real fault spends one). The cap exists so a
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
///   A table QuestDB reports as NOT EXISTING is counted in `tables_absent`
///   instead and never blocks the latch: there is nothing to move. Until
///   2026-09-23 the 19 retired tables in the sweep list were counted here,
///   and the day never latched on 6 of 6 attempts, every day.
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
    // PR AA review: the two real faults are named BEFORE a lagging WAL, so
    // a pass that both waited on QuestDB and failed a partition spends its
    // attempt (`wal_not_applied` alone is given back, `refunds_attempt`).
    if summary.tables_list_failed > 0 {
        return PassVerdict::Incomplete("table_list_failed");
    }
    if summary.failed > 0 {
        return PassVerdict::Incomplete("partition_failed");
    }
    if summary.tables_wal_not_applied > 0 {
        // A table's WAL could not be proven fully applied at the
        // per-partition gate (most often: it was merely behind), so its
        // partitions were kept. A later attempt the same day usually finds
        // it caught up.
        return PassVerdict::Incomplete("wal_not_applied");
    }
    // `>=` and not `>`: hitting the cap exactly means the worklist was
    // truncated at it, which is the same "there is more" as exceeding it.
    // DA-13d re-review: held depth partitions take worklist slots but no
    // work, so they do not count toward the budget. Otherwise a backlog of
    // 200 or more read as budget_exhausted, was not refunded, and the hold
    // still used up the day's attempts.
    if max_partitions_per_run > 0
        && summary
            .partitions_considered
            .saturating_sub(summary.held_deferred_depth)
            .saturating_sub(summary.held_quiesce_busy)
            .saturating_sub(summary.held_in_flight)
            >= max_partitions_per_run
    {
        return PassVerdict::Incomplete("budget_exhausted");
    }
    if summary.held_deferred_depth > 0 {
        // DA-13d: depth partitions were held while the after-close pass
        // wrote shed rows back into them. Checked LAST, so it is the verdict
        // only when nothing else went wrong: the caller refunds the attempt on
        // this reason ([`refunds_attempt`]) and polls again, and a real
        // failure in the same pass must still spend one. The pass holds the
        // gate until at most ~17:16:05 IST (its 17:15 stop plus the settle),
        // so the first poll after that archives the depth partitions with the
        // rewritten rows in. If the 17:30 box stop comes first, they are
        // still past retention at the next day's sweep and go then.
        return PassVerdict::Incomplete("deferred_depth_pass");
    }
    if summary.held_quiesce_busy > 0 {
        // PR AA step 2: hour partitions were held because a WAL replay, a
        // spill replay or the after-close depth pass had the archive quiesce
        // lock. Another job is doing work right now, exactly like
        // `contended`: the caller refunds the attempt and polls again.
        return PassVerdict::Incomplete("quiesce_busy");
    }
    // `held_in_flight` (PR AA step 2) does NOT keep the day open: an hour
    // whose rows may still be on their way to QuestDB (most often the last
    // depth hour, not acknowledged until the next session's first ack) is a
    // partition not yet old enough, like any other; a later pass archives it.
    // The `top_volume_spill_pending` verdict (2026-10-13) went with the
    // top_volume spill and its archive hold on 2026-10-22 (plan item 62).
    PassVerdict::Complete
}

/// Whether an attempt with this outcome is given back rather than spent.
///
/// Only the "another job is doing this right now" verdicts: `contended`
/// (the disk-pressure leg holds the archive lock), `deferred_depth_pass`
/// (DA-13d: the after-close depth pass still has rows of a depth hour to
/// write back) and `quiesce_busy` (PR AA step 2: a WAL replay, a spill
/// replay or the after-close depth pass had the archive quiesce lock).
/// Spending an attempt on any of them would exhaust the day's six attempts
/// and page STORAGE-GAP-04 on a box that is working.
///
/// PR AA review: also `wal_not_applied`, a pass that only waited on a WAL
/// QuestDB had not applied yet (no listing failure, no failed partition:
/// [`pass_verdict`] names those first). Since the window runs to 07:30 IST,
/// six such passes (about an hour of evening apply lag at a 300 s snapshot
/// wait each) used to end the leg for the whole night, and the hours that
/// became old enough afterwards were never archived. The 07:30 window end
/// bounds those passes now; apply lag has its own watchers. Pure, O(1).
#[must_use]
pub fn refunds_attempt(outcome: PassOutcome) -> bool {
    matches!(
        outcome,
        PassOutcome::Ran(PassVerdict::Incomplete(
            "contended" | "deferred_depth_pass" | "quiesce_busy" | "wal_not_applied"
        ))
    )
}

/// Every `tv_daily_archive_attempts_total{outcome}` value, seeded at 0 when
/// the loop starts so a first event never lands on a missing series.
pub const SEEDED_ATTEMPT_OUTCOMES: [&str; 12] = [
    "complete",
    "contended",
    "did_not_run",
    "wal_suspended",
    "wal_not_applied",
    "table_list_failed",
    "partition_failed",
    "budget_exhausted",
    "deferred_depth_pass",
    "quiesce_busy",
    "skipped",
    "failed",
];

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

/// The seconds-of-day (IST) at which a session day's archive window ends:
/// 07:30, the drain hold's cap (owner Quote 31; Quote 51 "at most until
/// 7:30 AM").
pub const ARCHIVE_WINDOW_END_SECS_IST: u32 = crate::drain_hold::DRAIN_HOLD_CEILING_SECS_IST as u32;
const _: () = assert!(crate::drain_hold::DRAIN_HOLD_CEILING_SECS_IST <= u32::MAX as u64);

/// Seconds in a day, for the after-midnight half of a window.
const SECS_PER_DAY: u32 = 86_400;

/// Which session day's archive window `now` belongs to, and how far into
/// that day it is (PR AA step 5, owner Quote 51 "keeps running after the
/// close until free space reaches its target").
///
/// Before 07:30 IST the clock still belongs to the PREVIOUS day's window, and
/// its seconds run past 86,400 (00:05 reads as 86,700), so [`archive_tick`]
/// keeps answering `Run` after midnight instead of stopping at it. From
/// 07:30 the session day is today and the window has not opened yet. If the
/// previous day does not exist (the first date chrono knows), the clock is
/// read as today. Pure, O(1).
#[must_use]
// TEST-EXEMPT: covered by test_loop_continues_past_midnight_until_0730
pub fn archive_session_day(date_ist: NaiveDate, secs_of_day_ist: u32) -> (NaiveDate, u32) {
    if secs_of_day_ist >= ARCHIVE_WINDOW_END_SECS_IST {
        return (date_ist, secs_of_day_ist);
    }
    match date_ist.pred_opt() {
        Some(previous) => (previous, secs_of_day_ist.saturating_add(SECS_PER_DAY)),
        None => (date_ist, secs_of_day_ist),
    }
}

/// The instant (Unix seconds) a daily pass of `session_day` must stop at:
/// 07:30 IST the next calendar day, the end of its window (PR AA review).
/// Without it a pass started at 07:29 ran its whole worklist (up to 200
/// partitions, each holding the quiesce lock and a QuestDB export) into the
/// 08:30 boot and the 09:00 session; the pressure leg already stops at its
/// gate for the same reason. The pass stops at the next partition boundary
/// once this passes. `None` only past chrono's last date. Pure, O(1).
#[must_use]
// TEST-EXEMPT: covered by test_daily_pass_stops_at_the_window_end
pub fn daily_pass_stop_unix_secs(session_day: NaiveDate) -> Option<i64> {
    let end_day = session_day.succ_opt()?;
    let end = end_day.and_hms_opt(0, 0, 0)?
        + chrono::TimeDelta::seconds(i64::from(ARCHIVE_WINDOW_END_SECS_IST));
    Some(end.and_utc().timestamp() - i64::from(tickvault_common::constants::IST_UTC_OFFSET_SECONDS))
}

/// What one pass means for the day (PR AA step 5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LatchDecision {
    /// The day is done: latch it.
    Latch,
    /// The day stays open. `refund` gives the attempt back, so the attempt
    /// cap counts only passes that made no progress and found a real fault.
    KeepOpen {
        /// Stable label for the log line.
        reason: &'static str,
        /// Whether the attempt is given back.
        refund: bool,
    },
}

/// Decides whether a pass closes the day (PR AA step 5, owner Quote 51:
/// "does not mark the day done while partitions old enough to drop remain,
/// and keeps running after the close until free space reaches its target").
///
/// Latches ONLY when the pass was clean ([`PassVerdict::Complete`]), it
/// dropped nothing (so nothing it could act on was left), and free space
/// is known and at or above the target. Otherwise the day stays open:
/// - an incomplete pass keeps its own reason; its attempt is given back when
///   another job stood in its way ([`refunds_attempt`]) or when it dropped
///   at least one partition (it made progress);
/// - a clean pass that dropped something is given back and polls again,
///   since more may have become old enough while it ran;
/// - a clean pass with free space below the target, or unknown, is given
///   back too: nothing is broken, the leg just keeps polling until the
///   window ends at 07:30 IST.
///
/// So the attempt cap still bounds a pass that keeps failing without
/// progress, and the 07:30 window end bounds the rest. Pure, O(1).
#[must_use]
// TEST-EXEMPT: covered by test_no_latch_while_eligible_or_below_target
pub fn daily_latch_decision(
    outcome: PassOutcome,
    dropped: u32,
    free_and_target_bytes: Option<(u64, u64)>,
) -> LatchDecision {
    if !outcome.latches_the_day() {
        return LatchDecision::KeepOpen {
            reason: outcome.as_str(),
            refund: refunds_attempt(outcome) || dropped > 0,
        };
    }
    if dropped > 0 {
        return LatchDecision::KeepOpen {
            reason: "dropped_this_pass",
            refund: true,
        };
    }
    match free_and_target_bytes {
        None => LatchDecision::KeepOpen {
            reason: "free_space_unknown",
            refund: true,
        },
        Some((free, target)) if free < target => LatchDecision::KeepOpen {
            reason: "below_free_target",
            refund: true,
        },
        Some(_) => LatchDecision::Latch,
    }
}

/// Whether this tick should read the session day's size (PR AA step 5): not
/// yet measured, the session's writes have ended (15:40 IST,
/// `TICK_PERSIST_END_SECS_OF_DAY_IST`), still the same calendar day (an
/// after-midnight reading would come after the night's drops), and this
/// process has not dropped any partition of that day yet. Pure, O(1).
#[must_use]
pub fn should_measure_session(
    already_measured: bool,
    secs_into_session_day: u32,
    dropped_this_session_day: u64,
) -> bool {
    !already_measured
        && (TICK_PERSIST_END_SECS_OF_DAY_IST..SECS_PER_DAY).contains(&secs_into_session_day)
        && dropped_this_session_day == 0
}

/// The config a daily pass runs with (PR AA step 5, owner decision 3:
/// "below target after the close, the daily leg uses the hour path").
///
/// Above the target, or with free space unknown, the config is unchanged.
/// Below it, the pass carries the hour-path marker, so `tick_quotes`,
/// `market_depth_book` and `feed_aux_packets` (and `ticks` behind its own
/// switch) are archived by the hour, each hour more than 4 hours past its
/// end. Two things are deliberately NOT taken from the disk-pressure config:
/// the day windows stay as configured (candles, SEBI and audit tables keep
/// them), and `market_depth` is never forced onto the hour path while a
/// depth spill file is pending (that override is the pressure leg's). Pure.
#[must_use]
pub fn daily_pass_config(
    cfg: &PartitionRetentionConfig,
    below_target: bool,
) -> PartitionRetentionConfig {
    if !below_target {
        return cfg.clone();
    }
    PartitionRetentionConfig {
        under_disk_pressure: true,
        market_depth_force_with_spill_pending: false,
        ..cfg.clone()
    }
}

/// One `statvfs` of the data volume, off the runtime worker:
/// `(free bytes, total bytes)`, or `None` when it fails.
async fn probe_data_volume(data_dir: &Path) -> Option<(u64, u64)> {
    let dir = data_dir.to_path_buf();
    match tokio::task::spawn_blocking(move || probe_disk_free_bytes(&dir)).await {
        Ok(DiskHealthOutcome::Ok {
            free_bytes,
            total_bytes,
        }) => Some((free_bytes, total_bytes)),
        Ok(DiskHealthOutcome::ProbeFailed { .. }) | Err(_) => None,
    }
}

/// What one attempt produced: its outcome, the pass summary, and (when
/// asked for) the session day's size read before the pass.
struct PassReport {
    outcome: PassOutcome,
    summary: ArchiveRunSummary,
    session_bytes: Option<anyhow::Result<u64>>,
}

/// Runs ONE sweep and classifies it. Never panics; never returns `Err`.
///
/// With `measure_day` set, the session day's size is read first, before the
/// pass can drop any partition of it (PR AA step 5).
async fn run_one_pass(
    questdb: &QuestDbConfig,
    cfg: &PartitionRetentionConfig,
    verified_day_gate: Option<&VerifiedDayGate>,
    measure_day: Option<NaiveDate>,
    stop_at_unix_secs: Option<i64>,
) -> PassReport {
    match PartitionArchiver::new(questdb, cfg).await {
        Ok(Some(archiver)) => {
            let session_bytes = match measure_day {
                Some(day) => Some(archiver.session_bytes_on(day).await),
                None => None,
            };
            // The cross-verification hold (no-rest lock §12.15): a day's
            // partitions wait for that day's 1-minute check. Only THIS leg
            // carries the gate — the disk-pressure leg never waits on it.
            let mut archiver = match verified_day_gate {
                Some(gate) => archiver.with_verified_day_gate(std::sync::Arc::clone(gate)),
                None => archiver,
            };
            // PR AA step 6: the daily leg measures the copy work it leaves
            // for the drain hold.
            if cfg.drain_hold_counts_archive {
                archiver = archiver.with_backlog_measure();
            }
            // PR AA review: the pass stops at its window's end (07:30 IST),
            // never running into the 08:30 boot and the 09:00 session.
            archiver = archiver.with_pass_limits(ArchivePassLimits {
                stop_at_unix_secs,
                ..ArchivePassLimits::default()
            });
            let summary = archiver.archive_and_drop_old_partitions().await;
            let verdict = pass_verdict(&summary, cfg.max_partitions_per_run);
            info!(
                tables_scanned = summary.tables_scanned,
                partitions_considered = summary.partitions_considered,
                verified = summary.verified,
                dropped = summary.dropped,
                failed = summary.failed,
                tables_list_failed = summary.tables_list_failed,
                tables_absent = summary.tables_absent,
                rows_archived = summary.rows_archived,
                held_unverified = summary.held_unverified,
                gzip_bytes_uploaded = summary.gzip_bytes_uploaded,
                csv_bytes_exported = summary.csv_bytes_exported,
                verdict = ?verdict,
                "daily partition archive pass complete (verified S3 copy before every drop)"
            );
            PassReport {
                outcome: PassOutcome::Ran(verdict),
                summary,
                session_bytes,
            }
        }
        // No explicit archive bucket and no resolvable environment — archival
        // skipped rather than guessing the prod bucket. The constructor
        // already logged the actionable warn. Never latches the day.
        Ok(None) => PassReport {
            outcome: PassOutcome::Skipped,
            summary: ArchiveRunSummary::default(),
            session_bytes: None,
        },
        Err(err) => {
            error!(
                ?err,
                code = ErrorCode::StorageGap04S3ArchiveFailed.code_str(),
                "partition archiver construction failed — this attempt is a loud no-op \
                 and the day stays UNLATCHED so the next tick retries"
            );
            PassReport {
                outcome: PassOutcome::Failed,
                summary: ArchiveRunSummary::default(),
                session_bytes: None,
            }
        }
    }
}

/// Spawns the supervised daily-archive loop, or `None` when it must not run.
///
/// Returning `None` is always a deliberate refusal with a log line naming the
/// reason — never a silent no-op.
///
/// `data_dir` is the data volume the free-space target is measured on, and the
/// home of `state/archive_session_bytes.json` (PR AA step 5); `main.rs` passes
/// the same path the disk-pressure loop probes.
pub fn spawn_supervised_daily_archive_loop(
    data_dir: PathBuf,
    questdb: QuestDbConfig,
    cfg: PartitionRetentionConfig,
    market_close_time: String,
    verified_day_gate: Option<VerifiedDayGate>,
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
            let d = data_dir.clone();
            let q = questdb.clone();
            let c = cfg.clone();
            let g = verified_day_gate.clone();
            let handle =
                tokio::spawn(async move { run_daily_archive_loop(d, q, c, window_open, g).await });
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
    data_dir: PathBuf,
    questdb: QuestDbConfig,
    cfg: PartitionRetentionConfig,
    window_open: u32,
    verified_day_gate: Option<VerifiedDayGate>,
) {
    let mut ledger = AttemptLedger::default();
    let mut ticker = tokio::time::interval(DAILY_ARCHIVE_POLL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    // Pre-register both series at zero so the CloudWatch agent's counter-delta
    // baseline is the harmless zero rather than the first real event. The
    // repo has been bitten by an absent series reading as a healthy one.
    for outcome in SEEDED_ATTEMPT_OUTCOMES {
        metrics::counter!("tv_daily_archive_attempts_total", "outcome" => outcome).increment(0);
    }
    // PR AA (owner Quote 51): every `writer` of the archive quiesce counter.
    tickvault_storage::archive_quiesce::register_quiesce_counters();
    // PR AA step 6 (AA-5, AA-6): the backlog the drain hold reads and the
    // snapshot-wait gauge, both at 0 before the first pass.
    crate::archive_backlog::register_archive_backlog_gauge();
    tickvault_storage::partition_archive::register_archive_snapshot_wait_gauge();
    let completed_gauge = metrics::gauge!("tv_daily_archive_completed_today");
    // 2026-10-06 (plan item 51a): the day this process latched, held in RAM.
    // The marker can fail to save; without this latch every tick would
    // re-run a finished sweep until the attempt cap, then log a false
    // "did NOT complete" error.
    let mut latched_day: Option<i32> = None;
    // PR AA step 5: the measured session sizes behind the free-space target.
    // A refused file is replaced by the next save; until five days are
    // measured again the target uses the configured fallback.
    let ring_path = session_bytes_path(&data_dir);
    let mut ring = match load_session_bytes(&ring_path) {
        Ok(ring) => ring,
        Err(reason) => {
            warn!(
                code = ErrorCode::StorageGap04S3ArchiveFailed.code_str(),
                source = "archive_session_bytes_refused",
                path = %ring_path.display(),
                reason = %reason,
                "daily archive: the session-size file was refused; the free-space \
                 target uses the fallback session size until five days are measured again"
            );
            SessionBytesRing::default()
        }
    };
    // Partitions this process dropped per session day: a session is measured
    // only before the first of them.
    let mut dropped_on: (i32, u64) = (0, 0);

    loop {
        ticker.tick().await;

        // Read the day ONCE, before anything else, and use this captured value
        // for the whole tick. A sweep that crosses midnight must latch the day
        // it STARTED, or tomorrow opens already sealed. PR AA step 5: the day
        // is the SESSION day, so the window runs past midnight until 07:30 IST.
        let (calendar_date, calendar_secs) = now_ist();
        let (today_date, now_secs) = archive_session_day(calendar_date, calendar_secs);
        let today = today_date.num_days_from_ce();

        let completed_today =
            crate::daily_task_marker::daily_marker_exists(DAILY_ARCHIVE_MARKER_TASK, today_date)
                || latched_day == Some(today);
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

        // PR AA step 5: below the free-space target the pass also archives
        // the Quote 51 tables by the hour (owner decision 3).
        let disk_before = probe_data_volume(&data_dir).await;
        let below_target =
            disk_before.is_some_and(|(free, total)| free < free_target_bytes(&ring, &cfg, total));
        let dropped_today = if dropped_on.0 == today {
            dropped_on.1
        } else {
            0
        };
        let measure_day = should_measure_session(ring.has_day(today_date), now_secs, dropped_today)
            .then_some(today_date);
        let pass_cfg = daily_pass_config(&cfg, below_target);

        // PR AA step 6: a running pass keeps the last backlog reading fresh
        // for the drain hold. The process's first pass has no reading yet,
        // so it writes a provisional one from the probe above (PR AA
        // finding 5): the box stays up while that first pass runs.
        if cfg.drain_hold_counts_archive {
            crate::archive_backlog::ARCHIVE_BACKLOG
                .note_pass_started(chrono::Utc::now().timestamp(), below_target);
        }
        let report = run_one_pass(
            &questdb,
            &pass_cfg,
            verified_day_gate.as_ref(),
            measure_day,
            daily_pass_stop_unix_secs(today_date),
        )
        .await;
        let outcome = report.outcome;
        metrics::counter!("tv_daily_archive_attempts_total", "outcome" => outcome.as_str())
            .increment(1);

        match report.session_bytes {
            // A day that wrote nothing (a holiday) is not recorded, so it
            // never pulls the p95 down; it is not read again either way once
            // the next session day starts.
            Some(Ok(0)) | None => {}
            Some(Ok(bytes)) => {
                ring.record(today_date, bytes);
                info!(
                    session_day = %today_date,
                    session_bytes = bytes,
                    measured_days = ring.len(),
                    "daily archive: session size measured for the free-space target"
                );
                if let Err(err) = save_session_bytes(&ring_path, &ring) {
                    // APPROVED: the reading is kept in memory for this process; the file is a sizing estimate, not market data, and a lost save only makes a restart use fewer measured days
                    warn!(
                        code = ErrorCode::StorageGap04S3ArchiveFailed.code_str(),
                        source = "archive_session_bytes_not_saved",
                        path = %ring_path.display(),
                        ?err,
                        "daily archive: the session size could not be saved; it is kept for \
                         this process"
                    );
                }
            }
            Some(Err(err)) => warn!(
                code = ErrorCode::StorageGap04S3ArchiveFailed.code_str(),
                source = "archive_session_bytes_unread",
                session_day = %today_date,
                ?err,
                "daily archive: the session size could not be read; the next tick tries \
                 again until a partition of the day is dropped"
            ),
        }

        let dropped = report.summary.dropped;
        dropped_on = (today, dropped_today.saturating_add(u64::from(dropped)));
        let disk_after = probe_data_volume(&data_dir).await;
        let free_and_target =
            disk_after.map(|(free, total)| (free, free_target_bytes(&ring, &cfg, total)));
        let latch = daily_latch_decision(outcome, dropped, free_and_target);
        // PR AA step 6 (AA-5): the copy work this pass left, for the drain
        // hold. Off with `drain_hold_counts_archive = false`.
        if cfg.drain_hold_counts_archive {
            let cell = &crate::archive_backlog::ARCHIVE_BACKLOG;
            if report.summary.pass_ran {
                cell.publish(
                    report.summary.archive_backlog_bytes,
                    free_and_target.is_some_and(|(free, target)| free < target),
                    chrono::Utc::now().timestamp(),
                );
            } else {
                cell.note_pass_ended();
            }
        }
        info!(
            attempt,
            outcome = outcome.as_str(),
            dropped,
            hour_path_for_free_target = below_target,
            archive_backlog_bytes = report.summary.archive_backlog_bytes,
            free_bytes = free_and_target.map(|(free, _)| free),
            target_free_bytes = free_and_target.map(|(_, target)| target),
            decision = ?latch,
            "daily archive pass judged"
        );

        // REFUND before anything else reads the ledger. A contended pass never
        // started — the disk-pressure leg holds the lock and is doing exactly
        // this work — so charging the day's six attempts for it would exhaust
        // the budget on someone else's success and then abandon the daily
        // sweep once the lock freed. Found by adversarial review 2026-09-01.
        // DA-13d: a pass that held depth partitions for the after-close depth
        // pass is refunded the same way (see `refunds_attempt`). PR AA step 5:
        // a pass that dropped something, or a clean pass below the free-space
        // target, is given back too (see `daily_latch_decision`).
        if let LatchDecision::KeepOpen {
            reason,
            refund: true,
        } = latch
        {
            ledger.refund(today);
            debug!(
                attempt,
                reason,
                "daily archive tick keeps the day open — another job held what this pass \
                 needs, or the pass made progress, or free space is still below the target; \
                 attempt refunded, not spent, and the next tick retries"
            );
            continue;
        }

        if latch == LatchDecision::Latch {
            // Latch in RAM FIRST, so this process never repeats a finished
            // sweep even if the marker cannot be saved. The marker makes it
            // durable, so a crash-restart three seconds later does not re-run
            // it either; a marker that fails to save costs at most one
            // redundant sweep after a restart, never a skipped one.
            latched_day = Some(today);
            completed_gauge.set(1.0);
            match crate::daily_task_marker::try_write_daily_marker(
                DAILY_ARCHIVE_MARKER_TASK,
                today_date,
            ) {
                Ok(crate::daily_task_marker::MarkerDurability::Durable) => info!(
                    attempt,
                    "daily partition archive COMPLETE for today — latched (durable marker \
                     written; a restart will not repeat it)"
                ),
                Ok(crate::daily_task_marker::MarkerDurability::RenamedNotDirSynced) => warn!(
                    code = ErrorCode::StorageGap04S3ArchiveFailed.code_str(),
                    source = "daily_archive_latch_marker_failed",
                    attempt,
                    durability = "renamed_not_dir_synced",
                    "daily partition archive COMPLETED; only its latch marker was not saved \
                     durably — this process will not repeat it, a restart may repeat the \
                     sweep once"
                ),
                Err(err) => warn!(
                    code = ErrorCode::StorageGap04S3ArchiveFailed.code_str(),
                    source = "daily_archive_latch_marker_failed",
                    attempt,
                    ?err,
                    "daily partition archive COMPLETED; only its latch marker was not saved \
                     durably — this process will not repeat it, a restart may repeat the \
                     sweep once"
                ),
            }
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

    /// 2026-09-23: 19 retired tables made the day unlatchable. An absent
    /// table must latch; a real list failure beside it must still not.
    #[test]
    fn absent_tables_latch_the_day_but_a_real_list_failure_does_not() {
        let absent_only = ArchiveRunSummary {
            tables_absent: 19,
            ..summary(0, 0, 0)
        };
        assert!(matches!(
            pass_verdict(&absent_only, 200),
            PassVerdict::Complete
        ));
        let absent_and_failed = ArchiveRunSummary {
            tables_absent: 19,
            ..summary(0, 0, 1)
        };
        assert!(matches!(
            pass_verdict(&absent_and_failed, 200),
            PassVerdict::Incomplete("table_list_failed")
        ));
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
    /// window the box is actually alive for (15:30 close → the daily stop,
    /// 16:30 since 2026-10-22, plan item 64).
    #[test]
    fn the_attempt_budget_fits_inside_the_evening_window() {
        let window_secs = u64::from(crate::shutdown_class::SCHEDULED_DAILY_STOP_SECS_OF_DAY_IST)
            - (15 * 3600 + 30 * 60);
        assert_eq!(window_secs, 3600, "15:30 → 16:30 IST");
        let needed = DAILY_ARCHIVE_POLL.as_secs() * u64::from(MAX_DAILY_ARCHIVE_ATTEMPTS);
        assert!(
            needed <= window_secs,
            "{MAX_DAILY_ARCHIVE_ATTEMPTS} attempts at {}s apart needs {needed}s, \
             which does not fit the ~{window_secs}s the box is alive after the \
             close — the later attempts would never be reached",
            DAILY_ARCHIVE_POLL.as_secs()
        );
    }

    /// 2026-10-06 (plan item 51a): a latch marker that fails to save must not
    /// make this process re-run a finished sweep. The day is latched in RAM
    /// before the write, `completed_today` reads that latch, and the failure
    /// arm says the archive COMPLETED so a grep for STORAGE-GAP-04 is not read
    /// as an archive failure.
    #[test]
    fn test_latch_marker_failure_still_latches_in_process() {
        let src = include_str!("daily_archive_boot.rs");
        let prod = src.split("#[cfg(test)]").next().unwrap_or("");
        let body = prod
            .split("async fn run_daily_archive_loop(")
            .nth(1)
            .and_then(|s| s.split("\n}\n").next())
            .unwrap_or("");
        assert!(!body.is_empty(), "the loop body must exist");
        assert!(
            body.contains("|| latched_day == Some(today);"),
            "completed_today must OR the in-process latch"
        );
        // PR AA review: each pass stops at its window's end, 07:30 IST.
        assert!(body.contains("daily_pass_stop_unix_secs(today_date),"));
        assert!(prod.contains("archiver = archiver.with_pass_limits(ArchivePassLimits {"));
        let latch = body
            .find("latched_day = Some(today);")
            .unwrap_or(usize::MAX);
        let write = body
            .find("daily_task_marker::try_write_daily_marker(")
            .unwrap_or(0);
        assert_ne!(latch, usize::MAX, "the day must be latched in RAM");
        assert!(latch < write, "latch BEFORE the marker write");
        let failure = &body[write..];
        assert_eq!(
            failure
                .matches("code = ErrorCode::StorageGap04S3ArchiveFailed.code_str()")
                .count(),
            2,
            "both failure arms must be coded"
        );
        assert_eq!(
            failure
                .matches("source = \"daily_archive_latch_marker_failed\"")
                .count(),
            2
        );
        assert!(failure.contains("archive COMPLETED"));
        assert!(!prod.contains("daily_task_marker::write_daily_marker("));
    }

    // ---- PR AA step 5 (owner Quote 51): the daily leg runs until done ----

    fn ist(y: i32, m: u32, d: u32, hh: u32, mm: u32, ss: u32) -> (NaiveDate, u32) {
        (
            NaiveDate::from_ymd_opt(y, m, d).expect("date"),
            hh * 3_600 + mm * 60 + ss,
        )
    }

    /// The window no longer stops at midnight: a clock before 07:30 IST
    /// still belongs to the previous session day, and from 07:30 the new day
    /// is before its own window.
    #[test]
    fn test_loop_continues_past_midnight_until_0730() {
        let open = window_open_secs_of_day("15:40:00").expect("opens");
        let fri = NaiveDate::from_ymd_opt(2026, 11, 6).expect("date");
        let sat = NaiveDate::from_ymd_opt(2026, 11, 7).expect("date");
        let tick_at = |(date, secs): (NaiveDate, u32), completed: Option<NaiveDate>| {
            let (session, into) = archive_session_day(date, secs);
            (
                session,
                archive_tick(
                    into,
                    open,
                    completed == Some(session),
                    0,
                    MAX_DAILY_ARCHIVE_ATTEMPTS,
                ),
            )
        };
        for (clock, want_day, want) in [
            (ist(2026, 11, 6, 15, 45, 0), fri, ArchiveTick::Run),
            (ist(2026, 11, 6, 23, 59, 59), fri, ArchiveTick::Run),
            (ist(2026, 11, 7, 0, 0, 0), fri, ArchiveTick::Run),
            (ist(2026, 11, 7, 0, 5, 0), fri, ArchiveTick::Run),
            (ist(2026, 11, 7, 7, 29, 59), fri, ArchiveTick::Run),
            (ist(2026, 11, 7, 7, 30, 0), sat, ArchiveTick::BeforeWindow),
            (ist(2026, 11, 7, 8, 30, 0), sat, ArchiveTick::BeforeWindow),
            (ist(2026, 11, 7, 15, 39, 59), sat, ArchiveTick::BeforeWindow),
            (ist(2026, 11, 7, 15, 40, 2), sat, ArchiveTick::Run),
        ] {
            assert_eq!(tick_at(clock, None), (want_day, want), "{clock:?}");
        }
        // A day latched before midnight stays completed after it, and the
        // new day is not sealed by it.
        assert_eq!(
            tick_at(ist(2026, 11, 7, 2, 0, 0), Some(fri)),
            (fri, ArchiveTick::AlreadyCompleted)
        );
        assert_eq!(
            tick_at(ist(2026, 11, 7, 15, 45, 0), Some(fri)),
            (sat, ArchiveTick::Run)
        );
        // Attempts stay on the session day across midnight.
        let mut ledger = AttemptLedger::default();
        ledger.record(fri.num_days_from_ce());
        let (session, _) = archive_session_day(sat, 3_600);
        assert_eq!(ledger.attempts_on(session.num_days_from_ce()), 1);
        // 00:05 reads as 86,700 seconds into Friday.
        assert_eq!(archive_session_day(sat, 300), (fri, 86_700));
        // The first date chrono knows has no previous day: read as today.
        assert_eq!(
            archive_session_day(NaiveDate::MIN, 10),
            (NaiveDate::MIN, 10)
        );
        assert_eq!(ARCHIVE_WINDOW_END_SECS_IST, 27_000);
    }

    /// The day latches only on a clean pass that dropped nothing with free
    /// space at or above the target; every other case keeps it open.
    #[test]
    fn test_no_latch_while_eligible_or_below_target() {
        let complete = PassOutcome::Ran(PassVerdict::Complete);
        let target = 60;
        assert_eq!(
            daily_latch_decision(complete, 0, Some((60, target))),
            LatchDecision::Latch
        );
        assert_eq!(
            daily_latch_decision(complete, 0, Some((u64::MAX, target))),
            LatchDecision::Latch
        );
        assert_eq!(
            daily_latch_decision(complete, 0, Some((59, target))),
            LatchDecision::KeepOpen {
                reason: "below_free_target",
                refund: true
            }
        );
        assert_eq!(
            daily_latch_decision(complete, 0, None),
            LatchDecision::KeepOpen {
                reason: "free_space_unknown",
                refund: true
            }
        );
        // Something was dropped, so more may now be old enough: poll again,
        // even with the disk well above the target.
        assert_eq!(
            daily_latch_decision(complete, 3, Some((u64::MAX, target))),
            LatchDecision::KeepOpen {
                reason: "dropped_this_pass",
                refund: true
            }
        );
        // No incomplete pass ever latches, whatever the disk says.
        let incomplete = [
            PassOutcome::Skipped,
            PassOutcome::Failed,
            PassOutcome::Ran(PassVerdict::Incomplete("budget_exhausted")),
            PassOutcome::Ran(PassVerdict::Incomplete("partition_failed")),
            PassOutcome::Ran(PassVerdict::Incomplete("wal_not_applied")),
            PassOutcome::Ran(PassVerdict::Incomplete("contended")),
            PassOutcome::Ran(PassVerdict::Incomplete("quiesce_busy")),
            PassOutcome::Ran(PassVerdict::Incomplete("did_not_run")),
        ];
        for outcome in incomplete {
            for disk in [None, Some((0, target)), Some((u64::MAX, target))] {
                for dropped in [0, 1] {
                    let LatchDecision::KeepOpen { reason, .. } =
                        daily_latch_decision(outcome, dropped, disk)
                    else {
                        panic!("{outcome:?} {dropped} {disk:?} latched");
                    };
                    assert_eq!(reason, outcome.as_str());
                }
            }
        }
    }

    /// A pass that drops something gives its attempt back, so a backlog
    /// larger than one pass is never cut off by the cap; a pass that fails
    /// without progress still spends one, so the cap still ends a day that
    /// keeps failing.
    #[test]
    fn test_drop_refunds_attempt_and_attempts_cap_bounds() {
        let open = window_open_secs_of_day("15:40:00").expect("opens");
        let day = 740_000;
        let mut ledger = AttemptLedger::default();
        let run = |ledger: &mut AttemptLedger, outcome: PassOutcome, dropped: u32| {
            let tick = archive_tick(
                open + 60,
                open,
                false,
                ledger.attempts_on(day),
                MAX_DAILY_ARCHIVE_ATTEMPTS,
            );
            if tick != ArchiveTick::Run {
                return tick;
            }
            ledger.record(day);
            if let LatchDecision::KeepOpen { refund: true, .. } =
                daily_latch_decision(outcome, dropped, Some((0, 1)))
            {
                ledger.refund(day);
            }
            tick
        };
        let exhausted = PassOutcome::Ran(PassVerdict::Incomplete("budget_exhausted"));
        let failed = PassOutcome::Ran(PassVerdict::Incomplete("partition_failed"));
        // Fifty budget-capped passes that each dropped partitions: none spent.
        for _ in 0..50 {
            assert_eq!(run(&mut ledger, exhausted, 200), ArchiveTick::Run);
        }
        assert_eq!(ledger.attempts_on(day), 0);
        // Clean passes below the target are given back too.
        for _ in 0..50 {
            run(&mut ledger, PassOutcome::Ran(PassVerdict::Complete), 0);
        }
        assert_eq!(ledger.attempts_on(day), 0);
        // A pass that fails and drops nothing spends one; the sixth such
        // pass is the last, and the seventh tick reads exhausted.
        for n in 1..=MAX_DAILY_ARCHIVE_ATTEMPTS {
            assert_eq!(run(&mut ledger, failed, 0), ArchiveTick::Run);
            assert_eq!(ledger.attempts_on(day), n);
        }
        assert_eq!(run(&mut ledger, failed, 0), ArchiveTick::AttemptsExhausted);
        // A failing pass that still dropped something is progress, not spent.
        let mut fresh = AttemptLedger::default();
        run(&mut fresh, failed, 1);
        assert_eq!(fresh.attempts_on(day), 0);
    }

    /// PR AA review: passes that drop nothing only because QuestDB had not
    /// applied a WAL yet (evening apply backlog) are given back. Before the
    /// fix six of them (about an hour) ended the leg for the night, and the
    /// hours old enough from 18:00 to 20:00 were never archived. A pass that
    /// also failed a partition reads `partition_failed` and still spends.
    #[test]
    fn test_regression_wal_lag_passes_never_exhaust_the_night() {
        let day = 740_000;
        let mut ledger = AttemptLedger::default();
        let lagging = ArchiveRunSummary {
            pass_ran: true,
            tables_wal_not_applied: 1,
            partitions_considered: 4,
            ..ArchiveRunSummary::default()
        };
        // 18:00 to 07:30 at the five-minute poll.
        for tick in 0..162_u32 {
            let now = 18 * 3_600 + tick * 300;
            assert_eq!(
                archive_tick(
                    now,
                    56_400,
                    false,
                    ledger.attempts_on(day),
                    MAX_DAILY_ARCHIVE_ATTEMPTS
                ),
                ArchiveTick::Run,
                "tick {tick}"
            );
            ledger.record(day);
            let outcome = PassOutcome::Ran(pass_verdict(&lagging, 200));
            if let LatchDecision::KeepOpen { refund: true, .. } =
                daily_latch_decision(outcome, 0, Some((0, 1)))
            {
                ledger.refund(day);
            }
        }
        assert_eq!(ledger.attempts_on(day), 0);
        let lagging_and_failed = ArchiveRunSummary {
            failed: 1,
            ..lagging
        };
        let verdict = pass_verdict(&lagging_and_failed, 200);
        assert_eq!(verdict, PassVerdict::Incomplete("partition_failed"));
        assert!(!refunds_attempt(PassOutcome::Ran(verdict)));
    }

    /// PR AA review: a daily pass stops at its window's end, 07:30 IST the
    /// next calendar day, and the loop hands that stop to the archiver.
    #[test]
    fn test_daily_pass_stops_at_the_window_end() {
        let day = NaiveDate::from_ymd_opt(2026, 11, 9).expect("date");
        let stop = daily_pass_stop_unix_secs(day).expect("stop");
        let expected = chrono::NaiveDate::from_ymd_opt(2026, 11, 10)
            .and_then(|d| d.and_hms_opt(2, 0, 0))
            .expect("02:00 UTC = 07:30 IST")
            .and_utc()
            .timestamp();
        assert_eq!(stop, expected);
        let started_0729 = expected - 60;
        assert!(
            !tickvault_storage::partition_archive::pass_deadline_reached(Some(stop), started_0729)
        );
        assert!(tickvault_storage::partition_archive::pass_deadline_reached(
            Some(stop),
            expected
        ));
        // The wiring is pinned in `test_latch_marker_failure_still_latches_in_process`.
    }

    /// A session is read once, after 15:40, the same calendar day, and only
    /// before this process dropped any of its partitions.
    #[test]
    fn test_should_measure_session_once_after_1540_before_any_drop() {
        let end = TICK_PERSIST_END_SECS_OF_DAY_IST;
        assert!(should_measure_session(false, end, 0));
        assert!(should_measure_session(false, 86_399, 0));
        assert!(!should_measure_session(false, end - 1, 0), "before 15:40");
        assert!(!should_measure_session(false, 86_400, 0), "after midnight");
        assert!(!should_measure_session(false, 86_700, 0), "after midnight");
        assert!(!should_measure_session(true, end, 0), "already measured");
        assert!(!should_measure_session(false, end, 1), "after a drop");
    }

    /// Below the target the daily pass gets the hour-path marker and nothing
    /// else from the pressure config: day windows stay, and `market_depth`
    /// is not forced past a pending spill file.
    #[test]
    fn test_daily_pass_config_below_target_uses_hour_path_only() {
        let cfg = PartitionRetentionConfig {
            depth_hot_hours: 4,
            market_depth_force_with_spill_pending: true,
            ..PartitionRetentionConfig::default()
        };
        // The config has no `PartialEq`; its `Debug` text names every field.
        let text = |c: &PartitionRetentionConfig| format!("{c:?}");
        assert_eq!(text(&daily_pass_config(&cfg, false)), text(&cfg));
        let below = daily_pass_config(&cfg, true);
        assert!(below.under_disk_pressure);
        assert!(!below.market_depth_force_with_spill_pending);
        assert_eq!(
            text(&PartitionRetentionConfig {
                under_disk_pressure: false,
                market_depth_force_with_spill_pending: true,
                ..below
            }),
            text(&cfg),
            "every other field, the day windows included, is unchanged"
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

    /// A table kept by the per-partition WAL-applied gate (2026-10-02) must
    /// keep the day open, or a day on which a lagging table archived
    /// nothing would latch as done.
    #[test]
    fn wal_not_applied_tables_prevent_a_complete_day() {
        let lagging = ArchiveRunSummary {
            pass_ran: true,
            tables_wal_not_applied: 1,
            ..ArchiveRunSummary::default()
        };
        assert_eq!(
            pass_verdict(&lagging, 200),
            PassVerdict::Incomplete("wal_not_applied")
        );
    }

    // Regression: 2026-10-13 (DA-13d) — depth partitions held while the
    // after-close pass writes them must keep the day open, or they wait a day.
    #[test]
    fn test_regression_da13d_held_depth_partitions_keep_the_day_open() {
        let held = ArchiveRunSummary {
            pass_ran: true,
            held_deferred_depth: 1,
            ..ArchiveRunSummary::default()
        };
        assert_eq!(
            pass_verdict(&held, 200),
            PassVerdict::Incomplete("deferred_depth_pass")
        );
        assert!(!PassOutcome::Ran(pass_verdict(&held, 200)).latches_the_day());
    }

    // `test_regression_held_top_volume_partitions_keep_the_day_open` REMOVED
    // 2026-10-22 (plan item 62): the hold and its verdict are gone.
    #[test]
    fn test_top_volume_spill_pending_is_no_longer_a_seeded_outcome() {
        assert!(!SEEDED_ATTEMPT_OUTCOMES.contains(&"top_volume_spill_pending"));
    }

    // Regression: 2026-10-13 (DA-13d review) — the after-close depth pass
    // holds the depth partitions from ~15:45 to ~17:16 IST. Every daily tick
    // in that stretch spent an attempt, the six ran out by ~16:10, and
    // STORAGE-GAP-04 paged on a working box. A hold must be refunded, and a
    // real failure in the same pass must still spend its attempt.
    #[test]
    fn test_regression_da13d_refunds_attempt_only_for_a_held_depth_pass() {
        let held = ArchiveRunSummary {
            pass_ran: true,
            held_deferred_depth: 3,
            ..ArchiveRunSummary::default()
        };
        let mut ledger = AttemptLedger::default();
        // Twenty ticks: 15:45 to 17:25 at the five-minute poll.
        for _ in 0..20 {
            assert_eq!(
                archive_tick(60_000, 55_802, false, ledger.attempts_on(9), 6),
                ArchiveTick::Run,
                "a held depth pass must never exhaust the day's attempts"
            );
            ledger.record(9);
            let outcome = PassOutcome::Ran(pass_verdict(&held, 200));
            if refunds_attempt(outcome) {
                ledger.refund(9);
            }
        }
        assert_eq!(ledger.attempts_on(9), 0);

        // A partition failure in the same pass is the verdict, and it spends.
        let held_and_failed = ArchiveRunSummary { failed: 1, ..held };
        let verdict = pass_verdict(&held_and_failed, 200);
        assert_eq!(verdict, PassVerdict::Incomplete("partition_failed"));
        assert!(!refunds_attempt(PassOutcome::Ran(verdict)));
        assert!(refunds_attempt(PassOutcome::Ran(PassVerdict::Incomplete(
            "contended"
        ))));
        assert!(!refunds_attempt(PassOutcome::Ran(PassVerdict::Complete)));
        assert!(!refunds_attempt(PassOutcome::Failed));
    }

    /// PR AA step 2: a pass whose hour partitions were held because an
    /// old-hour writer had the archive quiesce lock is incomplete and gives
    /// its attempt back; held partitions take no budget; an hour held only
    /// because its rows may still be in flight does not keep the day open.
    #[test]
    fn test_quiesce_busy_refunds_and_in_flight_does_not_hold_the_day() {
        let ran = ArchiveRunSummary {
            pass_ran: true,
            ..ArchiveRunSummary::default()
        };
        let busy = ArchiveRunSummary {
            held_quiesce_busy: 2,
            ..ran
        };
        let verdict = pass_verdict(&busy, 200);
        assert_eq!(verdict, PassVerdict::Incomplete("quiesce_busy"));
        assert!(refunds_attempt(PassOutcome::Ran(verdict)));
        assert!(SEEDED_ATTEMPT_OUTCOMES.contains(&"quiesce_busy"));
        // A real failure in the same pass still spends the attempt.
        let busy_and_failed = ArchiveRunSummary { failed: 1, ..busy };
        assert_eq!(
            pass_verdict(&busy_and_failed, 200),
            PassVerdict::Incomplete("partition_failed")
        );
        let in_flight = ArchiveRunSummary {
            held_in_flight: 3,
            ..ran
        };
        assert_eq!(pass_verdict(&in_flight, 200), PassVerdict::Complete);
        // Held partitions take worklist slots but no budget.
        let full_of_holds = ArchiveRunSummary {
            partitions_considered: 200,
            held_in_flight: 100,
            held_quiesce_busy: 100,
            ..ran
        };
        assert_eq!(
            pass_verdict(&full_of_holds, 200),
            PassVerdict::Incomplete("quiesce_busy")
        );
    }

    // Regression: 2026-10-13 (DA-13d re-review) — held depth partitions took
    // slots in the per-run budget, so a backlog of 200 read as
    // budget_exhausted, was not refunded, and the attempts still ran out.
    #[test]
    fn test_regression_da13d_held_partitions_do_not_spend_the_budget() {
        let full_of_held = ArchiveRunSummary {
            pass_ran: true,
            partitions_considered: 200,
            held_deferred_depth: 200,
            ..ArchiveRunSummary::default()
        };
        let verdict = pass_verdict(&full_of_held, 200);
        assert_eq!(verdict, PassVerdict::Incomplete("deferred_depth_pass"));
        assert!(refunds_attempt(PassOutcome::Ran(verdict)));

        let truly_full = ArchiveRunSummary {
            held_deferred_depth: 0,
            ..full_of_held
        };
        let verdict = pass_verdict(&truly_full, 200);
        assert_eq!(verdict, PassVerdict::Incomplete("budget_exhausted"));
        assert!(!refunds_attempt(PassOutcome::Ran(verdict)));

        // 200 real partitions besides the held ones: still budget-bound.
        let over = ArchiveRunSummary {
            partitions_considered: 210,
            held_deferred_depth: 10,
            ..full_of_held
        };
        assert_eq!(
            pass_verdict(&over, 200),
            PassVerdict::Incomplete("budget_exhausted")
        );
    }

    // Regression: 2026-10-13 (DA-13d review) — the archiver's hold counter
    // was not seeded at 0. The boot seed (`count_depth_partitions_held(0)`
    // in main.rs) must put the exact series a hold increments on /metrics.
    #[test]
    fn test_regression_da13d_depth_hold_counter_seeds_at_zero() {
        let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, || {
            tickvault_storage::partition_archive::count_depth_partitions_held(0);
        });
        let rendered = handle.render();
        let line = rendered
            .lines()
            .find(|l| {
                l.starts_with("tv_partition_archive_skipped_total{")
                    && l.contains("reason=\"deferred_depth_pass\"")
            })
            .unwrap_or_else(|| panic!("hold series not seeded; got:\n{rendered}"));
        assert!(line.ends_with(" 0"), "seeded at 0, got {line}");
    }

    // PR AA step 1: the archive quiesce counter puts every `writer` series on
    // /metrics at 0 (seeded by the daily archive loop), and a deferral adds
    // one to its own writer only. Local /metrics only: no page, no alarm.
    #[test]
    fn test_archive_quiesce_counters_seed_at_zero_and_count() {
        use tickvault_storage::archive_quiesce::{
            QUIESCE_HELD_COUNTER, QuiesceWriter, note_quiesce_held, register_quiesce_counters,
        };
        let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, || {
            register_quiesce_counters();
            note_quiesce_held(QuiesceWriter::CatchUp);
        });
        let rendered = handle.render();
        for writer in QuiesceWriter::ALL {
            let label = format!("writer=\"{}\"", writer.label());
            let line = rendered
                .lines()
                .find(|l| l.starts_with(&format!("{QUIESCE_HELD_COUNTER}{{")) && l.contains(&label))
                .unwrap_or_else(|| panic!("{label} not seeded; got:\n{rendered}"));
            let expected = if writer == QuiesceWriter::CatchUp {
                " 1"
            } else {
                " 0"
            };
            assert!(line.ends_with(expected), "{line}");
        }
        // The daily loop is the seeding site.
        let src = include_str!("daily_archive_boot.rs");
        let body = src
            .split("async fn run_daily_archive_loop(")
            .nth(1)
            .expect("the loop exists");
        let seed = body.find("register_quiesce_counters();").expect("seeded");
        assert!(
            seed < body.find("loop {").expect("loop"),
            "seeded before the loop"
        );
    }

    // Regression: 2026-10-13 (DA-13d review) — the new outcome label was not
    // seeded at 0. Every reason `pass_verdict` can return, and every other
    // outcome, must be in the seeded set.
    #[test]
    fn test_regression_da13d_every_attempt_outcome_is_seeded() {
        let ran = ArchiveRunSummary {
            pass_ran: true,
            ..ArchiveRunSummary::default()
        };
        let summaries = [
            ArchiveRunSummary {
                contended: true,
                ..ArchiveRunSummary::default()
            },
            ArchiveRunSummary::default(),
            ArchiveRunSummary {
                tables_wal_suspended: 1,
                ..ran
            },
            ArchiveRunSummary {
                tables_wal_not_applied: 1,
                ..ran
            },
            ArchiveRunSummary {
                tables_list_failed: 1,
                ..ran
            },
            ArchiveRunSummary { failed: 1, ..ran },
            ArchiveRunSummary {
                partitions_considered: 200,
                ..ran
            },
            ArchiveRunSummary {
                held_deferred_depth: 1,
                ..ran
            },
            ArchiveRunSummary {
                held_quiesce_busy: 1,
                ..ran
            },
            ran,
        ];
        let mut outcomes: Vec<PassOutcome> = summaries
            .iter()
            .map(|s| PassOutcome::Ran(pass_verdict(s, 200)))
            .collect();
        outcomes.push(PassOutcome::Skipped);
        outcomes.push(PassOutcome::Failed);
        for outcome in outcomes {
            assert!(
                SEEDED_ATTEMPT_OUTCOMES.contains(&outcome.as_str()),
                "outcome {} is not seeded at 0",
                outcome.as_str()
            );
        }
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
            spawn_supervised_daily_archive_loop(
                PathBuf::from("data"),
                test_questdb(),
                cfg,
                "15:30:00".to_string(),
                None,
            )
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
                spawn_supervised_daily_archive_loop(
                    PathBuf::from("data"),
                    test_questdb(),
                    enabled_cfg(),
                    bad.to_string(),
                    None,
                )
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
            PathBuf::from("data"),
            test_questdb(),
            enabled_cfg(),
            "15:30:00".to_string(),
            None,
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
