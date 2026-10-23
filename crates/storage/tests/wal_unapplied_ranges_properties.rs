//! Property test for the bounded WAL unapplied ranges (2026-10-20, plan item
//! PR W): whole process LIFETIMES against a ground-truth model.
//!
//! Each case runs about 40 lifetimes over more than 30 simulated days, so the
//! 128-slot bucket table's ids wrap many times. A lifetime is: bind (load the
//! files the previous lifetime left), the boot fold, a catch-up of random
//! capacity (rounds, a trim after each, then the drained reset or the backlog
//! guard), a live session of acks, sheds, rescue failures and sometimes a
//! QuestDB suspension episode (half of them across an overnight gap, longer
//! than the bucket table covers), with periodic persists (under the real 60 s
//! durability lag) and prunes (with or without a verified S3 copy), and a
//! crash or a clean stop. Crashes land after the fold, between catch-up rounds
//! (before the guard), and mid-session.
//!
//! In half the cases the oldest segment NEVER drains: it sits in
//! `settling/unreadable/`, where the replay never reads it and the trim can
//! never cut below it, and every catch-up ends with the backlog guard instead
//! of the drained reset (2026-10-20, PR W review).
//!
//! **Added 2026-10-20 (PR W review round 3, L6):** the boot fold gets the
//! waiting spans (`fold_at_boot_with_spans`) and the frame counter is
//! ratcheted to `own_floor`, as the production wrapper does; sometimes the
//! clock steps back after the fold; a segment's second record sometimes
//! carries a sequence half a slack BELOW its first (the sixteen-socket race);
//! the catch-up sometimes sees live frames written into the writer's open
//! segment (which the drain never reads), half of them inside a QuestDB
//! suspension episode, before the drained reset; and after the fold, after
//! the guard or reset, and at every clean stop, `applied.tvaw` is decoded
//! the way an OLDER binary decodes it (check **O** below). **Added
//! 2026-10-20 (PR W review round 4, A):** one new segment in ten starts just
//! above a `2^38` bucket boundary and always carries a record half a slack
//! below its first, so a shed of that record marks only the bucket BELOW
//! the segment's first record. A second property
//! (`proptest_stuck_backlog_keeps_later_sessions_prunable`) runs 40 to 55
//! boots over a backlog that never drains (check **M3**). **Added
//! 2026-10-20 (PR W review round 5):** with `mirror` on (half the cases),
//! one segment rotation in ten crosses a `2^38` boundary the other way: the
//! older segment's LAST record is minted just above the boundary and shed,
//! and the next segment's first record just below it (minted earlier,
//! written later), so the shed bucket sits above the older segment's range
//! end (`World::rotate_across_boundary`). **Added 2026-10-20 (PR W review
//! round 4, pass 2):** segments are named in write order (`World::last_name`,
//! as the writer names them), and with `extra` on (half the cases): at a
//! suspicion, half the time the open segment's last record is acked just
//! after a clean probe, a quarter slack below its watermark, while the next
//! segment's first record sits 1.2 slacks below it
//! (`straddle_at_suspicion`); and one segment rotation in ten writes a tiny
//! segment, then a long one whose first record sits half a slack below the
//! tiny one's, with a frame shed in its middle (`rotate_inverted`). Check
//! **B** now walks the segments in write order, where a gap between two
//! neighbours is empty by the reorder bound. **Added 2026-10-21 (PR W
//! review round 6):** with `straddle` on (half the cases), at the end of a
//! session with no open suspicion, half the time the same rotation at the
//! PLAIN watermark (`straddle_at_stop`): `g` and `H` acked and durably
//! persisted, `f` (the older segment's last record) never acked, then an
//! in-session prune and a crash or a clean stop. The sessions above leave
//! only their LAST frames unacked, so no unacked frame ever sat below a later
//! ack outside a suspicion; this one does. Checks **G** and **M3** now allow
//! two slacks below the last ack (the whole-segment rule's second slack).
//!
//! The model knows, for every frame, whether its rows are in the database.
//! Invariants checked on every step:
//!
//! * **Z** — nothing is skipped by the replay (a whole segment, or one frame)
//!   and nothing is pruned unless every frame it drops is applied in the
//!   model.
//! * **R** — the bucket table never overflows: not from the backlog guard
//!   (it writes ranges now), not from a next-day shed colliding with an
//!   earlier day's id (the boot fold empties the table), and not from a
//!   suspension episode across a night (the probe writes ranges now).
//! * **G** — after a clean stop, a session with no shed and no rescue failure
//!   reads its own segments as applied at the next boot (below the last ack
//!   and the reorder slack), so the prune can delete them. Round 5 costs it
//!   nothing: the only extra hold is a segment ending within one slack below
//!   a MARKED bucket's start, and a clean session marks none of its own.
//! * **B** — after every trim no range lies wholly inside a stretch that no
//!   waiting segment can hold (a gap wider than `SPAN_SCAN_GAP_SEQ` between
//!   one segment's last record and the next one's first), and none starts
//!   below the oldest waiting segment less the slack: coverage stays bounded
//!   by what is on disk, even above a backlog that never drains.
//! * **S** — a waiting segment of a clean, shed-free session at least two
//!   lifetimes old (away from its session's edges, where a neighbour's shed
//!   bucket or the next boot's tail legitimately reach) is never tainted, so
//!   the prune can delete it. Checked while no range merge has happened in the
//!   case (32 slots full means two neighbours may have been merged). The
//!   two-bucket margin also covers the round-5 cost (a neighbour's shed
//!   bucket reaching one slack below its start).
//! * **O** — the rollback view: `applied.tvaw` decoded with no sidecar (an
//!   older binary's view) never lets that binary skip or prune a frame the
//!   model says is not in the database. Exempt: a frame protected ONLY by a
//!   per-frame bucket mark (`Frame::shed`) that lies outside its segment's
//!   range in a different bucket (the recorded older-binary limit, both
//!   sides); since round 5 no other frame is exempt. **Round 6:** also a
//!   frame above its segment's range end and above that binary's per-frame
//!   floor (the plain-watermark straddle: an older binary judges whole
//!   segments with one slack).
//! * **M3** — with a backlog that never drains, every segment of every
//!   session is pruned except the ones reaching its last durable ack less
//!   the slack (its tail, at most three per session by the tail alone), the
//!   ones overlapping a shed bucket, and the ones overlapping a suspicion
//!   episode's stretch (round 5), even past 32 islands, and Z holds.
//! * Ranges never exceed 32, and their coverage shrinks only through a trim
//!   or the drained reset.
//!
//! Real code throughout: `AppliedWatermark`, its files, `waiting_segments`,
//! `guard_pending_backlog` and `trim_to_waiting` over real segment files
//! (every frame a CRC-checked record: the replay and the prune read the first
//! record's sequence, the trim's tail probe the last one). The replay and
//! prune DECISIONS are mirrored from `ws_frame_spill` (segment range `[first,
//! next first - 1]`, the newest segment never judged, per-frame
//! `frame_is_applied`).

use std::io::Write as _;
use std::path::{Path, PathBuf};

use proptest::prelude::*;
use tickvault_storage::wal_applied_watermark::{
    APPLIED_WATERMARK_FILE, AppliedSink, AppliedSnapshot, AppliedWatermark,
    REPLAY_REORDER_SLACK_SEQ, UNAPPLIED_BUCKET_SHIFT, UNAPPLIED_BUCKETS,
    WATERMARK_DURABILITY_LAG_SECS,
};
use tickvault_storage::wal_replay_settle::{SETTLING_SUBDIR, SETTLING_UNREADABLE_SUBDIR};
use tickvault_storage::wal_unapplied_ranges::{
    SPAN_SCAN_GAP_SEQ, UNAPPLIED_RANGE_SLOTS, UNAPPLIED_RANGES_FILE, UnappliedRanges,
    guard_pending_backlog, trim_to_waiting, waiting_segments, waiting_spans,
};
use tickvault_storage::ws_frame_spill::{PACKET_INDEX_BITS, WalEndpoint, WsType};

/// A per-process serial for each case's temp folder. The folder name was the
/// process id plus the seed (and the seed's first draw), so two tests in one
/// binary replaying the same pinned seed on two threads wrote the SAME folder
/// at once and read each other's segments (2026-10-22: reproduced 5 of 5 runs
/// under `cargo test`; one thread passes; nextest's one process per test hid
/// it). The serial makes every case's folder its own.
static CASE_DIR_SERIAL: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn next_case_dir_serial() -> u64 {
    CASE_DIR_SERIAL.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

const SEC: u64 = 1_000_000_000;
const BASE_NS: u64 = 1_780_000_000 * SEC;
const LAG: u64 = WATERMARK_DURABILITY_LAG_SECS * SEC;
const LIFETIMES: usize = 40;
const SLACK: u64 = REPLAY_REORDER_SLACK_SEQ;
/// One frame's sequence space: a
/// range's low end is rounded down to a frame base.
const FRAME_BASE: u64 = 1 << PACKET_INDEX_BITS;
/// Segments written before the first lifetime.
const PRE_BOOT: usize = usize::MAX;

/// splitmix64: the proptest seed drives every choice, so a failing case
/// replays exactly.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: u64) -> u64 {
        if n == 0 { 0 } else { self.next() % n }
    }
    fn chance(&mut self, pct: u64) -> bool {
        self.below(100) < pct
    }
}

fn crc32(chunks: &[&[u8]]) -> u32 {
    let mut c: u32 = 0xFFFF_FFFF;
    for chunk in chunks {
        for &b in *chunk {
            c ^= u32::from(b);
            for _ in 0..8 {
                c = if c & 1 != 0 {
                    0xEDB8_8320 ^ (c >> 1)
                } else {
                    c >> 1
                };
            }
        }
    }
    c ^ 0xFFFF_FFFF
}

/// One TVW4 record for frame `seq` (what the writer produces, byte for byte).
fn record_bytes(seq: u64) -> Vec<u8> {
    let ws = WsType::LiveFeed.as_u8();
    let ep = WalEndpoint::MainFeed.as_u8();
    let frame = [0xABu8; 16];
    let seq = seq.to_le_bytes();
    let recv = 7i64.to_le_bytes();
    let len = 16u32.to_le_bytes();
    let crc = crc32(&[&[ws], &seq[..], &recv[..], &[ep], &len[..], &frame[..]]);
    let mut v = Vec::with_capacity(64);
    v.extend_from_slice(b"TVW4");
    v.push(ws);
    v.extend_from_slice(&seq);
    v.extend_from_slice(&recv);
    v.push(ep);
    v.extend_from_slice(&len);
    v.extend_from_slice(&frame);
    v.extend_from_slice(&crc.to_le_bytes());
    v
}

/// A segment file named `name` (its place in write order, as the writer's
/// rising nanosecond names are) whose first record is frame `first`.
fn write_segment(dir: &Path, name: u64, first: u64) -> PathBuf {
    std::fs::create_dir_all(dir).expect("segment dir");
    let path = dir.join(format!("ws-frames-{name:020}.wal"));
    std::fs::write(&path, record_bytes(first)).expect("write segment");
    path
}

/// Appends frame `seq` to the segment at `path`.
fn append_record(path: &Path, seq: u64) {
    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .open(path)
        .expect("open segment");
    f.write_all(&record_bytes(seq)).expect("append record");
}

struct Frame {
    seq: u64,
    applied: bool,
    /// Marked by a per-frame bucket mark (`note_unapplied`, or
    /// `note_unapplied_range` over one frame), the only protection check
    /// **O** may exempt (PR W review round 5).
    shed: bool,
}

struct Seg {
    first: u64,
    frames: Vec<usize>,
    path: PathBuf,
    /// The lifetime that wrote it, or [`PRE_BOOT`].
    lifetime: usize,
    /// It already holds a record below its first (2026-10-20, round 3, L6).
    reordered: bool,
}

/// A session that ended in a clean stop with no shed, rescue failure or
/// suspension episode.
#[derive(Clone, Copy)]
struct CleanSession {
    first: u64,
    last_ack: u64,
}

struct World {
    dir: PathBuf,
    frames: Vec<Frame>,
    /// Segments still on disk, in capture order.
    waiting: Vec<Seg>,
    clock: u64,
    last_minted: u64,
    /// The frame counter's floor: the boot fold ratchets it to `own_floor`
    /// (2026-10-20, round 3, M1), so every mint is above it whatever the
    /// clock reads. The highest FRAME stays in `last_minted`.
    ratchet: u64,
    /// The last segment name written: names rise strictly in write order,
    /// as the writer's do (round 4, pass 2). A segment is named
    /// `max(first, last_name + 1)`, which is its first sequence unless its
    /// first record sits below an earlier segment's (the inversion).
    last_name: u64,
    /// The never-draining backlog segment in `settling/unreadable/`, as
    /// `(first, last)` frame sequence.
    stuck: Option<(u64, u64)>,
    /// Per lifetime: the clean session it ran, if any.
    sessions: Vec<Option<CleanSession>>,
}

impl Drop for World {
    fn drop(&mut self) {
        let _cleanup = std::fs::remove_dir_all(&self.dir);
    }
}

impl World {
    fn mint(&mut self, rng: &mut Rng) -> usize {
        self.clock += SEC * (1 + rng.below(120));
        let seq = self
            .clock
            .max(self.last_minted + SEC)
            .max(self.ratchet + SEC);
        self.last_minted = seq;
        self.frames.push(Frame {
            seq,
            applied: false,
            shed: false,
        });
        self.frames.len() - 1
    }

    /// [`Self::mint`], but the sequence lands just above the next `2^38`
    /// bucket boundary, less than a quarter slack past it (2026-10-20, PR W
    /// review round 4). A segment opened on it, followed by
    /// [`Self::append_below_first`], holds a record just BELOW the boundary
    /// (in the previous bucket) behind a first record just above it: the
    /// shape a per-frame bucket mark with no slack lost. Biased in because a
    /// uniform clock reaches it about once in 500 segments.
    fn mint_above_boundary(&mut self, rng: &mut Rng) -> usize {
        self.clock += SEC * (1 + rng.below(120));
        let floor = self
            .clock
            .max(self.last_minted + SEC)
            .max(self.ratchet + SEC);
        let boundary = ((floor >> UNAPPLIED_BUCKET_SHIFT) + 1) << UNAPPLIED_BUCKET_SHIFT;
        let seq = boundary + 1 + rng.below(SLACK / 4);
        self.clock = seq;
        self.last_minted = seq;
        self.frames.push(Frame {
            seq,
            applied: false,
            shed: false,
        });
        self.frames.len() - 1
    }

    /// PR W review round 5, the mirror of [`Self::mint_above_boundary`]:
    /// mints `low` just below the next `2^38` bucket boundary and `high`
    /// just above it (`high − low` under half a slack), appends `high` to
    /// the newest segment as its LAST record, then opens a new segment on
    /// `low`. `low` was minted first (on another socket) and written later,
    /// so the older segment's range `[first, low − 1]` ends in the bucket
    /// below `high`'s: the shape a bucket read that stopped at the range end
    /// lost. The new segment takes no record below its first. Returns
    /// `(high, low)` as frame indices.
    fn rotate_across_boundary(&mut self, rng: &mut Rng, lifetime: usize) -> (usize, usize) {
        self.clock += SEC * (1 + rng.below(120));
        let floor = self
            .clock
            .max(self.last_minted + SEC)
            .max(self.ratchet + SEC);
        let boundary = (((floor + SLACK) >> UNAPPLIED_BUCKET_SHIFT) + 1) << UNAPPLIED_BUCKET_SHIFT;
        let low = boundary - 1 - rng.below(SLACK / 4);
        let high = boundary + 1 + rng.below(SLACK / 4);
        for seq in [low, high] {
            self.frames.push(Frame {
                seq,
                applied: false,
                shed: false,
            });
        }
        let (low_idx, high_idx) = (self.frames.len() - 2, self.frames.len() - 1);
        self.clock = high;
        self.last_minted = high;
        self.append_frame(high_idx);
        self.open_segment(low_idx, lifetime);
        if let Some(seg) = self.waiting.last_mut() {
            seg.reordered = true;
        }
        (high_idx, low_idx)
    }

    /// The next segment name in write order (see `last_name`).
    fn next_name(&mut self, first: u64) -> u64 {
        self.last_name = first.max(self.last_name + 1);
        self.last_name
    }

    fn open_segment(&mut self, first_frame: usize, lifetime: usize) {
        let first = self.frames[first_frame].seq;
        let name = self.next_name(first);
        let path = write_segment(&self.dir, name, first);
        self.waiting.push(Seg {
            first,
            frames: vec![first_frame],
            path,
            lifetime,
            reordered: false,
        });
    }

    /// Adds frame `f` to the newest segment.
    fn append_frame(&mut self, f: usize) {
        let seq = self.frames[f].seq;
        if let Some(seg) = self.waiting.last_mut() {
            seg.frames.push(f);
            append_record(&seg.path, seq);
        }
    }

    /// Appends to the newest segment a record whose sequence is BELOW the
    /// segment's first, inside the reorder slack (2026-10-20, round 3, L6):
    /// sixteen sockets race to the writer, so a later record can carry an
    /// earlier sequence. At most one per segment, and only as its second
    /// record; every earlier frame is at least a second (one slack) below
    /// the first, so the value is unique.
    fn append_below_first(&mut self) -> Option<usize> {
        let seg = self.waiting.last()?;
        // Only right after the first record: the slack bounds how far a
        // record can sit below one written BEFORE it, so a record half a
        // slack below the first may follow the first, not a later record.
        if seg.reordered || seg.frames.len() != 1 {
            return None;
        }
        let seq = seg.first - SLACK / 2;
        self.frames.push(Frame {
            seq,
            applied: false,
            shed: false,
        });
        let f = self.frames.len() - 1;
        let seg = self.waiting.last_mut()?;
        seg.reordered = true;
        seg.frames.push(f);
        append_record(&seg.path, seq);
        Some(f)
    }

    /// The index of the writer's OPEN segment: the newest one, when the
    /// current lifetime wrote it. The replay never reads it.
    fn open_index(&self, lifetime: usize) -> Option<usize> {
        let last = self.waiting.len().checked_sub(1)?;
        (self.waiting[last].lifetime == lifetime).then_some(last)
    }

    /// The highest frame sequence in `seg` (its last record).
    fn seg_last(&self, seg: &Seg) -> u64 {
        seg.frames
            .iter()
            .map(|&f| self.frames[f].seq)
            .max()
            .unwrap_or(seg.first)
    }

    /// `(lo, hi)` per waiting segment, the replay's bound; the newest has none.
    fn bounds(&self) -> Vec<(u64, Option<u64>)> {
        (0..self.waiting.len())
            .map(|i| {
                (
                    self.waiting[i].first,
                    self.waiting.get(i + 1).map(|n| n.first - 1),
                )
            })
            .collect()
    }

    fn remove_segments(&mut self, idx: &[usize]) {
        for &i in idx.iter().rev() {
            let seg = self.waiting.remove(i);
            std::fs::remove_file(&seg.path).expect("remove segment");
        }
    }
}

/// This binary's whole-segment decision (replay skip and prune), the shared
/// `AppliedSnapshot::segment_is_applied` (PR W review round 3).
fn both_applied(snap: &AppliedSnapshot, lo: u64, hi: u64) -> bool {
    snap.segment_is_applied(lo, hi)
}

/// An older binary's decision, kept separate so the rollback check never
/// follows a later change to this binary's rule.
fn both_applied_older(snap: &AppliedSnapshot, lo: u64, hi: u64) -> bool {
    snap.range_is_applied(AppliedSink::Ticks, lo, hi)
        && snap.range_is_applied(AppliedSink::Depth, lo, hi)
}

fn ack(wm: &AppliedWatermark, seq: u64) {
    wm.note_ticks_acked(seq);
    wm.note_depth_acked(seq);
}

fn check_shape(r: &UnappliedRanges) -> Result<(), TestCaseError> {
    prop_assert!(r.len() <= UNAPPLIED_RANGE_SLOTS);
    for w in r.as_slice().windows(2) {
        prop_assert!(
            w[0].1.saturating_add(1) < w[1].0,
            "ranges sorted and disjoint"
        );
    }
    Ok(())
}

/// 32 slots full means the closest ranges may have been merged (the
/// recorded limit); S is not checked for the rest of the case.
fn note_coarsening(wm: &AppliedWatermark, coarsened: &mut bool) {
    if wm.snapshot().ranges.len() >= UNAPPLIED_RANGE_SLOTS {
        *coarsened = true;
    }
}

/// Z: every frame of `seg` that the caller is about to drop unread.
fn assert_all_applied(world: &World, frames: &[usize], what: &str) -> Result<(), TestCaseError> {
    for &f in frames {
        prop_assert!(
            world.frames[f].applied,
            "Z violated: {what} drops frame {} which is not in the database",
            world.frames[f].seq
        );
    }
    Ok(())
}

/// One catch-up round over the files on disk, mirroring `replay_all`.
fn catchup_round(
    world: &mut World,
    wm: &AppliedWatermark,
    capacity: usize,
    open: Option<usize>,
) -> Result<(), TestCaseError> {
    let snap = AppliedSnapshot::load(&world.dir);
    let bounds = world.bounds();
    let mut consumed = 0usize;
    let mut pending_skips: Vec<usize> = Vec::new();
    let mut archive: Vec<usize> = Vec::new();
    let mut max_refolded = 0u64;
    for (i, &(lo, hi)) in bounds.iter().enumerate() {
        if Some(i) == open {
            break; // the writer's open segment is never read
        }
        let skip = match (&snap, hi) {
            (Some(s), Some(hi)) if hi > 0 => both_applied(s, lo, hi),
            _ => false,
        };
        if skip {
            pending_skips.push(i);
            continue;
        }
        if consumed >= capacity {
            break;
        }
        // The kept segment that follows them was reached: skips archive.
        for &p in &pending_skips {
            assert_all_applied(world, &world.waiting[p].frames, "a segment skip")?;
        }
        archive.append(&mut pending_skips);
        let frames = world.waiting[i].frames.clone();
        for f in frames {
            let s = world.frames[f].seq;
            let dropped = snap
                .as_ref()
                .is_some_and(|sn| sn.frame_is_applied(AppliedSink::Ticks, s));
            if dropped {
                assert_all_applied(world, &[f], "a per-record skip")?;
            } else {
                world.frames[f].applied = true;
                max_refolded = max_refolded.max(s);
            }
        }
        archive.push(i);
        consumed += 1;
    }
    archive.sort_unstable();
    world.remove_segments(&archive);
    if max_refolded > 0 {
        ack(wm, max_refolded);
    }
    Ok(())
}

/// The final pass: `true` when it would return no frame (drained). Every
/// frame it would drop unread is checked against the model (Z).
fn final_pass_is_empty(world: &World, open: Option<usize>) -> Result<bool, TestCaseError> {
    let Some(snap) = AppliedSnapshot::load(&world.dir) else {
        return Ok(world.waiting.iter().all(|seg| seg.frames.is_empty()));
    };
    let mut empty = true;
    for (i, (lo, hi)) in world.bounds().into_iter().enumerate() {
        if Some(i) == open {
            continue; // the writer's open segment is never read
        }
        let frames = &world.waiting[i].frames;
        if let Some(hi) = hi
            && hi > 0
            && both_applied(&snap, lo, hi)
        {
            assert_all_applied(world, frames, "the final pass's segment skip")?;
            continue;
        }
        for &f in frames {
            if snap.frame_is_applied(AppliedSink::Ticks, world.frames[f].seq) {
                assert_all_applied(world, &[f], "the final pass's per-record skip")?;
            } else {
                empty = false;
            }
        }
    }
    Ok(empty)
}

fn prune(world: &mut World, rng: &mut Rng) -> Result<(), TestCaseError> {
    let Some(snap) = AppliedSnapshot::load(&world.dir) else {
        return Ok(());
    };
    let bounds = world.bounds();
    let mut delete = Vec::new();
    for (i, &(lo, hi)) in bounds.iter().enumerate() {
        let Some(hi) = hi else { continue };
        let copied = rng.chance(70);
        if hi >= lo && both_applied(&snap, lo, hi) && copied {
            assert_all_applied(world, &world.waiting[i].frames, "the prune")?;
            delete.push(i);
        }
    }
    world.remove_segments(&delete);
    Ok(())
}

fn persist(wm: &AppliedWatermark, mono: &mut u64, step: u64) {
    *mono += step;
    wm.persist_at(*mono);
}

/// B: right after a trim, no range lies wholly inside a stretch no waiting
/// segment can hold, and none starts below the oldest waiting segment less
/// the slack.
fn check_clipped(world: &World, ranges: &UnappliedRanges) -> Result<(), TestCaseError> {
    // In WRITE order (round 4, pass 2): the stuck segment was written first,
    // then `waiting` in its order. A gap between two segments written one
    // after the other is empty by the reorder bound; between two neighbours
    // in first-sequence order it is not (a long segment written after a tiny
    // one can start below it).
    let segs: Vec<(u64, u64)> = world
        .stuck
        .into_iter()
        .chain(world.waiting.iter().map(|s| (s.first, world.seg_last(s))))
        .collect();
    let Some(lowest) = segs.iter().map(|s| s.0).min() else {
        return Ok(());
    };
    if let Some(&(lo, _)) = ranges.as_slice().first() {
        prop_assert!(
            lo.saturating_add(SLACK) >= lowest,
            "B violated: a range starts below the oldest waiting segment"
        );
    }
    for w in segs.windows(2) {
        let ((a_first, a_last), (b_first, _)) = (w[0], w[1]);
        if b_first.saturating_sub(a_first) <= SPAN_SCAN_GAP_SEQ {
            continue;
        }
        let (gap_lo, gap_hi) = (a_last + SLACK, b_first - SLACK);
        for &(lo, hi) in ranges.as_slice() {
            prop_assert!(
                !(lo > gap_lo && hi < gap_hi),
                "B violated: range [{lo}, {hi}] lies wholly in ({gap_lo}, {gap_hi}), where no \
                 waiting segment can hold a frame"
            );
        }
    }
    Ok(())
}

/// S: an old, clean, shed-free session's waiting segment, away from its
/// session's edges, reads as applied, so the prune can delete it.
fn check_stale_taint(world: &World, lifetime: usize) -> Result<(), TestCaseError> {
    let Some(snap) = AppliedSnapshot::load(&world.dir) else {
        return Ok(());
    };
    let bounds = world.bounds();
    let margin = 2u64 << UNAPPLIED_BUCKET_SHIFT;
    for (i, seg) in world.waiting.iter().enumerate() {
        let j = seg.lifetime;
        if j == PRE_BOOT || j + 2 > lifetime {
            continue;
        }
        let Some(Some(session)) = world.sessions.get(j) else {
            continue;
        };
        let Some(hi) = bounds[i].1 else { continue };
        let Some(next) = world.waiting.get(i + 1) else {
            continue;
        };
        if next.lifetime != j
            || seg.first < session.first + margin
            || hi + margin + SLACK > session.last_ack
        {
            continue;
        }
        prop_assert!(
            both_applied(&snap, seg.first, hi),
            "S violated: segment [{}, {hi}] of clean session {j} still reads unapplied at \
             lifetime {lifetime} (ranges {:?}, overflowed {})",
            seg.first,
            snap.ranges.as_slice(),
            snap.overflowed
        );
    }
    Ok(())
}

/// A backlog segment that never drains: `settling/unreadable/`, where the
/// replay never reads it.
fn write_stuck(world: &mut World, rng: &mut Rng) {
    let dir = world
        .dir
        .join(SETTLING_SUBDIR)
        .join(SETTLING_UNREADABLE_SUBDIR);
    let first_frame = world.mint(rng);
    let first = world.frames[first_frame].seq;
    let name = world.next_name(first);
    let path = write_segment(&dir, name, first);
    let mut last = first;
    for _ in 0..rng.below(3) {
        let f = world.mint(rng);
        last = world.frames[f].seq;
        append_record(&path, last);
    }
    world.stuck = Some((first, last));
}

/// The first lifetime boots onto files an earlier binary left.
fn seed_start_state(world: &mut World, rng: &mut Rng, start: u8, stuck: bool) {
    if stuck {
        write_stuck(world, rng);
    }
    let segs = 1 + rng.below(4);
    for _ in 0..segs {
        let first = world.mint(rng);
        world.open_segment(first, PRE_BOOT);
        for _ in 0..(1 + rng.below(4)) {
            let f = world.mint(rng);
            world.append_frame(f);
        }
    }
    for f in &mut world.frames {
        f.applied = rng.chance(60);
    }
    if start == 0 {
        // No watermark file at all: nothing is vouched for.
        world.clock += 86_400 * SEC;
        return;
    }
    let old = AppliedWatermark::new();
    old.bind(&world.dir);
    if start == 1 {
        // An older binary's unbounded overflow bit.
        old.note_unapplied_range(
            BASE_NS,
            BASE_NS + ((UNAPPLIED_BUCKETS as u64 + 3) << UNAPPLIED_BUCKET_SHIFT),
        );
    } else {
        // Prior-day bucket marks for exactly the frames not in the database.
        for f in &mut world.frames {
            if !f.applied {
                old.note_unapplied(f.seq);
                f.shed = true;
            }
        }
    }
    ack(&old, world.last_minted);
    old.persist_at(1);
    old.persist_at(2 + LAG);
    if start == 1 {
        // Round 4: this binary's persist bounds an overflow of known height
        // into a range (which does not fit the bucket table, so the file's
        // bit is still set for an older binary). Without the sidecar the bit
        // loads at an unknown height: the older binary's shape.
        let _absent = std::fs::remove_file(world.dir.join(UNAPPLIED_RANGES_FILE));
    }
    world.clock += 86_400 * SEC + rng.below(3_600) * SEC;
}

/// Round 4 (finding 1): appends to the writer's open segment a frame a third
/// of a slack BELOW `healthy` (the frame that set the last clean watermark,
/// the newest frame) and acks it after that clean probe. The episode that
/// follows may have lied about it, so the model holds it unapplied. The
/// record is within the slack of every record written before it, so the
/// reorder bound holds.
fn late_reordered_ack(world: &mut World, wm: &AppliedWatermark, healthy: u64) {
    let seq = healthy - SLACK / 3;
    world.frames.push(Frame {
        seq,
        applied: false,
        shed: false,
    });
    let f = world.frames.len() - 1;
    if let Some(seg) = world.waiting.last_mut() {
        seg.frames.push(f);
        append_record(&seg.path, seq);
    }
    ack(wm, seq);
}

/// Round 4, pass 2 (with `extra` on): a rotation across `H − slack` just
/// before a suspicion. Mints `g = H − 1.2 slack`, `f = H − slack / 4` and
/// `H`; writes `f` as the open segment's LAST record, then opens a new
/// segment on `g` (minted earlier on another socket, written after `f`:
/// `g ≥ f − slack`) and appends `H` to it. `g` and `H` are acked before a
/// clean probe (so the last clean watermark is `H`), `f` after it (so the
/// model never counts it applied). The caller then probes unclean. The
/// older segment is judged by `[first, g − 1]`, which ends below a range
/// starting one slack under `H`.
fn straddle_at_suspicion(
    world: &mut World,
    wm: &AppliedWatermark,
    lifetime: usize,
    model_healthy: &mut u64,
) {
    let h = world
        .clock
        .max(world.last_minted + 3 * SLACK)
        .max(world.ratchet + 3 * SLACK);
    let mut push = |seq: u64| {
        world.frames.push(Frame {
            seq,
            applied: false,
            shed: false,
        });
        world.frames.len() - 1
    };
    let g = push(h - SLACK - SLACK / 5);
    let f = push(h - SLACK / 4);
    let top = push(h);
    world.clock = h;
    world.last_minted = h;
    world.append_frame(f);
    world.open_segment(g, lifetime);
    world.append_frame(top);
    for i in [g, top] {
        ack(wm, world.frames[i].seq);
        world.frames[i].applied = true;
    }
    wm.note_questdb_probe(true);
    let snap = wm.snapshot();
    *model_healthy = snap.hwm_ticks.min(snap.hwm_depth);
    ack(wm, world.frames[f].seq);
}

/// Round 6 (with `straddle` on): [`straddle_at_suspicion`]'s rotation at
/// the PLAIN watermark, with no probe at all. Mints `g = H − 1.2 slack`, `f
/// = H − slack / 4` and `H`; writes `f` as the open segment's LAST record,
/// opens a new segment on `g` (`g ≥ f − slack`) and appends `H` to it. `g`
/// and `H` are acked (applied in the model); `f` never is, as when its
/// flush is stuck at a QuestDB stall and the process is killed. The older
/// segment is judged by `[first, g − 1]`, under `skip_below` (`H` less one
/// slack) and under the next boot's tail; only the segment rule's second
/// slack keeps it. Returns `H`.
fn straddle_at_stop(world: &mut World, wm: &AppliedWatermark, lifetime: usize) -> u64 {
    let h = world
        .clock
        .max(world.last_minted + 3 * SLACK)
        .max(world.ratchet + 3 * SLACK);
    let mut push = |seq: u64| {
        world.frames.push(Frame {
            seq,
            applied: false,
            shed: false,
        });
        world.frames.len() - 1
    };
    let g = push(h - SLACK - SLACK / 5);
    let f = push(h - SLACK / 4);
    let top = push(h);
    world.clock = h;
    world.last_minted = h;
    world.append_frame(f);
    world.open_segment(g, lifetime);
    world.append_frame(top);
    for i in [g, top] {
        ack(wm, world.frames[i].seq);
        world.frames[i].applied = true;
    }
    h
}

/// Round 4, pass 2 (with `extra` on): an inversion. Opens a tiny segment A
/// on a fresh frame `a`, then a segment X whose first record `x = a −
/// slack / 2` sits BELOW A's (minted earlier on another socket, written
/// after `a`), and appends two or three frames to X several minutes apart,
/// so X spans more than one bucket and the gap from A's first to the next
/// segment is wide. The middle frame is shed. Returns the frames for the
/// caller's ack-or-shed roll (`a`, `x` and the rest), with the shed one
/// already marked.
fn rotate_inverted(
    world: &mut World,
    wm: &AppliedWatermark,
    rng: &mut Rng,
    lifetime: usize,
) -> Vec<usize> {
    let a = world.mint(rng);
    world.open_segment(a, lifetime);
    world.frames.push(Frame {
        seq: world.frames[a].seq - SLACK / 2,
        applied: false,
        shed: false,
    });
    let x = world.frames.len() - 1;
    world.open_segment(x, lifetime);
    let mut out = vec![a, x];
    let tail = 2 + rng.below(2);
    for i in 0..tail {
        world.clock += SEC * (150 + rng.below(150));
        let f = world.mint(rng);
        world.append_frame(f);
        if i == 0 {
            wm.note_unapplied(world.frames[f].seq);
            world.frames[f].shed = true;
        } else {
            out.push(f);
        }
    }
    out
}

/// The QuestDB probe reports clean again: the parked range is marked and the
/// model's "last clean watermark" moves to the frozen one.
fn end_suspicion(wm: &AppliedWatermark, model_healthy: &mut u64) {
    wm.note_questdb_probe(true);
    let snap = wm.snapshot();
    *model_healthy = snap.hwm_ticks.min(snap.hwm_depth);
}

/// `mirror` turns on the round-5 rotation across a bucket boundary
/// ([`World::rotate_across_boundary`]); off, the random stream is the one
/// the round-4 pinned seeds were found with. `extra` stays off: the pinned
/// seeds keep the stream they were found with.
fn run_case(seed: u64, start: u8, stuck: bool, mirror: bool) -> Result<(), TestCaseError> {
    run_case_with(seed, start, stuck, mirror, false)
}

/// [`run_case`] with `extra` (round 4, pass 2): turns on the rotation across
/// `H − slack` at a suspicion ([`straddle_at_suspicion`]) and the inverted
/// rotation ([`rotate_inverted`]). Off, no extra draw is made.
fn run_case_with(
    seed: u64,
    start: u8,
    stuck: bool,
    mirror: bool,
    extra: bool,
) -> Result<(), TestCaseError> {
    run_case_full(seed, start, stuck, mirror, extra, false)
}

/// [`run_case_with`] with `straddle` (round 6): at the end of a session
/// with no open suspicion, half the time the same rotation across `H −
/// slack` as [`straddle_at_suspicion`] but at the PLAIN watermark
/// ([`straddle_at_stop`]): `g` and `H` acked and durably persisted, `f`
/// (the older segment's last record) never acked, then a crash or a clean
/// stop. Off, no extra draw is made, so the pinned seeds keep their stream.
fn run_case_full(
    seed: u64,
    start: u8,
    stuck: bool,
    mirror: bool,
    extra: bool,
    straddle: bool,
) -> Result<(), TestCaseError> {
    let mut rng = Rng(seed);
    let dir = std::env::temp_dir().join(format!(
        "tv-ranges-prop-{}-{seed:x}-{}-{}",
        std::process::id(),
        rng.next(),
        next_case_dir_serial()
    ));
    std::fs::create_dir_all(&dir).expect("case dir");
    let mut world = World {
        dir,
        frames: Vec::new(),
        waiting: Vec::new(),
        clock: BASE_NS,
        last_minted: BASE_NS,
        ratchet: 0,
        last_name: 0,
        stuck: None,
        sessions: Vec::new(),
    };
    seed_start_state(&mut world, &mut rng, start, stuck);
    let mut coarsened = false;

    for lifetime in 0..LIFETIMES {
        world.sessions.push(None);
        let wm = AppliedWatermark::new();
        wm.bind(&world.dir);
        let mut mono = 1u64;
        let mut model_healthy = {
            let snap = wm.snapshot();
            snap.hwm_ticks.min(snap.hwm_depth)
        };

        // ---- boot fold ----
        let own_floor = (world.last_minted + 1).max(world.clock);
        let waiting = waiting_segments(&world.dir, true);
        let expected_lowest = world
            .stuck
            .map(|s| s.0)
            .into_iter()
            .chain(world.waiting.iter().map(|s| s.first))
            .min();
        prop_assert_eq!(waiting.lowest, expected_lowest);
        prop_assert!(!waiting.listing_failed);
        let oldest = if waiting.unknown {
            None
        } else {
            waiting.lowest
        };
        let spans = waiting_spans(&world.dir).unwrap_or_default();
        let before = wm.snapshot();
        prop_assert!(
            wm.fold_at_boot_with_spans(oldest, world.last_minted, own_floor, &spans)
                .is_some()
        );
        // M1: the production wrapper ratchets the frame counter to
        // `own_floor` before the fold; every mint from here is above it.
        world.ratchet = world.ratchet.max(own_floor);
        if rng.chance(10) {
            // L6: a backward clock step after the fold.
            world.clock = world
                .clock
                .saturating_sub(SEC * 3_600 * (1 + rng.below(3)))
                .max(BASE_NS);
        }
        let after = wm.snapshot();
        prop_assert!(!after.overflowed, "R: no overflow survives the boot fold");
        prop_assert!(after.ranges.covers(&before.ranges), "the fold only adds");
        prop_assert!(
            after
                .buckets
                .iter()
                .all(|&(id, _)| id == 0 || id >= own_floor >> UNAPPLIED_BUCKET_SHIFT),
            "the table holds nothing from an earlier bucket"
        );
        check_shape(&after.ranges)?;
        note_coarsening(&wm, &mut coarsened);
        persist(&wm, &mut mono, SEC);
        check_older_binary(&world)?;
        if rng.chance(5) {
            world.clock += SEC * (60 + rng.below(3_600));
            continue; // crash right after the fold
        }

        // ---- catch-up ----
        // The drain snapshots the frame counter, which the fold's ratchet
        // left at `own_floor`.
        let ceiling = own_floor;
        let rounds = rng.below(4);
        let capacity = 1 + rng.below(3) as usize;
        // L6: live frames (and sometimes a suspension episode) while the
        // drain runs, before the drained reset.
        let live_at = if rng.chance(30) {
            Some(rng.below(rounds + 1))
        } else {
            None
        };
        let mut catchup_clean = true;
        let mut crashed = false;
        for round in 0..=rounds {
            if live_at == Some(round) {
                catchup_clean &=
                    catchup_live_frames(&mut world, &wm, &mut rng, lifetime, &mut model_healthy);
            }
            if round == rounds {
                break;
            }
            let open = world.open_index(lifetime);
            catchup_round(&mut world, &wm, capacity, open)?;
            let trimmed = trim_to_waiting(&wm, &world.dir);
            let ranges = wm.snapshot().ranges;
            check_shape(&ranges)?;
            note_coarsening(&wm, &mut coarsened);
            if trimmed.is_some() {
                check_clipped(&world, &ranges)?;
            }
            persist(&wm, &mut mono, SEC);
            if rng.chance(5) {
                crashed = true; // crash before the guard
                break;
            }
        }
        if crashed {
            world.clock += SEC * (60 + rng.below(3_600));
            continue;
        }
        let open = world.open_index(lifetime);
        let drained = rounds > 0 && final_pass_is_empty(&world, open)?;
        if drained && !stuck {
            wm.reset_unapplied_below(ceiling);
        } else {
            // A backlog that never drains always ends here.
            let before = wm.snapshot().ranges;
            let _guarded = guard_pending_backlog(&wm, &world.dir, ceiling);
            let after = wm.snapshot();
            prop_assert!(after.ranges.covers(&before), "the guard only adds");
            prop_assert!(!after.overflowed, "R: the guard never overflows the table");
        }
        check_shape(&wm.snapshot().ranges)?;
        note_coarsening(&wm, &mut coarsened);
        persist(&wm, &mut mono, SEC);
        check_older_binary(&world)?;
        if !coarsened {
            check_stale_taint(&world, lifetime)?;
        }

        // ---- live session ----
        let frames = rng.below(30);
        let seg_size = 2 + rng.below(5);
        let shed_pct = if rng.chance(50) { 0 } else { 20 };
        let suspicion_at = if rng.chance(20) {
            Some(rng.below(frames.max(1)))
        } else {
            None
        };
        let suspicion_len = 1 + rng.below(8);
        let mut suspect_until: Option<u64> = None;
        let mut clean_session = catchup_clean;
        let mut last_acked = 0u64;
        let mut crashed = false;
        let mut session_first: Option<u64> = None;
        for k in 0..frames {
            if suspect_until.is_none() && rng.chance(30) {
                // Round 4: a routine clean probe moves the last clean
                // watermark (the episode's range starts from it).
                wm.note_questdb_probe(true);
                let snap = wm.snapshot();
                model_healthy = snap.hwm_ticks.min(snap.hwm_depth);
            }
            if suspicion_at == Some(k) {
                // Round 4, pass 2 (`extra` only, so the pinned seeds keep
                // their stream): sometimes the open segment's last record is
                // acked just after a clean probe, a quarter slack below its
                // watermark, and the next segment's first record sits 1.2
                // slacks below it.
                if extra && world.open_index(lifetime).is_some() && rng.chance(50) {
                    straddle_at_suspicion(&mut world, &wm, lifetime, &mut model_healthy);
                }
                // Round 4 (finding 1): sometimes a frame minted just before
                // the one that set the clean watermark is acked AFTER that
                // probe, below it (the ring reorders sixteen sockets).
                if rng.chance(50)
                    && world.open_index(lifetime).is_some()
                    && world.last_minted == model_healthy
                {
                    late_reordered_ack(&mut world, &wm, model_healthy);
                }
                wm.note_questdb_probe(false);
                clean_session = false;
                // Everything acked since the last clean probe may be a lie.
                for f in &mut world.frames {
                    if f.seq >= model_healthy {
                        f.applied = false;
                    }
                }
                suspect_until = Some(k + suspicion_len);
                if rng.chance(50) {
                    // The episode runs across a night: longer than the
                    // bucket table's ~9.8 h.
                    world.clock += 16 * 3_600 * SEC;
                }
                note_coarsening(&wm, &mut coarsened);
            }
            if suspect_until == Some(k) {
                end_suspicion(&wm, &mut model_healthy);
                suspect_until = None;
                note_coarsening(&wm, &mut coarsened);
            }
            // Round 4 (A): sometimes a new segment starts just above a bucket
            // boundary, and then always gets a record below its first.
            let at_boundary = k % seg_size == 0 && rng.chance(10);
            // Round 5: sometimes the rotation crosses a boundary the other
            // way, with the high record (the older segment's last) shed.
            // Short-circuits before any draw when `mirror` is off.
            let rotate = mirror
                && k % seg_size == 0
                && !at_boundary
                && world.open_index(lifetime).is_some()
                && rng.chance(10);
            // Round 4, pass 2 (`extra` only): sometimes a tiny segment, then
            // a long one whose first record sits below the tiny one's.
            let invert = extra
                && k % seg_size == 0
                && !at_boundary
                && !rotate
                && world.open_index(lifetime).is_some()
                && rng.chance(10);
            let mut inverted: Vec<usize> = Vec::new();
            let f = if rotate {
                let (high, low) = world.rotate_across_boundary(&mut rng, lifetime);
                wm.note_unapplied(world.frames[high].seq);
                world.frames[high].shed = true;
                clean_session = false;
                low
            } else if invert {
                inverted = rotate_inverted(&mut world, &wm, &mut rng, lifetime);
                clean_session = false;
                inverted[0]
            } else if at_boundary {
                world.mint_above_boundary(&mut rng)
            } else {
                world.mint(&mut rng)
            };
            let s = world.frames[f].seq;
            session_first.get_or_insert(s);
            let mut batch = vec![f];
            if rotate {
                // The rotation already opened the segment on `low`.
            } else if invert {
                // Both segments are open and written; the shed one is marked.
                batch = inverted;
            } else if k % seg_size == 0 {
                world.open_segment(f, lifetime);
                // L6: a record below its segment's first, inside the slack.
                if (at_boundary || rng.chance(15))
                    && let Some(below) = world.append_below_first()
                {
                    batch.push(below);
                }
            } else {
                world.append_frame(f);
            }
            for f in batch {
                let s = world.frames[f].seq;
                let roll = rng.below(100);
                if roll < shed_pct {
                    wm.note_unapplied(s);
                    world.frames[f].shed = true;
                    clean_session = false;
                } else if roll < shed_pct + 5 && shed_pct > 0 {
                    wm.note_unapplied_range(s, s);
                    world.frames[f].shed = true;
                    clean_session = false;
                } else {
                    ack(&wm, s);
                    if suspect_until.is_none() {
                        // Acked while suspect is parked, not applied.
                        world.frames[f].applied = true;
                        last_acked = last_acked.max(s);
                    }
                }
            }
            prop_assert!(!wm.snapshot().overflowed, "R: no in-session overflow");
            if rng.chance(30) {
                persist(&wm, &mut mono, SEC * (1 + rng.below(120)));
            }
            if rng.chance(10) {
                prune(&mut world, &mut rng)?;
            }
            if rng.chance(2) {
                crashed = true;
                break;
            }
        }
        // Round 6 (`straddle` only): the plain-watermark straddle, then a
        // crash half the time (the kill while `f`'s flush is stuck).
        if straddle
            && !crashed
            && suspect_until.is_none()
            && world.open_index(lifetime).is_some()
            && rng.chance(50)
        {
            let h = straddle_at_stop(&mut world, &wm, lifetime);
            last_acked = last_acked.max(h);
            persist(&wm, &mut mono, SEC);
            persist(&wm, &mut mono, LAG + SEC);
            // The in-session prune reads the durable view, where `H` is
            // vouched for and `f` is not (`segment_is_applied`'s second
            // slack keeps the older segment).
            prune(&mut world, &mut rng)?;
            crashed = rng.chance(50);
        }
        if !crashed && suspect_until.is_some() && rng.chance(50) {
            end_suspicion(&wm, &mut model_healthy);
            prop_assert!(!wm.snapshot().overflowed, "R: a night-long episode");
            note_coarsening(&wm, &mut coarsened);
        }
        if !crashed {
            // Clean stop: a persist, then one past the durability lag.
            persist(&wm, &mut mono, SEC);
            persist(&wm, &mut mono, LAG + SEC);
            check_older_binary(&world)?;
            if clean_session && last_acked > 0 {
                check_g(&world, own_floor, session_first, last_acked)?;
                if let Some(first) = session_first {
                    world.sessions[lifetime] = Some(CleanSession {
                        first,
                        last_ack: last_acked,
                    });
                }
            }
        }
        // Overnight, a weekend, or a quick restart.
        world.clock += match rng.below(4) {
            0 => SEC * (60 + rng.below(600)),
            1 => 86_400 * SEC,
            2 => 3 * 86_400 * SEC,
            _ => SEC * 3_600 * (1 + rng.below(20)),
        };
    }
    Ok(())
}

/// L6 (2026-10-20, round 3): one to three live frames written while the
/// catch-up drain runs, into a new segment of this lifetime (the writer's
/// open segment, which the drain never reads), half of them inside a
/// QuestDB suspension episode that opens and closes around their acks.
/// Returns `false` when the episode ran (the session is then not clean).
fn catchup_live_frames(
    world: &mut World,
    wm: &AppliedWatermark,
    rng: &mut Rng,
    lifetime: usize,
    model_healthy: &mut u64,
) -> bool {
    let suspect = rng.chance(50);
    if suspect {
        wm.note_questdb_probe(false);
        for f in &mut world.frames {
            if f.seq >= *model_healthy {
                f.applied = false;
            }
        }
    }
    let first = world.mint(rng);
    world.open_segment(first, lifetime);
    let mut written = vec![first];
    for _ in 0..rng.below(3) {
        let f = world.mint(rng);
        world.append_frame(f);
        written.push(f);
    }
    for f in written {
        ack(wm, world.frames[f].seq);
        // Acked while suspect is parked, not applied.
        world.frames[f].applied = !suspect;
    }
    if suspect {
        end_suspicion(wm, model_healthy);
    }
    !suspect
}

/// L6 (2026-10-20, round 3): the ROLLBACK view. `applied.tvaw` decoded the
/// way an older binary decodes it (version 1, no sidecar: a range-encoded
/// bucket is an ordinary bucket and the overflow bit is unbounded) must
/// never let that binary skip or prune a frame the model says is not in the
/// database.
fn check_older_binary(world: &World) -> Result<(), TestCaseError> {
    let Ok(bytes) = std::fs::read(world.dir.join(APPLIED_WATERMARK_FILE)) else {
        return Ok(());
    };
    let Some(old) = AppliedSnapshot::from_bytes(&bytes, u64::MAX) else {
        return Ok(());
    };
    for (i, (lo, hi)) in world.bounds().into_iter().enumerate() {
        let frames = &world.waiting[i].frames;
        if let Some(hi) = hi
            && hi >= lo
            && both_applied_older(&old, lo, hi)
        {
            // Recorded rollback limit (PR W review round 4, A; the upper
            // side added in round 5): an older binary reads the bucket marks
            // over the segment's range `[first, hi]` only, so a record
            // reordered below a bucket boundary behind a first record above
            // it, or above a boundary past a range end below it, is not
            // protected by its shed bucket there. Every older binary has
            // always had this gap; this binary closes it
            // (`segment_is_applied`), and the file cannot carry the fix to
            // code that does not read it. **⚠ NARROWED (round 5):** this
            // exempted EVERY frame outside the range's buckets, whatever
            // protected it; now only a frame whose protection is a per-frame
            // bucket mark (`shed`), so a range that stops covering such a
            // frame in the older binary's view still fails here.
            //
            // Round 6, a second recorded rollback limit: an older binary
            // judges a whole segment against its ack watermark with ONE
            // slack, so a frame reordered ABOVE the segment's range end and
            // above that binary's own per-frame floor (`skip_below`: it
            // would replay the frame if it read the segment) is lost with
            // the segment, the plain-watermark straddle. Every older binary
            // has this gap (the one-slack skip predates PR W); this binary
            // closes it (`segment_is_applied` needs two slacks).
            let floor = old
                .skip_below(AppliedSink::Ticks)
                .min(old.skip_below(AppliedSink::Depth));
            let checked: Vec<usize> = frames
                .iter()
                .copied()
                .filter(|&f| {
                    let frame = &world.frames[f];
                    let (seq, sh) = (frame.seq, UNAPPLIED_BUCKET_SHIFT);
                    let below = seq < lo && seq >> sh != lo >> sh;
                    let above = seq > hi && seq >> sh != hi >> sh;
                    let straddle = seq > hi && seq > floor;
                    !(frame.shed && (below || above)) && !straddle
                })
                .collect();
            assert_all_applied(world, &checked, "an older binary's segment skip or prune")?;
            continue;
        }
        for &f in frames {
            if old.frame_is_applied(AppliedSink::Ticks, world.frames[f].seq) {
                assert_all_applied(world, &[f], "an older binary's per-record skip")?;
            }
        }
    }
    Ok(())
}

/// M3 (2026-10-20, round 3): a backlog that never drains. The oldest
/// segment sits in `settling/unreadable/` (never read), no catch-up round
/// ever reaches a later segment, and every session ends with its last
/// frames unacknowledged. Each boot's tail then keeps that session's last
/// segment for good: one island per boot, more than 40 of them, so the 32
/// range slots must merge. Every OTHER segment of every session must still
/// become prunable (the merge goes across stretches with nothing waiting,
/// and the guard adds no second island), and Z holds throughout.
///
/// **Round 4 (findings 2 and 4):** an EMPTY segment (a process stopped in
/// its catch-up drain) and a segment whose first record is TORN (its second
/// record an unacknowledged frame) are left in the live directory early on
/// and never removed; the trim and the fold must still work around them,
/// and the torn segment's frame must stay protected. And about a quarter of
/// the sessions END IN SUSPICION (a QuestDB probe saw a lagging or
/// suspended table and no clean probe followed before the stop), with
/// routine clean probes before it. The honest bound for such a session is
/// not "three segments": every segment reaching above its last CONFIRMED
/// watermark less the slack stays pinned until the backlog drains, because
/// none of those acks was ever confirmed. Only the clean sessions keep the
/// three-segment bound.
///
/// **Round 5 (two lows and a medium):** about a third of the sessions shed
/// one frame mid-session (a ring shed, an append or a rescue failure:
/// `note_unapplied`), and half the suspicion episodes CLOSE with a clean
/// probe before the stop. On a backlog that never drains, a session also
/// keeps for good every segment that overlaps one of its shed buckets (with
/// the reorder slack on both sides) and every segment that overlaps a
/// closed episode's stretch (from its last clean watermark less the slack to
/// its highest parked ack plus the slack). The bound checked is therefore:
/// each kept segment reaches the tail, overlaps a shed bucket, or overlaps
/// an episode stretch, and at most three per session are kept by the tail
/// alone.
fn run_stuck_case(seed: u64, boots: usize) -> Result<(), TestCaseError> {
    let mut rng = Rng(seed);
    let dir = std::env::temp_dir().join(format!(
        "tv-ranges-stuck-{}-{seed:x}-{}-{}",
        std::process::id(),
        rng.next(),
        next_case_dir_serial()
    ));
    std::fs::create_dir_all(&dir).expect("case dir");
    let mut world = World {
        dir,
        frames: Vec::new(),
        waiting: Vec::new(),
        clock: BASE_NS,
        last_minted: BASE_NS,
        ratchet: 0,
        last_name: 0,
        stuck: None,
        sessions: Vec::new(),
    };
    write_stuck(&mut world, &mut rng);
    // Per lifetime: the watermark below which (less the slack) every frame
    // is confirmed applied (`None` when nothing was acked).
    let mut last_acks: Vec<Option<u64>> = Vec::new();
    // Per lifetime: it ended in suspicion.
    let mut suspicious: Vec<bool> = Vec::new();
    // The intact frame behind the torn first record, once written.
    let mut torn_frame: Option<u64> = None;
    // Round 5: every shed bucket `[lo, hi]` and every closed episode's
    // stretch `[lo, hi]`, with the slack each mark carries already applied.
    let mut shed_reach: Vec<(u64, u64)> = Vec::new();
    let mut episode_reach: Vec<(u64, u64)> = Vec::new();
    let mut merged_once = false;
    for lifetime in 0..boots {
        let wm = AppliedWatermark::new();
        wm.bind(&world.dir);
        let mut mono = 1u64;
        let own_floor = (world.last_minted + 1).max(world.clock);
        let waiting = waiting_segments(&world.dir, true);
        prop_assert!(
            !waiting.unknown,
            "an empty or a neighbour-bounded torn segment is not unknown"
        );
        let oldest = if waiting.unknown {
            None
        } else {
            waiting.lowest
        };
        let spans = waiting_spans(&world.dir).unwrap_or_default();
        prop_assert!(!spans.is_empty(), "the spans are known");
        prop_assert!(
            wm.fold_at_boot_with_spans(oldest, world.last_minted, own_floor, &spans)
                .is_some()
        );
        world.ratchet = world.ratchet.max(own_floor);
        prop_assert!(trim_to_waiting(&wm, &world.dir).is_some(), "the trim runs");
        let _guard = guard_pending_backlog(&wm, &world.dir, own_floor);
        let ranges = wm.snapshot().ranges;
        check_shape(&ranges)?;
        merged_once |= ranges.len() >= UNAPPLIED_RANGE_SLOTS;
        persist(&wm, &mut mono, SEC);

        // A session: every frame acknowledged except the last one or two,
        // routine clean probes, and sometimes a suspicion that is still
        // open at the stop.
        let mut model_healthy = {
            let snap = wm.snapshot();
            snap.hwm_ticks.min(snap.hwm_depth)
        };
        let frames = 4 + rng.below(12);
        let unacked = 1 + rng.below(2);
        let seg_size = 2 + rng.below(4);
        let suspicion_at = rng.chance(25).then(|| rng.below(frames));
        // Round 5: half the episodes close with a clean probe 1 to 3 frames
        // later (when the session runs that long).
        let close_after = 1 + rng.below(3);
        let closes = rng.chance(50);
        let shed_at = rng.chance(33).then(|| rng.below(frames));
        let mut suspect = false;
        // The episode's stretch: `[last clean watermark, highest ack]`.
        let mut stretch = (0u64, 0u64);
        let mut last_ack = None;
        for k in 0..frames {
            if suspicion_at == Some(k) {
                let snap = wm.snapshot();
                wm.note_questdb_probe(false);
                suspect = true;
                stretch = (model_healthy, snap.hwm_ticks.min(snap.hwm_depth));
                // Everything acked since the last clean probe may be a lie.
                for f in &mut world.frames {
                    if f.seq >= model_healthy {
                        f.applied = false;
                    }
                }
            }
            if suspect && closes && suspicion_at.is_some_and(|at| at + close_after == k) {
                wm.note_questdb_probe(true);
                suspect = false;
                let snap = wm.snapshot();
                model_healthy = snap.hwm_ticks.min(snap.hwm_depth);
                episode_reach.push((
                    stretch.0.saturating_sub(2 * SLACK + FRAME_BASE),
                    stretch.1 + SLACK,
                ));
            }
            let f = world.mint(&mut rng);
            if k % seg_size == 0 {
                world.open_segment(f, lifetime);
            } else {
                world.append_frame(f);
            }
            if shed_at == Some(k) {
                // A ring shed (or an append or rescue failure): never acked.
                let s = world.frames[f].seq;
                wm.note_unapplied(s);
                world.frames[f].shed = true;
                let id = s >> UNAPPLIED_BUCKET_SHIFT;
                shed_reach.push((
                    (id << UNAPPLIED_BUCKET_SHIFT).saturating_sub(SLACK + FRAME_BASE),
                    ((id + 1) << UNAPPLIED_BUCKET_SHIFT) - 1 + SLACK,
                ));
            } else if k + unacked < frames {
                let s = world.frames[f].seq;
                ack(&wm, s);
                if suspect {
                    stretch.1 = stretch.1.max(s);
                } else {
                    world.frames[f].applied = true;
                    last_ack = Some(s);
                    if rng.chance(50) {
                        wm.note_questdb_probe(true);
                        let snap = wm.snapshot();
                        model_healthy = snap.hwm_ticks.min(snap.hwm_depth);
                    }
                }
            }
        }
        if suspect {
            // Confirmed: below the last clean watermark less TWO slacks (and
            // a frame base), where the open episode's range starts (round
            // 4, pass 2). `reaches_tail` allows two slacks above it (round
            // 6, the segment rule's second slack), one more than needed.
            last_ack = Some(model_healthy.saturating_sub(SLACK + FRAME_BASE));
        }
        persist(&wm, &mut mono, SEC);
        persist(&wm, &mut mono, LAG + SEC);
        last_acks.push(last_ack);
        suspicious.push(suspect);

        // Round 4 (finding 2): an empty and a torn segment, never removed.
        if lifetime == 1 {
            let name = world.last_minted + 1;
            std::fs::write(world.dir.join(format!("ws-frames-{name:020}.wal")), b"")
                .expect("empty segment");
        }
        if lifetime == 2 {
            let f0 = world.mint(&mut rng);
            let f1 = world.mint(&mut rng);
            let (s0, s1) = (world.frames[f0].seq, world.frames[f1].seq);
            let mut bytes = record_bytes(s0);
            let n = bytes.len();
            bytes[n - 1] ^= 0xFF;
            bytes.extend_from_slice(&record_bytes(s1));
            std::fs::write(world.dir.join(format!("ws-frames-{s0:020}.wal")), bytes)
                .expect("torn segment");
            torn_frame = Some(s1);
        }

        // Everything copied: the prune deletes whatever reads as applied.
        let Some(snap) = AppliedSnapshot::load(&world.dir) else {
            prop_assert!(false, "a clean stop leaves a readable file");
            return Ok(());
        };
        if let Some(s1) = torn_frame {
            prop_assert!(
                !snap.frame_is_applied(AppliedSink::Ticks, s1),
                "Z violated: the torn segment's unacknowledged frame {s1} reads applied"
            );
        }
        let bounds = world.bounds();
        let mut delete = Vec::new();
        for (i, &(lo, hi)) in bounds.iter().enumerate() {
            let Some(hi) = hi else { continue };
            if hi >= lo && both_applied(&snap, lo, hi) {
                assert_all_applied(&world, &world.waiting[i].frames, "the prune")?;
                delete.push(i);
            }
        }
        world.remove_segments(&delete);
        check_older_binary(&world)?;

        // What may still wait: the newest segment, and a segment of an
        // earlier session whose prune range reaches above that session's
        // confirmed watermark less the slack (its tail, or its unconfirmed
        // stretch when it ended in suspicion), or (round 5) overlaps one of
        // the shed buckets or closed episode stretches, which a backlog that
        // never drains also keeps for good.
        let bounds = world.bounds();
        let mut tail_only = vec![0usize; lifetime + 1];
        for (i, seg) in world.waiting.iter().enumerate() {
            let Some(hi) = bounds[i].1 else { continue };
            let j = seg.lifetime;
            let lo = seg.first;
            let overlaps = |r: &[(u64, u64)]| r.iter().any(|&(a, b)| lo <= b && hi >= a);
            let by_mark = overlaps(&shed_reach) || overlaps(&episode_reach);
            // Round 6: two slacks, as `segment_is_applied` now requires
            // below an ack watermark.
            let reaches_tail = last_acks[j].is_none_or(|ack| hi + 2 * SLACK >= ack);
            if reaches_tail && !by_mark {
                tail_only[j] += 1;
            }
            prop_assert!(
                reaches_tail || by_mark,
                "M3 violated: segment [{lo}, {hi}] of session {j} is confirmed, below its \
                 session's tail, overlaps no shed bucket or episode, and is still waiting \
                 after boot {lifetime} (ranges {:?})",
                snap.ranges.as_slice()
            );
        }
        for (j, &kept) in tail_only.iter().enumerate() {
            prop_assert!(
                suspicious[j] || kept <= 3,
                "clean session {j} keeps {kept} segments by its tail alone after boot {lifetime}"
            );
        }
        world.clock += SEC * 3_600 * (1 + rng.below(30));
    }
    prop_assert!(merged_once, "the case filled the 32 range slots");
    Ok(())
}

/// G: a session with no shed and no rescue failure reads its own segments
/// as applied after a clean stop.
fn check_g(
    world: &World,
    own_floor: u64,
    session_first: Option<u64>,
    last_acked: u64,
) -> Result<(), TestCaseError> {
    let Some(session_first) = session_first else {
        return Ok(());
    };
    let snap = AppliedSnapshot::load(&world.dir);
    prop_assert!(snap.is_some(), "a clean stop leaves a readable file");
    let Some(snap) = snap else { return Ok(()) };
    // Past the bucket the boot shared with the previous process, plus one
    // reorder slack (PR W review round 4): a whole-segment check reads the
    // bucket marks from one slack below the segment's first record, so a
    // segment starting within one second above that bucket's end is held
    // by it. That is the stated cost of the widened read.
    let shared_top = (((own_floor >> UNAPPLIED_BUCKET_SHIFT) + 1) << UNAPPLIED_BUCKET_SHIFT)
        + REPLAY_REORDER_SLACK_SEQ;
    for (lo, hi) in world.bounds() {
        let Some(hi) = hi else { continue };
        if lo < session_first || lo <= shared_top {
            continue;
        }
        // Round 6: a whole segment needs TWO slacks below the ack watermark
        // (`AppliedSnapshot::segment_is_applied`), so one ending within two
        // slacks below the last ack is held (the stated cost).
        if hi + 2 * REPLAY_REORDER_SLACK_SEQ < last_acked {
            prop_assert!(
                both_applied(&snap, lo, hi),
                "G violated: an acked, shed-free segment [{lo}, {hi}] still reads unapplied \
                 (ranges {:?}, overflowed {})",
                snap.ranges.as_slice(),
                snap.overflowed
            );
        }
    }
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    #[test]
    fn proptest_wal_lifetimes_never_skip_or_prune_an_unapplied_frame(
        seed in any::<u64>(),
        start in 0u8..3,
        stuck in any::<bool>(),
        mirror in any::<bool>(),
        extra in any::<bool>(),
        straddle in any::<bool>(),
    ) {
        run_case_full(seed, start, stuck, mirror, extra, straddle)?;
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    #[test]
    fn proptest_stuck_backlog_keeps_later_sessions_prunable(
        seed in any::<u64>(),
        boots in 40usize..56,
    ) {
        run_stuck_case(seed, boots)?;
    }
}

/// PR W review round 4 (A): two cases the boundary-biased generator found,
/// pinned so they run on every build whatever the random seeds. With the
/// whole-segment check reading the bucket marks from the first record (the
/// old rule), seed `16702864118144502203` fails Z (the prune drops a frame
/// shed just below a bucket boundary behind a first record just above it);
/// seed `3747820598283899350` reaches the recorded older-binary limit in
/// check O.
#[test]
fn test_regression_round4_bucket_boundary_reorder_seeds() {
    run_case(16_702_864_118_144_502_203, 0, false, false).expect("Z across a bucket boundary");
    run_case(3_747_820_598_283_899_350, 0, true, false).expect("older-binary limit is recorded");
}

/// PR W review round 5: cases the mirror generator
/// ([`World::rotate_across_boundary`]) found, pinned so they run on every
/// build whatever the random seeds. Each fails Z under the old rules: the
/// first two with the boot fold's bucket ranges starting AT the bucket base
/// (no slack on the bottom), the last two with the whole-segment bucket read
/// stopping at the segment's range end (no slack above it). Found by
/// sweeping 600 seeds at three start states each with each half reverted.
#[test]
fn test_regression_round5_mirror_boundary_rotation_seeds() {
    run_case(305_419_896, 2, true, true).expect("Z: folded bucket, bottom slack");
    run_case(11_400_714_819_024_595_565, 0, false, true).expect("Z: folded bucket, bottom slack");
    run_case(15_755_400_384_025_928_263, 0, false, true).expect("Z: bucket read above hi");
    run_case(4_354_685_564_635_622_994, 2, true, true).expect("Z: bucket read above hi");
}

/// PR W review round 5: the four seeds in
/// `wal_unapplied_ranges_properties.proptest-regressions`, pinned as plain
/// tests. The regressions file replays them only through the proptest
/// runner, whose generated inputs move whenever the strategy tuple changes
/// (round 5 added `mirror`); a pinned `run_case` call is the durable form.
/// Two more seed lines were lost from that file during round 4 and cannot be
/// recovered (recorded in plan item W).
#[test]
fn test_regression_saved_proptest_seeds_replay_as_plain_cases() {
    run_case(1_348_121_622_768_296_934, 0, false, false).expect("saved seed 1");
    run_case(13_323_464_164_761_636_786, 1, false, false).expect("saved seed 2");
    run_case(8_362_470_527_334_602_142, 0, false, false).expect("saved seed 3");
    run_case(13_013_751_188_033_078_155, 0, true, false).expect("saved seed 4");
}

/// PR W review round 3: an in-session overflow of the bucket table is
/// bounded into a range at the next persist, and the overflow bounds are then
/// reset, so a LATER overflow of the same session is bounded on its own. The
/// bounds used to accumulate (`fetch_min` / `fetch_max`, never reset after a
/// bound), so the second overflow was persisted from the first one's low end
/// to its own top and pinned every acked segment written in between.
///
/// Two to six overflows, far apart, each a wide failed-rescue range (more
/// buckets than the table holds) or a collision on one slot, with persists
/// between some of them and acks above all of them. Overflows with no persist
/// between them are one group (the persist bounds them together, which is
/// fine: in production the persist runs once a second). After each persist
/// and at the end (read back from the file): every marked sequence is
/// unapplied, the RAM flag is clear, and a point in a gap between two groups
/// is applied.
fn run_repeated_overflow_case(seed: u64) -> Result<(), TestCaseError> {
    let mut rng = Rng(seed);
    let dir = std::env::temp_dir().join(format!(
        "tv-ranges-prop-overflow-{}-{seed:x}-{}-{}",
        std::process::id(),
        rng.next(),
        next_case_dir_serial()
    ));
    std::fs::create_dir_all(&dir).expect("case dir");
    let wm = AppliedWatermark::new();
    wm.bind(&dir);
    let bucket = 1u64 << UNAPPLIED_BUCKET_SHIFT;
    let table = UNAPPLIED_BUCKETS as u64 * bucket;
    // `[lo, hi]` of every marked stretch, and the groups (one per persist).
    let mut marked: Vec<(u64, u64)> = Vec::new();
    let mut groups: Vec<(u64, u64)> = Vec::new();
    let mut open: Option<(u64, u64)> = None;
    let mut cursor = BASE_NS;
    let mut mono = 0u64;
    let overflows = 2 + rng.below(5);
    let check = |snap: &AppliedSnapshot,
                 marked: &[(u64, u64)],
                 groups: &[(u64, u64)]|
     -> Result<(), TestCaseError> {
        for &(lo, hi) in marked {
            prop_assert!(
                !snap.frame_is_applied(AppliedSink::Ticks, lo),
                "{lo} replays"
            );
            prop_assert!(
                !snap.frame_is_applied(AppliedSink::Ticks, hi),
                "{hi} replays"
            );
        }
        for w in groups.windows(2) {
            let (gap_lo, gap_hi) = (w[0].1, w[1].0);
            if gap_hi > gap_lo + 4 * bucket + 2 * SLACK {
                let mid = gap_lo + (gap_hi - gap_lo) / 2;
                prop_assert!(
                    both_applied(snap, mid, mid + SEC),
                    "a point between two overflows stays prunable ({mid}, groups {groups:?})"
                );
            }
        }
        Ok(())
    };
    let widen = |open: &mut Option<(u64, u64)>, lo: u64, hi: u64| {
        *open = Some(open.map_or((lo, hi), |(a, b)| (a.min(lo), b.max(hi))));
    };
    for _ in 0..overflows {
        cursor += table + (1 + rng.below(4_000)) * bucket;
        if rng.chance(50) {
            let hi = cursor + table + rng.below(8) * bucket;
            wm.note_unapplied_range(cursor, hi);
            marked.push((cursor, hi));
            widen(&mut open, cursor, hi);
            cursor = hi;
        } else {
            // Two ids one table apart share a slot: the second overflows.
            let first = cursor & !(bucket - 1);
            let second = first + table * (1 + rng.below(3));
            wm.note_unapplied(first);
            wm.note_unapplied(second);
            marked.push((first, first + bucket - 1));
            marked.push((second, second + bucket - 1));
            widen(&mut open, first, second + bucket - 1);
            cursor = second + bucket;
        }
        ack(&wm, cursor + table);
        if rng.chance(70) {
            persist(&wm, &mut mono, SEC * (1 + rng.below(30)));
            prop_assert!(!wm.snapshot().overflowed, "bounded at the persist");
            groups.extend(open.take());
            check(&wm.snapshot(), &marked, &groups)?;
        }
    }
    persist(&wm, &mut mono, SEC);
    persist(&wm, &mut mono, LAG + SEC);
    groups.extend(open.take());
    let on_disk = AppliedSnapshot::load(&dir).expect("file");
    prop_assert!(!on_disk.overflowed, "no unbounded bit on disk");
    check(&on_disk, &marked, &groups)?;
    let _cleanup = std::fs::remove_dir_all(&dir);
    Ok(())
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    #[test]
    fn proptest_repeated_in_session_overflows_are_bounded_one_by_one(
        seed in any::<u64>(),
    ) {
        run_repeated_overflow_case(seed)?;
    }
}

/// PR W review round 4, pass 2: cases the `extra` generator found, pinned
/// so they run on every build whatever the random seeds. Found by sweeping
/// 400 seeds at three start states each with one rule reverted at a time,
/// then checked one by one: each of the four fails Z with the suspension
/// ranges starting ONE slack below their watermark, and each fails Z with
/// the trim's spans built in first-sequence order. With both fixes, 1,500
/// `extra` cases (500 seeds, three start states) ran with 0 failures.
#[test]
fn test_regression_round4_pass2_extra_generator_seeds() {
    run_case_with(0, 0, false, false, true).expect("Z: either old rule");
    run_case_with(0, 2, true, false, true).expect("Z: either old rule, stuck");
    run_case_with(2, 1, false, true, true).expect("Z: either old rule, mirror");
    run_case_with(1, 2, true, false, true).expect("Z: either old rule, stuck");
    // Found by the proptest runner against the first draft of the write
    // order fix (also saved in the regressions file): B failed because a
    // segment written after a higher first started its span one slack below
    // its own first, not at the running floor.
    run_case_with(16_922_757_407_861_016_088, 1, true, false, true)
        .expect("B: spans start at the running floor");
}

/// PR W review round 6: cases the plain-watermark straddle generator
/// ([`straddle_at_stop`]) found, pinned so they run on every build whatever
/// the random seeds. Found by sweeping 400 seeds at three start states each
/// (1,200 cases) with one rule reverted at a time: with the whole-segment
/// rule at ONE slack below the ack watermark, every one of the three fails Z
/// at the in-session prune; with the boot tail and the backlog guard
/// starting at `boot_applied_floor + 1`, every one fails Z (the prune, the
/// boot replay's segment skip, or the final catch-up pass's skip). With both
/// fixes the 1,200 cases ran with 0 failures.
#[test]
fn test_regression_round6_plain_watermark_straddle_seeds() {
    run_case_full(0, 0, false, false, false, true).expect("Z: either old rule");
    run_case_full(1, 1, true, false, false, true).expect("Z: either old rule, stuck");
    run_case_full(2, 1, false, false, false, true).expect("Z: either old rule, final pass");
}
