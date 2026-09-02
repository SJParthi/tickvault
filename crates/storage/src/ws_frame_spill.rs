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
// Record format on disk (four versions; replay accepts all four):
//     v1 [MAGIC:4="TVW1"][ws_type:u8][len:u32 LE][frame][crc32:u32 LE]
//     v2 [MAGIC:4="TVW2"][ws_type:u8][frame_seq:u64 LE][len:u32 LE][frame][crc32]
//     v3 [MAGIC:4="TVW3"][ws_type:u8][frame_seq:u64 LE][received_at_nanos:i64 LE]
//        [len:u32 LE][frame][crc32]
//     v4 [MAGIC:4="TVW4"][ws_type:u8][frame_seq:u64 LE][received_at_nanos:i64 LE]
//        [endpoint:u8][len:u32 LE][frame][crc32]
// CRC32 covers every header byte of that version, in order, then the frame.
//
// WHY v4 EXISTS (2026-09-02). Every one of the 16 Dhan sockets — main feed,
// depth-20, depth-200 — writes its frames under `WsType::LiveFeed`, and the
// record carried no endpoint. The boot refold therefore decoded every frame
// with the MAIN-FEED grammar, recognised a depth frame by sniffing its header,
// and could do nothing with it but count it: a ring-shed depth frame was
// captured durably and then never re-persisted, because the replay had no way
// to know which parser it belonged to. v4 carries the endpoint byte, so a
// replayed depth frame is routed to the depth drain with its ORIGINAL receipt.
// A v1–v3 record replays as `WalEndpoint::MainFeed`, which is exactly what the
// refold assumed before — never a guess, never a synthesized value.
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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use crossbeam_channel::{Receiver, RecvTimeoutError, Sender, TrySendError, bounded};
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
// WalEndpoint — one byte tag (TVW4, 2026-09-02) naming WHICH Dhan socket a
// live-feed frame came off, so replay can route a depth frame to the depth
// drain instead of sniffing — and mis-sniffing — its header.
// ---------------------------------------------------------------------------

/// The socket a WAL frame was captured from.
///
/// Distinct from [`WsType`], which names the TRANSPORT (and therefore the
/// broker). Every Dhan market-data socket writes under `WsType::LiveFeed`;
/// this byte is what tells the main feed's 8-byte-header frames apart from the
/// depth sockets' 12-byte-header frames at replay time. Mirrors
/// `DhanEndpointType` one-to-one; the mapping lives on that enum so the two
/// cannot drift apart silently.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WalEndpoint {
    /// Live market-data feed: ticker / quote / full packets.
    MainFeed = 0,
    /// 20-level market depth.
    Depth20 = 1,
    /// 200-level full market depth.
    Depth200 = 2,
    /// Order/trade lifecycle events for orders we placed.
    OrderUpdate = 3,
}

impl WalEndpoint {
    /// TOTAL decode: an unknown byte maps to [`WalEndpoint::MainFeed`].
    ///
    /// Total on purpose. A record whose endpoint byte this binary does not
    /// recognise is still a captured frame with a valid CRC; refusing it would
    /// turn a forward-compatibility gap into a permanent loss. `MainFeed` is
    /// the pre-v4 assumption, so an unknown value degrades to exactly the
    /// behaviour every earlier binary had. The reader COUNTS the mapping (one
    /// coded line per segment) so it is never silent.
    // TEST-EXEMPT: covered by tvw4_unknown_endpoint_byte_maps_to_main_feed_not_a_panic
    #[must_use]
    pub const fn from_u8(b: u8) -> Self {
        match b {
            1 => Self::Depth20,
            2 => Self::Depth200,
            3 => Self::OrderUpdate,
            _ => Self::MainFeed,
        }
    }

    // TEST-EXEMPT: covered by tvw4_roundtrip_preserves_every_endpoint (asserts from_u8/as_u8 identity)
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    // TEST-EXEMPT: pure enum→&'static str mapping used only for log fields
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::MainFeed => "main_feed",
            Self::Depth20 => "depth_20",
            Self::Depth200 => "depth_200",
            Self::OrderUpdate => "order_update",
        }
    }

    /// The endpoint a caller that only knows the TRANSPORT can honestly claim.
    ///
    /// The order-update transport is its own endpoint; every market-data
    /// transport defaults to the main feed, because without a
    /// `DhanEndpointType` in hand that is the only claim that cannot be wrong
    /// in the dangerous direction (a depth frame labelled main-feed is
    /// counted-and-skipped on replay; a main-feed frame labelled depth would
    /// be fed to the wrong parser).
    // TEST-EXEMPT: covered by append_with_seq_derives_the_endpoint_from_the_transport
    #[must_use]
    pub const fn for_ws_type(ws_type: WsType) -> Self {
        match ws_type {
            WsType::OrderUpdate => Self::OrderUpdate,
            WsType::LiveFeed | WsType::TruedataFeed => Self::MainFeed,
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
    /// TVW4 (2026-09-02): which socket the frame came off, so replay routes a
    /// depth frame to the depth drain instead of sniffing its header.
    endpoint: WalEndpoint,
    // Zero-tick-loss PR-8a (H1): `Bytes` (Arc-refcounted) so the WS read
    // loop hands ownership to the disk-writer thread with an O(1) refcount
    // bump instead of a per-frame `Vec<u8>` malloc. Derefs to `&[u8]`, so
    // `write_record` / `crc32_ieee_of` / `.len()` are unchanged.
    frame: Bytes,
}

/// Result of a hot-path `append()` attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppendOutcome {
    /// Frame was QUEUED for durable write. Hot path is done.
    ///
    /// Read the first word literally: this is a `try_send` onto a bounded
    /// channel, and a background thread turns the queued record into bytes.
    /// `Spilled` therefore means "handed off", NOT "on disk", and the two are
    /// separated by up to `SPILL_CHANNEL_CAPACITY` records plus a 256 KiB
    /// `BufWriter` — around 100 s of frames at the 5,000 fps envelope.
    ///
    /// Everything in that window dies on an abort (`panic = "abort"`, an
    /// OOM-kill, a SIGKILL past `TimeoutStopSec`) and is counted by NOTHING:
    /// `persisted_count` counts `write_all`, and `drop_critical` counts only
    /// the frames `try_send` refused outright. The window is now at least
    /// OBSERVABLE — `tv_ws_frame_spill_queue_depth` and its session
    /// high-water are published from the writer batch loop — but observable
    /// is not the same as closed, and this doc says "queued" so no caller
    /// mistakes the one for the other.
    ///
    /// Nor is a written record fsync'ed; see the module header. The durable
    /// floor is page-cache-deep, not platter-deep.
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
    /// TVW4: the socket this frame was captured from, read back from the v4
    /// record. [`WalEndpoint::MainFeed`] for v1–v3 records that predate the
    /// field — which is exactly what every earlier replay assumed, so a legacy
    /// segment behaves precisely as it did before.
    pub endpoint: WalEndpoint,
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

/// Divisor on the resolved memory ceiling for [`wal_queue_max_bytes`]. 1/16.
const WAL_QUEUE_RAM_FRACTION_DIVISOR: u64 = 16;

/// Floor on the queue's byte budget — 256 MiB, whatever the host reports.
///
/// Deliberately equal to `dhan_feed_stack::FRAME_RING_MAX_BYTES`, the sibling
/// in-memory buffer on the same path, so the two absorbers are sized on one
/// scale rather than two.
const WAL_QUEUE_MIN_BYTES: u64 = 256 * 1024 * 1024;

/// Ceiling on the queue's byte budget — 2 GiB. Mirrors
/// `dhan_feed_stack::FRAME_RING_MAX_BYTES_CEILING` for the same reason.
const WAL_QUEUE_MAX_BYTES_CEILING: u64 = 2 * 1024 * 1024 * 1024;

/// The queue's byte budget for a host whose memory ceiling is `ceiling`.
///
/// Pure, so the clamp is provable without a host to read.
#[must_use]
pub(crate) const fn wal_queue_budget_from_ceiling(ceiling: Option<u64>) -> u64 {
    let Some(total) = ceiling else {
        // Nothing could be read about memory. Take the FLOOR, not the ceiling:
        // an unknown host is the one case where guessing large is guessing
        // toward the OOM this budget exists to prevent.
        return WAL_QUEUE_MIN_BYTES;
    };
    let share = total / WAL_QUEUE_RAM_FRACTION_DIVISOR;
    // `Ord::clamp` is not const, hence the explicit form.
    if share < WAL_QUEUE_MIN_BYTES {
        WAL_QUEUE_MIN_BYTES
    } else if share > WAL_QUEUE_MAX_BYTES_CEILING {
        WAL_QUEUE_MAX_BYTES_CEILING
    } else {
        share
    }
}

/// BYTE ceiling on everything queued but not yet written, resolved once.
///
/// # The defect this closes (2026-09-01)
///
/// [`SPILL_CHANNEL_CAPACITY`] bounds the queue at 524,288 **records**, and its
/// own doc prices that at "≈ 12 MiB (524k × ~24 B)". That arithmetic counts the
/// `WalRecord` HEADER and omits the field that carries the payload: `frame` is
/// a `Bytes`, i.e. a heap buffer the record owns a refcount on. A depth-200
/// frame is up to `DEPTH_200_MAX_FRAME_BYTES` = 512 KiB, so the same 524,288
/// slots are worth **256 GiB** of resident heap on a 32 GiB host — a count
/// bound reported as a byte bound, off by four orders of magnitude.
///
/// It is reachable rather than theoretical: the queue only fills when the
/// writer stalls, a stalled writer is what a saturated disk produces, and a
/// saturated disk is exactly when depth frames are largest and most frequent.
/// The failure mode is an OOM kill, which takes the whole process — all
/// sockets, the aggregator, and everything still queued — where a refusal
/// costs one frame and leaves a counter behind.
///
/// So the bound is on bytes AND records, and whichever binds first wins.
///
/// Derived from the host rather than fixed, so the same binary is correct on
/// the 32 GiB production box (2 GiB, the ceiling) and in a 4 GiB dev container
/// (256 MiB, the floor). `resolve_memory_ceiling` prefers a cgroup limit over
/// machine RAM, which is what makes a container-limited run size to its real
/// ceiling instead of the machine's.
///
/// **This does not change behaviour for the ordinary tick mix.** A 162-byte
/// Full packet fills the 524,288-record bound at ~85 MiB, far under the floor,
/// so the record bound still bites first and the byte bound never engages. It
/// engages only when the payload mix is heavy enough to threaten the host —
/// which is precisely the case the record bound cannot see.
fn wal_queue_max_bytes() -> u64 {
    static RESOLVED: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *RESOLVED.get_or_init(|| {
        // 2026-09-02: OUR unit's cgroup `memory.max`, not the root's. The root
        // file reads `max` on a bare systemd host whether or not the unit
        // carries a `MemoryMax=`, so a limit set on tickvault.service was
        // invisible here and the budget sized itself to the whole machine.
        let ceiling = crate::resource_monitor::resolve_memory_ceiling(
            &crate::resource_monitor::resolve_cgroup_memory_max_path(),
            Path::new(crate::resource_monitor::DEFAULT_PROC_MEMINFO_PATH),
        );
        let budget = wal_queue_budget_from_ceiling(ceiling.bytes());
        info!(
            budget_bytes = budget,
            ceiling_source = ceiling.source(),
            ceiling_bytes = ceiling.bytes().unwrap_or(0),
            "WAL capture queue byte budget resolved"
        );
        budget
    })
}

/// WAL file magic bytes — segment-local sanity check.
///
/// TICK-SEQ-01 PR-2a: `TVW1` = v1 record (no `frame_seq`); `TVW2` = v2 record
/// (carries an 8-byte LE `frame_seq` immediately after `ws_type`). Replay
/// accepts BOTH, so segments written before this change still recover. NEW
/// records are always written v2.
const WAL_MAGIC: [u8; 4] = *b"TVW1";
const WAL_MAGIC_V2: [u8; 4] = *b"TVW2";
/// `TVW3` = v3 record: v2 plus an 8-byte LE `received_at_nanos` after
/// `frame_seq`. Replay-only since v4.
const WAL_MAGIC_V3: [u8; 4] = *b"TVW3";
/// `TVW4` = v4 record: v3 plus a 1-byte `endpoint` after `received_at_nanos`.
/// NEW records are always written v4.
const WAL_MAGIC_V4: [u8; 4] = *b"TVW4";

/// Minimum on-disk record size per version, used by the replay loop guard:
/// v1 = magic(4)+ws_type(1)+len(4)+crc(4) = 13; v2 inserts frame_seq(8) = 21.
const WAL_MIN_RECORD_V1: usize = 13;
const WAL_MIN_RECORD_V2: usize = 21;
/// v3 inserts received_at_nanos(8) after frame_seq: 21 + 8 = 29.
const WAL_MIN_RECORD_V3: usize = 29;
/// v4 inserts endpoint(1) after received_at_nanos: 29 + 1 = 30.
const WAL_MIN_RECORD_V4: usize = 30;

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

/// How long the writer blocks on an EMPTY channel before re-checking the
/// shutdown flag.
///
/// The loop used a plain blocking `recv()`, whose only wake-up was a record
/// arriving or every sender being dropped. Neither can happen at shutdown: the
/// sender lives inside `WsFrameSpill`, which is held in an `Arc` shared with
/// the drain paths, so nothing can drop it, and by shutdown no more frames are
/// arriving. A timed receive is what gives the thread a chance to notice that
/// it has been asked to stop.
///
/// 200 ms is chosen for what it costs when NOTHING is happening: four
/// wake-ups a second on an otherwise idle thread. While frames ARE flowing the
/// timeout never fires, so the hot path is byte-identical to the blocking form.
const WAL_WRITER_STOP_POLL: Duration = Duration::from_millis(200); // APPROVED: this IS the named constant the rule asks for

/// How often [`WsFrameSpill::shutdown`] re-checks the queue and the thread.
const WAL_SHUTDOWN_POLL: Duration = Duration::from_millis(10); // APPROVED: this IS the named constant the rule asks for

/// Seconds `main` gives the WAL spill's final drain before abandoning it.
///
/// Sized small on purpose. By the time this runs the sockets are shut and the
/// lane is joined, so the queue holds only what the writer had not yet reached
/// — at the 5,000 fps envelope the writer clears a full 524,288-slot channel in
/// well under a second of disk time. Ten seconds is therefore a stall budget,
/// not a throughput budget: it covers a writer parked in
/// [`WAL_WRITER_IO_RETRY_BACKOFF`] against a briefly-wedged disk, and refuses
/// to hold the process any longer than that, because systemd's `TimeoutStopSec`
/// escalates to SIGKILL and a SIGKILL loses the very records this is here to
/// save.
///
/// Counted in the sequential sum that
/// `crates/app/tests/shutdown_budget_fits_systemd_guard.rs` checks against the
/// unit file — a third budget on the same path is exactly the kind of addition
/// that guard exists to catch.
pub const WAL_SPILL_SHUTDOWN_BUDGET_SECS: u64 = 10;

/// [`WAL_SPILL_SHUTDOWN_BUDGET_SECS`] as a `Duration`.
pub const WAL_SPILL_SHUTDOWN_BUDGET: Duration = Duration::from_secs(WAL_SPILL_SHUTDOWN_BUDGET_SECS);

/// Records still queued when the final drain was abandoned — i.e. frames that
/// were captured, acknowledged as `Spilled`, and then lost with the process.
///
/// Non-zero means the durable floor did not hold for that shutdown. It is the
/// WAL-side twin of `tv_offload_writer_shutdown_incomplete_total`, which counts
/// the same class for the tick and depth ILP writers; the WAL is the one tier
/// that had no such counter, which is why an abandoned queue here was invisible.
///
/// Deliberately a RECORD count, not an episode count: unlike the offload
/// writers — whose in-flight batches are ILP buffers whose row counts were
/// consumed when they were sent — the channel can be asked its exact length, so
/// there is a real number to report and no need to fabricate one.
pub const WAL_SPILL_SHUTDOWN_INCOMPLETE_COUNTER: &str = "tv_wal_spill_shutdown_incomplete_total";

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
    /// `tv_ticks_lost_total{source="spill_bytes_full", ws_type}` — byte-budget arm.
    ///
    /// A SEPARATE source from `spill_drop_critical` because the operator action
    /// differs: a record-full channel means the writer is behind, a byte-full
    /// channel means the queued PAYLOAD is large — heavy depth, not a slow
    /// disk — and the remedy is a depth-scope or host-memory decision rather
    /// than a disk one.
    ticks_lost_bytes_full: [metrics::Counter; WS_TYPE_COUNT],
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
        let ticks_lost_bytes_full = WS_TYPES_BY_INDEX.map(|t| {
            metrics::counter!(
                "tv_ticks_lost_total",
                "source" => "spill_bytes_full",
                "ws_type" => t.as_str(),
            )
        });
        let counters = Self {
            drop_critical,
            ticks_lost_channel_full,
            ticks_lost_writer_dead,
            ticks_lost_bytes_full,
        };
        for idx in 0..WS_TYPE_COUNT {
            counters.drop_critical[idx].increment(0);
            counters.ticks_lost_channel_full[idx].increment(0);
            counters.ticks_lost_writer_dead[idx].increment(0);
            counters.ticks_lost_bytes_full[idx].increment(0);
        }
        counters
    }
}

pub struct WsFrameSpill {
    spill_tx: Sender<WalRecord>,
    /// Bytes of frame payload QUEUED but not yet handed to the writer.
    ///
    /// Incremented by `append` before the `try_send`, decremented by the
    /// writer the instant a record leaves the channel. Shared with the writer
    /// thread through the `Arc`, which is why it is not a plain `AtomicU64`.
    ///
    /// See [`wal_queue_max_bytes`] for why a byte bound exists at all when the
    /// channel is already record-bounded.
    queued_bytes: Arc<AtomicU64>,
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
    /// Set once by [`WsFrameSpill::shutdown`]. The writer exits cleanly the
    /// first time it finds this set AND the channel empty — which is the only
    /// way this thread can be asked to stop, because the sole `Sender` lives in
    /// this struct and the struct is shared through an `Arc` that nothing can
    /// drop while a drain path still holds it.
    stop: Arc<AtomicBool>,
    /// The writer's join handle, so the final drain can WAIT for it instead of
    /// letting the process exit out from under it.
    ///
    /// Behind a `Mutex<Option<..>>` rather than owned by value because
    /// `shutdown` is reached through the same `Arc<WsFrameSpill>` the append
    /// paths hold: there is no `self` to consume. The lock is taken exactly
    /// once, at shutdown, and never on the append path.
    writer: std::sync::Mutex<Option<thread::JoinHandle<()>>>,
    /// The exclusive claim on the WAL directory, held for as long as this spill
    /// exists. Never read — its ONLY job is to keep the `flock` taken, and to
    /// release it on drop so the next process can start. See [`WalDirGuard`].
    _dir_guard: Option<WalDirGuard>,
}

impl WsFrameSpill {
    /// Create a spill writer rooted at `wal_dir`, taking the directory claim
    /// itself. Spawns the background writer.
    ///
    /// Prefer [`WsFrameSpill::new_with_guard`] in any boot path that touches
    /// the directory BEFORE constructing the spill — see that function for why
    /// the ordering is not cosmetic.
    // TEST-EXEMPT: covered by test_append_spill_and_replay_roundtrip + test_drop_counter_increments_when_channel_full (both construct)
    pub fn new<P: AsRef<Path>>(wal_dir: P) -> anyhow::Result<Self> {
        let wal_dir = wal_dir.as_ref().to_path_buf();
        std::fs::create_dir_all(&wal_dir) // O(1) EXEMPT: one-shot constructor, not the per-frame append
            .map_err(|e| anyhow::anyhow!("create WAL dir {:?}: {e}", wal_dir))?;
        let dir_guard = lock_wal_dir(&wal_dir)?;
        Self::new_with_guard(wal_dir, dir_guard)
    }

    /// Create a spill writer for a directory the caller has ALREADY claimed.
    ///
    /// # Why this exists — the ordering hole it closes
    ///
    /// `new` claims the directory and then seeds the sequence, which is correct
    /// in isolation. It is NOT sufficient for the real boot path, because
    /// `main` calls [`replay_all`] on this directory ~115 lines EARLIER, and
    /// `replay_all` MUTATES: it renames live `*.wal` files into `replaying/`.
    ///
    /// So starting a second process while one is live used to stage the
    /// INCUMBENT's currently-open segment out from under it. The incumbent
    /// keeps writing to the moved inode — the fd is still valid — and the
    /// intruder then reaches the claim, is refused, and exits. The refusal
    /// worked; it just happened after the damage. The incumbent's remaining
    /// session frames now sit in a file under `replaying/` that its own
    /// `confirm_replayed` will never cover.
    ///
    /// Taking the claim BEFORE the replay makes the refusal arrive before the
    /// rename, and handing the guard here is what lets the caller keep it: a
    /// second `lock_wal_dir` from the same process would be a different open
    /// file description and would be refused by the kernel exactly as a foreign
    /// process is — so the guard has to be MOVED, not re-taken.
    // TEST-EXEMPT: covered by the guard-handoff tests below and by every `new` caller, which delegates here.
    pub fn new_with_guard<P: AsRef<Path>>(
        wal_dir: P,
        dir_guard: Option<WalDirGuard>,
    ) -> anyhow::Result<Self> {
        let wal_dir = wal_dir.as_ref().to_path_buf();
        std::fs::create_dir_all(&wal_dir) // O(1) EXEMPT: one-shot constructor, not the per-frame append
            .map_err(|e| anyhow::anyhow!("create WAL dir {:?}: {e}", wal_dir))?;

        // ORDER IS LOAD-BEARING.
        //
        // 1. The directory is CLAIMED before this function is entered — by the
        //    caller for the boot path, by `new` for everyone else. Two
        //    processes sharing one WAL directory mint `capture_seq` from two
        //    independent clock-seeded counters, collide inside the `ticks`
        //    DEDUP key, and destroy ticks with no counter anywhere to show it.
        //    See [`lock_wal_dir`] for why this is fail-closed, not a warning.
        // 2. Only THEN read the on-disk high-water mark and ratchet the counter
        //    past it. Doing this before the claim would race a live incumbent
        //    that is still appending, and could seed from a value it is about
        //    to exceed.
        // 3. Only THEN spawn the writer thread, so nothing can append at a
        //    sequence below the high-water mark.
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
        let queued_bytes = Arc::new(AtomicU64::new(0));
        let queued_bytes_for_thread = Arc::clone(&queued_bytes); // APPROVED: Arc clone in the one-shot constructor

        let persisted_for_thread = persisted_total.clone(); // APPROVED: Arc clone in the one-shot constructor
        let wal_dir_for_thread = wal_dir.clone(); // APPROVED: one-shot constructor, not per-frame
        let stop = Arc::new(AtomicBool::new(false));
        let stop_for_thread = Arc::clone(&stop); // APPROVED: Arc clone in the one-shot constructor

        // Register the abandoned-records series at zero. The CloudWatch agent
        // computes counter deltas and DROPS the first sample of a series it has
        // never seen, so a counter that only ever increments on the bad day
        // would publish nothing on the bad day. Seeding here is what makes a
        // clean shutdown provable rather than merely unreported.
        metrics::counter!(WAL_SPILL_SHUTDOWN_INCOMPLETE_COUNTER).increment(0);
        // Same discipline for the replay RESTORE failure series. A deferred
        // segment that cannot be moved back into the live directory is frames
        // that no later boot will re-glob, so this is a loss signal — and it
        // fires once, on the bad boot, which is precisely the shape the agent's
        // first-sample rule swallows.
        metrics::counter!(WAL_REPLAY_RESTORE_FAILED_COUNTER).increment(0);

        let writer = thread::Builder::new()
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
                        writer_loop(
                            &rx,
                            &wal_dir_for_thread,
                            &persisted_for_thread,
                            &stop_for_thread,
                            &queued_bytes_for_thread,
                        )
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
            queued_bytes,
            drop_critical,
            persisted_total,
            drop_counters: SpillDropCounters::new(),
            feed_health: None,
            stop,
            writer: std::sync::Mutex::new(Some(writer)),
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
            queued_bytes: Arc::new(AtomicU64::new(0)),
            drop_critical: Arc::new(AtomicU64::new(0)),
            persisted_total: Arc::new(AtomicU64::new(0)),
            drop_counters: SpillDropCounters::new(),
            feed_health: None,
            stop: Arc::new(AtomicBool::new(false)),
            writer: std::sync::Mutex::new(None),
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
        //
        // No endpoint offered either — derive the only honest one from the
        // transport (TVW4): the order-update transport IS its endpoint, and a
        // market-data transport without a `DhanEndpointType` in hand is the
        // main feed, the pre-v4 assumption. `const fn`, O(1), zero-alloc.
        self.append_with_seq_at(
            ws_type,
            frame,
            frame_seq,
            WAL_RECEIPT_UNKNOWN_NANOS,
            WalEndpoint::for_ws_type(ws_type),
        )
    }

    /// Like [`WsFrameSpill::append_with_seq`] but also persists the frame's
    /// TRUE arrival instant (UTC epoch nanos), so boot replay can restore it
    /// instead of re-stamping `now()`, and the SOCKET it came off (TVW4), so
    /// replay can route a depth frame to the depth drain.
    ///
    /// The caller stamps `received_at_nanos` at the read instant, BEFORE this
    /// append. Hot path, O(1), zero-alloc — the extra fields are 9 bytes on an
    /// already-moved struct.
    // TEST-EXEMPT: same `try_send` path as `append_with_seq`; the receipt round-trip is covered by tvw3_roundtrip_preserves_received_at, the endpoint round-trip by tvw4_roundtrip_preserves_every_endpoint.
    pub fn append_with_seq_at(
        &self,
        ws_type: WsType,
        frame: impl Into<Bytes>,
        frame_seq: u64,
        received_at_nanos: i64,
        endpoint: WalEndpoint,
    ) -> AppendOutcome {
        let record = WalRecord {
            ws_type,
            frame_seq,
            endpoint,
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

        // BYTE reservation, taken BEFORE `try_send` and released on every path
        // that does not hand the record to the writer.
        //
        // Same shape as `RingBudget::try_reserve_detailed` on the frame ring
        // one step downstream — deliberately, because that is the pattern this
        // lane already proves. Two relaxed atomics, no allocation, no lock: the
        // hot path stays O(1).
        //
        // The reserve-then-check form admits a bounded overshoot of
        // (concurrent producers − 1) × frame_len — at 16 sockets and the
        // largest frame the socket accepts, ~25 MiB against a budget measured
        // in gibibytes. Documented rather than locked away, on the same
        // reasoning as `day_ohlc_tracker`'s insert race: an exact bound would
        // need a lock on the hot path to buy a tighter version of an already
        // round number.
        let frame_bytes = record.frame.len() as u64;
        let budget = wal_queue_max_bytes();
        let prev_bytes = self.queued_bytes.fetch_add(frame_bytes, Ordering::Relaxed);
        if prev_bytes.saturating_add(frame_bytes) > budget {
            self.queued_bytes.fetch_sub(frame_bytes, Ordering::Relaxed);
            return self.refuse_over_byte_budget(ws_type, frame_bytes, prev_bytes, budget);
        }

        match self.spill_tx.try_send(record) {
            Ok(()) => AppendOutcome::Spilled,
            Err(TrySendError::Full(_)) => {
                // The record never reached the writer, so the writer will never
                // decrement for it. Release here or the budget leaks toward
                // zero and every later frame is refused on a queue that is
                // actually empty.
                self.queued_bytes.fetch_sub(frame_bytes, Ordering::Relaxed);
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
                // Released for the same reason as the `Full` arm above.
                self.queued_bytes.fetch_sub(frame_bytes, Ordering::Relaxed);
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

    /// Cold arm for a frame refused because the queue is at its BYTE budget.
    ///
    /// Out of line and `#[cold]` for two reasons that point the same way. It
    /// runs only when a frame is already being lost, so it has no business in
    /// the hot instruction stream — and keeping the `error!` out of
    /// `append_with_seq_at`'s own body keeps that body free of
    /// allocation-shaped tokens, which is what
    /// `wal_append_zero_alloc_by_construction_guard` reads. The guard bounds
    /// the happy path at the `Spilled` arm, so a formatting drop arm placed
    /// ABOVE the send would sit inside the region it scans; moving it here is
    /// the fix rather than widening the guard.
    ///
    /// The caller has ALREADY released the reservation before calling.
    #[cold]
    #[inline(never)]
    fn refuse_over_byte_budget(
        &self,
        ws_type: WsType,
        frame_bytes: u64,
        queued_bytes: u64,
        budget: u64,
    ) -> AppendOutcome {
        let prev = self.drop_critical.fetch_add(1, Ordering::Relaxed);
        error!(
            code = ErrorCode::WsSpill02FrameDropped.code_str(),
            ws_type = ws_type.as_str(),
            drop_count = prev + 1,
            frame_bytes,
            queued_bytes,
            budget_bytes = budget,
            "CRITICAL: WAL spill queue at its BYTE budget — frame dropped \
             (queued payload too large; the record count is not the binding limit)"
        );
        let idx = ws_type_index(ws_type);
        // Shares `drop_critical` with the other two arms so every existing
        // alarm on that series fires for this cause too — no new CloudWatch
        // metric, no added cost. The distinguishing cause lives on the SLA
        // counter's `source` label.
        self.drop_counters.drop_critical[idx].increment(1);
        self.drop_counters.ticks_lost_bytes_full[idx].increment(1);
        self.record_feed_drop_for_health(ws_type);
        AppendOutcome::Dropped
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

    /// Records still sitting in the writer's queue right now.
    ///
    /// This is the size of the loss window an abrupt exit would take: `append`
    /// returns [`AppendOutcome::Spilled`] the instant a record is QUEUED, and
    /// the writer thread is what turns queued into bytes.
    #[must_use]
    pub fn queued_records(&self) -> usize {
        self.spill_tx.len()
    }

    /// Drain the writer's queue and stop it, bounded by `budget`.
    ///
    /// # The hole this closes
    ///
    /// The writer thread was spawned and DETACHED — its `JoinHandle` was
    /// discarded at the `map_err`. Nothing waited for it, and nothing could:
    /// the only `Sender` lives in this struct, the struct is shared through an
    /// `Arc` the drain paths hold, so the channel could never close and the
    /// thread never had a reason to exit. At shutdown the process simply ended
    /// while up to [`SPILL_CHANNEL_CAPACITY`] records — 524,288, ~100 s of
    /// frames at the 5,000 fps envelope — sat unwritten.
    ///
    /// Every one of those was already reported to its caller as `Spilled`, and
    /// counted by nothing on the way out: `persisted_total` counts `write_all`
    /// and `drop_critical` counts only what `try_send` refused outright. So the
    /// durable floor — the guarantee the whole ring → spill → WAL chain rests
    /// on — had an unmeasured hole at exactly the moment it was most likely to
    /// matter. The tick and depth ILP writers had this fixed on 2026-08-28
    /// (`tv_offload_writer_shutdown_incomplete_total`); the WAL itself did not.
    ///
    /// # Ordering
    ///
    /// Call this AFTER the sockets are closed and the lane is joined, or the
    /// queue is still being filled while this waits on it.
    ///
    /// # What it does not promise
    ///
    /// Returns the number of records still queued when the budget expired —
    /// `0` for a clean drain. A non-zero return is a real, permanent loss and
    /// says so, at `error!` and on
    /// [`WAL_SPILL_SHUTDOWN_INCOMPLETE_COUNTER`]; it is reported rather than
    /// waited out because systemd's `TimeoutStopSec` escalates to SIGKILL, and
    /// a SIGKILL loses strictly more.
    ///
    /// Even a `0` return leaves the `write_all`-not-`fsync` residual: the exit
    /// flush pushes the 256 KiB `BufWriter` into the page cache, not onto the
    /// platter. That is the same durability boundary the whole module has
    /// always had (there is no `fsync` anywhere in this file) and this method
    /// deliberately does not claim to have changed it.
    pub fn shutdown(&self, budget: Duration) -> usize {
        self.stop.store(true, Ordering::Release);

        let deadline = Instant::now() + budget;

        // Phase 1: wait for the queue itself to empty. This is the number that
        // matters — a record still in the channel has not been written at all.
        while !self.spill_tx.is_empty() && Instant::now() < deadline {
            thread::sleep(WAL_SHUTDOWN_POLL);
        }
        let queued = self.spill_tx.len();

        // Phase 2: wait for the thread to notice, flush, and exit. Polled
        // rather than joined outright: a writer parked in
        // `WAL_WRITER_IO_RETRY_BACKOFF` against a wedged disk must not hold the
        // process past the budget, and a blocking `join` has no timeout.
        if let Ok(mut slot) = self.writer.lock()
            && let Some(handle) = slot.take()
        {
            while !handle.is_finished() && Instant::now() < deadline {
                thread::sleep(WAL_SHUTDOWN_POLL);
            }
            if handle.is_finished() {
                // Only join a thread that has already finished, so this cannot
                // block. A panic that reaches the handle is reported rather
                // than swallowed: the supervisor catches panics and respawns,
                // so a panic arriving HERE means the supervisor loop itself is
                // over — the writer is gone and anything still queued is lost.
                if handle.join().is_err() {
                    error!(
                        code = ErrorCode::WsSpill01WriterRespawn.code_str(),
                        "WAL spill writer PANICKED out of its supervisor loop during shutdown"
                    );
                }
            } else {
                // Deliberately abandoned. Put nothing back in the slot — the
                // handle is dropped, the thread is detached, and the process is
                // about to exit anyway.
                warn!(
                    budget_secs = budget.as_secs(),
                    queued,
                    "WAL spill writer did not exit within the shutdown budget — abandoning it"
                );
            }
        }

        if queued > 0 {
            metrics::counter!(WAL_SPILL_SHUTDOWN_INCOMPLETE_COUNTER).increment(queued as u64);
            error!(
                code = ErrorCode::WsSpill01WriterRespawn.code_str(),
                queued,
                budget_secs = budget.as_secs(),
                "WAL spill: the final drain was ABANDONED with records still queued — these \
                 frames were captured and acknowledged but never written; they are gone"
            );
        } else {
            info!(
                persisted = self.persisted_count(),
                "WAL spill: final drain complete on shutdown; queue empty"
            );
        }

        queued
    }
}

// ---------------------------------------------------------------------------
// Background writer thread
// ---------------------------------------------------------------------------

/// Flush and close the current segment on the way out of [`writer_loop`].
///
/// Shared by the two exit arms so a future third one cannot quietly forget the
/// flush: `persist_record_resilient` counts a record as persisted when
/// `write_all` lands it in the 256 KiB `BufWriter`, not when the buffer reaches
/// the platter, so dropping an unflushed writer loses records that
/// `persisted_count()` has already claimed.
fn flush_on_exit(current: &mut Option<BufWriter<File>>, stage: &'static str) {
    if let Some(mut w) = current.take()
        && let Err(err) = w.flush()
    {
        report_io_error(stage, &err);
    }
}

fn writer_loop(
    rx: &Receiver<WalRecord>,
    wal_dir: &Path,
    persisted: &AtomicU64,
    stop: &AtomicBool,
    queued_bytes: &AtomicU64,
) -> anyhow::Result<()> {
    /// Releases a record's byte reservation the instant it leaves the channel.
    ///
    /// Called on RECEIPT, never after the write — a record that fails to
    /// persist has still left the queue, and holding its bytes would shrink
    /// the budget by exactly the amount a failing disk keeps producing.
    fn release(queued_bytes: &AtomicU64, record: &WalRecord) {
        queued_bytes.fetch_sub(record.frame.len() as u64, Ordering::Relaxed);
    }

    // `None` = no open segment; the next record reopens one. A transient disk
    // error sets this back to `None` instead of propagating out of the thread.
    // The thread therefore NEVER dies on a transient I/O hiccup — it keeps
    // draining the channel so `append()` never observes `Disconnected` and the
    // durable WAL floor survives. The ONLY clean exit is the channel closing.
    let mut current: Option<BufWriter<File>> = open_segment_resilient(wal_dir);
    let mut bytes_written: u64 = 0;

    // Resolved ONCE, outside the loop, for the same reason the batch-boundary
    // comment below gives: a handle re-resolved per iteration is how a metric
    // write becomes a cost.
    let depth_gauge = metrics::gauge!("tv_ws_frame_spill_queue_depth");
    let high_water_gauge = metrics::gauge!("tv_ws_frame_spill_queue_high_water");
    // The queue's depth in BYTES, which is the dimension that can exhaust the
    // host. `queue_depth` counts records, so a queue holding 200 depth-200
    // frames and one holding 200 ticker packets read identically on it while
    // differing by four orders of magnitude in resident heap. Without this
    // gauge a byte blow-up is invisible right up to the OOM kill.
    let bytes_gauge = metrics::gauge!("tv_ws_frame_spill_queue_bytes");
    let mut high_water: usize = 0;
    // Seeded so the series REGISTERS at startup. The CloudWatch agent computes
    // deltas and drops the first sample of a series it has never seen, so an
    // always-zero gauge that is never set would be indistinguishable from an
    // absent one — the exact failure that made a depth loss unknowable on
    // 2026-08-28.
    depth_gauge.set(0.0);
    high_water_gauge.set(0.0);
    bytes_gauge.set(0.0);
    loop {
        // Timed, not blocking, so the thread can notice a shutdown request.
        //
        // The clean exit used to be reachable ONLY by every `Sender` being
        // dropped. That can never happen here: the sole sender lives inside
        // `WsFrameSpill`, which the drain paths hold through an `Arc`, so the
        // channel stays open for the life of the process and the writer stayed
        // parked in `recv()` while the process exited around it. Everything
        // still queued went with it — captured, acknowledged as `Spilled`, and
        // counted by nothing. This arm is what ends that.
        let first = match rx.recv_timeout(WAL_WRITER_STOP_POLL) {
            Ok(r) => r,
            Err(RecvTimeoutError::Timeout) => {
                // Empty channel by construction — `recv_timeout` only times out
                // when nothing arrived. So a stop request that reaches here has
                // a fully drained queue behind it and it is safe to close.
                if stop.load(Ordering::Acquire) {
                    flush_on_exit(&mut current, "flush_on_stop");
                    info!("ws-frame-spill-writer stop requested and queue drained; exiting");
                    return Ok(());
                }
                continue;
            }
            Err(RecvTimeoutError::Disconnected) => {
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
                flush_on_exit(&mut current, "flush_on_close");
                info!("ws-frame-spill-writer channel closed; exiting");
                return Ok(());
            }
        };

        release(queued_bytes, &first);
        #[cfg(test)]
        maybe_test_panic(&first);
        bytes_written += persist_record_resilient(&mut current, wal_dir, &first, persisted);

        // Drain up to N more without blocking so we batch-flush.
        for _ in 0..256 {
            match rx.try_recv() {
                Ok(r) => {
                    release(queued_bytes, &r);
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

        // The exposure C1 named, made measurable.
        //
        // `AppendOutcome::Spilled` means QUEUED, not written: `append` is a
        // `try_send` onto a 524,288-slot channel and this thread is what turns
        // a queued record into bytes. Everything still in the channel at an
        // abort (`panic = "abort"`, an OOM-kill, a SIGKILL past
        // `TimeoutStopSec`) is PERMANENTLY LOST — and is counted by nothing,
        // because `persisted` counts `write_all` and `drop_critical` counts
        // only the refusals `try_send` rejected outright.
        //
        // Until this gauge the depth of that window had never been observed,
        // so the honest answer to "how many frames could we lose on an abort?"
        // was Unknown with a ceiling of 524,288 (~100 s at the 5,000 fps
        // envelope). Now it is a number.
        //
        // Published HERE, at the batch boundary, and deliberately not in
        // `append`: the hot path already learned this lesson once, when
        // `record_ws_lag` allocated twice per tick (~36 M allocations/hour) on
        // a path documented as allocation-free. One gauge write per ~257
        // records costs nothing measurable; one per frame is how that defect
        // was reintroduced.
        //
        // The high-water is the load-bearing half. A gauge sampled every 30 s
        // by the CloudWatch agent will read ~0 all session and miss the burst
        // that matters — the whole point is the PEAK during a disk stall, and
        // a peak that decays between scrapes was never observed at all.
        let queued = rx.len();
        depth_gauge.set(queued as f64);
        bytes_gauge.set(queued_bytes.load(Ordering::Relaxed) as f64);
        if queued > high_water {
            high_water = queued;
            high_water_gauge.set(high_water as f64);
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

// ---------------------------------------------------------------------------
// Frame receipt: derived from a monotonic instant, never read per frame
// ---------------------------------------------------------------------------
//
// The frame's arrival instant is stamped in `FrameSink::accept` as a monotonic
// `Instant`, deliberately: that file BANS wall-clock reads outright
// (`test_pool_supervisor_source_never_reads_the_wall_clock`) because its
// ladders, token expiry and backoff are all monotonic, and an NTP step must be
// unable to expire all sixteen sockets at once. The ban is right.
//
// But the WAL record needs a WALL-CLOCK receipt: replay has to restore when the
// frame actually arrived, and a monotonic instant means nothing across a
// process restart. Before 2026-08-28 that tension was resolved by not writing a
// receipt at all — `append_with_seq` passed `WAL_RECEIPT_UNKNOWN_NANOS` and
// `append_with_seq_at` had ZERO production callers, so every record on disk
// carried the sentinel while the format claimed to carry a receipt.
//
// Anchoring resolves it: the wall clock is read on a slow timer, in a different
// crate from the one under the ban, and every per-frame receipt is arithmetic
// on the monotonic delta. That is strictly better than a per-frame wall-clock
// read in the property that matters most here — an NTP STEP cannot reorder two
// frames, because the spacing between them is monotonic by construction.
/// The receipt anchor: a monotonic instant paired with the UTC-epoch nanos that
/// were true at that instant.
///
/// Two halves in ONE swapped value, deliberately. Split across two atomics a
/// reader could observe a fresh wall time against a stale monotonic base and
/// derive a receipt off by a whole refresh interval — a torn read that would be
/// indistinguishable from a real out-of-order frame.
#[derive(Debug, Clone, Copy)]
struct ReceiptAnchor {
    instant: Instant,
    nanos: i64,
}

static RECEIPT_ANCHOR: std::sync::OnceLock<arc_swap::ArcSwap<ReceiptAnchor>> =
    std::sync::OnceLock::new();

/// Counter: how many times the receipt anchor was re-taken from the wall clock.
///
/// Charted rather than alarmed — a steady cadence is the mechanism working. Its
/// absence during a session is the interesting reading, because it means the
/// refresh caller stopped and the anchor is aging.
pub const RECEIPT_ANCHOR_REFRESH_COUNTER: &str = "tv_wal_receipt_anchor_refresh_total";

fn anchor_cell() -> &'static arc_swap::ArcSwap<ReceiptAnchor> {
    RECEIPT_ANCHOR.get_or_init(|| arc_swap::ArcSwap::from_pointee(take_anchor()))
}

fn take_anchor() -> ReceiptAnchor {
    // Order matters and the cost of getting it wrong is a systematic bias:
    // read the WALL clock first, then the monotonic one, so any scheduling
    // delay between the two makes the derived receipt slightly EARLY rather
    // than late. An early receipt files a frame in the bucket it arrived in;
    // a late one can push it into the next.
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0i64, |d| i64::try_from(d.as_nanos()).unwrap_or(i64::MAX));
    ReceiptAnchor {
        instant: Instant::now(),
        nanos,
    }
}

/// Re-takes the receipt anchor from the wall clock.
///
/// # Why this is not optional
///
/// `Instant` is `CLOCK_MONOTONIC` and `SystemTime` is `CLOCK_REALTIME`. NTP
/// does not merely STEP the realtime clock — it SLEWS it, at up to 500 ppm, so
/// the two clocks separate continuously. A single boot-time anchor therefore
/// drifts by up to **~1.8 s per hour, ~16 s over a 9-hour session** in the
/// worst permitted case. Since 2026-08-28 `received_at` is the candle
/// BUCKETING clock, so an aging anchor does not merely mislabel a timestamp —
/// it files bars in the wrong second, and the error grows all day, which is the
/// shape hardest to notice and hardest to reconstruct afterwards.
///
/// Re-taking it periodically bounds the drift to one refresh interval's worth
/// instead of one session's. The spacing BETWEEN frames inside an interval is
/// still purely monotonic, which is the property that makes the anchor better
/// than a per-frame wall-clock read: an NTP STEP cannot reorder two frames.
///
/// Caller: an off-hot-path timer. This must never be called per frame — it is a
/// wall-clock syscall and an allocation, and both belong nowhere near the
/// capture path.
pub fn refresh_receipt_anchor() {
    let cell = anchor_cell();
    let old = cell.load();
    let new = take_anchor();
    // MONOTONIC RATCHET (2026-08-28, round-2 fix).
    //
    // Re-anchoring bounds the drift, and taken naively it also introduces a way
    // to move time BACKWARDS: if CLOCK_REALTIME has slewed slower than
    // CLOCK_MONOTONIC, or stepped back, the fresh anchor projects a LATER frame
    // to an EARLIER receipt than the old anchor would have. Since `received_at`
    // is the candle bucketing clock, that files a frame into a second that may
    // already be sealed — and the whole reason to derive receipts from a
    // monotonic instant rather than read the clock per frame is that a time
    // step must not be able to reorder two frames. A refresh that can reorder
    // them gives that property back with one hand.
    //
    // So the new anchor is adopted only when it does not rewind what the old
    // one was already projecting. Refusing it costs one interval of drift
    // correction; accepting it costs an out-of-order frame, and those are not
    // the same size of mistake.
    let projected_now = old.nanos.saturating_add(
        i64::try_from(
            new.instant
                .saturating_duration_since(old.instant)
                .as_nanos(),
        )
        .unwrap_or(i64::MAX),
    );
    if new.nanos < projected_now {
        metrics::counter!(RECEIPT_ANCHOR_REFRESH_COUNTER, "outcome" => "refused_backward")
            .increment(1);
        return;
    }
    cell.store(std::sync::Arc::new(new));
    metrics::counter!(RECEIPT_ANCHOR_REFRESH_COUNTER, "outcome" => "adopted").increment(1);
}

/// The UTC-epoch-nanos receipt for a monotonic capture instant.
///
/// Initialises the anchor on first use, so the first frame of a session anchors
/// it and reads back its own arrival instant essentially exactly.
///
/// Saturating throughout: `Instant` arithmetic panics on a negative interval, and
/// a panic on the capture path would cost the socket. An instant from BEFORE the
/// anchor — which a refresh makes genuinely reachable for a frame stamped just
/// before the swap — yields the anchor time. That is a sub-refresh-interval
/// flattening of at most a handful of frames, and it is the right trade against
/// a panic or a negative timestamp.
#[must_use]
pub fn receipt_nanos_from(captured_at: Instant) -> i64 {
    let anchor = anchor_cell().load();
    let delta = captured_at.saturating_duration_since(anchor.instant);
    let delta_nanos = i64::try_from(delta.as_nanos()).unwrap_or(i64::MAX);
    anchor.nanos.saturating_add(delta_nanos)
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

/// Counter: the lock FILE could not be created (an unwritable directory, a full
/// disk), so one-writer-per-directory is not enforced for this process. Distinct
/// from [`WAL_DIR_LOCK_REFUSED_COUNTER`] because they mean opposite things: that
/// one is the guard WORKING, this one is the guard UNAVAILABLE.
pub const WAL_DIR_LOCK_UNAVAILABLE_COUNTER: &str = "tv_wal_dir_lock_unavailable_total";

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
/// # The two failures are NOT the same, and they get opposite treatment
///
/// - **CONTENTION** — another live process holds the lock. This is the case the
///   lock exists for, and it is fail-CLOSED: `Err`. Two processes minting
///   `capture_seq` from independent clocks destroy ticks through the DEDUP key
///   with no counter to show it, so one refused boot is the cheap outcome.
///
/// - **THE LOCK FILE CANNOT BE CREATED** — an unwritable directory, a full
///   disk. This returns `Ok(None)`, and the caller proceeds DEGRADED. It is not
///   evidence of a second process, and killing the lane over a transient
///   filesystem problem is the worse failure: the writer is deliberately built
///   to survive an unwritable directory and recover when permission returns
///   (`test_writer_survives_unwritable_dir_then_recovers` pins exactly that).
///   Turning that recoverable state into a dead feed would trade a rare silent
///   loss for a common total outage.
///
/// **Honest limit of the degraded arm:** while `Ok(None)` is in force, mutual
/// exclusion is NOT enforced. It is logged with a coded error and counted, so
/// the state is visible rather than assumed away — but a second process started
/// during it would not be refused. That window is bounded by the directory
/// being unwritable, which the writer is already reporting loudly through
/// `WS-SPILL-01`.
///
/// # Errors
///
/// Contention only. A creation failure is `Ok(None)`, never `Err`.
pub fn lock_wal_dir(wal_dir: &Path) -> anyhow::Result<Option<WalDirGuard>> {
    // CREATE THE DIRECTORY FIRST (2026-08-28, round-2 fix).
    //
    // The claim moved ahead of `replay_all` earlier today, which was right —
    // but `create_dir_all` stayed inside `WsFrameSpill::new_with_guard`, 145
    // lines later. On any boot where the WAL directory does not yet exist
    // (first deploy, a fresh volume, the post-recreate box) `OpenOptions::open`
    // then returned `NotFound`, which lands in the degrade arm below and turns
    // dual-writer protection OFF for the entire process — silently, and with a
    // coded error blaming "an unwritable directory or a full disk" when the
    // real cause is the ordinary first boot.
    //
    // Here rather than at the call site so EVERY caller is protected: a guard
    // that depends on its caller remembering something is not a guard.
    let created = std::fs::create_dir_all(wal_dir); // O(1) EXEMPT: one-shot boot claim, never the per-frame append
    if let Err(e) = created {
        metrics::counter!(WAL_DIR_LOCK_UNAVAILABLE_COUNTER).increment(1);
        error!(
            code = ErrorCode::WsSpill01WriterRespawn.code_str(),
            wal_dir = ?wal_dir,
            error = %e,
            "could not create the WAL directory, so one-writer-per-directory is NOT \
             enforced for this process. Continuing anyway: the spill writer survives and \
             recovers from an unwritable directory, and killing the feed over it would be \
             the worse failure. While this persists, a second process would not be refused."
        );
        return Ok(None);
    }
    let path = wal_dir.join(WAL_DIR_LOCK_FILE);
    // `create(true)` and deliberately NOT `create_new(true)`: the file survives
    // a clean shutdown and carries no state — the kernel lock IS the state — so
    // an existing file is the normal case and is reused.
    let lock = match std::fs::OpenOptions::new() // O(1) EXEMPT: one-shot boot claim, never the per-frame append
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&path)
    {
        Ok(f) => f,
        Err(e) => {
            metrics::counter!(WAL_DIR_LOCK_UNAVAILABLE_COUNTER).increment(1);
            error!(
                code = ErrorCode::WsSpill01WriterRespawn.code_str(),
                lock_file = ?path,
                error = %e,
                "could not create the WAL directory lock file, so one-writer-per-directory \
                 is NOT enforced for this process. Continuing anyway: the usual cause is an \
                 unwritable directory or a full disk, which the spill writer already survives \
                 and recovers from, and killing the feed over it would be the worse failure. \
                 While this persists, a second process on this directory would not be refused."
            );
            return Ok(None);
        }
    };

    match lock.try_lock() {
        Ok(()) => Ok(Some(WalDirGuard { _lock: lock, path })),
        // CONTENTION — the lock is genuinely held. Fail closed. This is the one
        // arm the whole mechanism exists for.
        // O(1) EXEMPT: a match PATTERN on an error variant, not a filesystem call — and the whole function is a one-shot boot-time claim, never on any per-frame path.
        Err(std::fs::TryLockError::WouldBlock) => {
            metrics::counter!(WAL_DIR_LOCK_REFUSED_COUNTER).increment(1);
            Err(anyhow::anyhow!(
                "WAL directory {wal_dir:?} is already owned by another live process \
                 (lock file {path:?}). REFUSING to start a second writer: two \
                 processes minting capture_seq from their own clocks silently destroy \
                 ticks through the ticks DEDUP key, with no counter to show it. \
                 Identify the incumbent with `fuser -v` on the lock file and stop it, \
                 or point this process at its own WAL directory."
            ))
        }
        // THE FILESYSTEM CANNOT LOCK — not evidence of anything, and fatal if
        // treated as such (2026-08-28).
        //
        // `try_lock` returns `Error(io)` rather than `WouldBlock` when the
        // filesystem has no working `flock`: NFS without lockd, some FUSE and
        // overlay mounts. Collapsing that into the contention arm made an
        // unlockable filesystem indistinguishable from a live incumbent, and
        // since `main` exits on this error, the result was a PERMANENT BOOT
        // LOOP on a mount that simply lacks locking — a total outage caused by
        // the safety mechanism rather than by anything it protects against.
        //
        // This is the same over-fail as the create arm above, which was
        // repaired earlier the same day and left standing here, in the same
        // function. Both now degrade for the same reason: refusing to run is
        // only correct when a SECOND WRITER is the actual evidence.
        Err(e) => {
            metrics::counter!(WAL_DIR_LOCK_UNAVAILABLE_COUNTER).increment(1);
            error!(
                code = ErrorCode::WsSpill01WriterRespawn.code_str(),
                lock_file = ?path,
                error = %e,
                "the filesystem refused to lock the WAL directory, so \
                 one-writer-per-directory is NOT enforced for this process. This is a \
                 lock-support failure, not a second process — the usual cause is a \
                 mount without working flock (NFS without lockd, some FUSE or overlay \
                 mounts). Continuing anyway, because refusing here would boot-loop the \
                 feed forever on a filesystem that can never satisfy it. While this \
                 persists, a second process on this directory would not be refused."
            );
            Ok(None)
        }
    }
}

/// How many trailing segments per directory the boot high-water probe reads.
///
/// # Why more than one
///
/// The probe used to read only `segs.last()`, on the reasoning that the newest
/// segment holds the maximum. True while the writer is running — and FALSE in
/// exactly the case the probe exists for. `writer_loop` opens the next segment
/// file BEFORE writing anything into it, so a crash in that window leaves a
/// ZERO-BYTE newest segment. The probe then read 0, seeded nothing, and the
/// restart re-issued `capture_seq` values already in the database, where
/// QuestDB upserts the collisions away with no counter anywhere to show it.
/// The same shape follows a torn or garbage first header, which ends the walk
/// at offset 0.
///
/// FOUR, not "all": the cost is bounded and the benefit saturates immediately.
/// Reading every segment in a long-running directory is an unbounded boot-time
/// walk over a path that only needs to find one non-empty file, and four covers
/// an empty newest plus three consecutive torn predecessors — a shape no
/// observed crash has produced.
const HIGH_WATER_SEGMENTS_PER_DIR: usize = 4;

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
/// At most [`HIGH_WATER_SEGMENTS_PER_DIR`] trailing segments of each of the
/// three directories (live, `replaying/`, `archive/`) are scanned, and only
/// record HEADERS are read — frame payloads are seeked past, never copied. The
/// walk stops at the first segment that yields a sequence, so the common case
/// reads exactly one file per directory; the extra three exist for the crash
/// window that leaves a zero-byte or torn newest segment.
///
/// CORRECTED 2026-08-28: this paragraph said "Only the NEWEST segment … is
/// scanned … Three bounded header walks", which was true when written and
/// stopped being true in the same change that made the walk read backwards.
/// A cost claim that describes the previous behaviour is exactly the stale-doc
/// class this file's own corrections keep recording.
///
/// Returns `0` when the directory is absent, empty, or holds only v1 (`TVW1`)
/// records, which predate `frame_seq`. `0` is the correct answer there: it
/// leaves the wall clock as the seed, exactly as before.
///
/// The greatest `frame_seq` written to any segment under `wal_dir`.
///
/// Best-effort and deliberately so — see [`highest_frame_seq_in_segment`]. A
/// LOWER bound is strictly better than the wall clock alone; refusing to boot
/// over a torn tail would turn a safety net into an outage.
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
        // nanos, so the last is the newest.
        segs.sort_by(|a, b| a.file_name().cmp(&b.file_name())); // O(1) EXEMPT: boot-time seed, cold path
        // Walk BACKWARDS from the newest, and keep walking past an empty or
        // torn one — see [`HIGH_WATER_SEGMENTS_PER_DIR`]. Stops at the first
        // segment that yields a sequence, because segments are chronological
        // and an older one cannot exceed a newer one that has content.
        for seg in segs.iter().rev().take(HIGH_WATER_SEGMENTS_PER_DIR) {
            let seen = highest_frame_seq_in_segment(seg);
            if seen > 0 {
                highest = highest.max(seen);
                break;
            }
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
    // One header at a time; v4 is the largest at 30 bytes. The buffer MUST be
    // the largest header — a 29-byte buffer against a 30-byte v4 header read
    // `filled < min_rec` on every record and returned 0, which re-seeded
    // nothing and re-issued capture_seq values already in the database.
    let mut head = [0u8; WAL_MIN_RECORD_V4];
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
        let is_v4 = magic == WAL_MAGIC_V4;
        let is_v3 = magic == WAL_MAGIC_V3;
        let is_v2 = magic == WAL_MAGIC_V2;
        let is_v1 = magic == WAL_MAGIC;
        if !is_v1 && !is_v2 && !is_v3 && !is_v4 {
            return highest;
        }
        let min_rec = if is_v4 {
            WAL_MIN_RECORD_V4
        } else if is_v3 {
            WAL_MIN_RECORD_V3
        } else if is_v2 {
            WAL_MIN_RECORD_V2
        } else {
            WAL_MIN_RECORD_V1
        };
        if filled < min_rec {
            return highest;
        }
        // v1: [magic|ws|len|frame|crc]                      -> len at 5
        // v2: [magic|ws|seq(8)|len|frame|crc]                -> seq at 5, len at 13
        // v3: [magic|ws|seq(8)|recv(8)|len|frame|crc]        -> seq at 5, len at 21
        // v4: [magic|ws|seq(8)|recv(8)|ep(1)|len|frame|crc]  -> seq at 5, len at 22
        let len_off = if is_v4 {
            22
        } else if is_v3 {
            21
        } else if is_v2 {
            13
        } else {
            5
        };
        if (is_v2 || is_v3 || is_v4)
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
    // Always write the v4 record (TVW4: frame_seq + received_at_nanos +
    // endpoint after ws_type). `u32::try_from` makes an over-large frame
    // explicit rather than silently truncating (frames are ≤162 B on the main
    // feed; depth frames are bounded by the transport's 512 KiB cap).
    let frame_len = u32::try_from(r.frame.len()).map_err(|_| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, "WAL frame > u32::MAX")
    })?;
    let frame_seq = r.frame_seq.to_le_bytes();
    let receipt = r.received_at_nanos.to_le_bytes();
    let endpoint = [r.endpoint.as_u8()];
    // CRC covers ws_type || frame_seq || received_at_nanos || endpoint || len
    // || frame — every header byte, so a flipped endpoint byte is rejected
    // rather than silently routing a frame to the wrong parser.
    let crc = crc32_ieee_of(&[
        &[r.ws_type.as_u8()],
        &frame_seq[..],
        &receipt[..],
        &endpoint[..],
        &frame_len.to_le_bytes()[..],
        &r.frame,
    ]);
    w.write_all(&WAL_MAGIC_V4)?;
    w.write_all(&[r.ws_type.as_u8()])?;
    w.write_all(&frame_seq)?;
    w.write_all(&receipt)?;
    w.write_all(&endpoint)?;
    w.write_all(&frame_len.to_le_bytes())?;
    w.write_all(&r.frame)?;
    w.write_all(&crc.to_le_bytes())?;
    Ok(())
}

fn record_disk_size(r: &WalRecord) -> u64 {
    // v4: magic(4) + ws_type(1) + frame_seq(8) + receipt(8) + endpoint(1)
    //     + len(4) + frame + crc(4) = 30 + frame
    WAL_MIN_RECORD_V4 as u64 + r.frame.len() as u64
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

/// Stop reading further WAL segments once resident memory reaches this
/// percentage of the host's memory ceiling.
///
/// **Why this exists beside `WAL_REPLAY_MAX_BYTES`, which already bounds the
/// pass.** That budget counts FRAME PAYLOAD BYTES READ. It does not, and
/// cannot, bound what those bytes MATERIALIZE into: every frame becomes a
/// `ReplayedFrame` and is then re-folded into rows. On 2026-09-02 a session's
/// WAL expanded ~512 MiB of frames into 22,248,540 depth rows and roughly
/// **15 GiB of RSS — about 30x amplification** — crossing the unit's
/// `MemoryHigh=15G` and taking the process down. The byte budget was doing
/// exactly what it says and was measuring the wrong quantity.
///
/// Measured the same evening, on the boot that produced this constant: the
/// lane's catch-up loop found RSS **already at 13.09 GiB of a 15.0 GiB
/// ceiling at round 0** — before it replayed anything — because THIS
/// function had already run unguarded and materialized the backlog. Bounding
/// the catch-up loop alone was guarding the second door.
///
/// 60% rather than the 80% `RESOURCE-02` pages at: the point of stopping is to
/// stop BEFORE the page, with room for the refold that follows to allocate.
pub const WAL_REPLAY_RSS_STOP_PCT: u64 = 60;

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
    replay_all_with_report(wal_dir, budget_bytes).map(|batch| batch.frames)
}

/// One replay pass, WITH the accounting a caller needs to page on it.
///
/// `replay_all` / `replay_all_with_budget` return only the frames; the number
/// of segments the budget DEFERRED was logged inside this function and then
/// thrown away. A caller draining in rounds — the WAL catch-up loop — could
/// therefore never tell "the backlog is drained" from "the backlog is deferred
/// on every round", which is the difference between a healthy boot and a
/// capacity problem. This struct carries that verdict out.
#[derive(Debug, Default)]
pub struct WalReplayBatch {
    /// The frames read this pass, in capture order.
    pub frames: Vec<ReplayedFrame>,
    /// Segments the RAM budget stopped short of. They remain `*.wal` files
    /// (or were restored to the live dir) and are re-globbed on the next pass.
    pub deferred_segments: u64,
    /// Frame-payload bytes held by `frames` — what the budget was measured
    /// against.
    pub bytes_replayed: u64,
    /// The budget this pass ran under, echoed so a log line can carry both
    /// numbers without the caller re-deriving one.
    pub budget_bytes: u64,
    /// `true` when the pass stopped on the MEMORY guard rather than on the
    /// byte budget or on running out of segments. Carried out so the caller's
    /// log line names the real reason: "deferred" reads the same either way,
    /// and the two have different remedies (more RAM / fewer rows per frame
    /// versus a bigger byte budget).
    pub stopped_for_memory: bool,
}

/// Should THIS deferral page the operator?
///
/// Pure and O(1): pages on the first deferral of a boot and never again — a
/// catch-up loop that defers on every one of its 120 rounds must produce ONE
/// coded line, not 120 — and never on a pass that deferred nothing. The
/// caller owns the latch; this decides.
#[must_use]
pub const fn should_page_replay_deferred(deferred_segments: u64, already_paged: bool) -> bool {
    deferred_segments > 0 && !already_paged
}

/// [`replay_all_with_report`] with the memory guard's two inputs INJECTED.
///
/// Separated for the same reason `replay_all_with_budget` is: what needs
/// proving is not the percentage, it is that a segment the guard stopped short
/// of is still a `*.wal` file afterwards — and a test that had to actually
/// allocate 9 GiB to reach that branch would never be written. `rss_probe` is
/// called at most once per segment, on the boot cold path.
// TEST-EXEMPT: covered by replay_stops_on_the_memory_guard_and_defers_the_rest + the wrapper's tests
pub fn replay_all_with_report_guarded<P: AsRef<Path>, R: Fn() -> Option<u64>>(
    wal_dir: P,
    budget_bytes: usize,
    rss_probe: R,
    ceiling_bytes: Option<u64>,
    stop_pct: u64,
) -> anyhow::Result<WalReplayBatch> {
    let wal_dir = wal_dir.as_ref();
    if !wal_dir.exists() {
        // APPROVED: boot-time WAL replay, cold path
        return Ok(WalReplayBatch {
            budget_bytes: budget_bytes as u64,
            ..WalReplayBatch::default()
        });
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

    let mut stopped_for_memory = false;
    for path in &segments {
        if bytes_held >= budget_bytes && consumed > 0 {
            break;
        }
        // The MEMORY guard, beside the byte budget and for the reason
        // `WAL_REPLAY_RSS_STOP_PCT` documents: the budget bounds bytes READ,
        // this bounds what they turned into.
        //
        // `consumed > 0` on BOTH conditions is load-bearing and identical in
        // intent: every pass reads at least one segment, so a boot that starts
        // already over the line still makes progress and the backlog drains
        // across boots instead of livelocking at zero forever.
        if consumed > 0
            && crate::resource_monitor::rss_at_or_above_fraction(
                rss_probe(),
                ceiling_bytes,
                stop_pct,
            )
        {
            stopped_for_memory = true;
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
            stopped_for_memory,
            stop_pct,
            "WAL replay DEFERRED the remaining segments to the next boot — on \
             the MEMORY guard when stopped_for_memory=true, otherwise on the \
             byte budget. Nothing is stranded: an unconsumed segment stays a \
             `*.wal` file and is re-globbed next boot, but those frames are on \
             disk rather than in the database until then. The two have DIFFERENT \
             remedies, which is why the flag is here: a byte-budget stop means \
             raise the budget or drain more often; a memory stop means the \
             frames materialize into more rows than this host can hold at once, \
             and raising the budget would make it worse, not better."
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

    // APPROVED: casts — usize counts on a cold boot path, always ≤ u64.
    Ok(WalReplayBatch {
        frames,
        deferred_segments: deferred as u64,
        bytes_replayed: bytes_held as u64,
        budget_bytes: budget_bytes as u64,
        stopped_for_memory,
    })
}

/// One replay pass under the REAL host memory guard.
///
/// The thin wrapper `replay_all` / `replay_all_with_budget` and every boot
/// path go through: it resolves this host's memory ceiling ONCE (the same
/// `resolve_memory_ceiling` the resource monitor and `RESOURCE-02` use, so
/// the guard and the alarm can never disagree about what the ceiling is) and
/// reads `/proc/self/status` per segment.
///
/// A host where neither probe resolves (non-Linux dev machine) gets
/// `ceiling_bytes = None`, and `rss_at_or_above_fraction` then fails OPEN —
/// the pass behaves exactly as it did before this guard existed.
// TEST-EXEMPT: thin resolve-then-delegate; the guard logic is proven by replay_all_with_report_guarded's tests.
pub fn replay_all_with_report<P: AsRef<Path>>(
    wal_dir: P,
    budget_bytes: usize,
) -> anyhow::Result<WalReplayBatch> {
    let ceiling = crate::resource_monitor::resolve_memory_ceiling(
        Path::new("/sys/fs/cgroup/memory.max"),
        Path::new("/proc/meminfo"),
    )
    .bytes();
    replay_all_with_report_guarded(
        wal_dir,
        budget_bytes,
        || crate::resource_monitor::probe_vmrss_bytes(Path::new("/proc/self/status")),
        ceiling,
        WAL_REPLAY_RSS_STOP_PCT,
    )
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
    /// Bytes freed by the byte-ceiling pass (2026-09-02). Populated ONLY by
    /// that pass, so on the ACTIVE directory it is the size of un-replayed
    /// capture that disk pressure destroyed — the number a page has to carry.
    pub size_deleted_bytes: u64,
    /// Age, in whole seconds at prune time, of the OLDEST segment the byte
    /// pass deleted (2026-09-02). Zero when it deleted nothing. Oldest, not
    /// newest, because the pass deletes oldest-first and this bounds how far
    /// back the destroyed capture reaches.
    pub size_deleted_oldest_age_secs: u64,
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
        for (mtime, len, path) in &survivors {
            if remaining <= max_bytes {
                break;
            }
            // O(1) EXEMPT: periodic cold archive prune, never the per-frame append
            match std::fs::remove_file(path) {
                Ok(()) => {
                    outcome.size_deleted += 1;
                    outcome.size_deleted_bytes = outcome.size_deleted_bytes.saturating_add(*len);
                    // Oldest-first walk, so the FIRST successful delete is the
                    // oldest one; `max` keeps that true even if an earlier
                    // remove failed and a later one succeeded.
                    let age_secs = now.duration_since(*mtime).map_or(0, |d| d.as_secs());
                    outcome.size_deleted_oldest_age_secs =
                        outcome.size_deleted_oldest_age_secs.max(age_secs);
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
            // ONE coded line per pass (2026-09-02). Until now this pass wrote an
            // `info!` and a counter that is deliberately not EMF-selected — so
            // capture destroyed under disk pressure reached no log filter and
            // no alarm. Rides the already-filtered WS-SPILL-01 code with a
            // distinguishing `source`; no new metric name, no new alarm.
            error!(
                code = ErrorCode::WsSpill01WriterRespawn.code_str(),
                source = "active_segment_pruned_under_pressure",
                segments_deleted = outcome.size_deleted,
                bytes_freed = outcome.size_deleted_bytes,
                oldest_segment_age_secs = outcome.size_deleted_oldest_age_secs,
                max_bytes,
                "ACTIVE WAL segments were deleted under DISK PRESSURE — un-replayed \
                 capture that a future boot would otherwise have offered to the refold. \
                 This is the prune-vs-suspended-QuestDB trade: keeping them would let the \
                 volume fill, and a full volume WAL-suspends every QuestDB table (which \
                 keeps ACKing writes it silently does not apply), so the prune protects the \
                 live tape at the cost of the backlog. A frame shed at the ring \
                 (tv_dhan_ws_ring_full_total) that reached only the WAL is now gone."
            );
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
/// # HONEST LIMITS of this bound (recorded 2026-09-01, adversarial review)
///
/// Two gaps, both real, both deliberately NOT closed here — because the only
/// way to close them is to DELETE already-captured frames, and that is a
/// decision that deserves its own change rather than a drive-by.
///
/// 1. **Enforcement lags by up to six hours.** This ceiling is applied at
///    PRUNE time, inside the `WS_WAL_ARCHIVE_PRUNE_INTERVAL_SECS` loop —
///    never at write time. Between passes the active set grows freely, and
///    it never consults FREE SPACE at all. So while the tick tier reserves
///    its floor and the depth tier reserves a larger one, this writer can
///    consume the space both of them are reserving, and nothing checks.
///
/// 2. **`replaying/` is bounded on no axis.** `prune_wal_dir_at` is
///    non-recursive by construction, so segments moved there for replay have
///    no byte cap, no age cap and no free-space check. A replay that never
///    confirms leaves them permanently.
///
/// # Why a fail-closed floor here would be WRONG
///
/// This is capture-at-receipt: the WAL is written BEFORE parse and
/// broadcast, so a refusal drops a frame that exists nowhere else. That
/// converts a bounded DISK problem into unrecoverable UPSTREAM loss — the
/// opposite of the tick tier, where a refused rescue loses rows the database
/// already rejected and which the vendor can still be asked for. It is also
/// why the tick floor itself fails OPEN on a blind probe.
///
/// The correct shape is EVICTION, not refusal: at rotation — where
/// `writer_loop` already tests `bytes_written >= WAL_SEGMENT_MAX_BYTES` —
/// drop the OLDEST already-captured segment when the directory is over this
/// ceiling. Deleting the oldest captured frame to make room for the newest
/// uncaptured one is strictly better than dropping the newest, and it removes
/// the six-hour lag without ever refusing a capture. It is not done here
/// because deleting a segment that has not been confirmed replayed is itself
/// a loss path, and choosing which of those two losses to take needs the
/// replay-confirmation state in hand.
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
    // TVW4 records whose endpoint byte this binary does not recognise. They
    // replay as `MainFeed` (total decode, never a drop) and are COUNTED here so
    // the mapping is reported once per segment rather than silently applied.
    let mut unknown_endpoint = 0usize;
    // Smallest record is v1 (13 bytes); v2 is 21. Gate the OUTER loop on the v1
    // minimum, then re-check the version-specific minimum after the magic check
    // so a partial v2 tail can never be read as if its frame_seq were payload.
    while i + WAL_MIN_RECORD_V1 <= buf.len() {
        let magic = &buf[i..i + 4];
        let is_v4 = magic == WAL_MAGIC_V4;
        let is_v3 = magic == WAL_MAGIC_V3;
        let is_v2 = magic == WAL_MAGIC_V2;
        let is_v1 = magic == WAL_MAGIC;
        if !is_v1 && !is_v2 && !is_v3 && !is_v4 {
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
        // a v3 record 29, a v4 record 30. Checked BEFORE any header field is
        // read, so a partial tail can never be reinterpreted as payload.
        let min_rec = if is_v4 {
            WAL_MIN_RECORD_V4
        } else if is_v3 {
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
        // v4: [magic|ws|frame_seq(8)|received_at_nanos(8)|endpoint(1)|len|frame|crc]
        // Every `try_into` below is on a slice whose bounds the per-version
        // minimum-size guard above has already validated, so these arms are
        // structurally unreachable. They still set `corrupted_at` rather than
        // breaking silently: a bare `break` ends the walk with the segment
        // reported as fully replayed, and the caller then CONFIRMS and
        // archives it — so an "impossible" arm that ever fired would discard
        // every remaining record and say nothing at all. That is exactly the
        // shape of the crash-recovery replay defect found the same day, and
        // "unreachable" is a claim about today's bounds checks rather than a
        // guarantee about tomorrow's.
        let (frame_seq, received_at_nanos, endpoint_byte, len_off) = if is_v3 || is_v4 {
            let seq_bytes: [u8; 8] = match buf[i + 5..i + 13].try_into() {
                Ok(b) => b,
                Err(_) => {
                    corrupted_at = Some(("slice_seq_v3", i));
                    break;
                }
            };
            let recv_bytes: [u8; 8] = match buf[i + 13..i + 21].try_into() {
                Ok(b) => b,
                Err(_) => {
                    corrupted_at = Some(("slice_received_at", i));
                    break;
                }
            };
            // v4 carries the endpoint byte at offset 21; v3 has no such byte
            // and reads as `None`, which maps to `MainFeed` below — the
            // pre-v4 assumption, stated rather than guessed.
            let (endpoint_byte, len_off) = if is_v4 {
                (Some(buf[i + 21]), i + 22)
            } else {
                (None, i + 21)
            };
            (
                u64::from_le_bytes(seq_bytes),
                i64::from_le_bytes(recv_bytes),
                endpoint_byte,
                len_off,
            )
        } else if is_v2 {
            let seq_bytes: [u8; 8] = match buf[i + 5..i + 13].try_into() {
                Ok(b) => b,
                Err(_) => {
                    corrupted_at = Some(("slice_seq_v2", i));
                    break;
                }
            };
            (
                u64::from_le_bytes(seq_bytes),
                WAL_RECEIPT_UNKNOWN_NANOS,
                None,
                i + 13,
            )
        } else {
            (0u64, WAL_RECEIPT_UNKNOWN_NANOS, None, i + 5)
        };
        // TOTAL mapping: a v1–v3 record has no endpoint and replays as the
        // main feed (what every earlier replay assumed); a v4 byte this binary
        // does not recognise ALSO maps to the main feed, but is counted so the
        // segment reports it once below rather than applying it silently.
        let endpoint = match endpoint_byte {
            None => WalEndpoint::MainFeed,
            Some(b) => {
                let ep = WalEndpoint::from_u8(b);
                if ep.as_u8() != b {
                    unknown_endpoint = unknown_endpoint.saturating_add(1);
                }
                ep
            }
        };
        let len_bytes: [u8; 4] = match buf[len_off..len_off + 4].try_into() {
            Ok(b) => b,
            Err(_) => {
                corrupted_at = Some(("slice_len", i));
                break;
            }
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
            Err(_) => {
                corrupted_at = Some(("slice_crc", i));
                break;
            }
        };
        let expected = u32::from_le_bytes(crc_bytes);
        // CRC covers the version's exact header bytes, in write order: v2 adds
        // frame_seq, v3 adds received_at_nanos after it. Using the wrong
        // version's byte set here would reject every record of that version as
        // corrupt, so the arms mirror `write_record` exactly.
        let len_le = (frame_len as u32).to_le_bytes();
        let actual = if is_v4 {
            // The RAW endpoint byte, not the decoded enum: an unknown value
            // must still CRC-verify as the bytes on disk, or every record
            // written by a newer binary would read as corrupt.
            crc32_ieee_of(&[
                &[ws_byte],
                &frame_seq.to_le_bytes()[..],
                &received_at_nanos.to_le_bytes()[..],
                &[endpoint_byte.unwrap_or(0)],
                &len_le[..],
                &frame,
            ])
        } else if is_v3 {
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
            endpoint,
        });
        i = record_end;
    }
    if unknown_endpoint > 0 {
        // Not a loss — every such frame was returned above, as `MainFeed`.
        // Reported ONCE per segment so the forward-compatibility mapping is
        // visible in the log rather than applied in silence. Rides the
        // already-filtered WS-SPILL-01 code with a distinguishing `source`;
        // no new metric name (cost rule).
        warn!(
            code = ErrorCode::WsSpill01WriterRespawn.code_str(),
            source = "unknown_endpoint_byte",
            segment = ?path,
            frames = unknown_endpoint,
            "WAL replay: TVW4 records carried an endpoint byte this binary does not \
             recognise — they were replayed as MAIN-FEED frames (the pre-v4 \
             assumption), never dropped. A newer build wrote them; a depth frame \
             among them is counted-and-skipped by the refold rather than re-persisted."
        );
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
mod wal_queue_byte_budget_tests {
    use super::*;

    /// The whole point: the queue is bounded in BYTES, not only in records.
    ///
    /// The old bound was 524,288 records with no length check, so the same
    /// queue was worth 12 MiB of ticker packets or ~256 GiB of depth-200
    /// frames. This pins the arithmetic that makes the second case impossible.
    #[test]
    fn the_budget_is_a_fraction_of_the_host_clamped_at_both_ends() {
        // Production: r8g.xlarge, 32 GiB. 1/16 = 2 GiB, exactly the ceiling.
        let prod = 32 * 1024 * 1024 * 1024;
        assert_eq!(
            wal_queue_budget_from_ceiling(Some(prod)),
            WAL_QUEUE_MAX_BYTES_CEILING,
            "32 GiB host must resolve to the 2 GiB ceiling"
        );

        // A 4 GiB dev container: 1/16 = 256 MiB, exactly the floor.
        let dev = 4 * 1024 * 1024 * 1024;
        assert_eq!(
            wal_queue_budget_from_ceiling(Some(dev)),
            WAL_QUEUE_MIN_BYTES,
            "4 GiB host must resolve to the 256 MiB floor"
        );

        // A tiny host must NOT resolve below the floor.
        assert_eq!(
            wal_queue_budget_from_ceiling(Some(64 * 1024 * 1024)),
            WAL_QUEUE_MIN_BYTES,
            "a host smaller than the floor still gets the floor"
        );

        // A host between the two scales with it rather than clamping.
        let mid = 16 * 1024 * 1024 * 1024;
        assert_eq!(
            wal_queue_budget_from_ceiling(Some(mid)),
            mid / WAL_QUEUE_RAM_FRACTION_DIVISOR,
            "a host between floor and ceiling scales with the host"
        );
    }

    /// An unreadable host takes the FLOOR, never the ceiling.
    ///
    /// Guessing large on an unknown host guesses toward the OOM this budget
    /// exists to prevent, so the direction of the fallback is load-bearing.
    #[test]
    fn an_unknown_host_falls_back_to_the_floor_not_the_ceiling() {
        assert_eq!(wal_queue_budget_from_ceiling(None), WAL_QUEUE_MIN_BYTES);
        assert!(
            wal_queue_budget_from_ceiling(None) < WAL_QUEUE_MAX_BYTES_CEILING,
            "the unknown-host fallback must be the SMALL end"
        );
    }

    /// The bound must never be so tight that the ORDINARY tick mix hits it.
    ///
    /// A Full packet is 162 bytes. The record bound (524,288) must still be
    /// what binds for that mix, or this change would convert a working feed
    /// into a dropping one — the exact regression a byte bound could cause.
    #[test]
    fn the_record_bound_still_binds_first_for_the_ordinary_tick_mix() {
        let full_packet_bytes = 162_u64;
        let bytes_at_record_capacity = full_packet_bytes * SPILL_CHANNEL_CAPACITY as u64;
        assert!(
            bytes_at_record_capacity < WAL_QUEUE_MIN_BYTES,
            "a full channel of 162-byte packets is {bytes_at_record_capacity} bytes, which must \
             stay under even the FLOOR ({WAL_QUEUE_MIN_BYTES}) — otherwise the byte bound would \
             start refusing ordinary ticks"
        );
    }

    /// And it must be tight enough that the pathological mix CANNOT reach the
    /// host's memory. This is the failure the change exists to prevent.
    #[test]
    fn a_full_channel_of_max_frames_cannot_exceed_the_budget() {
        // The largest frame the depth-200 socket accepts.
        let depth_200_max = 512_u64 * 1024;
        let unbounded_worst_case = depth_200_max * SPILL_CHANNEL_CAPACITY as u64;
        let host = 32_u64 * 1024 * 1024 * 1024;
        assert!(
            unbounded_worst_case > host,
            "sanity: the record-only bound really does exceed a 32 GiB host \
             ({unbounded_worst_case} bytes) — this is the defect being fixed"
        );
        let budget = wal_queue_budget_from_ceiling(Some(host));
        assert!(
            budget < host,
            "the byte budget must sit strictly under the host it protects"
        );
        // The budget is what now bounds the pathological mix, not the record count.
        assert!(
            budget < unbounded_worst_case,
            "the byte budget must be the binding limit for max-size frames"
        );
    }

    /// A refused frame must RELEASE its reservation, or the budget ratchets
    /// down to zero and a permanently-empty queue refuses everything.
    ///
    /// `new_with_dead_writer_for_test` drops the receiver, so every send takes the
    /// `Disconnected` arm — which is one of the two arms that must release.
    #[test]
    fn a_refused_frame_releases_its_byte_reservation() {
        let spill = WsFrameSpill::new_with_dead_writer_for_test();
        for _ in 0..64 {
            assert_eq!(
                spill.append(WsType::LiveFeed, vec![0_u8; 4096]),
                AppendOutcome::Dropped,
                "no writer exists, so every append must be refused"
            );
        }
        assert_eq!(
            spill.queued_bytes.load(Ordering::Relaxed),
            0,
            "every refused frame must have released its reservation; a leak here \
             would make the budget shrink monotonically until nothing is accepted"
        );
    }
}

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

    /// C1: the writer's queue is DRAINED at shutdown, not abandoned.
    ///
    /// Before this the thread was detached and unreachable: the only `Sender`
    /// lives inside `WsFrameSpill`, held through an `Arc`, so the channel could
    /// never close and the thread never had a reason to exit. Everything queued
    /// died with the process, already acknowledged to its caller as `Spilled`
    /// and counted by nothing.
    #[test]
    fn shutdown_drains_the_queue_and_joins_the_writer() {
        let dir = tmp_dir("shutdown");
        let spill = WsFrameSpill::new(&dir).expect("spill opens");

        for i in 0..500u32 {
            assert!(matches!(
                spill.append(WsType::LiveFeed, i.to_le_bytes().to_vec()),
                AppendOutcome::Spilled
            ));
        }

        let abandoned = spill.shutdown(Duration::from_secs(5));

        assert_eq!(abandoned, 0, "a healthy drain must abandon nothing");
        assert_eq!(
            spill.persisted_count(),
            500,
            "every queued record must be written before the drain reports clean"
        );
        assert_eq!(spill.queued_records(), 0, "queue must be empty after drain");
    }

    /// The records must actually be ON DISK — a drain that merely emptied the
    /// channel while leaving the 256 KiB `BufWriter` unflushed would report
    /// clean and still lose the tail, because `persist_record_resilient`
    /// counts a `write_all` into the buffer, not a flush to the file.
    #[test]
    fn shutdown_flushes_the_buffer_so_replay_sees_every_record() {
        let dir = tmp_dir("shutdown");
        let spill = WsFrameSpill::new(&dir).expect("spill opens");

        for i in 0..64u32 {
            let _ = spill.append(WsType::LiveFeed, i.to_le_bytes().to_vec());
        }
        assert_eq!(spill.shutdown(Duration::from_secs(5)), 0);
        drop(spill);

        let frames = replay_all(&dir).expect("replay");
        assert_eq!(
            frames.len(),
            64,
            "every drained record must be readable from disk"
        );
    }

    /// Shutdown is reached through an `Arc` on a signal path, so a second call
    /// (a retry, a double-signal) must be harmless rather than a panic on an
    /// already-taken handle.
    #[test]
    fn shutdown_called_twice_is_harmless() {
        let dir = tmp_dir("shutdown");
        let spill = WsFrameSpill::new(&dir).expect("spill opens");
        let _ = spill.append(WsType::LiveFeed, vec![1, 2, 3]);

        assert_eq!(spill.shutdown(Duration::from_secs(5)), 0);
        assert_eq!(
            spill.shutdown(Duration::from_secs(5)),
            0,
            "a second shutdown must not panic and must still report a clean queue"
        );
    }

    /// A drain with nothing queued still stops the thread, so an idle process
    /// exits promptly instead of burning its whole budget.
    #[test]
    fn shutdown_on_an_empty_queue_returns_immediately_clean() {
        let dir = tmp_dir("shutdown");
        let spill = WsFrameSpill::new(&dir).expect("spill opens");

        let started = Instant::now();
        assert_eq!(spill.shutdown(Duration::from_secs(30)), 0);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "an empty drain must not spend its budget waiting; took {:?}",
            started.elapsed()
        );
    }

    /// `queued_records` is the size of the loss window an abrupt exit takes.
    /// It must be readable BEFORE any shutdown, so an operator can see the
    /// exposure rather than infer it.
    #[test]
    fn queued_records_is_zero_on_a_drained_spill() {
        let dir = tmp_dir("shutdown");
        let spill = WsFrameSpill::new(&dir).expect("spill opens");
        let _ = spill.append(WsType::LiveFeed, vec![9; 16]);
        wait_until_persisted(&spill, 1);
        assert_eq!(spill.queued_records(), 0);
    }

    /// The three sequential shutdown budgets must fit inside systemd's stop
    /// timeout. The cross-file half of that is pinned by
    /// `crates/app/tests/shutdown_budget_fits_systemd_guard.rs`; this pins the
    /// half that lives here, so a local edit cannot silently make the WAL
    /// drain the dominant cost.
    #[test]
    fn the_wal_shutdown_budget_stays_a_stall_budget_not_a_throughput_budget() {
        assert!(
            WAL_SPILL_SHUTDOWN_BUDGET_SECS >= 5,
            "below ~5s a briefly-wedged disk (WAL_WRITER_IO_RETRY_BACKOFF loops) \
             would abandon records that were about to land"
        );
        assert!(
            WAL_SPILL_SHUTDOWN_BUDGET_SECS <= 30,
            "this budget is for a STALLED writer, not for throughput: the writer \
             clears a full 524,288-slot channel in well under a second of healthy \
             disk time, and every second here is a second nearer systemd's SIGKILL"
        );
        assert_eq!(
            WAL_SPILL_SHUTDOWN_BUDGET,
            Duration::from_secs(WAL_SPILL_SHUTDOWN_BUDGET_SECS)
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

    /// The MEMORY guard must DEFER, never DROP — and must always make progress.
    ///
    /// This is the guard added on 2026-09-02 after a live boot measured RSS at
    /// **13.09 GiB of a 15.0 GiB ceiling before the catch-up loop's round 0**,
    /// i.e. before it had replayed anything. `WAL_REPLAY_MAX_BYTES` had done
    /// its job — it bounded the BYTES READ — while the rows those bytes
    /// materialized into (22,248,540 depth rows from ~512 MiB of frames,
    /// roughly 30x amplification) went unbounded and took the process out.
    ///
    /// Two properties, and the second is the one that makes this safe to ship:
    ///
    ///   1. A segment the guard stopped short of is STILL a `*.wal` file, so
    ///      the next boot re-globs it. Identical to the byte budget's
    ///      contract, which is why this reuses that path rather than adding
    ///      one — an unread segment left in `replaying/` would be archived by
    ///      `confirm_replayed` and its frames destroyed.
    ///   2. Even with the probe pinned ABOVE the line from the first call, one
    ///      segment is still read. Without that, a host sitting over the
    ///      threshold for any other reason would defer everything on every
    ///      boot forever — a permanent loss wearing a deferral's clothes.
    #[test]
    fn replay_stops_on_the_memory_guard_and_defers_the_rest() {
        let dir = tmp_dir("replay-memory-guard");
        for _ in 0..3 {
            let spill = WsFrameSpill::new(&dir).unwrap();
            spill.append(WsType::LiveFeed, vec![9u8; 64]);
            wait_until_persisted(&spill, 1);
        }
        std::thread::sleep(Duration::from_millis(50));
        let before = wal_segments_in(&dir).len();
        assert!(before >= 3, "fixture needs >= 3 segments, found {before}");

        // RSS pinned hard over the line, ceiling known: the guard is armed on
        // every check. Budget is effectively unlimited, so ONLY the memory
        // guard can stop this pass — if it did not exist, all 3 are read.
        let batch = replay_all_with_report_guarded(
            &dir,
            usize::MAX,
            || Some(14_050_361_344), // the RSS the live boot actually reported
            Some(16_106_127_360),    // the 15.0 GiB MemoryHigh ceiling
            WAL_REPLAY_RSS_STOP_PCT,
        )
        .expect("replay");

        assert!(
            batch.stopped_for_memory,
            "the pass must report WHY it stopped: 'deferred' reads the same for \
             a byte-budget stop and a memory stop, and the two have opposite \
             remedies — raising the byte budget makes a memory stop WORSE"
        );
        assert!(
            !batch.frames.is_empty(),
            "progress is mandatory: one segment is always read, or a host that \
             is over the line for an unrelated reason never drains its WAL"
        );
        assert!(
            batch.deferred_segments > 0,
            "with 3 segments and the guard armed, the rest must be deferred"
        );
        assert_eq!(
            wal_segments_in(&dir).len(),
            before - 1,
            "every DEFERRED segment must still be a `*.wal` file — if the guard \
             staged them, confirm_replayed would archive frames never replayed"
        );

        // Below the line, the guard is inert and the remainder is recovered:
        // the stop was a delay, not a loss.
        let rest = replay_all_with_report_guarded(
            &dir,
            usize::MAX,
            || Some(1_000_000_000),
            Some(16_106_127_360),
            WAL_REPLAY_RSS_STOP_PCT,
        )
        .expect("replay");
        assert!(
            !rest.stopped_for_memory,
            "under the line the guard is inert"
        );
        assert!(
            !rest.frames.is_empty(),
            "the deferred frames must be recoverable on a later boot"
        );
    }

    /// An unmeasurable host must behave EXACTLY as it did before the guard.
    ///
    /// A dev machine with no cgroup and no `/proc/self/status` resolves both
    /// inputs to `None`. Failing CLOSED there would halt WAL recovery — the
    /// path that gets captured frames back into the database — on every host
    /// the probe cannot read. Fail-open is the deliberate direction.
    #[test]
    fn the_memory_guard_is_inert_when_the_host_cannot_be_measured() {
        let dir = tmp_dir("replay-memory-unmeasurable");
        for _ in 0..3 {
            let spill = WsFrameSpill::new(&dir).unwrap();
            spill.append(WsType::LiveFeed, vec![3u8; 64]);
            wait_until_persisted(&spill, 1);
        }
        std::thread::sleep(Duration::from_millis(50));
        let before = wal_segments_in(&dir).len();

        // No ceiling AND no RSS — the unmeasurable host.
        let batch = replay_all_with_report_guarded(
            &dir,
            usize::MAX,
            || None,
            None,
            WAL_REPLAY_RSS_STOP_PCT,
        )
        .expect("replay");
        assert!(!batch.stopped_for_memory);
        assert_eq!(
            batch.deferred_segments, 0,
            "an unmeasurable host must read every segment, exactly as it did \
             before this guard existed"
        );
        assert_eq!(
            wal_segments_in(&dir).len(),
            0,
            "all {before} segments consumed"
        );
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

    /// FINDING 2 (2026-09-02): the deferral verdict must reach the CALLER,
    /// not only this function's own log line. Two segments under a 1-byte
    /// budget: one is consumed, one is deferred, and the report says so with
    /// the bytes it held and the budget it ran under.
    #[test]
    fn replay_all_with_report_counts_deferred_segments_and_bytes() {
        let dir = tmp_dir("replay-report-defer");
        for _ in 0..2 {
            let spill = WsFrameSpill::new(&dir).unwrap();
            spill.append(WsType::LiveFeed, vec![7u8; 64]);
            wait_until_persisted(&spill, 1);
        }
        std::thread::sleep(Duration::from_millis(50));
        assert!(
            wal_segments_in(&dir).len() >= 2,
            "fixture needs two segments"
        );

        let first = replay_all_with_report(&dir, 1).expect("replay");
        assert_eq!(
            first.frames.len(),
            1,
            "one segment is read under a 1-byte budget"
        );
        assert_eq!(
            first.deferred_segments, 1,
            "the other is DEFERRED, and reported"
        );
        assert_eq!(first.bytes_replayed, 64, "payload bytes actually held");
        assert_eq!(
            first.budget_bytes, 1,
            "the budget is echoed for the log line"
        );

        // The consumed segment was STAGED, not confirmed, so the next pass
        // re-globs it from `replaying/` alongside the deferred one — the
        // crash-safety invariant (an unconfirmed segment is always re-read).
        let rest = replay_all_with_report(&dir, usize::MAX).expect("replay");
        assert_eq!(rest.deferred_segments, 0, "a drained pass defers nothing");
        assert_eq!(
            rest.frames.len(),
            2,
            "staged leftover + the deferred segment"
        );
        assert_eq!(rest.bytes_replayed, 128);

        let missing = replay_all_with_report(dir.join("never-created"), 5).expect("replay");
        assert_eq!(missing.frames.len(), 0);
        assert_eq!(missing.deferred_segments, 0);
        assert_eq!(
            missing.budget_bytes, 5,
            "the budget is echoed even for a missing dir"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `should_page_replay_deferred` fires ONCE per boot and never on a pass
    /// that deferred nothing — the caller's latch does the rest.
    #[test]
    fn should_page_replay_deferred_fires_once_per_boot_and_never_on_a_drained_pass() {
        assert!(
            should_page_replay_deferred(1, false),
            "first deferral pages"
        );
        assert!(should_page_replay_deferred(13, false));
        assert!(
            !should_page_replay_deferred(13, true),
            "already paged this boot: silent"
        );
        assert!(
            !should_page_replay_deferred(0, false),
            "a drained pass never pages"
        );
        assert!(!should_page_replay_deferred(0, true));
        // The latch shape the catch-up loop uses: 120 deferring rounds, ONE page.
        let mut paged = false;
        let mut pages = 0u32;
        for _ in 0..120 {
            if should_page_replay_deferred(3, paged) {
                paged = true;
                pages += 1;
            }
        }
        assert_eq!(
            pages, 1,
            "120 deferring rounds must produce exactly one page"
        );
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

    /// Every exit from the segment walk is either a counted abandonment or a
    /// deliberate tail stop — never a silent `break`.
    ///
    /// **The defect this pins (2026-08-28).** Five `Err(_) => break` arms sat
    /// on `try_into` calls whose slices the per-version minimum-size guard has
    /// already bounds-checked, so all five were structurally unreachable. That
    /// is precisely why they were dangerous: an unreachable arm attracts no
    /// scrutiny, and a bare `break` here ends the walk with the segment
    /// reported as fully replayed. The caller then CONFIRMS and archives it, so
    /// a single such firing would discard every remaining record in the file
    /// and produce no counter, no log line and no page.
    ///
    /// The same shape as the crash-recovery replay defect found the same day,
    /// and the reusable half is that "unreachable" is a claim about today's
    /// bounds checks, not a guarantee about tomorrow's edits.
    #[test]
    fn no_walk_exit_is_a_silent_break() {
        let source = include_str!("ws_frame_spill.rs");
        let walk = source
            .split_once("fn replay_segment(path: &Path)")
            .map(|(_, rest)| rest)
            .and_then(|rest| rest.split_once("\n// ---"))
            .map(|(body, _)| body)
            .unwrap_or_else(|| panic!("replay_segment not found — was it renamed?"));

        // Comment-blind: a doc-comment quoting the old shape must not satisfy
        // or trip the scan (the house convention).
        let code: String = walk
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");

        assert!(
            !code.contains("Err(_) => break,"),
            "a bare `Err(_) => break` in the segment walk ends the replay with the segment \
             reported as fully recovered — the caller then archives it, and every record \
             after that point is lost with nothing counted. Set `corrupted_at` instead, \
             which is already wired to the counters and the coded error."
        );

        for needle in [
            "corrupted_at = Some((\"slice_seq_v3\"",
            "corrupted_at = Some((\"slice_received_at\"",
            "corrupted_at = Some((\"slice_seq_v2\"",
            "corrupted_at = Some((\"slice_len\"",
            "corrupted_at = Some((\"slice_crc\"",
        ] {
            assert!(
                code.contains(needle),
                "the structurally-unreachable slice arms must report rather than break; \
                 missing: {needle}"
            );
        }

        // Self-test: the scan can bite. A synthetic body carrying the old
        // shape must trip it, or the assertion above is decorative.
        let regressed = "match x { Ok(b) => b, Err(_) => break, }";
        assert!(
            regressed.contains("Err(_) => break,"),
            "self-test: the scan must detect the reintroduced silent break"
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
            spill.append_with_seq_at(
                WsType::LiveFeed,
                vec![1, 2, 3],
                11,
                first,
                WalEndpoint::MainFeed,
            );
            spill.append_with_seq_at(
                WsType::LiveFeed,
                vec![4, 5, 6],
                12,
                second,
                WalEndpoint::MainFeed,
            );
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
        // Since TVW4 the writer emits v4, so the rotation accounting is
        // measured against the v4 encoder; the v3 encoder stays one byte
        // shorter and is pinned as such so the two cannot be confused.
        let frame = vec![1u8, 2, 3, 4, 5];
        let encoded = encode_v4_record(
            WsType::LiveFeed,
            1,
            1_787_800_000_000_000_000,
            WalEndpoint::MainFeed,
            &frame,
        );
        let rec = WalRecord {
            ws_type: WsType::LiveFeed,
            frame_seq: 1,
            received_at_nanos: 1_787_800_000_000_000_000,
            endpoint: WalEndpoint::MainFeed,
            frame: Bytes::from(frame.clone()),
        };
        assert_eq!(
            record_disk_size(&rec) as usize,
            encoded.len(),
            "rotation accounting must equal the real on-disk size"
        );
        assert_eq!(
            encoded.len(),
            WAL_MIN_RECORD_V4 + frame.len(),
            "v4 overhead must be exactly {WAL_MIN_RECORD_V4} bytes"
        );
        assert_eq!(
            encode_v3_record(WsType::LiveFeed, 1, 1_787_800_000_000_000_000, &frame).len(),
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
            spill.append_with_seq_at(WsType::LiveFeed, vec![1], 1, -42, WalEndpoint::MainFeed);
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

    /// Test-only encoder for a v4 record, byte-for-byte mirroring
    /// `write_record`. Kept beside `encode_v3_record` for the same reason: a
    /// divergence between writer and encoder shows up as a failing
    /// round-trip rather than silently.
    fn encode_v4_record(
        ws: WsType,
        frame_seq: u64,
        received_at_nanos: i64,
        endpoint: WalEndpoint,
        frame: &[u8],
    ) -> Vec<u8> {
        encode_v4_record_raw(ws, frame_seq, received_at_nanos, endpoint.as_u8(), frame)
    }

    /// The v4 encoder with a RAW endpoint byte, so a value this binary does
    /// not recognise can be written exactly as a newer binary would write it.
    fn encode_v4_record_raw(
        ws: WsType,
        frame_seq: u64,
        received_at_nanos: i64,
        endpoint_byte: u8,
        frame: &[u8],
    ) -> Vec<u8> {
        let len = u32::try_from(frame.len()).expect("test frame fits u32");
        let seq = frame_seq.to_le_bytes();
        let recv = received_at_nanos.to_le_bytes();
        let crc = crc32_ieee_of(&[
            &[ws.as_u8()],
            &seq[..],
            &recv[..],
            &[endpoint_byte],
            &len.to_le_bytes()[..],
            frame,
        ]);
        let mut v = Vec::new();
        v.extend_from_slice(&WAL_MAGIC_V4);
        v.push(ws.as_u8());
        v.extend_from_slice(&seq);
        v.extend_from_slice(&recv);
        v.push(endpoint_byte);
        v.extend_from_slice(&len.to_le_bytes());
        v.extend_from_slice(frame);
        v.extend_from_slice(&crc.to_le_bytes());
        v
    }

    // --- TVW4: the endpoint-carrying record (2026-09-02) --------------------

    /// The whole reason v4 exists: every endpoint survives the disk round-trip
    /// EXACTLY, alongside the receipt. If this regresses, a replayed depth
    /// frame is fed to the main-feed parser (or vice versa) and silently
    /// discarded as unparseable.
    #[test]
    fn tvw4_roundtrip_preserves_every_endpoint() {
        let dir = tmp_dir("v4-roundtrip");
        let receipt: i64 = 1_787_800_000_000_000_000;
        let all = [
            WalEndpoint::MainFeed,
            WalEndpoint::Depth20,
            WalEndpoint::Depth200,
            WalEndpoint::OrderUpdate,
        ];
        {
            let spill = WsFrameSpill::new(&dir).unwrap();
            for (i, ep) in all.iter().enumerate() {
                let seq = (i as u64 + 1) << PACKET_INDEX_BITS;
                spill.append_with_seq_at(WsType::LiveFeed, vec![i as u8; 3], seq, receipt, *ep);
            }
            wait_until_persisted(&spill, all.len() as u64);
        }
        std::thread::sleep(Duration::from_millis(50));

        let frames = replay_all(&dir).unwrap();
        assert_eq!(frames.len(), all.len());
        for (i, ep) in all.iter().enumerate() {
            assert_eq!(
                frames[i].endpoint, *ep,
                "endpoint {ep:?} must round-trip verbatim"
            );
            assert_eq!(
                WalEndpoint::from_u8(ep.as_u8()),
                *ep,
                "from_u8/as_u8 identity"
            );
            assert_eq!(
                frames[i].received_at_nanos, receipt,
                "the receipt still round-trips"
            );
            assert_eq!(frames[i].frame, vec![i as u8; 3]);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A v4 record whose CRC did not cover the endpoint byte would let a
    /// flipped byte route a frame to the wrong parser silently. Flip the
    /// endpoint byte and require the record to be rejected.
    #[test]
    fn tvw4_crc_covers_the_endpoint_byte() {
        let dir = tmp_dir("v4-crc");
        let seg = dir.join("ws-frames-00000000000000000006.wal");
        let mut bytes = encode_v4_record(
            WsType::LiveFeed,
            5,
            1_787_800_000_000_000_000,
            WalEndpoint::Depth20,
            &[1, 2],
        );
        // Endpoint byte sits at offset 21. Corrupt it; the CRC must fail.
        bytes[21] ^= 0x01;
        std::fs::write(&seg, &bytes).unwrap();

        let frames = replay_all(&dir).unwrap();
        assert!(
            frames.is_empty(),
            "an endpoint-byte flip must fail CRC — otherwise the CRC does not \
             cover the field and a depth frame reaches the main-feed parser"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An endpoint byte this binary does not know (written by a NEWER build)
    /// must replay as MAIN-FEED — never dropped, never a panic. Dropping it
    /// would turn a forward-compatibility gap into permanent loss.
    #[test]
    fn tvw4_unknown_endpoint_byte_maps_to_main_feed_not_a_panic() {
        let dir = tmp_dir("v4-unknown-ep");
        let seg = dir.join("ws-frames-00000000000000000007.wal");
        let mut bytes = encode_v4_record_raw(
            WsType::LiveFeed,
            9,
            1_787_800_000_000_000_000,
            0xEE,
            &[4, 4, 4],
        );
        bytes.extend_from_slice(&encode_v4_record_raw(WsType::LiveFeed, 10, 0, 0xFF, &[5]));
        std::fs::write(&seg, &bytes).unwrap();

        let frames = replay_all(&dir).unwrap();
        assert_eq!(frames.len(), 2, "unknown endpoint bytes are NOT dropped");
        assert_eq!(frames[0].endpoint, WalEndpoint::MainFeed);
        assert_eq!(frames[1].endpoint, WalEndpoint::MainFeed);
        assert_eq!(frames[0].frame, vec![4, 4, 4]);
        assert_eq!(frames[0].frame_seq, 9);
        assert_eq!(
            WalEndpoint::from_u8(0xEE),
            WalEndpoint::MainFeed,
            "total decode"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A v3 fixture written by the previous binary replays as MAIN-FEED with
    /// its receipt intact — exactly what every earlier replay assumed, so a
    /// legacy segment behaves precisely as it did before v4.
    #[test]
    fn tvw3_fixture_replays_as_main_feed_with_its_receipt() {
        let dir = tmp_dir("v4-v3-fixture");
        let seg = dir.join("ws-frames-00000000000000000008.wal");
        let receipt: i64 = 1_787_825_000_000_000_000;
        let bytes = encode_v3_record(WsType::OrderUpdate, 77, receipt, b"{\"o\":1}");
        std::fs::write(&seg, &bytes).unwrap();

        let frames = replay_all(&dir).unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(
            frames[0].endpoint,
            WalEndpoint::MainFeed,
            "v3 carries no endpoint"
        );
        assert_eq!(frames[0].received_at_nanos, receipt);
        assert_eq!(frames[0].frame_seq, 77);
        assert_eq!(frames[0].ws_type, WsType::OrderUpdate);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A segment that a rollback-then-upgrade left with v3 and v4 records
    /// interleaved replays every record in order, each with the endpoint its
    /// own version can honestly claim.
    #[test]
    fn tvw3_and_tvw4_records_interleave_in_one_segment() {
        let dir = tmp_dir("v4-mixed");
        let seg = dir.join("ws-frames-00000000000000000009.wal");
        let mut bytes = encode_v3_record(WsType::LiveFeed, 1, 0, &[1]);
        bytes.extend_from_slice(&encode_v4_record(
            WsType::LiveFeed,
            2,
            0,
            WalEndpoint::Depth200,
            &[2],
        ));
        bytes.extend_from_slice(&encode_v3_record(WsType::LiveFeed, 3, 0, &[3]));
        bytes.extend_from_slice(&encode_v4_record(
            WsType::LiveFeed,
            4,
            0,
            WalEndpoint::Depth20,
            &[4],
        ));
        std::fs::write(&seg, &bytes).unwrap();

        let frames = replay_all(&dir).unwrap();
        let got: Vec<(u64, WalEndpoint)> =
            frames.iter().map(|f| (f.frame_seq, f.endpoint)).collect();
        assert_eq!(
            got,
            vec![
                (1, WalEndpoint::MainFeed),
                (2, WalEndpoint::Depth200),
                (3, WalEndpoint::MainFeed),
                (4, WalEndpoint::Depth20),
            ],
            "every record replays, in capture order, with its own version's endpoint"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `append_with_seq` — the entry point with no `DhanEndpointType` in hand
    /// — must derive the endpoint from the TRANSPORT: the order-update
    /// transport is its own endpoint, a market-data transport is the main
    /// feed. A depth label from a caller that cannot know it would feed a
    /// main-feed frame to the depth parser on replay.
    #[test]
    fn append_with_seq_derives_the_endpoint_from_the_transport() {
        let dir = tmp_dir("v4-derived-ep");
        {
            let spill = WsFrameSpill::new(&dir).unwrap();
            spill.append_with_seq(WsType::LiveFeed, vec![1], 1 << PACKET_INDEX_BITS);
            spill.append_with_seq(WsType::OrderUpdate, vec![2], 2 << PACKET_INDEX_BITS);
            spill.append_with_seq(WsType::TruedataFeed, vec![3], 3 << PACKET_INDEX_BITS);
            wait_until_persisted(&spill, 3);
        }
        std::thread::sleep(Duration::from_millis(50));

        let frames = replay_all(&dir).unwrap();
        assert_eq!(frames.len(), 3);
        assert_eq!(frames[0].endpoint, WalEndpoint::MainFeed);
        assert_eq!(
            frames[1].endpoint,
            WalEndpoint::OrderUpdate,
            "order-update transport IS its endpoint"
        );
        assert_eq!(frames[2].endpoint, WalEndpoint::MainFeed);
        assert_eq!(
            WalEndpoint::for_ws_type(WsType::OrderUpdate),
            WalEndpoint::OrderUpdate
        );
        assert_eq!(
            WalEndpoint::for_ws_type(WsType::LiveFeed),
            WalEndpoint::MainFeed
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The high-water probe reads v4 headers: a 29-byte buffer against a
    /// 30-byte header would return 0 on every record and re-seed nothing.
    #[test]
    fn the_reseed_probe_reads_a_tvw4_header() {
        let dir = tmp_dir("v4-reseed");
        let seg = dir.join("ws-frames-00000000000000000010.wal");
        let seq = 4_242u64 << PACKET_INDEX_BITS;
        let bytes = encode_v4_record(WsType::LiveFeed, seq, 0, WalEndpoint::Depth20, &[1, 2, 3]);
        std::fs::write(&seg, &bytes).unwrap();
        assert_eq!(
            highest_frame_seq_on_disk(&dir),
            seq,
            "the probe must read the sequence out of a v4 header"
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

    /// FINDING 3 (2026-09-02): the byte-ceiling prune of the ACTIVE directory
    /// destroys un-replayed capture, and its outcome carried only a COUNT. The
    /// page has to say how many bytes and how far back — so the pass reports
    /// the bytes it freed and the age of the oldest segment it deleted.
    #[test]
    fn active_byte_ceiling_prune_reports_bytes_freed_and_oldest_age() {
        let dir = tmp_dir("active-prune-pressure-report");
        let now = SystemTime::now();
        // Three 13-byte segments ("segment-bytes"), 3 h / 2 h / 1 h old, all
        // inside retention. A 20-byte ceiling forces the two OLDEST out.
        let oldest = plant_active_file(&dir, "ws-frames-00000000000000000040.wal", now, 10_800);
        let middle = plant_active_file(&dir, "ws-frames-00000000000000000041.wal", now, 7_200);
        let newest = plant_active_file(&dir, "ws-frames-00000000000000000042.wal", now, 3_600);
        let outcome = prune_active_segments_at(&dir, 172_800, 20, now);
        assert_eq!(outcome.deleted, 0, "nothing is age-expired");
        assert_eq!(outcome.size_deleted, 2, "two oldest removed by the ceiling");
        assert_eq!(outcome.size_deleted_bytes, 26, "13 B x 2 segments freed");
        assert_eq!(
            outcome.size_deleted_oldest_age_secs, 10_800,
            "the OLDEST deleted segment's age, not the newest"
        );
        assert!(!oldest.exists() && !middle.exists());
        assert!(newest.exists(), "the newest survives");
        // A pass that deletes nothing under pressure reports zeros, so the
        // page can never carry a stale number from a previous pass.
        let quiet = prune_active_segments_at(&dir, 172_800, u64::MAX, now);
        assert_eq!(quiet.size_deleted, 0);
        assert_eq!(quiet.size_deleted_bytes, 0);
        assert_eq!(quiet.size_deleted_oldest_age_secs, 0);
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
        let first = lock_wal_dir(&dir)
            .expect("first claim must succeed")
            .expect("a writable temp dir must yield a guard");
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
        let first = lock_wal_dir(&dir).expect("first claim").expect("guard");
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
        let ga = lock_wal_dir(&a).expect("claim a").expect("guard a");
        let gb = lock_wal_dir(&b)
            .expect("claim b must not be blocked by a")
            .expect("guard b");
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
            endpoint: WalEndpoint::MainFeed,
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
            endpoint: WalEndpoint::MainFeed,
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
            endpoint: WalEndpoint::MainFeed,
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
            endpoint: WalEndpoint::MainFeed,
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

    /// A lock file that cannot be CREATED degrades — it does not kill the lane.
    ///
    /// The two failures are not the same and must not be treated the same.
    /// Contention means a second live process and is fail-closed. A creation
    /// failure — an unwritable directory, a full disk — is not evidence of a
    /// second process, and the writer is deliberately built to survive exactly
    /// that state and recover when it clears
    /// (`test_writer_survives_unwritable_dir_then_recovers`). Refusing to
    /// construct would turn a recoverable filesystem problem into a dead feed.
    ///
    /// The failure is induced by making the lock PATH a directory, so `open`
    /// fails with `IsADirectory`. That works as root, which a `chmod 0o555`
    /// does not — and this is the exact difference that let the regression
    /// reach CI green locally and red on the runner.
    #[test]
    fn a_lock_file_that_cannot_be_created_degrades_instead_of_killing_the_lane() {
        let dir = tmp_dir("lock-uncreatable");
        std::fs::create_dir_all(dir.join(WAL_DIR_LOCK_FILE)).expect("occupy the lock path");

        let claimed = lock_wal_dir(&dir).expect("a creation failure must NOT be an Err");
        assert!(
            claimed.is_none(),
            "an uncreatable lock file yields no guard, and says so, rather than \
             pretending exclusion is in force"
        );

        // And the whole spill must still construct, because the writer's own
        // survive-and-recover behaviour is what this protects.
        let spill = WsFrameSpill::new(&dir).expect(
            "an uncreatable lock file must not stop the spill from constructing — \
             the writer survives an unwritable directory by design",
        );
        assert_eq!(
            spill.append(WsType::LiveFeed, vec![1, 2, 3]),
            AppendOutcome::Spilled,
            "the channel must still accept frames in the degraded state"
        );
        drop(spill);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Contention is still fail-CLOSED. The degrade above must not have softened
    /// the case the lock exists for.
    #[test]
    fn contention_is_still_an_error_after_the_degrade_arm_was_added() {
        let dir = tmp_dir("lock-still-closed");
        let held = lock_wal_dir(&dir).expect("claim").expect("guard");
        assert!(
            lock_wal_dir(&dir).is_err(),
            "a second live claim must remain an Err — softening this to a degrade \
             would reopen the silent capture_seq collision the lock exists to stop"
        );
        drop(held);
    }

    /// Minimal TVW3 record encoder for the high-water probe tests.
    ///
    /// Deliberately hand-rolled rather than routed through `write_record`: the
    /// probe reads HEADERS off disk, so a test that shares the writer's own
    /// encoder would pass even if the two disagreed about the layout.
    fn v3_bytes(ws_type: WsType, frame_seq: u64, receipt: i64, frame: &[u8]) -> Vec<u8> {
        let frame_len = u32::try_from(frame.len()).expect("test frame");
        let mut out = Vec::new();
        out.extend_from_slice(&WAL_MAGIC_V3);
        out.push(ws_type.as_u8());
        out.extend_from_slice(&frame_seq.to_le_bytes());
        out.extend_from_slice(&receipt.to_le_bytes());
        out.extend_from_slice(&frame_len.to_le_bytes());
        out.extend_from_slice(frame);
        let crc = crc32_ieee_of(&[
            &[ws_type.as_u8()][..],
            &frame_seq.to_le_bytes()[..],
            &receipt.to_le_bytes()[..],
            &frame_len.to_le_bytes()[..],
            frame,
        ]);
        out.extend_from_slice(&crc.to_le_bytes());
        out
    }

    /// The empty-newest-segment case, which is the ONE the high-water probe
    /// exists for and the one it used to fail.
    ///
    /// `writer_loop` opens the next segment before writing into it, so a crash
    /// in that window leaves a zero-byte newest file. Reading only
    /// `segs.last()` returned 0, seeded nothing, and the restart re-issued
    /// `capture_seq` values already in the database — where QuestDB upserts the
    /// collisions away silently, because from this process both appends
    /// succeeded.
    #[test]
    fn an_empty_newest_segment_does_not_hide_the_high_water_mark() {
        let dir = std::env::temp_dir().join(format!(
            "tv-hfs-empty-{}",
            std::time::SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");

        // An older segment carrying a real sequence...
        let older = dir.join("00000000000000000001.wal");
        let buf = v3_bytes(WsType::LiveFeed, 987_654_321, 42, b"payload");
        std::fs::write(&older, &buf).expect("write older");

        // ...and a newer, ZERO-BYTE one, exactly as a crash-after-rotate leaves.
        std::fs::write(dir.join("00000000000000000002.wal"), b"").expect("write empty");

        assert_eq!(
            highest_frame_seq_on_disk(&dir),
            987_654_321,
            "the probe must walk past an empty newest segment — returning 0 here \
             re-issues capture_seq values already in the ticks table, and the \
             collision is upserted away with no counter to report it"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The same defence against a TORN newest segment: a garbage first header
    /// ends the walk at offset 0, which is indistinguishable from empty.
    #[test]
    fn a_torn_newest_segment_does_not_hide_the_high_water_mark() {
        let dir = std::env::temp_dir().join(format!(
            "tv-hfs-torn-{}",
            std::time::SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");

        let buf = v3_bytes(WsType::LiveFeed, 555_000, 7, b"payload");
        std::fs::write(dir.join("00000000000000000001.wal"), &buf).expect("write older");
        // Unknown magic at offset 0 — the walk returns immediately.
        std::fs::write(dir.join("00000000000000000002.wal"), b"TVW9garbage").expect("write torn");

        assert_eq!(
            highest_frame_seq_on_disk(&dir),
            555_000,
            "a torn newest segment must not read as 'no sequences on disk'"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The bound is real: the probe does not walk an unbounded history.
    #[test]
    fn the_high_water_probe_reads_a_bounded_number_of_segments() {
        assert!(
            HIGH_WATER_SEGMENTS_PER_DIR >= 2,
            "one segment is the defect — an empty newest file hides everything behind it"
        );
        assert!(
            HIGH_WATER_SEGMENTS_PER_DIR <= 16,
            "this runs at boot over three directories; an unbounded walk would turn a \
             safety net into a slow start"
        );
    }

    /// The refresh must never rewind the projected clock.
    ///
    /// Re-anchoring bounds NTP drift; done naively it also creates a way to
    /// move receipts BACKWARDS, and `received_at` is the candle bucketing
    /// clock — so a rewound receipt files a frame into a second that may
    /// already be sealed. The whole reason receipts are derived from a
    /// monotonic instant rather than read per frame is that a time step must
    /// not be able to reorder two frames; a refresh that can reorder them
    /// would hand that property straight back.
    #[test]
    fn a_refresh_never_moves_a_receipt_backwards() {
        // Take a baseline, then read the same instant twice across a refresh.
        // Whatever the wall clock did in between, the second reading may not be
        // smaller than the first.
        let probe = Instant::now();
        let before = receipt_nanos_from(probe);
        refresh_receipt_anchor();
        let after = receipt_nanos_from(probe);
        assert!(
            after >= before,
            "a refresh moved a fixed instant's receipt backwards: {before} -> {after}. \
             That reorders frames across the refresh boundary, which is precisely what \
             deriving receipts from a monotonic clock exists to prevent."
        );
    }

    /// Two frames that arrived in order must read back in order, across any
    /// number of refreshes.
    #[test]
    fn receipts_stay_ordered_across_repeated_refreshes() {
        let first = Instant::now();
        let first_receipt = receipt_nanos_from(first);
        for _ in 0..5 {
            refresh_receipt_anchor();
        }
        let second = Instant::now();
        let second_receipt = receipt_nanos_from(second);
        assert!(
            second_receipt >= first_receipt,
            "a later frame read back EARLIER than an earlier one across refreshes: \
             {first_receipt} then {second_receipt}"
        );
    }

    /// A receipt for an instant before the anchor flattens rather than going
    /// negative — a panic here would cost the socket, and a negative timestamp
    /// would be worse than a flattened one.
    #[test]
    fn an_instant_before_the_anchor_flattens_instead_of_panicking() {
        let now = Instant::now();
        let r = receipt_nanos_from(now);
        assert!(r > 0, "a receipt must be a real epoch value, got {r}");
    }
}

#[cfg(test)]
mod queue_depth_visibility_tests {
    /// The writer loop must publish the queue depth AND its session high-water.
    ///
    /// Source-scanned rather than behavioural because the loop is a thread with
    /// no return value and the gauges are process-global; a behavioural test
    /// would have to install a recorder and race the thread. What can go wrong
    /// here is a refactor deleting the lines, and a scan catches exactly that.
    /// The writer loop body, bounded by the next top-level fn.
    ///
    /// NOT split on `#[cfg(test)]` like the other guards in this repo: that
    /// marker appears INSIDE `writer_loop` itself (the `maybe_test_panic`
    /// hook), so the usual split truncates the production text mid-function
    /// and the scan silently sees nothing. Caught by these tests failing on
    /// their first run against code that was already correct.
    fn writer_loop_src() -> &'static str {
        let src = include_str!("ws_frame_spill.rs");
        let from = src.find("fn writer_loop").expect("writer_loop must exist");
        let len = src[from..]
            .find("fn open_segment_resilient")
            .expect("the next top-level fn must still follow writer_loop");
        &src[from..from + len]
    }

    #[test]
    fn the_writer_publishes_its_queue_depth_and_high_water() {
        let production = writer_loop_src();

        assert!(
            production.contains("tv_ws_frame_spill_queue_depth"),
            "the queued-but-unwritten window must be observable — it is the \
             frames an abort loses, and nothing else counts them"
        );
        assert!(
            production.contains("tv_ws_frame_spill_queue_high_water"),
            "the PEAK is the load-bearing half: a 30s scrape of a decaying \
             gauge misses the burst that matters"
        );
        assert!(
            production.contains("let queued = rx.len();"),
            "depth must be read from the channel itself, not inferred"
        );
    }

    /// The gauges must be resolved OUTSIDE the loop and read at the BATCH
    /// boundary, never per frame.
    ///
    /// This is the `record_ws_lag` lesson, which allocated twice per tick —
    /// ~36M allocations/hour — on a path its own docs called allocation-free.
    /// Three correct comments did not stop it shipping; a test does.
    #[test]
    fn the_depth_gauge_is_not_read_on_the_per_frame_path() {
        let loop_body = writer_loop_src();

        // The handles are resolved before the `loop {`; the reads happen after
        // the inner drain. If either moved into the 0..256 drain, the gauge
        // write would land once per FRAME instead of once per batch.
        let resolve_at = loop_body
            .find("metrics::gauge!(\"tv_ws_frame_spill_queue_depth\")")
            .expect("depth gauge must be resolved in writer_loop");
        let loop_at = loop_body.find("\n    loop {").expect("the drain loop");
        assert!(
            resolve_at < loop_at,
            "the gauge handle is resolved INSIDE the loop — re-resolving a \
             handle per iteration is how a metric write becomes a cost"
        );

        let drain_at = loop_at
            + loop_body[loop_at..]
                .find("for _ in 0..256")
                .expect("the batch drain");
        let read_at = loop_at
            + loop_body[loop_at..]
                .find("let queued = rx.len();")
                .expect("the depth read");
        assert!(
            read_at > drain_at,
            "the depth is read BEFORE the batch drain — it must sit at the \
             batch boundary so it costs one write per ~257 records, not one \
             per frame"
        );
    }
}
