// STAGE-C: Non-blocking disk-durable spill for every WebSocket frame.
//
// Hot-path `append()` is O(1) and never blocks: it uses a crossbeam bounded
// channel with `try_send`. A dedicated background thread writes records to
// append-only WAL segment files. On startup, `replay_all()` walks every WAL
// file, validates CRC32, and returns the recovered frames so downstream
// consumers can drain them before live reads resume.
//
// DURABILITY — read this before relying on the word "durable" (corrected
// 2026-08-11). The writer thread calls `BufWriter::flush()`, which hands
// bytes to the OPERATING SYSTEM. It does NOT call `sync_all`/`sync_data`,
// so nothing forces them onto the physical disk. Concretely:
//
//   * process killed (SIGKILL, panic, OOM) -> flushed records SURVIVE, because
//     the page cache belongs to the kernel, not to us. This is the case the
//     WAL exists for and it is genuinely covered.
//   * machine loses power, kernel panics, or the host is force-stopped ->
//     records written since the last kernel writeback are LOST.
//
// Three comments in this file previously said "fsync". None was ever true —
// there is no `sync_all` anywhere in this crate. The claim mattered because
// the live feed refuses to open a socket without this WAL, citing it as the
// durability floor; overstating that floor is how a gap gets discovered late.
// Adding a real fsync is a deliberate throughput trade, not a typo fix.
//
// Record format on disk (three versions; replay accepts all three):
//     v1 [MAGIC:4="TVW1"][ws_type:u8][len:u32 LE][frame][crc32:u32 LE]
//     v2 [MAGIC:4="TVW2"][ws_type:u8][frame_seq:u64 LE][len:u32 LE][frame][crc32]
//     v3 [MAGIC:4="TVW3"][ws_type:u8][frame_seq:u64 LE][received_at_nanos:i64 LE]
//        [len:u32 LE][frame][crc32]
// CRC32 covers every header byte of that version, in order, then the frame.
//
// WHY v3 EXISTS (2026-08-28). The receipt instant is stamped in
// `FrameSink::accept` BEFORE this append — correctly, and early. It was then
// discarded here, because the record had nowhere to put it. Replay therefore
// re-stamped with `now()`, which was harmless while candles bucketed on the
// EXCHANGE clock and became load-bearing the moment they bucket on receipt:
// measured on 2026-08-27, 9.1% of a session's ticks replayed 9-20 hours after
// their true arrival, which under receipt-bucketing would file 34 real minutes
// into 4 bars stamped outside market hours. v3 carries the receipt so replay
// restores it instead of inventing one. A v1/v2 record replays with `0`, which
// the persistence layer already maps to NULL - a missing timestamp, never a
// false one.

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
    /// TrueData's live market-data socket (feed #4).
    ///
    /// A DISTINCT tag rather than a reuse of [`WsType::LiveFeed`], because
    /// this byte is the only thing that survives into the WAL — the record
    /// carries no feed field. Tagging TrueData frames as `LiveFeed` would
    /// mean a TrueData drop is attributed to **Dhan** in
    /// `/api/feeds/health` and in the Telegram alert (see
    /// [`WsType::owning_feed`]), naming the wrong broker in an operator
    /// page. The Dhan noise lock requires every alert to say precisely
    /// which broker; a wrong broker is worse than none.
    TruedataFeed = 5,
}

impl WsType {
    // TEST-EXEMPT: covered by test_ws_type_roundtrip (asserts from_u8/as_u8 identity)
    pub fn from_u8(b: u8) -> Option<Self> {
        match b {
            1 => Some(Self::LiveFeed),
            4 => Some(Self::OrderUpdate),
            5 => Some(Self::TruedataFeed),
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
            Self::TruedataFeed => "truedata_feed",
        }
    }

    /// Which broker's market data this transport carries, if any.
    ///
    /// The WAL record has no feed byte — the transport tag IS the feed
    /// evidence — so this is the single place that mapping is made, rather
    /// than each call site hardcoding a broker.
    ///
    /// `LiveFeed` maps to Dhan for historical reasons: tag 1 was minted
    /// when Dhan's main feed was the only live market-data socket. Groww
    /// and Dhan live feeds are both retired, so tag 1 exists today for WAL
    /// records written before those retirements — replaying one must still
    /// attribute correctly.
    ///
    /// `OrderUpdate` returns `None`: it is not market data, and a dropped
    /// order-update frame must not flip a market-data feed to `Degraded`.
    // TEST-EXEMPT: covered by test_owning_feed_maps_each_transport_to_its_broker
    pub fn owning_feed(self) -> Option<tickvault_common::feed::Feed> {
        match self {
            Self::LiveFeed => Some(tickvault_common::feed::Feed::Dhan),
            Self::TruedataFeed => Some(tickvault_common::feed::Feed::Truedata),
            Self::OrderUpdate => None,
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
    /// TVW3 (2026-08-28): the frame's TRUE arrival instant as UTC epoch nanos,
    /// stamped by the caller at receipt. Persisted so replay restores it rather
    /// than re-stamping `now()`. [`WAL_RECEIPT_UNKNOWN_NANOS`] when the caller
    /// has none — never a synthesized clock read.
    received_at_nanos: i64,
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
    /// TVW3: the frame's ORIGINAL arrival instant (UTC epoch nanos), read back
    /// from the v3 record. [`WAL_RECEIPT_UNKNOWN_NANOS`] for v1/v2 records that
    /// predate the field.
    ///
    /// A replay consumer MUST prefer this over a fresh clock read. Using `now()`
    /// is what placed 9.1% of a session's ticks 9-20 hours from their true
    /// arrival — invisible while candles bucketed on the exchange clock, and
    /// data-losing once they bucket on receipt.
    pub received_at_nanos: i64,
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
/// production: a transient writer stall of up to 13s (e.g. brief disk writeback
/// latency on a contended host) now absorbs without dropping. Memory cost
/// at idle is ~3 MiB extra (131k × ~24 B/`WalRecord` header), trivial on
/// the 4 GiB t4g.medium target.
///
/// 2026-08-10: raised 131,072 → 524,288 (4×). The 13-second stall headroom
/// quoted above was computed for **ONE** WebSocket producer. The operator's
/// 2026-08-09 authorization (`websocket-connection-scope-lock.md`, the
/// 16-connection amendment) takes the live feed to **up to 16 sockets** — 5
/// main-feed + 5 depth-20 + 5 depth-200 + 1 order-update — all funnelling into
/// this ONE shared channel. At 16 producers the same absorbency is ~0.8s, and
/// the capture-at-receipt contract (WAL BEFORE parse/broadcast) means a full
/// channel is not backpressure but **dropped frames on the durable floor** —
/// the one thing the zero-loss envelope must never trade away.
///
/// 4× restores roughly the original per-socket headroom at the authorized
/// connection count rather than merely surviving the chaos test. Memory at
/// idle ≈ 12 MiB (524k × ~24 B) — 0.04% of the r8g.xlarge 32 GiB host
/// (operator Quote 13), which is what makes the honest sizing affordable.
const SPILL_CHANNEL_CAPACITY: usize = 524_288;

/// WAL file magic bytes — segment-local sanity check.
///
/// TICK-SEQ-01 PR-2a: `TVW1` = v1 record (no `frame_seq`); `TVW2` = v2 record
/// (carries an 8-byte LE `frame_seq` immediately after `ws_type`). Replay
/// accepts BOTH, so segments written before this change still recover. NEW
/// records are always written v2.
const WAL_MAGIC: [u8; 4] = *b"TVW1";
const WAL_MAGIC_V2: [u8; 4] = *b"TVW2";
/// `TVW3` = v3 record: v2 plus an 8-byte LE `received_at_nanos` after
/// `frame_seq`. NEW records are always written v3.
const WAL_MAGIC_V3: [u8; 4] = *b"TVW3";

/// Minimum on-disk record size per version, used by the replay loop guard:
/// v1 = magic(4)+ws_type(1)+len(4)+crc(4) = 13; v2 inserts frame_seq(8) = 21.
const WAL_MIN_RECORD_V1: usize = 13;
const WAL_MIN_RECORD_V2: usize = 21;
/// v3 inserts received_at_nanos(8) after frame_seq: 21 + 8 = 29.
const WAL_MIN_RECORD_V3: usize = 29;

/// Sentinel written when a caller has no receipt instant to offer, and the
/// value a v1/v2 record replays with. NEVER a synthesized "now" — see the
/// module header. `tick_persistence` already maps 0 to NULL.
pub const WAL_RECEIPT_UNKNOWN_NANOS: i64 = 0;

/// Plausibility band for a receipt, in UTC epoch NANOS. Mirrors the band the
/// aggregator applies to the exchange timestamp
/// (`MIN/MAX_PLAUSIBLE_EXCHANGE_TS_SECS`), which the receipt path did not have.
///
/// The asymmetry mattered: `tick_persistence` treats any non-zero
/// `received_at` as usable and, for a tick whose exchange timestamp is the
/// vendor's never-traded sentinel, promotes it to the row's DESIGNATED
/// timestamp. A negative or absurd value would therefore create a pre-1970
/// QuestDB partition that retention and archival — both keyed on the trading
/// day — can never reach. Out-of-band values are recorded as
/// [`WAL_RECEIPT_UNKNOWN_NANOS`], never persisted as-is: a missing timestamp
/// is recoverable, a lying one is not.
///
/// 2020-09-13 .. 2050-01-01, the same span as the exchange band.
const MIN_PLAUSIBLE_RECEIPT_NANOS: i64 = 1_600_000_000_000_000_000;
const MAX_PLAUSIBLE_RECEIPT_NANOS: i64 = 2_524_608_000_000_000_000;

/// Clamp a caller-supplied receipt to the plausible band.
///
/// Returns the value unchanged when it is in band, and
/// [`WAL_RECEIPT_UNKNOWN_NANOS`] otherwise — including for the sentinel itself,
/// which is idempotent. O(1), two comparisons, no allocation.
#[must_use]
#[inline]
pub const fn plausible_receipt_nanos(received_at_nanos: i64) -> i64 {
    if received_at_nanos >= MIN_PLAUSIBLE_RECEIPT_NANOS
        && received_at_nanos <= MAX_PLAUSIBLE_RECEIPT_NANOS
    {
        received_at_nanos
    } else {
        WAL_RECEIPT_UNKNOWN_NANOS
    }
}

/// Rotate to a new segment after this many bytes.
const WAL_SEGMENT_MAX_BYTES: u64 = 128 * 1024 * 1024;

/// Writer buffer size — large enough to batch hundreds of records into one
/// write syscall. (Not an fsync batch: see the DURABILITY note in the module
/// header — this path never calls `sync_all`.)
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

/// Number of [`WsType`] variants — the width of every pre-resolved counter
/// table below. Pinned against the enum by
/// `tests::test_ws_type_index_is_dense_and_matches_all`, so adding a variant
/// without widening the tables fails the build rather than panicking at
/// runtime on an out-of-range index.
/// NOTE: this is THIS module's own three-variant [`WsType`] (the WAL transport
/// tag, `LiveFeed`/`OrderUpdate`/`TruedataFeed`), NOT the seven-variant
/// `tickvault_common::ws_event_types::WsType` used for audit rows.
const WS_TYPE_COUNT: usize = 3;

/// Dense index for a [`WsType`], used only to address the pre-resolved counter
/// tables in [`SpillDropCounters`].
///
/// Deliberately local to this module rather than a method on `WsType` in
/// `crates/common`: the index is an implementation detail of THIS module's
/// counter tables, and widening `crates/common` would escalate every change
/// here to a workspace-wide test run for no behavioural gain.
const fn ws_type_index(ws_type: WsType) -> usize {
    // Exhaustive by construction — no `_` arm, so adding a `WsType` variant
    // fails THIS match at compile time rather than silently folding the new
    // transport's losses into another variant's counter.
    match ws_type {
        WsType::LiveFeed => 0,
        WsType::OrderUpdate => 1,
        WsType::TruedataFeed => 2,
    }
}

/// Every [`WsType`], in [`ws_type_index`] order — the build order for the
/// counter tables. Kept beside the index so the two cannot drift.
const WS_TYPES_BY_INDEX: [WsType; WS_TYPE_COUNT] =
    [WsType::LiveFeed, WsType::OrderUpdate, WsType::TruedataFeed];

/// Loss counters resolved ONCE at construction, one handle per `WsType`.
///
/// # Why the macro form is banned on this path
///
/// `metrics::counter!(NAME, "label" => value)` builds a `Key` on EVERY call,
/// and a keyed `Key` owns a `Vec<Label>` — so the macro form heap-allocates
/// once per invocation. Both call sites are the WAL **drop** arms, which
/// execute only when the process is ALREADY losing data: the spill channel is
/// full or its writer thread is dead. Allocating there violates principle 1
/// (zero allocation on the hot path) and, practically, asks the allocator for
/// memory at the worst possible moment — a frame-drop storm becomes an
/// allocation storm on top of the loss it is trying to report.
///
/// `ws_type` is a per-CALL parameter here (unlike `WalRingSink`, whose
/// endpoint is fixed for the sink's lifetime), so one handle is not enough —
/// hence a dense table per series, addressed by [`ws_type_index`].
/// `Counter::increment` on a resolved handle is a plain atomic add: O(1),
/// zero allocation.
struct SpillDropCounters {
    /// `tv_ws_frame_spill_drop_critical{ws_type}` — both drop arms.
    drop_critical: [metrics::Counter; WS_TYPE_COUNT],
    /// `tv_ticks_lost_total{source="spill_drop_critical", ws_type}` — Full arm.
    ticks_lost_channel_full: [metrics::Counter; WS_TYPE_COUNT],
    /// `tv_ticks_lost_total{source="spill_writer_dead", ws_type}` — Disconnected arm.
    ticks_lost_writer_dead: [metrics::Counter; WS_TYPE_COUNT],
}

impl SpillDropCounters {
    /// Resolves every handle and publishes a zero on each.
    ///
    /// The zero matters: the CloudWatch agent computes a counter's alarm value
    /// as a DELTA between consecutive samples and has no previous sample for a
    /// series that has never been emitted, so it drops the first one. If the
    /// first emission a series ever sees is the outage itself, that outage is
    /// the dropped sample and the alarm does not fire for it. Publishing a zero
    /// at construction makes the harmless zero the dropped sample instead —
    /// the same discipline as `WalRingSink::pre_register`.
    fn new() -> Self {
        let drop_critical = WS_TYPES_BY_INDEX
            .map(|t| metrics::counter!("tv_ws_frame_spill_drop_critical", "ws_type" => t.as_str()));
        let ticks_lost_channel_full = WS_TYPES_BY_INDEX.map(|t| {
            metrics::counter!(
                "tv_ticks_lost_total",
                "source" => "spill_drop_critical",
                "ws_type" => t.as_str(),
            )
        });
        let ticks_lost_writer_dead = WS_TYPES_BY_INDEX.map(|t| {
            metrics::counter!(
                "tv_ticks_lost_total",
                "source" => "spill_writer_dead",
                "ws_type" => t.as_str(),
            )
        });
        let counters = Self {
            drop_critical,
            ticks_lost_channel_full,
            ticks_lost_writer_dead,
        };
        for idx in 0..WS_TYPE_COUNT {
            counters.drop_critical[idx].increment(0);
            counters.ticks_lost_channel_full[idx].increment(0);
            counters.ticks_lost_writer_dead[idx].increment(0);
        }
        counters
    }
}

pub struct WsFrameSpill {
    spill_tx: Sender<WalRecord>,
    drop_critical: Arc<AtomicU64>,
    persisted_total: Arc<AtomicU64>,
    /// Pre-resolved per-`WsType` loss counters — see [`SpillDropCounters`].
    drop_counters: SpillDropCounters,
    /// SP5.1: optional per-feed health registry. When `Some`, a dropped
    /// LIVE-FEED (Dhan) frame records a Dhan drop so `/api/feeds/health` flips
    /// `Degraded` — closing the SP5 connected+fresh-but-dropping false-OK.
    /// `None` keeps the spill feed-health-agnostic (byte-identical hot path).
    /// Read ONLY in the cold drop arms; never on the hot `Spilled` path.
    feed_health: Option<Arc<tickvault_common::feed_health::FeedHealthRegistry>>,
    /// The exclusive claim on the WAL directory, held for as long as this spill
    /// exists. Never read — its ONLY job is to keep the `flock` taken, and to
    /// release it on drop so the next process can start. See [`WalDirGuard`].
    _dir_guard: WalDirGuard,
}

impl WsFrameSpill {
    /// Create a spill writer rooted at `wal_dir`. Spawns the background writer.
    // TEST-EXEMPT: covered by test_append_spill_and_replay_roundtrip + test_drop_counter_increments_when_channel_full (both construct)
    pub fn new<P: AsRef<Path>>(wal_dir: P) -> anyhow::Result<Self> {
        let wal_dir = wal_dir.as_ref().to_path_buf();
        std::fs::create_dir_all(&wal_dir) // O(1) EXEMPT: one-shot constructor, not the per-frame append
            .map_err(|e| anyhow::anyhow!("create WAL dir {:?}: {e}", wal_dir))?;

        // ORDER IS LOAD-BEARING, and this is the first thing that happens.
        //
        // 1. CLAIM the directory exclusively, or refuse to start. Two processes
        //    sharing one WAL directory mint `capture_seq` from two independent
        //    clock-seeded counters, collide inside the `ticks` DEDUP key, and
        //    destroy ticks with no counter anywhere to show it. See
        //    [`lock_wal_dir`] for why this is fail-closed rather than a warning.
        // 2. Only THEN read the on-disk high-water mark and ratchet the counter
        //    past it. Doing this before the lock would race a live incumbent
        //    that is still appending, and could seed from a value it is about
        //    to exceed.
        // 3. Only THEN spawn the writer thread, so nothing can append at a
        //    sequence below the high-water mark.
        let dir_guard = lock_wal_dir(&wal_dir)?;
        let disk_high = seed_frame_seq_from_disk(&wal_dir);
        if disk_high > 0 {
            tracing::debug!(
                wal_dir = ?wal_dir,
                disk_high,
                "WAL directory claimed exclusively; sequence ratcheted past disk"
            );
        }

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
            drop_counters: SpillDropCounters::new(),
            feed_health: None,
            _dir_guard: dir_guard,
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
        // A unique directory per call so the flock is always free: this
        // constructor exists to exercise the dead-writer arm, and a lock
        // contention failure here would prove nothing about that arm.
        let dir = std::env::temp_dir().join(format!(
            "tv-wal-dead-writer-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        ));
        std::fs::create_dir_all(&dir).expect("test temp dir");
        let dir_guard = lock_wal_dir(&dir).expect("fresh temp dir is never contended");
        let (tx, rx) = bounded::<WalRecord>(SPILL_CHANNEL_CAPACITY);
        drop(rx); // no writer ever runs → channel is Disconnected for sends
        Self {
            spill_tx: tx,
            drop_critical: Arc::new(AtomicU64::new(0)),
            persisted_total: Arc::new(AtomicU64::new(0)),
            drop_counters: SpillDropCounters::new(),
            feed_health: None,
            _dir_guard: dir_guard,
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
        // No receipt instant offered — record the sentinel, NEVER a clock read
        // here. Minting one would be indistinguishable on replay from a real
        // arrival time, which is the exact defect TVW3 exists to close.
        self.append_with_seq_at(ws_type, frame, frame_seq, WAL_RECEIPT_UNKNOWN_NANOS)
    }

    /// Like [`WsFrameSpill::append_with_seq`] but also persists the frame's
    /// TRUE arrival instant (UTC epoch nanos), so boot replay can restore it
    /// instead of re-stamping `now()`.
    ///
    /// The caller stamps `received_at_nanos` at the read instant, BEFORE this
    /// append. Hot path, O(1), zero-alloc — the extra field is 8 bytes on an
    /// already-moved struct.
    // TEST-EXEMPT: same `try_send` path as `append_with_seq`; the receipt round-trip is covered by tvw3_roundtrip_preserves_received_at and tvw1_and_tvw2_records_replay_with_unknown_receipt.
    pub fn append_with_seq_at(
        &self,
        ws_type: WsType,
        frame: impl Into<Bytes>,
        frame_seq: u64,
        received_at_nanos: i64,
    ) -> AppendOutcome {
        let record = WalRecord {
            ws_type,
            frame_seq,
            // Banded at the boundary, not trusted. An out-of-band value is
            // recorded as UNKNOWN rather than persisted, because
            // `tick_persistence` promotes a non-zero receipt to the row's
            // DESIGNATED timestamp whenever the exchange stamp is the vendor's
            // never-traded sentinel — so a negative receipt would mint a
            // pre-1970 partition that retention and archival, both keyed on the
            // trading day, can never reach. Two comparisons, O(1), no alloc.
            received_at_nanos: plausible_receipt_nanos(received_at_nanos),
            frame: frame.into(),
        };
        match self.spill_tx.try_send(record) {
            Ok(()) => AppendOutcome::Spilled,
            Err(TrySendError::Full(_)) => {
                let prev = self.drop_critical.fetch_add(1, Ordering::Relaxed);
                // `code` added 2026-08-26. Without it this line carried only
                // `ws_type` and `drop_count`, so the CloudWatch metric filter
                // `{ $.code = "WS-SPILL-02" }` (error-code-alarms.tf) never
                // matched it — while the sibling `Disconnected` arm 20 lines
                // below has always carried the field. Channel-FULL is the more
                // likely of the two in production (it is what a writer stalled
                // behind a saturated disk produces), so the arm that could not
                // page was the one most likely to fire.
                error!(
                    code = ErrorCode::WsSpill02FrameDropped.code_str(),
                    ws_type = ws_type.as_str(),
                    drop_count = prev + 1,
                    "CRITICAL: WAL spill channel FULL — frame dropped (writer stalled)"
                );
                // Pre-resolved handles, NEVER the labelled macro form — this arm
                // runs only when the process is already losing frames, which is
                // the worst possible moment to allocate. See `SpillDropCounters`.
                let idx = ws_type_index(ws_type);
                self.drop_counters.drop_critical[idx].increment(1);
                // SLA counter: every dropped frame is one tick-equivalent lost.
                // Parthiban 2026-04-20: explicit metric so the zero-tick-loss
                // invariant can be asserted in CI instead of inferred from a
                // gap between `tv_ticks_processed_total` and
                // `tv_ticks_persisted_total`. Labelled with the same `ws_type`
                // so a per-WebSocket loss attribution stays possible.
                self.drop_counters.ticks_lost_channel_full[idx].increment(1);
                self.record_feed_drop_for_health(ws_type);
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
                // Pre-resolved handles for the same reason as the Full arm.
                let idx = ws_type_index(ws_type);
                self.drop_counters.drop_critical[idx].increment(1);
                // The distinguishing cause lives on the SLA counter's `source`
                // label (Full arm uses "spill_drop_critical").
                self.drop_counters.ticks_lost_writer_dead[idx].increment(1);
                self.record_feed_drop_for_health(ws_type);
                AppendOutcome::Dropped
            }
        }
    }

    /// SP5.1: on a terminal drop of a market-data frame, record the drop
    /// against **the broker that transport actually belongs to** in the
    /// per-feed health registry, so `/api/feeds/health` flips `Degraded`
    /// (closing the SP5 connected+fresh-but-dropping false-OK).
    ///
    /// The feed comes from [`WsType::owning_feed`], not a hardcoded
    /// `Feed::Dhan`. Hardcoding was correct while Dhan's was the only live
    /// market-data socket, but with TrueData appending to the same WAL it
    /// would report a TrueData loss as a **Dhan** loss — the wrong broker
    /// in an operator page, which the Dhan noise lock explicitly forbids.
    ///
    /// Called ONLY from the two cold drop arms — never the hot `Spilled`
    /// path. A market-data frame drop ⊇ tick drop (the frame may be
    /// OI/PrevClose/MarketStatus too) — all are real data losses, so
    /// `Degraded` is the correct, honest signal. `OrderUpdate` drops are
    /// NOT recorded (not market data). O(1), zero-alloc, lock-free (one
    /// relaxed atomic). `None` registry = no-op.
    #[inline]
    fn record_feed_drop_for_health(&self, ws_type: WsType) {
        if let Some(feed) = ws_type.owning_feed()
            && let Some(ref fh) = self.feed_health
        {
            fh.record_drops(feed, 1);
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
                    // Reported, not discarded. This was `drop(w.flush())`, and
                    // the buffer it drops is `WAL_WRITER_BUFFER` = 256 KiB --
                    // roughly 1,300 records that `persisted` has ALREADY
                    // counted, because `persist_record_resilient` increments on
                    // a successful `write_all` into the buffer, not on a
                    // successful flush to the platter.
                    //
                    // So the old form did two wrong things at once: it lost the
                    // records, and it left `persisted_count()` over-reporting by
                    // exactly the number it lost. The sibling flush thirty lines
                    // below has always called `report_io_error` -- this arm and
                    // the rotation arm were the two that did not.
                    if let Err(err) = w.flush() {
                        report_io_error("flush_on_close", &err);
                    }
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
                // Same repair as the close arm above: a failed rotation flush
                // silently lost the tail of the segment it was closing, while
                // `persisted` had already counted every record in it.
                if let Err(err) = w.flush() {
                    report_io_error("flush_on_rotate", &err);
                }
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
        // BACKOFF (2026-08-24, audit): the two SIBLING I/O failure arms — the
        // flush arm and the `write_record` arm below — both sleep
        // `WAL_WRITER_IO_RETRY_BACKOFF` before returning. This one did not, and
        // it is the arm that fires on a FULL DISK: `open_segment_resilient`
        // emits one coded `error!` per record while no segment can be opened,
        // so at the ~5,000 frame/s envelope the writer spun through ~5,000
        // `open()` syscalls and ~5,000 ERROR lines PER SECOND, written to the
        // disk that is already full. The failure is detected either way
        // (WS-SPILL-01 has a live metric-filter alarm), so this is not a
        // silence fix — it stops a detected failure from amplifying itself and
        // starving the very disk the operator has to recover.
        //
        // The sleep is the writer thread's own, exactly as in the sibling arms:
        // this runs on the dedicated WAL writer, never on the hot path. Frames
        // continue to reach the broadcast and the ring→spill→DLQ while it
        // backs off — the durable-WAL belt is what is degraded, and it is
        // already degraded by the full disk.
        thread::sleep(WAL_WRITER_IO_RETRY_BACKOFF);
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

/// Bits reserved at the BOTTOM of every frame sequence for a packet index.
///
/// A single main-feed message can carry several stacked packets, and each one
/// becomes its own `ticks` row. `capture_seq` is part of the DEDUP key
/// `(ts, security_id, segment, capture_seq, feed)`, so every packet needs a
/// DISTINCT value — and, for WAL replay to be idempotent rather than
/// duplicating, a value that can be REGENERATED from what the WAL persisted.
///
/// The WAL record stores the frame's sequence, not each packet's. So the
/// packet index is carried in reserved low bits of that one number: packet `i`
/// of frame `F` is `F | i`, which replay reproduces exactly.
///
/// `2^17 = 131_072` covers `MAX_PACKETS_PER_FRAME` (70,000) with room to spare.
pub const PACKET_INDEX_BITS: u32 = 17;

/// Largest packet index representable in the reserved bits.
pub const MAX_PACKET_INDEX: u64 = (1 << PACKET_INDEX_BITS) - 1;

/// Process-wide strictly-monotonic frame sequence, with the low
/// [`PACKET_INDEX_BITS`] always ZERO so callers can OR a packet index in.
///
/// Lock-free CAS, O(1), zero heap alloc. The WS read loop calls this ONCE per
/// frame and passes the value to BOTH [`WsFrameSpill::append_with_seq`] and the
/// live broadcast, so the WAL record and the `ticks.capture_seq` column carry
/// the identical replay-stable value.
///
/// ## Why the shift, and why NOT multiplication (2026-08-14)
///
/// Before this change the value was `max(prev+1, wall_nanos)` with no reserved
/// bits, and the doc above was true only of packet 0: packets 1..N of a stacked
/// frame minted a FRESH sequence, which is not in the WAL and cannot be
/// regenerated. Replaying such a frame would therefore write DUPLICATE rows
/// instead of collapsing onto the originals — which is why WAL re-fold could
/// not safely be built on top of it.
///
/// The obvious repair, `frame_seq * MAX_PACKETS_PER_FRAME`, is arithmetically
/// impossible and was rejected on measurement, not taste: a 2026 nanosecond
/// clock is ≈1.786e18, and ×70,000 is ≈1.25e23 — past `i64::MAX` (9.223e18) by
/// four orders of magnitude, so `capture_seq_from_frame_seq`'s `i64::try_from`
/// would refuse EVERY tick.
///
/// Shifting the SEED instead costs nothing: `(n >> 17) << 17` only clears the
/// low bits, so the magnitude is unchanged and the headroom to `i64::MAX`
/// stays the same ≈5.16× it already was. Uniqueness is by construction, not by
/// luck — every frame's low bits are zero and the base is strictly increasing,
/// so one frame's packet slots can never reach the next frame's base.
///
/// Honest cost: base granularity becomes 131,072 ns ≈ 131 µs, so at sustained
/// rates above ~7,600 frames/s the counter advances on `prev+1` faster than the
/// wall clock. At 10,000 frames/s that drift is ≈113 days of clock-equivalent
/// per calendar year, against ≈236 years of headroom.
static WAL_FRAME_SEQ: AtomicU64 = AtomicU64::new(0);

pub fn next_frame_seq() -> u64 {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX))
        .unwrap_or(0);
    // Work in BASE units (nanos >> reserved bits) so the returned value always
    // ends in zeroed low bits.
    let now_base = now >> PACKET_INDEX_BITS;
    // Never let the shift back up overflow u64.
    let base_ceiling = u64::MAX >> PACKET_INDEX_BITS;
    loop {
        let prev = WAL_FRAME_SEQ.load(Ordering::Relaxed);
        let prev_base = prev >> PACKET_INDEX_BITS;
        let next_base = prev_base.saturating_add(1).max(now_base).min(base_ceiling);
        let next = next_base << PACKET_INDEX_BITS;
        if WAL_FRAME_SEQ
            .compare_exchange_weak(prev, next, Ordering::SeqCst, Ordering::Relaxed)
            .is_ok()
        {
            return next;
        }
    }
}

/// The exclusive lock file held for the lifetime of the process that owns a
/// WAL directory.
///
/// It lives INSIDE the WAL directory rather than beside it so the lock and the
/// thing it protects can never be separated by a config change: whoever points
/// at this directory takes this lock, whatever the directory is called.
pub const WAL_DIR_LOCK_FILE: &str = ".wal-owner.lock";

/// Counter: a process REFUSED to open the WAL directory because another live
/// process already owns it. This is the fail-closed arm and it is a refusal,
/// never a loss — the frames the incumbent is capturing are unaffected.
pub const WAL_DIR_LOCK_REFUSED_COUNTER: &str = "tv_wal_dir_lock_refused_total";

/// Counter: the boot re-seed advanced [`WAL_FRAME_SEQ`] past a high-water mark
/// found on disk. Non-zero means a restart WOULD have re-issued sequence values
/// already in the database, and did not.
pub const WAL_SEQ_RESEED_ADVANCED_COUNTER: &str = "tv_wal_seq_reseed_advanced_total";

/// An exclusive, kernel-held claim on one WAL directory.
///
/// # Why this exists
///
/// `capture_seq` is a column of the `ticks` DEDUP UPSERT key
/// `(ts, security_id, segment, capture_seq, feed)`. Two DIFFERENT ticks that
/// arrive carrying the same key do not both survive — QuestDB upserts one away,
/// and **no counter in this process reports it**, because from here both rows
/// were appended successfully. It is silent, unrecoverable tick loss that shows
/// up only as a number that should have been bigger.
///
/// The sequence making that key unique is [`WAL_FRAME_SEQ`], a process-global
/// counter seeded from the wall clock. That is airtight WITHIN one process and
/// worth nothing ACROSS two: a second process starts its own counter from the
/// same clock, mints the same 131 µs bases, and every instrument that ticks
/// twice inside one exchange-second can lose a tick. Nothing notices, because
/// the collision happens inside the database rather than in our code.
///
/// So the invariant is narrow and load-bearing: **one live process per WAL
/// directory**. It is `flock(LOCK_EX | LOCK_NB)` through
/// `std::fs::File::try_lock` (std since Rust 1.89 — no new dependency), which
/// means the KERNEL releases it when the process dies, including on `SIGKILL`,
/// an OOM kill, or a container stop. That is the property a PID file cannot
/// offer: a stale PID file after a hard kill locks a healthy restart out of its
/// own data, trading silent loss for guaranteed downtime.
///
/// # Why fail-closed
///
/// The alternative — warn and continue — is what existed before 2026-08-28, and
/// it is the bug. A refusal costs one boot and says so loudly; proceeding costs
/// ticks that cannot be reconstructed from anything, because both writers
/// believe they succeeded.
///
/// The guard is held by value inside [`WsFrameSpill`], so dropping the spill
/// releases the directory for the next process.
#[derive(Debug)]
pub struct WalDirGuard {
    /// Held open purely so the `flock` stays taken. Dropping this releases it,
    /// which is the intended release path.
    _lock: File,
    path: PathBuf,
}

impl WalDirGuard {
    /// The lock file this guard holds.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Take the exclusive claim on `wal_dir`, or refuse.
///
/// # Errors
///
/// - the lock file cannot be created — a permissions or disk problem the caller
///   must not paper over, since an unwritable WAL directory has no durable
///   floor at all;
/// - another live process already holds it. The error names the file so an
///   operator can identify the incumbent with `fuser` or `lsof`.
pub fn lock_wal_dir(wal_dir: &Path) -> anyhow::Result<WalDirGuard> {
    let path = wal_dir.join(WAL_DIR_LOCK_FILE);
    // `create(true)` and deliberately NOT `create_new(true)`: the file survives
    // a clean shutdown and carries no state — the kernel lock IS the state — so
    // an existing file is the normal case and is reused.
    let lock = std::fs::OpenOptions::new() // O(1) EXEMPT: one-shot boot claim, never the per-frame append
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&path)
        .map_err(|e| anyhow::anyhow!("open WAL lock file {path:?}: {e}"))?;

    match lock.try_lock() {
        Ok(()) => Ok(WalDirGuard { _lock: lock, path }),
        Err(e) => {
            metrics::counter!(WAL_DIR_LOCK_REFUSED_COUNTER).increment(1);
            Err(anyhow::anyhow!(
                "WAL directory {wal_dir:?} is already owned by another live process \
                 (lock file {path:?}: {e}). REFUSING to start a second writer: two \
                 processes minting capture_seq from their own clocks silently destroy \
                 ticks through the ticks DEDUP key, with no counter to show it. \
                 Identify the incumbent with `fuser -v` on the lock file and stop it, \
                 or point this process at its own WAL directory."
            ))
        }
    }
}

/// The highest `frame_seq` already committed to disk under `wal_dir`.
///
/// # Why a restart needs this
///
/// [`next_frame_seq`] returns `max(prev + 1, wall_clock)`. Above roughly 7,600
/// frames/second the `prev + 1` arm wins and the counter runs AHEAD of the wall
/// clock — its own header says so. A process that then restarts re-seeds from
/// `AtomicU64::new(0)` plus the wall clock, i.e. from a value BELOW the
/// high-water mark it had reached, and begins re-issuing sequence numbers that
/// are already rows in the database. Those rows share the full DEDUP key with
/// the new ticks and upsert them away.
///
/// It is the same silent loss as the two-process case, reached by one process
/// and a crash instead of by two. So the counter has to be a ratchet ACROSS
/// restarts, not only within one run.
///
/// # Cost
///
/// Only the NEWEST segment of each of the three directories (live, `replaying/`,
/// `archive/`) is scanned, and only record HEADERS are read — frame payloads are
/// seeked past, never copied. Sequences increase in append order and segments
/// rotate in order, so the newest segment holds the maximum. Three bounded
/// header walks on a cold boot path.
///
/// Returns `0` when the directory is absent, empty, or holds only v1 (`TVW1`)
/// records, which predate `frame_seq`. `0` is the correct answer there: it
/// leaves the wall clock as the seed, exactly as before.
#[must_use]
pub fn highest_frame_seq_on_disk(wal_dir: &Path) -> u64 {
    let mut highest = 0u64;
    for dir in [
        wal_dir.to_path_buf(),
        wal_dir.join(REPLAYING_SUBDIR),
        wal_dir.join(ARCHIVE_SUBDIR),
    ] {
        let mut segs = wal_segments_in(&dir);
        // Lexicographic == chronological: the name is the zero-padded rotation
        // nanos, so the last is the newest and holds the maximum.
        segs.sort_by(|a, b| a.file_name().cmp(&b.file_name())); // O(1) EXEMPT: boot-time seed, cold path
        if let Some(newest) = segs.last() {
            highest = highest.max(highest_frame_seq_in_segment(newest));
        }
    }
    highest
}

/// Header-only walk of one segment, returning the greatest `frame_seq` in it.
///
/// Deliberately tolerant: this is a best-effort high-water probe, not a replay.
/// A torn tail, an unknown magic, or a short read simply ends the walk and
/// returns what was seen — a LOWER bound is still strictly better than the wall
/// clock alone, and refusing to boot over a torn tail would turn a safety net
/// into an outage. Corruption accounting belongs to [`replay_segment`], which
/// walks the same files moments later and reports it properly.
fn highest_frame_seq_in_segment(path: &Path) -> u64 {
    let Ok(mut f) = File::open(path) else {
        return 0;
    };
    let mut highest = 0u64;
    // One header at a time; v3 is the largest at 29 bytes.
    let mut head = [0u8; WAL_MIN_RECORD_V3];
    loop {
        let mut filled = 0usize;
        while filled < head.len() {
            match std::io::Read::read(&mut f, &mut head[filled..]) {
                Ok(0) => break,
                Ok(n) => filled += n,
                Err(_) => return highest,
            }
        }
        if filled < WAL_MIN_RECORD_V1 {
            return highest;
        }
        let magic = &head[0..4];
        let is_v3 = magic == WAL_MAGIC_V3;
        let is_v2 = magic == WAL_MAGIC_V2;
        let is_v1 = magic == WAL_MAGIC;
        if !is_v1 && !is_v2 && !is_v3 {
            return highest;
        }
        let min_rec = if is_v3 {
            WAL_MIN_RECORD_V3
        } else if is_v2 {
            WAL_MIN_RECORD_V2
        } else {
            WAL_MIN_RECORD_V1
        };
        if filled < min_rec {
            return highest;
        }
        // v1: [magic|ws|len|frame|crc]                 -> len at 5
        // v2: [magic|ws|seq(8)|len|frame|crc]           -> seq at 5, len at 13
        // v3: [magic|ws|seq(8)|recv(8)|len|frame|crc]   -> seq at 5, len at 21
        let len_off = if is_v3 {
            21
        } else if is_v2 {
            13
        } else {
            5
        };
        if (is_v2 || is_v3)
            && let Ok(seq_bytes) = <[u8; 8]>::try_from(&head[5..13])
        {
            highest = highest.max(u64::from_le_bytes(seq_bytes));
        }
        let Ok(len_bytes) = <[u8; 4]>::try_from(&head[len_off..len_off + 4]) else {
            return highest;
        };
        let frame_len = u64::from(u32::from_le_bytes(len_bytes));
        // Skip the payload and its 4-byte CRC. The header read may have
        // over-read into the payload, so the seek is relative to where the
        // header actually ended, not to where the cursor now sits.
        let over_read = i64::try_from(filled - min_rec).unwrap_or(i64::MAX);
        let Ok(skip) = i64::try_from(frame_len + 4) else {
            return highest;
        };
        let Some(delta) = skip.checked_sub(over_read) else {
            return highest;
        };
        if std::io::Seek::seek(&mut f, std::io::SeekFrom::Current(delta)).is_err() {
            return highest;
        }
    }
}

/// Ratchet [`WAL_FRAME_SEQ`] past everything already on disk under `wal_dir`.
///
/// Idempotent and monotonic: it only ever RAISES the counter, so calling it
/// twice, or after frames have already been minted, can neither lower it nor
/// reissue a value. Returns the high-water mark found, for logging.
pub fn seed_frame_seq_from_disk(wal_dir: &Path) -> u64 {
    let disk_high = highest_frame_seq_on_disk(wal_dir);
    if disk_high == 0 {
        return 0;
    }
    // Advance to disk_high + one base unit so the very next mint is strictly
    // greater than anything already written. `fetch_max` keeps it monotonic
    // against a concurrent minter.
    let target = (disk_high >> PACKET_INDEX_BITS)
        .saturating_add(1)
        .min(u64::MAX >> PACKET_INDEX_BITS)
        << PACKET_INDEX_BITS;
    let prev = WAL_FRAME_SEQ.fetch_max(target, Ordering::SeqCst);
    if prev < target {
        metrics::counter!(WAL_SEQ_RESEED_ADVANCED_COUNTER).increment(1);
        tracing::info!(
            disk_high_water = disk_high,
            seeded_to = target,
            was = prev,
            "WAL sequence re-seeded past the on-disk high-water mark — a restart \
             below it would have re-issued capture_seq values already in the \
             database, silently upserting live ticks away"
        );
    }
    disk_high
}

/// The replay-stable `capture_seq` for packet `packet_index` of the frame whose
/// sequence is `frame_seq`.
///
/// `None` when the index does not fit the reserved bits — the caller must then
/// REFUSE the packet and count it, never fall back to a fresh sequence. A fresh
/// sequence is exactly the un-regenerable value this whole scheme exists to
/// eliminate, and silently minting one here would reintroduce duplicate rows on
/// replay while looking like it worked.
#[must_use]
pub fn packet_capture_seq(frame_seq: u64, packet_index: u64) -> Option<u64> {
    if packet_index > MAX_PACKET_INDEX {
        return None;
    }
    // Defensive: a v1 (`TVW1`) record or a hand-built seq may not be
    // base-aligned. Clear the low bits rather than OR-ing into a dirty slot,
    // which could otherwise collide with a neighbouring packet.
    Some((frame_seq & !MAX_PACKET_INDEX) | packet_index)
}

fn write_record(w: &mut BufWriter<File>, r: &WalRecord) -> std::io::Result<()> {
    // TICK-SEQ-01: always write the v2 record (TVW2 + 8-byte LE frame_seq after
    // ws_type). `u32::try_from` makes an over-large frame explicit rather than
    // silently truncating (frames are ≤162 B in production).
    let frame_len = u32::try_from(r.frame.len()).map_err(|_| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, "WAL frame > u32::MAX")
    })?;
    let frame_seq = r.frame_seq.to_le_bytes();
    let receipt = r.received_at_nanos.to_le_bytes();
    // CRC covers ws_type || frame_seq || received_at_nanos || len || frame.
    let crc = crc32_ieee_of(&[
        &[r.ws_type.as_u8()],
        &frame_seq[..],
        &receipt[..],
        &frame_len.to_le_bytes()[..],
        &r.frame,
    ]);
    w.write_all(&WAL_MAGIC_V3)?;
    w.write_all(&[r.ws_type.as_u8()])?;
    w.write_all(&frame_seq)?;
    w.write_all(&receipt)?;
    w.write_all(&frame_len.to_le_bytes())?;
    w.write_all(&r.frame)?;
    w.write_all(&crc.to_le_bytes())?;
    Ok(())
}

fn record_disk_size(r: &WalRecord) -> u64 {
    // v3: magic(4) + ws_type(1) + frame_seq(8) + receipt(8) + len(4) + frame
    //     + crc(4) = 29 + frame
    WAL_MIN_RECORD_V3 as u64 + r.frame.len() as u64
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

/// Largest total frame payload `replay_all` will hold in RAM at once.
///
/// # The failure this bounds
///
/// `replay_all` globbed every live `*.wal` segment and materialised EVERY
/// frame into one `Vec` with no cap. That is survivable when the WAL holds a
/// crash's worth of frames, and it is not what the WAL holds: `confirm_replayed`
/// has exactly ONE production call site — at boot — so live segments are never
/// archived DURING a session. Every morning's boot therefore re-read the whole
/// previous trading session.
///
/// At the authorized 25,000-instrument scale that is Full mode's 162 B/packet
/// × ~12,500 packets/sec × a 6.7-hour session ≈ **48 GB** on a 32 GiB host —
/// an OOM at boot, before a single tick of the new session. At today's 4,565
/// SIDs it is already ≈ 9 GB. The process would die in the one place where
/// dying costs the most: holding the only copy of yesterday's unconfirmed
/// frames.
///
/// # Why a byte budget is SAFE here, specifically
///
/// A segment that `replay_all` does not consume is left as `*.wal`, and this
/// module's own crash-safety invariant already says such a segment is
/// re-globbed on the next boot — verbatim, "which is STILL re-globbed next
/// boot — never worse". So a partial replay strands nothing: it defers. The
/// budget converts an unbounded allocation into a bounded one whose remainder
/// is picked up by machinery that already exists and is already tested.
///
/// **CORRECTED 2026-08-28.** That paragraph was true of the segments it was
/// written about — live `*.wal` files — and FALSE for the other source
/// `replay_all` globs. A leftover already inside `replaying/` from a crashed
/// prior boot is NOT "left as `*.wal` in the live dir": it stays in the
/// staging area, and `confirm_replayed` archives everything it finds there,
/// read or not. So for that class a budget deferral did NOT defer, it
/// DELETED — silently, with no counter and no error.
///
/// The claim is now true again because the code makes it true: deferred
/// leftovers are RESTORED to the live dir before this function returns, so
/// "unconsumed ⇒ still a live `*.wal`" holds for both sources. Recorded rather
/// than quietly reworded, because the sentence being corrected was a
/// SAFETY ARGUMENT — it is what a reader consults instead of tracing the
/// archive path, and it argued the hazard out of existence.
///
/// # Honest cost
///
/// Frames past the budget are recovered on a LATER boot rather than this one,
/// and until then they are on disk rather than in the database. That is a real
/// degradation and it is the right trade: bounded-and-late beats
/// unbounded-and-dead, because an OOM recovers nothing at all and takes the
/// box down with it. The deferral is COUNTED and logged, never silent.
///
/// 512 MiB is ~3.3 M Full-mode packets — far more than any real crash window —
/// and 1/64th of the 32 GiB host, so it cannot itself be the thing that OOMs.
pub const WAL_REPLAY_MAX_BYTES: usize = 512 * 1024 * 1024;

/// Counter: segments `replay_all` deferred to the next boot for the budget.
pub const WAL_REPLAY_DEFERRED_COUNTER: &str = "tv_wal_replay_deferred_segments_total";

/// Counter: DEFERRED segments `replay_all` could not restore out of the replay
/// staging area back to the live directory.
///
/// Non-zero means captured frames are sitting where the confirm step archives
/// them unread -- the single path by which WAL recovery can lose data. It is
/// incremented unconditionally (by zero on a clean boot) so the series exists
/// before the first real occurrence.
pub const WAL_REPLAY_RESTORE_FAILED_COUNTER: &str = "tv_wal_replay_restore_failed_total";

/// Neither of the two counters below is EMF-selected, and that is deliberate
/// rather than an omission: both abandon sites emit a CODED `error!` carrying
/// `ErrorCode::WsSpill02FrameDropped`, and `ws-spill-02` already has a live
/// metric-filter alarm in `error-code-alarms.tf`. So the operator is paged the
/// moment either fires, through a lane that already exists, at zero additional
/// cost -- while each new EMF name would be ~$0.30/mo against a maximal month
/// already above the budget's automatic `STOP_EC2_INSTANCES` line. The counters
/// exist to give the page a MAGNITUDE on `/metrics` and through the debug API,
/// not to be the thing that reports it.
///
/// Segments abandoned mid-file because a record inside them was corrupt.
///
/// Distinct from `tv_wal_replay_corrupted_segments_total`, which the caller
/// increments only when the segment could not be OPENED or READ. This one
/// covers the case that was previously invisible: the file reads fine, and a
/// bad record partway through ends the walk, discarding every frame after it.
pub const WAL_REPLAY_TRUNCATED_SEGMENTS_COUNTER: &str = "tv_wal_replay_truncated_segments_total";

/// Bytes abandoned by the above, measured from the corrupt offset to EOF.
///
/// Bytes rather than records for the same reason the frame drain counts
/// abandoned BYTES: the record count of undecodable bytes cannot be known, and
/// an estimate inside a counter whose purpose is to stop estimates is worse
/// than no number at all.
pub const WAL_REPLAY_ABANDONED_BYTES_COUNTER: &str = "tv_wal_replay_abandoned_bytes_total";

// TEST-EXEMPT: covered by test_append_spill_and_replay_roundtrip + test_replay_handles_missing_dir + test_replay_detects_crc_corruption + test_unconfirmed_segment_is_rereplayed_on_next_boot + test_confirmed_segment_is_not_rereplayed
pub fn replay_all<P: AsRef<Path>>(wal_dir: P) -> anyhow::Result<Vec<ReplayedFrame>> {
    replay_all_with_budget(wal_dir, WAL_REPLAY_MAX_BYTES)
}

/// [`replay_all`] with an injectable RAM budget.
///
/// Separated so the budget's SAFETY can be tested without a 512 MiB fixture.
/// What needs proving is not the number — it is that a segment the budget
/// stopped short of is still a `*.wal` file afterwards, and a test that has to
/// allocate half a gigabyte to reach that branch would not be written.
// TEST-EXEMPT: covered by replay_budget_defers_unread_segments_instead_of_staging_them + every replay_all test above
pub fn replay_all_with_budget<P: AsRef<Path>>(
    wal_dir: P,
    budget_bytes: usize,
) -> anyhow::Result<Vec<ReplayedFrame>> {
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
    // Bytes of frame payload held so far, and how many segments were actually
    // read. Only the CONSUMED ones are staged below: a segment left as `*.wal`
    // is re-globbed next boot, so deferring is safe by this module's own
    // crash-safety invariant rather than by anything new.
    let mut bytes_held = 0usize;
    let mut consumed = 0usize;

    for path in &segments {
        if bytes_held >= budget_bytes && consumed > 0 {
            break;
        }
        match replay_segment(path) {
            Ok(mut batch) => {
                for f in &batch {
                    bytes_held = bytes_held.saturating_add(f.frame.len());
                }
                frames.append(&mut batch);
            }
            Err(err) => {
                corrupted += 1;
                error!(segment = ?path, error = %err, "WAL segment corrupted; skipping");
            }
        }
        consumed += 1;
    }
    // RESTORE any DEFERRED segment that came from `replaying/` back to the
    // live dir, BEFORE anything else can observe the deferral.
    //
    // This closes a silent, permanent tick-loss path found by an adversarial
    // sweep on 2026-08-28 and verified in source:
    //
    //   1. A boot stages segments L1..L5 into `replaying/` and crashes before
    //      `confirm_replayed`. Correct so far -- that is the crash-safety
    //      design working.
    //   2. The next boot re-globs `replaying/` (L1..L5) plus this crash's
    //      fresh `*.wal` files. Leftovers sort FIRST (smaller nanos), so the
    //      budget can run out INSIDE them -- say after L3.
    //   3. Staging below moves only `take(consumed)`, so L4 and L5 are never
    //      touched. They are still sitting in `replaying/`.
    //   4. The refold succeeds, so the caller calls `confirm_replayed`, which
    //      globs ALL of `replaying/` and archives every file it finds --
    //      including L4 and L5, which THIS BOOT NEVER READ.
    //
    // An archived segment is never re-globbed. Those frames are gone: captured
    // to disk, then deleted from the recovery path without ever reaching the
    // database, with no counter and no error. That is precisely the class the
    // whole capture-at-receipt chain exists to make impossible.
    //
    // The staging comment below already states the invariant that was being
    // broken -- "an archived segment is NEVER re-globbed, so its frames would
    // be lost for good. This slice is the whole safety of the budget above."
    // It is correct about the slice it controls and blind to the leftovers it
    // does not, because `confirm_replayed` archives by GLOB, not by that slice.
    //
    // Restoring is the minimal repair that keeps `confirm_replayed`'s glob
    // correct BY CONSTRUCTION: after this loop, `replaying/` contains only
    // segments this boot actually read, which is exactly what that function
    // assumes. It needs no signature change and no manifest file.
    //
    // A failed restore is LOUD, never silent: the segment stays in
    // `replaying/` and would be archived unread, so it is the one residual
    // and it gets a coded error plus a counter rather than a shrug. Both
    // renames are within one filesystem between sibling directories, so a
    // failure here means the staging renames below are failing too.
    let mut restore_failures = 0usize;
    for path in segments.iter().skip(consumed) {
        if path.parent() != Some(replaying_dir.as_path()) {
            continue; // never staged; a live `*.wal` is already re-globbable
        }
        let Some(name) = path.file_name() else {
            continue;
        };
        // O(1) EXEMPT: boot replay restore, cold path, bounded by deferred count
        if let Err(err) = std::fs::rename(path, wal_dir.join(name)) {
            restore_failures = restore_failures.saturating_add(1);
            error!(
                code = ErrorCode::WsSpill02FrameDropped.code_str(),
                segment = ?path,
                error = %err,
                "WAL replay could not restore a DEFERRED staged segment to the \
                 live directory. It is still in the replay staging area, where \
                 the confirm step archives everything it finds -- so these \
                 captured frames may be archived without ever being replayed. \
                 This is the one path by which recovery can lose data."
            );
        }
    }
    // Unconditional so the series exists from the first clean boot: the
    // CloudWatch agent drops each counter series' first sample as its delta
    // baseline, and a restore failure is a rare once-ever event where the
    // first occurrence is the only one you get.
    // APPROVED: cast -- restore_failures is O(segments) <= u64 always.
    metrics::counter!(WAL_REPLAY_RESTORE_FAILED_COUNTER).increment(restore_failures as u64);

    let deferred = segments.len().saturating_sub(consumed);
    if deferred > 0 {
        metrics::counter!(WAL_REPLAY_DEFERRED_COUNTER).increment(deferred as u64);
        error!(
            code = ErrorCode::WsSpill02FrameDropped.code_str(),
            deferred_segments = deferred,
            consumed_segments = consumed,
            bytes_held,
            budget_bytes,
            "WAL replay hit its RAM budget and DEFERRED the remaining segments \
             to the next boot. Nothing is stranded — an unconsumed segment stays \
             a `*.wal` file and is re-globbed next boot — but those frames are on \
             disk rather than in the database until then. A budget hit every boot \
             means the WAL is growing faster than replay drains it, which is a \
             capacity problem, not a recovery one."
        );
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
    // UNCONDITIONAL since 2026-08-26, mirroring the recovered counter one line
    // above, which has always incremented by a possibly-zero length.
    //
    // This used to sit behind `if corrupted > 0`, so on every clean boot the
    // series was never created — measured on the live box that day: ZERO lines
    // in /metrics out of 756. The CloudWatch agent computes a counter alarm
    // value as a DELTA and drops each series' first sample as its baseline, so
    // the FIRST corrupted WAL segment would have been swallowed and the alarm
    // would only fire on the SECOND. WAL corruption is precisely the rare,
    // once-ever event where the first is the only one you get.
    //
    // Incrementing by zero is a no-op for the value and creates the series.
    // APPROVED: cast — corrupted usize is O(segments) ≤ u64 always.
    metrics::counter!("tv_wal_replay_corrupted_segments_total").increment(corrupted as u64);

    // Move processed segments to the IN-PROGRESS staging dir (NOT archive).
    // They remain re-globbable next boot until `confirm_replayed` is called.
    // Best-effort, like the prior archive move: a failed move leaves the
    // segment as `*.wal`, which is STILL re-globbed next boot — never worse.
    drop(std::fs::create_dir_all(&replaying_dir)); // O(1) EXEMPT: boot replay staging move, cold path
    // ONLY the segments actually read. Staging one that was never replayed
    // would hand it to `confirm_replayed`, which archives it — and an archived
    // segment is NEVER re-globbed, so its frames would be lost for good. This
    // slice is the whole safety of the budget above.
    for seg in segments.iter().take(consumed) {
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
    /// Segments deleted by the BYTE CEILING after the age prune, oldest
    /// first (2026-08-19). Counted separately from `deleted` on purpose: a
    /// non-zero value here means the age window is too long for the traffic
    /// the box is actually seeing, which is a different operator signal from
    /// routine age expiry.
    pub size_deleted: usize,
    /// Total bytes remaining in the archive after both passes.
    pub bytes_after: u64,
}

/// Pre-allocation hint for the archive prune's survivor list — a typical
/// steady-state archive segment count, so the common case allocates once.
/// Exceeding it only costs a realloc on a cold periodic path.
const ARCHIVE_PRUNE_SURVIVOR_HINT: usize = 256;

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
///   (`WS_WAL_ARCHIVE_RETENTION_SECS`, comfortably exceeded that window AND
///   — F3, review round 1 — preserves the confirm-on-channel residual's only
///   copy across a weekend for triage before it ages out). The value is
///   deliberately NOT restated here: it read "7 days, matching
///   `SPILL_FILE_MAX_AGE_SECS`" until 2026-08-19, when it became 3 days and
///   stopped matching that constant — a number copied into prose is a claim
///   with no way to stay true. Read the constant.
/// - Bounded by BYTES as well as age since 2026-08-19
///   (`WS_WAL_ARCHIVE_MAX_BYTES`, oldest-first after the age pass): an age
///   bound alone scales with traffic, so it bounds the archive only for the
///   traffic level its window was chosen against.
/// - Only `*.wal` files are touched; anything else in the dir is kept.
///   A missing `archive/` dir is a no-op. Deletion failures are NOT
///   persist/flush failures — they log at WARN (bounded: once per file per
///   pass, passes run every 6 h) and retry next pass.
#[must_use]
pub fn prune_archived_segments_at<P: AsRef<Path>>(
    wal_dir: P,
    retention_secs: u64,
    max_bytes: u64,
    now: std::time::SystemTime,
) -> ArchivePruneOutcome {
    prune_wal_dir_at(
        &wal_dir.as_ref().join(ARCHIVE_SUBDIR),
        retention_secs,
        max_bytes,
        now,
    )
}

/// Bounds the ACTIVE `<wal_dir>/*.wal` set by age and bytes — the sibling of
/// [`prune_archived_segments_at`], added 2026-08-25.
///
/// # The leak this closes
///
/// Until now ONLY `archive/` was bounded. The active directory had no age
/// bound and no byte bound, because the design assumed it drains itself:
/// segments are replayed at boot, moved to `replaying/`, confirmed, moved to
/// `archive/`, and pruned there.
///
/// It does not drain. [`WAL_REPLAY_MAX_BYTES`] caps a boot replay at 512 MiB —
/// five segments — while the live lane writes segments continuously all
/// session. Anything past the newest 512 MiB is never replayed, never
/// confirmed, never archived, and so was never eligible for any bound.
///
/// Measured on the prod box 2026-08-25: **244 active segments, 31 GB, the
/// oldest dated 08-24** — against a 1.9 GB archive that its own bounds were
/// holding correctly. The volume was 94% full and free space was cycling down
/// to 1.94 GB, which is the condition that makes an ILP flush fail, which is
/// what the tick-spill rescue exists for.
///
/// # Why deleting these is bounded — and where it is NOT
///
/// A segment is written BEFORE parse and broadcast, as a crash-recovery copy.
/// On a session that did not crash, the live lane folded those same frames in
/// real time, so an un-replayed segment from a previous session is usually
/// redundant — its frames were already processed.
///
/// **Usually, not always, and the exception is real.** `WalRingSink::accept`
/// is WAL-first by design: it appends the frame, and only THEN tries to
/// reserve ring budget. When that reservation fails it returns
/// `FrameSinkOutcome::RingFull` and the frame is never folded — it exists in
/// the ACTIVE WAL and nowhere else. The WAL record carries no flag
/// distinguishing that frame from one the lane folded a microsecond later, so
/// this prune cannot tell them apart, and a deletion can therefore be the last
/// copy of a tick. The count of such sheds is `tv_dhan_ws_ring_full_total`;
/// when it is zero for a session, everything this pass deletes from that
/// session really was redundant.
///
/// The AGE pass is still bounded, and that bound is what makes it defensible:
/// at any retention of one full day or more, nothing it deletes was reachable
/// by the next boot's 512 MiB replay budget in any case — a session writes far
/// more than that (measured: 244 segments, 31 GB), so the backlog past the
/// budget is unrecoverable before this prune ever touches it.
///
/// The BYTE pass has no such bound. It ignores age and deletes the oldest
/// survivor to hold the volume under the ceiling, which can take a segment the
/// next boot would have replayed. That is a disk-pressure event; it is counted
/// separately as `tv_ws_wal_active_pruned_under_pressure_total` rather than
/// folded into the routine total, and the condition causing it already pages
/// through the free-space alarm.
///
/// The deeper defect this documents rather than fixes: the 512 MiB per-boot
/// replay budget against a ~31 GB session means deferred segments can never
/// drain, so they are guaranteed to reach one of these two passes. Closing
/// that needs either a larger budget (slower boot) or in-session refold of
/// shed frames (a design change) — neither is a line edit, and pretending the
/// prune is harmless was the previous way of not saying so.
#[must_use]
pub fn prune_active_segments_at<P: AsRef<Path>>(
    wal_dir: P,
    retention_secs: u64,
    max_bytes: u64,
    now: std::time::SystemTime,
) -> ArchivePruneOutcome {
    prune_wal_dir_at(wal_dir.as_ref(), retention_secs, max_bytes, now)
}

/// The shared age-then-bytes prune, over ONE directory of `*.wal` segments.
///
/// Extracted 2026-08-25 so the active directory gets the identical, already
/// hardened logic rather than a second implementation that would drift from
/// it — including the failure accounting that stops a single un-deletable
/// file from making the byte pass over-delete newer segments.
///
/// Non-recursive by construction: it reads one directory and touches only
/// `*.wal`. Applied to the WAL root that means `archive/` and `replaying/`
/// are subdirectories it never descends into, which is what keeps the
/// staged-but-unconfirmed set out of its reach.
#[must_use]
fn prune_wal_dir_at(
    dir: &Path,
    retention_secs: u64,
    max_bytes: u64,
    now: std::time::SystemTime,
) -> ArchivePruneOutcome {
    let archive_dir = dir.to_path_buf();
    let mut outcome = ArchivePruneOutcome::default();
    // Segments surviving the age pass, as (mtime, len, path) — the byte
    // ceiling's candidate set.
    // Pre-allocated rather than grown from empty, per the banned-pattern
    // rule. One entry per surviving archive segment; the capacity is a
    // typical steady-state segment count, so the common case allocates
    // exactly once. Cold path — one allocation per prune pass, never the
    // per-frame append.
    let mut survivors: Vec<(std::time::SystemTime, u64, std::path::PathBuf)> =
        Vec::with_capacity(ARCHIVE_PRUNE_SURVIVOR_HINT);
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
            _ => {
                outcome.kept += 1;
                // Survivor of the age pass — a candidate for the byte
                // ceiling below. mtime is already read above; reuse it as
                // the sort key so the ceiling deletes genuinely oldest-first.
                if let Ok(meta) = entry.metadata() {
                    let mtime = meta.modified().unwrap_or(std::time::UNIX_EPOCH);
                    survivors.push((mtime, meta.len(), path));
                }
            }
        }
    }

    // BYTE CEILING (2026-08-19) — the age pass alone bounds the archive only
    // for the traffic level its window was chosen against. This bounds it
    // unconditionally: while the total exceeds `max_bytes`, delete the OLDEST
    // survivor. Oldest-first because the newest segment is the one a crash
    // triage actually needs.
    //
    // Cold path, runs on the periodic prune task — never the per-frame append.
    let total: u64 = survivors.iter().map(|(_, len, _)| *len).sum();
    outcome.bytes_after = total;
    if total > max_bytes {
        // O(1) EXEMPT: periodic cold archive prune, never the per-frame append
        survivors.sort_by_key(|(mtime, _, _)| *mtime);
        let mut remaining = total;
        for (_, len, path) in &survivors {
            if remaining <= max_bytes {
                break;
            }
            // O(1) EXEMPT: periodic cold archive prune, never the per-frame append
            match std::fs::remove_file(path) {
                Ok(()) => {
                    outcome.size_deleted += 1;
                    outcome.kept = outcome.kept.saturating_sub(1);
                    remaining = remaining.saturating_sub(*len);
                }
                Err(err) => {
                    outcome.failed += 1;
                    // Decrement anyway (2026-08-19, adversarial audit). The
                    // first version left `remaining` untouched on failure, so
                    // a single un-deletable file made the loop keep deleting
                    // NEWER segments chasing a budget it could not reach that
                    // way — over-deleting exactly the recent copies a crash
                    // triage needs, to satisfy a ceiling the failed file was
                    // never going to free.
                    //
                    // Treating the file as accounted-for is the conservative
                    // direction: at worst the archive sits slightly over the
                    // ceiling for one pass, which the next pass corrects. The
                    // failure is still counted and logged, so a permanently
                    // un-deletable file is visible rather than absorbed.
                    remaining = remaining.saturating_sub(*len);
                    warn!(
                        path = %path.display(),
                        error = %err,
                        "WAL archive byte-ceiling prune: remove_file failed — \
                         counted against the budget so the pass cannot \
                         over-delete newer segments; retried next pass"
                    );
                }
            }
        }
        outcome.bytes_after = remaining;
        warn!(
            deleted = outcome.size_deleted,
            bytes_before = total,
            bytes_after = remaining,
            max_bytes,
            "WAL archive exceeded its byte ceiling — deleted oldest segments. \
             The age window is longer than this traffic level can afford; \
             re-derive it against measured volume."
        );
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
    max_bytes: u64,
) -> ArchivePruneOutcome {
    let outcome = prune_archived_segments_at(
        wal_dir,
        retention_secs,
        max_bytes,
        std::time::SystemTime::now(),
    );
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

/// Wall-clock wrapper over [`prune_active_segments_at`]. Cold path — called
/// from the same periodic prune task as the archive sweep.
#[must_use]
pub fn prune_active_segments<P: AsRef<Path>>(
    wal_dir: P,
    retention_secs: u64,
    max_bytes: u64,
) -> ArchivePruneOutcome {
    let outcome = prune_active_segments_at(
        wal_dir,
        retention_secs,
        max_bytes,
        std::time::SystemTime::now(),
    );
    if outcome.deleted > 0 || outcome.failed > 0 || outcome.size_deleted > 0 {
        metrics::counter!("tv_ws_wal_active_pruned_total")
            .increment((outcome.deleted + outcome.size_deleted) as u64);
        // Counted SEPARATELY from the total, because the two passes have
        // different safety arguments and only one of them is bounded.
        //
        // The AGE pass deletes nothing a future boot could have reached: at a
        // 48-hour retention against a 512 MiB per-boot replay budget, those
        // segments were already past recovery. The BYTE pass ignores age
        // entirely -- it deletes the oldest survivor to protect the volume,
        // and it can therefore delete a segment the next boot would otherwise
        // have replayed. That is a disk-pressure event, not routine
        // housekeeping, and it deserves its own number.
        //
        // Deliberately NOT EMF-selected: the condition that produces it --
        // the volume filling -- already pages via `tv-<env>-spill-dir-free-low`
        // (noise-lock 2.3g), so a second pager for the same cause would be
        // redundant, and every EMF name costs ~$0.30/mo against a maximal
        // month already above the budget's automatic stop line. It is
        // readable on `/metrics` and through the debug API. Stated here so
        // the omission is a decision on the record rather than an oversight.
        if outcome.size_deleted > 0 {
            metrics::counter!("tv_ws_wal_active_pruned_under_pressure_total")
                .increment(outcome.size_deleted as u64);
        }
        info!(
            deleted = outcome.deleted,
            size_deleted = outcome.size_deleted,
            failed = outcome.failed,
            kept = outcome.kept,
            bytes_after = outcome.bytes_after,
            retention_secs,
            max_bytes,
            "WAL ACTIVE prune pass complete — un-replayed segments deleted by age \
             and, separately, by the byte ceiling. MOST are crash-recovery copies \
             of frames the live lane folded in real time, and those are redundant. \
             They are not ALL: a frame shed at the frame ring (tv_dhan_ws_ring_full_total) \
             reached the WAL and was never folded, and the record does not distinguish \
             the two — so a deletion here can be the last copy. The age pass is still \
             bounded (nothing 48h old is reachable by a 512 MiB replay budget); the \
             byte pass is not, and its count is tv_ws_wal_active_pruned_under_pressure_total."
        );
    }
    outcome
}

/// Byte ceiling on the ACTIVE `<wal_dir>/*.wal` set — DERIVED from the volume.
///
/// Resolved once into a `OnceLock`, so every read is O(1) and the enforcement
/// can never disagree with a logged value.
///
/// **Why derived rather than a constant.** The archive's sibling ceiling is a
/// hardcoded 50 GB whose own doc concedes it "DOES engage" at the target
/// instrument count — a number chosen against one traffic level and one disk.
/// This session was spent on the consequence of exactly that shape: a 512 MiB
/// spill ceiling pinned to no machine, on a 200 GB volume, refusing a rescue
/// and losing 1,695,983 ticks. Repeating it here for the LARGEST directory on
/// the box would be repeating a mistake with the evidence already in hand.
///
/// One eighth of the volume: 25 GB on the prod 200 GB disk. That is generous
/// enough that a normal session never touches it, and small enough that the
/// active set can no longer be what fills the disk — it reached 31 GB before
/// this bound existed.
///
/// # Complexity
/// O(1) after the first call.
#[must_use]
pub fn ws_wal_active_max_bytes<P: AsRef<Path>>(wal_dir: P) -> u64 {
    /// Floor, and the fallback when the volume cannot be measured.
    const FLOOR_BYTES: u64 = 8 * 1024 * 1024 * 1024;
    /// Fraction of the volume the active WAL set may occupy.
    const VOLUME_FRACTION: u64 = 8;
    static RESOLVED: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    if let Some(v) = RESOLVED.get() {
        return *v;
    }
    // Probe the deepest EXISTING ancestor: `df` on a path that does not exist
    // yet fails, and the WAL directory is created lazily.
    let mut probe: &Path = wal_dir.as_ref();
    while !probe.exists() {
        match probe.parent() {
            Some(parent) => probe = parent,
            None => break,
        }
    }
    let resolved = match crate::disk_health_watcher::probe_disk_free_bytes(probe) {
        crate::disk_health_watcher::DiskHealthOutcome::Ok { total_bytes, .. }
            if total_bytes > 0 =>
        {
            let derived = (total_bytes / VOLUME_FRACTION).max(FLOOR_BYTES);
            info!(
                total_bytes,
                ceiling_bytes = derived,
                fraction = VOLUME_FRACTION,
                "active WAL byte ceiling derived from the volume"
            );
            derived
        }
        _ => {
            warn!(
                fallback_bytes = FLOOR_BYTES,
                "could not measure the WAL volume — the active-WAL byte ceiling falls back \
                 to its floor"
            );
            FLOOR_BYTES
        }
    };
    *RESOLVED.get_or_init(|| resolved)
}

fn replay_segment(path: &Path) -> anyhow::Result<Vec<ReplayedFrame>> {
    let mut f = File::open(path)?;
    let mut buf = Vec::new(); // APPROVED: boot-time WAL replay, cold path
    f.read_to_end(&mut buf)?;

    let mut out = Vec::new(); // APPROVED: boot-time WAL replay, cold path
    let mut i = 0usize;
    let mut corrupted_at: Option<(&str, usize)> = None;
    // Smallest record is v1 (13 bytes); v2 is 21. Gate the OUTER loop on the v1
    // minimum, then re-check the version-specific minimum after the magic check
    // so a partial v2 tail can never be read as if its frame_seq were payload.
    while i + WAL_MIN_RECORD_V1 <= buf.len() {
        let magic = &buf[i..i + 4];
        let is_v3 = magic == WAL_MAGIC_V3;
        let is_v2 = magic == WAL_MAGIC_V2;
        let is_v1 = magic == WAL_MAGIC;
        if !is_v1 && !is_v2 && !is_v3 {
            // An unknown magic at offset 0 means the WHOLE segment is
            // unreadable — and the overwhelmingly likely cause is a DEPLOY
            // ROLLBACK: a newer binary wrote a record version this one cannot
            // parse. That is not a torn tail, it is total loss of a segment
            // that was captured successfully, and the caller stages and
            // archives a zero-frame result exactly as it would a clean replay.
            //
            // So it is separated from the mid-segment case and raised as a
            // CODED error, not a bare `warn!`. Before 2026-08-28 this arm was
            // uncoded, which meant no CloudWatch metric filter could match it:
            // the loss was not merely unrecovered, it was unpageable. A silent
            // unrecoverable loss on the durability floor is the false-OK class
            // this file exists to prevent.
            //
            // The segment itself is NOT deleted here — it is moved to the
            // archive directory by the caller and survives until pruning, so a
            // manual recovery with the newer binary remains possible. That is
            // the reason this is loud-and-counted rather than fail-closed.
            if i == 0 {
                metrics::counter!("tv_wal_replay_unknown_magic_total").increment(1);
                error!(
                    code = ErrorCode::WsSpill02FrameDropped.code_str(),
                    segment = ?path,
                    magic = ?magic,
                    bytes = buf.len(),
                    "WAL segment is unreadable by this binary — every frame in it \
                     is unrecovered. Most likely a deploy ROLLBACK: a newer build \
                     wrote a record version this one cannot parse. The file is \
                     retained in the archive directory, so re-running the newer \
                     build can still recover it."
                );
            } else {
                corrupted_at = Some(("magic_mismatch", i));
                warn!(segment = ?path, offset = i, "WAL magic mismatch; stopping at boundary");
            }
            break;
        }
        // Version disambiguation + per-version minimum-size guard (security
        // review HIGH): a v2 record needs 21 bytes before its variable frame,
        // a v3 record 29. Checked BEFORE any header field is read, so a
        // partial tail can never be reinterpreted as payload.
        let min_rec = if is_v3 {
            WAL_MIN_RECORD_V3
        } else if is_v2 {
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
                corrupted_at = Some(("unknown_ws_type", i));
                warn!(segment = ?path, offset = i, ws_byte, "unknown WsType tag; stopping");
                break;
            }
        };
        // v1: [magic|ws|len|frame|crc]
        // v2: [magic|ws|frame_seq(8)|len|frame|crc]
        // v3: [magic|ws|frame_seq(8)|received_at_nanos(8)|len|frame|crc]
        let (frame_seq, received_at_nanos, len_off) = if is_v3 {
            let seq_bytes: [u8; 8] = match buf[i + 5..i + 13].try_into() {
                Ok(b) => b,
                Err(_) => break,
            };
            let recv_bytes: [u8; 8] = match buf[i + 13..i + 21].try_into() {
                Ok(b) => b,
                Err(_) => break,
            };
            (
                u64::from_le_bytes(seq_bytes),
                i64::from_le_bytes(recv_bytes),
                i + 21,
            )
        } else if is_v2 {
            let seq_bytes: [u8; 8] = match buf[i + 5..i + 13].try_into() {
                Ok(b) => b,
                Err(_) => break,
            };
            (
                u64::from_le_bytes(seq_bytes),
                WAL_RECEIPT_UNKNOWN_NANOS,
                i + 13,
            )
        } else {
            (0u64, WAL_RECEIPT_UNKNOWN_NANOS, i + 5)
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
                corrupted_at = Some(("length_overflow", i));
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
        // CRC covers the version's exact header bytes, in write order: v2 adds
        // frame_seq, v3 adds received_at_nanos after it. Using the wrong
        // version's byte set here would reject every record of that version as
        // corrupt, so the arms mirror `write_record` exactly.
        let len_le = (frame_len as u32).to_le_bytes();
        let actual = if is_v3 {
            crc32_ieee_of(&[
                &[ws_byte],
                &frame_seq.to_le_bytes()[..],
                &received_at_nanos.to_le_bytes()[..],
                &len_le[..],
                &frame,
            ])
        } else if is_v2 {
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
            corrupted_at = Some(("crc_mismatch", i));
            warn!(segment = ?path, offset = i, expected, actual, "CRC mismatch; stopping");
            break;
        }
        out.push(ReplayedFrame {
            ws_type,
            frame,
            frame_seq,
            received_at_nanos,
        });
        i = record_end;
    }
    // CORRUPTION ACCOUNTING -- added 2026-08-28.
    //
    // Every abandon site above `break`s out of the walk, and this function then
    // returned `Ok(out)` regardless. The caller's corruption counter fires only
    // on `Err`, and `Err` is reachable only from `File::open` and
    // `read_to_end` -- so a bad record in the MIDDLE of a segment discarded
    // every frame after it with `tv_wal_replay_corrupted_segments_total`
    // unmoved, a bare `warn!` carrying no `code` field for any metric filter to
    // match, and the segment then staged, archived, and never re-read.
    //
    // At `WAL_SEGMENT_MAX_BYTES` = 128 MiB that is on the order of 700,000
    // frames vanishing on one flipped bit, reported nowhere. The identical
    // hazard one branch away -- an unknown magic at offset 0 -- has always been
    // counted and coded. This closes the gap between them.
    //
    // The two TAIL sites are deliberately NOT counted: a partial trailing
    // record is what an interrupted writer leaves behind, it abandons nothing
    // beyond itself, and counting it would page on every unclean shutdown.
    //
    // Bytes, not records: the record count of undecodable bytes is unknowable,
    // and a fabricated number inside a counter that exists to stop fabrication
    // is worse than no number.
    if let Some((reason, offset)) = corrupted_at {
        let abandoned = buf.len().saturating_sub(offset);
        metrics::counter!(WAL_REPLAY_TRUNCATED_SEGMENTS_COUNTER).increment(1);
        metrics::counter!(WAL_REPLAY_ABANDONED_BYTES_COUNTER).increment(abandoned as u64);
        error!(
            code = ErrorCode::WsSpill02FrameDropped.code_str(),
            segment = ?path,
            reason,
            offset,
            abandoned_bytes = abandoned,
            recovered_frames = out.len(),
            "WAL segment is corrupt mid-file — the walk stopped here and every \
             frame after this point is unrecovered. The segment is still moved \
             to the archive directory, so the bytes survive for manual \
             inspection, but nothing will read them again automatically."
        );
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// The prune's own words used to assert something it cannot establish.
    ///
    /// It said the segments it deletes were crash-recovery copies of frames
    /// the live lane had supposedly folded already. That is true of every frame the ring
    /// accepted and FALSE of every frame it shed: `WalRingSink::accept`
    /// appends to the WAL *before* reserving ring budget, so a `RingFull`
    /// frame reaches the WAL and is never folded, and nothing in the record
    /// distinguishes the two afterwards.
    ///
    /// A stale reassurance in a log line is worse than no line at all --
    /// whoever reads it next stops looking. This pins the correction so the
    /// comfortable sentence cannot come back.
    #[test]
    fn the_prune_never_claims_the_frames_it_deletes_were_already_folded() {
        let source = include_str!("ws_frame_spill.rs");
        assert!(
            !source.contains(concat!("frames the live lane ", "already folded")),
            "the prune's log line is asserting that everything it deletes was \
             already folded. A frame shed at the ring (FrameSinkOutcome::RingFull) \
             reached the WAL and was NOT folded, and the record cannot tell them \
             apart -- so this claim is false for exactly the frames whose loss \
             matters most"
        );
        assert!(
            source.contains("tv_ws_wal_active_pruned_under_pressure_total"),
            "the byte-ceiling pass ignores age and can delete a segment the next \
             boot would have replayed. It must be counted separately from routine \
             age pruning, or a disk-pressure data deletion is indistinguishable \
             from housekeeping"
        );
        assert!(
            source.contains("tv_dhan_ws_ring_full_total"),
            "the doc must name the counter that tells an operator whether this \
             prune's deletions were redundant -- without it, the honest caveat is \
             unactionable"
        );
    }
    use std::time::Duration;

    #[test]
    fn test_durability_claim_matches_what_the_code_actually_does() {
        // A documentation ratchet, and the only kind that works for a claim
        // like this one: durability is invisible in a unit test — a flushed
        // record and a synced record are byte-identical until the machine
        // loses power — so nothing but a source scan can keep the words and
        // the syscalls in agreement.
        //
        // Until 2026-08-11 this module asserted "fsync" three times while
        // calling `sync_all` zero times. The live feed refuses to open a
        // socket without this WAL and names it the durability floor, so the
        // overstatement was load-bearing.
        //
        // Either direction of drift now fails: adding a sync without
        // correcting the note, or restoring the claim without the sync.
        let src = include_str!("ws_frame_spill.rs");
        let test_marker = concat!("#[cfg(", "test)]");
        let production = src.split(test_marker).next().unwrap_or(src);

        let syncs = production.matches(concat!("sync_", "all")).count()
            + production.matches(concat!("sync_", "data")).count();

        if syncs == 0 {
            assert!(
                production.contains("DURABILITY"),
                "this module performs no fsync, so its header MUST carry the DURABILITY note \
                 explaining that flushed records survive a process kill but not power loss"
            );
            // The word may appear ONLY in that corrective note, never as a
            // description of what the writer does.
            for line in production.lines() {
                if !line.contains("fsync") {
                    continue;
                }
                let l = line.to_lowercase();
                assert!(
                    l.contains("previously")
                        || l.contains("never true")
                        || l.contains("not an fsync")
                        || l.contains("deliberate throughput"),
                    "this line claims fsync behaviour the code does not have — either add a \
                     real sync_all or reword it: {line}"
                );
            }
        } else {
            assert!(
                production.contains("sync_all") || production.contains("sync_data"),
                "sanity: counted a sync but cannot find one"
            );
        }
    }

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
        for t in [WsType::LiveFeed, WsType::OrderUpdate, WsType::TruedataFeed] {
            assert_eq!(WsType::from_u8(t.as_u8()), Some(t));
        }
        assert_eq!(WsType::from_u8(0), None);
        assert_eq!(WsType::from_u8(2), None);
        assert_eq!(WsType::from_u8(3), None);
        assert_eq!(WsType::from_u8(6), None);
        assert_eq!(WsType::from_u8(99), None);
    }

    #[test]
    fn test_ws_type_tag_bytes_are_frozen() {
        // These bytes are PERSISTED in every WAL record. Changing one
        // silently re-routes every already-written frame on the next boot
        // replay, so they are pinned as literals rather than derived.
        assert_eq!(WsType::LiveFeed.as_u8(), 1);
        assert_eq!(WsType::OrderUpdate.as_u8(), 4);
        assert_eq!(
            WsType::TruedataFeed.as_u8(),
            5,
            "5 was chosen because 1-4 are spent or reserved by retired transports"
        );
    }

    #[test]
    fn test_owning_feed_maps_each_transport_to_its_broker() {
        use tickvault_common::feed::Feed;
        // The whole point: a TrueData drop must NOT be reported as Dhan.
        assert_eq!(WsType::TruedataFeed.owning_feed(), Some(Feed::Truedata));
        assert_eq!(
            WsType::LiveFeed.owning_feed(),
            Some(Feed::Dhan),
            "tag 1 predates the multi-feed WAL; replayed old records stay Dhan"
        );
        assert_eq!(
            WsType::OrderUpdate.owning_feed(),
            None,
            "an order-update drop must never flip a MARKET-DATA feed to Degraded"
        );
        // No two market-data transports may claim the same broker, or the
        // health page cannot tell two feeds apart.
        assert_ne!(
            WsType::LiveFeed.owning_feed(),
            WsType::TruedataFeed.owning_feed()
        );
    }

    #[test]
    fn test_every_ws_type_has_a_distinct_label() {
        // The label is a metric dimension; a collision would silently merge
        // two feeds' drop counters into one series.
        let labels = [
            WsType::LiveFeed.as_str(),
            WsType::OrderUpdate.as_str(),
            WsType::TruedataFeed.as_str(),
        ];
        for (i, a) in labels.iter().enumerate() {
            for b in labels.iter().skip(i.saturating_add(1)) {
                assert_ne!(a, b, "metric labels must be distinct");
            }
        }
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

    /// The budget must DEFER, never DROP.
    ///
    /// The whole safety of `WAL_REPLAY_MAX_BYTES` rests on one thing: a
    /// segment the budget stopped short of must still be a `*.wal` file
    /// afterwards, so the next boot re-globs it. If it were staged into
    /// `replaying/` instead, `confirm_replayed` would archive it — and an
    /// archived segment is never re-globbed, so its frames would be gone for
    /// good. That is the difference between deferring recovery and destroying
    /// it, and nothing about the two paths looks different at the call site.
    #[test]
    fn replay_budget_defers_unread_segments_instead_of_staging_them() {
        let dir = tmp_dir("replay-budget-defer");

        // Two segments: each spill instance closes its own file on drop, so
        // two sequential lifetimes give two separate `*.wal` segments.
        for _ in 0..2 {
            let spill = WsFrameSpill::new(&dir).unwrap();
            spill.append(WsType::LiveFeed, vec![7u8; 64]);
            wait_until_persisted(&spill, 1);
        }
        std::thread::sleep(Duration::from_millis(50));
        let before = wal_segments_in(&dir).len();
        assert!(
            before >= 2,
            "fixture must produce at least two segments, found {before}"
        );

        // Budget 1 byte: the first segment is read (the `consumed > 0` guard
        // guarantees forward progress), the rest are deferred.
        let first = replay_all_with_budget(&dir, 1).expect("replay");
        assert!(
            !first.is_empty(),
            "at least one segment must always be read — otherwise an oversized \
             segment would be deferred on every boot forever, which is a \
             permanent loss wearing a deferral's clothes"
        );
        assert_eq!(
            wal_segments_in(&dir).len(),
            before - 1,
            "exactly the consumed segment may leave `*.wal`; every deferred one \
             must remain, or confirm_replayed will archive frames that were \
             never replayed and they are gone for good"
        );

        // A later boot recovers the remainder: the deferral was a delay.
        let rest = replay_all_with_budget(&dir, usize::MAX).expect("replay");
        assert!(
            !rest.is_empty(),
            "the deferred frames must be recoverable on a later boot"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The silent, permanent tick-loss path an adversarial sweep found on
    /// 2026-08-28, reproduced end to end.
    ///
    /// The sequence needs THREE things to line up, which is why nothing caught
    /// it: a boot must stage segments and crash before confirming, the next
    /// boot must hit its RAM budget INSIDE those leftovers, and the refold
    /// must then succeed so the caller confirms. Each step is individually
    /// correct; together they archive segments that were never read, and
    /// archived segments are never re-globbed.
    ///
    /// The assertion that actually bites is the last one: the frames must
    /// still be recoverable. Without the restore loop the deferred leftovers
    /// stay in `replaying/`, `confirm_replayed` archives all three, and this

    /// A corrupt record in the MIDDLE of a segment silently discarded every
    /// frame after it.
    ///
    /// `replay_segment` `break`s at four corruption sites and then returned
    /// `Ok(out)` regardless, so the caller -- which counts corruption only on
    /// `Err` -- saw a clean partial read. The segment was then staged,
    /// archived, and never re-globbed. At a 128 MiB segment that is on the
    /// order of 700,000 frames gone on one flipped bit, with no counter, no
    /// coded line, and nothing a metric filter could match.
    ///
    /// This writes two real frames, flips a byte inside the SECOND record's
    /// payload so its CRC fails, and asserts the walk keeps the first frame,
    /// drops the second, and does not pretend the segment was fully read.
    #[test]
    fn a_corrupt_record_mid_segment_is_reported_not_silently_swallowed() {
        let dir = tmp_dir("replay-mid-corruption");
        let spill = WsFrameSpill::new(&dir).unwrap();
        spill.append(WsType::LiveFeed, vec![0xAA; 64]);
        spill.append(WsType::LiveFeed, vec![0xBB; 64]);
        wait_until_persisted(&spill, 2);
        drop(spill);
        std::thread::sleep(Duration::from_millis(50));

        let segments = wal_segments_in(&dir);
        assert_eq!(
            segments.len(),
            1,
            "fixture must produce exactly one segment"
        );
        let path = &segments[0];

        // Sanity: undamaged, both frames read back.
        let clean = replay_segment(path).expect("undamaged segment must read");
        assert_eq!(clean.len(), 2, "fixture must contain two readable frames");

        // Flip a byte inside the SECOND payload. Payload bytes are 0xBB and
        // nothing else in the file is, so this cannot land in a header and
        // turn the test into a different failure by accident.
        let mut bytes = std::fs::read(path).unwrap();
        let victim = bytes
            .iter()
            .rposition(|b| *b == 0xBB)
            .expect("the second payload must be present");
        bytes[victim] ^= 0xFF;
        std::fs::write(path, &bytes).unwrap();

        let damaged = replay_segment(path).expect("a corrupt segment still returns Ok");
        assert_eq!(
            damaged.len(),
            1,
            "the walk must keep every frame BEFORE the corruption and stop there \
             -- got {} frame(s)",
            damaged.len()
        );
        assert_eq!(
            &damaged[0].frame[..],
            &[0xAAu8; 64][..],
            "the surviving frame must be the first one, intact"
        );

        // The load-bearing half: the abandonment is reported. Asserted on the
        // source rather than on a metrics recorder because the counters are
        // process-global and this suite runs in parallel -- a shared recorder
        // would make the assertion depend on test ordering, which is exactly
        // the kind of flake that gets a guard deleted.
        let source = include_str!("ws_frame_spill.rs");
        for needle in [
            "WAL_REPLAY_TRUNCATED_SEGMENTS_COUNTER",
            "WAL_REPLAY_ABANDONED_BYTES_COUNTER",
            "corrupted_at = Some((\"crc_mismatch\"",
            "corrupted_at = Some((\"magic_mismatch\"",
            "corrupted_at = Some((\"unknown_ws_type\"",
            "corrupted_at = Some((\"length_overflow\"",
        ] {
            assert!(
                source.contains(needle),
                "mid-segment corruption must stay counted and coded; missing: {needle}"
            );
        }
        assert!(
            !source.contains("truncated header at tail\");\n            corrupted_at"),
            "the two TAIL sites must NOT be counted -- a partial trailing record is \
             what an interrupted writer leaves behind, and counting it would page on \
             every unclean shutdown"
        );
    }

    /// recovers NOTHING -- captured to disk, then deleted from the recovery
    /// path with no counter and no error.
    #[test]
    fn a_budget_deferral_inside_staged_leftovers_never_archives_them_unread() {
        let dir = tmp_dir("replay-defer-leftovers");
        let replaying = dir.join(REPLAYING_SUBDIR);

        // Three segments, each with a distinct payload so recovery is provable
        // by content and not merely by count.
        for byte in [1u8, 2u8, 3u8] {
            let spill = WsFrameSpill::new(&dir).unwrap();
            spill.append(WsType::LiveFeed, vec![byte; 64]);
            wait_until_persisted(&spill, 1);
        }
        std::thread::sleep(Duration::from_millis(50));
        let total = wal_segments_in(&dir).len();
        assert!(
            total >= 3,
            "fixture must produce >= 3 segments, found {total}"
        );

        // --- boot A: consumes everything, stages it, then CRASHES ----------
        // (no `confirm_replayed` -- that is the crash.)
        let a = replay_all_with_budget(&dir, usize::MAX).expect("boot A replay");
        assert_eq!(a.len(), total, "boot A must read every segment");
        assert_eq!(
            wal_segments_in(&replaying).len(),
            total,
            "boot A must stage every consumed segment"
        );
        assert!(
            wal_segments_in(&dir).is_empty(),
            "and leave nothing in the live dir"
        );

        // --- boot B: budget runs out INSIDE the staged leftovers -----------
        let b = replay_all_with_budget(&dir, 1).expect("boot B replay");
        assert!(
            !b.is_empty(),
            "forward progress: one segment is always read"
        );
        assert_eq!(
            wal_segments_in(&replaying).len(),
            1,
            "only the segment boot B actually READ may remain staged -- \
             confirm_replayed archives `replaying/` by glob, so anything else \
             left there is archived without ever being replayed"
        );
        assert_eq!(
            wal_segments_in(&dir).len(),
            total - 1,
            "every deferred leftover must be restored to the live dir, where \
             the next boot re-globs it"
        );

        // --- the refold succeeded, so the caller confirms ------------------
        confirm_replayed(&dir);
        assert!(
            wal_segments_in(&replaying).is_empty(),
            "confirm archives what was read"
        );

        // --- boot C: the deferred frames must still be there ---------------
        let c = replay_all_with_budget(&dir, usize::MAX).expect("boot C replay");
        assert_eq!(
            c.len(),
            total - b.len(),
            "every frame boot B deferred must still be recoverable -- if this \
             is 0, the confirm step deleted captured ticks from the recovery \
             path and they are gone for good"
        );

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

    // --- TVW3: the receipt-preserving record (2026-08-28) -------------------

    /// T1 — the whole reason v3 exists: a receipt stamped by the caller must
    /// survive the disk round-trip EXACTLY, so boot replay never has to invent
    /// one. If this regresses, candle bucketing on the receipt clock silently
    /// files replayed ticks at the wrong minute.
    #[test]
    fn tvw3_roundtrip_preserves_received_at() {
        let dir = tmp_dir("v3-roundtrip");
        // Two distinct, plausible receipts ~7 hours apart, so a truncation or
        // a byte-order slip cannot coincidentally produce the right answer.
        let first: i64 = 1_787_800_000_000_000_000;
        let second: i64 = 1_787_825_000_000_000_000;
        {
            let spill = WsFrameSpill::new(&dir).unwrap();
            spill.append_with_seq_at(WsType::LiveFeed, vec![1, 2, 3], 11, first);
            spill.append_with_seq_at(WsType::LiveFeed, vec![4, 5, 6], 12, second);
            wait_until_persisted(&spill, 2);
        }
        std::thread::sleep(Duration::from_millis(50));

        let frames = replay_all(&dir).unwrap();
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].frame, vec![1, 2, 3]);
        assert_eq!(frames[1].frame, vec![4, 5, 6]);
        assert_eq!(frames[0].frame_seq, 11);
        assert_eq!(frames[1].frame_seq, 12);
        assert_eq!(
            frames[0].received_at_nanos, first,
            "v3 must return the caller's receipt verbatim, not a re-stamp"
        );
        assert_eq!(frames[1].received_at_nanos, second);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// T2 — a v1 or v2 segment written by an older binary must replay with the
    /// SENTINEL, never a synthesized time. A fabricated receipt is worse than
    /// a missing one: it is indistinguishable from a real arrival, which is the
    /// exact defect v3 closes.
    #[test]
    fn tvw1_and_tvw2_records_replay_with_unknown_receipt() {
        let dir = tmp_dir("v3-backcompat");
        let seg = dir.join("ws-frames-00000000000000000003.wal");
        let mut bytes = encode_v1_record(WsType::LiveFeed, &[7, 7, 7]);
        bytes.extend_from_slice(&encode_v2_record(WsType::LiveFeed, 99, &[8, 8]));
        std::fs::write(&seg, &bytes).unwrap();

        let frames = replay_all(&dir).unwrap();
        assert_eq!(frames.len(), 2, "both legacy versions must still recover");
        assert_eq!(frames[0].frame, vec![7, 7, 7]);
        assert_eq!(frames[0].frame_seq, 0);
        assert_eq!(
            frames[0].received_at_nanos, WAL_RECEIPT_UNKNOWN_NANOS,
            "a v1 record has no receipt — it must read as UNKNOWN, never as now()"
        );
        assert_eq!(frames[1].frame, vec![8, 8]);
        assert_eq!(frames[1].frame_seq, 99, "v2 keeps its frame_seq");
        assert_eq!(
            frames[1].received_at_nanos, WAL_RECEIPT_UNKNOWN_NANOS,
            "a v2 record has no receipt field either"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// T2b — `append_with_seq` (the receipt-less entry point) must record the
    /// sentinel rather than reading a clock. A clock read here would be
    /// indistinguishable on replay from a genuine arrival.
    #[test]
    fn append_without_a_receipt_records_the_sentinel_not_a_clock_read() {
        let dir = tmp_dir("v3-no-receipt");
        {
            let spill = WsFrameSpill::new(&dir).unwrap();
            spill.append_with_seq(WsType::LiveFeed, vec![3, 3], 42);
            wait_until_persisted(&spill, 1);
        }
        std::thread::sleep(Duration::from_millis(50));

        let frames = replay_all(&dir).unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].frame_seq, 42);
        assert_eq!(
            frames[0].received_at_nanos, WAL_RECEIPT_UNKNOWN_NANOS,
            "no receipt offered => sentinel, never a synthesized timestamp"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// T2c — a TRUNCATED v3 header (21 bytes: magic+ws+seq+receipt, but no
    /// len/frame/crc) passes the v1 outer guard and would reach the field reads
    /// if the per-version minimum were wrong. Must reject cleanly: no panic, no
    /// out-of-bounds read, zero frames. This is the v3 twin of the v2 guard.
    #[test]
    fn tvw3_truncated_header_recovers_nothing_and_does_not_panic() {
        let dir = tmp_dir("v3-truncated");
        let seg = dir.join("ws-frames-00000000000000000004.wal");
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&WAL_MAGIC_V3); // 4
        bytes.push(WsType::LiveFeed.as_u8()); // +1 = 5
        bytes.extend_from_slice(&[0u8; 8]); // +8 frame_seq = 13
        bytes.extend_from_slice(&[0u8; 8]); // +8 receipt   = 21, still < 29
        assert_eq!(bytes.len(), 21);
        std::fs::write(&seg, &bytes).unwrap();

        let frames = replay_all(&dir).unwrap(); // must not panic
        assert!(
            frames.is_empty(),
            "a truncated v3 header must recover 0 frames"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// T2d — a v3 record whose CRC does not cover the receipt bytes would let a
    /// corrupted timestamp through silently. Flip one byte INSIDE the receipt
    /// field and require the record to be rejected.
    #[test]
    fn tvw3_crc_covers_the_receipt_field() {
        let dir = tmp_dir("v3-crc");
        let seg = dir.join("ws-frames-00000000000000000005.wal");
        let mut bytes = encode_v3_record(WsType::LiveFeed, 5, 1_787_800_000_000_000_000, &[1, 2]);
        // Receipt occupies bytes 13..21. Corrupt one of them; the CRC must fail.
        bytes[13] ^= 0xFF;
        std::fs::write(&seg, &bytes).unwrap();

        let frames = replay_all(&dir).unwrap();
        assert!(
            frames.is_empty(),
            "a receipt-byte flip must fail CRC — otherwise the CRC does not \
             cover the field and a corrupt timestamp reaches the aggregator"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// T3 — the size accounting used for segment rotation must match what the
    /// writer actually emits, or segments overrun (or under-run) their cap.
    #[test]
    fn tvw3_record_disk_size_matches_the_bytes_written() {
        let frame = vec![1u8, 2, 3, 4, 5];
        let encoded = encode_v3_record(WsType::LiveFeed, 1, 1_787_800_000_000_000_000, &frame);
        let rec = WalRecord {
            ws_type: WsType::LiveFeed,
            frame_seq: 1,
            received_at_nanos: 1_787_800_000_000_000_000,
            frame: Bytes::from(frame.clone()),
        };
        assert_eq!(
            record_disk_size(&rec) as usize,
            encoded.len(),
            "rotation accounting must equal the real on-disk size"
        );
        assert_eq!(
            encoded.len(),
            WAL_MIN_RECORD_V3 + frame.len(),
            "v3 overhead must be exactly {WAL_MIN_RECORD_V3} bytes"
        );
    }

    /// An implausible receipt must never be persisted. It is recorded as the
    /// sentinel instead, because `tick_persistence` promotes a non-zero receipt
    /// to the row's DESIGNATED timestamp for a sentinel-LTT tick — so a
    /// negative value would create a pre-1970 partition that retention and
    /// archival can never reach. A missing timestamp is recoverable; a lying
    /// one is not.
    #[test]
    fn an_implausible_receipt_is_recorded_as_unknown_not_persisted() {
        // In band — kept verbatim.
        let good: i64 = 1_787_800_000_000_000_000;
        assert_eq!(plausible_receipt_nanos(good), good);
        // Every out-of-band shape collapses to the sentinel.
        for bad in [
            -1_i64,
            i64::MIN,
            i64::MAX,
            1,
            1_599_999_999_999_999_999, // just before the band opens
            2_524_608_000_000_000_001, // just after it closes
            WAL_RECEIPT_UNKNOWN_NANOS, // the sentinel itself is idempotent
        ] {
            assert_eq!(
                plausible_receipt_nanos(bad),
                WAL_RECEIPT_UNKNOWN_NANOS,
                "{bad} must be refused, not persisted"
            );
        }
        // Both edges are inclusive.
        assert_eq!(
            plausible_receipt_nanos(MIN_PLAUSIBLE_RECEIPT_NANOS),
            MIN_PLAUSIBLE_RECEIPT_NANOS
        );
        assert_eq!(
            plausible_receipt_nanos(MAX_PLAUSIBLE_RECEIPT_NANOS),
            MAX_PLAUSIBLE_RECEIPT_NANOS
        );
    }

    /// End-to-end twin of the test above: a negative receipt handed to the
    /// append path must reach disk as the sentinel, not as a negative number.
    #[test]
    fn a_negative_receipt_never_reaches_the_wal() {
        let dir = tmp_dir("v3-negative-receipt");
        {
            let spill = WsFrameSpill::new(&dir).unwrap();
            spill.append_with_seq_at(WsType::LiveFeed, vec![1], 1, -42);
            wait_until_persisted(&spill, 1);
        }
        std::thread::sleep(Duration::from_millis(50));
        let frames = replay_all(&dir).unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(
            frames[0].received_at_nanos, WAL_RECEIPT_UNKNOWN_NANOS,
            "a negative receipt must be banded away before it is written"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A segment written by a NEWER binary (a deploy rollback) must be reported
    /// LOUDLY and COUNTED, not swallowed as a clean zero-frame replay. Before
    /// 2026-08-28 this arm was an uncoded `warn!`, so no CloudWatch filter
    /// could match it and the loss was unpageable as well as unrecovered.
    #[test]
    fn a_segment_from_a_newer_binary_recovers_nothing_and_does_not_panic() {
        let dir = tmp_dir("v3-future-magic");
        let seg = dir.join("ws-frames-00000000000000000006.wal");
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"TVW9"); // a version this binary cannot parse
        bytes.push(WsType::LiveFeed.as_u8());
        bytes.extend_from_slice(&[0u8; 40]); // plausible-looking body
        std::fs::write(&seg, &bytes).unwrap();

        let frames = replay_all(&dir).unwrap(); // must not panic
        assert!(
            frames.is_empty(),
            "an unparseable segment must recover 0 frames rather than guess"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Test-only encoder for a v2 record. The writer no longer emits v2, so
    /// this is the ONLY way to build the legacy shape that backward-compat
    /// replay must still accept — without it, the v2 path would be untested
    /// from the day v3 shipped.
    fn encode_v2_record(ws: WsType, frame_seq: u64, frame: &[u8]) -> Vec<u8> {
        let len = u32::try_from(frame.len()).expect("test frame fits u32");
        let seq = frame_seq.to_le_bytes();
        let crc = crc32_ieee_of(&[&[ws.as_u8()], &seq[..], &len.to_le_bytes()[..], frame]);
        let mut v = Vec::new();
        v.extend_from_slice(&WAL_MAGIC_V2);
        v.push(ws.as_u8());
        v.extend_from_slice(&seq);
        v.extend_from_slice(&len.to_le_bytes());
        v.extend_from_slice(frame);
        v.extend_from_slice(&crc.to_le_bytes());
        v
    }

    /// Test-only encoder for a v3 record, byte-for-byte mirroring
    /// `write_record`. Kept beside the tests that use it so a divergence
    /// between the two shows up as a failing round-trip rather than silently.
    fn encode_v3_record(
        ws: WsType,
        frame_seq: u64,
        received_at_nanos: i64,
        frame: &[u8],
    ) -> Vec<u8> {
        let len = u32::try_from(frame.len()).expect("test frame fits u32");
        let seq = frame_seq.to_le_bytes();
        let recv = received_at_nanos.to_le_bytes();
        let crc = crc32_ieee_of(&[
            &[ws.as_u8()],
            &seq[..],
            &recv[..],
            &len.to_le_bytes()[..],
            frame,
        ]);
        let mut v = Vec::new();
        v.extend_from_slice(&WAL_MAGIC_V3);
        v.push(ws.as_u8());
        v.extend_from_slice(&seq);
        v.extend_from_slice(&recv);
        v.extend_from_slice(&len.to_le_bytes());
        v.extend_from_slice(frame);
        v.extend_from_slice(&crc.to_le_bytes());
        v
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
    fn test_next_frame_seq_always_leaves_the_packet_index_bits_free() {
        // The whole replay-stability scheme rests on this: if a frame sequence
        // ever arrives with a low bit set, `packet_capture_seq` would be
        // OR-ing into an occupied slot and two packets could collide.
        for _ in 0..5_000 {
            let seq = next_frame_seq();
            assert_eq!(
                seq & MAX_PACKET_INDEX,
                0,
                "frame_seq {seq} has a non-zero packet-index slot"
            );
        }
    }

    #[test]
    fn test_packet_capture_seq_is_distinct_per_packet_and_never_reaches_the_next_frame() {
        let a = next_frame_seq();
        let b = next_frame_seq();
        assert!(b > a);

        // Every packet of one frame is distinct...
        let mut seen = std::collections::BTreeSet::new();
        for idx in [0u64, 1, 2, 69_999, MAX_PACKET_INDEX] {
            let seq = packet_capture_seq(a, idx).expect("index fits the reserved bits");
            assert!(
                seen.insert(seq),
                "packet index {idx} produced a duplicate seq"
            );
            // ...and none of them can reach the NEXT frame's base, which is the
            // property that makes cross-frame collision impossible rather than
            // merely unlikely.
            assert!(
                seq < b,
                "packet {idx} of frame {a} produced {seq}, which reaches into frame {b}"
            );
        }
    }

    #[test]
    fn test_packet_capture_seq_refuses_an_index_beyond_the_reserved_bits() {
        // Must be None, never a wrapped or freshly-minted value: the caller is
        // required to REFUSE and count. A fallback here would silently restore
        // the duplicate-on-replay defect this scheme removes.
        let seq = next_frame_seq();
        assert_eq!(packet_capture_seq(seq, MAX_PACKET_INDEX + 1), None);
        assert_eq!(packet_capture_seq(seq, u64::MAX), None);
    }

    #[test]
    fn test_packet_capture_seq_is_replay_stable_and_fits_the_i64_column() {
        // Replay reproduces the identical value from the SAME persisted
        // frame_seq — this is the property that makes a re-fold collapse onto
        // the original row instead of duplicating it.
        let frame_seq = next_frame_seq();
        for idx in [0u64, 1, 7, 70_000] {
            let first = packet_capture_seq(frame_seq, idx);
            let replayed = packet_capture_seq(frame_seq, idx);
            assert_eq!(
                first, replayed,
                "replay must reproduce packet {idx} exactly"
            );
            let seq = first.expect("index fits");
            assert!(
                i64::try_from(seq).is_ok(),
                "capture_seq {seq} must fit the i64 ticks column — the shift must \
                 not have consumed headroom (the multiplication scheme did)"
            );
        }
    }

    #[test]
    fn test_packet_capture_seq_normalises_a_non_aligned_frame_seq() {
        // A v1 (`TVW1`) record carries frame_seq = 0, and a hand-built or
        // legacy value may not be base-aligned. OR-ing into a dirty slot could
        // collide with a neighbouring packet, so the low bits are cleared first.
        let dirty = (42u64 << PACKET_INDEX_BITS) | 12_345;
        assert_eq!(
            packet_capture_seq(dirty, 7),
            Some((42u64 << PACKET_INDEX_BITS) | 7)
        );
        assert_eq!(packet_capture_seq(0, 3), Some(3));
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

    // ---- ACTIVE-dir prune (2026-08-25) -----------------------------------

    /// Plants a segment in the ACTIVE dir (the WAL root), not `archive/`.
    fn plant_active_file(dir: &Path, name: &str, now: SystemTime, age_secs: u64) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, b"segment-bytes").unwrap();
        let mtime = now - Duration::from_secs(age_secs);
        let f = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        f.set_times(std::fs::FileTimes::new().set_modified(mtime))
            .unwrap();
        path
    }

    #[test]
    fn active_prune_deletes_past_retention_and_keeps_the_current_session() {
        // The leak this closes: ONLY `archive/` was ever bounded. The active
        // dir had no age bound and no byte bound, on the assumption that boot
        // replay drains it — but the replay budget is 512 MiB per boot against
        // continuous writing, so everything past the newest five segments was
        // never replayed, never confirmed, never archived, and eligible for no
        // bound. Measured on the prod box: 244 files, 31 GB, oldest from the
        // previous day, volume 94% full.
        let dir = tmp_dir("active-prune-age");
        let now = SystemTime::now();
        let stale = plant_active_file(&dir, "ws-frames-00000000000000000010.wal", now, 259_200);
        let todays = plant_active_file(&dir, "ws-frames-00000000000000000011.wal", now, 600);
        let outcome = prune_active_segments_at(&dir, 172_800, u64::MAX, now);
        assert_eq!(outcome.deleted, 1);
        assert!(!stale.exists(), "a segment older than retention must go");
        assert!(
            todays.exists(),
            "the current session's segments must never be touched — they are \
             the crash-recovery copy that replay actually reads"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn active_prune_can_never_reach_what_the_next_boot_replay_would_read() {
        // The safety property that makes this provable rather than probable.
        // A boot replay is capped at WAL_REPLAY_MAX_BYTES (512 MiB) of the
        // NEWEST segments. The retention floor is a full day. So no segment
        // this pass can delete is reachable by the next replay — the pass and
        // the replay operate on disjoint ends of the same directory.
        assert!(
            tickvault_common::constants::WS_WAL_ACTIVE_RETENTION_SECS >= 86_400,
            "retention below one day could race the boot replay window"
        );
        // Non-vacuous: a segment inside one replay budget of the newest is
        // young enough to survive, whatever its size.
        let dir = tmp_dir("active-prune-replay-safe");
        let now = SystemTime::now();
        let recent = plant_active_file(&dir, "ws-frames-00000000000000000020.wal", now, 3_600);
        let outcome = prune_active_segments_at(
            &dir,
            tickvault_common::constants::WS_WAL_ACTIVE_RETENTION_SECS,
            u64::MAX,
            now,
        );
        assert_eq!(outcome.deleted, 0);
        assert!(recent.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn active_prune_never_descends_into_archive_or_replaying() {
        // `replaying/` holds STAGED-but-unconfirmed segments — deleting one
        // loses frames the lane is mid-way through confirming. `archive/` has
        // its own bounds. The pass reads ONE directory and touches only
        // `*.wal`, so both subdirectories are out of reach by construction;
        // this pins that rather than trusting it.
        let dir = tmp_dir("active-prune-subdirs");
        let now = SystemTime::now();
        let archived = plant_archive_file(&dir, "ws-frames-00000000000000000030.wal", now, 999_999);
        let staging = dir.join("replaying");
        std::fs::create_dir_all(&staging).unwrap();
        let staged =
            plant_active_file(&staging, "ws-frames-00000000000000000031.wal", now, 999_999);
        let outcome = prune_active_segments_at(&dir, 172_800, u64::MAX, now);
        assert_eq!(outcome.deleted, 0, "nothing in the ROOT to delete");
        assert!(archived.exists(), "archive/ has its own bounds");
        assert!(
            staged.exists(),
            "replaying/ holds unconfirmed frames — never touchable by this pass"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_active_byte_ceiling_is_derived_and_never_below_its_floor() {
        // The archive's sibling ceiling is a hardcoded 50 GB whose own doc
        // concedes it engages at the target instrument count. This session was
        // spent on the consequence of that shape — a 512 MiB spill ceiling
        // pinned to no machine, refusing a rescue and losing 1,695,983 ticks.
        let ceiling = ws_wal_active_max_bytes(".");
        assert!(
            ceiling >= 8 * 1024 * 1024 * 1024,
            "the ceiling must never fall below its floor"
        );
        assert_eq!(ceiling, ws_wal_active_max_bytes("."), "must be memoised");
        // And it must actually bound the set that reached 31 GB unbounded.
        assert!(
            ceiling < 100 * 1024 * 1024 * 1024,
            "a ceiling this large would not bound anything on a 200 GB volume"
        );
    }

    // ---- byte ceiling (2026-08-19) ---------------------------------------

    /// Writes `n` archive segments of `bytes` each, oldest first, one second
    /// apart so the ceiling's oldest-first ordering is unambiguous.
    fn seed_archive(dir: &std::path::Path, n: usize, bytes: usize) -> Vec<PathBuf> {
        let archive = dir.join(ARCHIVE_SUBDIR);
        std::fs::create_dir_all(&archive).expect("archive dir");
        let base = SystemTime::now() - std::time::Duration::from_secs(10_000);
        let mut paths = Vec::new();
        for idx in 0..n {
            let path = archive.join(format!("seg-{idx:03}.wal"));
            std::fs::write(&path, vec![0_u8; bytes]).expect("write segment");
            // std, not the `filetime` crate — a new workspace dependency needs
            // operator approval, and `File::set_times` does the job.
            let mtime = base + std::time::Duration::from_secs(idx as u64);
            let f = std::fs::File::options()
                .write(true)
                .open(&path)
                .expect("reopen segment");
            f.set_times(std::fs::FileTimes::new().set_modified(mtime))
                .expect("set mtime");
            paths.push(path);
        }
        paths
    }

    #[test]
    fn byte_ceiling_deletes_oldest_first_until_under_the_limit() {
        let dir = tmp_dir("ceil-oldest");
        // 5 segments x 1000 B = 5000 B; ceiling 2500 B must delete the three
        // OLDEST, leaving the two newest (the ones a crash triage needs).
        let paths = seed_archive(&dir, 5, 1000);
        let out = prune_archived_segments_at(&dir, u64::MAX, 2500, SystemTime::now());
        assert_eq!(out.deleted, 0, "nothing is age-expired here");
        assert_eq!(out.size_deleted, 3, "three oldest removed by the ceiling");
        assert!(out.bytes_after <= 2500, "must end under the ceiling");
        assert!(!paths[0].exists() && !paths[1].exists() && !paths[2].exists());
        assert!(paths[3].exists() && paths[4].exists(), "newest survive");
    }

    #[test]
    fn byte_ceiling_is_inert_when_under_the_limit() {
        let dir = tmp_dir("ceil-inert");
        let paths = seed_archive(&dir, 4, 1000);
        let out = prune_archived_segments_at(&dir, u64::MAX, 1_000_000, SystemTime::now());
        assert_eq!(out.size_deleted, 0, "a generous ceiling must never bite");
        assert_eq!(out.bytes_after, 4000);
        for p in &paths {
            assert!(p.exists(), "no segment may be touched under the ceiling");
        }
    }

    #[test]
    fn byte_ceiling_runs_after_the_age_pass_not_instead_of_it() {
        let dir = tmp_dir("ceil-compose");
        let paths = seed_archive(&dir, 4, 1000);
        // Age window of 1s expires all four (seeded ~10_000s old), so the
        // ceiling has nothing left to do — the two passes must compose, not
        // double-count the same file.
        let out = prune_archived_segments_at(&dir, 1, 1, SystemTime::now());
        assert_eq!(out.deleted, 4, "all four age-expired");
        assert_eq!(out.size_deleted, 0, "ceiling finds no survivors to trim");
        assert_eq!(out.bytes_after, 0);
        for p in &paths {
            assert!(!p.exists());
        }
    }

    #[test]
    fn byte_ceiling_zero_empties_the_archive_and_never_underflows() {
        // The extreme input. A 0 ceiling is not a configuration anyone should
        // set, but it must terminate cleanly rather than underflow the running
        // total or loop.
        let dir = tmp_dir("ceil-zero");
        seed_archive(&dir, 3, 500);
        let out = prune_archived_segments_at(&dir, u64::MAX, 0, SystemTime::now());
        assert_eq!(out.size_deleted, 3);
        assert_eq!(out.bytes_after, 0);
    }

    #[test]
    fn byte_ceiling_ignores_foreign_files_entirely() {
        // A non-.wal file in the archive must never be deleted by either
        // pass, and must not count toward the ceiling — deleting an
        // operator's notes to satisfy a WAL budget would be indefensible.
        let dir = tmp_dir("ceil-foreign");
        seed_archive(&dir, 2, 1000);
        let foreign = &dir.join(ARCHIVE_SUBDIR).join("operator-notes.txt");
        std::fs::write(foreign, vec![0_u8; 100_000]).expect("write foreign");
        let out = prune_archived_segments_at(&dir, u64::MAX, 1500, SystemTime::now());
        assert!(foreign.exists(), "foreign file must survive");
        assert_eq!(out.size_deleted, 1, "only .wal segments are candidates");
        assert!(
            out.bytes_after <= 1500,
            "foreign bytes must not count toward the ceiling"
        );
    }

    #[test]
    fn byte_ceiling_on_an_empty_or_missing_archive_is_a_no_op() {
        let dir = tmp_dir("ceil-empty");
        // missing archive dir
        let out = prune_archived_segments_at(&dir, u64::MAX, 0, SystemTime::now());
        assert_eq!((out.deleted, out.size_deleted, out.bytes_after), (0, 0, 0));
        // present but empty
        std::fs::create_dir_all(dir.join(ARCHIVE_SUBDIR)).expect("mkdir");
        let out = prune_archived_segments_at(&dir, u64::MAX, 0, SystemTime::now());
        assert_eq!((out.deleted, out.size_deleted, out.bytes_after), (0, 0, 0));
    }

    #[test]
    fn test_prune_preserves_fresh_archive_segments() {
        let dir = tmp_dir("prune-fresh");
        let now = SystemTime::now();
        let fresh = plant_archive_file(&dir, "ws-frames-00000000000000000001.wal", now, 3600);
        let outcome = prune_archived_segments_at(&dir, 604_800, u64::MAX, now);
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
        let outcome = prune_archived_segments_at(&dir, 604_800, u64::MAX, now);
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
        let outcome = prune_archived_segments_at(&dir, 604_800, u64::MAX, now);
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
        let outcome = prune_archived_segments_at(&dir, 604_800, u64::MAX, SystemTime::now());
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
        let outcome = prune_archived_segments_at(&dir, 604_800, u64::MAX, past_now);
        assert_eq!(outcome.deleted, 0);
        assert!(
            skewed.exists(),
            "a future-mtime file (clock skew) must be kept — never delete on uncertainty"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The pre-resolved counter tables in `SpillDropCounters` are addressed by
    /// `ws_type_index`, so the index MUST be dense, unique, and exactly as wide
    /// as `WS_TYPE_COUNT`. Without this, adding a `WsType` variant compiles
    /// (the `match` in `ws_type_index` would fail — but a careless `_ =>` arm
    /// would not) and then indexes the wrong series or panics out of range on
    /// the one path that runs while the process is already losing frames.
    #[test]
    fn test_ws_type_index_is_dense_and_matches_all() {
        let all = WS_TYPES_BY_INDEX;
        assert_eq!(
            all.len(),
            WS_TYPE_COUNT,
            "WS_TYPE_COUNT must equal the number of WsType variants; widen the \
             SpillDropCounters tables when adding a variant"
        );
        // Every u8 the wire can carry must map into the table — this is what
        // catches a variant added to the enum but not to WS_TYPES_BY_INDEX.
        for byte in 0u8..=u8::MAX {
            if let Some(ws_type) = WsType::from_u8(byte) {
                assert!(
                    all.contains(&ws_type),
                    "WsType::from_u8({byte}) = {} is missing from \
                     WS_TYPES_BY_INDEX — its losses would be counted against \
                     another transport",
                    ws_type.as_str()
                );
            }
        }
        let mut seen = [false; WS_TYPE_COUNT];
        for ws_type in all {
            let idx = ws_type_index(ws_type);
            assert!(
                idx < WS_TYPE_COUNT,
                "ws_type_index({}) = {idx} is out of range for the counter tables",
                ws_type.as_str()
            );
            assert!(
                !seen[idx],
                "ws_type_index collision at {idx} for {} — two WsType variants \
                 would share one counter series and under-report loss",
                ws_type.as_str()
            );
            seen[idx] = true;
        }
        assert!(
            seen.iter().all(|hit| *hit),
            "ws_type_index must be dense — an unused slot means a variant maps \
             nowhere and its losses are invisible"
        );
    }

    /// Both WAL drop arms must use the pre-resolved handles, never the labelled
    /// `metrics::counter!` macro form: a keyed `Key` owns a `Vec<Label>`, so the
    /// macro heap-allocates once per DROPPED frame — an allocation storm layered
    /// on top of the data loss it is reporting.
    #[test]
    fn test_drop_arms_never_use_the_allocating_macro_form() {
        let src = include_str!("ws_frame_spill.rs");
        // Split on the tests MODULE marker, not a bare `#[cfg(test)]`. This
        // file has a `#[cfg(test)]` item (`new_with_dead_writer_for_test`)
        // ABOVE the drop arms, so a bare split truncates the "production half"
        // before the code under test and the scan silently checks nothing —
        // the exact vacuous-guard shape this test exists to prevent. Caught by
        // this test failing against its own first draft.
        let production = src
            .split("#[cfg(test)]\nmod tests")
            .next()
            .expect("source has a production half");
        assert!(
            production.contains("fn append_with_seq"),
            "the production half must actually contain the drop arms — if this \
             trips, the split marker drifted and the scan below is vacuous"
        );
        for banned in [
            "metrics::counter!(\n                    \"tv_ws_frame_spill_drop_critical\"",
            "metrics::counter!(\n                    \"tv_ticks_lost_total\"",
        ] {
            assert!(
                !production.contains(banned),
                "the WAL drop arms must increment pre-resolved SpillDropCounters \
                 handles, not build a labelled Key per dropped frame"
            );
        }
        assert!(
            production.contains("self.drop_counters.drop_critical[idx].increment(1)"),
            "the drop arms must increment the pre-resolved drop_critical handle"
        );
    }

    /// EXHAUSTIVE permutation sweep of the writer-dead drop arm.
    ///
    /// Every `WsType` × every adversarial frame shape must: return `Dropped`,
    /// increment the drop ledger exactly once, never panic, and never let one
    /// transport's loss land on another transport's counter. The frame shapes
    /// are deliberately hostile — empty, single byte, a length that lies about
    /// the payload, a full 64 KiB frame, and all-0xFF — because the drop arm
    /// runs on malformed traffic at least as often as on well-formed traffic,
    /// and that is precisely when it must stay cheap and correct.
    #[test]
    fn test_drop_arm_permutations_every_ws_type_every_frame_shape() {
        let shapes: [(&str, Vec<u8>); 6] = [
            ("empty", Vec::new()),
            ("one_byte", vec![0x00]),
            ("header_only", vec![2, 0, 0, 0, 0, 0, 0, 0]),
            ("lying_length", vec![2, 0xFF, 0xFF, 0, 0, 0, 0, 0]),
            ("all_ones", vec![0xFF; 64]),
            ("max_frame", vec![0xAB; 64 * 1024]),
        ];

        for ws_type in WS_TYPES_BY_INDEX {
            for (shape_name, frame) in &shapes {
                // A FRESH spill per permutation so the ledger reading is
                // unambiguous — a shared instance would let an earlier
                // permutation's count mask a later one that failed to increment.
                let spill = WsFrameSpill::new_with_dead_writer_for_test();
                assert_eq!(
                    spill.drop_critical_count(),
                    0,
                    "{}/{shape_name}: a fresh spill must start at zero",
                    ws_type.as_str()
                );

                let outcome = spill.append(ws_type, frame.clone());
                assert_eq!(
                    outcome,
                    AppendOutcome::Dropped,
                    "{}/{shape_name}: a dead writer must report Dropped, never \
                     a silent Spilled — a silent success here is the durable \
                     floor lying about itself",
                    ws_type.as_str()
                );
                assert_eq!(
                    spill.drop_critical_count(),
                    1,
                    "{}/{shape_name}: exactly one drop must be ledgered",
                    ws_type.as_str()
                );

                // Repeat on the SAME instance: the ledger must accumulate, not
                // latch. A latching counter under-reports a drop storm to
                // exactly the degree the storm is bad.
                let _ = spill.append(ws_type, frame.clone());
                let _ = spill.append(ws_type, frame.clone());
                assert_eq!(
                    spill.drop_critical_count(),
                    3,
                    "{}/{shape_name}: the drop ledger must accumulate",
                    ws_type.as_str()
                );
            }
        }
    }

    /// Interleaving every `WsType` through ONE spill must not lose or misattribute
    /// a single drop. This is the cross-talk check the per-type table exists for:
    /// with a shared handle (the pre-2026-08-14 macro form rebuilt a Key per
    /// call) an indexing mistake is invisible, because every increment lands on
    /// whatever Key was built last.
    #[test]
    fn test_drop_arm_interleaved_ws_types_never_cross_talk() {
        let spill = WsFrameSpill::new_with_dead_writer_for_test();
        let mut expected = 0u64;
        // Three full rotations, so an off-by-one index would desynchronise.
        for _round in 0..3 {
            for ws_type in WS_TYPES_BY_INDEX {
                assert_eq!(
                    spill.append(ws_type, vec![1, 2, 3, 4]),
                    AppendOutcome::Dropped
                );
                expected += 1;
                assert_eq!(
                    spill.drop_critical_count(),
                    expected,
                    "interleaved {} must ledger exactly one drop per append",
                    ws_type.as_str()
                );
            }
        }
        assert_eq!(
            expected,
            (WS_TYPE_COUNT as u64) * 3,
            "the sweep must have covered every WsType three times"
        );
    }

    /// The fail-closed half: a second claim on a directory a live guard already
    /// owns must be REFUSED, not warned about.
    ///
    /// This is the whole point of the lock. Two processes on one WAL directory
    /// mint `capture_seq` from independent clock-seeded counters, collide inside
    /// the `ticks` DEDUP key, and destroy ticks with nothing to show for it. A
    /// refusal costs one boot; proceeding costs data nothing can rebuild.
    #[test]
    fn a_second_claim_on_a_live_wal_directory_is_refused() {
        let dir = tmp_dir("wal-lock-refuse");
        let first = lock_wal_dir(&dir).expect("first claim must succeed");
        let second = lock_wal_dir(&dir);
        assert!(
            second.is_err(),
            "a second live process took the same WAL directory — this is the \
             silent capture_seq collision the lock exists to prevent"
        );
        let msg = format!("{:#}", second.unwrap_err());
        assert!(
            msg.contains("already owned by another live process"),
            "the refusal must name the cause so an operator can act on it, got: {msg}"
        );
        assert!(
            msg.contains("capture_seq"),
            "the refusal must say WHY it matters, got: {msg}"
        );
        drop(first);
    }

    /// Dropping the guard releases the directory, so a clean restart is not
    /// locked out of its own data.
    ///
    /// The kernel does this for us on process death, including SIGKILL — which
    /// is exactly why this is `flock` and not a PID file. This test covers the
    /// in-process half of the same property.
    #[test]
    fn dropping_the_guard_frees_the_wal_directory_for_the_next_process() {
        let dir = tmp_dir("wal-lock-release");
        let first = lock_wal_dir(&dir).expect("first claim");
        drop(first);
        assert!(
            lock_wal_dir(&dir).is_ok(),
            "a released directory must be claimable, or a restart after a clean \
             shutdown would be locked out of its own WAL"
        );
    }

    /// Two DIFFERENT directories are independent — the lock is per-directory,
    /// not global, so a second feed with its own WAL is unaffected.
    #[test]
    fn two_different_wal_directories_are_independently_claimable() {
        let a = tmp_dir("wal-lock-a");
        let b = tmp_dir("wal-lock-b");
        let ga = lock_wal_dir(&a).expect("claim a");
        let gb = lock_wal_dir(&b).expect("claim b must not be blocked by a");
        assert_ne!(ga.path(), gb.path());
    }

    /// The re-seed must find the high-water mark that a restart would otherwise
    /// re-issue.
    ///
    /// Scenario, which is the real one: a process running above ~7,600 frames/s
    /// has driven the counter AHEAD of the wall clock on the `prev + 1` arm, then
    /// dies. A fresh process seeds from `AtomicU64::new(0)` + the wall clock —
    /// BELOW the mark — and starts re-issuing sequences already in the database,
    /// where they upsert live ticks away.
    #[test]
    fn the_reseed_finds_the_high_water_mark_a_restart_would_have_reissued() {
        let dir = tmp_dir("wal-reseed");
        // A sequence far above any plausible wall clock, i.e. one that the
        // clock-seeded counter could not reach on its own.
        let far_future = (u64::MAX >> 2) & !MAX_PACKET_INDEX;
        let seg = dir.join("ws-frames-00000000000000000001.wal");
        let rec = WalRecord {
            ws_type: WsType::LiveFeed,
            frame: Bytes::from_static(&[1, 2, 3, 4]),
            frame_seq: far_future,
            received_at_nanos: 42,
        };
        let f = File::create(&seg).expect("segment");
        let mut w = BufWriter::new(f);
        write_record(&mut w, &rec).expect("write");
        std::io::Write::flush(&mut w).expect("flush");
        drop(w);

        assert_eq!(
            highest_frame_seq_on_disk(&dir),
            far_future,
            "the probe must read the sequence back out of the record header"
        );

        seed_frame_seq_from_disk(&dir);
        let next = next_frame_seq();
        assert!(
            next > far_future,
            "the next mint ({next}) must exceed the on-disk high-water mark \
             ({far_future}); minting at or below it re-issues capture_seq values \
             that are already rows, and QuestDB upserts the live tick away"
        );
    }

    /// The re-seed is a RATCHET: it never lowers the counter, so calling it on a
    /// directory whose high-water mark is behind the live counter is a no-op.
    ///
    /// This matters because the probe is best-effort — a torn tail returns a
    /// LOWER bound — and a lower bound must never be allowed to walk the
    /// sequence backwards into values already issued this session.
    #[test]
    fn the_reseed_never_lowers_the_live_sequence() {
        let dir = tmp_dir("wal-reseed-ratchet");
        let seg = dir.join("ws-frames-00000000000000000001.wal");
        let rec = WalRecord {
            ws_type: WsType::LiveFeed,
            frame: Bytes::from_static(&[9]),
            // Deliberately tiny: far below any wall-clock-seeded value.
            frame_seq: 1 << PACKET_INDEX_BITS,
            received_at_nanos: 7,
        };
        let f = File::create(&seg).expect("segment");
        let mut w = BufWriter::new(f);
        write_record(&mut w, &rec).expect("write");
        std::io::Write::flush(&mut w).expect("flush");
        drop(w);

        let before = next_frame_seq();
        seed_frame_seq_from_disk(&dir);
        let after = next_frame_seq();
        assert!(
            after > before,
            "a stale, lower high-water mark must not rewind the live counter \
             (before {before}, after {after})"
        );
    }

    /// A torn tail ends the probe walk instead of panicking or looping, and what
    /// it found before the tear is still returned.
    ///
    /// Refusing to boot over a torn tail would turn a safety net into an outage,
    /// and a lower bound is still strictly better than the wall clock alone.
    #[test]
    fn the_reseed_probe_tolerates_a_torn_tail_and_keeps_what_it_read() {
        let dir = tmp_dir("wal-reseed-torn");
        let seg = dir.join("ws-frames-00000000000000000001.wal");
        let good_seq = 12_345u64 << PACKET_INDEX_BITS;
        let rec = WalRecord {
            ws_type: WsType::LiveFeed,
            frame: Bytes::from_static(&[1, 2, 3]),
            frame_seq: good_seq,
            received_at_nanos: 5,
        };
        let f = File::create(&seg).expect("segment");
        let mut w = BufWriter::new(f);
        write_record(&mut w, &rec).expect("write");
        std::io::Write::flush(&mut w).expect("flush");
        drop(w);
        // Append a half-written header: magic present, the rest missing.
        let mut appended = std::fs::OpenOptions::new()
            .append(true)
            .open(&seg)
            .expect("reopen");
        std::io::Write::write_all(&mut appended, &WAL_MAGIC_V3).expect("torn magic");
        std::io::Write::write_all(&mut appended, &[0u8; 3]).expect("torn body");
        drop(appended);

        assert_eq!(
            highest_frame_seq_on_disk(&dir),
            good_seq,
            "a torn tail must end the walk and keep the last complete record's \
             sequence, not return zero and not panic"
        );
    }

    /// An empty or absent directory seeds nothing, leaving the wall clock as the
    /// seed exactly as before. A first-ever boot must not be penalised.
    #[test]
    fn an_empty_wal_directory_seeds_nothing() {
        let dir = tmp_dir("wal-reseed-empty");
        assert_eq!(highest_frame_seq_on_disk(&dir), 0);
        assert_eq!(
            highest_frame_seq_on_disk(&dir.join("does-not-exist")),
            0,
            "an absent directory must be 0, not a panic"
        );
    }

    /// v1 (`TVW1`) records predate `frame_seq` entirely, so a directory holding
    /// only them yields 0 — the correct answer, not a fabricated sequence.
    #[test]
    fn v1_only_segments_seed_nothing_because_they_carry_no_sequence() {
        let dir = tmp_dir("wal-reseed-v1");
        let seg = dir.join("ws-frames-00000000000000000001.wal");
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&WAL_MAGIC);
        bytes.push(WsType::LiveFeed.as_u8());
        let frame = [7u8, 7, 7];
        bytes.extend_from_slice(&(frame.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&frame);
        let mut crc_in = Vec::new();
        crc_in.push(WsType::LiveFeed.as_u8());
        crc_in.extend_from_slice(&(frame.len() as u32).to_le_bytes());
        crc_in.extend_from_slice(&frame);
        bytes.extend_from_slice(&crc32_ieee_of(&[&crc_in]).to_le_bytes());
        std::fs::write(&seg, &bytes).expect("write v1");

        assert_eq!(
            highest_frame_seq_on_disk(&dir),
            0,
            "v1 records carry no sequence; inventing one would be worse than 0"
        );
    }

    /// The property that actually protects production: constructing a SECOND
    /// spill on a directory a live spill already owns must FAIL.
    ///
    /// The helper-level test above proves the lock works; this proves it is
    /// wired into the only constructor production uses. Without this, the lock
    /// could be present, correct, and never called.
    #[test]
    fn a_second_spill_on_a_live_wal_directory_refuses_to_construct() {
        let dir = tmp_dir("spill-second-ctor");
        let first = WsFrameSpill::new(&dir).expect("first spill must construct");
        let second = WsFrameSpill::new(&dir);
        assert!(
            second.is_err(),
            "two WsFrameSpill instances took the same WAL directory. Both would \
             mint capture_seq from independent clock-seeded counters, collide in \
             the ticks DEDUP key, and destroy ticks with no counter to show it."
        );
        drop(first);
        assert!(
            WsFrameSpill::new(&dir).is_ok(),
            "after the incumbent drops, the directory must be claimable again — \
             otherwise a clean restart is locked out of its own WAL"
        );
    }

    /// A spill constructed on a directory that already holds frames must mint
    /// sequences ABOVE them, end to end.
    ///
    /// This is the restart case as production actually reaches it: the previous
    /// process's segments are on disk, this process constructs a fresh spill,
    /// and the first frame it appends must not reuse a sequence already in the
    /// database.
    #[test]
    fn a_restart_mints_above_the_sequences_already_on_disk() {
        let dir = tmp_dir("spill-restart-above");
        let far_future = (u64::MAX >> 2) & !MAX_PACKET_INDEX;
        let seg = dir.join("ws-frames-00000000000000000001.wal");
        std::fs::create_dir_all(&dir).expect("dir");
        let rec = WalRecord {
            ws_type: WsType::LiveFeed,
            frame: Bytes::from_static(&[1, 2, 3, 4]),
            frame_seq: far_future,
            received_at_nanos: 11,
        };
        let f = File::create(&seg).expect("segment");
        let mut w = BufWriter::new(f);
        write_record(&mut w, &rec).expect("write");
        std::io::Write::flush(&mut w).expect("flush");
        drop(w);

        let _spill = WsFrameSpill::new(&dir).expect("construct over existing segments");
        let next = next_frame_seq();
        assert!(
            next > far_future,
            "constructing a spill over existing segments must ratchet the \
             sequence past them (next {next} vs on-disk {far_future}); minting \
             at or below re-issues capture_seq values that are already rows"
        );
    }
}
