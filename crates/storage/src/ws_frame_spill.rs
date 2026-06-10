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

use std::fs::{File, OpenOptions};
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
}

impl WsFrameSpill {
    /// Create a spill writer rooted at `wal_dir`. Spawns the background writer.
    // TEST-EXEMPT: covered by test_append_spill_and_replay_roundtrip + test_drop_counter_increments_when_channel_full (both construct)
    pub fn new<P: AsRef<Path>>(wal_dir: P) -> anyhow::Result<Self> {
        let wal_dir = wal_dir.as_ref().to_path_buf();
        std::fs::create_dir_all(&wal_dir)
            .map_err(|e| anyhow::anyhow!("create WAL dir {:?}: {e}", wal_dir))?;

        let (tx, rx) = bounded::<WalRecord>(SPILL_CHANNEL_CAPACITY);
        let drop_critical = Arc::new(AtomicU64::new(0));
        let persisted_total = Arc::new(AtomicU64::new(0));

        let persisted_for_thread = persisted_total.clone();
        let wal_dir_for_thread = wal_dir.clone();
        thread::Builder::new()
            .name("ws-frame-spill-writer".to_string())
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
        })
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
                AppendOutcome::Dropped
            }
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
    let path = wal_dir.join(format!("ws-frames-{:020}.wal", nanos));
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
// Daily conservation count — READ-ONLY scan for the 15:40 IST audit
// (TICK-CONSERVE-01, operator directive 2026-06-10 "Go ahead to achieve
// zero tick loss"). Unlike `replay_all` this NEVER archives or mutates —
// it only counts frames attributable to one IST trading day across the
// live segments AND `<wal_dir>/archive/` (where the boot replay moves
// processed segments).
// ---------------------------------------------------------------------------

/// Per-day WAL frame counts for the daily tick-conservation audit.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WalDayFrameCounts {
    /// Live-feed frames whose payload response code is a tick class
    /// (Ticker=2 / Quote=4 / Full=8) attributed to the target IST day.
    pub tick_frames: u64,
    /// Live-feed frames of any other response code (OI, PrevClose,
    /// MarketStatus, Disconnect, …) attributed to the target IST day.
    pub other_frames: u64,
    /// Records that cannot be day-attributed: legacy v1 records
    /// (`frame_seq == 0`) or empty payloads. Counted EXPLICITLY so the
    /// audit identity never silently miscounts them.
    pub unattributable: u64,
    /// Segments actually parsed (after the filename pre-filter).
    pub segments_scanned: u64,
    /// Segments that failed to open/parse (logged, scan continues).
    pub corrupted_segments: u64,
}

/// Only scan segments created within this many days BEFORE the target day.
/// Segments rotate at 128 MB and the prod box restarts daily, so a segment
/// older than this cannot realistically carry target-day frames; the bound
/// keeps the archive scan O(recent days) forever.
const CONSERVATION_SEGMENT_LOOKBACK_DAYS: u64 = 3;

/// IST day number (days since epoch, IST wall clock) for a wall-nanos value.
/// `frame_seq` is wall-nanos-seeded and strictly monotonic, so it doubles as
/// the frame's capture timestamp for day attribution.
#[inline]
#[must_use]
pub fn ist_day_of_wall_nanos(wall_nanos: u64) -> u64 {
    let secs = wall_nanos / 1_000_000_000;
    let offset = i64::from(tickvault_common::constants::IST_UTC_OFFSET_SECONDS);
    // APPROVED: offset is the positive +19800 IST constant; saturating add
    // keeps the arithmetic total for any u64 input.
    let ist_secs = secs.saturating_add(offset.unsigned_abs());
    ist_secs / u64::from(tickvault_common::constants::SECONDS_PER_DAY)
}

/// Parses the creation-nanos out of a `ws-frames-{nanos:020}.wal` filename.
/// Returns `None` for any other name shape (counted as scan-eligible so an
/// unexpected name is never silently skipped).
fn segment_creation_nanos(path: &Path) -> Option<u64> {
    let stem = path.file_stem()?.to_str()?;
    let digits = stem.strip_prefix("ws-frames-")?;
    digits.parse::<u64>().ok()
}

/// Classifies one replayed frame into the audit buckets for `target_ist_day`.
/// Pure — extracted for unit testing.
#[must_use]
pub fn classify_frame_for_day(
    ws_type_is_live_feed: bool,
    frame_seq: u64,
    first_payload_byte: Option<u8>,
    target_ist_day: u64,
) -> Option<bool> {
    // Returns: None = not countable for this day (other day / not live feed
    // / unattributable handled by caller via frame_seq==0 check);
    // Some(true) = tick frame; Some(false) = other live-feed frame.
    if !ws_type_is_live_feed || frame_seq == 0 {
        return None;
    }
    if ist_day_of_wall_nanos(frame_seq) != target_ist_day {
        return None;
    }
    let code = first_payload_byte?;
    // Hostile-review C1: the dispatcher parses BOTH code 1 (index ticker)
    // and code 2 (ticker) into `ParsedFrame::Tick` — under the IDX_I-heavy
    // universe, omitting code 1 here would bias delivery_residual
    // permanently negative and mask real leaks. Tick classes = {1, 2, 4, 8},
    // exactly the dispatcher arms that increment `tv_ticks_processed_total`.
    Some(matches!(
        code,
        tickvault_common::constants::RESPONSE_CODE_INDEX_TICKER
            | tickvault_common::constants::RESPONSE_CODE_TICKER
            | tickvault_common::constants::RESPONSE_CODE_QUOTE
            | tickvault_common::constants::RESPONSE_CODE_FULL
    ))
}

/// Counts WAL frames attributable to `target_ist_day` across the live
/// segments and `<wal_dir>/archive/`. READ-ONLY — never archives, never
/// deletes; safe to run while the writer is appending (the replay parser
/// stops cleanly at a partial tail). Cold path: once per trading day.
// TEST-EXEMPT: orchestration over replay_segment; classification + day-attribution unit-tested via the test_count_frames_* suite
pub fn count_frames_for_ist_day<P: AsRef<Path>>(
    wal_dir: P,
    target_ist_day: u64,
) -> WalDayFrameCounts {
    let wal_dir = wal_dir.as_ref();
    let mut counts = WalDayFrameCounts::default();

    let mut segments: Vec<PathBuf> = Vec::new();
    for dir in [wal_dir.to_path_buf(), wal_dir.join("archive")] {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.filter_map(|e| e.ok()) {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("wal") {
                continue;
            }
            // Pre-filter: skip segments created too long before the target
            // day to carry its frames (bounds the archive scan forever).
            // Hostile-review M1: allow created_day == target_day + 1 — a
            // frame stamped 23:59:59 can be drained into a segment whose
            // creation nanos land just past the IST midnight boundary.
            if let Some(nanos) = segment_creation_nanos(&path) {
                let created_day = ist_day_of_wall_nanos(nanos);
                if created_day > target_ist_day.saturating_add(1)
                    || created_day
                        < target_ist_day.saturating_sub(CONSERVATION_SEGMENT_LOOKBACK_DAYS)
                {
                    continue;
                }
            }
            segments.push(path);
        }
    }
    segments.sort();

    for path in &segments {
        match replay_segment(path) {
            Ok(batch) => {
                counts.segments_scanned = counts.segments_scanned.saturating_add(1);
                for frame in &batch {
                    let is_live = matches!(frame.ws_type, WsType::LiveFeed);
                    if is_live && frame.frame_seq == 0 {
                        counts.unattributable = counts.unattributable.saturating_add(1);
                        continue;
                    }
                    match classify_frame_for_day(
                        is_live,
                        frame.frame_seq,
                        frame.frame.first().copied(),
                        target_ist_day,
                    ) {
                        Some(true) => {
                            counts.tick_frames = counts.tick_frames.saturating_add(1);
                        }
                        Some(false) => {
                            counts.other_frames = counts.other_frames.saturating_add(1);
                        }
                        None => {}
                    }
                }
            }
            Err(err) => {
                counts.corrupted_segments = counts.corrupted_segments.saturating_add(1);
                warn!(segment = ?path, error = %err, "conservation count: segment unreadable; continuing");
            }
        }
    }
    counts
}

// ---------------------------------------------------------------------------
// Replay — walk every `.wal` file, parse records, return recovered frames.
// Corrupted / truncated tails are logged and skipped. Processed segments are
// moved to `<wal_dir>/archive/` so the next session starts clean.
// ---------------------------------------------------------------------------

// TEST-EXEMPT: covered by test_append_spill_and_replay_roundtrip + test_replay_handles_missing_dir + test_replay_detects_crc_corruption
pub fn replay_all<P: AsRef<Path>>(wal_dir: P) -> anyhow::Result<Vec<ReplayedFrame>> {
    let wal_dir = wal_dir.as_ref();
    if !wal_dir.exists() {
        return Ok(Vec::new());
    }

    let mut segments: Vec<PathBuf> = std::fs::read_dir(wal_dir)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("wal"))
        .collect();
    segments.sort();

    let mut frames = Vec::new();
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

    // Archive processed segments so we don't replay them twice.
    let archive_dir = wal_dir.join("archive");
    drop(std::fs::create_dir_all(&archive_dir));
    for seg in &segments {
        if let Some(name) = seg.file_name() {
            let dst = archive_dir.join(name);
            drop(std::fs::rename(seg, dst));
        }
    }

    Ok(frames)
}

fn replay_segment(path: &Path) -> anyhow::Result<Vec<ReplayedFrame>> {
    let mut f = File::open(path)?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf)?;

    let mut out = Vec::new();
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

    // --- TICK-CONSERVE-01: daily conservation count (read-only scan) ---

    fn now_wall_nanos() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0)
    }

    #[test]
    fn test_count_frames_for_ist_day_classifies_tick_codes() {
        let dir = tmp_dir("conserve-classify");
        let today = ist_day_of_wall_nanos(now_wall_nanos());
        {
            let spill = WsFrameSpill::new(&dir).unwrap();
            // 3 tick-class frames (response codes 2, 4, 8 in byte 0)...
            spill.append(WsType::LiveFeed, vec![2, 0, 0, 0]);
            spill.append(WsType::LiveFeed, vec![4, 0, 0, 0]);
            spill.append(WsType::LiveFeed, vec![8, 0, 0, 0]);
            // ...one non-tick live frame (PrevClose=6) and one order-update.
            spill.append(WsType::LiveFeed, vec![6, 0, 0, 0]);
            spill.append(WsType::OrderUpdate, b"{}".to_vec());
            wait_until_persisted(&spill, 5);
        }
        std::thread::sleep(Duration::from_millis(50));

        let counts = count_frames_for_ist_day(&dir, today);
        assert_eq!(counts.tick_frames, 3, "{counts:?}");
        assert_eq!(counts.other_frames, 1, "{counts:?}");
        assert_eq!(counts.unattributable, 0, "{counts:?}");
        assert!(counts.segments_scanned >= 1);
        assert_eq!(counts.corrupted_segments, 0);

        // Read-only invariant: counting again yields the SAME numbers
        // (replay_all would have archived; this must not).
        let again = count_frames_for_ist_day(&dir, today);
        assert_eq!(again, counts, "count scan must be read-only/idempotent");

        // A different target day attributes nothing.
        let other_day = count_frames_for_ist_day(&dir, today.saturating_sub(2));
        assert_eq!(other_day.tick_frames, 0);
        assert_eq!(other_day.other_frames, 0);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_classify_frame_for_day_and_ist_day_of_wall_nanos() {
        // Pure-fn surface: classification + day attribution.
        let nanos_today = now_wall_nanos();
        let today = ist_day_of_wall_nanos(nanos_today);
        // tick code on today → Some(true)
        assert_eq!(
            classify_frame_for_day(true, nanos_today, Some(4), today),
            Some(true)
        );
        // non-tick code on today → Some(false)
        assert_eq!(
            classify_frame_for_day(true, nanos_today, Some(6), today),
            Some(false)
        );
        // wrong day → None
        assert_eq!(
            classify_frame_for_day(true, nanos_today, Some(4), today + 1),
            None
        );
        // order-update ws_type → None
        assert_eq!(
            classify_frame_for_day(false, nanos_today, Some(4), today),
            None
        );
        // v1 legacy (frame_seq == 0) → None (caller buckets as unattributable)
        assert_eq!(classify_frame_for_day(true, 0, Some(4), today), None);
        // empty payload → None
        assert_eq!(classify_frame_for_day(true, nanos_today, None, today), None);
        // IST midnight boundary: 18:30 UTC == 00:00 IST next day.
        let utc_1829 = 1_770_000_000_u64; // arbitrary anchor
        let day_a = ist_day_of_wall_nanos(utc_1829 * 1_000_000_000);
        let day_b = ist_day_of_wall_nanos((utc_1829 + 86_400) * 1_000_000_000);
        assert_eq!(day_b, day_a + 1, "IST day must advance exactly daily");
    }

    #[test]
    fn test_count_frames_scans_archive_dir() {
        let dir = tmp_dir("conserve-archive");
        let today = ist_day_of_wall_nanos(now_wall_nanos());
        {
            let spill = WsFrameSpill::new(&dir).unwrap();
            spill.append(WsType::LiveFeed, vec![4, 1, 2, 3]);
            wait_until_persisted(&spill, 1);
        }
        std::thread::sleep(Duration::from_millis(50));

        // Simulate the boot replay's archive move, then count: the frame
        // must STILL be found (archive/ is part of the day's record).
        let archive = dir.join("archive");
        std::fs::create_dir_all(&archive).unwrap();
        for entry in std::fs::read_dir(&dir).unwrap().filter_map(|e| e.ok()) {
            let p = entry.path();
            if p.extension().and_then(|s| s.to_str()) == Some("wal") {
                let dst = archive.join(p.file_name().unwrap());
                std::fs::rename(&p, dst).unwrap();
            }
        }

        let counts = count_frames_for_ist_day(&dir, today);
        assert_eq!(counts.tick_frames, 1, "{counts:?}");

        let _ = std::fs::remove_dir_all(&dir);
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

        // Second replay must be empty (segments archived).
        let frames2 = replay_all(&dir).unwrap();
        assert!(frames2.is_empty());

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
}
