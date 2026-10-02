//! WAL applied-watermark — the record of which captured frames have ALREADY
//! reached the database, so a restart never replays them a second time.
//!
//! # The defect this closes (MEASURED 2026-09-03/04, not inferred)
//!
//! The frame WAL is capture-at-receipt: every frame is appended BEFORE the
//! ring reservation (`pool_supervisor.rs`), and nothing ever marked a frame
//! "applied". The only things that removed a live segment were the NEXT
//! boot's replay and the 48-hour prune. So every restart re-read the whole
//! prior session up to its budgets, refolded ~15–28 M ticks and ~10–30 M
//! depth rows through DEDUP into hour partitions, rescued whatever the sink
//! could not land into spill files, and cost **25–75 GB of disk per
//! restart**. Seven restarts on 2026-09-03 consumed ~300 GB; the disk reached
//! 20 KB free at 23:07 IST and 2026-09-04 booted onto it (`used_pct:99`),
//! WAL-suspended 26 tables, and persisted nothing all day.
//!
//! Every one of those frames had been folded LIVE, minutes earlier, on the
//! session that wrote them. The replay was 100 % redundant work — and the
//! only reason it ran was that the process had no memory of what it had
//! already done.
//!
//! # What this module is
//!
//! A small, durable, CRC-checked file beside the WAL segments —
//! [`APPLIED_WATERMARK_FILE`] — holding:
//!
//! * `hwm_ticks` / `hwm_depth`: the highest `capture_seq` whose rows were
//!   ACKED by QuestDB or DURABLY RESCUED to a spill file, per sink. A rescue
//!   counts as applied because the spill tier is re-ingestable and its replay
//!   is DEDUP-idempotent — the frame does not need the WAL any more.
//! * an UNAPPLIED map: fixed-size buckets over `capture_seq` recording every
//!   frame that was captured but never applied — a `RingFull` shed, a tick
//!   whose ILP append failed, a rescue that itself failed. A segment
//!   overlapping such a bucket is always replayed in full.
//!
//! In RAM it is a handful of atomics. The hot `Captured` arm of the drain
//! touches NOTHING here; the producers are the sink threads (one `fetch_max`
//! per batch) and the cold refusal arms. Persistence is tmp+rename, at most
//! once a second, on the sink thread, into pre-built paths.
//!
//! # What it guarantees, and what it does not
//!
//! The watermark only ever REDUCES the replay set. `capture_seq` derivation is
//! untouched, so a frame that is re-offered inside the reorder slack collapses
//! onto its original row exactly as before. Every failure direction is
//! "replay more": an absent, short, corrupt or implausible file, a sink that
//! never acks, a bucket-table collision, a persist that fails — each leaves
//! the next boot doing exactly what it did before this module existed.
//!
//! NOT fixed here: the live per-session disk burn, depth's 24x row volume,
//! and a WAL-suspended table that ACKs silently (that lie advances this
//! watermark exactly as it advanced the old `confirm_replayed`; the
//! `wal_suspension_watcher` owns it).

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use tracing::warn;

/// The watermark file, beside the `*.wal` segments. Not a `.wal`, so the
/// segment glob and the prune never see it.
pub const APPLIED_WATERMARK_FILE: &str = "applied.tvaw";
/// Written first, then renamed over the live file, so a crash mid-write leaves
/// the previous snapshot intact.
pub const APPLIED_WATERMARK_TMP: &str = "applied.tvaw.tmp";
const APPLIED_MAGIC: [u8; 4] = *b"TVAW";
const APPLIED_VERSION: u8 = 1;

/// `capture_seq >> UNAPPLIED_BUCKET_SHIFT` is a bucket id. The sequence is a
/// nanosecond base with 17 reserved low bits, so `2^38` ns ≈ 4.6 minutes per
/// bucket — a session of ~6.5 hours spans ~85 buckets, and a segment
/// (~128 MiB, ~1.6 min at the measured rate) overlaps at most two.
pub const UNAPPLIED_BUCKET_SHIFT: u32 = 38;
/// Slots in the fixed-size bucket table: `128 × 4.6 min ≈ 9.8 h` before ids
/// wrap onto occupied slots, which is longer than any session this box runs.
pub const UNAPPLIED_BUCKETS: usize = 128;
/// Persist at most this often from the sink threads.
pub const APPLIED_PERSIST_INTERVAL_NANOS: u64 = 1_000_000_000;
/// Frames within this much of the watermark are re-replayed rather than
/// skipped. `next_frame_seq` is `max(prev+1, now)`, so sixteen sockets reorder
/// against each other by microseconds; one second of slack is three orders of
/// magnitude of margin, and a re-replayed frame collapses on DEDUP.
pub const REPLAY_REORDER_SLACK_SEQ: u64 = 1_000_000_000;
/// How often `wait_for_offload_drained` re-checks the writer counters.
/// Boot / catch-up only, never a hot path.
pub const OFFLOAD_DRAIN_POLL_MILLIS: u64 = 50;
/// A watermark further than this ahead of the wall clock is implausible
/// (a stepped clock, a foreign directory) and is ignored — replay more.
const APPLIED_MAX_FUTURE_NANOS: u64 = 86_400 * 1_000_000_000;

/// magic | version | flags | pad | hwm_ticks | hwm_depth | persisted_at | dir_tag
const HEADER_LEN: usize = 4 + 1 + 1 + 2 + 8 + 8 + 8 + 8;
const BODY_LEN: usize = UNAPPLIED_BUCKETS * 16;
/// The whole file: header + buckets + crc32.
pub const APPLIED_WATERMARK_LEN: usize = HEADER_LEN + BODY_LEN + 4;

const FLAG_DEPTH_UNTRACKED: u8 = 0b01;
const FLAG_OVERFLOWED: u8 = 0b10;

/// Counter: the sink-side persist could not write the file (loss-shaped name,
/// so it is paired with a `warn!` at its one emit site).
pub const APPLIED_PERSIST_FAILED_COUNTER: &str = "tv_wal_applied_watermark_persist_failed_total";
/// Counter: a file was present but rejected (short, bad magic, bad crc,
/// implausible values). Replay proceeds in full.
pub const APPLIED_INVALID_COUNTER: &str = "tv_wal_applied_watermark_invalid_total";

/// How long an acknowledgement must have stood before the PERSISTED
/// watermark may cover it (Z8b, 2026-10-02).
///
/// An ILP `2xx` means QuestDB wrote the rows into its files, not that they
/// reached the disk: its default `cairo.commit.mode` is `nosync`, which leaves
/// them in the kernel page cache. A host or kernel crash keeps the watermark
/// file (it is fsynced before its rename) and can lose those pages, so a
/// watermark persisted the instant it was acked can vouch for rows the disk
/// never got, and the next boot archives their WAL segment unread.
///
/// Switching QuestDB to `sync` was MEASURED and rejected on 2026-10-02: a
/// 1,000-row tick flush went from p50 3.3 ms to 21–26 ms and a 10,000-row
/// depth flush from 14 ms to 37–42 ms. Instead the value written to disk is
/// the watermark that was already acknowledged at least this long ago. Linux
/// writes back dirty pages older than `dirty_expire_centisecs` (30 s by
/// default) on a `dirty_writeback_centisecs` (5 s) cadence, so a commit older
/// than ~35 s has normally reached the disk; 60 s leaves margin.
///
/// The RAM watermark is NOT delayed — only what is written to the file.
///
/// **Honest limit:** this relies on the kernel's writeback timing. It is not
/// an fsync of QuestDB's files, and a host tuned with a longer
/// `dirty_expire_centisecs`, or a disk too saturated to write back on time,
/// can still lose acknowledged rows. A host crash replays at most the last
/// ~60 s of frames more than strictly needed; the DEDUP keys absorb them.
pub const WATERMARK_DURABILITY_LAG_SECS: u64 = 60;
const WATERMARK_DURABILITY_LAG_NANOS: u64 = WATERMARK_DURABILITY_LAG_SECS * 1_000_000_000;
/// Samples kept by [`DurabilityLag`]. With one slot per second at most, 128
/// slots hold more than two minutes of history — over twice the lag.
pub const DURABILITY_SAMPLE_SLOTS: usize = 128;
/// A new sample slot opens at most this often; a persist inside the window
/// refreshes the newest slot instead, so a burst of persists cannot push the
/// samples the lag needs out of the ring.
pub const DURABILITY_SAMPLE_SPACING_NANOS: u64 = 1_000_000_000;
const _: () = assert!(
    (DURABILITY_SAMPLE_SLOTS as u64 - 1) * DURABILITY_SAMPLE_SPACING_NANOS
        > WATERMARK_DURABILITY_LAG_NANOS + DURABILITY_SAMPLE_SPACING_NANOS,
    "the sample ring must outlive the durability lag"
);

/// Slots per sink for the queued-rescue floors (Z7, 2026-10-02). A sink's
/// rescue queue holds `RESCUE_QUEUE_DEPTH` (2) batches plus the one its thread
/// is writing, so three are live at most; eight leave room for a second
/// writer of the same sink. A full table falls back to marking the batch's
/// range unapplied, which only replays more.
pub const RESCUE_FLOOR_SLOTS: usize = 8;
/// Counter: a rescue hand-off found no free floor slot, so its range was
/// marked unapplied instead (it is replayed next boot even if it lands).
pub const RESCUE_FLOOR_FULL_COUNTER: &str = "tv_wal_rescue_floor_full_total";

/// A queued rescue batch's hold on the persisted watermark (Z7). Returned by
/// [`AppliedWatermark::hold_rescue_floor`], carried with the batch, and handed
/// back to [`AppliedWatermark::release_rescue_floor`] once the rescue write is
/// on disk (or has failed and marked its range unapplied).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RescueFloor {
    sink: AppliedSink,
    slot: u8,
}

#[derive(Debug, Clone, Copy)]
struct DurabilitySample {
    /// Monotonic nanos when the watermarks below were read. Every ack they
    /// include happened at or before this instant.
    taken_at: u64,
    hwm_ticks: u64,
    hwm_depth: u64,
}

/// The ring behind [`WATERMARK_DURABILITY_LAG_SECS`] (Z8b).
///
/// Fixed-size: [`DURABILITY_SAMPLE_SLOTS`] samples of `(time, ticks, depth)`
/// in an array, so it never allocates after construction. Written and read
/// only under the persist lock, about once a second, on the sink threads —
/// never on the frame drain. [`Self::record`] is O(1); [`Self::durable_at`]
/// is a newest-first scan bounded by [`DURABILITY_SAMPLE_SLOTS`] that stops
/// at the first sample old enough, so in steady state it visits ~61 slots.
#[derive(Debug)]
pub struct DurabilityLag {
    samples: [DurabilitySample; DURABILITY_SAMPLE_SLOTS],
    newest: usize,
    filled: usize,
    newest_opened_at: u64,
    durable_ticks: u64,
    durable_depth: u64,
}

impl Default for DurabilityLag {
    fn default() -> Self {
        Self::new()
    }
}

impl DurabilityLag {
    /// An empty ring with a zero durable floor.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            samples: [DurabilitySample {
                taken_at: 0,
                hwm_ticks: 0,
                hwm_depth: 0,
            }; DURABILITY_SAMPLE_SLOTS],
            newest: 0,
            filled: 0,
            newest_opened_at: 0,
            durable_ticks: 0,
            durable_depth: 0,
        }
    }

    /// Raises the durable floor. The boot seed comes from the file the
    /// previous process wrote, which was itself held back by the lag, so it
    /// is as durable as anything this process can know.
    pub fn seed(&mut self, hwm_ticks: u64, hwm_depth: u64) {
        self.durable_ticks = self.durable_ticks.max(hwm_ticks);
        self.durable_depth = self.durable_depth.max(hwm_depth);
    }

    /// Records the live watermarks as read at `now` (monotonic nanos). O(1).
    pub fn record(&mut self, now: u64, hwm_ticks: u64, hwm_depth: u64) {
        if self.filled == 0 {
            self.newest = 0;
            self.filled = 1;
            self.newest_opened_at = now;
        } else if now < self.newest_opened_at
            || now
                >= self
                    .newest_opened_at
                    .saturating_add(DURABILITY_SAMPLE_SPACING_NANOS)
        {
            self.newest = (self.newest + 1) % DURABILITY_SAMPLE_SLOTS;
            self.filled = (self.filled + 1).min(DURABILITY_SAMPLE_SLOTS);
            self.newest_opened_at = now;
        }
        // Inside the spacing window the newest slot is refreshed: its time
        // moves forward with its values, so a slot never claims an older read
        // than the one it holds.
        self.samples[self.newest] = DurabilitySample {
            taken_at: now,
            hwm_ticks,
            hwm_depth,
        };
    }

    /// The watermarks that were already acknowledged at least `lag_nanos`
    /// before `now`: the newest sample old enough, folded into the durable
    /// floor (monotone). No sample old enough leaves the floor where it is —
    /// the direction is always "replay more".
    #[must_use]
    pub fn durable_at(&mut self, now: u64, lag_nanos: u64) -> (u64, u64) {
        // O(1) EXEMPT: bounded by DURABILITY_SAMPLE_SLOTS, once-a-second persist, off the drain
        for back in 0..self.filled {
            let at = (self.newest + DURABILITY_SAMPLE_SLOTS - back) % DURABILITY_SAMPLE_SLOTS;
            let sample = self.samples[at];
            if sample.taken_at <= now && now - sample.taken_at >= lag_nanos {
                self.durable_ticks = self.durable_ticks.max(sample.hwm_ticks);
                self.durable_depth = self.durable_depth.max(sample.hwm_depth);
                break;
            }
        }
        (self.durable_ticks, self.durable_depth)
    }
}

/// Monotonic nanoseconds since the first call in this process. The durability
/// lag measures elapsed time, so a wall-clock step (NTP) can neither release
/// it early nor hold it forever.
fn mono_nanos() -> u64 {
    static EPOCH: OnceLock<std::time::Instant> = OnceLock::new();
    let epoch = EPOCH.get_or_init(std::time::Instant::now);
    u64::try_from(epoch.elapsed().as_nanos()).unwrap_or(u64::MAX)
}

/// `hwm` held strictly below `floor` (the lowest sequence of a queued rescue
/// batch); `u64::MAX` means no floor.
const fn cap_below_floor(hwm: u64, floor: u64) -> u64 {
    if floor == u64::MAX || hwm < floor {
        hwm
    } else {
        floor.saturating_sub(1)
    }
}

/// Which sink a frame's rows go to; decides which watermark covers it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppliedSink {
    Ticks,
    Depth,
}

/// A point-in-time copy of the watermark, as loaded from disk or taken from
/// RAM. ~2 KiB, cold path only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppliedSnapshot {
    pub hwm_ticks: u64,
    pub hwm_depth: u64,
    pub depth_untracked: bool,
    pub overflowed: bool,
    pub persisted_at_nanos: u64,
    /// FNV-1a of the canonical WAL directory this file was written beside.
    /// A file copied or restored in from ANOTHER directory carries the wrong
    /// tag and is rejected by [`Self::load`]: its watermark says nothing about
    /// the segments it now sits next to, and trusting it would archive frames
    /// unread. `0` = unbound (tests, or a snapshot taken from RAM before bind).
    pub dir_tag: u64,
    /// `(bucket_id, mark)` per slot; `(0, _)` is an empty slot, any other id
    /// is an unapplied bucket. `mark` is informational (1 once marked).
    pub buckets: [(u64, u64); UNAPPLIED_BUCKETS],
}

impl Default for AppliedSnapshot {
    fn default() -> Self {
        Self {
            hwm_ticks: 0,
            hwm_depth: 0,
            depth_untracked: false,
            overflowed: false,
            persisted_at_nanos: 0,
            dir_tag: 0,
            buckets: [(0, 0); UNAPPLIED_BUCKETS],
        }
    }
}

impl AppliedSnapshot {
    /// The highest `capture_seq` below which a frame for `sink` is applied,
    /// AFTER the reorder slack. `0` means "nothing is known to be applied".
    ///
    /// A main-feed frame feeds BOTH sinks (ticks plus inline depth), so it is
    /// applied only when the LOWER of the two watermarks covers it — unless
    /// the lane runs without a depth sink at all, in which case inline depth
    /// is discarded by design and the tick watermark alone decides.
    #[must_use]
    pub fn skip_below(&self, sink: AppliedSink) -> u64 {
        let raw = match sink {
            AppliedSink::Depth => self.hwm_depth,
            AppliedSink::Ticks => {
                if self.depth_untracked {
                    self.hwm_ticks
                } else {
                    self.hwm_ticks.min(self.hwm_depth)
                }
            }
        };
        raw.saturating_sub(REPLAY_REORDER_SLACK_SEQ)
    }

    /// `true` when ANY sequence in `[lo, hi]` was recorded as captured-but-
    /// unapplied, or when the table can no longer answer (overflowed, or the
    /// range spans more buckets than the table holds). Both fail towards
    /// "replay it".
    #[must_use]
    pub fn range_has_unapplied(&self, lo: u64, hi: u64) -> bool {
        if self.overflowed || lo > hi {
            return true;
        }
        let first = lo >> UNAPPLIED_BUCKET_SHIFT;
        let last = hi >> UNAPPLIED_BUCKET_SHIFT;
        if last.saturating_sub(first) >= UNAPPLIED_BUCKETS as u64 {
            return true;
        }
        // O(1) EXEMPT: bounded by UNAPPLIED_BUCKETS, boot-time replay decision
        let mut id = first;
        loop {
            // A claimed id IS the mark. The count beside it is informational
            // and may lag the id by one store, so it is never load-bearing:
            // an `(id, 0)` snapshot taken between the two stores must still
            // read as unapplied, never as clean.
            let (slot_id, _count) = self.buckets[slot_of(id)];
            if slot_id == id {
                return true;
            }
            if id == last {
                return false;
            }
            id += 1;
        }
    }

    /// `true` when every frame with a sequence in `[lo, hi]` for `sink` is
    /// known to be applied: the whole range sits below the slack-adjusted
    /// watermark and overlaps no unapplied bucket. `lo == 0` (a v1 record with
    /// no sequence) is never applied.
    #[must_use]
    pub fn range_is_applied(&self, sink: AppliedSink, lo: u64, hi: u64) -> bool {
        if lo == 0 || lo > hi {
            return false;
        }
        let below = self.skip_below(sink);
        if below == 0 || hi > below {
            return false;
        }
        !self.range_has_unapplied(lo, hi)
    }

    /// One frame. Same rule as [`Self::range_is_applied`] on a single point.
    #[must_use]
    pub fn frame_is_applied(&self, sink: AppliedSink, seq: u64) -> bool {
        self.range_is_applied(sink, seq, seq)
    }

    fn to_bytes(&self) -> [u8; APPLIED_WATERMARK_LEN] {
        let mut out = [0u8; APPLIED_WATERMARK_LEN];
        out[0..4].copy_from_slice(&APPLIED_MAGIC);
        out[4] = APPLIED_VERSION;
        let mut flags = 0u8;
        if self.depth_untracked {
            flags |= FLAG_DEPTH_UNTRACKED;
        }
        if self.overflowed {
            flags |= FLAG_OVERFLOWED;
        }
        out[5] = flags;
        out[8..16].copy_from_slice(&self.hwm_ticks.to_le_bytes());
        out[16..24].copy_from_slice(&self.hwm_depth.to_le_bytes());
        out[24..32].copy_from_slice(&self.persisted_at_nanos.to_le_bytes());
        out[32..40].copy_from_slice(&self.dir_tag.to_le_bytes());
        // O(1) EXEMPT: fixed UNAPPLIED_BUCKETS iterations, cold persist path
        for (i, (id, count)) in self.buckets.iter().enumerate() {
            let at = HEADER_LEN + i * 16;
            out[at..at + 8].copy_from_slice(&id.to_le_bytes());
            out[at + 8..at + 16].copy_from_slice(&count.to_le_bytes());
        }
        let crc = crc32_ieee(&out[..HEADER_LEN + BODY_LEN]);
        out[HEADER_LEN + BODY_LEN..].copy_from_slice(&crc.to_le_bytes());
        out
    }

    /// Parses the on-disk form. `None` on any shape or checksum failure —
    /// the caller treats that as "nothing is known" and replays in full.
    #[must_use]
    pub fn from_bytes(bytes: &[u8], now_nanos: u64) -> Option<Self> {
        if bytes.len() != APPLIED_WATERMARK_LEN
            || bytes[0..4] != APPLIED_MAGIC
            || bytes[4] != APPLIED_VERSION
        {
            return None;
        }
        let expected = u32::from_le_bytes(bytes[HEADER_LEN + BODY_LEN..].try_into().ok()?);
        if crc32_ieee(&bytes[..HEADER_LEN + BODY_LEN]) != expected {
            return None;
        }
        let flags = bytes[5];
        let hwm_ticks = u64::from_le_bytes(bytes[8..16].try_into().ok()?);
        let hwm_depth = u64::from_le_bytes(bytes[16..24].try_into().ok()?);
        let persisted_at_nanos = u64::from_le_bytes(bytes[24..32].try_into().ok()?);
        let dir_tag = u64::from_le_bytes(bytes[32..40].try_into().ok()?);
        let ceiling = now_nanos.saturating_add(APPLIED_MAX_FUTURE_NANOS);
        if hwm_ticks > ceiling || hwm_depth > ceiling {
            return None;
        }
        let mut buckets = [(0u64, 0u64); UNAPPLIED_BUCKETS];
        // O(1) EXEMPT: fixed UNAPPLIED_BUCKETS iterations, boot-time load
        for (i, slot) in buckets.iter_mut().enumerate() {
            let at = HEADER_LEN + i * 16;
            let id = u64::from_le_bytes(bytes[at..at + 8].try_into().ok()?);
            let count = u64::from_le_bytes(bytes[at + 8..at + 16].try_into().ok()?);
            *slot = (id, count);
        }
        Some(Self {
            hwm_ticks,
            hwm_depth,
            depth_untracked: flags & FLAG_DEPTH_UNTRACKED != 0,
            overflowed: flags & FLAG_OVERFLOWED != 0,
            persisted_at_nanos,
            dir_tag,
            buckets,
        })
    }

    /// Reads and validates `<wal_dir>/applied.tvaw`. Absent → `None`, silently
    /// (a first boot has no file). Present-but-rejected → `None` and the
    /// invalid counter, so a corrupt watermark is visible rather than a
    /// mystery full replay.
    #[must_use]
    pub fn load(wal_dir: &Path) -> Option<Self> {
        let path = wal_dir.join(APPLIED_WATERMARK_FILE);
        let bytes = std::fs::read(&path).ok()?; // APPROVED: boot-time load, cold path
        let expected_tag = dir_tag_of(wal_dir);
        let parsed = Self::from_bytes(&bytes, wall_nanos());
        let foreign = parsed
            .as_ref()
            .is_some_and(|snap| snap.dir_tag != expected_tag);
        if parsed.is_none() || foreign {
            metrics::counter!(APPLIED_INVALID_COUNTER).increment(1);
            warn!(
                path = %path.display(),
                len = bytes.len(),
                foreign,
                "WAL applied-watermark file rejected (short, wrong magic/version, bad crc, \
                 implausible values, or written beside a DIFFERENT directory) — ignored; \
                 this boot replays the WAL in full"
            );
            return None;
        }
        parsed
    }
}

/// The directory identity stored in the file: FNV-1a 64 over the canonical
/// path, or the path as given when it cannot be canonicalised (a directory
/// that does not exist yet has no file to load anyway). Never `0`, so an
/// unbound snapshot can never match a real directory by accident.
pub(crate) fn dir_tag_of(wal_dir: &Path) -> u64 {
    let canonical = std::fs::canonicalize(wal_dir).unwrap_or_else(|_| wal_dir.to_path_buf()); // APPROVED: boot-time bind, cold path
    let bytes = canonical.as_os_str().as_encoded_bytes();
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    // O(1) EXEMPT: one pass over a path string, boot-time bind
    for b in bytes {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    if hash == 0 { 1 } else { hash }
}

const fn slot_of(bucket_id: u64) -> usize {
    (bucket_id % UNAPPLIED_BUCKETS as u64) as usize
}

/// The process-wide watermark. One per process, like `WAL_FRAME_SEQ`.
pub struct AppliedWatermark {
    hwm_ticks: AtomicU64,
    hwm_depth: AtomicU64,
    depth_untracked: AtomicBool,
    overflowed: AtomicBool,
    /// Conservative upper bound on the sequences the bucket table FAILED to
    /// record. `overflowed` alone says "something is unprotected"; without a
    /// height there is no way to tell whether a later confirmed drain covered
    /// it, so `reset_unapplied_below` used to clear the flag on the strength
    /// of `kept == 0` — a count of the buckets it DID record, which is exactly
    /// the set that excludes these. See that function for the loss this
    /// prevents.
    overflow_high_seq: AtomicU64,
    bucket_ids: [AtomicU64; UNAPPLIED_BUCKETS],
    bucket_counts: [AtomicU64; UNAPPLIED_BUCKETS],
    ticks_handed_off: AtomicU64,
    ticks_completed: AtomicU64,
    depth_handed_off: AtomicU64,
    depth_completed: AtomicU64,
    last_persist_nanos: AtomicU64,
    paths: OnceLock<(PathBuf, PathBuf)>,
    /// Serialises persists AND owns the durability-lag ring (Z8b): the ring is
    /// only ever touched by the thread holding this lock.
    persist_lock: Mutex<DurabilityLag>,
    /// [`dir_tag_of`] the bound directory; `0` until [`Self::bind`].
    dir_tag: AtomicU64,
    /// `true` while the QuestDB WAL-suspension probe says a table is suspended,
    /// its apply lag is growing, or the probe itself cannot see. An ILP `2xx`
    /// from a suspended table is a LIE — the rows are acknowledged and never
    /// applied — and before this gate that lie advanced the watermark exactly
    /// as it advanced the old `confirm_replayed`. Worse than before: the old
    /// path re-replayed the whole backlog on the next restart, so a later
    /// `RESUME WAL` got a second chance at those rows; a skipped segment gets
    /// none. So while suspect, acks do NOT move the watermark (they park in
    /// `suspect_max_*`), and on recovery the parked range is marked unapplied.
    sink_suspect: AtomicBool,
    /// The watermarks as of the last CLEAN probe. When suspicion begins, the
    /// frames acked since then — the probe's detection window, one poll
    /// interval — are marked unapplied, because the table may have been
    /// lying for the whole of it.
    healthy_ticks: AtomicU64,
    healthy_depth: AtomicU64,
    /// Highest ack received while suspect, per sink.
    suspect_max_ticks: AtomicU64,
    suspect_max_depth: AtomicU64,
    /// Clean probes reported so far this process (audit PR41b). Monotone. The
    /// mid-session seal replay reads it to reopen its gate without live
    /// traffic and to archive a file only after a whole clean probe ran
    /// behind its last write.
    clean_probes: AtomicU64,
    /// Batches whose rows landed NOWHERE — the ILP write failed AND the
    /// rescue to the spill tier failed. Monotone. The replay confirm compares
    /// it before and after a batch: a batch that lost rows must never have its
    /// WAL segment archived, because the segment is now the ONLY copy.
    unlanded: AtomicU64,
    /// Lowest `capture_seq` of each rescue batch queued for (or being written
    /// by) a rescue thread, per sink; `0` is a free slot (Z7, 2026-10-02).
    ///
    /// A queued batch is neither acked nor marked unapplied, while the writer
    /// thread keeps acking LATER batches, so the high-water mark climbs over
    /// it. Before these floors, a crash (OOM, `panic=abort`, SIGKILL) with a
    /// batch still queued left a persisted watermark that vouched for rows in
    /// neither QuestDB nor a spill file: the next boot archived their segment
    /// unread and nothing counted the loss. The persisted value is now held
    /// below the oldest floor; the RAM value is unchanged.
    rescue_floors_ticks: [AtomicU64; RESCUE_FLOOR_SLOTS],
    rescue_floors_depth: [AtomicU64; RESCUE_FLOOR_SLOTS],
}

static APPLIED: AppliedWatermark = AppliedWatermark::new();

/// The one process-global watermark.
#[must_use]
pub fn applied_watermark() -> &'static AppliedWatermark {
    &APPLIED
}

impl AppliedWatermark {
    /// A fresh, unbound instance for tests that must not share the global.
    #[cfg(test)]
    pub(crate) const fn new_for_tests() -> Self {
        Self::new()
    }

    const fn new() -> Self {
        Self {
            hwm_ticks: AtomicU64::new(0),
            hwm_depth: AtomicU64::new(0),
            depth_untracked: AtomicBool::new(false),
            overflowed: AtomicBool::new(false),
            overflow_high_seq: AtomicU64::new(0),
            bucket_ids: [const { AtomicU64::new(0) }; UNAPPLIED_BUCKETS],
            bucket_counts: [const { AtomicU64::new(0) }; UNAPPLIED_BUCKETS],
            ticks_handed_off: AtomicU64::new(0),
            ticks_completed: AtomicU64::new(0),
            depth_handed_off: AtomicU64::new(0),
            depth_completed: AtomicU64::new(0),
            last_persist_nanos: AtomicU64::new(0),
            paths: OnceLock::new(),
            persist_lock: Mutex::new(DurabilityLag::new()),
            dir_tag: AtomicU64::new(0),
            sink_suspect: AtomicBool::new(false),
            healthy_ticks: AtomicU64::new(0),
            healthy_depth: AtomicU64::new(0),
            suspect_max_ticks: AtomicU64::new(0),
            suspect_max_depth: AtomicU64::new(0),
            clean_probes: AtomicU64::new(0),
            unlanded: AtomicU64::new(0),
            rescue_floors_ticks: [const { AtomicU64::new(0) }; RESCUE_FLOOR_SLOTS],
            rescue_floors_depth: [const { AtomicU64::new(0) }; RESCUE_FLOOR_SLOTS],
        }
    }

    /// Binds the watermark to its directory and seeds RAM from the file there,
    /// so the first in-session persist cannot overwrite a good snapshot with
    /// zeros. First caller wins; a second bind is ignored (one WAL dir per
    /// process, enforced by `lock_wal_dir`).
    pub fn bind(&self, wal_dir: &Path) {
        let set = self
            .paths
            .set((
                wal_dir.join(APPLIED_WATERMARK_FILE),
                wal_dir.join(APPLIED_WATERMARK_TMP),
            ))
            .is_ok();
        if set {
            self.dir_tag.store(dir_tag_of(wal_dir), Ordering::Release);
            if let Some(snap) = AppliedSnapshot::load(wal_dir) {
                self.seed(&snap);
            }
        }
    }

    /// Loads a snapshot into RAM (monotone on the watermarks, replace on the
    /// buckets).
    pub fn seed(&self, snap: &AppliedSnapshot) {
        self.hwm_ticks.fetch_max(snap.hwm_ticks, Ordering::AcqRel);
        self.hwm_depth.fetch_max(snap.hwm_depth, Ordering::AcqRel);
        // The file was written with the durability lag already applied, so
        // its values are the durable floor this process starts from (Z8b).
        self.lock_lag().seed(snap.hwm_ticks, snap.hwm_depth);
        // The seeded value is the last clean point this process knows of; the
        // first probe of the session moves it or opens suspicion from here.
        self.healthy_ticks
            .store(self.hwm_ticks.load(Ordering::Acquire), Ordering::Release);
        self.healthy_depth
            .store(self.hwm_depth.load(Ordering::Acquire), Ordering::Release);
        if snap.depth_untracked {
            self.depth_untracked.store(true, Ordering::Release);
        }
        if snap.overflowed {
            // A persisted flag carries no HEIGHT — the file format has one
            // bit, not a sequence — so this process cannot know how far the
            // previous one failed to record. Seed the height at the maximum
            // so `reset_unapplied_below` can never clear it here, and the
            // boot replays rather than skipping. Fails towards replay, which
            // costs time; the alternative costs data.
            self.overflow_high_seq.store(u64::MAX, Ordering::Release);
            self.overflowed.store(true, Ordering::Release);
        }
        // O(1) EXEMPT: fixed UNAPPLIED_BUCKETS iterations, boot-time seed
        for (i, (id, count)) in snap.buckets.iter().enumerate() {
            self.bucket_ids[i].store(*id, Ordering::Release);
            self.bucket_counts[i].store(*count, Ordering::Release);
        }
    }

    /// Rows up to `max_seq` landed in QuestDB or were durably rescued.
    pub fn note_ticks_acked(&self, max_seq: u64) {
        if self.sink_suspect.load(Ordering::Acquire) {
            self.suspect_max_ticks.fetch_max(max_seq, Ordering::AcqRel);
        } else {
            self.hwm_ticks.fetch_max(max_seq, Ordering::AcqRel);
        }
    }

    /// Depth rows up to `max_seq` landed in QuestDB or were durably rescued.
    pub fn note_depth_acked(&self, max_seq: u64) {
        if self.sink_suspect.load(Ordering::Acquire) {
            self.suspect_max_depth.fetch_max(max_seq, Ordering::AcqRel);
        } else {
            self.hwm_depth.fetch_max(max_seq, Ordering::AcqRel);
        }
    }

    /// The QuestDB WAL-suspension probe reported. `clean` = every table
    /// visible, none suspended, no growing apply lag. Called once per poll
    /// (60 s) from the watcher task — cold path.
    ///
    /// Entering suspicion marks `[last clean watermark, current + slack]`
    /// unapplied: the detection window, plus one second of slack for an ack
    /// that raced the flag. Leaving it marks `[watermark, highest parked ack]`
    /// unapplied and re-opens the watermark from where it stopped. Every
    /// direction is "replay more"; a suspension longer than the bucket table
    /// covers (~9.8 h) overflows it, which is the old full replay.
    pub fn note_questdb_probe(&self, clean: bool) {
        if clean {
            self.clean_probes.fetch_add(1, Ordering::AcqRel);
            if self.sink_suspect.swap(false, Ordering::AcqRel) {
                let parked_ticks = self.suspect_max_ticks.swap(0, Ordering::AcqRel);
                let parked_depth = self.suspect_max_depth.swap(0, Ordering::AcqRel);
                let hwm_ticks = self.hwm_ticks.load(Ordering::Acquire);
                let hwm_depth = self.hwm_depth.load(Ordering::Acquire);
                if parked_ticks > hwm_ticks {
                    self.note_unapplied_range(hwm_ticks.max(1), parked_ticks);
                }
                if parked_depth > hwm_depth {
                    self.note_unapplied_range(hwm_depth.max(1), parked_depth);
                }
            }
            self.healthy_ticks
                .store(self.hwm_ticks.load(Ordering::Acquire), Ordering::Release);
            self.healthy_depth
                .store(self.hwm_depth.load(Ordering::Acquire), Ordering::Release);
        } else if !self.sink_suspect.swap(true, Ordering::AcqRel) {
            let hwm_ticks = self.hwm_ticks.load(Ordering::Acquire);
            let hwm_depth = self.hwm_depth.load(Ordering::Acquire);
            self.note_unapplied_range(
                self.healthy_ticks.load(Ordering::Acquire).max(1),
                hwm_ticks.saturating_add(REPLAY_REORDER_SLACK_SEQ),
            );
            self.note_unapplied_range(
                self.healthy_depth.load(Ordering::Acquire).max(1),
                hwm_depth.saturating_add(REPLAY_REORDER_SLACK_SEQ),
            );
        }
    }

    /// How many clean probes have reported this process (audit PR41b). `0`
    /// until the WAL-suspension watcher first sees every table healthy, so a
    /// reader can tell "no prober running" from "prober running".
    #[must_use]
    pub fn clean_probe_count(&self) -> u64 {
        self.clean_probes.load(Ordering::Acquire)
    }

    /// Whether acks are currently parked rather than applied.
    #[must_use]
    pub fn is_sink_suspect(&self) -> bool {
        self.sink_suspect.load(Ordering::Acquire)
    }

    /// The lane runs without a depth sink: inline depth is discarded by design,
    /// so the tick watermark alone decides whether a main-feed frame is applied.
    pub fn mark_depth_untracked(&self) {
        self.depth_untracked.store(true, Ordering::Release);
    }

    /// The lane HAS a depth sink: a main-feed frame is applied only when both
    /// sinks acked it. Clears a flag persisted by an earlier session that ran
    /// without one, so enabling depth persistence never inherits the weaker
    /// rule from the file.
    pub fn mark_depth_tracked(&self) {
        self.depth_untracked.store(false, Ordering::Release);
    }

    /// A captured frame at `seq` will NOT be applied by this session (shed at
    /// the ring, append failed, rescue failed). Its bucket is marked so replay
    /// never skips a segment overlapping it. Three atomics, cold arm only.
    pub fn note_unapplied(&self, seq: u64) {
        let id = seq >> UNAPPLIED_BUCKET_SHIFT;
        if id == 0 {
            // A v1-era or zero sequence has no bucket; the replay never skips
            // a zero-sequence record anyway.
            return;
        }
        let slot = slot_of(id);
        let current = self.bucket_ids[slot].load(Ordering::Acquire);
        if current == id {
            self.mark_slot(slot);
            return;
        }
        if current == 0
            && self.bucket_ids[slot]
                .compare_exchange(0, id, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
        {
            self.mark_slot(slot);
            return;
        }
        // Someone else claimed the slot first: either the same id (mark it)
        // or a different one (the table has wrapped — fail towards replay).
        if self.bucket_ids[slot].load(Ordering::Acquire) == id {
            self.mark_slot(slot);
        } else {
            // The table could not record this frame. Remember how HIGH the
            // loss reached, conservatively as the TOP of the bucket, so a
            // later confirmed drain can only clear the flag once it has
            // provably passed it. Storing the bucket top rather than the
            // seq keeps this an upper bound on every frame the bucket
            // would have held.
            let bucket_top = ((id + 1) << UNAPPLIED_BUCKET_SHIFT).saturating_sub(1);
            self.overflow_high_seq
                .fetch_max(bucket_top, Ordering::AcqRel);
            self.overflowed.store(true, Ordering::Release);
        }
    }

    /// Idempotent per bucket: the first shed frame in a bucket writes the
    /// mark, every later one in the same bucket is a single relaxed load.
    /// A RingFull storm sheds thousands of frames per second on the WS read
    /// task and all sixteen sockets land in the SAME bucket for ~4.6 minutes,
    /// so a contended read-modify-write per frame was the wrong cost for a
    /// value whose only reader asks "is it nonzero". The `Release` publish
    /// that readers depend on is the id store above, not this one.
    #[inline]
    fn mark_slot(&self, slot: usize) {
        if self.bucket_counts[slot].load(Ordering::Relaxed) == 0 {
            self.bucket_counts[slot].store(1, Ordering::Relaxed);
        }
    }

    /// Every bucket touched by `[lo, hi]` is marked unapplied — a whole batch
    /// whose rescue failed. Bounded by the table size; a range wider than the
    /// table overflows it, which fails towards replaying everything.
    pub fn note_unapplied_range(&self, lo: u64, hi: u64) {
        if lo == 0 || lo > hi {
            return;
        }
        let first = lo >> UNAPPLIED_BUCKET_SHIFT;
        let last = hi >> UNAPPLIED_BUCKET_SHIFT;
        if last.saturating_sub(first) >= UNAPPLIED_BUCKETS as u64 {
            // Same reasoning as the collision arm in `note_unapplied`: this whole
            // range goes unrecorded, so remember its TOP before failing towards
            // replay. Without it the flag below is the only trace, and a later
            // confirmed drain would clear that on the strength of a bucket count
            // that never included these frames.
            self.overflow_high_seq.fetch_max(hi, Ordering::AcqRel);
            self.overflowed.store(true, Ordering::Release);
            return;
        }
        // O(1) EXEMPT: bounded by UNAPPLIED_BUCKETS, cold failure arm
        let mut id = first;
        loop {
            self.note_unapplied(id << UNAPPLIED_BUCKET_SHIFT);
            if id == last {
                return;
            }
            id += 1;
        }
    }

    /// [`Self::persist_if_due`] against the wall clock.
    pub fn persist_if_due_now(&self) -> bool {
        self.persist_if_due(wall_nanos())
    }

    /// The WAL backlog is fully drained and confirmed: nothing unapplied can
    /// remain on disk, so the bucket table starts clean. Called by the lane
    /// ONLY when the catch-up ended with zero deferred segments.
    pub fn reset_unapplied(&self) {
        self.reset_unapplied_below(u64::MAX);
    }

    /// [`Self::reset_unapplied`] for the buckets BELOW `ceiling_seq` only.
    /// The catch-up drain snapshots the frame sequence when it starts; every
    /// segment it can have confirmed holds frames below that point, so those
    /// buckets have nothing left to guard — but a frame shed from a socket
    /// dialed DURING the drain sits in the writer's open segment, above the
    /// ceiling, and its bucket must survive. `overflowed` is cleared only
    /// when nothing above the ceiling remains: a wrapped table cannot say
    /// which ids it lost.
    pub fn reset_unapplied_below(&self, ceiling_seq: u64) {
        let ceiling_id = ceiling_seq >> UNAPPLIED_BUCKET_SHIFT;
        let mut kept = 0usize;
        // O(1) EXEMPT: fixed UNAPPLIED_BUCKETS iterations, boot-time reset
        for i in 0..UNAPPLIED_BUCKETS {
            let id = self.bucket_ids[i].load(Ordering::Acquire);
            if id == 0 {
                continue;
            }
            if id < ceiling_id {
                self.bucket_ids[i].store(0, Ordering::Release);
                self.bucket_counts[i].store(0, Ordering::Release);
            } else {
                kept += 1;
            }
        }
        // `kept` counts the buckets this table DID record at or above the
        // ceiling. It is silent about the ones it could NOT record, and
        // those are precisely what `overflowed` exists to flag — so
        // clearing on `kept == 0` alone erased the only protection the
        // unrecorded frames had. A later ack then lifted the high-water
        // mark past them, `range_has_unapplied` answered false, and the
        // replay renamed their whole segment to archive/ UNREAD: silent,
        // permanent loss of every tick and depth row in it, with no
        // counter anywhere. Found by an adversarial sweep on 2026-09-05,
        // the same day this module landed, and reproduced before fixing.
        //
        // The flag may only be cleared once the confirmed ceiling has
        // provably passed everything the table failed to record.
        if kept == 0 && self.overflow_high_seq.load(Ordering::Acquire) < ceiling_seq {
            self.overflow_high_seq.store(0, Ordering::Release);
            self.overflowed.store(false, Ordering::Release);
        }
    }

    /// A batch's rows landed NOWHERE: the ILP write failed and the rescue to
    /// the spill tier failed. The bucket mark (made by the caller through
    /// [`Self::note_unapplied_range`]) keeps the next boot from skipping it;
    /// this counter keeps THIS boot from archiving it.
    pub fn note_unlanded(&self) {
        self.unlanded.fetch_add(1, Ordering::AcqRel);
    }

    /// Monotone count of batches that landed nowhere. Snapshot it before a
    /// replay batch is folded; hand the snapshot to
    /// [`Self::wait_for_offload_drained`].
    #[must_use]
    pub fn unlanded_total(&self) -> u64 {
        self.unlanded.load(Ordering::Acquire)
    }

    /// A batch left the producer for the tick writer thread.
    pub fn note_ticks_handed_off(&self) {
        self.ticks_handed_off.fetch_add(1, Ordering::AcqRel);
    }
    /// The tick writer thread finished with one batch (landed OR rescued).
    pub fn note_ticks_completed(&self) {
        self.ticks_completed.fetch_add(1, Ordering::AcqRel);
    }
    /// A batch left the producer for the depth writer thread.
    pub fn note_depth_handed_off(&self) {
        self.depth_handed_off.fetch_add(1, Ordering::AcqRel);
    }
    /// The depth writer thread finished with one batch (landed OR rescued).
    pub fn note_depth_completed(&self) {
        self.depth_completed.fetch_add(1, Ordering::AcqRel);
    }

    /// Blocks (polling, bounded by `timeout`) until every batch handed off so
    /// far has been processed by its writer thread. `true` when reached.
    ///
    /// This is what makes "durably re-captured" TRUE before a replay confirm:
    /// `flush()` on the offloaded path returns rows HANDED OFF, not rows
    /// landed, and `confirm_replayed` archived on the strength of it.
    #[must_use]
    pub fn wait_for_offload_drained(&self, timeout: Duration, unlanded_before: u64) -> bool {
        let target_ticks = self.ticks_handed_off.load(Ordering::Acquire);
        let target_depth = self.depth_handed_off.load(Ordering::Acquire);
        let deadline = std::time::Instant::now() + timeout;
        loop {
            // A completion is not a landing. A batch whose ILP write failed
            // AND whose rescue failed still completes — and its rows exist
            // only in the WAL segment the caller is about to archive. So the
            // wait is also "nothing was lost since the caller's snapshot".
            if self.unlanded.load(Ordering::Acquire) != unlanded_before {
                return false;
            }
            if self.ticks_completed.load(Ordering::Acquire) >= target_ticks
                && self.depth_completed.load(Ordering::Acquire) >= target_depth
            {
                return true;
            }
            if std::time::Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(OFFLOAD_DRAIN_POLL_MILLIS));
        }
    }

    /// Current RAM state.
    #[must_use]
    pub fn snapshot(&self) -> AppliedSnapshot {
        let mut buckets = [(0u64, 0u64); UNAPPLIED_BUCKETS];
        // O(1) EXEMPT: fixed UNAPPLIED_BUCKETS iterations, cold path
        for (i, slot) in buckets.iter_mut().enumerate() {
            *slot = (
                self.bucket_ids[i].load(Ordering::Acquire),
                self.bucket_counts[i].load(Ordering::Acquire),
            );
        }
        AppliedSnapshot {
            hwm_ticks: self.hwm_ticks.load(Ordering::Acquire),
            hwm_depth: self.hwm_depth.load(Ordering::Acquire),
            depth_untracked: self.depth_untracked.load(Ordering::Acquire),
            overflowed: self.overflowed.load(Ordering::Acquire),
            persisted_at_nanos: 0,
            dir_tag: self.dir_tag.load(Ordering::Acquire),
            buckets,
        }
    }

    /// The snapshot as it is written to disk: the live state with both
    /// high-water marks held back by the durability lag (Z8b) and below every
    /// queued rescue floor (Z7). Cold: once per persist, under the lock.
    fn durable_view(&self, lag: &mut DurabilityLag, mono_now: u64) -> AppliedSnapshot {
        // The watermarks are read BEFORE the floors. A floor is pushed before
        // its batch is handed off, and every ack that can lift the watermark
        // past that batch happens after; so a watermark value read here that
        // has passed the batch guarantees the floor is visible to the loads
        // below (Acquire on the watermark, Release on the floor store).
        let mut snap = self.snapshot();
        let floor_ticks = self.rescue_floor_min(AppliedSink::Ticks);
        let floor_depth = self.rescue_floor_min(AppliedSink::Depth);
        lag.record(mono_now, snap.hwm_ticks, snap.hwm_depth);
        let (durable_ticks, durable_depth) =
            lag.durable_at(mono_now, WATERMARK_DURABILITY_LAG_NANOS);
        snap.hwm_ticks = cap_below_floor(durable_ticks.min(snap.hwm_ticks), floor_ticks);
        snap.hwm_depth = cap_below_floor(durable_depth.min(snap.hwm_depth), floor_depth);
        snap
    }

    /// [`Self::durable_view`] taken under the lock, for tests that check the
    /// persisted value without a file.
    #[cfg(test)]
    fn durable_view_at(&self, mono_now: u64) -> AppliedSnapshot {
        let mut lag = self.lock_lag();
        self.durable_view(&mut lag, mono_now)
    }

    /// The persist lock, recovered from poison (the guarded data is a ring of
    /// plain integers; a panic mid-update can only leave it conservative).
    fn lock_lag(&self) -> std::sync::MutexGuard<'_, DurabilityLag> {
        self.persist_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn rescue_floors(&self, sink: AppliedSink) -> &[AtomicU64; RESCUE_FLOOR_SLOTS] {
        match sink {
            AppliedSink::Ticks => &self.rescue_floors_ticks,
            AppliedSink::Depth => &self.rescue_floors_depth,
        }
    }

    /// A rescue batch covering `[min_seq, max_seq]` is about to be handed to a
    /// rescue thread (Z7). Until [`Self::release_rescue_floor`] is called, the
    /// PERSISTED watermark for `sink` stays below `min_seq`, so a crash while
    /// the batch is queued replays it rather than archiving its segment.
    ///
    /// Called on the frame drain, on the rescue arm only (never per tick): at
    /// most [`RESCUE_FLOOR_SLOTS`] compare-and-swaps, no allocation, no lock —
    /// O(RESCUE_FLOOR_SLOTS), a constant 8, not O(1). `None` when the batch
    /// has no sequence (nothing in the WAL to protect) or when every slot is
    /// taken; a full table marks the range unapplied instead and counts it,
    /// which fails toward replay.
    #[must_use]
    pub fn hold_rescue_floor(
        &self,
        sink: AppliedSink,
        min_seq: u64,
        max_seq: u64,
    ) -> Option<RescueFloor> {
        if min_seq == 0 {
            return None;
        }
        let floors = self.rescue_floors(sink);
        // O(1) EXEMPT: bounded by RESCUE_FLOOR_SLOTS (8), rescue arm only
        for (slot, cell) in floors.iter().enumerate() {
            if cell
                .compare_exchange(0, min_seq, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return Some(RescueFloor {
                    sink,
                    slot: u8::try_from(slot).unwrap_or(u8::MAX),
                });
            }
        }
        metrics::counter!(RESCUE_FLOOR_FULL_COUNTER).increment(1);
        self.note_unapplied_range(min_seq, max_seq.max(min_seq));
        None
    }

    /// The rescue batch holding `floor` is settled: its spill write is on disk
    /// (synced), or it failed and its range was marked unapplied, or the
    /// hand-off was refused and the producer took the batch back. O(1).
    pub fn release_rescue_floor(&self, floor: RescueFloor) {
        if let Some(cell) = self.rescue_floors(floor.sink).get(usize::from(floor.slot)) {
            cell.store(0, Ordering::Release);
        }
    }

    /// Whether some slot of `sink` holds exactly `min_seq` (tests in the sink
    /// modules check their batch's floor on the shared global).
    #[cfg(test)]
    pub(crate) fn rescue_floor_is_held(&self, sink: AppliedSink, min_seq: u64) -> bool {
        self.rescue_floors(sink)
            .iter()
            .any(|c| c.load(Ordering::Acquire) == min_seq)
    }

    /// The lowest held floor for `sink`, or `u64::MAX` when none is held.
    fn rescue_floor_min(&self, sink: AppliedSink) -> u64 {
        let mut lowest = u64::MAX;
        // O(1) EXEMPT: bounded by RESCUE_FLOOR_SLOTS (8), once-a-second persist
        for cell in self.rescue_floors(sink) {
            let held = cell.load(Ordering::Acquire);
            if held != 0 {
                lowest = lowest.min(held);
            }
        }
        lowest
    }

    /// Persists if at least [`APPLIED_PERSIST_INTERVAL_NANOS`] elapsed since
    /// the last persist. Returns whether a write was attempted. Cheap when not
    /// due: one load and one compare.
    pub fn persist_if_due(&self, now_nanos: u64) -> bool {
        let last = self.last_persist_nanos.load(Ordering::Acquire);
        if now_nanos < last.saturating_add(APPLIED_PERSIST_INTERVAL_NANOS) {
            return false;
        }
        if self
            .last_persist_nanos
            .compare_exchange(last, now_nanos, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return false; // another thread took this interval
        }
        self.persist_now();
        true
    }

    /// Writes the current state to the bound file (tmp + rename). Unbound →
    /// nothing to do. A failure is counted and warned, never propagated: this
    /// runs on a writer thread whose job is the network, and the fail-safe
    /// direction of a missing persist is "replay more".
    ///
    /// What is written is [`Self::snapshot`] with both high-water marks held
    /// back twice: to the value acknowledged at least
    /// [`WATERMARK_DURABILITY_LAG_SECS`] ago (Z8b), and strictly below the
    /// lowest sequence of any rescue batch still queued (Z7). After a quiet
    /// period of at least the lag, the next persist writes the live value, so
    /// a stop after the close needs no special case; a stop inside a busy
    /// minute leaves the next boot replaying that minute, which DEDUP absorbs.
    pub fn persist_now(&self) {
        self.persist_at(mono_nanos());
    }

    /// [`Self::persist_now`] at an explicit monotonic instant (tests drive the
    /// durability lag through this).
    pub(crate) fn persist_at(&self, mono_now: u64) {
        let Some((path, tmp)) = self.paths.get() else {
            return;
        };
        // Serialise concurrent persists from the two sink threads. A held lock
        // means a snapshot at most one interval old is being written; skip.
        let mut lag = match self.persist_lock.try_lock() {
            Ok(guard) => guard,
            Err(std::sync::TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => return,
        };
        // The lock stays held through the write: two persists racing on the
        // same tmp path would refuse each other at `create_new`.
        let mut snap = self.durable_view(&mut lag, mono_now);
        snap.persisted_at_nanos = wall_nanos();
        let bytes = snap.to_bytes();
        let written = write_fresh(tmp, &bytes).and_then(|()| std::fs::rename(tmp, path));
        if let Err(err) = written {
            metrics::counter!(APPLIED_PERSIST_FAILED_COUNTER).increment(1);
            warn!(
                path = %path.display(),
                error = %err,
                "WAL applied-watermark could not be persisted — the RAM value is kept and \
                 the next boot replays MORE than it needs to, never less"
            );
        }
    }
}

/// Creates `tmp` fresh (`O_CREAT|O_EXCL`) so a pre-planted symlink at that
/// path is never followed — `O_EXCL` fails on an existing symlink rather than
/// writing through it. A stale tmp from a crashed persist is removed first;
/// it holds nothing the live file does not.
pub(crate) fn write_fresh(tmp: &Path, bytes: &[u8]) -> std::io::Result<()> {
    drop(std::fs::remove_file(tmp)); // APPROVED: once-a-second persist, sink thread, cold path
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(tmp)?; // APPROVED: once-a-second persist, sink thread, cold path
    file.write_all(bytes)?;
    // Durable BEFORE the caller renames it over the live file: without this a
    // host crash can keep the rename and lose the data, leaving a file that
    // loads as rejected (2026-09-29, item 45a security review).
    file.sync_all()
}

pub(crate) fn wall_nanos() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

const CRC32_TABLE: [u32; 256] = {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut j = 0;
        while j < 8 {
            c = if c & 1 != 0 {
                0xEDB8_8320 ^ (c >> 1)
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

pub(crate) fn crc32_ieee(bytes: &[u8]) -> u32 {
    let mut c: u32 = 0xFFFF_FFFF;
    for &b in bytes {
        c = CRC32_TABLE[((c ^ u32::from(b)) & 0xFF) as usize] ^ (c >> 8);
    }
    c ^ 0xFFFF_FFFF
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seq(secs: u64) -> u64 {
        // A 2026-era sequence: nanos with the 17 reserved low bits zeroed.
        ((1_780_000_000 + secs) * 1_000_000_000) >> 17 << 17
    }

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "tv-applied-{tag}-{}-{}",
            std::process::id(),
            wall_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    #[test]
    fn applied_watermark_roundtrip_is_crc_checked() {
        let mut snap = AppliedSnapshot {
            hwm_ticks: seq(100),
            hwm_depth: seq(90),
            depth_untracked: false,
            overflowed: false,
            persisted_at_nanos: 7,
            ..AppliedSnapshot::default()
        };
        snap.buckets[3] = (seq(50) >> UNAPPLIED_BUCKET_SHIFT, 2);
        let bytes = snap.to_bytes();
        assert_eq!(bytes.len(), APPLIED_WATERMARK_LEN);
        let now = seq(200);
        assert_eq!(AppliedSnapshot::from_bytes(&bytes, now), Some(snap.clone()));

        let mut flipped = bytes;
        flipped[20] ^= 0x01;
        assert_eq!(
            AppliedSnapshot::from_bytes(&flipped, now),
            None,
            "crc must catch a bit flip"
        );
        assert_eq!(
            AppliedSnapshot::from_bytes(&bytes[..10], now),
            None,
            "short file"
        );
        let mut bad_magic = bytes;
        bad_magic[0] = b'X';
        assert_eq!(
            AppliedSnapshot::from_bytes(&bad_magic, now),
            None,
            "wrong magic"
        );
        // A watermark a week in the future is a stepped clock, not knowledge.
        assert_eq!(
            AppliedSnapshot::from_bytes(&bytes, seq(0) - 7 * 86_400 * 1_000_000_000),
            None
        );
    }

    #[test]
    fn applied_watermark_load_returns_none_for_an_absent_file() {
        let dir = scratch("absent");
        assert!(AppliedSnapshot::load(&dir).is_none());
    }

    #[test]
    fn applied_watermark_persist_and_load_roundtrip_on_disk() {
        let dir = scratch("persist");
        let wm = AppliedWatermark::new();
        wm.bind(&dir);
        wm.note_ticks_acked(seq(30));
        wm.note_depth_acked(seq(25));
        wm.note_unapplied(seq(10));
        // The acks are read at t0 and may reach the file only once they have
        // stood for the durability lag (Z8b).
        wm.persist_at(1);
        wm.persist_at(1 + WATERMARK_DURABILITY_LAG_NANOS);
        assert!(dir.join(APPLIED_WATERMARK_FILE).exists());
        assert!(
            !dir.join(APPLIED_WATERMARK_TMP).exists(),
            "tmp is renamed away"
        );
        let loaded = AppliedSnapshot::load(&dir).expect("valid file");
        assert_eq!(loaded.hwm_ticks, seq(30));
        assert_eq!(loaded.hwm_depth, seq(25));
        assert!(loaded.range_has_unapplied(seq(10), seq(10)));
        // Buckets are ~4.6 minutes wide: the same bucket ten seconds later
        // reads unapplied, a bucket half an hour later does not.
        assert!(loaded.range_has_unapplied(seq(20), seq(24)));
        assert!(!loaded.range_has_unapplied(seq(2_000), seq(2_004)));

        // A fresh process binding the same dir starts from the file, not zero.
        let again = AppliedWatermark::new();
        again.bind(&dir);
        assert_eq!(again.snapshot().hwm_ticks, seq(30));
    }

    #[test]
    fn skip_below_takes_the_lower_watermark_unless_depth_is_untracked() {
        let mut snap = AppliedSnapshot {
            hwm_ticks: seq(100),
            hwm_depth: seq(40),
            ..AppliedSnapshot::default()
        };
        assert_eq!(
            snap.skip_below(AppliedSink::Ticks),
            seq(40) - REPLAY_REORDER_SLACK_SEQ
        );
        assert_eq!(
            snap.skip_below(AppliedSink::Depth),
            seq(40) - REPLAY_REORDER_SLACK_SEQ
        );
        snap.depth_untracked = true;
        assert_eq!(
            snap.skip_below(AppliedSink::Ticks),
            seq(100) - REPLAY_REORDER_SLACK_SEQ
        );
        let none = AppliedSnapshot::default();
        assert_eq!(none.skip_below(AppliedSink::Ticks), 0);
    }

    #[test]
    fn range_is_applied_never_skips_zero_seq_or_the_slack_window() {
        let snap = AppliedSnapshot {
            hwm_ticks: seq(100),
            hwm_depth: seq(100),
            ..AppliedSnapshot::default()
        };
        assert!(snap.range_is_applied(AppliedSink::Ticks, seq(1), seq(50)));
        assert!(
            !snap.range_is_applied(AppliedSink::Ticks, 0, seq(50)),
            "v1 record"
        );
        assert!(
            !snap.frame_is_applied(AppliedSink::Ticks, seq(100)),
            "inside the slack"
        );
        assert!(snap.frame_is_applied(AppliedSink::Ticks, seq(98)));
        assert!(
            !snap.frame_is_applied(AppliedSink::Ticks, seq(101)),
            "above the watermark"
        );
        assert!(!AppliedSnapshot::default().frame_is_applied(AppliedSink::Ticks, seq(1)));
    }

    #[test]
    fn an_unapplied_bucket_blocks_the_skip_and_only_that_bucket() {
        let wm = AppliedWatermark::new();
        wm.note_ticks_acked(seq(3600));
        wm.note_depth_acked(seq(3600));
        wm.note_unapplied(seq(1000));
        let snap = wm.snapshot();
        assert!(snap.range_has_unapplied(seq(999), seq(1001)));
        assert!(!snap.range_is_applied(AppliedSink::Ticks, seq(900), seq(1100)));
        // 4.6-minute buckets: 20 minutes away is a different bucket.
        assert!(snap.range_is_applied(AppliedSink::Ticks, seq(2000), seq(2100)));
        assert!(snap.range_is_applied(AppliedSink::Ticks, seq(1), seq(500)));
    }

    #[test]
    fn a_wrapped_bucket_table_fails_towards_replaying_everything() {
        let wm = AppliedWatermark::new();
        wm.note_ticks_acked(seq(999_999));
        wm.note_depth_acked(seq(999_999));
        wm.note_unapplied(seq(0));
        // Exactly UNAPPLIED_BUCKETS buckets later lands on the same slot with a
        // different id: the table cannot hold both.
        let wrapped = seq(0) + ((UNAPPLIED_BUCKETS as u64) << UNAPPLIED_BUCKET_SHIFT);
        wm.note_unapplied(wrapped);
        let snap = wm.snapshot();
        assert!(snap.overflowed);
        assert!(!snap.range_is_applied(AppliedSink::Ticks, seq(1), seq(2)));
        wm.reset_unapplied();
        let clean = wm.snapshot();
        assert!(!clean.overflowed);
        assert!(clean.range_is_applied(AppliedSink::Ticks, seq(1), seq(2)));
    }

    #[test]
    fn note_unapplied_range_marks_every_bucket_it_spans() {
        let wm = AppliedWatermark::new();
        wm.note_ticks_acked(seq(9_000));
        wm.note_depth_acked(seq(9_000));
        // 4.6-minute buckets: a 12-minute range touches three or four of them.
        wm.note_unapplied_range(seq(1_000), seq(1_720));
        let snap = wm.snapshot();
        assert!(snap.range_has_unapplied(seq(1_100), seq(1_100)));
        assert!(snap.range_has_unapplied(seq(1_700), seq(1_700)));
        assert!(!snap.range_has_unapplied(seq(5_000), seq(5_100)));
        assert!(!snap.overflowed);
        wm.note_unapplied_range(0, seq(5));
        assert!(
            !wm.snapshot().overflowed,
            "a zero lower bound is ignored, not an overflow"
        );
    }

    #[test]
    fn note_unapplied_ignores_a_zero_sequence() {
        let wm = AppliedWatermark::new();
        wm.note_unapplied(0);
        let snap = wm.snapshot();
        assert!(snap.buckets.iter().all(|(id, c)| *id == 0 && *c == 0));
        assert!(!snap.overflowed);
    }

    #[test]
    fn persist_if_due_writes_at_most_once_per_interval() {
        let dir = scratch("cadence");
        let wm = AppliedWatermark::new();
        wm.bind(&dir);
        let t0 = seq(10);
        assert!(wm.persist_if_due(t0));
        assert!(!wm.persist_if_due(t0 + APPLIED_PERSIST_INTERVAL_NANOS / 2));
        assert!(wm.persist_if_due(t0 + APPLIED_PERSIST_INTERVAL_NANOS));
    }

    #[test]
    fn persist_now_without_a_bound_directory_is_a_no_op() {
        let wm = AppliedWatermark::new();
        wm.note_ticks_acked(seq(1));
        wm.persist_now(); // must not panic, must not write anywhere
    }

    #[test]
    fn wait_for_offload_drained_tracks_handed_off_against_completed() {
        let wm = AppliedWatermark::new();
        assert!(
            wm.wait_for_offload_drained(Duration::from_millis(1), 0),
            "nothing pending"
        );
        wm.note_ticks_handed_off();
        wm.note_depth_handed_off();
        assert!(
            !wm.wait_for_offload_drained(Duration::from_millis(60), 0),
            "two batches in flight"
        );
        wm.note_ticks_completed();
        assert!(
            !wm.wait_for_offload_drained(Duration::from_millis(60), 0),
            "depth still in flight"
        );
        wm.note_depth_completed();
        assert!(wm.wait_for_offload_drained(Duration::from_millis(1), 0));
    }

    #[test]
    fn mark_depth_untracked_and_seed_are_monotone() {
        let wm = AppliedWatermark::new();
        wm.note_ticks_acked(seq(50));
        wm.seed(&AppliedSnapshot {
            hwm_ticks: seq(20),
            hwm_depth: seq(20),
            ..AppliedSnapshot::default()
        });
        assert_eq!(
            wm.snapshot().hwm_ticks,
            seq(50),
            "seed never lowers a watermark"
        );
        assert_eq!(wm.snapshot().hwm_depth, seq(20));
        wm.mark_depth_untracked();
        assert!(wm.snapshot().depth_untracked);
    }

    #[test]
    fn the_global_watermark_is_one_static() {
        assert!(std::ptr::eq(applied_watermark(), applied_watermark()));
    }

    #[test]
    fn a_watermark_file_from_another_directory_is_rejected() {
        let a = scratch("dir_tag_a");
        let b = scratch("dir_tag_b");
        let wm = AppliedWatermark::new_for_tests();
        wm.bind(&a);
        wm.note_ticks_acked(seq(500));
        wm.persist_now();
        let file = a.join(APPLIED_WATERMARK_FILE);
        assert!(
            AppliedSnapshot::load(&a).is_some(),
            "own directory accepts its file"
        );
        std::fs::copy(&file, b.join(APPLIED_WATERMARK_FILE)).expect("copy");
        assert!(
            AppliedSnapshot::load(&b).is_none(),
            "a file written beside directory A must not vouch for directory B's segments"
        );
        // And an unbound snapshot (tag 0) never matches a real directory.
        let bytes = AppliedSnapshot {
            hwm_ticks: seq(500),
            ..AppliedSnapshot::default()
        }
        .to_bytes();
        std::fs::write(a.join(APPLIED_WATERMARK_FILE), bytes).expect("write");
        assert!(AppliedSnapshot::load(&a).is_none());
    }

    #[test]
    fn a_suspended_questdb_parks_acks_and_marks_the_window_unapplied() {
        let wm = AppliedWatermark::new_for_tests();
        wm.note_ticks_acked(seq(100));
        wm.note_depth_acked(seq(100));
        wm.note_questdb_probe(true); // clean: healthy = 100
        wm.note_ticks_acked(seq(400));
        wm.note_depth_acked(seq(400));
        // The probe now reports a suspended table: everything acked since the
        // last clean probe (100..400) may be a lie.
        wm.note_questdb_probe(false);
        assert!(wm.is_sink_suspect());
        let snap = wm.snapshot();
        assert!(
            snap.range_has_unapplied(seq(150), seq(150)),
            "the detection window is unapplied"
        );
        assert!(
            !snap.frame_is_applied(AppliedSink::Ticks, seq(300)),
            "a frame inside the window is never skipped"
        );
        // Acks while suspect do NOT advance the watermark.
        wm.note_ticks_acked(seq(900));
        wm.note_depth_acked(seq(900));
        let snap = wm.snapshot();
        assert_eq!(snap.hwm_ticks, seq(400));
        assert_eq!(snap.hwm_depth, seq(400));
        assert!(!snap.frame_is_applied(AppliedSink::Ticks, seq(800)));
        // Recovery: the parked range 400..900 is unapplied, and NEW acks
        // advance again from there.
        wm.note_questdb_probe(true);
        assert!(!wm.is_sink_suspect());
        let snap = wm.snapshot();
        assert!(snap.range_has_unapplied(seq(700), seq(700)));
        assert!(!snap.frame_is_applied(AppliedSink::Ticks, seq(800)));
        wm.note_ticks_acked(seq(2_000_000));
        assert_eq!(wm.snapshot().hwm_ticks, seq(2_000_000));
        // A second not-clean probe while already suspect is idempotent.
        wm.note_questdb_probe(false);
        wm.note_questdb_probe(false);
        assert!(wm.is_sink_suspect());
    }

    #[test]
    fn a_planted_symlink_at_the_tmp_path_is_never_written_through() {
        let dir = scratch("symlink_tmp");
        let victim = dir.join("victim");
        std::fs::write(&victim, b"untouched").expect("victim");
        let tmp = dir.join(APPLIED_WATERMARK_TMP);
        std::os::unix::fs::symlink(&victim, &tmp).expect("symlink");
        let wm = AppliedWatermark::new_for_tests();
        wm.bind(&dir);
        wm.note_ticks_acked(seq(10));
        wm.persist_now();
        assert_eq!(
            std::fs::read(&victim).expect("victim readable"),
            b"untouched",
            "the persist must never write through a pre-planted symlink"
        );
        assert!(
            AppliedSnapshot::load(&dir).is_some(),
            "the live file is written fresh once the symlink is out of the way"
        );
    }

    #[test]
    fn unlanded_total_and_note_unlanded_fail_the_ack_wait_for_a_batch_that_landed_nowhere() {
        let wm = AppliedWatermark::new_for_tests();
        let before = wm.unlanded_total();
        wm.note_ticks_handed_off();
        wm.note_ticks_completed();
        assert!(wm.wait_for_offload_drained(Duration::from_millis(1), before));
        // The writer completed a batch whose ILP write AND rescue failed: the
        // counters balance, the rows are in the WAL segment only.
        wm.note_ticks_handed_off();
        wm.note_unlanded();
        wm.note_ticks_completed();
        assert!(
            !wm.wait_for_offload_drained(Duration::from_millis(1), before),
            "a completion that landed nowhere must not let the segment be archived"
        );
        // A caller that snapshots AFTER the loss is not blocked by it.
        assert!(wm.wait_for_offload_drained(Duration::from_millis(1), wm.unlanded_total()));
    }

    #[test]
    fn reset_unapplied_below_keeps_buckets_captured_after_the_ceiling() {
        let wm = AppliedWatermark::new_for_tests();
        let old = seq(10);
        let ceiling = seq(10) + (5u64 << UNAPPLIED_BUCKET_SHIFT);
        let new = ceiling + (1u64 << UNAPPLIED_BUCKET_SHIFT);
        wm.note_unapplied(old);
        wm.note_unapplied(new);
        wm.reset_unapplied_below(ceiling);
        let snap = wm.snapshot();
        assert!(
            !snap.range_has_unapplied(old, old),
            "a bucket below the ceiling was confirmed and is cleared"
        );
        assert!(
            snap.range_has_unapplied(new, new),
            "a bucket above the ceiling — a shed during the drain — survives"
        );
        assert!(!snap.overflowed);
        wm.reset_unapplied_below(u64::MAX);
        assert!(!wm.snapshot().range_has_unapplied(new, new));
    }

    /// The 2026-09-05 loss, pinned. An adversarial sweep found this on the day
    /// the module landed and reproduced it before it was fixed.
    ///
    /// The chain: a bucket-id collision makes `note_unapplied` set `overflowed`
    /// and store NOTHING. `reset_unapplied_below` then clears `overflowed`
    /// because `kept == 0` — but `kept` counts only buckets it DID record, and
    /// the collided frame is by definition not among them. With the only trace
    /// erased, a later ack lifts the high-water mark past the frame,
    /// `range_has_unapplied` answers false, and the replay renames the whole
    /// segment to `archive/` unread. Every tick and depth row in it is gone,
    /// permanently, with no counter anywhere.
    #[test]
    fn a_collided_bucket_survives_a_reset_that_has_not_passed_it() {
        let wm = AppliedWatermark::new_for_tests();
        // Two ids that land in the SAME slot: the table is indexed modulo
        // UNAPPLIED_BUCKETS, so ids exactly that far apart collide.
        let first = seq(10);
        let colliding = first + ((UNAPPLIED_BUCKETS as u64) << UNAPPLIED_BUCKET_SHIFT);
        wm.note_unapplied(first);
        wm.note_unapplied(colliding);
        assert!(
            wm.snapshot().overflowed,
            "a colliding id must set the overflow flag — it could not be recorded"
        );

        // A drain confirms everything up to a ceiling BELOW the collided
        // frame. The first bucket is legitimately cleared; the collided one
        // was never stored, so `kept` is 0.
        wm.reset_unapplied_below(first + (1u64 << UNAPPLIED_BUCKET_SHIFT));

        assert!(
            wm.snapshot().overflowed,
            "the flag MUST survive: the confirmed ceiling has not passed the \
             frame the table failed to record. Clearing here is the silent \
             permanent segment loss this test exists to prevent."
        );
        assert!(
            wm.snapshot().range_has_unapplied(colliding, colliding),
            "the collided frame must still read as unapplied, so its segment \
             is replayed rather than archived unread"
        );
    }

    #[test]
    fn a_reset_that_passes_the_overflow_height_may_clear_the_flag() {
        // The other direction, so the fix cannot be "never clear it", which
        // would make every later boot replay everything forever.
        let wm = AppliedWatermark::new_for_tests();
        let first = seq(10);
        let colliding = first + ((UNAPPLIED_BUCKETS as u64) << UNAPPLIED_BUCKET_SHIFT);
        wm.note_unapplied(first);
        wm.note_unapplied(colliding);
        assert!(wm.snapshot().overflowed);

        // Now the drain confirms PAST the collided frame's bucket top.
        wm.reset_unapplied_below(colliding + (2u64 << UNAPPLIED_BUCKET_SHIFT));
        assert!(
            !wm.snapshot().overflowed,
            "once the confirmed ceiling has provably passed everything the \
             table failed to record, skipping is safe again"
        );
    }

    #[test]
    fn an_over_wide_unapplied_range_survives_a_reset_below_its_top() {
        // The second overflow arm: a range spanning more buckets than the
        // table holds records nothing at all.
        let wm = AppliedWatermark::new_for_tests();
        let lo = seq(10);
        let hi = lo + (((UNAPPLIED_BUCKETS + 8) as u64) << UNAPPLIED_BUCKET_SHIFT);
        wm.note_unapplied_range(lo, hi);
        assert!(wm.snapshot().overflowed);

        wm.reset_unapplied_below(lo + (4u64 << UNAPPLIED_BUCKET_SHIFT));
        assert!(
            wm.snapshot().overflowed,
            "a reset below the range's top must not clear it — the frames \
             between the ceiling and `hi` were never recorded"
        );

        wm.reset_unapplied_below(hi + 1);
        assert!(
            !wm.snapshot().overflowed,
            "a reset past the range's top may clear it"
        );
    }

    #[test]
    fn a_persisted_overflow_flag_can_never_be_cleared_by_this_process() {
        // The file format carries one BIT, not a height, so a loaded flag
        // must be treated as unbounded: this process cannot know how far the
        // previous one failed to record.
        let wm = AppliedWatermark::new_for_tests();
        wm.seed(&AppliedSnapshot {
            overflowed: true,
            ..AppliedSnapshot::default()
        });
        wm.reset_unapplied_below(u64::MAX);
        assert!(
            wm.snapshot().overflowed,
            "a persisted overflow has no known height and must fail towards \
             replay for the life of the process"
        );
    }

    #[test]
    fn mark_depth_tracked_clears_a_persisted_untracked_flag() {
        let wm = AppliedWatermark::new_for_tests();
        wm.seed(&AppliedSnapshot {
            depth_untracked: true,
            ..AppliedSnapshot::default()
        });
        assert!(wm.snapshot().depth_untracked);
        wm.mark_depth_tracked();
        assert!(!wm.snapshot().depth_untracked);
        wm.mark_depth_untracked();
        assert!(wm.snapshot().depth_untracked);
    }

    #[test]
    fn note_unlanded_increments_the_monotone_total() {
        let wm = AppliedWatermark::new_for_tests();
        assert_eq!(wm.unlanded_total(), 0);
        wm.note_unlanded();
        wm.note_unlanded();
        assert_eq!(wm.unlanded_total(), 2, "monotone: it never goes back down");
    }

    // The push-scoped pub-fn ratchet matches a test to a function by NAME
    // prefix. The functions below are all exercised by the scenario tests
    // above; these pin each one on its own so the ratchet can see it.

    #[test]
    fn note_ticks_acked_advances_only_the_tick_watermark() {
        let wm = AppliedWatermark::new_for_tests();
        wm.note_ticks_acked(seq(50));
        let snap = wm.snapshot();
        assert_eq!(snap.hwm_ticks, seq(50));
        assert_eq!(snap.hwm_depth, 0);
        wm.note_ticks_acked(seq(40));
        assert_eq!(wm.snapshot().hwm_ticks, seq(50), "monotone");
    }

    #[test]
    fn note_depth_acked_advances_only_the_depth_watermark() {
        let wm = AppliedWatermark::new_for_tests();
        wm.note_depth_acked(seq(50));
        let snap = wm.snapshot();
        assert_eq!(snap.hwm_depth, seq(50));
        assert_eq!(snap.hwm_ticks, 0);
    }

    #[test]
    fn note_ticks_handed_off_is_balanced_by_a_completion() {
        let wm = AppliedWatermark::new_for_tests();
        wm.note_ticks_handed_off();
        assert!(!wm.wait_for_offload_drained(Duration::from_millis(1), 0));
        wm.note_ticks_completed();
        assert!(wm.wait_for_offload_drained(Duration::from_millis(1), 0));
    }

    #[test]
    fn note_ticks_completed_without_a_hand_off_is_harmless() {
        let wm = AppliedWatermark::new_for_tests();
        wm.note_ticks_completed();
        assert!(wm.wait_for_offload_drained(Duration::from_millis(1), 0));
    }

    #[test]
    fn note_depth_handed_off_is_balanced_by_a_completion() {
        let wm = AppliedWatermark::new_for_tests();
        wm.note_depth_handed_off();
        assert!(!wm.wait_for_offload_drained(Duration::from_millis(1), 0));
        wm.note_depth_completed();
        assert!(wm.wait_for_offload_drained(Duration::from_millis(1), 0));
    }

    #[test]
    fn note_depth_completed_without_a_hand_off_is_harmless() {
        let wm = AppliedWatermark::new_for_tests();
        wm.note_depth_completed();
        assert!(wm.wait_for_offload_drained(Duration::from_millis(1), 0));
    }

    #[test]
    fn frame_is_applied_respects_the_slack_and_the_lower_sink() {
        let wm = AppliedWatermark::new_for_tests();
        wm.mark_depth_tracked();
        wm.note_ticks_acked(seq(100));
        wm.note_depth_acked(seq(60));
        let snap = wm.snapshot();
        // Ticks alone would allow 90; the depth sink at 60 (minus 1 s of
        // slack) is the binding one for a main-feed frame.
        assert!(snap.frame_is_applied(AppliedSink::Ticks, seq(58)));
        assert!(!snap.frame_is_applied(AppliedSink::Ticks, seq(90)));
        assert!(snap.frame_is_applied(AppliedSink::Depth, seq(58)));
        assert!(!snap.frame_is_applied(AppliedSink::Depth, seq(59) + 1));
    }

    #[test]
    fn range_has_unapplied_sees_only_the_marked_bucket() {
        let wm = AppliedWatermark::new_for_tests();
        let marked = seq(1_000);
        wm.note_unapplied(marked);
        let snap = wm.snapshot();
        assert!(snap.range_has_unapplied(marked, marked));
        assert!(snap.range_has_unapplied(marked - 5, marked + 5));
        let far = marked + (3u64 << UNAPPLIED_BUCKET_SHIFT);
        assert!(!snap.range_has_unapplied(far, far + 5));
        assert!(
            snap.range_has_unapplied(5, 1),
            "lo > hi fails towards replay"
        );
    }

    #[test]
    fn clean_probe_count_counts_only_clean_probes() {
        let wm = AppliedWatermark::new_for_tests();
        assert_eq!(wm.clean_probe_count(), 0);
        wm.note_questdb_probe(true);
        wm.note_questdb_probe(false);
        wm.note_questdb_probe(true);
        assert_eq!(wm.clean_probe_count(), 2, "a suspect probe is not counted");
    }

    #[test]
    fn is_sink_suspect_follows_the_last_probe() {
        let wm = AppliedWatermark::new_for_tests();
        assert!(!wm.is_sink_suspect());
        wm.note_questdb_probe(false);
        assert!(wm.is_sink_suspect());
        wm.note_questdb_probe(true);
        assert!(!wm.is_sink_suspect());
    }

    #[test]
    fn note_questdb_probe_clean_re_opens_the_watermark_for_new_acks() {
        let wm = AppliedWatermark::new_for_tests();
        wm.note_ticks_acked(seq(10));
        wm.note_questdb_probe(false);
        wm.note_ticks_acked(seq(20));
        assert_eq!(wm.snapshot().hwm_ticks, seq(10), "parked while suspect");
        wm.note_questdb_probe(true);
        wm.note_ticks_acked(seq(30));
        assert_eq!(wm.snapshot().hwm_ticks, seq(30), "advancing again");
    }

    #[test]
    fn persist_if_due_now_writes_at_most_once_per_interval() {
        let dir = scratch("persist_due_now");
        let wm = AppliedWatermark::new_for_tests();
        wm.bind(&dir);
        wm.note_ticks_acked(seq(5));
        assert!(wm.persist_if_due_now(), "first persist is always due");
        assert!(
            !wm.persist_if_due_now(),
            "the second inside the interval is not"
        );
        assert!(AppliedSnapshot::load(&dir).is_some());
    }

    #[test]
    fn test_dir_tag_of_is_stable_nonzero_and_distinct_per_directory() {
        let base = std::env::temp_dir().join(format!("tv-dirtag-{}", std::process::id()));
        let a = base.join("a");
        let b = base.join("b");
        std::fs::create_dir_all(&a).expect("dir a");
        std::fs::create_dir_all(&b).expect("dir b");
        assert_eq!(dir_tag_of(&a), dir_tag_of(&a));
        assert_ne!(dir_tag_of(&a), 0);
        assert_ne!(dir_tag_of(&a), dir_tag_of(&b));
        drop(std::fs::remove_dir_all(&base));
    }

    #[test]
    fn test_write_fresh_replaces_a_stale_tmp() {
        let dir = std::env::temp_dir().join(format!("tv-writefresh-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("dir");
        let tmp = dir.join("marks.tmp");
        std::fs::write(&tmp, b"stale leftover").expect("stale");
        write_fresh(&tmp, b"new").expect("write_fresh");
        assert_eq!(std::fs::read(&tmp).expect("read"), b"new");
        drop(std::fs::remove_dir_all(&dir));
    }

    #[test]
    fn test_wall_nanos_is_a_real_wall_clock_reading() {
        let first = wall_nanos();
        // After 2020-01-01 and not in the far future.
        assert!(first > 1_577_836_800_000_000_000);
        assert!(wall_nanos() >= first);
    }

    // -----------------------------------------------------------------------
    // Z8b — durability lag on the PERSISTED watermark (2026-10-02)
    // -----------------------------------------------------------------------

    const LAG: u64 = WATERMARK_DURABILITY_LAG_NANOS;
    const SEC: u64 = 1_000_000_000;

    #[test]
    fn durability_lag_record_and_durable_at_never_exceed_the_value_acked_a_lag_ago() {
        let mut lag = DurabilityLag::new();
        // One ack a second for five minutes; the watermark at second `s` is s.
        // Persisted at second `t`, the value must be the newest one read at
        // least LAG earlier — never anything acked inside the last minute.
        for t in 0..300u64 {
            lag.record(t * SEC, t, t * 10);
            let (ticks, depth) = lag.durable_at(t * SEC, LAG);
            let expected = t.saturating_sub(WATERMARK_DURABILITY_LAG_SECS);
            if t >= WATERMARK_DURABILITY_LAG_SECS {
                assert_eq!((ticks, depth), (expected, expected * 10), "t={t}");
            } else {
                assert_eq!((ticks, depth), (0, 0), "nothing is a lag old yet at t={t}");
            }
        }
    }

    #[test]
    fn durability_lag_durable_at_catches_up_to_live_after_a_quiet_lag() {
        let mut lag = DurabilityLag::new();
        lag.record(10 * SEC, 500, 700);
        assert_eq!(lag.durable_at(10 * SEC, LAG), (0, 0));
        // Quiet: no new acks, persists keep sampling the same live value.
        lag.record(40 * SEC, 500, 700);
        assert_eq!(lag.durable_at(40 * SEC, LAG), (0, 0));
        lag.record(10 * SEC + LAG, 500, 700);
        assert_eq!(
            lag.durable_at(10 * SEC + LAG, LAG),
            (500, 700),
            "after a quiet lag the persisted value equals the live one"
        );
    }

    #[test]
    fn durability_lag_seed_is_the_starting_floor_and_is_monotone() {
        let mut lag = DurabilityLag::default();
        lag.seed(100, 90);
        lag.record(0, 150, 140);
        assert_eq!(
            lag.durable_at(1, LAG),
            (100, 90),
            "the seed stands until a sample ages"
        );
        lag.seed(50, 50);
        assert_eq!(
            lag.durable_at(2, LAG),
            (100, 90),
            "a lower seed never lowers it"
        );
        assert_eq!(lag.durable_at(LAG, LAG), (150, 140));
    }

    #[test]
    fn durability_lag_record_burst_refreshes_the_newest_slot_instead_of_evicting() {
        let mut lag = DurabilityLag::new();
        lag.record(0, 1, 1);
        lag.record(SEC, 2, 2);
        // Ten thousand persists inside one second must not push the t=0 and
        // t=1 s samples out of a 128-slot ring.
        for i in 0..10_000u64 {
            lag.record(2 * SEC + i * 1_000, 3 + i, 3 + i);
        }
        assert_eq!(lag.durable_at(SEC + LAG, LAG), (2, 2));
        // The refreshed slot carries its LATEST read time, so its values are
        // not released early.
        let last_read = 2 * SEC + 9_999 * 1_000;
        assert_eq!(lag.durable_at(last_read + LAG - 1, LAG), (2, 2));
        assert_eq!(lag.durable_at(last_read + LAG, LAG), (10_002, 10_002));
    }

    #[test]
    fn durability_lag_record_wraps_the_fixed_ring_without_growing() {
        let mut lag = DurabilityLag::new();
        let before = std::mem::size_of_val(&lag);
        for t in 0..(4 * DURABILITY_SAMPLE_SLOTS as u64) {
            lag.record(t * SEC, t, t);
        }
        assert_eq!(
            std::mem::size_of_val(&lag),
            before,
            "a fixed array, no heap"
        );
        assert_eq!(lag.filled, DURABILITY_SAMPLE_SLOTS);
        let now = (4 * DURABILITY_SAMPLE_SLOTS as u64 - 1) * SEC;
        let expected = now / SEC - WATERMARK_DURABILITY_LAG_SECS;
        assert_eq!(lag.durable_at(now, LAG), (expected, expected));
    }

    #[test]
    fn durability_lag_ignores_a_sample_from_the_future() {
        let mut lag = DurabilityLag::new();
        lag.record(500 * SEC, 9, 9);
        assert_eq!(lag.durable_at(SEC, LAG), (0, 0));
    }

    #[test]
    fn test_regression_persisted_watermark_lags_acks_by_the_durability_lag() {
        let dir = scratch("durability_lag");
        let wm = AppliedWatermark::new_for_tests();
        wm.bind(&dir);
        wm.note_ticks_acked(seq(100));
        wm.note_depth_acked(seq(100));
        wm.persist_at(5 * SEC);
        let first = AppliedSnapshot::load(&dir).expect("file");
        assert_eq!(
            (first.hwm_ticks, first.hwm_depth),
            (0, 0),
            "an ack seconds old may still be only in QuestDB's page cache"
        );
        // RAM is NOT delayed.
        assert_eq!(wm.snapshot().hwm_ticks, seq(100));
        wm.note_ticks_acked(seq(200));
        wm.note_depth_acked(seq(200));
        wm.persist_at(5 * SEC + LAG);
        let second = AppliedSnapshot::load(&dir).expect("file");
        assert_eq!(
            (second.hwm_ticks, second.hwm_depth),
            (seq(100), seq(100)),
            "only what was acked a full lag ago is persisted"
        );
        wm.persist_at(5 * SEC + 2 * LAG);
        let third = AppliedSnapshot::load(&dir).expect("file");
        assert_eq!(third.hwm_ticks, seq(200), "a quiet lag later it catches up");
        drop(std::fs::remove_dir_all(&dir));
    }

    #[test]
    fn a_bound_watermark_persists_the_seeded_value_before_any_sample_ages() {
        let dir = scratch("durability_seed");
        let wm = AppliedWatermark::new_for_tests();
        wm.bind(&dir);
        wm.seed(&AppliedSnapshot {
            hwm_ticks: seq(40),
            hwm_depth: seq(40),
            ..AppliedSnapshot::default()
        });
        wm.note_ticks_acked(seq(90));
        wm.persist_at(SEC);
        let loaded = AppliedSnapshot::load(&dir).expect("file");
        assert_eq!(
            loaded.hwm_ticks,
            seq(40),
            "the previous process's file is the floor, never zero"
        );
        drop(std::fs::remove_dir_all(&dir));
    }

    // -----------------------------------------------------------------------
    // Z7 — queued rescue floors (2026-10-02)
    // -----------------------------------------------------------------------

    fn floors_held(wm: &AppliedWatermark, sink: AppliedSink) -> usize {
        wm.rescue_floors(sink)
            .iter()
            .filter(|c| c.load(Ordering::Acquire) != 0)
            .count()
    }

    #[test]
    fn hold_rescue_floor_holds_the_persisted_watermark_below_a_queued_batch() {
        for sink in [AppliedSink::Ticks, AppliedSink::Depth] {
            let wm = AppliedWatermark::new_for_tests();
            let floor = wm.hold_rescue_floor(sink, seq(50), seq(60));
            assert!(floor.is_some());
            // The writer thread acks a LATER batch while ours sits queued.
            wm.note_ticks_acked(seq(80));
            wm.note_depth_acked(seq(80));
            let view = wm.durable_view_at(LAG + SEC * 10);
            let wm_view = wm.durable_view_at(2 * LAG + SEC * 10);
            for v in [&view, &wm_view] {
                let hwm = match sink {
                    AppliedSink::Ticks => v.hwm_ticks,
                    AppliedSink::Depth => v.hwm_depth,
                };
                assert!(
                    hwm < seq(50),
                    "{sink:?}: persisted {hwm} must stay below the queued batch"
                );
                assert!(!v.range_is_applied(sink, seq(50), seq(60)));
            }
            assert_eq!(wm.snapshot().hwm_ticks, seq(80), "RAM is unchanged");
        }
    }

    #[test]
    fn release_rescue_floor_lets_the_persisted_watermark_pass_the_batch() {
        for sink in [AppliedSink::Ticks, AppliedSink::Depth] {
            let wm = AppliedWatermark::new_for_tests();
            let floor = wm.hold_rescue_floor(sink, seq(50), seq(60)).expect("slot");
            wm.note_ticks_acked(seq(80));
            wm.note_depth_acked(seq(80));
            let _ = wm.durable_view_at(0);
            wm.release_rescue_floor(floor);
            assert_eq!(floors_held(&wm, sink), 0);
            let view = wm.durable_view_at(LAG);
            assert_eq!(
                (view.hwm_ticks, view.hwm_depth),
                (seq(80), seq(80)),
                "{sink:?}"
            );
        }
    }

    #[test]
    fn hold_rescue_floor_refused_hand_off_retracts_its_floor() {
        // The producer holds, the `try_send` is refused, the producer
        // releases: nothing is left held, the other sink is untouched.
        let wm = AppliedWatermark::new_for_tests();
        let floor = wm
            .hold_rescue_floor(AppliedSink::Depth, seq(5), seq(6))
            .expect("slot");
        assert_eq!(floors_held(&wm, AppliedSink::Depth), 1);
        assert_eq!(floors_held(&wm, AppliedSink::Ticks), 0);
        wm.release_rescue_floor(floor);
        assert_eq!(floors_held(&wm, AppliedSink::Depth), 0);
    }

    #[test]
    fn hold_rescue_floor_ignores_a_batch_with_no_sequence() {
        let wm = AppliedWatermark::new_for_tests();
        assert!(
            wm.hold_rescue_floor(AppliedSink::Ticks, 0, seq(9))
                .is_none()
        );
        assert_eq!(floors_held(&wm, AppliedSink::Ticks), 0);
        assert!(!wm.snapshot().overflowed);
        assert!(wm.snapshot().buckets.iter().all(|(id, _)| *id == 0));
    }

    #[test]
    fn hold_rescue_floor_on_a_full_table_marks_the_range_unapplied() {
        let wm = AppliedWatermark::new_for_tests();
        for i in 0..RESCUE_FLOOR_SLOTS as u64 {
            assert!(
                wm.hold_rescue_floor(AppliedSink::Ticks, seq(10 + i), seq(10 + i))
                    .is_some()
            );
        }
        let far = seq(5_000);
        assert!(
            wm.hold_rescue_floor(AppliedSink::Ticks, far, far + 10)
                .is_none()
        );
        assert!(
            wm.snapshot().range_has_unapplied(far, far),
            "a batch that could not hold a floor must replay anyway"
        );
    }

    #[test]
    fn hold_rescue_floor_takes_the_lowest_of_several_queued_batches() {
        let wm = AppliedWatermark::new_for_tests();
        let a = wm
            .hold_rescue_floor(AppliedSink::Ticks, seq(70), seq(71))
            .expect("a");
        let _b = wm
            .hold_rescue_floor(AppliedSink::Ticks, seq(30), seq(31))
            .expect("b");
        wm.note_ticks_acked(seq(90));
        wm.note_depth_acked(seq(90));
        let _ = wm.durable_view_at(0);
        assert!(wm.durable_view_at(LAG).hwm_ticks < seq(30));
        wm.release_rescue_floor(a);
        assert!(
            wm.durable_view_at(LAG + SEC).hwm_ticks < seq(30),
            "b still queued"
        );
    }

    #[test]
    fn test_regression_an_abandoned_rescue_survives_into_the_persisted_file() {
        for sink in [AppliedSink::Ticks, AppliedSink::Depth] {
            let dir = scratch("abandoned_rescue");
            let wm = AppliedWatermark::new_for_tests();
            wm.bind(&dir);
            // A batch is queued for the rescue thread and never written
            // (shutdown abandonment, OOM, SIGKILL): its floor is never released.
            let _held = wm.hold_rescue_floor(sink, seq(50), seq(60));
            wm.note_ticks_acked(seq(400));
            wm.note_depth_acked(seq(400));
            wm.persist_at(0);
            wm.persist_at(10 * LAG);
            let loaded = AppliedSnapshot::load(&dir).expect("file");
            assert!(
                !loaded.range_is_applied(sink, seq(50), seq(60)),
                "{sink:?}: the next boot must replay the queued batch's range"
            );
            assert!(
                loaded.range_is_applied(sink, seq(1), seq(40)),
                "{sink:?}: everything below it is still skipped"
            );
            drop(std::fs::remove_dir_all(&dir));
        }
    }

    #[test]
    fn test_cap_below_floor_holds_strictly_below() {
        assert_eq!(cap_below_floor(10, u64::MAX), 10);
        assert_eq!(cap_below_floor(10, 20), 10);
        assert_eq!(cap_below_floor(20, 20), 19);
        assert_eq!(cap_below_floor(30, 20), 19);
        assert_eq!(cap_below_floor(30, 1), 0);
    }

    #[test]
    fn test_mono_nanos_is_monotone() {
        let a = mono_nanos();
        assert!(mono_nanos() >= a);
    }
}
