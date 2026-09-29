//! Deferred-depth marks — the record of which captured frames had their
//! order-book rows SKIPPED by the ingest shed, so their raw WAL segments are
//! kept until those rows are written back (plan item 45a, 2026-09-29).
//!
//! # Why this exists
//!
//! The ingest shed (`tickvault_common::ingest_shed`) stops the QuestDB depth
//! write when the database falls behind or the disk runs short. The frame is
//! already durable in the capture-at-receipt WAL at that point, so the shed
//! was meant to drop the DATABASE write and never the capture. It dropped both:
//! nothing recorded that the frame's depth rows were missing, the depth
//! acknowledgements that followed moved the applied watermark past it, and the
//! WAL prunes then deleted the segment after its retention window. MEASURED on
//! the live box 2026-09-29: 5,792,427 inline-depth packets shed by 10:08 IST,
//! every one of them lost for good two to three days later.
//!
//! Operator, 2026-09-29: "neevr ever anythign shodul be fucking missed dude
//! okay?" (`websocket-connection-scope-lock.md` "2026-09-29 — ZERO LOSS").
//!
//! # Why a separate table and not `note_unapplied`
//!
//! `wal_applied_watermark::note_unapplied` marks a whole ~4.6-minute bucket as
//! unapplied, and the next boot re-folds EVERY segment overlapping such a
//! bucket — ticks, candles and depth. A shed that lasts a session would mark
//! the whole ~25 GB day, and the next boot would replay all of it: the
//! 2026-09-29 morning flood, every day. These marks do one thing only: they
//! keep the segment from both prunes. The after-close pass writes back ONLY
//! the depth component the shed skipped, then clears the mark.
//!
//! # Shape
//!
//! A fixed, direct-mapped table of [`DEFERRED_BUCKETS`] slots keyed by
//! `capture_seq >> DEFERRED_BUCKET_SHIFT` (the same bucket width as the
//! unapplied table). Each slot holds its bucket id and a bitmask of which
//! depth kinds were shed in it ([`ShedDepth`]). The hot path is
//! [`DeferredDepth::note_shed`]: one relaxed load in the common case (the
//! bucket is already marked for that kind), at most one CAS and one
//! `fetch_or` when a bucket is marked for the first time. No allocation, no
//! lock, no I/O.
//!
//! A slot already holding a DIFFERENT bucket is a collision. The table cannot
//! record the mark, so it sets `overflowed` and remembers the highest sequence
//! it failed to record. While overflowed, every range reads as deferred (the
//! prunes keep everything): the failure direction is "keep more", never
//! "delete". `2048 × 4.6 min ≈ 6.5 days` of marks fit before two buckets can
//! share a slot.
//!
//! Persistence mirrors `applied.tvaw`: a CRC-checked file beside the segments,
//! written tmp + rename at most once a second from the writer threads, bound
//! to the WAL directory by a path tag so a file copied in from elsewhere is
//! rejected. A rejected or absent file seeds nothing. Marks newer than the
//! last successful persist are lost if the process dies — normally under a
//! second, but longer while persists are failing (full disk) or while no
//! writer thread is completing batches. The file is fsynced before the
//! rename, so a host crash cannot leave a renamed-but-empty file.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use tracing::{error, warn};

use crate::wal_applied_watermark::{crc32_ieee, dir_tag_of, wall_nanos, write_fresh};

/// The marks file, beside the `*.wal` segments. Not a `.wal`, so the segment
/// glob and the prunes never see it.
pub const DEFERRED_DEPTH_FILE: &str = "deferred_depth.tvdd";
/// Written first, then renamed over the live file.
pub const DEFERRED_DEPTH_TMP: &str = "deferred_depth.tvdd.tmp";
const DEFERRED_MAGIC: [u8; 4] = *b"TVDD";
const DEFERRED_VERSION: u8 = 1;

/// `capture_seq >> DEFERRED_BUCKET_SHIFT` is a bucket id — the same width as
/// `wal_applied_watermark::UNAPPLIED_BUCKET_SHIFT` (≈4.6 minutes), so one
/// segment (~1.6 min) overlaps at most two buckets.
pub const DEFERRED_BUCKET_SHIFT: u32 = crate::wal_applied_watermark::UNAPPLIED_BUCKET_SHIFT;
/// Slots in the direct-mapped table: `2048 × 4.6 min ≈ 6.5 days` of marked
/// buckets before two can collide. A full shed session marks ~85.
pub const DEFERRED_BUCKETS: usize = 2048;
/// Persist at most this often.
pub const DEFERRED_PERSIST_INTERVAL_NANOS: u64 = 1_000_000_000;

/// magic | version | flags | pad | overflow_high_seq | persisted_at | dir_tag
const HEADER_LEN: usize = 4 + 1 + 1 + 2 + 8 + 8 + 8;
/// Per slot: bucket id (8) + kinds (1) + pad (7).
const SLOT_LEN: usize = 16;
const BODY_LEN: usize = DEFERRED_BUCKETS * SLOT_LEN;
/// The whole file: header + slots + crc32.
pub const DEFERRED_DEPTH_LEN: usize = HEADER_LEN + BODY_LEN + 4;

const FLAG_OVERFLOWED: u8 = 0b01;

/// Counter: the marks file could not be written.
pub const DEFERRED_PERSIST_FAILED_COUNTER: &str = "tv_wal_deferred_depth_persist_failed_total";
/// Counter: a marks file was present but rejected.
pub const DEFERRED_INVALID_COUNTER: &str = "tv_wal_deferred_depth_invalid_total";

/// Which depth write a shed skipped. A bitmask so one slot can carry both.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ShedDepth {
    /// The five order-book levels inside a Full-mode main-feed packet.
    Inline = 0b01,
    /// A dedicated depth-20 or depth-200 frame.
    Dedicated = 0b10,
}

/// Both kinds — what an overflowed table, or an unknown range, must assume.
pub const SHED_DEPTH_ALL: u8 = ShedDepth::Inline as u8 | ShedDepth::Dedicated as u8;

/// A point-in-time copy of the marks. ~32 KiB, cold path only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeferredDepthSnapshot {
    pub overflowed: bool,
    /// Upper bound on every sequence the table failed to record.
    pub overflow_high_seq: u64,
    pub persisted_at_nanos: u64,
    /// See `wal_applied_watermark::AppliedSnapshot::dir_tag`.
    pub dir_tag: u64,
    /// `(bucket_id, kinds)` per slot; `(0, _)` is empty.
    pub slots: Vec<(u64, u8)>,
}

impl Default for DeferredDepthSnapshot {
    fn default() -> Self {
        Self {
            overflowed: false,
            overflow_high_seq: 0,
            persisted_at_nanos: 0,
            dir_tag: 0,
            slots: vec![(0, 0); DEFERRED_BUCKETS], // APPROVED: cold snapshot, fixed size
        }
    }
}

impl DeferredDepthSnapshot {
    /// The depth kinds shed anywhere in `[lo, hi]` (inclusive, capture
    /// sequences). `hi = None` means the range's end is unknown (the newest
    /// segment, or an unreadable successor): every mark at or after `lo`
    /// counts. `lo == 0` means the start is unknown too: any mark counts.
    /// While overflowed, every range reads as [`SHED_DEPTH_ALL`].
    ///
    /// O(DEFERRED_BUCKETS): every slot is visited. Cold prune path.
    #[must_use]
    pub fn kinds_in_range(&self, lo: u64, hi: Option<u64>) -> u8 {
        if self.overflowed {
            return SHED_DEPTH_ALL;
        }
        let lo_id = lo >> DEFERRED_BUCKET_SHIFT;
        let hi_id = hi.map(|h| h >> DEFERRED_BUCKET_SHIFT);
        let mut kinds = 0u8;
        // O(1) EXEMPT: fixed DEFERRED_BUCKETS iterations, cold prune path
        for &(id, k) in &self.slots {
            if id == 0 || k == 0 {
                continue;
            }
            let after_lo = lo == 0 || id >= lo_id;
            let before_hi = hi_id.is_none_or(|h| id <= h);
            if after_lo && before_hi {
                kinds |= k;
            }
        }
        kinds
    }

    /// Whether any depth in `[lo, hi]` is still waiting to be written back.
    #[must_use]
    pub fn range_is_deferred(&self, lo: u64, hi: Option<u64>) -> bool {
        self.kinds_in_range(lo, hi) != 0
    }

    /// Every marked bucket, oldest first — the after-close pass's work list.
    #[must_use]
    pub fn marked_buckets(&self) -> Vec<(u64, u8)> {
        let mut out: Vec<(u64, u8)> = self
            .slots
            .iter()
            .copied()
            .filter(|&(id, k)| id != 0 && k != 0)
            .collect(); // APPROVED: cold after-close work list
        out.sort_unstable_by_key(|&(id, _)| id); // O(1) EXEMPT: cold, at most DEFERRED_BUCKETS
        out
    }

    /// Whether nothing is waiting.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        !self.overflowed && self.slots.iter().all(|&(id, k)| id == 0 || k == 0)
    }

    /// The on-disk form: header, slots, CRC-32 over both.
    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = vec![0u8; DEFERRED_DEPTH_LEN]; // APPROVED: once-a-second persist, cold path
        out[0..4].copy_from_slice(&DEFERRED_MAGIC);
        out[4] = DEFERRED_VERSION;
        out[5] = if self.overflowed { FLAG_OVERFLOWED } else { 0 };
        out[8..16].copy_from_slice(&self.overflow_high_seq.to_le_bytes());
        out[16..24].copy_from_slice(&self.persisted_at_nanos.to_le_bytes());
        out[24..32].copy_from_slice(&self.dir_tag.to_le_bytes());
        // O(1) EXEMPT: fixed DEFERRED_BUCKETS iterations, cold persist path
        for (i, &(id, kinds)) in self.slots.iter().take(DEFERRED_BUCKETS).enumerate() {
            let at = HEADER_LEN + i * SLOT_LEN;
            out[at..at + 8].copy_from_slice(&id.to_le_bytes());
            out[at + 8] = kinds;
        }
        let crc = crc32_ieee(&out[..HEADER_LEN + BODY_LEN]);
        out[HEADER_LEN + BODY_LEN..].copy_from_slice(&crc.to_le_bytes());
        out
    }

    /// Parses the on-disk form. `None` on any shape or checksum failure.
    #[must_use]
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.len() != DEFERRED_DEPTH_LEN
            || bytes[0..4] != DEFERRED_MAGIC
            || bytes[4] != DEFERRED_VERSION
        {
            return None;
        }
        let expected = u32::from_le_bytes(bytes[HEADER_LEN + BODY_LEN..].try_into().ok()?);
        if crc32_ieee(&bytes[..HEADER_LEN + BODY_LEN]) != expected {
            return None;
        }
        let mut slots = vec![(0u64, 0u8); DEFERRED_BUCKETS]; // APPROVED: boot-time load
        // O(1) EXEMPT: fixed DEFERRED_BUCKETS iterations, boot-time load
        for (i, slot) in slots.iter_mut().enumerate() {
            let at = HEADER_LEN + i * SLOT_LEN;
            let id = u64::from_le_bytes(bytes[at..at + 8].try_into().ok()?);
            if id > MAX_BUCKET_ID {
                return None; // no capture sequence can produce it
            }
            let kinds = bytes[at + 8] & SHED_DEPTH_ALL;
            *slot = (id, kinds);
        }
        Some(Self {
            overflowed: bytes[5] & FLAG_OVERFLOWED != 0,
            overflow_high_seq: u64::from_le_bytes(bytes[8..16].try_into().ok()?),
            persisted_at_nanos: u64::from_le_bytes(bytes[16..24].try_into().ok()?),
            dir_tag: u64::from_le_bytes(bytes[24..32].try_into().ok()?),
            slots,
        })
    }

    /// Reads and validates `<wal_dir>/deferred_depth.tvdd`. Absent or
    /// rejected → `None`. See [`Self::load_file`] for the distinction, which
    /// only the boot bind needs.
    #[must_use]
    pub fn load(wal_dir: &Path) -> Option<Self> {
        match Self::load_file(wal_dir) {
            MarksFile::Loaded(snap) => Some(snap),
            MarksFile::Absent | MarksFile::Rejected => None,
        }
    }

    /// Reads `<wal_dir>/deferred_depth.tvdd`, telling an absent file (a first
    /// boot, or nothing ever shed) from a present-but-unusable one (torn,
    /// corrupt, wrong size or type, or written beside another directory).
    #[must_use]
    pub fn load_file(wal_dir: &Path) -> MarksFile {
        let path = wal_dir.join(DEFERRED_DEPTH_FILE);
        // Size and type BEFORE reading: a FIFO, a device or an oversized file
        // at this path must never hang or exhaust the prune or the boot.
        let meta = match std::fs::metadata(&path) {
            Ok(meta) => meta,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return MarksFile::Absent,
            Err(_) => return MarksFile::Rejected,
        };
        let bytes = if meta.is_file() && meta.len() == DEFERRED_DEPTH_LEN as u64 {
            match std::fs::read(&path) {
                Ok(bytes) => bytes, // APPROVED: boot-time load, cold path
                Err(_) => return MarksFile::Rejected,
            }
        } else {
            Vec::new() // APPROVED: rejected below as the wrong shape
        };
        match Self::from_bytes(&bytes) {
            Some(snap) if snap.dir_tag == dir_tag_of(wal_dir) => MarksFile::Loaded(snap),
            _ => {
                metrics::counter!(DEFERRED_INVALID_COUNTER).increment(1);
                MarksFile::Rejected
            }
        }
    }
}

/// What [`DeferredDepthSnapshot::load_file`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MarksFile {
    Absent,
    Loaded(DeferredDepthSnapshot),
    /// Present but unusable: the marks it held are unknown.
    Rejected,
}

impl DeferredDepthSnapshot {
    /// ORs `other` into `self`: a mark in either is kept. Two different
    /// buckets in one slot cannot both be held, so that case overflows —
    /// towards keeping everything, never towards dropping a mark.
    pub fn absorb(&mut self, other: &Self) {
        if other.overflowed {
            self.overflowed = true;
            self.overflow_high_seq = self.overflow_high_seq.max(other.overflow_high_seq);
        }
        // O(1) EXEMPT: fixed DEFERRED_BUCKETS iterations, cold prune path
        for (mine, &(id, kinds)) in self.slots.iter_mut().zip(other.slots.iter()) {
            if id == 0 || kinds == 0 {
                continue;
            }
            if mine.0 == 0 || mine.1 == 0 {
                *mine = (id, kinds);
            } else if mine.0 == id {
                mine.1 |= kinds;
            } else {
                self.overflowed = true;
                self.overflow_high_seq = self.overflow_high_seq.max(bucket_top(id));
            }
        }
    }
}

/// What the WAL prunes must respect: the process marks OR the file beside
/// `wal_dir`. The file is read too so that a process whose marks never bound
/// (or a rejected in-RAM seed) still protects what an earlier session
/// recorded. Cold prune path.
#[must_use]
pub fn prune_view(wal_dir: &Path) -> DeferredDepthSnapshot {
    let mut snap = deferred_depth().snapshot();
    if let Some(disk) = DeferredDepthSnapshot::load(wal_dir) {
        snap.absorb(&disk);
    }
    snap
}

/// The largest bucket id a `u64` capture sequence can produce.
const MAX_BUCKET_ID: u64 = u64::MAX >> DEFERRED_BUCKET_SHIFT;

/// The highest sequence inside bucket `id`. Total: an impossible id maps to
/// `u64::MAX` rather than overflowing (release builds abort on overflow).
const fn bucket_top(id: u64) -> u64 {
    if id >= MAX_BUCKET_ID {
        u64::MAX
    } else {
        ((id + 1) << DEFERRED_BUCKET_SHIFT) - 1
    }
}

const fn slot_of(bucket_id: u64) -> usize {
    (bucket_id % DEFERRED_BUCKETS as u64) as usize
}

/// The process-wide marks table.
pub struct DeferredDepth {
    ids: [AtomicU64; DEFERRED_BUCKETS],
    kinds: [AtomicU8; DEFERRED_BUCKETS],
    overflowed: AtomicBool,
    overflow_high_seq: AtomicU64,
    /// Sheds of a frame with no sequence (a v1-era record). Nothing can be
    /// kept for it by sequence; counted so the gap is visible.
    unrecordable: AtomicU64,
    last_persist_nanos: AtomicU64,
    paths: OnceLock<(PathBuf, PathBuf)>,
    persist_lock: Mutex<()>,
    dir_tag: AtomicU64,
    /// Set on every change; a persist with nothing changed is skipped, so an
    /// idle table is not rewritten every second.
    dirty: AtomicBool,
    /// Edge latch for the persist-failure warning: one line per episode, not
    /// one a second on a full disk.
    persist_failing: AtomicBool,
}

static DEFERRED: DeferredDepth = DeferredDepth::new();

/// The one process-global marks table.
#[must_use]
pub fn deferred_depth() -> &'static DeferredDepth {
    &DEFERRED
}

impl DeferredDepth {
    /// A fresh, unbound instance for tests that must not share the global.
    #[must_use]
    pub const fn new_for_tests() -> Self {
        Self::new()
    }

    const fn new() -> Self {
        Self {
            ids: [const { AtomicU64::new(0) }; DEFERRED_BUCKETS],
            kinds: [const { AtomicU8::new(0) }; DEFERRED_BUCKETS],
            overflowed: AtomicBool::new(false),
            overflow_high_seq: AtomicU64::new(0),
            unrecordable: AtomicU64::new(0),
            last_persist_nanos: AtomicU64::new(0),
            paths: OnceLock::new(),
            persist_lock: Mutex::new(()),
            dir_tag: AtomicU64::new(0),
            dirty: AtomicBool::new(false),
            persist_failing: AtomicBool::new(false),
        }
    }

    /// Binds to the WAL directory and seeds RAM from the file there. First
    /// caller wins, like `AppliedWatermark::bind`.
    pub fn bind(&self, wal_dir: &Path) {
        let set = self
            .paths
            .set((
                wal_dir.join(DEFERRED_DEPTH_FILE),
                wal_dir.join(DEFERRED_DEPTH_TMP),
            ))
            .is_ok();
        if set {
            self.dir_tag.store(dir_tag_of(wal_dir), Ordering::Release);
            match DeferredDepthSnapshot::load_file(wal_dir) {
                MarksFile::Loaded(snap) => self.seed(&snap),
                MarksFile::Absent => {}
                MarksFile::Rejected => self.adopt_rejected_file(wal_dir),
            }
        }
    }

    /// A marks file exists but cannot be read: the marks it held are unknown.
    /// Seeding nothing would let the first persist overwrite it with an EMPTY
    /// table a second later and unprotect every shed segment. Instead the
    /// file is moved aside for inspection and the table starts OVERFLOWED up
    /// to the highest sequence on disk, so every segment is kept until the
    /// after-close pass has swept them all.
    fn adopt_rejected_file(&self, wal_dir: &Path) {
        let path = wal_dir.join(DEFERRED_DEPTH_FILE);
        let aside = wal_dir.join(format!("{DEFERRED_DEPTH_FILE}.rejected.{}", wall_nanos())); // APPROVED: boot-time, once
        let moved = std::fs::rename(&path, &aside).is_ok();
        let high = crate::ws_frame_spill::highest_frame_seq_on_disk(wal_dir);
        self.overflow_high_seq
            .fetch_max(if high == 0 { u64::MAX } else { high }, Ordering::AcqRel);
        self.overflowed.store(true, Ordering::Release);
        self.dirty.store(true, Ordering::Release);
        error!(
            code = tickvault_common::error_code::ErrorCode::WsSpill01WriterRespawn.code_str(),
            source = "deferred_depth_marks_rejected",
            path = %path.display(),
            moved_aside = moved,
            aside = %aside.display(),
            "the record of order-book rows skipped under load could not be read — every WAL \
             segment is now kept until the after-close pass has swept them all. Nothing is \
             deleted; the unreadable file was moved aside for inspection."
        );
    }

    /// Loads a snapshot into RAM: marks are OR-ed in, never removed.
    pub fn seed(&self, snap: &DeferredDepthSnapshot) {
        if snap.overflowed {
            self.overflowed.store(true, Ordering::Release);
            self.overflow_high_seq
                .fetch_max(snap.overflow_high_seq, Ordering::AcqRel);
        }
        // O(1) EXEMPT: fixed DEFERRED_BUCKETS iterations, boot-time seed
        for &(id, kinds) in &snap.slots {
            if id == 0 || kinds == 0 {
                continue;
            }
            self.mark(id, kinds);
        }
    }

    /// Records that the depth rows of the frame at `seq` were shed. HOT PATH:
    /// called from the frame drain on every shed frame.
    ///
    /// O(1), zero allocation. The common case — the bucket is already marked
    /// for this kind — is two atomic loads (Acquire, then Relaxed) and a
    /// compare. Called once per shed PACKET on the inline arm.
    #[inline]
    pub fn note_shed(&self, seq: u64, kind: ShedDepth) {
        let id = seq >> DEFERRED_BUCKET_SHIFT;
        if id == 0 {
            self.unrecordable.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let slot = slot_of(id);
        let bit = kind as u8;
        if self.ids[slot].load(Ordering::Acquire) == id
            && self.kinds[slot].load(Ordering::Relaxed) & bit != 0
        {
            return;
        }
        if !self.mark(id, bit) {
            self.note_overflow(id);
        }
    }

    /// Records that bucket `id` could not be held: every range now reads as
    /// deferred until a complete sweep covers it.
    fn note_overflow(&self, id: u64) {
        self.overflow_high_seq
            .fetch_max(bucket_top(id), Ordering::AcqRel);
        self.overflowed.store(true, Ordering::Release);
        self.dirty.store(true, Ordering::Release);
    }

    /// Sets `bits` on bucket `id`, claiming an empty slot. `false` when the
    /// slot holds a different bucket. The kinds are written BEFORE the id is
    /// published on a fresh claim, so a reader that sees the id also sees at
    /// least those bits.
    fn mark(&self, id: u64, bits: u8) -> bool {
        let marked = self.mark_inner(id, bits);
        if marked {
            self.dirty.store(true, Ordering::Release);
        }
        marked
    }

    fn mark_inner(&self, id: u64, bits: u8) -> bool {
        let slot = slot_of(id);
        let current = self.ids[slot].load(Ordering::Acquire);
        if current == id {
            self.kinds[slot].fetch_or(bits, Ordering::AcqRel);
            return true;
        }
        if current == 0 {
            self.kinds[slot].fetch_or(bits, Ordering::AcqRel);
            match self.ids[slot].compare_exchange(0, id, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => return true,
                Err(now) if now == id => return true,
                Err(_) => {}
            }
        } else if self.ids[slot].load(Ordering::Acquire) == id {
            self.kinds[slot].fetch_or(bits, Ordering::AcqRel);
            return true;
        }
        false
    }

    /// Clears `done` bits from bucket `id` once the after-close pass has
    /// written those rows back and they are acknowledged. The slot is freed
    /// when no bits remain. A bucket re-marked meanwhile keeps its new bits.
    pub fn clear(&self, id: u64, done: u8) {
        let slot = slot_of(id);
        if self.ids[slot].load(Ordering::Acquire) != id {
            return;
        }
        let before = self.kinds[slot].fetch_and(!done, Ordering::AcqRel);
        self.dirty.store(true, Ordering::Release);
        if before & !done == 0 {
            // Free the slot only if nothing re-marked it in between; a
            // failed CAS means another bucket or a re-mark owns it now.
            let freed = self.ids[slot]
                .compare_exchange(id, 0, Ordering::AcqRel, Ordering::Acquire)
                .is_ok();
            if freed && self.kinds[slot].load(Ordering::Acquire) != 0 {
                // A shed landed between the fetch_and and the CAS: put the id
                // back so its bits are not orphaned. If another bucket took
                // the slot first, the bits cannot be attributed: fail towards
                // keeping everything.
                let bits = self.kinds[slot].load(Ordering::Acquire);
                if !self.mark(id, bits) {
                    self.note_overflow(id);
                }
            }
        }
    }

    /// Clears the overflow flag. Only the after-close pass calls this, after
    /// a complete sweep of every segment at or below `overflow_high_seq`.
    pub fn clear_overflow_through(&self, covered_seq: u64) {
        if covered_seq >= self.overflow_high_seq.load(Ordering::Acquire) {
            self.overflowed.store(false, Ordering::Release);
            self.dirty.store(true, Ordering::Release);
        }
    }

    /// Sheds that could not be recorded because the frame had no sequence.
    #[must_use]
    pub fn unrecordable_total(&self) -> u64 {
        self.unrecordable.load(Ordering::Relaxed)
    }

    /// Current RAM state. Cold path.
    #[must_use]
    pub fn snapshot(&self) -> DeferredDepthSnapshot {
        let mut slots = vec![(0u64, 0u8); DEFERRED_BUCKETS]; // APPROVED: cold snapshot
        // O(1) EXEMPT: fixed DEFERRED_BUCKETS iterations, cold path
        for (i, slot) in slots.iter_mut().enumerate() {
            *slot = (
                self.ids[i].load(Ordering::Acquire),
                self.kinds[i].load(Ordering::Acquire),
            );
        }
        DeferredDepthSnapshot {
            overflowed: self.overflowed.load(Ordering::Acquire),
            overflow_high_seq: self.overflow_high_seq.load(Ordering::Acquire),
            persisted_at_nanos: 0,
            dir_tag: self.dir_tag.load(Ordering::Acquire),
            slots,
        }
    }

    /// Persists when at least [`DEFERRED_PERSIST_INTERVAL_NANOS`] has passed.
    /// Cheap when not due: one load and one compare.
    pub fn persist_if_due(&self, now_nanos: u64) -> bool {
        if !self.dirty.load(Ordering::Acquire) {
            return false;
        }
        let last = self.last_persist_nanos.load(Ordering::Acquire);
        if now_nanos < last.saturating_add(DEFERRED_PERSIST_INTERVAL_NANOS) {
            return false;
        }
        if self
            .last_persist_nanos
            .compare_exchange(last, now_nanos, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return false;
        }
        self.persist_now();
        true
    }

    /// [`Self::persist_if_due`] against the wall clock.
    pub fn persist_if_due_now(&self) -> bool {
        self.persist_if_due(wall_nanos())
    }

    /// Writes the current state to the bound file (tmp + rename). Unbound →
    /// nothing. A failure is counted and warned, never propagated.
    pub fn persist_now(&self) {
        let Some((path, tmp)) = self.paths.get() else {
            return;
        };
        let _guard = match self.persist_lock.try_lock() {
            Ok(guard) => guard,
            // A panic mid-persist poisons the lock; the file it guards is
            // still written tmp + rename, so carry on rather than go silent.
            Err(std::sync::TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => return,
        };
        // Cleared BEFORE the snapshot: a change racing the write re-sets it
        // and is written next time.
        self.dirty.store(false, Ordering::Release);
        let mut snap = self.snapshot();
        snap.persisted_at_nanos = wall_nanos();
        let bytes = snap.to_bytes();
        let written = write_fresh(tmp, &bytes).and_then(|()| std::fs::rename(tmp, path));
        if let Err(err) = written {
            self.dirty.store(true, Ordering::Release);
            metrics::counter!(DEFERRED_PERSIST_FAILED_COUNTER).increment(1);
            if self.persist_failing.swap(true, Ordering::AcqRel) {
                return; // already reported this episode
            }
            warn!(
                path = %path.display(),
                error = %err,
                "WAL deferred-depth marks could not be persisted — the RAM marks are kept; \
                 a crash before the next successful persist would lose the newest marks"
            );
        } else if self.persist_failing.swap(false, Ordering::AcqRel) {
            tracing::info!(
                path = %path.display(),
                "WAL deferred-depth marks are persisting again"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seq(secs: u64) -> u64 {
        ((1_780_000_000 + secs) * 1_000_000_000) >> 17 << 17
    }

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "tv-deferred-{tag}-{}-{}",
            std::process::id(),
            wall_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("scratch dir");
        dir
    }

    #[test]
    fn a_fresh_table_defers_nothing() {
        let d = DeferredDepth::new_for_tests();
        let snap = d.snapshot();
        assert!(snap.is_empty());
        assert!(!snap.range_is_deferred(seq(0), Some(seq(10_000))));
        assert!(snap.marked_buckets().is_empty());
    }

    #[test]
    fn shed_inline_records_a_deferred_depth_mark() {
        let d = DeferredDepth::new_for_tests();
        d.note_shed(seq(100), ShedDepth::Inline);
        let snap = d.snapshot();
        assert_eq!(
            snap.kinds_in_range(seq(100), Some(seq(100))),
            ShedDepth::Inline as u8
        );
        assert_eq!(
            snap.marked_buckets(),
            vec![(seq(100) >> DEFERRED_BUCKET_SHIFT, ShedDepth::Inline as u8)]
        );
    }

    #[test]
    fn both_kinds_in_one_bucket_share_a_slot() {
        let d = DeferredDepth::new_for_tests();
        d.note_shed(seq(100), ShedDepth::Inline);
        d.note_shed(seq(101), ShedDepth::Dedicated);
        d.note_shed(seq(102), ShedDepth::Inline);
        let snap = d.snapshot();
        assert_eq!(
            snap.kinds_in_range(seq(100), Some(seq(102))),
            SHED_DEPTH_ALL
        );
        assert_eq!(snap.marked_buckets().len(), 1);
    }

    #[test]
    fn a_range_outside_every_mark_is_not_deferred() {
        let d = DeferredDepth::new_for_tests();
        d.note_shed(seq(10_000), ShedDepth::Inline);
        let snap = d.snapshot();
        // A range a full hour before the mark.
        assert!(!snap.range_is_deferred(seq(6_000), Some(seq(6_300))));
        // A range a full hour after it.
        assert!(!snap.range_is_deferred(seq(14_000), Some(seq(14_300))));
        // An open-ended range starting after it.
        assert!(!snap.range_is_deferred(seq(14_000), None));
    }

    #[test]
    fn an_unknown_range_end_keeps_every_later_mark() {
        let d = DeferredDepth::new_for_tests();
        d.note_shed(seq(10_000), ShedDepth::Dedicated);
        let snap = d.snapshot();
        assert!(snap.range_is_deferred(seq(9_000), None));
        assert!(
            snap.range_is_deferred(0, None),
            "an unknown start keeps any mark"
        );
    }

    #[test]
    fn a_zero_sequence_is_counted_not_marked() {
        let d = DeferredDepth::new_for_tests();
        d.note_shed(0, ShedDepth::Inline);
        assert_eq!(d.unrecordable_total(), 1);
        assert!(d.snapshot().is_empty());
    }

    #[test]
    fn a_colliding_bucket_overflows_towards_keeping_everything() {
        let d = DeferredDepth::new_for_tests();
        let a = seq(100);
        // Same slot, a different bucket: DEFERRED_BUCKETS buckets later.
        let b = a + ((DEFERRED_BUCKETS as u64) << DEFERRED_BUCKET_SHIFT);
        d.note_shed(a, ShedDepth::Inline);
        d.note_shed(b, ShedDepth::Inline);
        let snap = d.snapshot();
        assert!(snap.overflowed);
        assert!(snap.overflow_high_seq >= b);
        // Every range now reads as deferred, including one nowhere near a mark.
        assert_eq!(snap.kinds_in_range(seq(1), Some(seq(2))), SHED_DEPTH_ALL);
        // Clearing below the lost mark does not lift the flag; covering it does.
        d.clear_overflow_through(a);
        assert!(d.snapshot().overflowed);
        d.clear_overflow_through(b | ((1 << DEFERRED_BUCKET_SHIFT) - 1));
        assert!(!d.snapshot().overflowed);
    }

    #[test]
    fn deferred_mark_clears_only_the_done_kind() {
        let d = DeferredDepth::new_for_tests();
        d.note_shed(seq(100), ShedDepth::Inline);
        d.note_shed(seq(100), ShedDepth::Dedicated);
        let id = seq(100) >> DEFERRED_BUCKET_SHIFT;
        d.clear(id, ShedDepth::Inline as u8);
        let snap = d.snapshot();
        assert_eq!(
            snap.kinds_in_range(seq(100), Some(seq(100))),
            ShedDepth::Dedicated as u8
        );
        d.clear(id, ShedDepth::Dedicated as u8);
        assert!(
            d.snapshot().is_empty(),
            "the slot is freed once no kind remains"
        );
        // A cleared bucket can be marked again.
        d.note_shed(seq(100), ShedDepth::Inline);
        assert!(!d.snapshot().is_empty());
    }

    #[test]
    fn clearing_a_bucket_the_slot_no_longer_holds_is_a_no_op() {
        let d = DeferredDepth::new_for_tests();
        d.note_shed(seq(100), ShedDepth::Inline);
        d.clear(12345, SHED_DEPTH_ALL);
        assert!(!d.snapshot().is_empty());
    }

    #[test]
    fn deferred_marks_roundtrip_is_crc_checked() {
        let d = DeferredDepth::new_for_tests();
        d.note_shed(seq(100), ShedDepth::Inline);
        d.note_shed(seq(5_000), ShedDepth::Dedicated);
        let snap = d.snapshot();
        let bytes = snap.to_bytes();
        assert_eq!(bytes.len(), DEFERRED_DEPTH_LEN);
        assert_eq!(DeferredDepthSnapshot::from_bytes(&bytes), Some(snap));
        let mut flipped = bytes.clone();
        flipped[HEADER_LEN + 3] ^= 0x01;
        assert_eq!(
            DeferredDepthSnapshot::from_bytes(&flipped),
            None,
            "bit flip"
        );
        assert_eq!(
            DeferredDepthSnapshot::from_bytes(&bytes[..20]),
            None,
            "short"
        );
        let mut bad = bytes;
        bad[0] = b'X';
        assert_eq!(DeferredDepthSnapshot::from_bytes(&bad), None, "magic");
    }

    #[test]
    fn deferred_marks_persist_and_reload_on_disk() {
        let dir = scratch("persist");
        let d = DeferredDepth::new_for_tests();
        d.bind(&dir);
        d.note_shed(seq(100), ShedDepth::Inline);
        d.persist_now();
        let loaded = DeferredDepthSnapshot::load(&dir).expect("file written and accepted");
        assert!(loaded.range_is_deferred(seq(100), Some(seq(100))));
        // A second instance bound to the same dir seeds the mark.
        let e = DeferredDepth::new_for_tests();
        e.bind(&dir);
        assert!(e.snapshot().range_is_deferred(seq(100), Some(seq(100))));
        drop(std::fs::remove_dir_all(&dir));
    }

    #[test]
    fn a_marks_file_from_another_directory_is_rejected() {
        let a = scratch("dir-a");
        let b = scratch("dir-b");
        let d = DeferredDepth::new_for_tests();
        d.bind(&a);
        d.note_shed(seq(100), ShedDepth::Inline);
        d.persist_now();
        std::fs::copy(a.join(DEFERRED_DEPTH_FILE), b.join(DEFERRED_DEPTH_FILE)).expect("copy");
        assert!(DeferredDepthSnapshot::load(&b).is_none());
        drop(std::fs::remove_dir_all(&a));
        drop(std::fs::remove_dir_all(&b));
    }

    /// A torn or corrupt marks file must never become an EMPTY table: the
    /// bind moves it aside and keeps every segment until a full sweep.
    #[test]
    fn a_rejected_marks_file_keeps_everything_and_is_moved_aside() {
        let dir = scratch("rejected");
        std::fs::write(dir.join(DEFERRED_DEPTH_FILE), b"torn").unwrap();
        let d = DeferredDepth::new_for_tests();
        d.bind(&dir);
        let snap = d.snapshot();
        assert!(snap.overflowed, "unknown marks => keep everything");
        assert_eq!(snap.kinds_in_range(seq(1), Some(seq(2))), SHED_DEPTH_ALL);
        assert!(!dir.join(DEFERRED_DEPTH_FILE).exists(), "moved aside");
        let aside = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .any(|e| e.file_name().to_string_lossy().contains(".rejected."));
        assert!(aside, "kept for inspection");
        // A clearing without a full sweep must not lift it.
        d.clear_overflow_through(0);
        assert!(d.snapshot().overflowed);
        drop(std::fs::remove_dir_all(&dir));
    }

    /// An absent file is a first boot: nothing is kept.
    #[test]
    fn an_absent_marks_file_is_not_an_overflow() {
        let dir = scratch("absent");
        let d = DeferredDepth::new_for_tests();
        d.bind(&dir);
        assert!(d.snapshot().is_empty());
        drop(std::fs::remove_dir_all(&dir));
    }

    /// A forged id that no sequence can produce is rejected at load rather
    /// than overflowing the arithmetic (release builds abort on overflow).
    #[test]
    fn an_impossible_bucket_id_rejects_the_file() {
        let d = DeferredDepth::new_for_tests();
        d.note_shed(seq(100), ShedDepth::Inline);
        let mut bytes = d.snapshot().to_bytes();
        let at = HEADER_LEN;
        bytes[at..at + 8].copy_from_slice(&u64::MAX.to_le_bytes());
        let crc = crc32_ieee(&bytes[..HEADER_LEN + BODY_LEN]);
        bytes[HEADER_LEN + BODY_LEN..].copy_from_slice(&crc.to_le_bytes());
        assert_eq!(DeferredDepthSnapshot::from_bytes(&bytes), None);
        assert_eq!(bucket_top(u64::MAX), u64::MAX, "total, never overflows");
    }

    #[test]
    fn persist_if_due_writes_at_most_once_per_interval() {
        let dir = scratch("due");
        let d = DeferredDepth::new_for_tests();
        d.bind(&dir);
        assert!(
            !d.persist_if_due(DEFERRED_PERSIST_INTERVAL_NANOS),
            "nothing changed: nothing to write"
        );
        d.note_shed(seq(100), ShedDepth::Inline);
        assert!(d.persist_if_due(DEFERRED_PERSIST_INTERVAL_NANOS));
        d.note_shed(seq(9_000), ShedDepth::Inline);
        assert!(
            !d.persist_if_due(DEFERRED_PERSIST_INTERVAL_NANOS + 1),
            "inside the interval"
        );
        assert!(d.persist_if_due(3 * DEFERRED_PERSIST_INTERVAL_NANOS));
        assert!(
            !d.persist_if_due(5 * DEFERRED_PERSIST_INTERVAL_NANOS),
            "clean again after the write"
        );
        drop(std::fs::remove_dir_all(&dir));
    }

    #[test]
    fn marked_buckets_are_oldest_first() {
        let d = DeferredDepth::new_for_tests();
        d.note_shed(seq(20_000), ShedDepth::Inline);
        d.note_shed(seq(100), ShedDepth::Inline);
        d.note_shed(seq(9_000), ShedDepth::Dedicated);
        let ids: Vec<u64> = d
            .snapshot()
            .marked_buckets()
            .iter()
            .map(|&(id, _)| id)
            .collect();
        let mut sorted = ids.clone();
        sorted.sort_unstable();
        assert_eq!(ids, sorted);
        assert_eq!(ids.len(), 3);
    }

    // Named per public fn, so each is pinned by a test that says what it proves.

    #[test]
    fn test_kinds_in_range_reports_only_the_kinds_inside_the_range() {
        let d = DeferredDepth::new_for_tests();
        d.note_shed(seq(100), ShedDepth::Inline);
        d.note_shed(seq(20_000), ShedDepth::Dedicated);
        let snap = d.snapshot();
        assert_eq!(
            snap.kinds_in_range(seq(100), Some(seq(100))),
            ShedDepth::Inline as u8
        );
        assert_eq!(
            snap.kinds_in_range(seq(20_000), Some(seq(20_000))),
            ShedDepth::Dedicated as u8
        );
        assert_eq!(
            snap.kinds_in_range(seq(100), Some(seq(20_000))),
            SHED_DEPTH_ALL
        );
        assert_eq!(snap.kinds_in_range(seq(5_000), Some(seq(5_100))), 0);
    }

    #[test]
    fn test_range_is_deferred_only_where_a_mark_falls() {
        let d = DeferredDepth::new_for_tests();
        d.note_shed(seq(10_000), ShedDepth::Inline);
        let snap = d.snapshot();
        assert!(snap.range_is_deferred(seq(9_990), Some(seq(10_010))));
        assert!(!snap.range_is_deferred(seq(1_000), Some(seq(1_100))));
    }

    #[test]
    fn test_load_file_tells_absent_loaded_and_rejected_apart() {
        let dir = scratch("load-file");
        assert!(matches!(
            DeferredDepthSnapshot::load_file(&dir),
            MarksFile::Absent
        ));
        let d = DeferredDepth::new_for_tests();
        d.bind(&dir);
        d.note_shed(seq(100), ShedDepth::Inline);
        d.persist_now();
        match DeferredDepthSnapshot::load_file(&dir) {
            MarksFile::Loaded(snap) => assert_eq!(
                snap.kinds_in_range(seq(100), Some(seq(100))),
                ShedDepth::Inline as u8
            ),
            _ => panic!("a freshly persisted file must load"),
        }
        std::fs::write(dir.join(DEFERRED_DEPTH_FILE), b"torn").expect("write");
        assert!(matches!(
            DeferredDepthSnapshot::load_file(&dir),
            MarksFile::Rejected
        ));
        drop(std::fs::remove_dir_all(&dir));
    }

    /// A mark recorded by an earlier session (only on disk) still protects
    /// its segments through the prune view.
    #[test]
    fn test_prune_view_includes_the_marks_file_on_disk() {
        let dir = scratch("prune-view");
        let d = DeferredDepth::new_for_tests();
        d.bind(&dir);
        d.note_shed(seq(3_000), ShedDepth::Dedicated);
        d.persist_now();
        let view = prune_view(&dir);
        assert_ne!(
            view.kinds_in_range(seq(3_000), Some(seq(3_000))) & ShedDepth::Dedicated as u8,
            0
        );
        drop(std::fs::remove_dir_all(&dir));
    }

    #[test]
    fn test_deferred_depth_is_one_process_global() {
        assert!(std::ptr::eq(deferred_depth(), deferred_depth()));
    }

    #[test]
    fn test_note_shed_marks_the_bucket_of_its_sequence() {
        let d = DeferredDepth::new_for_tests();
        d.note_shed(seq(500), ShedDepth::Dedicated);
        assert_eq!(
            d.snapshot().marked_buckets(),
            vec![(
                seq(500) >> DEFERRED_BUCKET_SHIFT,
                ShedDepth::Dedicated as u8
            )]
        );
    }

    #[test]
    fn test_clear_overflow_through_lifts_only_once_the_lost_mark_is_covered() {
        let d = DeferredDepth::new_for_tests();
        let a = seq(100);
        let b = a + ((DEFERRED_BUCKETS as u64) << DEFERRED_BUCKET_SHIFT);
        d.note_shed(a, ShedDepth::Inline);
        d.note_shed(b, ShedDepth::Inline);
        d.clear_overflow_through(b - 1);
        assert!(d.snapshot().overflowed);
        d.clear_overflow_through(u64::MAX);
        assert!(!d.snapshot().overflowed);
    }

    #[test]
    fn test_unrecordable_total_counts_each_zero_sequence() {
        let d = DeferredDepth::new_for_tests();
        d.note_shed(0, ShedDepth::Inline);
        d.note_shed(0, ShedDepth::Dedicated);
        d.note_shed(seq(1), ShedDepth::Inline);
        assert_eq!(d.unrecordable_total(), 2);
    }
}
