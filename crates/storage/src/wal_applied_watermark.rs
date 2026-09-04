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
fn dir_tag_of(wal_dir: &Path) -> u64 {
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
    bucket_ids: [AtomicU64; UNAPPLIED_BUCKETS],
    bucket_counts: [AtomicU64; UNAPPLIED_BUCKETS],
    ticks_handed_off: AtomicU64,
    ticks_completed: AtomicU64,
    depth_handed_off: AtomicU64,
    depth_completed: AtomicU64,
    last_persist_nanos: AtomicU64,
    paths: OnceLock<(PathBuf, PathBuf)>,
    persist_lock: Mutex<()>,
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
            bucket_ids: [const { AtomicU64::new(0) }; UNAPPLIED_BUCKETS],
            bucket_counts: [const { AtomicU64::new(0) }; UNAPPLIED_BUCKETS],
            ticks_handed_off: AtomicU64::new(0),
            ticks_completed: AtomicU64::new(0),
            depth_handed_off: AtomicU64::new(0),
            depth_completed: AtomicU64::new(0),
            last_persist_nanos: AtomicU64::new(0),
            paths: OnceLock::new(),
            persist_lock: Mutex::new(()),
            dir_tag: AtomicU64::new(0),
            sink_suspect: AtomicBool::new(false),
            healthy_ticks: AtomicU64::new(0),
            healthy_depth: AtomicU64::new(0),
            suspect_max_ticks: AtomicU64::new(0),
            suspect_max_depth: AtomicU64::new(0),
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
        // O(1) EXEMPT: fixed UNAPPLIED_BUCKETS iterations, boot-time reset
        for i in 0..UNAPPLIED_BUCKETS {
            self.bucket_ids[i].store(0, Ordering::Release);
            self.bucket_counts[i].store(0, Ordering::Release);
        }
        self.overflowed.store(false, Ordering::Release);
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
    pub fn wait_for_offload_drained(&self, timeout: Duration) -> bool {
        let target_ticks = self.ticks_handed_off.load(Ordering::Acquire);
        let target_depth = self.depth_handed_off.load(Ordering::Acquire);
        let deadline = std::time::Instant::now() + timeout;
        loop {
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
    pub fn persist_now(&self) {
        let Some((path, tmp)) = self.paths.get() else {
            return;
        };
        // Serialise concurrent persists from the two sink threads. A held lock
        // means a snapshot at most one interval old is being written; skip.
        let Ok(_guard) = self.persist_lock.try_lock() else {
            return;
        };
        let mut snap = self.snapshot();
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
fn write_fresh(tmp: &Path, bytes: &[u8]) -> std::io::Result<()> {
    drop(std::fs::remove_file(tmp)); // APPROVED: once-a-second persist, sink thread, cold path
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(tmp)?; // APPROVED: once-a-second persist, sink thread, cold path
    file.write_all(bytes)
}

fn wall_nanos() -> u64 {
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

fn crc32_ieee(bytes: &[u8]) -> u32 {
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
        wm.persist_now();
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
            wm.wait_for_offload_drained(Duration::from_millis(1)),
            "nothing pending"
        );
        wm.note_ticks_handed_off();
        wm.note_depth_handed_off();
        assert!(
            !wm.wait_for_offload_drained(Duration::from_millis(60)),
            "two batches in flight"
        );
        wm.note_ticks_completed();
        assert!(
            !wm.wait_for_offload_drained(Duration::from_millis(60)),
            "depth still in flight"
        );
        wm.note_depth_completed();
        assert!(wm.wait_for_offload_drained(Duration::from_millis(1)));
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
}
