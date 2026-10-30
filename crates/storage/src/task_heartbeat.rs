//! Heartbeats for the long-lived background tasks the hot-path telemetry
//! does not watch (plan item 4, scenarios #57 and #58).
//!
//! [`crate::hot_path_telemetry`] already publishes a heartbeat age for the
//! two runtime probes, the socket readers, the frame drain and the two ILP
//! writer threads. Every other long-lived task was watched only for DEATH:
//! each `spawn_supervised_*` wrapper awaits its task's `JoinHandle` and
//! respawns when it ends. A task that is alive but stuck (an `await` that
//! never resolves, a lock that is never released) never ends, so its
//! supervisor waited for ever and nothing reported the silence.
//!
//! This module closes that gap for the tasks in [`WatchedTask`]:
//!
//! * each task calls [`beat`] once per loop pass: one clock read and one
//!   relaxed store into a fixed `static` slot, O(1), no allocation, no lock;
//! * [`publish_once`] (once a second, see [`crate::memory_watchdog`]) turns
//!   the slots into `tv_task_heartbeat_age_seconds{task}` and counts each
//!   transition into "missed" on `tv_task_heartbeat_missed_total{task}`,
//!   with one log-only `warn!` per episode;
//! * the supervisors await their task through [`await_watched`], which also
//!   notices three missed beats. With `[task_heartbeat]
//!   respawn_on_missed_beats = true` it aborts the stuck task and the
//!   supervisor respawns it, counted on `tv_task_respawn_total{task,
//!   reason="missed_beats"}`; with the default `false` it only counts
//!   `tv_task_heartbeat_would_respawn_total{task}` once per episode and
//!   keeps waiting.
//!
//! "Three missed beats" is `3 × cadence` plus a fixed slack for the work a
//! pass legitimately does (see [`WatchedTask::max_silence_secs`]).
//!
//! # Honest limits
//!
//! * `JoinHandle::abort` takes effect only at the task's next `.await`. A
//!   task stuck in synchronous code (a blocking syscall on a runtime worker)
//!   is reported but cannot be aborted; it keeps its worker until the call
//!   returns.
//! * The seal writer ([`WatchedTask::SealWriter`]) owns state that cannot be
//!   rebuilt, so it is watched and never respawned.
//! * The publisher runs on the `tv-task-watch` thread
//!   ([`crate::memory_watchdog::spawn_watch_thread`]), not on `tv-telemetry`:
//!   that thread's file belongs to another change in flight. Moving the one
//!   [`publish_once`] call there is a two-line change.
//! * The slots are per process: after a restart every task is unarmed until
//!   its supervisor arms it again.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use tracing::warn;

/// One long-lived background task watched by heartbeat.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchedTask {
    /// The PROC-01 OOM-kill monitor (60 s cadence).
    OomMonitor = 0,
    /// The RESOURCE-01/02/03 resource monitor (60 s cadence).
    ResourceMonitor = 1,
    /// The spill-disk health watcher (60 s cadence).
    SpillDiskWatcher = 2,
    /// The QuestDB WAL suspension watcher (60 s cadence).
    WalSuspensionWatcher = 3,
    /// The API bearer-token reload loop (300 s cadence).
    ApiTokenReload = 4,
    /// The sealed-candle writer loop (100 ms cadence; watched, never
    /// respawned).
    SealWriter = 5,
}

/// Every watched task, in index order.
pub const ALL_WATCHED_TASKS: [WatchedTask; 6] = [
    WatchedTask::OomMonitor,
    WatchedTask::ResourceMonitor,
    WatchedTask::SpillDiskWatcher,
    WatchedTask::WalSuspensionWatcher,
    WatchedTask::ApiTokenReload,
    WatchedTask::SealWriter,
];

const WATCHED_COUNT: usize = ALL_WATCHED_TASKS.len();

/// How many beats may be missed before a task reads as stuck.
pub const MISSED_BEATS_BEFORE_STALE: u64 = 3;

impl WatchedTask {
    /// The `task` label value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::OomMonitor => "oom_monitor",
            Self::ResourceMonitor => "resource_monitor",
            Self::SpillDiskWatcher => "spill_disk_watcher",
            Self::WalSuspensionWatcher => "wal_suspension_watcher",
            Self::ApiTokenReload => "api_token_reload",
            Self::SealWriter => "seal_writer",
        }
    }

    /// The task's loop cadence in seconds (its expected beat interval).
    ///
    /// The seal writer's 100 ms cadence rounds up to one second here; its
    /// silence bound is set by the slack below, not by three cadences.
    #[must_use]
    pub const fn cadence_secs(self) -> u64 {
        match self {
            Self::OomMonitor => crate::oom_monitor::OOM_MONITOR_POLL_INTERVAL_SECS,
            Self::ResourceMonitor => crate::resource_monitor::RESOURCE_MONITOR_POLL_INTERVAL_SECS,
            Self::SpillDiskWatcher => {
                crate::disk_health_watcher::SPILL_DISK_HEALTH_POLL_INTERVAL_SECS
            }
            Self::WalSuspensionWatcher => {
                crate::wal_suspension_watcher::WAL_SUSPENSION_POLL_INTERVAL_SECS
            }
            Self::ApiTokenReload => API_TOKEN_RELOAD_CADENCE_SECS,
            Self::SealWriter => 1,
        }
    }

    /// Seconds a pass may legitimately take on top of its cadence. The
    /// 60 s watchers make HTTP or file calls bounded well inside a minute;
    /// the token reload makes one SSM read; the seal writer can sit in one
    /// synchronous ILP flush during a QuestDB stall, bounded by the client's
    /// request timeout, so it gets 30 s.
    #[must_use]
    pub const fn work_slack_secs(self) -> u64 {
        match self {
            Self::OomMonitor
            | Self::ResourceMonitor
            | Self::SpillDiskWatcher
            | Self::WalSuspensionWatcher
            | Self::ApiTokenReload => 60,
            Self::SealWriter => 30,
        }
    }

    /// Seconds of silence after which the task reads as stuck: three
    /// missed cadences plus the work slack. O(1), `const`.
    #[must_use]
    pub const fn max_silence_secs(self) -> u64 {
        self.cadence_secs()
            .saturating_mul(MISSED_BEATS_BEFORE_STALE)
            .saturating_add(self.work_slack_secs())
    }

    /// Whether a stuck task may be aborted for its supervisor to respawn.
    /// False for the seal writer: its runner holds the sealed-candle ring
    /// and cannot be rebuilt by a fresh spawn.
    #[must_use]
    pub const fn respawnable(self) -> bool {
        !matches!(self, Self::SealWriter)
    }
}

/// The API token reload cadence. Mirrors
/// `tickvault_app::api_token_rotation::API_TOKEN_RELOAD_INTERVAL_SECS`
/// (the app crate sits above this one); a test there pins the two equal.
pub const API_TOKEN_RELOAD_CADENCE_SECS: u64 = 300;

/// One cache line per slot, so a beating task never invalidates another
/// task's line (the same reasoning as `hot_path_telemetry::TaskBeat`).
#[repr(align(64))]
struct Slot {
    /// Nanoseconds since [`anchor`] of the last beat, plus one; 0 means the
    /// task is not armed (never started, or disabled here).
    last: AtomicU64,
}

impl Slot {
    const fn new() -> Self {
        Self {
            last: AtomicU64::new(0),
        }
    }
}

static SLOTS: [Slot; WATCHED_COUNT] = [const { Slot::new() }; WATCHED_COUNT];
static ANCHOR: OnceLock<Instant> = OnceLock::new();
static RESPAWN_ON_MISSED: AtomicBool = AtomicBool::new(false);

fn anchor() -> Instant {
    *ANCHOR.get_or_init(Instant::now)
}

fn stamp(at: Instant) -> u64 {
    let nanos = at.saturating_duration_since(anchor()).as_nanos();
    u64::try_from(nanos)
        .unwrap_or(u64::MAX - 1)
        .saturating_add(1)
}

/// Sets whether a stuck task is aborted so its supervisor respawns it
/// (`[task_heartbeat] respawn_on_missed_beats`). Call once at boot; the
/// default is `false` (count only).
pub fn set_respawn_on_missed_beats(enabled: bool) {
    RESPAWN_ON_MISSED.store(enabled, Ordering::Relaxed);
}

/// Whether a stuck task is aborted for respawn.
#[must_use]
pub fn respawn_on_missed_beats() -> bool {
    RESPAWN_ON_MISSED.load(Ordering::Relaxed)
}

/// The task made progress at `at`. O(1), zero allocation, no lock.
#[inline]
pub fn beat_at(task: WatchedTask, at: Instant) {
    SLOTS[task as usize]
        .last
        .store(stamp(at), Ordering::Relaxed);
}

/// The task made progress now. O(1), zero allocation, no lock.
#[inline]
pub fn beat(task: WatchedTask) {
    beat_at(task, Instant::now());
}

/// Arms the task's slot at spawn time, so a task that hangs before its
/// first beat is still caught, and a respawned task starts a fresh window.
pub fn arm_at(task: WatchedTask, at: Instant) {
    beat_at(task, at);
}

/// PURE: whole seconds from a slot stamp to `now_stamp`; `None` for an
/// unarmed slot (stamp 0). A `now` before the beat saturates to 0.
#[must_use]
pub const fn age_secs_from(last_stamp: u64, now_stamp: u64) -> Option<u64> {
    if last_stamp == 0 {
        return None;
    }
    Some(now_stamp.saturating_sub(last_stamp) / 1_000_000_000)
}

/// Seconds since the task last beat at `now`; `None` if it is not armed.
#[must_use]
pub fn age_secs_at(task: WatchedTask, now: Instant) -> Option<u64> {
    age_secs_from(
        SLOTS[task as usize].last.load(Ordering::Relaxed),
        stamp(now),
    )
}

/// PURE: is a task whose last beat is `age_secs` old stuck?
/// An unarmed task (`None`) is never stuck: it is not running here.
#[must_use]
pub const fn is_stale(task: WatchedTask, age_secs: Option<u64>) -> bool {
    match age_secs {
        Some(age) => age > task.max_silence_secs(),
        None => false,
    }
}

/// Is `task` stuck at `now`?
#[must_use]
pub fn is_stale_at(task: WatchedTask, now: Instant) -> bool {
    is_stale(task, age_secs_at(task, now))
}

/// PURE: advances one task's episode latch. Returns `true` exactly on the
/// transition into silence (the moment to count and log once).
#[must_use]
pub fn advance_episode(in_episode: &mut bool, stale: bool) -> bool {
    let opened = stale && !*in_episode;
    *in_episode = stale;
    opened
}

/// Pre-resolved Prometheus handles plus the per-task episode latch.
/// Built once after the recorder is installed; [`publish_once`] then
/// allocates nothing.
pub struct HeartbeatPublisher {
    age: Vec<metrics::Gauge>,
    missed: Vec<metrics::Counter>,
    in_episode: [bool; WATCHED_COUNT],
}

impl HeartbeatPublisher {
    /// Resolves every handle and registers the missed counters at 0, so a
    /// reader sees "watched, never missed" rather than an absent series.
    #[must_use]
    pub fn resolve() -> Self {
        let mut age = Vec::with_capacity(WATCHED_COUNT);
        let mut missed = Vec::with_capacity(WATCHED_COUNT);
        for task in ALL_WATCHED_TASKS {
            age.push(metrics::gauge!("tv_task_heartbeat_age_seconds", "task" => task.as_str()));
            let counter =
                metrics::counter!("tv_task_heartbeat_missed_total", "task" => task.as_str());
            counter.increment(0);
            missed.push(counter);
        }
        Self {
            age,
            missed,
            in_episode: [false; WATCHED_COUNT],
        }
    }
}

/// Publishes every armed task's heartbeat age and counts each new episode
/// of silence. Returns how many tasks are stuck right now.
///
/// O(`WATCHED_COUNT`) = O(6) relaxed loads and gauge sets; zero allocation
/// after [`HeartbeatPublisher::resolve`] (the one `warn!` per episode is the
/// only allocating step, and it fires on the edge, never per call).
pub fn publish_once(publisher: &mut HeartbeatPublisher, now: Instant) -> usize {
    let mut ages = [None; WATCHED_COUNT];
    for (slot, task) in ages.iter_mut().zip(ALL_WATCHED_TASKS.iter()) {
        *slot = age_secs_at(*task, now);
    }
    publish_ages(publisher, &ages)
}

/// The body of [`publish_once`] over ages already read, so the latch and
/// counting can be tested without the process-global slots.
pub fn publish_ages(
    publisher: &mut HeartbeatPublisher,
    ages: &[Option<u64>; WATCHED_COUNT],
) -> usize {
    let mut stale_now = 0usize;
    for (i, (task, age)) in ALL_WATCHED_TASKS.iter().zip(ages.iter()).enumerate() {
        let age = *age;
        // An unarmed task publishes nothing: an absent series reads "not
        // running here", a stale one reads "stopped". 0 would be a false OK.
        if let (Some(secs), Some(gauge)) = (age, publisher.age.get(i)) {
            // u64 seconds through u32 is lossless for 136 years.
            // APPROVED: a duration in whole seconds (u32), not an f32 price
            gauge.set(f64::from(u32::try_from(secs).unwrap_or(u32::MAX)));
        }
        let stale = is_stale(*task, age);
        if stale {
            stale_now = stale_now.saturating_add(1);
        }
        let Some(latch) = publisher.in_episode.get_mut(i) else {
            continue;
        };
        if advance_episode(latch, stale) {
            if let Some(counter) = publisher.missed.get(i) {
                counter.increment(1);
            }
            warn!(
                task = task.as_str(),
                age_secs = age.unwrap_or(0),
                max_silence_secs = task.max_silence_secs(),
                respawn_enabled = respawn_on_missed_beats() && task.respawnable(),
                "background task missed {MISSED_BEATS_BEFORE_STALE} heartbeats: it is alive \
                 but has made no progress"
            );
        }
    }
    stale_now
}

/// How a watched task's supervisor learnt the task stopped.
#[derive(Debug)]
pub enum WatchedExit<T> {
    /// The task ended (returned, panicked or was cancelled).
    Joined(Result<T, tokio::task::JoinError>),
    /// The task missed [`MISSED_BEATS_BEFORE_STALE`] beats and was aborted.
    MissedBeats,
}

impl<T> WatchedExit<T> {
    /// The `reason` label for the supervisor's respawn counter:
    /// [`MISSED_BEATS_REASON`], or the join classification for a death.
    #[must_use]
    pub fn reason(&self) -> &'static str {
        match self {
            Self::Joined(Ok(_)) => "clean_exit",
            Self::Joined(Err(e)) if e.is_panic() => "panic",
            Self::Joined(Err(e)) if e.is_cancelled() => "cancelled",
            Self::Joined(Err(_)) => "unknown",
            Self::MissedBeats => MISSED_BEATS_REASON,
        }
    }
}

/// The `reason` label the supervisors use for a [`WatchedExit::MissedBeats`]
/// respawn.
pub const MISSED_BEATS_REASON: &str = "missed_beats";

/// How often [`await_watched`] checks the task's heartbeat: a third of the
/// silence bound, so a stuck task is noticed at most a third late.
#[must_use]
pub const fn check_interval(task: WatchedTask) -> Duration {
    let secs = task.max_silence_secs() / MISSED_BEATS_BEFORE_STALE;
    Duration::from_secs(if secs == 0 { 1 } else { secs })
}

/// Awaits a supervised task like `handle.await`, and also checks its
/// heartbeat every [`check_interval`].
///
/// Arms the slot first, so the window starts at spawn. If the task misses
/// its beats and `respawn_on_missed_beats` is on (and the task is
/// [`WatchedTask::respawnable`]), it is aborted and
/// [`WatchedExit::MissedBeats`] is returned for the supervisor to respawn.
/// Otherwise the episode is counted once on
/// `tv_task_heartbeat_would_respawn_total{task}` and the wait continues.
///
/// Cold: one timer wake per [`check_interval`] (at least 11 s); O(1) per
/// wake.
pub async fn await_watched<T>(
    task: WatchedTask,
    handle: tokio::task::JoinHandle<T>,
) -> WatchedExit<T> {
    await_watched_with(
        task,
        handle,
        respawn_on_missed_beats(),
        check_interval(task),
        Instant::now,
    )
    .await
}

/// [`await_watched`] with the respawn switch, the check interval and the
/// clock passed in, so tests can drive a stuck task without waiting
/// minutes or touching the process-global switch.
pub async fn await_watched_with<T>(
    task: WatchedTask,
    mut handle: tokio::task::JoinHandle<T>,
    respawn: bool,
    check_every: Duration,
    now: impl Fn() -> Instant,
) -> WatchedExit<T> {
    arm_at(task, Instant::now());
    let mut checker = tokio::time::interval(check_every);
    checker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // The first tick of a tokio interval fires at once; consume it.
    checker.tick().await;
    let mut counted_episode = false;
    loop {
        tokio::select! {
            joined = &mut handle => return WatchedExit::Joined(joined),
            _ = checker.tick() => {
                if !is_stale_at(task, now()) {
                    counted_episode = false;
                    continue;
                }
                if respawn && task.respawnable() {
                    handle.abort();
                    // Wait for the abort to land so two copies never run.
                    let _aborted: Result<T, tokio::task::JoinError> = (&mut handle).await;
                    return WatchedExit::MissedBeats;
                }
                if !counted_episode {
                    counted_episode = true;
                    metrics::counter!(
                        "tv_task_heartbeat_would_respawn_total",
                        "task" => task.as_str()
                    )
                    .increment(1);
                }
            }
        }
    }
}

/// Counts one respawn of `task` on `tv_task_respawn_total{task, reason}`
/// and re-arms its slot so the fresh copy starts a fresh window. Called by
/// every supervisor that awaits through [`await_watched`], for a death and
/// for a missed-beats abort alike. Cold (once per respawn).
pub fn note_respawn(task: WatchedTask, reason: &'static str) {
    metrics::counter!("tv_task_respawn_total", "task" => task.as_str(), "reason" => reason)
        .increment(1);
    arm_at(task, Instant::now());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_task_is_listed_once_in_index_order() {
        for (i, task) in ALL_WATCHED_TASKS.iter().enumerate() {
            assert_eq!(*task as usize, i, "{} out of order", task.as_str());
        }
        let mut names: Vec<&str> = ALL_WATCHED_TASKS.iter().map(|t| t.as_str()).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), WATCHED_COUNT, "label values must be distinct");
    }

    #[test]
    fn test_cadence_secs_work_slack_secs_max_silence_secs_bound() {
        assert_eq!(WatchedTask::OomMonitor.max_silence_secs(), 3 * 60 + 60);
        assert_eq!(WatchedTask::ApiTokenReload.max_silence_secs(), 3 * 300 + 60);
        assert_eq!(WatchedTask::SealWriter.max_silence_secs(), 3 + 30);
        for task in ALL_WATCHED_TASKS {
            assert!(
                task.max_silence_secs() > task.cadence_secs() * MISSED_BEATS_BEFORE_STALE,
                "{}: a healthy task beating every cadence must never read stuck",
                task.as_str()
            );
        }
    }

    #[test]
    fn test_respawnable_is_false_only_for_the_seal_writer() {
        for task in ALL_WATCHED_TASKS {
            assert_eq!(task.respawnable(), task != WatchedTask::SealWriter);
        }
    }

    #[test]
    fn is_stale_boundaries() {
        let t = WatchedTask::OomMonitor;
        let bound = t.max_silence_secs();
        assert!(!is_stale(t, None), "unarmed is never stuck");
        assert!(!is_stale(t, Some(0)));
        assert!(
            !is_stale(t, Some(bound)),
            "exactly at the bound is not yet stuck"
        );
        assert!(is_stale(t, Some(bound + 1)));
        assert!(is_stale(t, Some(u64::MAX)));
    }

    #[test]
    fn check_interval_is_a_third_of_the_bound_and_never_zero() {
        for task in ALL_WATCHED_TASKS {
            let every = check_interval(task).as_secs();
            assert!(every >= 1);
            assert!(every * MISSED_BEATS_BEFORE_STALE <= task.max_silence_secs());
        }
    }

    #[test]
    fn test_age_secs_from_whole_seconds_and_saturates() {
        assert_eq!(age_secs_from(0, 99), None, "stamp 0 = unarmed");
        assert_eq!(age_secs_from(1, 1), Some(0));
        assert_eq!(age_secs_from(1, 1 + 4_999_999_999), Some(4));
        assert_eq!(age_secs_from(1, 1 + 5_000_000_000), Some(5));
        assert_eq!(
            age_secs_from(10, 5),
            Some(0),
            "a clock behind the beat saturates"
        );
        assert_eq!(
            age_secs_from(1, u64::MAX),
            Some((u64::MAX - 1) / 1_000_000_000)
        );
    }

    #[test]
    fn stamp_is_never_zero_so_an_armed_slot_never_reads_unarmed() {
        let before_anchor = Instant::now()
            .checked_sub(Duration::from_secs(3600))
            .unwrap_or_else(Instant::now);
        let _ = anchor();
        assert!(stamp(before_anchor) >= 1);
        assert!(stamp(Instant::now()) >= 1);
    }

    #[test]
    fn advance_episode_opens_once_and_closes_on_recovery() {
        let mut latch = false;
        assert!(!advance_episode(&mut latch, false));
        assert!(advance_episode(&mut latch, true), "first stale opens");
        assert!(!advance_episode(&mut latch, true), "same episode");
        assert!(
            !advance_episode(&mut latch, false),
            "recovery closes quietly"
        );
        assert!(
            advance_episode(&mut latch, true),
            "a new silence is a new episode"
        );
    }

    #[test]
    fn publish_ages_counts_stuck_tasks_and_latches_each_once() {
        let mut publisher = HeartbeatPublisher::resolve();
        let mut ages = [None; WATCHED_COUNT];
        assert_eq!(publish_ages(&mut publisher, &ages), 0, "nothing armed");
        let oom = WatchedTask::OomMonitor as usize;
        let seal = WatchedTask::SealWriter as usize;
        ages[oom] = Some(WatchedTask::OomMonitor.max_silence_secs() + 1);
        ages[seal] = Some(1);
        assert_eq!(publish_ages(&mut publisher, &ages), 1);
        assert!(publisher.in_episode[oom]);
        assert!(!publisher.in_episode[seal]);
        assert_eq!(publish_ages(&mut publisher, &ages), 1, "still stuck");
        ages[oom] = Some(0);
        assert_eq!(publish_ages(&mut publisher, &ages), 0);
        assert!(!publisher.in_episode[oom], "a fresh beat ends the episode");
    }

    #[test]
    fn publish_once_reads_the_real_slots_without_panicking() {
        let mut publisher = HeartbeatPublisher::resolve();
        let stuck = publish_once(&mut publisher, Instant::now());
        assert!(stuck <= WATCHED_COUNT);
    }

    #[test]
    fn test_set_respawn_on_missed_beats_and_respawn_on_missed_beats_default_off() {
        // Nothing in this crate's tests turns the switch on (it is
        // process-global, so the tests below pass it explicitly instead).
        assert!(!respawn_on_missed_beats());
        set_respawn_on_missed_beats(false);
        assert!(!respawn_on_missed_beats());
    }

    #[test]
    fn watched_exit_reason_labels() {
        let stuck: WatchedExit<()> = WatchedExit::MissedBeats;
        assert_eq!(stuck.reason(), MISSED_BEATS_REASON);
    }

    fn far_future() -> Instant {
        Instant::now() + Duration::from_secs(86_400)
    }

    #[tokio::test]
    async fn await_watched_returns_joined_when_the_task_ends() {
        let handle = tokio::spawn(async { 7u32 });
        let exit = await_watched(WatchedTask::SealWriter, handle).await;
        assert_eq!(exit.reason(), "clean_exit");
        match exit {
            WatchedExit::Joined(Ok(v)) => assert_eq!(v, 7),
            other => panic!("expected Joined(Ok), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_await_watched_with_aborts_a_stuck_task_when_respawn_is_on() {
        let (dropped_tx, dropped_rx) = tokio::sync::oneshot::channel::<()>();
        let handle = tokio::spawn(async move {
            // Held until the task is dropped by the abort.
            let _guard = dropped_tx;
            std::future::pending::<()>().await;
        });
        let exit = await_watched_with(
            WatchedTask::OomMonitor,
            handle,
            true,
            Duration::from_millis(5),
            far_future,
        )
        .await;
        assert!(matches!(exit, WatchedExit::MissedBeats));
        // The aborted task really stopped: its guard was dropped.
        assert!(dropped_rx.await.is_err());
    }

    #[tokio::test]
    async fn a_stuck_task_keeps_running_when_respawn_is_off() {
        let handle = tokio::spawn(std::future::pending::<()>());
        let watched = tokio::spawn(await_watched_with(
            WatchedTask::WalSuspensionWatcher,
            handle,
            false,
            Duration::from_millis(5),
            far_future,
        ));
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert!(
            !watched.is_finished(),
            "respawn off: the supervisor keeps waiting"
        );
        watched.abort();
    }

    #[tokio::test]
    async fn the_seal_writer_is_never_aborted_even_with_respawn_on() {
        let handle = tokio::spawn(std::future::pending::<()>());
        let watched = tokio::spawn(await_watched_with(
            WatchedTask::SealWriter,
            handle,
            true,
            Duration::from_millis(5),
            far_future,
        ));
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert!(!watched.is_finished(), "the seal writer is watch-only");
        watched.abort();
    }

    #[tokio::test]
    async fn a_healthy_task_is_never_aborted() {
        let handle = tokio::spawn(std::future::pending::<()>());
        let watched = tokio::spawn(await_watched_with(
            WatchedTask::ResourceMonitor,
            handle,
            true,
            Duration::from_millis(5),
            // Judged at the arm instant: age 0, never stuck.
            Instant::now,
        ));
        tokio::time::sleep(Duration::from_millis(60)).await;
        assert!(!watched.is_finished());
        watched.abort();
    }

    #[test]
    fn test_arm_at_age_secs_at_is_stale_at_on_the_real_slot() {
        // SealWriter: no test in this binary spawns the seal writer loop
        // against this slot's timing assumptions (a stray beat only makes
        // the slot fresher, which these asserts tolerate).
        let task = WatchedTask::SealWriter;
        let t0 = Instant::now();
        arm_at(task, t0);
        assert!(age_secs_at(task, t0).is_some());
        let stuck = t0 + Duration::from_secs(task.max_silence_secs() + 60);
        assert!(is_stale_at(task, stuck));
        beat(task);
        assert!(!is_stale_at(task, Instant::now()));
    }

    #[test]
    fn note_respawn_arms_the_slot() {
        let task = WatchedTask::ApiTokenReload;
        note_respawn(task, MISSED_BEATS_REASON);
        assert!(age_secs_at(task, Instant::now()).is_some());
    }
}
