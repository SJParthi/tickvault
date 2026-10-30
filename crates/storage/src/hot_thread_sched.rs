//! Per-hot-thread scheduling-wait evidence (plan item DT0, 2026-10-29).
//!
//! The live tail (35 to 50 frames a session over 1 ms, worst 8 to 16 ms on
//! the 2-vCPU r8g.large) was traced by elimination to CPU scheduling: the
//! socket reader, the frame drain, the WAL writer, the writer threads,
//! telemetry and QuestDB all share both cores. This module turns that
//! inference into a measurement. Once a second, on the `tv-telemetry`
//! thread only, it reads the kernel's own account of how long each hot
//! thread sat RUNNABLE but not running:
//!
//! * `/proc/self/task/<tid>/schedstat`, second field: `run_delay`, the
//!   nanoseconds the thread waited on a run queue;
//! * `/proc/self/task/<tid>/status`, `nonvoluntary_ctxt_switches`: how often
//!   the thread was taken off the CPU while it still had work;
//! * `/proc/stat`, the `cpu` line's eighth field: hypervisor steal, in
//!   `USER_HZ` ticks (100 a second on Linux), so a tick over 1 ms can be
//!   attributed to this box or to the host.
//!
//! Published series (no EMF name, no metric filter, no alarm, no Telegram):
//! `tv_hot_thread_run_delay_ns_total{thread}`,
//! `tv_hot_thread_involuntary_switches_total{thread}`,
//! `tv_hot_thread_main_workers_registered`, `tv_host_cpu_steal_ticks_total`.
//! They are not paged, but they are NOT local-only on the box: the
//! CloudWatch agent scrapes all of `/metrics` once a minute with no relabel
//! filter and writes every sample to the metrics log group (see "Cost").
//!
//! # Which threads
//!
//! * `ws_reader_<n>`: each socket reader worker registers its thread id
//!   once, at thread start ([`register_ws_reader_thread`], up to
//!   [`MAX_WS_READER_SLOTS`]), one series per worker.
//! * `main_workers`: the frame drain is a TASK on the shared main runtime,
//!   so it moves between that runtime's worker threads (work stealing, the
//!   LIFO slot, a `block_in_place` that hands the worker to a new thread).
//!   Following the drain's last host with one baseline threw away every
//!   second in which it moved, which is the contended case this module is
//!   for (review of DT0, 2026-10-29). Instead EVERY thread that acts as a
//!   main-runtime worker registers itself from the runtime's
//!   `on_thread_park` hook ([`register_main_worker_thread`]; tokio calls that
//!   hook only on worker threads, never on blocking-pool threads), gets its
//!   own slot and its own baseline, and leaves its slot from the
//!   `on_thread_stop` hook ([`release_main_worker_thread`]). The series is
//!   the SUM of every registered worker's growth, so the drain moving
//!   between workers loses nothing.
//!
//! # Honest limits
//!
//! * Counting starts at the first reading of a thread (that reading is the
//!   baseline), so a thread's first second is not counted, and a worker's
//!   growth between the last read and its exit is lost. A worker past
//!   [`MAX_MAIN_WORKER_SLOTS`] at once is not measured until a slot frees
//!   (it retries every [`REFUSED_RETRY_EVERY_PARKS`] parks; the gauge
//!   `tv_hot_thread_main_workers_registered` shows how many are). Those
//!   read low.
//! * `main_workers` can also read HIGH relative to the drain: it is the
//!   OS-level wait of the threads that run all ~117 main-runtime tasks, so a
//!   second in which a worker waited while running some other task counts
//!   in full. And a thread that entered `block_in_place` keeps its slot
//!   while it runs the blocking closure and idles in the blocking pool
//!   (about 10 s), so its wait as a blocking thread counts too. The series
//!   answers "did the main runtime's threads wait for a CPU", not "how long
//!   did the drain task wait"; the task-level queueing is `ring_dwell`.
//! * On macOS, or wherever `/proc` cannot be read, nothing is published and
//!   nothing is logged: an absent series means "not measured here", never 0.
//!
//! # Cost
//!
//! About 143 series were added to `/metrics` by DT0 (two stages of ~35
//! series each, the 61 fine-histogram series, and 12 here). At one scrape a
//! minute, about 150 bytes a line and the box's ~8 hours a day, that is
//! about 10 MB a day, about 0.25 GB a month of log ingestion, roughly
//! $0.15 to $0.30 a month (Assumed: line size and the per-GB price are not
//! measured). Inside the $75 November line; no metric filter reads them.
//!
//! # Complexity
//! | Path | Cost |
//! |---|---|
//! | [`register_ws_reader_thread`] | one `/proc/thread-self` read link per reader thread, once at thread start (cold) |
//! | [`register_main_worker_thread`] | one thread-local read on every worker park after the first (plus one thread-local add while refused, and a slot scan once every [`REFUSED_RETRY_EVERY_PARKS`] parks); the first call on a thread reads `/proc/thread-self` once (one syscall, one small `PathBuf`) and scans at most [`MAX_MAIN_WORKER_SLOTS`] atomics. Never on the frame path |
//! | [`release_main_worker_thread`] | one thread-local read and at most one compare-and-swap, once per runtime thread exit |
//! | [`SchedPublisher::publish`] | O(reader slots + main slots) = O(12) pairs of file reads plus one `/proc/stat` read, once a second, on the `tv-telemetry` thread |

use std::cell::Cell;
use std::fmt::Write as _;
use std::io::Read as _;
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};

/// Reader worker slots. Matches `reader_runtime::MAX_WS_READER_THREADS` (4);
/// a reader past this is not measured.
pub const MAX_WS_READER_SLOTS: usize = 4;

/// Main-runtime worker slots measured at once. The runtime has 2 workers on
/// the r8g.large and at most `HOST_DERIVED_WORKER_CEILING` (4) unless
/// overridden; the extra slots cover threads that took over a worker core
/// after a `block_in_place` while the old thread still holds its slot.
pub const MAX_MAIN_WORKER_SLOTS: usize = 8;

/// One published series of scheduling wait.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HotThread {
    /// Socket reader worker 0.
    WsReader0 = 0,
    /// Socket reader worker 1.
    WsReader1 = 1,
    /// Socket reader worker 2.
    WsReader2 = 2,
    /// Socket reader worker 3.
    WsReader3 = 3,
    /// Every thread acting as a main-runtime worker (the drain's runtime),
    /// summed.
    MainWorkers = 4,
}

/// Every published series, in index order.
pub const ALL_HOT_THREADS: [HotThread; 5] = [
    HotThread::WsReader0,
    HotThread::WsReader1,
    HotThread::WsReader2,
    HotThread::WsReader3,
    HotThread::MainWorkers,
];

const SERIES_COUNT: usize = ALL_HOT_THREADS.len();

const _: () = assert!(MAX_WS_READER_SLOTS == HotThread::MainWorkers as usize);
// The thread-local slot marker is a `u8` with two reserved values.
const _: () = assert!(MAX_MAIN_WORKER_SLOTS < SLOT_REFUSED as usize);

impl HotThread {
    /// The `thread` label value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::WsReader0 => "ws_reader_0",
            Self::WsReader1 => "ws_reader_1",
            Self::WsReader2 => "ws_reader_2",
            Self::WsReader3 => "ws_reader_3",
            Self::MainWorkers => "main_workers",
        }
    }
}

/// Reader thread id per slot; 0 = unknown.
static READER_TIDS: [AtomicI64; MAX_WS_READER_SLOTS] =
    [const { AtomicI64::new(0) }; MAX_WS_READER_SLOTS];

/// Reader slots handed out so far.
static NEXT_READER_SLOT: AtomicUsize = AtomicUsize::new(0);

/// Main-runtime worker thread id per slot; 0 = free.
static MAIN_TIDS: [AtomicI64; MAX_MAIN_WORKER_SLOTS] =
    [const { AtomicI64::new(0) }; MAX_MAIN_WORKER_SLOTS];

/// The thread-local main slot marker before the thread tried to register.
const SLOT_UNSET: u8 = u8::MAX;
/// The thread-local main slot marker after a refused registration (every
/// slot taken, or the id unreadable): not retried on every park.
const SLOT_REFUSED: u8 = u8::MAX - 1;

/// A refused thread retries its registration once every this-many parks
/// (a power of two). Slots free up as `block_in_place` threads idle out of
/// the blocking pool (about 10 s), so a worker refused while they held every
/// slot is measured again soon after, instead of never (DT0 re-review,
/// 2026-10-30). The retry is the cold path: one slot scan of
/// [`MAX_MAIN_WORKER_SLOTS`] compare-and-swaps, no syscall (the thread id is
/// cached).
const REFUSED_RETRY_EVERY_PARKS: u32 = 1_024;

const _: () = assert!(REFUSED_RETRY_EVERY_PARKS.is_power_of_two());

thread_local! {
    /// The calling thread's id, read once per thread; 0 = not read yet or
    /// unreadable. A `const` initialiser with no destructor: accessing it
    /// never allocates.
    static THIS_TID: Cell<i64> = const { Cell::new(0) };
    /// True once the OS was asked for this thread's id (so an unreadable id
    /// is asked for once, not on every call).
    static TID_ASKED: Cell<bool> = const { Cell::new(false) };
    /// This thread's main-worker slot, or [`SLOT_UNSET`] / [`SLOT_REFUSED`].
    static MAIN_SLOT: Cell<u8> = const { Cell::new(SLOT_UNSET) };
    /// Parks since this thread's registration was refused.
    static PARKS_SINCE_REFUSED: Cell<u32> = const { Cell::new(0) };
}

/// The calling thread's kernel id from `/proc/thread-self`
/// (`<pid>/task/<tid>`); 0 where it cannot be read (macOS, no `/proc`).
fn os_thread_id() -> i64 {
    std::fs::read_link("/proc/thread-self")
        .ok()
        .and_then(|link| {
            link.file_name()
                .and_then(|name| name.to_str())
                .and_then(|name| name.parse::<i64>().ok())
        })
        .unwrap_or(0)
}

/// The calling thread's id, cached per thread; 0 when unknown.
fn current_thread_id() -> i64 {
    if TID_ASKED.with(Cell::get) {
        return THIS_TID.with(Cell::get);
    }
    let tid = os_thread_id();
    THIS_TID.with(|c| c.set(tid));
    TID_ASKED.with(|c| c.set(true));
    tid
}

/// Registers the calling thread as the next socket reader worker. Call once,
/// from the reader runtime's `on_thread_start`, on a worker thread. Returns
/// the slot taken, or `None` when every reader slot is taken or the thread
/// id cannot be read (nothing is published for it then).
pub fn register_ws_reader_thread() -> Option<HotThread> {
    let tid = current_thread_id();
    if tid == 0 {
        return None;
    }
    let slot = NEXT_READER_SLOT.fetch_add(1, Ordering::Relaxed);
    let thread = *ALL_HOT_THREADS
        .get(slot)
        .filter(|_| slot < MAX_WS_READER_SLOTS)?;
    READER_TIDS[slot].store(tid, Ordering::Relaxed);
    Some(thread)
}

/// Registers the calling thread as a main-runtime worker. Call from the main
/// runtime's `on_thread_park` hook (tokio runs it only on worker threads).
/// After the first call on a thread it is one thread-local read, zero
/// allocation. Returns the slot held, or `None` when every slot is taken or
/// the thread id cannot be read; a refused thread retries once every
/// [`REFUSED_RETRY_EVERY_PARKS`] parks (one more thread-local add per park
/// while refused).
#[inline]
pub fn register_main_worker_thread() -> Option<usize> {
    let held = MAIN_SLOT.with(Cell::get);
    if held == SLOT_REFUSED {
        let parks = PARKS_SINCE_REFUSED.with(|c| {
            let n = c.get().wrapping_add(1);
            c.set(n);
            n
        });
        if parks & (REFUSED_RETRY_EVERY_PARKS - 1) != 0 {
            return None;
        }
        return register_main_worker_thread_cold();
    }
    if held != SLOT_UNSET {
        return Some(usize::from(held));
    }
    register_main_worker_thread_cold()
}

#[cold]
fn register_main_worker_thread_cold() -> Option<usize> {
    let tid = current_thread_id();
    let taken = (tid != 0)
        .then(|| {
            MAIN_TIDS.iter().position(|slot| {
                slot.compare_exchange(0, tid, Ordering::Relaxed, Ordering::Relaxed)
                    .is_ok()
            })
        })
        .flatten();
    let marker = taken
        .and_then(|i| u8::try_from(i).ok())
        .unwrap_or(SLOT_REFUSED);
    MAIN_SLOT.with(|c| c.set(marker));
    taken
}

/// Gives the calling thread's main-worker slot back. Call from the main
/// runtime's `on_thread_stop` hook (every runtime thread runs it; a thread
/// that never registered does nothing). O(1).
pub fn release_main_worker_thread() {
    let held = MAIN_SLOT.with(Cell::get);
    if let Some(slot) = MAIN_TIDS.get(usize::from(held)) {
        let tid = THIS_TID.with(Cell::get);
        // A failed exchange means the slot already holds another thread:
        // nothing of ours to give back.
        let _released = slot
            .compare_exchange(tid, 0, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok();
    }
    MAIN_SLOT.with(|c| c.set(SLOT_UNSET));
}

/// How many main-runtime worker slots are held now.
#[must_use]
pub fn main_workers_registered() -> usize {
    MAIN_TIDS
        .iter()
        .filter(|slot| slot.load(Ordering::Relaxed) != 0)
        .count()
}

/// The thread id registered for a reader series; 0 when none is, and always
/// 0 for [`HotThread::MainWorkers`] (a sum over many threads).
#[must_use]
pub fn registered_thread_id(thread: HotThread) -> i64 {
    READER_TIDS
        .get(thread as usize)
        .map_or(0, |slot| slot.load(Ordering::Relaxed))
}

/// `run_delay` (nanoseconds waiting on a run queue) from a
/// `/proc/<pid>/task/<tid>/schedstat` line: `<on_cpu> <run_delay> <slices>`.
/// `None` when the second field is missing or not a number.
#[must_use]
pub fn parse_schedstat_run_delay_ns(schedstat: &str) -> Option<u64> {
    schedstat.split_ascii_whitespace().nth(1)?.parse().ok()
}

/// `nonvoluntary_ctxt_switches` from a `/proc/<pid>/task/<tid>/status`
/// file. `None` when the line is missing or its value is not a number.
#[must_use]
pub fn parse_status_nonvoluntary_switches(status: &str) -> Option<u64> {
    status
        .lines()
        .find_map(|line| line.strip_prefix("nonvoluntary_ctxt_switches:"))?
        .trim()
        .parse()
        .ok()
}

/// Hypervisor steal, in `USER_HZ` ticks, from `/proc/stat`'s aggregate
/// `cpu` line (`cpu  user nice system idle iowait irq softirq steal ...`).
/// `None` when the first line is not the aggregate line or has no steal
/// field (kernels before 2.6.11).
#[must_use]
pub fn parse_proc_stat_steal_ticks(stat: &str) -> Option<u64> {
    let mut fields = stat.lines().next()?.split_ascii_whitespace();
    if fields.next()? != "cpu" {
        return None;
    }
    fields.nth(7)?.parse().ok()
}

/// One thread slot's last reading.
#[derive(Debug, Clone, Copy, Default)]
struct SlotState {
    tid: i64,
    last_delay: u64,
    last_switches: u64,
    baselined: bool,
}

impl SlotState {
    /// Folds one reading of `tid` and returns the growth since the previous
    /// reading of the same thread. A new thread id or a counter that went
    /// backwards is a new baseline (growth 0).
    fn observe(&mut self, tid: i64, delay: u64, switches: u64) -> (u64, u64) {
        let growth = if !self.baselined || tid != self.tid || delay < self.last_delay {
            self.tid = tid;
            self.baselined = true;
            (0, 0)
        } else {
            (
                delay - self.last_delay,
                switches.saturating_sub(self.last_switches),
            )
        };
        self.last_delay = delay;
        self.last_switches = switches;
        growth
    }
}

/// The once-a-second reader and publisher. Lives on the `tv-telemetry`
/// thread only. Series are resolved the first time one is read, so a series
/// with no readable thread publishes nothing (absent, not 0).
pub struct SchedPublisher {
    readers: [SlotState; MAX_WS_READER_SLOTS],
    mains: [SlotState; MAX_MAIN_WORKER_SLOTS],
    /// Accumulated `(run_delay_ns, involuntary_switches)` per series.
    totals: [(u64, u64); SERIES_COUNT],
    delay: [Option<metrics::Counter>; SERIES_COUNT],
    switches: [Option<metrics::Counter>; SERIES_COUNT],
    registered: Option<metrics::Gauge>,
    steal: Option<metrics::Counter>,
    path: String,
    buf: String,
}

impl Default for SchedPublisher {
    fn default() -> Self {
        Self::new()
    }
}

impl SchedPublisher {
    /// Nothing read yet.
    #[must_use]
    pub fn new() -> Self {
        Self {
            readers: [SlotState::default(); MAX_WS_READER_SLOTS],
            mains: [SlotState::default(); MAX_MAIN_WORKER_SLOTS],
            totals: [(0, 0); SERIES_COUNT],
            delay: [const { None }; SERIES_COUNT],
            switches: [const { None }; SERIES_COUNT],
            registered: None,
            steal: None,
            path: String::with_capacity(64),
            buf: String::with_capacity(4_096),
        }
    }

    /// Reads `path` into the reused buffer; false when it cannot be read.
    fn read_into_buf(&mut self) -> bool {
        self.buf.clear();
        std::fs::File::open(&self.path)
            .and_then(|mut f| f.read_to_string(&mut self.buf))
            .is_ok()
    }

    /// Reads one thread's `schedstat` and `status`.
    fn read_thread(&mut self, tid: i64) -> Option<(u64, u64)> {
        self.path.clear();
        write!(self.path, "/proc/self/task/{tid}/schedstat").ok()?;
        if !self.read_into_buf() {
            return None;
        }
        let delay = parse_schedstat_run_delay_ns(&self.buf)?;
        self.path.clear();
        write!(self.path, "/proc/self/task/{tid}/status").ok()?;
        if !self.read_into_buf() {
            return None;
        }
        let switches = parse_status_nonvoluntary_switches(&self.buf)?;
        Some((delay, switches))
    }

    /// Adds one slot's growth to `series`.
    fn add(&mut self, series: HotThread, growth: (u64, u64)) {
        let t = &mut self.totals[series as usize];
        t.0 = t.0.saturating_add(growth.0);
        t.1 = t.1.saturating_add(growth.1);
    }

    /// Folds one reading of main-worker slot `slot` into `main_workers`.
    fn fold_main(&mut self, slot: usize, tid: i64, delay: u64, switches: u64) {
        let growth = self.mains[slot].observe(tid, delay, switches);
        self.add(HotThread::MainWorkers, growth);
    }

    /// Publishes `series` from its accumulated totals.
    fn publish_series(&mut self, series: HotThread) {
        let i = series as usize;
        let name = series.as_str();
        let (delay, switches) = self.totals[i];
        self.delay[i]
            .get_or_insert_with(
                || metrics::counter!("tv_hot_thread_run_delay_ns_total", "thread" => name),
            )
            .absolute(delay);
        self.switches[i]
            .get_or_insert_with(
                || metrics::counter!("tv_hot_thread_involuntary_switches_total", "thread" => name),
            )
            .absolute(switches);
    }

    /// One read and publish of every registered thread plus host steal.
    /// Returns how many thread slots were read. Never logs and never errors:
    /// an unreadable file leaves its series as it was.
    pub fn publish(&mut self) -> usize {
        let mut read = 0usize;
        for (slot, thread) in ALL_HOT_THREADS
            .iter()
            .copied()
            .take(MAX_WS_READER_SLOTS)
            .enumerate()
        {
            let tid = READER_TIDS[slot].load(Ordering::Relaxed);
            if tid == 0 {
                continue;
            }
            let Some((delay, switches)) = self.read_thread(tid) else {
                continue;
            };
            let growth = self.readers[slot].observe(tid, delay, switches);
            self.add(thread, growth);
            self.publish_series(thread);
            read += 1;
        }
        let mut mains_read = 0usize;
        for (slot, cell) in MAIN_TIDS.iter().enumerate() {
            let tid = cell.load(Ordering::Relaxed);
            if tid == 0 {
                continue;
            }
            let Some((delay, switches)) = self.read_thread(tid) else {
                continue;
            };
            self.fold_main(slot, tid, delay, switches);
            mains_read += 1;
        }
        if mains_read > 0 {
            self.publish_series(HotThread::MainWorkers);
            self.registered
                .get_or_insert_with(|| metrics::gauge!("tv_hot_thread_main_workers_registered"))
                .set(main_workers_registered() as f64);
        }
        read += mains_read;
        self.path.clear();
        self.path.push_str("/proc/stat");
        if self.read_into_buf()
            && let Some(steal) = parse_proc_stat_steal_ticks(&self.buf)
        {
            self.steal
                .get_or_insert_with(|| metrics::counter!("tv_host_cpu_steal_ticks_total"))
                .absolute(steal);
        }
        read
    }

    /// The accumulated `(run_delay_ns, involuntary_switches)` of `series`.
    #[cfg(test)]
    fn totals(&self, series: HotThread) -> (u64, u64) {
        self.totals[series as usize]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The slot statics are process-wide and libtest runs tests on parallel
    /// threads: every test that registers or publishes takes this lock, and
    /// gives its main slot back before it ends, so one test can never fill a
    /// slot another reads.
    static STATICS: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn lock() -> std::sync::MutexGuard<'static, ()> {
        STATICS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn proc_readable() -> bool {
        current_thread_id() != 0
    }

    #[test]
    fn test_parse_schedstat_run_delay_ns_reads_the_second_field() {
        assert_eq!(
            parse_schedstat_run_delay_ns("123456789 98765 4321\n"),
            Some(98_765)
        );
        assert_eq!(parse_schedstat_run_delay_ns("1 2 3"), Some(2));
        assert_eq!(parse_schedstat_run_delay_ns("123"), None);
        assert_eq!(parse_schedstat_run_delay_ns(""), None);
        assert_eq!(parse_schedstat_run_delay_ns("1 x 3"), None);
        assert_eq!(parse_schedstat_run_delay_ns("1 -2 3"), None);
    }

    #[test]
    fn test_parse_status_nonvoluntary_switches_finds_the_line() {
        let status = "Name:\ttv-ws-reader\nState:\tS (sleeping)\n\
                      voluntary_ctxt_switches:\t5000\n\
                      nonvoluntary_ctxt_switches:\t42\n";
        assert_eq!(parse_status_nonvoluntary_switches(status), Some(42));
        // The voluntary line alone is not the involuntary count.
        assert_eq!(
            parse_status_nonvoluntary_switches("voluntary_ctxt_switches:\t7\n"),
            None
        );
        assert_eq!(
            parse_status_nonvoluntary_switches("nonvoluntary_ctxt_switches:\tabc\n"),
            None
        );
        assert_eq!(parse_status_nonvoluntary_switches(""), None);
    }

    #[test]
    fn test_parse_proc_stat_steal_ticks_reads_the_eighth_field() {
        let stat = "cpu  10 20 30 40 50 60 70 88 0 0\ncpu0 1 2 3 4 5 6 7 8 0 0\n";
        assert_eq!(parse_proc_stat_steal_ticks(stat), Some(88));
        // A kernel without the steal field.
        assert_eq!(parse_proc_stat_steal_ticks("cpu  1 2 3 4 5 6 7\n"), None);
        // A per-CPU line first is not the aggregate.
        assert_eq!(parse_proc_stat_steal_ticks("cpu0 1 2 3 4 5 6 7 8\n"), None);
        assert_eq!(parse_proc_stat_steal_ticks(""), None);
    }

    #[test]
    fn test_slot_state_baselines_then_returns_growth_and_rebaselines_on_a_new_thread() {
        let mut s = SlotState::default();
        assert_eq!(s.observe(7, 1_000, 3), (0, 0), "first read is a baseline");
        assert_eq!(s.observe(7, 1_500, 5), (500, 2));
        // The slot now holds another thread: its counters are not ours.
        assert_eq!(s.observe(9, 900_000, 100), (0, 0));
        assert_eq!(s.observe(9, 900_100, 101), (100, 1));
        // A counter that went backwards (a reused id) re-baselines.
        assert_eq!(s.observe(9, 10, 1), (0, 0));
    }

    /// The DT0 review finding: the drain moves between workers A and B in
    /// the contended seconds. With one baseline that followed the drain's
    /// host, readings in the order A, B, A, B added NOTHING (every reading a
    /// new baseline). With a slot per worker, every second of both workers'
    /// growth is counted.
    #[test]
    fn test_regression_alternating_drain_hosts_lose_no_wait() {
        let mut p = SchedPublisher::new();
        let (a, b) = (101, 202);
        // Second 0: both baselines.
        p.fold_main(0, a, 1_000_000, 10);
        p.fold_main(1, b, 2_000_000, 20);
        // Seconds 1..=4: worker A waits 8 ms a second, B 3 ms.
        for s in 1..=4u64 {
            p.fold_main(0, a, 1_000_000 + s * 8_000_000, 10 + s * 2);
            p.fold_main(1, b, 2_000_000 + s * 3_000_000, 20 + s);
        }
        assert_eq!(
            p.totals(HotThread::MainWorkers),
            (4 * 8_000_000 + 4 * 3_000_000, 4 * 2 + 4),
            "every second of both workers' wait is counted"
        );
    }

    /// A slot handed to a new thread (the old worker exited) starts a new
    /// baseline for that slot only; the series keeps what it had.
    #[test]
    fn test_a_reused_main_slot_keeps_the_series_total() {
        let mut p = SchedPublisher::new();
        p.fold_main(0, 5, 100, 1);
        p.fold_main(0, 5, 600, 3);
        p.fold_main(0, 6, 50, 0); // a new thread took slot 0
        p.fold_main(0, 6, 150, 1);
        assert_eq!(p.totals(HotThread::MainWorkers), (600, 3));
    }

    #[test]
    fn test_hot_thread_labels_are_distinct_and_indexed() {
        for (i, t) in ALL_HOT_THREADS.iter().enumerate() {
            assert_eq!(*t as usize, i);
        }
        let mut names: Vec<&str> = ALL_HOT_THREADS.iter().map(|t| t.as_str()).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), ALL_HOT_THREADS.len());
    }

    /// Register, publish (this thread's slot is read and stays readable
    /// because the thread is alive), release (the slot is free again).
    #[test]
    fn test_release_main_worker_thread_frees_the_slot_main_workers_registered_counts_it() {
        let _g = lock();
        let before = main_workers_registered();
        let slot = register_main_worker_thread();
        if !proc_readable() {
            assert_eq!(slot, None);
            release_main_worker_thread();
            return;
        }
        let slot = slot.expect("a free main slot");
        assert_eq!(
            MAIN_TIDS[slot].load(Ordering::Relaxed),
            current_thread_id(),
            "the slot holds this thread's id"
        );
        // A second call (a later park) keeps the same slot.
        assert_eq!(register_main_worker_thread(), Some(slot));
        assert_eq!(main_workers_registered(), before + 1);
        let mut publisher = SchedPublisher::new();
        assert!(publisher.publish() >= 1, "this thread's slot was read");
        let _ = publisher.publish();
        let (delay, switches) = publisher.totals(HotThread::MainWorkers);
        assert!(delay < u64::MAX && switches < u64::MAX);
        release_main_worker_thread();
        assert_eq!(MAIN_TIDS[slot].load(Ordering::Relaxed), 0, "slot freed");
        assert_eq!(main_workers_registered(), before);
        // A thread that never registered releases nothing.
        release_main_worker_thread();
        assert_eq!(main_workers_registered(), before);
    }

    /// A real runtime whose `on_thread_park` registers: a worker registers
    /// once it parks, and the runtime's shutdown (`on_thread_stop`) frees
    /// its slots. Blocking-pool threads never register.
    #[test]
    fn test_park_hook_registers_workers_and_stop_hook_releases_them() {
        let _g = lock();
        if !proc_readable() {
            return;
        }
        let before = main_workers_registered();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .on_thread_park(|| {
                let _slot = register_main_worker_thread();
            })
            .on_thread_stop(release_main_worker_thread)
            .build()
            .expect("runtime");
        let blocking_slot = rt.block_on(async {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            tokio::task::spawn_blocking(|| MAIN_SLOT.with(Cell::get))
                .await
                .expect("blocking task")
        });
        assert_eq!(
            blocking_slot, SLOT_UNSET,
            "a blocking-pool thread never parks as a worker"
        );
        assert!(
            main_workers_registered() > before,
            "a parked worker registered"
        );
        drop(rt);
        assert_eq!(
            main_workers_registered(),
            before,
            "shutdown freed the slots"
        );
    }

    /// Regression (DT0 re-review, 2026-10-30): a worker refused because
    /// every slot was held (by threads that went into `block_in_place`) was
    /// never measured again. It now retries every
    /// `REFUSED_RETRY_EVERY_PARKS` parks and takes a slot that has freed.
    #[test]
    fn test_regression_refused_worker_retries_after_slots_free_up() {
        let _g = lock();
        if !proc_readable() {
            return;
        }
        // Run on a fresh thread so its thread-locals start unset.
        std::thread::spawn(|| {
            let fake = -7_i64;
            let filled: Vec<usize> = MAIN_TIDS
                .iter()
                .enumerate()
                .filter(|(_, slot)| {
                    slot.compare_exchange(0, fake, Ordering::Relaxed, Ordering::Relaxed)
                        .is_ok()
                })
                .map(|(i, _)| i)
                .collect();
            assert_eq!(register_main_worker_thread(), None, "every slot held");
            let freed = *filled.first().expect("at least one slot was free");
            MAIN_TIDS[freed].store(0, Ordering::Relaxed);
            for park in 1..REFUSED_RETRY_EVERY_PARKS {
                assert_eq!(
                    register_main_worker_thread(),
                    None,
                    "no retry before the interval (park {park})"
                );
            }
            assert_eq!(
                register_main_worker_thread(),
                Some(freed),
                "the retry takes the freed slot"
            );
            assert_eq!(register_main_worker_thread(), Some(freed), "kept");
            release_main_worker_thread();
            for i in filled.into_iter().skip(1) {
                MAIN_TIDS[i].store(0, Ordering::Relaxed);
            }
        })
        .join()
        .expect("test thread");
    }

    /// A reader registration takes a slot on Linux; past the reader slots it
    /// is refused.
    #[test]
    fn test_registered_thread_id_after_register_ws_reader_thread_fills_reader_slots_only() {
        let _g = lock();
        if !proc_readable() {
            assert_eq!(register_ws_reader_thread(), None);
            return;
        }
        let mut taken = Vec::new();
        for _ in 0..(MAX_WS_READER_SLOTS + 2) {
            if let Some(t) = register_ws_reader_thread() {
                taken.push(t);
            }
        }
        assert!(taken.len() <= MAX_WS_READER_SLOTS);
        assert!(!taken.contains(&HotThread::MainWorkers));
        assert_eq!(registered_thread_id(HotThread::MainWorkers), 0);
    }
}
