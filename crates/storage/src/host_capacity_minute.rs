//! `host_capacity_minute` and `capacity_verdict_daily` — the live answer to
//! "does the 2-core box keep every in-memory tick step under 1 µs with the
//! database keeping up, or is the 4-core box needed?" (owner, 2026-10-30).
//!
//! ## What it records
//!
//! Once a minute, on the `tv-telemetry` thread (never on the frame path), one
//! host row: how long each hot thread group waited for a CPU (the kernel's
//! `run_delay`, from [`crate::hot_thread_sched`]), process and host CPU use,
//! disk throughput and write latency of the device behind the data folder,
//! the QuestDB WAL apply backlog the existing watcher already reads, and the
//! ticks accepted, not landed and lost that minute. After 15:40 IST, one
//! verdict row for the day, built from the minutes this process saw.
//!
//! ## Tables
//!
//! ```sql
//! CREATE TABLE IF NOT EXISTS host_capacity_minute (
//!     ts                             TIMESTAMP,  -- minute start (IST nanos)
//!     feed                           SYMBOL,     -- dhan / ...
//!     sched_seconds                  LONG,       -- one-second scheduling reads folded in
//!     reader_cpu_wait_ns             LONG,       -- socket reader threads, total wait in the minute
//!     reader_cpu_wait_max_ns_per_s   LONG,       -- worst one-second wait of the reader threads
//!     drain_cpu_wait_ns              LONG,       -- main-runtime workers (frame drain, seal writer), total
//!     drain_cpu_wait_max_ns_per_s    LONG,       -- worst one-second wait of those workers
//!     wal_writer_cpu_wait_ns         LONG,       -- WAL writer thread, total
//!     wal_writer_cpu_wait_max_ns_per_s LONG,     -- worst one-second wait of the WAL writer
//!     process_cpu_pct                DOUBLE,     -- this process, percent of all cores
//!     host_cpu_busy_pct              DOUBLE,     -- whole host, percent of all cores
//!     host_cpu_steal_pct             DOUBLE,     -- hypervisor steal, percent of all cores
//!     host_cores                     LONG,       -- cores the process may run on
//!     disk_read_bytes_per_s          LONG,       -- device behind the data folder
//!     disk_write_bytes_per_s         LONG,       -- same device
//!     disk_write_await_us            LONG,       -- mean time per completed write
//!     questdb_backlog_txns           LONG,       -- largest WAL apply lag at the watcher's last poll
//!     questdb_backlog_growing_tables LONG,       -- tables whose apply lag is growing
//!     ticks_accepted                 LONG,       -- `ticks` rows appended this minute
//!     ticks_not_landed               LONG,       -- rows spilled, failed or deferred to replay
//!     ticks_lost                     LONG,       -- `tv_ticks_lost_total` growth
//!     ring_refused_frames            LONG        -- main-feed frames the ring refused (they bypass every stage)
//! ) timestamp(ts) PARTITION BY DAY
//!   DEDUP UPSERT KEYS(ts, feed);
//!
//! CREATE TABLE IF NOT EXISTS capacity_verdict_daily (
//!     ts                               TIMESTAMP, -- IST midnight of the day
//!     trading_date_ist                 TIMESTAMP, -- same instant
//!     feed                             SYMBOL,
//!     verdict                          SYMBOL,    -- UPGRADE_NEEDED / NOT_NEEDED / UNKNOWN
//!     reasons                          VARCHAR,   -- why, one clause per signal
//!     session_minutes                  LONG,      -- 09:15–15:30 minutes this process saw
//!     tick_step_p99_result             SYMBOL,    -- pass / fail / unknown
//!     worst_tick_step                  SYMBOL,    -- stage of the worst p99
//!     worst_tick_step_p99_ns           LONG,      -- its p99 bucket upper bound
//!     ticks_over_1ms_result            SYMBOL,
//!     minutes_with_tick_over_1ms       LONG,
//!     cpu_wait_result                  SYMBOL,
//!     worst_reader_cpu_wait_ns_per_s   LONG,
//!     worst_drain_cpu_wait_ns_per_s    LONG,
//!     disk_result                      SYMBOL,
//!     disk_hot_minutes                 LONG,      -- minutes at or above 90% of 125 MiB/s
//!     backlog_result                   SYMBOL,
//!     backlog_txns_at_close            LONG,
//!     backlog_growing_tables_at_close  LONG,
//!     worst_host_cpu_busy_pct          DOUBLE,
//!     process_start                    TIMESTAMP  -- this process's first minute
//! ) timestamp(ts) PARTITION BY DAY
//!   DEDUP UPSERT KEYS(ts, trading_date_ist, feed);
//! ```
//!
//! DEDUP carries `feed` (operator override 2026-06-28). Per host and per day,
//! not per instrument, so I-P1-11's `(security_id, exchange_segment)` is N/A.
//! A NULL column means "not measured" (no `/proc`, no device, no thread
//! registered, watcher blind), never zero.
//!
//! ## The verdict (pure: [`decide_verdict`])
//!
//! | Signal | Pass when |
//! |---|---|
//! | tick-step p99 | every minute's p99 bound ≤ 1,024 ns on `socket_to_wal`, `ring_dwell`, `drain_main_frame`, `receipt_to_fold` |
//! | ticks over 1 ms | no session minute with a `receipt_to_fold` sample over 1 ms |
//! | CPU wait | the reader and drain groups never waited ≥ 1 ms for a CPU within one second |
//! | disk | at most 5 minutes at or above 90% of 125 MiB/s (read + write; the gp3 cap counts both) |
//! | backlog | at the first minute at or after 15:40, no table's apply lag growing and the backlog ≤ 500 transactions |
//!
//! * **UNKNOWN** when fewer than 300 of the 375 session minutes were seen, or
//!   any signal had no data. Never a green verdict on zero samples.
//! * **UPGRADE_NEEDED** when the CPU-wait signal fails, or the backlog fails
//!   while the host was at least 90% busy: those are the failures more cores
//!   fix.
//! * **NOT_NEEDED** otherwise. A failing tick-step p99, ticks over 1 ms or a
//!   busy disk with no CPU wait are named in `reasons` but do not ask for
//!   cores: waiting was not the cause, so more cores would not fix them.
//!
//! ## Honest limits
//!
//! * The verdict covers the minutes THIS process saw; after a mid-session
//!   restart each process sees part of the day and is likely UNKNOWN. A
//!   process that saw no session minute writes no verdict, so a restart
//!   after the close never overwrites the day's row.
//! * "Over 1 µs" is over 1,024 ns (the histograms' first bucket bound);
//!   counting exactly 1,000 would need one more compare on the recording
//!   path, which stays untouched.
//! * CPU wait per second is the kernel's `run_delay` growth between two
//!   one-second reads; a thread's first reading is its baseline. The seal
//!   writer runs as a task on the main runtime, so its wait is inside the
//!   drain group, not a series of its own.
//! * Disk figures are minute averages from `/proc/diskstats`, so a burst
//!   inside a minute reads lower than its peak.
//! * `ticks_lost` sums the `tv_ticks_lost_total` series read from this
//!   process's own `/metrics` once a minute by a separate thread; NULL until
//!   the first read.

use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result};
use questdb::ingress::{Buffer, TimestampNanos};

use tickvault_common::constants::SECONDS_PER_DAY;
use tickvault_common::feed::Feed;

use crate::hot_path_latency_minute::LatencyMinuteRows;
use crate::hot_path_telemetry::Stage;
use crate::hot_thread_sched::{HotThread, SchedPublisher};

/// Host table name.
pub const HOST_CAPACITY_MINUTE_TABLE: &str = "host_capacity_minute";

/// Host table DEDUP key.
pub const DEDUP_KEY_HOST_CAPACITY_MINUTE: &str = "ts, feed";

/// Verdict table name.
pub const CAPACITY_VERDICT_DAILY_TABLE: &str = "capacity_verdict_daily";

/// Verdict table DEDUP key: the designated `ts` (IST midnight of the day),
/// the day and the feed.
pub const DEDUP_KEY_CAPACITY_VERDICT_DAILY: &str = "ts, trading_date_ist, feed";

/// `(column, type)` for the host table, in DDL order.
pub const HOST_CAPACITY_MINUTE_COLUMNS: &[(&str, &str)] = &[
    ("ts", "TIMESTAMP"),
    ("feed", "SYMBOL"),
    ("sched_seconds", "LONG"),
    ("reader_cpu_wait_ns", "LONG"),
    ("reader_cpu_wait_max_ns_per_s", "LONG"),
    ("drain_cpu_wait_ns", "LONG"),
    ("drain_cpu_wait_max_ns_per_s", "LONG"),
    ("wal_writer_cpu_wait_ns", "LONG"),
    ("wal_writer_cpu_wait_max_ns_per_s", "LONG"),
    ("process_cpu_pct", "DOUBLE"),
    ("host_cpu_busy_pct", "DOUBLE"),
    ("host_cpu_steal_pct", "DOUBLE"),
    ("host_cores", "LONG"),
    ("disk_read_bytes_per_s", "LONG"),
    ("disk_write_bytes_per_s", "LONG"),
    ("disk_write_await_us", "LONG"),
    ("questdb_backlog_txns", "LONG"),
    ("questdb_backlog_growing_tables", "LONG"),
    ("ticks_accepted", "LONG"),
    ("ticks_not_landed", "LONG"),
    ("ticks_lost", "LONG"),
    ("ring_refused_frames", "LONG"),
];

/// `(column, type)` for the verdict table, in DDL order.
pub const CAPACITY_VERDICT_DAILY_COLUMNS: &[(&str, &str)] = &[
    ("ts", "TIMESTAMP"),
    ("trading_date_ist", "TIMESTAMP"),
    ("feed", "SYMBOL"),
    ("verdict", "SYMBOL"),
    ("reasons", "VARCHAR"),
    ("session_minutes", "LONG"),
    ("tick_step_p99_result", "SYMBOL"),
    ("worst_tick_step", "SYMBOL"),
    ("worst_tick_step_p99_ns", "LONG"),
    ("ticks_over_1ms_result", "SYMBOL"),
    ("minutes_with_tick_over_1ms", "LONG"),
    ("cpu_wait_result", "SYMBOL"),
    ("worst_reader_cpu_wait_ns_per_s", "LONG"),
    ("worst_drain_cpu_wait_ns_per_s", "LONG"),
    ("disk_result", "SYMBOL"),
    ("disk_hot_minutes", "LONG"),
    ("backlog_result", "SYMBOL"),
    ("backlog_txns_at_close", "LONG"),
    ("backlog_growing_tables_at_close", "LONG"),
    ("worst_host_cpu_busy_pct", "DOUBLE"),
    ("process_start", "TIMESTAMP"),
];

/// The folder whose device the disk columns read (relative to the process's
/// working directory, as every `data/...` path in this app is).
pub const HOST_DATA_DIR: &str = "data";

/// Session minutes counted by the verdict: minute starts in [09:15, 15:30) IST.
pub const SESSION_START_SECS_OF_DAY_IST: u32 = 9 * 3_600 + 15 * 60;
/// End (exclusive) of the counted session.
pub const SESSION_END_SECS_OF_DAY_IST: u32 = 15 * 3_600 + 30 * 60;
/// The verdict is written at the first closed minute at or after this.
pub const VERDICT_AT_SECS_OF_DAY_IST: u32 = 15 * 3_600 + 40 * 60;
/// Fewer session minutes than this → UNKNOWN (80% of 375).
pub const MIN_SESSION_MINUTES: u64 = 300;
/// The tick steps judged against the 1 µs target. The runtime-lag probes
/// (a 100 ms sleep's lateness) and the metrics upkeep are not tick steps.
pub const TICK_STEP_STAGES: [Stage; 4] = [
    Stage::SocketToWal,
    Stage::RingDwell,
    Stage::DrainMainFrame,
    Stage::ReceiptToFold,
];
/// A tick step passes when its p99 bucket bound is at most this (the
/// histograms' first bucket, standing in for 1 µs).
pub const TICK_STEP_P99_PASS_NS: u64 = 1_024;
/// A `receipt_to_fold` sample above this counts its minute as "over 1 ms".
pub const TICK_OVER_NS: u64 = 1_000_000;
/// A hot group fails when it waited this long for a CPU within one second.
pub const CPU_WAIT_FAIL_NS_PER_S: u64 = 1_000_000;
/// The root volume's provisioned throughput: 125 MiB/s (gp3, reads plus
/// writes).
pub const DISK_THROUGHPUT_CAP_BYTES_PER_S: u64 = 125 * 1_048_576;
/// "Hot" = at or above 90% of the cap for a whole minute.
pub const DISK_HOT_BYTES_PER_S: u64 = DISK_THROUGHPUT_CAP_BYTES_PER_S / 10 * 9;
/// The disk signal fails above this many hot minutes.
pub const DISK_HOT_MINUTES_PASS: u64 = 5;
/// The backlog signal fails above this many transactions at 15:40 (the
/// after-close write-back's own pause threshold).
pub const BACKLOG_PASS_TXNS: u64 = 500;
/// A failing backlog asks for cores only when the host was at least this
/// busy in its worst session minute.
pub const HOST_BUSY_UPGRADE_PCT: f64 = 90.0;

/// `/proc` jiffies per second (`USER_HZ`), 100 on Linux.
const USER_HZ: u64 = 100;
const NANOS_PER_SECOND: i64 = 1_000_000_000;
const SECTOR_BYTES: u64 = 512;
const PROC_BUF_BYTES: usize = 65_536;
const UNKNOWN: u64 = u64::MAX;

static MAIN_FEED_RING_REFUSED: AtomicU64 = AtomicU64::new(0);
static TICKS_LOST_TOTAL: AtomicU64 = AtomicU64::new(UNKNOWN);

/// Counts one frame the ring refused when `main_feed` is true (depth frames
/// are not counted). O(1): one branch and at most one relaxed add; called
/// on the socket read task's ring-shed path only, never on the normal path.
#[inline]
pub fn note_ring_refused(main_feed: bool) {
    if main_feed {
        MAIN_FEED_RING_REFUSED.fetch_add(1, Ordering::Relaxed);
    }
}

/// Main-feed ring refusals over this process's life. O(1).
#[must_use]
pub fn main_feed_ring_refused_total() -> u64 {
    MAIN_FEED_RING_REFUSED.load(Ordering::Relaxed)
}

/// Publishes this process's `tv_ticks_lost_total` sum (all series), read
/// from its own `/metrics` by the tally thread. O(1).
pub fn publish_ticks_lost_total(total: u64) {
    TICKS_LOST_TOTAL.store(total.min(UNKNOWN - 1), Ordering::Relaxed);
}

/// The last published `tv_ticks_lost_total` sum; `None` before the first.
#[must_use]
pub fn ticks_lost_total() -> Option<u64> {
    let v = TICKS_LOST_TOTAL.load(Ordering::Relaxed);
    (v != UNKNOWN).then_some(v)
}

/// The sum of every sample of counter `name` in a Prometheus text exposition
/// (`name{...} value` and `name value` lines). `None` when no sample is
/// present. Cold: once a minute on the tally thread.
#[must_use]
pub fn prometheus_counter_sum(text: &str, name: &str) -> Option<u64> {
    let mut found = false;
    let mut sum = 0u64;
    for line in text.lines() {
        let Some(rest) = line.strip_prefix(name) else {
            continue;
        };
        if !(rest.starts_with('{') || rest.starts_with(' ')) {
            continue; // a longer metric name sharing the prefix
        }
        let Some(value) = rest.rsplit(' ').next() else {
            continue;
        };
        if let Ok(v) = value.trim().parse::<f64>()
            && v.is_finite()
            && v >= 0.0
        {
            found = true;
            // Counter samples are integers; a float-to-int `as` saturates.
            let n = v as u64;
            sum = sum.saturating_add(n);
        }
    }
    found.then_some(sum)
}

fn secs_of_day(ist_nanos: i64) -> u32 {
    let secs = ist_nanos.div_euclid(NANOS_PER_SECOND);
    u32::try_from(secs.rem_euclid(i64::from(SECONDS_PER_DAY))).unwrap_or(0)
}

fn ist_midnight(ist_nanos: i64) -> i64 {
    let day = i64::from(SECONDS_PER_DAY) * NANOS_PER_SECOND;
    ist_nanos - ist_nanos.rem_euclid(day)
}

/// True when the minute starting at `minute_ist_nanos` is a counted session
/// minute.
#[must_use]
pub fn is_session_minute(minute_ist_nanos: i64) -> bool {
    (SESSION_START_SECS_OF_DAY_IST..SESSION_END_SECS_OF_DAY_IST)
        .contains(&secs_of_day(minute_ist_nanos))
}

// ---------------------------------------------------------------------------
// /proc parsers (pure)
// ---------------------------------------------------------------------------

/// `(utime + stime)` in jiffies from `/proc/self/stat`. The command name is
/// in parentheses and may hold spaces, so fields are counted after the LAST
/// `)`.
#[must_use]
pub fn parse_proc_self_stat_cpu_jiffies(stat: &str) -> Option<u64> {
    let after = &stat[stat.rfind(')')? + 1..];
    let mut fields = after.split_ascii_whitespace();
    // After ")": state(3) ... utime is field 14, stime 15 → indices 11, 12.
    let utime: u64 = fields.nth(11)?.parse().ok()?;
    let stime: u64 = fields.next()?.parse().ok()?;
    Some(utime.saturating_add(stime))
}

/// The aggregate `cpu` line of `/proc/stat`: `(total, busy, steal)` jiffies,
/// where total sums user..steal (guest time is already inside user) and busy
/// is total less idle and iowait.
#[must_use]
pub fn parse_proc_stat_cpu_totals(stat: &str) -> Option<(u64, u64, u64)> {
    let mut fields = stat.lines().next()?.split_ascii_whitespace();
    if fields.next()? != "cpu" {
        return None;
    }
    let mut v = [0u64; 8];
    for slot in &mut v {
        *slot = fields.next()?.parse().ok()?;
    }
    let total = v.iter().fold(0u64, |a, b| a.saturating_add(*b));
    let idle = v[3].saturating_add(v[4]);
    Some((total, total.saturating_sub(idle), v[7]))
}

/// One `/proc/diskstats` device reading.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DiskReading {
    /// Sectors read (512 bytes each).
    pub sectors_read: u64,
    /// Writes completed.
    pub writes: u64,
    /// Sectors written.
    pub sectors_written: u64,
    /// Milliseconds spent writing.
    pub write_ms: u64,
}

/// The line of `/proc/diskstats` for device `major:minor`.
#[must_use]
pub fn parse_diskstats_for(diskstats: &str, major: u64, minor: u64) -> Option<DiskReading> {
    for line in diskstats.lines() {
        let mut f = line.split_ascii_whitespace();
        let (Some(ma), Some(mi)) = (f.next(), f.next()) else {
            continue;
        };
        if ma.parse::<u64>().ok() != Some(major) || mi.parse::<u64>().ok() != Some(minor) {
            continue;
        }
        let rest: [Option<&str>; 8] = [
            f.next(), // name
            f.next(), // reads completed
            f.next(), // reads merged
            f.next(), // sectors read
            f.next(), // ms reading
            f.next(), // writes completed
            f.next(), // writes merged
            f.next(), // sectors written
        ];
        let write_ms = f.next()?.parse().ok()?;
        return Some(DiskReading {
            sectors_read: rest[3]?.parse().ok()?,
            writes: rest[5]?.parse().ok()?,
            sectors_written: rest[7]?.parse().ok()?,
            write_ms,
        });
    }
    None
}

/// Linux `major` and `minor` from a `st_dev` (glibc encoding).
#[must_use]
pub const fn dev_major_minor(dev: u64) -> (u64, u64) {
    let major = ((dev >> 8) & 0xfff) | ((dev >> 32) & !0xfff);
    let minor = (dev & 0xff) | ((dev >> 12) & !0xff);
    (major, minor)
}

/// The `major:minor` of the device holding `path`; `None` off Unix or when
/// the path cannot be read.
#[must_use]
pub fn device_of(path: &str) -> Option<(u64, u64)> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        std::fs::metadata(path)
            .ok()
            .map(|m| dev_major_minor(m.dev()))
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}

// ---------------------------------------------------------------------------
// The minute's host row
// ---------------------------------------------------------------------------

/// One minute of host evidence. `Copy`, fixed size. `None` = not measured.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct HostMinuteRow {
    /// Minute start, IST nanos (the designated timestamp).
    pub minute_ist_nanos: i64,
    /// One-second scheduling reads folded in.
    pub sched_seconds: u64,
    /// Reader threads: total wait, and worst one-second wait.
    pub reader_wait: Option<(u64, u64)>,
    /// Main-runtime workers (drain, seal writer): total and worst second.
    pub drain_wait: Option<(u64, u64)>,
    /// WAL writer: total and worst second.
    pub wal_wait: Option<(u64, u64)>,
    /// This process, percent of all cores.
    pub process_cpu_pct: Option<f64>,
    /// Whole host busy, percent.
    pub host_cpu_busy_pct: Option<f64>,
    /// Hypervisor steal, percent.
    pub host_cpu_steal_pct: Option<f64>,
    /// Cores this process may run on.
    pub host_cores: u64,
    /// Device read bytes per second.
    pub disk_read_bytes_per_s: Option<u64>,
    /// Device write bytes per second.
    pub disk_write_bytes_per_s: Option<u64>,
    /// Mean microseconds per completed write; `None` with no write.
    pub disk_write_await_us: Option<u64>,
    /// Largest WAL apply lag at the watcher's last poll.
    pub questdb_backlog_txns: Option<u64>,
    /// Tables whose apply lag is growing.
    pub questdb_backlog_growing_tables: Option<u64>,
    /// `ticks` rows appended this minute.
    pub ticks_accepted: Option<u64>,
    /// Rows spilled, failed or deferred this minute.
    pub ticks_not_landed: Option<u64>,
    /// `tv_ticks_lost_total` growth this minute.
    pub ticks_lost: Option<u64>,
    /// Main-feed frames the ring refused this minute.
    pub ring_refused_frames: u64,
}

impl HostMinuteRow {
    /// Bytes per second read plus written, when both are known.
    #[must_use]
    pub fn disk_total_bytes_per_s(&self) -> Option<u64> {
        Some(
            self.disk_read_bytes_per_s?
                .saturating_add(self.disk_write_bytes_per_s?),
        )
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct WaitSum {
    seen: bool,
    total: u64,
    max: u64,
}

impl WaitSum {
    fn add(&mut self, second: Option<u64>) {
        if let Some(ns) = second {
            self.seen = true;
            self.total = self.total.saturating_add(ns);
            self.max = self.max.max(ns);
        }
    }

    fn take(&mut self) -> Option<(u64, u64)> {
        let out = self.seen.then_some((self.total, self.max));
        *self = Self::default();
        out
    }
}

/// Folds one-second scheduling reads and reads `/proc` once a minute. Lives
/// on the `tv-telemetry` thread only. Every buffer is built once.
pub struct HostSampler {
    reader: WaitSum,
    drain: WaitSum,
    wal: WaitSum,
    seconds: u64,
    cores: u64,
    device: Option<(u64, u64)>,
    buf: String,
    prev: Option<HostCounters>,
}

/// The cumulative counters behind one minute's deltas.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct HostCounters {
    /// When read, IST nanos.
    pub at_ist_nanos: i64,
    /// Process utime + stime, jiffies.
    pub process_jiffies: Option<u64>,
    /// Host `(total, busy, steal)` jiffies.
    pub host: Option<(u64, u64, u64)>,
    /// The data device's counters.
    pub disk: Option<DiskReading>,
    /// `ticks` rows accepted on the IST day (`(day, rows)`).
    pub ticks_accepted: (i64, u64),
    /// Rows not landed, process total.
    pub not_landed: u64,
    /// `tv_ticks_lost_total` sum.
    pub ticks_lost: Option<u64>,
    /// Main-feed ring refusals, process total.
    pub ring_refused: u64,
}

impl Default for HostSampler {
    fn default() -> Self {
        Self::new()
    }
}

impl HostSampler {
    /// Reads the core count and the data device once (cold).
    #[must_use]
    pub fn new() -> Self {
        let cores =
            std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get) as u64;
        Self {
            reader: WaitSum::default(),
            drain: WaitSum::default(),
            wal: WaitSum::default(),
            seconds: 0,
            cores,
            device: device_of(HOST_DATA_DIR),
            buf: String::with_capacity(PROC_BUF_BYTES),
            prev: None,
        }
    }

    /// Adds the last second's scheduling reads. O(series), no allocation.
    pub fn absorb_second(&mut self, sched: &SchedPublisher) {
        let mut readers: Option<u64> = None;
        for t in [
            HotThread::WsReader0,
            HotThread::WsReader1,
            HotThread::WsReader2,
            HotThread::WsReader3,
        ] {
            if let Some(ns) = sched.last_second_run_delay_ns(t) {
                readers = Some(readers.unwrap_or(0).saturating_add(ns));
            }
        }
        self.absorb_second_values(
            readers,
            sched.last_second_run_delay_ns(HotThread::MainWorkers),
            sched.last_second_run_delay_ns(HotThread::WalWriter),
        );
    }

    fn absorb_second_values(&mut self, readers: Option<u64>, drain: Option<u64>, wal: Option<u64>) {
        self.reader.add(readers);
        self.drain.add(drain);
        self.wal.add(wal);
        self.seconds = self.seconds.saturating_add(1);
    }

    fn read(&mut self, path: &str) -> bool {
        use std::io::Read as _;
        self.buf.clear();
        std::fs::File::open(path)
            .and_then(|mut f| f.read_to_string(&mut self.buf))
            .is_ok()
    }

    /// Reads the cumulative counters now. Cold: once a minute.
    pub fn read_counters(&mut self, now_ist_nanos: i64) -> HostCounters {
        let process_jiffies = if self.read("/proc/self/stat") {
            parse_proc_self_stat_cpu_jiffies(&self.buf)
        } else {
            None
        };
        let host = if self.read("/proc/stat") {
            parse_proc_stat_cpu_totals(&self.buf)
        } else {
            None
        };
        let device = self.device;
        let disk = match device {
            Some((major, minor)) if self.read("/proc/diskstats") => {
                parse_diskstats_for(&self.buf, major, minor)
            }
            _ => None,
        };
        let day =
            crate::tick_conservation_ledger::TickConservationLedger::ist_day_of(now_ist_nanos);
        let ledger = crate::tick_conservation_ledger::ledger();
        HostCounters {
            at_ist_nanos: now_ist_nanos,
            process_jiffies,
            host,
            disk,
            ticks_accepted: (day, ledger.accepted_rows_on(day)),
            not_landed: ledger.rows_not_landed(),
            ticks_lost: ticks_lost_total(),
            ring_refused: main_feed_ring_refused_total(),
        }
    }

    /// Closes the minute starting at `minute_ist_nanos`: reads the counters
    /// at `now_ist_nanos` and builds the row from their growth since the
    /// previous close (the first close of a process has no previous reading,
    /// so its delta columns are NULL). Resets the per-second sums.
    pub fn close_minute(&mut self, minute_ist_nanos: i64, now_ist_nanos: i64) -> HostMinuteRow {
        let now = self.read_counters(now_ist_nanos);
        let row = build_host_row(
            minute_ist_nanos,
            self.prev.as_ref(),
            &now,
            self.cores,
            (self.reader.take(), self.drain.take(), self.wal.take()),
            self.seconds,
            (
                tickvault_common::ingest_shed::wal_apply_lag_max(),
                // The watcher's growing count is 0 until its first poll and
                // is only meaningful once the lag is known.
                tickvault_common::ingest_shed::wal_apply_lag_max()
                    .map(|_| u64::from(tickvault_common::ingest_shed::wal_apply_lag_growing())),
            ),
        );
        self.seconds = 0;
        self.prev = Some(now);
        row
    }
}

/// One group's `(total, worst second)` wait in a minute; `None` = not read.
pub type WaitPair = Option<(u64, u64)>;

/// The reader, drain and WAL writer groups' waits, in that order.
pub type GroupWaits = (WaitPair, WaitPair, WaitPair);

/// Pure: the minute's row from two counter readings. `prev = None` (first
/// minute of a process) leaves every delta column NULL.
#[must_use]
pub fn build_host_row(
    minute_ist_nanos: i64,
    prev: Option<&HostCounters>,
    now: &HostCounters,
    cores: u64,
    waits: GroupWaits,
    sched_seconds: u64,
    backlog: (Option<u64>, Option<u64>),
) -> HostMinuteRow {
    let mut row = HostMinuteRow {
        minute_ist_nanos,
        sched_seconds,
        reader_wait: waits.0,
        drain_wait: waits.1,
        wal_wait: waits.2,
        host_cores: cores,
        questdb_backlog_txns: backlog.0,
        questdb_backlog_growing_tables: backlog.1,
        ..HostMinuteRow::default()
    };
    let Some(prev) = prev else {
        return row;
    };
    let elapsed_ns = now.at_ist_nanos.saturating_sub(prev.at_ist_nanos);
    // A clock step back (or no time passing) makes every rate meaningless.
    if elapsed_ns <= 0 {
        return row;
    }
    let elapsed_s = elapsed_ns as f64 / NANOS_PER_SECOND as f64;
    if let (Some(a), Some(b)) = (prev.process_jiffies, now.process_jiffies)
        && b >= a
    {
        let cpu_s = (b - a) as f64 / USER_HZ as f64;
        row.process_cpu_pct = Some(cpu_s / elapsed_s / cores.max(1) as f64 * 100.0);
    }
    if let (Some((t0, b0, s0)), Some((t1, b1, s1))) = (prev.host, now.host)
        && t1 > t0
    {
        let dt = (t1 - t0) as f64;
        row.host_cpu_busy_pct = Some(b1.saturating_sub(b0) as f64 / dt * 100.0);
        row.host_cpu_steal_pct = Some(s1.saturating_sub(s0) as f64 / dt * 100.0);
    }
    if let (Some(a), Some(b)) = (prev.disk, now.disk)
        && b.sectors_written >= a.sectors_written
        && b.sectors_read >= a.sectors_read
    {
        let per_s = |sectors: u64| -> u64 {
            let bytes = sectors.saturating_mul(SECTOR_BYTES);
            (bytes as f64 / elapsed_s) as u64
        };
        row.disk_read_bytes_per_s = Some(per_s(b.sectors_read - a.sectors_read));
        row.disk_write_bytes_per_s = Some(per_s(b.sectors_written - a.sectors_written));
        // No completed write leaves the await NULL.
        let writes = b.writes.saturating_sub(a.writes);
        let ms = b.write_ms.saturating_sub(a.write_ms);
        row.disk_write_await_us = ms.saturating_mul(1_000).checked_div(writes);
    }
    row.ticks_accepted = Some(if now.ticks_accepted.0 == prev.ticks_accepted.0 {
        now.ticks_accepted.1.saturating_sub(prev.ticks_accepted.1)
    } else {
        // The IST day changed: the new day's count started from zero.
        now.ticks_accepted.1
    });
    row.ticks_not_landed = Some(now.not_landed.saturating_sub(prev.not_landed));
    row.ticks_lost = match (prev.ticks_lost, now.ticks_lost) {
        (Some(a), Some(b)) => Some(b.saturating_sub(a)),
        _ => None,
    };
    row.ring_refused_frames = now.ring_refused.saturating_sub(prev.ring_refused);
    row
}

// ---------------------------------------------------------------------------
// The daily verdict
// ---------------------------------------------------------------------------

/// One signal's result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignalResult {
    /// Inside its bound.
    Pass,
    /// Outside its bound.
    Fail,
    /// No data.
    Unknown,
}

impl SignalResult {
    /// The SYMBOL value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::Fail => "fail",
            Self::Unknown => "unknown",
        }
    }
}

/// The day's verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// More cores would fix a measured failure.
    UpgradeNeeded,
    /// No failure more cores would fix.
    NotNeeded,
    /// Not enough data to say.
    Unknown,
}

impl Verdict {
    /// The SYMBOL value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::UpgradeNeeded => "UPGRADE_NEEDED",
            Self::NotNeeded => "NOT_NEEDED",
            Self::Unknown => "UNKNOWN",
        }
    }
}

/// One day's verdict row. `Copy`; the `reasons` text is built by the writer
/// thread from it ([`verdict_reasons`]).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CapacityVerdictRow {
    /// IST midnight of the day (designated timestamp and `trading_date_ist`).
    pub day_ist_nanos: i64,
    /// The verdict.
    pub verdict: Verdict,
    /// Session minutes seen.
    pub session_minutes: u64,
    /// Tick-step p99 result, the worst stage and its p99 bound (`u64::MAX`
    /// = above the histogram's top bound).
    pub tick_step: (SignalResult, Option<Stage>, Option<u64>),
    /// Ticks-over-1-ms result and the count of such minutes.
    pub ticks_over_1ms: (SignalResult, u64),
    /// CPU-wait result, worst reader and worst drain second.
    pub cpu_wait: (SignalResult, Option<u64>, Option<u64>),
    /// Disk result and hot minutes.
    pub disk: (SignalResult, u64),
    /// Backlog result, backlog and growing tables at the close.
    pub backlog: (SignalResult, Option<u64>, Option<u64>),
    /// Worst session minute's host busy percent.
    pub worst_host_cpu_busy_pct: Option<f64>,
    /// This process's first minute, IST nanos.
    pub process_start_ist_nanos: i64,
}

/// What the day accumulated, before the decision. Pure input to
/// [`decide_verdict`].
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct DayEvidence {
    /// Session minutes seen.
    pub session_minutes: u64,
    /// Worst p99 bound per tick step (index into [`TICK_STEP_STAGES`]);
    /// `None` = no minute with samples.
    pub worst_step_p99: [Option<u64>; 4],
    /// Session minutes whose `receipt_to_fold` max exceeded 1 ms.
    pub minutes_over_1ms: u64,
    /// Worst one-second wait of the reader and the drain groups.
    pub worst_reader_wait: Option<u64>,
    /// Worst one-second wait of the drain group.
    pub worst_drain_wait: Option<u64>,
    /// Session minutes with a disk reading, and how many were hot.
    pub disk_minutes: u64,
    /// Hot minutes.
    pub disk_hot_minutes: u64,
    /// Backlog and growing tables at the verdict minute.
    pub backlog_at_close: (Option<u64>, Option<u64>),
    /// Worst session minute's host busy percent.
    pub worst_host_busy_pct: Option<f64>,
}

/// Pure: the five signals and the verdict from one day's evidence.
#[must_use]
pub fn decide_verdict(e: &DayEvidence) -> (Verdict, [SignalResult; 5]) {
    let step = if e.worst_step_p99.iter().all(Option::is_some) {
        if e.worst_step_p99
            .iter()
            .all(|p| p.is_some_and(|n| n <= TICK_STEP_P99_PASS_NS))
        {
            SignalResult::Pass
        } else {
            SignalResult::Fail
        }
    } else {
        SignalResult::Unknown
    };
    let over_1ms = match e.worst_step_p99[3] {
        None => SignalResult::Unknown,
        Some(_) if e.minutes_over_1ms == 0 => SignalResult::Pass,
        Some(_) => SignalResult::Fail,
    };
    let cpu = match (e.worst_reader_wait, e.worst_drain_wait) {
        (Some(r), Some(d)) => {
            if r < CPU_WAIT_FAIL_NS_PER_S && d < CPU_WAIT_FAIL_NS_PER_S {
                SignalResult::Pass
            } else {
                SignalResult::Fail
            }
        }
        _ => SignalResult::Unknown,
    };
    let disk = if e.disk_minutes == 0 {
        SignalResult::Unknown
    } else if e.disk_hot_minutes <= DISK_HOT_MINUTES_PASS {
        SignalResult::Pass
    } else {
        SignalResult::Fail
    };
    let backlog = match e.backlog_at_close {
        (Some(txns), Some(growing)) => {
            if growing == 0 && txns <= BACKLOG_PASS_TXNS {
                SignalResult::Pass
            } else {
                SignalResult::Fail
            }
        }
        _ => SignalResult::Unknown,
    };
    let results = [step, over_1ms, cpu, disk, backlog];
    let verdict =
        if e.session_minutes < MIN_SESSION_MINUTES || results.contains(&SignalResult::Unknown) {
            Verdict::Unknown
        } else if cpu == SignalResult::Fail
            || (backlog == SignalResult::Fail
                && e.worst_host_busy_pct
                    .is_some_and(|p| p >= HOST_BUSY_UPGRADE_PCT))
        {
            Verdict::UpgradeNeeded
        } else {
            Verdict::NotNeeded
        };
    (verdict, results)
}

/// The `reasons` text: one clause per signal, plus the session coverage.
/// Writer thread only (allocates).
#[must_use]
pub fn verdict_reasons(row: &CapacityVerdictRow) -> String {
    let mut s = String::with_capacity(512);
    let push = |s: &mut String, text: &str| {
        if !s.is_empty() {
            s.push_str("; ");
        }
        s.push_str(text);
    };
    if row.session_minutes < MIN_SESSION_MINUTES {
        push(
            &mut s,
            &format!(
                "only {} of 375 session minutes seen (need {MIN_SESSION_MINUTES})",
                row.session_minutes
            ),
        );
    }
    let stage = row.tick_step.1.map_or("none", Stage::as_str);
    let p99 = row
        .tick_step
        .2
        .map_or_else(|| "none".to_owned(), |n| n.to_string());
    push(
        &mut s,
        &format!(
            "tick-step p99 {}: worst {stage} at most {p99} ns (bound {TICK_STEP_P99_PASS_NS})",
            row.tick_step.0.as_str()
        ),
    );
    push(
        &mut s,
        &format!(
            "ticks over 1 ms {}: {} minutes",
            row.ticks_over_1ms.0.as_str(),
            row.ticks_over_1ms.1
        ),
    );
    let ns = |v: Option<u64>| v.map_or_else(|| "none".to_owned(), |n| n.to_string());
    push(
        &mut s,
        &format!(
            "cpu wait {}: worst reader {} ns/s, drain {} ns/s (fail at {CPU_WAIT_FAIL_NS_PER_S})",
            row.cpu_wait.0.as_str(),
            ns(row.cpu_wait.1),
            ns(row.cpu_wait.2)
        ),
    );
    push(
        &mut s,
        &format!(
            "disk {}: {} minutes at or above 90% of 125 MiB/s",
            row.disk.0.as_str(),
            row.disk.1
        ),
    );
    push(
        &mut s,
        &format!(
            "questdb backlog {}: {} txns, {} growing tables at 15:40",
            row.backlog.0.as_str(),
            ns(row.backlog.1),
            ns(row.backlog.2)
        ),
    );
    if row.verdict == Verdict::NotNeeded
        && (row.tick_step.0 == SignalResult::Fail || row.ticks_over_1ms.0 == SignalResult::Fail)
    {
        push(
            &mut s,
            "target missed without CPU wait: more cores would not fix it",
        );
    }
    s
}

/// Accumulates one IST day from the closed minutes. Lives on the
/// `tv-telemetry` thread. Fixed fields, no allocation.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DayVerdict {
    day: Option<i64>,
    evidence: DayEvidence,
    emitted: bool,
    process_start: Option<i64>,
    skipped_backwards: u64,
}

impl Default for DayVerdict {
    fn default() -> Self {
        Self::new()
    }
}

impl DayVerdict {
    /// Nothing seen yet.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            day: None,
            evidence: DayEvidence {
                session_minutes: 0,
                worst_step_p99: [None; 4],
                minutes_over_1ms: 0,
                worst_reader_wait: None,
                worst_drain_wait: None,
                disk_minutes: 0,
                disk_hot_minutes: 0,
                backlog_at_close: (None, None),
                worst_host_busy_pct: None,
            },
            emitted: false,
            process_start: None,
            skipped_backwards: 0,
        }
    }

    /// Minutes ignored because they belonged to an earlier day than the one
    /// accumulating (a clock stepped back across midnight).
    #[must_use]
    pub const fn skipped_backwards(&self) -> u64 {
        self.skipped_backwards
    }

    /// The evidence so far.
    #[must_use]
    pub const fn evidence(&self) -> &DayEvidence {
        &self.evidence
    }

    /// Folds one closed minute. Returns a verdict row when this minute is the
    /// first at or after 15:40 IST of a day this process saw session minutes
    /// of, or when the day rolled over before that happened (the previous
    /// day's verdict, decided on what it saw). O(stages), no allocation.
    pub fn observe(
        &mut self,
        latency: &LatencyMinuteRows,
        host: &HostMinuteRow,
    ) -> Option<CapacityVerdictRow> {
        let minute = host.minute_ist_nanos;
        let day = ist_midnight(minute);
        self.process_start.get_or_insert(minute);
        let mut out = None;
        match self.day {
            Some(current) if day < current => {
                self.skipped_backwards = self.skipped_backwards.saturating_add(1);
                return None;
            }
            Some(current) if day > current => {
                if !self.emitted && self.evidence.session_minutes > 0 {
                    out = Some(self.row(current));
                }
                self.reset(day);
            }
            Some(_) => {}
            None => self.reset(day),
        }
        if is_session_minute(minute) {
            self.fold_session_minute(latency, host);
        }
        if out.is_none()
            && !self.emitted
            && secs_of_day(minute) >= VERDICT_AT_SECS_OF_DAY_IST
            && self.evidence.session_minutes > 0
        {
            self.evidence.backlog_at_close = (
                host.questdb_backlog_txns,
                host.questdb_backlog_growing_tables,
            );
            self.emitted = true;
            out = Some(self.row(day));
        }
        out
    }

    fn reset(&mut self, day: i64) {
        let start = self.process_start;
        let skipped = self.skipped_backwards;
        *self = Self::new();
        self.day = Some(day);
        self.process_start = start;
        self.skipped_backwards = skipped;
    }

    fn fold_session_minute(&mut self, latency: &LatencyMinuteRows, host: &HostMinuteRow) {
        let e = &mut self.evidence;
        e.session_minutes = e.session_minutes.saturating_add(1);
        for (slot, stage) in e.worst_step_p99.iter_mut().zip(TICK_STEP_STAGES.iter()) {
            let row = &latency[*stage as usize];
            if row.samples == 0 {
                continue;
            }
            // A NULL p99 with samples is above the histogram's top bound.
            let p99 = row.p99_ns.unwrap_or(u64::MAX);
            *slot = Some(slot.map_or(p99, |w| w.max(p99)));
        }
        if latency[Stage::ReceiptToFold as usize].max_ns > TICK_OVER_NS {
            e.minutes_over_1ms = e.minutes_over_1ms.saturating_add(1);
        }
        if let Some((_, max)) = host.reader_wait {
            e.worst_reader_wait = Some(e.worst_reader_wait.map_or(max, |w| w.max(max)));
        }
        if let Some((_, max)) = host.drain_wait {
            e.worst_drain_wait = Some(e.worst_drain_wait.map_or(max, |w| w.max(max)));
        }
        if let Some(total) = host.disk_total_bytes_per_s() {
            e.disk_minutes = e.disk_minutes.saturating_add(1);
            if total >= DISK_HOT_BYTES_PER_S {
                e.disk_hot_minutes = e.disk_hot_minutes.saturating_add(1);
            }
        }
        if let Some(busy) = host.host_cpu_busy_pct {
            e.worst_host_busy_pct = Some(e.worst_host_busy_pct.map_or(busy, |w| w.max(busy)));
        }
    }

    fn row(&self, day: i64) -> CapacityVerdictRow {
        let e = &self.evidence;
        let (verdict, r) = decide_verdict(e);
        let (worst_stage, worst_p99) = e
            .worst_step_p99
            .iter()
            .zip(TICK_STEP_STAGES.iter())
            .filter_map(|(p, s)| p.map(|n| (*s, n)))
            .max_by_key(|(_, n)| *n)
            .map_or((None, None), |(s, n)| (Some(s), Some(n)));
        CapacityVerdictRow {
            day_ist_nanos: day,
            verdict,
            session_minutes: e.session_minutes,
            tick_step: (r[0], worst_stage, worst_p99),
            ticks_over_1ms: (r[1], e.minutes_over_1ms),
            cpu_wait: (r[2], e.worst_reader_wait, e.worst_drain_wait),
            disk: (r[3], e.disk_hot_minutes),
            backlog: (r[4], e.backlog_at_close.0, e.backlog_at_close.1),
            worst_host_cpu_busy_pct: e.worst_host_busy_pct,
            process_start_ist_nanos: self.process_start.unwrap_or(day),
        }
    }
}

// ---------------------------------------------------------------------------
// DDL and ILP rows (writer thread only)
// ---------------------------------------------------------------------------

fn ddl_statements(table: &str, columns: &[(&str, &str)], dedup: &str) -> Vec<String> {
    // O(1) EXEMPT: begin — boot-time DDL build over a fixed column list
    let cols = columns
        .iter()
        .map(|(name, ty)| format!("{name} {ty}"))
        .collect::<Vec<_>>()
        .join(", ");
    let mut statements = vec![format!(
        "CREATE TABLE IF NOT EXISTS {table} ({cols}) \
         timestamp(ts) PARTITION BY DAY \
         DEDUP UPSERT KEYS({dedup});"
    )];
    for (col, ty) in columns {
        if *col == "ts" {
            continue;
        }
        statements.push(format!(
            "ALTER TABLE {table} ADD COLUMN IF NOT EXISTS {col} {ty};"
        ));
    }
    // O(1) EXEMPT: end
    statements.push(format!(
        "ALTER TABLE {table} DEDUP ENABLE UPSERT KEYS({dedup});"
    ));
    statements
}

/// `CREATE` → per-column `ALTER ADD COLUMN IF NOT EXISTS` → `DEDUP ENABLE`
/// for `host_capacity_minute`. Never a drop. Pure.
#[must_use]
pub fn host_capacity_minute_ddl_statements() -> Vec<String> {
    ddl_statements(
        HOST_CAPACITY_MINUTE_TABLE,
        HOST_CAPACITY_MINUTE_COLUMNS,
        DEDUP_KEY_HOST_CAPACITY_MINUTE,
    )
}

/// The same for `capacity_verdict_daily`. Pure.
#[must_use]
pub fn capacity_verdict_daily_ddl_statements() -> Vec<String> {
    ddl_statements(
        CAPACITY_VERDICT_DAILY_TABLE,
        CAPACITY_VERDICT_DAILY_COLUMNS,
        DEDUP_KEY_CAPACITY_VERDICT_DAILY,
    )
}

fn to_i64(n: u64) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

/// Appends one host row (a NULL is the column left out). Writer thread only.
///
/// # Errors
/// Propagates ILP buffer errors.
pub fn append_host_row(buffer: &mut Buffer, r: &HostMinuteRow) -> Result<()> {
    let mut row = buffer
        .table(HOST_CAPACITY_MINUTE_TABLE)
        .context("table")?
        .symbol("feed", Feed::Dhan.as_str())
        .context("feed")?
        .column_i64("sched_seconds", to_i64(r.sched_seconds))
        .context("sched_seconds")?
        .column_i64("host_cores", to_i64(r.host_cores))
        .context("host_cores")?
        .column_i64("ring_refused_frames", to_i64(r.ring_refused_frames))
        .context("ring_refused_frames")?;
    if let Some((total, max)) = r.reader_wait {
        row = row
            .column_i64("reader_cpu_wait_ns", to_i64(total))
            .context("reader_cpu_wait_ns")?
            .column_i64("reader_cpu_wait_max_ns_per_s", to_i64(max))
            .context("reader_cpu_wait_max_ns_per_s")?;
    }
    if let Some((total, max)) = r.drain_wait {
        row = row
            .column_i64("drain_cpu_wait_ns", to_i64(total))
            .context("drain_cpu_wait_ns")?
            .column_i64("drain_cpu_wait_max_ns_per_s", to_i64(max))
            .context("drain_cpu_wait_max_ns_per_s")?;
    }
    if let Some((total, max)) = r.wal_wait {
        row = row
            .column_i64("wal_writer_cpu_wait_ns", to_i64(total))
            .context("wal_writer_cpu_wait_ns")?
            .column_i64("wal_writer_cpu_wait_max_ns_per_s", to_i64(max))
            .context("wal_writer_cpu_wait_max_ns_per_s")?;
    }
    if let Some(v) = r.process_cpu_pct {
        row = row
            .column_f64("process_cpu_pct", v)
            .context("process_cpu_pct")?;
    }
    if let Some(v) = r.host_cpu_busy_pct {
        row = row
            .column_f64("host_cpu_busy_pct", v)
            .context("host_cpu_busy_pct")?;
    }
    if let Some(v) = r.host_cpu_steal_pct {
        row = row
            .column_f64("host_cpu_steal_pct", v)
            .context("host_cpu_steal_pct")?;
    }
    let longs: [(&str, Option<u64>); 8] = [
        ("disk_read_bytes_per_s", r.disk_read_bytes_per_s),
        ("disk_write_bytes_per_s", r.disk_write_bytes_per_s),
        ("disk_write_await_us", r.disk_write_await_us),
        ("questdb_backlog_txns", r.questdb_backlog_txns),
        (
            "questdb_backlog_growing_tables",
            r.questdb_backlog_growing_tables,
        ),
        ("ticks_accepted", r.ticks_accepted),
        ("ticks_not_landed", r.ticks_not_landed),
        ("ticks_lost", r.ticks_lost),
    ];
    for (name, value) in longs {
        if let Some(v) = value {
            row = row.column_i64(name, to_i64(v)).context(name)?;
        }
    }
    row.at(TimestampNanos::new(r.minute_ist_nanos))
        .context("designated timestamp")?;
    Ok(())
}

/// Appends one verdict row, with its `reasons` text. Writer thread only.
///
/// # Errors
/// Propagates ILP buffer errors.
pub fn append_verdict_row(buffer: &mut Buffer, r: &CapacityVerdictRow) -> Result<()> {
    let reasons = verdict_reasons(r);
    let mut row = buffer
        .table(CAPACITY_VERDICT_DAILY_TABLE)
        .context("table")?
        .symbol("feed", Feed::Dhan.as_str())
        .context("feed")?
        .symbol("verdict", r.verdict.as_str())
        .context("verdict")?
        .symbol("tick_step_p99_result", r.tick_step.0.as_str())
        .context("tick_step_p99_result")?
        .symbol("ticks_over_1ms_result", r.ticks_over_1ms.0.as_str())
        .context("ticks_over_1ms_result")?
        .symbol("cpu_wait_result", r.cpu_wait.0.as_str())
        .context("cpu_wait_result")?
        .symbol("disk_result", r.disk.0.as_str())
        .context("disk_result")?
        .symbol("backlog_result", r.backlog.0.as_str())
        .context("backlog_result")?;
    if let Some(stage) = r.tick_step.1 {
        row = row
            .symbol("worst_tick_step", stage.as_str())
            .context("worst_tick_step")?;
    }
    row = row
        .column_ts("trading_date_ist", TimestampNanos::new(r.day_ist_nanos))
        .context("trading_date_ist")?
        .column_str("reasons", reasons.as_str())
        .context("reasons")?
        .column_i64("session_minutes", to_i64(r.session_minutes))
        .context("session_minutes")?
        .column_i64("minutes_with_tick_over_1ms", to_i64(r.ticks_over_1ms.1))
        .context("minutes_with_tick_over_1ms")?
        .column_i64("disk_hot_minutes", to_i64(r.disk.1))
        .context("disk_hot_minutes")?
        .column_ts(
            "process_start",
            TimestampNanos::new(r.process_start_ist_nanos),
        )
        .context("process_start")?;
    let longs: [(&str, Option<u64>); 5] = [
        ("worst_tick_step_p99_ns", r.tick_step.2),
        ("worst_reader_cpu_wait_ns_per_s", r.cpu_wait.1),
        ("worst_drain_cpu_wait_ns_per_s", r.cpu_wait.2),
        ("backlog_txns_at_close", r.backlog.1),
        ("backlog_growing_tables_at_close", r.backlog.2),
    ];
    for (name, value) in longs {
        if let Some(v) = value {
            row = row.column_i64(name, to_i64(v)).context(name)?;
        }
    }
    if let Some(v) = r.worst_host_cpu_busy_pct {
        row = row
            .column_f64("worst_host_cpu_busy_pct", v)
            .context("worst_host_cpu_busy_pct")?;
    }
    row.at(TimestampNanos::new(r.day_ist_nanos))
        .context("designated timestamp")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hot_path_latency_minute::{LatencyMinuteRow, minute_floor_ist_nanos};
    use crate::hot_path_telemetry::ALL_STAGES;
    use crate::hot_thread_sched::MAX_WS_READER_SLOTS;

    const NS: i64 = 1_000_000_000;
    /// IST midnight of some day.
    const DAY0: i64 = 20_000 * 86_400 * NS;

    fn at(day: i64, h: i64, m: i64) -> i64 {
        DAY0 + day * 86_400 * NS + (h * 3_600 + m * 60) * NS
    }

    fn latency(minute: i64, p99: u64, max: u64) -> LatencyMinuteRows {
        ALL_STAGES.map(|stage| LatencyMinuteRow {
            minute_ist_nanos: minute,
            stage,
            histogram: "log2",
            samples: 100,
            p50_ns: Some(512.min(p99)),
            p99_ns: Some(p99),
            p999_ns: Some(p99),
            max_ns: max,
            samples_over_1024ns: 0,
            publishes: 60,
        })
    }

    fn host(minute: i64, wait: u64, disk_bps: u64, busy: f64) -> HostMinuteRow {
        HostMinuteRow {
            minute_ist_nanos: minute,
            sched_seconds: 60,
            reader_wait: Some((wait, wait)),
            drain_wait: Some((wait, wait)),
            wal_wait: Some((wait, wait)),
            host_cpu_busy_pct: Some(busy),
            disk_read_bytes_per_s: Some(0),
            disk_write_bytes_per_s: Some(disk_bps),
            questdb_backlog_txns: Some(3),
            questdb_backlog_growing_tables: Some(0),
            ..HostMinuteRow::default()
        }
    }

    /// Runs a whole session (09:15–15:30) then the 15:40 minute.
    fn run_day(
        v: &mut DayVerdict,
        day: i64,
        mut each: impl FnMut(i64) -> (LatencyMinuteRows, HostMinuteRow),
    ) -> Option<CapacityVerdictRow> {
        let mut out = None;
        let mut m = at(day, 9, 15);
        while m < at(day, 15, 30) {
            let (l, h) = each(m);
            if let Some(r) = v.observe(&l, &h) {
                out = Some(r);
            }
            m += 60 * NS;
        }
        let (l, mut h) = each(at(day, 15, 40));
        h.minute_ist_nanos = at(day, 15, 40);
        v.observe(&l, &h).or(out)
    }

    fn good(m: i64) -> (LatencyMinuteRows, HostMinuteRow) {
        (latency(m, 1_024, 900), host(m, 10_000, 10_000_000, 50.0))
    }

    fn evidence_pass() -> DayEvidence {
        DayEvidence {
            session_minutes: 375,
            worst_step_p99: [Some(1_024); 4],
            minutes_over_1ms: 0,
            worst_reader_wait: Some(10_000),
            worst_drain_wait: Some(10_000),
            disk_minutes: 375,
            disk_hot_minutes: 0,
            backlog_at_close: (Some(3), Some(0)),
            worst_host_busy_pct: Some(50.0),
        }
    }

    #[test]
    fn test_decide_verdict_all_pass_is_not_needed() {
        let (v, r) = decide_verdict(&evidence_pass());
        assert_eq!(v, Verdict::NotNeeded);
        assert_eq!(r, [SignalResult::Pass; 5]);
    }

    /// Every single-signal failure, and what it asks for.
    #[test]
    fn test_decide_verdict_each_single_failure() {
        let mut e = evidence_pass();
        e.worst_step_p99[1] = Some(2_048);
        assert_eq!(
            decide_verdict(&e).0,
            Verdict::NotNeeded,
            "p99 alone: not cores"
        );
        assert_eq!(decide_verdict(&e).1[0], SignalResult::Fail);

        let mut e = evidence_pass();
        e.minutes_over_1ms = 3;
        assert_eq!(
            decide_verdict(&e),
            (
                Verdict::NotNeeded,
                [
                    SignalResult::Pass,
                    SignalResult::Fail,
                    SignalResult::Pass,
                    SignalResult::Pass,
                    SignalResult::Pass
                ]
            )
        );

        let mut e = evidence_pass();
        e.worst_reader_wait = Some(CPU_WAIT_FAIL_NS_PER_S);
        assert_eq!(
            decide_verdict(&e).0,
            Verdict::UpgradeNeeded,
            "reader waited"
        );
        let mut e = evidence_pass();
        e.worst_drain_wait = Some(CPU_WAIT_FAIL_NS_PER_S + 1);
        assert_eq!(decide_verdict(&e).0, Verdict::UpgradeNeeded, "drain waited");

        let mut e = evidence_pass();
        e.disk_hot_minutes = DISK_HOT_MINUTES_PASS + 1;
        assert_eq!(
            decide_verdict(&e).0,
            Verdict::NotNeeded,
            "disk is not cores"
        );
        assert_eq!(decide_verdict(&e).1[3], SignalResult::Fail);
        e.disk_hot_minutes = DISK_HOT_MINUTES_PASS;
        assert_eq!(
            decide_verdict(&e).1[3],
            SignalResult::Pass,
            "boundary passes"
        );

        let mut e = evidence_pass();
        e.backlog_at_close = (Some(BACKLOG_PASS_TXNS + 1), Some(0));
        assert_eq!(
            decide_verdict(&e).0,
            Verdict::NotNeeded,
            "backlog, host idle"
        );
        e.worst_host_busy_pct = Some(HOST_BUSY_UPGRADE_PCT);
        assert_eq!(
            decide_verdict(&e).0,
            Verdict::UpgradeNeeded,
            "backlog, host busy"
        );
        let mut e = evidence_pass();
        e.backlog_at_close = (Some(0), Some(1));
        assert_eq!(decide_verdict(&e).1[4], SignalResult::Fail, "growing fails");
    }

    /// Every combination of the five signals' pass/fail: UPGRADE_NEEDED is
    /// exactly "cpu fails, or backlog fails on a busy host".
    #[test]
    fn test_decide_verdict_every_pass_fail_combination() {
        for mask in 0u32..32 {
            for busy in [10.0, 95.0] {
                let mut e = evidence_pass();
                e.worst_host_busy_pct = Some(busy);
                if mask & 1 != 0 {
                    e.worst_step_p99[0] = Some(4_096);
                }
                if mask & 2 != 0 {
                    e.minutes_over_1ms = 1;
                }
                if mask & 4 != 0 {
                    e.worst_reader_wait = Some(5_000_000);
                }
                if mask & 8 != 0 {
                    e.disk_hot_minutes = 50;
                }
                if mask & 16 != 0 {
                    e.backlog_at_close = (Some(9_000), Some(2));
                }
                let (v, r) = decide_verdict(&e);
                for (bit, got) in r.iter().enumerate() {
                    let want = if mask & (1 << bit) != 0 {
                        SignalResult::Fail
                    } else {
                        SignalResult::Pass
                    };
                    assert_eq!(*got, want, "mask {mask:05b} bit {bit}");
                }
                let upgrade = mask & 4 != 0 || (mask & 16 != 0 && busy >= HOST_BUSY_UPGRADE_PCT);
                let want = if upgrade {
                    Verdict::UpgradeNeeded
                } else {
                    Verdict::NotNeeded
                };
                assert_eq!(v, want, "mask {mask:05b} busy {busy}");
            }
        }
    }

    /// Missing data is UNKNOWN, never green.
    #[test]
    fn test_decide_verdict_missing_data_is_unknown() {
        assert_eq!(
            decide_verdict(&DayEvidence::default()).0,
            Verdict::Unknown,
            "empty day"
        );
        let mut e = evidence_pass();
        e.session_minutes = MIN_SESSION_MINUTES - 1;
        assert_eq!(decide_verdict(&e).0, Verdict::Unknown, "partial day");
        e.session_minutes = MIN_SESSION_MINUTES;
        assert_eq!(decide_verdict(&e).0, Verdict::NotNeeded, "boundary");
        let mut e = evidence_pass();
        e.disk_minutes = 0;
        let (v, r) = decide_verdict(&e);
        assert_eq!(
            (v, r[3]),
            (Verdict::Unknown, SignalResult::Unknown),
            "no disk device"
        );
        let mut e = evidence_pass();
        e.worst_step_p99[2] = None;
        assert_eq!(decide_verdict(&e).1[0], SignalResult::Unknown);
        let mut e = evidence_pass();
        e.worst_step_p99[3] = None;
        assert_eq!(
            decide_verdict(&e).1[1],
            SignalResult::Unknown,
            "no receipt_to_fold"
        );
        let mut e = evidence_pass();
        e.worst_drain_wait = None;
        assert_eq!(decide_verdict(&e).1[2], SignalResult::Unknown);
        let mut e = evidence_pass();
        e.backlog_at_close = (None, Some(0));
        assert_eq!(
            decide_verdict(&e),
            (
                Verdict::Unknown,
                [
                    SignalResult::Pass,
                    SignalResult::Pass,
                    SignalResult::Pass,
                    SignalResult::Pass,
                    SignalResult::Unknown
                ]
            )
        );
        // Even a measured CPU failure is UNKNOWN when other data is missing:
        // the verdict never claims more than it saw.
        e.worst_reader_wait = Some(9_000_000);
        assert_eq!(decide_verdict(&e).0, Verdict::Unknown);
    }

    #[test]
    fn test_day_verdict_full_good_day_is_not_needed_at_1540() {
        let mut v = DayVerdict::new();
        let row = run_day(&mut v, 0, good).expect("verdict at 15:40");
        assert_eq!(row.verdict, Verdict::NotNeeded);
        assert_eq!(row.session_minutes, 375);
        assert_eq!(row.day_ist_nanos, at(0, 0, 0));
        assert_eq!(row.tick_step.2, Some(1_024));
        assert_eq!(row.backlog, (SignalResult::Pass, Some(3), Some(0)));
        // A later minute the same day writes nothing more.
        let (l, h) = good(at(0, 15, 41));
        assert!(v.observe(&l, &h).is_none());
    }

    #[test]
    fn test_day_verdict_cpu_starved_day_is_upgrade_needed() {
        let mut v = DayVerdict::new();
        let row = run_day(&mut v, 0, |m| {
            let (l, mut h) = good(m);
            if m == at(0, 11, 0) {
                h.drain_wait = Some((9_000_000, 3_000_000));
            }
            (l, h)
        })
        .expect("verdict");
        assert_eq!(row.verdict, Verdict::UpgradeNeeded);
        assert_eq!(
            row.cpu_wait,
            (SignalResult::Fail, Some(10_000), Some(3_000_000))
        );
    }

    #[test]
    fn test_day_verdict_counts_slow_minutes_and_disk_hot_minutes() {
        let mut v = DayVerdict::new();
        let row = run_day(&mut v, 0, |m| {
            let (mut l, mut h) = good(m);
            if m < at(0, 9, 22) {
                l[Stage::ReceiptToFold as usize].max_ns = 3_000_000;
                h.disk_write_bytes_per_s = Some(DISK_HOT_BYTES_PER_S);
            }
            (l, h)
        })
        .expect("verdict");
        assert_eq!(row.ticks_over_1ms, (SignalResult::Fail, 7));
        assert_eq!(row.disk, (SignalResult::Fail, 7));
        assert_eq!(row.verdict, Verdict::NotNeeded);
        assert!(verdict_reasons(&row).contains("more cores would not fix it"));
    }

    /// A NULL p99 with samples (above the top bound) is the worst, never
    /// skipped.
    #[test]
    fn test_day_verdict_overflowed_p99_counts_as_worst() {
        let mut v = DayVerdict::new();
        let row = run_day(&mut v, 0, |m| {
            let (mut l, h) = good(m);
            if m == at(0, 10, 0) {
                l[Stage::RingDwell as usize].p99_ns = None;
            }
            (l, h)
        })
        .expect("verdict");
        assert_eq!(
            row.tick_step,
            (SignalResult::Fail, Some(Stage::RingDwell), Some(u64::MAX))
        );
    }

    /// Minutes outside 09:15–15:30 never count, and a process that saw no
    /// session minute writes no verdict (a restart after the close cannot
    /// overwrite the day's row).
    #[test]
    fn test_day_verdict_no_session_minutes_writes_nothing() {
        let mut v = DayVerdict::new();
        for m in [
            at(0, 8, 40),
            at(0, 9, 14),
            at(0, 15, 30),
            at(0, 15, 45),
            at(0, 16, 20),
        ] {
            let (l, h) = good(m);
            assert!(v.observe(&l, &h).is_none());
        }
        assert_eq!(v.evidence().session_minutes, 0);
    }

    /// A partial day (boot at 13:00) is UNKNOWN, with the coverage in the
    /// reasons.
    #[test]
    fn test_day_verdict_partial_day_is_unknown() {
        let mut v = DayVerdict::new();
        let mut out = None;
        let mut m = at(0, 13, 0);
        while m <= at(0, 15, 40) {
            let (l, h) = good(m);
            out = v.observe(&l, &h).or(out);
            m += 60 * NS;
        }
        let row = out.expect("verdict");
        assert_eq!(row.verdict, Verdict::Unknown);
        assert_eq!(row.session_minutes, 150);
        assert!(verdict_reasons(&row).contains("only 150 of 375"));
    }

    /// Day rollover before 15:40 (process up past midnight): the previous
    /// day's verdict is written at the first minute of the next day, then
    /// the next day accumulates from zero.
    #[test]
    fn test_day_verdict_rollover_writes_the_previous_day() {
        let mut v = DayVerdict::new();
        let mut m = at(0, 9, 15);
        while m < at(0, 15, 30) {
            let (l, h) = good(m);
            assert!(v.observe(&l, &h).is_none());
            m += 60 * NS;
        }
        let (l, h) = good(at(1, 0, 1));
        let row = v.observe(&l, &h).expect("day 0 written at rollover");
        assert_eq!(row.day_ist_nanos, at(0, 0, 0));
        assert_eq!(row.session_minutes, 375);
        assert_eq!(row.backlog.0, SignalResult::Unknown, "no 15:40 reading");
        assert_eq!(row.verdict, Verdict::Unknown);
        assert_eq!(v.evidence().session_minutes, 0, "day 1 starts empty");
        // Day 1 then runs normally.
        let row = run_day(&mut v, 1, good).expect("day 1");
        assert_eq!(row.day_ist_nanos, at(1, 0, 0));
        assert_eq!(row.verdict, Verdict::NotNeeded);
    }

    /// Clock skew: a minute from an earlier day (clock stepped back across
    /// midnight) is skipped and counted, never reopening a written day.
    #[test]
    fn test_day_verdict_clock_back_across_midnight_is_skipped() {
        let mut v = DayVerdict::new();
        let _ = run_day(&mut v, 1, good).expect("day 1");
        let (l, h) = good(at(0, 15, 50));
        assert!(v.observe(&l, &h).is_none());
        assert_eq!(v.skipped_backwards(), 1);
        let (l, h) = good(at(1, 15, 55));
        assert!(v.observe(&l, &h).is_none(), "day 1 already written");
    }

    #[test]
    fn test_is_session_minute_bounds() {
        assert!(!is_session_minute(at(0, 9, 14)));
        assert!(is_session_minute(at(0, 9, 15)));
        assert!(is_session_minute(at(0, 15, 29)));
        assert!(!is_session_minute(at(0, 15, 30)));
        assert_eq!(minute_floor_ist_nanos(at(0, 9, 15) + 59 * NS), at(0, 9, 15));
    }

    #[test]
    fn test_parse_proc_self_stat_cpu_jiffies_handles_spaces_in_the_name() {
        let stat = "1234 (tv worker (x)) S 1 2 3 4 5 6 7 8 9 10 250 75 0 0 20 0";
        assert_eq!(parse_proc_self_stat_cpu_jiffies(stat), Some(325));
        assert_eq!(parse_proc_self_stat_cpu_jiffies("1234 (x) S 1 2"), None);
        assert_eq!(parse_proc_self_stat_cpu_jiffies("no paren"), None);
        assert_eq!(parse_proc_self_stat_cpu_jiffies(""), None);
    }

    #[test]
    fn test_parse_proc_stat_cpu_totals_busy_excludes_idle_and_iowait() {
        let stat = "cpu  100 0 50 800 40 5 5 10 0 0\ncpu0 1 1 1 1 1 1 1 1 0 0\n";
        assert_eq!(parse_proc_stat_cpu_totals(stat), Some((1_010, 170, 10)));
        assert_eq!(parse_proc_stat_cpu_totals("cpu  1 2 3"), None, "too short");
        assert_eq!(parse_proc_stat_cpu_totals("cpu0 1 2 3 4 5 6 7 8"), None);
        assert_eq!(parse_proc_stat_cpu_totals(""), None);
    }

    #[test]
    fn test_parse_diskstats_for_finds_the_device_and_misses_cleanly() {
        let ds = "   7       0 loop0 1 0 2 0 0 0 0 0 0 0 0\n 259       1 nvme0n1p1 100 5 2000 30 400 10 8000 1200 0 900 1230\n";
        assert_eq!(
            parse_diskstats_for(ds, 259, 1),
            Some(DiskReading {
                sectors_read: 2_000,
                writes: 400,
                sectors_written: 8_000,
                write_ms: 1_200
            })
        );
        assert_eq!(parse_diskstats_for(ds, 259, 9), None, "missing device");
        assert_eq!(
            parse_diskstats_for("259 1 nvme 1 2", 259, 1),
            None,
            "short line"
        );
        assert_eq!(parse_diskstats_for("", 8, 0), None);
    }

    #[test]
    fn test_dev_major_minor_glibc_encoding() {
        // makedev(259, 1) and makedev(8, 17), and a large minor.
        assert_eq!(dev_major_minor((259 << 8) | 1), (259, 1));
        assert_eq!(dev_major_minor((8 << 8) | 17), (8, 17));
        let dev = (0x12u64 << 8) | 0x34 | (0x5u64 << 20);
        assert_eq!(dev_major_minor(dev), (0x12, 0x34 | (0x5 << 8)));
    }

    #[test]
    fn test_device_of_a_missing_path_is_none() {
        assert_eq!(device_of("/definitely/not/here/x"), None);
    }

    #[test]
    fn test_prometheus_counter_sum_adds_every_series() {
        let text = "# TYPE tv_ticks_lost_total counter\n\
                    tv_ticks_lost_total{source=\"a\"} 3\n\
                    tv_ticks_lost_total{source=\"b\"} 4\n\
                    tv_ticks_lost_total_other 99\n\
                    tv_ticks_lost 7\n";
        assert_eq!(prometheus_counter_sum(text, "tv_ticks_lost_total"), Some(7));
        assert_eq!(
            prometheus_counter_sum("tv_x 1\n", "tv_ticks_lost_total"),
            None
        );
        assert_eq!(
            prometheus_counter_sum("tv_ticks_lost_total 2\n", "tv_ticks_lost_total"),
            Some(2)
        );
        assert_eq!(
            prometheus_counter_sum(
                "tv_ticks_lost_total{a=\"b c\"} NaN\n",
                "tv_ticks_lost_total"
            ),
            None,
            "not a finite value"
        );
    }

    #[test]
    fn test_note_ring_refused_raises_main_feed_ring_refused_total_and_publish_ticks_lost_total_reads_back()
     {
        publish_ticks_lost_total(42);
        assert_eq!(ticks_lost_total(), Some(42));
        publish_ticks_lost_total(u64::MAX);
        assert_eq!(
            ticks_lost_total(),
            Some(u64::MAX - 1),
            "never the unknown sentinel"
        );
        let before = main_feed_ring_refused_total();
        note_ring_refused(true);
        assert!(main_feed_ring_refused_total() > before);
        let after_main = main_feed_ring_refused_total();
        note_ring_refused(false);
        assert!(
            main_feed_ring_refused_total() >= after_main,
            "depth adds nothing"
        );
    }

    fn counters(at_s: i64, jiffies: u64, host: (u64, u64, u64), disk: DiskReading) -> HostCounters {
        HostCounters {
            at_ist_nanos: at_s * NS,
            process_jiffies: Some(jiffies),
            host: Some(host),
            disk: Some(disk),
            ticks_accepted: (7, 1_000),
            not_landed: 10,
            ticks_lost: Some(2),
            ring_refused: 5,
        }
    }

    #[test]
    fn test_build_host_row_rates_from_two_readings() {
        let a = counters(
            0,
            1_000,
            (10_000, 4_000, 100),
            DiskReading {
                sectors_read: 0,
                writes: 100,
                sectors_written: 0,
                write_ms: 1_000,
            },
        );
        let mut b = counters(
            60,
            1_000 + 6_000,
            (22_000, 10_000, 220),
            DiskReading {
                sectors_read: 120 * 1_024,
                writes: 700,
                sectors_written: 60 * 2_048 * 1_000,
                write_ms: 1_600,
            },
        );
        b.ticks_accepted = (7, 4_000);
        b.not_landed = 13;
        b.ticks_lost = Some(5);
        b.ring_refused = 9;
        let row = build_host_row(
            60 * NS,
            Some(&a),
            &b,
            2,
            (Some((5, 3)), None, Some((1, 1))),
            60,
            (Some(40), Some(1)),
        );
        // 6,000 jiffies = 60 s of CPU over 60 s on 2 cores = 50%.
        assert_eq!(row.process_cpu_pct, Some(50.0));
        assert_eq!(row.host_cpu_busy_pct, Some(50.0));
        assert_eq!(row.host_cpu_steal_pct, Some(1.0));
        // 122,880 sectors × 512 B / 60 s = 1 MiB/s; writes 1,000× that.
        assert_eq!(row.disk_read_bytes_per_s, Some(1_048_576));
        assert_eq!(row.disk_write_bytes_per_s, Some(1_048_576_000));
        assert_eq!(row.disk_write_await_us, Some(1_000));
        assert_eq!(row.ticks_accepted, Some(3_000));
        assert_eq!(row.ticks_not_landed, Some(3));
        assert_eq!(row.ticks_lost, Some(3));
        assert_eq!(row.ring_refused_frames, 4);
        assert_eq!(row.reader_wait, Some((5, 3)));
        assert_eq!(row.drain_wait, None, "not measured stays NULL");
        assert_eq!(row.questdb_backlog_txns, Some(40));
        assert_eq!(row.host_cores, 2);
    }

    /// First minute of a process, a clock step back, a day change, counters
    /// that went backwards and a missing device: never a made-up rate.
    #[test]
    fn test_build_host_row_degenerate_inputs() {
        let a = counters(60, 1_000, (10_000, 4_000, 100), DiskReading::default());
        let first = build_host_row(0, None, &a, 2, (None, None, None), 3, (None, None));
        assert_eq!(first.process_cpu_pct, None);
        assert_eq!(first.ticks_accepted, None);
        assert_eq!(first.sched_seconds, 3);

        let back = counters(0, 9_000, (20_000, 9_000, 100), DiskReading::default());
        let r = build_host_row(0, Some(&a), &back, 2, (None, None, None), 60, (None, None));
        assert_eq!(
            (r.process_cpu_pct, r.disk_write_bytes_per_s),
            (None, None),
            "clock back"
        );

        let mut later = counters(120, 500, (9_000, 1, 1), DiskReading::default());
        later.ticks_accepted = (8, 250);
        later.disk = None;
        later.ticks_lost = None;
        let r = build_host_row(0, Some(&a), &later, 2, (None, None, None), 60, (None, None));
        assert_eq!(r.process_cpu_pct, None, "jiffies went backwards");
        assert_eq!(r.host_cpu_busy_pct, None, "host total went backwards");
        assert_eq!(r.disk_write_bytes_per_s, None, "no device");
        assert_eq!(r.ticks_accepted, Some(250), "new IST day counts from zero");
        assert_eq!(r.ticks_lost, None, "tally not read");
        assert_eq!(r.disk_total_bytes_per_s(), None);
    }

    #[test]
    fn test_close_minute_and_read_counters_on_this_host() {
        let mut s = HostSampler::new();
        assert!(s.cores >= 1);
        s.absorb_second_values(Some(10), None, Some(1));
        s.absorb_second_values(Some(30), None, None);
        assert_eq!(s.seconds, 2);
        assert_eq!(s.reader.take(), Some((40, 30)));
        assert_eq!(s.drain.take(), None);
        assert_eq!(s.wal.take(), Some((1, 1)));
        assert_eq!(s.reader.take(), None, "take resets");
        let c = s.read_counters(5 * NS);
        assert_eq!(c.at_ist_nanos, 5 * NS);
        // A real close on this host never panics, whatever /proc holds.
        let r1 = s.close_minute(0, 60 * NS);
        let r2 = s.close_minute(60 * NS, 120 * NS);
        assert_eq!(r1.sched_seconds, 2);
        assert_eq!(r2.sched_seconds, 0);
    }

    #[test]
    fn test_verdict_reasons_names_every_signal() {
        let mut v = DayVerdict::new();
        let row = run_day(&mut v, 0, good).expect("verdict");
        let text = verdict_reasons(&row);
        for needle in [
            "tick-step p99 pass",
            "ticks over 1 ms pass",
            "cpu wait pass",
            "disk pass",
            "questdb backlog pass",
        ] {
            assert!(text.contains(needle), "{needle} in {text}");
        }
        assert_eq!(Verdict::UpgradeNeeded.as_str(), "UPGRADE_NEEDED");
        assert_eq!(Verdict::NotNeeded.as_str(), "NOT_NEEDED");
        assert_eq!(Verdict::Unknown.as_str(), "UNKNOWN");
        assert_eq!(SignalResult::Unknown.as_str(), "unknown");
    }

    #[test]
    fn test_disk_total_bytes_per_s_needs_both_directions() {
        let mut r = HostMinuteRow {
            disk_read_bytes_per_s: Some(3),
            disk_write_bytes_per_s: Some(4),
            ..HostMinuteRow::default()
        };
        assert_eq!(r.disk_total_bytes_per_s(), Some(7));
        r.disk_read_bytes_per_s = None;
        assert_eq!(r.disk_total_bytes_per_s(), None);
        r.disk_read_bytes_per_s = Some(u64::MAX);
        assert_eq!(r.disk_total_bytes_per_s(), Some(u64::MAX), "saturates");
    }

    #[test]
    fn test_host_capacity_minute_ddl_statements_and_capacity_verdict_daily_ddl_statements() {
        for (stmts, table, cols, key) in [
            (
                host_capacity_minute_ddl_statements(),
                HOST_CAPACITY_MINUTE_TABLE,
                HOST_CAPACITY_MINUTE_COLUMNS,
                DEDUP_KEY_HOST_CAPACITY_MINUTE,
            ),
            (
                capacity_verdict_daily_ddl_statements(),
                CAPACITY_VERDICT_DAILY_TABLE,
                CAPACITY_VERDICT_DAILY_COLUMNS,
                DEDUP_KEY_CAPACITY_VERDICT_DAILY,
            ),
        ] {
            assert_eq!(stmts.len(), cols.len() + 1, "{table}");
            assert!(stmts[0].starts_with(&format!("CREATE TABLE IF NOT EXISTS {table} (")));
            assert!(stmts[0].contains("timestamp(ts) PARTITION BY DAY"));
            assert!(stmts[0].contains(&format!("DEDUP UPSERT KEYS({key})")));
            for (col, ty) in cols {
                assert!(stmts[0].contains(&format!("{col} {ty}")), "{table}.{col}");
            }
            assert!(
                stmts
                    .iter()
                    .all(|s| !s.to_ascii_uppercase().contains("DROP"))
            );
            assert!(
                stmts
                    .last()
                    .is_some_and(|s| s.contains("DEDUP ENABLE UPSERT KEYS"))
            );
            assert!(key.contains("feed") && key.starts_with("ts"));
        }
    }

    #[test]
    fn test_append_host_row_writes_known_columns_and_omits_nulls() {
        let mut buf = Buffer::new(questdb::ingress::ProtocolVersion::V1);
        let row = host(at(0, 10, 0), 5_000, 2_000, 40.0);
        append_host_row(&mut buf, &row).expect("append");
        let text = String::from_utf8_lossy(buf.as_bytes()).into_owned();
        assert!(
            text.starts_with("host_capacity_minute,feed=dhan "),
            "{text}"
        );
        for col in [
            "reader_cpu_wait_max_ns_per_s=5000i",
            "drain_cpu_wait_ns=5000i",
            "host_cpu_busy_pct=40",
            "disk_write_bytes_per_s=2000i",
            "questdb_backlog_txns=3i",
            "ring_refused_frames=0i",
        ] {
            assert!(text.contains(col), "{col} in {text}");
        }
        assert!(!text.contains("process_cpu_pct"), "NULL is omitted");
        assert!(!text.contains("ticks_lost"), "NULL is omitted");
        let mut bad = row;
        bad.minute_ist_nanos = -1;
        let mut buf = Buffer::new(questdb::ingress::ProtocolVersion::V1);
        assert!(
            append_host_row(&mut buf, &bad).is_err(),
            "negative ts refused"
        );
    }

    #[test]
    fn test_append_verdict_row_writes_results_and_reasons() {
        let mut v = DayVerdict::new();
        let row = run_day(&mut v, 0, good).expect("verdict");
        let mut buf = Buffer::new(questdb::ingress::ProtocolVersion::V1);
        append_verdict_row(&mut buf, &row).expect("append");
        let text = String::from_utf8_lossy(buf.as_bytes()).into_owned();
        assert!(
            text.starts_with("capacity_verdict_daily,feed=dhan,verdict=NOT_NEEDED,"),
            "{text}"
        );
        for col in [
            "cpu_wait_result=pass",
            "worst_tick_step=",
            "reasons=\"",
            "session_minutes=375i",
            "worst_tick_step_p99_ns=1024i",
            "backlog_txns_at_close=3i",
        ] {
            assert!(text.contains(col), "{col} in {text}");
        }
    }

    #[test]
    fn test_constants_are_the_documented_values() {
        assert_eq!(
            SESSION_END_SECS_OF_DAY_IST - SESSION_START_SECS_OF_DAY_IST,
            375 * 60
        );
        assert_eq!(VERDICT_AT_SECS_OF_DAY_IST, 56_400);
        assert_eq!(DISK_THROUGHPUT_CAP_BYTES_PER_S, 131_072_000);
        assert_eq!(DISK_HOT_BYTES_PER_S, 117_964_800);
        assert_eq!(DEDUP_KEY_HOST_CAPACITY_MINUTE, "ts, feed");
        assert_eq!(
            DEDUP_KEY_CAPACITY_VERDICT_DAILY,
            "ts, trading_date_ist, feed"
        );
        assert_eq!(MAX_WS_READER_SLOTS, 4);
    }
}
