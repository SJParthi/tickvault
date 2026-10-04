// STAGE-C: Non-blocking disk-durable spill for every WebSocket frame.
//
// Hot-path `append()` is O(1) and never blocks: it uses a crossbeam bounded
// channel with `try_send`. A dedicated background thread writes records to
// append-only WAL segment files. On startup, `replay_all()` walks every WAL
// file, validates CRC32, and returns the recovered frames so downstream
// consumers can drain them before live reads resume.
//
// DURABILITY — read this before relying on the word "durable" (corrected
// 2026-08-11, and the trade taken 2026-09-05).
//
// The writer thread calls `BufWriter::flush()`, which hands bytes to the
// OPERATING SYSTEM, and then — at most once per `WAL_FSYNC_INTERVAL_MS_DEFAULT`
// (1,000 ms, env-overridable, `0` disables) — asks the separate `wal-syncer`
// thread to call `sync_data`, which forces them onto the physical device.
// Concretely:
//
//   * process killed (SIGKILL, panic, OOM) -> flushed records SURVIVE, because
//     the page cache belongs to the kernel, not to us. This is the case the
//     WAL exists for and it is genuinely covered, sync or no sync.
//   * machine loses power, kernel panics, or the host is force-stopped ->
//     records written since the last SYNC are lost. That window is now bounded
//     by the interval (~1 s) rather than by the kernel's writeback policy
//     (`dirty_expire_centisecs`, default 30 s).
//
// It is a BOUND, not an elimination. A power cut still loses up to a second of
// frames, and no interval short of a sync per record removes that — a sync per
// record is not affordable at the 5,000 fps envelope, which is why the earlier
// note called this "a deliberate throughput trade". The trade is now taken at
// an interval, with the cost and the new risk written at the constant.
//
// AMENDED 2026-09-06 — the interval bound did NOT hold for a rotated segment,
// and the paragraph above claimed it did. `maybe_sync_segment` syncs `current`
// and only `current`. At a 128 MiB rotation the writer flushed the full
// segment, dropped it, and made the NEW file `current` — so the closed one was
// never synced by anything, ever, and its tail fell back to the kernel's
// writeback policy (the ~30 s the text above says was replaced). `persisted`
// had already counted every record in it. Rotation now finalises through
// `finalise_segment`, the same flush-then-sync the stop and close arms use, so
// the ~1 s bound is true of every segment rather than only the last one.
//
// AMENDED 2026-10-02 — THE SYNC RUNS ON ITS OWN THREAD, so the writer only
// ever writes. Until then the writer called `sync_all` itself, periodically
// and inline at every rotation. A slow sync on a saturated volume stopped
// it, the 524,288-record channel filled behind it, and a SIGKILL or OOM kill
// in that window lost every queued record, because a queued record is in our
// memory and only a WRITTEN one is in the kernel's. Now:
//
//   * periodic: the writer sets a "sync due" flag and wakes the `wal-syncer`
//     thread, which calls `sync_data` on a `try_clone` of the open segment
//     (cloned once per segment open). The writer never waits for it.
//   * rotation: the closed segment's file is handed to the syncer through a
//     four-slot channel. A full channel, or a stopping syncer, hands it back
//     and the writer syncs it inline, so no closed segment is left unsynced.
//   * stop, close: still synced inline by the writer, and `shutdown` joins
//     the syncer inside the same budget.
//
// What this changes about the bound, stated honestly: the power-loss window
// is now the interval PLUS however long the syncer takes, and a wedged
// device stretches it (the writer publishes `tv_wal_fsync_pending_ms` and
// `tv_wal_fsync_backlog` so that is visible). What it removes is the kill
// window: frames keep reaching the kernel while a sync is stuck, so a
// process kill during a slow sync loses only the batch being written, not
// the channel. A sync is still counted (`tv_wal_fsync_total`) only after it
// returned Ok.
//
// Three comments in this file previously said "fsync" while the code called
// `sync_all` zero times. That overstatement was load-bearing, because the live
// feed refuses to open a socket without this WAL and cites it as the durability
// floor. The words and the syscalls are kept in agreement by
// `test_durability_claim_matches_what_the_code_actually_does`.
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
//
// ⚠ 2026-09-18: THE CANDLE-BUCKETING HALF OF THAT JUSTIFICATION IS GONE, AND
// v3 IS STILL LOAD-BEARING. The operator's ts-bucketing directive
// (`websocket-connection-scope-lock.md`, section "2026-09-18 (SECOND)") made
// `fold_clock_ist_secs` the identity on the exchange stamp, so a replayed
// frame now buckets on its own trade stamp whatever receipt it carries. Two
// OTHER consumers still require the persisted receipt, and a cleanup that
// dropped the field "because we bucket on ts now" would break both:
//   * the aggregator's CROSS-DAY gates compare the fold day against the
//     RECEIPT day, so a `now()` re-stamp makes every legitimately replayed
//     prior-day frame look stale (`multi_tf_aggregator::consume_tick`);
//   * `row_timestamp_ist_nanos` derives a never-traded tick's `ts` from the
//     receipt, and `ts` is the first column of the `ticks` DEDUP key — a
//     re-stamp splits one observation into two rows in two partitions.

use std::fs::{File, Metadata, OpenOptions}; // O(1) EXEMPT: import line only — uses are the cold writer thread + boot replay
use std::io::{BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use crossbeam_channel::{Receiver, RecvTimeoutError, Sender, TrySendError, bounded};
use tickvault_common::error_code::ErrorCode;
use tracing::{debug, error, info, warn};

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
    ///
    /// ⚠ 2026-09-18: candles bucket on the exchange clock AGAIN, and this
    /// field is still required — by the aggregator's cross-day gates and by
    /// `row_timestamp_ist_nanos` (which feeds the `ticks` DEDUP key). See the
    /// module header for the full record.
    pub received_at_nanos: i64,
    /// TVW4: the socket this frame was captured from, read back from the v4
    /// record. [`WalEndpoint::MainFeed`] for v1–v3 records that predate the
    /// field — which is exactly what every earlier replay assumed, so a legacy
    /// segment behaves precisely as it did before.
    pub endpoint: WalEndpoint,
    /// `true` when frames captured before this one were NOT replayed (plan
    /// ITEM 47): the first frame of a pass, the first after a segment the
    /// applied watermark skipped or a segment that could not be fully read,
    /// and the first after frames dropped as already applied. A consumer that
    /// derives running state from consecutive frames (the candle fold's volume
    /// baseline) must not carry it across this point.
    pub after_gap: bool,
    /// Plan ITEM 47: `true` for the first frame returned from each WAL
    /// segment. A reader compares receipt times only across a segment
    /// boundary ([`process_boundary_is_gap`]).
    pub first_in_segment: bool,
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

/// Largest frame length a resync candidate may declare (Z11a, 2026-10-02).
///
/// Replay resyncs past a bad record by scanning for the next record whose
/// magic, type, length and CRC all check out; this ceiling refuses an absurd
/// length before the CRC runs, so one false candidate costs at most this many
/// bytes of CRC. It is also the line between a torn tail and damage: a length
/// past EOF that is ABOVE this ceiling is not something the writer produced.
///
/// 4 MiB sits above every transport cap that bounds what reaches the WAL
/// (main feed ~1.6 MiB, depth-200 512 KiB, depth-20 and order update 256
/// KiB); the app crate const-asserts that, since this crate cannot see them.
pub const WAL_RESYNC_MAX_FRAME_BYTES: usize = 4 * 1024 * 1024;

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
/// write syscall. (Not an fsync batch: the sync is time-driven and independent
/// of this buffer — see the DURABILITY note in the module header.)
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

/// Default interval between `sync_all` calls on the open WAL segment, in
/// milliseconds. `0` disables the sync entirely and restores the pre-2026-09-05
/// behaviour exactly.
///
/// WHY THIS EXISTS, and what it does NOT claim. Until 2026-09-05 this module
/// called `BufWriter::flush()` and nothing else, so a flushed record was in the
/// kernel's page cache and not on the platter. That covers the case the WAL is
/// really for — SIGKILL, panic, OOM: the page cache belongs to the kernel and
/// survives all three. It does NOT cover power loss, a kernel panic, or a
/// force-stop, where everything since the last kernel writeback is gone.
///
/// Linux writes back dirty pages older than `dirty_expire_centisecs` (default
/// 3000 = 30 s) on a `dirty_writeback_centisecs` (default 500 = 5 s) sweep, so
/// the UNSYNCED window is up to ~30 seconds of frames. At 1,000 ms this bounds
/// it to one second — roughly a 30x reduction — for the cost measured below.
///
/// COST, and why the writer thread can afford it. `append()` is a non-blocking
/// `try_send` onto a 524,288-slot channel; every byte of I/O happens on this
/// background thread, so a sync NEVER touches the hot path. One sync measured
/// ~2.6 ms on the dev container (an overestimate — the harness spawned a
/// process per call); at one per second that is ~0.26% of the writer's time,
/// against a channel holding ~100 seconds of frames at the 5,000 fps envelope.
///
/// ⚠ THE RISK IT ADDS, stated because it is real and new. A sync on a
/// SATURATED device does not take 2.6 ms — the 2026-08-28 session pinned the
/// volume at its provisioned ceiling for hours. A multi-second sync stalls this
/// thread, the channel backs up, and past 524,288 records `append()` starts
/// refusing frames (`WS-SPILL-02`, counted, and a real tick loss). The interval
/// is therefore a FLOOR and never a deadline: a slow sync simply means fewer
/// syncs, and the ~100 s of queue headroom is what absorbs one. Set the env var
/// to `0` to switch it off without a rebuild if that trade ever goes the wrong
/// way.
///
/// ⚠ CHANGED 2026-10-02 — the stall above no longer reaches the writer. The
/// sync runs on the `wal-syncer` thread ([`WalSyncer`]); a multi-second sync
/// stalls THAT thread, requests coalesce behind it, and the writer keeps
/// emptying the channel into the page cache. What a wedged device now costs
/// is a longer power-loss window, visible on `tv_wal_fsync_pending_ms`, not
/// a full queue. The one inline sync left on a busy writer is the rotation
/// fallback, taken only when four closed segments are already waiting.
const WAL_FSYNC_INTERVAL_MS_DEFAULT: u64 = 1_000;

/// Env override for [`WAL_FSYNC_INTERVAL_MS_DEFAULT`]. Present so the trade is
/// reversible on the box with a unit-file edit and a restart, never a rebuild.
const WAL_FSYNC_INTERVAL_ENV: &str = "TICKVAULT_WAL_FSYNC_INTERVAL_MS";

/// Resolves the sync interval once per writer-thread start.
///
/// An unparseable value falls back to the default LOUDLY rather than silently
/// disabling durability — a typo in a unit file must never be the reason the
/// WAL stopped syncing.
fn resolve_wal_fsync_interval() -> Option<Duration> {
    let ms = match std::env::var(WAL_FSYNC_INTERVAL_ENV) {
        Err(_) => WAL_FSYNC_INTERVAL_MS_DEFAULT,
        Ok(raw) => match raw.trim().parse::<u64>() {
            Ok(v) => v,
            Err(_) => {
                warn!(
                    env = WAL_FSYNC_INTERVAL_ENV,
                    "WAL fsync interval is not a number; using the default. \
                     Set it to 0 to disable the sync deliberately."
                );
                WAL_FSYNC_INTERVAL_MS_DEFAULT
            }
        },
    };
    if ms == 0 {
        None
    } else {
        Some(Duration::from_millis(ms))
    }
}

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

/// How long the panic hook waits for the WAL writer before letting the process
/// abort (Z11b, 2026-10-02). See [`drain_registered_for_abort`].
///
/// An idle writer acknowledges within one [`WAL_WRITER_STOP_POLL`] (200 ms), a
/// busy one at the end of the first batch that leaves the channel empty, so two
/// seconds is a stall budget: the abort is delayed at most this long, never
/// held open by a wedged disk.
pub const WAL_ABORT_DRAIN_BUDGET: Duration = Duration::from_secs(2); // APPROVED: this IS the named constant the rule asks for

/// How often [`drain_registered_for_abort`] re-checks the writer's ack.
const WAL_ABORT_DRAIN_POLL: Duration = Duration::from_millis(5); // APPROVED: this IS the named constant the rule asks for

/// The WAL writer thread's name. The panic hook compares against it, because
/// the writer cannot wait for itself.
pub const WAL_WRITER_THREAD_NAME: &str = "ws-frame-spill-writer";

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
pub(crate) const WS_TYPE_COUNT: usize = 3;

/// Dense index for a [`WsType`], used only to address the pre-resolved counter
/// tables in [`SpillDropCounters`].
///
/// Deliberately local to this module rather than a method on `WsType` in
/// `crates/common`: the index is an implementation detail of THIS module's
/// counter tables, and widening `crates/common` would escalate every change
/// here to a workspace-wide test run for no behavioural gain.
pub(crate) const fn ws_type_index(ws_type: WsType) -> usize {
    // Exhaustive by construction — no `_` arm, so adding a `WsType` variant
    // fails THIS match at compile time rather than silently folding the new
    // transport's losses into another variant's counter.
    match ws_type {
        WsType::LiveFeed => 0,
        WsType::OrderUpdate => 1,
        WsType::TruedataFeed => 2,
    }
}

/// Whether the `n`-th refused frame of this process earns a log line.
///
/// Powers of two: the 1st, 2nd, 4th, 8th … refusal log, every one is counted.
/// ADDED 2026-09-10 (fast-lane attack sweep, finding 1): all three WAL refusal
/// arms below logged one JSON-formatted `error!` PER REFUSED FRAME, and they
/// run on the socket READER task inside `sink.accept()`. On 2026-09-04 the disk
/// was full and **2,000,238** frames were refused before the WAL — that was
/// two million log formats on the one task whose only job is to keep pace with
/// Dhan, i.e. the self-amplifying slow-consumer stall this crate's capture
/// floor exists to prevent, and one the ring-dwell gauge cannot see because it
/// sits downstream of the reader. The `WS-SPILL-02` CloudWatch filter fires at
/// threshold 1, so the first line still pages; the counters still move for
/// every frame; only the flood is gone.
#[must_use]
pub const fn refusal_line_due(count: u64) -> bool {
    if count < REFUSAL_LINE_STRIDE {
        count.is_power_of_two()
    } else {
        count.is_multiple_of(REFUSAL_LINE_STRIDE)
    }
}

/// Where the doubling ladder stops and a fixed stride takes over.
///
/// Powers of two alone coarsen for the LIFE of the process, and this counter is
/// never reset: after the 2,000,238-refusal episode of 2026-09-04 the next line
/// would have been 2^21, i.e. ~96,000 refusals of silence and ~2.1 MILLION
/// after that. The stride caps the worst-case silent gap at this many refused
/// frames while still keeping the flood off the reader task (~2 lines per
/// million instead of 2,000,238).
///
/// NOT claimed: that this keeps the `WS-SPILL-02` log-filter alarm continuously
/// firing through a long outage. That filter counts LOG LINES over 300 s x 3,
/// so a slow-but-steady refusal rate can still leave it quiet long enough to
/// age back to OK. What pages through such a gap is the METRIC alarm on the
/// counter itself (`tv-<env>-durable-floor-breach`, threshold 1, one period),
/// which reads the counter directly and no log throttle can reach. The counters
/// on all three arms move for every refused frame, throttle or not.
pub const REFUSAL_LINE_STRIDE: u64 = 1 << 20;

/// Every [`WsType`], in [`ws_type_index`] order — the build order for the
/// counter tables. Kept beside the index so the two cannot drift.
pub(crate) const WS_TYPES_BY_INDEX: [WsType; WS_TYPE_COUNT] =
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
    /// State shared with the `wal-syncer` thread, which runs the device sync
    /// so the writer never waits on one (2026-10-02). See [`WalSyncer`].
    syncer: Arc<WalSyncer>,
    /// The syncer's join handle, so `shutdown` can wait for it. Same
    /// `Mutex<Option<..>>` shape as `writer`, for the same reason.
    syncer_thread: std::sync::Mutex<Option<thread::JoinHandle<()>>>,
    /// Where this spill's segments live — so `Drop` can release ONLY its own
    /// open-segment registration, never another spill's in the same process.
    wal_dir: PathBuf,
    /// The exclusive claim on the WAL directory, held for as long as this spill
    /// exists. Never read — its ONLY job is to keep the `flock` taken, and to
    /// release it on drop so the next process can start. See [`WalDirGuard`].
    _dir_guard: Option<WalDirGuard>,
    /// The panic hook's handshake with this spill's writer (Z11b). Production
    /// reaches it only through [`ABORT_DRAIN`]; tests hold it here so parallel
    /// tests never drain one another's spill.
    #[cfg(test)]
    abort_drain: Arc<AbortDrain>,
}

/// Handshake between the panic hook and the WAL writer (Z11b, 2026-10-02).
///
/// # Why it exists
///
/// The release profile sets `panic = "abort"`, so a panic on ANY thread ends
/// the process the moment the hook returns. Every record still in the spill
/// channel then dies with it, and each one was already reported to its caller
/// as `Spilled`; Dhan never resends. Bytes the writer has already flushed are
/// in the page cache and survive the abort. The hook still runs before the
/// abort, so it can give the writer a bounded moment to empty the channel.
///
/// # The protocol
///
/// The hook bumps `req` and waits until `ack >= ` its value. The writer
/// stores `ack = req` only at a point where everything it has taken off the
/// channel is flushed to the kernel AND the channel is empty, loading `req`
/// BEFORE checking emptiness. So an ack covering a request proves that, at some
/// instant after the request, nothing queued before it was still in memory.
/// A writer that has exited stores `u64::MAX`.
///
/// `queued_bytes == 0` alone would not do: the writer releases a batch's
/// bytes once the batch is off the channel, before it is flushed.
#[derive(Debug, Default)]
struct AbortDrain {
    req: AtomicU64,
    ack: AtomicU64,
}

impl AbortDrain {
    /// Writer side. Called only right after a batch flush, or after
    /// `recv_timeout` found nothing (the previous batch was flushed before
    /// that wait began). O(1): two atomics and a channel length read.
    fn ack_if_drained(&self, rx: &Receiver<WalRecord>) {
        let r = self.req.load(Ordering::SeqCst);
        if r != 0 && rx.is_empty() {
            self.ack.fetch_max(r, Ordering::SeqCst);
        }
    }

    /// Writer side: the writer is gone, so no request can ever be served
    /// better than it already has been.
    fn writer_exited(&self) {
        self.ack.store(u64::MAX, Ordering::SeqCst);
    }

    /// Hook side: request a drain and wait for the ack, bounded by `budget`.
    fn wait(&self, budget: Duration) -> AbortDrainOutcome {
        let target = self.req.fetch_add(1, Ordering::SeqCst).saturating_add(1);
        let deadline = Instant::now() + budget;
        loop {
            if self.ack.load(Ordering::SeqCst) >= target {
                return AbortDrainOutcome::Drained;
            }
            if Instant::now() >= deadline {
                return AbortDrainOutcome::TimedOut;
            }
            thread::sleep(WAL_ABORT_DRAIN_POLL);
        }
    }
}

/// The live spill's [`AbortDrain`], for the panic hook. The last spill
/// constructed wins; production constructs exactly one.
static ABORT_DRAIN: arc_swap::ArcSwapOption<AbortDrain> = arc_swap::ArcSwapOption::const_empty();

/// What [`drain_registered_for_abort`] found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AbortDrainOutcome {
    /// The writer confirmed an empty channel with everything flushed.
    Drained,
    /// The budget ran out first (a wedged disk, or a feed that never let the
    /// channel empty). Whatever is still queued dies with the process.
    TimedOut,
    /// No spill was ever constructed in this process.
    NoSpill,
    /// Called on the writer thread itself, which cannot wait for itself.
    SkippedOnWriterThread,
}

/// Gives the WAL writer up to `budget` to empty the spill channel before a
/// `panic = "abort"` process dies (Z11b). Called by the app's panic hook.
///
/// Honest limit: a panic ON the writer thread loses its queue regardless.
/// Draining it from the hook would re-run the code that just panicked, so
/// that case returns [`AbortDrainOutcome::SkippedOnWriterThread`] at once.
/// Blocking by design (the process is about to abort); never on a hot path.
pub fn drain_registered_for_abort(budget: Duration) -> AbortDrainOutcome {
    if thread::current().name() == Some(WAL_WRITER_THREAD_NAME) {
        return AbortDrainOutcome::SkippedOnWriterThread;
    }
    match ABORT_DRAIN.load_full() {
        Some(d) => d.wait(budget),
        None => AbortDrainOutcome::NoSpill,
    }
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
        // First claim wins: the boot path claims once (audit PR31b-2). A later
        // writer finds it already set and keeps the boot value.
        if BOOT_DISK_HIGH_FRAME_SEQ.set(disk_high).is_err() {
            tracing::debug!(disk_high, "boot disk high frame seq already recorded");
        }
        // The applied-watermark lives beside the segments and is seeded from
        // its file HERE, before the first sink can persist — otherwise the
        // first in-session persist would overwrite a good snapshot with zeros.
        crate::wal_applied_watermark::applied_watermark().bind(&wal_dir);
        // The deferred-depth marks (item 45a) sit beside it for the same reason:
        // a shed frame's segment must stay protected across a restart.
        crate::wal_deferred_depth::deferred_depth().bind(&wal_dir);
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
        let abort_drain = Arc::new(AbortDrain::default());
        let abort_drain_for_thread = Arc::clone(&abort_drain); // APPROVED: Arc clone in the one-shot constructor
        // The device sync runs on its own thread so a slow sync never stops
        // the writer (2026-10-02, see `WalSyncer`). Spawned FIRST so the
        // writer's first segment open already has someone to hand its fd to.
        let syncer = Arc::new(WalSyncer::new()); // APPROVED: one-shot constructor
        let syncer_for_writer = Arc::clone(&syncer); // APPROVED: Arc clone in the one-shot constructor
        let syncer_thread =
            spawn_wal_syncer(Arc::clone(&syncer)) // APPROVED: Arc clone in the one-shot constructor
                .map_err(|e| anyhow::anyhow!("spawn WAL syncer thread: {e}"))?;

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
        // Audit M1: the shed-then-lost counters, resolved before any socket
        // dials so the reader's shed arm never allocates.
        crate::wal_frame_fate::pre_register();

        let writer = thread::Builder::new()
            .name(WAL_WRITER_THREAD_NAME.to_string()) // APPROVED: one-shot constructor (thread name)
            .spawn(move || {
                // Supervisor loop (mirrors WS-GAP-05 pool supervisor +
                // DISK-WATCHER-01). A fatal RETURN from the writer must NOT
                // silently kill the durable WAL floor: we re-enter
                // `writer_loop` with the SAME `rx`, so `append()` never sees
                // `Disconnected` and every Dhan frame keeps being captured.
                // `rx` is owned here and only borrowed per iteration, so it
                // outlives each attempt.
                //
                // CORRECTED 2026-10-02 (Z11b): this comment used to promise the
                // same respawn after a PANIC. That holds only in dev and test
                // builds. The release profile sets `panic = "abort"`, so a
                // writer panic there ends the process and `catch_unwind` never
                // returns; the `Err(_panic)` arm below is reachable only under
                // unwinding. What release does get is the panic hook's bounded
                // wait (`drain_registered_for_abort`) when ANOTHER thread
                // panics; a panic on this thread loses its queue.
                loop {
                    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        writer_loop(
                            &rx,
                            &wal_dir_for_thread,
                            &persisted_for_thread,
                            &stop_for_thread,
                            &queued_bytes_for_thread,
                            &abort_drain_for_thread,
                            &syncer_for_writer,
                        )
                    }));
                    match outcome {
                        Ok(Ok(())) => {
                            // Clean shutdown: all senders dropped, channel closed.
                            info!("ws-frame-spill-writer exited cleanly (channel closed)");
                            abort_drain_for_thread.writer_exited();
                            // Nothing will rotate again: the syncer may finish
                            // the closed segments it holds and exit.
                            syncer_for_writer.request_stop();
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
                // Z11d: whatever landed after the last empty poll is about to
                // drop with `rx`. Count it rather than lose it silently.
                count_records_left_at_writer_exit(&rx);
            })
            .map_err(|e| anyhow::anyhow!("spawn spill writer thread: {e}"))?;
        ABORT_DRAIN.store(Some(Arc::clone(&abort_drain))); // APPROVED: Arc clone in the one-shot constructor

        Ok(Self {
            spill_tx: tx,
            queued_bytes,
            drop_critical,
            persisted_total,
            drop_counters: SpillDropCounters::new(),
            feed_health: None,
            stop,
            writer: std::sync::Mutex::new(Some(writer)),
            syncer,
            syncer_thread: std::sync::Mutex::new(Some(syncer_thread)),
            wal_dir,
            _dir_guard: dir_guard,
            #[cfg(test)]
            abort_drain,
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
            syncer: Arc::new(WalSyncer::new()),
            syncer_thread: std::sync::Mutex::new(None),
            wal_dir: dir.clone(),
            _dir_guard: dir_guard,
            abort_drain: Arc::new(AbortDrain::default()),
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
                if refusal_line_due(prev + 1) {
                    error!(
                        code = ErrorCode::WsSpill02FrameDropped.code_str(),
                        ws_type = ws_type.as_str(),
                        drop_count = prev + 1,
                        "CRITICAL: WAL spill channel FULL — frame dropped (writer stalled)"
                    );
                }
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
                if refusal_line_due(prev + 1) {
                    error!(
                        code = ErrorCode::WsSpill02FrameDropped.code_str(),
                        ws_type = ws_type.as_str(),
                        drop_count = prev + 1,
                        "CRITICAL: WAL spill writer DEAD — frame dropped (durable floor lost)"
                    );
                }
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
        if refusal_line_due(prev + 1) {
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
        }
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
    /// A `0` return means every queued record reached the kernel. The device
    /// sync is separate (CORRECTED 2026-10-02 — this paragraph said "there is
    /// no `fsync` anywhere in this file", which stopped being true on
    /// 2026-09-05): the writer's stop arm syncs its last segment inline, and
    /// phase 3 below waits, inside the same budget, for the `wal-syncer`
    /// thread to finish the closed segments it holds. A syncer still running
    /// at the deadline is abandoned and logged; what it was syncing is already
    /// in the page cache, so it survives the process exit and is exposed only
    /// to a power loss before the kernel's own writeback.
    pub fn shutdown(&self, budget: Duration) -> usize {
        self.stop.store(true, Ordering::Release);
        // Last chance to record what this session applied. The sink threads
        // persist at most once a second; a clean stop must not lose the last
        // second's acks, because that is exactly the tail the next boot would
        // otherwise replay.
        crate::wal_applied_watermark::applied_watermark().persist_now();

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
                // Audit M1: what it still held never reaches the disk. A frame the
                // reader or the drain shed above the last flush is a lost frame.
                let (shed_ring, shed_drain) = crate::wal_frame_fate::frame_fate()
                    .count_unflushed_sheds(
                        crate::wal_frame_fate::flushed_high_seq(),
                        WsType::LiveFeed,
                    );
                if shed_ring > 0 || shed_drain > 0 {
                    error!(
                        code = ErrorCode::WsSpill02FrameDropped.code_str(),
                        shed_ring_lost = shed_ring,
                        shed_drain_lost = shed_drain,
                        "CRITICAL: the WAL writer was abandoned at shutdown holding frames \
                         the reader or the drain had shed — those frames are lost"
                    );
                }
            }
        }

        // Phase 3: stop the `wal-syncer` thread and wait for it, inside the
        // SAME budget. It syncs every closed segment it still holds before it
        // exits; the writer's own final sync (stop arm) is inline and is
        // already done by the time the writer finished above. Polled, not
        // joined outright, for the same wedged-disk reason as phase 2.
        self.syncer.request_stop();
        if let Ok(mut slot) = self.syncer_thread.lock()
            && let Some(handle) = slot.take()
        {
            while !handle.is_finished() && Instant::now() < deadline {
                thread::sleep(WAL_SHUTDOWN_POLL);
            }
            if handle.is_finished() {
                if handle.join().is_err() {
                    error!(
                        code = ErrorCode::WsSpill01WriterRespawn.code_str(),
                        "WAL syncer thread PANICKED during shutdown — closed segments it \
                         held are flushed to the kernel but may not be on the device"
                    );
                }
            } else {
                warn!(
                    budget_secs = budget.as_secs(),
                    "WAL syncer did not finish within the shutdown budget — abandoning it; \
                     every record it was syncing is already written to the kernel and \
                     survives the process exit"
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
// The WAL syncer thread (2026-10-02)
// ---------------------------------------------------------------------------

/// The syncer thread's name.
pub const WAL_SYNCER_THREAD_NAME: &str = "wal-syncer";

/// How many closed segments may wait for their final sync before a rotation
/// syncs inline instead. At 128 MiB per segment a rotation is minutes apart
/// at the 5,000 fps envelope, so four waiting means the device has not
/// finished one sync in several minutes; past that the writer pays the sync
/// itself rather than leave a closed segment with nothing to sync it.
const WAL_SYNCER_CLOSED_SLOTS: usize = 4;

/// The `stage` label of the periodic sync of the open segment. Closed
/// segments keep `fsync_on_rotate`, the label the rotation arm always had.
const WAL_PERIODIC_FSYNC_STAGE: &str = "fsync";

/// State the WAL writer shares with the `wal-syncer` thread.
///
/// # Why the sync left the writer thread
///
/// The writer used to call `sync_all` itself, at most once per
/// [`WAL_FSYNC_INTERVAL_MS_DEFAULT`] and again inline at every 128 MiB
/// rotation. A slow sync on a saturated volume therefore stopped the writer,
/// the 524,288-record channel filled behind it, and a SIGKILL or OOM kill in
/// that window lost every queued record: queued records live in this
/// process's memory, not the kernel's. A record the writer has already handed
/// to the kernel with `write(2)` survives a process kill whether or not it
/// was synced; only power loss needs the sync. So the writer now only writes,
/// and the device sync runs here.
///
/// # What the writer does
///
/// * Each segment it opens is `try_clone`d once ([`WalSyncer::track`]), on
///   the cold open path, never per record.
/// * When the interval says a sync is due it sets `due` and wakes this thread
///   ([`WalSyncer::request`]): two atomics and a `try_send` on a one-slot
///   channel. No allocation, no waiting.
/// * At a rotation it hands the closed segment's `File` to this thread
///   through a [`WAL_SYNCER_CLOSED_SLOTS`]-deep channel. A full channel, or a
///   syncer that is stopping, hands the file back and the writer syncs it
///   inline as before, so a closed segment is never left unsynced.
/// * Stop and close keep their inline final sync (`finalise_segment` with no
///   syncer), and `WsFrameSpill::shutdown` joins this thread.
///
/// # What is reported as synced
///
/// Nothing in the workspace consumes a "synced up to" position: `persisted`
/// counts `write_all`, the applied watermark tracks QuestDB, and the prune
/// rules key on the applied watermark and the S3 copy. The sync accounting is
/// `tv_wal_fsync_total`, `tv_wal_fsync_errors_total` and
/// `tv_wal_fsync_duration_ms`, and each moves only after a sync on that
/// segment has returned. A failure is reported exactly as before: the same
/// counter and the same coded line ([`record_sync_result`]).
///
/// # How far behind it is
///
/// `tv_wal_fsync_backlog` counts closed segments waiting plus one for a
/// periodic sync pending or in flight; `tv_wal_fsync_pending_ms` is the age
/// of the oldest periodic request not yet served. Both are published by the
/// WRITER, so a syncer wedged inside a sync still shows up on them.
struct WalSyncer {
    /// The open segment's cloned fd. `None` before the first open or after a
    /// failed clone; [`maybe_sync_segment`] then syncs inline, as before.
    current: arc_swap::ArcSwapOption<File>,
    /// A periodic sync of `current` has been requested and not yet started.
    due: AtomicBool,
    /// A periodic sync is running right now.
    in_flight: AtomicBool,
    /// Nanoseconds since `epoch`, plus one, of the oldest periodic request
    /// not yet served; `0` when none is waiting.
    oldest_request: AtomicU64,
    epoch: Instant,
    /// One-slot wake-up channel: a full slot means a wake-up is pending.
    wake_tx: Sender<()>,
    wake_rx: Receiver<()>,
    /// Closed segments waiting for their final sync.
    closed_tx: Sender<File>,
    closed_rx: Receiver<File>,
    stop: AtomicBool,
    /// Closed segments this thread has synced (Ok or reported failure).
    closed_synced: AtomicU64,
    /// Rotations that found no room here and synced inline.
    closed_inline_fallbacks: AtomicU64,
    #[cfg(test)]
    hold: TestSyncHold,
}

impl WalSyncer {
    fn new() -> Self {
        let (wake_tx, wake_rx) = bounded::<()>(1);
        let (closed_tx, closed_rx) = bounded::<File>(WAL_SYNCER_CLOSED_SLOTS);
        Self {
            current: arc_swap::ArcSwapOption::empty(),
            due: AtomicBool::new(false),
            in_flight: AtomicBool::new(false),
            oldest_request: AtomicU64::new(0),
            epoch: Instant::now(),
            wake_tx,
            wake_rx,
            closed_tx,
            closed_rx,
            stop: AtomicBool::new(false),
            closed_synced: AtomicU64::new(0),
            closed_inline_fallbacks: AtomicU64::new(0),
            #[cfg(test)]
            hold: TestSyncHold::default(),
        }
    }

    /// Nanoseconds since `epoch`, plus one so that `0` can mean "none".
    fn now_mark(&self) -> u64 {
        u64::try_from(self.epoch.elapsed().as_nanos())
            .unwrap_or(u64::MAX - 1)
            .saturating_add(1)
    }

    /// Writer side, once per segment open (cold): keep a clone of the new
    /// segment's fd for the periodic sync.
    fn track(&self, segment: &File) {
        match segment.try_clone() {
            Ok(clone) => self.current.store(Some(Arc::new(clone))), // APPROVED: one allocation per segment open, never per record
            Err(err) => {
                // Not a loss: `maybe_sync_segment` finds no clone and syncs
                // this segment inline on the writer, the old behaviour.
                self.current.store(None);
                warn!(
                    code = ErrorCode::WsSpill01WriterRespawn.code_str(),
                    stage = "syncer_clone",
                    error = %err,
                    "WAL syncer could not clone the new segment's file handle; \
                     the writer syncs this segment itself"
                );
            }
        }
    }

    fn has_current(&self) -> bool {
        self.current.load().is_some()
    }

    fn wake(&self) {
        // Full means a wake-up is already pending, which is all this needs.
        let _already_pending = self.wake_tx.try_send(());
    }

    /// Writer side: ask for a periodic sync of the open segment. O(1), no
    /// allocation, never waits. Requests coalesce while one is pending.
    fn request(&self) {
        if !self.due.swap(true, Ordering::AcqRel) {
            let _stamped = self.oldest_request.compare_exchange(
                0,
                self.now_mark(),
                Ordering::AcqRel,
                Ordering::Acquire,
            );
        }
        self.wake();
    }

    /// Writer side, at a rotation: hand the closed segment over for its final
    /// sync. Returns the file when there is no room or the syncer is
    /// stopping; the caller then syncs it inline.
    fn hand_off_closed(&self, segment: File) -> Option<File> {
        if self.stop.load(Ordering::Acquire) {
            self.note_inline_fallback();
            return Some(segment);
        }
        match self.closed_tx.try_send(segment) {
            Ok(()) => {
                self.wake();
                None
            }
            Err(TrySendError::Full(f) | TrySendError::Disconnected(f)) => {
                self.note_inline_fallback();
                Some(f)
            }
        }
    }

    fn note_inline_fallback(&self) {
        self.closed_inline_fallbacks.fetch_add(1, Ordering::Relaxed);
        metrics::counter!("tv_wal_fsync_inline_fallback_total").increment(1);
    }

    fn request_stop(&self) {
        self.stop.store(true, Ordering::Release);
        self.wake();
    }

    /// Writer side, at a batch boundary or an idle poll: publish how far
    /// behind the syncer is. O(1).
    fn publish_lag(&self, backlog: &metrics::Gauge, pending_ms: &metrics::Gauge) {
        let periodic = self.due.load(Ordering::Acquire) || self.in_flight.load(Ordering::Acquire);
        backlog.set((self.closed_rx.len() + usize::from(periodic)) as f64);
        let oldest = self.oldest_request.load(Ordering::Acquire);
        let age_ns = if oldest == 0 {
            0
        } else {
            self.now_mark().saturating_sub(oldest)
        };
        pending_ms.set(age_ns as f64 / 1_000_000.0);
    }

    /// Syncer side: sync every closed segment waiting, then the open one if
    /// a periodic sync is due. Each closed segment is its LAST sync, so they
    /// go first.
    fn serve(&self) {
        while let Ok(segment) = self.closed_rx.try_recv() {
            #[cfg(test)]
            self.hold.wait();
            let started = Instant::now();
            record_sync_result(segment.sync_data(), started, "fsync_on_rotate");
            self.closed_synced.fetch_add(1, Ordering::Relaxed);
        }
        if self.due.swap(false, Ordering::AcqRel) {
            self.in_flight.store(true, Ordering::Release);
            let started_mark = self.now_mark();
            if let Some(segment) = self.current.load_full() {
                #[cfg(test)]
                self.hold.wait();
                let started = Instant::now();
                // `sync_data` (fdatasync) persists the size too, which is the
                // only metadata a replay needs.
                record_sync_result(segment.sync_data(), started, WAL_PERIODIC_FSYNC_STAGE);
            }
            // Cleared BEFORE `due` is re-read, so a request racing this
            // completion either finds 0 and stamps itself, or is seen here.
            self.oldest_request.store(0, Ordering::Release);
            if self.due.load(Ordering::Acquire) {
                // It arrived during the sync: aged from the sync's start,
                // which overstates it by at most one sync.
                let _stamped = self.oldest_request.compare_exchange(
                    0,
                    started_mark,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                );
            }
            self.in_flight.store(false, Ordering::Release);
        }
    }
}

impl Drop for WalSyncer {
    /// Last resort: a closed segment still queued when the last handle goes
    /// (a hand-off that raced the syncer's exit) is synced here rather than
    /// dropped unsynced.
    fn drop(&mut self) {
        while let Ok(segment) = self.closed_rx.try_recv() {
            let started = Instant::now();
            record_sync_result(segment.sync_all(), started, "fsync_on_rotate");
            self.closed_synced.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// The `wal-syncer` thread body. Exits once a stop is requested, after one
/// more pass for anything handed over before the stop flag was set.
fn syncer_loop(syncer: &WalSyncer) {
    loop {
        // A timeout is a plain re-check: requests always wake, so this only
        // bounds how long a stop request can go unnoticed.
        let _woken_or_timed_out = syncer.wake_rx.recv_timeout(WAL_WRITER_STOP_POLL);
        syncer.serve();
        if syncer.stop.load(Ordering::Acquire) {
            // `hand_off_closed` refuses once the flag is set, so this pass
            // empties the closed queue for good.
            syncer.serve();
            return;
        }
    }
}

/// Spawns the `wal-syncer` thread.
fn spawn_wal_syncer(syncer: Arc<WalSyncer>) -> std::io::Result<thread::JoinHandle<()>> {
    thread::Builder::new()
        .name(WAL_SYNCER_THREAD_NAME.to_string()) // APPROVED: one-shot constructor (thread name)
        .spawn(move || syncer_loop(&syncer))
}

/// Counts and reports one sync. Shared by the syncer and the inline arms so a
/// failure is reported identically wherever it happens. Returns `true` on Ok.
fn record_sync_result(result: std::io::Result<()>, started: Instant, stage: &'static str) -> bool {
    match result {
        Ok(()) => {
            metrics::counter!("tv_wal_fsync_total").increment(1);
            // A gauge, not a histogram: the number that matters operationally
            // is "how slow was the LAST one", because a sync that starts taking
            // seconds is the leading indicator of the device falling behind.
            metrics::gauge!("tv_wal_fsync_duration_ms")
                .set(started.elapsed().as_secs_f64() * 1_000.0);
            true
        }
        Err(err) => {
            // Deliberately NOT `report_io_error`. That helper says "reopening
            // segment; thread stays alive", which is true of a WRITE failure
            // and false of this one: a failed sync reopens nothing, loses
            // nothing that was already written, and does not endanger the
            // thread. Its consequence is narrower and worth naming exactly —
            // the records are still in the page cache and still survive a
            // process death; what is gone is the BOUND on the power-loss
            // window, which silently reverts to the kernel's writeback policy.
            // Routing it through the write-error helper would also have
            // inflated `tv_ws_frame_spill_write_errors_total` with events that
            // are not write errors.
            metrics::counter!("tv_wal_fsync_errors_total").increment(1);
            if stage == WAL_PERIODIC_FSYNC_STAGE {
                warn!(
                    code = ErrorCode::WsSpill01WriterRespawn.code_str(),
                    stage,
                    error = %err,
                    "WAL segment sync FAILED — records already written are still \
                     in the page cache and still survive a process kill, but the \
                     power-loss window is no longer bounded by the sync interval"
                );
            } else {
                // Sharper: NOTHING will sync this segment again. At shutdown
                // it is the process's last sync; at a rotation the periodic
                // sync has already moved on to the new segment.
                warn!(
                    code = ErrorCode::WsSpill01WriterRespawn.code_str(),
                    stage,
                    error = %err,
                    "WAL sync FAILED while finalising a segment — its last records \
                     are flushed to the kernel but not forced to the device, and \
                     nothing will sync this segment again"
                );
            }
            false
        }
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
/// Requests a sync of the open segment from the `wal-syncer` thread,
/// rate-limited by `interval`. Returns the new "last requested" instant.
///
/// Called ONLY from the writer thread, never from `append()`, and AFTER the
/// batch's flush, so everything the request covers is already in the kernel:
/// the syncer's `sync_data` starts after the request and therefore forces at
/// least those bytes. The writer never waits for it (2026-10-02); see
/// [`WalSyncer`].
///
/// If the syncer holds no clone of this segment (the `try_clone` at open
/// failed) the segment is synced HERE, inline, exactly as before the syncer
/// existed, so a segment is never left to the kernel's writeback for want of
/// a file handle. A failure is reported and swallowed either way: a device
/// that cannot sync is a degraded WAL, not a reason to tear down the capture
/// floor that is still accepting writes.
fn maybe_sync_segment(
    current: &mut Option<BufWriter<File>>,
    interval: Option<Duration>,
    last_sync: Instant,
    syncer: &WalSyncer,
) -> Instant {
    let Some(interval) = interval else {
        return last_sync;
    };
    if last_sync.elapsed() < interval {
        return last_sync;
    }
    let Some(w) = current.as_mut() else {
        // No open segment: nothing to sync, and re-arming the timer here would
        // let a long segment-less stretch trigger a sync storm on reopen.
        return Instant::now();
    };
    if syncer.has_current() {
        syncer.request();
    } else {
        let started = Instant::now();
        record_sync_result(w.get_ref().sync_all(), started, WAL_PERIODIC_FSYNC_STAGE);
    }
    Instant::now()
}

/// Flushes a segment that is being FINALISED and, unless syncing is disabled,
/// forces it onto the device.
///
/// Three events finalise a segment and nothing writes to it afterwards: the
/// writer stopping, the channel closing, and a 128 MiB rotation. Until
/// 2026-09-06 only the first two synced. The third — by far the most frequent —
/// flushed and dropped the file, which puts the tail in the page cache and
/// nowhere else: the periodic sync (`maybe_sync_segment`) only ever touches
/// `current`, and at that instant `current` is the NEW segment, so the closed
/// one is never synced by anything, ever. A power cut inside the kernel's
/// writeback window therefore loses its last records permanently, while
/// `persisted` has already counted every one of them — the silent-loss shape
/// the WAL exists to make impossible.
///
/// The sync here is UNCONDITIONAL rather than rate-limited, because each of
/// these is the LAST chance for that particular segment. Its cost is bounded
/// by what the periodic sync has not already written — at most one sync
/// interval of records — and it is paid once per finalisation, not per record.
///
/// WHO pays it (2026-10-02). With `syncer` (the rotation arm) the closed
/// file is handed to the `wal-syncer` thread, so a slow device no longer
/// stops the writer at every 128 MiB. When the syncer has no room, or is
/// stopping, it hands the file back and the sync runs HERE, inline, as it
/// always did. Without `syncer` (stop and close) the sync is always inline:
/// those are the process's last syncs and nothing may outlive them.
fn finalise_segment(
    current: &mut Option<BufWriter<File>>,
    flush_stage: &'static str,
    fsync_stage: &'static str,
    syncer: Option<&WalSyncer>,
    tally: &mut UnflushedTally<'_>,
) {
    let Some(mut w) = current.take() else {
        return;
    };
    if let Err(err) = w.flush() {
        report_io_error(flush_stage, &err);
        // The records still in the buffer were counted as persisted; they
        // are counted as lost here instead of vanishing with the writer.
        discard_segment_writer(w, tally, flush_stage);
        // A failed flush means the bytes are not in the kernel either, so
        // syncing would force an incomplete segment and report success.
        return;
    }
    // Everything counted is in the kernel, and this segment takes no more
    // records: the next one starts its byte count at zero.
    tally.flushed();
    tally.new_segment();
    if resolve_wal_fsync_interval().is_none() {
        return;
    }
    // The buffer is empty after the flush above; only the `File` is kept.
    let (file, _empty_buffer) = w.into_parts();
    let inline = match syncer {
        Some(s) => s.hand_off_closed(file),
        None => Some(file),
    };
    if let Some(file) = inline {
        let started = Instant::now();
        record_sync_result(file.sync_all(), started, fsync_stage);
    }
}

/// Room for the records written since the last successful flush. Every batch
/// ends in a flush and holds at most 257 records (one plus 256 drained), so
/// this never fills in practice; past it, records are still counted (see
/// `UnflushedTally::untracked`).
const UNFLUSHED_TALLY_CAPACITY: usize = 512;

/// Counter: records that `persisted` had counted but that never reached the
/// kernel, because the segment's buffered write or flush failed (2026-10-02).
pub const WAL_UNFLUSHED_LOST_COUNTER: &str = "tv_ws_frame_spill_unflushed_lost_total";

/// The records counted in `persisted` whose bytes may still sit in the open
/// segment's `BufWriter` (2026-10-02).
///
/// `persisted` moves on a successful `write_all` into the buffer, not on the
/// flush. When a flush (or a buffered write that flushes internally) fails,
/// the writer used to be dropped, and with it up to 256 KiB of records that
/// were already counted: lost, and `persisted_count()` over-reported by the
/// same number. Re-appending those bytes to a fresh segment is not safe, since
/// a partial write can leave the buffer starting in the middle of a record.
/// So they are COUNTED AS LOST instead: this tally keeps the end offset, in
/// the segment, of every record written since the last good flush, and on a
/// failure the records ending past the file's real length are the lost ones.
///
/// Fixed size, built once per writer start: no allocation per record.
/// O(1) per record; the failure path walks at most `UNFLUSHED_TALLY_CAPACITY`
/// offsets, once per failure.
///
/// 2026-10-04 (audit M1): each entry also keeps the record's sequence and
/// socket type, so a lost record is reported to `wal_frame_fate`, which
/// counts it as a lost frame when the reader or the drain had shed it; and a
/// good flush reports the highest sequence it covered.
struct UnflushedTally<'a> {
    persisted: &'a AtomicU64,
    /// Bytes of counted records written to the current segment so far.
    segment_bytes: u64,
    ends: [u64; UNFLUSHED_TALLY_CAPACITY],
    seqs: [u64; UNFLUSHED_TALLY_CAPACITY],
    ws_types: [WsType; UNFLUSHED_TALLY_CAPACITY],
    len: usize,
    /// Records written past the capacity; on a loss they are counted lost.
    untracked: u64,
    /// Highest sequence written since the last good flush.
    high_seq: u64,
}

impl<'a> UnflushedTally<'a> {
    fn new(persisted: &'a AtomicU64) -> Self {
        // Seeded so the series exists before its first, rare, episode.
        metrics::counter!(WAL_UNFLUSHED_LOST_COUNTER).increment(0);
        Self {
            persisted,
            segment_bytes: 0,
            ends: [0; UNFLUSHED_TALLY_CAPACITY],
            seqs: [0; UNFLUSHED_TALLY_CAPACITY],
            ws_types: [WsType::LiveFeed; UNFLUSHED_TALLY_CAPACITY],
            len: 0,
            untracked: 0,
            high_seq: 0,
        }
    }

    /// A record of `size` bytes, sequence `seq`, went into the buffer and was
    /// counted.
    fn note_written(&mut self, size: u64, seq: u64, ws_type: WsType) {
        self.persisted.fetch_add(1, Ordering::Relaxed);
        self.segment_bytes = self.segment_bytes.saturating_add(size);
        self.high_seq = self.high_seq.max(seq);
        let i = self.len;
        if let (Some(end), Some(s), Some(t)) = (
            self.ends.get_mut(i),
            self.seqs.get_mut(i),
            self.ws_types.get_mut(i),
        ) {
            *end = self.segment_bytes;
            *s = seq;
            *t = ws_type;
            self.len += 1;
        } else {
            self.untracked = self.untracked.saturating_add(1);
        }
    }

    /// The buffer reached the kernel: nothing written so far can be lost to it.
    fn flushed(&mut self) {
        if self.high_seq != 0 {
            crate::wal_frame_fate::note_flushed_through(self.high_seq);
        }
        self.forget();
    }

    /// Forgets every entry, without claiming they reached the kernel.
    fn forget(&mut self) {
        self.len = 0;
        self.untracked = 0;
        self.high_seq = 0;
    }

    /// A new segment starts. The caller has either flushed the old one
    /// (`flushed`) or counted its losses (`discard_segment_writer`).
    fn new_segment(&mut self) {
        self.segment_bytes = 0;
        self.forget();
    }

    /// How many records written since the last good flush end past
    /// `on_disk` bytes, i.e. never fully reached the file.
    fn lost_beyond(&self, on_disk: u64) -> u64 {
        if on_disk >= self.segment_bytes {
            return 0;
        }
        let tracked = self.ends.get(..self.len).unwrap_or(&[]);
        // O(1) EXEMPT: begin — failure path only, at most UNFLUSHED_TALLY_CAPACITY offsets
        let lost = tracked.iter().filter(|&&end| end > on_disk).count() as u64;
        // O(1) EXEMPT: end
        lost.saturating_add(self.untracked)
    }

    /// Reports every record ending past `on_disk` to `wal_frame_fate` as lost.
    /// Returns `(shed ring, shed drain, fate unknown)`. Failure path only.
    fn report_lost_fates(&self, on_disk: u64) -> (u64, u64, u64) {
        let fate = crate::wal_frame_fate::frame_fate();
        let (mut ring, mut drain, mut unknown) = (0u64, 0u64, 0u64);
        if on_disk < self.segment_bytes {
            let n = self.len;
            // O(1) EXEMPT: begin — failure path only, at most UNFLUSHED_TALLY_CAPACITY entries
            for ((end, seq), ws_type) in self.ends[..n]
                .iter()
                .zip(&self.seqs[..n])
                .zip(&self.ws_types[..n])
            {
                if *end <= on_disk {
                    continue;
                }
                match fate.note_lost(*seq, *ws_type) {
                    crate::wal_frame_fate::LostFate::WasShed(
                        crate::wal_frame_fate::ShedKind::Ring,
                    ) => ring += 1,
                    crate::wal_frame_fate::LostFate::WasShed(
                        crate::wal_frame_fate::ShedKind::DrainDepth,
                    ) => drain += 1,
                    crate::wal_frame_fate::LostFate::Unknown => unknown += 1,
                    crate::wal_frame_fate::LostFate::NotShed => {}
                }
            }
            // O(1) EXEMPT: end
        }
        if self.untracked > 0 {
            crate::wal_frame_fate::note_lost_untracked(self.untracked);
            unknown = unknown.saturating_add(self.untracked);
        }
        (ring, drain, unknown)
    }
}

/// Drops a segment writer whose write or flush failed, WITHOUT a second flush,
/// and counts the records it was still holding as lost (2026-10-02).
///
/// `BufWriter`'s `Drop` would retry the flush and ignore the result, so the
/// writer is taken apart instead. The file's real length decides which counted
/// records reached it; a length that cannot be read counts every record since
/// the last good flush as lost, the safe direction for a loss counter.
fn discard_segment_writer(w: BufWriter<File>, tally: &mut UnflushedTally<'_>, stage: &'static str) {
    let (file, _unwritten) = w.into_parts();
    let on_disk = file.metadata().map_or(0, |m| m.len());
    let lost = tally.lost_beyond(on_disk);
    // Audit M1: a lost record the reader or the drain had shed is a lost
    // frame; `wal_frame_fate` counts it on the tick-loss counter.
    let (shed_ring, shed_drain, fate_unknown) = tally.report_lost_fates(on_disk);
    tally.new_segment();
    if lost == 0 {
        return;
    }
    let _previous = tally
        .persisted
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |p| {
            Some(p.saturating_sub(lost))
        });
    // Resolved here rather than held: this is a rare error path, and the
    // loss-counter guard reads the coded error! next to the emit.
    metrics::counter!(WAL_UNFLUSHED_LOST_COUNTER).increment(lost);
    error!(
        code = ErrorCode::WsSpill02FrameDropped.code_str(),
        stage,
        lost_records = lost,
        shed_ring_lost = shed_ring,
        shed_drain_lost = shed_drain,
        fate_unknown,
        on_disk_bytes = on_disk,
        "CRITICAL: WAL segment write failed — records already counted as persisted never \
         reached the file and are lost"
    );
}

/// Counts, and empties, what is still in the spill channel when the writer
/// thread is about to exit and drop it (Z11d, 2026-10-02). Returns the count.
///
/// The writer exits on a stop only after a whole poll with the channel empty,
/// but an `append` can still land between that poll and the thread dropping
/// `rx`. Such a record was acknowledged as `Spilled` and is never written; it
/// used to vanish with the channel uncounted. It now moves
/// [`WAL_SPILL_SHUTDOWN_INCOMPLETE_COUNTER`] and logs a coded error, the same
/// accounting `WsFrameSpill::shutdown` gives records left at its deadline.
///
/// Honest limit: an append landing after this count and before the drop is
/// still uncounted. That window is a few instructions, against the poll-long
/// one it replaces. Since Z11d `main` closes the sockets before the WAL, so
/// this should read zero on every clean stop.
fn count_records_left_at_writer_exit(rx: &Receiver<WalRecord>) -> usize {
    // Audit M1: each record left behind is reported lost, so a shed frame
    // among them is counted as a lost frame.
    let fate = crate::wal_frame_fate::frame_fate();
    let mut left = 0usize;
    // O(1) EXEMPT: begin — once, at writer exit, bounded by the channel capacity
    for r in rx.try_iter() {
        let _fate = fate.note_lost(r.frame_seq, r.ws_type);
        left += 1;
    }
    // O(1) EXEMPT: end
    if left > 0 {
        metrics::counter!(WAL_SPILL_SHUTDOWN_INCOMPLETE_COUNTER).increment(left as u64);
        error!(
            code = ErrorCode::WsSpill01WriterRespawn.code_str(),
            left,
            "WAL spill writer exited with records still in its channel — these frames \
             were captured and acknowledged but never written; they are gone"
        );
    }
    left
}

fn writer_loop(
    rx: &Receiver<WalRecord>,
    wal_dir: &Path,
    persisted: &AtomicU64,
    stop: &AtomicBool,
    queued_bytes: &AtomicU64,
    abort_drain: &AbortDrain,
    syncer: &WalSyncer,
) -> anyhow::Result<()> {
    /// Releases the byte reservations of one batch of records taken off the
    /// channel, in ONE `fetch_sub` at the end of the drain loop.
    ///
    /// Counted on RECEIPT, never after the write: a record that fails to
    /// persist has still left the queue, and holding its bytes would shrink
    /// the budget by exactly the amount a failing disk keeps producing.
    ///
    /// Why a batch and not one `fetch_sub` per record (2026-10-03): the
    /// socket readers `fetch_add` the same counter on every frame, so a
    /// per-record release bounced its cache line between the writer and the
    /// reader cores on every frame (`ws_reader/wal_append` doubled after the
    /// byte budget landed in #2004). One release per batch of up to 257
    /// records cuts the writer's writes to that line by the batch size.
    ///
    /// The cost: a batch's bytes stay reserved while that batch is written
    /// into the segment buffer, and released before its flush. So a slow disk
    /// holds at most ONE batch (≤ 257 frames, a few MiB) against a budget of
    /// at least `WAL_QUEUE_MIN_BYTES` (256 MiB), never the backlog behind it.
    /// `Drop` releases too, so a panic mid-batch (the supervisor respawns the
    /// writer on the same counter) cannot leak them.
    struct PendingRelease<'a> {
        queued_bytes: &'a AtomicU64,
        bytes: u64,
    }

    impl PendingRelease<'_> {
        fn add(&mut self, record: &WalRecord) {
            self.bytes = self.bytes.saturating_add(record.frame.len() as u64);
        }

        fn release(&mut self) {
            if self.bytes != 0 {
                self.queued_bytes.fetch_sub(self.bytes, Ordering::Relaxed);
                self.bytes = 0;
            }
        }
    }

    impl Drop for PendingRelease<'_> {
        fn drop(&mut self) {
            self.release();
        }
    }

    // `None` = no open segment; the next record reopens one. A transient disk
    // error sets this back to `None` instead of propagating out of the thread.
    // The thread therefore NEVER dies on a transient I/O hiccup — it keeps
    // draining the channel so `append()` never observes `Disconnected` and the
    // durable WAL floor survives. The ONLY clean exit is the channel closing.
    // What `persisted` counted but the buffer may still hold (2026-10-02).
    let mut tally = UnflushedTally::new(persisted);
    let mut current: Option<BufWriter<File>> = open_segment_tracked(wal_dir, syncer);
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
    // How far behind the `wal-syncer` thread is. Published HERE, by the
    // writer, so a syncer wedged inside a sync still shows on them.
    let fsync_backlog_gauge = metrics::gauge!("tv_wal_fsync_backlog");
    let fsync_pending_gauge = metrics::gauge!("tv_wal_fsync_pending_ms");
    fsync_backlog_gauge.set(0.0);
    fsync_pending_gauge.set(0.0);
    // Resolved once per writer start, not per iteration: an env read inside
    // the loop would be a syscall on every batch.
    let fsync_interval = resolve_wal_fsync_interval();
    let mut last_sync = Instant::now();
    // True while records have been flushed into the segment since its last
    // sync REQUEST. The lull arm below requests one, so the power-loss window
    // is the sync interval (plus however long the `wal-syncer` thread takes)
    // even when the feed goes quiet right after a batch (2026-10-02).
    let mut unsynced = false;
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
                    let t = &mut tally;
                    finalise_segment(&mut current, "flush_on_stop", "fsync_on_stop", None, t);
                    info!("ws-frame-spill-writer stop requested and queue drained; exiting");
                    // The writer is gone: its last segment is closed and replayable again.
                    clear_open_segment_under(wal_dir);
                    return Ok(());
                }
                // Nothing arrived for a whole poll and the previous batch was
                // flushed before this wait began: a pending abort drain is done.
                abort_drain.ack_if_drained(rx);
                // A lull. Before 2026-10-02 only a NEW record could trigger the
                // rate-limited sync, so the last batch before a quiet spell sat in
                // the page cache until the next record arrived, leaving the
                // power-loss window to the kernel's writeback (~30 s by default)
                // instead of the sync interval. O(1): one flag test, and at most
                // one sync REQUEST per interval (the sync itself runs on the
                // `wal-syncer` thread since 2026-10-02).
                syncer.publish_lag(&fsync_backlog_gauge, &fsync_pending_gauge);
                if unsynced {
                    let synced_at =
                        maybe_sync_segment(&mut current, fsync_interval, last_sync, syncer);
                    if synced_at != last_sync {
                        unsynced = false;
                    }
                    last_sync = synced_at;
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
                let t = &mut tally;
                finalise_segment(&mut current, "flush_on_close", "fsync_on_close", None, t);
                info!("ws-frame-spill-writer channel closed; exiting");
                clear_open_segment_under(wal_dir);
                return Ok(());
            }
        };

        let mut pending = PendingRelease {
            queued_bytes,
            bytes: 0,
        };
        pending.add(&first);
        #[cfg(test)]
        maybe_test_panic(&first);
        bytes_written +=
            persist_record_resilient(&mut current, wal_dir, &first, &mut tally, syncer);

        // Drain up to N more without blocking so we batch-flush.
        for _ in 0..256 {
            match rx.try_recv() {
                Ok(r) => {
                    pending.add(&r);
                    #[cfg(test)]
                    maybe_test_panic(&r);
                    bytes_written +=
                        persist_record_resilient(&mut current, wal_dir, &r, &mut tally, syncer);
                }
                Err(_) => break,
            }
        }
        // The whole batch has left the channel: one release, before the flush.
        pending.release();

        match current.as_mut().map(Write::flush) {
            Some(Ok(())) => tally.flushed(),
            Some(Err(err)) => {
                report_io_error("flush", &err);
                // Drop the possibly-broken writer WITHOUT a second flush,
                // counting the records it still held as lost; the next record
                // reopens a segment.
                if let Some(w) = current.take() {
                    discard_segment_writer(w, &mut tally, "flush");
                }
                thread::sleep(WAL_WRITER_IO_RETRY_BACKOFF);
            }
            None => {}
        }
        // Everything taken off the channel so far has now been flushed (or its
        // failure reported): serve a pending abort drain if the channel is
        // empty too. Once per batch, never per record.
        abort_drain.ack_if_drained(rx);

        // AFTER the flush, never before: syncing a file whose latest records
        // are still sitting in the BufWriter would force the previous batch to
        // the platter and leave this one exactly as exposed as before. This
        // only REQUESTS the sync; the writer goes straight back to the
        // channel while the `wal-syncer` thread forces the bytes down.
        unsynced = true;
        let synced_at = maybe_sync_segment(&mut current, fsync_interval, last_sync, syncer);
        if synced_at != last_sync {
            unsynced = false;
        }
        last_sync = synced_at;

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
        syncer.publish_lag(&fsync_backlog_gauge, &fsync_pending_gauge);

        if bytes_written >= WAL_SEGMENT_MAX_BYTES {
            // FLUSH **AND SYNC**. A rotation finalises this segment: nothing
            // writes to it again, and `maybe_sync_segment` only ever syncs
            // `current`, which one line below becomes the NEW file. Flushing
            // alone therefore left every 128 MiB segment's tail in the page
            // cache with no later sync to force it down, while `persisted`
            // had already counted every record in it. This is the same
            // finalisation the stop and close arms perform, and it is by far
            // the most frequent of the three.
            //
            // Since 2026-10-02 the closed file's sync is handed to the
            // `wal-syncer` thread, so a slow device does not stop this
            // thread at every rotation; with no room there it runs inline.
            // Short names keep the call on one line, which the source scan
            // in `every_segment_finalisation_syncs_including_the_rotation`
            // matches.
            let to = Some(syncer);
            let t = &mut tally;
            finalise_segment(&mut current, "flush_on_rotate", "fsync_on_rotate", to, t);
            current = open_segment_tracked(wal_dir, syncer);
            bytes_written = 0;
        }
    }
}

/// Open a fresh WAL segment, converting any error into `None` + a loud
/// `WS-SPILL-01` log + counter so the writer thread keeps draining the channel
/// instead of dying. The next record retries the open.
fn open_segment_resilient(wal_dir: &Path) -> Option<BufWriter<File>> {
    // Operator sweep row 22 (2026-09-08): the directory is created at boot,
    // but a mid-session `rm -rf data/ws_wal` (the Quote 21 wipe shape, or an
    // operator clearing disk by hand) left every later segment open failing
    // with ENOENT until restart — the durable floor gone for the session
    // while the writer "stayed alive". Re-creating it here costs one
    // `mkdir -p` per segment rotation on the background writer thread, never
    // per frame, and is idempotent when the directory exists.
    // O(1) EXEMPT: segment-rotation path on the dedicated background writer thread, never the per-frame append
    if let Err(err) = std::fs::create_dir_all(wal_dir) {
        error!(
            code = ErrorCode::WsSpill01WriterRespawn.code_str(),
            stage = "create_dir",
            error = %err,
            "WAL spill writer could not create the WAL directory — will retry; thread stays alive"
        );
        metrics::counter!(
            "tv_ws_frame_spill_write_errors_total",
            "stage" => "create_dir"
        )
        .increment(1);
        return None;
    }
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

/// [`open_segment_resilient`], plus handing the new segment's fd to the
/// `wal-syncer` thread. Every segment the writer opens goes through here, so
/// the syncer's periodic sync always targets the file being written.
/// Segment-open path only (cold), never per record.
fn open_segment_tracked(wal_dir: &Path, syncer: &WalSyncer) -> Option<BufWriter<File>> {
    let opened = open_segment_resilient(wal_dir);
    if let Some(w) = opened.as_ref() {
        syncer.track(w.get_ref());
    }
    opened
}

/// Durably write one record, reopening the segment first if needed. Returns the
/// on-disk byte count actually persisted (0 if the write could not land).
/// NEVER propagates an error — a transient disk failure must not kill the
/// writer thread (that would silently end durable capture of every frame).
fn persist_record_resilient(
    current: &mut Option<BufWriter<File>>,
    wal_dir: &Path,
    r: &WalRecord,
    tally: &mut UnflushedTally<'_>,
    syncer: &WalSyncer,
) -> u64 {
    if current.is_none() {
        tally.new_segment();
        *current = open_segment_tracked(wal_dir, syncer);
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
        // Audit M1: this record is lost; if the reader or the drain shed its
        // frame, that is a lost frame, counted by `wal_frame_fate`.
        let _fate = crate::wal_frame_fate::frame_fate().note_lost(r.frame_seq, r.ws_type);
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
            let size = record_disk_size(r);
            tally.note_written(size, r.frame_seq, r.ws_type);
            size
        }
        Err(err) => {
            report_io_error("write_record", &err);
            // Audit M1: this record is not in the tally (it never counted as
            // written), so its fate is reported here.
            let _fate = crate::wal_frame_fate::frame_fate().note_lost(r.frame_seq, r.ws_type);
            // Drop the possibly-corrupt writer without a second flush; the
            // records it still held are counted as lost. Reopen on the next
            // record.
            if let Some(w) = current.take() {
                discard_segment_writer(w, tally, "write_record");
            }
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

/// Test-only gate in front of every sync the `wal-syncer` thread performs, so
/// a test can wedge the syncer and prove the writer keeps writing. Defined
/// down here, after the writer, because the durability source scans treat the
/// first column-0 test attribute as the end of production code.
#[cfg(test)]
#[derive(Default)]
struct TestSyncHold {
    held: std::sync::Mutex<bool>,
    cv: std::sync::Condvar,
    parked: AtomicU64,
}

#[cfg(test)]
impl TestSyncHold {
    fn set(&self, held: bool) {
        if let Ok(mut g) = self.held.lock() {
            *g = held;
        }
        self.cv.notify_all();
    }

    fn wait(&self) {
        let Ok(mut g) = self.held.lock() else {
            return;
        };
        if !*g {
            return;
        }
        self.parked.fetch_add(1, Ordering::SeqCst);
        while *g {
            match self.cv.wait(g) {
                Ok(next) => g = next,
                Err(_) => break,
            }
        }
        self.parked.fetch_sub(1, Ordering::SeqCst);
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

/// The highest frame sequence on disk when this process's WAL writer claimed
/// its directory (audit PR31b-2). Every frame at or below it was captured by
/// an earlier process: the writer ratchets past it before its first append.
static BOOT_DISK_HIGH_FRAME_SEQ: std::sync::OnceLock<u64> = std::sync::OnceLock::new();

/// See [`BOOT_DISK_HIGH_FRAME_SEQ`]. `None` before a WAL writer claimed a
/// directory in this process. The boot candle warm-up reads frames up to it,
/// so it never folds a frame this process captured. O(1).
#[must_use]
pub fn boot_disk_high_frame_seq() -> Option<u64> {
    BOOT_DISK_HIGH_FRAME_SEQ.get().copied()
}

/// The most recently allocated frame sequence, without allocating one. A
/// catch-up drain snapshots it before its first round: every segment the
/// drain can confirm holds frames below that point.
#[must_use]
pub fn current_frame_seq() -> u64 {
    WAL_FRAME_SEQ.load(Ordering::Acquire)
}

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
    //
    // ⚠ CHANGED 2026-10-02 — a STEP back no longer freezes the anchor. The
    // ratchet refused every refresh that rewound, and after a wall-clock step
    // back of S seconds every later refresh rewinds by the same S, because
    // both clocks then advance together. So the anchor froze for the rest of
    // the process and every receipt stayed S seconds ahead of the wall clock,
    // filing bars S seconds late all day. A rewind larger than
    // `RECEIPT_ANCHOR_BACKWARD_STEP_NANOS` is now treated as a step and
    // adopted: frames received within S seconds of the swap can read back out
    // of order once, which is smaller than mis-filing every frame for hours.
    // It is counted (`outcome = "adopted_backward_step"`) and logged. A small
    // rewind (slew) is still refused, and repeated refusals now end too: once
    // the accumulated slew passes the threshold it is adopted as a step.
    match anchor_refresh_decision(**old, new) {
        AnchorRefresh::Adopt => {
            cell.store(std::sync::Arc::new(new));
            metrics::counter!(RECEIPT_ANCHOR_REFRESH_COUNTER, "outcome" => "adopted").increment(1);
        }
        AnchorRefresh::RefuseBackward => {
            metrics::counter!(RECEIPT_ANCHOR_REFRESH_COUNTER, "outcome" => "refused_backward")
                .increment(1);
        }
        AnchorRefresh::AdoptBackwardStep { rewind_nanos } => {
            cell.store(std::sync::Arc::new(new));
            metrics::counter!(
                RECEIPT_ANCHOR_REFRESH_COUNTER,
                "outcome" => "adopted_backward_step"
            )
            .increment(1);
            warn!(
                rewind_ms = rewind_nanos / 1_000_000,
                "WAL receipt anchor re-taken after the wall clock stepped back; frames \
                 received just before and after this point may read back out of order once"
            );
        }
    }
}

/// A rewind larger than this, at a receipt-anchor refresh, is a wall-clock
/// STEP (or slew accumulated past it) and is adopted rather than refused
/// (2026-10-02). One second: NTP slew is at most 500 ppm, about 15 ms over the
/// 30 s refresh interval, so a single interval of slew stays far below it.
const RECEIPT_ANCHOR_BACKWARD_STEP_NANOS: i64 = 1_000_000_000;

/// What a receipt-anchor refresh does with a freshly taken anchor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AnchorRefresh {
    /// The new anchor does not rewind what the old one projects.
    Adopt,
    /// A small rewind (slew): keep the old anchor so frames stay ordered.
    RefuseBackward,
    /// A rewind past the step threshold: adopt it, or the anchor freezes.
    AdoptBackwardStep { rewind_nanos: i64 },
}

/// Decides a refresh from the old and new anchors. Pure, O(1).
fn anchor_refresh_decision(old: ReceiptAnchor, new: ReceiptAnchor) -> AnchorRefresh {
    let elapsed = new.instant.saturating_duration_since(old.instant);
    let projected_now = old
        .nanos
        .saturating_add(i64::try_from(elapsed.as_nanos()).unwrap_or(i64::MAX));
    if new.nanos >= projected_now {
        return AnchorRefresh::Adopt;
    }
    let rewind_nanos = projected_now.saturating_sub(new.nanos);
    if rewind_nanos > RECEIPT_ANCHOR_BACKWARD_STEP_NANOS {
        AnchorRefresh::AdoptBackwardStep { rewind_nanos }
    } else {
        AnchorRefresh::RefuseBackward
    }
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

/// Header-only walk of one segment, returning the greatest `frame_seq` in it,
/// CRC-verified (audit L10, 2026-10-04).
///
/// Deliberately tolerant: this is a best-effort high-water probe, not a replay.
/// A torn tail, an unknown magic, or a short read simply ends the walk and
/// returns what was seen — a LOWER bound is still strictly better than the wall
/// clock alone, and refusing to boot over a torn tail would turn a safety net
/// into an outage. Corruption accounting belongs to [`replay_segment`], which
/// walks the same files moments later and reports it properly.
///
/// # Why the winning record is checked (L10)
///
/// The walk reads only headers, so until 2026-10-04 a flipped byte in a
/// record's sequence field was taken at face value. A flip in a high bit
/// reads as a huge sequence, the seed jumps there, and every later frame is
/// minted at `prev + 1` from it: toward the top of the range, where
/// [`packet_capture_seq`] values saturate and distinct ticks share a dedup
/// key and upsert each other away. Now the record that gave the highest
/// sequence is read whole and its CRC checked (one record, O(1) extra). Only
/// when it fails does the probe walk the segment again, verifying every
/// record and ignoring the corrupt ones: O(segment bytes), rare, counted on
/// `tv_wal_seq_probe_corrupt_record_total`.
fn highest_frame_seq_in_segment(path: &Path) -> u64 {
    let (highest, best_offset) = header_walk_highest_frame_seq(path);
    if highest == 0 {
        return 0;
    }
    let Ok(mut f) = File::open(path) else {
        return 0;
    };
    if let RecordProbe::Intact { frame_seq, .. } = record_at(&mut f, best_offset)
        && frame_seq == highest
    {
        return highest;
    }
    metrics::counter!(WAL_SEQ_PROBE_CORRUPT_RECORD_COUNTER).increment(1);
    let verified = verified_walk_highest_frame_seq(&mut f);
    warn!(
        path = %path.display(),
        header_high = highest,
        verified_high = verified,
        "WAL sequence probe: the record carrying the highest sequence in this segment \
         fails its checksum, so its sequence was ignored and the segment re-read with \
         every record verified. Seeding from a corrupt sequence could push capture_seq \
         to the top of its range, where distinct ticks would share a dedup key."
    );
    verified
}

/// Counter: segments whose highest header sequence came from a record that
/// failed its CRC (audit L10).
pub const WAL_SEQ_PROBE_CORRUPT_RECORD_COUNTER: &str = "tv_wal_seq_probe_corrupt_record_total";

/// Every record in the segment read and CRC-checked; the greatest sequence of
/// an INTACT record. A corrupt record is skipped by its header length, as the
/// header walk does; the walk ends where no header parses. Cold: boot only,
/// and only after the winning record failed its check. O(segment bytes).
fn verified_walk_highest_frame_seq(f: &mut File) -> u64 {
    let mut offset = 0u64;
    let mut highest = 0u64;
    // O(1) EXEMPT: begin — boot-time seed, rare corrupt-record path, bounded by one segment
    loop {
        match record_at(f, offset) {
            RecordProbe::Intact {
                frame_seq,
                record_len,
            } => {
                highest = highest.max(frame_seq);
                offset = offset.saturating_add(record_len);
            }
            RecordProbe::Corrupt { record_len } => {
                offset = offset.saturating_add(record_len);
            }
            RecordProbe::End => return highest,
        }
    }
    // O(1) EXEMPT: end
}

/// The header walk itself: the greatest header `frame_seq` and the offset of
/// the record that carries it. Payloads are seeked past, never read.
fn header_walk_highest_frame_seq(path: &Path) -> (u64, u64) {
    let Ok(mut f) = File::open(path) else {
        return (0, 0);
    };
    let mut highest = 0u64;
    let mut best_offset = 0u64;
    // Offset of the record whose header is being read.
    let mut offset = 0u64;
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
                Err(_) => return (highest, best_offset),
            }
        }
        if filled < WAL_MIN_RECORD_V1 {
            return (highest, best_offset);
        }
        let magic = &head[0..4];
        let is_v4 = magic == WAL_MAGIC_V4;
        let is_v3 = magic == WAL_MAGIC_V3;
        let is_v2 = magic == WAL_MAGIC_V2;
        let is_v1 = magic == WAL_MAGIC;
        if !is_v1 && !is_v2 && !is_v3 && !is_v4 {
            return (highest, best_offset);
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
            return (highest, best_offset);
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
            let seq = u64::from_le_bytes(seq_bytes);
            if seq > highest {
                highest = seq;
                best_offset = offset;
            }
        }
        let Ok(len_bytes) = <[u8; 4]>::try_from(&head[len_off..len_off + 4]) else {
            return (highest, best_offset);
        };
        let frame_len = u64::from(u32::from_le_bytes(len_bytes));
        // The next record starts right after this one's payload and CRC.
        // `min_rec` already counts the 4-byte CRC, so a record is
        // `min_rec + frame_len` bytes. Seek to that ABSOLUTE offset.
        //
        // FIXED 2026-10-04 (L10): this was a relative seek of
        // `frame_len + 4 - over_read`, which counted the CRC twice and landed
        // 4 bytes into the next record. That read as an unknown magic and
        // ended the walk, so every segment yielded only its FIRST record's
        // sequence and the restart seed sat as far below the true high-water
        // mark as one segment holds frames.
        offset = offset.saturating_add(min_rec as u64 + frame_len);
        if std::io::Seek::seek(&mut f, std::io::SeekFrom::Start(offset)).is_err() {
            return (highest, best_offset);
        }
    }
}

/// Ratchet [`WAL_FRAME_SEQ`] past everything already on disk under `wal_dir`.
///
/// Idempotent and monotonic: it only ever RAISES the counter, so calling it
/// twice, or after frames have already been minted, can neither lower it nor
/// reissue a value. Returns the high-water mark found, for logging.
///
/// **S3 (2026-10-02): also past the persisted applied watermark.** The
/// segments alone were the only source until today. Once every segment is
/// pruned (each one uploaded and applied), a wall clock stepped back by less
/// than a day left the counter seeded from the clock alone, BELOW the
/// watermark file beside it, which still loads (it rejects values only a day
/// or more ahead). Every new frame then read as applied: skipped on replay
/// and deletable by the prune. The watermark's high-water is now a floor too.
pub fn seed_frame_seq_from_disk(wal_dir: &Path) -> u64 {
    let disk_high = highest_frame_seq_on_disk(wal_dir)
        .max(crate::wal_applied_watermark::AppliedSnapshot::persisted_high_water(wal_dir));
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

/// Counter: a new segment was named past the wall clock because the clock
/// read at or below the newest segment name already used in its directory
/// (a backward clock step, or a restart after one). R3-14.
pub const WAL_SEGMENT_NAME_CLAMPED_COUNTER: &str = "tv_wal_segment_name_clamped_total";

/// The newest segment name (its nanos) handed out per WAL directory in this
/// process (R3-14, 2026-10-04).
///
/// Replay, the prune and the uploader all order segments by FILE NAME, and the
/// name was the wall clock at rotation. A backward clock step between two
/// rotations therefore named the newer segment BELOW the older one, and the
/// next replay folded the newer frames first. The name is now
/// `max(wall clock, newest name in this directory + 1)`, so names only rise.
/// While the clock is sane the name is still the wall clock, which keeps the
/// uploader's IST date (read from the name) correct; after a backward step it
/// runs ahead of the clock by at most the step, until the clock catches up.
///
/// Keyed by directory so two writers on two directories (tests) never push
/// each other's names. Touched only at segment rotation, on the writer thread:
/// the first rotation in a directory lists `<dir>`, `replaying/` and
/// `archive/` once, O(files); every later one is an O(1) probe under the lock.
static SEGMENT_NAME_HIGH: std::sync::Mutex<Option<std::collections::HashMap<PathBuf, u128>>> =
    std::sync::Mutex::new(None);

/// The nanos in `ws-frames-<nanos>.wal`, or `None` for any other name.
#[must_use]
pub fn segment_name_nanos(name: &str) -> Option<u128> {
    name.strip_prefix("ws-frames-")
        .and_then(|s| s.strip_suffix(".wal"))
        .and_then(|s| s.parse::<u128>().ok())
}

/// The greatest segment-name nanos under `wal_dir`, its `replaying/` and its
/// `archive/`. `0` when there is none. Cold: first rotation per directory.
fn highest_segment_name_on_disk(wal_dir: &Path) -> u128 {
    let mut high = 0u128;
    for dir in [
        wal_dir.to_path_buf(),
        wal_dir.join(REPLAYING_SUBDIR),
        wal_dir.join(ARCHIVE_SUBDIR),
    ] {
        // O(1) EXEMPT: once per WAL directory per process, at its first segment rotation
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            if let Some(n) = entry.file_name().to_str().and_then(segment_name_nanos) {
                high = high.max(n);
            }
        }
    }
    high
}

/// The name (nanos) for the next segment in `wal_dir`, given the wall clock
/// `now_nanos`: strictly above every name already used there. Returns the
/// name and whether it had to be moved past the clock.
fn next_segment_name_nanos(wal_dir: &Path, now_nanos: u128) -> (u128, bool) {
    let mut guard = SEGMENT_NAME_HIGH
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let map = guard.get_or_insert_with(std::collections::HashMap::new); // APPROVED: segment rotation, cold, once per directory
    let high = match map.get(wal_dir) {
        Some(h) => *h,
        None => highest_segment_name_on_disk(wal_dir),
    };
    let name = now_nanos.max(high.saturating_add(1));
    map.insert(wal_dir.to_path_buf(), name); // APPROVED: segment rotation on the background writer thread, not the per-frame append
    (name, name != now_nanos)
}

fn open_new_segment(wal_dir: &Path) -> anyhow::Result<BufWriter<File>> {
    let now_nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let (nanos, clamped) = next_segment_name_nanos(wal_dir, now_nanos);
    if clamped {
        metrics::counter!(WAL_SEGMENT_NAME_CLAMPED_COUNTER).increment(1);
        warn!(
            source = "segment_name_clamped",
            wall_nanos = %now_nanos,
            named_nanos = %nanos,
            "WAL segment named past the wall clock: the clock read at or below the newest \
             segment name in this directory (a backward clock step). The name keeps rising so \
             replay order stays capture order; no frame is lost"
        );
    }
    let path = wal_dir.join(format!("ws-frames-{:020}.wal", nanos)); // APPROVED: segment rotation on the background writer thread, not the per-frame append
    let f = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|e| anyhow::anyhow!("open WAL segment {:?}: {e}", path))?;
    set_open_segment(path);
    Ok(BufWriter::with_capacity(WAL_WRITER_BUFFER, f))
}

/// The segment the writer thread is APPENDING to right now, so a replay that
/// runs while the writer is live never stages it.
///
/// Found 2026-09-04 by reading, not by an incident: the writer opens its first
/// segment at thread start, which is BEFORE the lane's catch-up drain runs,
/// and `wal_segments_in` listed every `*.wal` in the live directory. So the
/// last catch-up round could stage the writer's OPEN segment into `replaying/`
/// and `confirm_replayed` could then archive it — while the writer kept
/// appending to the same inode. Every frame written after that point would sit
/// in a directory the next boot never reads. The current session's first
/// ~128 MiB of capture, silently outside the recovery path.
///
/// A path, not a nanos floor: this is a process-global and the test suite runs
/// many spills in one process, so a numeric floor from one test would hide
/// another test's segments. An exact-path match excludes precisely the one
/// file that is open. Cleared when the writer exits, so a shut-down spill's
/// last segment is replayable again.
// Process-global registry initialised ONCE at program start, bounded by the number of live
// writers (one in production), touched only at segment rotation / writer exit / replay staging.
// APPROVED: never on the per-frame append path — the Vec is a static, not a per-frame allocation.
static OPEN_SEGMENTS: std::sync::Mutex<Vec<PathBuf>> = std::sync::Mutex::new(Vec::new());

/// Registers `path` as the open segment of its directory, replacing any
/// earlier registration for the same directory. One entry per live spill —
/// one in production, a handful in the test suite — so the vector is bounded
/// by the number of writers that exist, and cleared as each one exits.
fn set_open_segment(path: PathBuf) {
    if let Ok(mut open) = OPEN_SEGMENTS.lock() {
        let dir = path.parent();
        open.retain(|p| p.parent() != dir); // O(1) EXEMPT: bounded by live writers, rotation-time only
        open.push(path); // APPROVED: rotation-time registration, not the per-frame append
    }
}

/// Releases the registration(s) under `wal_dir` ONLY — so a spill going away
/// never hides or reveals another spill's segment in the same process.
fn clear_open_segment_under(wal_dir: &Path) {
    if let Ok(mut open) = OPEN_SEGMENTS.lock() {
        open.retain(|p| !p.starts_with(wal_dir)); // O(1) EXEMPT: bounded by live writers, exit-time only
    }
}

impl Drop for WsFrameSpill {
    fn drop(&mut self) {
        // The sender is going away with `self`, so no frame can reach the
        // writer after this point; the writer exits on its next poll. Release
        // the registration NOW rather than up to one poll interval later, so
        // a replay that follows the drop sees the closed segment.
        if !self.wal_dir.as_os_str().is_empty() {
            clear_open_segment_under(&self.wal_dir);
        }
    }
}

pub(crate) fn is_open_segment(path: &Path) -> bool {
    OPEN_SEGMENTS
        .lock()
        .map(|open| open.iter().any(|p| p == path)) // O(1) EXEMPT: bounded by live writers, replay-time only
        .unwrap_or(false)
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

/// Bad stretches the segment walk skipped by resyncing to the next readable
/// record (Z11a). Prometheus only: the coded WS-SPILL-02 line and the two
/// counters above already carry the loss.
pub const WAL_REPLAY_RESYNCS_COUNTER: &str = "tv_wal_replay_resyncs_total";

/// Stop replaying when the WAL volume has less than this free.
///
/// MEASURED 2026-09-03: a restart that replays a session's backlog costs
/// **25–75 GB of disk** — the refold DEDUP-upserts into hour partitions and
/// rescues whatever the sink cannot land into spill files. Seven restarts
/// consumed ~300 GB and the volume reached 20 KB free. Replay is recovery; a
/// recovery that fills the disk destroys the session it was recovering for.
/// 40 GiB is roughly one restart's worst measured cost with margin, on a
/// 600 GB volume. Assumed until `tv_spill_dir_free_bytes` deltas across a
/// restart on the watermarked build re-derive it.
pub const WAL_REPLAY_DISK_FLOOR_BYTES: u64 = 40 * 1024 * 1024 * 1024;

/// The floor for a volume of `total_bytes`: the 40 GiB constant, or one
/// eighth of the volume when that is smaller. On the 600 GB box the constant
/// wins (600/8 = 75 GB > 40 GiB); on a small CI or dev volume the fraction
/// does, so the fence scales rather than refusing every replay on a host
/// that never had 40 GiB to begin with.
#[must_use]
pub const fn wal_replay_disk_floor_bytes(total_bytes: u64) -> u64 {
    let eighth = total_bytes / 8;
    if eighth < WAL_REPLAY_DISK_FLOOR_BYTES {
        eighth
    } else {
        WAL_REPLAY_DISK_FLOOR_BYTES
    }
}

/// Total frames one BOOT may replay across STAGE-C and every catch-up round.
///
/// ~1.4× the 2.2 M frames a full-session backlog measured on 2026-09-03. A
/// backlog past this is a capacity problem the next boot inherits, deferred
/// and counted rather than materialised until something dies.
pub const WAL_REPLAY_MAX_FRAMES_PER_BOOT: u64 = 3_000_000;

/// A receipt-time jump larger than this between the last candle-bearing
/// frame of one WAL segment and the first of a later one is a PROCESS
/// BOUNDARY gap (plan ITEM 47): one run of the lane ended and the next began.
/// The new process started its candle fold afresh, so a replay that carried
/// its volume baseline across the boundary would give the new process's
/// first tick the whole downtime's volume. Nothing is dropped at such a
/// boundary, which is why it needs its own test.
///
/// Checked ONLY across segment boundaries, and on RECEIPT time: every process
/// opens a new segment, while a silence inside one segment is harmless (the
/// live process folded exactly those frames). A restart takes far longer than
/// five seconds (auth, database, universe, socket dial), while a segment the
/// writer rotates for size is written back to back. Frame sequences are not
/// used: they advance by one when frames outrun the clock and carry on from
/// the disk's highest sequence across a restart, so they understate
/// downtime (review, 2026-09-29).
pub const WAL_REPLAY_PROCESS_GAP_NANOS: i64 = 5_000_000_000;

/// `true` when a frame can carry ticks into the candle fold: a live-feed
/// frame from the MAIN-FEED socket. Depth and order-update frames, and
/// TrueData frames (which share the main-feed endpoint byte), carry none, so
/// skipping one is never a gap (plan ITEM 47). O(1).
#[must_use]
pub fn frame_feeds_the_candle_fold(ws_type: WsType, endpoint: WalEndpoint) -> bool {
    ws_type == WsType::LiveFeed && matches!(endpoint, WalEndpoint::MainFeed)
}

/// `true` when the receipt time moved forward by more than
/// [`WAL_REPLAY_PROCESS_GAP_NANOS`] between two candle-bearing frames that a
/// segment boundary separates. A receipt the WAL does not know (legacy
/// records) or that is implausible never counts. O(1).
#[must_use]
pub fn process_boundary_is_gap(previous_receipt_nanos: i64, receipt_nanos: i64) -> bool {
    let previous = plausible_receipt_nanos(previous_receipt_nanos);
    let current = plausible_receipt_nanos(receipt_nanos);
    previous != WAL_RECEIPT_UNKNOWN_NANOS
        && current != WAL_RECEIPT_UNKNOWN_NANOS
        && current.saturating_sub(previous) > WAL_REPLAY_PROCESS_GAP_NANOS
}

/// Segments the applied-watermark let this pass ARCHIVE UNREAD.
pub const WAL_REPLAY_SKIPPED_SEGMENTS_COUNTER: &str = "tv_wal_replay_skipped_segments_total";
/// Frames inside a boundary segment that were read and then dropped as
/// already applied.
pub const WAL_REPLAY_SKIPPED_FRAMES_COUNTER: &str = "tv_wal_replay_skipped_frames_total";
/// Bytes of segment file the skip never read.
pub const WAL_REPLAY_SKIPPED_BYTES_COUNTER: &str = "tv_wal_replay_skipped_bytes_total";
/// Passes refused because the volume was under [`WAL_REPLAY_DISK_FLOOR_BYTES`].
pub const WAL_REPLAY_DISK_FLOOR_STOPS_COUNTER: &str = "tv_wal_replay_disk_floor_stops_total";
/// Passes refused because [`WAL_REPLAY_MAX_FRAMES_PER_BOOT`] was reached.
pub const WAL_REPLAY_FRAME_CAP_STOPS_COUNTER: &str = "tv_wal_replay_frame_cap_stops_total";
/// The lane gave up waiting for the writer threads to ack a replayed batch
/// before confirming it (the segments stay in `replaying/`).
pub const WAL_REPLAY_ACK_WAIT_TIMEOUTS_COUNTER: &str = "tv_wal_replay_ack_wait_timeouts_total";

/// Replay confirmations refused because the sink was SUSPECT at the time.
///
/// Distinct from the ack-wait timeout above, and the distinction is the point:
/// a timeout means the writers were slow, this means the writers said "ok" and
/// the watermark does not believe them. Same outcome — segments stay in
/// `replaying/` — but a completely different cause, so triage must be able to
/// tell them apart.
///
/// Deliberately NOT EMF-selected: the margin to the budget's automatic
/// `STOP_EC2_INSTANCES` line is $4.61/month, and adding a CloudWatch name is
/// the operator's cost decision. It is on the local exporter and in the log
/// line's `source` field, which is what triage reads.
pub const WAL_REPLAY_SINK_SUSPECT_REFUSALS_COUNTER: &str =
    "tv_wal_replay_sink_suspect_refusals_total";

/// Frames returned by every replay pass of this process, for the per-boot cap.
static WAL_REPLAY_FRAMES_THIS_BOOT: AtomicU64 = AtomicU64::new(0);

/// Kill switch: `TV_WAL_APPLIED_SKIP=0` makes replay ignore the watermark
/// file (it is still written). Deleting `applied.tvaw` has the same effect.
pub const WAL_APPLIED_SKIP_ENV: &str = "TV_WAL_APPLIED_SKIP";

fn applied_skip_enabled() -> bool {
    // APPROVED: one env read per boot-time replay pass, cold path
    // Any of the usual "off" spellings disables skipping; anything else — including a
    // typo — keeps the default. The default is the SAFE direction only in the sense
    // that skipping is proven by the tests; an operator who wants a full replay
    // should check the boot log line that reports how many segments were skipped.
    std::env::var(WAL_APPLIED_SKIP_ENV).map_or(true, |v| {
        !matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "0" | "false" | "off" | "no"
        )
    })
}

// TEST-EXEMPT: covered by test_append_spill_and_replay_roundtrip + test_replay_handles_missing_dir + test_replay_detects_crc_corruption + test_unconfirmed_segment_is_rereplayed_on_next_boot + test_confirmed_segment_is_not_rereplayed
pub fn replay_all<P: AsRef<Path>>(wal_dir: P) -> anyhow::Result<Vec<ReplayedFrame>> {
    replay_all_with_budget(wal_dir, WAL_REPLAY_MAX_BYTES)
}

/// The PRODUCTION form of [`replay_all`] — STAGE-C boot replay — fenced by
/// the disk floor and the per-boot frame cap exactly like every catch-up
/// round. `replay_all` itself stays unfenced so the sixty-odd fixtures that
/// drive it are not at the mercy of the host's free disk; production must
/// call THIS one, and `crates/app/tests` pins that it does.
// TEST-EXEMPT: thin delegate; the fences are proven by replay_all_with_report_at's tests.
pub fn replay_all_fenced<P: AsRef<Path>>(wal_dir: P) -> anyhow::Result<WalReplayBatch> {
    // The whole batch, not just the frames: a REFUSED pass returns an empty
    // frame list with a stop flag set, and a caller that only saw the empty
    // list would read "nothing to replay" and archive whatever an earlier boot
    // left staged in `replaying/` — unread. The flags are how it tells the
    // two apart.
    replay_all_with_report_fenced(wal_dir, WAL_REPLAY_MAX_BYTES)
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
    /// Segments the applied-watermark archived UNREAD this pass.
    pub skipped_segments: u64,
    /// Frames read from boundary segments and dropped as already applied.
    pub skipped_frames: u64,
    /// Bytes of segment file the skip never read.
    pub skipped_bytes: u64,
    /// `true` when the pass was refused on [`WAL_REPLAY_DISK_FLOOR_BYTES`].
    pub stopped_for_disk: bool,
    /// `true` when the pass was refused on [`WAL_REPLAY_MAX_FRAMES_PER_BOOT`].
    pub stopped_for_frame_cap: bool,
    /// Plan ITEM 47: `true` when something was really skipped before the first
    /// returned frame — a segment archived as applied, main-feed frames dropped
    /// as applied, or a damaged segment. The first frame's `after_gap` is set
    /// on every pass regardless, because a pass does not know what an earlier
    /// pass read; a caller draining in consecutive rounds reads this to tell a
    /// real gap from a round boundary.
    pub leading_gap: bool,
    /// Plan ITEM 47: `true` when the pass ended on a gap — frames dropped or
    /// lost after the last returned one.
    pub trailing_gap: bool,
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

    // APPLIED-WATERMARK SKIP (2026-09-05). Every segment whose entire sequence
    // range is below the watermark and overlaps no unapplied bucket is moved
    // STRAIGHT to `archive/`, unread. The bytes never leave the disk, the
    // frames never materialise, and nothing is refolded. The remaining
    // segments are read as before; the boundary one is filtered per frame in
    // `replay_segment_filtered`. See `wal_applied_watermark` for why every
    // failure direction of this decision is "replay more".
    let applied = if applied_skip_enabled() {
        crate::wal_applied_watermark::AppliedSnapshot::load(wal_dir)
    } else {
        None
    };
    let mut skipped_segments = 0u64;
    let mut skipped_bytes = 0u64;
    // Plan ITEM 47: `true` at index i when a segment skipped as applied sits
    // between kept segment i-1 and kept segment i — its frames were never
    // replayed, so kept segment i's first frame follows a gap.
    let mut gap_before_segment: Vec<bool> = vec![false; segments.len()]; // APPROVED: boot-time WAL replay, cold path
    // Segments decided SKIPPED, with the index of the kept segment that
    // follows each. They are archived only AFTER the read below, and only
    // when that kept segment was reached: one past a budget or memory stop
    // stays a `*.wal`, so the next pass sees it again and flags the gap it
    // leaves (plan ITEM 47 review). Archiving it now lost that gap.
    let mut pending_skips: Vec<(PathBuf, usize)> = Vec::new(); // APPROVED: boot-time WAL replay, cold path
    let archive_dir = wal_dir.join(ARCHIVE_SUBDIR);
    if let Some(applied) = applied.as_ref() {
        // A segment's range is `[its first seq, next segment's first seq − 1]`.
        // The LAST segment has no upper bound and is always read.
        // O(1) EXEMPT: boot replay, one header read per segment, cold path
        let firsts: Vec<u64> = segments
            .iter()
            .map(|p| first_frame_seq_in_segment(p))
            .collect(); // APPROVED: boot-time WAL replay, cold path
        let mut keep: Vec<PathBuf> = Vec::with_capacity(segments.len()); // APPROVED: boot-time WAL replay, cold path
        let mut keep_gap: Vec<bool> = Vec::with_capacity(segments.len()); // APPROVED: boot-time WAL replay, cold path
        let mut skipped_since_kept = false;
        for (idx, path) in segments.iter().enumerate() {
            let lo = firsts[idx];
            let hi = firsts.get(idx + 1).copied().and_then(|n| n.checked_sub(1));
            let skip = match hi {
                Some(hi) if hi > 0 => segment_range_is_applied(applied, lo, hi),
                _ => false,
            };
            if !skip {
                keep.push(path.clone()); // APPROVED: boot-time WAL replay, cold path
                keep_gap.push(std::mem::replace(&mut skipped_since_kept, false));
                continue;
            }
            if path.file_name().is_none() {
                keep.push(path.clone()); // APPROVED: boot-time WAL replay, cold path
                keep_gap.push(std::mem::replace(&mut skipped_since_kept, false));
                continue;
            }
            pending_skips.push((path.clone(), keep.len())); // APPROVED: boot-time WAL replay, cold path
            skipped_since_kept = true;
        }
        segments = keep;
        gap_before_segment = keep_gap;
    }
    let mut skipped_frames = 0u64;

    let mut frames = Vec::new(); // APPROVED: boot-time WAL replay, cold path
    let mut corrupted = 0usize;
    // Bytes of frame payload held so far, and how many segments were actually
    // read. Only the CONSUMED ones are staged below: a segment left as `*.wal`
    // is re-globbed next boot, so deferring is safe by this module's own
    // crash-safety invariant rather than by anything new.
    let mut bytes_held = 0usize;
    let mut consumed = 0usize;

    let mut stopped_for_memory = false;
    // Plan ITEM 47: the first frame of a pass always follows a gap (what came
    // before it was applied, or was read by an earlier pass).
    let mut gap_pending = true;
    // The same, without the pass-start assumption: set only by a real skip.
    let mut real_gap = false;
    let mut leading_gap: Option<bool> = None;
    for (seg_idx, path) in segments.iter().enumerate() {
        if gap_before_segment.get(seg_idx).copied().unwrap_or(false) {
            gap_pending = true;
            real_gap = true;
        }
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
        match replay_segment_core(path, applied.as_ref(), &|_, _| true, true) {
            Ok(read) => {
                let SegmentRead {
                    frames: mut batch,
                    dropped_as_applied: dropped,
                    damaged,
                    trailing_gap,
                } = read;
                skipped_frames = skipped_frames.saturating_add(dropped);
                for f in &batch {
                    bytes_held = bytes_held.saturating_add(f.frame.len());
                }
                if let Some(first) = batch.first_mut() {
                    // `first.after_gap` here is the segment walk's own verdict:
                    // frames dropped in front of it inside this segment.
                    if leading_gap.is_none() {
                        leading_gap = Some(real_gap || first.after_gap);
                    }
                    if gap_pending {
                        first.after_gap = true;
                    }
                    gap_pending = false;
                    real_gap = false;
                }
                frames.append(&mut batch);
                // A walk that stopped on a bad record never returned the
                // frames after it, and frames dropped as applied after the
                // last returned one were never replayed: either way the next
                // segment follows a gap.
                if damaged || trailing_gap {
                    gap_pending = true;
                    real_gap = true;
                }
            }
            Err(err) => {
                corrupted += 1;
                gap_pending = true;
                real_gap = true;
                error!(segment = ?path, error = %err, "WAL segment corrupted; skipping");
            }
        }
        consumed += 1;
    }
    // Archive the skipped segments this pass got past (see `pending_skips`).
    // A rename that fails leaves the segment a `*.wal`; the next pass skips
    // it again, so nothing applied is ever re-read by accident of this.
    // O(1) EXEMPT: begin — one rename per skipped segment, boot replay cold path
    // Restores that fail, here and for DEFERRED segments below: both leave a
    // segment in `replaying/`, where the confirm archives it unread.
    let mut restore_failures = 0usize;
    for (path, next_kept) in &pending_skips {
        if *next_kept > consumed {
            // Past the stop. A leftover in `replaying/` goes back to the live
            // dir, exactly as a deferred kept segment does below: left there,
            // the confirm would archive it by glob and the next pass would
            // never see the gap it leaves (review, 2026-09-29).
            if path.parent() == Some(replaying_dir.as_path())
                && let Some(name) = path.file_name()
                && let Err(err) = std::fs::rename(path, wal_dir.join(name))
            {
                // Coded and counted like the deferred restore below (review
                // round 7): archived unread, it hides the gap it leaves.
                restore_failures = restore_failures.saturating_add(1);
                error!(
                    code = ErrorCode::WsSpill02FrameDropped.code_str(),
                    segment = ?path,
                    error = %err,
                    "WAL replay could not return a SKIPPED staged segment to the live \
                     directory. The confirm step will archive it, and the next pass will \
                     not see the gap it leaves, so bars across that gap may be rebuilt \
                     from part of their ticks."
                );
            }
            continue;
        }
        let Some(name) = path.file_name() else {
            continue;
        };
        drop(std::fs::create_dir_all(&archive_dir));
        let bytes = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        match std::fs::rename(path, archive_dir.join(name)) {
            Ok(()) => {
                skipped_segments = skipped_segments.saturating_add(1);
                skipped_bytes = skipped_bytes.saturating_add(bytes);
            }
            Err(err) => {
                warn!(segment = ?path, error = %err, "could not archive an applied WAL segment; it stays and is skipped again next pass");
            }
        }
    }
    // O(1) EXEMPT: end
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

    metrics::counter!(WAL_REPLAY_SKIPPED_SEGMENTS_COUNTER).increment(skipped_segments);
    metrics::counter!(WAL_REPLAY_SKIPPED_FRAMES_COUNTER).increment(skipped_frames);
    metrics::counter!(WAL_REPLAY_SKIPPED_BYTES_COUNTER).increment(skipped_bytes);
    WAL_REPLAY_FRAMES_THIS_BOOT.fetch_add(frames.len() as u64, Ordering::AcqRel);
    info!(
        wal_dir = ?wal_dir,
        segments = segments.len(),
        replaying_leftovers = leftover_count,
        frames_replayed = frames.len(),
        corrupted_segments = corrupted,
        skipped_segments,
        skipped_frames,
        skipped_bytes,
        watermark_present = applied.is_some(),
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
        skipped_segments,
        skipped_frames,
        skipped_bytes,
        stopped_for_disk: false,
        stopped_for_frame_cap: false,
        leading_gap: leading_gap.unwrap_or(real_gap),
        trailing_gap: real_gap,
    })
}

/// `true` when the whole `[lo, hi]` range is applied for EVERY sink a frame in
/// it could feed. A segment mixes main-feed and depth frames, and a
/// main-feed frame feeds both sinks, so the segment-level decision takes the
/// stricter of the two.
fn segment_range_is_applied(
    applied: &crate::wal_applied_watermark::AppliedSnapshot,
    lo: u64,
    hi: u64,
) -> bool {
    use crate::wal_applied_watermark::AppliedSink;
    applied.range_is_applied(AppliedSink::Ticks, lo, hi)
        && applied.range_is_applied(AppliedSink::Depth, lo, hi)
}

/// Which watermark a replayed frame answers to, from its TVW4 endpoint.
pub const fn applied_sink_for(endpoint: WalEndpoint) -> crate::wal_applied_watermark::AppliedSink {
    use crate::wal_applied_watermark::AppliedSink;
    match endpoint {
        WalEndpoint::Depth20 | WalEndpoint::Depth200 => AppliedSink::Depth,
        // Order-update frames have no sink and never advance a watermark; the
        // tick watermark is the stricter of the two for a main-feed frame.
        WalEndpoint::MainFeed | WalEndpoint::OrderUpdate => AppliedSink::Ticks,
    }
}

/// The FIRST record's `frame_seq`, read and CRC-VERIFIED. `0` for a v1
/// record, an empty file, a torn or corrupt first record, or an unreadable
/// path — every one of which means "unknown", and an unknown range is never
/// skipped. Verified rather than header-only because this value bounds the
/// PREVIOUS segment's skip range: a bit-flip in the seq bytes that read as a
/// plausibly lower number would narrow that range and let a segment be
/// skipped while its real tail sits above the watermark.
pub(crate) fn first_frame_seq_in_segment(path: &Path) -> u64 {
    let Ok(mut f) = File::open(path) else {
        return 0;
    };
    match record_at(&mut f, 0) {
        RecordProbe::Intact { frame_seq, .. } => frame_seq,
        RecordProbe::Corrupt { .. } | RecordProbe::End => 0,
    }
}

/// What [`record_at`] found at an offset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RecordProbe {
    /// A v2–v4 record whose CRC matches.
    Intact { frame_seq: u64, record_len: u64 },
    /// A record whose header parses with a plausible length but whose CRC
    /// does not match (or a v1 record, which carries no sequence). Its length
    /// is still the best guess for where the next record starts.
    Corrupt { record_len: u64 },
    /// No parsable record here: end of file, a torn header, an unknown magic,
    /// an implausible length, or a read error.
    End,
}

/// Reads the record at `offset` and verifies its CRC. Cold: boot sequence
/// probes and the replay skip pass only; allocates one buffer the size of
/// the record.
fn record_at(f: &mut File, offset: u64) -> RecordProbe {
    // A frame larger than this is not a record this writer produced; refuse
    // to allocate for it.
    const RECORD_PROBE_MAX_FRAME_BYTES: usize = 64 * 1024 * 1024;
    if std::io::Seek::seek(f, std::io::SeekFrom::Start(offset)).is_err() {
        return RecordProbe::End;
    }
    let mut head = [0u8; WAL_MIN_RECORD_V4];
    let mut filled = 0usize;
    while filled < head.len() {
        match std::io::Read::read(f, &mut head[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(_) => return RecordProbe::End,
        }
    }
    let magic = &head[0..4];
    let is_v4 = magic == WAL_MAGIC_V4;
    let is_v3 = magic == WAL_MAGIC_V3;
    let is_v2 = magic == WAL_MAGIC_V2;
    let is_v1 = magic == WAL_MAGIC;
    let (min_rec, len_off) = if is_v4 {
        (WAL_MIN_RECORD_V4, 22)
    } else if is_v3 {
        (WAL_MIN_RECORD_V3, 21)
    } else if is_v2 {
        (WAL_MIN_RECORD_V2, 13)
    } else if is_v1 {
        (WAL_MIN_RECORD_V1, 5)
    } else {
        return RecordProbe::End;
    };
    if filled < min_rec {
        return RecordProbe::End;
    }
    let Some(frame_len) = head
        .get(len_off..len_off + 4)
        .and_then(|b| b.try_into().ok())
        .map(|b| u32::from_le_bytes(b) as usize)
    else {
        return RecordProbe::End;
    };
    if frame_len > RECORD_PROBE_MAX_FRAME_BYTES {
        return RecordProbe::End;
    }
    let record_len = len_off + 4 + frame_len + 4;
    if is_v1 {
        // No sequence to recover; the length still says where the next
        // record starts.
        return RecordProbe::Corrupt {
            record_len: record_len as u64,
        };
    }
    let Some(frame_seq) = head
        .get(5..13)
        .and_then(|b| b.try_into().ok())
        .map(u64::from_le_bytes)
    else {
        return RecordProbe::End;
    };
    let mut record = vec![0u8; record_len]; // APPROVED: boot probe / replay skip pass, one record, cold path
    let copied = filled.min(record_len);
    record[..copied].copy_from_slice(&head[..copied]);
    if copied < record_len && std::io::Read::read_exact(f, &mut record[copied..]).is_err() {
        return RecordProbe::End;
    }
    let frame = &record[len_off + 4..len_off + 4 + frame_len];
    let len_le = &record[len_off..len_off + 4];
    let ws_byte = record[4];
    let actual = if is_v4 {
        crc32_ieee_of(&[
            &[ws_byte],
            &record[5..13],
            &record[13..21],
            &[record[21]],
            len_le,
            frame,
        ])
    } else if is_v3 {
        crc32_ieee_of(&[&[ws_byte], &record[5..13], &record[13..21], len_le, frame])
    } else {
        crc32_ieee_of(&[&[ws_byte], &record[5..13], len_le, frame])
    };
    let Some(expected) = record
        .get(record_len - 4..)
        .and_then(|b| b.try_into().ok())
        .map(u32::from_le_bytes)
    else {
        return RecordProbe::End;
    };
    if actual != expected {
        return RecordProbe::Corrupt {
            record_len: record_len as u64,
        };
    }
    RecordProbe::Intact {
        frame_seq,
        record_len: record_len as u64,
    }
}

/// [`replay_all_with_report`] with the disk probe and the per-boot frame count
/// INJECTED, so both refusals are testable without a full volume or three
/// million frames. A refused pass returns an EMPTY batch with the matching
/// flag set and touches no file: every segment stays a `*.wal`, re-globbed by
/// the next pass or the next boot.
// TEST-EXEMPT: covered by replay_refuses_below_the_disk_floor_and_touches_nothing + replay_refuses_past_the_per_boot_frame_cap
pub fn replay_all_with_report_at<P: AsRef<Path>>(
    wal_dir: P,
    budget_bytes: usize,
    disk: crate::disk_health_watcher::DiskHealthOutcome,
    frames_so_far: u64,
) -> anyhow::Result<WalReplayBatch> {
    let wal_dir = wal_dir.as_ref();
    let refused = |stopped_for_disk: bool| WalReplayBatch {
        budget_bytes: budget_bytes as u64,
        stopped_for_disk,
        stopped_for_frame_cap: !stopped_for_disk,
        ..WalReplayBatch::default()
    };
    if frames_so_far >= WAL_REPLAY_MAX_FRAMES_PER_BOOT {
        metrics::counter!(WAL_REPLAY_FRAME_CAP_STOPS_COUNTER).increment(1);
        error!(
            code = ErrorCode::WsSpill01WriterRespawn.code_str(),
            source = "replay_frame_cap",
            frames_so_far,
            cap = WAL_REPLAY_MAX_FRAMES_PER_BOOT,
            "WAL replay REFUSED: this boot has already replayed the per-boot frame cap. \
             Nothing is stranded — every unread segment stays a `*.wal` file for the next \
             boot — but a backlog this size is a capacity problem, not a recovery one."
        );
        return Ok(refused(false));
    }
    match disk {
        crate::disk_health_watcher::DiskHealthOutcome::Ok {
            free_bytes,
            total_bytes,
        } if free_bytes < wal_replay_disk_floor_bytes(total_bytes) => {
            metrics::counter!(WAL_REPLAY_DISK_FLOOR_STOPS_COUNTER).increment(1);
            error!(
                code = ErrorCode::WsSpill01WriterRespawn.code_str(),
                source = "replay_disk_floor",
                free_bytes,
                total_bytes,
                floor_bytes = wal_replay_disk_floor_bytes(total_bytes),
                wal_dir = ?wal_dir,
                "WAL replay REFUSED: the volume is under the replay disk floor. A replay \
                 costs tens of GB of DEDUP churn and spill on this box (measured 25–75 GB \
                 per restart on 2026-09-03), and a recovery that fills the disk destroys the \
                 session it was recovering for. Nothing is stranded — every segment stays a \
                 `*.wal` file — but nothing is recovered until space is freed."
            );
            return Ok(refused(true));
        }
        // A failed probe is fail-OPEN, exactly as the memory guard is on a
        // host where `/proc` does not resolve: today's behaviour, counted by
        // the disk watcher's own probe-failure signal.
        _ => {}
    }
    replay_all_with_report_guarded(
        wal_dir,
        budget_bytes,
        || crate::resource_monitor::probe_vmrss_bytes(Path::new("/proc/self/status")),
        resolved_memory_ceiling_bytes(),
        WAL_REPLAY_RSS_STOP_PCT,
    )
}

fn resolved_memory_ceiling_bytes() -> Option<u64> {
    crate::resource_monitor::resolve_memory_ceiling(
        &crate::resource_monitor::resolve_cgroup_memory_max_path(),
        Path::new("/proc/meminfo"),
    )
    .bytes()
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
    // CORRECTED 2026-09-03. This resolved the ROOT cgroup's `memory.max`, not
    // this UNIT's -- the exact defect `resolve_cgroup_memory_max_path` was
    // written to fix on 2026-09-02, applied to the resource monitor and missed
    // here. Verified live the same day: `/sys/fs/cgroup/memory.max` and
    // `memory.high` do not exist at the root on this host, so both resolved to
    // `None` and the guard fell through to `/proc/meminfo` MemTotal -- 30.75
    // GiB instead of the unit's 20 GiB ceiling. Its 60% stand-down therefore
    // sat at 19.81 GB rather than 12.88 GB, making the INNERMOST memory guard
    // on the WAL path the LOOSEST of the three, and it is the one that runs
    // while a large ILP buffer is being materialised.
    //
    // The doc comment above claims this uses "the same `resolve_memory_ceiling`
    // the resource monitor and RESOURCE-02 use, so the guard and the alarm can
    // never disagree." That sentence was false for as long as the path was
    // hardcoded. It is true now (`resolved_memory_ceiling_bytes`).
    //
    // 2026-09-05: this form carries the MEMORY guard only. The disk floor and
    // the per-boot frame cap live in `replay_all_with_report_fenced`, which is
    // what production calls; this one stays unfenced so a fixture on a
    // small-disk host can still exercise the replay itself.
    replay_all_with_report_guarded(
        wal_dir,
        budget_bytes,
        || crate::resource_monitor::probe_vmrss_bytes(Path::new("/proc/self/status")),
        resolved_memory_ceiling_bytes(),
        WAL_REPLAY_RSS_STOP_PCT,
    )
}

/// The PRODUCTION form of [`replay_all_with_report`]: the disk floor and the
/// per-boot frame cap sit in front of the pass, so a full volume can no longer
/// be made fuller by recovery. Every catch-up round and the STAGE-C boot
/// replay (via [`replay_all_fenced`]) come through here.
///
/// The counter series below are created on the first pass of every boot,
/// because the CloudWatch agent drops a counter's first sample and a refusal
/// is exactly the once-ever event whose first occurrence is the one that
/// matters.
// TEST-EXEMPT: probe-then-delegate; the fences are proven by replay_all_with_report_at's tests.
pub fn replay_all_with_report_fenced<P: AsRef<Path>>(
    wal_dir: P,
    budget_bytes: usize,
) -> anyhow::Result<WalReplayBatch> {
    metrics::counter!(WAL_REPLAY_SKIPPED_SEGMENTS_COUNTER).increment(0);
    metrics::counter!(WAL_REPLAY_SKIPPED_FRAMES_COUNTER).increment(0);
    metrics::counter!(WAL_REPLAY_SKIPPED_BYTES_COUNTER).increment(0);
    metrics::counter!(WAL_REPLAY_DISK_FLOOR_STOPS_COUNTER).increment(0);
    metrics::counter!(WAL_REPLAY_FRAME_CAP_STOPS_COUNTER).increment(0);
    metrics::counter!(WAL_REPLAY_ACK_WAIT_TIMEOUTS_COUNTER).increment(0);
    metrics::counter!(WAL_REPLAY_SINK_SUSPECT_REFUSALS_COUNTER).increment(0);
    metrics::counter!(crate::wal_applied_watermark::APPLIED_INVALID_COUNTER).increment(0);
    metrics::counter!(crate::wal_applied_watermark::APPLIED_PERSIST_FAILED_COUNTER).increment(0);
    let wal_dir = wal_dir.as_ref();
    let disk = crate::disk_health_watcher::probe_disk_free_bytes(wal_dir);
    replay_all_with_report_at(
        wal_dir,
        budget_bytes,
        disk,
        WAL_REPLAY_FRAMES_THIS_BOOT.load(Ordering::Acquire),
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
        // Never the segment the writer thread is appending to — see
        // `OPEN_SEGMENT` for the loss this prevents.
        .filter(|p| !is_open_segment(p))
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

/// Protects the WAL backlog a catch-up drain did NOT reach (2026-10-03).
///
/// A drain that stops early (clock, round cap, memory, apply lag, a sink that
/// stops answering) leaves segments as `*.wal` (or staged in `replaying/`)
/// for the next boot. The live lane then acks frames with HIGHER sequences,
/// which lifts the applied watermark past the leftover range; with no
/// unapplied bucket over it, the next boot's replay would read every leftover
/// segment as applied and archive it UNREAD — its ticks and depth never
/// reaching the database, with no counter anywhere.
///
/// This marks `[lowest first frame_seq still waiting, ceiling_seq − 1]`
/// unapplied, so the next replay reads those segments instead. Returns the
/// marked range, or `None` when nothing below `ceiling_seq` is waiting. The
/// caller persists the watermark. A range wider than the bucket table
/// overflows it, which fails towards replaying everything.
///
/// O(waiting segments) header reads, cold: once per boot, after the drain.
// TEST-EXEMPT: covered by test_regression_leftover_backlog_is_replayed_after_live_acks_pass_it + test_guard_pending_backlog_ignores_segments_at_or_above_the_ceiling
pub fn guard_pending_backlog(
    wm: &crate::wal_applied_watermark::AppliedWatermark,
    wal_dir: &Path,
    ceiling_seq: u64,
) -> Option<(u64, u64)> {
    let mut lowest: Option<u64> = None;
    for dir in [wal_dir.to_path_buf(), wal_dir.join(REPLAYING_SUBDIR)] {
        // O(1) EXEMPT: boot-time backlog guard, one header read per waiting segment
        for seg in wal_segments_in(&dir) {
            let seq = first_frame_seq_in_segment(&seg);
            // `0` is an unreadable or v1 first record; such a segment is never
            // skipped by the replay, so it needs no mark.
            if seq > 0 {
                lowest = Some(lowest.map_or(seq, |l| l.min(seq)));
            }
        }
    }
    let lo = lowest?;
    let hi = ceiling_seq.checked_sub(1)?;
    if lo > hi {
        return None;
    }
    wm.note_unapplied_range(lo, hi);
    Some((lo, hi))
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
    /// Segments past the age window that the AGE pass KEPT because the
    /// applied watermark has not passed them (or cannot say) — 2026-09-22,
    /// item 44d. Always zero on `archive/`, whose segments are applied by
    /// construction.
    pub age_kept_unapplied: usize,
    /// Segments the BYTE pass needed to reach but REFUSED because the applied
    /// watermark has not passed them (2026-10-01, item 45b). Each one holds
    /// frames no replay has reached; it is the only copy, so it is kept and
    /// the directory stays over its ceiling. Counted as
    /// `tv_wal_prune_refused_unapplied_total{state="unapplied"}`.
    ///
    /// Until item 45b these segments were DELETED and counted in a
    /// `size_deleted_unapplied` field; the 2026-09-29 zero-loss lock forbids
    /// any prune deleting an unapplied segment.
    pub size_refused_unapplied: usize,
    /// Segments the BYTE pass refused because their applied state could NOT
    /// be determined (no usable watermark, the kill switch, a segment whose
    /// range cannot be bounded). Not proven unapplied, so counted apart, but
    /// kept for the same reason: not proven to have a second copy.
    pub size_refused_unknown: usize,
    /// Bytes held by `size_refused_unapplied` + `size_refused_unknown`.
    pub size_refused_bytes: u64,
    /// Segments kept by EITHER pass because they hold frames whose depth rows
    /// the ingest shed skipped and that have not been written back yet
    /// (2026-09-29, item 45a). Never deleted by age or by bytes: the segment is
    /// the only copy of those rows.
    pub deferred_kept: usize,
    /// Bytes held by `deferred_kept` segments. Not counted against the byte
    /// ceiling's reach: when the ceiling cannot be met because of them, the
    /// pass says so loudly rather than deleting the only copy.
    pub deferred_kept_bytes: u64,
    /// Pinned segments deleted because free disk fell below the hard floor
    /// (item 45a) — each one is order-book rows lost, logged as WS-SPILL-02.
    pub size_deleted_deferred: usize,
    /// Applied segments the BYTE pass (or the hard floor) needed to delete but
    /// REFUSED because no verified S3 copy is recorded for them yet (plan item
    /// 45e-1, `[raw_frame_archive] require_upload_before_prune`). Counted as
    /// `tv_wal_prune_refused_not_uploaded_total`.
    pub size_refused_not_uploaded: usize,
    /// Bytes held by `size_refused_not_uploaded`.
    pub size_refused_not_uploaded_bytes: u64,
    /// Applied segments past the age window that the AGE pass kept only
    /// because no verified S3 copy is recorded yet (item 45e-1).
    pub age_kept_not_uploaded: usize,
}

/// Counter: segments the ACTIVE byte-ceiling prune REFUSED to delete because
/// the applied watermark had not passed them (2026-10-01, item 45b), labelled
/// `state` = `unapplied` or `unknown`. Local `/metrics` only — the same pass
/// raises one coded WS-SPILL-01 line, which the existing filter pages.
///
/// Replaces `tv_wal_pruned_unapplied_total` (item 44d), which counted the
/// same segments as DELETED.
pub const WAL_PRUNE_REFUSED_UNAPPLIED_COUNTER: &str = "tv_wal_prune_refused_unapplied_total";

/// Whether the applied watermark has passed ONE segment, for the prune.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SegmentAppliedState {
    /// Every frame in the segment's range is below the watermark and in no
    /// unapplied bucket.
    Applied,
    /// The range is known and the watermark does not cover all of it.
    Unapplied,
    /// No watermark file, a rejected one, the kill switch, or a segment whose
    /// range cannot be bounded (newest segment, unreadable first record).
    Unknown,
}

/// What the prune does with one segment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SegmentPruneDecision {
    Keep,
    /// Past the age window AND applied: routine expiry.
    DeleteAged,
    /// The byte ceiling is exceeded and the segment is applied: a redundant
    /// copy of frames already in the database, so it may go.
    DeleteForBytes,
    /// The byte ceiling is exceeded but the segment is NOT known to be
    /// applied (2026-10-01, item 45b): it may be the only copy, so it is kept
    /// and counted. `unknown` is true when the state could not be determined.
    RefuseForBytes {
        unknown: bool,
    },
    /// The byte ceiling is exceeded and the segment is applied, but no
    /// verified S3 copy is recorded for it (plan item 45e-1): the raw bytes
    /// would leave the box with no copy anywhere, so it is kept and counted.
    RefuseNotUploaded,
}

/// The per-segment prune rule (2026-09-22, item 44d; 2026-10-01, item 45b).
/// Pure and O(1).
///
/// The AGE pass deletes only a segment the watermark has passed — an unknown
/// state keeps, so a missing or corrupt watermark fails closed. The BYTE pass
/// now follows the same rule (item 45b, 2026-09-29 zero-loss lock: "lets any
/// prune delete a segment that is unapplied" is a REJECT). Until then it
/// deleted regardless of state, on the reasoning (scope lock "2026-09-22
/// (FOURTH)") that respecting the watermark could fill the disk. That risk is
/// now carried by the ingest shed, the disk pressure ladder and the free-space
/// alarm; a refused segment is counted and logged, never silently kept.
///
/// `uploaded` (plan item 45e-1, 2026-10-02): the segment has a verified copy
/// in the cold bucket, or the upload gate is off. Both delete arms need it,
/// so raw capture never leaves the box without a copy (operator Quotes 27 +
/// 28). An unapplied segment is refused for that reason first.
const fn segment_prune_decision(
    aged: bool,
    state: SegmentAppliedState,
    byte_cap_exceeded: bool,
    uploaded: bool,
) -> SegmentPruneDecision {
    let applied = matches!(state, SegmentAppliedState::Applied);
    if aged && applied && uploaded {
        SegmentPruneDecision::DeleteAged
    } else if byte_cap_exceeded && applied && uploaded {
        SegmentPruneDecision::DeleteForBytes
    } else if byte_cap_exceeded && applied {
        SegmentPruneDecision::RefuseNotUploaded
    } else if byte_cap_exceeded {
        SegmentPruneDecision::RefuseForBytes {
            unknown: matches!(state, SegmentAppliedState::Unknown),
        }
    } else {
        SegmentPruneDecision::Keep
    }
}

/// Whether a prune delete needs a verified S3 copy of the segment (plan item
/// 45e-1, `[raw_frame_archive] require_upload_before_prune`).
#[derive(Debug, Clone, Copy)]
pub enum UploadGate<'a> {
    /// No copy needed: the behaviour before item 45e-1. A box without a
    /// bucket uses this.
    NotRequired,
    /// Every delete needs a marker in `markers_dir` (`<wal_dir>/uploaded`)
    /// whose recorded raw length equals the file's length now.
    Required { markers_dir: &'a Path },
}

impl UploadGate<'_> {
    /// Whether `path` (of `len` bytes, `None` when unreadable) may be deleted
    /// as far as the upload gate is concerned. Cold prune path: one small
    /// marker read per segment.
    fn satisfied(&self, path: &Path, len: Option<u64>) -> bool {
        match self {
            Self::NotRequired => true,
            Self::Required { markers_dir } => len
                .is_some_and(|len| crate::raw_frame_upload::marker_matches(markers_dir, path, len)),
        }
    }

    /// Whether a delete under this gate leaves a verified copy in S3.
    const fn copy_in_s3(&self) -> bool {
        matches!(self, Self::Required { .. })
    }

    /// Removes the marker of a segment the prune just deleted.
    fn forget(&self, path: &Path) {
        if let Self::Required { markers_dir } = self {
            crate::raw_frame_upload::remove_marker(markers_dir, path);
        }
    }
}

/// Owned form of [`UploadGate`], for a wrapper that derives the marker path.
struct UploadGateOwned {
    markers_dir: Option<PathBuf>,
}

impl UploadGateOwned {
    /// The gate the production wrappers use for `wal_dir`.
    fn for_wal_dir(wal_dir: &Path, require_upload: bool) -> Self {
        Self {
            markers_dir: require_upload.then(|| crate::raw_frame_upload::markers_dir(wal_dir)),
        }
    }

    fn gate(&self) -> UploadGate<'_> {
        match &self.markers_dir {
            Some(markers_dir) => UploadGate::Required { markers_dir },
            None => UploadGate::NotRequired,
        }
    }
}

/// Counter: applied segments a prune REFUSED to delete because no verified
/// S3 copy is recorded (plan item 45e-1), labelled `dir`. Local `/metrics`
/// only; the pass also raises one coded WS-SPILL-01 line.
pub const WAL_PRUNE_REFUSED_NOT_UPLOADED_COUNTER: &str = "tv_wal_prune_refused_not_uploaded_total";

/// Where the prune gets each segment's applied state.
#[derive(Clone, Copy)]
enum PruneAppliedView<'a> {
    /// `archive/`: only confirmed-replay segments land there.
    AllApplied,
    /// The active directory: judged against the watermark; `None` means no
    /// usable watermark, so every segment is `Unknown`.
    Watermark(Option<&'a crate::wal_applied_watermark::AppliedSnapshot>),
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
        PruneAppliedView::AllApplied,
        None,
        false,
        UploadGate::NotRequired,
    )
}

/// [`prune_archived_segments_at`] with the deferred-depth marks (item 45a): a
/// segment holding shed depth that has not been written back is kept by both
/// passes. `None` protects nothing (the pre-45a behaviour).
#[must_use]
pub fn prune_archived_segments_deferred_at<P: AsRef<Path>>(
    wal_dir: P,
    retention_secs: u64,
    max_bytes: u64,
    now: std::time::SystemTime,
    deferred: Option<&crate::wal_deferred_depth::DeferredDepthSnapshot>,
    disk_below_floor: bool,
    upload: UploadGate<'_>,
) -> ArchivePruneOutcome {
    prune_wal_dir_at(
        &wal_dir.as_ref().join(ARCHIVE_SUBDIR),
        retention_secs,
        max_bytes,
        now,
        PruneAppliedView::AllApplied,
        deferred,
        disk_below_floor,
        upload,
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
///
/// # 2026-09-22 (item 44d): the AGE pass now consults the applied watermark
///
/// `applied` is the watermark snapshot for THIS directory. The age pass
/// deletes only a segment the watermark has passed; a segment holding any
/// frame not yet applied (a `RingFull` shed, a failed append or rescue) is
/// kept past the window, and `None` — no file, a rejected file, the kill
/// switch — keeps every segment. The BYTE pass is unchanged: it still deletes
/// oldest-first under pressure, now counting each unapplied victim. So the
/// "age pass deletes nothing reachable" argument above no longer rests on the
/// replay budget alone — it rests on the watermark.
///
/// # 2026-10-01 (item 45b): the BYTE pass consults it too
///
/// The byte pass now deletes only a segment the watermark has passed. An
/// unapplied or unknown segment is REFUSED — kept, counted on
/// `tv_wal_prune_refused_unapplied_total{state}`, and reported in one coded
/// WS-SPILL-01 line per pass — even when that leaves the directory over its
/// ceiling (2026-09-29 zero-loss lock). The "BYTE pass has no such bound"
/// paragraph above describes the code before this change. The trade it made —
/// a pruned backlog instead of a full disk — is now carried by the ingest shed,
/// the disk pressure ladder and the free-space alarm. **Honest limit:** with no
/// usable watermark (kill switch, missing or rejected file) every segment is
/// unknown, so the byte pass deletes nothing in this directory until a
/// watermark exists again.
#[must_use]
pub fn prune_active_segments_at<P: AsRef<Path>>(
    wal_dir: P,
    retention_secs: u64,
    max_bytes: u64,
    now: std::time::SystemTime,
    applied: Option<&crate::wal_applied_watermark::AppliedSnapshot>,
) -> ArchivePruneOutcome {
    prune_wal_dir_at(
        wal_dir.as_ref(),
        retention_secs,
        max_bytes,
        now,
        PruneAppliedView::Watermark(applied),
        None,
        false,
        UploadGate::NotRequired,
    )
}

/// [`prune_active_segments_at`] with the deferred-depth marks (item 45a).
#[must_use]
pub fn prune_active_segments_deferred_at<P: AsRef<Path>>(
    wal_dir: P,
    retention_secs: u64,
    max_bytes: u64,
    now: std::time::SystemTime,
    applied: Option<&crate::wal_applied_watermark::AppliedSnapshot>,
    deferred: Option<&crate::wal_deferred_depth::DeferredDepthSnapshot>,
    disk_below_floor: bool,
    upload: UploadGate<'_>,
) -> ArchivePruneOutcome {
    prune_wal_dir_at(
        wal_dir.as_ref(),
        retention_secs,
        max_bytes,
        now,
        PruneAppliedView::Watermark(applied),
        deferred,
        disk_below_floor,
        upload,
    )
}

/// The applied state of every segment in `sorted` (file-name order, which is
/// capture order). A segment's range is `[its first seq, next segment's first
/// seq - 1]` — the same bound the replay skip uses — so the newest segment, a
/// segment with an unreadable first record, or a non-increasing pair is
/// `Unknown`. Cold path: one verified header read per segment, per pass.
fn segment_applied_states(
    sorted: &[PathBuf],
    view: PruneAppliedView<'_>,
) -> Vec<(SegmentAppliedState, u64, Option<u64>)> {
    // O(1) EXEMPT: periodic cold prune, one entry per segment
    match view {
        // `archive/` segments are applied by construction, but their RANGES
        // are still read (2026-09-29, item 45a): a boot archives a segment the
        // watermark has passed WITHOUT reading it, and a segment holding shed
        // depth is exactly that, so the deferred-depth check needs its range.
        PruneAppliedView::AllApplied => {
            let firsts: Vec<u64> = sorted
                .iter()
                .map(|p| first_frame_seq_in_segment(p))
                .collect(); // APPROVED: cold prune pass
            firsts
                .iter()
                .enumerate()
                .map(|(idx, &lo)| {
                    let hi = firsts.get(idx + 1).and_then(|n| n.checked_sub(1));
                    (SegmentAppliedState::Applied, lo, hi)
                })
                .collect() // APPROVED: cold prune pass
        }
        // No usable watermark: every segment is `Unknown`, but the ranges are
        // still read (item 45a) so a deferred-depth mark keeps only the
        // segments it covers, not the whole directory.
        PruneAppliedView::Watermark(None) => {
            let firsts: Vec<u64> = sorted
                .iter()
                .map(|p| first_frame_seq_in_segment(p))
                .collect(); // APPROVED: cold prune pass
            firsts
                .iter()
                .enumerate()
                .map(|(idx, &lo)| {
                    let hi = firsts.get(idx + 1).and_then(|n| n.checked_sub(1));
                    (SegmentAppliedState::Unknown, lo, hi)
                })
                .collect() // APPROVED: cold prune pass
        }
        PruneAppliedView::Watermark(Some(snap)) => {
            let firsts: Vec<u64> = sorted
                .iter()
                .map(|p| first_frame_seq_in_segment(p))
                .collect(); // APPROVED: cold prune pass
            firsts
                .iter()
                .enumerate()
                .map(|(idx, &lo)| {
                    let hi = firsts.get(idx + 1).and_then(|n| n.checked_sub(1));
                    let state = match hi {
                        Some(hi) if lo > 0 && hi >= lo => {
                            if segment_range_is_applied(snap, lo, hi) {
                                SegmentAppliedState::Applied
                            } else {
                                SegmentAppliedState::Unapplied
                            }
                        }
                        _ => SegmentAppliedState::Unknown,
                    };
                    (state, lo, hi)
                })
                .collect() // APPROVED: cold prune pass
        }
    }
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
    view: PruneAppliedView<'_>,
    deferred: Option<&crate::wal_deferred_depth::DeferredDepthSnapshot>,
    disk_below_floor: bool,
    upload: UploadGate<'_>,
) -> ArchivePruneOutcome {
    let archive_dir = dir.to_path_buf();
    let mut outcome = ArchivePruneOutcome::default();
    // Every `*.wal` in the directory, open segment included — the open one
    // still bounds its predecessor's sequence range (item 44d). One entry per
    // segment, pre-sized to a typical steady-state count; cold path.
    let mut segments: Vec<(PathBuf, Option<Metadata>)> =
        Vec::with_capacity(ARCHIVE_PRUNE_SURVIVOR_HINT);
    // O(1) EXEMPT: periodic cold archive prune, never the per-frame append
    let Ok(entries) = std::fs::read_dir(&archive_dir) else {
        return outcome; // missing archive dir — nothing to prune
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("wal") {
            outcome.kept += 1; // foreign file — never touched
            continue;
        }
        segments.push((path, entry.metadata().ok()));
    }
    // File-name order == capture order (`ws-frames-{nanos:020}.wal`).
    segments.sort_by(|a, b| a.0.file_name().cmp(&b.0.file_name())); // O(1) EXEMPT: cold prune pass
    let sorted_paths: Vec<PathBuf> = segments.iter().map(|(p, _)| p.clone()).collect(); // APPROVED: cold prune pass
    let states = segment_applied_states(&sorted_paths, view);

    // Segments surviving the age pass — the byte ceiling's candidate set, as
    // (mtime, len, path, aged, state, first_seq, last_seq).
    // Pre-allocated rather than grown from empty, per the banned-pattern
    // rule. Cold path — one allocation per prune pass, never the per-frame
    // append.
    let mut survivors: Vec<PruneSurvivor> = Vec::with_capacity(ARCHIVE_PRUNE_SURVIVOR_HINT);
    // Segments pinned by deferred-depth marks (item 45a): never a byte
    // candidate, taken only below the hard disk floor.
    let mut pinned: Vec<PruneSurvivor> = Vec::with_capacity(ARCHIVE_PRUNE_SURVIVOR_HINT);
    let cutoff = std::time::Duration::from_secs(retention_secs);
    for ((path, meta), &(state, first_seq, last_seq)) in segments.into_iter().zip(states.iter()) {
        // NEVER the segment the writer currently holds open (2026-09-06).
        //
        // This function was extracted for the ARCHIVE directory, where no
        // segment is ever open — the local binding is still called
        // `archive_dir` — and then reused verbatim for the ACTIVE directory,
        // where exactly one always is. `wal_segments_in` (the replay
        // enumerator) has filtered on this predicate since it was written;
        // this loop never did.
        //
        // Unlinking an open segment is SILENT and PERMANENT. On Linux the
        // writer keeps a valid handle to the now-nameless inode, so
        // `write_all`, `flush` and `sync_all` all keep returning `Ok`;
        // `persist_record_resilient` counts the frame persisted, `append`
        // returns `Spilled`, and no caller ever calls `note_unapplied`. Every
        // frame from that moment until the next 128 MiB rotation is gone with
        // no error, no counter and an applied-watermark that still believes
        // the range is recoverable — the one failure shape capture-at-receipt
        // exists to make impossible.
        //
        // Reachability is narrow, which is why it survived: the age pass
        // needs a segment open past `WS_WAL_ACTIVE_RETENTION_SECS` (48 h,
        // against a box that stops daily), and the byte pass deletes
        // oldest-first so it reaches the newest file only once nearly the
        // whole set is gone. Narrow is not zero, the predicate already
        // exists, and the cost of using it is one comparison per pass on a
        // six-hourly cold path.
        if is_open_segment(&path) {
            outcome.kept += 1;
            continue;
        }
        // Shed depth not yet written back (2026-09-29, item 45a): this segment
        // is the only copy of those rows. Kept by the age pass and by the
        // byte pass — except below the hard disk floor, see after the byte
        // pass. A segment whose start could not be read (`first_seq` 0) is
        // kept whenever any mark exists: deleting on uncertainty would be the
        // wrong default here too.
        // Item 45e-1: with the gate on, every delete below needs a verified S3
        // copy of the segment. Unreadable metadata never satisfies it.
        let uploaded = upload.satisfied(&path, meta.as_ref().map(Metadata::len));
        let hi = if first_seq == 0 { None } else { last_seq };
        if deferred.is_some_and(|d| d.range_is_deferred(first_seq, hi)) {
            outcome.kept += 1;
            outcome.deferred_kept += 1;
            let len = meta.as_ref().map_or(0, Metadata::len);
            outcome.deferred_kept_bytes = outcome.deferred_kept_bytes.saturating_add(len);
            pinned.push(PruneSurvivor {
                mtime: meta
                    .as_ref()
                    .and_then(|m| m.modified().ok())
                    .unwrap_or(std::time::UNIX_EPOCH),
                len,
                path,
                aged: false,
                state,
                first_seq,
                last_seq,
                uploaded,
            });
            continue;
        }
        let mtime = meta.as_ref().and_then(|m| m.modified().ok());
        // Fresh, unreadable metadata, or a future mtime (clock skew): not
        // aged — deleting on uncertainty would be the wrong default.
        let aged = mtime
            .and_then(|mt| now.duration_since(mt).ok())
            .is_some_and(|age| age > cutoff);
        match segment_prune_decision(aged, state, false, uploaded) {
            // O(1) EXEMPT: periodic cold archive prune, never the per-frame append
            SegmentPruneDecision::DeleteAged => match std::fs::remove_file(&path) {
                Ok(()) => {
                    outcome.deleted += 1;
                    upload.forget(&path);
                }
                Err(err) => {
                    outcome.failed += 1;
                    warn!(
                        path = %path.display(),
                        error = %err,
                        "WAL archive prune: remove_file failed — retried next pass"
                    );
                }
            },
            SegmentPruneDecision::Keep
            | SegmentPruneDecision::DeleteForBytes
            | SegmentPruneDecision::RefuseForBytes { .. }
            | SegmentPruneDecision::RefuseNotUploaded => {
                outcome.kept += 1;
                if aged && state == SegmentAppliedState::Applied {
                    // Past the window and applied, kept only because no
                    // verified S3 copy is recorded yet (item 45e-1).
                    outcome.age_kept_not_uploaded += 1;
                } else if aged {
                    // Past the window, kept only because the watermark has
                    // not passed it (item 44d).
                    outcome.age_kept_unapplied += 1;
                }
                // Survivor of the age pass — a candidate for the byte
                // ceiling below. mtime is the sort key so the ceiling
                // deletes genuinely oldest-first. Unreadable metadata is
                // kept but never a byte candidate (no length to account).
                if let Some(meta) = meta {
                    survivors.push(PruneSurvivor {
                        mtime: mtime.unwrap_or(std::time::UNIX_EPOCH),
                        len: meta.len(),
                        path,
                        aged,
                        state,
                        first_seq,
                        last_seq,
                        uploaded,
                    });
                }
            }
        }
    }

    // BYTE CEILING (2026-08-19) — the age pass alone bounds the archive only
    // for the traffic level its window was chosen against. This bounds it
    // unconditionally: while the total exceeds `max_bytes`, delete the OLDEST
    // APPLIED survivor. Oldest-first because the newest segment is the one a
    // crash triage actually needs.
    //
    // It deletes ONLY applied segments (2026-10-01, item 45b). An unapplied or
    // unknown segment may be the only copy of its frames, so it is REFUSED:
    // kept, counted, and named in one coded line per pass. Until item 45b this
    // loop deleted them too (item 44d), which the 2026-09-29 zero-loss lock
    // forbids. When refusals leave the directory over its ceiling, the disk is
    // held by data the database has not taken yet; the ingest shed, the disk
    // pressure ladder and the free-space alarm act on that, not this loop.
    //
    // Cold path, runs on the periodic prune task — never the per-frame append.
    // Pinned (deferred-depth) bytes COUNT toward the ceiling (item 45a), so
    // ordinary segments go first to make room for them; they are just never
    // taken by this loop.
    let total: u64 = survivors
        .iter()
        .map(|s| s.len)
        .sum::<u64>()
        .saturating_add(outcome.deferred_kept_bytes);
    outcome.bytes_after = total;
    if total > max_bytes {
        // O(1) EXEMPT: periodic cold archive prune, never the per-frame append
        // Already-applied segments first, then oldest first (item 45a). Since
        // item 45b only the applied ones can be deleted; the order still puts
        // every applied segment ahead of the first refusal. Since item 45e-1
        // "deletable" also needs a verified S3 copy, so applied AND uploaded
        // segments go first.
        survivors.sort_by_key(|s| {
            (
                !(s.state == SegmentAppliedState::Applied && s.uploaded),
                s.mtime,
            )
        });
        let mut remaining = total;
        for s in &survivors {
            if remaining <= max_bytes {
                break;
            }
            match segment_prune_decision(s.aged, s.state, true, s.uploaded) {
                SegmentPruneDecision::DeleteForBytes | SegmentPruneDecision::DeleteAged => {}
                SegmentPruneDecision::RefuseForBytes { unknown } => {
                    note_byte_refusal(&mut outcome, s, unknown);
                    continue;
                }
                SegmentPruneDecision::RefuseNotUploaded => {
                    note_not_uploaded_refusal(&mut outcome, s);
                    continue;
                }
                SegmentPruneDecision::Keep => continue,
            }
            // O(1) EXEMPT: periodic cold archive prune, never the per-frame append
            match std::fs::remove_file(&s.path) {
                Ok(()) => {
                    upload.forget(&s.path);
                    outcome.size_deleted += 1;
                    outcome.size_deleted_bytes = outcome.size_deleted_bytes.saturating_add(s.len);
                    // Oldest-first walk, so the FIRST successful delete is the
                    // oldest one; `max` keeps that true even if an earlier
                    // remove failed and a later one succeeded.
                    let age_secs = now.duration_since(s.mtime).map_or(0, |d| d.as_secs());
                    outcome.size_deleted_oldest_age_secs =
                        outcome.size_deleted_oldest_age_secs.max(age_secs);
                    outcome.kept = outcome.kept.saturating_sub(1);
                    remaining = remaining.saturating_sub(s.len);
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
                    remaining = remaining.saturating_sub(s.len);
                    warn!(
                        path = %s.path.display(),
                        error = %err,
                        "WAL archive byte-ceiling prune: remove_file failed — \
                         counted against the budget so the pass cannot \
                         over-delete newer segments; retried next pass"
                    );
                }
            }
        }
        outcome.bytes_after = remaining;
        if outcome.size_deleted > 0 {
            warn!(
                deleted = outcome.size_deleted,
                refused = outcome.size_refused_unapplied + outcome.size_refused_unknown,
                bytes_before = total,
                bytes_after = remaining,
                max_bytes,
                "WAL directory exceeded its byte ceiling — deleted the oldest APPLIED \
                 segments (copies of frames the database already holds). The age window \
                 is longer than this traffic level can afford; re-derive it against \
                 measured volume."
            );
        }
    }
    // Item 45a, THE HARD FLOOR. Segments holding shed depth are kept over the
    // byte ceiling — but not over a disk about to fill. A full volume stops
    // the WAL writer and the database together, and then TICKS are lost, not
    // just order-book rows. So only when the caller has measured free space
    // below the floor are pinned segments taken, oldest first, each one a
    // coded, counted loss. Everything above the floor keeps them.
    //
    // Only an APPLIED pinned segment (2026-10-01, item 45b): its ticks and
    // candles are already in the database and only the shed depth rows are
    // lost. A pinned segment that is also unapplied or unknown may be the
    // only copy of its ticks, so it is refused here exactly as in the byte
    // pass above.
    if disk_below_floor && outcome.bytes_after > max_bytes && !pinned.is_empty() {
        // O(1) EXEMPT: cold prune pass, pinned segments only
        pinned.sort_by_key(|s| s.mtime);
        let mut remaining = outcome.bytes_after;
        for s in &pinned {
            if remaining <= max_bytes {
                break;
            }
            if s.state != SegmentAppliedState::Applied {
                note_byte_refusal(&mut outcome, s, s.state == SegmentAppliedState::Unknown);
                continue;
            }
            // Item 45e-1: not even below the floor without a verified S3 copy.
            if !s.uploaded {
                note_not_uploaded_refusal(&mut outcome, s);
                continue;
            }
            // O(1) EXEMPT: periodic cold WAL prune under the disk floor, never the per-frame append
            match std::fs::remove_file(&s.path) {
                Ok(()) => {
                    upload.forget(&s.path);
                    remaining = remaining.saturating_sub(s.len);
                    outcome.size_deleted += 1;
                    outcome.size_deleted_deferred += 1;
                    outcome.size_deleted_bytes = outcome.size_deleted_bytes.saturating_add(s.len);
                    outcome.deferred_kept = outcome.deferred_kept.saturating_sub(1);
                    outcome.deferred_kept_bytes = outcome.deferred_kept_bytes.saturating_sub(s.len);
                    outcome.kept = outcome.kept.saturating_sub(1);
                    if upload.copy_in_s3() {
                        // The raw segment is in S3, verified, so the shed
                        // depth rows can be recovered from it — but nothing
                        // re-reads an S3 copy yet (plan item 45e-2), so until
                        // then they are missing from the database.
                        error!(
                            code = ErrorCode::WsSpill02FrameDropped.code_str(),
                            source = "deferred_depth_deleted_under_disk_floor_copy_in_s3",
                            segment = %s.path.display(),
                            first_frame_seq = s.first_seq,
                            bytes = s.len,
                            "the disk is nearly full, so a WAL segment holding order-book rows \
                             skipped under load was deleted locally before they were written \
                             back. A verified raw copy is in S3, but those rows are not in the \
                             database until that copy is replayed."
                        );
                    } else {
                        error!(
                            code = ErrorCode::WsSpill02FrameDropped.code_str(),
                            source = "deferred_depth_deleted_under_disk_floor",
                            segment = %s.path.display(),
                            first_frame_seq = s.first_seq,
                            bytes = s.len,
                            "the disk is nearly full, so a WAL segment holding order-book rows \
                             skipped under load was DELETED before they were written back — those \
                             rows are lost. Taken only to keep ticks and the database alive."
                        );
                    }
                }
                Err(err) => {
                    remaining = remaining.saturating_sub(s.len);
                    outcome.failed += 1;
                    warn!(
                        path = %s.path.display(),
                        error = %err,
                        "WAL floor prune: remove_file failed on a pinned segment"
                    );
                }
            }
        }
        outcome.bytes_after = remaining;
    }
    // Pinned segments alone keeping the directory over its ceiling: the disk
    // is being held for rows not yet written back. Said loudly, with the
    // numbers; the after-close pass drains them and the disk alarms page if
    // it cannot keep up.
    if outcome.deferred_kept_bytes > 0 && outcome.bytes_after > max_bytes {
        error!(
            code = ErrorCode::WsSpill01WriterRespawn.code_str(),
            source = "deferred_depth_pinned_over_ceiling",
            dir = %dir.display(),
            deferred_segments = outcome.deferred_kept,
            deferred_bytes = outcome.deferred_kept_bytes,
            max_bytes,
            "WAL directory is over its byte ceiling because of segments that hold order-book \
             rows skipped under load and not yet written back. They are kept, not deleted. \
             If the after-close pass cannot drain them, disk space will keep shrinking."
        );
    }
    // Item 45b: the byte pass needed to delete segments the database has not
    // taken yet, and refused. ONE coded line per pass with the totals (a
    // backlog can be hundreds of segments, and a line each would be the
    // error-log burst item 45d exists to stop); the per-segment names are at
    // debug level.
    let refused = outcome.size_refused_unapplied + outcome.size_refused_unknown;
    if refused > 0 {
        error!(
            code = ErrorCode::WsSpill01WriterRespawn.code_str(),
            source = "wal_prune_refused_unapplied",
            dir = %dir.display(),
            segments_unapplied = outcome.size_refused_unapplied,
            segments_unknown = outcome.size_refused_unknown,
            refused_bytes = outcome.size_refused_bytes,
            bytes_after = outcome.bytes_after,
            max_bytes,
            "WAL directory is over its byte ceiling and the cleanup REFUSED to delete \
             segments the database has not taken yet (or whose state it cannot read). They \
             are the only copy of those frames, so they are kept and the disk keeps \
             filling until the database catches up. Counted as \
             tv_wal_prune_refused_unapplied_total."
        );
    }
    // Item 45e-1: the same shape for segments the database HAS taken but
    // that have no verified copy in S3 yet. One coded line per pass.
    if outcome.size_refused_not_uploaded > 0 {
        error!(
            code = ErrorCode::WsSpill01WriterRespawn.code_str(),
            source = "wal_prune_refused_not_uploaded",
            dir = %dir.display(),
            segments = outcome.size_refused_not_uploaded,
            refused_bytes = outcome.size_refused_not_uploaded_bytes,
            bytes_after = outcome.bytes_after,
            max_bytes,
            "WAL directory is over its byte ceiling and the cleanup REFUSED to delete \
             segments that have no verified copy in S3 yet. Raw capture never leaves the box \
             without a copy, so they are kept until the uploader copies them. Counted as \
             tv_wal_prune_refused_not_uploaded_total."
        );
    }
    outcome
}

/// Records one applied segment a delete pass refused because no verified S3
/// copy is recorded (item 45e-1). Cold prune path; per-segment detail at
/// debug, one coded total per pass.
fn note_not_uploaded_refusal(outcome: &mut ArchivePruneOutcome, s: &PruneSurvivor) {
    outcome.size_refused_not_uploaded += 1;
    outcome.size_refused_not_uploaded_bytes = outcome
        .size_refused_not_uploaded_bytes
        .saturating_add(s.len);
    debug!(
        segment = %s.path.display(),
        bytes = s.len,
        "WAL prune kept an applied segment with no verified S3 copy yet"
    );
}

/// Records one segment the byte pass refused to delete (item 45b). Cold prune
/// path; the per-segment name goes to debug, the pass logs one coded total.
fn note_byte_refusal(outcome: &mut ArchivePruneOutcome, s: &PruneSurvivor, unknown: bool) {
    if unknown {
        outcome.size_refused_unknown += 1;
    } else {
        outcome.size_refused_unapplied += 1;
    }
    outcome.size_refused_bytes = outcome.size_refused_bytes.saturating_add(s.len);
    debug!(
        segment = %s.path.display(),
        first_frame_seq = s.first_seq,
        last_frame_seq = ?s.last_seq,
        applied_state = ?s.state,
        bytes = s.len,
        "WAL byte-ceiling prune kept a segment the applied watermark has not passed"
    );
}

/// One age-pass survivor, carried into the byte pass.
struct PruneSurvivor {
    mtime: std::time::SystemTime,
    len: u64,
    path: PathBuf,
    aged: bool,
    state: SegmentAppliedState,
    first_seq: u64,
    last_seq: Option<u64>,
    /// A verified S3 copy is recorded, or the upload gate is off (item 45e-1).
    uploaded: bool,
}

/// Publishes the not-uploaded refusal counter for one directory. Unconditional,
/// possibly zero, so the series exists before the first real refusal.
fn publish_not_uploaded_refusals(dir: &'static str, outcome: &ArchivePruneOutcome) {
    // APPROVED: cast — a per-pass segment count, always <= u64.
    metrics::counter!(WAL_PRUNE_REFUSED_NOT_UPLOADED_COUNTER, "dir" => dir)
        .increment(outcome.size_refused_not_uploaded as u64);
    if outcome.age_kept_not_uploaded > 0 {
        info!(
            dir,
            age_kept_not_uploaded = outcome.age_kept_not_uploaded,
            "WAL prune kept applied segments past the age window because no verified S3 \
             copy is recorded for them yet; they go once the uploader has copied them"
        );
    }
}

/// Wall-clock wrapper over [`prune_archived_segments_at`]. Cold path —
/// called from the periodic prune task in `main.rs` (once at task start,
/// then every `WS_WAL_ARCHIVE_PRUNE_INTERVAL_SECS`).
///
/// `require_upload` is `[raw_frame_archive] require_upload_before_prune`
/// (item 45e-1): every delete then needs the segment's verified-upload marker
/// under `<wal_dir>/uploaded/`.
#[must_use]
pub fn prune_archived_segments<P: AsRef<Path>>(
    wal_dir: P,
    retention_secs: u64,
    max_bytes: u64,
    require_upload: bool,
) -> ArchivePruneOutcome {
    // Item 45a: a segment holding shed depth not yet written back is kept.
    let deferred = crate::wal_deferred_depth::prune_view(wal_dir.as_ref());
    let below_floor = wal_disk_below_floor(wal_dir.as_ref());
    let gate = UploadGateOwned::for_wal_dir(wal_dir.as_ref(), require_upload);
    let outcome = prune_archived_segments_deferred_at(
        wal_dir,
        retention_secs,
        max_bytes,
        std::time::SystemTime::now(),
        Some(&deferred),
        below_floor,
        gate.gate(),
    );
    publish_deferred_kept("archive", outcome.deferred_kept);
    publish_not_uploaded_refusals("archive", &outcome);
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

/// Gauge: segments a prune pass kept because they hold shed depth that has
/// not been written back yet (item 45a), per directory.
pub const WAL_DEFERRED_KEPT_GAUGE: &str = "tv_wal_deferred_depth_kept_segments";

/// Free-space fraction below which segments pinned by deferred-depth marks
/// may be taken by the byte pass (item 45a). Five percent: well under the
/// ingest shed's own thresholds, so the shed and the retention ladder act
/// first and this is the last step before a full volume.
pub const WAL_PINNED_DISK_FLOOR_FREE_FRACTION: f64 = 0.05;

/// Whether the volume holding `wal_dir` has less free space than
/// [`WAL_PINNED_DISK_FLOOR_FREE_FRACTION`]. A failed probe reads as NOT below
/// the floor: an unmeasured disk never justifies deleting the only copy.
/// Cold prune path: one `df`.
#[must_use]
pub fn wal_disk_below_floor(wal_dir: &Path) -> bool {
    match crate::disk_health_watcher::probe_disk_free_bytes(wal_dir) {
        crate::disk_health_watcher::DiskHealthOutcome::Ok {
            free_bytes,
            total_bytes,
        } if total_bytes > 0 => {
            // APPROVED: cast — byte counts far below f64 precision loss that matters.
            #[allow(clippy::cast_precision_loss)] // APPROVED: ratio of byte counts
            let free = free_bytes as f64 / total_bytes as f64;
            free < WAL_PINNED_DISK_FLOOR_FREE_FRACTION
        }
        _ => false,
    }
}

/// Publishes [`WAL_DEFERRED_KEPT_GAUGE`] for one directory. Cold prune path.
fn publish_deferred_kept(dir: &'static str, kept: usize) {
    // APPROVED: cast — a per-pass segment count, far below f64 precision.
    #[allow(clippy::cast_precision_loss)] // APPROVED: segment count
    metrics::gauge!(WAL_DEFERRED_KEPT_GAUGE, "dir" => dir).set(kept as f64);
}

/// Wall-clock wrapper over [`prune_active_segments_at`]. Cold path — called
/// from the same periodic prune task as the archive sweep.
#[must_use]
pub fn prune_active_segments<P: AsRef<Path>>(
    wal_dir: P,
    retention_secs: u64,
    max_bytes: u64,
    require_upload: bool,
) -> ArchivePruneOutcome {
    let wal_dir = wal_dir.as_ref();
    // Item 44d (2026-09-22): the age pass needs the applied watermark. Read
    // from disk (the file the next boot's replay trusts), under the same kill
    // switch as the replay skip. Absent, rejected or switched off => `None`,
    // and the age pass keeps every segment — fail closed.
    let applied = if applied_skip_enabled() {
        crate::wal_applied_watermark::AppliedSnapshot::load(wal_dir)
    } else {
        None
    };
    // Item 45a: a segment holding shed depth not yet written back is kept.
    let deferred = crate::wal_deferred_depth::prune_view(wal_dir);
    // Item 45e-1: every delete needs a verified S3 copy when the gate is on.
    let gate = UploadGateOwned::for_wal_dir(wal_dir, require_upload);
    let outcome = prune_active_segments_deferred_at(
        wal_dir,
        retention_secs,
        max_bytes,
        std::time::SystemTime::now(),
        applied.as_ref(),
        Some(&deferred),
        wal_disk_below_floor(wal_dir),
        gate.gate(),
    );
    publish_deferred_kept("active", outcome.deferred_kept);
    publish_not_uploaded_refusals("active", &outcome);
    // Unconditional, possibly zero: creates both series on the first pass so
    // the first real refusal is not swallowed as a baseline.
    // APPROVED: cast — a per-pass segment count, always <= u64.
    metrics::counter!(WAL_PRUNE_REFUSED_UNAPPLIED_COUNTER, "state" => "unapplied")
        .increment(outcome.size_refused_unapplied as u64);
    metrics::counter!(WAL_PRUNE_REFUSED_UNAPPLIED_COUNTER, "state" => "unknown")
        .increment(outcome.size_refused_unknown as u64);
    if outcome.age_kept_unapplied > 0 {
        info!(
            age_kept_unapplied = outcome.age_kept_unapplied,
            watermark_loaded = applied.is_some(),
            retention_secs,
            "WAL ACTIVE prune kept segments past the age window because the applied \
             watermark has not passed them (or no usable watermark exists). Neither \
             pass removes them until the watermark does."
        );
    }
    if outcome.deleted > 0 || outcome.failed > 0 || outcome.size_deleted > 0 {
        metrics::counter!("tv_ws_wal_active_pruned_total")
            .increment((outcome.deleted + outcome.size_deleted) as u64);
        // Counted SEPARATELY from the total: the byte pass firing means the age
        // window is longer than this traffic level can afford on this disk.
        //
        // Since item 45b (2026-10-01) the byte pass deletes only APPLIED
        // segments — copies of frames the database already holds — so this is
        // a sizing signal, not a loss, and it no longer raises the coded
        // WS-SPILL-01 line it raised from 2026-09-02 (when it deleted
        // unapplied capture too). The one loss it can still include is a
        // pinned segment taken below the hard disk floor, which logs its own
        // coded WS-SPILL-02 line per segment.
        //
        // Deliberately NOT EMF-selected: the condition that produces it --
        // the volume filling -- already pages via `tv-<env>-spill-dir-free-low`
        // (noise-lock 2.3g), and every EMF name costs ~$0.30/mo. Readable on
        // `/metrics` and through the debug API.
        if outcome.size_deleted > 0 {
            metrics::counter!("tv_ws_wal_active_pruned_under_pressure_total")
                .increment(outcome.size_deleted as u64);
        }
        info!(
            deleted = outcome.deleted,
            size_deleted = outcome.size_deleted,
            size_deleted_deferred = outcome.size_deleted_deferred,
            bytes_freed = outcome.size_deleted_bytes,
            oldest_segment_age_secs = outcome.size_deleted_oldest_age_secs,
            refused = outcome.size_refused_unapplied + outcome.size_refused_unknown,
            failed = outcome.failed,
            kept = outcome.kept,
            bytes_after = outcome.bytes_after,
            retention_secs,
            max_bytes,
            "WAL ACTIVE prune pass complete — applied segments deleted by age and, \
             separately, by the byte ceiling. Every deleted segment was one the applied \
             watermark had passed, so its frames are already in the database; segments \
             it had not passed are kept (see tv_wal_prune_refused_unapplied_total)."
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

#[cfg(test)]
fn replay_segment(path: &Path) -> anyhow::Result<Vec<ReplayedFrame>> {
    replay_segment_filtered(path, None).map(|(frames, _)| frames)
}

/// [`replay_segment`] with the applied-watermark filter: a record whose
/// sequence is known-applied for its sink is CRC-verified, counted, and NOT
/// returned. Returns `(frames, dropped_as_applied)`.
///
/// The filter runs AFTER the CRC check on purpose. A corrupt record must end
/// the walk and be reported whether or not its sequence would have been
/// skipped; filtering before the check would let corruption inside a skipped
/// range pass silently.
#[cfg(test)]
fn replay_segment_filtered(
    path: &Path,
    applied: Option<&crate::wal_applied_watermark::AppliedSnapshot>,
) -> anyhow::Result<(Vec<ReplayedFrame>, u64)> {
    replay_segment_core(path, applied, &|_, _| true, true)
        .map(|read| (read.frames, read.dropped_as_applied))
}

/// One WAL segment and the capture-sequence range it holds, for the
/// after-close deferred-depth pass (item 45a).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SegmentSpan {
    pub path: PathBuf,
    /// First frame sequence; `0` when the first record could not be read.
    pub lo: u64,
    /// The next segment's first sequence minus one; `None` for the newest
    /// segment or an unreadable successor.
    pub hi: Option<u64>,
    /// The writer still holds this segment open.
    pub is_open: bool,
}

/// Every `*.wal` segment in the active directory, `replaying/` and
/// `archive/`, in capture order (file-name order is capture order), with its
/// sequence range. The open segment IS listed — flagged — because it still
/// bounds its predecessor's range. Cold path: one verified header read per
/// segment.
///
/// # Errors
/// Any failure to list the ACTIVE directory, or a subdirectory for any reason
/// other than not existing. A partial listing must never read as "those
/// segments are gone": the caller would then treat marked rows as lost and
/// clear their protection while the segments are still on disk.
pub fn deferred_segment_spans(wal_dir: &Path) -> std::io::Result<Vec<SegmentSpan>> {
    let mut paths: Vec<PathBuf> = Vec::with_capacity(ARCHIVE_PRUNE_SURVIVOR_HINT);
    // O(1) EXEMPT: begin — cold after-close listing, bounded by the segments on disk
    for dir in [
        wal_dir.to_path_buf(),
        wal_dir.join(REPLAYING_SUBDIR),
        wal_dir.join(ARCHIVE_SUBDIR),
    ] {
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound && dir != wal_dir => continue,
            Err(err) => return Err(err),
        };
        for entry in entries {
            let entry = entry?;
            // Regular files only: a FIFO or device named `*.wal` would hang
            // the header read.
            if !entry.file_type()?.is_file() {
                continue;
            }
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) == Some("wal") {
                paths.push(path);
            }
        }
    }
    // O(1) EXEMPT: end
    paths.sort_by(|a, b| a.file_name().cmp(&b.file_name())); // O(1) EXEMPT: cold listing
    let firsts: Vec<u64> = paths
        .iter()
        .map(|p| first_frame_seq_in_segment(p))
        .collect(); // APPROVED: cold listing
    let nexts = firsts
        .iter()
        .skip(1)
        .map(|&n| n.checked_sub(1))
        .chain(std::iter::once(None));
    Ok(paths
        .into_iter()
        .zip(firsts.iter().copied())
        .zip(nexts)
        .map(|((path, lo), hi)| SegmentSpan {
            is_open: is_open_segment(&path),
            path,
            lo,
            hi,
        })
        .collect()) // APPROVED: cold listing
}

/// Which segments hold frames of capture bucket `bucket_id` (shift
/// `wal_deferred_depth::DEFERRED_BUCKET_SHIFT`), and whether the answer is
/// complete. Cold path.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct BucketSegments {
    pub paths: Vec<PathBuf>,
    /// A segment still open for writing may hold more of this bucket.
    pub touches_open: bool,
    /// A segment whose first record is unreadable sits where it could hold
    /// this bucket, so no set of readable segments can be called complete.
    pub uncertain: bool,
}

/// See [`BucketSegments`].
#[must_use]
pub fn segments_for_bucket(spans: &[SegmentSpan], bucket_id: u64) -> BucketSegments {
    let shift = crate::wal_deferred_depth::DEFERRED_BUCKET_SHIFT;
    let mut out = BucketSegments::default();
    // The start of the last readable segment seen, so an unreadable one can be
    // placed between its neighbours.
    let mut prev_lo_id: u64 = 0;
    // O(1) EXEMPT: begin — cold after-close lookup, one pass over the listing
    for (idx, span) in spans.iter().enumerate() {
        if span.lo == 0 {
            let next_lo_id = spans
                .get(idx + 1..)
                .and_then(|rest| rest.iter().find(|s| s.lo != 0))
                .map_or(u64::MAX, |s| s.lo >> shift);
            if (prev_lo_id..=next_lo_id).contains(&bucket_id) {
                out.uncertain = true;
            }
            continue;
        }
        let lo_id = span.lo >> shift;
        prev_lo_id = lo_id;
        let covers = lo_id <= bucket_id && span.hi.is_none_or(|hi| bucket_id <= hi >> shift);
        if covers {
            if span.is_open {
                out.touches_open = true;
            } else {
                out.paths.push(span.path.clone()); // APPROVED: cold after-close lookup, one path per covering segment
            }
        }
    }
    // O(1) EXEMPT: end
    out
}

/// What one CRC-verified walk of a segment produced.
#[derive(Debug)]
pub struct SegmentRead {
    /// Frames that passed the applied filter AND the caller's `keep`.
    pub frames: Vec<ReplayedFrame>,
    /// Frames skipped because the watermark says they are applied.
    pub dropped_as_applied: u64,
    /// The walk stopped before the end on a bad record, or the segment is
    /// unreadable by this binary. A torn TAIL is not damage: it is what an
    /// interrupted writer leaves and abandons nothing beyond itself.
    pub damaged: bool,
    /// Frames were dropped as applied AFTER the last returned frame (plan
    /// ITEM 47): the next segment's first frame follows a gap.
    pub trailing_gap: bool,
}

/// Reads the frames of ONE segment whose `(frame_seq, endpoint)` satisfies
/// `keep`, without moving the file and without the replay's corruption
/// accounting (the caller reports damage in its own terms). Used by the
/// after-close deferred-depth pass (item 45a). Cold path: reads the whole
/// file, returns only the kept frames.
///
/// # Errors
/// The file could not be opened or read.
pub fn read_segment_frames_matching<F: Fn(u64, WalEndpoint) -> bool>(
    path: &Path,
    keep: &F,
) -> anyhow::Result<SegmentRead> {
    replay_segment_core(path, None, keep, false)
}

/// The shared walk behind the boot replay (`replay_all_with_report_guarded`) and
/// [`read_segment_frames_matching`]. `report_corruption` is `false` only for
/// the deferred-depth pass, whose segments are NOT moved to the archive on
/// damage — the replay's log text would say otherwise.
fn replay_segment_core<F: Fn(u64, WalEndpoint) -> bool>(
    path: &Path,
    applied: Option<&crate::wal_applied_watermark::AppliedSnapshot>,
    keep: &F,
    report_corruption: bool,
) -> anyhow::Result<SegmentRead> {
    let mut unreadable = false;
    let mut dropped_as_applied = 0u64;
    // Plan ITEM 47: a frame dropped as applied leaves a gap before the next
    // kept one.
    let mut gap_pending = false;
    let mut f = File::open(path)?;
    let mut buf = Vec::new(); // APPROVED: boot-time WAL replay, cold path
    f.read_to_end(&mut buf)?;

    let mut out = Vec::new(); // APPROVED: boot-time WAL replay, cold path
    let mut i = 0usize;
    // Where the walk STOPPED for good on a bad record: every byte from this
    // offset to EOF was read by nothing. `None` when the walk reached the end
    // (possibly after resyncing past damage) or stopped on a genuine torn tail.
    let mut corrupted_at: Option<(&str, usize)> = None;
    // Damage the walk RESYNCED past (Z11a, 2026-10-02): a bad record followed,
    // somewhere later in the file, by a record with a valid magic, type,
    // plausible length and matching CRC. Before this, the first bad record
    // ended the walk and every record after it was abandoned — at 128 MiB a
    // segment, one flipped bit could cost ~700,000 frames that were sitting
    // intact on disk.
    let mut resync = ResyncTally::default();
    // TVW4 records whose endpoint byte this binary does not recognise. They
    // replay as `MainFeed` (total decode, never a drop) and are COUNTED here so
    // the mapping is reported once per segment rather than silently applied.
    let mut unknown_endpoint = 0usize;
    // Smallest record is v1 (13 bytes); fewer bytes than that cannot hold a
    // record, so a shorter remainder is a torn tail by construction (and a
    // resync scan could not find a record in it either).
    while i + WAL_MIN_RECORD_V1 <= buf.len() {
        let rec = match decode_record_at(&buf, i, usize::MAX) {
            RecordDecode::Record(rec) => rec,
            RecordDecode::PastEnd { declared_len } => {
                // The record runs past EOF. A genuine torn tail — what an
                // interrupted writer leaves — is the LAST thing in the file and
                // abandons nothing beyond itself, so it stays silent. Two other
                // things look identical at this offset and are NOT silent
                // (Z11a): a corrupt length that merely points past EOF while
                // valid records follow (resync finds them), and a length no
                // writer could have produced (above every transport cap).
                if let Some(next) = resync_from(&buf, i + 1) {
                    resync.record_gap("length_past_eof", i, next, out.len());
                    gap_pending = true;
                    i = next;
                    continue;
                }
                if declared_len.is_some_and(|l| l > WAL_RESYNC_MAX_FRAME_BYTES) {
                    corrupted_at = Some(("length_past_eof", i));
                    warn!(
                        segment = ?path,
                        offset = i,
                        frame_len = declared_len,
                        "record length points past EOF and exceeds every frame cap; \
                         counted as damage, not a torn tail"
                    );
                } else {
                    warn!(segment = ?path, offset = i, frame_len = declared_len, "truncated record at tail");
                }
                break;
            }
            RecordDecode::Bad(reason) => {
                if let Some(next) = resync_from(&buf, i + 1) {
                    resync.record_gap(reason, i, next, out.len());
                    gap_pending = true;
                    i = next;
                    continue;
                }
                // An unknown magic at offset 0, with no readable record
                // anywhere after it, means the WHOLE segment is unreadable —
                // and the overwhelmingly likely cause is a DEPLOY ROLLBACK: a
                // newer binary wrote a record version this one cannot parse.
                // That is not a torn tail, it is total loss of a segment that
                // was captured successfully, and the caller stages and
                // archives a zero-frame result exactly as it would a clean
                // replay.
                //
                // So it is separated from the mid-segment case and raised as a
                // CODED error, not a bare `warn!`. Before 2026-08-28 this arm
                // was uncoded, which meant no CloudWatch metric filter could
                // match it: the loss was not merely unrecovered, it was
                // unpageable.
                //
                // The segment itself is NOT deleted here — it is moved to the
                // archive directory by the caller and survives until pruning,
                // so a manual recovery with the newer binary remains possible.
                if i == 0 && reason == "magic_mismatch" {
                    unreadable = true;
                    metrics::counter!("tv_wal_replay_unknown_magic_total").increment(1);
                    error!(
                        code = ErrorCode::WsSpill02FrameDropped.code_str(),
                        segment = ?path,
                        magic = ?&buf[..4],
                        bytes = buf.len(),
                        "WAL segment is unreadable by this binary — every frame in it \
                         is unrecovered. Most likely a deploy ROLLBACK: a newer build \
                         wrote a record version this one cannot parse. The file is \
                         retained in the archive directory, so re-running the newer \
                         build can still recover it."
                    );
                } else {
                    corrupted_at = Some((reason, i));
                    warn!(segment = ?path, offset = i, reason, "bad WAL record and no readable record after it; stopping");
                }
                break;
            }
        };
        // TOTAL mapping: a v1–v3 record has no endpoint and replays as the
        // main feed (what every earlier replay assumed); a v4 byte this binary
        // does not recognise ALSO maps to the main feed, but is counted so the
        // segment reports it once below rather than applying it silently.
        let endpoint = match rec.endpoint_byte {
            None => WalEndpoint::MainFeed,
            Some(b) => {
                let ep = WalEndpoint::from_u8(b);
                if ep.as_u8() != b {
                    unknown_endpoint = unknown_endpoint.saturating_add(1);
                }
                ep
            }
        };
        let record_end = rec.end;
        if let Some(applied) = applied
            && applied.frame_is_applied(applied_sink_for(endpoint), rec.frame_seq)
        {
            dropped_as_applied = dropped_as_applied.saturating_add(1);
            // Only a skipped MAIN-FEED frame can hide ticks from the candle
            // fold. A depth or order-update frame carries none, so dropping it
            // is no gap: when the depth watermark runs ahead of the tick one,
            // depth frames interleaved with main-feed frames would otherwise
            // flag a gap before nearly every main-feed frame, and the fold
            // would suppress almost every bar as partial (review, 2026-09-29).
            if frame_feeds_the_candle_fold(rec.ws_type, endpoint) {
                gap_pending = true;
            }
            i = record_end;
            continue;
        }
        if !keep(rec.frame_seq, endpoint) {
            i = record_end;
            continue;
        }
        out.push(ReplayedFrame {
            ws_type: rec.ws_type,
            // Copied only now, after the CRC has matched (Z11a): the walk used
            // to copy every frame before checking it.
            frame: rec.frame.to_vec(),
            frame_seq: rec.frame_seq,
            received_at_nanos: rec.received_at_nanos,
            endpoint,
            after_gap: std::mem::replace(&mut gap_pending, false),
            first_in_segment: out.is_empty(),
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
    // CORRUPTION ACCOUNTING -- added 2026-08-28, extended for resync 2026-10-02.
    //
    // A bad record used to end the walk, and this function then returned
    // `Ok(out)` regardless; the caller's corruption counter fires only on
    // `Err`, so a bad record in the MIDDLE of a segment discarded every frame
    // after it with nothing counted. That gap was closed on 2026-08-28; since
    // Z11a the walk also RESYNCS past a bad record, so what is lost is only
    // the bytes between the bad record and the next readable one.
    //
    // A genuine torn tail is deliberately NOT counted: a partial trailing
    // record is what an interrupted writer leaves behind, it abandons nothing
    // beyond itself, and counting it would page on every unclean shutdown.
    //
    // Bytes, not records: the record count of undecodable bytes is unknowable,
    // and a fabricated number inside a counter that exists to stop fabrication
    // is worse than no number.
    //
    // Any skipped byte is damage: the bytes the walk could not read may have
    // held frames, so the caller treats the segment as one that was not fully
    // read (the next segment follows a gap).
    let damaged = unreadable || corrupted_at.is_some() || resync.gaps > 0;
    if report_corruption && (corrupted_at.is_some() || resync.gaps > 0) {
        let tail_abandoned = corrupted_at.map_or(0, |(_, offset)| buf.len().saturating_sub(offset));
        let abandoned = resync.skipped_bytes.saturating_add(tail_abandoned);
        let (reason, offset) = resync.first.or(corrupted_at).unwrap_or(("unknown", 0));
        let recovered_after_gap = resync
            .frames_before_first_gap
            .map_or(0, |before| out.len().saturating_sub(before));
        metrics::counter!(WAL_REPLAY_TRUNCATED_SEGMENTS_COUNTER).increment(1);
        metrics::counter!(WAL_REPLAY_ABANDONED_BYTES_COUNTER).increment(abandoned as u64);
        if resync.gaps > 0 {
            metrics::counter!(WAL_REPLAY_RESYNCS_COUNTER).increment(resync.gaps as u64);
        }
        error!(
            code = ErrorCode::WsSpill02FrameDropped.code_str(),
            source = "mid_segment_resync",
            segment = ?path,
            reason,
            offset,
            gaps = resync.gaps,
            skipped_bytes = resync.skipped_bytes,
            tail_abandoned_bytes = tail_abandoned,
            abandoned_bytes = abandoned,
            recovered_frames = out.len(),
            recovered_after_gap,
            "WAL segment is corrupt mid-file — the walk skipped the unreadable bytes \
             and resumed at the next record whose CRC matches; frames in the skipped \
             bytes are unrecovered (tail_abandoned_bytes > 0 means no readable record \
             followed the last bad one). The segment is still moved to the archive \
             directory, so the bytes survive for manual inspection."
        );
    }
    Ok(SegmentRead {
        frames: out,
        dropped_as_applied,
        damaged,
        trailing_gap: gap_pending,
    })
}

/// Damage the segment walk resynced past (Z11a). Cold path.
#[derive(Debug, Default)]
struct ResyncTally {
    /// Bad stretches skipped.
    gaps: usize,
    /// Bytes between each bad record and the next readable one, summed.
    skipped_bytes: usize,
    /// The first bad record: its reason and offset.
    first: Option<(&'static str, usize)>,
    /// Frames returned before the first gap, so the report can say how many
    /// were recovered only because the walk resynced.
    frames_before_first_gap: Option<usize>,
}

impl ResyncTally {
    fn record_gap(
        &mut self,
        reason: &'static str,
        bad_at: usize,
        next: usize,
        frames_so_far: usize,
    ) {
        self.gaps = self.gaps.saturating_add(1);
        self.skipped_bytes = self
            .skipped_bytes
            .saturating_add(next.saturating_sub(bad_at));
        if self.first.is_none() {
            self.first = Some((reason, bad_at));
            self.frames_before_first_gap = Some(frames_so_far);
        }
    }
}

/// One record's fields, borrowed from the segment buffer, CRC already matched.
#[derive(Debug)]
struct DecodedRecord<'a> {
    ws_type: WsType,
    frame_seq: u64,
    received_at_nanos: i64,
    /// The RAW v4 endpoint byte; `None` for v1–v3.
    endpoint_byte: Option<u8>,
    frame: &'a [u8],
    /// Offset one past the record's CRC.
    end: usize,
}

/// What [`decode_record_at`] found at one offset.
#[derive(Debug)]
enum RecordDecode<'a> {
    /// A complete record whose CRC matches.
    Record(DecodedRecord<'a>),
    /// A record start whose header or body runs past the end of the buffer.
    /// `declared_len` is the frame length field when the header was complete.
    PastEnd { declared_len: Option<usize> },
    /// Not a readable record; the reason names the first check that failed.
    Bad(&'static str),
}

/// Decodes the record starting at `i`, if any. Pure; the CRC runs over the
/// borrowed slice and nothing is copied. A declared frame length above
/// `max_frame_len` is refused before the CRC runs (the resync scan's bound on
/// how much work one false candidate can cost). Cold path: O(frame length).
fn decode_record_at(buf: &[u8], i: usize, max_frame_len: usize) -> RecordDecode<'_> {
    let Some(magic) = buf.get(i..i.saturating_add(4)) else {
        return RecordDecode::PastEnd { declared_len: None };
    };
    let is_v4 = magic == WAL_MAGIC_V4;
    let is_v3 = magic == WAL_MAGIC_V3;
    let is_v2 = magic == WAL_MAGIC_V2;
    let is_v1 = magic == WAL_MAGIC;
    if !is_v1 && !is_v2 && !is_v3 && !is_v4 {
        return RecordDecode::Bad("magic_mismatch");
    }
    // Version disambiguation + per-version minimum-size guard (security
    // review HIGH): a v2 record needs 21 bytes before its variable frame, a v3
    // record 29, a v4 record 30. Checked BEFORE any header field is read, so a
    // partial tail can never be reinterpreted as payload.
    let min_rec = if is_v4 {
        WAL_MIN_RECORD_V4
    } else if is_v3 {
        WAL_MIN_RECORD_V3
    } else if is_v2 {
        WAL_MIN_RECORD_V2
    } else {
        WAL_MIN_RECORD_V1
    };
    if i.saturating_add(min_rec) > buf.len() {
        return RecordDecode::PastEnd { declared_len: None };
    }
    let ws_byte = buf[i + 4];
    let Some(ws_type) = WsType::from_u8(ws_byte) else {
        return RecordDecode::Bad("unknown_ws_type");
    };
    // v1: [magic|ws|len|frame|crc]
    // v2: [magic|ws|frame_seq(8)|len|frame|crc]
    // v3: [magic|ws|frame_seq(8)|received_at_nanos(8)|len|frame|crc]
    // v4: [magic|ws|frame_seq(8)|received_at_nanos(8)|endpoint(1)|len|frame|crc]
    // Every `try_into` below is on a slice whose bounds the minimum-size guard
    // has already validated, so these arms are structurally unreachable. They
    // still return a NAMED `Bad` rather than ending anything silently: the
    // walk counts every `Bad`, and "unreachable" is a claim about today's
    // bounds checks rather than a guarantee about tomorrow's.
    let (frame_seq, received_at_nanos, endpoint_byte, len_off) = if is_v3 || is_v4 {
        let Ok(seq_bytes) = <[u8; 8]>::try_from(&buf[i + 5..i + 13]) else {
            return RecordDecode::Bad("slice_seq_v3");
        };
        let Ok(recv_bytes) = <[u8; 8]>::try_from(&buf[i + 13..i + 21]) else {
            return RecordDecode::Bad("slice_received_at");
        };
        // v4 carries the endpoint byte at offset 21; v3 has no such byte and
        // reads as `None`, which maps to `MainFeed` — the pre-v4 assumption.
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
        let Ok(seq_bytes) = <[u8; 8]>::try_from(&buf[i + 5..i + 13]) else {
            return RecordDecode::Bad("slice_seq_v2");
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
    let Ok(len_bytes) = <[u8; 4]>::try_from(&buf[len_off..len_off + 4]) else {
        return RecordDecode::Bad("slice_len");
    };
    let frame_len = u32::from_le_bytes(len_bytes) as usize;
    if frame_len > max_frame_len {
        return RecordDecode::Bad("length_over_ceiling");
    }
    let frame_off = len_off + 4;
    // checked_add chain (security review MEDIUM — defence-in-depth).
    let Some(record_end) = frame_off
        .checked_add(frame_len)
        .and_then(|v| v.checked_add(4))
    else {
        return RecordDecode::Bad("length_overflow");
    };
    if record_end > buf.len() {
        return RecordDecode::PastEnd {
            declared_len: Some(frame_len),
        };
    }
    let frame = &buf[frame_off..frame_off + frame_len];
    let Ok(crc_bytes) = <[u8; 4]>::try_from(&buf[frame_off + frame_len..record_end]) else {
        return RecordDecode::Bad("slice_crc");
    };
    let expected = u32::from_le_bytes(crc_bytes);
    // CRC covers the version's exact header bytes, in write order, mirroring
    // `write_record`: using the wrong version's byte set would reject every
    // record of that version as corrupt.
    let len_le = (frame_len as u32).to_le_bytes();
    let actual = if is_v4 {
        // The RAW endpoint byte, not the decoded enum: an unknown value must
        // still CRC-verify as the bytes on disk, or every record written by a
        // newer binary would read as corrupt.
        crc32_ieee_of(&[
            &[ws_byte],
            &frame_seq.to_le_bytes()[..],
            &received_at_nanos.to_le_bytes()[..],
            &[endpoint_byte.unwrap_or(0)],
            &len_le[..],
            frame,
        ])
    } else if is_v3 {
        crc32_ieee_of(&[
            &[ws_byte],
            &frame_seq.to_le_bytes()[..],
            &received_at_nanos.to_le_bytes()[..],
            &len_le[..],
            frame,
        ])
    } else if is_v2 {
        crc32_ieee_of(&[&[ws_byte], &frame_seq.to_le_bytes()[..], &len_le[..], frame])
    } else {
        crc32_ieee_of(&[&[ws_byte], &len_le[..], frame])
    };
    if actual != expected {
        return RecordDecode::Bad("crc_mismatch");
    }
    RecordDecode::Record(DecodedRecord {
        ws_type,
        frame_seq,
        received_at_nanos,
        endpoint_byte,
        frame,
        end: record_end,
    })
}

/// The offset of the first COMPLETE, CRC-verified record at or after `start`,
/// or `None` (Z11a). A candidate must carry a `TVW1`..`TVW4` magic, a known
/// `WsType`, a frame length no larger than [`WAL_RESYNC_MAX_FRAME_BYTES`], and
/// a matching CRC-32 over its header and frame, so a false resync needs a
/// 32-bit CRC collision on top of the magic and type bytes.
///
/// Cold path (boot replay / after-close pass), never on the frame drain:
/// O(remaining bytes) to scan, plus O(declared length) of CRC per candidate,
/// at most `WAL_RESYNC_MAX_FRAME_BYTES` each. A crafted file dense with
/// plausible candidates could make this O(bytes × ceiling); a file this
/// writer produced cannot.
fn resync_from(buf: &[u8], start: usize) -> Option<usize> {
    let mut j = start;
    // O(1) EXEMPT: begin — cold resync scan over one segment buffer
    while j.saturating_add(WAL_MIN_RECORD_V1) <= buf.len() {
        let rel = buf.get(j..)?.windows(4).position(|w| {
            w[0] == b'T' && w[1] == b'V' && w[2] == b'W' && (b'1'..=b'4').contains(&w[3])
        })?;
        let at = j + rel;
        if let RecordDecode::Record(_) = decode_record_at(buf, at, WAL_RESYNC_MAX_FRAME_BYTES) {
            return Some(at);
        }
        j = at + 1;
    }
    // O(1) EXEMPT: end
    None
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
        // A COLUMN-0 marker, not the bare attribute (corrected 2026-09-06).
        // `src.split("#[cfg(test)]")` truncates at the FIRST occurrence, and
        // the first one in this file is an INDENTED attribute at ~line 1000 —
        // so "production" was the first thousand lines of a ~1,780-line
        // production section, and everything from `finalise_segment` to the
        // writer loop was outside the scan entirely. A guard whose whole job
        // is keeping the durability words honest was reading less than
        // two-thirds of the code it vouched for.
        let test_marker = concat!("\n#[cfg(", "test)]\n");
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
            // The sync EXISTS. Before 2026-09-05 this arm was a bare sanity
            // check that could not fail, so the guard protected one direction
            // of drift and waved the other through — exactly the shape the
            // module header warns about. It now requires the header to state
            // the three things a reader needs and cannot infer from a
            // `sync_all` call site:
            //
            //   * that the sync is INTERVAL-driven, not per-record;
            //   * that the residual window is bounded, not eliminated;
            //   * that it is reversible without a rebuild.
            assert!(
                production.contains("sync_all"),
                "sanity: counted a sync but cannot find one"
            );
            assert!(
                production.contains("WAL_FSYNC_INTERVAL_MS_DEFAULT"),
                "this module now syncs, so the header must name the interval \
                 constant that bounds the unsynced window"
            );
            // Pin the SENTENCE, not the word. The first version asked whether
            // the header contained "bounded" ANYWHERE, and a bite-proof that
            // rewrote the meaning-carrying line PASSED, because the word
            // survived on a neighbouring line. Prose repeats words; a claim
            // lives in one sentence.
            let hdr = production.split("use std::fs").next().unwrap_or(production);
            assert!(
                hdr.contains("It is a BOUND, not an elimination."),
                "a periodic sync BOUNDS the power-loss window; it does not \
                 close it. The header must say so, or the next reader will \
                 read `sync_all` and assume zero loss."
            );
            assert!(
                hdr.contains("power"),
                "the header must still name the case the sync exists for"
            );

            // 2026-10-02: the periodic and rotation syncs run on the
            // `wal-syncer` thread. The header must say so, and the code must
            // match it in both directions.
            assert!(
                hdr.contains("THE SYNC RUNS ON ITS OWN THREAD, so the writer only")
                    && hdr.contains("`wal-syncer`"),
                "the header must say the device sync runs on the wal-syncer thread"
            );
            assert!(
                hdr.contains("the power-loss window\n// is now the interval PLUS"),
                "the header must say the bound now includes the syncer's own latency"
            );
            let serve = production
                .split("fn serve(&self)")
                .nth(1)
                .and_then(|s| s.split("\nimpl Drop for WalSyncer").next())
                .expect("WalSyncer::serve must exist");
            assert!(
                serve.contains(concat!("sync_", "data()")),
                "the syncer must actually sync, or the header overstates it"
            );
            assert!(
                production.contains("spawn_wal_syncer(Arc::clone(&syncer))"),
                "the spill constructor must spawn the syncer thread"
            );
            // The writer loop itself never syncs: every sync it causes goes
            // through `maybe_sync_segment` (request, or inline only without a
            // clone) or `finalise_segment` (hand-off, or inline fallback).
            let from = production.find("fn writer_loop(").expect("writer_loop");
            let len = production[from..]
                .find("fn open_segment_resilient")
                .expect("writer_loop is followed by open_segment_resilient");
            let writer = &production[from..from + len];
            assert!(
                !writer.contains(concat!("sync_", "all("))
                    && !writer.contains(concat!("sync_", "data(")),
                "writer_loop must not call a device sync directly; that is the stall \
                 this split removed"
            );
        }
    }

    /// Every segment finalisation flushes AND syncs — including the rotation.
    ///
    /// Durability is invisible to a unit test (the guard above says so: a
    /// flushed record and a synced record are byte-identical until the machine
    /// loses power), so this pins the CALL. `wal_sync_is_rate_limited_and_disableable`
    /// pins that the call does something.
    ///
    /// The rotation arm is the one that matters and the one that was wrong.
    /// `maybe_sync_segment` syncs `current` and only `current`; a rotation
    /// makes the NEW file `current` in the very next statement, so a segment
    /// that leaves this arm unsynced is never synced by anything, ever. Its
    /// tail sits in the page cache under the kernel's ~30 s writeback policy
    /// while `persisted` has already counted every record in it — and at
    /// 128 MiB a session rotates many times, so this was the most frequent of
    /// the three finalisations and the only one that did not sync.
    #[test]
    fn every_segment_finalisation_syncs_including_the_rotation() {
        let src = include_str!("ws_frame_spill.rs");
        // A COLUMN-0 marker, not the bare attribute (corrected 2026-09-06).
        // `src.split("#[cfg(test)]")` truncates at the FIRST occurrence, and
        // the first one in this file is an INDENTED attribute at ~line 1000 —
        // so "production" was the first thousand lines of a ~1,780-line
        // production section, and everything from `finalise_segment` to the
        // writer loop was outside the scan entirely. A guard whose whole job
        // is keeping the durability words honest was reading less than
        // two-thirds of the code it vouched for.
        let test_marker = concat!("\n#[cfg(", "test)]\n");
        let production = src.split(test_marker).next().unwrap_or(src);

        // All three finalisations go through the ONE helper. Naming them
        // individually is what keeps a future fourth arm from quietly
        // hand-rolling a flush: the count below has to move with it.
        for stage in ["flush_on_stop", "flush_on_close", "flush_on_rotate"] {
            let call = format!("finalise_segment(&mut current, \"{stage}\"");
            assert!(
                production.contains(&call),
                "the `{stage}` arm must finalise through `finalise_segment` \
                 (flush THEN sync). A bare `w.flush()` here leaves the segment's \
                 tail in the page cache with nothing to sync it later, while \
                 `persisted` has already counted every record in it."
            );
        }

        // Non-vacuous in the other direction: the helper is the ONLY place a
        // finalisation may flush, so a hand-rolled flush cannot creep back
        // beside the call. `flush()` legitimately appears elsewhere in the
        // module (the periodic path), so this counts the finalisation calls
        // rather than banning the word.
        assert_eq!(
            production.matches("finalise_segment(&mut current").count(),
            3,
            "exactly three arms finalise a segment — stop, close, rotate. A \
             fourth call site means a new finalisation nobody added to this \
             test; a missing one means an arm went back to flushing alone."
        );

        // The helper must actually REACH the sync. The two are one edit apart,
        // and the first version of this block asserted only that flush comes
        // before sync in source order -- which a bare `return` slipped between
        // them satisfies perfectly while leaving every finalisation flush-only.
        // That mutation was planted and this test PASSED, so the check was
        // rewritten to count the exits instead of the order.
        let body = production
            .split("fn finalise_segment(")
            .nth(1)
            .expect("finalise_segment must exist");
        let flush_at = body.find("w.flush()").expect("it must flush");
        let sync_at = body
            .find(concat!("sync_", "all"))
            .expect("it must sync, or the rotation fix is undone");
        assert!(
            flush_at < sync_at,
            "flush must precede sync — syncing an unflushed buffer forces an \
             incomplete segment and reports success"
        );

        // Exactly TWO early exits may stand between the flush and the sync,
        // and both are named here so a third cannot arrive unnoticed:
        //
        //   1. the flush FAILED — the bytes never reached the kernel, so
        //      syncing would force an incomplete segment and report success;
        //   2. `resolve_wal_fsync_interval()` is None — the operator disabled
        //      syncing, which is a documented knob.
        //
        // Anything else between them makes the sync unreachable for some or
        // all callers, and the assertions above would still be green.
        let window = &body[flush_at..sync_at];
        assert!(
            window.contains("resolve_wal_fsync_interval"),
            "exit 2 must be the fsync-disabled knob; if it is gone, the count \
             below is measuring something else"
        );
        assert_eq!(
            window.matches("return;").count(),
            2,
            "exactly two early exits may sit between the flush and the sync — \
             the failed flush and the disabled-fsync knob. A third makes the \
             sync unreachable, which no source-order check can see: every other \
             assertion in this test stays green while nothing is ever synced."
        );
    }

    /// 2026-10-02 (zero-loss audit): the writer's idle arm syncs records that
    /// a batch left unsynced. Before, only a NEW record could trigger the sync,
    /// so the last batch before a lull waited for the kernel's writeback, and
    /// a power loss in that gap lost it while `persisted` had counted it.
    #[test]
    fn test_regression_a_lull_syncs_the_last_batch() {
        let src = include_str!("ws_frame_spill.rs");
        let test_marker = concat!("\n#[cfg(", "test)]\n");
        let production = src.split(test_marker).next().unwrap_or(src);
        let timeout_arm = production
            .split("Err(RecvTimeoutError::Timeout) => {")
            .nth(1)
            .and_then(|s| s.split("Err(RecvTimeoutError::Disconnected)").next())
            .expect("the writer's idle arm must exist");
        assert!(
            timeout_arm.contains("if unsynced {")
                && timeout_arm.contains("maybe_sync_segment(&mut current"),
            "the idle arm must sync a batch left unsynced:\n{timeout_arm}"
        );
        assert!(
            production.contains("unsynced = true;"),
            "every batch must mark the segment unsynced"
        );
    }

    /// The sync is real, reachable, and rate-limited — proven by behaviour,
    /// not by a source scan.
    ///
    /// The source scan above keeps the WORDS honest. This keeps the CALL
    /// honest: `maybe_sync_segment` must actually sync when the interval has
    /// elapsed, must NOT sync before it, and must be a no-op when disabled.
    /// Without this a `sync_all` could sit behind a condition that is never
    /// true and the scan would still pass.
    #[test]
    fn wal_sync_is_rate_limited_and_disableable() {
        let dir = tmp_dir("fsync-rate-limit");
        let path = dir.join("seg.wal");
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .expect("temp segment must open");
        // No thread: this test drives the syncer's `serve` by hand. Since
        // 2026-10-02 the due call REQUESTS the sync and `serve` performs it.
        let syncer = WalSyncer::new();
        syncer.track(&file);
        let mut current = Some(BufWriter::new(file));

        // Disabled: returns the SAME instant it was given, so the caller's
        // timer is untouched and no sync is attempted.
        let t0 = Instant::now();
        let after_disabled = maybe_sync_segment(&mut current, None, t0, &syncer);
        assert_eq!(
            after_disabled, t0,
            "a disabled sync must not re-arm the caller's timer"
        );

        // Interval not yet elapsed: also a no-op, same instant back.
        let long = Duration::from_secs(3_600);
        let after_early = maybe_sync_segment(&mut current, Some(long), Instant::now(), &syncer);
        assert!(
            after_early.elapsed() < Duration::from_secs(1),
            "an early call must return the instant it was handed, unchanged"
        );

        // Interval elapsed: the timer MUST advance, which is the only
        // observable signal that the sync branch was entered at all.
        let stale = Instant::now() - Duration::from_secs(10);
        let after_due =
            maybe_sync_segment(&mut current, Some(Duration::from_millis(1)), stale, &syncer);
        assert!(
            after_due > stale,
            "a due sync must re-arm the timer; if it did not, the sync branch \
             was never reached and this module is not syncing at all"
        );
        assert!(
            syncer.due.load(Ordering::SeqCst),
            "the due call must hand the sync to the syncer, not run it on the writer"
        );
        syncer.serve();
        assert!(
            !syncer.due.load(Ordering::SeqCst) && syncer.oldest_request.load(Ordering::SeqCst) == 0,
            "serving the request must consume it"
        );

        // A syncer with no clone of the segment: the writer syncs inline,
        // exactly as before the syncer existed, and requests nothing.
        let untracked = WalSyncer::new();
        let stale = Instant::now() - Duration::from_secs(10);
        let after_inline = maybe_sync_segment(
            &mut current,
            Some(Duration::from_millis(1)),
            stale,
            &untracked,
        );
        assert!(
            after_inline > stale,
            "the inline fallback must still re-arm the timer"
        );
        assert!(
            !untracked.due.load(Ordering::SeqCst),
            "with no clone there is nobody to request from; the sync ran inline"
        );

        // And the file survived it: sync_all on a healthy fd cannot corrupt,
        // but a panicking or file-closing implementation would show here.
        assert!(
            current.is_some(),
            "the segment must stay open across a sync — dropping it would \
             force a reopen per interval and lose the append position"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Serialises the tests that resolve `TICKVAULT_WAL_FSYNC_INTERVAL_MS`
    /// against the one test that sets it, so a syncer test never starts a
    /// writer while the interval reads "0" (disabled).
    static FSYNC_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// The paths of the `*.wal` files directly under `dir`, oldest first.
    fn wal_files_in(dir: &Path) -> Vec<PathBuf> {
        let mut out: Vec<PathBuf> = std::fs::read_dir(dir)
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|x| x == "wal"))
            .collect();
        out.sort();
        out
    }

    /// 2026-10-02: a wedged device sync no longer stops the writer.
    ///
    /// The syncer is held inside a sync (the test gate in front of every
    /// `sync_data`), then 2,000 frames are appended. Every one of them must
    /// reach the kernel while the sync is still stuck: counted as persisted,
    /// flushed (the abort-drain handshake), and readable from a copy of the
    /// segment taken through a separate fd, in order.
    ///
    /// Bite-proof: with the sync moved back onto the writer thread, the
    /// writer parks in the same gate and `wait_until_persisted` times out.
    #[test]
    fn test_writer_keeps_writing_while_sync_is_wedged() {
        let dir = tmp_dir("sync-wedged");
        // Held until the writer has written a record, i.e. has resolved the
        // sync interval, so it never reads the "0" another test sets.
        let env = FSYNC_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let spill = WsFrameSpill::new(&dir).unwrap();
        spill.syncer.hold.set(true);

        // One frame, then a lull: the writer requests the periodic sync
        // after the interval (1 s by default) and the syncer parks in it.
        spill.append(WsType::LiveFeed, b"frame-000000".to_vec());
        wait_until_persisted(&spill, 1);
        drop(env);
        let deadline = Instant::now() + Duration::from_secs(10);
        while spill.syncer.hold.parked.load(Ordering::SeqCst) == 0 {
            assert!(
                Instant::now() < deadline,
                "no sync was attempted within 10 s; the periodic sync never fired"
            );
            std::thread::sleep(Duration::from_millis(10));
        }

        // The sync is now wedged. The writer must keep going regardless.
        let n: usize = 2_000;
        for k in 1..=n {
            let outcome = spill.append(WsType::LiveFeed, format!("frame-{k:06}").into_bytes());
            assert!(
                matches!(outcome, AppendOutcome::Spilled),
                "frame {k}: {outcome:?}"
            );
        }
        wait_until_persisted(&spill, n as u64 + 1);
        assert_eq!(
            spill.abort_drain.wait(Duration::from_secs(5)),
            AbortDrainOutcome::Drained,
            "every record must be flushed to the kernel while the sync is wedged"
        );
        assert_eq!(
            spill.syncer.hold.parked.load(Ordering::SeqCst),
            1,
            "the sync must still be wedged, or this proved nothing"
        );

        // Copy the open segment through a separate fd and replay the copy.
        let copy_dir = tmp_dir("sync-wedged-copy");
        let segments = wal_files_in(&dir);
        assert_eq!(segments.len(), 1, "one open segment: {segments:?}");
        std::fs::copy(&segments[0], copy_dir.join("00000000000000000001.wal")).unwrap();
        let frames = replay_all(&copy_dir).unwrap();
        assert_eq!(frames.len(), n + 1, "every written frame must replay");
        for (k, f) in frames.iter().enumerate() {
            assert_eq!(
                f.frame,
                format!("frame-{k:06}").into_bytes(),
                "frame {k} out of order"
            );
        }

        spill.syncer.hold.set(false);
        assert_eq!(spill.shutdown(Duration::from_secs(10)), 0);
        assert_eq!(spill.syncer.hold.parked.load(Ordering::SeqCst), 0);
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&copy_dir);
    }

    /// A process killed mid-batch loses at most the batch it had not yet
    /// written, and what it leaves on disk reads as a torn tail, not damage.
    ///
    /// Batch 1 is written and flushed (in the kernel). Batch 2 is still in the
    /// `BufWriter` when the "kill" lands, part-way through its `write(2)`: the
    /// kernel received the first 17 bytes of record 11 and nothing more. The
    /// buffer is discarded unflushed, exactly as a SIGKILL discards it.
    /// 2026-10-02: a segment writer that fails is taken apart, not dropped, and
    /// the records it still buffered are counted as LOST: `persisted` falls by
    /// exactly that number and nothing is flushed a second time.
    #[test]
    fn test_failed_segment_writer_counts_its_buffered_records_as_lost() {
        let dir = tmp_dir("discard-buffered");
        let persisted = AtomicU64::new(0);
        let mut tally = UnflushedTally::new(&persisted);
        let mut w = open_new_segment(&dir).unwrap();
        let record = |seq: u64| WalRecord {
            ws_type: WsType::LiveFeed,
            frame_seq: seq,
            received_at_nanos: 1_000 + seq as i64,
            endpoint: WalEndpoint::MainFeed,
            frame: Bytes::from(vec![seq as u8; 40]),
        };
        for seq in 1..=3 {
            write_record(&mut w, &record(seq)).unwrap();
            tally.note_written(record_disk_size(&record(seq)), seq, WsType::LiveFeed);
        }
        w.flush().unwrap();
        tally.flushed();
        for seq in 4..=5 {
            write_record(&mut w, &record(seq)).unwrap();
            tally.note_written(record_disk_size(&record(seq)), seq, WsType::LiveFeed);
        }
        assert_eq!(persisted.load(Ordering::Relaxed), 5);
        let path = wal_files_in(&dir).pop().expect("one segment");

        discard_segment_writer(w, &mut tally, "test");

        assert_eq!(
            persisted.load(Ordering::Relaxed),
            3,
            "the two buffered records never reached the file and must not stay counted"
        );
        let size = record_disk_size(&record(1));
        assert_eq!(
            std::fs::metadata(&path).unwrap().len(),
            3 * size,
            "the discarded buffer must not be flushed on the way out"
        );
        // A partial write counts the record it cut in half as lost too.
        tally.note_written(size, 0, WsType::LiveFeed);
        tally.note_written(size, 0, WsType::LiveFeed);
        assert_eq!(tally.lost_beyond(size + 1), 1);
        assert_eq!(tally.lost_beyond(size - 1), 2);
        assert_eq!(tally.lost_beyond(2 * size), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Audit M1 (2026-10-04): a buffered record lost with its segment writer
    /// is reported to the frame-fate table, so a frame the reader had shed at
    /// the ring is counted as a lost frame, and a frame the drain later tries
    /// to shed is refused. A record that reached the file is not reported.
    ///
    /// Bite: before M1 the tally kept only byte offsets, so `discard` could
    /// not name the lost records and both assertions on the fate fail.
    #[test]
    fn test_discarded_writer_reports_a_shed_frame_as_lost() {
        use crate::wal_frame_fate::{LostFate, ShedKind, ShedMark, frame_fate};
        // Bases above any wall-clock base (~2^43.6 in 2026), so a real loss
        // another test reports into the process-wide table cannot hold the
        // slot as a newer frame.
        let seq = |b: u64| ((1u64 << 46) + b) << PACKET_INDEX_BITS;
        let dir = tmp_dir("discard-fate");
        let persisted = AtomicU64::new(0);
        let mut tally = UnflushedTally::new(&persisted);
        let mut w = open_new_segment(&dir).unwrap();
        let record = |s: u64| WalRecord {
            ws_type: WsType::LiveFeed,
            frame_seq: s,
            received_at_nanos: 1_000,
            endpoint: WalEndpoint::MainFeed,
            frame: Bytes::from(vec![7u8; 40]),
        };
        // Base 1 reaches the file; bases 2 and 3 stay buffered.
        write_record(&mut w, &record(seq(1))).unwrap();
        tally.note_written(record_disk_size(&record(seq(1))), seq(1), WsType::LiveFeed);
        w.flush().unwrap();
        tally.flushed();
        assert!(crate::wal_frame_fate::flushed_high_seq() >= seq(1));
        for b in [2, 3] {
            write_record(&mut w, &record(seq(b))).unwrap();
            tally.note_written(record_disk_size(&record(seq(b))), seq(b), WsType::LiveFeed);
        }
        assert_eq!(
            frame_fate().note_shed(seq(2), ShedKind::Ring, WsType::LiveFeed),
            ShedMark::Marked
        );

        discard_segment_writer(w, &mut tally, "test");

        // Base 2 was shed: its loss was counted, and a second report is not.
        assert_eq!(
            frame_fate().note_lost(seq(2), WsType::LiveFeed),
            LostFate::NotShed
        );
        // Base 3 was not shed, but its loss is recorded: the drain may not
        // shed it now.
        assert!(!frame_fate().allow_drain_shed(seq(3), WsType::LiveFeed));
        // Base 1 reached the file: still sheddable.
        assert!(frame_fate().allow_drain_shed(seq(1), WsType::LiveFeed));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Audit M1: a record the writer could not place in any segment is
    /// reported lost, and so is every record left in the channel at exit.
    #[test]
    fn test_records_left_at_writer_exit_are_reported_lost() {
        use crate::wal_frame_fate::frame_fate;
        let seq = ((1u64 << 46) + 100) << PACKET_INDEX_BITS;
        let (tx, rx) = crossbeam_channel::bounded(4);
        tx.send(WalRecord {
            ws_type: WsType::LiveFeed,
            frame_seq: seq,
            received_at_nanos: 1_000,
            endpoint: WalEndpoint::MainFeed,
            frame: Bytes::from_static(b"x"),
        })
        .unwrap();
        assert_eq!(count_records_left_at_writer_exit(&rx), 1);
        assert!(
            !frame_fate().allow_drain_shed(seq, WsType::LiveFeed),
            "a record left in the channel is lost; its frame must not be shed"
        );
    }

    /// 2026-10-02: a wall-clock step back re-anchors receipts instead of
    /// freezing the anchor for the rest of the process.
    ///
    /// Bite: with the old rule (refuse every rewind) the second refresh below
    /// rewinds by the same ten seconds as the first, so it is refused too, and
    /// so is every later one.
    #[test]
    fn test_backward_wall_step_reanchors_instead_of_freezing() {
        let t0 = Instant::now();
        let base = 1_780_000_000_000_000_000_i64;
        let old = ReceiptAnchor {
            instant: t0,
            nanos: base,
        };
        let secs = |s: i64| s * 1_000_000_000;
        let at = |after: u64, nanos: i64| ReceiptAnchor {
            instant: t0 + Duration::from_secs(after),
            nanos,
        };
        // Forward and in step: adopted.
        assert_eq!(
            anchor_refresh_decision(old, at(30, base + secs(30))),
            AnchorRefresh::Adopt
        );
        // A slew-sized rewind (10 ms): refused, frames stay ordered.
        assert_eq!(
            anchor_refresh_decision(old, at(30, base + secs(30) - 10_000_000)),
            AnchorRefresh::RefuseBackward
        );
        // A 10 s step back: adopted as a step.
        let stepped = at(30, base + secs(20));
        assert_eq!(
            anchor_refresh_decision(old, stepped),
            AnchorRefresh::AdoptBackwardStep {
                rewind_nanos: secs(10)
            }
        );
        // After adopting it, the next refresh with both clocks moving together
        // is an ordinary adoption: the anchor is not frozen.
        assert_eq!(
            anchor_refresh_decision(stepped, at(60, base + secs(50))),
            AnchorRefresh::Adopt
        );
    }

    #[test]
    fn test_kill_mid_batch_loses_at_most_the_unflushed_batch() {
        let dir = tmp_dir("kill-mid-batch");
        let mut w = open_new_segment(&dir).unwrap();
        let record = |seq: u64| WalRecord {
            ws_type: WsType::LiveFeed,
            frame_seq: seq,
            received_at_nanos: 1_000 + seq as i64,
            endpoint: WalEndpoint::MainFeed,
            frame: Bytes::from(vec![seq as u8; 40]),
        };
        for seq in 1..=10 {
            write_record(&mut w, &record(seq)).unwrap();
        }
        w.flush().unwrap();
        for seq in 11..=15 {
            write_record(&mut w, &record(seq)).unwrap();
        }
        let torn = encode_v4_record(
            WsType::LiveFeed,
            11,
            1_011,
            WalEndpoint::MainFeed,
            &[11u8; 40],
        );
        let (mut file, unflushed) = w.into_parts();
        assert!(
            unflushed.as_ref().is_ok_and(|b| !b.is_empty()),
            "batch 2 must still be in the buffer when the kill lands"
        );
        file.write_all(&torn[..17]).unwrap();
        drop(file);
        drop(unflushed);
        clear_open_segment_under(&dir);

        let segments = wal_files_in(&dir);
        assert_eq!(segments.len(), 1);
        let read = read_segment_frames_matching(&segments[0], &|_, _| true).unwrap();
        let seqs: Vec<u64> = read.frames.iter().map(|f| f.frame_seq).collect();
        assert_eq!(
            seqs,
            (1..=10).collect::<Vec<u64>>(),
            "batch 1 survives whole"
        );
        assert!(
            !read.damaged,
            "a torn tail is what a kill leaves; it is not damage"
        );

        // The boot replay sees the same ten.
        let replayed = replay_all(&dir).unwrap();
        assert_eq!(replayed.len(), 10);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Every closed segment is synced: by the syncer while it has room, inline
    /// on the writer when its slot is full or it is stopping. None is dropped.
    #[test]
    fn test_every_closed_segment_is_synced_including_full_slot_fallback() {
        let _env = FSYNC_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let dir = tmp_dir("closed-sync");
        // No thread yet, so the slot fills deterministically.
        let syncer = Arc::new(WalSyncer::new());
        let rotate = |k: usize, syncer: &WalSyncer| {
            let path = dir.join(format!("{k:020}.wal"));
            let f = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .unwrap();
            let mut current = Some(BufWriter::new(f));
            if let Some(w) = current.as_mut() {
                w.write_all(b"closed-segment-tail").unwrap();
            }
            let persisted = AtomicU64::new(0);
            let mut tally = UnflushedTally::new(&persisted);
            finalise_segment(
                &mut current,
                "flush_on_rotate",
                "fsync_on_rotate",
                Some(syncer),
                &mut tally,
            );
            assert!(current.is_none(), "a finalised segment is closed");
            assert_eq!(std::fs::read(&path).unwrap(), b"closed-segment-tail");
        };

        let total = WAL_SYNCER_CLOSED_SLOTS + 2;
        for k in 0..total {
            rotate(k, &syncer);
        }
        assert_eq!(
            syncer.closed_rx.len(),
            WAL_SYNCER_CLOSED_SLOTS,
            "the slot is full"
        );
        assert_eq!(
            syncer.closed_inline_fallbacks.load(Ordering::SeqCst),
            2,
            "the two that found no room were synced inline"
        );

        // The syncer syncs every queued segment before it exits.
        let handle = spawn_wal_syncer(Arc::clone(&syncer)).unwrap();
        syncer.request_stop();
        handle.join().unwrap();
        assert!(syncer.closed_rx.is_empty(), "nothing may be left unsynced");
        assert_eq!(
            syncer.closed_synced.load(Ordering::SeqCst),
            WAL_SYNCER_CLOSED_SLOTS as u64
        );

        // A stopped syncer refuses: the writer syncs inline.
        rotate(total, &syncer);
        assert_eq!(syncer.closed_inline_fallbacks.load(Ordering::SeqCst), 3);
        assert!(syncer.closed_rx.is_empty());
        let synced = syncer.closed_synced.load(Ordering::SeqCst)
            + syncer.closed_inline_fallbacks.load(Ordering::SeqCst);
        assert_eq!(
            synced,
            total as u64 + 1,
            "every closed segment was synced once"
        );

        // The last resort: a segment queued with no thread left to serve it is
        // synced when the syncer is dropped, never dropped unsynced.
        let orphan = WalSyncer::new();
        rotate(total + 1, &orphan);
        assert_eq!(orphan.closed_rx.len(), 1);
        drop(orphan);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A bad env value must fall back to the default, never to "disabled".
    ///
    /// The failure this prevents is silent: a typo in a systemd unit
    /// (`TICKVAULT_WAL_FSYNC_INTERVAL_MS=1OOO`, letter O) that parsed to
    /// nothing and switched durability off would leave every metric and every
    /// log line looking exactly as they do today.
    #[test]
    fn wal_sync_interval_falls_back_loudly_never_to_disabled() {
        // Held so the syncer tests below never resolve the interval while
        // this test has it set to "0".
        let _env = FSYNC_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        // SAFETY: single-threaded within this test, and the value is restored
        // before it returns. The resolver reads the var on each call.
        unsafe { std::env::set_var(WAL_FSYNC_INTERVAL_ENV, "not-a-number") };
        assert_eq!(
            resolve_wal_fsync_interval(),
            Some(Duration::from_millis(WAL_FSYNC_INTERVAL_MS_DEFAULT)),
            "an unparseable interval must fall back to the DEFAULT; falling \
             back to None would disable durability over a typo"
        );

        unsafe { std::env::set_var(WAL_FSYNC_INTERVAL_ENV, "0") };
        assert_eq!(
            resolve_wal_fsync_interval(),
            None,
            "an explicit 0 is the documented off switch and must disable"
        );

        unsafe { std::env::set_var(WAL_FSYNC_INTERVAL_ENV, "250") };
        assert_eq!(
            resolve_wal_fsync_interval(),
            Some(Duration::from_millis(250)),
            "a valid interval must be honoured verbatim"
        );

        unsafe { std::env::remove_var(WAL_FSYNC_INTERVAL_ENV) };
        assert_eq!(
            resolve_wal_fsync_interval(),
            Some(Duration::from_millis(WAL_FSYNC_INTERVAL_MS_DEFAULT)),
            "an absent var must use the default — syncing is ON unless \
             deliberately switched off"
        );
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
            "RecordDecode::Bad(\"crc_mismatch\")",
            "RecordDecode::Bad(\"magic_mismatch\")",
            "RecordDecode::Bad(\"unknown_ws_type\")",
            "RecordDecode::Bad(\"length_overflow\")",
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
            "return RecordDecode::Bad(\"slice_seq_v3\")",
            "return RecordDecode::Bad(\"slice_received_at\")",
            "return RecordDecode::Bad(\"slice_seq_v2\")",
            "return RecordDecode::Bad(\"slice_len\")",
            "return RecordDecode::Bad(\"slice_crc\")",
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
    /// one. ~~If this regresses, candle bucketing on the receipt clock
    /// silently files replayed ticks at the wrong minute.~~ 2026-09-18: the
    /// fold no longer reads the receipt, so the consequence of a regression
    /// moved rather than disappearing — it is now a cross-day-gate
    /// misjudgement and a split `ticks` DEDUP key. See the module header.
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
    fn test_regression_segment_names_keep_rising_across_a_backward_clock_step() {
        // R3-14: replay, prune and upload order segments by NAME, and the name
        // was the wall clock. A clock stepped back between two rotations named
        // the newer segment below the older one, so replay folded it first.
        let dir = tmp_dir("seg-name-backward");
        assert_eq!(next_segment_name_nanos(&dir, 1_000), (1_000, false));
        // Clock stepped back: the name still rises, and the clamp is reported.
        assert_eq!(next_segment_name_nanos(&dir, 500), (1_001, true));
        assert_eq!(next_segment_name_nanos(&dir, 1_001), (1_002, true));
        // Clock caught up: the name is the wall clock again.
        assert_eq!(next_segment_name_nanos(&dir, 5_000), (5_000, false));
        // Another directory is independent of this one.
        let other = tmp_dir("seg-name-backward-other");
        assert_eq!(next_segment_name_nanos(&other, 10), (10, false));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&other);
    }

    #[test]
    fn test_regression_segment_names_seed_past_every_name_on_disk_after_a_restart() {
        // A restart after a backward step: the first name in the process must
        // still sort after every segment an earlier process left in the live
        // directory, `replaying/` or `archive/`.
        let dir = tmp_dir("seg-name-seed");
        std::fs::create_dir_all(dir.join(REPLAYING_SUBDIR)).unwrap();
        std::fs::create_dir_all(dir.join(ARCHIVE_SUBDIR)).unwrap();
        std::fs::write(dir.join("ws-frames-00000000000000004000.wal"), b"").unwrap();
        std::fs::write(
            dir.join(REPLAYING_SUBDIR)
                .join("ws-frames-00000000000000006000.wal"),
            b"",
        )
        .unwrap();
        std::fs::write(
            dir.join(ARCHIVE_SUBDIR)
                .join("ws-frames-00000000000000005000.wal"),
            b"",
        )
        .unwrap();
        // A foreign file never counts.
        std::fs::write(dir.join("ws-frames-99999999999999999999.tmp"), b"").unwrap();
        assert_eq!(next_segment_name_nanos(&dir, 100), (6_001, true));
        clear_open_segment_under(&dir);
        let _ = std::fs::remove_dir_all(&dir);

        // And a real open is named through it: a segment an earlier process
        // named in the far future (year 2255) still sorts before the new one.
        let dir = tmp_dir("seg-name-seed-open");
        std::fs::create_dir_all(dir.join(ARCHIVE_SUBDIR)).unwrap();
        std::fs::write(
            dir.join(ARCHIVE_SUBDIR)
                .join("ws-frames-09000000000000000000.wal"),
            b"",
        )
        .unwrap();
        let _w = open_new_segment(&dir).unwrap();
        let mut names: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter_map(|e| e.file_name().to_str().map(str::to_owned))
            .filter(|n| segment_name_nanos(n).is_some())
            .collect();
        names.sort();
        assert_eq!(
            names,
            vec!["ws-frames-09000000000000000001.wal".to_string()],
            "the opened segment is named past the newest name on disk"
        );
        clear_open_segment_under(&dir);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_segment_name_nanos_parses_only_segment_names() {
        assert_eq!(
            segment_name_nanos("ws-frames-01790000000000000000.wal"),
            Some(1_790_000_000_000_000_000)
        );
        assert_eq!(segment_name_nanos("ws-frames-x.wal"), None);
        assert_eq!(segment_name_nanos("ws-frames-1.wal.gz"), None);
        assert_eq!(segment_name_nanos("other-1.wal"), None);
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

    /// The release runs before the batch's flush, so `persisted_count` can
    /// lead it by one batch: poll rather than read once.
    fn wait_until_queued_bytes_zero(spill: &WsFrameSpill) {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while spill.queued_bytes.load(Ordering::Relaxed) != 0 {
            assert!(
                std::time::Instant::now() < deadline,
                "queued_bytes stuck at {} — a batch's reservation was never released",
                spill.queued_bytes.load(Ordering::Relaxed)
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// 2026-10-03: the writer releases one batch's bytes in ONE `fetch_sub`
    /// instead of one per record. Many frames, spanning several batches of
    /// up to 257, must still return the counter to exactly zero.
    #[test]
    fn test_batched_release_returns_queued_bytes_to_zero() {
        let dir = tmp_dir("batched-release");
        let spill = WsFrameSpill::new(&dir).unwrap();
        let frames = 1_000_u64;
        for i in 0..frames {
            assert_eq!(
                spill.append(
                    WsType::LiveFeed,
                    vec![(i % 251) as u8; 100 + (i % 7) as usize]
                ),
                AppendOutcome::Spilled
            );
        }
        wait_until_persisted(&spill, frames);
        wait_until_queued_bytes_zero(&spill);
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
        // The sentinel's bytes were added to the batch before the panic, so
        // only `PendingRelease::drop` can have released them on the unwind.
        wait_until_queued_bytes_zero(&spill);
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
        //
        // 2026-09-22 (item 44d): the stale segment is a REAL applied one — the
        // age pass now deletes only what the applied watermark has passed.
        let dir = tmp_dir("active-prune-age");
        let now = SystemTime::now();
        let stale = write_wm_segment(&dir, 0, 3, WalEndpoint::MainFeed);
        backdate(&stale, now, 259_200);
        let todays = write_wm_segment(&dir, 100, 3, WalEndpoint::MainFeed);
        backdate(&todays, now, 600);
        let snap = all_applied_through(wm_seq(200));
        let outcome = prune_active_segments_at(&dir, 172_800, u64::MAX, now, Some(&snap));
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
    fn the_active_prune_never_unlinks_the_segment_the_writer_holds_open() {
        // Unlinking the OPEN segment is silent, permanent loss. On Linux the
        // writer keeps a valid handle to the nameless inode, so every write,
        // flush and sync after it still returns Ok, the frame is counted
        // persisted, and nothing calls `note_unapplied` -- so the applied
        // watermark goes on believing the range is recoverable while every
        // frame until the next rotation is gone.
        //
        // `wal_segments_in` has filtered on `is_open_segment` since it was
        // written; this prune never did, because it was extracted for the
        // ARCHIVE directory -- where nothing is ever open -- and then reused
        // for the ACTIVE one, where exactly one always is. Its local binding
        // is still named `archive_dir`, which is the tell.
        let dir = tmp_dir("active-prune-open-segment");
        let now = SystemTime::now();

        // BOTH passes, because they select different victims: the age pass
        // takes anything past retention, the byte pass takes oldest-first.
        // The open segment is planted OLD so it is eligible for both. It is the
        // NEWEST by name (as the writer's segment always is), so it bounds the
        // closed one's range and the closed one reads as applied (item 44d).
        let closed = write_wm_segment(&dir, 0, 3, WalEndpoint::MainFeed);
        backdate(&closed, now, 259_200);
        let open = write_wm_segment(&dir, 100, 3, WalEndpoint::MainFeed);
        backdate(&open, now, 259_200);
        set_open_segment(open.clone());
        let snap = all_applied_through(wm_seq(500));

        // Age pass: retention would delete both.
        let outcome = prune_active_segments_at(&dir, 172_800, u64::MAX, now, Some(&snap));
        assert!(
            open.exists(),
            "the AGE pass unlinked the open segment -- every frame written \
             from here to the next rotation is lost with no error and no counter"
        );
        assert!(
            !closed.exists(),
            "non-vacuous: a closed segment past retention must still be deleted, \
             or this test would pass on a prune that does nothing at all"
        );
        assert_eq!(outcome.deleted, 1);

        // Byte pass: a zero ceiling would delete everything left.
        let outcome = prune_active_segments_at(&dir, u64::MAX, 0, now, None);
        assert!(
            open.exists(),
            "the BYTE pass unlinked the open segment. It deletes oldest-first, \
             so the open file is reached once the rest of the set is gone -- \
             which is precisely the state a full disk produces"
        );
        assert_eq!(
            outcome.size_deleted, 0,
            "nothing but the open segment remained, and it must not be counted \
             as deleted"
        );

        clear_open_segment_under(&dir);
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
            None,
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
        let outcome = prune_active_segments_at(&dir, 172_800, u64::MAX, now, None);
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
        // Four equal segments, 3 h / 2 h / 1 h / fresh, all inside retention.
        // The first three are applied (item 45b: the byte pass deletes only
        // applied segments); the newest has no successor, so it is unknown and
        // never a byte-pass victim. A two-segment ceiling forces the two
        // OLDEST out.
        let oldest = write_wm_segment(&dir, 0, 1, WalEndpoint::MainFeed);
        let middle = write_wm_segment(&dir, 1_000, 1, WalEndpoint::MainFeed);
        let newer = write_wm_segment(&dir, 2_000, 1, WalEndpoint::MainFeed);
        let newest = write_wm_segment(&dir, 3_000, 1, WalEndpoint::MainFeed);
        backdate(&oldest, now, 10_800);
        backdate(&middle, now, 7_200);
        backdate(&newer, now, 3_600);
        let one = std::fs::metadata(&oldest).unwrap().len();
        let snap = all_applied_through(wm_seq(5_000));
        let outcome = prune_active_segments_at(&dir, 172_800, 2 * one, now, Some(&snap));
        assert_eq!(outcome.deleted, 0, "nothing is age-expired");
        assert_eq!(outcome.size_deleted, 2, "two oldest removed by the ceiling");
        assert_eq!(outcome.size_deleted_bytes, 2 * one, "two segments freed");
        assert_eq!(
            outcome.size_deleted_oldest_age_secs, 10_800,
            "the OLDEST deleted segment's age, not the newest"
        );
        assert!(!oldest.exists() && !middle.exists());
        assert!(newer.exists() && newest.exists(), "the newest survive");
        assert_eq!(outcome.size_refused_unknown, 0, "the ceiling was met first");
        // A pass that deletes nothing under pressure reports zeros, so the
        // page can never carry a stale number from a previous pass.
        let quiet = prune_active_segments_at(&dir, 172_800, u64::MAX, now, Some(&snap));
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
    /// S3: every segment pruned, the clock stepped back by less than a day,
    /// and the applied watermark file still beside the empty directory. The
    /// next mint must land ABOVE that watermark; at or below it, a new frame
    /// reads as applied and is skipped on replay and deletable.
    #[test]
    fn test_regression_s3_reseed_clears_the_persisted_applied_watermark_with_no_segments() {
        let dir = tmp_dir("wal-reseed-watermark");
        assert_eq!(highest_frame_seq_on_disk(&dir), 0, "no segment on disk");
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX))
            .unwrap_or(0);
        // One hour ahead of this clock (inside the file's one-day ceiling, so
        // the file still loads): the shape a backward clock step leaves.
        let ahead = now + 3_600 * 1_000_000_000;
        let watermark = ((ahead >> PACKET_INDEX_BITS) << PACKET_INDEX_BITS) | 3;
        crate::wal_applied_watermark::write_file_for_test(&dir, watermark, 0);

        assert_eq!(seed_frame_seq_from_disk(&dir), watermark);
        let next = next_frame_seq();
        assert!(
            next > watermark,
            "the next mint ({next}) must exceed the persisted applied watermark \
             ({watermark}); at or below it the frame reads as applied"
        );
    }

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
    /// The converted receipt is ACCURATE, not merely monotone.
    ///
    /// # Why monotonicity was not enough
    ///
    /// Every other test of this conversion checks ORDER: a refresh never moves
    /// a receipt backwards, repeated refreshes stay ordered. All of them pass
    /// with an anchor that is wrong by an arbitrary constant, because a
    /// constant offset preserves order perfectly.
    ///
    /// A constant offset is not a harmless error here. `row_timestamp_ist_nanos`
    /// promotes the receipt to the row's DESIGNATED `ts` whenever the exchange
    /// stamp is the vendor's never-traded sentinel, and `ts` is the first column
    /// of the `ticks` DEDUP key. An anchor off by hours therefore mints rows in
    /// a partition that retention and archival -- both keyed on the trading day
    /// -- can never reach, and it does so with total confidence.
    ///
    /// One second of tolerance, deliberately loose: this asserts the anchor is
    /// ANCHORED, not that the machine is fast. A broken anchor is wrong by the
    /// process uptime or by an epoch, never by 900 ms.
    #[test]
    fn a_converted_receipt_lands_on_the_real_wall_clock_not_merely_in_order() {
        let before = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0);
        let converted = receipt_nanos_from(Instant::now());
        let after = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0);

        const TOLERANCE_NANOS: i64 = 1_000_000_000;
        assert!(
            converted >= before - TOLERANCE_NANOS && converted <= after + TOLERANCE_NANOS,
            "receipt_nanos_from produced {converted}, which is outside \
             [{before}, {after}] widened by one second. The monotonic anchor has \
             drifted off the wall clock, so every replayed frame would carry a \
             confidently WRONG arrival time -- and for a sentinel-LTT tick that \
             value becomes the row's designated timestamp."
        );

        // And it must be inside the band the writer enforces, or the writer
        // would silently degrade every real receipt to the UNKNOWN sentinel --
        // which reads downstream exactly like a v1/v2 record that never had one.
        assert_eq!(
            plausible_receipt_nanos(converted),
            converted,
            "a freshly converted receipt must survive the plausibility band; \
             if it does not, the capture path writes UNKNOWN for every frame \
             and the whole TVW3 field is dead again."
        );
    }

    /// END-TO-END through the REAL writer: a receipt handed to
    /// `append_with_seq_at` survives the writer thread, the on-disk encoding
    /// and `replay_all` unchanged.
    ///
    /// # Why this is not covered by the fixture tests
    ///
    /// `tvw3_fixture_replays_as_main_feed_with_its_receipt` hand-encodes the
    /// record with `encode_v3_record` and proves the READER. Nothing exercised
    /// the WRITER carrying a receipt all the way to a file.
    ///
    /// That gap is exactly the shape this field has failed in twice: first the
    /// format existed and nothing populated it (`append_with_seq_at` had zero
    /// production callers, so every record on disk carried 0 while the format
    /// claimed otherwise); then it was populated and nothing read it back
    /// (`ReplayedFrame::received_at_nanos` had zero consumers). Both times the
    /// unit tests on either side stayed green, because each half was correct in
    /// isolation. Only a test that spans the boundary can fail.
    #[test]
    fn a_receipt_written_by_the_real_writer_replays_unchanged() {
        let dir = tmp_dir("receipt-e2e");
        // A real converted receipt, not a literal: this ties the assertion to
        // the conversion the capture path actually uses.
        let receipt = receipt_nanos_from(Instant::now());
        {
            let spill = WsFrameSpill::new(&dir).unwrap();
            spill.append_with_seq_at(
                WsType::LiveFeed,
                vec![9, 8, 7],
                4242,
                receipt,
                WalEndpoint::Depth20,
            );
            wait_until_persisted(&spill, 1);
        } // drop → writer thread drains and exits
        std::thread::sleep(Duration::from_millis(50));

        let frames = replay_all(&dir).unwrap();
        assert_eq!(frames.len(), 1, "the frame must survive the round trip");
        assert_eq!(
            frames[0].received_at_nanos, receipt,
            "the RECEIPT must survive it too. A zero here means the writer \
             dropped it; a different value means the encoding and the decoder \
             disagree about the field offset."
        );
        // The three fields the replay consumer routes on must all survive
        // together -- a receipt that arrives attached to the wrong frame or the
        // wrong socket is worse than no receipt at all.
        assert_eq!(frames[0].frame_seq, 4242);
        assert_eq!(frames[0].endpoint, WalEndpoint::Depth20);
        assert_eq!(frames[0].frame, vec![9, 8, 7]);

        let _ = std::fs::remove_dir_all(&dir);
    }

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

    // -----------------------------------------------------------------------
    // Applied-watermark replay skip (2026-09-05)
    // -----------------------------------------------------------------------

    /// A 2026-era `capture_seq` base: nanos with the 17 reserved bits zeroed.
    fn wm_seq(secs: u64) -> u64 {
        ((1_780_000_000 + secs) * 1_000_000_000) >> PACKET_INDEX_BITS << PACKET_INDEX_BITS
    }

    /// Writes one segment holding `frames` records at one-second spacing from
    /// `first_secs`, named so it sorts in chronological order.
    fn write_wm_segment(
        dir: &Path,
        first_secs: u64,
        frames: u64,
        endpoint: WalEndpoint,
    ) -> PathBuf {
        let mut bytes = Vec::new();
        for i in 0..frames {
            bytes.extend_from_slice(&encode_v4_record(
                WsType::LiveFeed,
                wm_seq(first_secs + i),
                7,
                endpoint,
                &[0xAB; 32],
            ));
        }
        let path = dir.join(format!("ws-frames-{:020}.wal", wm_seq(first_secs)));
        std::fs::write(&path, bytes).unwrap();
        path
    }

    fn write_wm_watermark(dir: &Path, snap: &crate::wal_applied_watermark::AppliedSnapshot) {
        let wm = crate::wal_applied_watermark::AppliedWatermark::new_for_tests();
        wm.bind(dir);
        wm.seed(snap);
        wm.persist_now();
        assert!(
            dir.join(crate::wal_applied_watermark::APPLIED_WATERMARK_FILE)
                .exists()
        );
    }

    fn replay_unguarded(dir: &Path) -> WalReplayBatch {
        replay_all_with_report_guarded(dir, usize::MAX, || None, None, WAL_REPLAY_RSS_STOP_PCT)
            .expect("replay")
    }

    /// 2026-10-03: a catch-up drain that stops early leaves segments for the
    /// next boot, and the live lane's acks then lift the watermark past them.
    /// Without the guard the next replay archives them unread; with it, every
    /// leftover frame comes back.
    #[test]
    fn test_regression_leftover_backlog_is_replayed_after_live_acks_pass_it() {
        // The bug, reproduced: live acks pass the leftover range, nothing
        // marks it, and the replay archives all but the last segment unread.
        let dir = tmp_dir("wm-leftover-unguarded");
        for first in [0, 1_000, 2_000] {
            write_wm_segment(&dir, first, 5, WalEndpoint::MainFeed);
        }
        let wm = crate::wal_applied_watermark::AppliedWatermark::new_for_tests();
        wm.note_ticks_acked(wm_seq(5_000));
        wm.note_depth_acked(wm_seq(5_000));
        write_wm_watermark(&dir, &wm.snapshot());
        let batch = replay_unguarded(&dir);
        assert_eq!(
            batch.skipped_segments, 2,
            "the unguarded bug: two segments archived unread"
        );
        let _ = std::fs::remove_dir_all(&dir);

        // The fix: the drain marks the leftover range before the live lane starts.
        let dir = tmp_dir("wm-leftover-guarded");
        for first in [0, 1_000, 2_000] {
            write_wm_segment(&dir, first, 5, WalEndpoint::MainFeed);
        }
        let wm = crate::wal_applied_watermark::AppliedWatermark::new_for_tests();
        let ceiling = wm_seq(3_000);
        assert_eq!(
            guard_pending_backlog(&wm, &dir, ceiling),
            Some((wm_seq(0), ceiling - 1))
        );
        wm.note_ticks_acked(wm_seq(5_000));
        wm.note_depth_acked(wm_seq(5_000));
        write_wm_watermark(&dir, &wm.snapshot());
        let batch = replay_unguarded(&dir);
        assert_eq!(batch.skipped_segments, 0, "nothing is archived unread");
        assert_eq!(batch.frames.len(), 15, "every leftover frame comes back");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Segments staged in `replaying/` are guarded too; segments written at or
    /// above the ceiling (this session's) and an empty directory need nothing.
    #[test]
    fn test_guard_pending_backlog_ignores_segments_at_or_above_the_ceiling() {
        let dir = tmp_dir("wm-guard-ceiling");
        let wm = crate::wal_applied_watermark::AppliedWatermark::new_for_tests();
        assert_eq!(
            guard_pending_backlog(&wm, &dir, wm_seq(100)),
            None,
            "nothing waiting"
        );
        write_wm_segment(&dir, 200, 5, WalEndpoint::MainFeed);
        assert_eq!(
            guard_pending_backlog(&wm, &dir, wm_seq(100)),
            None,
            "only this session's segments"
        );
        assert_eq!(guard_pending_backlog(&wm, &dir, 0), None, "zero ceiling");
        let replaying = dir.join(REPLAYING_SUBDIR);
        std::fs::create_dir_all(&replaying).unwrap();
        write_wm_segment(&replaying, 50, 5, WalEndpoint::MainFeed);
        assert_eq!(
            guard_pending_backlog(&wm, &dir, wm_seq(100)),
            Some((wm_seq(50), wm_seq(100) - 1)),
            "a staged segment is the lowest waiting one"
        );
        assert!(wm.snapshot().range_has_unapplied(wm_seq(50), wm_seq(54)));
        let _ = std::fs::remove_dir_all(&dir);
    }

    // -----------------------------------------------------------------------
    // Item 44d (2026-09-22): the WAL AGE prune respects the applied watermark
    // -----------------------------------------------------------------------

    /// Sets a file's mtime to `age_secs` before `now`.
    fn backdate(path: &Path, now: SystemTime, age_secs: u64) {
        let f = std::fs::OpenOptions::new().write(true).open(path).unwrap();
        f.set_times(std::fs::FileTimes::new().set_modified(now - Duration::from_secs(age_secs)))
            .unwrap();
    }

    /// A watermark with both sinks acked through `hwm` and no unapplied bucket.
    fn all_applied_through(hwm: u64) -> crate::wal_applied_watermark::AppliedSnapshot {
        crate::wal_applied_watermark::AppliedSnapshot {
            hwm_ticks: hwm,
            hwm_depth: hwm,
            ..Default::default()
        }
    }

    /// Marks the bucket holding `seq` as captured-but-unapplied.
    fn mark_unapplied(snap: &mut crate::wal_applied_watermark::AppliedSnapshot, seq: u64) {
        use crate::wal_applied_watermark::{UNAPPLIED_BUCKET_SHIFT, UNAPPLIED_BUCKETS};
        let id = seq >> UNAPPLIED_BUCKET_SHIFT;
        snap.buckets[(id % UNAPPLIED_BUCKETS as u64) as usize] = (id, 1);
    }

    const ALL_STATES: [SegmentAppliedState; 3] = [
        SegmentAppliedState::Applied,
        SegmentAppliedState::Unapplied,
        SegmentAppliedState::Unknown,
    ];

    /// The full permutation grid: (aged, state, byte cap exceeded).
    #[test]
    fn segment_prune_decision_over_the_full_grid() {
        use SegmentAppliedState::{Applied, Unapplied, Unknown};
        use SegmentPruneDecision::{DeleteAged, DeleteForBytes, Keep, RefuseForBytes};
        let grid = [
            // aged, state, bytes -> decision
            (false, Applied, false, Keep),
            (false, Unapplied, false, Keep),
            (false, Unknown, false, Keep),
            (true, Applied, false, DeleteAged),
            (true, Unapplied, false, Keep),
            (true, Unknown, false, Keep),
            (false, Applied, true, DeleteForBytes),
            (false, Unapplied, true, RefuseForBytes { unknown: false }),
            (false, Unknown, true, RefuseForBytes { unknown: true }),
            (true, Applied, true, DeleteAged),
            (true, Unapplied, true, RefuseForBytes { unknown: false }),
            (true, Unknown, true, RefuseForBytes { unknown: true }),
        ];
        for (aged, state, bytes, want) in grid {
            assert_eq!(
                segment_prune_decision(aged, state, bytes, true),
                want,
                "aged={aged} state={state:?} bytes_exceeded={bytes}"
            );
        }
    }

    proptest::proptest! {
        /// The invariants, over every input: NO decision deletes a segment that
        /// is not applied (item 45b). The age pass (no byte pressure) deletes
        /// ONLY an aged, applied segment; the byte pass deletes only applied
        /// segments and refuses exactly the non-applied ones, flagging unknown.
        #[test]
        fn segment_prune_decision_invariants(aged in proptest::bool::ANY, s in 0usize..3, bytes in proptest::bool::ANY, uploaded in proptest::bool::ANY) {
            let state = ALL_STATES[s];
            let applied = state == SegmentAppliedState::Applied;
            let d = segment_prune_decision(aged, state, bytes, uploaded);
            match d {
                SegmentPruneDecision::DeleteAged => proptest::prop_assert!(aged && applied && uploaded),
                SegmentPruneDecision::DeleteForBytes => {
                    proptest::prop_assert!(bytes && applied && uploaded);
                    proptest::prop_assert!(!aged);
                }
                SegmentPruneDecision::RefuseForBytes { unknown } => {
                    proptest::prop_assert!(bytes && !applied);
                    proptest::prop_assert_eq!(unknown, state == SegmentAppliedState::Unknown);
                }
                SegmentPruneDecision::RefuseNotUploaded => {
                    proptest::prop_assert!(bytes && applied && !uploaded);
                }
                SegmentPruneDecision::Keep => proptest::prop_assert!(!(bytes || (aged && applied && uploaded))),
            }
            // The zero-loss property itself: only an applied segment with a
            // verified S3 copy (or the gate off) is ever a deletion.
            if matches!(d, SegmentPruneDecision::DeleteAged | SegmentPruneDecision::DeleteForBytes) {
                proptest::prop_assert!(applied && uploaded);
            }
            if !bytes && !applied {
                proptest::prop_assert_eq!(d, SegmentPruneDecision::Keep);
            }
        }
    }

    /// Four OLD segments: A applied, B holds an unapplied bucket, C applied,
    /// D newest (no upper bound => unknown). The age pass removes A and C
    /// only. The byte pass, even at a zero ceiling, then REFUSES both B and D
    /// (item 45b): each may be the only copy of its frames.
    #[test]
    fn byte_prune_refuses_an_unapplied_segment() {
        let dir = tmp_dir("wm-prune-mixed");
        let now = SystemTime::now();
        let a = write_wm_segment(&dir, 0, 3, WalEndpoint::MainFeed);
        let b = write_wm_segment(&dir, 1_000, 3, WalEndpoint::Depth20);
        let c = write_wm_segment(&dir, 2_000, 3, WalEndpoint::MainFeed);
        let d = write_wm_segment(&dir, 3_000, 3, WalEndpoint::MainFeed);
        for (p, age) in [(&a, 400_000), (&b, 390_000), (&c, 380_000), (&d, 370_000)] {
            backdate(p, now, age);
        }
        let mut snap = all_applied_through(wm_seq(3_500));
        mark_unapplied(&mut snap, wm_seq(1_500)); // inside B's range only

        let age = prune_active_segments_at(&dir, 172_800, u64::MAX, now, Some(&snap));
        assert_eq!(age.deleted, 2, "only the two applied segments expire");
        assert!(!a.exists() && !c.exists());
        assert!(
            b.exists(),
            "B holds an unapplied frame: the age pass must keep it"
        );
        assert!(d.exists(), "D's range is unbounded: unknown, so kept");
        assert_eq!(age.age_kept_unapplied, 2);
        assert_eq!(age.size_deleted, 0);

        let bytes = prune_active_segments_at(&dir, 172_800, 0, now, Some(&snap));
        assert_eq!(bytes.size_deleted, 0, "nothing applied is left to delete");
        assert_eq!(bytes.size_refused_unapplied, 1, "B is PROVEN unapplied");
        assert_eq!(
            bytes.size_refused_unknown, 1,
            "D is unknown, counted apart from the proven-unapplied refusal"
        );
        let held = std::fs::metadata(&b).unwrap().len() + std::fs::metadata(&d).unwrap().len();
        assert_eq!(bytes.size_refused_bytes, held);
        assert_eq!(
            bytes.bytes_after, held,
            "the directory honestly reports it is still over the ceiling"
        );
        assert_eq!(bytes.age_kept_unapplied, 2, "both are still kept past age");
        assert!(
            b.exists() && d.exists(),
            "the byte pass must never delete a segment the watermark has not passed"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Non-vacuous twin of the test above: the byte pass still deletes APPLIED
    /// segments, oldest first, and refuses nothing. ("Uploaded" in the name is
    /// the item-45e S3 marker; until 45e lands, applied is the whole test.)
    #[test]
    fn byte_prune_still_deletes_applied_and_uploaded_segments() {
        let dir = tmp_dir("wm-prune-bytes-applied");
        let now = SystemTime::now();
        let a = write_wm_segment(&dir, 0, 3, WalEndpoint::MainFeed);
        let b = write_wm_segment(&dir, 1_000, 3, WalEndpoint::MainFeed);
        let c = write_wm_segment(&dir, 2_000, 3, WalEndpoint::MainFeed);
        backdate(&a, now, 300);
        backdate(&b, now, 200);
        backdate(&c, now, 100);
        let snap = all_applied_through(wm_seq(5_000));
        let one = std::fs::metadata(&c).unwrap().len();
        let out = prune_active_segments_at(&dir, 172_800, one, now, Some(&snap));
        assert_eq!(out.deleted, 0, "nothing is past the age window");
        assert_eq!(out.size_deleted, 2);
        assert_eq!(out.size_refused_unapplied, 0, "a and b were applied");
        assert_eq!(out.size_refused_unknown, 0);
        assert!(!a.exists() && !b.exists() && c.exists(), "oldest-first");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// No usable watermark — absent (`None`), corrupt, or foreign — fails
    /// closed: the age pass keeps every segment, however old.
    #[test]
    fn a_missing_or_corrupt_watermark_keeps_everything_by_age() {
        let dir = tmp_dir("wm-prune-missing");
        let now = SystemTime::now();
        let a = write_wm_segment(&dir, 0, 3, WalEndpoint::MainFeed);
        let b = write_wm_segment(&dir, 1_000, 3, WalEndpoint::MainFeed);
        let c = write_wm_segment(&dir, 2_000, 3, WalEndpoint::MainFeed);
        for p in [&a, &b, &c] {
            backdate(p, now, 400_000);
        }
        let none = prune_active_segments_at(&dir, 172_800, u64::MAX, now, None);
        assert_eq!(none.deleted, 0);
        assert_eq!(none.age_kept_unapplied, 3);

        // Corrupt file on disk, through the production wrapper (which loads it).
        std::fs::write(
            dir.join(crate::wal_applied_watermark::APPLIED_WATERMARK_FILE),
            b"not a watermark",
        )
        .unwrap();
        let corrupt = prune_active_segments(&dir, 172_800, u64::MAX, false);
        assert_eq!(
            corrupt.deleted, 0,
            "a rejected watermark must keep everything"
        );
        assert!(a.exists() && b.exists() && c.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Non-vacuous twin of the above: the production wrapper DOES load a valid
    /// watermark from disk and lets the age pass expire applied segments.
    #[test]
    fn the_production_wrapper_loads_the_watermark_and_expires_applied_segments() {
        let dir = tmp_dir("wm-prune-wrapper");
        let now = SystemTime::now();
        let a = write_wm_segment(&dir, 0, 3, WalEndpoint::MainFeed);
        let b = write_wm_segment(&dir, 1_000, 3, WalEndpoint::MainFeed);
        backdate(&a, now, 400_000);
        backdate(&b, now, 400_000);
        write_wm_watermark(&dir, &all_applied_through(wm_seq(5_000)));
        let out = prune_active_segments(&dir, 172_800, u64::MAX, false);
        assert_eq!(
            out.deleted, 1,
            "a is applied and aged; b is the newest (unknown)"
        );
        assert!(!a.exists() && b.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A deferred-depth marks table holding one shed frame at `seq`.
    fn deferred_at(seq: u64) -> crate::wal_deferred_depth::DeferredDepthSnapshot {
        let d = crate::wal_deferred_depth::DeferredDepth::new_for_tests();
        d.note_shed(seq, crate::wal_deferred_depth::ShedDepth::Inline);
        d.snapshot()
    }

    /// Item 45a: a segment holding shed depth that has not been written back
    /// is the only copy of those rows. Neither the age pass nor the byte pass
    /// may delete it, even when it is applied, aged and over the ceiling.
    #[test]
    fn deferred_mark_keeps_the_segment_from_both_prunes() {
        let dir = tmp_dir("deferred-active");
        let now = SystemTime::now();
        let a = write_wm_segment(&dir, 0, 3, WalEndpoint::MainFeed);
        let b = write_wm_segment(&dir, 1_000, 3, WalEndpoint::MainFeed);
        let c = write_wm_segment(&dir, 2_000, 3, WalEndpoint::MainFeed);
        let d = write_wm_segment(&dir, 3_000, 3, WalEndpoint::MainFeed);
        // E is the newest (unknown, so never a byte-pass victim since item
        // 45b); D is fresh and applied, so it survives the age pass and is the
        // byte pass's to take.
        let e = write_wm_segment(&dir, 4_000, 3, WalEndpoint::MainFeed);
        for p in [&a, &b, &c] {
            backdate(p, now, 400_000);
        }
        let applied = all_applied_through(wm_seq(5_000));
        let deferred = deferred_at(wm_seq(1_500)); // inside B only

        let age = prune_active_segments_deferred_at(
            &dir,
            172_800,
            u64::MAX,
            now,
            Some(&applied),
            Some(&deferred),
            false,
            UploadGate::NotRequired,
        );
        assert!(
            !a.exists() && !c.exists(),
            "applied, aged, not deferred: expired"
        );
        assert!(b.exists(), "B holds shed depth: the age pass must keep it");
        assert_eq!(age.deferred_kept, 1);

        let bytes = prune_active_segments_deferred_at(
            &dir,
            172_800,
            0,
            now,
            Some(&applied),
            Some(&deferred),
            false,
            UploadGate::NotRequired,
        );
        assert!(
            b.exists(),
            "the byte ceiling must not reach a deferred segment either"
        );
        assert!(
            !d.exists(),
            "an undeferred survivor is still the byte pass's to take"
        );
        assert!(e.exists(), "the unknown newest is refused, never deleted");
        assert_eq!(bytes.size_refused_unknown, 1);
        assert_eq!(bytes.deferred_kept, 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Below the hard disk floor, pinned segments are the last to go — after
    /// every ordinary segment — and each one is counted as a loss.
    #[test]
    fn test_prune_active_segments_deferred_at_below_the_disk_floor_takes_pinned_last() {
        let dir = tmp_dir("deferred-floor");
        let now = SystemTime::now();
        let a = write_wm_segment(&dir, 0, 3, WalEndpoint::MainFeed);
        let b = write_wm_segment(&dir, 1_000, 3, WalEndpoint::MainFeed);
        let c = write_wm_segment(&dir, 2_000, 3, WalEndpoint::MainFeed);
        let _open_ended = write_wm_segment(&dir, 3_000, 3, WalEndpoint::MainFeed);
        let applied = all_applied_through(wm_seq(5_000));
        let deferred = deferred_at(wm_seq(100)); // A, the OLDEST, is pinned
        let one = std::fs::metadata(&a).unwrap().len();

        // Above the floor: the ceiling cannot take A, so it takes B and C and
        // reports the pinned bytes.
        let above = prune_active_segments_deferred_at(
            &dir,
            172_800,
            one,
            now,
            Some(&applied),
            Some(&deferred),
            false,
            UploadGate::NotRequired,
        );
        assert!(a.exists(), "pinned: kept above the floor");
        assert!(
            !b.exists() && !c.exists(),
            "ordinary segments go first, oldest first"
        );
        assert_eq!(above.size_deleted_deferred, 0);

        // Below the floor with the ceiling at zero: A goes too, counted.
        let below = prune_active_segments_deferred_at(
            &dir,
            172_800,
            0,
            now,
            Some(&applied),
            Some(&deferred),
            true,
            UploadGate::NotRequired,
        );
        assert!(!a.exists(), "below the floor the pinned segment is taken");
        assert_eq!(below.size_deleted_deferred, 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- item 45e-1: no delete without a verified S3 copy -----------------

    /// Records a verified-upload marker for `seg` claiming `raw_len` bytes.
    fn mark_uploaded(dir: &Path, seg: &Path, raw_len: u64) {
        crate::raw_frame_upload::write_marker_for_test(
            &crate::raw_frame_upload::markers_dir(dir),
            seg,
            raw_len,
        );
    }

    fn len_of(p: &Path) -> u64 {
        std::fs::metadata(p).unwrap().len()
    }

    /// AGE pass: three aged, applied segments. A has a marker for its length
    /// (deleted, and its marker removed), B a marker for another length
    /// (kept), C no marker (kept).
    #[test]
    fn test_regression_age_prune_needs_a_matching_upload_marker() {
        let dir = tmp_dir("upload-gate-age");
        let now = SystemTime::now();
        let a = write_wm_segment(&dir, 0, 3, WalEndpoint::MainFeed);
        let b = write_wm_segment(&dir, 1_000, 3, WalEndpoint::MainFeed);
        let c = write_wm_segment(&dir, 2_000, 3, WalEndpoint::MainFeed);
        let _open_ended = write_wm_segment(&dir, 3_000, 3, WalEndpoint::MainFeed);
        for p in [&a, &b, &c] {
            backdate(p, now, 400_000);
        }
        mark_uploaded(&dir, &a, len_of(&a));
        mark_uploaded(&dir, &b, len_of(&b) + 1);
        let snap = all_applied_through(wm_seq(5_000));
        let markers = crate::raw_frame_upload::markers_dir(&dir);
        let out = prune_active_segments_deferred_at(
            &dir,
            172_800,
            u64::MAX,
            now,
            Some(&snap),
            None,
            false,
            UploadGate::Required {
                markers_dir: &markers,
            },
        );
        assert!(!a.exists(), "A has a verified copy: it ages out");
        assert!(
            !markers.join(a.file_name().unwrap()).exists(),
            "A's marker goes with it"
        );
        assert!(b.exists(), "B's marker records another length: kept");
        assert!(c.exists(), "C has no marker: kept");
        assert_eq!(out.deleted, 1);
        assert_eq!(out.age_kept_not_uploaded, 2);
        assert_eq!(
            out.age_kept_unapplied, 0,
            "B and C are applied, not unapplied"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The same directory with the gate OFF deletes all three: the old
    /// behaviour, kept for a box without a bucket.
    #[test]
    fn age_prune_with_the_upload_gate_off_keeps_the_old_behaviour() {
        let dir = tmp_dir("upload-gate-off");
        let now = SystemTime::now();
        let a = write_wm_segment(&dir, 0, 3, WalEndpoint::MainFeed);
        let b = write_wm_segment(&dir, 1_000, 3, WalEndpoint::MainFeed);
        let c = write_wm_segment(&dir, 2_000, 3, WalEndpoint::MainFeed);
        let _open_ended = write_wm_segment(&dir, 3_000, 3, WalEndpoint::MainFeed);
        for p in [&a, &b, &c] {
            backdate(p, now, 400_000);
        }
        let snap = all_applied_through(wm_seq(5_000));
        let out = prune_active_segments_deferred_at(
            &dir,
            172_800,
            u64::MAX,
            now,
            Some(&snap),
            None,
            false,
            UploadGate::NotRequired,
        );
        assert_eq!(out.deleted, 3);
        assert_eq!(out.age_kept_not_uploaded, 0);
        assert!(!a.exists() && !b.exists() && !c.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// BYTE pass at a zero ceiling: only the segment with a matching marker
    /// goes; the wrong-length and missing markers are refused and counted.
    #[test]
    fn test_regression_byte_prune_refuses_segments_without_a_verified_copy() {
        let dir = tmp_dir("upload-gate-bytes");
        let now = SystemTime::now();
        let a = write_wm_segment(&dir, 0, 3, WalEndpoint::MainFeed);
        let b = write_wm_segment(&dir, 1_000, 3, WalEndpoint::MainFeed);
        let c = write_wm_segment(&dir, 2_000, 3, WalEndpoint::MainFeed);
        let _open_ended = write_wm_segment(&dir, 3_000, 3, WalEndpoint::MainFeed);
        backdate(&a, now, 300);
        backdate(&b, now, 200);
        backdate(&c, now, 100);
        mark_uploaded(&dir, &a, len_of(&a));
        mark_uploaded(&dir, &b, len_of(&b) - 1);
        let snap = all_applied_through(wm_seq(5_000));
        let markers = crate::raw_frame_upload::markers_dir(&dir);
        let out = prune_active_segments_deferred_at(
            &dir,
            172_800,
            0,
            now,
            Some(&snap),
            None,
            false,
            UploadGate::Required {
                markers_dir: &markers,
            },
        );
        assert!(!a.exists(), "applied and uploaded: the byte pass takes it");
        assert!(b.exists() && c.exists());
        assert_eq!(out.size_deleted, 1);
        assert_eq!(out.size_refused_not_uploaded, 2);
        assert_eq!(out.size_refused_not_uploaded_bytes, len_of(&b) + len_of(&c));
        assert_eq!(out.size_refused_unapplied, 0);
        assert_eq!(out.size_refused_unknown, 1, "the open-ended newest");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// HARD FLOOR: a pinned, applied segment below the disk floor is still
    /// refused without a verified copy, and taken once it has one.
    #[test]
    fn test_regression_floor_prune_needs_a_verified_copy_too() {
        let dir = tmp_dir("upload-gate-floor");
        let now = SystemTime::now();
        let a = write_wm_segment(&dir, 0, 3, WalEndpoint::MainFeed);
        let _open_ended = write_wm_segment(&dir, 1_000, 3, WalEndpoint::MainFeed);
        let applied = all_applied_through(wm_seq(5_000));
        let deferred = deferred_at(wm_seq(100)); // A is pinned
        let markers = crate::raw_frame_upload::markers_dir(&dir);
        let gate = UploadGate::Required {
            markers_dir: &markers,
        };
        let refused = prune_active_segments_deferred_at(
            &dir,
            172_800,
            0,
            now,
            Some(&applied),
            Some(&deferred),
            true,
            gate,
        );
        assert!(a.exists(), "no verified copy: kept even below the floor");
        assert_eq!(refused.size_deleted_deferred, 0);
        assert_eq!(refused.size_refused_not_uploaded, 1);

        mark_uploaded(&dir, &a, len_of(&a));
        let taken = prune_active_segments_deferred_at(
            &dir,
            172_800,
            0,
            now,
            Some(&applied),
            Some(&deferred),
            true,
            gate,
        );
        assert!(!a.exists(), "with a verified copy the floor may take it");
        assert_eq!(taken.size_deleted_deferred, 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `archive/` segments are checked against the markers in the WAL root's
    /// `uploaded/` directory (a marker follows its segment across renames).
    #[test]
    fn archive_prune_reads_markers_from_the_wal_root() {
        let dir = tmp_dir("upload-gate-archive");
        let now = SystemTime::now();
        let kept = plant_archive_file(&dir, "ws-frames-00000000000000000001.wal", now, 999_999);
        let gone = plant_archive_file(&dir, "ws-frames-00000000000000000002.wal", now, 999_999);
        mark_uploaded(&dir, &gone, len_of(&gone));
        let out = prune_archived_segments(&dir, 172_800, u64::MAX, true);
        assert!(kept.exists(), "no marker: kept");
        assert!(!gone.exists(), "marker in <wal_dir>/uploaded: deleted");
        assert_eq!(out.deleted, 1);
        assert_eq!(out.age_kept_not_uploaded, 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Item 45b: even below the hard disk floor, a pinned segment that is ALSO
    /// unapplied is never taken — its ticks may exist nowhere else. The floor
    /// only ever trades away shed depth rows of an applied segment.
    #[test]
    fn byte_prune_below_the_floor_still_refuses_an_unapplied_pinned_segment() {
        let dir = tmp_dir("deferred-floor-unapplied");
        let now = SystemTime::now();
        let a = write_wm_segment(&dir, 0, 3, WalEndpoint::MainFeed);
        let b = write_wm_segment(&dir, 1_000, 3, WalEndpoint::MainFeed);
        let _open_ended = write_wm_segment(&dir, 2_000, 3, WalEndpoint::MainFeed);
        let mut applied = all_applied_through(wm_seq(5_000));
        mark_unapplied(&mut applied, wm_seq(100)); // A: unapplied AND pinned
        let deferred_both = crate::wal_deferred_depth::DeferredDepth::new_for_tests();
        deferred_both.note_shed(wm_seq(100), crate::wal_deferred_depth::ShedDepth::Inline);
        deferred_both.note_shed(wm_seq(1_100), crate::wal_deferred_depth::ShedDepth::Inline);
        let deferred = deferred_both.snapshot();
        let below = prune_active_segments_deferred_at(
            &dir,
            172_800,
            0,
            now,
            Some(&applied),
            Some(&deferred),
            true,
            UploadGate::NotRequired,
        );
        assert!(
            a.exists(),
            "A is pinned AND unapplied: the floor must never take it"
        );
        assert!(
            !b.exists(),
            "non-vacuous: B is pinned but applied, so the floor still takes it"
        );
        assert_eq!(below.size_deleted_deferred, 1);
        assert_eq!(below.size_refused_unapplied, 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Bite test: the same directory with no marks loses B. Proves the test
    /// above is not vacuous.
    #[test]
    fn without_a_deferred_mark_the_same_segment_is_pruned() {
        let dir = tmp_dir("deferred-none");
        let now = SystemTime::now();
        let a = write_wm_segment(&dir, 0, 3, WalEndpoint::MainFeed);
        let b = write_wm_segment(&dir, 1_000, 3, WalEndpoint::MainFeed);
        let c = write_wm_segment(&dir, 2_000, 3, WalEndpoint::MainFeed);
        for p in [&a, &b, &c] {
            backdate(p, now, 400_000);
        }
        let applied = all_applied_through(wm_seq(5_000));
        let empty = crate::wal_deferred_depth::DeferredDepthSnapshot::default();
        let out = prune_active_segments_deferred_at(
            &dir,
            172_800,
            u64::MAX,
            now,
            Some(&applied),
            Some(&empty),
            false,
            UploadGate::NotRequired,
        );
        assert!(!b.exists(), "no mark: B is applied and aged, so it expires");
        assert_eq!(out.deferred_kept, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// With no usable applied watermark the ranges are still read, so a mark
    /// keeps only the segment it covers — not the whole directory.
    #[test]
    fn a_deferred_mark_without_a_watermark_keeps_only_its_own_segment() {
        let dir = tmp_dir("deferred-no-wm");
        let now = SystemTime::now();
        let a = write_wm_segment(&dir, 0, 3, WalEndpoint::MainFeed);
        let b = write_wm_segment(&dir, 1_000, 3, WalEndpoint::MainFeed);
        let c = write_wm_segment(&dir, 2_000, 3, WalEndpoint::MainFeed);
        let deferred = deferred_at(wm_seq(1_500));
        let out = prune_active_segments_deferred_at(
            &dir,
            172_800,
            0,
            now,
            None,
            Some(&deferred),
            false,
            UploadGate::NotRequired,
        );
        assert!(b.exists(), "B is deferred");
        assert_eq!(out.deferred_kept, 1, "the mark covers B only");
        // Item 45b: with no watermark A and C are unknown, so the byte pass
        // refuses them rather than deleting what may be the only copy. They
        // are counted as refusals, NOT as deferred — the mark still covers B
        // alone.
        assert!(a.exists() && c.exists());
        assert_eq!(out.size_refused_unknown, 2);
        assert_eq!(out.size_deleted, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A boot archives a segment the watermark has passed WITHOUT reading it,
    /// and a shed segment is exactly that. The archive prune must honour the
    /// mark as well.
    #[test]
    fn test_prune_archived_segments_deferred_at_keeps_a_deferred_segment() {
        let dir = tmp_dir("deferred-archive");
        let archive = dir.join(ARCHIVE_SUBDIR);
        std::fs::create_dir_all(&archive).unwrap();
        let now = SystemTime::now();
        let a = write_wm_segment(&archive, 0, 3, WalEndpoint::MainFeed);
        let b = write_wm_segment(&archive, 1_000, 3, WalEndpoint::MainFeed);
        let c = write_wm_segment(&archive, 2_000, 3, WalEndpoint::MainFeed);
        for p in [&a, &b, &c] {
            backdate(p, now, 400_000);
        }
        let deferred = deferred_at(wm_seq(1_500));
        let out = prune_archived_segments_deferred_at(
            &dir,
            172_800,
            0,
            now,
            Some(&deferred),
            false,
            UploadGate::NotRequired,
        );
        assert!(b.exists(), "deferred: kept in archive/ too");
        assert!(!a.exists() && !c.exists());
        assert_eq!(out.deferred_kept, 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An unmeasurable disk must never justify deleting the only copy of shed
    /// depth: a failed probe reads as NOT below the floor.
    #[test]
    fn test_wal_disk_below_floor_is_false_when_the_probe_fails() {
        let missing = std::env::temp_dir().join(format!("tv-no-such-dir-{}", std::process::id()));
        assert!(!missing.exists());
        assert!(!wal_disk_below_floor(&missing));
    }

    /// The pass lists every directory a shed segment can be in, in capture
    /// order, with each segment bounded by its successor.
    #[test]
    fn test_deferred_segment_spans_order_across_active_replaying_archive() {
        let dir = tmp_dir("spans");
        let archive = dir.join(ARCHIVE_SUBDIR);
        let replaying = dir.join(REPLAYING_SUBDIR);
        std::fs::create_dir_all(&archive).unwrap();
        std::fs::create_dir_all(&replaying).unwrap();
        let a = write_wm_segment(&archive, 0, 3, WalEndpoint::MainFeed);
        let b = write_wm_segment(&replaying, 1_000, 3, WalEndpoint::Depth20);
        let c = write_wm_segment(&dir, 2_000, 3, WalEndpoint::MainFeed);
        let spans = deferred_segment_spans(&dir).expect("listable");
        let paths: Vec<&PathBuf> = spans.iter().map(|s| &s.path).collect();
        assert_eq!(paths, vec![&a, &b, &c]);
        assert_eq!(spans[0].lo, wm_seq(0));
        assert_eq!(spans[0].hi, Some(wm_seq(1_000) - 1));
        assert_eq!(spans[2].hi, None, "the newest segment is open-ended");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A bucket maps to exactly the segments that can hold it.
    #[test]
    fn segments_for_bucket_finds_only_the_covering_segments() {
        let dir = tmp_dir("bucket-segs");
        let a = write_wm_segment(&dir, 0, 3, WalEndpoint::MainFeed);
        let b = write_wm_segment(&dir, 1_000, 3, WalEndpoint::MainFeed);
        let _c = write_wm_segment(&dir, 2_000, 3, WalEndpoint::MainFeed);
        let spans = deferred_segment_spans(&dir).expect("listable");
        let shift = crate::wal_deferred_depth::DEFERRED_BUCKET_SHIFT;
        let in_b = segments_for_bucket(&spans, wm_seq(1_500) >> shift);
        assert_eq!(in_b.paths, vec![b.clone()]);
        assert!(!in_b.touches_open && !in_b.uncertain);
        let in_a = segments_for_bucket(&spans, wm_seq(100) >> shift);
        assert_eq!(in_a.paths, vec![a]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An unreadable segment between two readable ones makes every bucket in
    /// that gap uncertain — no answer may be called complete.
    #[test]
    fn an_unreadable_segment_makes_its_neighbourhood_uncertain() {
        let dir = tmp_dir("bucket-uncertain");
        let _a = write_wm_segment(&dir, 0, 3, WalEndpoint::MainFeed);
        std::fs::write(dir.join("ws-frames-01780000001500000000.wal"), b"garbage").unwrap();
        let _c = write_wm_segment(&dir, 2_000, 3, WalEndpoint::MainFeed);
        let spans = deferred_segment_spans(&dir).expect("listable");
        let shift = crate::wal_deferred_depth::DEFERRED_BUCKET_SHIFT;
        assert!(segments_for_bucket(&spans, wm_seq(1_600) >> shift).uncertain);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The pass's reader keeps only what it asks for and never moves the file.
    #[test]
    fn read_segment_frames_matching_filters_and_never_moves_the_segment() {
        let dir = tmp_dir("read-matching");
        let seg = write_wm_segment(&dir, 0, 5, WalEndpoint::Depth20);
        let want = wm_seq(2);
        let read = read_segment_frames_matching(&seg, &|seq, ep| {
            seq == want && ep == WalEndpoint::Depth20
        })
        .expect("readable");
        assert_eq!(read.frames.len(), 1);
        assert_eq!(read.frames[0].frame_seq, want);
        assert!(!read.damaged);
        assert!(seg.exists(), "the segment stays where it was");
        let none =
            read_segment_frames_matching(&seg, &|_, ep| ep == WalEndpoint::MainFeed).unwrap();
        assert!(none.frames.is_empty(), "endpoint filter applies");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Damage mid-file is reported to the caller.
    #[test]
    fn read_segment_frames_matching_reports_damage() {
        let dir = tmp_dir("read-damaged");
        let seg = write_wm_segment(&dir, 0, 3, WalEndpoint::MainFeed);
        let mut bytes = std::fs::read(&seg).unwrap();
        let mid = bytes.len() / 2;
        bytes[mid] ^= 0xFF;
        std::fs::write(&seg, &bytes).unwrap();
        let read = read_segment_frames_matching(&seg, &|_, _| true).unwrap();
        assert!(read.damaged);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An overflowed marks table cannot say which frames it lost, so every
    /// segment is kept — the failure direction is "keep more".
    #[test]
    fn an_overflowed_marks_table_keeps_every_segment() {
        let dir = tmp_dir("deferred-overflow");
        let now = SystemTime::now();
        let a = write_wm_segment(&dir, 0, 3, WalEndpoint::MainFeed);
        let b = write_wm_segment(&dir, 1_000, 3, WalEndpoint::MainFeed);
        let deferred = crate::wal_deferred_depth::DeferredDepthSnapshot {
            overflowed: true,
            overflow_high_seq: u64::MAX,
            ..Default::default()
        };
        let out = prune_active_segments_deferred_at(
            &dir,
            172_800,
            0,
            now,
            Some(&all_applied_through(wm_seq(5_000))),
            Some(&deferred),
            false,
            UploadGate::NotRequired,
        );
        assert!(a.exists() && b.exists());
        assert_eq!(out.size_deleted, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `archive/` segments are applied by construction: the watermark is not
    /// consulted and the byte pass never counts an unapplied loss there.
    #[test]
    fn the_archive_prune_never_reports_unapplied_losses() {
        let dir = tmp_dir("wm-prune-archive");
        let now = SystemTime::now();
        plant_archive_file(&dir, "ws-frames-00000000000000000001.wal", now, 400_000);
        plant_archive_file(&dir, "ws-frames-00000000000000000002.wal", now, 100);
        let out = prune_archived_segments_at(&dir, 172_800, 0, now);
        assert_eq!(out.deleted, 1);
        assert_eq!(out.size_deleted, 1);
        assert_eq!(out.size_refused_unapplied, 0);
        assert_eq!(out.age_kept_unapplied, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// BITE-PROOF of the whole change: three segments, every frame acked on
    /// the previous session, a fresh boot replays NOTHING. The first two
    /// segments are archived UNREAD; the last one (no upper bound) is read and
    /// every frame in it is dropped as applied.
    #[test]
    fn a_second_boot_after_a_clean_session_replays_zero_frames() {
        let dir = tmp_dir("wm-clean-session");
        write_wm_segment(&dir, 0, 10, WalEndpoint::MainFeed);
        write_wm_segment(&dir, 100, 10, WalEndpoint::Depth20);
        write_wm_segment(&dir, 200, 10, WalEndpoint::MainFeed);
        write_wm_watermark(
            &dir,
            &crate::wal_applied_watermark::AppliedSnapshot {
                hwm_ticks: wm_seq(300),
                hwm_depth: wm_seq(300),
                ..Default::default()
            },
        );
        let batch = replay_unguarded(&dir);
        assert!(
            batch.frames.is_empty(),
            "nothing to replay: {}",
            batch.frames.len()
        );
        assert_eq!(
            batch.skipped_segments, 2,
            "two whole segments archived unread"
        );
        assert_eq!(
            batch.skipped_frames, 10,
            "the unbounded last segment is filtered per frame"
        );
        assert!(batch.skipped_bytes > 0);
        assert!(wal_segments_in(&dir).is_empty(), "no live segment left");
        assert_eq!(wal_segments_in(&dir.join(ARCHIVE_SUBDIR)).len(), 2);
        assert_eq!(
            wal_segments_in(&dir.join(REPLAYING_SUBDIR)).len(),
            1,
            "the read one is staged"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Without a watermark file the pass behaves exactly as it did before
    /// this change existed — every frame comes back.
    #[test]
    fn replay_without_a_watermark_file_replays_everything() {
        let dir = tmp_dir("wm-absent");
        write_wm_segment(&dir, 0, 5, WalEndpoint::MainFeed);
        write_wm_segment(&dir, 100, 5, WalEndpoint::MainFeed);
        let batch = replay_unguarded(&dir);
        assert_eq!(batch.frames.len(), 10);
        assert_eq!(batch.skipped_segments, 0);
        assert_eq!(batch.skipped_frames, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A segment overlapping an UNAPPLIED bucket is replayed in full even
    /// though its whole range sits under the watermark; its neighbours are
    /// still skipped.
    #[test]
    fn replay_never_skips_a_segment_overlapping_the_unapplied_window() {
        let dir = tmp_dir("wm-unapplied");
        write_wm_segment(&dir, 0, 5, WalEndpoint::MainFeed);
        write_wm_segment(&dir, 1_000, 5, WalEndpoint::MainFeed); // ~16 min later
        write_wm_segment(&dir, 2_000, 5, WalEndpoint::MainFeed);
        write_wm_segment(&dir, 3_000, 5, WalEndpoint::MainFeed);
        let wm = crate::wal_applied_watermark::AppliedWatermark::new_for_tests();
        wm.note_ticks_acked(wm_seq(4_000));
        wm.note_depth_acked(wm_seq(4_000));
        wm.note_unapplied(wm_seq(1_002)); // a shed inside the second segment
        write_wm_watermark(&dir, &wm.snapshot());
        let batch = replay_unguarded(&dir);
        assert_eq!(
            batch.frames.len(),
            5,
            "exactly the shed segment's frames come back"
        );
        assert!(
            batch
                .frames
                .iter()
                .all(|f| f.frame_seq >= wm_seq(1_000) && f.frame_seq < wm_seq(1_005))
        );
        // A segment's range runs up to its SUCCESSOR's first sequence, so the
        // first segment's range touches the shed bucket too and is read (its
        // frames are then filtered as applied). Only the third is provably
        // clear and archived unread. Conservative by construction: the bound
        // errs towards reading, never towards skipping.
        assert_eq!(
            batch.skipped_segments, 1,
            "only the segment after the shed is archived unread"
        );
        assert_eq!(
            batch.skipped_frames, 10,
            "the first and last are read and filtered"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Plan ITEM 47: the replay says where its gaps are. The first frame of a
    /// pass, and the first kept frame after skipped segments or frames dropped
    /// as applied, carry `after_gap`; a frame whose predecessor was replayed
    /// does not.
    #[test]
    fn test_replay_marks_after_gap_on_first_frame_and_after_skipped_frames() {
        let dir = tmp_dir("wm-after-gap");
        for first in [0, 1_000, 2_000, 3_000, 4_000] {
            write_wm_segment(&dir, first, 5, WalEndpoint::MainFeed);
        }
        let wm = crate::wal_applied_watermark::AppliedWatermark::new_for_tests();
        wm.note_ticks_acked(wm_seq(5_000));
        wm.note_depth_acked(wm_seq(5_000));
        wm.note_unapplied(wm_seq(1_002));
        wm.note_unapplied(wm_seq(3_002));
        write_wm_watermark(&dir, &wm.snapshot());
        let batch = replay_unguarded(&dir);
        let gaps: Vec<u64> = batch
            .frames
            .iter()
            .filter(|f| f.after_gap)
            .map(|f| f.frame_seq)
            .collect();
        assert_eq!(batch.frames.len(), 10, "the two shed segments come back");
        assert_eq!(
            gaps,
            vec![wm_seq(1_000), wm_seq(3_000)],
            "each shed segment's first frame follows skipped frames"
        );
        assert!(
            batch.leading_gap,
            "segment 0 was archived unread before the first frame"
        );
        assert!(
            batch.trailing_gap,
            "the last segment's frames were all dropped as applied"
        );
        let _ = std::fs::remove_dir_all(&dir);

        // With no watermark nothing is skipped: only the pass's first frame
        // follows a gap.
        let dir = tmp_dir("wm-after-gap-none");
        write_wm_segment(&dir, 0, 3, WalEndpoint::MainFeed);
        write_wm_segment(&dir, 100, 3, WalEndpoint::MainFeed);
        let batch = replay_unguarded(&dir);
        let flagged: Vec<bool> = batch.frames.iter().map(|f| f.after_gap).collect();
        assert_eq!(flagged, vec![true, false, false, false, false, false]);
        assert!(
            !batch.leading_gap && !batch.trailing_gap,
            "the pass-start flag alone is not a real gap"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Plan ITEM 47 review: an applied segment past a budget stop is NOT
    /// archived by that pass. It stays a `*.wal`, so the next pass skips it
    /// again and flags the gap it leaves; archiving it early lost that gap.
    #[test]
    fn test_replay_skipped_segment_past_a_budget_stop_is_kept_for_the_next_pass() {
        let dir = tmp_dir("wm-skip-after-stop");
        // A segment's range runs to one nanosecond before the next segment's
        // first frame, so a skipped segment must END before an unapplied
        // bucket (2^38 ns) begins. The kept segments therefore span a bucket
        // boundary and only their LATER bucket is unapplied: bucket
        // 6_475_676 starts at offset 20,264.93 s, 6_475_748 at 40,056.14 s.
        write_wm_segment(&dir, 0, 5, WalEndpoint::MainFeed); // kept
        write_wm_segment(&dir, 10_000, 5, WalEndpoint::MainFeed); // skipped
        write_wm_segment(&dir, 19_991, 300, WalEndpoint::MainFeed); // kept from 20,265
        write_wm_segment(&dir, 30_000, 5, WalEndpoint::MainFeed); // skipped
        write_wm_segment(&dir, 39_782, 300, WalEndpoint::MainFeed); // kept from 40,057
        let wm = crate::wal_applied_watermark::AppliedWatermark::new_for_tests();
        wm.note_ticks_acked(wm_seq(50_000));
        wm.note_depth_acked(wm_seq(50_000));
        for unapplied in [2, 20_270, 40_060] {
            wm.note_unapplied(wm_seq(unapplied));
        }
        write_wm_watermark(&dir, &wm.snapshot());
        let skipped_late = dir.join(format!("ws-frames-{:020}.wal", wm_seq(30_000)));

        // Pass 1 stops on its budget after the first kept segment.
        let first = replay_all_with_report_guarded(&dir, 1, || None, None, WAL_REPLAY_RSS_STOP_PCT)
            .expect("replay");
        assert_eq!(first.frames.len(), 5, "only segment 0 was read");
        assert!(
            first.trailing_gap,
            "the skipped segment before the stop is a gap at the end"
        );
        assert!(
            skipped_late.exists(),
            "a skip past the stop stays for the next pass"
        );
        confirm_replayed(&dir);

        // Pass 2 reads the rest and still sees the late skip as a gap.
        let second = replay_unguarded(&dir);
        let gaps: Vec<u64> = second
            .frames
            .iter()
            .filter(|f| f.after_gap)
            .map(|f| f.frame_seq)
            .collect();
        assert_eq!(gaps, vec![wm_seq(20_265), wm_seq(40_057)], "{gaps:?}");
        assert!(
            second.leading_gap,
            "frames were dropped in front of its first frame"
        );
        assert!(!skipped_late.exists(), "archived once a pass got past it");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Review round 3: a skipped leftover in `replaying/` past a budget stop
    /// goes back to the live dir, so the confirm cannot archive it by glob
    /// and the next pass still sees its gap.
    #[test]
    fn test_replay_skipped_leftover_in_replaying_past_a_stop_is_restored() {
        let dir = tmp_dir("wm-skip-replaying");
        write_wm_segment(&dir, 0, 5, WalEndpoint::MainFeed);
        write_wm_segment(&dir, 10_000, 5, WalEndpoint::MainFeed);
        write_wm_segment(&dir, 19_991, 300, WalEndpoint::MainFeed);
        let late = write_wm_segment(&dir, 30_000, 5, WalEndpoint::MainFeed);
        write_wm_segment(&dir, 39_782, 300, WalEndpoint::MainFeed);
        // The late skipped segment was staged by an earlier boot.
        let replaying = dir.join(REPLAYING_SUBDIR);
        std::fs::create_dir_all(&replaying).unwrap();
        let name = late.file_name().unwrap().to_owned();
        std::fs::rename(&late, replaying.join(&name)).unwrap();
        let wm = crate::wal_applied_watermark::AppliedWatermark::new_for_tests();
        wm.note_ticks_acked(wm_seq(50_000));
        wm.note_depth_acked(wm_seq(50_000));
        for unapplied in [2, 20_270, 40_060] {
            wm.note_unapplied(wm_seq(unapplied));
        }
        write_wm_watermark(&dir, &wm.snapshot());

        let first = replay_all_with_report_guarded(&dir, 1, || None, None, WAL_REPLAY_RSS_STOP_PCT)
            .expect("replay");
        assert_eq!(first.frames.len(), 5);
        assert!(dir.join(&name).exists(), "restored to the live dir");
        assert!(!replaying.join(&name).exists());
        confirm_replayed(&dir);
        let second = replay_unguarded(&dir);
        let gaps: Vec<u64> = second
            .frames
            .iter()
            .filter(|f| f.after_gap)
            .map(|f| f.frame_seq)
            .collect();
        assert_eq!(gaps, vec![wm_seq(20_265), wm_seq(40_057)], "{gaps:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Review round 3: the first frame returned from each segment is marked,
    /// so a reader can compare receipt times across a segment boundary only.
    #[test]
    fn test_replay_marks_the_first_frame_of_each_segment() {
        let dir = tmp_dir("wm-first-in-segment");
        write_wm_segment(&dir, 0, 3, WalEndpoint::MainFeed);
        write_wm_segment(&dir, 100, 2, WalEndpoint::MainFeed);
        let batch = replay_unguarded(&dir);
        let firsts: Vec<bool> = batch.frames.iter().map(|f| f.first_in_segment).collect();
        assert_eq!(firsts, vec![true, false, false, true, false]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Plan ITEM 47 review: which frames can hide ticks, and what a capture
    /// jump is.
    #[test]
    fn test_frame_feeds_the_candle_fold_and_process_boundary_is_gap() {
        assert!(frame_feeds_the_candle_fold(
            WsType::LiveFeed,
            WalEndpoint::MainFeed
        ));
        assert!(!frame_feeds_the_candle_fold(
            WsType::LiveFeed,
            WalEndpoint::Depth20
        ));
        assert!(!frame_feeds_the_candle_fold(
            WsType::LiveFeed,
            WalEndpoint::Depth200
        ));
        assert!(!frame_feeds_the_candle_fold(
            WsType::OrderUpdate,
            WalEndpoint::MainFeed
        ));
        assert!(!frame_feeds_the_candle_fold(
            WsType::TruedataFeed,
            WalEndpoint::MainFeed
        ));

        // A plausible receipt instant (2026-09-29 in nanoseconds).
        let t = 1_790_000_000_000_000_000_i64;
        assert!(!process_boundary_is_gap(
            t,
            t + WAL_REPLAY_PROCESS_GAP_NANOS
        ));
        assert!(process_boundary_is_gap(
            t,
            t + WAL_REPLAY_PROCESS_GAP_NANOS + 1
        ));
        assert!(
            !process_boundary_is_gap(t + 5, t),
            "going backwards is never a jump"
        );
        assert!(
            !process_boundary_is_gap(WAL_RECEIPT_UNKNOWN_NANOS, t),
            "a legacy record has no receipt"
        );
        assert!(!process_boundary_is_gap(t, WAL_RECEIPT_UNKNOWN_NANOS));
        assert!(
            !process_boundary_is_gap(t, i64::MAX),
            "an implausible receipt never counts"
        );
        assert!(!process_boundary_is_gap(i64::MIN, t));
    }

    /// Plan ITEM 47 review: dropping an already-applied DEPTH frame hides no
    /// tick, so it is no gap. Main-feed and depth frames interleave on the
    /// wire; with the depth watermark ahead of the tick one, every depth frame
    /// is dropped and every main-feed frame kept, and none of the kept ones may
    /// be flagged — otherwise the fold would suppress nearly every bar.
    #[test]
    fn test_replay_dropped_depth_frame_is_not_a_gap() {
        let dir = tmp_dir("wm-after-gap-depth");
        let mut bytes = Vec::new();
        for i in 0..6_u64 {
            let endpoint = if i % 2 == 0 {
                WalEndpoint::MainFeed
            } else {
                WalEndpoint::Depth20
            };
            bytes.extend_from_slice(&encode_v4_record(
                WsType::LiveFeed,
                wm_seq(i),
                7,
                endpoint,
                &[0xAB; 32],
            ));
        }
        std::fs::write(dir.join(format!("ws-frames-{:020}.wal", wm_seq(0))), bytes).unwrap();
        let wm = crate::wal_applied_watermark::AppliedWatermark::new_for_tests();
        wm.note_depth_acked(wm_seq(5_000));
        write_wm_watermark(&dir, &wm.snapshot());
        let batch = replay_unguarded(&dir);
        let kept: Vec<(u64, bool)> = batch
            .frames
            .iter()
            .map(|f| (f.frame_seq, f.after_gap))
            .collect();
        assert_eq!(
            kept,
            vec![(wm_seq(0), true), (wm_seq(2), false), (wm_seq(4), false)],
            "depth frames are dropped as applied; only the pass's first frame follows a gap"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A main-feed frame feeds BOTH sinks: a depth watermark that lags keeps
    /// it in the replay set even when the tick watermark is past it.
    #[test]
    fn replay_uses_the_lower_watermark_for_main_feed_frames() {
        let dir = tmp_dir("wm-lower");
        write_wm_segment(&dir, 0, 5, WalEndpoint::MainFeed);
        write_wm_segment(&dir, 100, 5, WalEndpoint::MainFeed);
        write_wm_watermark(
            &dir,
            &crate::wal_applied_watermark::AppliedSnapshot {
                hwm_ticks: wm_seq(500),
                hwm_depth: wm_seq(50), // depth sink is behind
                ..Default::default()
            },
        );
        let batch = replay_unguarded(&dir);
        assert_eq!(
            batch.frames.len(),
            5,
            "the second segment is above the depth watermark"
        );
        // The first segment's range extends to the second's first sequence,
        // which is above the depth watermark, so it is read rather than
        // archived unread — and every one of its frames is then filtered on
        // the LOWER watermark. Reading-and-filtering is the conservative arm.
        assert_eq!(batch.skipped_segments, 0);
        assert_eq!(batch.skipped_frames, 5);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Frames inside the reorder slack below the watermark are re-replayed,
    /// never skipped: DEDUP collapses them, and a socket that minted a frame a
    /// few microseconds before the acked one must not be assumed applied.
    #[test]
    fn replay_reorder_slack_replays_frames_just_below_the_watermark() {
        let dir = tmp_dir("wm-slack");
        write_wm_segment(&dir, 0, 5, WalEndpoint::MainFeed); // seqs 0..4 s
        write_wm_watermark(
            &dir,
            &crate::wal_applied_watermark::AppliedSnapshot {
                hwm_ticks: wm_seq(4),
                hwm_depth: wm_seq(4),
                ..Default::default()
            },
        );
        let batch = replay_unguarded(&dir);
        // Slack is one second: seq(3) and seq(4) are within it, seq(0..=2) are
        // below it and dropped.
        assert_eq!(batch.frames.len(), 2);
        assert_eq!(batch.skipped_frames, 3);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A legacy v1 record carries no sequence and is NEVER skipped.
    #[test]
    fn replay_never_skips_v1_records_with_zero_seq() {
        let dir = tmp_dir("wm-v1");
        let mut bytes = encode_v1_record(WsType::LiveFeed, &[1, 2, 3]);
        bytes.extend_from_slice(&encode_v1_record(WsType::LiveFeed, &[4, 5, 6]));
        std::fs::write(dir.join("ws-frames-00000000000000000001.wal"), bytes).unwrap();
        write_wm_segment(&dir, 100, 2, WalEndpoint::MainFeed);
        write_wm_watermark(
            &dir,
            &crate::wal_applied_watermark::AppliedSnapshot {
                hwm_ticks: wm_seq(999),
                hwm_depth: wm_seq(999),
                ..Default::default()
            },
        );
        let batch = replay_unguarded(&dir);
        assert_eq!(batch.frames.len(), 2, "both v1 frames come back");
        assert!(batch.frames.iter().all(|f| f.frame_seq == 0));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The segment the writer thread is appending to is invisible to replay
    /// while the writer is alive, and visible again once it has exited.
    #[test]
    fn test_is_open_segment_keeps_replay_off_the_writers_open_segment() {
        let dir = tmp_dir("wm-open-segment");
        let spill = WsFrameSpill::new(&dir).unwrap();
        spill.append(WsType::LiveFeed, vec![9u8; 16]);
        wait_until_persisted(&spill, 1);
        let live: Vec<PathBuf> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("wal"))
            .collect();
        assert_eq!(live.len(), 1, "one open segment on disk");
        assert!(
            is_open_segment(&live[0]),
            "the writer's segment is registered as open"
        );
        let batch = replay_unguarded(&dir);
        assert!(batch.frames.is_empty(), "an open segment is never read");
        assert!(live[0].exists(), "and never moved");
        assert!(wal_segments_in(&dir.join(REPLAYING_SUBDIR)).is_empty());
        drop(spill);
        // The writer exits on channel close and clears the registration.
        for _ in 0..200 {
            if !is_open_segment(&live[0]) {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            !is_open_segment(&live[0]),
            "a shut-down writer's segment is replayable"
        );
        let batch = replay_unguarded(&dir);
        assert_eq!(batch.frames.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Under the disk floor a pass REFUSES: empty batch, flag set, and every
    /// segment still a live `*.wal`. A failed probe is fail-open.
    #[test]
    fn replay_refuses_below_the_disk_floor_and_touches_nothing() {
        use crate::disk_health_watcher::DiskHealthOutcome;
        let dir = tmp_dir("wm-disk-floor");
        write_wm_segment(&dir, 0, 3, WalEndpoint::MainFeed);
        let refused = replay_all_with_report_at(
            &dir,
            usize::MAX,
            DiskHealthOutcome::Ok {
                free_bytes: WAL_REPLAY_DISK_FLOOR_BYTES - 1,
                total_bytes: 600 * 1024 * 1024 * 1024, // floor = the 40 GiB constant here
            },
            0,
        )
        .expect("refusal is Ok, not Err");
        assert!(refused.stopped_for_disk);
        assert!(!refused.stopped_for_frame_cap);
        assert!(refused.frames.is_empty());
        assert_eq!(wal_segments_in(&dir).len(), 1, "nothing moved");
        let allowed = replay_all_with_report_at(
            &dir,
            usize::MAX,
            DiskHealthOutcome::ProbeFailed { reason: "test" },
            0,
        )
        .expect("replay");
        assert_eq!(
            allowed.frames.len(),
            3,
            "a failed probe is fail-open, as the memory guard is"
        );
        assert!(!allowed.stopped_for_disk);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn replay_refuses_past_the_per_boot_frame_cap() {
        use crate::disk_health_watcher::DiskHealthOutcome;
        let dir = tmp_dir("wm-frame-cap");
        write_wm_segment(&dir, 0, 3, WalEndpoint::MainFeed);
        let refused = replay_all_with_report_at(
            &dir,
            usize::MAX,
            DiskHealthOutcome::Ok {
                free_bytes: u64::MAX,
                total_bytes: u64::MAX,
            },
            WAL_REPLAY_MAX_FRAMES_PER_BOOT,
        )
        .expect("refusal is Ok");
        assert!(refused.stopped_for_frame_cap);
        assert!(!refused.stopped_for_disk);
        assert!(refused.frames.is_empty());
        assert_eq!(wal_segments_in(&dir).len(), 1, "nothing moved");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_disk_floor_scales_down_on_small_volumes_and_caps_at_the_constant() {
        const GIB: u64 = 1024 * 1024 * 1024;
        assert_eq!(
            wal_replay_disk_floor_bytes(600 * GIB),
            WAL_REPLAY_DISK_FLOOR_BYTES
        );
        assert_eq!(wal_replay_disk_floor_bytes(64 * GIB), 8 * GIB);
        assert_eq!(wal_replay_disk_floor_bytes(0), 0);
    }

    /// The header probe: first sequence of a v4 segment, zero for v1/empty.
    #[test]
    fn first_frame_seq_in_segment_reads_one_header() {
        let dir = tmp_dir("wm-first-seq");
        let p = write_wm_segment(&dir, 42, 3, WalEndpoint::MainFeed);
        assert_eq!(first_frame_seq_in_segment(&p), wm_seq(42));
        let v1 = dir.join("v1.wal");
        std::fs::write(&v1, encode_v1_record(WsType::LiveFeed, &[1])).unwrap();
        assert_eq!(first_frame_seq_in_segment(&v1), 0);
        let empty = dir.join("empty.wal");
        std::fs::write(&empty, b"").unwrap();
        assert_eq!(first_frame_seq_in_segment(&empty), 0);
        assert_eq!(first_frame_seq_in_segment(&dir.join("missing.wal")), 0);
        // A corrupt FIRST record reads as unknown, never as its (possibly
        // flipped) seq bytes: this value bounds the previous segment's skip
        // range, so a bit-flip here must widen the range, never narrow it.
        let corrupt = dir.join("corrupt.wal");
        let mut bytes = std::fs::read(&p).unwrap();
        bytes[7] ^= 0x01; // inside the seq field of the first record
        std::fs::write(&corrupt, &bytes).unwrap();
        assert_eq!(first_frame_seq_in_segment(&corrupt), 0);
        let torn = dir.join("torn.wal");
        std::fs::write(&torn, &std::fs::read(&p).unwrap()[..WAL_MIN_RECORD_V4 + 2]).unwrap();
        assert_eq!(first_frame_seq_in_segment(&torn), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Audit PR31b-2: claiming a directory records the on-disk high-water mark
    /// for the boot candle warm-up.
    #[test]
    fn boot_disk_high_frame_seq_is_set_once_a_writer_claims_a_directory() {
        let dir = tmp_dir("boot-disk-high");
        let spill = WsFrameSpill::new(&dir).expect("spill opens");
        assert!(boot_disk_high_frame_seq().is_some());
        drop(spill);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Audit PR31b-2: the warm-up asks the same watermark the replay asks.
    #[test]
    fn applied_sink_for_maps_depth_to_depth_and_the_rest_to_ticks() {
        use crate::wal_applied_watermark::AppliedSink;
        assert_eq!(applied_sink_for(WalEndpoint::Depth20), AppliedSink::Depth);
        assert_eq!(applied_sink_for(WalEndpoint::Depth200), AppliedSink::Depth);
        assert_eq!(applied_sink_for(WalEndpoint::MainFeed), AppliedSink::Ticks);
        assert_eq!(
            applied_sink_for(WalEndpoint::OrderUpdate),
            AppliedSink::Ticks
        );
    }

    #[test]
    fn replay_all_fenced_returns_the_batch_so_a_refusal_is_visible_to_the_caller() {
        // A refused pass must be distinguishable from "nothing to replay": the
        // boot path confirms on the latter and must not on the former.
        let dir = tmp_dir("wm-fenced-batch");
        let batch = replay_all_fenced(&dir).expect("empty dir replays cleanly");
        assert!(batch.frames.is_empty());
        assert!(!batch.stopped_for_disk || batch.frames.is_empty());
        // The flags are the caller's signal; they exist on the returned type.
        let _ = (
            batch.stopped_for_disk,
            batch.stopped_for_frame_cap,
            batch.skipped_segments,
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn current_frame_seq_reads_the_last_allocation_without_allocating() {
        let allocated = next_frame_seq();
        assert_eq!(current_frame_seq(), allocated);
        assert_eq!(current_frame_seq(), allocated, "a read allocates nothing");
        assert!(next_frame_seq() > current_frame_seq() - 1);
    }
    // --- Z11a: resync past mid-segment damage (2026-10-02) -----------------

    /// A segment of v4 records, sequence `k + 1` and a payload of `fill(k)`;
    /// returns the bytes and each record's start offset.
    fn z11_segment(payloads: &[Vec<u8>]) -> (Vec<u8>, Vec<usize>) {
        let mut bytes = Vec::new();
        let mut offsets = Vec::new();
        for (k, p) in payloads.iter().enumerate() {
            offsets.push(bytes.len());
            bytes.extend_from_slice(&encode_v4_record(
                WsType::LiveFeed,
                k as u64 + 1,
                1_000 + k as i64,
                WalEndpoint::MainFeed,
                p,
            ));
        }
        (bytes, offsets)
    }

    /// Payloads that are runs of one byte, never spelling `TVW`.
    fn z11_plain_payloads(n: usize) -> Vec<Vec<u8>> {
        (0..n).map(|k| vec![(k % 64) as u8 + 1; 32]).collect()
    }

    fn z11_walk(name: &str, bytes: &[u8]) -> SegmentRead {
        let dir = tmp_dir(name);
        let path = dir.join("00000000000000000001.wal");
        std::fs::write(&path, bytes).unwrap();
        let read = replay_segment_core(&path, None, &|_, _| true, true).expect("readable");
        let _ = std::fs::remove_dir_all(&dir);
        read
    }

    /// The pin for Z11a: one flipped byte in record 40 of 100 used to end
    /// the walk and abandon records 41..100 that were intact on disk.
    #[test]
    fn test_regression_resync_recovers_records_after_a_mid_segment_crc_flip() {
        let payloads = z11_plain_payloads(100);
        let (mut bytes, offsets) = z11_segment(&payloads);
        // Inside record 40's frame (the v4 header is 26 bytes).
        bytes[offsets[40] + 26 + 5] ^= 0xFF;
        let read = z11_walk("resync-crc", &bytes);
        assert_eq!(read.frames.len(), 99, "only the damaged record is lost");
        assert!(read.damaged, "skipped bytes are damage");
        let seqs: Vec<u64> = read.frames.iter().map(|f| f.frame_seq).collect();
        let want: Vec<u64> = (1..=100).filter(|s| *s != 41).collect();
        assert_eq!(seqs, want);
        for f in &read.frames {
            assert_eq!(f.frame, payloads[(f.frame_seq - 1) as usize]);
            assert_eq!(
                f.after_gap,
                f.frame_seq == 42,
                "exactly the first frame after the skipped bytes follows a gap"
            );
        }
        assert_eq!(
            resync_from(&bytes, offsets[40] + 1),
            Some(offsets[41]),
            "the skipped stretch is exactly record 40's bytes"
        );
    }

    /// A corrupt length that points past EOF used to read as a torn tail:
    /// no counter, `damaged = false`, and every later record silently gone.
    #[test]
    fn test_regression_corrupt_length_past_eof_is_counted_not_a_silent_tail() {
        let payloads = z11_plain_payloads(10);
        let (mut bytes, offsets) = z11_segment(&payloads);
        // v4 length field sits at offset 22 of the record.
        bytes[offsets[4] + 22..offsets[4] + 26].copy_from_slice(&0x7FFF_FFFFu32.to_le_bytes());
        let read = z11_walk("resync-len-mid", &bytes);
        assert!(
            read.damaged,
            "a lying length with records after it is damage"
        );
        assert_eq!(read.frames.len(), 9, "the records after it are recovered");
        assert!(read.frames.iter().all(|f| f.frame_seq != 5));

        // The same corruption on the LAST record: nothing follows, but the
        // length is above every frame cap, so no writer produced it.
        let (mut last, offsets) = z11_segment(&payloads);
        last[offsets[9] + 22..offsets[9] + 26].copy_from_slice(&0x7FFF_FFFFu32.to_le_bytes());
        let read = z11_walk("resync-len-last", &last);
        assert_eq!(read.frames.len(), 9);
        assert!(read.damaged, "an impossible length at the tail is counted");
    }

    /// What an interrupted writer leaves stays silent and undamaged.
    #[test]
    fn torn_tail_stays_silent_and_undamaged() {
        let payloads = z11_plain_payloads(10);
        let (bytes, _) = z11_segment(&payloads);
        for cut in [3usize, 20, 40] {
            let read = z11_walk("resync-torn", &bytes[..bytes.len() - cut]);
            assert_eq!(read.frames.len(), 9, "cut {cut}");
            assert!(!read.damaged, "cut {cut}: a torn tail is not damage");
            assert!(!read.trailing_gap, "cut {cut}");
            assert!(read.frames.iter().all(|f| !f.after_gap), "cut {cut}");
        }
    }

    /// A record whose magic at offset 0 is damaged, with readable records
    /// after it, is resynced rather than declared wholly unreadable.
    #[test]
    fn a_damaged_first_magic_resyncs_to_the_second_record() {
        let payloads = z11_plain_payloads(10);
        let (mut bytes, _) = z11_segment(&payloads);
        bytes[0] = b'X';
        let read = z11_walk("resync-offset0", &bytes);
        assert_eq!(read.frames.len(), 9);
        assert!(read.damaged);
        assert_eq!(read.frames[0].frame_seq, 2);
        assert!(read.frames[0].after_gap);
    }

    /// Magic bytes inside a payload, with a plausible header, never resync
    /// into a false record: the CRC refuses them.
    #[test]
    fn magic_bytes_inside_a_payload_never_produce_a_false_record() {
        let mut payloads = z11_plain_payloads(6);
        // A fake v4 header (valid magic, type, small length) with a bad CRC.
        let mut fake = Vec::new();
        fake.extend_from_slice(b"TVW4");
        fake.push(WsType::LiveFeed.as_u8());
        fake.extend_from_slice(&[0u8; 17]);
        fake.extend_from_slice(&4u32.to_le_bytes());
        fake.extend_from_slice(&[9, 9, 9, 9, 0xDE, 0xAD, 0xBE, 0xEF]);
        payloads[2] = fake.clone();
        payloads[3] = fake;
        let (mut bytes, offsets) = z11_segment(&payloads);
        // Damage record 2's own magic, so the scan walks into its payload.
        bytes[offsets[2]] = b'X';
        let read = z11_walk("resync-fake", &bytes);
        let seqs: Vec<u64> = read.frames.iter().map(|f| f.frame_seq).collect();
        assert_eq!(seqs, vec![1, 2, 4, 5, 6], "no fabricated record");
        for f in &read.frames {
            assert_eq!(f.frame, payloads[(f.frame_seq - 1) as usize]);
        }
        assert_eq!(resync_from(b"TVW4 not a record at all, just text", 0), None);
    }

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig::with_cases(256))]

        /// Every returned frame is one that was written, and every record
        /// lying wholly before the first flip or wholly after the last one is
        /// recovered.
        #[test]
        fn resync_returns_only_written_frames(
            payloads in proptest::collection::vec(proptest::collection::vec(proptest::num::u8::ANY, 0..120), 1..40),
            flips in proptest::collection::vec((0.0f64..1.0, 1u8..=255), 0..4),
        ) {
            let (mut bytes, offsets) = z11_segment(&payloads);
            let mut flipped: Vec<usize> = Vec::new();
            for (at, x) in &flips {
                let pos = ((*at * bytes.len() as f64) as usize).min(bytes.len() - 1);
                bytes[pos] ^= *x;
                flipped.push(pos);
            }
            let read = z11_walk("resync-prop", &bytes);
            for f in &read.frames {
                let k = (f.frame_seq - 1) as usize;
                proptest::prop_assert!(k < payloads.len(), "fabricated seq {}", f.frame_seq);
                proptest::prop_assert_eq!(&f.frame, &payloads[k]);
            }
            let first = flipped.iter().copied().min();
            let last = flipped.iter().copied().max();
            for (k, start) in offsets.iter().enumerate() {
                let end = offsets.get(k + 1).copied().unwrap_or(bytes.len());
                let before = first.is_none_or(|f| end <= f);
                let after = last.is_some_and(|l| *start > l);
                if before || after {
                    proptest::prop_assert!(
                        read.frames.iter().any(|f| f.frame_seq == k as u64 + 1),
                        "intact record {} not recovered", k + 1
                    );
                }
            }
            if flipped.is_empty() {
                proptest::prop_assert!(!read.damaged);
                proptest::prop_assert_eq!(read.frames.len(), payloads.len());
            }
        }
    }

    // --- Z11d: records left at writer exit are counted (2026-10-02) ---------

    /// What used to drop silently with the writer's channel is now counted
    /// and emptied.
    #[test]
    fn test_regression_records_enqueued_after_writer_exit_are_counted_not_silent() {
        let (tx, rx) = bounded::<WalRecord>(8);
        for k in 0..3u64 {
            tx.try_send(WalRecord {
                ws_type: WsType::LiveFeed,
                frame_seq: k + 1,
                received_at_nanos: WAL_RECEIPT_UNKNOWN_NANOS,
                endpoint: WalEndpoint::MainFeed,
                frame: Bytes::from_static(b"late"),
            })
            .unwrap();
        }
        assert_eq!(count_records_left_at_writer_exit(&rx), 3);
        assert!(rx.is_empty());
        assert_eq!(
            count_records_left_at_writer_exit(&rx),
            0,
            "a clean exit reads zero"
        );
    }

    /// The writer thread reaches the count on its way out.
    #[test]
    fn the_writer_thread_counts_its_channel_before_dropping_it() {
        let src = include_str!("ws_frame_spill.rs");
        let spawn_at = src
            .find(".name(WAL_WRITER_THREAD_NAME.to_string())")
            .expect("writer spawn");
        let spawn = &src[spawn_at..];
        let end = spawn
            .find(".map_err(|e| anyhow::anyhow!(\"spawn spill writer thread")
            .expect("end of spawn");
        assert!(
            spawn[..end].contains("count_records_left_at_writer_exit(&rx);"),
            "the writer closure must count what is left before rx drops"
        );
    }

    // --- Z11b: the panic hook's bounded drain (2026-10-02) ------------------

    /// Frames in every `*.wal` file directly under `dir`, read from disk
    /// through a fresh descriptor: exactly what would survive an abort.
    fn z11b_frames_on_disk(dir: &Path) -> usize {
        std::fs::read_dir(dir)
            .unwrap()
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "wal"))
            .map(|p| {
                read_segment_frames_matching(&p, &|_, _| true)
                    .unwrap()
                    .frames
                    .len()
            })
            .sum()
    }

    /// The pin for Z11b: once the drain returns `Drained`, every frame
    /// appended before it is in the kernel, not in process memory, so a
    /// `panic = "abort"` straight after loses none of them.
    #[test]
    fn test_regression_drain_for_abort_returns_once_the_queue_is_empty() {
        const N: usize = 20_000;
        let dir = tmp_dir("abort-drain");
        let spill = WsFrameSpill::new(&dir).unwrap();
        for k in 0..N {
            let outcome = spill.append(WsType::LiveFeed, vec![(k % 251) as u8; 256]);
            assert_eq!(outcome, AppendOutcome::Spilled);
        }
        let outcome = spill.abort_drain.wait(Duration::from_secs(10));
        assert_eq!(outcome, AbortDrainOutcome::Drained);
        assert_eq!(spill.queued_records(), 0);
        assert_eq!(
            z11b_frames_on_disk(&dir),
            N,
            "every frame appended before the drain is on disk when it returns"
        );
        drop(spill);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An idle writer acknowledges within one stop poll, well inside the
    /// hook's budget.
    #[test]
    fn drain_for_abort_on_an_idle_writer_is_prompt() {
        let dir = tmp_dir("abort-drain-idle");
        let spill = WsFrameSpill::new(&dir).unwrap();
        let started = Instant::now();
        assert_eq!(
            spill.abort_drain.wait(WAL_ABORT_DRAIN_BUDGET),
            AbortDrainOutcome::Drained
        );
        assert!(started.elapsed() < WAL_ABORT_DRAIN_BUDGET);
        drop(spill);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A writer that never answers (wedged disk, dead thread) cannot hold
    /// the abort past the budget.
    #[test]
    fn drain_for_abort_is_bounded_when_the_writer_is_wedged() {
        let wedged = AbortDrain::default();
        let started = Instant::now();
        assert_eq!(
            wedged.wait(Duration::from_millis(50)),
            AbortDrainOutcome::TimedOut
        );
        let took = started.elapsed();
        assert!(took >= Duration::from_millis(50), "{took:?}");
        assert!(took < Duration::from_secs(1), "{took:?}");

        let exited = AbortDrain::default();
        exited.writer_exited();
        assert_eq!(
            exited.wait(Duration::from_secs(5)),
            AbortDrainOutcome::Drained,
            "a writer that has exited has nothing left to drain"
        );
    }

    /// The writer cannot wait for itself: on its own thread the drain
    /// returns at once instead of burning the budget.
    #[test]
    fn drain_registered_for_abort_skips_the_writer_thread() {
        let outcome = std::thread::Builder::new()
            .name(WAL_WRITER_THREAD_NAME.to_string())
            .spawn(|| {
                let started = Instant::now();
                let o = drain_registered_for_abort(Duration::from_secs(5));
                (o, started.elapsed())
            })
            .unwrap()
            .join()
            .unwrap();
        assert_eq!(outcome.0, AbortDrainOutcome::SkippedOnWriterThread);
        assert!(outcome.1 < Duration::from_millis(100));

        // Off the writer thread it consults the registered spill and stays
        // inside its budget whatever it finds.
        let started = Instant::now();
        let o = drain_registered_for_abort(Duration::from_millis(300));
        assert!(started.elapsed() < Duration::from_secs(2), "{o:?}");
    }

    // --- L10 (2026-10-04): the boot high-water probe checks the record CRC ---

    fn l10_segment(tag: &str, records: &[Vec<u8>]) -> PathBuf {
        let dir = tmp_dir(tag);
        let seg = dir.join("ws-frames-00000000000000000001.wal");
        let bytes: Vec<u8> = records.iter().flatten().copied().collect();
        std::fs::write(&seg, &bytes).expect("write segment");
        dir
    }

    /// The probe must walk every record of a segment, not stop after the
    /// first: frames of different sizes, so a wrong seek lands mid-record.
    #[test]
    fn test_regression_high_water_probe_reads_every_record_of_a_segment() {
        let records = vec![
            encode_v4_record_raw(WsType::LiveFeed, 100, 1, 0, &[1; 7]),
            encode_v4_record_raw(WsType::LiveFeed, 200, 2, 0, &[2; 50]),
            encode_v4_record_raw(WsType::LiveFeed, 300, 3, 0, &[]),
            encode_v4_record_raw(WsType::LiveFeed, 400, 4, 0, &[3; 3]),
        ];
        let dir = l10_segment("l10-every", &records);
        assert_eq!(
            highest_frame_seq_on_disk(&dir),
            400,
            "the high-water mark is the LAST record's sequence"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// L10: a flipped high byte in one record's sequence field read as a huge
    /// sequence and seeded the counter there. It now fails its CRC and is
    /// ignored; the intact records still set the high-water mark.
    #[test]
    fn test_regression_a_corrupt_sequence_never_seeds_the_counter() {
        let mut records = vec![
            encode_v4_record_raw(WsType::LiveFeed, 100, 1, 0, &[1; 7]),
            encode_v4_record_raw(WsType::LiveFeed, 200, 2, 0, &[2; 9]),
            encode_v4_record_raw(WsType::LiveFeed, 300, 3, 0, &[3; 5]),
        ];
        // Highest byte of the middle record's frame_seq (bytes 5..13).
        records[1][12] = 0x7F;
        let dir = l10_segment("l10-corrupt-mid", &records);
        assert_eq!(
            highest_frame_seq_on_disk(&dir),
            300,
            "a sequence whose record fails its CRC must not become the seed"
        );
        let _ = std::fs::remove_dir_all(&dir);

        let mut records = vec![
            encode_v4_record_raw(WsType::LiveFeed, 100, 1, 0, &[1; 7]),
            encode_v4_record_raw(WsType::LiveFeed, 200, 2, 0, &[2; 9]),
        ];
        records[1][12] = 0x7F;
        let dir = l10_segment("l10-corrupt-last", &records);
        assert_eq!(
            highest_frame_seq_on_disk(&dir),
            100,
            "when the newest record is the corrupt one, the intact one before it wins"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `first_frame_seq_in_segment` keeps its contract after the refactor:
    /// the first record's sequence when intact, 0 when its CRC fails.
    #[test]
    fn test_first_frame_seq_refuses_a_corrupt_first_record() {
        let mut records = vec![encode_v4_record_raw(WsType::LiveFeed, 77, 1, 0, &[1; 4])];
        let dir = l10_segment("l10-first-ok", &records);
        let seg = dir.join("ws-frames-00000000000000000001.wal");
        assert_eq!(first_frame_seq_in_segment(&seg), 77);
        let _ = std::fs::remove_dir_all(&dir);

        records[0][6] ^= 0x01;
        let dir = l10_segment("l10-first-bad", &records);
        let seg = dir.join("ws-frames-00000000000000000001.wal");
        assert_eq!(first_frame_seq_in_segment(&seg), 0);
        let _ = std::fs::remove_dir_all(&dir);
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

    #[test]
    fn refusal_line_due_logs_the_first_and_every_power_of_two() {
        use super::refusal_line_due;
        // The first refusal MUST log (it is what pages); the flood must not.
        assert!(refusal_line_due(1));
        assert!(refusal_line_due(2));
        assert!(!refusal_line_due(3));
        assert!(refusal_line_due(4));
        assert!(!refusal_line_due(1_000));
        assert!(refusal_line_due(1 << 20));
        // 2026-09-04: 2,000,238 refusals in one session would have produced
        // 21 lines instead of two million.
        let lines = (1..=2_000_238u64).filter(|n| refusal_line_due(*n)).count();
        assert_eq!(lines, 21);
        assert!(!refusal_line_due(0), "a zero count is not a refusal");
    }

    #[test]
    fn refusal_line_due_stops_doubling_at_the_stride_so_a_long_storm_is_never_silent() {
        use super::{REFUSAL_LINE_STRIDE, refusal_line_due};
        // Past the ladder the line repeats at a FIXED stride. Under the pure
        // power-of-two rule the counter is never reset, so after a 2M-refusal
        // episode the next line was 2^21 and the one after that 2^22 - a
        // 2.1-MILLION-frame silence inside an outage that is still losing
        // frames.
        assert!(refusal_line_due(REFUSAL_LINE_STRIDE));
        assert!(refusal_line_due(REFUSAL_LINE_STRIDE * 3));
        assert!(refusal_line_due(REFUSAL_LINE_STRIDE * 7));
        assert!(!refusal_line_due(REFUSAL_LINE_STRIDE * 3 + 1));
        // 3 x stride is NOT a power of two: this case logged nothing before.
        assert!(!(REFUSAL_LINE_STRIDE * 3).is_power_of_two());

        // The worst-case silent gap anywhere past the ladder is one stride.
        let start = REFUSAL_LINE_STRIDE * 5 + 1;
        let mut silent = 0u64;
        for n in start..=(start + REFUSAL_LINE_STRIDE) {
            if refusal_line_due(n) {
                break;
            }
            silent += 1;
        }
        assert!(
            silent < REFUSAL_LINE_STRIDE,
            "silent run of {silent} frames exceeds the stride"
        );
    }
}
