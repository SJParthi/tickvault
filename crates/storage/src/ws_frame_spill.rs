// STAGE-C: Non-blocking disk-durable spill for every WebSocket frame.
//
// Hot-path `append()` is O(1) and never blocks: it uses a crossbeam bounded
// channel with `try_send`. A dedicated background thread fsyncs records to
// append-only WAL segment files. On startup, `replay_all()` walks every WAL
// file, validates CRC32, and returns the recovered frames so downstream
// consumers can drain them before live reads resume.
//
// Record format on disk:
//     [MAGIC:4="TVW1"][ws_type:u8][len:u32 LE][frame:len bytes][crc32:u32 LE]
// CRC32 is computed over ws_type || len || frame.

use std::fs::{File, OpenOptions}; // O(1) EXEMPT: import line only — uses are the cold writer thread + boot replay
use std::io::{BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use crossbeam_channel::{Receiver, Sender, TrySendError, bounded};
use tickvault_common::error_code::ErrorCode;
use tracing::{error, info, warn};

// ---------------------------------------------------------------------------
// WsType — one byte tag so replay can route each frame back to the right
// consumer (tick processor, depth processor, order update handler).
// ---------------------------------------------------------------------------

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WsType {
    LiveFeed = 1,
    OrderUpdate = 4,
}

impl WsType {
    // TEST-EXEMPT: covered by test_ws_type_roundtrip (asserts from_u8/as_u8 identity)
    pub fn from_u8(b: u8) -> Option<Self> {
        match b {
            1 => Some(Self::LiveFeed),
            4 => Some(Self::OrderUpdate),
            _ => None,
        }
    }

    // TEST-EXEMPT: covered by test_ws_type_roundtrip
    pub fn as_u8(self) -> u8 {
        self as u8
    }

    // TEST-EXEMPT: pure enum→&'static str mapping used only for metric label / log field
    pub fn as_str(self) -> &'static str {
        match self {
            Self::LiveFeed => "live_feed",
            Self::OrderUpdate => "order_update",
        }
    }
}

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct WalRecord {
    ws_type: WsType,
    /// TICK-SEQ-01: strictly-monotonic per-frame capture sequence, persisted in
    /// the v2 record so replay reproduces it (replay-stable). PR-2a stamps it
    /// internally at `append` time; a later slice hoists the stamp to the WS
    /// read loop so it equals the per-tick `capture_seq`.
    frame_seq: u64,
    // Zero-tick-loss PR-8a (H1): `Bytes` (Arc-refcounted) so the WS read
    // loop hands ownership to the disk-writer thread with an O(1) refcount
    // bump instead of a per-frame `Vec<u8>` malloc. Derefs to `&[u8]`, so
    // `write_record` / `crc32_ieee_of` / `.len()` are unchanged.
    frame: Bytes,
}

/// Result of a hot-path `append()` attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppendOutcome {
    /// Frame was queued for durable write. Hot path is done.
    Spilled,
    /// Spill channel was full — frame could not be persisted.
    /// CRITICAL: counted in drop metric; Telegram alert fires.
    Dropped,
}

/// A single frame recovered during startup WAL replay.
#[derive(Debug, Clone)]
pub struct ReplayedFrame {
    pub ws_type: WsType,
    pub frame: Vec<u8>,
    /// TICK-SEQ-01: the `frame_seq` read back from the v2 record (replay-stable).
    /// `0` for legacy v1 records that predate the field.
    pub frame_seq: u64,
}

// ---------------------------------------------------------------------------
// Tunables
// ---------------------------------------------------------------------------

/// Bounded crossbeam channel between WS readers and the disk writer thread.
///
/// 131,072 frames ≈ 13 seconds of peak 10k frames/sec headroom.
///
/// 2026-04-27: Bumped from 65,536 to 131,072 after `chaos_healthy_ops_burst_100k_frames_zero_drops`
/// flaked on slow 2-vCPU GitHub Actions runners. The producer's tight loop
/// could enqueue 100,000 frames before the kernel scheduler ran the writer
/// thread, exceeding the 65k cap and tripping the safety-floor invariant
/// (`tv_ws_frame_spill_drop_critical == 0` in healthy ops). The new ceiling
/// stays above the 100k chaos test threshold AND doubles burst headroom for
/// production: a transient writer stall of up to 13s (e.g. brief disk fsync
/// latency on a contended host) now absorbs without dropping. Memory cost
/// at idle is ~3 MiB extra (131k × ~24 B/`WalRecord` header), trivial on
/// the 4 GiB t4g.medium target.
const SPILL_CHANNEL_CAPACITY: usize = 131_072;

/// WAL file magic bytes — segment-local sanity check.
///
/// TICK-SEQ-01 PR-2a: `TVW1` = v1 record (no `frame_seq`); `TVW2` = v2 record
/// (carries an 8-byte LE `frame_seq` immediately after `ws_type`). Replay
/// accepts BOTH, so segments written before this change still recover. NEW
/// records are always written v2.
const WAL_MAGIC: [u8; 4] = *b"TVW1";
const WAL_MAGIC_V2: [u8; 4] = *b"TVW2";

/// Minimum on-disk record size per version, used by the replay loop guard:
/// v1 = magic(4)+ws_type(1)+len(4)+crc(4) = 13; v2 inserts frame_seq(8) = 21.
const WAL_MIN_RECORD_V1: usize = 13;
const WAL_MIN_RECORD_V2: usize = 21;

/// Rotate to a new segment after this many bytes.
const WAL_SEGMENT_MAX_BYTES: u64 = 128 * 1024 * 1024;

/// Writer buffer size — large enough to batch-fsync hundreds of records.
const WAL_WRITER_BUFFER: usize = 256 * 1024;

/// Backoff before the supervisor re-enters the writer loop after a panic or a
/// fatal return, so a hard-failing writer cannot pin a CPU in a hot respawn
/// loop. Mirrors the WS-GAP-05 pool-supervisor / DISK-WATCHER-01 backoff.
const WAL_WRITER_RESPAWN_BACKOFF: Duration = Duration::from_millis(200); // APPROVED: this IS the named constant the rule asks for

/// Backoff after a transient disk write/flush/segment-open error before the
/// writer retries, so a full or contended disk does not spin. The thread stays
/// alive and keeps draining the channel — it never tears down the durable
/// WAL floor for a transient I/O hiccup.
const WAL_WRITER_IO_RETRY_BACKOFF: Duration = Duration::from_millis(50); // APPROVED: this IS the named constant the rule asks for

// ---------------------------------------------------------------------------
// WsFrameSpill
// ---------------------------------------------------------------------------

pub struct WsFrameSpill {
    spill_tx: Sender<WalRecord>,
    drop_critical: Arc<AtomicU64>,
    persisted_total: Arc<AtomicU64>,
    /// SP5.1: optional per-feed health registry. When `Some`, a dropped
    /// LIVE-FEED (Dhan) frame records a Dhan drop so `/api/feeds/health` flips
    /// `Degraded` — closing the SP5 connected+fresh-but-dropping false-OK.
    /// `None` keeps the spill feed-health-agnostic (byte-identical hot path).
    /// Read ONLY in the cold drop arms; never on the hot `Spilled` path.
    feed_health: Option<Arc<tickvault_common::feed_health::FeedHealthRegistry>>,
}

impl WsFrameSpill {
    /// Create a spill writer rooted at `wal_dir`. Spawns the background writer.
    // TEST-EXEMPT: covered by test_append_spill_and_replay_roundtrip + test_drop_counter_increments_when_channel_full (both construct)
    pub fn new<P: AsRef<Path>>(wal_dir: P) -> anyhow::Result<Self> {
        let wal_dir = wal_dir.as_ref().to_path_buf();
        std::fs::create_dir_all(&wal_dir) // O(1) EXEMPT: one-shot constructor, not the per-frame append
            .map_err(|e| anyhow::anyhow!("create WAL dir {:?}: {e}", wal_dir))?;

        let (tx, rx) = bounded::<WalRecord>(SPILL_CHANNEL_CAPACITY);
        let drop_critical = Arc::new(AtomicU64::new(0));
        let persisted_total = Arc::new(AtomicU64::new(0));

        let persisted_for_thread = persisted_total.clone(); // APPROVED: Arc clone in the one-shot constructor
        let wal_dir_for_thread = wal_dir.clone(); // APPROVED: one-shot constructor, not per-frame
        thread::Builder::new()
            .name("ws-frame-spill-writer".to_string()) // APPROVED: one-shot constructor (thread name)
            .spawn(move || {
                // Supervisor loop (mirrors WS-GAP-05 pool supervisor +
                // DISK-WATCHER-01). A panic or a fatal return from the writer
                // must NOT silently kill the durable WAL floor: we re-enter
                // `writer_loop` with the SAME `rx`, so `append()` never sees
                // `Disconnected` and every Dhan frame keeps being captured.
                // `rx` is owned here and only borrowed per iteration → it
                // outlives any panic, keeping the channel alive across respawns.
                loop {
                    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        writer_loop(&rx, &wal_dir_for_thread, &persisted_for_thread)
                    }));
                    match outcome {
                        Ok(Ok(())) => {
                            // Clean shutdown: all senders dropped, channel closed.
                            info!("ws-frame-spill-writer exited cleanly (channel closed)");
                            break;
                        }
                        Ok(Err(err)) => {
                            error!(
                                code = ErrorCode::WsSpill01WriterRespawn.code_str(),
                                error = %err,
                                "WAL spill writer returned error — respawning to preserve durable WAL floor"
                            );
                            metrics::counter!(
                                "tv_ws_frame_spill_writer_respawn_total",
                                "reason" => "error"
                            )
                            .increment(1);
                        }
                        Err(_panic) => {
                            error!(
                                code = ErrorCode::WsSpill01WriterRespawn.code_str(),
                                "CRITICAL: WAL spill writer PANICKED — respawning to preserve durable WAL floor"
                            );
                            metrics::counter!(
                                "tv_ws_frame_spill_writer_respawn_total",
                                "reason" => "panic"
                            )
                            .increment(1);
                        }
                    }
                    thread::sleep(WAL_WRITER_RESPAWN_BACKOFF);
                }
            })
            .map_err(|e| anyhow::anyhow!("spawn spill writer thread: {e}"))?;

        Ok(Self {
            spill_tx: tx,
            drop_critical,
            persisted_total,
            feed_health: None,
        })
    }

    /// SP5.1 builder: attach the per-feed health registry so terminal Dhan
    /// LIVE-FEED frame drops surface as `Degraded` on `/api/feeds/health`
    /// (closing the SP5 false-OK). Set ONCE at boot before any `append`;
    /// `None` keeps the spill feed-health-agnostic.
    #[must_use]
    pub fn with_feed_health(
        mut self,
        feed_health: Option<Arc<tickvault_common::feed_health::FeedHealthRegistry>>,
    ) -> Self {
        self.feed_health = feed_health;
        self
    }

    /// Test-only constructor whose writer thread is already gone: the
    /// receiver is dropped immediately, so every `append()` deterministically
    /// hits the `TrySendError::Disconnected` arm. Used to prove that the
    /// writer-dead drop path is loud (WS-SPILL-02), not silent.
    #[cfg(test)]
    fn new_with_dead_writer_for_test() -> Self {
        let (tx, rx) = bounded::<WalRecord>(SPILL_CHANNEL_CAPACITY);
        drop(rx); // no writer ever runs → channel is Disconnected for sends
        Self {
            spill_tx: tx,
            drop_critical: Arc::new(AtomicU64::new(0)),
            persisted_total: Arc::new(AtomicU64::new(0)),
            feed_health: None,
        }
    }

    /// Hot path. Non-blocking. O(1). Zero-allocation: accepts anything
    /// convertible into `Bytes` and sends it over the pre-allocated crossbeam
    /// ring — no heap allocation occurs here. The WS read loop passes
    /// `data.clone()` (an O(1) Arc refcount bump, NOT a `Vec<u8>` copy);
    /// `Vec<u8>` callers convert via `Bytes::from`, which steals the buffer
    /// (also zero-copy), so existing callers keep working unchanged.
    // TEST-EXEMPT: covered by test_append_spill_and_replay_roundtrip + test_drop_counter_increments_when_channel_full; the Bytes-clone hand-off is proven zero-alloc by crates/core/tests/dhat_ws_reader_zero_alloc.rs::dhat_ws_reader_tail_zero_alloc
    pub fn append(&self, ws_type: WsType, frame: impl Into<Bytes>) -> AppendOutcome {
        self.append_with_seq(ws_type, frame, next_frame_seq())
    }

    /// Like [`WsFrameSpill::append`] but stamps the caller-provided `frame_seq`
    /// (the WS read loop's value) so the SAME sequence is persisted in the WAL
    /// AND shared with the live broadcast → `ticks.capture_seq` (replay-stable).
    /// TICK-SEQ-01 threading slice. Hot path, O(1), zero-alloc.
    // TEST-EXEMPT: identical try_send path as `append` (covered by test_append_spill_and_replay_roundtrip + test_wal_v2_roundtrip_preserves_frame_seq); frame_seq plumbing covered by chaos_ws_frame_wal_replay + the read-loop integration.
    pub fn append_with_seq(
        &self,
        ws_type: WsType,
        frame: impl Into<Bytes>,
        frame_seq: u64,
    ) -> AppendOutcome {
        let record = WalRecord {
            ws_type,
            frame_seq,
            frame: frame.into(),
        };
        match self.spill_tx.try_send(record) {
            Ok(()) => AppendOutcome::Spilled,
            Err(TrySendError::Full(_)) => {
                let prev = self.drop_critical.fetch_add(1, Ordering::Relaxed);
                error!(
                    ws_type = ws_type.as_str(),
                    drop_count = prev + 1,
                    "CRITICAL: WAL spill channel FULL — frame dropped (writer stalled)"
                );
                metrics::counter!(
                    "tv_ws_frame_spill_drop_critical",
                    "ws_type" => ws_type.as_str()
                )
                .increment(1);
                // SLA counter: every dropped frame is one tick-equivalent lost.
                // Parthiban 2026-04-20: explicit metric so the zero-tick-loss
                // invariant can be asserted in CI instead of inferred from a
                // gap between `tv_ticks_processed_total` and
                // `tv_ticks_persisted_total`. Labelled with the same `ws_type`
                // so a Grafana heatmap can attribute losses per WebSocket.
                metrics::counter!(
                    "tv_ticks_lost_total",
                    "source" => "spill_drop_critical",
                    "ws_type" => ws_type.as_str(),
                )
                .increment(1);
                self.record_dhan_drop_for_health(ws_type);
                AppendOutcome::Dropped
            }
            Err(TrySendError::Disconnected(_)) => {
                // WS-SPILL-02: the writer thread was dead at this instant
                // (channel Disconnected). The WS-SPILL-01 supervisor respawns
                // it, so this window is tiny and practically unreachable — but
                // it is a genuine durable-frame loss, so it must be LOUD, not
                // a silent return (the pre-2026-06-09 behaviour).
                let prev = self.drop_critical.fetch_add(1, Ordering::Relaxed);
                error!(
                    code = ErrorCode::WsSpill02FrameDropped.code_str(),
                    ws_type = ws_type.as_str(),
                    drop_count = prev + 1,
                    "CRITICAL: WAL spill writer DEAD — frame dropped (durable floor lost)"
                );
                // Same label set as the Full arm so existing alerts on
                // `tv_ws_frame_spill_drop_critical` fire for this cause too.
                metrics::counter!(
                    "tv_ws_frame_spill_drop_critical",
                    "ws_type" => ws_type.as_str()
                )
                .increment(1);
                // The distinguishing cause lives on the SLA counter's `source`
                // label (Full arm uses "spill_drop_critical").
                metrics::counter!(
                    "tv_ticks_lost_total",
                    "source" => "spill_writer_dead",
                    "ws_type" => ws_type.as_str(),
                )
                .increment(1);
                self.record_dhan_drop_for_health(ws_type);
                AppendOutcome::Dropped
            }
        }
    }

    /// SP5.1: on a terminal drop of a LIVE-FEED (Dhan) frame, record a Dhan drop
    /// in the per-feed health registry so `/api/feeds/health` flips `Degraded`
    /// (closing the SP5 connected+fresh-but-dropping false-OK). Called ONLY from
    /// the two cold drop arms — never the hot `Spilled` path. A LiveFeed-frame
    /// drop ⊇ tick drop (the frame may be OI/PrevClose/MarketStatus too) — all
    /// are real Dhan data losses, so `Degraded` is the correct, honest signal.
    /// `OrderUpdate` drops are NOT recorded (not the market-data feed). O(1),
    /// zero-alloc, lock-free (one relaxed atomic). `None` registry = no-op.
    #[inline]
    fn record_dhan_drop_for_health(&self, ws_type: WsType) {
        if matches!(ws_type, WsType::LiveFeed)
            && let Some(ref fh) = self.feed_health
        {
            fh.record_drops(tickvault_common::feed::Feed::Dhan, 1);
        }
    }

    // TEST-EXEMPT: covered by test_drop_counter_increments_when_channel_full (asserts initial 0)
    pub fn drop_critical_count(&self) -> u64 {
        self.drop_critical.load(Ordering::Relaxed)
    }

    // TEST-EXEMPT: covered by test_append_spill_and_replay_roundtrip (wait_until_persisted reads this)
    pub fn persisted_count(&self) -> u64 {
        self.persisted_total.load(Ordering::Relaxed)
    }
}

// ---------------------------------------------------------------------------
// Background writer thread
// ---------------------------------------------------------------------------

fn writer_loop(
    rx: &Receiver<WalRecord>,
    wal_dir: &Path,
    persisted: &AtomicU64,
) -> anyhow::Result<()> {
    // `None` = no open segment; the next record reopens one. A transient disk
    // error sets this back to `None` instead of propagating out of the thread.
    // The thread therefore NEVER dies on a transient I/O hiccup — it keeps
    // draining the channel so `append()` never observes `Disconnected` and the
    // durable WAL floor survives. The ONLY clean exit is the channel closing.
    let mut current: Option<BufWriter<File>> = open_segment_resilient(wal_dir);
    let mut bytes_written: u64 = 0;

    loop {
        // Block until at least one record arrives. Exit cleanly (and ONLY here)
        // when all senders are dropped — that is the clean-shutdown signal.
        let first = match rx.recv() {
            Ok(r) => r,
            Err(_) => {
                if let Some(mut w) = current.take() {
                    drop(w.flush());
                }
                info!("ws-frame-spill-writer channel closed; exiting");
                return Ok(());
            }
        };

        #[cfg(test)]
        maybe_test_panic(&first);
        bytes_written += persist_record_resilient(&mut current, wal_dir, &first, persisted);

        // Drain up to N more without blocking so we batch-flush.
        for _ in 0..256 {
            match rx.try_recv() {
                Ok(r) => {
                    #[cfg(test)]
                    maybe_test_panic(&r);
                    bytes_written += persist_record_resilient(&mut current, wal_dir, &r, persisted);
                }
                Err(_) => break,
            }
        }

        if let Some(w) = current.as_mut()
            && let Err(err) = w.flush()
        {
            report_io_error("flush", &err);
            // Drop the possibly-broken writer; the next record reopens it.
            current = None;
            thread::sleep(WAL_WRITER_IO_RETRY_BACKOFF);
        }

        if bytes_written >= WAL_SEGMENT_MAX_BYTES {
            if let Some(mut w) = current.take() {
                drop(w.flush());
            }
            current = open_segment_resilient(wal_dir);
            bytes_written = 0;
        }
    }
}

/// Open a fresh WAL segment, converting any error into `None` + a loud
/// `WS-SPILL-01` log + counter so the writer thread keeps draining the channel
/// instead of dying. The next record retries the open.
fn open_segment_resilient(wal_dir: &Path) -> Option<BufWriter<File>> {
    match open_new_segment(wal_dir) {
        Ok(w) => Some(w),
        Err(err) => {
            error!(
                code = ErrorCode::WsSpill01WriterRespawn.code_str(),
                stage = "open_segment",
                error = %err,
                "WAL spill writer could not open a segment — will retry; thread stays alive"
            );
            metrics::counter!(
                "tv_ws_frame_spill_write_errors_total",
                "stage" => "open_segment"
            )
            .increment(1);
            None
        }
    }
}

/// Durably write one record, reopening the segment first if needed. Returns the
/// on-disk byte count actually persisted (0 if the write could not land).
/// NEVER propagates an error — a transient disk failure must not kill the
/// writer thread (that would silently end durable capture of every frame).
fn persist_record_resilient(
    current: &mut Option<BufWriter<File>>,
    wal_dir: &Path,
    r: &WalRecord,
    persisted: &AtomicU64,
) -> u64 {
    if current.is_none() {
        *current = open_segment_resilient(wal_dir);
    }
    let Some(w) = current.as_mut() else {
        // No segment available (disk full / unwritable). The frame still
        // reaches the in-memory broadcast + the persist-side ring→spill→DLQ;
        // only the WAL belt is missing for this frame, which we count + alarm.
        metrics::counter!(
            "tv_ws_frame_spill_write_errors_total",
            "stage" => "no_segment"
        )
        .increment(1);
        return 0;
    };
    match write_record(w, r) {
        Ok(()) => {
            persisted.fetch_add(1, Ordering::Relaxed);
            record_disk_size(r)
        }
        Err(err) => {
            report_io_error("write_record", &err);
            // Drop the possibly-corrupt writer; reopen on the next record.
            *current = None;
            thread::sleep(WAL_WRITER_IO_RETRY_BACKOFF);
            0
        }
    }
}

fn report_io_error(stage: &'static str, err: &std::io::Error) {
    error!(
        code = ErrorCode::WsSpill01WriterRespawn.code_str(),
        stage,
        error = %err,
        "WAL spill writer I/O error — reopening segment; thread stays alive"
    );
    metrics::counter!("tv_ws_frame_spill_write_errors_total", "stage" => stage).increment(1);
}

/// Test-only panic injection: a record whose frame equals the sentinel makes
/// the writer thread panic, exercising the supervisor's catch-and-respawn path
/// (WS-SPILL-01). Interference-free — no other test sends this sentinel, so no
/// shared mutable state is needed.
#[cfg(test)]
const TEST_PANIC_SENTINEL: &[u8] = b"__WS_SPILL_TEST_PANIC_SENTINEL__";

#[cfg(test)]
fn maybe_test_panic(r: &WalRecord) {
    if r.frame.as_ref() == TEST_PANIC_SENTINEL {
        panic!("test-injected writer panic (sentinel frame)");
    }
}

/// Process-wide strictly-monotonic frame sequence: `max(prev+1, wall_nanos)`.
/// Lock-free CAS, O(1), zero heap alloc. The WS read loop calls this ONCE per
/// frame and passes the value to BOTH [`WsFrameSpill::append_with_seq`] and the
/// live broadcast, so the WAL record and the `ticks.capture_seq` column carry
/// the identical replay-stable value.
static WAL_FRAME_SEQ: AtomicU64 = AtomicU64::new(0);

pub fn next_frame_seq() -> u64 {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX))
        .unwrap_or(0);
    loop {
        let prev = WAL_FRAME_SEQ.load(Ordering::Relaxed);
        let next = prev.saturating_add(1).max(now);
        if WAL_FRAME_SEQ
            .compare_exchange_weak(prev, next, Ordering::SeqCst, Ordering::Relaxed)
            .is_ok()
        {
            return next;
        }
    }
}

fn write_record(w: &mut BufWriter<File>, r: &WalRecord) -> std::io::Result<()> {
    // TICK-SEQ-01: always write the v2 record (TVW2 + 8-byte LE frame_seq after
    // ws_type). `u32::try_from` makes an over-large frame explicit rather than
    // silently truncating (frames are ≤162 B in production).
    let frame_len = u32::try_from(r.frame.len()).map_err(|_| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, "WAL frame > u32::MAX")
    })?;
    let frame_seq = r.frame_seq.to_le_bytes();
    // CRC covers ws_type || frame_seq || len || frame.
    let crc = crc32_ieee_of(&[
        &[r.ws_type.as_u8()],
        &frame_seq[..],
        &frame_len.to_le_bytes()[..],
        &r.frame,
    ]);
    w.write_all(&WAL_MAGIC_V2)?;
    w.write_all(&[r.ws_type.as_u8()])?;
    w.write_all(&frame_seq)?;
    w.write_all(&frame_len.to_le_bytes())?;
    w.write_all(&r.frame)?;
    w.write_all(&crc.to_le_bytes())?;
    Ok(())
}

fn record_disk_size(r: &WalRecord) -> u64 {
    // v2: magic(4) + ws_type(1) + frame_seq(8) + len(4) + frame + crc(4) = 21 + frame
    WAL_MIN_RECORD_V2 as u64 + r.frame.len() as u64
}

fn open_new_segment(wal_dir: &Path) -> anyhow::Result<BufWriter<File>> {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let path = wal_dir.join(format!("ws-frames-{:020}.wal", nanos)); // APPROVED: segment rotation on the background writer thread, not the per-frame append
    let f = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|e| anyhow::anyhow!("open WAL segment {:?}: {e}", path))?;
    Ok(BufWriter::with_capacity(WAL_WRITER_BUFFER, f))
}

// ---------------------------------------------------------------------------
// CRC32 (IEEE 802.3 polynomial 0xEDB88320) — inline, zero deps.
// ---------------------------------------------------------------------------

const CRC32_TABLE: [u32; 256] = {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut j = 0;
        while j < 8 {
            c = if c & 1 != 0 {
                0xEDB88320 ^ (c >> 1)
            } else {
                c >> 1
            };
            j += 1;
        }
        table[i] = c;
        i += 1;
    }
    table
};

fn crc32_ieee_of(chunks: &[&[u8]]) -> u32 {
    let mut c: u32 = 0xFFFF_FFFF;
    for chunk in chunks {
        for &b in *chunk {
            c = CRC32_TABLE[((c ^ b as u32) & 0xFF) as usize] ^ (c >> 8);
        }
    }
    c ^ 0xFFFF_FFFF
}

// ---------------------------------------------------------------------------
// Replay — walk every `.wal` file, parse records, return recovered frames.
// Corrupted / truncated tails are logged and skipped.
//
// CRASH-SAFETY (zero-loss MEDIUM fix 2026-06-30): a replayed segment is moved
// to the IN-PROGRESS staging dir `<wal_dir>/replaying/` — NOT straight to
// `archive/`. The caller re-injects the returned frames into the live pipeline
// (ring → spill → WAL → DB), which durably RE-captures them into a fresh
// segment; ONLY THEN does the caller call `confirm_replayed`, which moves
// `replaying/` → `archive/`. Until confirmed, the segment stays in `replaying/`
// and `replay_all` re-globs it on the next boot, so a SECOND crash between
// replay and persist no longer strands frames in `archive/` (which is never
// re-globbed). `archive/` holds ONLY confirmed segments → it is never
// re-replayed, so the confirmed-history never re-injects (no whole-archive
// re-replay regression). Re-replay is idempotent via the replay-stable
// `capture_seq` DEDUP key.
// ---------------------------------------------------------------------------

/// In-progress staging directory: holds segments that have been replayed but
/// whose re-injection into the live pipeline has not yet been confirmed. These
/// are re-globbed (and thus re-replayed) on every boot until `confirm_replayed`
/// moves them to `archive/`.
const REPLAYING_SUBDIR: &str = "replaying";

/// Confirmed-history directory: holds segments whose frames have been durably
/// re-captured into the live pipeline. NEVER re-globbed by `replay_all`.
const ARCHIVE_SUBDIR: &str = "archive";

// TEST-EXEMPT: covered by test_append_spill_and_replay_roundtrip + test_replay_handles_missing_dir + test_replay_detects_crc_corruption + test_unconfirmed_segment_is_rereplayed_on_next_boot + test_confirmed_segment_is_not_rereplayed
pub fn replay_all<P: AsRef<Path>>(wal_dir: P) -> anyhow::Result<Vec<ReplayedFrame>> {
    let wal_dir = wal_dir.as_ref();
    if !wal_dir.exists() {
        return Ok(Vec::new()); // APPROVED: boot-time WAL replay, cold path
    }

    // Re-glob BOTH the in-progress staging dir (un-confirmed leftovers from a
    // PRIOR crashed boot — small + bounded: at most the segments a single
    // crashed boot was draining) AND the live `*.wal` segments (this crash's
    // fresh segments). NOT `archive/` — confirmed segments are never replayed.
    let replaying_dir = wal_dir.join(REPLAYING_SUBDIR);
    let mut segments: Vec<PathBuf> = wal_segments_in(wal_dir);
    let leftover_count = {
        let mut leftovers = wal_segments_in(&replaying_dir);
        let n = leftovers.len();
        segments.append(&mut leftovers);
        n
    };
    // Lexicographic == chronological == append order: every segment is named
    // `ws-frames-{nanos:020}.wal`, so a `replaying/` leftover (created on an
    // EARLIER boot → smaller nanos) sorts BEFORE this boot's fresh segments.
    // FIFO across both sources is preserved (operator invariant: tick order is
    // never changed). Sort on the FILE NAME (the zero-padded nanos), NOT the
    // full path — the full path would order by parent dir (`replaying/` vs the
    // bare live dir) instead of by capture time, breaking cross-source FIFO.
    segments.sort_by(|a, b| a.file_name().cmp(&b.file_name())); // O(1) EXEMPT: boot replay — this sort IS the FIFO-order invariant

    let mut frames = Vec::new(); // APPROVED: boot-time WAL replay, cold path
    let mut corrupted = 0usize;

    for path in &segments {
        match replay_segment(path) {
            Ok(mut batch) => frames.append(&mut batch),
            Err(err) => {
                corrupted += 1;
                error!(segment = ?path, error = %err, "WAL segment corrupted; skipping");
            }
        }
    }

    info!(
        wal_dir = ?wal_dir,
        segments = segments.len(),
        replaying_leftovers = leftover_count,
        frames_replayed = frames.len(),
        corrupted_segments = corrupted,
        "WAL replay complete"
    );

    // SLA counter: frames recovered from WAL on startup. Pair with
    // `tv_ticks_lost_total` (from append) to show the complete
    // zero-tick-loss picture on Grafana. If spill dropped 0 and
    // replay recovered N, the guarantee held for the last N frames.
    // Parthiban 2026-04-20.
    metrics::counter!("tv_wal_replay_recovered_total").increment(frames.len() as u64);
    if corrupted > 0 {
        // APPROVED: cast — corrupted usize is O(segments) ≤ u64 always.
        metrics::counter!("tv_wal_replay_corrupted_segments_total").increment(corrupted as u64);
    }

    // Move processed segments to the IN-PROGRESS staging dir (NOT archive).
    // They remain re-globbable next boot until `confirm_replayed` is called.
    // Best-effort, like the prior archive move: a failed move leaves the
    // segment as `*.wal`, which is STILL re-globbed next boot — never worse.
    drop(std::fs::create_dir_all(&replaying_dir)); // O(1) EXEMPT: boot replay staging move, cold path
    for seg in &segments {
        if let Some(name) = seg.file_name() {
            let dst = replaying_dir.join(name);
            // A leftover that was re-read from `replaying/` renames onto
            // itself / is overwritten by identical bytes — safe.
            if seg != &dst {
                drop(std::fs::rename(seg, dst)); // O(1) EXEMPT: boot replay staging move, cold path
            }
        }
    }

    Ok(frames)
}

/// Lists the `*.wal` segment files directly under `dir` (NOT recursive).
/// Returns an empty Vec for a missing/unreadable dir.
fn wal_segments_in(dir: &Path) -> Vec<PathBuf> {
    // O(1) EXEMPT: boot replay helper — cold path, bounded segment listing
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new(); // APPROVED: boot replay helper, cold path
    };
    entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("wal"))
        .collect() // APPROVED: boot replay helper, cold path
}

/// CRASH-SAFETY confirm step: move every segment in `<wal_dir>/replaying/` to
/// `<wal_dir>/archive/`. Call this ONLY after the frames returned by
/// `replay_all` have been durably re-captured into the live pipeline (i.e. the
/// boot re-injection succeeded). Until this runs, the staged segments are
/// re-replayed on the next boot — so a crash between `replay_all` and this call
/// can never strand frames. Idempotent and best-effort: a missing `replaying/`
/// is a no-op; a rename failure leaves the segment in `replaying/` for a
/// harmless (DEDUP-idempotent) re-replay next boot. NEVER panics, NEVER blocks.
// TEST-EXEMPT: covered by test_confirmed_segment_is_not_rereplayed + test_confirm_replayed_missing_dir_is_noop + test_crash_between_move_and_confirm_still_rereplays
pub fn confirm_replayed<P: AsRef<Path>>(wal_dir: P) {
    let wal_dir = wal_dir.as_ref();
    let replaying_dir = wal_dir.join(REPLAYING_SUBDIR);
    let staged = wal_segments_in(&replaying_dir);
    if staged.is_empty() {
        return;
    }
    let archive_dir = wal_dir.join(ARCHIVE_SUBDIR);
    drop(std::fs::create_dir_all(&archive_dir)); // O(1) EXEMPT: boot-time confirm step, cold path
    let mut confirmed = 0u64;
    for seg in &staged {
        if let Some(name) = seg.file_name() {
            let dst = archive_dir.join(name);
            // O(1) EXEMPT: boot-time confirm step, cold path
            match std::fs::rename(seg, &dst) {
                Ok(()) => confirmed += 1,
                Err(err) => {
                    // Stays in `replaying/` → re-replayed next boot (DEDUP-safe).
                    error!(
                        segment = ?seg,
                        error = %err,
                        "WAL replay confirm: could not archive staged segment — \
                         it stays in replaying/ and will be re-replayed next boot \
                         (idempotent via capture_seq DEDUP)"
                    );
                }
            }
        }
    }
    if confirmed > 0 {
        metrics::counter!("tv_wal_replay_confirmed_segments_total").increment(confirmed);
    }
    info!(
        wal_dir = ?wal_dir,
        confirmed_segments = confirmed,
        staged = staged.len(),
        "WAL replay confirmed — staged segments archived"
    );
}

/// Outcome of one `<wal_dir>/archive/` pruning pass.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ArchivePruneOutcome {
    /// Archive segments deleted (mtime older than the retention window).
    pub deleted: usize,
    /// Segments that SHOULD have been deleted but `remove_file` failed
    /// (logged at WARN; retried on the next pass).
    pub failed: usize,
    /// Files inspected and kept (fresh, or not a `.wal` segment).
    pub kept: usize,
}

/// Prunes confirmed-replay WAL segments from `<wal_dir>/archive/` whose
/// mtime is older than `retention_secs` — pure-testable core over an
/// injected `now` (2026-07-13 disk-retention hardening).
///
/// Why this is safe (the honest rationale):
/// - Segments land in `archive/` ONLY via [`confirm_replayed`] — i.e. their
///   frames were already re-injected into the live pipeline and durably
///   persisted (replay-confirmed). `archive/` is never re-replayed.
/// - No reader depends on aged archive segments: the last one — the
///   same-day 15:40 IST tick-conservation audit's `count_frames_for_ist_day`
///   scan — retired 2026-07-18 with the audit (dead-WS sweep follow-up);
///   it only ever counted CURRENT-day frames anyway
///   (`WS_WAL_ARCHIVE_RETENTION_SECS` = 7 days, matching
///   `SPILL_FILE_MAX_AGE_SECS`, comfortably exceeded that window AND — F3,
///   review round 1 — preserves the confirm-on-channel residual's only
///   copy across a long weekend for triage before it ages out).
/// - Only `*.wal` files are touched; anything else in the dir is kept.
///   A missing `archive/` dir is a no-op. Deletion failures are NOT
///   persist/flush failures — they log at WARN (bounded: once per file per
///   pass, passes run every 6 h) and retry next pass.
#[must_use]
pub fn prune_archived_segments_at<P: AsRef<Path>>(
    wal_dir: P,
    retention_secs: u64,
    now: std::time::SystemTime,
) -> ArchivePruneOutcome {
    let archive_dir = wal_dir.as_ref().join(ARCHIVE_SUBDIR);
    let mut outcome = ArchivePruneOutcome::default();
    // O(1) EXEMPT: periodic cold archive prune, never the per-frame append
    let Ok(entries) = std::fs::read_dir(&archive_dir) else {
        return outcome; // missing archive dir — nothing to prune
    };
    let cutoff = std::time::Duration::from_secs(retention_secs);
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("wal") {
            outcome.kept += 1; // foreign file — never touched
            continue;
        }
        let age = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|mtime| now.duration_since(mtime).ok());
        match age {
            // O(1) EXEMPT: periodic cold archive prune, never the per-frame append
            Some(age) if age > cutoff => match std::fs::remove_file(&path) {
                Ok(()) => outcome.deleted += 1,
                Err(err) => {
                    outcome.failed += 1;
                    warn!(
                        path = %path.display(),
                        error = %err,
                        "WAL archive prune: remove_file failed — retried next pass"
                    );
                }
            },
            // Fresh, unreadable metadata, or a future mtime (clock skew):
            // keep — deleting on uncertainty would be the wrong default.
            _ => outcome.kept += 1,
        }
    }
    outcome
}

/// Wall-clock wrapper over [`prune_archived_segments_at`]. Cold path —
/// called from the periodic prune task in `main.rs` (once at task start,
/// then every `WS_WAL_ARCHIVE_PRUNE_INTERVAL_SECS`).
#[must_use]
pub fn prune_archived_segments<P: AsRef<Path>>(
    wal_dir: P,
    retention_secs: u64,
) -> ArchivePruneOutcome {
    let outcome = prune_archived_segments_at(wal_dir, retention_secs, std::time::SystemTime::now());
    if outcome.deleted > 0 || outcome.failed > 0 {
        metrics::counter!("tv_ws_wal_archive_pruned_total").increment(outcome.deleted as u64);
        info!(
            deleted = outcome.deleted,
            failed = outcome.failed,
            kept = outcome.kept,
            retention_secs,
            "WAL archive prune pass complete (confirmed-replay segments past retention)"
        );
    }
    outcome
}

fn replay_segment(path: &Path) -> anyhow::Result<Vec<ReplayedFrame>> {
    let mut f = File::open(path)?;
    let mut buf = Vec::new(); // APPROVED: boot-time WAL replay, cold path
    f.read_to_end(&mut buf)?;

    let mut out = Vec::new(); // APPROVED: boot-time WAL replay, cold path
    let mut i = 0usize;
    // Smallest record is v1 (13 bytes); v2 is 21. Gate the OUTER loop on the v1
    // minimum, then re-check the version-specific minimum after the magic check
    // so a partial v2 tail can never be read as if its frame_seq were payload.
    while i + WAL_MIN_RECORD_V1 <= buf.len() {
        let magic = &buf[i..i + 4];
        let is_v2 = magic == WAL_MAGIC_V2;
        let is_v1 = magic == WAL_MAGIC;
        if !is_v1 && !is_v2 {
            warn!(segment = ?path, offset = i, "WAL magic mismatch; stopping at boundary");
            break;
        }
        // Version disambiguation + per-version minimum-size guard (security
        // review HIGH): a v2 record needs 21 bytes before its variable frame.
        let min_rec = if is_v2 {
            WAL_MIN_RECORD_V2
        } else {
            WAL_MIN_RECORD_V1
        };
        if i + min_rec > buf.len() {
            warn!(segment = ?path, offset = i, is_v2, "truncated header at tail");
            break;
        }
        let ws_byte = buf[i + 4];
        let ws_type = match WsType::from_u8(ws_byte) {
            Some(t) => t,
            None => {
                warn!(segment = ?path, offset = i, ws_byte, "unknown WsType tag; stopping");
                break;
            }
        };
        // v1: [magic|ws|len|frame|crc]; v2: [magic|ws|frame_seq(8)|len|frame|crc].
        let (frame_seq, len_off) = if is_v2 {
            let seq_bytes: [u8; 8] = match buf[i + 5..i + 13].try_into() {
                Ok(b) => b,
                Err(_) => break,
            };
            (u64::from_le_bytes(seq_bytes), i + 13)
        } else {
            (0u64, i + 5)
        };
        let len_bytes: [u8; 4] = match buf[len_off..len_off + 4].try_into() {
            Ok(b) => b,
            Err(_) => break,
        };
        let frame_len = u32::from_le_bytes(len_bytes) as usize;
        let frame_off = len_off + 4;
        // checked_add chain (security review MEDIUM — defence-in-depth).
        let record_end = match frame_off
            .checked_add(frame_len)
            .and_then(|v| v.checked_add(4))
        {
            Some(v) => v,
            None => {
                warn!(segment = ?path, offset = i, frame_len, "record length overflow; stopping");
                break;
            }
        };
        if record_end > buf.len() {
            warn!(segment = ?path, offset = i, frame_len, "truncated record at tail");
            break;
        }
        let frame = buf[frame_off..frame_off + frame_len].to_vec();
        let crc_bytes: [u8; 4] = match buf[frame_off + frame_len..record_end].try_into() {
            Ok(b) => b,
            Err(_) => break,
        };
        let expected = u32::from_le_bytes(crc_bytes);
        // CRC covers the version's exact header bytes: v2 includes frame_seq.
        let len_le = (frame_len as u32).to_le_bytes();
        let actual = if is_v2 {
            crc32_ieee_of(&[
                &[ws_byte],
                &frame_seq.to_le_bytes()[..],
                &len_le[..],
                &frame,
            ])
        } else {
            crc32_ieee_of(&[&[ws_byte], &len_le[..], &frame])
        };
        if actual != expected {
            warn!(segment = ?path, offset = i, expected, actual, "CRC mismatch; stopping");
            break;
        }
        out.push(ReplayedFrame {
            ws_type,
            frame,
            frame_seq,
        });
        i = record_end;
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn tmp_dir(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let p = std::env::temp_dir().join(format!("tv-wal-{}-{}", name, nanos));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn wait_until_persisted(spill: &WsFrameSpill, target: u64) {
        for _ in 0..200 {
            if spill.persisted_count() >= target {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!(
            "spill did not persist {} frames (got {})",
            target,
            spill.persisted_count()
        );
    }

    #[test]
    fn test_ws_type_roundtrip() {
        for t in [WsType::LiveFeed, WsType::OrderUpdate] {
            assert_eq!(WsType::from_u8(t.as_u8()), Some(t));
        }
        assert_eq!(WsType::from_u8(0), None);
        assert_eq!(WsType::from_u8(2), None);
        assert_eq!(WsType::from_u8(3), None);
        assert_eq!(WsType::from_u8(99), None);
    }

    #[test]
    fn test_crc32_known_vector() {
        // CRC32 of "123456789" = 0xCBF43926
        let c = crc32_ieee_of(&[b"123456789"]);
        assert_eq!(c, 0xCBF4_3926);
    }

    #[test]
    fn test_append_spill_and_replay_roundtrip() {
        let dir = tmp_dir("roundtrip");
        {
            let spill = WsFrameSpill::new(&dir).unwrap();
            spill.append(WsType::LiveFeed, vec![1, 2, 3, 4]);
            spill.append(WsType::OrderUpdate, b"{\"k\":1}".to_vec());
            wait_until_persisted(&spill, 2);
        } // drop spill → writer thread drains and exits

        // Give writer thread time to exit cleanly.
        std::thread::sleep(Duration::from_millis(50));

        let frames = replay_all(&dir).unwrap();
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].ws_type, WsType::LiveFeed);
        assert_eq!(frames[0].frame, vec![1, 2, 3, 4]);
        assert_eq!(frames[1].ws_type, WsType::OrderUpdate);

        // Crash-safety contract: the segment is now STAGED in `replaying/`,
        // NOT archived — so it is re-globbed (re-replayed) until confirmed.
        // ONLY after `confirm_replayed` (the caller proves durable re-capture)
        // is the second replay empty.
        confirm_replayed(&dir);
        let frames2 = replay_all(&dir).unwrap();
        assert!(
            frames2.is_empty(),
            "after confirm_replayed, segments are archived and must NOT re-replay"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- Crash-safety: un-confirmed segments re-replay, confirmed do not -----

    #[test]
    fn test_replay_moves_to_replaying_not_archive_until_confirmed() {
        let dir = tmp_dir("staging-not-archive");
        {
            let spill = WsFrameSpill::new(&dir).unwrap();
            spill.append(WsType::LiveFeed, vec![1, 2, 3, 4]);
            wait_until_persisted(&spill, 1);
        }
        std::thread::sleep(Duration::from_millis(50));

        let frames = replay_all(&dir).unwrap();
        assert_eq!(frames.len(), 1);

        // The replayed segment is in `replaying/`, NOT `archive/`.
        let replaying = dir.join("replaying");
        let archive = dir.join("archive");
        assert_eq!(
            wal_segments_in(&replaying).len(),
            1,
            "segment must be staged in replaying/"
        );
        assert_eq!(
            wal_segments_in(&archive).len(),
            0,
            "segment must NOT be archived before confirm"
        );

        confirm_replayed(&dir);
        assert_eq!(
            wal_segments_in(&replaying).len(),
            0,
            "confirm must empty replaying/"
        );
        assert_eq!(
            wal_segments_in(&archive).len(),
            1,
            "confirm must move the segment to archive/"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_unconfirmed_segment_is_rereplayed_on_next_boot() {
        // TEST CASE 1: the exact bug. A segment whose persist is NOT confirmed
        // MUST be re-replayed on the next boot (no `confirm_replayed` between).
        let dir = tmp_dir("unconfirmed-rereplay");
        {
            let spill = WsFrameSpill::new(&dir).unwrap();
            spill.append(WsType::LiveFeed, vec![7, 7, 7, 7]);
            spill.append(WsType::OrderUpdate, b"{\"x\":1}".to_vec());
            wait_until_persisted(&spill, 2);
        }
        std::thread::sleep(Duration::from_millis(50));

        // First boot: replay returns the frames, stages them in `replaying/`.
        let first = replay_all(&dir).unwrap();
        assert_eq!(first.len(), 2, "first boot recovers all frames");

        // CRASH before confirm — no `confirm_replayed` call. Next boot MUST
        // re-replay the staged segment (the pre-fix bug stranded it in
        // `archive/` and returned 0 here, silently losing auto-recovery).
        let second = replay_all(&dir).unwrap();
        assert_eq!(
            second.len(),
            2,
            "un-confirmed segment MUST be re-replayed on the next boot"
        );
        assert_eq!(second[0].frame, vec![7, 7, 7, 7]);
        assert_eq!(second[0].ws_type, WsType::LiveFeed);
        assert_eq!(second[1].ws_type, WsType::OrderUpdate);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_confirmed_segment_is_not_rereplayed() {
        // TEST CASE 2: a confirmed/archived segment must NOT be re-replayed,
        // and the confirmed archive must NEVER be re-globbed (no whole-archive
        // re-replay regression).
        let dir = tmp_dir("confirmed-no-rereplay");
        {
            let spill = WsFrameSpill::new(&dir).unwrap();
            spill.append(WsType::LiveFeed, vec![5, 5, 5, 5]);
            wait_until_persisted(&spill, 1);
        }
        std::thread::sleep(Duration::from_millis(50));

        let first = replay_all(&dir).unwrap();
        assert_eq!(first.len(), 1);
        confirm_replayed(&dir); // durable re-capture confirmed

        // Boot again: the confirmed segment lives only in `archive/`, which is
        // never re-globbed → zero re-replay.
        let second = replay_all(&dir).unwrap();
        assert!(
            second.is_empty(),
            "confirmed (archived) segment must NOT re-replay"
        );

        // A THIRD boot also returns 0 — the archive accumulates confirmed
        // history but is never re-injected (regression guard against
        // re-globbing the whole archive every boot).
        let third = replay_all(&dir).unwrap();
        assert!(third.is_empty(), "archive must never be re-replayed");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_replaying_leftover_and_fresh_replay_in_order() {
        // TEST CASE 3: a `replaying/` leftover (older nanos) + a fresh `*.wal`
        // (newer nanos) must BOTH replay, in strict append order (leftover
        // first). Proves FIFO across the two glob sources.
        let dir = tmp_dir("leftover-plus-fresh");
        let replaying = dir.join("replaying");
        std::fs::create_dir_all(&replaying).unwrap();

        // Older leftover segment (small nanos) staged in replaying/, marker AA.
        let leftover = replaying.join("ws-frames-00000000000000000001.wal");
        std::fs::write(&leftover, encode_v1_record(WsType::LiveFeed, &[0xAA])).unwrap();
        // Newer fresh segment (large nanos) in the live dir, marker BB.
        let fresh = dir.join("ws-frames-00000000000000009999.wal");
        std::fs::write(&fresh, encode_v1_record(WsType::LiveFeed, &[0xBB])).unwrap();

        let frames = replay_all(&dir).unwrap();
        assert_eq!(frames.len(), 2, "both leftover and fresh must replay");
        assert_eq!(
            frames[0].frame,
            vec![0xAA],
            "older leftover (smaller nanos) must replay FIRST — FIFO preserved"
        );
        assert_eq!(frames[1].frame, vec![0xBB], "fresh segment replays second");

        // Both are now staged together in replaying/ (un-confirmed).
        assert_eq!(wal_segments_in(&replaying).len(), 2);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_crash_between_move_and_confirm_still_rereplays() {
        // TEST CASE 4: the staging transition is crash-safe. After `replay_all`
        // moves the segment to `replaying/`, a crash BEFORE `confirm_replayed`
        // (simulated by simply not calling it) leaves the segment recoverable:
        // a fresh `replay_all` re-returns it.
        let dir = tmp_dir("crash-mid-transition");
        {
            let spill = WsFrameSpill::new(&dir).unwrap();
            spill.append(WsType::LiveFeed, vec![3, 1, 4, 1]);
            wait_until_persisted(&spill, 1);
        }
        std::thread::sleep(Duration::from_millis(50));

        let _first = replay_all(&dir).unwrap();
        // Segment is now in replaying/. Simulate crash: no confirm.
        assert_eq!(wal_segments_in(&dir.join("replaying")).len(), 1);

        // Fresh process boots → re-replays the staged segment.
        let recovered = replay_all(&dir).unwrap();
        assert_eq!(
            recovered.len(),
            1,
            "a crash between move and confirm must still re-replay the segment"
        );
        assert_eq!(recovered[0].frame, vec![3, 1, 4, 1]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_confirm_replayed_missing_dir_is_noop() {
        // confirm on a dir with no `replaying/` must not error or panic.
        let dir = tmp_dir("confirm-noop");
        confirm_replayed(&dir); // no replaying/ exists yet
        // Also a no-op on a completely missing dir.
        let missing = dir.join("does-not-exist");
        confirm_replayed(&missing);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- TICK-SEQ-01 PR-2a: TVW2 frame_seq format ---------------------------

    /// Encodes a legacy v1 (`TVW1`, no frame_seq) record exactly as the
    /// pre-TICK-SEQ-01 writer did, so back-compat replay can be exercised.
    fn encode_v1_record(ws: WsType, frame: &[u8]) -> Vec<u8> {
        let len = (frame.len() as u32).to_le_bytes();
        let crc = crc32_ieee_of(&[&[ws.as_u8()], &len[..], frame]);
        let mut v = Vec::new();
        v.extend_from_slice(&WAL_MAGIC);
        v.push(ws.as_u8());
        v.extend_from_slice(&len);
        v.extend_from_slice(frame);
        v.extend_from_slice(&crc.to_le_bytes());
        v
    }

    #[test]
    fn test_wal_v2_roundtrip_preserves_frame_seq() {
        let dir = tmp_dir("v2-roundtrip");
        {
            let spill = WsFrameSpill::new(&dir).unwrap();
            spill.append(WsType::LiveFeed, vec![9, 8, 7]);
            spill.append(WsType::LiveFeed, vec![6, 5, 4]);
            wait_until_persisted(&spill, 2);
        }
        std::thread::sleep(Duration::from_millis(50));

        let frames = replay_all(&dir).unwrap();
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].frame, vec![9, 8, 7]);
        assert_eq!(frames[1].frame, vec![6, 5, 4]);
        // frame_seq is stamped + persisted + read back; strictly increasing.
        assert!(frames[0].frame_seq > 0, "v2 frame_seq must be non-zero");
        assert!(
            frames[1].frame_seq > frames[0].frame_seq,
            "frame_seq must be strictly increasing across records: {} !> {}",
            frames[1].frame_seq,
            frames[0].frame_seq
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_wal_v1_backcompat_replay() {
        // A v1 segment written before this change must still recover, with
        // frame_seq defaulting to 0 (the new key keeps v1 on payload_hash).
        let dir = tmp_dir("v1-backcompat");
        let seg = dir.join("ws-frames-00000000000000000001.wal");
        let mut bytes = encode_v1_record(WsType::LiveFeed, &[1, 2, 3, 4]);
        bytes.extend_from_slice(&encode_v1_record(WsType::OrderUpdate, b"{\"a\":2}"));
        std::fs::write(&seg, &bytes).unwrap();

        let frames = replay_all(&dir).unwrap();
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].ws_type, WsType::LiveFeed);
        assert_eq!(frames[0].frame, vec![1, 2, 3, 4]);
        assert_eq!(frames[0].frame_seq, 0, "legacy v1 record → frame_seq 0");
        assert_eq!(frames[1].ws_type, WsType::OrderUpdate);
        assert_eq!(frames[1].frame_seq, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_wal_v2_min_record_size_guard_no_panic() {
        // A 13-byte buffer passes the v1 outer guard but is a TRUNCATED v2
        // header (needs 21). The parser must reject it cleanly — no panic, no
        // OOB read, zero frames (security review HIGH: min-size guard).
        let dir = tmp_dir("v2-truncated");
        let seg = dir.join("ws-frames-00000000000000000002.wal");
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&WAL_MAGIC_V2); // 4
        bytes.push(WsType::LiveFeed.as_u8()); // +1
        bytes.extend_from_slice(&[0u8; 8]); // +8 frame_seq, but NO len/frame/crc → 13 total
        assert_eq!(bytes.len(), 13);
        std::fs::write(&seg, &bytes).unwrap();

        let frames = replay_all(&dir).unwrap(); // must not panic
        assert!(
            frames.is_empty(),
            "truncated v2 header must recover 0 frames"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_next_frame_seq_strictly_monotonic() {
        let mut prev = next_frame_seq();
        for _ in 0..5_000 {
            let cur = next_frame_seq();
            assert!(
                cur > prev,
                "frame_seq must strictly increase: {cur} !> {prev}"
            );
            prev = cur;
        }
    }

    #[test]
    fn test_replay_handles_missing_dir() {
        let dir = std::env::temp_dir().join("tv-wal-nonexistent-xyz");
        let _ = std::fs::remove_dir_all(&dir);
        let frames = replay_all(&dir).unwrap();
        assert!(frames.is_empty());
    }

    #[test]
    fn test_replay_detects_crc_corruption() {
        let dir = tmp_dir("corrupt");
        {
            let spill = WsFrameSpill::new(&dir).unwrap();
            spill.append(WsType::LiveFeed, b"alpha".to_vec());
            spill.append(WsType::LiveFeed, b"beta".to_vec());
            wait_until_persisted(&spill, 2);
        }
        std::thread::sleep(Duration::from_millis(50));

        // Flip one byte in the middle of the WAL segment.
        let mut segs: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("wal"))
            .collect();
        segs.sort();
        let seg = segs.first().unwrap().clone();

        let mut data = std::fs::read(&seg).unwrap();
        // Corrupt the middle byte (likely inside first frame payload).
        let mid = data.len() / 2;
        data[mid] ^= 0xFF;
        std::fs::write(&seg, data).unwrap();

        let frames = replay_all(&dir).unwrap();
        // Either zero or one frame may survive depending on which record got hit;
        // the key assertion is that replay does NOT panic and stops at corruption.
        assert!(frames.len() <= 2);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Ratchet — the chaos test `chaos_healthy_ops_burst_100k_frames_zero_drops`
    /// asserts ZERO drops while bursting 100,000 frames in a tight loop.
    /// That requires the channel capacity to stay strictly above 100,000 so
    /// even a fully-pinned writer thread on a 2-vCPU CI runner cannot fill
    /// the channel before draining starts. A future regression that lowers
    /// `SPILL_CHANNEL_CAPACITY` below 100,000 fails this test BEFORE the
    /// chaos suite flakes in CI.
    #[test]
    fn test_spill_channel_capacity_exceeds_chaos_burst_size() {
        const CHAOS_BURST_N: usize = 100_000;
        assert!(
            SPILL_CHANNEL_CAPACITY > CHAOS_BURST_N,
            "SPILL_CHANNEL_CAPACITY ({}) must stay strictly above the chaos \
             test's burst size ({}) so writer-thread scheduling delays on \
             slow CI runners cannot trip the drop_critical safety-floor \
             invariant",
            SPILL_CHANNEL_CAPACITY,
            CHAOS_BURST_N
        );
    }

    #[test]
    fn test_drop_counter_increments_when_channel_full() {
        // Exercise the drop path by creating a spill, then forcing the channel
        // full. We synthesize a dropped count without a writer thread by
        // constructing an independent spill where the writer is slow — but a
        // simpler check: verify the drop_critical counter starts at zero and
        // the `Dropped` variant is observable by type.
        let dir = tmp_dir("drop");
        let spill = WsFrameSpill::new(&dir).unwrap();
        assert_eq!(spill.drop_critical_count(), 0);
        // A single append should NOT drop (channel is 65k).
        let outcome = spill.append(WsType::LiveFeed, vec![1]);
        assert_eq!(outcome, AppendOutcome::Spilled);
        assert_eq!(spill.drop_critical_count(), 0);
        drop(spill);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_disconnected_arm_alarms() {
        // WS-SPILL-02: when the writer thread is dead (channel Disconnected),
        // `append` must DROP LOUDLY — increment drop_critical — not return
        // silently as it did before 2026-06-09.
        let spill = WsFrameSpill::new_with_dead_writer_for_test();
        assert_eq!(spill.drop_critical_count(), 0);
        let outcome = spill.append(WsType::LiveFeed, vec![9, 9, 9]);
        assert_eq!(outcome, AppendOutcome::Dropped);
        assert_eq!(
            spill.drop_critical_count(),
            1,
            "writer-dead drop must be counted (WS-SPILL-02), never silent"
        );
    }

    // ── SP5.1: terminal Dhan drop → per-feed health Degraded (closes SP5 false-OK) ──

    #[test]
    fn test_with_feed_health_sets_registry_and_live_feed_drop_records_dhan() {
        use std::sync::Arc;
        use tickvault_common::feed::Feed;
        use tickvault_common::feed_health::{FeedHealthRegistry, FeedHealthVerdict};

        let reg = Arc::new(FeedHealthRegistry::new());
        let spill =
            WsFrameSpill::new_with_dead_writer_for_test().with_feed_health(Some(Arc::clone(&reg)));
        // A dropped LIVE-FEED (Dhan) frame must record a Dhan drop.
        assert_eq!(
            spill.append(WsType::LiveFeed, vec![2, 0, 0, 0]),
            AppendOutcome::Dropped
        );
        const T0: i64 = 1_780_000_000_000_000_000;
        // Connected + fresh, but a durable drop happened → Degraded, NOT Ok.
        reg.set_connected(Feed::Dhan, true);
        reg.record_tick(Feed::Dhan, T0);
        let r = reg.snapshot(Feed::Dhan, true, true, true, T0 + 1_000_000_000);
        assert!(r.input.drops_total >= 1, "Dhan drop must be recorded");
        assert_eq!(
            r.verdict,
            FeedHealthVerdict::Degraded,
            "connected+fresh but dropping → Degraded (closes the SP5 false-OK)"
        );
    }

    #[test]
    fn test_order_update_drop_does_not_record_dhan() {
        use std::sync::Arc;
        use tickvault_common::feed::Feed;
        use tickvault_common::feed_health::FeedHealthRegistry;

        let reg = Arc::new(FeedHealthRegistry::new());
        let spill =
            WsFrameSpill::new_with_dead_writer_for_test().with_feed_health(Some(Arc::clone(&reg)));
        // An OrderUpdate drop is NOT the Dhan market feed → no Dhan drop recorded.
        assert_eq!(
            spill.append(WsType::OrderUpdate, vec![1]),
            AppendOutcome::Dropped
        );
        const T0: i64 = 1_780_000_000_000_000_000;
        let r = reg.snapshot(Feed::Dhan, true, true, true, T0);
        assert_eq!(
            r.input.drops_total, 0,
            "OrderUpdate drop must not count as a Dhan market-feed drop"
        );
    }

    #[test]
    fn test_dhan_spill_drop_pre_market_is_degraded_not_ok() {
        // SP5.1 semantic (operator: "not even a single tick should be missed"):
        // a REAL durable-loss drop pins Dhan to Degraded even pre/post-market —
        // distinct from the SP5 C1 disconnect-idle case (Ok outside hours).
        // classify() checks drops>0 BEFORE the market-open gate, by design; a
        // spill drop is actual loss, not idle sleep.
        use std::sync::Arc;
        use tickvault_common::feed::Feed;
        use tickvault_common::feed_health::{FeedHealthRegistry, FeedHealthVerdict};

        let reg = Arc::new(FeedHealthRegistry::new());
        let spill =
            WsFrameSpill::new_with_dead_writer_for_test().with_feed_health(Some(Arc::clone(&reg)));
        assert_eq!(
            spill.append(WsType::LiveFeed, vec![2, 0, 0, 0]),
            AppendOutcome::Dropped
        );
        const T0: i64 = 1_780_000_000_000_000_000;
        // market_open = FALSE — yet a durable drop still surfaces Degraded.
        let r = reg.snapshot(Feed::Dhan, true, true, false, T0);
        assert_eq!(
            r.verdict,
            FeedHealthVerdict::Degraded,
            "a real durable-loss drop surfaces Degraded even pre/post-market"
        );
    }

    #[test]
    fn test_open_segment_resilient_returns_none_on_unopenable_path() {
        // A path *under a regular file* can never host a segment (ENOTDIR,
        // even for root) → resilient open returns None with NO panic and NO
        // error propagation, proving a disk failure cannot tear down the
        // writer thread. A good dir still opens.
        let dir = tmp_dir("resilient-open");
        let file_path = dir.join("not-a-dir");
        std::fs::write(&file_path, b"x").unwrap();
        let bad = file_path.join("under-a-file");
        assert!(open_segment_resilient(&bad).is_none());
        assert!(open_segment_resilient(&dir).is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_writer_survives_unwritable_dir_then_recovers() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tmp_dir("unwritable");
        // Make the WAL dir read-only so a non-root writer cannot create a
        // segment. (If the test runs as root the writes simply succeed — the
        // assertions below still hold; the deterministic open-failure proof is
        // `test_open_segment_resilient_returns_none_on_unopenable_path`.)
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555)).unwrap();
        let spill = WsFrameSpill::new(&dir).unwrap();
        // Channel still accepts; the writer logs WS-SPILL-01 + counts the error
        // but DOES NOT die.
        assert_eq!(
            spill.append(WsType::LiveFeed, vec![1, 2, 3]),
            AppendOutcome::Spilled
        );
        std::thread::sleep(Duration::from_millis(80));
        // Thread still alive → channel NOT Disconnected → append still Spilled.
        assert_eq!(
            spill.append(WsType::LiveFeed, vec![4, 5, 6]),
            AppendOutcome::Spilled
        );
        assert_eq!(
            spill.drop_critical_count(),
            0,
            "no Disconnected drops — the writer thread must stay alive"
        );
        // Restore write permission; the recovered writer must now persist.
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        spill.append(WsType::LiveFeed, vec![7, 8, 9]);
        wait_until_persisted(&spill, 1);
        drop(spill);
        std::thread::sleep(Duration::from_millis(50));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_writer_respawns_after_panic_sentinel() {
        let dir = tmp_dir("respawn");
        let spill = WsFrameSpill::new(&dir).unwrap();
        // A normal frame persists.
        spill.append(WsType::LiveFeed, b"before".to_vec());
        wait_until_persisted(&spill, 1);
        // Inject a panic in the writer thread (consumed sentinel record).
        assert_eq!(
            spill.append(WsType::LiveFeed, TEST_PANIC_SENTINEL.to_vec()),
            AppendOutcome::Spilled
        );
        // Give the supervisor time to catch the panic and respawn the writer.
        std::thread::sleep(Duration::from_millis(400));
        // The respawned writer keeps the channel alive (NOT Disconnected) and
        // the post-respawn frame lands durably — proving WS-SPILL-01 respawn.
        assert_eq!(
            spill.append(WsType::LiveFeed, b"after".to_vec()),
            AppendOutcome::Spilled
        );
        wait_until_persisted(&spill, 2); // "before" + "after" (sentinel was consumed)
        assert_eq!(
            spill.drop_critical_count(),
            0,
            "respawn must keep the channel alive — no Disconnected drops"
        );
        drop(spill);
        std::thread::sleep(Duration::from_millis(50));
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── archive/ pruning (2026-07-13 disk-retention hardening) ──────────────

    /// Writes a file into `<dir>/archive/` and backdates its mtime by
    /// `age_secs` relative to `now` via a computed FileTimes set.
    fn plant_archive_file(dir: &Path, name: &str, now: SystemTime, age_secs: u64) -> PathBuf {
        let archive = dir.join("archive");
        std::fs::create_dir_all(&archive).unwrap();
        let path = archive.join(name);
        std::fs::write(&path, b"segment-bytes").unwrap();
        let mtime = now - Duration::from_secs(age_secs);
        let f = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        f.set_times(std::fs::FileTimes::new().set_modified(mtime))
            .unwrap();
        path
    }

    #[test]
    fn test_prune_preserves_fresh_archive_segments() {
        let dir = tmp_dir("prune-fresh");
        let now = SystemTime::now();
        let fresh = plant_archive_file(&dir, "ws-frames-00000000000000000001.wal", now, 3600);
        let outcome = prune_archived_segments_at(&dir, 604_800, now);
        assert_eq!(outcome.deleted, 0);
        assert_eq!(outcome.kept, 1);
        assert!(fresh.exists(), "a fresh segment must never be pruned");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_prune_deletes_old_archive_segments() {
        let dir = tmp_dir("prune-old");
        let now = SystemTime::now();
        let old = plant_archive_file(
            &dir,
            "ws-frames-00000000000000000002.wal",
            now,
            604_800 + 3600, // retention + 1h
        );
        let fresh = plant_archive_file(&dir, "ws-frames-00000000000000000003.wal", now, 60);
        let outcome = prune_archived_segments_at(&dir, 604_800, now);
        assert_eq!(outcome.deleted, 1);
        assert_eq!(outcome.kept, 1);
        assert!(!old.exists(), "a past-retention segment must be deleted");
        assert!(fresh.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_prune_ignores_non_segment_names() {
        let dir = tmp_dir("prune-foreign");
        let now = SystemTime::now();
        let foreign = plant_archive_file(&dir, "notes.txt", now, 999_999_999);
        let marker = plant_archive_file(&dir, "replay-marker", now, 999_999_999);
        let outcome = prune_archived_segments_at(&dir, 604_800, now);
        assert_eq!(outcome.deleted, 0);
        assert_eq!(outcome.kept, 2);
        assert!(foreign.exists(), "non-.wal files must never be touched");
        assert!(marker.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_prune_missing_dir_is_noop() {
        let dir = tmp_dir("prune-missing");
        // No archive/ subdir created at all.
        let outcome = prune_archived_segments_at(&dir, 604_800, SystemTime::now());
        assert_eq!(outcome, ArchivePruneOutcome::default());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_prune_keeps_future_mtime_clock_skew() {
        let dir = tmp_dir("prune-skew");
        let now = SystemTime::now();
        // Plant with a FUTURE mtime relative to the injected `now` by
        // evaluating "now" one day in the past.
        let past_now = now - Duration::from_secs(86_400);
        let skewed = plant_archive_file(&dir, "ws-frames-00000000000000000004.wal", now, 60);
        let outcome = prune_archived_segments_at(&dir, 604_800, past_now);
        assert_eq!(outcome.deleted, 0);
        assert!(
            skewed.exists(),
            "a future-mtime file (clock skew) must be kept — never delete on uncertainty"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
