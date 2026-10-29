//! Real-time proof that nothing on the live path is waiting (2026-10-02).
//!
//! Three instruments, all Prometheus-only, plus one phone page (below):
//!
//! 1. **Per-stage latency** — a fixed power-of-two histogram per hot-path
//!    stage, recorded with four relaxed atomic operations and ZERO heap
//!    allocation, published as `tv_hot_path_stage_ns_bucket{stage,le}` /
//!    `_count` / `_sum` so `histogram_quantile()` works on it unchanged.
//! 2. **Stall counters** — every sample above its stage's budget also bumps
//!    `tv_hot_path_stage_over_budget_total{stage}`, and the worst sample of
//!    each publish window is `tv_hot_path_stage_max_ns{stage}`.
//! 2b. **Per-second percentiles** (2026-10-26, plan item K5) — once a
//!    second the publisher takes each stage's buckets minus the previous
//!    read (this second's samples only) and publishes p50 / p99 / p99.9 as
//!    `tv_hot_path_stage_{p50,p99,p999}_ns{stage}`, local `/metrics` only
//!    (no EMF name, no filter, no alarm). Each figure is the UPPER bound of
//!    the bucket the quantile falls in, so it reads up to 2× high, never
//!    low ([`stage_quantiles`]). A p99 over the stage's stall budget, in a
//!    window of at least [`P99_WARN_MIN_SAMPLES`], writes one coded
//!    `warn!` per episode (log only, never paged).
//! 3. **Task heartbeats** — `tv_task_heartbeat_age_seconds{task}` (time since
//!    the task last made progress) and `tv_task_busy_seconds{task}` (how long
//!    a writer thread has been inside ONE batch; 0 while idle). A task that
//!    stops making progress shows up within one publish interval.
//!
//! # The stall page (`HOT-PATH-STALL-01`, owner-approved 2026-10-02)
//!
//! The publisher thread also runs [`StallAlarm`]: in session, a watched
//! heartbeat older than [`STALL_PAGE_SECS`] or a stage sample longer than it
//! starts an EPISODE, and the first episode signal writes ONE coded `error!`
//! line, which CloudWatch turns into a Telegram page. The signal re-arms when
//! it clears; at most one line per [`STALL_LINE_MIN_INTERVAL`], and episodes
//! that start and end inside that wait are counted into the next line. The
//! 100 µs / 100 ms / 10 ms / 1 ms stall budgets above stay dashboard-only: they
//! count small stalls, which would page all day.
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
//! | [`record_receipt_to_fold_nanos`] | O(1): the stage record plus one more relaxed add into the fine histogram |
//! | [`publish_once`] | O(stages × buckets + tasks) = O(7 × 28 + 6), plus O(58) for the fine `receipt_to_fold` histogram, once a second, on its own thread; the percentile windows add the same again and keep the previous read in fixed arrays (no allocation) |
//! | [`stage_quantiles`] / [`fine_quantiles`] | O(buckets) = O(28) / O(58), on the publisher thread only, never on the frame path |
//!
//! # Plan item DT0 (2026-10-29)
//!
//! Two stages are measurement only and never page (they sit past
//! `PAGED_STAGE_COUNT`): `receipt_to_fold` (a main-feed frame's receipt at
//! the socket read → the drain finished folding it, with a finer
//! quarter-octave histogram, `tv_hot_path_receipt_to_fold_fine_ns_bucket`
//! and `tv_hot_path_receipt_to_fold_fine_{p50,p99,p999}_ns`) and
//! `metrics_upkeep` (one run of the Prometheus exporter's upkeep on the main
//! runtime). The publisher thread also publishes the hot threads'
//! scheduling wait (`crate::hot_thread_sched`). `receipt_to_fold` covers
//! frames that took the live path only; ring-refused frames are on the ring
//! counters (see [`Stage::ReceiptToFold`]).
//!
//! These series are never paged and have no EMF name or metric filter, but
//! they are not confined to the box: the CloudWatch agent scrapes all of
//! `/metrics` once a minute, with no relabel filter, into the metrics log
//! group. DT0 added about 143 series (two stages of ~35 each, the 61 fine
//! ones, 12 scheduling ones); at ~150 bytes a line and ~8 hours a day that
//! is ~0.25 GB a month, about $0.15 to $0.30 (Assumed, not billed yet).

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
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
    /// The drain's work on ONE main-feed frame: decode every packet, fold
    /// it into the candles and append its row (plan item K6, 2026-10-26).
    /// Recorded once per main-feed frame; depth frames are not timed here.
    DrainMainFrame = 4,
    /// One main-feed frame, end to end inside this process: the read task's
    /// receipt instant → the drain finished folding it (plan item DT0,
    /// 2026-10-29). It spans `socket_to_wal`, `ring_dwell` and
    /// `drain_main_frame`, so a frame that took the LIVE path and took over
    /// 1 ms to reach RAM shows here once, whichever step held it. Recorded
    /// once per main-feed frame by [`record_receipt_to_fold_nanos`], which
    /// also fills the finer quarter-octave histogram below. Never paged.
    ///
    /// Not every tick: a frame the ring refused (frame-count or byte budget
    /// full, or the drain gone) is kept only in the WAL and reaches the fold
    /// later through refold or catch-up, the slowest path a tick can take,
    /// and records nothing here. Those frames are counted on
    /// `tv_dhan_ws_ring_full_total` and `tv_dhan_ws_ring_closed_total`
    /// (`endpoint="main_feed"`); read them beside this stage. In a window
    /// where either rose the tail is partly unmeasured, so this stage's
    /// p99.9 reads low for that window.
    ReceiptToFold = 5,
    /// One run of the Prometheus exporter's upkeep (it drains every
    /// histogram), which runs every 5 s on the MAIN tokio runtime, the one
    /// the frame drain shares (plan item DT0). Never paged.
    MetricsUpkeep = 6,
}

/// Every stage, in index order.
pub const ALL_STAGES: [Stage; 7] = [
    Stage::SocketToWal,
    Stage::RingDwell,
    Stage::MainRuntimeLag,
    Stage::ReaderRuntimeLag,
    Stage::DrainMainFrame,
    Stage::ReceiptToFold,
    Stage::MetricsUpkeep,
];

const STAGE_COUNT: usize = ALL_STAGES.len();

/// The stall page (`HOT-PATH-STALL-01`) reads only the first this-many
/// stages. `receipt_to_fold` and `metrics_upkeep` (DT0, 2026-10-29) are
/// measurement only: `receipt_to_fold` is the sum of three paged stages, so
/// paging on it could page where none of them reached 2 s, and no dated
/// noise-lock row authorizes a new page.
const PAGED_STAGE_COUNT: usize = 5;

const _: () = assert!(PAGED_STAGE_COUNT <= STAGE_COUNT);
const _: () = assert!(Stage::ReceiptToFold as usize >= PAGED_STAGE_COUNT);
const _: () = assert!(Stage::MetricsUpkeep as usize >= PAGED_STAGE_COUNT);

impl Stage {
    /// The `stage` label value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SocketToWal => "socket_to_wal",
            Self::RingDwell => "ring_dwell",
            Self::MainRuntimeLag => "main_runtime_lag",
            Self::ReaderRuntimeLag => "reader_runtime_lag",
            Self::DrainMainFrame => "drain_main_frame",
            Self::ReceiptToFold => "receipt_to_fold",
            Self::MetricsUpkeep => "metrics_upkeep",
        }
    }

    /// A sample above this many nanoseconds counts as a stall.
    ///
    /// * socket → WAL 100 µs: the step is two `try_send`s; anything near this
    ///   is the reader being descheduled.
    /// * ring dwell 100 ms: a frame waited a tenth of a second for the drain.
    /// * runtime lag 10 ms: a runtime that could not run a ready task for
    ///   10 ms could not poll a socket for 10 ms either.
    /// * main-feed frame 1 ms: about 1,100 packets of work at the measured
    ///   0.9 µs per packet; a frame that holds the drain this long delays
    ///   every frame queued behind it. A frame stacks at most
    ///   `MAX_PACKETS_PER_FRAME` packets, so a very large frame can pass this
    ///   honestly; the stall page (2 s) is far above either.
    /// * receipt → folded 1 ms: the owner's target for every tick
    ///   (2026-10-28, "less than 1 ms always").
    /// * metrics upkeep 1 ms: an upkeep this long held a main-runtime worker
    ///   the drain may have needed for that long.
    #[must_use]
    pub const fn stall_budget_nanos(self) -> u64 {
        match self {
            Self::SocketToWal => 100_000,
            Self::RingDwell => 100_000_000,
            Self::MainRuntimeLag | Self::ReaderRuntimeLag => 10_000_000,
            Self::DrainMainFrame | Self::ReceiptToFold | Self::MetricsUpkeep => 1_000_000,
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

/// One cache line per stage, and per task below (plan item J1, 2026-10-26).
/// The socket readers and the frame drain write their own entries on every
/// frame; packed side by side, two entries shared one 64-byte line, so each
/// write invalidated the other core's copy (false sharing). `align(64)`
/// gives every entry its own line (64 bytes is the line size on the
/// Graviton host and on x86). SPACE: `TaskBeat` grows 16 -> 64 bytes,
/// `StageStats` rounds up to a multiple of 64; both are fixed `static`
/// arrays, a few KiB in all.
#[repr(align(64))]
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

#[repr(align(64))]
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

/// Set once the process starts its shutdown. A planned stop inside the
/// session window winds the hot tasks down on purpose, so their heartbeats
/// age; the stall alarm treats the rest of the process as out of session
/// rather than paging that wind-down as a stall.
static SHUTTING_DOWN: AtomicBool = AtomicBool::new(false);

/// Marks the process as shutting down: the stall alarm stays silent from
/// here on. Idempotent; call it as soon as a stop signal is classified.
pub fn begin_shutdown() {
    SHUTTING_DOWN.store(true, Ordering::Relaxed);
}

/// True once [`begin_shutdown`] has been called.
#[must_use]
pub fn is_shutting_down() -> bool {
    SHUTTING_DOWN.load(Ordering::Relaxed)
}

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

/// Buckets in the finer `receipt_to_fold` histogram (plan item DT0,
/// 2026-10-29): bucket 0 holds samples up to 1,024 ns, then each power of
/// two from 2^10 to 2^24 ns (16.8 ms) is split into four equal parts, and the
/// last bucket holds everything above 2^24 ns. A quantile read from it is at
/// most 25% above the true value, where the stage histogram reads up to 2×:
/// between 10 µs and 1 ms, the range the owner's 1 ms target sits in, the
/// stage histogram has only seven buckets.
pub const FINE_BUCKETS: usize = 58;

/// Sub-buckets per power of two in the fine histogram, as a shift (4).
const FINE_SUB_SHIFT: u32 = 2;

/// The fine histogram's top bound is 2^this nanoseconds.
const FINE_TOP_EXP: u32 = 24;

const _: () = assert!(
    FINE_BUCKETS == 2 + ((FINE_TOP_EXP - BUCKET_BASE_SHIFT) as usize) * (1usize << FINE_SUB_SHIFT)
);

/// The fine bucket a `receipt_to_fold` sample of `nanos` lands in. O(1):
/// one `leading_zeros`, two shifts.
#[must_use]
pub const fn fine_bucket_index(nanos: u64) -> usize {
    if nanos <= (1 << BUCKET_BASE_SHIFT) {
        return 0;
    }
    // v in [2^e, 2^(e+1)); a sample is in a bucket when it is at most the
    // bucket's inclusive upper bound, i.e. when v is below it.
    let v = nanos - 1;
    let e = 63 - v.leading_zeros();
    if e >= FINE_TOP_EXP {
        return FINE_BUCKETS - 1;
    }
    let sub = ((v >> (e - FINE_SUB_SHIFT)) & ((1 << FINE_SUB_SHIFT) - 1)) as usize;
    1 + ((e - BUCKET_BASE_SHIFT) as usize) * (1 << FINE_SUB_SHIFT) + sub
}

/// The inclusive upper bound, in nanoseconds, of fine bucket `k`; `None`
/// for the last (unbounded) bucket.
#[must_use]
pub const fn fine_bucket_upper_bound_nanos(k: usize) -> Option<u64> {
    if k == 0 {
        return Some(1 << BUCKET_BASE_SHIFT);
    }
    if k + 1 >= FINE_BUCKETS {
        return None;
    }
    let j = k - 1;
    let e = BUCKET_BASE_SHIFT + (j >> FINE_SUB_SHIFT) as u32;
    let sub = (j & ((1 << FINE_SUB_SHIFT) - 1)) as u64;
    Some((1u64 << e) + (sub + 1) * (1u64 << (e - FINE_SUB_SHIFT)))
}

#[repr(align(64))]
struct FineStats {
    buckets: [AtomicU64; FINE_BUCKETS],
}

static RECEIPT_TO_FOLD_FINE: FineStats = FineStats {
    buckets: [const { AtomicU64::new(0) }; FINE_BUCKETS],
};

/// Records one main-feed frame's receipt → folded time: the
/// [`Stage::ReceiptToFold`] stage plus its fine histogram. O(1), five
/// relaxed atomics, zero allocation, lock-free. The drain calls it once per
/// main-feed frame, after the fold.
#[inline]
pub fn record_receipt_to_fold_nanos(nanos: u64) {
    record_stage_nanos(Stage::ReceiptToFold, nanos);
    RECEIPT_TO_FOLD_FINE.buckets[fine_bucket_index(nanos)].fetch_add(1, Ordering::Relaxed);
}

/// Per-bucket (NOT cumulative) counts of the fine `receipt_to_fold`
/// histogram, read without resetting anything.
#[must_use]
pub fn receipt_to_fold_fine_snapshot() -> [u64; FINE_BUCKETS] {
    let mut out = [0u64; FINE_BUCKETS];
    for (o, b) in out.iter_mut().zip(RECEIPT_TO_FOLD_FINE.buckets.iter()) {
        *o = b.load(Ordering::Relaxed);
    }
    out
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

/// The quantiles published per stage, as `(numerator, denominator)` of the
/// share of samples at or below each: p50, p99, p99.9.
const QUANTILE_FRACTIONS: [(u64, u64); 3] = [(1, 2), (99, 100), (999, 1_000)];

/// With fewer samples than this in one window the p99 warn stays quiet: the
/// p99 of under 100 samples is the window's largest sample, which
/// `tv_hot_path_stage_max_ns` and the stall page already report. The two
/// runtime-lag stages take 10 samples a second, so their p99 never warns.
pub const P99_WARN_MIN_SAMPLES: u64 = 100;

/// Where a quantile lies, at bucket resolution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuantileBound {
    /// At most this many nanoseconds: the UPPER bound of the bucket holding
    /// the quantile, so the true value is in `(bound / 2, bound]` (or
    /// `[0, 1,024]` for bucket 0). Conservative: never below the truth, up
    /// to twice above it.
    AtMost(u64),
    /// In the top bucket: above ~68.7 s, with no upper bound.
    Overflow,
}

impl QuantileBound {
    fn of_bucket(k: usize) -> Self {
        match bucket_upper_bound_nanos(k) {
            Some(n) => Self::AtMost(n),
            None => Self::Overflow,
        }
    }

    /// The gauge value: the bound in nanoseconds, `+Inf` for the top bucket.
    fn as_gauge_value(self) -> f64 {
        match self {
            Self::Overflow => f64::INFINITY,
            // Every bound is a power of two (`bucket_upper_bound_nanos`), so
            // its exponent rebuilds it exactly, with no lossy integer cast.
            Self::AtMost(n) => 2f64.powi(i32::try_from(n.trailing_zeros()).unwrap_or(i32::MAX)),
        }
    }

    /// The log field value: the bound in nanoseconds, `u64::MAX` for the top
    /// bucket.
    fn as_log_nanos(self) -> u64 {
        match self {
            Self::AtMost(n) => n,
            Self::Overflow => u64::MAX,
        }
    }
}

/// p50, p99 and p99.9 of one window of samples.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StageQuantiles {
    /// Samples in the window: the sum of the buckets passed in.
    pub samples: u64,
    /// Median.
    pub p50: QuantileBound,
    /// 99th percentile.
    pub p99: QuantileBound,
    /// 99.9th percentile.
    pub p999: QuantileBound,
}

/// `ceil(total × num / den)`, the 1-based rank of the sample at that
/// quantile; 0 only for an empty window. `num <= den`, so it never passes
/// `total`. u128 so the product cannot overflow.
fn quantile_rank(total: u64, num: u64, den: u64) -> u64 {
    let rank = (u128::from(total) * u128::from(num)).div_ceil(u128::from(den.max(1)));
    u64::try_from(rank).unwrap_or(total).max(1).min(total)
}

/// p50, p99 and p99.9 of one window, from per-bucket (NOT cumulative)
/// counts; `None` when the window holds no sample.
///
/// Each quantile is the UPPER bound of the bucket its rank falls in
/// ([`QuantileBound`]): a log2 histogram cannot say where inside a bucket a
/// sample was, so the figure is conservative and can read up to twice the
/// true value; it never reads low.
///
/// The total is the sum of `window` itself, never a count read separately:
/// the publisher reads the buckets one by one while the hot path keeps
/// recording, so a separately read count can disagree with them by the
/// samples that raced the read. Summing the buckets keeps every rank inside
/// the samples actually walked. O(buckets), no allocation.
#[must_use]
pub fn stage_quantiles(window: &[u64; STAGE_BUCKETS]) -> Option<StageQuantiles> {
    let (samples, found) = quantile_buckets(window)?;
    Some(StageQuantiles {
        samples,
        p50: QuantileBound::of_bucket(found[0]),
        p99: QuantileBound::of_bucket(found[1]),
        p999: QuantileBound::of_bucket(found[2]),
    })
}

/// The bucket each of p50 / p99 / p99.9 falls in, and the sample count, from
/// per-bucket counts; `None` for an empty window. O(buckets), no
/// allocation. Every rank is at most `samples`, which the walk reaches at
/// its last non-empty bucket, so every slot is set; the top bucket is the
/// safe (conservative) reading if that ever failed.
fn quantile_buckets(window: &[u64]) -> Option<(u64, [usize; 3])> {
    let samples = window.iter().fold(0u64, |acc, n| acc.saturating_add(*n));
    if samples == 0 {
        return None;
    }
    let ranks = QUANTILE_FRACTIONS.map(|(num, den)| quantile_rank(samples, num, den));
    let mut found: [Option<usize>; 3] = [None; 3];
    let mut cumulative = 0u64;
    for (k, count) in window.iter().enumerate() {
        cumulative = cumulative.saturating_add(*count);
        for (slot, rank) in found.iter_mut().zip(ranks.iter()) {
            if slot.is_none() && cumulative >= *rank {
                *slot = Some(k);
            }
        }
    }
    let top = window.len().saturating_sub(1);
    Some((samples, found.map(|slot| slot.unwrap_or(top))))
}

/// p50, p99 and p99.9 of one window of the fine `receipt_to_fold`
/// histogram, each the upper bound of its fine bucket in nanoseconds
/// (`None` = above the top bound, 2^24 ns). The true value is at most 25%
/// below it. O(buckets), no allocation.
#[must_use]
pub fn fine_quantiles(window: &[u64; FINE_BUCKETS]) -> Option<FineQuantiles> {
    let (samples, found) = quantile_buckets(window)?;
    Some(FineQuantiles {
        samples,
        p50: fine_bucket_upper_bound_nanos(found[0]),
        p99: fine_bucket_upper_bound_nanos(found[1]),
        p999: fine_bucket_upper_bound_nanos(found[2]),
    })
}

/// p50, p99 and p99.9 of one fine-histogram window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FineQuantiles {
    /// Samples in the window.
    pub samples: u64,
    /// Median bound, nanoseconds; `None` above 2^24 ns.
    pub p50: Option<u64>,
    /// 99th-percentile bound.
    pub p99: Option<u64>,
    /// 99.9th-percentile bound.
    pub p999: Option<u64>,
}

/// True when the window's p99 sample is above the stage budget. Exact, not
/// at bucket resolution: `over_budget` counts every sample above the budget,
/// so the p99 sample is over it when fewer than its rank are within it.
fn p99_over_budget(samples: u64, over_budget: u64) -> bool {
    samples.saturating_sub(over_budget) < quantile_rank(samples, 99, 100)
}

/// Edge latch for the p99 warn: true on the first over-budget window of an
/// episode. A window under budget ends the episode; a window with too few
/// samples to judge (`None`) neither starts nor ends one.
fn step_p99_latch(latched: &mut bool, over: Option<bool>) -> bool {
    match over {
        Some(true) if !*latched => {
            *latched = true;
            true
        }
        Some(false) => {
            *latched = false;
            false
        }
        _ => false,
    }
}

/// What one stage's window produced.
struct WindowOutcome {
    quantiles: Option<StageQuantiles>,
    over_budget: u64,
    starts_p99_episode: bool,
}

/// Per stage, what the quantile publish keeps between windows: the
/// cumulative buckets and over-budget count it last read, and whether the
/// p99 is inside an over-budget episode. Fixed arrays, built once; lives on
/// the publisher thread only.
struct QuantileWindows {
    prev_buckets: [[u64; STAGE_BUCKETS]; STAGE_COUNT],
    prev_over_budget: [u64; STAGE_COUNT],
    p99_over: [bool; STAGE_COUNT],
}

impl QuantileWindows {
    const fn new() -> Self {
        Self {
            prev_buckets: [[0; STAGE_BUCKETS]; STAGE_COUNT],
            prev_over_budget: [0; STAGE_COUNT],
            p99_over: [false; STAGE_COUNT],
        }
    }

    /// Turns the cumulative `snap` into this window's per-bucket counts
    /// (current minus last read), then computes its quantiles and steps the
    /// p99 latch. A sample recorded while the buckets were being read lands
    /// in this window or the next, never in both and never in neither.
    /// O(buckets), no allocation.
    fn advance(&mut self, stage: Stage, snap: &StageSnapshot) -> WindowOutcome {
        let i = stage as usize;
        let mut window = [0u64; STAGE_BUCKETS];
        for ((out, now), before) in window
            .iter_mut()
            .zip(snap.buckets.iter())
            .zip(self.prev_buckets[i].iter())
        {
            *out = now.saturating_sub(*before);
        }
        self.prev_buckets[i] = snap.buckets;
        let over_budget = snap.over_budget.saturating_sub(self.prev_over_budget[i]);
        self.prev_over_budget[i] = snap.over_budget;
        let quantiles = stage_quantiles(&window);
        let verdict = quantiles
            .filter(|q| q.samples >= P99_WARN_MIN_SAMPLES)
            .map(|q| p99_over_budget(q.samples, over_budget));
        let starts_p99_episode = step_p99_latch(&mut self.p99_over[i], verdict);
        WindowOutcome {
            quantiles,
            over_budget,
            starts_p99_episode,
        }
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
    // APPROVED: a duration in whole microseconds (u32), not an f32 price
    f64::from(micros) / 1_000_000.0
}

fn nanos_to_f64(nanos: u64) -> f64 {
    // Same lossless-u32 shape; a max above ~4.3 s saturates, which is
    // already a stall the over-budget counter has counted.
    // APPROVED: a duration in whole nanoseconds (u32), not an f32 price
    f64::from(u32::try_from(nanos).unwrap_or(u32::MAX))
}

struct StageHandles {
    buckets: Vec<metrics::Counter>,
    count: metrics::Counter,
    sum: metrics::Counter,
    over_budget: metrics::Counter,
    max: metrics::Gauge,
    p50: metrics::Gauge,
    p99: metrics::Gauge,
    p999: metrics::Gauge,
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
    windows: QuantileWindows,
    fine: FineHandles,
}

/// The fine `receipt_to_fold` histogram's series, and the counts it last
/// read (for the per-second quantiles). Publisher thread only.
struct FineHandles {
    buckets: Vec<metrics::Counter>,
    p50: metrics::Gauge,
    p99: metrics::Gauge,
    p999: metrics::Gauge,
    prev: [u64; FINE_BUCKETS],
}

impl FineHandles {
    fn resolve() -> Self {
        Self {
            buckets: (0..FINE_BUCKETS)
                .map(|k| {
                    // APPROVED: one label String per bucket, once at boot (cold)
                    let le = fine_bucket_upper_bound_nanos(k)
                        .map_or_else(|| String::from("+Inf"), |n| n.to_string());
                    metrics::counter!("tv_hot_path_receipt_to_fold_fine_ns_bucket", "le" => le)
                })
                .collect(),
            p50: metrics::gauge!("tv_hot_path_receipt_to_fold_fine_p50_ns"),
            p99: metrics::gauge!("tv_hot_path_receipt_to_fold_fine_p99_ns"),
            p999: metrics::gauge!("tv_hot_path_receipt_to_fold_fine_p999_ns"),
            prev: [0; FINE_BUCKETS],
        }
    }

    /// Publishes the cumulative fine buckets and this window's quantiles
    /// (NaN for an empty window, `+Inf` above the top bound). O(58), no
    /// allocation.
    fn publish(&mut self) {
        let now = receipt_to_fold_fine_snapshot();
        let mut window = [0u64; FINE_BUCKETS];
        let mut cumulative = 0u64;
        for (((count, counter), before), out) in now
            .iter()
            .zip(self.buckets.iter())
            .zip(self.prev.iter())
            .zip(window.iter_mut())
        {
            cumulative = cumulative.saturating_add(*count);
            counter.absolute(cumulative);
            *out = count.saturating_sub(*before);
        }
        self.prev = now;
        let gauge = |bound: Option<u64>| bound.map_or(f64::INFINITY, nanos_to_f64);
        match fine_quantiles(&window) {
            Some(q) => {
                self.p50.set(gauge(q.p50));
                self.p99.set(gauge(q.p99));
                self.p999.set(gauge(q.p999));
            }
            None => {
                self.p50.set(f64::NAN);
                self.p99.set(f64::NAN);
                self.p999.set(f64::NAN);
            }
        }
    }
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
                    p50: metrics::gauge!("tv_hot_path_stage_p50_ns", "stage" => name),
                    p99: metrics::gauge!("tv_hot_path_stage_p99_ns", "stage" => name),
                    p999: metrics::gauge!("tv_hot_path_stage_p999_ns", "stage" => name),
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
        Self {
            stages,
            tasks,
            windows: QuantileWindows::new(),
            fine: FineHandles::resolve(),
        }
    }
}

/// Publishes every series once. Resets each stage's window maximum and
/// returns what the stall alarm reads: each stage's window maximum and each
/// watched task's heartbeat age.
///
/// It also publishes each stage's p50 / p99 / p99.9 over the samples since
/// the previous call (one second in production; the first call covers
/// everything since boot) as `tv_hot_path_stage_{p50,p99,p999}_ns{stage}`,
/// local `/metrics` only. An empty window publishes NaN ("no samples this
/// second"), never 0, which would read as fast. A p99 above the stage's
/// [`Stage::stall_budget_nanos`] writes one log-only `warn!` per episode.
pub fn publish_once(handles: &mut TelemetryHandles, now: Instant) -> StallReadings {
    let mut readings = StallReadings {
        task_age_secs: [None; WATCHED_TASKS.len()],
        stage_max_nanos: [0; STAGE_COUNT],
    };
    for (slot, task) in readings.task_age_secs.iter_mut().zip(WATCHED_TASKS.iter()) {
        *slot = heartbeat_age_at(*task, now);
    }
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
        readings.stage_max_nanos[*stage as usize] = max;
        let outcome = handles.windows.advance(*stage, &snap);
        match outcome.quantiles {
            Some(q) => {
                h.p50.set(q.p50.as_gauge_value());
                h.p99.set(q.p99.as_gauge_value());
                h.p999.set(q.p999.as_gauge_value());
                if outcome.starts_p99_episode {
                    report_p99_over_budget(*stage, &q, outcome.over_budget);
                }
            }
            None => {
                h.p50.set(f64::NAN);
                h.p99.set(f64::NAN);
                h.p999.set(f64::NAN);
            }
        }
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
    handles.fine.publish();
    readings
}

/// Starts the publisher thread (`tv-telemetry`). Call once, after the
/// metrics recorder is installed.
///
/// # Errors
/// The OS refused to create the thread.
pub fn spawn_publisher() -> std::io::Result<std::thread::JoinHandle<()>> {
    // Fix the anchor before any stamp is taken by a racing task.
    let _ = anchor();
    let mut handles = TelemetryHandles::resolve();
    std::thread::Builder::new()
        .name("tv-telemetry".to_owned())
        .spawn(move || {
            let mut alarm = StallAlarm::new();
            let mut sched = crate::hot_thread_sched::SchedPublisher::new();
            loop {
                let now = Instant::now();
                let readings = publish_once(&mut handles, now);
                // DT0 (2026-10-29): the hot threads' scheduling wait, from
                // /proc, on this thread only (cold, once a second).
                let _published = sched.publish();
                let in_session = in_session_now() && !is_shutting_down();
                if let Some(line) = alarm.observe(now, in_session, &readings) {
                    report_stall(&line);
                }
                // APPROVED-BLOCKING: this is the publisher's own dedicated OS thread; sleeping here blocks nothing else, and it is why the publisher survives a wedged tokio runtime.
                std::thread::sleep(PUBLISH_INTERVAL);
            }
        })
}

/// A watched heartbeat this many seconds old, or one stage sample this long,
/// in session, pages. Two seconds is twenty of the runtime probe's 100 ms
/// beats and four of the drain's 500 ms flush beats, so a healthy task never
/// reaches it, while it is still far inside the 40 s Dhan waits before it
/// closes a socket that stopped answering pings: the page arrives while the
/// stall can still be acted on, not after the socket is gone.
pub const STALL_PAGE_SECS: u32 = 2;

/// [`STALL_PAGE_SECS`] in the unit the readings carry (seconds as `f64`).
/// Spelled out rather than converted at the compare site; the test
/// `test_stall_page_secs_f64_matches_the_integer_threshold` pins the two together.
const STALL_PAGE_SECS_F64: f64 = 2.0;

/// At most one `HOT-PATH-STALL-01` line per this interval.
// APPROVED: this line IS the named constant the no-hardcoded-Duration rule asks for.
pub const STALL_LINE_MIN_INTERVAL: Duration = Duration::from_secs(60);

/// Heartbeats the stall alarm watches. Excluded on purpose: the socket reader
/// heartbeat (it ages whenever no frame arrives, as before the open) and the
/// two ILP writer threads (they run off the socket path, behind a queue, and
/// a slow database already has its own pages).
pub const WATCHED_TASKS: [HotTask; 3] = [
    HotTask::MainRuntime,
    HotTask::ReaderRuntime,
    HotTask::FrameDrain,
];

const SIGNAL_COUNT: usize = WATCHED_TASKS.len() + PAGED_STAGE_COUNT;

/// What the stall alarm reads once per publish.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StallReadings {
    /// Heartbeat age of each [`WATCHED_TASKS`] entry; `None` if it never beat.
    pub task_age_secs: [Option<f64>; WATCHED_TASKS.len()],
    /// Each stage's largest sample in the window, nanoseconds.
    pub stage_max_nanos: [u64; STAGE_COUNT],
}

/// One `HOT-PATH-STALL-01` line's content.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StallLine {
    /// The worst newly stalled signal: a task or stage label.
    pub signal: &'static str,
    /// `"heartbeat"` or `"stage"`.
    pub kind: &'static str,
    /// How long that signal had stalled, seconds.
    pub stalled_secs: f64,
    /// Newly stalled signals this line reports (the worst is named).
    pub signals: usize,
    /// Episodes that started and ended while the line was rate-limited.
    pub missed_episodes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SignalState {
    /// Not stalled; the next stall starts a new episode.
    Clear,
    /// Stalled, not yet reported (waiting on the rate limit).
    Pending,
    /// Stalled and reported; silent until it clears.
    Reported,
}

/// Edge-triggered stall detector. Lives on the publisher thread only.
#[derive(Debug)]
pub struct StallAlarm {
    state: [SignalState; SIGNAL_COUNT],
    last_line: Option<Instant>,
    missed: u64,
}

impl Default for StallAlarm {
    fn default() -> Self {
        Self::new()
    }
}

impl StallAlarm {
    /// Every signal clear, nothing reported yet.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            state: [SignalState::Clear; SIGNAL_COUNT],
            last_line: None,
            missed: 0,
        }
    }

    /// Feeds one publish's readings. Returns the line to write, if any.
    /// Outside the session every signal re-arms and nothing is reported, so
    /// a stall at boot or after the close never pages. O(signals) = O(8):
    /// three heartbeats and the [`PAGED_STAGE_COUNT`] paged stages.
    pub fn observe(
        &mut self,
        now: Instant,
        in_session: bool,
        readings: &StallReadings,
    ) -> Option<StallLine> {
        if !in_session {
            self.state = [SignalState::Clear; SIGNAL_COUNT];
            return None;
        }
        let mut worst: Option<(usize, f64)> = None;
        let mut pending = 0usize;
        for (i, state) in self.state.iter_mut().enumerate() {
            let secs = signal_secs(readings, i);
            let stalled = secs.is_some_and(|s| s >= STALL_PAGE_SECS_F64);
            match (*state, stalled) {
                (SignalState::Clear, true) => *state = SignalState::Pending,
                (SignalState::Pending, false) => {
                    self.missed = self.missed.saturating_add(1);
                    *state = SignalState::Clear;
                }
                (SignalState::Reported, false) => *state = SignalState::Clear,
                _ => {}
            }
            if *state == SignalState::Pending {
                pending += 1;
                let s = secs.unwrap_or(0.0);
                if worst.is_none_or(|(_, w)| s > w) {
                    worst = Some((i, s));
                }
            }
        }
        let (index, stalled_secs) = worst?;
        let allowed = self
            .last_line
            .is_none_or(|at| now.saturating_duration_since(at) >= STALL_LINE_MIN_INTERVAL);
        if !allowed {
            return None;
        }
        for state in &mut self.state {
            if *state == SignalState::Pending {
                *state = SignalState::Reported;
            }
        }
        self.last_line = Some(now);
        let missed_episodes = std::mem::take(&mut self.missed);
        let (signal, kind) = signal_label(index);
        Some(StallLine {
            signal,
            kind,
            stalled_secs,
            signals: pending,
            missed_episodes,
        })
    }
}

/// Signal `i` in seconds: watched tasks first, then the paged stages.
/// A stage at or past [`PAGED_STAGE_COUNT`] is never a signal.
fn signal_secs(readings: &StallReadings, i: usize) -> Option<f64> {
    match readings.task_age_secs.get(i) {
        Some(age) => *age,
        None => {
            let stage = i - WATCHED_TASKS.len();
            if stage >= PAGED_STAGE_COUNT {
                return None;
            }
            readings
                .stage_max_nanos
                .get(stage)
                .map(|n| nanos_to_secs(*n))
        }
    }
}

fn signal_label(i: usize) -> (&'static str, &'static str) {
    match WATCHED_TASKS.get(i) {
        Some(task) => (task.as_str(), "heartbeat"),
        None => (
            ALL_STAGES
                .get(i - WATCHED_TASKS.len())
                .map_or("unknown", |s| s.as_str()),
            "stage",
        ),
    }
}

/// True inside the persist window `[09:00, 15:40)` IST for `secs_of_day`.
#[must_use]
pub fn is_in_session_secs_of_day(secs_of_day: u32) -> bool {
    (tickvault_common::constants::TICK_PERSIST_START_SECS_OF_DAY_IST
        ..tickvault_common::constants::TICK_PERSIST_END_SECS_OF_DAY_IST)
        .contains(&secs_of_day)
}

fn in_session_now() -> bool {
    let now_ist = chrono::Utc::now().timestamp().saturating_add(i64::from(
        tickvault_common::constants::IST_UTC_OFFSET_SECONDS,
    ));
    let secs = now_ist.rem_euclid(i64::from(tickvault_common::constants::SECONDS_PER_DAY));
    u32::try_from(secs).is_ok_and(is_in_session_secs_of_day)
}

/// Writes the `HOT-PATH-STALL-01` line (paged via CloudWatch → Telegram).
fn report_stall(line: &StallLine) {
    tracing::error!(
        code = tickvault_common::error_code::ErrorCode::HotPathStall01.code_str(),
        signal = line.signal,
        kind = line.kind,
        stalled_secs = line.stalled_secs,
        threshold_secs = STALL_PAGE_SECS,
        signals = line.signals,
        missed_episodes = line.missed_episodes,
        "Dhan live feed: a hot step stalled for at least {STALL_PAGE_SECS} s — the socket \
         reads may have waited and Dhan may have skipped ticks meanwhile"
    );
}

/// Writes one log-only line when a stage's p99 passes its budget for the
/// first window of an episode. `warn!`, not `error!`: the
/// `HOT-PATH-STALL-01` metric filter matches `level = ERROR` only, so this
/// line never pages (`source` tells it apart in triage).
fn report_p99_over_budget(stage: Stage, quantiles: &StageQuantiles, over_budget: u64) {
    tracing::warn!(
        code = tickvault_common::error_code::ErrorCode::HotPathStall01.code_str(),
        source = "stage_p99_over_budget",
        stage = stage.as_str(),
        p99_at_most_ns = quantiles.p99.as_log_nanos(),
        budget_ns = stage.stall_budget_nanos(),
        samples = quantiles.samples,
        over_budget,
        "Dhan live feed: more than 1 in 100 samples of one hot step took longer than its \
         budget in the last second (log only, no page)"
    );
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

    /// J1 (2026-10-26): every stage and task entry owns its own cache line,
    /// so per-frame writes from different cores never share one.
    #[test]
    fn test_telemetry_entries_are_cache_line_aligned() {
        assert_eq!(std::mem::align_of::<TaskBeat>(), 64);
        assert_eq!(std::mem::align_of::<StageStats>(), 64);
        assert_eq!(std::mem::size_of::<TaskBeat>() % 64, 0);
        assert_eq!(std::mem::size_of::<StageStats>() % 64, 0);
        // Neighbouring statics start on different lines.
        let a = std::ptr::addr_of!(TASKS[0]) as usize;
        let b = std::ptr::addr_of!(TASKS[1]) as usize;
        assert!(b - a >= 64);
        assert_eq!(a % 64, 0);
    }

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
    fn test_drain_main_frame_stage_label_budget_and_index() {
        // K6 (2026-10-26). `DrainMainFrame` is recorded only by the app
        // crate's drain, never inside this test binary, so the deltas are
        // exact.
        assert_eq!(Stage::DrainMainFrame.as_str(), "drain_main_frame");
        assert_eq!(Stage::DrainMainFrame.stall_budget_nanos(), 1_000_000);
        for (i, stage) in ALL_STAGES.iter().enumerate() {
            assert_eq!(*stage as usize, i, "ALL_STAGES must be in index order");
        }
        assert!(ALL_STAGES.contains(&Stage::DrainMainFrame));
        let before = stage_snapshot(Stage::DrainMainFrame);
        record_stage_nanos(Stage::DrainMainFrame, 1_000_000);
        record_stage_nanos(Stage::DrainMainFrame, 1_000_001);
        let after = stage_snapshot(Stage::DrainMainFrame);
        assert_eq!(after.count - before.count, 2);
        assert_eq!(
            after.over_budget - before.over_budget,
            1,
            "exactly the budget is not a stall, one nanosecond over is"
        );
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
        let mut handles = TelemetryHandles::resolve();
        publish_once(&mut handles, Instant::now());
        assert_eq!(
            STAGES[Stage::RingDwell as usize]
                .max_nanos
                .load(Ordering::Relaxed),
            0
        );
    }

    fn window_of(samples: &[(usize, u64)]) -> [u64; STAGE_BUCKETS] {
        let mut w = [0u64; STAGE_BUCKETS];
        for (k, n) in samples {
            w[*k] += *n;
        }
        w
    }

    #[test]
    fn test_stage_quantiles_empty_window_is_none() {
        assert_eq!(stage_quantiles(&[0; STAGE_BUCKETS]), None);
    }

    #[test]
    fn test_stage_quantiles_all_in_one_bucket_reads_that_bucket_upper_bound() {
        // 500 samples of 3,000 ns: bucket 2, bound 4,096.
        let mut w = [0u64; STAGE_BUCKETS];
        w[bucket_index(3_000)] = 500;
        let q = stage_quantiles(&w).expect("samples");
        assert_eq!(q.samples, 500);
        for b in [q.p50, q.p99, q.p999] {
            assert_eq!(b, QuantileBound::AtMost(4_096));
        }
        // A single sample is its own p50, p99 and p99.9.
        let one = stage_quantiles(&window_of(&[(0, 1)])).expect("one");
        assert_eq!(one.p50, QuantileBound::AtMost(1_024));
        assert_eq!(one.p999, QuantileBound::AtMost(1_024));
    }

    #[test]
    fn test_stage_quantiles_all_in_top_bucket_reads_overflow() {
        let w = window_of(&[(STAGE_BUCKETS - 1, 7)]);
        let q = stage_quantiles(&w).expect("samples");
        assert_eq!(q.p50, QuantileBound::Overflow);
        assert_eq!(q.p99, QuantileBound::Overflow);
        assert!(q.p99.as_gauge_value().is_infinite());
        assert_eq!(q.p999.as_log_nanos(), u64::MAX);
    }

    #[test]
    fn test_stage_quantiles_known_distribution_picks_the_ranked_bucket() {
        // 10,000 samples: 9,000 in bucket 0, 900 in bucket 5, 90 in bucket
        // 10, 10 in bucket 20. Ranks: p50 = 5,000 (bucket 0), p99 = 9,900
        // (cumulative 9,900 reached exactly at bucket 5), p99.9 = 9,990
        // (cumulative 9,990 at bucket 10).
        let w = window_of(&[(0, 9_000), (5, 900), (10, 90), (20, 10)]);
        let q = stage_quantiles(&w).expect("samples");
        assert_eq!(q.samples, 10_000);
        assert_eq!(q.p50, QuantileBound::AtMost(1 << 10));
        assert_eq!(q.p99, QuantileBound::AtMost(1 << 15));
        assert_eq!(q.p999, QuantileBound::AtMost(1 << 20));
        // One more slow sample pushes the 9,900th rank past bucket 5.
        let w2 = window_of(&[(0, 8_999), (5, 900), (10, 91), (20, 10)]);
        assert_eq!(
            stage_quantiles(&w2).expect("samples").p99,
            QuantileBound::AtMost(1 << 20)
        );
    }

    #[test]
    fn test_stage_quantiles_bound_is_never_below_the_true_sample() {
        // Every recorded value is at or below the bound of its quantile.
        for nanos in [0u64, 1, 1_024, 1_025, 99_999, 100_001, 1 << 30, u64::MAX] {
            let mut w = [0u64; STAGE_BUCKETS];
            w[bucket_index(nanos)] = 3;
            let q = stage_quantiles(&w).expect("samples");
            match q.p99 {
                QuantileBound::AtMost(bound) => {
                    assert!(nanos <= bound, "{nanos} above its bound {bound}");
                    assert!(
                        bound <= nanos.max(512).saturating_mul(2),
                        "bound {bound} more than 2x {nanos}"
                    );
                    assert_eq!(q.p99.as_gauge_value().to_bits(), {
                        let mut exact = 1f64;
                        for _ in 0..bound.trailing_zeros() {
                            exact *= 2.0;
                        }
                        exact.to_bits()
                    });
                }
                QuantileBound::Overflow => {
                    assert!(bucket_upper_bound_nanos(bucket_index(nanos)).is_none())
                }
            }
        }
    }

    #[test]
    fn test_stage_quantiles_uses_the_bucket_sum_not_a_racing_count() {
        // The snapshot's separately summed count can trail or lead the
        // buckets the walk reads (samples recorded mid-read). The function
        // takes only the buckets, so every rank stays inside the samples it
        // walks: a total of 2 can never ask for a third sample.
        let w = window_of(&[(3, 1), (9, 1)]);
        let q = stage_quantiles(&w).expect("samples");
        assert_eq!(q.samples, 2);
        assert_eq!(q.p50, QuantileBound::AtMost(1 << 13));
        assert_eq!(q.p99, QuantileBound::AtMost(1 << 19));
        // Saturating sums: buckets near u64::MAX do not wrap to a tiny total.
        let huge = window_of(&[(0, u64::MAX), (1, u64::MAX)]);
        assert_eq!(stage_quantiles(&huge).expect("samples").samples, u64::MAX);
    }

    #[test]
    fn test_quantile_rank_bounds() {
        assert_eq!(quantile_rank(0, 99, 100), 0);
        assert_eq!(quantile_rank(1, 1, 2), 1);
        assert_eq!(quantile_rank(100, 99, 100), 99);
        assert_eq!(quantile_rank(101, 99, 100), 100);
        assert_eq!(
            quantile_rank(u64::MAX, 999, 1_000),
            u64::MAX - u64::MAX / 1_000
        );
    }

    #[test]
    fn test_p99_over_budget_is_exact_on_the_counter() {
        // 100 samples: rank 99. Over iff fewer than 99 are within budget.
        assert!(!p99_over_budget(100, 1));
        assert!(p99_over_budget(100, 2));
        assert!(!p99_over_budget(0, 0));
        // A racing over-budget count larger than the window saturates.
        assert!(p99_over_budget(100, 500));
    }

    #[test]
    fn test_step_p99_latch_fires_once_per_episode_and_re_arms() {
        let mut latched = false;
        assert!(step_p99_latch(&mut latched, Some(true)));
        assert!(!step_p99_latch(&mut latched, Some(true)));
        // Too few samples: neither ends nor restarts the episode.
        assert!(!step_p99_latch(&mut latched, None));
        assert!(!step_p99_latch(&mut latched, Some(true)));
        assert!(!step_p99_latch(&mut latched, Some(false)));
        assert!(step_p99_latch(&mut latched, Some(true)));
    }

    #[test]
    fn test_quantile_windows_advance_reads_only_this_window_and_latches_once() {
        let mut windows = QuantileWindows::new();
        let stage = Stage::SocketToWal;
        let mut snap = StageSnapshot {
            buckets: [0; STAGE_BUCKETS],
            count: 0,
            sum_nanos: 0,
            over_budget: 0,
        };
        // Window 1: 1,000 fast samples (2 µs). Under budget.
        snap.buckets[bucket_index(2_000)] += 1_000;
        let first = windows.advance(stage, &snap);
        assert_eq!(first.quantiles.expect("w1").samples, 1_000);
        assert!(!first.starts_p99_episode);
        // Window 2: 200 slow samples (500 µs, over the 100 µs budget). Only
        // these 200 are in the window, so p99 is the slow bucket.
        snap.buckets[bucket_index(500_000)] += 200;
        snap.over_budget += 200;
        let second = windows.advance(stage, &snap);
        let q = second.quantiles.expect("w2");
        assert_eq!(q.samples, 200);
        assert_eq!(q.p50, QuantileBound::AtMost(1 << 19));
        assert_eq!(second.over_budget, 200);
        assert!(second.starts_p99_episode);
        // Window 3: same again, still one episode.
        snap.buckets[bucket_index(500_000)] += 200;
        snap.over_budget += 200;
        assert!(!windows.advance(stage, &snap).starts_p99_episode);
        // Window 4: empty. No quantiles, latch unchanged.
        let empty = windows.advance(stage, &snap);
        assert!(empty.quantiles.is_none());
        assert!(!empty.starts_p99_episode);
        // Window 5: healthy again re-arms; window 6 slow fires again.
        snap.buckets[bucket_index(2_000)] += 500;
        assert!(!windows.advance(stage, &snap).starts_p99_episode);
        snap.buckets[bucket_index(500_000)] += 150;
        snap.over_budget += 150;
        assert!(windows.advance(stage, &snap).starts_p99_episode);
        // Fewer than P99_WARN_MIN_SAMPLES never fires, even all slow.
        let mut quiet = QuantileWindows::new();
        let mut few = snap;
        few.buckets = [0; STAGE_BUCKETS];
        few.buckets[bucket_index(500_000)] = P99_WARN_MIN_SAMPLES - 1;
        few.over_budget = P99_WARN_MIN_SAMPLES - 1;
        assert!(!quiet.advance(stage, &few).starts_p99_episode);
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

    fn quiet() -> StallReadings {
        StallReadings {
            task_age_secs: [Some(0.1); WATCHED_TASKS.len()],
            stage_max_nanos: [1_000; STAGE_COUNT],
        }
    }

    fn drain_age(secs: f64) -> StallReadings {
        let mut r = quiet();
        r.task_age_secs[2] = Some(secs);
        r
    }

    #[test]
    fn test_stall_alarm_new_and_observe_fire_once_per_episode_and_re_arm() {
        let mut alarm = StallAlarm::new();
        let t0 = Instant::now();
        assert_eq!(alarm.observe(t0, true, &quiet()), None);
        let line = alarm
            .observe(t0 + Duration::from_secs(1), true, &drain_age(2.5))
            .expect("rising edge fires");
        assert_eq!(line.signal, "frame_drain");
        assert_eq!(line.kind, "heartbeat");
        assert_eq!(line.signals, 1);
        assert_eq!(line.missed_episodes, 0);
        // Still stalled, even long after the rate limit: one line per episode.
        for s in 2..200 {
            assert_eq!(
                alarm.observe(t0 + Duration::from_secs(s), true, &drain_age(3.0)),
                None
            );
        }
        // Recovery re-arms; the next stall fires again.
        assert_eq!(
            alarm.observe(t0 + Duration::from_secs(200), true, &quiet()),
            None
        );
        assert!(
            alarm
                .observe(t0 + Duration::from_secs(201), true, &drain_age(2.0))
                .is_some(),
            "a stall exactly at the threshold, after recovery, is a new episode"
        );
    }

    #[test]
    fn test_stall_alarm_observe_rate_limits_and_counts_missed_episodes() {
        let mut alarm = StallAlarm::new();
        let t0 = Instant::now();
        assert!(alarm.observe(t0, true, &drain_age(5.0)).is_some());
        assert_eq!(
            alarm.observe(t0 + Duration::from_secs(1), true, &quiet()),
            None
        );
        // A new episode 10 s after the line: held back, then it ends unseen.
        assert_eq!(
            alarm.observe(t0 + Duration::from_secs(10), true, &drain_age(4.0)),
            None
        );
        assert_eq!(
            alarm.observe(t0 + Duration::from_secs(11), true, &quiet()),
            None
        );
        // A stage stall 20 s later is held back while it lasts...
        let mut ring = quiet();
        ring.stage_max_nanos[Stage::RingDwell as usize] = 3_000_000_000;
        assert_eq!(
            alarm.observe(t0 + Duration::from_secs(30), true, &ring),
            None
        );
        // ...and reported once the minute has passed, carrying the missed one.
        let line = alarm
            .observe(t0 + Duration::from_secs(60), true, &ring)
            .expect("the held-back stall is reported when the limit allows");
        assert_eq!(line.signal, "ring_dwell");
        assert_eq!(line.kind, "stage");
        assert!((line.stalled_secs - 3.0).abs() < 0.001);
        assert_eq!(line.missed_episodes, 1);
    }

    #[test]
    fn test_stall_alarm_observe_names_the_worst_signal_and_ignores_never_beaten_tasks() {
        let mut alarm = StallAlarm::new();
        let mut r = quiet();
        r.task_age_secs = [None, Some(2.5), Some(9.0)];
        r.stage_max_nanos[Stage::MainRuntimeLag as usize] = 4_000_000_000;
        let line = alarm.observe(Instant::now(), true, &r).expect("fires");
        assert_eq!(line.signal, "frame_drain");
        assert_eq!(line.signals, 3);
        let mut never = StallAlarm::new();
        let mut none = quiet();
        none.task_age_secs = [None; WATCHED_TASKS.len()];
        assert_eq!(never.observe(Instant::now(), true, &none), None);
    }

    #[test]
    fn test_stall_alarm_observe_is_silent_and_re_armed_out_of_session() {
        let mut alarm = StallAlarm::new();
        let t0 = Instant::now();
        assert_eq!(alarm.observe(t0, false, &drain_age(30.0)), None);
        assert!(alarm.observe(t0, true, &drain_age(30.0)).is_some());
        // Out of session clears the latch, so the next session's stall pages.
        assert_eq!(
            alarm.observe(t0 + Duration::from_secs(120), false, &drain_age(30.0)),
            None
        );
        assert!(
            alarm
                .observe(t0 + Duration::from_secs(180), true, &drain_age(30.0))
                .is_some()
        );
    }

    #[test]
    fn test_stall_page_secs_f64_matches_the_integer_threshold() {
        assert_eq!(STALL_PAGE_SECS_F64.fract(), 0.0);
        assert_eq!(STALL_PAGE_SECS_F64 as u32, STALL_PAGE_SECS);
    }

    #[test]
    fn test_regression_begin_shutdown_and_is_shutting_down_latch_the_alarm_quiet() {
        // A planned stop in session must not page: once the flag is set the
        // publisher passes `in_session = false`, which the alarm treats as
        // silent and re-armed (see the out-of-session test).
        begin_shutdown();
        assert!(is_shutting_down());
        begin_shutdown();
        assert!(is_shutting_down(), "the flag never clears");
        let mut alarm = StallAlarm::new();
        let in_session = !is_shutting_down();
        assert!(
            alarm
                .observe(Instant::now(), in_session, &drain_age(30.0))
                .is_none()
        );
    }

    #[test]
    fn test_is_in_session_secs_of_day_bounds() {
        assert!(!is_in_session_secs_of_day(32_399));
        assert!(is_in_session_secs_of_day(32_400));
        assert!(is_in_session_secs_of_day(56_399));
        assert!(!is_in_session_secs_of_day(56_400));
        assert!(!is_in_session_secs_of_day(0));
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

    /// DT0 (2026-10-29): every sample lands in the fine bucket whose bound
    /// is the smallest one at or above it, and the bounds rise strictly.
    #[test]
    fn test_fine_bucket_index_puts_each_sample_under_its_upper_bound() {
        let mut prev = 0u64;
        for k in 0..FINE_BUCKETS - 1 {
            let bound = fine_bucket_upper_bound_nanos(k).expect("bounded");
            assert!(bound > prev, "bounds rise: {prev} then {bound}");
            assert_eq!(
                fine_bucket_index(bound),
                k,
                "the bound itself is in bucket {k}"
            );
            assert_eq!(
                fine_bucket_index(bound + 1),
                k + 1,
                "one past it is in the next"
            );
            prev = bound;
        }
        assert_eq!(fine_bucket_upper_bound_nanos(FINE_BUCKETS - 1), None);
        assert_eq!(prev, 1 << FINE_TOP_EXP, "the top bound is 2^24 ns");
        assert_eq!(fine_bucket_index(0), 0);
        assert_eq!(fine_bucket_index(u64::MAX), FINE_BUCKETS - 1);
        // Four buckets per octave: 1,280 / 1,536 / 1,792 / 2,048 ns.
        assert_eq!(fine_bucket_upper_bound_nanos(1), Some(1_280));
        assert_eq!(fine_bucket_upper_bound_nanos(4), Some(2_048));
        // A bound is at most 25% above any sample in its bucket.
        for nanos in [1_025u64, 9_999, 100_001, 999_999, 1_000_001, 16_000_000] {
            let bound = fine_bucket_upper_bound_nanos(fine_bucket_index(nanos)).expect("bounded");
            assert!(
                bound >= nanos && bound * 4 <= nanos * 5 + 4,
                "{nanos} -> {bound}"
            );
        }
    }

    #[test]
    fn test_fine_quantiles_reads_the_ranked_fine_bucket() {
        assert_eq!(fine_quantiles(&[0; FINE_BUCKETS]), None);
        let mut w = [0u64; FINE_BUCKETS];
        w[fine_bucket_index(20_000)] = 990; // p50 and p99 near 20 µs
        w[fine_bucket_index(500_000)] = 9; // p99.9 near 0.5 ms
        w[FINE_BUCKETS - 1] = 1; // one sample above 16.8 ms
        let q = fine_quantiles(&w).expect("samples");
        assert_eq!(q.samples, 1_000);
        assert_eq!(
            q.p50,
            fine_bucket_upper_bound_nanos(fine_bucket_index(20_000))
        );
        assert_eq!(q.p99, q.p50);
        assert_eq!(
            q.p999,
            fine_bucket_upper_bound_nanos(fine_bucket_index(500_000))
        );
        let mut top = [0u64; FINE_BUCKETS];
        top[FINE_BUCKETS - 1] = 5;
        assert_eq!(fine_quantiles(&top).expect("samples").p50, None, "overflow");
    }

    /// One call records one `receipt_to_fold` stage sample and one fine
    /// sample in the matching bucket.
    #[test]
    fn test_record_receipt_to_fold_nanos_fills_the_stage_and_the_fine_histogram() {
        assert_eq!(Stage::ReceiptToFold.as_str(), "receipt_to_fold");
        assert_eq!(Stage::MetricsUpkeep.as_str(), "metrics_upkeep");
        assert_eq!(Stage::ReceiptToFold.stall_budget_nanos(), 1_000_000);
        assert_eq!(Stage::MetricsUpkeep.stall_budget_nanos(), 1_000_000);
        let nanos = 1_234_567u64;
        let k = fine_bucket_index(nanos);
        let before = stage_snapshot(Stage::ReceiptToFold);
        let fine_before = receipt_to_fold_fine_snapshot();
        record_receipt_to_fold_nanos(nanos);
        let after = stage_snapshot(Stage::ReceiptToFold);
        let fine_after = receipt_to_fold_fine_snapshot();
        assert!(after.count > before.count);
        assert!(
            after.over_budget > before.over_budget,
            "1.23 ms is over the 1 ms budget"
        );
        assert!(fine_after[k] > fine_before[k]);
    }

    #[test]
    fn test_receipt_to_fold_fine_snapshot_counts_only_rise() {
        let before = receipt_to_fold_fine_snapshot();
        record_receipt_to_fold_nanos(50_000);
        let after = receipt_to_fold_fine_snapshot();
        assert!(before.iter().zip(after.iter()).all(|(b, a)| a >= b));
        assert!(after[fine_bucket_index(50_000)] > before[fine_bucket_index(50_000)]);
    }

    /// The two DT0 stages are measurement only: a 5 s sample in either never
    /// starts a stall page, and no signal is ever labelled with them.
    #[test]
    fn test_receipt_to_fold_and_metrics_upkeep_never_page() {
        let mut readings = quiet();
        readings.stage_max_nanos[Stage::ReceiptToFold as usize] = 5_000_000_000;
        readings.stage_max_nanos[Stage::MetricsUpkeep as usize] = 5_000_000_000;
        let mut alarm = StallAlarm::new();
        assert_eq!(alarm.observe(Instant::now(), true, &readings), None);
        for i in 0..SIGNAL_COUNT {
            let (label, _) = signal_label(i);
            assert_ne!(label, "receipt_to_fold");
            assert_ne!(label, "metrics_upkeep");
        }
        // The five paged stages still page.
        readings.stage_max_nanos[Stage::DrainMainFrame as usize] = 5_000_000_000;
        let line = alarm
            .observe(Instant::now(), true, &readings)
            .expect("a paged stage still pages");
        assert_eq!(line.signal, "drain_main_frame");
    }
}
