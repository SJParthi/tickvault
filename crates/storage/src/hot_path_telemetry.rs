//! Real-time proof that nothing on the live path is waiting (2026-10-02).
//!
//! Three instruments, all Prometheus-only (no CloudWatch metric, no alarm,
//! no Telegram page — those need a dated owner quote):
//!
//! 1. **Per-stage latency** — a fixed power-of-two histogram per hot-path
//!    stage, recorded with four relaxed atomic operations and ZERO heap
//!    allocation, published as `tv_hot_path_stage_ns_bucket{stage,le}` /
//!    `_count` / `_sum` so `histogram_quantile()` works on it unchanged.
//! 2. **Stall counters** — every sample above its stage's budget also bumps
//!    `tv_hot_path_stage_over_budget_total{stage}`, and the worst sample of
//!    each publish window is `tv_hot_path_stage_max_ns{stage}`.
//! 3. **Task heartbeats** — `tv_task_heartbeat_age_seconds{task}` (time since
//!    the task last made progress) and `tv_task_busy_seconds{task}` (how long
//!    a writer thread has been inside ONE batch; 0 while idle). A task that
//!    stops making progress shows up within one publish interval.
//!
//! # Why a hand-rolled histogram and not `metrics::histogram!`
//!
//! The Prometheus exporter's histogram appends every sample to a lock-free
//! block list that allocates a new block as each one fills — an allocation
//! every few dozen samples, on the per-frame path. These buckets are a fixed
//! array of `AtomicU64`: the record path can not allocate, and the DHAT test
//! `dhat_hot_path_telemetry` proves it.
//!
//! # Why the publisher is an OS thread
//!
//! A liveness signal must not depend on the health of the thing it reports
//! on (the same reasoning as the systemd watchdog pinger in `main`, moved off
//! tokio on 2026-09-01 after a starved runtime let it miss twelve beats). If
//! every tokio worker is wedged, this thread still publishes, and the ages it
//! publishes are what show the wedge.
//!
//! # Runtime-lag probes
//!
//! [`run_runtime_probe`] sleeps a fixed interval on a tokio runtime and
//! records how LATE it woke. A late wake is time the runtime could not poll
//! anything — including the socket readers — so it is the direct measure of
//! scheduler starvation. One probe runs on the main runtime and one on the
//! dedicated reader runtime.
//!
//! # Complexity
//! | Path | Cost |
//! |---|---|
//! | [`record_stage_nanos`] | O(1): one `leading_zeros`, four relaxed atomics |
//! | [`beat_at`] / [`busy_begin_at`] / [`busy_end_at`] | O(1): one `Instant` subtraction, one or two relaxed stores |
//! | [`publish_once`] | O(stages × buckets + tasks) = O(4 × 28 + 6), once a second, on its own thread |

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Number of latency buckets per stage. Bucket `k` holds samples in
/// `(2^(k+9), 2^(k+10)]` nanoseconds; bucket 0 holds everything up to
/// 1,024 ns and the last bucket holds everything above ~68.7 s (it is
/// published as `le="+Inf"`).
pub const STAGE_BUCKETS: usize = 28;

/// The log2 of bucket 0's upper bound (1,024 ns).
const BUCKET_BASE_SHIFT: u32 = 10;

/// How often the publisher thread refreshes every series.
// APPROVED: this line IS the named constant the no-hardcoded-Duration rule asks for.
pub const PUBLISH_INTERVAL: Duration = Duration::from_secs(1);

/// How long each runtime probe sleeps between wakes.
// APPROVED: this line IS the named constant the no-hardcoded-Duration rule asks for.
pub const RUNTIME_PROBE_INTERVAL: Duration = Duration::from_millis(100);

/// One hot-path stage. The discriminant is the array index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// Socket read task: frame in hand → WAL hand-off and ring hand-off done
    /// (`WalRingSink::accept`).
    SocketToWal = 0,
    /// Frame waiting in the ring: ring hand-off → drain picks it up.
    RingDwell = 1,
    /// How late a 100 ms sleep on the MAIN tokio runtime woke.
    MainRuntimeLag = 2,
    /// How late a 100 ms sleep on the socket READER runtime woke.
    ReaderRuntimeLag = 3,
}

/// Every stage, in index order.
pub const ALL_STAGES: [Stage; 4] = [
    Stage::SocketToWal,
    Stage::RingDwell,
    Stage::MainRuntimeLag,
    Stage::ReaderRuntimeLag,
];

const STAGE_COUNT: usize = ALL_STAGES.len();

impl Stage {
    /// The `stage` label value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SocketToWal => "socket_to_wal",
            Self::RingDwell => "ring_dwell",
            Self::MainRuntimeLag => "main_runtime_lag",
            Self::ReaderRuntimeLag => "reader_runtime_lag",
        }
    }

    /// A sample above this many nanoseconds counts as a stall.
    ///
    /// * socket → WAL 100 µs: the step is two `try_send`s; anything near this
    ///   is the reader being descheduled.
    /// * ring dwell 100 ms: a frame waited a tenth of a second for the drain.
    /// * runtime lag 10 ms: a runtime that could not run a ready task for
    ///   10 ms could not poll a socket for 10 ms either.
    #[must_use]
    pub const fn stall_budget_nanos(self) -> u64 {
        match self {
            Self::SocketToWal => 100_000,
            Self::RingDwell => 100_000_000,
            Self::MainRuntimeLag | Self::ReaderRuntimeLag => 10_000_000,
        }
    }
}

/// One long-lived task whose progress is published as a heartbeat age.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HotTask {
    /// The main tokio runtime's probe task.
    MainRuntime = 0,
    /// The socket reader runtime's probe task.
    ReaderRuntime = 1,
    /// Any socket read task accepting a frame (age = time since the last
    /// frame from ANY socket; it grows outside market hours by design).
    WsReader = 2,
    /// The frame drain (beats on every frame and every timer arm).
    FrameDrain = 3,
    /// The `tv-tick-writer` ILP offload thread.
    TickWriter = 4,
    /// The `tv-depth-writer` ILP offload thread.
    DepthWriter = 5,
}

/// Every task, in index order.
pub const ALL_TASKS: [HotTask; 6] = [
    HotTask::MainRuntime,
    HotTask::ReaderRuntime,
    HotTask::WsReader,
    HotTask::FrameDrain,
    HotTask::TickWriter,
    HotTask::DepthWriter,
];

const TASK_COUNT: usize = ALL_TASKS.len();

impl HotTask {
    /// The `task` label value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::MainRuntime => "main_runtime",
            Self::ReaderRuntime => "reader_runtime",
            Self::WsReader => "ws_reader",
            Self::FrameDrain => "frame_drain",
            Self::TickWriter => "tick_writer",
            Self::DepthWriter => "depth_writer",
        }
    }
}

struct StageStats {
    buckets: [AtomicU64; STAGE_BUCKETS],
    sum_nanos: AtomicU64,
    max_nanos: AtomicU64,
    over_budget: AtomicU64,
}

impl StageStats {
    const fn new() -> Self {
        Self {
            buckets: [const { AtomicU64::new(0) }; STAGE_BUCKETS],
            sum_nanos: AtomicU64::new(0),
            max_nanos: AtomicU64::new(0),
            over_budget: AtomicU64::new(0),
        }
    }
}

struct TaskBeat {
    /// Nanoseconds since [`anchor`] of the last progress, plus one; 0 means
    /// the task has never beaten.
    last: AtomicU64,
    /// Nanoseconds since [`anchor`] at which the current batch began, plus
    /// one; 0 means idle.
    busy_since: AtomicU64,
}

impl TaskBeat {
    const fn new() -> Self {
        Self {
            last: AtomicU64::new(0),
            busy_since: AtomicU64::new(0),
        }
    }
}

static STAGES: [StageStats; STAGE_COUNT] = [const { StageStats::new() }; STAGE_COUNT];
static TASKS: [TaskBeat; TASK_COUNT] = [const { TaskBeat::new() }; TASK_COUNT];
static ANCHOR: OnceLock<Instant> = OnceLock::new();

fn anchor() -> Instant {
    *ANCHOR.get_or_init(Instant::now)
}

/// `at` as nanoseconds since the process anchor, plus one so 0 stays the
/// "never" sentinel. Saturates instead of wrapping.
fn stamp(at: Instant) -> u64 {
    let nanos = at.saturating_duration_since(anchor()).as_nanos();
    u64::try_from(nanos)
        .unwrap_or(u64::MAX - 1)
        .saturating_add(1)
}

/// The bucket a sample of `nanos` lands in.
#[must_use]
pub const fn bucket_index(nanos: u64) -> usize {
    if nanos <= (1 << BUCKET_BASE_SHIFT) {
        return 0;
    }
    // ceil(log2(nanos)) - BUCKET_BASE_SHIFT, for nanos > 1024.
    let ceil_log2 = 64 - (nanos - 1).leading_zeros();
    let idx = ceil_log2 - BUCKET_BASE_SHIFT;
    if idx as usize >= STAGE_BUCKETS {
        STAGE_BUCKETS - 1
    } else {
        idx as usize
    }
}

/// The inclusive upper bound, in nanoseconds, of bucket `k`; `None` for the
/// last (unbounded) bucket.
#[must_use]
pub const fn bucket_upper_bound_nanos(k: usize) -> Option<u64> {
    if k + 1 >= STAGE_BUCKETS {
        None
    } else {
        Some(1u64 << (k as u32 + BUCKET_BASE_SHIFT))
    }
}

/// Records one sample. O(1), zero allocation, lock-free.
#[inline]
pub fn record_stage_nanos(stage: Stage, nanos: u64) {
    let s = &STAGES[stage as usize];
    s.buckets[bucket_index(nanos)].fetch_add(1, Ordering::Relaxed);
    s.sum_nanos.fetch_add(nanos, Ordering::Relaxed);
    s.max_nanos.fetch_max(nanos, Ordering::Relaxed);
    if nanos > stage.stall_budget_nanos() {
        s.over_budget.fetch_add(1, Ordering::Relaxed);
    }
}

/// Records one sample given as a `Duration`.
#[inline]
pub fn record_stage(stage: Stage, elapsed: Duration) {
    record_stage_nanos(stage, u64::try_from(elapsed.as_nanos()).unwrap_or(u64::MAX));
}

/// The task made progress at `at`. O(1), zero allocation.
#[inline]
pub fn beat_at(task: HotTask, at: Instant) {
    TASKS[task as usize]
        .last
        .store(stamp(at), Ordering::Relaxed);
}

/// The task made progress now.
#[inline]
pub fn beat(task: HotTask) {
    beat_at(task, Instant::now());
}

/// A writer thread starts one batch at `at`.
#[inline]
pub fn busy_begin_at(task: HotTask, at: Instant) {
    TASKS[task as usize]
        .busy_since
        .store(stamp(at), Ordering::Relaxed);
}

/// A writer thread finished its batch at `at`: idle again, and that is
/// progress.
#[inline]
pub fn busy_end_at(task: HotTask, at: Instant) {
    let t = &TASKS[task as usize];
    t.busy_since.store(0, Ordering::Relaxed);
    t.last.store(stamp(at), Ordering::Relaxed);
}

/// A point-in-time copy of one stage, for tests and the publisher.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StageSnapshot {
    /// Per-bucket (NOT cumulative) counts.
    pub buckets: [u64; STAGE_BUCKETS],
    /// Total samples.
    pub count: u64,
    /// Sum of every sample, nanoseconds.
    pub sum_nanos: u64,
    /// Samples over [`Stage::stall_budget_nanos`].
    pub over_budget: u64,
}

/// Reads one stage without resetting anything.
#[must_use]
pub fn stage_snapshot(stage: Stage) -> StageSnapshot {
    let s = &STAGES[stage as usize];
    let mut buckets = [0u64; STAGE_BUCKETS];
    let mut count = 0u64;
    for (out, b) in buckets.iter_mut().zip(s.buckets.iter()) {
        *out = b.load(Ordering::Relaxed);
        count = count.saturating_add(*out);
    }
    StageSnapshot {
        buckets,
        count,
        sum_nanos: s.sum_nanos.load(Ordering::Relaxed),
        over_budget: s.over_budget.load(Ordering::Relaxed),
    }
}

/// Seconds since `task` last made progress at `now`; `None` if it never has.
#[must_use]
pub fn heartbeat_age_at(task: HotTask, now: Instant) -> Option<f64> {
    let last = TASKS[task as usize].last.load(Ordering::Relaxed);
    if last == 0 {
        return None;
    }
    Some(nanos_to_secs(stamp(now).saturating_sub(last)))
}

/// Seconds `task` has been inside its current batch at `now`; 0 while idle.
#[must_use]
pub fn busy_seconds_at(task: HotTask, now: Instant) -> f64 {
    let since = TASKS[task as usize].busy_since.load(Ordering::Relaxed);
    if since == 0 {
        return 0.0;
    }
    nanos_to_secs(stamp(now).saturating_sub(since))
}

fn nanos_to_secs(nanos: u64) -> f64 {
    // Whole microseconds through u32 is lossless up to ~71 minutes, which is
    // far past any age worth reading; beyond that it saturates.
    let micros = u32::try_from(nanos / 1_000).unwrap_or(u32::MAX);
    // DATA-INTEGRITY-EXEMPT: a duration in microseconds, not price data
    f64::from(micros) / 1_000_000.0
}

fn nanos_to_f64(nanos: u64) -> f64 {
    // Same lossless-u32 shape; a max above ~4.3 s saturates, which is
    // already a stall the over-budget counter has counted.
    // DATA-INTEGRITY-EXEMPT: a duration in nanoseconds, not price data
    f64::from(u32::try_from(nanos).unwrap_or(u32::MAX))
}

struct StageHandles {
    buckets: Vec<metrics::Counter>,
    count: metrics::Counter,
    sum: metrics::Counter,
    over_budget: metrics::Counter,
    max: metrics::Gauge,
}

struct TaskHandles {
    age: metrics::Gauge,
    busy: metrics::Gauge,
}

/// Every Prometheus handle, resolved once. Built AFTER the recorder is
/// installed (a handle resolved before it is a no-op forever).
pub struct TelemetryHandles {
    stages: Vec<StageHandles>,
    tasks: Vec<TaskHandles>,
}

const BUCKET_LABELS: [&str; STAGE_BUCKETS] = [
    "1024",
    "2048",
    "4096",
    "8192",
    "16384",
    "32768",
    "65536",
    "131072",
    "262144",
    "524288",
    "1048576",
    "2097152",
    "4194304",
    "8388608",
    "16777216",
    "33554432",
    "67108864",
    "134217728",
    "268435456",
    "536870912",
    "1073741824",
    "2147483648",
    "4294967296",
    "8589934592",
    "17179869184",
    "34359738368",
    "68719476736",
    "+Inf",
];

impl TelemetryHandles {
    /// Resolves every handle. Cold: once, at boot.
    #[must_use]
    pub fn resolve() -> Self {
        let stages = ALL_STAGES
            .iter()
            .map(|stage| {
                let name = stage.as_str();
                StageHandles {
                    buckets: BUCKET_LABELS
                        .iter()
                        .map(|le| {
                            metrics::counter!(
                                "tv_hot_path_stage_ns_bucket",
                                "stage" => name,
                                "le" => *le
                            )
                        })
                        .collect(),
                    count: metrics::counter!("tv_hot_path_stage_ns_count", "stage" => name),
                    sum: metrics::counter!("tv_hot_path_stage_ns_sum", "stage" => name),
                    over_budget: metrics::counter!(
                        "tv_hot_path_stage_over_budget_total",
                        "stage" => name
                    ),
                    max: metrics::gauge!("tv_hot_path_stage_max_ns", "stage" => name),
                }
            })
            .collect();
        let tasks = ALL_TASKS
            .iter()
            .map(|task| {
                let name = task.as_str();
                TaskHandles {
                    age: metrics::gauge!("tv_task_heartbeat_age_seconds", "task" => name),
                    busy: metrics::gauge!("tv_task_busy_seconds", "task" => name),
                }
            })
            .collect();
        Self { stages, tasks }
    }
}

/// Publishes every series once. Resets each stage's window maximum.
pub fn publish_once(handles: &TelemetryHandles, now: Instant) {
    for (stage, h) in ALL_STAGES.iter().zip(handles.stages.iter()) {
        let snap = stage_snapshot(*stage);
        let mut cumulative = 0u64;
        for (count, counter) in snap.buckets.iter().zip(h.buckets.iter()) {
            cumulative = cumulative.saturating_add(*count);
            counter.absolute(cumulative);
        }
        h.count.absolute(snap.count);
        h.sum.absolute(snap.sum_nanos);
        h.over_budget.absolute(snap.over_budget);
        let max = STAGES[*stage as usize].max_nanos.swap(0, Ordering::Relaxed);
        h.max.set(nanos_to_f64(max));
    }
    for (task, h) in ALL_TASKS.iter().zip(handles.tasks.iter()) {
        // A task that has never beaten publishes nothing: an absent series
        // reads "not running here" (a depth writer with depth off), while a
        // stale one reads "stopped". Publishing 0 would be a false OK.
        if let Some(age) = heartbeat_age_at(*task, now) {
            h.age.set(age);
        }
        h.busy.set(busy_seconds_at(*task, now));
    }
}

/// Starts the publisher thread (`tv-telemetry`). Call once, after the
/// metrics recorder is installed.
///
/// # Errors
/// The OS refused to create the thread.
pub fn spawn_publisher() -> std::io::Result<std::thread::JoinHandle<()>> {
    // Fix the anchor before any stamp is taken by a racing task.
    let _ = anchor();
    let handles = TelemetryHandles::resolve();
    std::thread::Builder::new()
        .name("tv-telemetry".to_owned())
        .spawn(move || {
            loop {
                publish_once(&handles, Instant::now());
                // APPROVED-BLOCKING: this is the publisher's own dedicated OS thread; sleeping here blocks nothing else, and it is why the publisher survives a wedged tokio runtime.
                std::thread::sleep(PUBLISH_INTERVAL);
            }
        })
}

/// Sleeps [`RUNTIME_PROBE_INTERVAL`] on the current tokio runtime
/// `iterations` times, recording each wake's lateness under `stage` and
/// beating `task`. Production passes `u64::MAX`.
pub async fn run_runtime_probe(task: HotTask, stage: Stage, iterations: u64) {
    for _ in 0..iterations {
        let started = Instant::now();
        tokio::time::sleep(RUNTIME_PROBE_INTERVAL).await;
        let woke = Instant::now();
        record_stage(
            stage,
            woke.saturating_duration_since(started)
                .saturating_sub(RUNTIME_PROBE_INTERVAL),
        );
        beat_at(task, woke);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bucket_index_puts_each_sample_under_its_upper_bound() {
        assert_eq!(bucket_index(0), 0);
        assert_eq!(bucket_index(1024), 0);
        assert_eq!(bucket_index(1025), 1);
        assert_eq!(bucket_index(2048), 1);
        assert_eq!(bucket_index(2049), 2);
        assert_eq!(bucket_index(u64::MAX), STAGE_BUCKETS - 1);
        for nanos in [1u64, 999, 5_000, 123_456, 9_999_999, 1 << 35, (1 << 36) + 1] {
            let k = bucket_index(nanos);
            if let Some(upper) = bucket_upper_bound_nanos(k) {
                assert!(nanos <= upper, "{nanos} above bucket {k} bound {upper}");
            }
            if k > 0 {
                let lower = bucket_upper_bound_nanos(k - 1).expect("bounded");
                assert!(nanos > lower, "{nanos} belongs below bucket {k}");
            }
        }
    }

    #[test]
    fn bucket_upper_bound_nanos_matches_the_published_le_labels() {
        for (k, label) in BUCKET_LABELS.iter().enumerate() {
            match bucket_upper_bound_nanos(k) {
                Some(bound) => assert_eq!(label.parse::<u64>().expect("numeric"), bound),
                None => assert_eq!(*label, "+Inf"),
            }
        }
    }

    #[test]
    fn test_record_stage_nanos_and_stage_snapshot_count_samples_sum_and_stalls() {
        // Stage `SocketToWal` is recorded only by the core crate, so only by this test inside this module's
        // test binary, so the deltas below are exact.
        let before = stage_snapshot(Stage::SocketToWal);
        record_stage_nanos(Stage::SocketToWal, 1_000);
        record_stage(Stage::SocketToWal, Duration::from_micros(150));
        let after = stage_snapshot(Stage::SocketToWal);
        assert_eq!(after.count - before.count, 2);
        assert_eq!(after.sum_nanos - before.sum_nanos, 151_000);
        assert_eq!(
            after.over_budget - before.over_budget,
            1,
            "150 µs is over the 100 µs reader budget, 1 µs is not"
        );
        assert_eq!(after.buckets[0] - before.buckets[0], 1);
    }

    #[test]
    fn test_beat_at_and_heartbeat_age_at_measure_time_since_progress() {
        // `WsReader` is beaten only by the core crate, never inside this
        // test binary, so no sibling test can move it under our feet.
        let t0 = Instant::now();
        beat_at(HotTask::WsReader, t0);
        let age =
            heartbeat_age_at(HotTask::WsReader, t0 + Duration::from_millis(1_500)).expect("beaten");
        assert!((age - 1.5).abs() < 0.001, "age {age}");
        beat(HotTask::WsReader);
        assert!(heartbeat_age_at(HotTask::WsReader, Instant::now()).expect("beaten") < 1.0);
    }

    #[test]
    fn test_busy_begin_at_busy_end_at_and_busy_seconds_at_report_a_stuck_batch_and_then_idle() {
        let t0 = Instant::now();
        busy_begin_at(HotTask::DepthWriter, t0);
        let busy = busy_seconds_at(HotTask::DepthWriter, t0 + Duration::from_secs(3));
        assert!((busy - 3.0).abs() < 0.001, "busy {busy}");
        busy_end_at(HotTask::DepthWriter, t0 + Duration::from_secs(3));
        assert_eq!(busy_seconds_at(HotTask::DepthWriter, Instant::now()), 0.0);
        assert!(heartbeat_age_at(HotTask::DepthWriter, t0 + Duration::from_secs(3)).is_some());
    }

    #[test]
    fn a_task_that_never_beat_has_no_age() {
        // `ReaderRuntime` is never beaten by this module's tests.
        assert_eq!(
            heartbeat_age_at(HotTask::ReaderRuntime, Instant::now()),
            None
        );
    }

    #[test]
    fn publish_once_runs_without_a_recorder_and_resets_the_window_max() {
        record_stage_nanos(Stage::RingDwell, 7_777);
        let handles = TelemetryHandles::resolve();
        publish_once(&handles, Instant::now());
        assert_eq!(
            STAGES[Stage::RingDwell as usize]
                .max_nanos
                .load(Ordering::Relaxed),
            0
        );
    }

    #[test]
    fn spawn_publisher_starts_a_named_thread() {
        let handle = spawn_publisher().expect("thread");
        assert_eq!(handle.thread().name(), Some("tv-telemetry"));
    }

    #[tokio::test]
    async fn run_runtime_probe_records_lag_and_beats() {
        let before = stage_snapshot(Stage::MainRuntimeLag).count;
        run_runtime_probe(HotTask::MainRuntime, Stage::MainRuntimeLag, 2).await;
        assert_eq!(stage_snapshot(Stage::MainRuntimeLag).count - before, 2);
        assert!(heartbeat_age_at(HotTask::MainRuntime, Instant::now()).is_some());
    }

    #[test]
    fn every_stage_and_task_label_is_distinct() {
        let mut s: Vec<&str> = ALL_STAGES.iter().map(|x| x.as_str()).collect();
        s.sort_unstable();
        s.dedup();
        assert_eq!(s.len(), ALL_STAGES.len());
        let mut t: Vec<&str> = ALL_TASKS.iter().map(|x| x.as_str()).collect();
        t.sort_unstable();
        t.dedup();
        assert_eq!(t.len(), ALL_TASKS.len());
        for (i, st) in ALL_STAGES.iter().enumerate() {
            assert_eq!(*st as usize, i);
        }
        for (i, tk) in ALL_TASKS.iter().enumerate() {
            assert_eq!(*tk as usize, i);
        }
    }
}
