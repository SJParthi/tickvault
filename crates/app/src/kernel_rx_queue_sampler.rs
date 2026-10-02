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
//! # Honest limits
//!
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
//! each line one hash probe. Cold, 1 Hz, off the frame path. The inode set and
//! the read buffers are reused across samples.

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
}

impl ProcReader {
    fn new() -> Self {
        Self {
            inodes: HashSet::with_capacity(256),
            table: String::with_capacity(64 * 1024),
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
            continue;
        }
        let Some(sample) = reader.sample() else {
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
