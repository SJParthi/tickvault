//! "Are we a slow consumer?" — the kernel receive-queue early warning
//! (2026-10-02).
//!
//! # Why this exists
//!
//! Dhan's feed platform isolates a slow consumer and skips it forward to the
//! latest state. The feed carries no sequence number, so if our socket reader
//! ever falls behind, the ticks Dhan skips are lost AT DHAN'S SIDE and nothing
//! in this process can see them: our own counters only ever see the frames
//! that arrived.
//!
//! The one place the backlog IS visible before that happens is the kernel.
//! Bytes Dhan has sent and our reader has not yet read sit in the socket's
//! receive queue (`rx_queue` in `/proc/net/tcp`). A queue that stays large is
//! the measurable precursor of being skipped. This module samples it.
//!
//! # Shape
//!
//! * Its OWN tokio task, every [`KERNEL_RX_SAMPLE_INTERVAL_SECS`] during the
//!   trading session. Never on the frame path, never touches the ring or the
//!   drain.
//! * Only sockets owned by THIS process count: their inodes are read from
//!   `/proc/self/fd` (`socket:[N]` links) and matched against the inode column
//!   of `/proc/net/tcp` and `/proc/net/tcp6`.
//! * Only ESTABLISHED sockets to remote port 443 count — every Dhan feed
//!   endpoint is `wss://…:443`.
//! * Publishes two Prometheus gauges, [`KERNEL_RX_QUEUE_MAX_GAUGE`] and
//!   [`KERNEL_RX_QUEUE_SUM_GAUGE`]. No CloudWatch selector, alarm, terraform
//!   or Telegram notification is attached (deliberately — see the
//!   noise lock).
//! * One edge-triggered coded `error!` when the MAX stays above
//!   [`KERNEL_RX_BACKLOG_THRESHOLD_BYTES`] for
//!   [`KERNEL_RX_BACKLOG_CONSECUTIVE_SAMPLES`] consecutive samples, re-armed
//!   only once it drops below half the threshold.
//!
//! * Also reads the kernel's own TCP drop counters (2026-10-30, plan item
//!   52j phase A). Each sample reads `/proc/net/netstat` and finds nine
//!   `TcpExt` counters by NAME, never by column number (the column order
//!   changes between kernels): [`TCP_EXT_COUNTERS`]. Each one's growth since
//!   the last sample is added to the counter [`KERNEL_TCP_EXT_COUNTER`] with
//!   the label `counter=<kernel name>`. A counter this kernel does not have
//!   is never published (absent, never a fake zero). The first sample of a
//!   session only seeds; a value that went DOWN (a reset) adds nothing and
//!   seeds again.
//! * Once, when the task starts, reads the receive-buffer ceilings
//!   (`/proc/sys/net/core/rmem_max` and the third field of
//!   `/proc/sys/net/ipv4/tcp_rmem`) into two gauges,
//!   [`KERNEL_RMEM_MAX_GAUGE`] and [`KERNEL_TCP_RMEM_MAX_GAUGE`]. An
//!   unreadable file leaves its gauge absent.
//! * None of these new names is shipped to CloudWatch: local `/metrics` only
//!   (pinned by `crates/app/tests/kernel_tcp_ext_local_only_guard.rs`).
//!   There is no config switch: it is a pure `/proc` read inside an existing
//!   cold 1 Hz sampler, adding no work on the frame path.
//!
//! # Honest limits
//!
//! * The `TcpExt` counters cover the whole network namespace, not only our
//!   sockets. In the app's container that is our own traffic plus anything
//!   else in the same namespace. They say THAT the kernel dropped or pruned
//!   receive data, not on which socket. Per-socket detail needs a
//!   `getsockopt` probe, which is not built (it needs `unsafe`).
//! * `tcp_rmem` is per network namespace; `rmem_max` is host-wide. The
//!   gauges show what the app's namespace reads at task start; a later
//!   `sysctl` change is not seen until the next start.
//! * Port 443 is not unique to Dhan: an AWS SDK or SNS call in flight from
//!   this process also matches. Those are short-lived request/response
//!   sockets whose receive queue is drained by the HTTP client as it reads, so
//!   they add noise to the SUM, essentially never to a sustained MAX.
//! * Linux only. Elsewhere `/proc` does not exist, the reads fail, nothing is
//!   published (absent, never a fake zero), and one `info!` says so. No `cfg`.
//! * A 1 s sample can miss a sub-second spike. The alert needs FIVE
//!   consecutive high samples on purpose: a transient burst the reader then
//!   clears is not a slow consumer.
//!
//! # Complexity
//!
//! Per sample: O(fds) to list our sockets plus O(lines) over the two tables,
//! each line one hash probe. Cold, 1 Hz, off the frame path, and the reads run
//! on the blocking pool (`spawn_blocking`), never on a runtime worker. The inode set and
//! the read buffers are reused across samples. The `/proc/net/netstat` read
//! reuses the same buffer and adds O(fields × 9) name compares (about 120
//! fields today), no allocation. The counter handles are resolved once per
//! name, the first time that name is seen.

use std::collections::HashSet;
use std::io::Read;
use std::sync::Arc;

use tickvault_common::error_code::ErrorCode;
use tracing::{error, info};

/// Sampling cadence, in seconds.
pub const KERNEL_RX_SAMPLE_INTERVAL_SECS: u64 = 1;

/// A receive queue above this many bytes is a backlog (4 MiB).
pub const KERNEL_RX_BACKLOG_THRESHOLD_BYTES: u64 = 4 * 1024 * 1024;

/// How many consecutive samples above the threshold before the alert fires.
pub const KERNEL_RX_BACKLOG_CONSECUTIVE_SAMPLES: u32 = 5;

/// Remote port of every Dhan feed endpoint (`wss://…`).
pub const DHAN_FEED_REMOTE_PORT: u16 = 443;

/// Largest receive queue among this process's ESTABLISHED port-443 sockets.
pub const KERNEL_RX_QUEUE_MAX_GAUGE: &str = "tv_dhan_ws_kernel_rx_queue_bytes_max";

/// Sum of the receive queues of this process's ESTABLISHED port-443 sockets.
pub const KERNEL_RX_QUEUE_SUM_GAUGE: &str = "tv_dhan_ws_kernel_rx_queue_bytes_sum";

/// The `source` field on the coded log line, so a reader can tell this
/// WS-GAP-03 emit from the code's other sites.
pub const KERNEL_RX_BACKLOG_SOURCE: &str = "kernel_rx_queue_backlog";

/// Counter of the kernel's `TcpExt` receive-side drop and prune events, one
/// series per kernel counter name (label `counter`). Local `/metrics` only.
pub const KERNEL_TCP_EXT_COUNTER: &str = "tv_kernel_tcp_ext_total";

/// Label key on [`KERNEL_TCP_EXT_COUNTER`].
pub const KERNEL_TCP_EXT_LABEL: &str = "counter";

/// `net.core.rmem_max`: the largest receive buffer a socket may ask for.
pub const KERNEL_RMEM_MAX_GAUGE: &str = "tv_kernel_rmem_max_bytes";

/// Third field of `net.ipv4.tcp_rmem`: the largest receive buffer TCP
/// auto-tuning may grow to.
pub const KERNEL_TCP_RMEM_MAX_GAUGE: &str = "tv_kernel_tcp_rmem_max_bytes";

/// The `TcpExt` counters read from `/proc/net/netstat`, by kernel name.
///
/// * `PruneCalled`, `RcvPruned`, `OfoPruned`: the receive queue ran out of
///   buffer memory and the kernel pruned (dropped) queued data.
/// * `TCPRcvCollapsed`: queued data was compacted to save memory.
/// * `TCPRcvQDrop`: a packet was dropped because the receive buffer was full.
/// * `TCPBacklogDrop`: a packet was dropped from the socket backlog.
/// * `TCPZeroWindowDrop`: a packet was dropped against a zero window.
/// * `TCPToZeroWindowAdv`, `TCPWantZeroWindowAdv`: we told (or wanted to
///   tell) the sender to stop because our buffer was full.
pub const TCP_EXT_COUNTERS: [&str; TCP_EXT_COUNTER_COUNT] = [
    "PruneCalled",
    "RcvPruned",
    "OfoPruned",
    "TCPRcvCollapsed",
    "TCPRcvQDrop",
    "TCPBacklogDrop",
    "TCPZeroWindowDrop",
    "TCPToZeroWindowAdv",
    "TCPWantZeroWindowAdv",
];

/// How many `TcpExt` counters are read.
pub const TCP_EXT_COUNTER_COUNT: usize = 9;

/// One reading of the [`TCP_EXT_COUNTERS`], in the same order. `None` is a
/// counter this kernel does not report (absent, never zero).
pub type TcpExtReading = [Option<u64>; TCP_EXT_COUNTER_COUNT];

/// Prefix of the two `TcpExt` lines in `/proc/net/netstat`.
const TCP_EXT_LINE_PREFIX: &str = "TcpExt:";

/// `net.core.rmem_max`, host-wide.
const PROC_RMEM_MAX_PATH: &str = "/proc/sys/net/core/rmem_max";

/// `net.ipv4.tcp_rmem` (`min default max`), per network namespace.
const PROC_TCP_RMEM_PATH: &str = "/proc/sys/net/ipv4/tcp_rmem";

/// The kernel's per-namespace protocol counters.
const PROC_NET_NETSTAT_PATH: &str = "/proc/net/netstat";

/// `st` column value of an ESTABLISHED TCP socket (`TCP_ESTABLISHED` = 1).
const TCP_STATE_ESTABLISHED: u8 = 0x01;

/// The fields of one `/proc/net/tcp{,6}` row this sampler reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProcTcpRow {
    /// Remote port (host order).
    pub remote_port: u16,
    /// Connection state (`st`, hex in the file).
    pub state: u8,
    /// Bytes in the receive queue not yet read by the owner.
    pub rx_queue: u64,
    /// Socket inode — matches `socket:[inode]` in `/proc/<pid>/fd`.
    pub inode: u64,
}

/// Parses one data row of `/proc/net/tcp` or `/proc/net/tcp6`.
///
/// Returns `None` for the header row and for any row that does not parse —
/// never a guessed value. Pure, allocation-free.
///
/// Row shape (whitespace separated): `sl local rem st tx:rx tr:when retrnsmt
/// uid timeout inode …`; `rem` is `HEXADDR:HEXPORT` (32 hex digits for IPv6).
#[must_use]
pub fn parse_proc_net_tcp_line(line: &str) -> Option<ProcTcpRow> {
    let mut fields = line.split_whitespace();
    let slot = fields.next()?;
    // The header row's first field is "sl"; data rows are "N:".
    if !slot.ends_with(':') {
        return None;
    }
    let _local = fields.next()?;
    let remote = fields.next()?;
    let state = fields.next()?;
    let queues = fields.next()?;
    let _timer = fields.next()?;
    let _retransmits = fields.next()?;
    let _uid = fields.next()?;
    let _timeout = fields.next()?;
    let inode = fields.next()?;

    let (_, remote_port_hex) = remote.rsplit_once(':')?;
    let remote_port = u16::from_str_radix(remote_port_hex, 16).ok()?;
    let state = u8::from_str_radix(state, 16).ok()?;
    let (_, rx_hex) = queues.split_once(':')?;
    let rx_queue = u64::from_str_radix(rx_hex, 16).ok()?;
    let inode = inode.parse::<u64>().ok()?;
    Some(ProcTcpRow {
        remote_port,
        state,
        rx_queue,
        inode,
    })
}

/// Parses a `/proc/<pid>/fd` link target of the form `socket:[12345]`.
#[must_use]
pub fn parse_socket_inode(link_target: &str) -> Option<u64> {
    link_target
        .strip_prefix("socket:[")?
        .strip_suffix(']')?
        .parse::<u64>()
        .ok()
}

/// One sample's aggregate over the matching sockets.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RxQueueSample {
    /// Matching sockets seen.
    pub sockets: u32,
    /// Largest receive queue, bytes.
    pub max_bytes: u64,
    /// Sum of receive queues, bytes.
    pub sum_bytes: u64,
}

/// Folds one `/proc/net/tcp{,6}` table into `acc`: ESTABLISHED rows to
/// `remote_port` whose inode `is_ours` accepts. Pure: the inode test is the
/// caller's, so this is testable without `/proc`.
pub fn fold_rx_queue_table(
    table: &str,
    remote_port: u16,
    is_ours: impl Fn(u64) -> bool,
    acc: &mut RxQueueSample,
) {
    // O(1) EXEMPT: begin — cold 1 Hz sampler, one pass over a /proc table, never on the frame path.
    for line in table.lines() {
        let Some(row) = parse_proc_net_tcp_line(line) else {
            continue;
        };
        if row.state != TCP_STATE_ESTABLISHED
            || row.remote_port != remote_port
            || !is_ours(row.inode)
        {
            continue;
        }
        acc.sockets = acc.sockets.saturating_add(1);
        acc.max_bytes = acc.max_bytes.max(row.rx_queue);
        acc.sum_bytes = acc.sum_bytes.saturating_add(row.rx_queue);
    }
    // O(1) EXEMPT: end
}

/// Reads the [`TCP_EXT_COUNTERS`] out of a `/proc/net/netstat` text into
/// `out`, matching each value to its name by position on the header line.
///
/// The file holds pairs of lines: a header line of names and a value line,
/// both starting `TcpExt:`. Every slot of `out` is first set to `None`; a
/// name not in the header, or a value that does not parse, stays `None`
/// (absent, never zero). Returns `false` when there is no `TcpExt` pair at
/// all. Pure, allocation-free.
pub fn parse_tcp_ext_counters(netstat: &str, out: &mut TcpExtReading) -> bool {
    *out = [None; TCP_EXT_COUNTER_COUNT];
    let mut tcp_ext_lines = netstat
        .lines()
        .filter_map(|line| line.strip_prefix(TCP_EXT_LINE_PREFIX));
    let (Some(names), Some(values)) = (tcp_ext_lines.next(), tcp_ext_lines.next()) else {
        return false;
    };
    // O(1) EXEMPT: begin — cold 1 Hz sampler, one pass over ~120 fields with 9 name compares each.
    for (name, value) in names.split_whitespace().zip(values.split_whitespace()) {
        if let Some(index) = TCP_EXT_COUNTERS.iter().position(|wanted| *wanted == name) {
            out[index] = value.parse::<u64>().ok();
        }
    }
    // O(1) EXEMPT: end
    true
}

/// Parses `/proc/sys/net/core/rmem_max`: one decimal number.
#[must_use]
pub fn parse_rmem_max(text: &str) -> Option<u64> {
    text.trim().parse::<u64>().ok()
}

/// Parses `/proc/sys/net/ipv4/tcp_rmem` (`min default max`) and returns the
/// max. `None` unless there are exactly three numbers.
#[must_use]
pub fn parse_tcp_rmem_max(text: &str) -> Option<u64> {
    let mut fields = text.split_whitespace();
    let _min = fields.next()?.parse::<u64>().ok()?;
    let _default = fields.next()?.parse::<u64>().ok()?;
    let max = fields.next()?.parse::<u64>().ok()?;
    if fields.next().is_some() {
        return None;
    }
    Some(max)
}

/// Turns successive `TcpExt` readings into increments.
///
/// The first reading of a counter only seeds it. A later reading adds its
/// growth since the one before. A reading LOWER than the one before (the
/// counter was reset) adds nothing and seeds again, so a reset never shows
/// as a huge jump. A counter that disappears forgets its seed. O(9).
#[derive(Debug, Clone, Copy)]
pub struct TcpExtDeltas {
    last: TcpExtReading,
}

impl TcpExtDeltas {
    /// A tracker with nothing seeded.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            last: [None; TCP_EXT_COUNTER_COUNT],
        }
    }

    /// Feeds one reading; returns each counter's increment. `None` where the
    /// counter is absent now or this reading only seeded it.
    pub fn advance(&mut self, current: &TcpExtReading) -> TcpExtReading {
        let mut increments = [None; TCP_EXT_COUNTER_COUNT];
        for (index, increment) in increments.iter_mut().enumerate() {
            let now = current[index];
            *increment = match (self.last[index], now) {
                (Some(before), Some(value)) => Some(value.saturating_sub(before)),
                _ => None,
            };
            self.last[index] = now;
        }
        increments
    }
}

impl Default for TcpExtDeltas {
    fn default() -> Self {
        Self::new()
    }
}

/// What one observation did to the edge latch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BacklogEdge {
    /// Nothing to report.
    Quiet,
    /// The backlog has persisted long enough: log ONCE.
    Fire,
    /// The backlog cleared below half the threshold: re-armed.
    Rearm,
}

/// Edge-triggered latch over the per-sample MAX.
///
/// Fires once after `consecutive` samples above `threshold`; stays silent
/// while the backlog persists; re-arms only when the max drops below half the
/// threshold, so a value hovering at the threshold cannot flap.
#[derive(Debug, Clone, Copy)]
pub struct BacklogLatch {
    threshold: u64,
    consecutive: u32,
    run: u32,
    alerted: bool,
}

impl BacklogLatch {
    /// A latch with the given threshold and consecutive-sample requirement.
    #[must_use]
    pub const fn new(threshold: u64, consecutive: u32) -> Self {
        Self {
            threshold,
            consecutive,
            run: 0,
            alerted: false,
        }
    }

    /// Feeds one sample's max. O(1).
    pub fn observe(&mut self, max_bytes: u64) -> BacklogEdge {
        if max_bytes > self.threshold {
            self.run = self.run.saturating_add(1);
            if !self.alerted && self.run >= self.consecutive {
                self.alerted = true;
                return BacklogEdge::Fire;
            }
            return BacklogEdge::Quiet;
        }
        self.run = 0;
        if self.alerted && max_bytes < self.threshold / 2 {
            self.alerted = false;
            return BacklogEdge::Rearm;
        }
        BacklogEdge::Quiet
    }

    /// Whether the latch is currently in the alerted state.
    #[must_use]
    pub const fn is_alerted(&self) -> bool {
        self.alerted
    }
}

/// Reusable state for reading `/proc` once a second without reallocating.
struct ProcReader {
    inodes: HashSet<u64>,
    table: String,
    /// The last `/proc/net/netstat` reading; all `None` when unreadable.
    tcp_ext: TcpExtReading,
}

impl ProcReader {
    fn new() -> Self {
        Self {
            inodes: HashSet::with_capacity(256),
            table: String::with_capacity(64 * 1024),
            tcp_ext: [None; TCP_EXT_COUNTER_COUNT],
        }
    }

    /// Reads `/proc/net/netstat` into `self.tcp_ext`, reusing the table
    /// buffer. An unreadable file or a file with no `TcpExt` lines leaves
    /// every counter `None`.
    fn sample_tcp_ext(&mut self) {
        self.table.clear();
        let read = std::fs::File::open(PROC_NET_NETSTAT_PATH)
            .and_then(|mut file| file.read_to_string(&mut self.table));
        if read.is_err() || !parse_tcp_ext_counters(&self.table, &mut self.tcp_ext) {
            self.tcp_ext = [None; TCP_EXT_COUNTER_COUNT];
        }
    }

    /// Takes one sample. `None` when `/proc` cannot be read at all (not
    /// Linux, or no procfs) — reported as absent, never as zero.
    fn sample(&mut self) -> Option<RxQueueSample> {
        self.inodes.clear();
        let entries = std::fs::read_dir("/proc/self/fd").ok()?;
        // O(1) EXEMPT: begin — cold 1 Hz sampler, one pass over this process's fds.
        for entry in entries.flatten() {
            if let Ok(target) = std::fs::read_link(entry.path())
                && let Some(inode) = target.to_str().and_then(parse_socket_inode)
            {
                self.inodes.insert(inode);
            }
        }
        // O(1) EXEMPT: end
        let mut acc = RxQueueSample::default();
        let mut any_table = false;
        for path in ["/proc/net/tcp", "/proc/net/tcp6"] {
            self.table.clear();
            let read =
                std::fs::File::open(path).and_then(|mut file| file.read_to_string(&mut self.table));
            if read.is_ok() {
                any_table = true;
                let inodes = &self.inodes;
                fold_rx_queue_table(
                    &self.table,
                    DHAN_FEED_REMOTE_PORT,
                    |inode| inodes.contains(&inode),
                    &mut acc,
                );
            }
        }
        any_table.then_some(acc)
    }
}

/// The receive-buffer ceilings, read once: `(rmem_max, tcp_rmem max)`.
/// Either is `None` when its file cannot be read or parsed. Blocking file
/// I/O: call it on the blocking pool.
fn read_rmem_ceilings() -> (Option<u64>, Option<u64>) {
    let rmem_max = std::fs::read_to_string(PROC_RMEM_MAX_PATH)
        .ok()
        .and_then(|text| parse_rmem_max(&text));
    let tcp_rmem_max = std::fs::read_to_string(PROC_TCP_RMEM_PATH)
        .ok()
        .and_then(|text| parse_tcp_rmem_max(&text));
    (rmem_max, tcp_rmem_max)
}

/// Adds one sample's `TcpExt` increments to the counter. A handle is
/// resolved the first time its counter is present and kept after that, so a
/// counter this kernel never reports is never registered (absent, never
/// zero).
fn publish_tcp_ext(
    current: &TcpExtReading,
    increments: &TcpExtReading,
    handles: &mut [Option<metrics::Counter>; TCP_EXT_COUNTER_COUNT],
) {
    for (index, handle) in handles.iter_mut().enumerate() {
        if current[index].is_none() {
            continue;
        }
        let counter = handle.get_or_insert_with(|| {
            metrics::counter!(KERNEL_TCP_EXT_COUNTER, KERNEL_TCP_EXT_LABEL => TCP_EXT_COUNTERS[index])
        });
        if let Some(increment) = increments[index] {
            counter.increment(increment);
        }
    }
}

/// Spawns the sampler on its own tokio task. Returns the handle; the task
/// exits on `shutdown`.
///
/// Wired from the Dhan feed stack, so it runs exactly when the sockets it
/// watches can exist.
pub fn spawn_kernel_rx_queue_sampler(
    shutdown: Arc<tokio::sync::Notify>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(run_kernel_rx_queue_sampler(shutdown))
}

async fn run_kernel_rx_queue_sampler(shutdown: Arc<tokio::sync::Notify>) {
    // Registered BEFORE the loop, so a `notify_waiters` that lands while a
    // sample is being taken is not lost (audit-findings Rule 16).
    let notified = shutdown.notified();
    tokio::pin!(notified);
    notified.as_mut().enable();

    let mut timer = tokio::time::interval(std::time::Duration::from_secs(
        KERNEL_RX_SAMPLE_INTERVAL_SECS,
    ));
    timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let max_gauge = metrics::gauge!(KERNEL_RX_QUEUE_MAX_GAUGE);
    let sum_gauge = metrics::gauge!(KERNEL_RX_QUEUE_SUM_GAUGE);
    let mut latch = BacklogLatch::new(
        KERNEL_RX_BACKLOG_THRESHOLD_BYTES,
        KERNEL_RX_BACKLOG_CONSECUTIVE_SAMPLES,
    );
    let mut reader = ProcReader::new();
    let mut unavailable_logged = false;
    let mut tcp_ext_deltas = TcpExtDeltas::new();
    let mut tcp_ext_handles: [Option<metrics::Counter>; TCP_EXT_COUNTER_COUNT] =
        std::array::from_fn(|_| None);

    // The receive-buffer ceilings, once per task start, on the blocking pool.
    // An unreadable file leaves its gauge absent, never zero.
    if let Ok((rmem_max, tcp_rmem_max)) = tokio::task::spawn_blocking(read_rmem_ceilings).await {
        #[allow(clippy::cast_precision_loss)] // APPROVED: a byte ceiling well under 2^52 in a gauge
        {
            if let Some(bytes) = rmem_max {
                metrics::gauge!(KERNEL_RMEM_MAX_GAUGE).set(bytes as f64);
            }
            if let Some(bytes) = tcp_rmem_max {
                metrics::gauge!(KERNEL_TCP_RMEM_MAX_GAUGE).set(bytes as f64);
            }
        }
        info!(
            rmem_max_bytes = ?rmem_max,
            tcp_rmem_max_bytes = ?tcp_rmem_max,
            "kernel receive-buffer ceilings read once at sampler start"
        );
    }

    loop {
        tokio::select! {
            () = &mut notified => return,
            _ = timer.tick() => {}
        }
        if !tickvault_common::market_hours::is_within_trading_session_ist() {
            // Outside the session the sockets are idle; a stale latch must not
            // carry into the next morning.
            latch = BacklogLatch::new(
                KERNEL_RX_BACKLOG_THRESHOLD_BYTES,
                KERNEL_RX_BACKLOG_CONSECUTIVE_SAMPLES,
            );
            // The first in-session sample seeds again, so overnight growth is
            // never published as one jump at the open.
            tcp_ext_deltas = TcpExtDeltas::new();
            continue;
        }
        // The /proc reads are blocking file I/O: they run on the blocking
        // pool, never on a runtime worker the drain or a socket may need.
        // The reader (and its buffers) travels there and back, so nothing is
        // re-allocated per sample beyond the hand-off itself (once a second).
        let (returned, sampled) = match tokio::task::spawn_blocking(move || {
            let sample = reader.sample();
            reader.sample_tcp_ext();
            (reader, sample)
        })
        .await
        {
            Ok(pair) => pair,
            // The sample panicked: start a fresh reader next second.
            Err(_) => (ProcReader::new(), None),
        };
        reader = returned;
        // Published whether or not the socket table was readable: the two
        // reads are independent.
        let increments = tcp_ext_deltas.advance(&reader.tcp_ext);
        publish_tcp_ext(&reader.tcp_ext, &increments, &mut tcp_ext_handles);
        let Some(sample) = sampled else {
            if !unavailable_logged {
                unavailable_logged = true;
                info!(
                    "kernel receive-queue sampler: /proc is not readable on this host, so the \
                     slow-consumer early warning is unavailable (gauges stay absent, never zero)"
                );
            }
            continue;
        };
        #[allow(clippy::cast_precision_loss)] // APPROVED: a byte count well under 2^52 in a gauge
        {
            max_gauge.set(sample.max_bytes as f64);
            sum_gauge.set(sample.sum_bytes as f64);
        }
        match latch.observe(sample.max_bytes) {
            BacklogEdge::Fire => {
                error!(
                    code = ErrorCode::WsGapConnectionState.code_str(),
                    source = KERNEL_RX_BACKLOG_SOURCE,
                    max_bytes = sample.max_bytes,
                    sum_bytes = sample.sum_bytes,
                    sockets = sample.sockets,
                    threshold_bytes = KERNEL_RX_BACKLOG_THRESHOLD_BYTES,
                    consecutive_samples = KERNEL_RX_BACKLOG_CONSECUTIVE_SAMPLES,
                    "our socket reader is falling behind the Dhan feed: a kernel receive queue \
                     has stayed above the threshold for consecutive samples. Dhan skips a slow \
                     consumer forward to the latest state, so ticks may be lost at Dhan's side \
                     with no counter of ours able to see it. Check the frame drain (ring dwell, \
                     fold stalls, disk). Logged once per episode."
                );
            }
            BacklogEdge::Rearm => {
                info!(
                    max_bytes = sample.max_bytes,
                    "kernel receive-queue backlog cleared — slow-consumer early warning re-armed"
                );
            }
            BacklogEdge::Quiet => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEADER: &str = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode";
    // ESTABLISHED (01) to port 443 (0x01BB), rx_queue 0x00500000 = 5 MiB, inode 4242.
    const V4_ESTAB_443: &str = "   3: 0A00020F:A1B2 0D2A8F12:01BB 01 00000000:00500000 02:000000C8 00000000  1000        0 4242 2 0000000000000000 20 4 30 10 -1";
    // LISTEN (0A) on 9000 — never counted.
    const V4_LISTEN: &str = "   0: 00000000:2328 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 1111 1 0000000000000000 100 0 0 10 0";
    // ESTABLISHED to port 443 over IPv6, rx 0x10 = 16 bytes, inode 7777.
    const V6_ESTAB_443: &str = "   1: 0000000000000000FFFF00000F02000A:B3C4 0000000000000000FFFF0000128F2A0D:01BB 01 00000000:00000010 00:00000000 00000000  1000        0 7777 1 0000000000000000 20 4 30 10 -1";

    #[test]
    fn parse_proc_net_tcp_line_reads_the_hex_rx_queue() {
        let row = parse_proc_net_tcp_line(V4_ESTAB_443).expect("data row parses");
        assert_eq!(row.remote_port, 443);
        assert_eq!(row.state, TCP_STATE_ESTABLISHED);
        assert_eq!(
            row.rx_queue,
            5 * 1024 * 1024,
            "rx_queue is HEX, not decimal"
        );
        assert_eq!(row.inode, 4242);

        let v6 = parse_proc_net_tcp_line(V6_ESTAB_443).expect("tcp6 row parses");
        assert_eq!(v6.remote_port, 443);
        assert_eq!(v6.rx_queue, 16);
        assert_eq!(v6.inode, 7777);
    }

    #[test]
    fn parse_proc_net_tcp_line_refuses_header_and_garbage() {
        assert_eq!(parse_proc_net_tcp_line(HEADER), None);
        assert_eq!(parse_proc_net_tcp_line(""), None);
        assert_eq!(parse_proc_net_tcp_line("   0: short"), None);
        // Non-hex rx queue must refuse, never read as zero.
        let bad = V4_ESTAB_443.replace("00000000:00500000", "00000000:zz500000");
        assert_eq!(parse_proc_net_tcp_line(&bad), None);
    }

    #[test]
    fn parse_socket_inode_reads_only_socket_links() {
        assert_eq!(parse_socket_inode("socket:[4242]"), Some(4242));
        assert_eq!(parse_socket_inode("pipe:[4242]"), None);
        assert_eq!(parse_socket_inode("/dev/null"), None);
        assert_eq!(parse_socket_inode("socket:[]"), None);
    }

    #[test]
    fn fold_rx_queue_table_counts_only_our_established_443_sockets() {
        let foreign = V4_ESTAB_443.replace(" 4242 ", " 9999 ");
        let table = format!("{HEADER}\n{V4_LISTEN}\n{V4_ESTAB_443}\n{foreign}\n");
        let mut acc = RxQueueSample::default();
        fold_rx_queue_table(&table, 443, |inode| inode == 4242, &mut acc);
        assert_eq!(
            acc.sockets, 1,
            "the listener and the other process's socket are excluded"
        );
        assert_eq!(acc.max_bytes, 5 * 1024 * 1024);
        assert_eq!(acc.sum_bytes, 5 * 1024 * 1024);

        // Both tables fold into one sample.
        fold_rx_queue_table(V6_ESTAB_443, 443, |inode| inode == 7777, &mut acc);
        assert_eq!(acc.sockets, 2);
        assert_eq!(acc.max_bytes, 5 * 1024 * 1024);
        assert_eq!(acc.sum_bytes, 5 * 1024 * 1024 + 16);
    }

    #[test]
    fn backlog_latch_fires_once_after_consecutive_samples_and_rearms_below_half() {
        let t = KERNEL_RX_BACKLOG_THRESHOLD_BYTES;
        let mut latch = BacklogLatch::new(t, 5);
        for _ in 0..4 {
            assert_eq!(latch.observe(t + 1), BacklogEdge::Quiet);
        }
        assert_eq!(
            latch.observe(t + 1),
            BacklogEdge::Fire,
            "fifth consecutive sample"
        );
        assert!(latch.is_alerted());
        for _ in 0..20 {
            assert_eq!(
                latch.observe(t * 2),
                BacklogEdge::Quiet,
                "edge, never level"
            );
        }
        // Dropping to the threshold but not below half does NOT re-arm.
        assert_eq!(latch.observe(t), BacklogEdge::Quiet);
        assert!(latch.is_alerted());
        assert_eq!(latch.observe(t / 2 - 1), BacklogEdge::Rearm);
        assert!(!latch.is_alerted());
    }

    #[test]
    fn backlog_latch_resets_the_run_on_a_single_low_sample() {
        let t = KERNEL_RX_BACKLOG_THRESHOLD_BYTES;
        let mut latch = BacklogLatch::new(t, 5);
        for _ in 0..4 {
            assert_eq!(latch.observe(t + 1), BacklogEdge::Quiet);
        }
        assert_eq!(
            latch.observe(0),
            BacklogEdge::Quiet,
            "a cleared queue breaks the run"
        );
        for _ in 0..4 {
            assert_eq!(latch.observe(t + 1), BacklogEdge::Quiet);
        }
        assert_eq!(latch.observe(t + 1), BacklogEdge::Fire);
    }

    #[test]
    fn is_alerted_is_false_on_a_new_latch_and_follows_the_edges() {
        let mut latch = BacklogLatch::new(100, 1);
        assert!(!latch.is_alerted());
        assert_eq!(latch.observe(101), BacklogEdge::Fire);
        assert!(latch.is_alerted());
        assert_eq!(latch.observe(10), BacklogEdge::Rearm);
        assert!(!latch.is_alerted());
    }

    #[test]
    fn proc_reader_sample_is_absent_or_consistent() {
        // On Linux this reads this test process's own sockets; elsewhere it is
        // `None`. Either way it must not panic, and a present sample must be
        // internally consistent.
        let mut reader = ProcReader::new();
        if let Some(sample) = reader.sample() {
            assert!(sample.max_bytes <= sample.sum_bytes);
            assert!(sample.sockets > 0 || sample.sum_bytes == 0);
        }
    }

    #[tokio::test]
    async fn spawn_kernel_rx_queue_sampler_exits_on_shutdown() {
        let shutdown = Arc::new(tokio::sync::Notify::new());
        let handle = spawn_kernel_rx_queue_sampler(Arc::clone(&shutdown));
        // Give the task a tick to register before notifying.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        shutdown.notify_waiters();
        tokio::time::timeout(std::time::Duration::from_secs(5), handle)
            .await
            .expect("the sampler must exit on shutdown")
            .expect("the sampler task must not panic");
    }

    // A trimmed /proc/net/netstat: TcpExt header, TcpExt values, then the
    // IpExt pair, which must be ignored. Columns are in an order unlike the
    // TCP_EXT_COUNTERS order, so a by-position reader would get them wrong.
    const NETSTAT: &str = "TcpExt: SyncookiesSent RcvPruned PruneCalled OfoPruned TCPRcvCollapsed TCPBacklogDrop TCPRcvQDrop TCPZeroWindowDrop TCPWantZeroWindowAdv TCPToZeroWindowAdv\n\
TcpExt: 7 2 3 4 5 6 8 9 10 11\n\
IpExt: InNoRoutes PruneCalled\n\
IpExt: 99 99\n";

    #[test]
    fn parse_tcp_ext_counters_reads_by_name_not_by_column() {
        let mut out = [Some(123); TCP_EXT_COUNTER_COUNT];
        assert!(parse_tcp_ext_counters(NETSTAT, &mut out));
        assert_eq!(
            out,
            [
                Some(3),  // PruneCalled
                Some(2),  // RcvPruned
                Some(4),  // OfoPruned
                Some(5),  // TCPRcvCollapsed
                Some(8),  // TCPRcvQDrop
                Some(6),  // TCPBacklogDrop
                Some(9),  // TCPZeroWindowDrop
                Some(11), // TCPToZeroWindowAdv
                Some(10), // TCPWantZeroWindowAdv
            ],
            "the IpExt PruneCalled (99) must not leak in"
        );
    }

    #[test]
    fn parse_tcp_ext_counters_leaves_missing_names_absent_not_zero() {
        // An older kernel without the zero-window drop counters.
        let old = "TcpExt: PruneCalled RcvPruned OfoPruned\nTcpExt: 1 2 3\n";
        let mut out = [Some(42); TCP_EXT_COUNTER_COUNT];
        assert!(parse_tcp_ext_counters(old, &mut out));
        assert_eq!(&out[..3], &[Some(1), Some(2), Some(3)]);
        assert!(
            out[3..].iter().all(Option::is_none),
            "a counter the kernel does not report is None, never 0"
        );
    }

    #[test]
    fn parse_tcp_ext_counters_refuses_garbage_values_and_missing_pairs() {
        let mut out = [None; TCP_EXT_COUNTER_COUNT];
        assert!(!parse_tcp_ext_counters("", &mut out));
        assert!(!parse_tcp_ext_counters("TcpExt: PruneCalled\n", &mut out));
        assert!(!parse_tcp_ext_counters("IpExt: a\nIpExt: 1\n", &mut out));
        // A value that is not a number stays absent.
        assert!(parse_tcp_ext_counters(
            "TcpExt: PruneCalled RcvPruned\nTcpExt: x 5\n",
            &mut out
        ));
        assert_eq!(out[0], None);
        assert_eq!(out[1], Some(5));
        // A short value line leaves the trailing names absent.
        assert!(parse_tcp_ext_counters(
            "TcpExt: PruneCalled RcvPruned\nTcpExt: 4\n",
            &mut out
        ));
        assert_eq!(out[0], Some(4));
        assert_eq!(out[1], None);
    }

    #[test]
    fn parse_rmem_max_reads_one_number() {
        assert_eq!(parse_rmem_max("4194304\n"), Some(4_194_304));
        assert_eq!(parse_rmem_max("  67108864 "), Some(67_108_864));
        assert_eq!(parse_rmem_max(""), None);
        assert_eq!(parse_rmem_max("abc"), None);
        assert_eq!(parse_rmem_max("-1"), None);
    }

    #[test]
    fn parse_tcp_rmem_max_reads_the_third_field_only() {
        assert_eq!(
            parse_tcp_rmem_max("4096\t131072\t33554432\n"),
            Some(33_554_432)
        );
        assert_eq!(parse_tcp_rmem_max("4096 131072"), None);
        assert_eq!(parse_tcp_rmem_max("4096 131072 x"), None);
        assert_eq!(parse_tcp_rmem_max("4096 131072 6291456 9"), None);
        assert_eq!(parse_tcp_rmem_max(""), None);
    }

    #[test]
    fn tcp_ext_deltas_first_sample_seeds_then_adds_growth() {
        let mut deltas = TcpExtDeltas::new();
        let mut reading = [None; TCP_EXT_COUNTER_COUNT];
        reading[0] = Some(100);
        reading[1] = Some(5);
        assert_eq!(
            deltas.advance(&reading),
            [None; TCP_EXT_COUNTER_COUNT],
            "the first sample only seeds"
        );
        reading[0] = Some(130);
        reading[1] = Some(5);
        let increments = deltas.advance(&reading);
        assert_eq!(increments[0], Some(30));
        assert_eq!(increments[1], Some(0));
        assert!(increments[2..].iter().all(Option::is_none));
    }

    #[test]
    fn tcp_ext_deltas_saturate_on_a_reset_and_reseed() {
        let mut deltas = TcpExtDeltas::default();
        let mut reading = [None; TCP_EXT_COUNTER_COUNT];
        reading[4] = Some(1_000);
        deltas.advance(&reading);
        reading[4] = Some(10);
        assert_eq!(
            deltas.advance(&reading)[4],
            Some(0),
            "a counter that went down adds nothing"
        );
        reading[4] = Some(25);
        assert_eq!(
            deltas.advance(&reading)[4],
            Some(15),
            "the reset value is the new seed"
        );
    }

    #[test]
    fn tcp_ext_deltas_forget_a_counter_that_disappears() {
        let mut deltas = TcpExtDeltas::new();
        let mut reading = [None; TCP_EXT_COUNTER_COUNT];
        reading[2] = Some(50);
        deltas.advance(&reading);
        reading[2] = None;
        assert_eq!(deltas.advance(&reading)[2], None);
        reading[2] = Some(70);
        assert_eq!(
            deltas.advance(&reading)[2],
            None,
            "after a gap the counter seeds again instead of jumping"
        );
    }

    #[test]
    fn publish_tcp_ext_registers_only_present_counters_and_adds_increments() {
        let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, || {
            let mut handles: [Option<metrics::Counter>; TCP_EXT_COUNTER_COUNT] =
                std::array::from_fn(|_| None);
            let mut deltas = TcpExtDeltas::new();
            let mut reading = [None; TCP_EXT_COUNTER_COUNT];
            reading[1] = Some(10); // RcvPruned
            let increments = deltas.advance(&reading);
            publish_tcp_ext(&reading, &increments, &mut handles);
            reading[1] = Some(17);
            let increments = deltas.advance(&reading);
            publish_tcp_ext(&reading, &increments, &mut handles);
            assert!(handles[1].is_some());
            assert!(
                handles
                    .iter()
                    .enumerate()
                    .all(|(i, h)| i == 1 || h.is_none())
            );
        });
        let rendered = handle.render();
        assert!(
            rendered.contains("tv_kernel_tcp_ext_total{counter=\"RcvPruned\"} 7"),
            "seed adds nothing, then +7: {rendered}"
        );
        assert!(
            !rendered.contains("counter=\"PruneCalled\""),
            "an absent counter is never registered: {rendered}"
        );
    }

    #[test]
    fn read_rmem_ceilings_matches_the_parsers_on_this_host() {
        // On Linux both files exist; elsewhere both are None. Either way the
        // reader must agree with parsing the files directly.
        let (rmem_max, tcp_rmem_max) = read_rmem_ceilings();
        let direct = std::fs::read_to_string(PROC_RMEM_MAX_PATH)
            .ok()
            .and_then(|text| parse_rmem_max(&text));
        assert_eq!(rmem_max, direct);
        if let Some(max) = tcp_rmem_max {
            assert!(max > 0);
        }
    }

    #[test]
    fn proc_reader_sample_tcp_ext_is_absent_or_parsed() {
        let mut reader = ProcReader::new();
        reader.sample_tcp_ext();
        let readable = std::fs::read_to_string(PROC_NET_NETSTAT_PATH)
            .map(|text| text.contains(TCP_EXT_LINE_PREFIX))
            .unwrap_or(false);
        if readable {
            // PruneCalled has been in TcpExt since Linux 2.6.
            assert!(reader.tcp_ext[0].is_some());
        } else {
            assert!(reader.tcp_ext.iter().all(Option::is_none));
        }
    }

    #[test]
    fn tcp_ext_names_are_pinned() {
        assert_eq!(KERNEL_TCP_EXT_COUNTER, "tv_kernel_tcp_ext_total");
        assert_eq!(KERNEL_TCP_EXT_LABEL, "counter");
        assert_eq!(KERNEL_RMEM_MAX_GAUGE, "tv_kernel_rmem_max_bytes");
        assert_eq!(KERNEL_TCP_RMEM_MAX_GAUGE, "tv_kernel_tcp_rmem_max_bytes");
        assert_eq!(TCP_EXT_COUNTERS.len(), TCP_EXT_COUNTER_COUNT);
        let mut sorted = TCP_EXT_COUNTERS;
        sorted.sort_unstable();
        assert!(
            sorted.windows(2).all(|pair| pair[0] != pair[1]),
            "no duplicate counter names"
        );
    }

    #[test]
    fn names_are_pinned() {
        assert_eq!(
            KERNEL_RX_QUEUE_MAX_GAUGE,
            "tv_dhan_ws_kernel_rx_queue_bytes_max"
        );
        assert_eq!(
            KERNEL_RX_QUEUE_SUM_GAUGE,
            "tv_dhan_ws_kernel_rx_queue_bytes_sum"
        );
        assert_eq!(KERNEL_RX_BACKLOG_SOURCE, "kernel_rx_queue_backlog");
        assert_eq!(DHAN_FEED_REMOTE_PORT, 443);
    }
}
