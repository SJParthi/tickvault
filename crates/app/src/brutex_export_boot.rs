//! The daily Brutex export's scheduler (owner Quote 60, 2026-11-14).
//!
//! Polls once a minute on its own tokio task, spawned from the daily archive
//! loop's arm (same QuestDB and retention config, same `archive_enabled`
//! gate). It never exports between 08:30 and 15:45 IST on a trading day, nor
//! on a Muhurat date before its session end plus 6 minutes, and it re-checks
//! that window before every day, table and partition, so a run that starts
//! before 08:30 stops there (nothing sealed, nothing failed) instead of
//! reading on into the session.
//!
//! The SESSION day (before 07:30 the clock still belongs to the previous
//! day) is visited first, then past days of the last [`CATCH_UP_DAYS`]
//! without a DONE, oldest first. The session day runs only once its data is
//! final. The HARD gates ([`HARD_REASONS`]) never run anyway, because each
//! would make the export's object differ from what the archive later copies
//! (a full second copy) and leave Brutex a sealed day missing rows: the seal
//! writer drained and `candles_1m` applied
//! ([`crate::dhan_live_crossverify_boot::live_side_readiness`]), the catch-up
//! sweep sealed past the last candle bucket of the day
//! ([`candle_final_target_secs`]: 16:15 on a normal day, the hourly bar), the
//! arrival-time tables' hour holding the close ended (16:02 on a normal day,
//! `arrival_close_secs`; later hours of `tick_quotes` and `feed_aux_packets`
//! are left out of the manifest and named, because the feed stays connected
//! after the close), this process's after-close contract-name repair ended
//! and no repair run is rewriting the day, the after-close depth
//! pass still able to write skipped depth rows (marks, and the pass not
//! stopped at its disk floor for an hour), and tonight's evening WAL drain
//! still due to replay the frames shed during the session (2026-11-14
//! review: that replay now runs BEFORE the export, never after it). The
//! SOFT gates (an export table's or the archive audit's WAL not applied, a
//! spill file pending, and for a catch-up day a name repair still to reach
//! it) are waited for until [`EXPORT_SOFT_WAIT_SECS`] after
//! the window opens (18:00 on a normal day); then it runs and the manifest
//! names what was pending. NOTE reasons ([`NOTE_REASON_PREFIXES`]: frames
//! still waiting that no replay will reach tonight, skipped depth the pass
//! cannot write, a name repair that ended with failures) never delay, but go
//! into the manifest, so `complete` is false whenever rows of the day may
//! still be missing. A failed attempt waits [`RETRY_BACKOFF_SECS`] (doubling)
//! before the next; a stop for disk waits [`DISK_STOP_RETRY_SECS`]. From
//! [`EXPORT_GIVE_UP_AFTER_SECS`] after the window opened (20:45 on a normal
//! day) the session day is handled like a catch-up day (no drain hold), with
//! its hard gates still read. A past day runs once nothing delays it, and
//! anyway (its pending reasons in the manifest) once it is
//! [`PAST_DAY_FORCE_AFTER_DAYS`] days old or the oldest of the window, so a
//! day is never left waiting on a gate that never clears (a weekend no
//! longer hides a day's last chance); only a name repair rewriting the day,
//! or due to reach it before the session day's midnight
//! ([`BLOCKS_EVEN_FORCED`]), holds even a forced day back (2026-11-14 third
//! review). A day gives up after [`MAX_ATTEMPTS_PER_DAY`] attempts in one
//! process, with one coded `error!` (STORAGE-GAP-04, `source =
//! "brutex_daily_export"`); a loop that panics books its run as a failed
//! attempt ([`book_panicked_run`]). A session day that leaves the window
//! with no DONE logs the same code with `source =
//! "brutex_daily_export_left_window"` ONCE PER PROCESS (each boot of the
//! following [`LEFT_WINDOW_LOOKBACK_DAYS`] days reports it again), and a day
//! sealed INCOMPLETE `source = "brutex_daily_export_incomplete"`. All are
//! log-only: no CloudWatch filter matches STORAGE-GAP-04 (the data-at-risk
//! alarm sums upload counters, not log lines) and no daily report runs after
//! the export, so until a noise-lock row adds a page the owner learns of a
//! missed export only from the app logs (or Brutex finding no DONE).
//!
//! The box stays up (drain hold) while today's export is due and not done,
//! and while ANY export (a catch-up day included) is running; the one-hour
//! stall rule does not release it then. The evening WAL restart waits only
//! while an export RUNS.
//!
//! The export itself is `tickvault_storage::partition_archive::daily_export`.
//! Cold path: one poll a minute; never the frame drain.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use chrono::NaiveDate;
use tracing::{error, info, warn};

use tickvault_common::config::{PartitionRetentionConfig, QuestDbConfig};
use tickvault_common::error_code::ErrorCode;
use tickvault_common::market_hours::SessionDayInfo;
use tickvault_storage::partition_archive::daily_export::{
    DailyExporter, ExportOptions, ExportOutcome, StopReason, arrival_open_from,
    export_time_left_now, export_window_open_now, ist_now, session_info,
};
pub use tickvault_storage::partition_archive::daily_export::{
    EXPORT_FORBIDDEN_FROM_SECS_IST, EXPORT_OPEN_SECS_IST, MUHURAT_END_GRACE_SECS, export_open_secs,
    export_window_open,
};
use tickvault_trading::candles::TfIndex;

/// How long after the window opens the SOFT gates are waited for (18:00).
pub const EXPORT_SOFT_WAIT_SECS: u32 = 2 * 3600 + 15 * 60;
/// How long after the window opens the session day stops being treated as
/// the session day (20:45): it becomes a catch-up day, without a hold.
pub const EXPORT_GIVE_UP_AFTER_SECS: u32 = 5 * 3600;
/// Attempts per day per process.
pub const MAX_ATTEMPTS_PER_DAY: u32 = 3;
/// Wait after the first failed attempt; doubled after each later one.
pub const RETRY_BACKOFF_SECS: i64 = 15 * 60;
/// Calendar days the catch-up looks back (the session day included).
pub const CATCH_UP_DAYS: i64 = 7;
/// The local done cache's task name (S3 DONE stays the authority).
pub const EXPORT_MARKER_TASK: &str = "brutex_export";
/// Poll interval, seconds.
const POLL_SECS: u64 = 60;
/// Poll interval.
const POLL: Duration = Duration::from_secs(POLL_SECS);
/// How long one live-side readiness check may sample, seconds.
const LIVE_READINESS_SAMPLE_SECS: u64 = 5;
/// Seconds in a day.
const SECS_PER_DAY: u32 = 86_400;

/// Readiness reasons the session day never runs past: each means rows of
/// the day will still be written after the export.
pub const HARD_REASONS: [&str; 7] = [
    "seal_not_drained",
    "candles_not_final",
    "contract_names_unrepaired",
    "contract_names_repair_running",
    "deferred_depth_pass_running",
    "wal_backlog_replaying",
    "arrival_hours_open",
];

/// Reasons that hold back even a FORCED catch-up day (2026-11-14 third
/// review): a name-repair run is rewriting the day right now (always), or a
/// run still to come will reach it tonight (until the session day's
/// midnight; after it the next run is a day away, so a forced day runs and
/// names it). Exact labels.
pub const BLOCKS_EVEN_FORCED: [&str; 2] = [
    "contract_names_repair_running",
    "contract_names_repair_pending",
];

/// Reasons that never delay an export but always go into its manifest
/// (`complete` false): rows of the day may be missing, and waiting tonight
/// will not bring them. Matched as prefixes.
pub const NOTE_REASON_PREFIXES: [&str; 4] = [
    "wal_backlog:",
    "wal_backlog_unreadable",
    "deferred_depth_marks_floor_blocked",
    "contract_names_repair_failed",
];

/// A past day not ready runs anyway, its reasons in the manifest, once it
/// is this many calendar days before the session day (2026-11-14 review):
/// one more evening of waiting, then sealed honestly incomplete.
pub const PAST_DAY_FORCE_AFTER_DAYS: i64 = 2;

/// How far past the window the "left the window without DONE" check looks,
/// calendar days (a box off for a week still reports each day once).
pub const LEFT_WINDOW_LOOKBACK_DAYS: i64 = 7;

/// Wait after an attempt stopped for disk before the next (not counted).
pub const DISK_STOP_RETRY_SECS: i64 = 15 * 60;

/// The first day the export exists (owner Quote 60): no earlier day is ever
/// reported as having left the window.
#[must_use]
pub fn first_export_day() -> NaiveDate {
    NaiveDate::from_ymd_opt(2026, 11, 14).unwrap_or_default()
}

/// Today's export is due and not sealed or given up, or an export is
/// running: the drain hold keeps the box up for it. A store a minute.
static EXPORT_PENDING: AtomicBool = AtomicBool::new(false);

/// An export is running right now: the evening WAL restart waits for it.
static EXPORT_RUNNING: AtomicBool = AtomicBool::new(false);

/// Whether an export is running right now (owner Quote 60). O(1).
#[must_use]
// TEST-EXEMPT: a load of the flag the loop stores around each run; pinned by test_the_loop_marks_a_run_and_reads_every_gate
pub fn export_running() -> bool {
    EXPORT_RUNNING.load(Ordering::Relaxed)
}

/// Whether the drain hold must keep the box up for the Brutex export (owner
/// Quote 60). O(1).
#[must_use]
pub fn export_pending() -> bool {
    EXPORT_PENDING.load(Ordering::Relaxed)
}

/// Test hook for the drain hold's wiring test.
#[cfg(test)]
// TEST-EXEMPT: covered by test_measure_day_work_reads_the_export_pending_flag
pub(crate) fn set_export_pending_for_test(pending: bool) {
    EXPORT_PENDING.store(pending, Ordering::Relaxed);
}

/// The session day `now` belongs to, and its seconds (past 86,400 after
/// midnight): before 07:30 the clock belongs to the previous day, the
/// daily archive's rule. Pure, O(1).
#[must_use]
// TEST-EXEMPT: covered by test_session_day_before_0730_is_previous_day
pub fn session_day_of(date: NaiveDate, secs: u32) -> (NaiveDate, u32) {
    crate::daily_archive_boot::archive_session_day(date, secs)
}

/// The latest natural candle bucket end of a day with `info`: over every
/// timeframe, the end of the bucket holding the session's last second (the
/// 15:40 ingest end on a trading day, the Muhurat session end on a Muhurat
/// date). 16:15:00 on a normal day (the hourly 15:15 bar). `None` on a day
/// with no session. Pure, O(timeframes).
#[must_use]
// TEST-EXEMPT: covered by test_candle_final_target_is_the_hourly_bar_end
pub fn candle_final_target_secs(info: SessionDayInfo) -> Option<u32> {
    let trading_end = info
        .trading
        .then_some(tickvault_common::constants::TICK_PERSIST_END_SECS_OF_DAY_IST);
    let end = trading_end.max(info.muhurat_end_secs)?;
    let last = end.saturating_sub(1);
    TfIndex::ALL
        .iter()
        .map(|tf| tf.bucket_end(tf.bucket_start(last)))
        .max()
}

/// Whether the live catch-up sweep sealed every candle bucket of the day:
/// `progress` is `CATCHUP_PROGRESS` (fold seconds of the swept floor, and
/// the sweep's finish), `day_start_fold_secs` the IST midnight in fold
/// seconds. No progress means no live lane folded in this process (a
/// predecessor's exit seal or a mid-session boot's hand-over sealed the
/// day). Pure, O(1).
#[must_use]
// TEST-EXEMPT: covered by test_candle_final_target_is_the_hourly_bar_end
pub fn candles_final(progress: Option<(u32, u32)>, day_start_fold_secs: i64, target: u32) -> bool {
    progress.is_none_or(|(floor, _)| {
        i64::from(floor).saturating_sub(day_start_fold_secs) >= i64::from(target)
    })
}

/// What to do about one day now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DayStep {
    /// The day's window has not opened.
    NotDue,
    /// Due, but the data is not final yet, or a failed attempt's backoff runs.
    Wait,
    /// Export now.
    Run,
    /// Out of attempts or time.
    GiveUp,
}

/// Per-day state in this process.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct DayState {
    /// Attempts that ran to an outcome (a stopped attempt is not counted).
    pub attempts: u32,
    /// Unix seconds before which no new attempt starts (backoff).
    pub retry_at_unix: i64,
    /// Sealed (DONE written or found).
    pub sealed: bool,
    /// Given up in this process.
    pub gave_up: bool,
}

/// The backoff after the `attempts`-th failed attempt: 15, 30, 60 minutes...
/// Pure, O(1).
#[must_use]
// TEST-EXEMPT: covered by test_failed_attempt_backs_off_before_the_next
pub fn retry_backoff_secs(attempts: u32) -> i64 {
    RETRY_BACKOFF_SECS.saturating_mul(1_i64 << attempts.saturating_sub(1).min(8))
}

/// Whether `reasons` holds a gate the session day never runs past. Pure.
#[must_use]
// TEST-EXEMPT: covered by test_hard_reasons_are_exact_labels
pub fn has_hard_reason(reasons: &[String]) -> bool {
    reasons.iter().any(|r| HARD_REASONS.contains(&r.as_str()))
}

/// Whether `reason` delays an export (every reason but a NOTE). Pure.
#[must_use]
pub fn delays(reason: &str) -> bool {
    !NOTE_REASON_PREFIXES.iter().any(|p| reason.starts_with(p))
}

/// Whether nothing in `reasons` delays an export. Pure, O(reasons).
#[must_use]
// TEST-EXEMPT: covered by test_note_reasons_never_delays_but_stay_in_the_manifest
pub fn ready(reasons: &[String]) -> bool {
    !reasons.iter().any(|r| delays(r))
}

/// Whether `reasons` hold a FORCED catch-up day back (2026-11-14 third
/// review): a repair run rewriting it now, always; a repair still to come
/// tonight, until `after_midnight` (the clock past the session day's
/// midnight). Pure, O(reasons).
#[must_use]
pub fn blocks_even_forced(reasons: &[String], after_midnight: bool) -> bool {
    reasons
        .iter()
        .any(|r| r == BLOCKS_EVEN_FORCED[0] || (!after_midnight && r == BLOCKS_EVEN_FORCED[1]))
}

/// What the scheduler read about the gates outside the export tables.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Gates {
    /// The live side's seals drained and applied; `None` = not read (a past
    /// day: this process's live side says nothing about it).
    pub seal_applied: Option<bool>,
    /// The catch-up sweep passed the day's last candle bucket; `None` = not
    /// read (a past day).
    pub candles_final: Option<bool>,
    /// The name repair's state for the day, and whether the clock is still
    /// on the session day's own date (a past day: `false`, and the state
    /// counts a later run whose lookback reaches it); `None` = not read.
    pub repair: Option<(crate::contract_name_repair_boot::RepairDayState, bool)>,
    /// A repair run in progress can rewrite the day right now (2026-11-14
    /// third review): it takes the same shared quiesce side as the export.
    pub repair_running: bool,
    /// The session day's arrival-time hours that hold session rows have
    /// not all ended yet (`arrival_close_secs`); `None` = not read (a past
    /// day, or the session day after midnight).
    pub arrival_hours_open: Option<bool>,
    /// Frames waiting in the WAL backlog; `None` = unreadable.
    pub wal_backlog: Option<u64>,
    /// Tonight's evening WAL drain will still replay them.
    pub evening_replay_pending: bool,
    /// Every after-close depth pass has stopped at its disk floor for an
    /// hour (the drain hold's cap): skipped depth will not be written now.
    pub depth_pass_floor_blocked: bool,
}

/// The day's reasons: the exporter's `base` (WAL applied, spill folders,
/// skipped-depth marks) plus `gates`, each labelled HARD, soft or NOTE.
/// Pure, O(reasons).
#[must_use]
pub fn compose_reasons(base: Vec<String>, gates: Gates) -> Vec<String> {
    use crate::contract_name_repair_boot::RepairDayState;
    let mut out: Vec<String> = Vec::with_capacity(base.len() + 4);
    for reason in base {
        if reason == "deferred_depth_marks" {
            out.push(if gates.depth_pass_floor_blocked {
                "deferred_depth_marks_floor_blocked".to_string()
            } else {
                "deferred_depth_pass_running".to_string()
            });
        } else {
            out.push(reason);
        }
    }
    if gates.seal_applied == Some(false) {
        out.push("seal_not_drained".to_string());
    }
    if gates.candles_final == Some(false) {
        out.push("candles_not_final".to_string());
    }
    if gates.repair_running {
        out.push("contract_names_repair_running".to_string());
    }
    if gates.arrival_hours_open == Some(true) {
        out.push("arrival_hours_open".to_string());
    }
    match gates.repair {
        Some((RepairDayState::Pending, true)) => out.push("contract_names_unrepaired".to_string()),
        // Not the session date (a catch-up day, or the session day after
        // midnight): a run still to come may rewrite it, so it waits
        // (2026-11-14 third review: it was a NOTE and the day was sealed
        // with blank names that the next run then rewrote).
        Some((RepairDayState::Pending, false)) => {
            out.push("contract_names_repair_pending".to_string());
        }
        Some((RepairDayState::GaveUpWithFailures, _)) => {
            out.push("contract_names_repair_failed".to_string());
        }
        Some((RepairDayState::Clean, _)) | None => {}
    }
    match gates.wal_backlog {
        Some(0) => {}
        Some(_) if gates.evening_replay_pending => out.push("wal_backlog_replaying".to_string()),
        Some(n) => out.push(format!("wal_backlog:{n}")),
        None => out.push("wal_backlog_unreadable".to_string()),
    }
    out.sort();
    out.dedup();
    out
}

/// Whether a past `day` runs whatever is pending: the oldest day of the
/// window, or [`PAST_DAY_FORCE_AFTER_DAYS`] days or more before the session
/// day. Pure, O(1).
#[must_use]
pub fn past_day_forced(day: NaiveDate, session_day: NaiveDate) -> bool {
    Some(day) == oldest_window_day(session_day)
        || session_day.signed_duration_since(day).num_days() >= PAST_DAY_FORCE_AFTER_DAYS
}

/// The session days that have just left (or long left) the catch-up window
/// of `session_day`, from [`first_export_day`] on: each is checked once per
/// process for a DONE, and one without is reported. Pure, O(days).
#[must_use]
pub fn days_leaving_window(session_day: NaiveDate) -> Vec<NaiveDate> {
    let first = first_export_day();
    (CATCH_UP_DAYS..CATCH_UP_DAYS + LEFT_WINDOW_LOOKBACK_DAYS)
        .filter_map(|back| session_day.checked_sub_signed(chrono::TimeDelta::days(back)))
        .filter(|d| *d >= first && export_open_secs(session_info(*d)).is_some())
        .collect()
}

/// How one run ended, for the per-day bookkeeping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunEnd {
    /// DONE written or found.
    Sealed,
    /// Stopped by the closing window: not an attempt.
    StoppedWindow,
    /// Stopped for disk: not an attempt; wait [`DISK_STOP_RETRY_SECS`].
    StoppedDisk,
    /// Failed: an attempt; wait the backoff.
    Failed,
}

/// Applies one run's end to `state` (whose `attempts` the run already
/// counted) at `now_unix`. Pure, O(1).
pub fn apply_run_end(state: &mut DayState, end: RunEnd, now_unix: i64) {
    match end {
        RunEnd::Sealed => state.sealed = true,
        RunEnd::StoppedWindow => state.attempts = state.attempts.saturating_sub(1),
        RunEnd::StoppedDisk => {
            state.attempts = state.attempts.saturating_sub(1);
            state.retry_at_unix = now_unix.saturating_add(DISK_STOP_RETRY_SECS);
        }
        RunEnd::Failed => {
            state.retry_at_unix = now_unix.saturating_add(retry_backoff_secs(state.attempts));
        }
    }
}

/// The step for the SESSION day at `secs` (seconds since its midnight, past
/// 86,400 after midnight) with its window at `open`. Pure, O(reasons).
#[must_use]
pub fn session_day_step(
    secs: u32,
    open: u32,
    state: DayState,
    now_unix: i64,
    reasons: &[String],
) -> DayStep {
    if secs < open {
        return DayStep::NotDue;
    }
    if state.attempts >= MAX_ATTEMPTS_PER_DAY {
        return DayStep::GiveUp;
    }
    if now_unix < state.retry_at_unix || has_hard_reason(reasons) {
        return DayStep::Wait;
    }
    if ready(reasons) || secs >= open.saturating_add(EXPORT_SOFT_WAIT_SECS) {
        DayStep::Run
    } else {
        DayStep::Wait
    }
}

/// Whether the session day is still handled AS the session day at `secs`:
/// until [`EXPORT_GIVE_UP_AFTER_SECS`] after its window opened (20:45 on a
/// normal day). Past that it is handled like a catch-up day: no drain hold
/// and no give-up for time (a process that boots after midnight never gives
/// up a day it never tried). Pure, O(1).
#[must_use]
// TEST-EXEMPT: covered by test_session_day_step_hard_gates_never_run_anyway
pub fn within_session_horizon(secs: u32, open: u32) -> bool {
    secs < open.saturating_add(EXPORT_GIVE_UP_AFTER_SECS)
}

/// The step for a PAST day (catch-up, or the session day past its
/// horizon): it runs once nothing delays it, or once `forced`
/// ([`past_day_forced`]) whatever is pending, unless `blocked`
/// ([`blocks_even_forced`]: a name repair rewriting it, or due to). Pure,
/// O(1).
#[must_use]
// TEST-EXEMPT: covered by test_past_day_waits_for_ready_until_its_last_chance
pub fn past_day_step(
    state: DayState,
    now_unix: i64,
    ready: bool,
    forced: bool,
    blocked: bool,
) -> DayStep {
    if state.attempts >= MAX_ATTEMPTS_PER_DAY {
        return DayStep::GiveUp;
    }
    if now_unix < state.retry_at_unix || blocked {
        return DayStep::Wait;
    }
    if ready || forced {
        DayStep::Run
    } else {
        DayStep::Wait
    }
}

/// The days visited for `session_day`: the session day FIRST (so a catch-up
/// backlog never runs ahead of today's export or leaves the drain hold
/// unaware of it), then the earlier days of the window, oldest first. Pure.
#[must_use]
pub fn visit_order(session_day: NaiveDate) -> Vec<NaiveDate> {
    let mut days = vec![session_day];
    days.extend(
        (1..CATCH_UP_DAYS)
            .rev()
            .filter_map(|back| session_day.checked_sub_signed(chrono::TimeDelta::days(back))),
    );
    days
}

/// The oldest day of the window for `session_day`: its last chance. Pure.
#[must_use]
// TEST-EXEMPT: covered by test_visit_order_is_session_day_first_then_oldest_first
pub fn oldest_window_day(session_day: NaiveDate) -> Option<NaiveDate> {
    session_day.checked_sub_signed(chrono::TimeDelta::days(CATCH_UP_DAYS - 1))
}

/// What survives a panic of the export loop (2026-11-14 third review): the
/// per-day state, and the day an export is running for with its state (its
/// attempt already counted). Shared between the supervising task and each
/// spawned loop; a `std::sync::Mutex` locked for a copy only, never across
/// an await.
#[derive(Debug, Default)]
pub struct LoopCarry {
    /// Per-day state, re-seeded into the next loop after a panic.
    pub days: HashMap<NaiveDate, DayState>,
    /// The day of the run in flight, with its state.
    pub running: Option<(NaiveDate, DayState)>,
}

/// After a panic of the export loop: books the run in flight as a FAILED
/// attempt (its backoff, and it counts toward [`MAX_ATTEMPTS_PER_DAY`], so a
/// panic that repeats gives the day up instead of re-running the same
/// export every minute). Before, the respawn started with a fresh state and
/// the running flag stuck. O(1). (Release builds abort on panic, so this
/// covers debug and test builds; the flags are also cleared by the drop
/// guard.)
pub fn book_panicked_run(carry: &mut LoopCarry, now_unix: i64) {
    if let Some((day, mut state)) = carry.running.take() {
        apply_run_end(&mut state, RunEnd::Failed, now_unix);
        carry.days.insert(day, state);
    }
}

/// Locks the carry, whatever an earlier panic left in it.
fn lock_carry(carry: &std::sync::Mutex<LoopCarry>) -> std::sync::MutexGuard<'_, LoopCarry> {
    carry
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Sets [`EXPORT_RUNNING`] for one run and clears it when dropped, a panic
/// unwinding through `export_day` included (2026-11-14 third review: a
/// stuck flag held the evening WAL restart back for the night).
struct RunningFlag;

impl RunningFlag {
    fn set() -> Self {
        EXPORT_RUNNING.store(true, Ordering::Relaxed);
        Self
    }
}

impl Drop for RunningFlag {
    fn drop(&mut self) {
        EXPORT_RUNNING.store(false, Ordering::Relaxed);
    }
}

/// The free bytes the export keeps on a volume of `total` bytes: the
/// after-close depth pass's floor, and the disk-pressure line (2026-11-14
/// third review: between the two the export held the quiesce lock the
/// pressure archiver needed to free the disk). O(1).
#[must_use]
pub fn export_floor_bytes(total: u64, pressure_high_water_pct: u8) -> u64 {
    crate::dhan_feed_stack::deferred_depth_pass::after_close_floor_bytes(total).max(
        tickvault_storage::partition_archive::daily_export::pressure_free_floor_bytes(
            total,
            pressure_high_water_pct,
        ),
    )
}

/// Arms the export loop when the switch is on. Called from the daily
/// archive loop's arm, which already checked `archive_enabled`.
// TEST-EXEMPT: covered by test_spawn_brutex_export_loop_is_off_when_disabled and test_the_loop_marks_a_run_and_reads_every_gate (pins the flags' false start and the panic arm)
pub fn spawn_brutex_export_loop(
    questdb: QuestDbConfig,
    cfg: PartitionRetentionConfig,
) -> Option<tokio::task::JoinHandle<()>> {
    if !cfg.brutex_export_enabled {
        info!("brutex daily export disabled by config");
        return None;
    }
    tickvault_storage::partition_archive::daily_export::register_brutex_export_metrics();
    info!("brutex daily export ARMED (after 15:45 IST on session days)");
    let carry = std::sync::Arc::new(std::sync::Mutex::new(LoopCarry::default()));
    Some(tokio::spawn(async move {
        loop {
            let q = questdb.clone();
            let c = cfg.clone();
            let shared = std::sync::Arc::clone(&carry);
            let handle = tokio::spawn(async move { run_export_loop(q, c, shared).await });
            match handle.await {
                Ok(()) => return,
                Err(err) => {
                    EXPORT_PENDING.store(false, Ordering::Relaxed);
                    EXPORT_RUNNING.store(false, Ordering::Relaxed);
                    book_panicked_run(&mut lock_carry(&carry), chrono::Utc::now().timestamp());
                    error!(
                        ?err,
                        code = ErrorCode::StorageGap04S3ArchiveFailed.code_str(),
                        source = "brutex_daily_export",
                        "brutex daily export loop PANICKED; the run in flight is booked as a \
                         failed attempt; respawning"
                    );
                    tokio::time::sleep(POLL).await;
                }
            }
        }
    }))
}

/// IST midnight of `day` in fold seconds (IST wall clock read as epoch).
fn day_start_fold_secs(day: NaiveDate) -> i64 {
    day.and_hms_opt(0, 0, 0)
        .map_or(0, |dt| dt.and_utc().timestamp())
}

/// The gates every day reads: the WAL backlog (off the worker: a folder
/// scan), the evening replay, the depth pass's floor block, and whether a
/// name-repair run in progress can rewrite `day`. O(waiting segments), once
/// per day visited.
fn common_gates(day: NaiveDate) -> Gates {
    let wal_backlog = tickvault_storage::off_worker::off_worker(|| {
        tickvault_storage::wal_unapplied_ranges::backlog_segments(&crate::boot_helpers::ws_wal_dir())
    });
    let floor_blocked = crate::drain_hold::floor_blocked_secs(
        crate::dhan_feed_stack::deferred_depth_pass::floor_blocked_since_unix(),
        chrono::Utc::now().timestamp(),
    )
    .is_some_and(|secs| secs >= crate::drain_hold::DRAIN_HOLD_FLOOR_BLOCKED_CAP_SECS);
    Gates {
        seal_applied: None,
        candles_final: None,
        repair: None,
        repair_running: tickvault_storage::contract_name_repair::repair_run_covers_day(day),
        arrival_hours_open: None,
        wal_backlog,
        evening_replay_pending: crate::drain_hold::evening_replay_pending(),
        depth_pass_floor_blocked: floor_blocked,
    }
}

/// Readiness of the session day (also past its horizon): the export
/// tables, the live side, the candle close, the arrival-time hours, the
/// contract-name repair, the WAL backlog and the skipped depth. `ssecs`:
/// seconds since the session day's midnight (past 86,400 after it). On the
/// session date a pending repair is a hard gate; after midnight this process
/// can no longer run that day's repair, so it waits (soft) for the next run
/// that reaches the day.
async fn today_reasons(
    exporter: &DailyExporter,
    exec_url: &str,
    day: NaiveDate,
    ssecs: u32,
) -> Vec<String> {
    let same_date = ssecs < SECS_PER_DAY;
    let base = exporter.pending_reasons().await;
    let day_start = day_start_fold_secs(day).saturating_mul(1_000_000_000);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(LIVE_READINESS_SAMPLE_SECS);
    let live =
        crate::dhan_live_crossverify_boot::live_side_readiness(exec_url, day, day_start, deadline)
            .await;
    let target = candle_final_target_secs(session_info(day)).unwrap_or(0);
    let gates = Gates {
        seal_applied: Some(matches!(
            live.durability,
            crate::dhan_live_crossverify_boot::Durability::Applied
        )),
        candles_final: Some(candles_final(
            crate::dhan_feed_stack::CATCHUP_PROGRESS.load(),
            day_start_fold_secs(day),
            target,
        )),
        repair: Some((
            crate::contract_name_repair_boot::repair_state_for(day),
            same_date,
        )),
        arrival_hours_open: same_date.then(|| {
            tickvault_storage::partition_archive::daily_export::arrival_close_secs(session_info(
                day,
            ))
            .is_some_and(|close| ssecs < close)
        }),
        ..common_gates(day)
    };
    compose_reasons(base, gates)
}

/// Readiness of a past day: the export tables, the spill folders, the WAL
/// backlog, the skipped depth, and the name repair that reaches it (this
/// process's live side says nothing about an earlier day).
async fn past_reasons(exporter: &DailyExporter, day: NaiveDate) -> Vec<String> {
    let gates = Gates {
        repair: Some((
            crate::contract_name_repair_boot::repair_covered_state(day),
            false,
        )),
        ..common_gates(day)
    };
    compose_reasons(exporter.pending_reasons().await, gates)
}

/// Reports, once per process, every session day of the last
/// [`LEFT_WINDOW_LOOKBACK_DAYS`] past the window that has no DONE. `true`
/// when every day could be checked (a failed HEAD is retried next poll).
async fn report_days_left_window(
    ex: &DailyExporter,
    session_day: NaiveDate,
    reported: &mut std::collections::HashSet<NaiveDate>,
) -> bool {
    let mut all_checked = true;
    for day in days_leaving_window(session_day) {
        if reported.contains(&day) {
            continue;
        }
        match ex.is_sealed(day).await {
            Ok(true) => {
                reported.insert(day);
            }
            Ok(false) => {
                reported.insert(day);
                metrics::counter!("tv_brutex_export_runs_total", "outcome" => "left_window")
                    .increment(1);
                error!(
                    code = ErrorCode::StorageGap04S3ArchiveFailed.code_str(),
                    source = "brutex_daily_export_left_window",
                    %day,
                    "brutex daily export: this session day left the 7-day catch-up window \
                     with NO DONE, so Brutex never gets it from the export. Nothing was \
                     deleted; its partitions reach S3 through the archive as usual"
                );
            }
            Err(err) => {
                all_checked = false;
                warn!(%day, error = %format!("{err:#}"), "brutex export: DONE check failed");
            }
        }
    }
    all_checked
}

async fn run_export_loop(
    questdb: QuestDbConfig,
    cfg: PartitionRetentionConfig,
    carry: std::sync::Arc<std::sync::Mutex<LoopCarry>>,
) {
    let exec_url = format!("http://{}:{}/exec", questdb.host, questdb.http_port);
    let high_water = cfg.pressure_high_water_pct;
    let floor = move |total: u64| export_floor_bytes(total, high_water);
    let mut exporter: Option<DailyExporter> = None;
    // A respawn after a panic starts from the booked state (the panicked
    // run counted as a failed attempt), never from scratch.
    let mut days: HashMap<NaiveDate, DayState> = std::mem::take(&mut lock_carry(&carry).days);
    let mut left_checked: Option<NaiveDate> = None;
    let mut left_reported: std::collections::HashSet<NaiveDate> = std::collections::HashSet::new();
    let mut ticker = tokio::time::interval(POLL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        ticker.tick().await;
        let (date, secs) = ist_now();
        if !export_window_open(session_info(date), secs) {
            EXPORT_PENDING.store(false, Ordering::Relaxed);
            continue;
        }
        if exporter.is_none() {
            match DailyExporter::new(&questdb, &cfg).await {
                Ok(Some(e)) => exporter = Some(e),
                Ok(None) => {
                    info!("brutex daily export: no cold bucket resolves; not exporting");
                    return;
                }
                Err(err) => {
                    warn!(error = %format!("{err:#}"), "brutex daily export: client not built yet");
                    continue;
                }
            }
        }
        let Some(ex) = exporter.as_mut() else {
            continue;
        };
        let (session_day, ssecs) = session_day_of(date, secs);
        days.retain(|d, _| *d >= session_day - chrono::TimeDelta::days(CATCH_UP_DAYS));
        if left_checked != Some(session_day)
            && report_days_left_window(ex, session_day, &mut left_reported).await
        {
            left_checked = Some(session_day);
        }
        let mut session_pending = false;
        for day in visit_order(session_day) {
            // Re-checked before every day: a catch-up run never starts a
            // day once the trading-day window has closed on it.
            if !export_window_open_now() {
                break;
            }
            let Some(open) = export_open_secs(session_info(day)) else {
                continue;
            };
            let is_today = day == session_day;
            if is_today && ssecs < open {
                continue;
            }
            // Past the horizon the session day is a catch-up day (no hold).
            let is_session_day = is_today && within_session_horizon(ssecs, open);
            let state = days.entry(day).or_default();
            if state.sealed || state.gave_up {
                continue;
            }
            if crate::daily_task_marker::daily_marker_exists(EXPORT_MARKER_TASK, day) {
                state.sealed = true;
                continue;
            }
            match ex.is_sealed(day).await {
                Ok(true) => {
                    state.sealed = true;
                    if let Err(err) =
                        crate::daily_task_marker::try_write_daily_marker(EXPORT_MARKER_TASK, day)
                    {
                        warn!(%day, error = %err, "brutex export: local done cache not written");
                    }
                    continue;
                }
                Ok(false) => {}
                Err(err) => {
                    warn!(%day, error = %format!("{err:#}"), "brutex export: DONE check failed");
                    session_pending |= is_session_day;
                    continue;
                }
            }
            let now_unix = chrono::Utc::now().timestamp();
            // The session day past its horizon keeps its hard gates (it is
            // never forced: it becomes a past day tomorrow).
            let forced = !is_today && past_day_forced(day, session_day);
            let reasons = if is_today {
                today_reasons(ex, &exec_url, day, ssecs).await
            } else {
                past_reasons(ex, day).await
            };
            let step = if is_session_day {
                session_day_step(ssecs, open, *state, now_unix, &reasons)
            } else {
                let blocked = blocks_even_forced(&reasons, ssecs >= SECS_PER_DAY);
                past_day_step(*state, now_unix, ready(&reasons), forced, blocked)
            };
            match step {
                DayStep::NotDue => {}
                DayStep::Wait => session_pending |= is_session_day,
                DayStep::GiveUp => {
                    state.gave_up = true;
                    metrics::counter!("tv_brutex_export_runs_total", "outcome" => "gave_up")
                        .increment(1);
                    error!(
                        code = ErrorCode::StorageGap04S3ArchiveFailed.code_str(),
                        source = "brutex_daily_export",
                        %day,
                        attempts = state.attempts,
                        reasons = ?reasons,
                        "brutex daily export GAVE UP for this day: no DONE was written, so \
                         Brutex sees the day as not exported. Nothing was deleted; the next \
                         process's catch-up tries again within 7 days"
                    );
                }
                DayStep::Run => {
                    state.attempts = state.attempts.saturating_add(1);
                    // What a panic inside the run would leave: booked as a
                    // failed attempt by the supervisor (2026-11-14 third review).
                    {
                        let mut c = lock_carry(&carry);
                        c.days = days.clone();
                        c.running = days.get(&day).map(|s| (day, *s));
                    }
                    // Hold the box while ANY export runs, a catch-up day too,
                    // and hold the evening WAL restart back while it runs.
                    EXPORT_PENDING.store(true, Ordering::Relaxed);
                    let running = RunningFlag::set();
                    let keep_going = export_window_open_now;
                    let time_left = export_time_left_now;
                    let (now_date, now_secs) = ist_now();
                    let opts = ExportOptions {
                        keep_going: &keep_going,
                        time_left: &time_left,
                        floor_for_total: &floor,
                        open_from: arrival_open_from(day, now_date, now_secs),
                        last_chance: forced,
                    };
                    let outcome = ex.export_day(day, reasons, opts).await;
                    drop(running);
                    lock_carry(&carry).running = None;
                    let state = days.entry(day).or_default();
                    let now_unix = chrono::Utc::now().timestamp();
                    match outcome {
                        Ok(ExportOutcome::Sealed(report)) => {
                            apply_run_end(state, RunEnd::Sealed, now_unix);
                            if let Err(err) = crate::daily_task_marker::try_write_daily_marker(
                                EXPORT_MARKER_TASK,
                                day,
                            ) {
                                warn!(%day, error = %err, "brutex export: local done cache not written");
                            }
                            if report.complete {
                                info!(%day, attempt = report.attempt, rows = report.rows,
                                    "brutex daily export sealed");
                            } else {
                                error!(
                                    code = ErrorCode::StorageGap04S3ArchiveFailed.code_str(),
                                    source = "brutex_daily_export_incomplete",
                                    %day,
                                    attempt = report.attempt,
                                    reasons = ?report.incomplete_reasons,
                                    "brutex daily export sealed INCOMPLETE: DONE is final, so \
                                     Brutex reads this day with the gaps the manifest names \
                                     (later copies appear only as amend records)"
                                );
                            }
                        }
                        Ok(ExportOutcome::Stopped(StopReason::Window)) => {
                            // The window closed mid-run: not an attempt.
                            apply_run_end(state, RunEnd::StoppedWindow, now_unix);
                        }
                        Ok(ExportOutcome::Stopped(StopReason::Disk)) => {
                            // Too little disk: not an attempt; wait.
                            apply_run_end(state, RunEnd::StoppedDisk, now_unix);
                        }
                        Err(err) => {
                            apply_run_end(state, RunEnd::Failed, now_unix);
                            warn!(
                                code = ErrorCode::StorageGap04S3ArchiveFailed.code_str(),
                                %day,
                                attempt = state.attempts,
                                retry_in_secs = retry_backoff_secs(state.attempts),
                                error = %format!("{err:#}"),
                                "brutex daily export attempt failed (no DONE written; retried \
                                 after the backoff)"
                            );
                        }
                    }
                    if is_session_day && !(state.sealed || state.gave_up) {
                        session_pending = true;
                    }
                    EXPORT_PENDING.store(session_pending, Ordering::Relaxed);
                }
            }
            if is_session_day {
                EXPORT_PENDING.store(session_pending, Ordering::Relaxed);
            }
        }
        EXPORT_PENDING.store(session_pending, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TRADING: SessionDayInfo = SessionDayInfo {
        trading: true,
        muhurat_end_secs: None,
    };
    const MUHURAT: SessionDayInfo = SessionDayInfo {
        trading: false,
        muhurat_end_secs: Some(19 * 3600 + 15 * 60),
    };

    fn reasons(r: &[&str]) -> Vec<String> {
        r.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn test_muhurat_export_waits_for_session_end() {
        let end = 19 * 3600 + 15 * 60;
        let open = end + MUHURAT_END_GRACE_SECS;
        assert_eq!(export_open_secs(MUHURAT), Some(open));
        let st = DayState::default();
        assert_eq!(
            session_day_step(open - 1, open, st, 0, &[]),
            DayStep::NotDue
        );
        assert_eq!(session_day_step(open, open, st, 0, &[]), DayStep::Run);
    }

    #[test]
    fn test_session_day_before_0730_is_previous_day() {
        let d = NaiveDate::from_ymd_opt(2026, 11, 14).expect("date");
        let (day, secs) = session_day_of(d, 3600);
        assert_eq!(day, d.pred_opt().expect("prev"));
        assert_eq!(secs, 86_400 + 3600);
        assert_eq!(session_day_of(d, 8 * 3600), (d, 8 * 3600));
    }

    /// Regression (2026-11-14 review, HIGH x2 + MEDIUM): the session day
    /// never runs past a hard gate (it would seal a day whose candles or
    /// names still change and make the archive write a second copy); soft
    /// gates are waited for until 18:00; give-up is 20:45.
    #[test]
    fn test_session_day_step_hard_gates_never_run_anyway() {
        let open = EXPORT_OPEN_SECS_IST;
        let st = DayState::default();
        assert_eq!(
            session_day_step(open - 1, open, st, 0, &[]),
            DayStep::NotDue
        );
        assert_eq!(session_day_step(open, open, st, 0, &[]), DayStep::Run);
        let soft = reasons(&["deferred_depth_marks", "spill_pending:ticks"]);
        assert_eq!(session_day_step(open, open, st, 0, &soft), DayStep::Wait);
        let soft_deadline = 18 * 3600;
        assert_eq!(open + EXPORT_SOFT_WAIT_SECS, soft_deadline);
        assert_eq!(
            session_day_step(soft_deadline - 1, open, st, 0, &soft),
            DayStep::Wait
        );
        assert_eq!(
            session_day_step(soft_deadline, open, st, 0, &soft),
            DayStep::Run
        );
        for hard in HARD_REASONS {
            let r = reasons(&[hard, "deferred_depth_marks"]);
            assert_eq!(
                session_day_step(soft_deadline + 60, open, st, 0, &r),
                DayStep::Wait,
                "{hard}"
            );
        }
        let give_up = 20 * 3600 + 45 * 60;
        assert_eq!(open + EXPORT_GIVE_UP_AFTER_SECS, give_up);
        assert!(within_session_horizon(give_up - 1, open));
        assert!(
            !within_session_horizon(give_up, open),
            "past 20:45 it is a catch-up day, never a time give-up"
        );
        assert!(!within_session_horizon(SECS_PER_DAY + 3_600, open));
        let spent = DayState {
            attempts: MAX_ATTEMPTS_PER_DAY,
            ..DayState::default()
        };
        assert_eq!(session_day_step(open, open, spent, 0, &[]), DayStep::GiveUp);
    }

    /// Regression (2026-11-14 review, HIGH + MEDIUM): three attempts are not
    /// spent in three minutes; a failed attempt waits 15, then 30 minutes.
    #[test]
    fn test_failed_attempt_backs_off_before_the_next() {
        assert_eq!(retry_backoff_secs(1), 900);
        assert_eq!(retry_backoff_secs(2), 1_800);
        assert_eq!(retry_backoff_secs(3), 3_600);
        let open = EXPORT_OPEN_SECS_IST;
        let st = DayState {
            attempts: 1,
            retry_at_unix: 10_000,
            ..DayState::default()
        };
        assert_eq!(
            session_day_step(open + 60, open, st, 9_999, &[]),
            DayStep::Wait
        );
        assert_eq!(
            session_day_step(open + 60, open, st, 10_000, &[]),
            DayStep::Run
        );
        assert_eq!(past_day_step(st, 9_999, true, true, false), DayStep::Wait);
        assert_eq!(past_day_step(st, 10_000, true, false, false), DayStep::Run);
    }

    #[test]
    fn test_past_day_waits_for_ready_until_its_last_chance() {
        let st = DayState::default();
        assert_eq!(past_day_step(st, 0, false, false, false), DayStep::Wait);
        assert_eq!(past_day_step(st, 0, true, false, false), DayStep::Run);
        assert_eq!(past_day_step(st, 0, false, true, false), DayStep::Run);
        let spent = DayState {
            attempts: MAX_ATTEMPTS_PER_DAY,
            ..st
        };
        assert_eq!(past_day_step(spent, 0, true, true, false), DayStep::GiveUp);
    }

    /// Regression (2026-11-14 third review, MEDIUM): a catch-up day never
    /// runs while a name repair rewrites it, nor (before the session day's
    /// midnight) while a run still to come tonight will reach it, even when
    /// forced; after midnight a forced day runs and names the pending repair.
    #[test]
    fn test_blocks_even_forced_holds_a_repair_even_on_a_forced_past_day() {
        let st = DayState::default();
        let running = reasons(&["contract_names_repair_running"]);
        let pending = reasons(&["contract_names_repair_pending"]);
        assert!(blocks_even_forced(&running, false));
        assert!(blocks_even_forced(&running, true));
        assert!(blocks_even_forced(&pending, false));
        assert!(!blocks_even_forced(&pending, true));
        assert!(!blocks_even_forced(
            &reasons(&["spill_pending:ticks"]),
            false
        ));
        assert_eq!(
            past_day_step(st, 0, false, true, blocks_even_forced(&running, true)),
            DayStep::Wait
        );
        assert_eq!(
            past_day_step(st, 0, false, true, blocks_even_forced(&pending, true)),
            DayStep::Run
        );
        let spent = DayState {
            attempts: MAX_ATTEMPTS_PER_DAY,
            ..st
        };
        assert_eq!(past_day_step(spent, 0, false, true, true), DayStep::GiveUp);
    }

    /// Regression (2026-11-14 third review, MEDIUM x2): a panic of the loop
    /// books the run in flight as a failed attempt (backoff, counted toward
    /// the give-up) and the running flag is cleared by its drop guard.
    #[test]
    fn test_book_panicked_run_counts_the_run_as_failed() {
        let day = NaiveDate::from_ymd_opt(2026, 11, 13).expect("d");
        let mut carry = LoopCarry::default();
        book_panicked_run(&mut carry, 100);
        assert!(carry.days.is_empty(), "nothing in flight, nothing booked");
        for n in 1..=MAX_ATTEMPTS_PER_DAY {
            carry.running = Some((
                day,
                DayState {
                    attempts: n,
                    ..carry.days.get(&day).copied().unwrap_or_default()
                },
            ));
            book_panicked_run(&mut carry, 100);
            assert!(carry.running.is_none());
            let st = carry.days.get(&day).copied().expect("booked");
            assert_eq!(st.attempts, n);
            assert_eq!(st.retry_at_unix, 100 + retry_backoff_secs(n));
        }
        let st = carry.days.get(&day).copied().expect("booked");
        assert_eq!(
            past_day_step(st, i64::MAX, true, true, false),
            DayStep::GiveUp,
            "a repeating panic gives the day up"
        );
        let flag = RunningFlag::set();
        assert!(export_running());
        drop(flag);
        assert!(!export_running());
    }

    /// Regression (2026-11-14 third review, MEDIUM): the export's floor is
    /// the larger of the after-close floor and the disk-pressure line.
    #[test]
    fn test_export_floor_bytes_includes_the_pressure_line() {
        let gb = 1_000_000_000_u64;
        let total = 150 * gb;
        let after_close =
            crate::dhan_feed_stack::deferred_depth_pass::after_close_floor_bytes(total);
        let floor = export_floor_bytes(total, 75);
        assert_eq!(floor, after_close.max(total / 4));
        assert!(floor >= total / 4, "30 GB free at 75% used is pressure");
        assert_eq!(export_floor_bytes(total, 100), after_close);
    }

    /// Regression (2026-11-14 review, HIGH x2): the session day is visited
    /// FIRST, so a catch-up backlog never runs ahead of it.
    #[test]
    fn test_visit_order_is_session_day_first_then_oldest_first() {
        let d = NaiveDate::from_ymd_opt(2026, 11, 14).expect("date");
        let days = visit_order(d);
        assert_eq!(days.len(), 7);
        assert_eq!(days.first(), Some(&d));
        assert_eq!(
            days.get(1),
            Some(&NaiveDate::from_ymd_opt(2026, 11, 8).expect("d"))
        );
        assert_eq!(
            days.last(),
            Some(&NaiveDate::from_ymd_opt(2026, 11, 13).expect("d"))
        );
        assert_eq!(oldest_window_day(d), NaiveDate::from_ymd_opt(2026, 11, 8));
    }

    /// Regression (2026-11-14 review, HIGH): the candles are final only once
    /// the catch-up sweep passed the hourly 15:15 bar's end (16:15), the
    /// latest bucket of a normal day.
    #[test]
    fn test_candle_final_target_is_the_hourly_bar_end() {
        assert_eq!(candle_final_target_secs(TRADING), Some(16 * 3600 + 15 * 60));
        assert_eq!(
            candle_final_target_secs(SessionDayInfo {
                trading: false,
                muhurat_end_secs: None
            }),
            None
        );
        // A Muhurat session ending 19:15 ends an hourly bucket exactly.
        assert_eq!(candle_final_target_secs(MUHURAT), Some(19 * 3600 + 15 * 60));
        let day = 1_000 * 86_400_i64;
        let target = 16 * 3600 + 15 * 60;
        let floor = |sod: i64| Some((u32::try_from(day + sod).expect("u32"), 0));
        assert!(!candles_final(floor(15 * 3600 + 49 * 60), day, target));
        assert!(!candles_final(floor(i64::from(target) - 1), day, target));
        assert!(candles_final(floor(i64::from(target)), day, target));
        assert!(candles_final(None, day, target), "no live lane here");
        assert!(
            !candles_final(floor(-3_600), day, target),
            "yesterday's floor"
        );
    }

    #[test]
    fn test_hard_reasons_are_exact_labels() {
        assert!(has_hard_reason(&reasons(&["candles_not_final"])));
        assert!(!has_hard_reason(&reasons(&["wal_not_applied:ticks"])));
        assert!(!has_hard_reason(&[]));
    }

    #[test]
    fn test_spawn_brutex_export_loop_is_off_when_disabled() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("rt");
        let off = rt.block_on(async {
            spawn_brutex_export_loop(
                QuestDbConfig {
                    host: "127.0.0.1".to_string(),
                    http_port: 9000,
                    pg_port: 8812,
                    ilp_port: 9009,
                },
                PartitionRetentionConfig::default(),
            )
        });
        assert!(off.is_none(), "default config has the export off");
        assert_eq!(
            day_start_fold_secs(NaiveDate::from_ymd_opt(1970, 1, 2).expect("d")),
            86_400
        );
    }

    fn gates() -> Gates {
        Gates {
            seal_applied: Some(true),
            candles_final: Some(true),
            repair: Some((RepairDayState::Clean, true)),
            repair_running: false,
            arrival_hours_open: Some(false),
            wal_backlog: Some(0),
            evening_replay_pending: false,
            depth_pass_floor_blocked: false,
        }
    }

    use crate::contract_name_repair_boot::RepairDayState;

    /// Regression (2026-11-14 review, HIGH x2 + MEDIUM x3): every gate is
    /// read into a reason, and each reason is HARD, soft or a NOTE as
    /// documented: shed frames a replay will still bring, and skipped depth
    /// the pass can still write, never let the session day seal; a backlog
    /// no replay reaches tonight, floor-blocked depth and an exhausted name
    /// repair are named in the manifest (complete false) without waiting.
    #[test]
    fn test_compose_reasons_labels_hard_soft_and_note() {
        assert!(compose_reasons(vec![], gates()).is_empty());
        let r = compose_reasons(
            vec![],
            Gates {
                wal_backlog: Some(12),
                evening_replay_pending: true,
                ..gates()
            },
        );
        assert_eq!(r, reasons(&["wal_backlog_replaying"]));
        assert!(has_hard_reason(&r));
        let r = compose_reasons(
            vec![],
            Gates {
                wal_backlog: Some(12),
                ..gates()
            },
        );
        assert_eq!(r, reasons(&["wal_backlog:12"]));
        assert!(!has_hard_reason(&r) && ready(&r) && !r.is_empty());
        let r = compose_reasons(
            vec![],
            Gates {
                wal_backlog: None,
                ..gates()
            },
        );
        assert_eq!(r, reasons(&["wal_backlog_unreadable"]));
        let marks = reasons(&["deferred_depth_marks"]);
        let r = compose_reasons(marks.clone(), gates());
        assert_eq!(r, reasons(&["deferred_depth_pass_running"]));
        assert!(has_hard_reason(&r));
        let r = compose_reasons(
            marks,
            Gates {
                depth_pass_floor_blocked: true,
                ..gates()
            },
        );
        assert_eq!(r, reasons(&["deferred_depth_marks_floor_blocked"]));
        assert!(ready(&r));
        let repair = |state, same| {
            compose_reasons(
                vec![],
                Gates {
                    repair: Some((state, same)),
                    ..gates()
                },
            )
        };
        assert_eq!(
            repair(RepairDayState::Pending, true),
            reasons(&["contract_names_unrepaired"])
        );
        assert_eq!(
            repair(RepairDayState::Pending, false),
            reasons(&["contract_names_repair_pending"])
        );
        assert!(!ready(&repair(RepairDayState::Pending, false)));
        let r = compose_reasons(
            vec![],
            Gates {
                repair_running: true,
                arrival_hours_open: Some(true),
                ..gates()
            },
        );
        assert_eq!(
            r,
            reasons(&["arrival_hours_open", "contract_names_repair_running"])
        );
        assert!(has_hard_reason(&r));
        assert_eq!(
            repair(RepairDayState::GaveUpWithFailures, true),
            reasons(&["contract_names_repair_failed"])
        );
        let r = compose_reasons(
            vec!["spill_pending:ticks".into()],
            Gates {
                seal_applied: Some(false),
                candles_final: Some(false),
                ..gates()
            },
        );
        assert_eq!(
            r,
            reasons(&[
                "candles_not_final",
                "seal_not_drained",
                "spill_pending:ticks"
            ])
        );
        // A past day reads no live-side gate.
        let past = Gates {
            seal_applied: None,
            candles_final: None,
            repair: None,
            ..gates()
        };
        assert!(compose_reasons(vec![], past).is_empty());
    }

    /// NOTE reasons never delay, but `ready` is about delay only: the
    /// manifest still names them, so the day is sealed `complete: false`.
    #[test]
    fn test_note_reasons_never_delays_but_stay_in_the_manifest() {
        let open = EXPORT_OPEN_SECS_IST;
        let notes = reasons(&[
            "wal_backlog:3",
            "deferred_depth_marks_floor_blocked",
            "contract_names_repair_failed",
            "wal_backlog_unreadable",
        ]);
        assert!(ready(&notes));
        assert_eq!(
            session_day_step(open, open, DayState::default(), 0, &notes),
            DayStep::Run
        );
        assert!(!ready(&reasons(&["spill_pending:ticks"])));
        for hard in HARD_REASONS {
            assert!(delays(hard), "{hard}");
        }
    }

    /// Regression (2026-11-14 review, HIGH): a past day not ready runs anyway
    /// two days on, so a weekend never hides its last chance (the oldest day
    /// of the window was the only forced one, and the box is off then).
    #[test]
    fn test_past_day_forced_two_days_on_and_across_a_weekend() {
        let mon = NaiveDate::from_ymd_opt(2026, 11, 16).expect("d");
        let tue = NaiveDate::from_ymd_opt(2026, 11, 17).expect("d");
        let wed = NaiveDate::from_ymd_opt(2026, 11, 18).expect("d");
        assert!(!past_day_forced(mon, tue), "one more evening of waiting");
        assert!(past_day_forced(mon, wed));
        let fri = NaiveDate::from_ymd_opt(2026, 11, 20).expect("d");
        let next_mon = NaiveDate::from_ymd_opt(2026, 11, 23).expect("d");
        assert!(past_day_forced(fri, next_mon));
        assert!(past_day_forced(
            oldest_window_day(next_mon).expect("d"),
            next_mon
        ));
        let st = DayState::default();
        assert_eq!(
            past_day_step(st, 0, false, past_day_forced(mon, wed), false),
            DayStep::Run
        );
    }

    /// Regression (2026-11-14 review, HIGH): a session day past the window
    /// is checked for a DONE (and reported without one) for a week, never a
    /// day before the export existed and never a closed day.
    #[test]
    fn test_days_leaving_window_start_at_first_export_day() {
        let d = NaiveDate::from_ymd_opt(2026, 12, 9).expect("d");
        let days = days_leaving_window(d);
        let window = visit_order(d);
        for day in &days {
            assert!(!window.contains(day), "{day} is still in the window");
            assert!(*day >= first_export_day());
            assert!(export_open_secs(session_info(*day)).is_some());
            assert!(
                d.signed_duration_since(*day).num_days()
                    < CATCH_UP_DAYS + LEFT_WINDOW_LOOKBACK_DAYS
            );
        }
        assert!(
            days.contains(&NaiveDate::from_ymd_opt(2026, 12, 2).expect("d"))
                || export_open_secs(session_info(
                    NaiveDate::from_ymd_opt(2026, 12, 2).expect("d")
                ))
                .is_none(),
            "the day that just left is checked: {days:?}"
        );
        assert!(days_leaving_window(first_export_day()).is_empty());
    }

    /// Regression (2026-11-14 review, MEDIUM): one run's end is booked as
    /// documented: a stop is never an attempt, a disk stop waits, a failure
    /// backs off.
    #[test]
    fn test_apply_run_end_books_each_end() {
        let ran = DayState {
            attempts: 2,
            ..DayState::default()
        };
        let mut st = ran;
        apply_run_end(&mut st, RunEnd::Sealed, 100);
        assert!(st.sealed);
        let mut st = ran;
        apply_run_end(&mut st, RunEnd::StoppedWindow, 100);
        assert_eq!((st.attempts, st.retry_at_unix), (1, 0));
        let mut st = ran;
        apply_run_end(&mut st, RunEnd::StoppedDisk, 100);
        assert_eq!(
            (st.attempts, st.retry_at_unix),
            (1, 100 + DISK_STOP_RETRY_SECS)
        );
        let mut st = ran;
        apply_run_end(&mut st, RunEnd::Failed, 100);
        assert_eq!(
            (st.attempts, st.retry_at_unix),
            (2, 100 + retry_backoff_secs(2))
        );
    }

    /// Regression (2026-11-14 review, MEDIUM): the archive leg opens after
    /// the archiver's no-sort window ends, so its first pass sorts against
    /// the export's objects (before, it opened at 15:40:02 inside a window
    /// that ran to 15:45 and copied each hour a second time).
    #[test]
    fn test_the_archive_leg_opens_after_the_archiver_sort_window() {
        let leg_open =
            crate::daily_archive_boot::window_open_secs_of_day("15:40:00").expect("open");
        let sort_end =
            tickvault_storage::partition_archive::daily_export::archiver_sort_end_secs(TRADING)
                .expect("end");
        assert!(leg_open >= sort_end, "leg {leg_open} < sort end {sort_end}");
    }

    /// Regression (2026-11-14 review, MEDIUM): the loop's wiring is pinned:
    /// it marks a run for the evening restart, reads every gate, routes a
    /// past day through the forced rule and books each end through
    /// `apply_run_end`.
    #[test]
    fn test_the_loop_marks_a_run_and_reads_every_gate() {
        let src = include_str!("brutex_export_boot.rs");
        // Up to the test module (the loop is the last item before it).
        let body = src
            .split("async fn run_export_loop")
            .nth(1)
            .and_then(|rest| rest.split("#[cfg(test)]").next())
            .expect("loop body");
        for want in [
            "today_reasons(ex, &exec_url, day, ssecs)",
            "past_reasons(ex, day)",
            "past_day_forced(day, session_day)",
            "ready(&reasons)",
            "blocks_even_forced(&reasons, ssecs >= SECS_PER_DAY)",
            "let running = RunningFlag::set();",
            "drop(running);",
            "c.running = days.get(&day)",
            "lock_carry(&carry).running = None;",
            "std::mem::take(&mut lock_carry(&carry).days)",
            "time_left: &time_left",
            "floor_for_total: &floor",
            "export_floor_bytes(total, high_water)",
            "open_from: arrival_open_from(day, now_date, now_secs)",
            "last_chance: forced",
            "RunEnd::StoppedDisk",
            "RunEnd::Failed",
            "report_days_left_window(",
        ] {
            assert!(body.contains(want), "run_export_loop lost `{want}`");
        }
        // 2026-11-14 third review: the pending flag (which holds the box)
        // is set true only for a run, never before the Run arm.
        let run_arm = body.find("DayStep::Run =>").expect("run arm");
        let first_true = body
            .find("EXPORT_PENDING.store(true")
            .expect("pending set for a run");
        assert!(first_true > run_arm, "pending set before the Run arm");
        // Both flags start false, so a fresh process never holds the box.
        for want in [
            "static EXPORT_PENDING: AtomicBool = AtomicBool::new(false);",
            "static EXPORT_RUNNING: AtomicBool = AtomicBool::new(false);",
        ] {
            assert!(src.contains(want), "lost `{want}`");
        }
        // The panic arm clears both flags and books the run in flight.
        let spawn = src
            .split("pub fn spawn_brutex_export_loop(")
            .nth(1)
            .and_then(|rest| rest.split("/// IST midnight of `day`").next())
            .expect("spawn body");
        for want in [
            "EXPORT_PENDING.store(false",
            "EXPORT_RUNNING.store(false",
            "book_panicked_run(&mut lock_carry(&carry)",
            "run_export_loop(q, c, shared)",
        ] {
            assert!(spawn.contains(want), "the panic arm lost `{want}`");
        }
        let gates = src
            .split("fn common_gates(day: NaiveDate)")
            .nth(1)
            .and_then(|rest| rest.split("/// Readiness of the session day").next())
            .expect("common_gates body");
        for want in [
            "backlog_segments(",
            "evening_replay_pending()",
            "floor_blocked_since_unix()",
            "repair_run_covers_day(day)",
        ] {
            assert!(gates.contains(want), "common_gates lost `{want}`");
        }
        let today = src
            .split("async fn today_reasons(")
            .nth(1)
            .and_then(|rest| rest.split("/// Reports, once per process").next())
            .expect("today_reasons and past_reasons");
        for want in ["arrival_close_secs(", "repair_covered_state(day)"] {
            assert!(today.contains(want), "readiness lost `{want}`");
        }
    }
}
