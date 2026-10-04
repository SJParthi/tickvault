//! Frame fate — which WAL-backed frames were SHED, so a WAL write that later
//! fails on one of them is counted as a real loss (audit M1, 2026-10-04).
//!
//! # The gap this closes
//!
//! A frame is "WAL-backed" the moment its record is QUEUED for the WAL writer
//! thread (`AppendOutcome::Spilled`), not when it is written. Two places then
//! drop the frame's in-memory copy on the strength of that record:
//!
//! - the socket reader SHEDS the whole frame when the ring is full
//!   (`WalRingSink::accept`), marking it unapplied so the next boot re-folds
//!   it from the WAL;
//! - the frame drain SHEDS the frame's depth rows when the database is behind,
//!   marking it deferred so the after-close pass writes them from the WAL.
//!
//! If the writer then fails to write that record (disk full, a write or flush
//! error, a segment it cannot open), the frame exists nowhere. Before this
//! module only the writer's own I/O counters moved, so a real loss of ticks or
//! depth rows read as a WAL hiccup and never reached the tick-loss counter.
//!
//! # Shape
//!
//! A fixed, direct-mapped table of `N` slots indexed by the frame's sequence
//! BASE (`capture_seq >> PACKET_INDEX_BITS`), which is strictly increasing by
//! at least one per frame (`ws_frame_spill::next_frame_seq`), so consecutive
//! frames take consecutive slots and never collide inside a window of `N`
//! bases. Each slot packs the base (as a tag) with three bits:
//!
//! - [`SHED_RING`]  — the reader shed the frame at the ring;
//! - [`SHED_DRAIN`] — the drain shed the frame's depth rows;
//! - [`LOST`]       — the WAL writer failed to write the frame's record.
//!
//! Whichever side completes a SHED + LOST pair counts the loss, so the order
//! the two events happen in does not matter. A drain that finds LOST already
//! set does not shed: it writes the rows instead, and nothing is lost.
//!
//! Every operation is O(1): one load and at most a few CAS retries on one
//! slot, no allocation, no lock. The shed side runs only on the shed arms
//! (already a degraded path), the lost side only on the writer's failure arms.
//!
//! # Window
//!
//! [`FRAME_FATE_SLOTS`] = 2^22. Bases advance by one per frame, or by the wall
//! clock (one per 131 µs) when frames are slower, so the table remembers a
//! shed for at least ~5.5 minutes at the 12,500 frame/s open burst and ~9
//! minutes at idle. The WAL queue holds at most 524,288 records (~42 s at the
//! burst). A writer more than a whole window behind cannot tell whether a lost
//! record was shed: it counts it on [`FATE_UNKNOWN_COUNTER`] instead, never as
//! "not shed".
//!
//! # Honest limits
//!
//! - A crash (SIGKILL, OOM, `panic = "abort"`) loses whatever the WAL queue
//!   held, shed or not, and nothing in the process survives to count it; the
//!   queue-depth gauges measure that window.
//! - A shed frame whose record is still queued when shutdown abandons a wedged
//!   writer is counted by [`FrameFate::count_unflushed_sheds`], which treats
//!   every shed above the writer's last flushed sequence as lost. A record
//!   minted below that sequence but appended after it (two sockets racing for
//!   a few microseconds) is not counted.
//! - Space: the table is `N × 8` bytes = 32 MiB of address space in `.bss`.
//!   Only the pages a shed or a loss touches become resident.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

use tickvault_common::error_code::ErrorCode;
use tracing::error;

use crate::ws_frame_spill::{
    PACKET_INDEX_BITS, WS_TYPE_COUNT, WS_TYPES_BY_INDEX, WsType, refusal_line_due, ws_type_index,
};

/// Slots in the production table. A power of two, so the slot is a mask.
pub const FRAME_FATE_SLOTS: usize = 1 << 22;

/// The reader shed the whole frame at the ring.
pub const SHED_RING: u64 = 0b001;
/// The drain shed the frame's depth rows.
pub const SHED_DRAIN: u64 = 0b010;
/// The WAL writer failed to write the frame's record.
pub const LOST: u64 = 0b100;
const FLAG_BITS: u32 = 3;
const FLAG_MASK: u64 = (1 << FLAG_BITS) - 1;

/// Counter: shed frames whose WAL record was then lost, by `shed` kind
/// (`ring` = the whole frame, `drain_depth` = its depth rows).
pub const SHED_LOST_COUNTER: &str = "tv_wal_shed_frames_lost_total";
/// Counter: records the WAL writer lost whose shed state could not be known
/// (the table had moved past them, or they were past the writer's tally).
pub const FATE_UNKNOWN_COUNTER: &str = "tv_wal_lost_fate_unknown_total";
/// Counter: depth sheds the drain refused, writing the rows instead, by
/// `reason` (`wal_lost` = the record was already lost, `unrecordable` = the
/// table could not record the shed).
pub const DRAIN_SHED_REFUSED_COUNTER: &str = "tv_wal_drain_shed_refused_total";
/// The tick-loss SLA counter's `source` for a ring-shed frame whose WAL record
/// was lost. Same series family as the WAL drop arms.
pub const TICKS_LOST_SOURCE: &str = "wal_lost_after_shed";

/// Which side shed the frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShedKind {
    /// The socket reader, at the ring.
    Ring,
    /// The frame drain, the frame's depth rows only.
    DrainDepth,
}

impl ShedKind {
    const fn bit(self) -> u64 {
        match self {
            Self::Ring => SHED_RING,
            Self::DrainDepth => SHED_DRAIN,
        }
    }
}

/// What [`FrameFate::note_shed`] found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShedMark {
    /// The shed is recorded; a later WAL loss of this frame will be counted.
    Marked,
    /// The WAL writer had already lost this frame's record. A ring shed is
    /// counted as lost right here; a drain must write the rows instead.
    AlreadyLost,
    /// The slot belongs to a newer frame, so the shed cannot be recorded. A
    /// drain must write the rows instead.
    Unrecordable,
    /// Sequence zero (a pre-v2 record): nothing to record.
    Untracked,
}

/// What [`FrameFate::note_lost`] found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LostFate {
    /// The frame had been shed: a real loss, counted.
    WasShed(ShedKind),
    /// Not shed (so far): its in-memory copy went on to the drain. Recorded,
    /// so a later drain shed of it is refused.
    NotShed,
    /// The table had moved past this frame: counted as unknown.
    Unknown,
}

/// Pre-resolved counter handles: the shed arm runs on the socket reader, so
/// it must not build a labelled key per call.
struct FateCounters {
    ring_lost: metrics::Counter,
    drain_lost: metrics::Counter,
    ticks_lost: [metrics::Counter; WS_TYPE_COUNT],
    unknown: metrics::Counter,
    refused_wal_lost: metrics::Counter,
    refused_unrecordable: metrics::Counter,
}

static COUNTERS: OnceLock<FateCounters> = OnceLock::new();
/// Losses attributed so far, for the log-line throttle.
static LOSSES_LOGGED: AtomicU64 = AtomicU64::new(0);

fn counters() -> &'static FateCounters {
    COUNTERS.get_or_init(|| FateCounters {
        ring_lost: metrics::counter!(SHED_LOST_COUNTER, "shed" => "ring"),
        drain_lost: metrics::counter!(SHED_LOST_COUNTER, "shed" => "drain_depth"),
        ticks_lost: WS_TYPES_BY_INDEX.map(|t| {
            metrics::counter!(
                "tv_ticks_lost_total",
                "source" => TICKS_LOST_SOURCE,
                "ws_type" => t.as_str()
            )
        }),
        unknown: metrics::counter!(FATE_UNKNOWN_COUNTER),
        refused_wal_lost: metrics::counter!(DRAIN_SHED_REFUSED_COUNTER, "reason" => "wal_lost"),
        refused_unrecordable: metrics::counter!(
            DRAIN_SHED_REFUSED_COUNTER,
            "reason" => "unrecordable"
        ),
    })
}

/// Resolves the counter handles and publishes a zero on every series, so the
/// first real loss is not the sample the CloudWatch agent drops. Called once
/// at boot, before any socket is dialled (`WsFrameSpill::new`); after that
/// the shed arm never allocates.
pub fn pre_register() {
    let c = counters();
    c.ring_lost.increment(0);
    c.drain_lost.increment(0);
    for t in &c.ticks_lost {
        t.increment(0);
    }
    c.unknown.increment(0);
    c.refused_wal_lost.increment(0);
    c.refused_unrecordable.increment(0);
}

/// Counts `n` lost records whose sequence the writer did not keep (written
/// past its tally's capacity), so their shed state cannot be checked. O(1).
pub fn note_lost_untracked(n: u64) {
    if n > 0 {
        counters().unknown.increment(n);
    }
}

/// The process-wide table.
static FRAME_FATE: FrameFate<FRAME_FATE_SLOTS> = FrameFate::new();

/// The process-wide table. O(1).
#[must_use]
pub fn frame_fate() -> &'static FrameFate<FRAME_FATE_SLOTS> {
    &FRAME_FATE
}

/// The highest frame sequence the WAL writer has flushed to the kernel.
static FLUSHED_HIGH_SEQ: AtomicU64 = AtomicU64::new(0);

/// Records that every record up to `seq` written so far reached the kernel.
/// Called by the WAL writer after each successful flush; one `fetch_max`.
pub fn note_flushed_through(seq: u64) {
    FLUSHED_HIGH_SEQ.fetch_max(seq, Ordering::Relaxed);
}

/// The value [`note_flushed_through`] last raised.
#[must_use]
pub fn flushed_high_seq() -> u64 {
    FLUSHED_HIGH_SEQ.load(Ordering::Relaxed)
}

/// The direct-mapped fate table. `N` must be a power of two.
pub struct FrameFate<const N: usize> {
    slots: [AtomicU64; N],
}

impl<const N: usize> FrameFate<N> {
    const MASK: u64 = {
        assert!(N.is_power_of_two(), "FrameFate needs a power-of-two size");
        (N as u64) - 1
    };

    /// An empty table. `const`, so the production table lives in `.bss` and
    /// is never initialised at runtime.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            slots: [const { AtomicU64::new(0) }; N],
        }
    }

    /// The frame's base, or `None` for sequence zero.
    const fn base_of(seq: u64) -> Option<u64> {
        let base = seq >> PACKET_INDEX_BITS;
        if base == 0 { None } else { Some(base) }
    }

    fn slot(&self, base: u64) -> &AtomicU64 {
        // The mask keeps the index below N, so this never goes out of range.
        &self.slots[(base & Self::MASK) as usize]
    }

    /// Records that `seq` was shed by `kind`. O(1), zero allocation.
    ///
    /// When the writer has already lost the record, a ring shed is counted
    /// as a lost frame here (its only copy was the ring slot it was just
    /// refused); a drain shed is NOT counted, because the drain is told to
    /// write the rows instead ([`ShedMark::AlreadyLost`]).
    pub fn note_shed(&self, seq: u64, kind: ShedKind, ws_type: WsType) -> ShedMark {
        let Some(base) = Self::base_of(seq) else {
            return ShedMark::Untracked;
        };
        let slot = self.slot(base);
        let mut cur = slot.load(Ordering::Acquire);
        loop {
            let tag = cur >> FLAG_BITS;
            let next = if tag == base {
                if cur & LOST != 0 {
                    if kind == ShedKind::Ring {
                        record_shed_loss(kind, ws_type, seq);
                    }
                    return ShedMark::AlreadyLost;
                }
                cur | kind.bit()
            } else if tag < base {
                // An older frame (or nothing) holds the slot: claim it.
                (base << FLAG_BITS) | kind.bit()
            } else {
                return ShedMark::Unrecordable;
            };
            match slot.compare_exchange_weak(cur, next, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => return ShedMark::Marked,
                Err(observed) => cur = observed,
            }
        }
    }

    /// Records that the WAL writer lost `seq`'s record. Counts the loss when
    /// the frame had been shed. O(1), zero allocation; the writer thread's
    /// failure arms only.
    pub fn note_lost(&self, seq: u64, ws_type: WsType) -> LostFate {
        let Some(base) = Self::base_of(seq) else {
            counters().unknown.increment(1);
            return LostFate::Unknown;
        };
        let slot = self.slot(base);
        let mut cur = slot.load(Ordering::Acquire);
        loop {
            let tag = cur >> FLAG_BITS;
            let next = if tag == base {
                if cur & LOST != 0 {
                    // Reported twice; the first report already counted it.
                    return LostFate::NotShed;
                }
                cur | LOST
            } else if tag < base {
                (base << FLAG_BITS) | LOST
            } else {
                counters().unknown.increment(1);
                return LostFate::Unknown;
            };
            match slot.compare_exchange_weak(cur, next, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => {
                    let shed = if tag == base { cur & FLAG_MASK } else { 0 };
                    return if shed & SHED_RING != 0 {
                        record_shed_loss(ShedKind::Ring, ws_type, seq);
                        LostFate::WasShed(ShedKind::Ring)
                    } else if shed & SHED_DRAIN != 0 {
                        record_shed_loss(ShedKind::DrainDepth, ws_type, seq);
                        LostFate::WasShed(ShedKind::DrainDepth)
                    } else {
                        LostFate::NotShed
                    };
                }
                Err(observed) => cur = observed,
            }
        }
    }

    /// The drain's gate before it sheds a frame's depth rows. `true` = the
    /// shed is recorded and may go ahead; `false` = write the rows instead
    /// (the record is already lost, or the shed cannot be recorded), counted
    /// on [`DRAIN_SHED_REFUSED_COUNTER`]. O(1), zero allocation.
    pub fn allow_drain_shed(&self, seq: u64, ws_type: WsType) -> bool {
        match self.note_shed(seq, ShedKind::DrainDepth, ws_type) {
            ShedMark::Marked | ShedMark::Untracked => true,
            ShedMark::AlreadyLost => {
                counters().refused_wal_lost.increment(1);
                false
            }
            ShedMark::Unrecordable => {
                counters().refused_unrecordable.increment(1);
                false
            }
        }
    }

    /// Sheds recorded above `flushed_seq` that no loss has claimed yet:
    /// `(ring, drain)`. Each is counted as lost. For shutdown only, when a
    /// wedged writer is abandoned with records still queued: a shed frame
    /// whose sequence is above the writer's last flush never reached the
    /// kernel.
    ///
    /// Each counted slot is marked lost with a compare-and-swap first, so the
    /// abandoned writer, still running detached, cannot count the same frame
    /// again when it later fails a write or exits (review of audit M1,
    /// 2026-10-04). Honest limit: if that writer instead completes its flush
    /// before the process exits, a frame counted here did reach the kernel;
    /// the count errs toward loss.
    ///
    /// # Complexity
    /// O(N): one pass over the table, once, at an abandoned shutdown.
    pub fn count_unflushed_sheds(&self, flushed_seq: u64, ws_type: WsType) -> (u64, u64) {
        let floor = flushed_seq >> PACKET_INDEX_BITS;
        let mut ring = 0u64;
        let mut drain = 0u64;
        // O(1) EXEMPT: begin — once per abandoned shutdown, bounded by N
        for slot in &self.slots {
            let mut v = slot.load(Ordering::Acquire);
            loop {
                let tag = v >> FLAG_BITS;
                if tag <= floor || v & LOST != 0 || v & (SHED_RING | SHED_DRAIN) == 0 {
                    break;
                }
                match slot.compare_exchange_weak(v, v | LOST, Ordering::AcqRel, Ordering::Acquire) {
                    Ok(_) => {
                        if v & SHED_RING != 0 {
                            ring += 1;
                        } else {
                            drain += 1;
                        }
                        break;
                    }
                    Err(now) => v = now,
                }
            }
        }
        // O(1) EXEMPT: end
        let c = counters();
        if ring > 0 {
            c.ring_lost.increment(ring);
            c.ticks_lost[ws_type_index(ws_type)].increment(ring);
        }
        if drain > 0 {
            c.drain_lost.increment(drain);
        }
        (ring, drain)
    }
}

impl<const N: usize> Default for FrameFate<N> {
    fn default() -> Self {
        Self::new()
    }
}

/// Counts one shed frame whose WAL record was lost, with a throttled coded
/// line (the first loss and then the refusal ladder). Cold.
#[cold]
fn record_shed_loss(kind: ShedKind, ws_type: WsType, seq: u64) {
    let c = counters();
    match kind {
        ShedKind::Ring => {
            c.ring_lost.increment(1);
            c.ticks_lost[ws_type_index(ws_type)].increment(1);
        }
        ShedKind::DrainDepth => c.drain_lost.increment(1),
    }
    let n = LOSSES_LOGGED.fetch_add(1, Ordering::Relaxed) + 1;
    if refusal_line_due(n) {
        error!(
            code = ErrorCode::WsSpill02FrameDropped.code_str(),
            ws_type = ws_type.as_str(),
            shed = match kind {
                ShedKind::Ring => "ring",
                ShedKind::DrainDepth => "drain_depth",
            },
            capture_seq = seq,
            losses = n,
            "CRITICAL: a shed frame's WAL record was never written — the frame is lost \
             (its in-memory copy was dropped on the strength of a record the disk refused)"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A sequence for base `b`, as `next_frame_seq` mints it.
    fn seq(b: u64) -> u64 {
        b << PACKET_INDEX_BITS
    }

    const LIVE: WsType = WsType::LiveFeed;

    #[test]
    fn test_note_shed_then_note_lost_counts_a_ring_shed_as_lost() {
        let t = FrameFate::<64>::new();
        assert_eq!(t.note_shed(seq(10), ShedKind::Ring, LIVE), ShedMark::Marked);
        assert_eq!(
            t.note_lost(seq(10), LIVE),
            LostFate::WasShed(ShedKind::Ring)
        );
    }

    #[test]
    fn test_note_lost_then_note_shed_reports_already_lost() {
        let t = FrameFate::<64>::new();
        assert_eq!(t.note_lost(seq(11), LIVE), LostFate::NotShed);
        assert_eq!(
            t.note_shed(seq(11), ShedKind::Ring, LIVE),
            ShedMark::AlreadyLost
        );
    }

    #[test]
    fn test_note_lost_of_a_frame_never_shed_is_not_a_loss() {
        let t = FrameFate::<64>::new();
        assert_eq!(t.note_lost(seq(12), LIVE), LostFate::NotShed);
        // A second report of the same record is not counted twice.
        assert_eq!(t.note_lost(seq(12), LIVE), LostFate::NotShed);
    }

    #[test]
    fn test_allow_drain_shed_refuses_a_frame_the_writer_already_lost() {
        let t = FrameFate::<64>::new();
        assert!(
            t.allow_drain_shed(seq(13), LIVE),
            "an unlost frame may be shed"
        );
        assert_eq!(
            t.note_lost(seq(13), LIVE),
            LostFate::WasShed(ShedKind::DrainDepth)
        );
        assert_eq!(t.note_lost(seq(14), LIVE), LostFate::NotShed);
        assert!(
            !t.allow_drain_shed(seq(14), LIVE),
            "a frame whose record is lost must be written, not shed"
        );
    }

    #[test]
    fn test_a_newer_frame_in_the_slot_makes_the_older_fate_unknown() {
        let t = FrameFate::<64>::new();
        assert_eq!(
            t.note_shed(seq(5 + 64), ShedKind::Ring, LIVE),
            ShedMark::Marked
        );
        // Base 5 shares the slot with base 69, which is newer.
        assert_eq!(t.note_lost(seq(5), LIVE), LostFate::Unknown);
        assert_eq!(
            t.note_shed(seq(5), ShedKind::DrainDepth, LIVE),
            ShedMark::Unrecordable
        );
        assert!(!t.allow_drain_shed(seq(5), LIVE));
    }

    #[test]
    fn test_a_newer_frame_claims_a_slot_an_older_one_held() {
        let t = FrameFate::<64>::new();
        assert_eq!(t.note_shed(seq(7), ShedKind::Ring, LIVE), ShedMark::Marked);
        // Base 71 replaces base 7 in the slot; its loss is not base 7's shed.
        assert_eq!(t.note_lost(seq(7 + 64), LIVE), LostFate::NotShed);
        assert_eq!(t.note_lost(seq(7), LIVE), LostFate::Unknown);
    }

    #[test]
    fn test_sequence_zero_is_untracked() {
        let t = FrameFate::<64>::new();
        assert_eq!(t.note_shed(0, ShedKind::Ring, LIVE), ShedMark::Untracked);
        assert_eq!(t.note_lost(0, LIVE), LostFate::Unknown);
        assert!(t.allow_drain_shed(0, LIVE));
    }

    #[test]
    fn test_count_unflushed_sheds_counts_only_unclaimed_sheds_above_the_flush() {
        let t = FrameFate::<64>::new();
        assert_eq!(t.note_shed(seq(20), ShedKind::Ring, LIVE), ShedMark::Marked);
        assert_eq!(t.note_shed(seq(30), ShedKind::Ring, LIVE), ShedMark::Marked);
        assert_eq!(
            t.note_shed(seq(31), ShedKind::DrainDepth, LIVE),
            ShedMark::Marked
        );
        assert_eq!(t.note_shed(seq(32), ShedKind::Ring, LIVE), ShedMark::Marked);
        // Already counted through the writer: not counted again.
        assert_eq!(
            t.note_lost(seq(32), LIVE),
            LostFate::WasShed(ShedKind::Ring)
        );
        // Flushed through base 25: base 20 reached the kernel.
        assert_eq!(t.count_unflushed_sheds(seq(25), LIVE), (1, 1));
    }

    /// Review of audit M1: the abandoned writer keeps running detached. A
    /// frame the shutdown scan counted must not be counted again when that
    /// writer later reports it lost, nor by a second scan.
    #[test]
    fn test_count_unflushed_sheds_marks_what_it_counts_so_a_late_loss_is_not_recounted() {
        let t = FrameFate::<64>::new();
        assert_eq!(t.note_shed(seq(40), ShedKind::Ring, LIVE), ShedMark::Marked);
        assert_eq!(
            t.note_shed(seq(41), ShedKind::DrainDepth, LIVE),
            ShedMark::Marked
        );
        assert_eq!(t.count_unflushed_sheds(seq(10), LIVE), (1, 1));
        assert_eq!(t.note_lost(seq(40), LIVE), LostFate::NotShed);
        assert_eq!(t.note_lost(seq(41), LIVE), LostFate::NotShed);
        assert_eq!(t.count_unflushed_sheds(seq(10), LIVE), (0, 0));
    }

    #[test]
    fn test_note_flushed_through_only_rises() {
        note_flushed_through(seq(1_000));
        note_flushed_through(seq(900));
        assert!(flushed_high_seq() >= seq(1_000));
    }

    #[test]
    fn test_flushed_high_seq_reads_the_highest_flushed_sequence() {
        note_flushed_through(seq(2_000));
        let high = flushed_high_seq();
        assert!(high >= seq(2_000));
        note_flushed_through(seq(1_500));
        assert!(flushed_high_seq() >= high, "a lower flush never lowers it");
    }

    #[test]
    fn test_note_lost_untracked_ignores_zero_and_counts_without_panicking() {
        pre_register();
        note_lost_untracked(0);
        note_lost_untracked(3);
        assert!(COUNTERS.get().is_some());
    }

    #[test]
    fn test_pre_register_resolves_every_handle() {
        pre_register();
        assert!(COUNTERS.get().is_some());
    }

    #[test]
    fn test_frame_fate_returns_the_process_table() {
        let t = frame_fate();
        assert_eq!(t.slots.len(), FRAME_FATE_SLOTS);
        assert!(std::ptr::eq(t, frame_fate()));
    }

    /// Both sides racing on one slot: exactly one of them sees the pair and
    /// the loss is counted once, whichever order the threads run in.
    #[test]
    fn test_a_shed_racing_a_loss_is_counted_exactly_once() {
        for round in 0..200u64 {
            let t = std::sync::Arc::new(FrameFate::<64>::new());
            let s = seq(100 + round);
            let a = std::sync::Arc::clone(&t);
            let shed = std::thread::spawn(move || a.note_shed(s, ShedKind::Ring, LIVE));
            let lost = t.note_lost(s, LIVE);
            let shed = shed.join().expect("shed thread");
            let counted_by_lost = lost == LostFate::WasShed(ShedKind::Ring);
            let counted_by_shed = shed == ShedMark::AlreadyLost;
            assert!(
                counted_by_lost ^ counted_by_shed,
                "round {round}: lost={lost:?} shed={shed:?} must count exactly once"
            );
        }
    }
}
