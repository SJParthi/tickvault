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
//!
//! **⚠ CHANGED 2026-10-20 (plan item PR W):** the bucket table is no longer
//! the only unapplied record. A bounded set of at most 32 sequence RANGES
//! (`wal_unapplied_ranges`) holds what the table cannot: the catch-up backlog
//! guard, the boot tail, an overflow inherited from an earlier process, and
//! the buckets an earlier process left behind (folded into ranges at boot so
//! the table starts every session empty). The ranges live in a sidecar file;
//! this file stays version 1. An overflow now carries a HEIGHT in RAM, and a
//! reloaded one is bounded at the next boot instead of tainting every later
//! segment forever.
//!
//! **⚠ CORRECTED 2026-10-20 (PR W review, rollback).** The first draft set
//! this file's overflow bit whenever any range existed. The boot tail exists
//! on nearly every boot, and an older binary seeds a loaded bit at height
//! `u64::MAX`, which it can never clear: rolling back would have re-created
//! the WAL-growth bug on a clean box. Now each range is ALSO written into the
//! version-1 bucket table as the bucket ids it spans (count
//! [`UNAPPLIED_BUCKET_RANGE_MARK`]) whenever they fit in free slots, so an
//! older binary reads exact buckets it can clear. The bit is set only for
//! state an older binary cannot represent: an overflow in RAM, an overflow
//! migrated at a boot (the box was already overflowed for an older binary),
//! or a range whose buckets do not fit. The span between the highest
//! sequence an earlier process wrote and this boot's `own_floor` holds no
//! frame of any earlier process and is left out of that encoding
//! ([`AppliedSnapshot::frameless_gap`]), which is what keeps the overnight
//! boot tail down to a few buckets.
//!
//! **⚠ ADDED 2026-10-20 (PR W review round 4):**
//!
//! * A per-frame shed bucket mark carries no reorder slack, so the
//!   whole-segment check ([`AppliedSnapshot::segment_is_applied`]) now reads
//!   the bucket table from one slack below the segment's first record, and
//!   the boot fold gives each folded bucket one slack on its top. The range
//!   lookup is unchanged.
//! * **Honest limit, measured by counters, not fixed:** while a QuestDB
//!   probe is unclean (a suspended table, a latched apply lag, or a blind
//!   probe) every ack parks, and the whole stretch stays pinned until a
//!   replay reaches it; on a box whose backlog does not drain that is about
//!   one session of WAL per lag-latched day. [`WAL_SUSPECT_EPISODES_COUNTER`]
//!   and [`WAL_SUSPECT_SECONDS_COUNTER`] (local `/metrics` only, registered
//!   at 0 at boot) show how often and how long. The follow-up, not built:
//!   confirm parked live acks once a full [`WATERMARK_DURABILITY_LAG_SECS`]
//!   (60 s) and two clean probes follow them, as `settling/` does for
//!   replays.
//!
//! **⚠ ADDED 2026-10-20 (PR W review round 5), the upper side:** the same
//! reorder bound lets a segment's last record sit up to one slack ABOVE the
//! next segment's first record, so above the segment's range end. The
//! whole-segment check now also reads the bucket table up to one slack above
//! the range end ([`reorder_reach_hi`]), and the ranges built from per-frame
//! marks (the boot-folded buckets and the in-session overflow bound) carry
//! one slack on their bottom as well as their top. Cost: a segment ending
//! within one second below a marked bucket's start is held too.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use tracing::warn;

use crate::wal_unapplied_ranges::{BootFold, UnappliedRanges, UnappliedRangesFile};

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

/// 2026-10-20 (PR W review, rollback): the `mark` written beside a bucket id
/// that encodes part of a bounded RANGE rather than a shed frame. An older
/// binary reads the id as an ordinary unapplied bucket (the mark is
/// informational to it and is never 0, so its `mark_slot` leaves it alone),
/// and clears it like any other once a drain passes it. This binary drops
/// such slots when the matching sidecar is trusted (the sidecar holds the
/// exact ranges) and keeps them as ordinary buckets when it is not. ASCII
/// `TVURANGE`, a value no shed count reaches.
pub const UNAPPLIED_BUCKET_RANGE_MARK: u64 = u64::from_le_bytes(*b"TVURANGE");

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

/// Slots per sink for the queued-rescue floors (Z7, 2026-10-02). A full
/// table falls back to marking the batch's range unapplied, which only
/// replays more.
///
/// **Sized 8 → 16 by deep audit DA-12 (2026-10-12).** The old note said a
/// sink's rescue queue holds `RESCUE_QUEUE_DEPTH` (2) batches plus the one its
/// thread is writing, "so three are live at most". Since the H4 park
/// (2026-10-03) a batch the queue refuses can also be PARKED, up to
/// `PARKED_RESCUE_MAX_BATCHES` (8), and a parked batch keeps its floor. So one
/// writer can hold 2 queued + 8 parked + 1 in flight = 11 floors, more than
/// the 8 slots, and the overflow was counted on
/// [`RESCUE_FLOOR_FULL_COUNTER`] in normal operation. 16 covers the 11 with
/// room; compile-time asserts in `tick_persistence` and `depth_persistence`
/// fail the build if either sink's queue + park + 1 outgrows it. The scan is
/// still bounded (16 compare-and-swaps), on the rescue arm only.
pub const RESCUE_FLOOR_SLOTS: usize = 16;
/// Counter: a rescue hand-off found no free floor slot, so its range was
/// marked unapplied instead (it is replayed next boot even if it lands).
pub const RESCUE_FLOOR_FULL_COUNTER: &str = "tv_wal_rescue_floor_full_total";
/// Counter: QuestDB suspicion episodes the applied watermark entered (a probe
/// saw a suspended or lagging table, or could not see), once per transition
/// into suspicion (2026-10-20, PR W review round 4). Local `/metrics` only:
/// no CloudWatch filter, no EMF name, no alarm. Registered at 0 at boot.
pub const WAL_SUSPECT_EPISODES_COUNTER: &str = "tv_wal_suspect_episodes_total";
/// Counter: whole seconds the applied watermark spent suspect, counted at
/// each probe while suspect and at the clean probe that ends it (the
/// sub-second remainder is carried, never dropped). While suspect every ack
/// parks and the session's WAL stays pinned until a replay reaches it, so
/// this, read beside `tv_wal_unapplied_pinned_bytes`, is what shows whether
/// pinned bytes grow with lag-latched days. Local `/metrics` only, as above.
pub const WAL_SUSPECT_SECONDS_COUNTER: &str = "tv_wal_suspect_seconds_total";

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

/// The lowest sequence a frame of a WAL segment whose first record is
/// `first` can carry: one [`REPLAY_REORDER_SLACK_SEQ`] lower, never below 1
/// (2026-10-20, PR W review round 4). Sixteen sockets race to the writer, so
/// a record can sit up to the slack below the segment's first. A per-frame
/// mark (a shed bucket, a deferred-depth mark) carries no slack of its own,
/// so a whole-segment lookup of such marks starts here. `0` (an unreadable
/// first record) stays `0`, which every caller reads as "start unknown".
/// O(1).
#[must_use]
pub const fn reorder_reach_lo(first: u64) -> u64 {
    if first == 0 {
        0
    } else {
        let lo = first.saturating_sub(REPLAY_REORDER_SLACK_SEQ);
        if lo == 0 { 1 } else { lo }
    }
}

/// The highest sequence a frame of a WAL segment whose range ends at `hi`
/// (the next segment's first record less one) can carry: one
/// [`REPLAY_REORDER_SLACK_SEQ`] higher, saturating (2026-10-20, PR W review
/// round 5). The mirror of [`reorder_reach_lo`]: a record is never more than
/// the slack below one written before it, so a record written into a segment
/// just before the writer rotated can carry a sequence up to the slack ABOVE
/// the next segment's first record. A per-frame mark on such a record (a
/// shed bucket, a deferred-depth mark) can therefore sit in a bucket above
/// `hi`, and a whole-segment lookup of such marks ends here. O(1).
#[must_use]
pub const fn reorder_reach_hi(hi: u64) -> u64 {
    hi.saturating_add(REPLAY_REORDER_SLACK_SEQ)
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

/// `seq` rounded DOWN to a frame base (the low
/// [`crate::ws_frame_spill::PACKET_INDEX_BITS`] cleared), at least 1
/// (2026-10-20, PR W review round 4). The replay checks each frame by its
/// BASE, so a range starting inside a frame (`base + k`) would miss the
/// frame it starts in; every range [`AppliedWatermark::add_unapplied_range`]
/// adds starts at a base. O(1).
#[must_use]
pub(crate) const fn frame_floor(seq: u64) -> u64 {
    let base = seq & !crate::ws_frame_spill::MAX_PACKET_INDEX;
    if base == 0 { 1 } else { base }
}

/// Where a QuestDB suspension range starts, given the watermark it starts
/// from: TWO [`REPLAY_REORDER_SLACK_SEQ`] lower, at a frame base, at least 1.
/// O(1). Each slack has its own reason:
///
/// * (2026-10-20, PR W review round 4) a frame `f` minted up to one slack
///   before the frame that set the watermark can be acked after it, so it
///   can sit in the batch an episode lied about;
/// * (2026-10-20, PR W review round 4, pass 2) that `f` can be the LAST
///   record of a segment whose successor's first record `g` sits up to one
///   more slack below it (`g` minted earlier on another socket, written
///   after `f`, so `g ≥ f − slack`). The replay skip and the prune judge
///   that segment by `[first, g − 1]`, which can end almost two slacks below
///   the watermark; a range starting one slack below missed the segment, so
///   it read applied and `f` was skipped and could be pruned. A range
///   derived from an ACK watermark needs both slacks on its bottom; the
///   range lookup itself is not widened. **⚠ CORRECTED 2026-10-21 (PR W
///   review round 6):** the rule was applied here only; the plain
///   whole-segment skip and the boot tail are ack derived too and now carry
///   the second slack as well ([`AppliedSnapshot::segment_is_applied`],
///   [`AppliedWatermark::boot_range_floor`]).
#[must_use]
pub(crate) const fn suspension_range_lo(watermark: u64) -> u64 {
    frame_floor(watermark.saturating_sub(2 * REPLAY_REORDER_SLACK_SEQ))
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
    /// 2026-10-20: the highest sequence the bucket table FAILED to record
    /// while `overflowed`. `u64::MAX` (or `0`, which reads the same) is an
    /// unknown height: a flag loaded from disk with no matching sidecar.
    pub overflow_high_seq: u64,
    /// 2026-10-20: the bounded unapplied ranges (the sidecar's contents).
    pub ranges: UnappliedRanges,
    /// The sidecar generation these ranges were read from or written as;
    /// `0` = none durable. Written into the file's pad bytes.
    pub ranges_gen: u16,
    /// The ranges include an overflow migrated at a boot, and some range
    /// still intersects [`Self::migrated_span`]. While set, the file's
    /// overflow bit stays set for an older binary (the box was already
    /// overflowed for it).
    pub migrated_overflow: bool,
    /// 2026-10-20 (PR W review round 3): the `[lo, hi]` range the boot fold
    /// migrated the overflow into (the union, when more than one boot
    /// migrated). `migrated_overflow` clears once no range intersects it,
    /// not only once every range is gone. `None` with `migrated_overflow`
    /// set reads as `(1, u64::MAX)` (unknown: cleared only when no range is
    /// left).
    pub migrated_span: Option<(u64, u64)>,
    /// 2026-10-20 (PR W review): `[highest frame sequence on disk at the boot
    /// fold + 1, own_floor]`, a span that holds no frame of any process:
    /// every earlier process's frame is at or below the seeded sequence, and
    /// the boot fold ratchets the frame counter to `own_floor` so every frame
    /// this process mints is above it (round 3; `own_floor` itself is never
    /// minted). Left out when the ranges are encoded into the version-1
    /// bucket table for an older binary, so an overnight boot tail costs a
    /// few buckets instead of hundreds. RAM only; never read back from disk.
    pub frameless_gap: Option<(u64, u64)>,
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
            overflow_high_seq: 0,
            ranges: UnappliedRanges::new(),
            ranges_gen: 0,
            migrated_overflow: false,
            migrated_span: None,
            frameless_gap: None,
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
    /// unapplied, or when the record can no longer answer for it. Every
    /// "cannot answer" fails towards "replay it".
    ///
    /// **⚠ CORRECTED 2026-10-20 (plan item PR W).** This read "overflowed →
    /// everything is unapplied". An overflow now has a HEIGHT
    /// ([`Self::overflow_high_seq`], the top of what the bucket table failed
    /// to record), so it taints only a range that STARTS at or below it; a
    /// range wholly above the height cannot hold a frame the table lost. A
    /// flag loaded from disk with no trusted sidecar has an unknown height
    /// (`u64::MAX`) and still taints everything. A range is also unapplied
    /// when it intersects one of the bounded ranges ([`Self::ranges`]).
    #[must_use]
    pub fn range_has_unapplied(&self, lo: u64, hi: u64) -> bool {
        self.unapplied_in(lo, hi, lo, hi)
    }

    /// [`Self::range_has_unapplied`] with the bucket-table half (the
    /// per-frame bucket marks and the in-RAM overflow height) read over
    /// `[bucket_lo, bucket_hi]` instead of `[lo, hi]`, and the bounded ranges
    /// still read over `[lo, hi]` exactly (2026-10-20, PR W review round 4;
    /// `bucket_hi` added in round 5). `bucket_lo` is at or below `lo` and
    /// `bucket_hi` at or above `hi` (both are clamped to that). O(1) plus at
    /// most [`UNAPPLIED_BUCKETS`] slot reads.
    fn unapplied_in(&self, lo: u64, hi: u64, bucket_lo: u64, bucket_hi: u64) -> bool {
        if lo > hi {
            return true;
        }
        let bucket_lo = bucket_lo.min(lo);
        let bucket_hi = bucket_hi.max(hi);
        if self.overflowed {
            let height = if self.overflow_high_seq == 0 {
                u64::MAX
            } else {
                self.overflow_high_seq
            };
            if bucket_lo <= height {
                return true;
            }
        }
        if self.ranges.intersects(lo, hi) {
            return true;
        }
        // Bucket id 0 is the empty-slot marker, so a widened low end never
        // reaches it from a range that starts above it.
        let first = match bucket_lo >> UNAPPLIED_BUCKET_SHIFT {
            0 if lo >> UNAPPLIED_BUCKET_SHIFT != 0 => 1,
            id => id,
        };
        let last = bucket_hi >> UNAPPLIED_BUCKET_SHIFT;
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

    /// `true` when a WAL segment whose first record is `first` and whose
    /// range ends at `hi` (the next segment's first less one) is applied for
    /// BOTH sinks: a segment mixes main-feed and depth frames, and a
    /// main-feed frame feeds both. The one decision the boot replay's
    /// whole-segment skip and the prune share (2026-10-20, PR W review
    /// round 3). O(1) plus two range lookups.
    ///
    /// The ring reorders frames from sixteen sockets, so a segment can hold a
    /// frame up to one reorder slack BELOW its first record. Two kinds of
    /// mark cover such a frame differently:
    ///
    /// * **Ranges** over particular frames of THIS process carry one slack
    ///   on their TOP (both suspension-episode marks, the in-session
    ///   overflow bound, and since round 4 every bucket the boot fold turns
    ///   into a range), so they reach the first record of any segment such a
    ///   frame can sit in. The range lookup here stays at `[first, hi]`. A
    ///   lower margin on the range lookup was tried and rejected on
    ///   measurement: the lifetime property tests showed it keeps the
    ///   segment just above EVERY range end (a clean session's first segment
    ///   after the boot tail read unapplied, check G; one more segment per
    ///   range stayed pinned on a backlog that never drains, check M3),
    ///   which is the WAL growth this module closes.
    /// * **Ranges derived from an ACK watermark** (both suspension-episode
    ///   marks) need TWO slacks on their bottom, not one (2026-10-20, PR W
    ///   review round 4, pass 2). A frame `f` acked after the last clean
    ///   probe can sit up to one slack below that watermark, and be the
    ///   LAST record of a segment whose successor's first record sits up to
    ///   one more slack below `f`. This segment's `hi` (that successor's
    ///   first less one) then ends almost two slacks below the watermark,
    ///   so a range starting one slack below it missed the segment and `f`
    ///   was skipped. `suspension_range_lo` gives both marks the second
    ///   slack; this lookup is unchanged.
    /// * **Per-frame bucket marks** (`note_unapplied`, bucket = `seq >> 38`)
    ///   and the in-RAM overflow height carry NO slack: a frame shed just
    ///   below a bucket boundary marks only the bucket below it.
    ///   **⚠ CORRECTED 2026-10-20 (PR W review round 4):** this doc said
    ///   every per-frame mark was covered by the slack on range tops; the
    ///   bucket marks are not ranges and never were. A frame `s` shed just
    ///   below a `2^38` boundary, inside a segment whose first record `b` is
    ///   just above it (`b − s` under one second, reordered across sockets),
    ///   made that segment read applied: skipped by replay and prunable. The
    ///   bucket-table half is therefore read from `first −
    ///   REPLAY_REORDER_SLACK_SEQ` (saturating). It costs at most one extra
    ///   bucket lookup, and holds a segment only when a marked bucket ends
    ///   within one second below its first record.
    /// * **The upper side (2026-10-20, PR W review round 5).** The same
    ///   reorder bound lets a record of this segment, written just before the
    ///   writer rotated, carry a sequence up to one slack ABOVE the next
    ///   segment's first record, so above `hi`. A frame `f` shed just above
    ///   a `2^38` boundary, written as this segment's last record while the
    ///   next segment's first record `g` sits just below the boundary
    ///   (minted earlier on another socket, written later), marked only the
    ///   bucket above `hi`: this segment read applied, was skipped by replay
    ///   and could be pruned. The bucket-table half is therefore also read
    ///   up to `hi + REPLAY_REORDER_SLACK_SEQ` ([`reorder_reach_hi`]), and
    ///   the ranges built from per-frame marks (the boot-folded buckets and
    ///   the in-session overflow bound) carry one slack on their BOTTOM as
    ///   well as their top. Cost: a segment ending within one second below a
    ///   marked bucket's start is held as well.
    /// * **The plain ack watermark needs the second slack too (2026-10-21,
    ///   PR W review round 6).** Round 4, pass 2 gave the second slack only
    ///   to the suspension ranges; the shape is the same for the watermark
    ///   itself. Acks follow write order within a sink, so a frame `f` above
    ///   `skip_below` (the watermark `H` less one slack) can still be
    ///   unacked when `H` is acked and durable (its flush stuck at a QuestDB
    ///   stall and the process killed, or acked but not yet durable at a
    ///   power cut). `f` can be the LAST record of a segment whose
    ///   successor's first record `g ≥ f − slack` (minted earlier, written
    ///   after `f`), so that segment's `hi = g − 1` can sit up to almost two
    ///   slacks below `H`: under `skip_below`, and below the boot tail,
    ///   which starts at `skip_below + 1`. The segment read applied, the
    ///   replay archived it unread and the prune could delete it with `f`
    ///   unconfirmed. A whole segment is therefore applied only when `hi`
    ///   sits more than one slack below `skip_below` for both sinks
    ///   ([`Self::below_watermarks_by_a_slack`]). The per-frame check
    ///   ([`Self::frame_is_applied`]) keeps one slack: for a single frame it
    ///   is enough. Cost: at most one more segment per session kept or read
    ///   (the one ending within a slack below `skip_below`).
    #[must_use]
    pub fn segment_is_applied(&self, first: u64, hi: u64) -> bool {
        self.range_is_applied(AppliedSink::Ticks, first, hi)
            && self.range_is_applied(AppliedSink::Depth, first, hi)
            && self.below_watermarks_by_a_slack(hi)
            && !self.unapplied_in(first, hi, reorder_reach_lo(first), reorder_reach_hi(hi))
    }

    /// `true` when `hi` plus one [`REPLAY_REORDER_SLACK_SEQ`] is strictly
    /// below [`Self::skip_below`] for both sinks: the second slack a whole
    /// segment needs below an ack watermark (2026-10-21, PR W review round
    /// 6; see [`Self::segment_is_applied`]). Strict, because a frame AT
    /// `skip_below + 1` is unacked by the per-frame rule and its successor's
    /// first record can then sit at `skip_below + 1 − slack`. O(1).
    fn below_watermarks_by_a_slack(&self, hi: u64) -> bool {
        let reach = hi.saturating_add(REPLAY_REORDER_SLACK_SEQ);
        reach < self.skip_below(AppliedSink::Ticks) && reach < self.skip_below(AppliedSink::Depth)
    }

    fn to_bytes(&self) -> [u8; APPLIED_WATERMARK_LEN] {
        let mut out = [0u8; APPLIED_WATERMARK_LEN];
        out[0..4].copy_from_slice(&APPLIED_MAGIC);
        out[4] = APPLIED_VERSION;
        let mut flags = 0u8;
        if self.depth_untracked {
            flags |= FLAG_DEPTH_UNTRACKED;
        }
        // 2026-10-20 (PR W review, rollback): the ranges are written into the
        // bucket table for an older binary when they fit, and the bit is set
        // only for what that table cannot represent (see `v1_buckets`). The
        // pad bytes name the sidecar generation that holds the exact ranges;
        // an in-RAM overflow writes 0 there, which loads as unbounded.
        let (buckets, overflow_bit) = self.v1_buckets();
        if overflow_bit {
            flags |= FLAG_OVERFLOWED;
        }
        out[5] = flags;
        let generation = if !self.overflowed && !self.ranges.is_empty() {
            self.ranges_gen
        } else {
            0
        };
        out[6..8].copy_from_slice(&generation.to_le_bytes());
        out[8..16].copy_from_slice(&self.hwm_ticks.to_le_bytes());
        out[16..24].copy_from_slice(&self.hwm_depth.to_le_bytes());
        out[24..32].copy_from_slice(&self.persisted_at_nanos.to_le_bytes());
        out[32..40].copy_from_slice(&self.dir_tag.to_le_bytes());
        // O(1) EXEMPT: fixed UNAPPLIED_BUCKETS iterations, cold persist path
        for (i, (id, count)) in buckets.iter().enumerate() {
            let at = HEADER_LEN + i * 16;
            out[at..at + 8].copy_from_slice(&id.to_le_bytes());
            out[at + 8..at + 16].copy_from_slice(&count.to_le_bytes());
        }
        let crc = crc32_ieee(&out[..HEADER_LEN + BODY_LEN]);
        out[HEADER_LEN + BODY_LEN..].copy_from_slice(&crc.to_le_bytes());
        out
    }

    /// The bucket table as written to disk, and whether the overflow bit is
    /// set (2026-10-20, PR W review, rollback).
    ///
    /// An older binary reads only this table and the bit, and seeds a set
    /// bit at height `u64::MAX`, which it can never clear. So every range is
    /// written here as the bucket ids it spans (mark
    /// [`UNAPPLIED_BUCKET_RANGE_MARK`]) whenever each id lands on a free slot
    /// or on a slot already holding that id, and the bit is set only when:
    ///
    /// * the bucket table itself overflowed in RAM (the old meaning);
    /// * the ranges include an overflow migrated at a boot (the box was
    ///   already overflowed for an older binary; nothing narrower is true);
    /// * a range does not fit (it spans 128 buckets or more, touches bucket
    ///   id 0, or lands on a slot holding a different id).
    ///
    /// **Honest limit (2026-10-20, PR W review round 3):** the table is
    /// indexed modulo 128, so on a shed-heavy day, when this session's shed
    /// buckets already fill many slots, a range id is likely to land on a
    /// slot holding a different id and the bit is then set for that persist.
    /// Nothing is lost (this binary reads the sidecar; an older binary
    /// replays everything after a rollback that day), and the bit goes again
    /// once a drain clears the sheds. A range with no file to start from (no
    /// or a rejected `applied.tvaw` at the boot, so the tail starts at 1 or
    /// at the oldest waiting segment) usually spans far more than 128
    /// buckets and also sets the bit until a trim or a drained reset narrows
    /// it.
    ///
    /// The [`Self::frameless_gap`] is left out of every range first: no
    /// earlier process wrote a frame there, and an older binary never
    /// protected it. When the bit is set the real table is written unchanged.
    /// O(32 ranges × 2 pieces × at most 128 ids), cold: once per persist.
    ///
    /// **Honest limit (2026-10-20, PR W review round 4): the ids planted here
    /// raise an older binary's own odds of overflowing.** It has no boot
    /// fold and clears an id only on a drained reset. On a box whose backlog
    /// never drains, ranges from many earlier days stay held and their ids
    /// sit in slots across those days, so after a rollback the first shed
    /// of the older binary's session that lands on a slot holding a range id
    /// (or an island of an earlier day) with a different id overflows its
    /// table, and it reloads that as the unbounded bit: the original
    /// next-day collision, made more likely by these ids. Nothing is lost
    /// (it replays more), but the rollback cost arrives later through a
    /// shed rather than in this encoding. Leaving old ranges out of the
    /// encoding and setting the bit for them was considered and not taken:
    /// it hands the older binary the unbounded bit at once on every such
    /// box, which is the same cost, earlier and certain.
    fn v1_buckets(&self) -> ([(u64, u64); UNAPPLIED_BUCKETS], bool) {
        if self.overflowed {
            return (self.buckets, true);
        }
        if self.ranges.is_empty() {
            return (self.buckets, false);
        }
        if self.migrated_overflow {
            return (self.buckets, true);
        }
        let mut out = self.buckets;
        // O(1) EXEMPT: bounded by UNAPPLIED_RANGE_SLOTS (32), cold persist
        for &(lo, hi) in self.ranges.as_slice() {
            let (left, right) = split_around_gap(lo, hi, self.frameless_gap);
            // O(1) EXEMPT: at most two pieces per range
            for (a, b) in [left, right].into_iter().flatten() {
                if !encode_range_buckets(&mut out, a, b) {
                    return (self.buckets, true);
                }
            }
        }
        (out, false)
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
        let ranges_gen = u16::from_le_bytes(bytes[6..8].try_into().ok()?);
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
        let overflowed = flags & FLAG_OVERFLOWED != 0;
        // This is exactly what an older binary decodes: the bit, no height.
        // `load` replaces it with the sidecar's ranges when they match.
        Some(Self {
            hwm_ticks,
            hwm_depth,
            depth_untracked: flags & FLAG_DEPTH_UNTRACKED != 0,
            overflowed,
            persisted_at_nanos,
            dir_tag,
            buckets,
            overflow_high_seq: if overflowed { u64::MAX } else { 0 },
            ranges: UnappliedRanges::new(),
            ranges_gen,
            migrated_overflow: false,
            migrated_span: None,
            frameless_gap: None,
        })
    }

    /// Reads and validates `<wal_dir>/applied.tvaw`. Absent → `None`, silently
    /// (a first boot has no file). Present-but-rejected → `None` and the
    /// invalid counter, so a corrupt watermark is visible rather than a
    /// mystery full replay.
    #[must_use]
    pub fn load(wal_dir: &Path) -> Option<Self> {
        Self::load_quiet(wal_dir)
            .inspect_err(|len| {
                metrics::counter!(APPLIED_INVALID_COUNTER).increment(1);
                warn!(
                    path = %wal_dir.join(APPLIED_WATERMARK_FILE).display(),
                    len,
                    "WAL applied-watermark file rejected (short, wrong magic/version, bad crc, \
                     implausible values, or written beside a DIFFERENT directory) — ignored; \
                     this boot replays the WAL in full"
                );
            })
            .ok()
            .flatten()
    }

    /// S3 (2026-10-02): the highest `capture_seq` the file beside `wal_dir`
    /// vouches for as applied, on either sink; `0` when there is no file or
    /// it is rejected (a rejected file vouches for nothing, so no new
    /// sequence can read as applied through it). Quiet: [`Self::load`] at the
    /// bind reports a rejected file once.
    ///
    /// The WAL sequence is seeded past this as well as past the segments on
    /// disk: once every segment is pruned, a backward clock step of less than
    /// [`APPLIED_MAX_FUTURE_NANOS`] would otherwise mint sequences at or below
    /// a watermark that still loads, and their segments would read as
    /// applied — skipped on replay and deletable.
    #[must_use]
    pub fn persisted_high_water(wal_dir: &Path) -> u64 {
        Self::load_quiet(wal_dir)
            .ok()
            .flatten()
            .map_or(0, |snap| snap.hwm_ticks.max(snap.hwm_depth))
    }

    /// [`Self::load`] without the report. `Ok(None)` when absent, `Err(len)`
    /// when present but rejected.
    fn load_quiet(wal_dir: &Path) -> Result<Option<Self>, usize> {
        let path = wal_dir.join(APPLIED_WATERMARK_FILE);
        // APPROVED: boot-time load, cold path
        let Ok(bytes) = std::fs::read(&path) else {
            return Ok(None);
        };
        let expected_tag = dir_tag_of(wal_dir);
        match Self::from_bytes(&bytes, wall_nanos()) {
            Some(mut snap) if snap.dir_tag == expected_tag => {
                snap.attach_ranges_sidecar(wal_dir);
                Ok(Some(snap))
            }
            _ => Err(bytes.len()),
        }
    }

    /// 2026-10-20: reads the ranges sidecar beside `wal_dir` into a snapshot
    /// just decoded from `applied.tvaw`. The sidecar is TRUSTED only when it
    /// is valid, belongs to this directory, holds at least one range, and
    /// carries the generation written into this file's pad bytes. Then its
    /// ranges replace the range-encoded buckets (they are the same ranges,
    /// exact rather than bucket-wide) and explain any overflow bit.
    ///
    /// Anything else (no sidecar, a corrupt one, a stale one from a crash
    /// between the two writes, a file written by an older binary with zero
    /// pad) keeps the bit unbounded and turns every range-encoded bucket into
    /// an ordinary unapplied bucket, which the boot fold then folds into its
    /// exact bucket span: coverage never shrinks. A readable but untrusted
    /// sidecar still adds its ranges (more replay, never less).
    ///
    /// **⚠ CHANGED 2026-10-20 (PR W review, rollback):** this read the
    /// sidecar only when the bit was set. The bit is now set only for what an
    /// older binary cannot represent, so a non-zero pad alone also asks for
    /// it. O(`UNAPPLIED_BUCKETS`) plus one small file read, cold.
    fn attach_ranges_sidecar(&mut self, wal_dir: &Path) {
        let pad = self.ranges_gen;
        self.ranges_gen = 0;
        let side = if pad != 0 || self.overflowed {
            UnappliedRangesFile::load(wal_dir)
        } else {
            None
        };
        let trusted = pad != 0 && side.is_some_and(|s| s.generation == pad && !s.ranges.is_empty());
        if let Some(side) = side {
            self.ranges = side.ranges;
            self.migrated_overflow = side.migrated_overflow;
            self.migrated_span = side.migrated_span;
        }
        // O(1) EXEMPT: fixed UNAPPLIED_BUCKETS iterations, cold load
        for slot in &mut self.buckets {
            if slot.0 != 0 && slot.1 == UNAPPLIED_BUCKET_RANGE_MARK {
                *slot = if trusted { (0, 0) } else { (slot.0, 1) };
            }
        }
        if trusted {
            self.overflowed = false;
            self.overflow_high_seq = 0;
            self.ranges_gen = pad;
        }
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

/// `[lo, hi]` with the inclusive `gap` removed: up to two pieces, below and
/// above it. O(1).
fn split_around_gap(
    lo: u64,
    hi: u64,
    gap: Option<(u64, u64)>,
) -> (Option<(u64, u64)>, Option<(u64, u64)>) {
    let Some((g_lo, g_hi)) = gap.filter(|&(g_lo, g_hi)| g_lo <= g_hi) else {
        return (Some((lo, hi)), None);
    };
    if hi < g_lo || lo > g_hi {
        return (Some((lo, hi)), None);
    }
    let left = if lo < g_lo {
        Some((lo, g_lo - 1))
    } else {
        None
    };
    let right = if hi > g_hi {
        Some((g_hi + 1, hi))
    } else {
        None
    };
    (left, right)
}

/// Writes every bucket id `[lo, hi]` spans into `table` with the range mark,
/// leaving a slot that already holds the same id as it is. `false` (and a
/// partly written `table` the caller discards) when the span is 128 buckets
/// or wider, touches id 0 (an empty slot), or lands on a slot holding a
/// different id. O(at most 128), cold: the persist encoding only.
fn encode_range_buckets(table: &mut [(u64, u64); UNAPPLIED_BUCKETS], lo: u64, hi: u64) -> bool {
    let first = lo >> UNAPPLIED_BUCKET_SHIFT;
    let last = hi >> UNAPPLIED_BUCKET_SHIFT;
    if first == 0 || last < first || last - first >= UNAPPLIED_BUCKETS as u64 {
        return false;
    }
    let mut id = first;
    // O(1) EXEMPT: bounded by UNAPPLIED_BUCKETS (128), cold persist encoding
    loop {
        let slot = &mut table[slot_of(id)];
        if slot.0 == 0 {
            *slot = (id, UNAPPLIED_BUCKET_RANGE_MARK);
        } else if slot.0 != id {
            return false;
        }
        if id == last {
            return true;
        }
        id += 1;
    }
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
    /// Conservative LOWER bound on the sequences the bucket table failed to
    /// record (`u64::MAX` = none recorded). 2026-10-20 (PR W review round 4):
    /// round 3 removed the first version of this field because its only
    /// reader was the boot fold; its reader is now the persist, which turns
    /// an in-session overflow of known height into the bounded range
    /// `[low, height]` instead of writing the bare bit (see
    /// `bound_known_overflow_locked`). Written only on the two overflow arms
    /// (`fetch_min`, before the flag is set), never on a shed that fits.
    overflow_low_seq: AtomicU64,
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
    /// Serialises the suspect-flag check with the watermark or parked-ack
    /// update in [`Self::note_ticks_acked`] / [`Self::note_depth_acked`],
    /// and the flag flip with the watermark and parked-ack reads at both
    /// probe transitions (2026-10-21, PR W review round 6). With only the
    /// atomics, an ack that loaded the flag on one side of a transition and
    /// wrote on the other was in neither episode range: at the entry it
    /// raised the watermark after the range read it, at the exit it parked
    /// into a slot already drained. The bucket table this replaced marked
    /// whole 4.6-minute buckets, which covered both. Taken once per ack
    /// BATCH (never per tick) and once per probe transition, held for a
    /// flag load and one `fetch_max` (or a swap and four loads), never
    /// across a file write. No allocation.
    ack_gate: Mutex<()>,
    /// Set while persisting the snapshot keeps failing, so a failing disk is
    /// reported once per episode and not once a second (2026-10-09). The
    /// counter still counts every failure; `durability-sync-failing` pages on it.
    persist_failing: AtomicBool,
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
    /// Suspicion episodes this process (audit DA-13b, DA-13c). Monotone;
    /// bumped once per transition into suspicion. The tick and depth spill
    /// replays stamp it with each ack and refuse to empty a spill file whose
    /// acks an episode may have lied about, even when the episode began and
    /// ended between two of their rounds; the WAL replay settle step records
    /// it at an ack and refuses to archive the acked segments if it has moved
    /// since.
    suspect_episodes: AtomicU64,
    /// Monotonic instant up to which suspect time has been counted on
    /// [`WAL_SUSPECT_SECONDS_COUNTER`] (2026-10-20, PR W review round 4).
    suspect_accounted_nanos: AtomicU64,
    /// Whole seconds counted on [`WAL_SUSPECT_SECONDS_COUNTER`] so far.
    suspect_seconds: AtomicU64,
    /// Where the LATEST suspicion episode began (audit DA-13b review): its
    /// number (0 while being rewritten), the clean-probe count and the wall
    /// clock at its start. A spill replay round that runs after an episode
    /// began and ended between two of its rounds uses it to tell acks that
    /// were already confirmed BEFORE the episode (settled, never re-sent)
    /// from acks the episode may have lied about. A seqlock with one writer
    /// (the WAL-suspension watcher): the tag is cleared, the fields written,
    /// then the tag set, and a reader that sees two different tags around
    /// its reads discards them.
    episode_start_tag: AtomicU64,
    episode_start_probes: AtomicU64,
    episode_start_secs: AtomicU64,
    /// DA-13a review: set once, before the seal writer starts, by a process
    /// that WILL run the WAL-suspension watcher. Until the watcher's first
    /// verdict (an HTTP round trip after it spawns) `clean_probes` is 0 and
    /// `sink_suspect` is false, which reads exactly like "no watcher at all";
    /// this flag tells the seal replay to wait for the watcher's confirmation
    /// instead of archiving on a bare 2xx in that window.
    probe_expected: AtomicBool,
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
    /// 2026-10-20 (plan item PR W): the bounded unapplied ranges, the sidecar
    /// generation that holds them durably, and the migrated flag. Touched
    /// only on cold paths (boot, the backlog guard, a trim, the drained reset,
    /// a QuestDB suspension episode, the settle recovery, the once-a-second
    /// persist); never by a shed or an ack.
    ranges: Mutex<RangesState>,
    /// `(sidecar, its tmp, the directory)`; set by the first [`Self::bind`].
    ranges_paths: OnceLock<(PathBuf, PathBuf, PathBuf)>,
    /// The slack-adjusted watermark loaded at the first [`Self::bind`]
    /// (`0` with no file), frozen there: every frame at or below it was
    /// applied by an earlier process unless a bucket, a range or an overflow
    /// says otherwise. Later acks never move it.
    boot_applied_floor: AtomicU64,
    /// The `own_floor` of the boot fold (`0` before it runs): every frame an
    /// earlier process wrote sits at or below it, and (round 3) every frame
    /// this process mints is above it, because the fold wrapper ratchets the
    /// frame counter to it first.
    boot_own_floor: AtomicU64,
    /// 2026-10-20 (PR W review round 3): the seeded frame sequence at the
    /// boot fold (`0` before it runs or with nothing on disk). Every frame
    /// of an earlier process is at or below it.
    boot_frame_high: AtomicU64,
    /// The boot fold runs once per process.
    boot_folded: AtomicBool,
    /// 2026-10-20 (PR W review): [`AppliedSnapshot::frameless_gap`], set by
    /// the boot fold; `(0, 0)` = none.
    boot_gap_lo: AtomicU64,
    boot_gap_hi: AtomicU64,
    /// A failing sidecar write is reported once per episode.
    ranges_failing: AtomicBool,
}

/// The ranges and how durable they are. One per watermark, under its mutex.
#[derive(Debug, Clone, Copy)]
struct RangesState {
    ranges: UnappliedRanges,
    /// 2026-10-20 (PR W review round 3): the range an overflow was migrated
    /// into at a boot (the union when several boots migrated), or `None`.
    /// Cleared once no range intersects it ([`Self::settle_migrated`]); it
    /// was a bare flag cleared only when the whole set was empty, so a box
    /// that never fully drains kept an older binary's overflow bit set for
    /// ever.
    migrated_span: Option<(u64, u64)>,
    /// Generation of the sidecar on disk that holds exactly `ranges`; `0`
    /// while the RAM ranges are not yet durable (the persist then writes the
    /// file's pad as `0`, which loads as an unbounded overflow).
    durable_gen: u16,
    /// The next generation to write; `0` = derive one from the clock.
    next_gen: u16,
}

impl RangesState {
    const fn new() -> Self {
        Self {
            ranges: UnappliedRanges::new(),
            migrated_span: None,
            durable_gen: 0,
            next_gen: 0,
        }
    }

    /// Adds `span` to the migrated span (the union of the two). O(1).
    fn widen_migrated(&mut self, span: (u64, u64)) {
        self.migrated_span = Some(match self.migrated_span {
            Some((a, b)) => (a.min(span.0), b.max(span.1)),
            None => span,
        });
    }

    /// Clears the migrated span once no range intersects it. Returns whether
    /// it changed. O(32).
    fn settle_migrated(&mut self) -> bool {
        match self.migrated_span {
            Some((a, b)) if !self.ranges.intersects(a, b) => {
                self.migrated_span = None;
                true
            }
            _ => false,
        }
    }
}

impl Default for AppliedWatermark {
    fn default() -> Self {
        Self::new()
    }
}

static APPLIED: AppliedWatermark = AppliedWatermark::new();

/// The one process-global watermark.
#[must_use]
pub fn applied_watermark() -> &'static AppliedWatermark {
    &APPLIED
}

/// `fate` of a tick or depth batch whose spill rescue FAILED, when the batch
/// carries a capture-sequence range the WAL accepted AND the WAL writer had
/// already flushed every frame of that range to the kernel: the range is
/// marked unapplied, so the next boot's WAL replay re-inserts it, unless the
/// host crashes before the WAL's device sync.
pub(crate) const RESCUE_FATE_DEFERRED_TO_WAL_REPLAY: &str = "deferred_to_wal_replay";

/// `fate` of a failed spill rescue whose range the WAL accepted but had not
/// yet flushed when the rescue failed. The range is marked unapplied; the
/// next boot's replay re-inserts it only if the WAL writer then flushed those
/// frames. A rise in `tv_ws_frame_spill_unflushed_lost_total` in the same
/// window means some were not, and those rows are gone.
pub(crate) const RESCUE_FATE_DEFERRED_IF_WAL_FLUSHES: &str = "deferred_if_wal_flushes";

/// `fate` of a failed spill rescue whose rows no WAL record backs (no capture
/// sequence, or a frame the WAL refused): they are gone.
pub(crate) const RESCUE_FATE_LOST: &str = "lost";

/// What happens to rows whose spill rescue failed (audit DB-8, 2026-10-11),
/// judged against the WAL writer's flushed high-water mark now.
///
/// Until this date every failed-rescue log line said the rows "are
/// permanently lost and nothing re-inserts them". That is false for a batch
/// with a WAL-backed sequence range: `note_rescue_outcome_*` marks the range
/// unapplied and the next boot's replay re-inserts it. It is true only for
/// rows with no sequence (`min_seq == 0`) or rows the WAL refused
/// (`unbacked`). Shared by the tick and depth writers so the two cannot
/// drift. O(1), no allocation: one relaxed atomic load.
///
/// **⚠ CORRECTED 2026-10-11 (review of DB-8, same day).** The first version
/// returned `deferred_to_wal_replay` for every backed range and the log line
/// said outright that the replay "re-inserts them". "Backed" only means the
/// WAL took the frame into its queue; its writer can still lose it (a failed
/// buffered write or flush, counted on
/// `tv_ws_frame_spill_unflushed_lost_total`), and a spill rescue usually
/// fails because the disk the WAL shares is full or failing. So a range the
/// WAL has not flushed yet now reads `deferred_if_wal_flushes`.
pub(crate) fn rescue_failure_fate(min_seq: u64, max_seq: u64, unbacked: bool) -> &'static str {
    rescue_failure_fate_at(
        min_seq,
        max_seq,
        unbacked,
        crate::wal_frame_fate::flushed_high_seq(),
    )
}

/// [`rescue_failure_fate`] against a given WAL flushed high-water mark.
/// Pure; compares frame bases (the sequence with its packet index dropped),
/// because the WAL records one sequence per frame and a row's sequence adds
/// its packet index to it.
pub(crate) const fn rescue_failure_fate_at(
    min_seq: u64,
    max_seq: u64,
    unbacked: bool,
    wal_flushed_high_seq: u64,
) -> &'static str {
    if min_seq == 0 || unbacked {
        RESCUE_FATE_LOST
    } else if (max_seq >> crate::ws_frame_spill::PACKET_INDEX_BITS)
        <= (wal_flushed_high_seq >> crate::ws_frame_spill::PACKET_INDEX_BITS)
    {
        RESCUE_FATE_DEFERRED_TO_WAL_REPLAY
    } else {
        RESCUE_FATE_DEFERRED_IF_WAL_FLUSHES
    }
}

impl AppliedWatermark {
    /// A fresh, unbound instance for tests that must not share the global.
    #[cfg(test)]
    pub(crate) const fn new_for_tests() -> Self {
        Self::new()
    }

    /// A fresh, unbound watermark. Production has exactly one, the
    /// process-global [`applied_watermark`]; public since 2026-10-20 so the
    /// integration proptest can model whole process lifetimes on its own
    /// instances without sharing the global. Hidden from the docs (PR W
    /// review round 4): public only for
    /// `tests/wal_unapplied_ranges_properties.rs`.
    #[doc(hidden)]
    #[must_use]
    pub const fn new() -> Self {
        Self {
            hwm_ticks: AtomicU64::new(0),
            hwm_depth: AtomicU64::new(0),
            depth_untracked: AtomicBool::new(false),
            overflowed: AtomicBool::new(false),
            overflow_high_seq: AtomicU64::new(0),
            overflow_low_seq: AtomicU64::new(u64::MAX),
            bucket_ids: [const { AtomicU64::new(0) }; UNAPPLIED_BUCKETS],
            bucket_counts: [const { AtomicU64::new(0) }; UNAPPLIED_BUCKETS],
            ticks_handed_off: AtomicU64::new(0),
            ticks_completed: AtomicU64::new(0),
            depth_handed_off: AtomicU64::new(0),
            depth_completed: AtomicU64::new(0),
            last_persist_nanos: AtomicU64::new(0),
            paths: OnceLock::new(),
            persist_lock: Mutex::new(DurabilityLag::new()),
            ack_gate: Mutex::new(()),
            dir_tag: AtomicU64::new(0),
            sink_suspect: AtomicBool::new(false),
            persist_failing: AtomicBool::new(false),
            healthy_ticks: AtomicU64::new(0),
            healthy_depth: AtomicU64::new(0),
            suspect_max_ticks: AtomicU64::new(0),
            suspect_max_depth: AtomicU64::new(0),
            clean_probes: AtomicU64::new(0),
            suspect_episodes: AtomicU64::new(0),
            suspect_accounted_nanos: AtomicU64::new(0),
            suspect_seconds: AtomicU64::new(0),
            episode_start_tag: AtomicU64::new(0),
            episode_start_probes: AtomicU64::new(0),
            episode_start_secs: AtomicU64::new(0),
            probe_expected: AtomicBool::new(false),
            unlanded: AtomicU64::new(0),
            rescue_floors_ticks: [const { AtomicU64::new(0) }; RESCUE_FLOOR_SLOTS],
            rescue_floors_depth: [const { AtomicU64::new(0) }; RESCUE_FLOOR_SLOTS],
            ranges: Mutex::new(RangesState::new()),
            ranges_paths: OnceLock::new(),
            boot_applied_floor: AtomicU64::new(0),
            boot_own_floor: AtomicU64::new(0),
            boot_frame_high: AtomicU64::new(0),
            boot_folded: AtomicBool::new(false),
            boot_gap_lo: AtomicU64::new(0),
            boot_gap_hi: AtomicU64::new(0),
            ranges_failing: AtomicBool::new(false),
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
            let ranges_paths_set = self
                .ranges_paths
                .set((
                    wal_dir.join(crate::wal_unapplied_ranges::UNAPPLIED_RANGES_FILE),
                    wal_dir.join(crate::wal_unapplied_ranges::UNAPPLIED_RANGES_TMP),
                    wal_dir.to_path_buf(),
                ))
                .is_ok();
            let loaded = AppliedSnapshot::load(wal_dir);
            // 2026-10-20: frozen HERE, from the file, before any sink exists.
            if ranges_paths_set {
                self.boot_applied_floor
                    .store(loaded.as_ref().map_or(0, boot_floor_of), Ordering::Release);
            }
            if let Some(snap) = loaded {
                self.seed(&snap);
            }
        }
    }

    /// Whether [`Self::bind`] bound this watermark to `wal_dir`. Two
    /// canonicalisations, cold (boot fold, trims).
    #[must_use]
    pub(crate) fn is_bound_to(&self, wal_dir: &Path) -> bool {
        let tag = self.dir_tag.load(Ordering::Acquire);
        tag != 0 && tag == dir_tag_of(wal_dir)
    }

    /// The slack-adjusted applied watermark frozen at the first bind (`0`
    /// with no file): `min(hwm_ticks, hwm_depth)` less the reorder slack,
    /// whatever the file's `depth_untracked` flag says (round 3, M2). The
    /// backlog guard never marks below `floor + 1`.
    #[must_use]
    pub fn boot_applied_floor(&self) -> u64 {
        self.boot_applied_floor.load(Ordering::Acquire)
    }

    /// Where a range over the frames the boot file never vouched for starts
    /// (the boot tail, the backlog guard): [`Self::boot_applied_floor`] less
    /// one MORE [`REPLAY_REORDER_SLACK_SEQ`], at a frame base, at least 1
    /// (2026-10-21, PR W review round 6). The floor is an ACK watermark less
    /// one slack, and a range derived from an ack watermark needs two (see
    /// [`suspension_range_lo`]): an unacked frame just above the floor can be
    /// the LAST record of a segment whose successor's first record sits up
    /// to one slack below it, so that segment's judged range `[first,
    /// successor first − 1]` ends below `floor + 1`. A tail starting at
    /// `floor + 1` missed it; once this session's acks lifted the watermark
    /// past it, the next replay skipped the segment unread (found by the
    /// lifetime property test's plain-watermark straddle). Cost: up to one
    /// more second of frames, so at most one more segment, per boot. O(1).
    #[must_use]
    pub(crate) fn boot_range_floor(&self) -> u64 {
        frame_floor(
            self.boot_applied_floor()
                .saturating_sub(REPLAY_REORDER_SLACK_SEQ),
        )
    }

    /// Loads a snapshot into RAM (monotone on the watermarks, replace on the
    /// buckets, union on the ranges).
    ///
    /// **⚠ CORRECTED 2026-10-20 (plan item PR W).** A seeded overflow flag
    /// is still stored at height `u64::MAX` (nothing on disk names a
    /// height), but it is no longer "never clearable by this process": the
    /// boot fold ([`Self::fold_at_boot_with_spans`]) bounds it as the range
    /// `[oldest waiting segment − REPLAY_REORDER_SLACK_SEQ, own_floor]` (from
    /// 1 when the oldest is unknown) before any sink exists, and a drain or a
    /// trim past that range clears it. Before, it tainted every segment
    /// written after it, so the prune could never delete anything again.
    /// The sidecar's ranges are added (a union) and its migrated span is
    /// widened by the loaded one (round 3, M4: a migrated flag with no span
    /// reads as `(1, u64::MAX)`).
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
            // costs time; the alternative costs data. The boot fold then
            // bounds it (see the 2026-10-20 note above).
            self.overflow_high_seq.store(u64::MAX, Ordering::Release);
            self.overflowed.store(true, Ordering::Release);
        }
        if !snap.ranges.is_empty() || snap.migrated_overflow {
            let mut state = self.lock_ranges();
            let before = state.ranges;
            // O(1) EXEMPT: bounded by UNAPPLIED_RANGE_SLOTS (32), boot-time seed
            for &(lo, hi) in snap.ranges.as_slice() {
                state.ranges.add_range(lo, hi);
            }
            if snap.migrated_overflow {
                state.widen_migrated(snap.migrated_span.unwrap_or((1, u64::MAX)));
            }
            if snap.ranges_gen != 0 {
                state.next_gen = next_generation(snap.ranges_gen);
            }
            // Durable as-is only when the RAM set IS the sidecar it came from.
            if before.is_empty() && snap.ranges_gen != 0 && state.ranges == snap.ranges {
                state.durable_gen = snap.ranges_gen;
            } else if state.ranges != before {
                state.durable_gen = 0;
            }
        }
        // O(1) EXEMPT: fixed UNAPPLIED_BUCKETS iterations, boot-time seed
        for (i, (id, count)) in snap.buckets.iter().enumerate() {
            self.bucket_ids[i].store(*id, Ordering::Release);
            self.bucket_counts[i].store(*count, Ordering::Release);
        }
    }

    /// Rows up to `max_seq` landed in QuestDB or were durably rescued.
    /// The flag check and the update run under [`Self::ack_gate`] (round 6).
    pub fn note_ticks_acked(&self, max_seq: u64) {
        let _gate = self.lock_ack_gate();
        if self.sink_suspect.load(Ordering::Acquire) {
            self.suspect_max_ticks.fetch_max(max_seq, Ordering::AcqRel);
        } else {
            self.hwm_ticks.fetch_max(max_seq, Ordering::AcqRel);
        }
    }

    /// Depth rows up to `max_seq` landed in QuestDB or were durably rescued.
    /// The flag check and the update run under [`Self::ack_gate`] (round 6).
    pub fn note_depth_acked(&self, max_seq: u64) {
        let _gate = self.lock_ack_gate();
        if self.sink_suspect.load(Ordering::Acquire) {
            self.suspect_max_depth.fetch_max(max_seq, Ordering::AcqRel);
        } else {
            self.hwm_depth.fetch_max(max_seq, Ordering::AcqRel);
        }
    }

    /// `(watermark file, ranges sidecar)`: whether persisting each is in a
    /// failing episode right now (2026-10-21, PR W review round 6). Read once
    /// by the boot fold, which runs before the tracing subscriber exists, so
    /// the episode's one `warn!` was lost and the latch then silenced every
    /// later failure; the fold records this and the boot log reports it. Two
    /// atomic loads.
    pub(crate) fn persist_failing_latched(&self) -> (bool, bool) {
        (
            self.persist_failing.load(Ordering::Acquire),
            self.ranges_failing.load(Ordering::Acquire),
        )
    }

    /// The ack gate, recovered from poison (it guards no data of its own).
    fn lock_ack_gate(&self) -> std::sync::MutexGuard<'_, ()> {
        self.ack_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The QuestDB WAL-suspension probe reported. `clean` = every table
    /// visible, none suspended, no growing apply lag. Called once per poll
    /// (60 s) from the watcher task — cold path.
    ///
    /// Entering suspicion marks `[last clean watermark, current + slack]`
    /// unapplied: the detection window, plus one second of slack for an ack
    /// that raced the flag. Leaving it marks `[watermark, highest parked ack]`
    /// unapplied and re-opens the watermark from where it stopped. Every
    /// direction is "replay more".
    ///
    /// **⚠ CHANGED 2026-10-20 (PR W review):** both marks go to the bounded
    /// RANGES ([`Self::add_unapplied_range`]), never the bucket table. A
    /// suspension longer than the table covers (~9.8 h, an episode across an
    /// overnight gap) used to overflow it into an unbounded bit that tainted
    /// every later segment; a range is bounded, trimmed as the backlog is read
    /// and cleared by the drained reset. Cold: one small sidecar write per
    /// mark, on the watcher task.
    ///
    /// **⚠ CHANGED 2026-10-20 (PR W review round 3, L8):** a range add
    /// writes and fsyncs the sidecar, and this runs on the watcher's tokio
    /// task, so each add now runs through `off_worker` (`block_in_place` on a
    /// multi-thread runtime, inline otherwise): the worker's other tasks keep
    /// running while the file is synced. Only an episode's start and end add
    /// a range; every other probe touches no file. The watermark is then
    /// persisted at once, so `applied.tvaw` names the new sidecar generation:
    /// the property test found that a crash in the up-to-a-second gap before
    /// the next routine persist left the file naming a generation no longer
    /// on disk, which loads as an unbounded overflow and makes the next boot
    /// migrate (replay) every waiting segment.
    ///
    /// **⚠ CHANGED 2026-10-20 (PR W review round 4):** both marks now start
    /// one reorder slack BELOW the watermark they start from, rounded down to
    /// a frame base ([`suspension_range_lo`]). The ring reorders frames from
    /// sixteen sockets, so a frame minted just before the one that set the
    /// last clean watermark can be acked AFTER that probe, inside the window
    /// the episode may have lied about; a range starting at the watermark
    /// missed it, and the next boot's per-record filter dropped it as
    /// applied. The bucket table this replaced marked whole 4.6-minute
    /// buckets, which almost always covered it.
    ///
    /// **⚠ CHANGED 2026-10-20 (PR W review round 4, pass 2):** two slacks
    /// below, not one. That `f` can be the last record of a segment whose
    /// successor's first record sits up to one more slack below `f`, so the
    /// segment's judged range `[first, successor first − 1]` ended below a
    /// range starting one slack under the watermark: the segment read
    /// applied, the replay skipped it and the prune could delete it. See
    /// [`suspension_range_lo`]. Cost: the episode's range keeps up to one
    /// more second of frames, so at most one more segment.
    ///
    /// **⚠ CHANGED 2026-10-21 (PR W review round 6):** both transitions
    /// flip the flag and read the watermarks (and, at the exit, drain the
    /// parked acks) under [`Self::ack_gate`], which the ack functions also
    /// take around their flag check and update. Before, an ack that read
    /// the flag on one side of a transition and wrote on the other raised
    /// the watermark after the entry range read it, or parked into a slot
    /// the exit had already drained; either way it was in no range. The
    /// file writes run after the gate is released.
    ///
    /// **ADDED 2026-10-20 (PR W review round 4):** each transition into
    /// suspicion counts on [`WAL_SUSPECT_EPISODES_COUNTER`], and the time
    /// spent suspect on [`WAL_SUSPECT_SECONDS_COUNTER`]. One monotonic clock
    /// read per probe, a few atomics, cold (once per watcher poll), never
    /// per frame.
    pub fn note_questdb_probe(&self, clean: bool) {
        self.note_questdb_probe_at(clean, mono_nanos());
    }

    /// The acknowledgement marks `(ticks, depth)` to pass to
    /// [`Self::note_questdb_probe_with_basis`]: read BEFORE the probe's query
    /// is sent. O(1).
    #[must_use]
    pub fn probe_basis(&self) -> (u64, u64) {
        (
            self.hwm_ticks.load(Ordering::Acquire),
            self.hwm_depth.load(Ordering::Acquire),
        )
    }

    /// [`Self::note_questdb_probe`] with the healthy marks taken from `basis`
    /// (from [`Self::probe_basis`], read before the query was sent) instead
    /// of the marks when the reply came back. PR W review round 7: a probe
    /// is in flight for seconds; a table that starts lying after its query
    /// snapshot is not in the reply, so acks taken during the flight are not
    /// vouched for by it, and recording them as healthy put them in no
    /// episode range when the next probe failed.
    pub fn note_questdb_probe_with_basis(&self, clean: bool, basis: (u64, u64)) {
        self.note_questdb_probe_full(clean, Some(basis), mono_nanos());
    }

    /// [`Self::note_questdb_probe`] at monotonic instant `now`.
    fn note_questdb_probe_at(&self, clean: bool, now: u64) {
        self.note_questdb_probe_full(clean, None, now);
    }

    /// The probe report: `basis` = the marks when the query was sent, or
    /// `None` for the marks now.
    fn note_questdb_probe_full(&self, clean: bool, basis: Option<(u64, u64)>, now: u64) {
        if clean {
            self.clean_probes.fetch_add(1, Ordering::AcqRel);
            // Round 6: the flip and the reads under the ack gate, so an ack
            // either parked before it (and is read here) or raises the
            // watermark after it; none can park into the drained slot.
            let exited = {
                let _gate = self.lock_ack_gate();
                self.sink_suspect.swap(false, Ordering::AcqRel).then(|| {
                    (
                        self.suspect_max_ticks.swap(0, Ordering::AcqRel),
                        self.suspect_max_depth.swap(0, Ordering::AcqRel),
                        self.hwm_ticks.load(Ordering::Acquire),
                        self.hwm_depth.load(Ordering::Acquire),
                    )
                })
            };
            if let Some((parked_ticks, parked_depth, hwm_ticks, hwm_depth)) = exited {
                self.account_suspect_time_at(now);
                if parked_ticks > hwm_ticks || parked_depth > hwm_depth {
                    // The top carries one reorder slack too (PR W review
                    // round 3), mirroring the start-of-episode mark: a frame
                    // acked under suspicion can sit in a segment whose first
                    // record is above `parked`, and only a range reaching
                    // that first record keeps the segment.
                    crate::off_worker::off_worker(|| {
                        if parked_ticks > hwm_ticks {
                            self.add_unapplied_range(
                                suspension_range_lo(hwm_ticks),
                                parked_ticks.saturating_add(REPLAY_REORDER_SLACK_SEQ),
                            );
                        }
                        if parked_depth > hwm_depth {
                            self.add_unapplied_range(
                                suspension_range_lo(hwm_depth),
                                parked_depth.saturating_add(REPLAY_REORDER_SLACK_SEQ),
                            );
                        }
                        self.persist_now();
                    });
                }
            }
            // The marks the reply vouches for: those when the query was sent
            // (never above the marks now, which only rise).
            let (now_ticks, now_depth) = self.probe_basis();
            let (healthy_ticks, healthy_depth) = basis.map_or((now_ticks, now_depth), |(t, d)| {
                (t.min(now_ticks), d.min(now_depth))
            });
            self.healthy_ticks.store(healthy_ticks, Ordering::Release);
            self.healthy_depth.store(healthy_depth, Ordering::Release);
        } else {
            // Round 6: the flip and the watermark reads under the ack gate,
            // so an ack either raised the watermark before it (and is
            // inside the range below) or parks after it.
            let entered = {
                let _gate = self.lock_ack_gate();
                (!self.sink_suspect.swap(true, Ordering::AcqRel)).then(|| {
                    (
                        self.hwm_ticks.load(Ordering::Acquire),
                        self.hwm_depth.load(Ordering::Acquire),
                    )
                })
            };
            let Some((hwm_ticks, hwm_depth)) = entered else {
                self.account_suspect_time_at(now);
                return;
            };
            self.suspect_accounted_nanos.store(now, Ordering::Release);
            metrics::counter!(WAL_SUSPECT_EPISODES_COUNTER).increment(1);
            // Record where this episode begins BEFORE its number is published
            // (audit DA-13b review): a reader that sees the new number can then
            // also see its start, and a reader mid-rewrite sees the tag change.
            let episode = self
                .suspect_episodes
                .load(Ordering::Acquire)
                .saturating_add(1);
            self.episode_start_tag.store(0, Ordering::SeqCst);
            self.episode_start_probes
                .store(self.clean_probes.load(Ordering::Acquire), Ordering::SeqCst);
            self.episode_start_secs
                .store(wall_nanos() / 1_000_000_000, Ordering::SeqCst);
            self.episode_start_tag.store(episode, Ordering::SeqCst);
            self.suspect_episodes.fetch_add(1, Ordering::AcqRel);
            crate::off_worker::off_worker(|| {
                self.add_unapplied_range(
                    suspension_range_lo(self.healthy_ticks.load(Ordering::Acquire)),
                    hwm_ticks.saturating_add(REPLAY_REORDER_SLACK_SEQ),
                );
                self.add_unapplied_range(
                    suspension_range_lo(self.healthy_depth.load(Ordering::Acquire)),
                    hwm_depth.saturating_add(REPLAY_REORDER_SLACK_SEQ),
                );
                self.persist_now();
            });
        }
    }

    /// Counts the whole seconds since the last counted instant on
    /// [`WAL_SUSPECT_SECONDS_COUNTER`] and carries the remainder. Only the
    /// watcher task reports probes, so a load and a store suffice. O(1).
    fn account_suspect_time_at(&self, now: u64) -> u64 {
        let since = self.suspect_accounted_nanos.load(Ordering::Acquire);
        let secs = now.saturating_sub(since) / 1_000_000_000;
        if secs == 0 {
            return 0;
        }
        self.suspect_accounted_nanos
            .store(since + secs * 1_000_000_000, Ordering::Release);
        self.suspect_seconds.fetch_add(secs, Ordering::AcqRel);
        metrics::counter!(WAL_SUSPECT_SECONDS_COUNTER).increment(secs);
        secs
    }

    /// How many clean probes have reported this process (audit PR41b). `0`
    /// until the WAL-suspension watcher first sees every table healthy, so a
    /// reader can tell "no prober running" from "prober running".
    #[must_use]
    pub fn clean_probe_count(&self) -> u64 {
        self.clean_probes.load(Ordering::Acquire)
    }

    /// How many suspicion episodes have begun this process (audit DA-13b,
    /// DA-13c). A change between two reads means the sink was suspect at some
    /// point in between, even if it has recovered since: an ack stamped with
    /// one value and read back with another may have been acknowledged by a
    /// table that never applied it.
    #[must_use]
    pub fn suspect_episode_count(&self) -> u64 {
        self.suspect_episodes.load(Ordering::Acquire)
    }

    /// Where the latest suspicion episode began, as `(episode, clean probes
    /// at its start, wall-clock seconds at its start)`, or `None` before the
    /// first episode or when the record was being rewritten during the read
    /// (audit DA-13b review). O(1): four atomic loads, no lock.
    #[must_use]
    pub fn last_episode_start(&self) -> Option<(u64, u64, u64)> {
        let tag_before = self.episode_start_tag.load(Ordering::SeqCst);
        let probes = self.episode_start_probes.load(Ordering::SeqCst);
        let secs = self.episode_start_secs.load(Ordering::SeqCst);
        let tag_after = self.episode_start_tag.load(Ordering::SeqCst);
        (tag_before != 0 && tag_before == tag_after).then_some((tag_before, probes, secs))
    }

    /// DA-13a review: this process will run the QuestDB WAL-suspension
    /// watcher. Called once by `main.rs` BEFORE the seal writer's boot drain
    /// can run (the watcher itself is spawned later), so a file the drain
    /// flushes before the watcher's first verdict waits for that verdict
    /// instead of being archived on a 2xx a suspended table may have sent.
    /// One Release store. Never cleared: the watcher is supervised and
    /// respawns, so once expected it stays expected.
    pub fn mark_probe_expected(&self) {
        self.probe_expected.store(true, Ordering::Release);
    }

    /// Whether [`Self::mark_probe_expected`] has been called. One Acquire load.
    #[must_use]
    pub fn is_probe_expected(&self) -> bool {
        self.probe_expected.load(Ordering::Acquire)
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
            // Low before high (PR W review round 3): a bound that reads this
            // high (Acquire) then reads this low, so it never pairs the new
            // high with a stale low.
            self.overflow_low_seq
                .fetch_min(id << UNAPPLIED_BUCKET_SHIFT, Ordering::AcqRel);
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
    ///
    /// 2026-10-20 (PR W review): the cold wide-range callers (the QuestDB
    /// suspension probe, the settle recovery, the backlog guard) use
    /// [`Self::add_unapplied_range`] instead; what still comes here is one
    /// rescue batch at a time, which spans a bucket or two.
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
            // Low before high, as in `note_unapplied`.
            self.overflow_low_seq.fetch_min(lo, Ordering::AcqRel);
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
            self.overflow_low_seq.store(u64::MAX, Ordering::Release);
            self.overflowed.store(false, Ordering::Release);
        }
        // 2026-10-20: the bounded ranges below the ceiling go too. The boot
        // fold, the boot tail and the backlog guard cover frames of EARLIER
        // processes only, all at or below the boot fold's `boot_frame_high`.
        // A range over THIS process's frames (a QuestDB suspension episode,
        // the settle recovery) is cut below the drain ceiling only, exactly
        // like a bucket. `migrated_overflow` and, with it, the file's overflow
        // bit clear once no range is left over the migrated span.
        let range_ceiling = self.range_reset_ceiling(ceiling_seq);
        let mut state = self.lock_ranges();
        let changed = if range_ceiling == u64::MAX {
            let had = !state.ranges.is_empty();
            state.ranges = UnappliedRanges::new();
            had
        } else {
            state.ranges.trim_below(range_ceiling)
        };
        if state.settle_migrated() || changed {
            self.write_ranges_locked(&mut state);
        }
    }

    /// The sequence the drained reset cuts the RANGES below (2026-10-20, PR W
    /// review round 3, M1). O(1).
    ///
    /// The first draft cut below `max(ceiling, own_floor + 1)`, on the
    /// assumption that this process mints nothing at or below `own_floor`.
    /// That was assumed, not enforced: the counter stayed at the seeded value
    /// and minted `max(prev + 1, now)`, so a backward clock step after the
    /// fold minted frames inside `(boot_frame_high, own_floor)`, and a reset
    /// could clear a suspension range over them. Now the fold wrapper
    /// ratchets the counter to `own_floor` first, and this uses the safer
    /// boundary as well:
    ///
    /// * the ceiling at or above `own_floor` (the counter was read after the
    ///   ratchet): `max(ceiling, own_floor + 1)`. `own_floor` is the
    ///   ratchet's value and is never minted, so nothing sits at it;
    /// * a ceiling below `own_floor` (a watermark folded without the ratchet,
    ///   tests only): `max(ceiling, boot_frame_high + 1)`, which can only
    ///   leave a frameless remnant of the boot tail, never clear a range over
    ///   a frame;
    /// * no fold in this process: the ceiling; `u64::MAX`: everything.
    fn range_reset_ceiling(&self, ceiling_seq: u64) -> u64 {
        if ceiling_seq == u64::MAX || !self.boot_folded.load(Ordering::Acquire) {
            return ceiling_seq;
        }
        let own_floor = self.boot_own_floor.load(Ordering::Acquire);
        if ceiling_seq >= own_floor {
            ceiling_seq.max(own_floor.saturating_add(1))
        } else {
            ceiling_seq.max(
                self.boot_frame_high
                    .load(Ordering::Acquire)
                    .saturating_add(1),
            )
        }
    }

    /// The seeded frame sequence the boot fold recorded (every frame of an
    /// earlier process is at or below it), or `None` before the fold ran.
    /// O(1).
    #[must_use]
    pub(crate) fn boot_frame_high(&self) -> Option<u64> {
        self.boot_folded
            .load(Ordering::Acquire)
            .then(|| self.boot_frame_high.load(Ordering::Acquire))
    }

    /// Adds `[lo, hi]` to the bounded ranges and writes the sidecar (when
    /// bound) before returning. Returns whether coverage changed. The caller
    /// persists the watermark so the file's pad names the new generation.
    /// O(32) plus one small file write; cold (the backlog guard).
    ///
    /// 2026-10-20 (PR W review round 4): `lo` is rounded down to a frame base
    /// ([`frame_floor`]) after the `lo > hi` check, so a frame never
    /// straddles a range's lower edge (the replay checks a frame by its
    /// base). Only ever widens.
    pub fn add_unapplied_range(&self, lo: u64, hi: u64) -> bool {
        if lo > hi {
            return false;
        }
        let mut state = self.lock_ranges();
        let changed = state.ranges.add_range(frame_floor(lo), hi);
        if changed {
            self.write_ranges_locked(&mut state);
        }
        changed
    }

    /// Keeps only the parts of the ranges that fall inside `spans` (sorted,
    /// disjoint `[lo, hi]` sequence spans of the segments still waiting, the
    /// newest open-ended) and writes the sidecar when anything changed
    /// (2026-10-20, PR W review). A range part over no waiting segment
    /// protects nothing on disk. When the pieces need more than 32 slots, a
    /// merge prefers a gap that holds no waiting span
    /// ([`UnappliedRanges::add_range_avoiding`]). Clears `migrated_overflow`
    /// once no range intersects the migrated span (**⚠ CHANGED 2026-10-20,
    /// PR W review round 3, M4:** it cleared only when no range at all was
    /// left, so one later boot tail kept an older binary's overflow bit set
    /// for ever). O(32 × log spans + spans), cold: after the boot refold and
    /// each catch-up round.
    pub fn clip_unapplied_ranges_to(&self, spans: &[(u64, u64)]) -> bool {
        let mut state = self.lock_ranges();
        let changed = state.ranges.clip_to(spans);
        if state.settle_migrated() || changed {
            self.write_ranges_locked(&mut state);
        }
        changed
    }

    /// `trim_below(seq)` on the ranges, writing the sidecar when anything
    /// changed (2026-10-20, PR W review round 3, L1): the partial trim used
    /// when the only waiting segments of unknown span sit in
    /// `settling/unreadable/`. Returns whether the ranges changed. O(32) plus
    /// at most one small file write, cold.
    pub(crate) fn trim_unapplied_ranges_below(&self, seq: u64) -> bool {
        let mut state = self.lock_ranges();
        let changed = state.ranges.trim_below(seq);
        if state.settle_migrated() || changed {
            self.write_ranges_locked(&mut state);
        }
        changed
    }

    /// The boot fold (2026-10-20, plan item PR W) with no waiting spans: a
    /// full range set merges its lowest neighbours. See
    /// [`Self::fold_at_boot_with_spans`]. Test-only since 2026-10-20 (PR W
    /// review round 4): production folds through
    /// `wal_unapplied_ranges::fold_at_boot`, which passes the spans.
    #[cfg(test)]
    pub(crate) fn fold_at_boot(
        &self,
        oldest_waiting: Option<u64>,
        boot_frame_high: u64,
        own_floor: u64,
    ) -> Option<BootFold> {
        self.fold_at_boot_with_spans(oldest_waiting, boot_frame_high, own_floor, &[])
    }

    /// The boot fold (2026-10-20, plan item PR W). Once per process, before
    /// any sink or socket exists. `boot_frame_high` is the seeded frame
    /// sequence (every frame an earlier process wrote is at or below it),
    /// `own_floor` is at or above it, and (round 3, M1) the production
    /// wrapper ratchets the frame counter to `own_floor` before calling
    /// this, so every frame this process mints is above `own_floor`.
    /// `oldest_waiting` is the lowest first sequence of a segment a replay
    /// may still read (`None` when there is none, one is unreadable, or a
    /// folder could not be listed, which widens the migrated range to start
    /// at 1). `spans` are the waiting segments' sequence spans
    /// (the boot scan inside `wal_unapplied_ranges::fold_at_boot`, empty when
    /// unknown; `waiting_spans` is only its test seam): when a
    /// range set is full, a merge prefers a gap no waiting span intersects.
    ///
    /// 1. An overflow present now (inherited at `u64::MAX`, or a known height
    ///    at or below `own_floor`) becomes the range `[oldest_waiting −
    ///    REPLAY_REORDER_SLACK_SEQ, own_floor]` (`[that, height]` with a
    ///    known height) and the flag clears; that range is recorded as the
    ///    migrated span. Frames below the oldest waiting segment, less the
    ///    slack, are archived (applied by construction) or gone. **⚠
    ///    CORRECTED 2026-10-20 (PR W review):** the range started at the
    ///    oldest segment's FIRST sequence, but sixteen sockets race to the
    ///    writer, so a later record in that segment can carry a sequence
    ///    below its first. **⚠ CHANGED (round 3, L5):** the overflow's low
    ///    end is no longer tracked; it had no production writer before the
    ///    fold.
    /// 2. Every bucket whose id is below `own_floor`'s bucket becomes its
    ///    exact range plus one reorder slack on the top (PR W review round
    ///    4: a frame of the bucket can sit in a segment whose first record
    ///    is just above the bucket's end) and one on the bottom, at a frame
    ///    base, at least 1 (round 5: a frame of the bucket can sit in a
    ///    segment whose range ends just below the bucket's start), and its
    ///    slot is emptied, so the
    ///    table starts the session holding nothing from an earlier day and a
    ///    next-day shed cannot collide with a stale id.
    /// 3. The boot tail `[boot_range_floor, own_floor]` (**⚠ CHANGED
    ///    2026-10-21, PR W review round 6:** was `boot_applied_floor + 1`,
    ///    one slack too high; see [`Self::boot_range_floor`]) is added: the
    ///    earlier process's frames the file never vouched for stay unapplied
    ///    even after this session's acks pass them. **⚠ CHANGED (round 3,
    ///    L3):** with a known oldest waiting segment it starts no lower than
    ///    that segment less the slack (what the trim would cut anyway), so a
    ///    boot with no or a rejected `applied.tvaw` no longer records
    ///    `[1, own_floor]`.
    /// 4. `[boot_frame_high + 1, own_floor]` is recorded as the frameless
    ///    gap (no process wrote or will write a frame there), left out when
    ///    the ranges are encoded for an older binary.
    /// 5. The sidecar is written before returning; the caller persists the
    ///    watermark.
    ///
    /// O(UNAPPLIED_BUCKETS + folded × (32 + log spans)), cold, no allocation.
    pub fn fold_at_boot_with_spans(
        &self,
        oldest_waiting: Option<u64>,
        boot_frame_high: u64,
        own_floor: u64,
        spans: &[(u64, u64)],
    ) -> Option<BootFold> {
        if self.boot_folded.swap(true, Ordering::AcqRel) {
            return None;
        }
        self.boot_own_floor.store(own_floor, Ordering::Release);
        self.boot_frame_high
            .store(boot_frame_high, Ordering::Release);
        let gap_lo = boot_frame_high.saturating_add(1);
        if gap_lo <= own_floor {
            self.boot_gap_lo.store(gap_lo, Ordering::Release);
            self.boot_gap_hi.store(own_floor, Ordering::Release);
        }
        let mut out = BootFold {
            own_floor,
            boot_frame_high,
            boot_applied_floor: self.boot_applied_floor(),
            ..BootFold::default()
        };
        let oldest_lo =
            oldest_waiting.map(|oldest| oldest.saturating_sub(REPLAY_REORDER_SLACK_SEQ).max(1));
        let mut state = self.lock_ranges();
        let before = state.ranges;
        if self.overflowed.load(Ordering::Acquire) {
            let height = self.overflow_high_seq.load(Ordering::Acquire);
            if height == u64::MAX || height <= own_floor {
                let hi = if height == u64::MAX || height == 0 {
                    own_floor
                } else {
                    height
                };
                let lo = oldest_lo.unwrap_or(1).clamp(1, hi.max(1));
                state.ranges.add_range_avoiding(lo, hi, spans);
                state.widen_migrated((lo, hi));
                self.overflow_high_seq.store(0, Ordering::Release);
                self.overflow_low_seq.store(u64::MAX, Ordering::Release);
                self.overflowed.store(false, Ordering::Release);
                out.migrated_overflow = true;
            }
        }
        let own_bucket = own_floor >> UNAPPLIED_BUCKET_SHIFT;
        // O(1) EXEMPT: fixed UNAPPLIED_BUCKETS iterations, once per boot
        for i in 0..UNAPPLIED_BUCKETS {
            let id = self.bucket_ids[i].load(Ordering::Acquire);
            if id == 0 || id >= own_bucket {
                continue;
            }
            // One reorder slack on the bottom (PR W review round 5): a frame
            // shed just above the bucket's start can sit in a segment whose
            // range ends just below it, and a range is looked up up to that
            // end, not above it (see `AppliedSnapshot::segment_is_applied`).
            let lo = frame_floor(
                (id << UNAPPLIED_BUCKET_SHIFT).saturating_sub(REPLAY_REORDER_SLACK_SEQ),
            );
            // One reorder slack on the top (PR W review round 4): a frame
            // shed just below the bucket's end can sit in a segment whose
            // first record is just above it, and a range is looked up from
            // that first record, not below it (see
            // `AppliedSnapshot::segment_is_applied`).
            let hi = ((id + 1) << UNAPPLIED_BUCKET_SHIFT)
                .saturating_sub(1)
                .saturating_add(REPLAY_REORDER_SLACK_SEQ);
            state.ranges.add_range_avoiding(lo, hi, spans);
            self.bucket_ids[i].store(0, Ordering::Release);
            self.bucket_counts[i].store(0, Ordering::Release);
            out.folded_buckets = out.folded_buckets.saturating_add(1);
        }
        // Round 6: one slack below `boot_applied_floor + 1` (see
        // `boot_range_floor`).
        let tail_lo = self.boot_range_floor().max(oldest_lo.unwrap_or(1));
        if tail_lo <= own_floor {
            state.ranges.add_range_avoiding(tail_lo, own_floor, spans);
            out.tail = Some((tail_lo, own_floor));
        }
        state.settle_migrated();
        if state.ranges != before || out.migrated_overflow || state.durable_gen == 0 {
            self.write_ranges_locked(&mut state);
        }
        out.ranges = state.ranges.len();
        Some(out)
    }

    /// The ranges lock, recovered from poison (plain integers; every mutation
    /// builds the new set before storing it, so a panic leaves a whole set).
    fn lock_ranges(&self) -> std::sync::MutexGuard<'_, RangesState> {
        self.ranges
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Writes `state` to the sidecar under the caller's lock and records the
    /// generation that now holds it durably. Unbound: nothing to write
    /// (`durable_gen` stays 0, so the file's pad names no sidecar and the
    /// ranges load only as their bucket encoding, or as an unbounded bit when
    /// they do not fit). A failure leaves `durable_gen` at 0 and is counted,
    /// warned once per episode, and retried by the next persist. Cold: one
    /// small file, two fsyncs.
    ///
    /// **Honest limit (2026-10-20, PR W review round 3, L2), not fixed:** on
    /// a full disk the tmp file cannot be created, so `durable_gen` stays 0,
    /// the watermark file names no sidecar, and a range that does not fit
    /// the bucket table writes the unbounded bit: the prune then refuses
    /// every segment, which cannot free the disk that blocked the write.
    /// `applied.tvaw` is written the same tmp + rename way and fails the
    /// same way, which was already true before this change. An in-place
    /// rewrite of a pre-created sidecar was considered and NOT taken: the
    /// sidecar is trusted only when its generation matches the pad bytes of
    /// `applied.tvaw`, so rewriting one file in place while the other cannot
    /// be written turns a trusted pair into an untrusted one. Every
    /// direction stays "replay more"; freeing the disk is the operator's step.
    fn write_ranges_locked(&self, state: &mut RangesState) {
        state.durable_gen = 0;
        let Some((path, tmp, dir)) = self.ranges_paths.get() else {
            return;
        };
        let generation = if state.next_gen == 0 {
            next_generation(u16::try_from((wall_nanos() >> 20) & 0xFFFF).unwrap_or(1))
        } else {
            state.next_gen
        };
        let file = UnappliedRangesFile {
            ranges: state.ranges,
            migrated_overflow: state.migrated_span.is_some(),
            migrated_span: state.migrated_span,
            generation,
            dir_tag: self.dir_tag.load(Ordering::Acquire),
        };
        match file.persist(path, tmp, dir) {
            Ok(()) => {
                state.durable_gen = generation;
                state.next_gen = next_generation(generation);
                if self.ranges_failing.swap(false, Ordering::AcqRel) {
                    tracing::info!(
                        path = %path.display(),
                        "WAL unapplied ranges are persisting again"
                    );
                }
            }
            Err(err) => {
                metrics::counter!(APPLIED_PERSIST_FAILED_COUNTER).increment(1);
                if !self.ranges_failing.swap(true, Ordering::AcqRel) {
                    warn!(
                        code = tickvault_common::error_code::ErrorCode::WsSpill01WriterRespawn.code_str(),
                        source = "unapplied_ranges_persist_failed",
                        path = %path.display(),
                        error = %err,
                        "WAL unapplied ranges could not be persisted — the watermark file then \
                         names no sidecar, so the next boot reads the ranges as whole buckets \
                         (or, when they do not fit, as an overflow and replays everything); \
                         nothing is skipped"
                    );
                }
            }
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
        let state = *self.lock_ranges();
        self.snapshot_with(&state)
    }

    /// [`Self::snapshot`] with the ranges the caller already holds.
    fn snapshot_with(&self, state: &RangesState) -> AppliedSnapshot {
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
            overflow_high_seq: self.overflow_high_seq.load(Ordering::Acquire),
            ranges: state.ranges,
            ranges_gen: state.durable_gen,
            migrated_overflow: state.migrated_span.is_some(),
            migrated_span: state.migrated_span,
            frameless_gap: {
                let (lo, hi) = (
                    self.boot_gap_lo.load(Ordering::Acquire),
                    self.boot_gap_hi.load(Ordering::Acquire),
                );
                (lo != 0 && lo <= hi).then_some((lo, hi))
            },
        }
    }

    /// The snapshot as it is written to disk: the live state with both
    /// high-water marks held back by the durability lag (Z8b) and below every
    /// queued rescue floor (Z7). Cold: once per persist, under the lock.
    fn durable_view(
        &self,
        lag: &mut DurabilityLag,
        mono_now: u64,
        state: &RangesState,
    ) -> AppliedSnapshot {
        // The watermarks are read BEFORE the floors. A floor is pushed before
        // its batch is handed off, and every ack that can lift the watermark
        // past that batch happens after; so a watermark value read here that
        // has passed the batch guarantees the floor is visible to the loads
        // below (Acquire on the watermark, Release on the floor store).
        let mut snap = self.snapshot_with(state);
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
        let state = *self.lock_ranges();
        self.durable_view(&mut lag, mono_now, &state)
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
    /// O(RESCUE_FLOOR_SLOTS), a constant 16, not O(1). `None` when the batch
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
        // O(1) EXEMPT: bounded by RESCUE_FLOOR_SLOTS (16), rescue arm only
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
        // O(1) EXEMPT: bounded by RESCUE_FLOOR_SLOTS (16), once-a-second persist
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
    /// durability lag through this; public since 2026-10-20 for the
    /// integration proptest, which models the 60 s lag across lifetimes).
    /// Hidden from the docs (PR W review round 4): public only for
    /// `tests/wal_unapplied_ranges_properties.rs`; production calls
    /// [`Self::persist_now`].
    #[doc(hidden)]
    pub fn persist_at(&self, mono_now: u64) {
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
        // 2026-10-20: the ranges are read under their own lock, held through
        // the write, so the pad written below names the sidecar that holds
        // exactly these ranges. A range mutation in progress holds it; skip,
        // the mutating caller persists right after. RAM ranges not yet
        // durable (a failed sidecar write) are written first.
        let mut state = match self.ranges.try_lock() {
            Ok(guard) => guard,
            Err(std::sync::TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => return,
        };
        let bounded = self.bound_known_overflow_locked(&mut state);
        if bounded || (state.durable_gen == 0 && !state.ranges.is_empty()) {
            self.write_ranges_locked(&mut state);
        }
        // The lock stays held through the write: two persists racing on the
        // same tmp path would refuse each other at `create_new`.
        let mut snap = self.durable_view(&mut lag, mono_now, &state);
        snap.persisted_at_nanos = wall_nanos();
        let bytes = snap.to_bytes();
        let written = write_fresh(tmp, &bytes).and_then(|()| std::fs::rename(tmp, path));
        if let Err(err) = written {
            metrics::counter!(APPLIED_PERSIST_FAILED_COUNTER).increment(1);
            if self.persist_failing.swap(true, Ordering::AcqRel) {
                return; // already reported this episode
            }
            warn!(
                code = tickvault_common::error_code::ErrorCode::WsSpill01WriterRespawn.code_str(),
                source = "applied_watermark_persist_failed",
                path = %path.display(),
                error = %err,
                "WAL applied-watermark could not be persisted — the RAM value is kept and \
                 the next boot replays MORE than it needs to, never less"
            );
        } else if self.persist_failing.swap(false, Ordering::AcqRel) {
            tracing::info!(
                path = %path.display(),
                "WAL applied-watermark is persisting again"
            );
        }
    }

    /// Turns an in-RAM bucket-table overflow of KNOWN height into the bounded
    /// range `[overflow_low_seq, overflow_high_seq]` (from 1 when the low end
    /// is unknown) and clears the flag, under the ranges lock the persist
    /// holds (2026-10-20, PR W review round 4). Returns whether the ranges
    /// changed (the caller writes the sidecar).
    ///
    /// Before, `to_bytes` wrote any in-RAM overflow as the bare bit with pad
    /// 0, which loads as height `u64::MAX`: the prune reads the FILE, so from
    /// the first overflow on, every prune pass of the session refused every
    /// segment, and the next boot migrated the bit into a range over the
    /// whole session. A catch-up round that re-appends earlier days' frames
    /// can overflow the table (a failed rescue marks their original
    /// `capture_seq`, whose bucket id can share a slot with today's sheds,
    /// or a batch across a day boundary spans 128 buckets or more). The boot
    /// fold already bounds an overflow of known height the same way; this
    /// does it in the session. An unknown height (`u64::MAX`, seeded from a
    /// file, or `0`) is left as the bit for the boot fold.
    ///
    /// After the range is added, the two bounds are reset (high to 0, low to
    /// `u64::MAX`) by compare-and-swap, so the NEXT overflow of the session
    /// is bounded on its own (PR W review round 3). Before, the bounds were
    /// never reset here (only the drained reset and the boot fold did), so a
    /// second overflow hours later was persisted as one range from the FIRST
    /// overflow's low end to the second one's top, pinning every acked
    /// segment written in between.
    ///
    /// Races with an overflow arm (which lowers `low`, then raises `high`,
    /// then sets the flag). The flag is cleared BEFORE the swaps:
    /// - `high` moved since it was read: the swap fails, the flag is set
    ///   back and the next pass bounds the new values (low was not reset).
    /// - `low` moved since it was read: the low swap fails, `high` is put
    ///   back (`fetch_max`), the flag is set back and the next pass runs.
    /// - both swaps succeed and `high` is then seen raised while `low` reads
    ///   `u64::MAX`: a racing overflow lowered `low` before it was read (so
    ///   its low end is at or above the low just bounded) and the reset
    ///   wiped it; that low is put back.
    /// Honest limit: an overflow arm stalled between its two atomic steps
    /// across this whole reset leaves `low` at `u64::MAX` under a real high;
    /// the next bound then starts at sequence 1 (keeps more, never less).
    /// At most four passes, then the flag stays set for the next persist.
    /// O(1) per pass plus one O(32) range insert, cold: the once-a-second
    /// persist.
    fn bound_known_overflow_locked(&self, state: &mut RangesState) -> bool {
        let mut changed = false;
        // O(1) EXEMPT: at most four passes, each re-run only when a racing overflow moved a bound
        for _ in 0..4 {
            if !self.overflowed.load(Ordering::Acquire) {
                break;
            }
            let high = self.overflow_high_seq.load(Ordering::Acquire);
            if high == 0 || high == u64::MAX {
                break;
            }
            let low = self.overflow_low_seq.load(Ordering::Acquire);
            let lo = if low == 0 || low > high { 1 } else { low };
            // One reorder slack on the top, as on the episode marks: a frame
            // of the overflowed batch can sit in a segment whose first
            // record is above `high` (see `AppliedSnapshot::segment_is_applied`).
            // And one on the bottom (PR W review round 5): a frame at `low`
            // can sit in a segment whose range ends below it.
            changed |= state.ranges.add_range(
                frame_floor(lo.saturating_sub(REPLAY_REORDER_SLACK_SEQ)),
                high.saturating_add(REPLAY_REORDER_SLACK_SEQ),
            );
            self.overflowed.store(false, Ordering::SeqCst);
            match self.overflow_high_seq.compare_exchange(
                high,
                0,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {}
                // The drained reset cleared everything meanwhile.
                Err(0) => break,
                Err(_) => {
                    self.overflowed.store(true, Ordering::Release);
                    continue;
                }
            }
            if self
                .overflow_low_seq
                .compare_exchange(low, u64::MAX, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
            {
                self.overflow_high_seq.fetch_max(high, Ordering::AcqRel);
                self.overflowed.store(true, Ordering::Release);
                continue;
            }
            if self.overflow_high_seq.load(Ordering::Acquire) != 0
                && self.overflow_low_seq.load(Ordering::Acquire) == u64::MAX
                && low != u64::MAX
            {
                self.overflow_low_seq.fetch_min(low, Ordering::AcqRel);
                self.overflowed.store(true, Ordering::Release);
            }
            break;
        }
        changed
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

/// The boot applied floor for a loaded file: `min(hwm_ticks, hwm_depth)` less
/// the reorder slack, WHATEVER the file's `depth_untracked` flag says.
///
/// **⚠ CHANGED 2026-10-20 (PR W review round 3, M2).** This was
/// `skip_below(Ticks)`, which honours the flag (`hwm_ticks` alone when the
/// last persist had no depth sink). After an untracked-to-tracked depth
/// switch, the boot tail and the backlog guard then started above the depth
/// frames in `(hwm_depth, hwm_ticks]`, which the old guard covered; once the
/// live depth acks lifted `hwm_depth` past them, the next replay skipped
/// them. Cost: on a lane with no depth sink `hwm_depth` does not move, so the
/// tail starts lower and the boot replays more (the production lane always
/// installs a depth sink). O(1).
fn boot_floor_of(snap: &AppliedSnapshot) -> u64 {
    snap.hwm_ticks
        .min(snap.hwm_depth)
        .saturating_sub(REPLAY_REORDER_SLACK_SEQ)
}

/// The generation after `g`, skipping 0 (which means "none").
const fn next_generation(g: u16) -> u16 {
    let n = g.wrapping_add(1);
    if n == 0 { 1 } else { n }
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

// Kept after every production item on purpose: `loss_counter_visibility_guard`
// stops reading a file at its first column-0 `#[cfg(test)]`, so a test-only
// item above production code hides that code's emit sites from the guard.
// Kept before the test module so clippy's `items_after_test_module` holds.

/// Writes a valid watermark file for `wal_dir` holding the two high-water
/// marks, as a persist would.
#[cfg(test)]
// TEST-EXEMPT: test-only helper for the S3 sequence-seed tests
pub(crate) fn write_file_for_test(wal_dir: &Path, hwm_ticks: u64, hwm_depth: u64) {
    let bytes = AppliedSnapshot {
        hwm_ticks,
        hwm_depth,
        persisted_at_nanos: wall_nanos(),
        dir_tag: dir_tag_of(wal_dir),
        ..AppliedSnapshot::default()
    }
    .to_bytes();
    std::fs::write(wal_dir.join(APPLIED_WATERMARK_FILE), bytes).expect("write watermark"); // APPROVED: test-only
}

/// Writes `applied.tvaw` as an OLDER binary leaves it after an overflow: the
/// overflow bit set, the pad bytes zero (no sidecar named), both sinks at
/// `hwm`, the given bucket table (2026-10-20, PR W review round 4). The new
/// binary's persist no longer writes this shape for an overflow of known
/// height, so the tests that start from it build it directly.
#[cfg(test)]
// TEST-EXEMPT: test-only helper for the older-binary overflow tests
pub(crate) fn write_v1_overflow_file_for_test(
    wal_dir: &Path,
    hwm: u64,
    buckets: [(u64, u64); UNAPPLIED_BUCKETS],
) {
    let bytes = AppliedSnapshot {
        hwm_ticks: hwm,
        hwm_depth: hwm,
        overflowed: true,
        overflow_high_seq: u64::MAX,
        persisted_at_nanos: wall_nanos(),
        dir_tag: dir_tag_of(wal_dir),
        buckets,
        ..AppliedSnapshot::default()
    }
    .to_bytes();
    std::fs::write(wal_dir.join(APPLIED_WATERMARK_FILE), bytes).expect("write watermark"); // APPROVED: test-only
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_boot_range_floor_is_one_more_slack_below_the_boot_floor() {
        let wm = AppliedWatermark::new();
        // No file: the floor is 0 and the range floor stays at the start.
        assert_eq!(wm.boot_range_floor(), frame_floor(0));
        let floor = seq(100);
        wm.boot_applied_floor.store(floor, Ordering::Release);
        assert_eq!(
            wm.boot_range_floor(),
            frame_floor(floor - REPLAY_REORDER_SLACK_SEQ)
        );
        assert!(wm.boot_range_floor() < floor);
    }

    #[test]
    fn test_persist_failing_latched_reports_both_episodes() {
        let wm = AppliedWatermark::new();
        assert_eq!(wm.persist_failing_latched(), (false, false));
        wm.persist_failing.store(true, Ordering::Release);
        assert_eq!(wm.persist_failing_latched(), (true, false));
        wm.ranges_failing.store(true, Ordering::Release);
        assert_eq!(wm.persist_failing_latched(), (true, true));
        wm.persist_failing.store(false, Ordering::Release);
        assert_eq!(wm.persist_failing_latched(), (false, true));
    }

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

    // Regression: 2026-10-09 — a failing persist logged a warning every
    // second for as long as the disk refused it. It now reports once per
    // episode and re-arms when a persist succeeds.
    #[test]
    fn test_regression_failing_persist_reports_once_and_rearms_on_success() {
        let dir = scratch("persist_latch").join("not_yet_created");
        let wm = AppliedWatermark::new();
        wm.bind(&dir);
        assert!(!wm.persist_failing.load(Ordering::Acquire));
        wm.persist_at(1);
        assert!(
            wm.persist_failing.load(Ordering::Acquire),
            "a failed write opens the episode"
        );
        wm.persist_at(2);
        assert!(wm.persist_failing.load(Ordering::Acquire), "still failing");
        std::fs::create_dir_all(&dir).expect("create dir");
        wm.persist_at(3);
        assert!(
            !wm.persist_failing.load(Ordering::Acquire),
            "a successful write closes the episode"
        );
        assert!(dir.join(APPLIED_WATERMARK_FILE).exists());
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
    fn test_regression_s3_persisted_high_water_reads_only_a_valid_own_file() {
        let a = scratch("s3_hwm_a");
        let b = scratch("s3_hwm_b");
        assert_eq!(AppliedSnapshot::persisted_high_water(&a), 0, "no file");
        write_file_for_test(&a, seq(300), seq(700));
        assert_eq!(
            AppliedSnapshot::persisted_high_water(&a),
            seq(700),
            "the higher of the two sinks"
        );
        std::fs::copy(
            a.join(APPLIED_WATERMARK_FILE),
            b.join(APPLIED_WATERMARK_FILE),
        )
        .expect("copy");
        assert_eq!(
            AppliedSnapshot::persisted_high_water(&b),
            0,
            "another directory's file vouches for nothing here"
        );
        std::fs::write(a.join(APPLIED_WATERMARK_FILE), b"torn").expect("write");
        assert_eq!(AppliedSnapshot::persisted_high_water(&a), 0, "damaged file");
    }

    /// The test-only writer produces a file `load` accepts in this directory,
    /// carrying both sinks and no overflow.
    #[test]
    fn test_write_file_for_test_writes_a_file_load_accepts() {
        let dir = scratch("helper_write_file");
        write_file_for_test(&dir, seq(300), seq(700));
        let snap = AppliedSnapshot::load(&dir).expect("own valid file");
        assert_eq!(snap.hwm_ticks, seq(300));
        assert_eq!(snap.hwm_depth, seq(700));
        assert!(!snap.overflowed, "no overflow written");
    }

    /// The older-binary writer produces the shape the boot fold must migrate:
    /// overflow bit set with no height, both sinks at `hwm`, the given
    /// bucket table, and no ranges sidecar.
    #[test]
    fn test_write_v1_overflow_file_for_test_writes_the_older_binary_shape() {
        let dir = scratch("helper_write_v1_overflow");
        let mut buckets = [(0u64, 0u64); UNAPPLIED_BUCKETS];
        buckets[0] = (7, 1);
        write_v1_overflow_file_for_test(&dir, seq(400), buckets);
        let snap = AppliedSnapshot::load(&dir).expect("own valid file");
        assert!(snap.overflowed, "overflow bit set");
        assert_eq!(snap.overflow_high_seq, u64::MAX, "no height on disk");
        assert_eq!(snap.hwm_ticks, seq(400));
        assert_eq!(snap.hwm_depth, seq(400));
        assert_eq!(snap.buckets, buckets, "bucket table kept as written");
        assert!(
            !dir.join(crate::wal_unapplied_ranges::UNAPPLIED_RANGES_FILE)
                .exists(),
            "no sidecar"
        );
    }

    /// Regression (PR W review round 7): a clean probe vouches only for the
    /// acks taken before its query left. Acks during the flight (100 -> 400
    /// here) must fall inside the range the next failed probe writes; with the
    /// marks read after the reply, healthy was 400 and they fell in none.
    #[test]
    fn test_note_questdb_probe_with_basis_keeps_in_flight_acks_unsettled() {
        let wm = AppliedWatermark::new_for_tests();
        wm.note_ticks_acked(seq(100));
        wm.note_depth_acked(seq(100));
        let basis = wm.probe_basis();
        // In flight: a table starts lying, acks keep coming.
        wm.note_ticks_acked(seq(400));
        wm.note_depth_acked(seq(400));
        wm.note_questdb_probe_with_basis(true, basis);
        wm.note_questdb_probe(false);
        let snap = wm.snapshot();
        assert!(
            !snap.frame_is_applied(AppliedSink::Ticks, seq(300)),
            "an ack taken during the clean probe's flight is not vouched for"
        );
        assert!(snap.range_has_unapplied(seq(300), seq(300)));
        // The marks now never go backwards: a basis above them is clamped.
        let wm = AppliedWatermark::new_for_tests();
        wm.note_ticks_acked(seq(50));
        wm.note_depth_acked(seq(50));
        wm.note_questdb_probe_with_basis(true, (seq(900), seq(900)));
        assert_eq!(wm.probe_basis(), (seq(50), seq(50)));
    }

    #[test]
    fn probe_basis_reads_both_acked_watermarks() {
        let wm = AppliedWatermark::new_for_tests();
        assert_eq!(wm.probe_basis(), (0, 0));
        wm.note_ticks_acked(seq(70));
        wm.note_depth_acked(seq(30));
        assert_eq!(wm.probe_basis(), (seq(70), seq(30)));
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
        //
        // ⚠ CORRECTED 2026-10-20 (plan item PR W): "never" now means "never
        // by a reset alone". The boot fold bounds a loaded flag as a range
        // (`fold_at_boot`), and a drain past that range clears it; that path
        // is pinned by
        // `test_regression_reloaded_overflow_clears_after_a_drain_past_its_height`.
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

    // Regression: 2026-10-13 — DA-13a review. Before the watcher's first
    // verdict the watermark read exactly like "no watcher", so the seal boot
    // drain archived on a bare 2xx.
    #[test]
    fn test_regression_da13a_mark_probe_expected_is_sticky_and_independent_of_probes() {
        let wm = AppliedWatermark::new_for_tests();
        assert!(!wm.is_probe_expected());
        wm.mark_probe_expected();
        assert!(wm.is_probe_expected());
        assert_eq!(wm.clean_probe_count(), 0, "marking reports no probe");
        assert!(!wm.is_sink_suspect());
        wm.note_questdb_probe(false);
        wm.note_questdb_probe(true);
        wm.mark_probe_expected();
        assert!(wm.is_probe_expected(), "never cleared");
    }

    #[test]
    fn suspect_episode_count_counts_each_entry_into_suspicion_once() {
        let wm = AppliedWatermark::new_for_tests();
        assert_eq!(wm.suspect_episode_count(), 0);
        wm.note_questdb_probe(true);
        assert_eq!(wm.suspect_episode_count(), 0, "a clean probe is no episode");
        wm.note_questdb_probe(false);
        wm.note_questdb_probe(false);
        assert_eq!(wm.suspect_episode_count(), 1, "one episode, however long");
        wm.note_questdb_probe(true);
        assert_eq!(
            wm.suspect_episode_count(),
            1,
            "recovery keeps the count, so a reader can still see the episode"
        );
        wm.note_questdb_probe(false);
        assert_eq!(wm.suspect_episode_count(), 2);
    }

    /// Audit DA-13b review: the latest episode's start records its number and
    /// the clean-probe count at that moment, so the spill replay can tell
    /// acks confirmed before it from acks it may have lied about.
    #[test]
    fn test_regression_da13b_last_episode_start_records_the_probes_at_its_start() {
        let wm = AppliedWatermark::new_for_tests();
        assert_eq!(wm.last_episode_start(), None, "no episode yet");
        wm.note_questdb_probe(true);
        wm.note_questdb_probe(true);
        wm.note_questdb_probe(true);
        wm.note_questdb_probe(false);
        let (episode, probes, secs) = wm.last_episode_start().expect("an episode began");
        assert_eq!(episode, 1);
        assert_eq!(probes, 3, "three clean probes ran before it");
        assert!(secs > 0);
        wm.note_questdb_probe(false);
        assert_eq!(
            wm.last_episode_start().map(|s| s.0),
            Some(1),
            "a second suspect probe in the same episode does not move it"
        );
        wm.note_questdb_probe(true);
        wm.note_questdb_probe(false);
        assert_eq!(wm.last_episode_start().map(|s| (s.0, s.1)), Some((2, 4)));
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

    /// PR W review round 3: the range written when a suspension episode
    /// ends had no reorder slack on its top. Two socket read tasks can reach
    /// the WAL in one order and the ring in the other, so frame `f` can sit
    /// in segment B below B's first record `b`. If `f` was acked under
    /// suspicion and the clean probe lands before the ack of anything at or
    /// above `b`, the range ended at `parked < b`, B's check range `[b, ..]`
    /// missed it, and once later acks lifted the watermark the next boot
    /// skipped B unread (and the prune could delete it). The end range now
    /// reaches `parked + slack`.
    #[test]
    fn test_regression_episode_end_range_keeps_a_segment_holding_a_reordered_frame() {
        let wm = AppliedWatermark::new_for_tests();
        wm.note_ticks_acked(seq(10));
        wm.note_depth_acked(seq(10));
        wm.note_questdb_probe(true);
        wm.note_questdb_probe(false);
        // `f`, acked while the table may be lying: parked, not applied.
        let parked = seq(20);
        wm.note_ticks_acked(parked);
        wm.note_depth_acked(parked);
        wm.note_questdb_probe(true);
        // B's first record `b`, written to the WAL before `f`.
        let b = (parked + REPLAY_REORDER_SLACK_SEQ / 2) >> 17 << 17;
        assert!(b > parked && b - parked < REPLAY_REORDER_SLACK_SEQ);
        // Later clean acks lift the watermark far past B.
        wm.note_ticks_acked(seq(60));
        wm.note_depth_acked(seq(60));
        let snap = wm.snapshot();
        assert!(
            !snap.frame_is_applied(AppliedSink::Ticks, parked),
            "f is not applied"
        );
        assert!(
            !snap.segment_is_applied(b, seq(40)),
            "segment B (first record above the parked frame) is kept"
        );
        assert!(
            snap.segment_is_applied(parked + 2 * REPLAY_REORDER_SLACK_SEQ, seq(40)),
            "a segment starting more than the slack above is not held"
        );
    }

    #[test]
    fn test_reorder_reach_lo_is_one_slack_lower_and_keeps_zero() {
        assert_eq!(reorder_reach_lo(0), 0, "unknown start stays unknown");
        assert_eq!(reorder_reach_lo(5), 1, "never below 1");
        assert_eq!(reorder_reach_lo(REPLAY_REORDER_SLACK_SEQ), 1);
        assert_eq!(
            reorder_reach_lo(seq(40)),
            seq(40) - REPLAY_REORDER_SLACK_SEQ
        );
    }

    #[test]
    fn test_reorder_reach_hi_is_one_slack_higher_and_saturates() {
        assert_eq!(reorder_reach_hi(0), REPLAY_REORDER_SLACK_SEQ);
        assert_eq!(
            reorder_reach_hi(seq(40)),
            seq(40) + REPLAY_REORDER_SLACK_SEQ
        );
        assert_eq!(reorder_reach_hi(u64::MAX - 1), u64::MAX, "saturates");
    }

    /// PR W review round 4 (C): every transition into suspicion is one
    /// episode, the time spent suspect is counted in whole seconds at each
    /// probe while suspect and at the clean probe that ends it (the
    /// remainder carried), and a clean probe outside suspicion counts
    /// nothing.
    #[test]
    fn test_note_questdb_probe_counts_suspect_episodes_and_seconds() {
        const SEC_NS: u64 = 1_000_000_000;
        let wm = AppliedWatermark::new_for_tests();
        let t0 = 1_000 * SEC_NS;
        wm.note_questdb_probe_at(true, t0);
        assert_eq!(wm.suspect_episode_count(), 0);
        wm.note_questdb_probe_at(false, t0);
        assert_eq!(wm.suspect_episode_count(), 1);
        wm.note_questdb_probe_at(false, t0 + 2_500_000_000);
        assert_eq!(wm.suspect_seconds.load(Ordering::Acquire), 2);
        wm.note_questdb_probe_at(true, t0 + 4 * SEC_NS);
        assert_eq!(
            wm.suspect_seconds.load(Ordering::Acquire),
            4,
            "the half second is carried, not dropped"
        );
        wm.note_questdb_probe_at(true, t0 + 9 * SEC_NS);
        assert_eq!(wm.suspect_seconds.load(Ordering::Acquire), 4, "clean time");
        wm.note_questdb_probe_at(false, t0 + 10 * SEC_NS);
        wm.note_questdb_probe_at(false, t0 + 13 * SEC_NS);
        assert_eq!(wm.suspect_episode_count(), 2, "one per transition");
        assert_eq!(wm.suspect_seconds.load(Ordering::Acquire), 7);
        // The public probe reads the monotonic clock and counts the same way.
        wm.note_questdb_probe(true);
        assert!(!wm.is_sink_suspect());
    }

    /// PR W review round 4 (A): a per-frame shed BUCKET mark carries no
    /// reorder slack. A frame `s` shed just below a `2^38` boundary sits in
    /// segment B whose first record `b` is just above it (`b − s` under one
    /// second, reordered across sockets). B must read unapplied in RAM (the
    /// replay skip and the prune), and again after the boot fold turns the
    /// bucket into a range.
    #[test]
    fn test_regression_bucket_mark_holds_a_segment_starting_just_above_the_boundary() {
        let boundary = ((seq(30) >> UNAPPLIED_BUCKET_SHIFT) + 1) << UNAPPLIED_BUCKET_SHIFT;
        let s = (boundary - REPLAY_REORDER_SLACK_SEQ / 2) >> 17 << 17;
        let b = boundary + 1;
        assert!(s < boundary && b - s < REPLAY_REORDER_SLACK_SEQ);
        let far = boundary + 2 * REPLAY_REORDER_SLACK_SEQ;
        let hi = far + (1 << 20);
        let wm = AppliedWatermark::new_for_tests();
        wm.note_unapplied(s);
        let top = far + (5u64 << UNAPPLIED_BUCKET_SHIFT);
        wm.note_ticks_acked(top);
        wm.note_depth_acked(top);
        let snap = wm.snapshot();
        assert!(!snap.frame_is_applied(AppliedSink::Ticks, s), "s is shed");
        assert!(
            !snap.range_has_unapplied(b, hi),
            "the range lookup itself stays at [first, hi]"
        );
        assert!(
            !snap.segment_is_applied(b, hi),
            "segment B (first record just above the shed bucket) is kept"
        );
        assert!(
            snap.segment_is_applied(far, hi),
            "a segment starting more than the slack above the bucket is not held"
        );
        // The same after the boot fold moves the bucket into a range. The
        // file vouched for everything up to `top`, so the boot tail starts
        // above B and only the folded bucket can hold it.
        wm.boot_applied_floor
            .store(top - REPLAY_REORDER_SLACK_SEQ, Ordering::Release);
        let own_floor = top + (10u64 << UNAPPLIED_BUCKET_SHIFT);
        let fold = wm.fold_at_boot(Some(s), top, own_floor).expect("fold");
        assert_eq!(fold.folded_buckets, 1);
        assert!(
            fold.tail.is_some_and(|(lo, _)| lo > hi),
            "tail starts above B"
        );
        let folded = wm.snapshot();
        assert!(
            folded.buckets.iter().all(|&(id, _)| id == 0),
            "bucket folded"
        );
        assert!(
            !folded.segment_is_applied(b, hi),
            "the folded range carries the slack on its top"
        );
    }

    /// PR W review round 5, the mirror of the round-4 case above: a frame
    /// `f` shed just ABOVE a `2^38` boundary is written as segment A's last
    /// record, and the next segment's first record `g` sits just below the
    /// boundary (minted earlier on another socket, written later; `f − g`
    /// under one second). A's range `[a, g − 1]` ends in the bucket below
    /// `f`'s. A must read unapplied in RAM, and again after the boot fold
    /// turns the bucket into a range.
    #[test]
    fn test_regression_bucket_mark_holds_a_segment_ending_just_below_the_boundary() {
        let slack = REPLAY_REORDER_SLACK_SEQ;
        let boundary = ((seq(30) >> UNAPPLIED_BUCKET_SHIFT) + 1) << UNAPPLIED_BUCKET_SHIFT;
        let f = boundary + (1 << 17);
        let g = (boundary - slack / 2) >> 17 << 17;
        let a = (boundary - 5 * slack) >> 17 << 17;
        assert!(g < boundary && f - g < slack);
        let hi = g - 1;
        let wm = AppliedWatermark::new_for_tests();
        wm.note_unapplied(f);
        let top = boundary + (5u64 << UNAPPLIED_BUCKET_SHIFT);
        wm.note_ticks_acked(top);
        wm.note_depth_acked(top);
        let snap = wm.snapshot();
        assert!(!snap.frame_is_applied(AppliedSink::Ticks, f), "f is shed");
        assert!(
            !snap.range_has_unapplied(a, hi),
            "the range lookup itself stays at [first, hi]"
        );
        assert!(
            !snap.segment_is_applied(a, hi),
            "segment A (range ending just below the shed bucket) is kept"
        );
        assert!(
            snap.segment_is_applied(a, boundary - 2 * slack),
            "a segment ending more than the slack below the bucket is not held"
        );
        wm.boot_applied_floor.store(top - slack, Ordering::Release);
        let own_floor = top + (10u64 << UNAPPLIED_BUCKET_SHIFT);
        let fold = wm.fold_at_boot(Some(a), top, own_floor).expect("fold");
        assert_eq!(fold.folded_buckets, 1);
        assert!(
            fold.tail.is_some_and(|(lo, _)| lo > hi),
            "tail starts above A"
        );
        let folded = wm.snapshot();
        assert!(
            folded.ranges.intersects(a, hi),
            "the folded range carries the slack on its bottom"
        );
        assert!(!folded.segment_is_applied(a, hi), "A is still kept");
        assert!(
            folded.segment_is_applied(a, boundary - 2 * slack),
            "and only within one slack of the bucket"
        );
    }

    /// PR W review round 5: the in-session overflow bound starts one slack
    /// below the lowest frame it covers, so a segment whose range ends just
    /// below that frame (the frame was written into it before the rotation)
    /// stays unapplied once the bit becomes a range.
    #[test]
    fn test_regression_overflow_bound_holds_a_segment_ending_just_below_its_low() {
        let wm = AppliedWatermark::new_for_tests();
        let lo = seq(10);
        let hi = lo + ((UNAPPLIED_BUCKETS as u64 + 1) << UNAPPLIED_BUCKET_SHIFT);
        wm.note_unapplied_range(lo, hi);
        let top = hi + (5u64 << UNAPPLIED_BUCKET_SHIFT);
        wm.note_ticks_acked(top);
        wm.note_depth_acked(top);
        let mut state = *wm.lock_ranges();
        assert!(wm.bound_known_overflow_locked(&mut state));
        *wm.lock_ranges() = state;
        let snap = wm.snapshot();
        assert!(!snap.overflowed, "bounded");
        let s = REPLAY_REORDER_SLACK_SEQ;
        let first = lo - 5 * s;
        assert!(!snap.segment_is_applied(first, lo - s / 2), "held");
        assert!(snap.segment_is_applied(first, lo - 2 * s), "not held");
    }

    /// PR W review round 4 (A): the in-RAM overflow height is the top of a
    /// bucket the table failed to record, with no slack either; a segment
    /// whose first record is within one slack above it is held.
    #[test]
    fn test_regression_overflow_height_holds_a_segment_starting_just_above_it() {
        let wm = AppliedWatermark::new_for_tests();
        let first = seq(30);
        let colliding = first + ((UNAPPLIED_BUCKETS as u64) << UNAPPLIED_BUCKET_SHIFT);
        wm.note_unapplied(first);
        wm.note_unapplied(colliding);
        let top = colliding + (5u64 << UNAPPLIED_BUCKET_SHIFT);
        wm.note_ticks_acked(top);
        wm.note_depth_acked(top);
        let snap = wm.snapshot();
        let height = snap.overflow_high_seq;
        assert!(snap.overflowed && height > colliding && height < u64::MAX);
        let b = height + 1;
        let hi = b + REPLAY_REORDER_SLACK_SEQ * 4;
        assert!(!snap.range_has_unapplied(b, hi), "range lookup unchanged");
        assert!(!snap.segment_is_applied(b, hi), "held by the widened read");
        let far = height + 2 * REPLAY_REORDER_SLACK_SEQ;
        assert!(
            snap.segment_is_applied(far, far + 10),
            "not held further up"
        );
    }

    /// PR W review round 3 (low): both overflow arms lower `overflow_low_seq`
    /// BEFORE they raise `overflow_high_seq`, and the in-session bound reads
    /// the height first. A bound that sees an overflow's height then also
    /// sees its low end (release/acquire), so it never pairs the new height
    /// with a stale, higher low end and writes a range that misses the
    /// overflowed frame. The order cannot be forced in a single-threaded
    /// test, so the source is checked.
    #[test]
    fn test_overflow_arms_lower_the_low_end_before_raising_the_height() {
        let src = include_str!("wal_applied_watermark.rs");
        // O(1) EXEMPT: two fixed function bodies, test only
        for name in [
            "    pub fn note_unapplied(&self, seq: u64) {",
            "    pub fn note_unapplied_range(&self, lo: u64, hi: u64) {",
        ] {
            let start = src.find(name).expect("overflow arm present");
            let body = &src[start..];
            let body = &body[..body.find("\n    }\n").expect("end of the function")];
            let low = body.find("overflow_low_seq").expect("low end written");
            let high = body.find("overflow_high_seq").expect("height written");
            assert!(low < high, "{name}: the low end must be lowered first");
        }
        let bound = &src[src
            .find("    fn bound_known_overflow_locked(")
            .expect("bound present")..];
        let high = bound
            .find("self.overflow_high_seq.load")
            .expect("height read");
        let low = bound.find("self.overflow_low_seq.load").expect("low read");
        assert!(high < low, "the bound reads the height first");
    }

    /// PR W review round 3: the shared whole-segment rule is the range check
    /// on BOTH sinks over `[first, hi]`, and deliberately not below `first`
    /// (the range producers carry the slack on their tops instead; the doc on
    /// `segment_is_applied` records why).
    #[test]
    fn test_segment_is_applied_is_the_range_check_on_both_sinks() {
        let wm = AppliedWatermark::new_for_tests();
        wm.note_ticks_acked(seq(100));
        wm.note_depth_acked(seq(100));
        let first = seq(50);
        assert!(wm.snapshot().segment_is_applied(first, seq(60)));
        wm.add_unapplied_range(first - REPLAY_REORDER_SLACK_SEQ / 2, first - 1);
        assert!(
            wm.snapshot().segment_is_applied(first, seq(60)),
            "a range wholly below the first record does not hold the segment"
        );
        wm.add_unapplied_range(seq(55), seq(55));
        assert!(
            !wm.snapshot().segment_is_applied(first, seq(60)),
            "a range inside"
        );
        let depth = AppliedWatermark::new_for_tests();
        depth.note_ticks_acked(seq(100));
        depth.note_depth_acked(seq(55));
        assert!(
            !depth.snapshot().segment_is_applied(first, seq(60)),
            "the depth watermark has not passed the segment"
        );
        let snap = wm.snapshot();
        assert!(!snap.segment_is_applied(0, seq(60)));
        assert!(!snap.segment_is_applied(seq(60), seq(50)));
        assert!(
            !snap.segment_is_applied(seq(90), seq(200)),
            "above the watermark"
        );
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
    fn test_durable_at_catches_up_to_live_after_a_quiet_lag() {
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
    fn test_persist_at_writes_the_seeded_value_before_any_sample_ages() {
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
    fn test_rescue_floor_is_held_tracks_hold_and_release() {
        for sink in [AppliedSink::Ticks, AppliedSink::Depth] {
            let wm = AppliedWatermark::new_for_tests();
            assert!(!wm.rescue_floor_is_held(sink, seq(50)));
            let floor = wm.hold_rescue_floor(sink, seq(50), seq(60)).expect("slot");
            assert!(wm.rescue_floor_is_held(sink, seq(50)), "{sink:?}");
            assert!(
                !wm.rescue_floor_is_held(sink, seq(51)),
                "exact min_seq only"
            );
            wm.release_rescue_floor(floor);
            assert!(!wm.rescue_floor_is_held(sink, seq(50)), "{sink:?}");
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

    /// DA-12 (2026-10-12): one writer's worst case — every queue slot and
    /// every park slot holding a WAL-backed batch, plus the batch its rescue
    /// thread is writing — takes a floor each, with no full-table fallback.
    #[test]
    fn test_rescue_floor_table_covers_queue_park_and_in_flight() {
        let cases = [
            (
                AppliedSink::Ticks,
                crate::tick_persistence::RESCUE_QUEUE_DEPTH
                    + crate::tick_persistence::PARKED_RESCUE_MAX_BATCHES
                    + 1,
            ),
            (
                AppliedSink::Depth,
                crate::depth_persistence::DEPTH_RESCUE_QUEUE_DEPTH
                    + crate::depth_persistence::DEPTH_PARKED_RESCUE_MAX_BATCHES
                    + 1,
            ),
        ];
        for (sink, live) in cases {
            assert!(live > 8, "the worst case outgrew the old table of 8");
            let wm = AppliedWatermark::new_for_tests();
            for i in 0..live as u64 {
                let lo = seq(100 + i * 10);
                assert!(
                    wm.hold_rescue_floor(sink, lo, lo + 5).is_some(),
                    "{sink:?}: batch {i} of {live} found no floor slot"
                );
            }
            assert_eq!(floors_held(&wm, sink), live);
            assert!(
                !wm.snapshot().overflowed && wm.snapshot().buckets.iter().all(|(id, _)| *id == 0),
                "{sink:?}: no batch fell back to the unapplied mark"
            );
        }
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

    /// PR W review round 4: a range start never falls inside a frame.
    #[test]
    fn test_frame_floor_rounds_down_to_a_frame_base_and_never_to_zero() {
        let base = seq(1_000) & !crate::ws_frame_spill::MAX_PACKET_INDEX;
        assert_eq!(frame_floor(base), base);
        assert_eq!(frame_floor(base + 7), base);
        assert_eq!(
            frame_floor(base + crate::ws_frame_spill::MAX_PACKET_INDEX),
            base
        );
        assert_eq!(frame_floor(0), 1);
        assert_eq!(frame_floor(5), 1);
    }

    /// PR W review round 4 (pass 2): a suspension range starts TWO reorder
    /// slacks below the watermark, at a frame base, and never at 0.
    #[test]
    fn test_suspension_range_lo_is_two_slacks_lower_at_a_frame_base() {
        let base = seq(1_000) & !crate::ws_frame_spill::MAX_PACKET_INDEX;
        let lo = suspension_range_lo(base + 2 * REPLAY_REORDER_SLACK_SEQ + 7);
        assert_eq!(lo, base);
        assert!(suspension_range_lo(seq(1_000) | 5) <= seq(1_000) - 2 * REPLAY_REORDER_SLACK_SEQ);
        assert_eq!(suspension_range_lo(0), 1);
        assert_eq!(suspension_range_lo(REPLAY_REORDER_SLACK_SEQ), 1);
        assert_eq!(suspension_range_lo(2 * REPLAY_REORDER_SLACK_SEQ - 1), 1);
    }

    /// PR W review round 4 (pass 2): a frame `f` acked after the last clean
    /// probe, a quarter slack below its watermark `H`, is the LAST record of
    /// segment S, and the next segment's first record `g` sits 1.2 slacks
    /// below `H` (minted earlier on another socket, written after `f`, so
    /// `g ≥ f − slack`). The next probe is unclean, and the process restarts
    /// before a clean drain. S is judged by `[first(S), g − 1]`, which ends
    /// below a range starting one slack under `H`: S read applied, so the
    /// replay skipped it and the prune could delete it with `f` unapplied.
    #[test]
    fn test_regression_suspension_range_holds_a_segment_whose_last_record_was_acked_late() {
        let slack = REPLAY_REORDER_SLACK_SEQ;
        let dir = std::env::temp_dir().join(format!(
            "tv-wm-susp-two-slacks-{}-{}",
            std::process::id(),
            wall_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("dir");
        let wm = AppliedWatermark::new();
        wm.bind(&dir);
        let h = seq(1_000);
        let first_s = seq(900);
        let g = h - slack - slack / 5;
        let f = h - slack / 4;
        assert!(g >= f - slack, "the reorder bound holds for g after f");
        // `g` and `H` acked, then a clean probe: the last clean watermark is H.
        wm.note_ticks_acked(g);
        wm.note_depth_acked(g);
        wm.note_ticks_acked(h);
        wm.note_depth_acked(h);
        wm.note_questdb_probe(true);
        // `f`, acked after that probe: below H, so it does not move it.
        wm.note_ticks_acked(f);
        wm.note_depth_acked(f);
        wm.note_questdb_probe(false);
        let snap = wm.snapshot();
        assert!(!snap.frame_is_applied(AppliedSink::Ticks, f), "f replays");
        assert!(
            !snap.segment_is_applied(first_s, g - 1),
            "S, whose last record is f, is kept (ranges {:?})",
            snap.ranges.as_slice()
        );
        // The restart: what the next boot loads says the same.
        wm.persist_at(1);
        wm.persist_at(2 + WATERMARK_DURABILITY_LAG_SECS * 1_000_000_000);
        let loaded = AppliedSnapshot::load(&dir).expect("file");
        assert!(
            !loaded.segment_is_applied(first_s, g - 1),
            "after a restart"
        );
        // A segment ending more than two slacks below H is not held.
        assert!(loaded.segment_is_applied(seq(800), h - 2 * slack - 2 * (1 << 17)));
        let _cleanup = std::fs::remove_dir_all(&dir);
    }

    /// PR W review round 6: the same straddle at the PLAIN watermark, with
    /// no suspicion at all. `g` and `H` are acked and durably persisted; `f`
    /// (a quarter slack below `H`, the LAST record of segment S) is never
    /// acked, its flush stuck when the process is killed. The next segment's
    /// first record is `g` (1.2 slacks below `H`, `g ≥ f − slack`), so S is
    /// judged by `[first(S), g − 1]`, which sits under `skip_below` (H less
    /// one slack) and under the boot tail. With one slack S read applied:
    /// the replay archived it unread and the prune could delete it.
    #[test]
    fn test_regression_plain_watermark_straddle_keeps_a_segment_whose_last_record_was_never_acked()
    {
        let slack = REPLAY_REORDER_SLACK_SEQ;
        let dir = std::env::temp_dir().join(format!(
            "tv-wm-plain-straddle-{}-{}",
            std::process::id(),
            wall_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("dir");
        let wm = AppliedWatermark::new();
        wm.bind(&dir);
        let h = seq(1_000);
        let first_s = seq(900);
        let g = h - slack - slack / 5;
        let f = h - slack / 4;
        assert!(g >= f - slack, "the reorder bound holds for g after f");
        wm.note_ticks_acked(g);
        wm.note_depth_acked(g);
        wm.note_ticks_acked(h);
        wm.note_depth_acked(h);
        wm.note_questdb_probe(true);
        wm.persist_at(1);
        wm.persist_at(2 + WATERMARK_DURABILITY_LAG_SECS * 1_000_000_000);
        // The kill: `f` was never acked. The next boot loads the file.
        let loaded = AppliedSnapshot::load(&dir).expect("file");
        assert!(loaded.ranges.is_empty(), "no suspicion: no range at all");
        assert!(!loaded.frame_is_applied(AppliedSink::Ticks, f), "f replays");
        assert!(
            !loaded.segment_is_applied(first_s, g - 1),
            "S, whose last record is the unacked f, is kept and read"
        );
        // A segment ending more than two slacks below H is still applied.
        assert!(loaded.segment_is_applied(seq(800), h - 2 * slack - 2 * (1 << 17)));
        // The boundary is strict: `hi + slack` AT `skip_below` is held.
        let below = loaded.skip_below(AppliedSink::Ticks);
        assert!(!loaded.segment_is_applied(seq(800), below - slack));
        assert!(loaded.segment_is_applied(seq(800), below - slack - 1));
        let _cleanup = std::fs::remove_dir_all(&dir);
    }

    /// PR W review round 6 (low): the suspect-flag check and the watermark
    /// or parked-ack update in both ack functions, and the flag flip and its
    /// reads at both probe transitions, run under the ack gate. With bare
    /// atomics an ack that loaded the flag on one side of a transition and
    /// wrote on the other was in neither episode range. The race needs a
    /// preemption between two adjacent atomics, which a test cannot force,
    /// so the source is checked (and the stress test below checks the
    /// outcome).
    #[test]
    fn test_ack_gate_serialises_the_flag_check_with_the_update() {
        let src = include_str!("wal_applied_watermark.rs");
        // O(1) EXEMPT: two fixed function bodies, test only
        for name in [
            "    pub fn note_ticks_acked(&self, max_seq: u64) {",
            "    pub fn note_depth_acked(&self, max_seq: u64) {",
        ] {
            let start = src.find(name).expect("ack fn present");
            let body = &src[start..];
            let body = &body[..body.find("\n    }\n").expect("end of the function")];
            let gate = body.find("self.lock_ack_gate()").expect("gate taken");
            let flag = body.find("sink_suspect.load").expect("flag read");
            assert!(gate < flag, "{name}: the gate is taken before the flag");
        }
        let start = src
            .find("    fn note_questdb_probe_full(&self, clean: bool, basis: Option<(u64, u64)>, now: u64) {")
            .expect("probe present");
        let body = &src[start..];
        let body = &body[..body.find("\n    }\n").expect("end of the function")];
        // O(1) EXEMPT: two fixed transitions, test only
        for flip in ["sink_suspect.swap(false", "sink_suspect.swap(true"] {
            let at = body.find(flip).expect("transition present");
            let gate = body[..at].rfind("self.lock_ack_gate()").expect("gate");
            let between = &body[gate..at];
            assert!(
                !between.contains("};"),
                "{flip}: the flip sits inside the gated block"
            );
        }
        for read in ["suspect_max_ticks.swap(0", "suspect_max_depth.swap(0"] {
            let at = body.find(read).expect("parked read");
            let flip = body.find("sink_suspect.swap(false").expect("exit flip");
            let block_end = flip + body[flip..].find("};").expect("gated block end");
            assert!(flip < at && at < block_end, "{read} under the exit gate");
        }
    }

    /// PR W review round 6 (low): acks racing many probe transitions. Every
    /// ack that STARTED after a clean probe returned and FINISHED before the
    /// episode's closing probe began lies in an episode range (it either
    /// raised the watermark before the entry, which the entry range reaches,
    /// or parked, which the exit range reaches). Consecutive acks are four
    /// slacks apart, so a range that missed one cannot reach it through its
    /// slack.
    #[test]
    fn test_acks_racing_probe_transitions_are_always_in_an_episode_range() {
        use std::sync::Arc;
        let wm = Arc::new(AppliedWatermark::new_for_tests());
        let started = Arc::new(AtomicU64::new(0));
        let done = Arc::new(AtomicU64::new(0));
        let base = seq(10_000);
        let step = 4 * REPLAY_REORDER_SLACK_SEQ;
        let total: u64 = 20_000;
        let acker = {
            let (wm, started, done) = (Arc::clone(&wm), Arc::clone(&started), Arc::clone(&done));
            std::thread::spawn(move || {
                // O(1) EXEMPT: test-only stress loop
                for i in 1..=total {
                    started.store(i, Ordering::SeqCst);
                    wm.note_ticks_acked(base + i * step);
                    wm.note_depth_acked(base + i * step);
                    done.store(i, Ordering::SeqCst);
                }
            })
        };
        let mut windows: Vec<(u64, u64)> = Vec::new();
        // O(1) EXEMPT: test-only stress loop
        while done.load(Ordering::SeqCst) < total {
            wm.note_questdb_probe(true);
            let after_clean = started.load(Ordering::SeqCst);
            std::thread::yield_now();
            wm.note_questdb_probe(false);
            std::thread::yield_now();
            let before_close = done.load(Ordering::SeqCst);
            wm.note_questdb_probe(true);
            windows.push((after_clean, before_close));
        }
        acker.join().expect("acker");
        let snap = wm.snapshot();
        let mut checked = 0u64;
        for (after_clean, before_close) in windows {
            // O(1) EXEMPT: test-only check
            for i in (after_clean + 1)..=before_close {
                let s = base + i * step;
                checked += 1;
                assert!(
                    snap.ranges.intersects(s, s),
                    "ack {i} ({s}) sat inside an episode and is in no range"
                );
            }
        }
        assert!(checked > 0, "some acks fell inside an episode");
    }

    /// PR W review round 4: an in-RAM overflow of known height becomes a
    /// bounded range at the persist; an unknown height (seeded from a file)
    /// stays the bit for the boot fold.
    #[test]
    fn test_bound_known_overflow_locked_bounds_a_known_height_only() {
        let wm = AppliedWatermark::new_for_tests();
        let lo = seq(10);
        let hi = lo + ((UNAPPLIED_BUCKETS as u64 + 1) << UNAPPLIED_BUCKET_SHIFT);
        wm.note_unapplied_range(lo, hi);
        let mut state = *wm.lock_ranges();
        assert!(wm.bound_known_overflow_locked(&mut state));
        assert!(!wm.overflowed.load(Ordering::Acquire));
        let s = REPLAY_REORDER_SLACK_SEQ;
        assert_eq!(
            state.ranges.as_slice(),
            &[(frame_floor(lo - s), hi + s)],
            "one reorder slack on the top (round 3) and on the bottom (round 5)"
        );
        assert_eq!(
            wm.overflow_high_seq.load(Ordering::Acquire),
            0,
            "round 3: the height is reset after the bound"
        );
        assert_eq!(
            wm.overflow_low_seq.load(Ordering::Acquire),
            u64::MAX,
            "round 3: the low end is reset after the bound"
        );
        assert!(
            !wm.bound_known_overflow_locked(&mut state),
            "nothing left to bound"
        );
        // A second overflow far above is bounded on its own.
        let lo2 = hi + (1_000 << UNAPPLIED_BUCKET_SHIFT);
        let hi2 = lo2 + ((UNAPPLIED_BUCKETS as u64 + 1) << UNAPPLIED_BUCKET_SHIFT);
        wm.note_unapplied_range(lo2, hi2);
        assert!(wm.bound_known_overflow_locked(&mut state));
        assert_eq!(
            state.ranges.as_slice(),
            &[
                (frame_floor(lo - s), hi + s),
                (frame_floor(lo2 - s), hi2 + s)
            ],
            "the gap between the two overflows stays unmarked"
        );
        let seeded = AppliedWatermark::new_for_tests();
        seeded.seed(&AppliedSnapshot {
            overflowed: true,
            overflow_high_seq: u64::MAX,
            ..AppliedSnapshot::default()
        });
        let mut state = *seeded.lock_ranges();
        assert!(!seeded.bound_known_overflow_locked(&mut state));
        assert!(
            seeded.overflowed.load(Ordering::Acquire),
            "unknown height: still the bit"
        );
    }
}
