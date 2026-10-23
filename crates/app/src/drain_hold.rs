//! The after-close drain hold (operator Quote 31, 2026-10-21).
//!
//! Operator, 2026-10-21: "until or unelss movign all tehse dtaa to db never
//! evr shut down" and "evryday move shdou lbe finsihed withint that day".
//! MEASURED the same morning: on 20 Oct the after-close depth pass wrote
//! 3,067,442,594 rows between 15:45 and 17:01, QuestDB fell 6,758
//! transactions behind, the box stopped at 17:30 still ~6,000 behind, and the
//! next morning QuestDB applied that backlog on top of the boot replay and the
//! open: the disk sat at its 500 MiB/s limit from 08:35 to 09:55 and live tick
//! flushes were rescued to spill every few seconds.
//!
//! # What it does
//!
//! From 15:40 IST until 07:30 IST the next morning, once a minute, it measures
//! whether the day's data is in the database ([`DayWork`]): every skipped
//! order-book bucket written back (or left only for a reason a rerun tonight
//! cannot fix), every spill folder empty, today's sealed-candle spill drained,
//! and QuestDB's WAL apply backlog at its idle level with no suspended table
//! and a trusted sink. While work is left it writes the SSM String parameter
//! `/tickvault/<env>/drain-hold`, refreshed every [`DRAIN_HOLD_REFRESH_SECS`]
//! with an expiry [`DRAIN_HOLD_TTL_SECS`] ahead and never past 07:30 IST. The
//! 16:30 daily stop (17:30 until 2026-10-22), the 16:45 stop check, the
//! hourly curfew check and the hard-stop guard's out-of-window stop leave the box running while it is
//! fresh (`crates/aws-lambdas/src/drain_hold.rs`). After
//! [`DRAIN_HOLD_DONE_POLLS`] done readings in a row it writes `released`, and
//! the next scheduled check stops the box.
//!
//! # What it never does
//!
//! It never gates the budget stop (the Lambdas read the hold only on their
//! schedule and curfew paths). A hold the app stops refreshing lapses within
//! [`DRAIN_HOLD_TTL_SECS`], so a crashed app cannot keep the box up. It never
//! pages: a failed write is one coded `DRAIN-HOLD-01` line per failure
//! episode, log-sink-only (Quote 31).
//!
//! # Cost
//!
//! Cold: once a minute, a handful of directory listings (O(files) in the spill
//! folders, empty on a healthy evening), one seal-spill scan for today, and a
//! few atomic loads; one SSM write every five minutes while holding.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tickvault_common::error_code::ErrorCode;
use tracing::{error, info, warn};

/// 15:40 IST, the capture end: the hold starts being judged.
pub const DRAIN_HOLD_START_SECS_IST: u64 =
    tickvault_common::constants::TICK_PERSIST_END_SECS_OF_DAY_IST as u64;
/// 07:30 IST: no hold reaches past this. The 07:35 curfew check then stops
/// the box and the 08:30 start boots it fresh.
pub const DRAIN_HOLD_CEILING_SECS_IST: u64 = 7 * 3_600 + 30 * 60;
/// How often the day's work is measured.
pub const DRAIN_HOLD_EVAL_SECS: u64 = 60;
/// How often a held box's parameter is rewritten.
pub const DRAIN_HOLD_REFRESH_SECS: i64 = 300;
/// How far ahead each written hold expires. Four refreshes' worth, so one
/// slow SSM write never lets the hold lapse; short enough that a dead app
/// frees the box within 20 minutes.
pub const DRAIN_HOLD_TTL_SECS: i64 = 1_200;
/// Done readings in a row before the hold is released. Two minutes apart, so
/// one quiet poll between two writes cannot release it.
pub const DRAIN_HOLD_DONE_POLLS: u32 = 2;
/// QuestDB's largest WAL apply lag (transactions) that counts as caught up.
/// After the close a few small writers (audit and monitor tables) keep
/// committing, so the backlog flickers between 0 and a handful rather than
/// sitting at exactly 0 (Assumed; MEASURED live 12–350 during a session).
pub const DRAIN_HOLD_IDLE_LAG: u64 = 10;
/// Timeout for one SSM write.
const DRAIN_HOLD_PUT_TIMEOUT: Duration = Duration::from_secs(10); // APPROVED: this IS the named constant

// The hold must start after capture ends and end before capture starts.
const _: () = assert!(DRAIN_HOLD_CEILING_SECS_IST < DRAIN_HOLD_START_SECS_IST);
const _: () = assert!(
    DRAIN_HOLD_CEILING_SECS_IST
        <= tickvault_common::constants::TICK_PERSIST_START_SECS_OF_DAY_IST as u64
);
const _: () = assert!(DRAIN_HOLD_REFRESH_SECS * 3 < DRAIN_HOLD_TTL_SECS);

/// Set once this process has written a hold or a release tonight; cleared
/// when the window ends. Read once, at shutdown classification.
static HELD_TONIGHT: AtomicBool = AtomicBool::new(false);

/// How long after the hold window's 07:30 end a held box's stop can land:
/// the 07:35 curfew check plus EC2's delivery of the shutdown (Assumed).
const HELD_STOP_LAG_SECS: u64 = 600;

/// 16:25 IST: no scheduled stop exists before this (the 16:30 daily stop's
/// jitter window opens here), so a stop between the first hold write and
/// 16:25 is never the hold's stop and stays loud (hostile review, 2026-10-21).
/// Derived from the one stop constant (2026-10-22, plan item 64, Quote 37:
/// was 17:25 with the 17:30 stop).
const HELD_STOP_EARLIEST_SECS_IST: u64 =
    crate::shutdown_class::SCHEDULED_STOP_WINDOW_START_SECS_OF_DAY_IST as u64;

/// Whether a SIGTERM at `secs_ist` is the scheduled stop of a box this
/// process held after the close (Quote 31): a hold was written tonight, and
/// it is 16:25 IST or later, overnight, or the minutes after 07:30. O(1).
#[must_use]
pub fn stop_follows_tonights_hold(secs_ist: u32) -> bool {
    let secs = u64::from(secs_ist);
    HELD_TONIGHT.load(Ordering::Acquire)
        && !(DRAIN_HOLD_CEILING_SECS_IST + HELD_STOP_LAG_SECS..HELD_STOP_EARLIEST_SECS_IST)
            .contains(&secs)
}

/// The SSM parameter the Lambdas read, for `environment` (e.g. `prod`).
#[must_use]
pub fn drain_hold_param(environment: &str) -> String {
    format!("/tickvault/{environment}/drain-hold")
}

/// Whether the hold is judged at `secs_ist` (15:40 to 07:30, wrapping
/// midnight). O(1).
#[must_use]
pub const fn in_hold_window(secs_ist: u64) -> bool {
    secs_ist >= DRAIN_HOLD_START_SECS_IST || secs_ist < DRAIN_HOLD_CEILING_SECS_IST
}

/// The expiry to write at `now_unix`: [`DRAIN_HOLD_TTL_SECS`] ahead, cut at
/// the next 07:30 IST. O(1).
#[must_use]
pub const fn hold_until_unix(now_unix: i64, now_secs_ist: u64) -> i64 {
    let to_ceiling = if now_secs_ist < DRAIN_HOLD_CEILING_SECS_IST {
        DRAIN_HOLD_CEILING_SECS_IST - now_secs_ist
    } else {
        86_400 - now_secs_ist + DRAIN_HOLD_CEILING_SECS_IST
    };
    #[allow(clippy::cast_possible_wrap)] // APPROVED: to_ceiling < 86_400
    let to_ceiling = to_ceiling as i64;
    let ahead = if to_ceiling < DRAIN_HOLD_TTL_SECS {
        to_ceiling
    } else {
        DRAIN_HOLD_TTL_SECS
    };
    now_unix + ahead
}

/// What is still between the day's data and the database.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DayWork {
    /// Marked skipped-depth buckets.
    pub deferred_marks: usize,
    /// The mark table overflowed (it cannot name every bucket).
    pub deferred_overflowed: bool,
    /// The last after-close pass's buckets a rerun could still finish;
    /// `None` until a pass has finished in this process.
    pub deferred_retryable_left: Option<u64>,
    /// Non-empty files waiting in the tick, depth, top-volume and audit
    /// spill folders.
    pub spill_files: usize,
    /// Today's sealed-candle spill files not yet confirmed in the database,
    /// plus parked ones; `None` when the folder could not be read.
    pub seal_staged: Option<usize>,
    /// QuestDB's largest WAL apply lag; `None` when unknown.
    pub apply_lag: Option<u64>,
    /// Suspended QuestDB tables; `None` when unknown.
    pub suspended: Option<u64>,
    /// The applied watermark distrusts the sink.
    pub sink_suspect: bool,
    /// WAL segments the unapplied ranges still pin (replay work left for a
    /// boot's catch-up); `None` when the waiting set could not be read.
    pub wal_backlog: Option<u64>,
    /// The backlog keeps the hold: an evening restart will replay it
    /// (`evening_drain::EveningStep::holds`). Set by the supervisor.
    pub wal_holding: bool,
}

impl DayWork {
    /// Whether the day's data is in the database. Unknown readings count as
    /// not done, the direction that keeps the box up. O(1).
    #[must_use]
    pub fn is_done(&self) -> bool {
        let deferred_done = (self.deferred_marks == 0 && !self.deferred_overflowed)
            || self.deferred_retryable_left == Some(0);
        deferred_done
            && self.spill_files == 0
            && self.seal_staged == Some(0)
            && self.apply_lag.is_some_and(|lag| lag <= DRAIN_HOLD_IDLE_LAG)
            && self.suspended == Some(0)
            && !self.sink_suspect
            && !self.wal_holding
    }

    /// A short, plain summary for the parameter and the log line.
    #[must_use]
    pub fn left_text(&self) -> String {
        let show = |v: Option<u64>| v.map_or_else(|| "unknown".to_string(), |v| v.to_string());
        format!(
            "depth_buckets={}{} spill_files={} seal_files={} db_backlog={} suspended={}{} \
             wal_segments={}{}",
            self.deferred_marks,
            if self.deferred_overflowed {
                "+overflow"
            } else {
                ""
            },
            self.spill_files,
            self.seal_staged
                .map_or_else(|| "unknown".to_string(), |v| v.to_string()),
            show(self.apply_lag),
            show(self.suspended),
            if self.sink_suspect {
                " sink_suspect"
            } else {
                ""
            },
            show(self.wal_backlog),
            if self.wal_holding { "+replaying" } else { "" },
        )
    }
}

/// The parameter value: `{"v":1,"state":…,"until_unix":…,"written_unix":…,
/// "left":…}`, the contract the Lambdas parse.
#[must_use]
pub fn hold_value(holding: bool, until_unix: i64, written_unix: i64, left: &str) -> String {
    serde_json::json!({
        "v": 1,
        "state": if holding { "holding" } else { "released" },
        "until_unix": if holding { until_unix } else { 0 },
        "written_unix": written_unix,
        "left": left,
    })
    .to_string()
}

/// Non-empty spill files waiting in `dir` (quarantine excluded). A drained
/// file can be left behind empty, which is not work. O(files).
fn waiting_spill_files(dir: &Path) -> usize {
    tickvault_storage::tick_spill_replay::list_spill_files(dir)
        .iter()
        .filter(|p| std::fs::metadata(p).is_ok_and(|m| m.len() > 0))
        .count()
}

/// Measures the day's work. Blocking file I/O: run it off the async workers.
#[must_use]
// TEST-EXEMPT: reads the production spill folders and process globals; each part is unit-tested (waiting_spill_files, is_done, last_pass_retryable_left, wal_apply_lag_max).
pub fn measure_day_work(today_ist: chrono::NaiveDate) -> DayWork {
    use tickvault_storage::audit_spill::AuditSpillTable;
    let marks = tickvault_storage::wal_deferred_depth::deferred_depth().snapshot();
    // The top_volume spill folder left this list on 2026-10-22 (plan item
    // 62, operator Quote 34): nothing replays it any more, so a file left in
    // it would hold the box up all night for work no task will ever do.
    let mut spill_files = [
        tickvault_storage::tick_persistence::TICK_SPILL_DIR,
        tickvault_storage::depth_persistence::DEPTH_SPILL_DIR,
    ]
    .iter()
    .map(|d| waiting_spill_files(Path::new(d)))
    .sum::<usize>();
    for table in AuditSpillTable::ALL {
        spill_files += waiting_spill_files(&table.default_dir());
    }
    let seal_staged =
        tickvault_storage::seal_spill::staged_production_spill_records_for_day(today_ist)
            .ok()
            .map(|s| s.staged + s.parked);
    DayWork {
        deferred_marks: marks.marked_buckets().len(),
        deferred_overflowed: marks.overflowed,
        deferred_retryable_left:
            crate::dhan_feed_stack::deferred_depth_pass::last_pass_retryable_left(),
        spill_files,
        seal_staged,
        apply_lag: tickvault_common::ingest_shed::wal_apply_lag_max(),
        suspended: tickvault_common::ingest_shed::wal_suspended_tables(),
        sink_suspect: tickvault_storage::wal_applied_watermark::applied_watermark()
            .is_sink_suspect(),
        wal_backlog: tickvault_storage::wal_unapplied_ranges::backlog_segments(
            &crate::boot_helpers::ws_wal_dir(),
        ),
        // Decided by the supervisor (`evening_drain::evening_step`).
        wal_holding: false,
    }
}

/// What the supervisor writes after one measurement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HoldAction {
    /// Nothing to write.
    None,
    /// Write (or refresh) a hold.
    Hold,
    /// Write the release.
    Release,
}

/// How long the work may show no progress before the hold is released anyway.
/// Holding the box saves data only while something is moving; a reading that
/// never settles (a table that stays behind, a probe that stays blind) would
/// otherwise hold it to 07:30 every night, about 14 hours (security review,
/// 2026-10-21). An hour covers the longest pass seen between progress signals
/// (Assumed): the pass writes rows continuously and QuestDB's backlog falls
/// while it pauses.
pub const DRAIN_HOLD_STALL_SECS: i64 = 3_600;

/// Tracks whether the day's work is still moving. Pure, O(1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ProgressTracker {
    /// Marks, spill files, seal files, backlog and WAL segments at the last
    /// progress; `u64::MAX` for a part that was unknown then.
    anchor: [u64; 5],
    /// Pass rows written as of the last reading.
    pass_rows: u64,
    /// When progress was last seen; `None` before the first reading.
    last_progress_unix: Option<i64>,
}

impl ProgressTracker {
    /// Folds one reading in; `true` when nothing has improved for
    /// [`DRAIN_HOLD_STALL_SECS`]. Improvement is the pass writing rows, or any
    /// part falling below its level at the last progress, which then becomes
    /// the new level. So a backlog that rose while the pass wrote and is now
    /// falling counts (3,000 → 600 after a pass), while one that only swings
    /// up and down around a level does not. Unknown parts never count.
    pub fn observe(&mut self, work: &DayWork, pass_rows: u64, now_unix: i64) -> bool {
        let parts = [
            u64::try_from(work.deferred_marks).unwrap_or(u64::MAX),
            u64::try_from(work.spill_files).unwrap_or(u64::MAX),
            work.seal_staged
                .map_or(u64::MAX, |v| u64::try_from(v).unwrap_or(u64::MAX)),
            work.apply_lag.unwrap_or(u64::MAX),
            work.wal_backlog.unwrap_or(u64::MAX),
        ];
        let Some(last) = self.last_progress_unix else {
            self.anchor = parts;
            self.pass_rows = pass_rows;
            self.last_progress_unix = Some(now_unix);
            return false;
        };
        let wrote = pass_rows > self.pass_rows;
        self.pass_rows = pass_rows;
        let fell = self
            .anchor
            .iter()
            .zip(parts)
            .any(|(anchor, part)| part < *anchor);
        if wrote || fell {
            self.anchor = parts;
            self.last_progress_unix = Some(now_unix);
            return false;
        }
        now_unix - last >= DRAIN_HOLD_STALL_SECS
    }
}

/// The supervisor's state between measurements.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct HoldState {
    /// Done readings in a row.
    pub done_streak: u32,
    /// The release has been written and nothing has needed holding since.
    pub released: bool,
    /// When a hold was last written (unix seconds), `None` if never.
    pub last_hold_write: Option<i64>,
}

impl HoldState {
    /// Folds one measurement in and says what to write. Pure, O(1).
    pub fn step(&mut self, done: bool, now_unix: i64) -> HoldAction {
        if done {
            self.done_streak = self.done_streak.saturating_add(1);
            if self.done_streak >= DRAIN_HOLD_DONE_POLLS && !self.released {
                self.released = true;
                self.last_hold_write = None;
                return HoldAction::Release;
            }
            return HoldAction::None;
        }
        self.done_streak = 0;
        self.released = false;
        let due = self
            .last_hold_write
            .is_none_or(|at| now_unix - at >= DRAIN_HOLD_REFRESH_SECS);
        if due {
            self.last_hold_write = Some(now_unix);
            HoldAction::Hold
        } else {
            HoldAction::None
        }
    }
}

/// Spawns the drain-hold supervisor for `environment`. Runs for the life of
/// the process; outside 15:40–07:30 IST it only sleeps.
// TEST-EXEMPT: an endless loop over the SSM client; its decisions are HoldState::step, hold_value and hold_until_unix, all unit-tested.
pub fn spawn_drain_hold_supervisor(environment: String) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let param = drain_hold_param(&environment);
        let mut ssm: Option<aws_sdk_ssm::Client> = None;
        let mut state = HoldState::default();
        let mut progress = ProgressTracker::default();
        let mut stall_reported = false;
        let mut put_failing = false;
        let mut evening_state_bad = false;
        loop {
            let secs_ist = crate::dhan_feed_stack::now_ist_secs_of_day();
            if !in_hold_window(secs_ist) {
                state = HoldState::default();
                progress = ProgressTracker::default();
                // Past the hold window's end plus the stop lag, tonight is over.
                if secs_ist >= DRAIN_HOLD_CEILING_SECS_IST + HELD_STOP_LAG_SECS {
                    HELD_TONIGHT.store(false, Ordering::Release);
                }
                tokio::time::sleep(Duration::from_secs(DRAIN_HOLD_EVAL_SECS)).await;
                continue;
            }
            let today = chrono::Utc::now()
                .with_timezone(&tickvault_common::trading_calendar::ist_offset())
                .date_naive();
            // The evening belongs to the trading day that just closed.
            let day = if secs_ist < DRAIN_HOLD_CEILING_SECS_IST {
                today.pred_opt().unwrap_or(today)
            } else {
                today
            };
            let evening_path = Path::new(crate::evening_drain::EVENING_DRAIN_STATE_PATH);
            let measured = tokio::task::spawn_blocking(move || {
                (
                    measure_day_work(day),
                    crate::evening_drain::load_state(evening_path),
                    crate::daily_task_marker::daily_marker_exists(
                        crate::dhan_live_crossverify_boot::CROSSVERIFY_MARKER_TASK,
                        day,
                    ),
                )
            })
            .await;
            let (mut work, evening, xverify_marker) = match measured {
                Ok(pair) => pair,
                Err(err) => {
                    // Not measured means not refreshed: repeated, the hold lapses.
                    error!(
                        code = ErrorCode::DrainHold01NotWritten.code_str(),
                        source = "drain_hold_measure_failed",
                        error = %err,
                        "drain hold: the day's work could not be measured this minute, so the \
                         hold was not refreshed; retrying in a minute"
                    );
                    tokio::time::sleep(Duration::from_secs(DRAIN_HOLD_EVAL_SECS)).await;
                    continue;
                }
            };
            let now_unix = chrono::Utc::now().timestamp();
            // Quote 31's evening drain: the WAL backlog holds the box only
            // while a planned restart will replay it (AWS only: elsewhere
            // nothing restarts the process).
            let on_aws = matches!(
                tickvault_core::notification::source_badge::runtime_source(),
                tickvault_core::notification::source_badge::RuntimeSource::Aws
            );
            let evening_day = chrono::Datelike::num_days_from_ce(&day);
            if evening.is_none() && !evening_state_bad {
                evening_state_bad = true;
                error!(
                    code = ErrorCode::DrainHold01NotWritten.code_str(),
                    source = "evening_drain_state",
                    path = crate::evening_drain::EVENING_DRAIN_STATE_PATH,
                    "evening drain: the state file cannot be read, so no restart is made (its \
                     count could not be kept); the WAL backlog waits for the next boot"
                );
            }
            let evening_step = match evening {
                Some(evening) if on_aws => crate::evening_drain::evening_step(
                    &evening,
                    evening_day,
                    work.is_done(),
                    crate::evening_drain::crossverify_settled(xverify_marker, secs_ist),
                    work.wal_backlog,
                    now_unix,
                    secs_ist,
                ),
                _ => crate::evening_drain::EveningStep::Idle,
            };
            if let Some(evening) = evening.filter(|e| {
                evening_step == crate::evening_drain::EveningStep::GaveUp
                    && !(e.day == evening_day && e.gave_up)
            }) {
                // Latch the verdict so it cannot flip back as the count moves.
                let latched = crate::evening_drain::state_for_give_up(&evening, evening_day);
                let stored = tokio::task::spawn_blocking(move || {
                    crate::evening_drain::store_state(evening_path, &latched)
                })
                .await;
                if matches!(stored, Ok(Ok(()))) {
                    info!(
                        left = %work.left_text(),
                        restarts = evening.restarts,
                        "evening drain: no more restarts tonight (cap, clock or no progress); the \
                         WAL backlog no longer holds the box and waits for the next boot"
                    );
                }
            }
            work.wal_holding = evening_step.holds();
            let pass_rows = crate::dhan_feed_stack::deferred_depth_pass::pass_rows_written_total();
            let stalled = progress.observe(&work, pass_rows, now_unix);
            let done = work.is_done();
            let restart_now =
                evening_step == crate::evening_drain::EveningStep::Restart && !stalled;
            if restart_now {
                // Refresh the hold this minute, so the restarted process
                // finds a fresh one while it boots.
                state.last_hold_write = None;
            }
            if stalled && !done && !stall_reported {
                stall_reported = true;
                warn!(
                    left = %work.left_text(),
                    stall_secs = DRAIN_HOLD_STALL_SECS,
                    "drain hold given up: nothing has moved for an hour, so holding the box \
                     would only cost money. The work stays on disk for the next evening"
                );
            }
            if !stalled {
                stall_reported = false;
            }
            let action = state.step(done || stalled, now_unix);
            let mut left = work.left_text();
            if stalled && !done {
                left.push_str(" stalled");
            }
            let value = match action {
                HoldAction::None => None,
                HoldAction::Hold => Some(hold_value(
                    true,
                    hold_until_unix(now_unix, secs_ist),
                    now_unix,
                    &left,
                )),
                HoldAction::Release => Some(hold_value(false, 0, now_unix, &left)),
            };
            if let Some(value) = value {
                if ssm.is_none() {
                    ssm = Some(
                        tickvault_core::auth::secret_manager::create_ssm_client_public().await,
                    );
                }
                let written = match &ssm {
                    Some(client) => put_hold(client, &param, &value).await,
                    None => Err("no SSM client".to_string()),
                };
                match written {
                    Ok(()) => {
                        // Only a HOLD marks the night held: a release-only
                        // evening (nothing to hold) keeps its stops loud
                        // outside the usual window.
                        if action == HoldAction::Hold {
                            HELD_TONIGHT.store(true, Ordering::Release);
                        }
                        if put_failing {
                            put_failing = false;
                            info!(param = %param, "drain hold: SSM write recovered");
                        }
                        if action == HoldAction::Release {
                            info!(
                                param = %param,
                                left = %left,
                                "drain hold released: the day's data is in the database, so the \
                                 next scheduled check may stop the box"
                            );
                        } else {
                            info!(
                                param = %param,
                                left = %left,
                                "drain hold written: the day's data is not all in the database \
                                 yet, so the scheduled stops keep the box running"
                            );
                        }
                        // A fresh hold is in place: restart now if due.
                        if let Some(evening) =
                            evening.filter(|_| restart_now && action == HoldAction::Hold)
                        {
                            request_evening_restart(&evening, evening_day, &work, now_unix).await;
                        }
                    }
                    Err(reason) => {
                        // Retry at the next measurement instead of waiting a
                        // whole refresh period.
                        if action == HoldAction::Hold {
                            state.last_hold_write = None;
                        } else {
                            state.released = false;
                            state.done_streak = DRAIN_HOLD_DONE_POLLS.saturating_sub(1);
                        }
                        if !put_failing {
                            put_failing = true;
                            error!(
                                code = ErrorCode::DrainHold01NotWritten.code_str(),
                                source = "drain_hold_put_failed",
                                param = %param,
                                error = %reason,
                                left = %left,
                                "drain hold could not be written to SSM — the box may be stopped \
                                 before the day's data is in the database. Nothing is lost: the \
                                 work stays on disk for the next evening"
                            );
                        }
                    }
                }
            }
            tokio::time::sleep(Duration::from_secs(DRAIN_HOLD_EVAL_SECS)).await;
        }
    })
}

/// Records the evening restart and asks `main` for it (Quote 31's evening
/// drain). The record is written first and the restart asked only when it
/// landed: an unrecorded restart would not count toward the evening's cap.
// TEST-EXEMPT: file I/O and a Notify around evening_drain::state_for_restart and request_self_restart, both unit-tested.
async fn request_evening_restart(
    evening: &crate::evening_drain::EveningState,
    day: i32,
    work: &DayWork,
    now_unix: i64,
) {
    let backlog = work.wal_backlog.unwrap_or(0);
    let next = crate::evening_drain::state_for_restart(evening, day, backlog, now_unix);
    let path = Path::new(crate::evening_drain::EVENING_DRAIN_STATE_PATH);
    let stored =
        tokio::task::spawn_blocking(move || crate::evening_drain::store_state(path, &next)).await;
    match stored {
        Ok(Ok(())) => {
            info!(
                restart = next.restarts,
                wal_segments = backlog,
                "evening drain: the day's work is in the database and WAL segments still wait \
                 for replay, so the app restarts to replay them; the hold stays in place"
            );
            crate::evening_drain::request_self_restart();
        }
        Ok(Err(err)) => error!(
            code = ErrorCode::DrainHold01NotWritten.code_str(),
            source = "evening_drain_state",
            error = %err,
            "evening drain: the restart could not be recorded, so it is not made this minute"
        ),
        Err(err) => error!(
            code = ErrorCode::DrainHold01NotWritten.code_str(),
            source = "evening_drain_state",
            error = %err,
            "evening drain: the restart record task failed, so it is not made this minute"
        ),
    }
}

/// One SSM write of the hold, under [`DRAIN_HOLD_PUT_TIMEOUT`].
// TEST-EXEMPT: a single AWS SDK call; the value it writes is unit-tested via hold_value, and the decision to write via HoldState::step.
async fn put_hold(ssm: &aws_sdk_ssm::Client, param: &str, value: &str) -> Result<(), String> {
    let put = ssm
        .put_parameter()
        .name(param)
        .value(value)
        .r#type(aws_sdk_ssm::types::ParameterType::String)
        .overwrite(true)
        .send();
    match tokio::time::timeout(DRAIN_HOLD_PUT_TIMEOUT, put).await {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(err)) => Err(aws_sdk_ssm::error::DisplayErrorContext(&err).to_string()),
        Err(_) => Err("timed out".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn done_work() -> DayWork {
        DayWork {
            deferred_retryable_left: Some(0),
            seal_staged: Some(0),
            apply_lag: Some(0),
            suspended: Some(0),
            ..DayWork::default()
        }
    }

    #[test]
    fn stop_follows_tonights_hold_only_after_a_hold_and_only_out_of_hours() {
        // The one test that touches this global.
        HELD_TONIGHT.store(false, Ordering::Release);
        assert!(
            !stop_follows_tonights_hold(70_000),
            "no hold tonight: stays loud"
        );
        HELD_TONIGHT.store(true, Ordering::Release);
        assert!(stop_follows_tonights_hold(70_000), "19:26 IST after a hold");
        assert!(stop_follows_tonights_hold(3_600), "01:00 IST after a hold");
        assert!(
            stop_follows_tonights_hold(7 * 3_600 + 35 * 60),
            "the 07:35 curfew stop"
        );
        assert!(
            !stop_follows_tonights_hold(8 * 3_600 + 45 * 60),
            "08:45 IST is a new day"
        );
        assert!(
            !stop_follows_tonights_hold(11 * 3_600),
            "mid-session stays loud"
        );
        // 2026-10-22 (plan item 64): the 16:30 stop's window opens at 16:25.
        assert!(
            stop_follows_tonights_hold(16 * 3_600 + 25 * 60),
            "16:25 IST, the stop window"
        );
        assert!(
            stop_follows_tonights_hold(16 * 3_600 + 30 * 60),
            "the 16:30 daily stop of a held box"
        );
        assert!(
            !stop_follows_tonights_hold(16 * 3_600 + 25 * 60 - 1),
            "16:24:59 IST stays loud"
        );
        HELD_TONIGHT.store(false, Ordering::Release);
    }

    #[test]
    fn observe_reports_a_stall_only_after_an_hour_without_any_progress() {
        let stuck = DayWork {
            apply_lag: Some(40),
            ..done_work()
        };
        let mut t = ProgressTracker::default();
        assert!(!t.observe(&stuck, 0, 0), "the first reading is a baseline");
        assert!(!t.observe(&stuck, 0, DRAIN_HOLD_STALL_SECS - 1));
        assert!(
            t.observe(&stuck, 0, DRAIN_HOLD_STALL_SECS),
            "an hour with nothing moving"
        );
        // The pass writing rows is progress, and it resets the clock.
        assert!(!t.observe(&stuck, 10, DRAIN_HOLD_STALL_SECS + 60));
        assert!(!t.observe(&stuck, 10, 2 * DRAIN_HOLD_STALL_SECS));
        assert!(t.observe(&stuck, 10, 2 * DRAIN_HOLD_STALL_SECS + 60));
        // A falling backlog is progress.
        let lower = DayWork {
            apply_lag: Some(30),
            ..done_work()
        };
        assert!(!t.observe(&lower, 10, 3 * DRAIN_HOLD_STALL_SECS));
    }

    /// Hostile review (2026-10-21): a backlog that rose while the pass wrote
    /// and is now falling is progress; one that only swings around a level
    /// is not.
    #[test]
    fn observe_counts_a_falling_backlog_after_a_rise_but_not_a_swing() {
        let lag = |l: u64| DayWork {
            deferred_marks: 3,
            apply_lag: Some(l),
            ..done_work()
        };
        let mut t = ProgressTracker::default();
        assert!(!t.observe(&lag(12), 0, 0));
        // The pass writes; the backlog rises to 3,000 meanwhile.
        assert!(!t.observe(&lag(3_000), 500, 60));
        // The pass pauses; the backlog falls for over an hour.
        let mut now = 60;
        for l in [2_500, 2_000, 1_500, 1_000, 600] {
            now += 900;
            assert!(!t.observe(&lag(l), 500, now), "falling to {l} is progress");
        }
        // Swinging between 600 and 650 for an hour is not.
        for (i, l) in [650, 600, 650, 600, 650].into_iter().enumerate() {
            now += 900;
            let stalled = t.observe(&lag(l), 500, now);
            assert_eq!(stalled, i >= 3, "swing step {i}");
        }
    }

    #[test]
    fn observe_never_counts_an_unknown_reading_as_progress() {
        let blind = DayWork {
            apply_lag: None,
            seal_staged: None,
            ..done_work()
        };
        let mut t = ProgressTracker::default();
        assert!(!t.observe(&blind, 0, 0));
        assert!(
            t.observe(&blind, 0, DRAIN_HOLD_STALL_SECS),
            "a blind probe holds for an hour, not all night"
        );
        // A rising part is not progress either.
        let worse = DayWork {
            spill_files: 5,
            ..blind
        };
        assert!(t.observe(&worse, 0, DRAIN_HOLD_STALL_SECS + 60));
    }

    #[test]
    fn drain_hold_param_names_the_environment() {
        assert_eq!(drain_hold_param("prod"), "/tickvault/prod/drain-hold");
    }

    #[test]
    fn in_hold_window_covers_the_evening_and_night_only() {
        assert!(!in_hold_window(DRAIN_HOLD_START_SECS_IST - 1));
        assert!(in_hold_window(DRAIN_HOLD_START_SECS_IST));
        assert!(in_hold_window(63_000), "17:30 IST");
        assert!(in_hold_window(0), "midnight");
        assert!(in_hold_window(DRAIN_HOLD_CEILING_SECS_IST - 1));
        assert!(!in_hold_window(DRAIN_HOLD_CEILING_SECS_IST), "07:30 IST");
        assert!(!in_hold_window(9 * 3_600 + 15 * 60), "the session");
    }

    #[test]
    fn hold_until_unix_never_passes_the_morning_ceiling() {
        let now = 1_800_000_000;
        // Evening: a full TTL ahead.
        assert_eq!(hold_until_unix(now, 63_000), now + DRAIN_HOLD_TTL_SECS);
        // 07:25 IST: only five minutes left to the ceiling.
        assert_eq!(
            hold_until_unix(now, DRAIN_HOLD_CEILING_SECS_IST - 300),
            now + 300
        );
        // 23:59 IST: a TTL ahead (the ceiling is hours away).
        assert_eq!(hold_until_unix(now, 86_399), now + DRAIN_HOLD_TTL_SECS);
    }

    proptest::proptest! {
        /// The written expiry never reaches past the next 07:30 IST and never
        /// more than the TTL ahead, at any time inside the window.
        #[test]
        fn hold_until_is_bounded(secs in 0u64..86_400, now in 1_700_000_000i64..1_900_000_000) {
            proptest::prop_assume!(in_hold_window(secs));
            let until = hold_until_unix(now, secs);
            proptest::prop_assert!(until > now);
            proptest::prop_assert!(until - now <= DRAIN_HOLD_TTL_SECS);
            let to_ceiling = if secs < DRAIN_HOLD_CEILING_SECS_IST {
                DRAIN_HOLD_CEILING_SECS_IST - secs
            } else {
                86_400 - secs + DRAIN_HOLD_CEILING_SECS_IST
            };
            proptest::prop_assert!(until - now <= i64::try_from(to_ceiling).unwrap());
        }
    }

    #[test]
    fn is_done_needs_every_part_and_unknown_is_not_done() {
        assert!(done_work().is_done());
        assert!(
            DayWork {
                deferred_marks: 0,
                deferred_retryable_left: None,
                ..done_work()
            }
            .is_done(),
            "no marks at all is done even before a pass ran"
        );
        let cases = [
            DayWork {
                deferred_marks: 3,
                deferred_retryable_left: Some(2),
                ..done_work()
            },
            DayWork {
                deferred_marks: 3,
                deferred_retryable_left: None,
                ..done_work()
            },
            DayWork {
                deferred_overflowed: true,
                deferred_retryable_left: None,
                ..done_work()
            },
            DayWork {
                spill_files: 1,
                ..done_work()
            },
            DayWork {
                seal_staged: Some(1),
                ..done_work()
            },
            DayWork {
                seal_staged: None,
                ..done_work()
            },
            DayWork {
                apply_lag: Some(DRAIN_HOLD_IDLE_LAG + 1),
                ..done_work()
            },
            DayWork {
                apply_lag: None,
                ..done_work()
            },
            DayWork {
                suspended: Some(1),
                ..done_work()
            },
            DayWork {
                suspended: None,
                ..done_work()
            },
            DayWork {
                sink_suspect: true,
                ..done_work()
            },
            DayWork {
                wal_backlog: Some(4),
                wal_holding: true,
                ..done_work()
            },
        ];
        assert!(
            DayWork {
                wal_backlog: Some(4),
                ..done_work()
            }
            .is_done(),
            "a backlog no evening restart will replay does not hold the box"
        );
        for work in cases {
            assert!(!work.is_done(), "must hold: {work:?}");
        }
        // Marks left only for reasons a rerun tonight cannot fix do not hold.
        assert!(
            DayWork {
                deferred_marks: 2,
                deferred_retryable_left: Some(0),
                ..done_work()
            }
            .is_done()
        );
        assert!(
            DayWork {
                apply_lag: Some(DRAIN_HOLD_IDLE_LAG),
                ..done_work()
            }
            .is_done()
        );
    }

    /// Regression (Quote 31, 2026-10-21): on 20 Oct the box stopped at 17:30
    /// with QuestDB ~6,000 transactions behind. That reading must hold.
    #[test]
    fn test_regression_a_6000_transaction_backlog_holds_the_box() {
        let evening = DayWork {
            apply_lag: Some(6_000),
            ..done_work()
        };
        assert!(!evening.is_done());
        let mut state = HoldState::default();
        assert_eq!(state.step(evening.is_done(), 1_000), HoldAction::Hold);
    }

    #[test]
    fn step_refreshes_releases_once_and_reholds() {
        let mut s = HoldState::default();
        assert_eq!(s.step(false, 0), HoldAction::Hold);
        assert_eq!(s.step(false, 60), HoldAction::None, "not due yet");
        assert_eq!(
            s.step(false, DRAIN_HOLD_REFRESH_SECS),
            HoldAction::Hold,
            "refresh"
        );
        assert_eq!(
            s.step(true, 400),
            HoldAction::None,
            "one done reading is not enough"
        );
        assert_eq!(s.step(true, 460), HoldAction::Release);
        assert_eq!(s.step(true, 520), HoldAction::None, "released once");
        // A done reading broken by work: the streak restarts.
        let mut t = HoldState::default();
        assert_eq!(t.step(true, 0), HoldAction::None);
        assert_eq!(t.step(false, 60), HoldAction::Hold);
        assert_eq!(t.step(true, 120), HoldAction::None);
        assert_eq!(t.step(true, 180), HoldAction::Release);
        // New work after the release holds again at once.
        assert_eq!(t.step(false, 240), HoldAction::Hold);
    }

    #[test]
    fn hold_value_is_the_lambda_contract() {
        let held: serde_json::Value =
            serde_json::from_str(&hold_value(true, 1_800_001_200, 1_800_000_000, "x")).unwrap();
        assert_eq!(held["v"], 1);
        assert_eq!(held["state"], "holding");
        assert_eq!(held["until_unix"], 1_800_001_200_i64);
        assert_eq!(held["written_unix"], 1_800_000_000_i64);
        assert_eq!(held["left"], "x");
        let released: serde_json::Value =
            serde_json::from_str(&hold_value(false, 99, 1_800_000_000, "")).unwrap();
        assert_eq!(released["state"], "released");
        assert_eq!(
            released["until_unix"], 0,
            "a release never carries an expiry"
        );
    }

    #[test]
    fn left_text_names_every_part_and_unknowns() {
        let text = DayWork {
            deferred_marks: 2,
            deferred_overflowed: true,
            spill_files: 1,
            seal_staged: None,
            apply_lag: Some(42),
            suspended: None,
            sink_suspect: true,
            wal_backlog: Some(7),
            wal_holding: true,
            ..DayWork::default()
        }
        .left_text();
        assert_eq!(
            text,
            "depth_buckets=2+overflow spill_files=1 seal_files=unknown db_backlog=42 \
             suspended=unknown sink_suspect wal_segments=7+replaying"
        );
        assert!(
            DayWork::default()
                .left_text()
                .ends_with("wal_segments=unknown"),
            "an unread backlog says so"
        );
    }

    #[test]
    fn waiting_spill_files_ignores_empty_and_missing() {
        let dir = std::env::temp_dir().join(format!("tv-drain-hold-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(
            waiting_spill_files(&dir),
            0,
            "a missing folder holds nothing"
        );
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("ticks-dhan-1.ilp"), b"").unwrap();
        assert_eq!(waiting_spill_files(&dir), 0, "an emptied file is not work");
        std::fs::write(dir.join("ticks-dhan-2.ilp"), b"row\n").unwrap();
        assert_eq!(waiting_spill_files(&dir), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
