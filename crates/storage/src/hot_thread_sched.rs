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
//! Published, local `/metrics` only (no EMF name, no metric filter, no
//! alarm, no Telegram):
//! `tv_hot_thread_run_delay_ns_total{thread}`,
//! `tv_hot_thread_involuntary_switches_total{thread}`,
//! `tv_host_cpu_steal_ticks_total`.
//!
//! # Which threads
//!
//! Each socket reader worker registers its thread id once, at thread start
//! ([`register_ws_reader_thread`], up to [`MAX_WS_READER_SLOTS`]). The frame
//! drain is a TASK on the shared main runtime today, so it can move between
//! worker threads; it notes the thread it is running on from its 500 ms
//! flush arm ([`note_frame_drain_thread`]), and the `frame_drain` series is
//! the scheduling wait of whichever worker last hosted it.
//!
//! # Honest limits
//!
//! * Counting starts at the first reading of a thread (that reading is the
//!   baseline), so the first second a thread is seen is not counted; when
//!   the drain moves to another worker the new worker's first reading is a
//!   baseline too. The totals read low, never high.
//! * `frame_drain` is the wait of the drain's host THREAD, which also runs
//!   the other ~117 tasks of the main runtime: a runnable wait there delays
//!   the drain only when the drain was the task due to run.
//! * On macOS, or wherever `/proc` cannot be read, nothing is published and
//!   nothing is logged: an absent series means "not measured here", never 0.
//!
//! # Complexity
//! | Path | Cost |
//! |---|---|
//! | [`register_ws_reader_thread`] | one `/proc/thread-self` read link per reader thread, once at thread start (cold) |
//! | [`note_frame_drain_thread`] | one thread-local read and one relaxed store, twice a second; the first call on a thread reads `/proc/thread-self` once |
//! | [`SchedPublisher::publish`] | O(slots) = O(5) file reads plus one `/proc/stat` read, once a second, on the `tv-telemetry` thread |

use std::cell::Cell;
use std::fmt::Write as _;
use std::io::Read as _;
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};

/// Reader worker slots. Matches `reader_runtime::MAX_WS_READER_THREADS` (4);
/// a reader past this is not measured.
pub const MAX_WS_READER_SLOTS: usize = 4;

/// One hot thread whose scheduling wait is published.
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
    /// The worker thread that last ran the frame drain.
    FrameDrain = 4,
}

/// Every hot thread, in index order.
pub const ALL_HOT_THREADS: [HotThread; 5] = [
    HotThread::WsReader0,
    HotThread::WsReader1,
    HotThread::WsReader2,
    HotThread::WsReader3,
    HotThread::FrameDrain,
];

const SLOT_COUNT: usize = ALL_HOT_THREADS.len();

const _: () = assert!(MAX_WS_READER_SLOTS == HotThread::FrameDrain as usize);

impl HotThread {
    /// The `thread` label value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::WsReader0 => "ws_reader_0",
            Self::WsReader1 => "ws_reader_1",
            Self::WsReader2 => "ws_reader_2",
            Self::WsReader3 => "ws_reader_3",
            Self::FrameDrain => "frame_drain",
        }
    }
}

/// Thread id per slot; 0 = unknown.
static TIDS: [AtomicI64; SLOT_COUNT] = [const { AtomicI64::new(0) }; SLOT_COUNT];

/// Reader slots handed out so far.
static NEXT_READER_SLOT: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    /// The calling thread's id, read once per thread; 0 = not read yet or
    /// unreadable. A `const` initialiser with no destructor: accessing it
    /// never allocates.
    static THIS_TID: Cell<i64> = const { Cell::new(0) };
    /// True once the OS was asked for this thread's id (so an unreadable id
    /// is asked for once, not on every call).
    static TID_ASKED: Cell<bool> = const { Cell::new(false) };
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
    TIDS[thread as usize].store(tid, Ordering::Relaxed);
    Some(thread)
}

/// Notes that the frame drain is running on the calling thread. O(1): one
/// thread-local read and one relaxed store, zero allocation after the first
/// call on a thread. The drain calls it from its 500 ms flush arm.
#[inline]
pub fn note_frame_drain_thread() {
    let tid = current_thread_id();
    if tid != 0 {
        TIDS[HotThread::FrameDrain as usize].store(tid, Ordering::Relaxed);
    }
}

/// The thread id registered for `thread`; 0 when none is.
#[must_use]
pub fn registered_thread_id(thread: HotThread) -> i64 {
    TIDS[thread as usize].load(Ordering::Relaxed)
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

/// Per-slot accounting between reads.
#[derive(Debug, Clone, Copy, Default)]
struct SlotState {
    tid: i64,
    last_delay: u64,
    last_switches: u64,
    total_delay: u64,
    total_switches: u64,
    baselined: bool,
}

impl SlotState {
    /// Folds one reading of `tid`. A new thread id or a counter that went
    /// backwards is a new baseline (nothing added); otherwise the growth is
    /// added.
    fn observe(&mut self, tid: i64, delay: u64, switches: u64) {
        if !self.baselined || tid != self.tid || delay < self.last_delay {
            self.tid = tid;
            self.baselined = true;
        } else {
            self.total_delay = self.total_delay.saturating_add(delay - self.last_delay);
            self.total_switches = self
                .total_switches
                .saturating_add(switches.saturating_sub(self.last_switches));
        }
        self.last_delay = delay;
        self.last_switches = switches;
    }
}

/// The once-a-second reader and publisher. Lives on the `tv-telemetry`
/// thread only. Series are resolved the first time a slot is read, so a
/// thread that was never seen publishes nothing (absent, not 0).
pub struct SchedPublisher {
    slots: [SlotState; SLOT_COUNT],
    delay: [Option<metrics::Counter>; SLOT_COUNT],
    switches: [Option<metrics::Counter>; SLOT_COUNT],
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
            slots: [SlotState::default(); SLOT_COUNT],
            delay: [const { None }; SLOT_COUNT],
            switches: [const { None }; SLOT_COUNT],
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

    /// One read and publish of every registered thread plus host steal.
    /// Returns how many thread slots were published. Never logs and never
    /// errors: an unreadable file leaves its series as it was.
    pub fn publish(&mut self) -> usize {
        let mut published = 0usize;
        for thread in ALL_HOT_THREADS {
            let i = thread as usize;
            let tid = registered_thread_id(thread);
            if tid == 0 {
                continue;
            }
            let Some((delay, switches)) = self.read_thread(tid) else {
                continue;
            };
            self.slots[i].observe(tid, delay, switches);
            let name = thread.as_str();
            self.delay[i]
                .get_or_insert_with(
                    || metrics::counter!("tv_hot_thread_run_delay_ns_total", "thread" => name),
                )
                .absolute(self.slots[i].total_delay);
            self.switches[i]
                .get_or_insert_with(|| {
                    metrics::counter!("tv_hot_thread_involuntary_switches_total", "thread" => name)
                })
                .absolute(self.slots[i].total_switches);
            published += 1;
        }
        self.path.clear();
        self.path.push_str("/proc/stat");
        if self.read_into_buf()
            && let Some(steal) = parse_proc_stat_steal_ticks(&self.buf)
        {
            self.steal
                .get_or_insert_with(|| metrics::counter!("tv_host_cpu_steal_ticks_total"))
                .absolute(steal);
        }
        published
    }

    /// The accumulated `(run_delay_ns, involuntary_switches)` of `thread`.
    #[cfg(test)]
    fn totals(&self, thread: HotThread) -> (u64, u64) {
        let s = &self.slots[thread as usize];
        (s.total_delay, s.total_switches)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn test_slot_state_baselines_then_adds_growth_and_rebaselines_on_a_new_thread() {
        let mut s = SlotState::default();
        s.observe(7, 1_000, 3);
        assert_eq!(
            (s.total_delay, s.total_switches),
            (0, 0),
            "first read is a baseline"
        );
        s.observe(7, 1_500, 5);
        assert_eq!((s.total_delay, s.total_switches), (500, 2));
        // The drain moved to another worker: its counters are not ours.
        s.observe(9, 900_000, 100);
        assert_eq!((s.total_delay, s.total_switches), (500, 2));
        s.observe(9, 900_100, 101);
        assert_eq!((s.total_delay, s.total_switches), (600, 3));
        // A counter that went backwards (a reused id) re-baselines.
        s.observe(9, 10, 1);
        assert_eq!((s.total_delay, s.total_switches), (600, 3));
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

    /// On Linux the calling thread's id is readable, the drain note stores
    /// it, and one publish reads that thread; elsewhere nothing is
    /// published and nothing fails.
    #[test]
    fn test_note_frame_drain_thread_and_publish_read_this_thread() {
        note_frame_drain_thread();
        let mut publisher = SchedPublisher::new();
        let published = publisher.publish();
        if cfg!(target_os = "linux") && std::path::Path::new("/proc/thread-self").exists() {
            let tid = registered_thread_id(HotThread::FrameDrain);
            assert!(tid > 0, "the drain slot holds this thread's id");
            assert!(published >= 1, "the drain slot was read");
            // A second read adds growth (never negative).
            let _ = publisher.publish();
            let (delay, switches) = publisher.totals(HotThread::FrameDrain);
            assert!(delay < u64::MAX && switches < u64::MAX);
        } else {
            assert_eq!(published, 0);
        }
    }

    #[test]
    fn test_registered_thread_id_follows_the_drain_note() {
        note_frame_drain_thread();
        let tid = registered_thread_id(HotThread::FrameDrain);
        if current_thread_id() == 0 {
            assert_eq!(tid, 0, "nothing is stored where the id cannot be read");
        } else {
            assert!(tid > 0);
        }
    }

    /// A reader registration takes a slot on Linux; past the reader slots it
    /// is refused, never written into the drain slot.
    #[test]
    fn test_register_ws_reader_thread_fills_reader_slots_only() {
        if current_thread_id() == 0 {
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
        assert!(!taken.contains(&HotThread::FrameDrain));
    }
}
