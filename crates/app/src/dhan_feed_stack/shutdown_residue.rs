//! What the frame drain and its writer threads still held at shutdown
//! (plan item J2, 2026-10-26).
//!
//! Two counting gaps, both on the shutdown path only, never per tick.
//!
//! 1. **The frame ring.** The drain's shutdown arm used to `break` and drop the
//!    ring receiver with frames still queued. A WAL-backed frame is replayed on
//!    the next boot, so it is deferred, not lost; a frame the WAL refused
//!    (`wal_backed == false`) exists only in that queue, so dropping it was a
//!    loss with no counter and no coded line. The drain now keeps folding until
//!    the ring is empty or [`SHUTDOWN_RING_TAIL_BUDGET`] passes, and whatever is
//!    still queued then is counted here: backed frames on
//!    [`SHUTDOWN_RING_DEFERRED_COUNTER`], unbacked ones on
//!    `tv_ticks_lost_total{source="shutdown_ring_unbacked"}`.
//!
//! 2. **The tick writer and rescue threads.** An abandoned thread counted `+1`
//!    per event, never rows, so the rows of an unbacked batch it was holding
//!    reached no row-level loss counter. Each thread now takes batches through
//!    an [`OffloadQueue`] that records the batch it holds (rows, and whether any
//!    row has no WAL record) under the queue lock, and the shutdown join, on
//!    abandonment, reads that record plus every batch still queued
//!    ([`OffloadQueue::abandoned_residue`]).
//!
//! # Honest limits
//!
//! - A batch flags "some row has no WAL record", not how many, so
//!   `unbacked_rows` counts every row of a flagged batch: an upper bound.
//! - An abandoned thread is detached, not stopped. If it finishes its write
//!   before the process exits, rows counted lost here did land (errs toward
//!   loss, never toward a false all-clear).
//! - Frames a socket still delivers after the residue count are dropped with
//!   the receiver; the window is the few instructions between the last
//!   `try_recv` and the drop. The sockets are stopped before the drain is
//!   signalled, so in practice nothing arrives there.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex, TryLockError};
use std::time::Duration;

use tickvault_common::error_code::ErrorCode;
use tracing::{error, info};

/// `tv_ticks_lost_total` source for frames the WAL refused that were still in
/// the ring when the drain stopped.
pub(super) const SHUTDOWN_RING_UNBACKED_SOURCE: &str = "shutdown_ring_unbacked";

/// `tv_ticks_lost_total` source for unbacked rows the tick rescue thread still
/// held when the shutdown gave up on it.
pub(super) const RESCUE_ABANDONED_SOURCE: &str = "rescue_abandoned";

/// `tv_ticks_lost_total` source for unbacked rows the tick writer thread still
/// held when the shutdown gave up on it.
pub(super) const WRITER_ABANDONED_SOURCE: &str = "writer_abandoned";

/// WAL-backed frames still in the ring when the drain stopped. Not a loss: the
/// next boot's WAL replay folds them. Local `/metrics` only.
pub(super) const SHUTDOWN_RING_DEFERRED_COUNTER: &str = "tv_dhan_feed_shutdown_ring_deferred_total";

/// How long the drain keeps folding queued frames after the shutdown signal.
///
/// Taken from the slack in `main`'s lane budget (30 s) left over the offload
/// grace (12 s) plus the pre-join margin (two ILP timeouts, 10 s): 8 s. Two of
/// those 8 s go here; the test `the_ring_tail_budget_fits_the_lane_budget`
/// re-reads `main.rs` so the sum cannot drift past the budget unseen.
pub(super) const SHUTDOWN_RING_TAIL_BUDGET_SECS: u64 = 2;

/// [`SHUTDOWN_RING_TAIL_BUDGET_SECS`] as a `Duration`.
pub(super) const SHUTDOWN_RING_TAIL_BUDGET: Duration =
    Duration::from_secs(SHUTDOWN_RING_TAIL_BUDGET_SECS);

/// Puts every new loss series on the wire at zero, so an absent series is
/// never read as a healthy one (the same reason as the drain's own seeding).
pub(super) fn seed_shutdown_loss_baselines() {
    for source in [
        SHUTDOWN_RING_UNBACKED_SOURCE,
        RESCUE_ABANDONED_SOURCE,
        WRITER_ABANDONED_SOURCE,
    ] {
        metrics::counter!("tv_ticks_lost_total", "source" => source).increment(0);
    }
    metrics::counter!(SHUTDOWN_RING_DEFERRED_COUNTER).increment(0);
}

/// Frames left in the ring, split by whether the WAL holds them.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(super) struct RingResidue {
    /// The WAL holds these; the next boot replays them.
    pub(super) backed: u64,
    /// The WAL refused these; dropping them loses them.
    pub(super) unbacked: u64,
}

impl RingResidue {
    /// Counts one frame.
    pub(super) fn note(&mut self, wal_backed: bool) {
        if wal_backed {
            self.backed = self.backed.saturating_add(1);
        } else {
            self.unbacked = self.unbacked.saturating_add(1);
        }
    }

    /// True when nothing was left.
    pub(super) const fn is_empty(&self) -> bool {
        self.backed == 0 && self.unbacked == 0
    }
}

/// Takes every frame still queued, at most `limit` (a socket still delivering
/// cannot keep this loop running), and counts it. `on_frame` sees each frame
/// first, so the caller can release its ring byte reservation.
pub(super) fn drain_ring_residue<T>(
    mut try_recv: impl FnMut() -> Option<T>,
    limit: usize,
    mut on_frame: impl FnMut(&T) -> bool,
) -> RingResidue {
    let mut residue = RingResidue::default();
    for _ in 0..limit {
        let Some(frame) = try_recv() else {
            break;
        };
        residue.note(on_frame(&frame));
    }
    residue
}

/// Reports what [`drain_ring_residue`] found. Silent when nothing was left.
pub(super) fn report_ring_residue(residue: RingResidue) {
    if residue.is_empty() {
        return;
    }
    metrics::counter!(SHUTDOWN_RING_DEFERRED_COUNTER).increment(residue.backed);
    if residue.unbacked > 0 {
        metrics::counter!("tv_ticks_lost_total", "source" => SHUTDOWN_RING_UNBACKED_SOURCE)
            .increment(residue.unbacked);
        error!(
            code = ErrorCode::WsSpill02FrameDropped.code_str(),
            source = SHUTDOWN_RING_UNBACKED_SOURCE,
            frames_lost = residue.unbacked,
            frames_deferred = residue.backed,
            "Dhan live feed: {} frame(s) the WAL had refused were still in the frame ring \
             when the drain stopped — they existed nowhere else and are LOST; {} WAL-backed \
             frame(s) left with them are replayed on the next boot",
            residue.unbacked,
            residue.backed
        );
    } else {
        info!(
            frames_deferred = residue.backed,
            "Dhan live feed: {} WAL-backed frame(s) were still in the frame ring when the \
             drain stopped — the next boot's WAL replay folds them, nothing is lost",
            residue.backed
        );
    }
}

/// Batches an abandoned writer thread still held.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(super) struct BatchResidue {
    /// Batches held (the one in flight plus every one still queued).
    pub(super) batches: u64,
    /// Rows in those batches.
    pub(super) rows: u64,
    /// Rows in batches that carry at least one row with no WAL record (an
    /// upper bound, see the module header). The rest are replayed from the
    /// WAL on the next boot.
    pub(super) unbacked_rows: u64,
}

impl BatchResidue {
    /// Counts one batch of `rows` rows.
    pub(super) fn note(&mut self, rows: u64, unbacked: bool) {
        self.batches = self.batches.saturating_add(1);
        self.rows = self.rows.saturating_add(rows);
        if unbacked {
            self.unbacked_rows = self.unbacked_rows.saturating_add(rows);
        }
    }
}

/// The batch a writer thread is working on, written under the queue lock.
#[derive(Debug, Default)]
struct InFlight {
    rows: AtomicU64,
    /// `u64::from(bool)`; 1 when the batch carries an unbacked row.
    unbacked: AtomicU64,
}

/// A writer thread's queue, shared with the shutdown join so an abandoned
/// thread's batches can be counted in rows.
///
/// The thread takes each batch with [`OffloadQueue::next`], which records it as
/// in flight before the lock is released, and clears it with
/// [`OffloadQueue::finished`]. The join reads both only after the producer
/// closed the queue, when the thread holds the lock only for an instant, so
/// `try_lock` succeeds unless that instant is now (then the residue is
/// unknown, and said so). One uncontended lock per batch, on the writer
/// thread, never on the frame drain.
pub(super) struct OffloadQueue<T> {
    queue: Arc<Mutex<Receiver<T>>>,
    in_flight: Arc<InFlight>,
    measure: fn(&T) -> (u64, bool),
}

impl<T> Clone for OffloadQueue<T> {
    fn clone(&self) -> Self {
        Self {
            queue: Arc::clone(&self.queue),
            in_flight: Arc::clone(&self.in_flight),
            measure: self.measure,
        }
    }
}

impl<T> OffloadQueue<T> {
    /// Wraps a receiver. `measure` returns a batch's rows and whether any of
    /// them has no WAL record.
    pub(super) fn new(rx: Receiver<T>, measure: fn(&T) -> (u64, bool)) -> Self {
        Self {
            queue: Arc::new(Mutex::new(rx)),
            in_flight: Arc::new(InFlight::default()),
            measure,
        }
    }

    /// Blocks for the next batch; `None` once the producer closed the queue
    /// and it is empty. The batch is recorded as in flight before the lock is
    /// released, so the join never sees it in neither place.
    pub(super) fn next(&self) -> Option<T> {
        let guard = match self.queue.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let batch = guard.recv().ok()?;
        let (rows, unbacked) = (self.measure)(&batch);
        self.in_flight.rows.store(rows, Ordering::Release);
        self.in_flight
            .unbacked
            .store(u64::from(unbacked), Ordering::Release);
        drop(guard);
        Some(batch)
    }

    /// The batch from the last [`Self::next`] is done (landed or rescued).
    pub(super) fn finished(&self) {
        self.in_flight.rows.store(0, Ordering::Release);
        self.in_flight.unbacked.store(0, Ordering::Release);
    }

    /// What the thread still holds: the batch in flight plus every batch
    /// still queued, which this call takes (and drops: the process is
    /// exiting, and a dropped rescue batch keeps its watermark floor, so its
    /// backed rows stay replayable). `None` when the thread holds the lock at
    /// this instant, so the queue cannot be read.
    pub(super) fn abandoned_residue(&self) -> Option<BatchResidue> {
        let guard = match self.queue.try_lock() {
            Ok(guard) => guard,
            Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
            Err(TryLockError::WouldBlock) => return None,
        };
        let mut residue = BatchResidue::default();
        let in_flight_rows = self.in_flight.rows.load(Ordering::Acquire);
        if in_flight_rows > 0 {
            residue.note(
                in_flight_rows,
                self.in_flight.unbacked.load(Ordering::Acquire) != 0,
            );
        }
        while let Ok(batch) = guard.try_recv() {
            let (rows, unbacked) = (self.measure)(&batch);
            residue.note(rows, unbacked);
        }
        Some(residue)
    }
}

/// Counts an abandoned thread's unbacked rows on `tv_ticks_lost_total`.
/// `source` is one of the `*_ABANDONED_SOURCE` constants; the caller's coded
/// line carries [`residue_fields`].
pub(super) fn count_abandoned_rows(source: &'static str, residue: Option<BatchResidue>) {
    if let Some(r) = residue
        && r.unbacked_rows > 0
    {
        // The caller's coded `error!` (HOT-PATH-02) names these rows.
        metrics::counter!("tv_ticks_lost_total", "source" => source).increment(r.unbacked_rows);
    }
}

/// The three numbers a coded abandonment line carries; `-1` each when the
/// queue could not be read.
pub(super) fn residue_fields(residue: Option<BatchResidue>) -> (i64, i64, i64) {
    residue.map_or((-1, -1, -1), |r| {
        (
            i64::try_from(r.batches).unwrap_or(i64::MAX),
            i64::try_from(r.rows).unwrap_or(i64::MAX),
            i64::try_from(r.unbacked_rows).unwrap_or(i64::MAX),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_residue_splits_backed_from_unbacked() {
        let frames = [true, false, true, true, false];
        let mut iter = frames.iter().copied();
        let residue = drain_ring_residue(|| iter.next(), usize::MAX, |b| *b);
        assert_eq!(
            residue,
            RingResidue {
                backed: 3,
                unbacked: 2
            }
        );
        assert!(!residue.is_empty());
    }

    #[test]
    fn ring_residue_empty_ring_is_empty() {
        let residue = drain_ring_residue(|| None::<bool>, 10, |b| *b);
        assert!(residue.is_empty());
    }

    #[test]
    fn ring_residue_stops_at_the_limit_on_an_endless_source() {
        let mut calls = 0_u64;
        let residue = drain_ring_residue(
            || {
                calls += 1;
                Some(false)
            },
            4,
            |b| *b,
        );
        assert_eq!(
            residue.unbacked, 4,
            "a socket still delivering cannot hold the drain"
        );
        assert_eq!(calls, 4);
    }

    #[test]
    fn ring_residue_on_frame_sees_every_frame() {
        let mut seen = 0;
        let mut iter = [true, true].into_iter();
        let _ = drain_ring_residue(
            || iter.next(),
            8,
            |b| {
                seen += 1;
                *b
            },
        );
        assert_eq!(
            seen, 2,
            "every drained frame must have its ring budget released"
        );
    }

    #[test]
    fn report_ring_residue_handles_all_three_shapes() {
        report_ring_residue(RingResidue::default());
        report_ring_residue(RingResidue {
            backed: 2,
            unbacked: 0,
        });
        report_ring_residue(RingResidue {
            backed: 1,
            unbacked: 3,
        });
    }

    #[test]
    fn batch_residue_counts_unbacked_rows_only_for_flagged_batches() {
        let mut r = BatchResidue::default();
        r.note(100, false);
        r.note(40, true);
        r.note(0, true);
        assert_eq!(
            r,
            BatchResidue {
                batches: 3,
                rows: 140,
                unbacked_rows: 40
            }
        );
    }

    #[test]
    fn batch_residue_saturates_instead_of_wrapping() {
        let mut r = BatchResidue::default();
        r.note(u64::MAX, true);
        r.note(5, true);
        assert_eq!(r.rows, u64::MAX);
        assert_eq!(r.unbacked_rows, u64::MAX);
    }

    fn measure_pair(b: &(u64, bool)) -> (u64, bool) {
        *b
    }

    #[test]
    fn offload_queue_counts_in_flight_plus_queued_after_close() {
        let (tx, rx) = std::sync::mpsc::sync_channel::<(u64, bool)>(8);
        let queue = OffloadQueue::new(rx, measure_pair);
        tx.send((10, true)).unwrap();
        tx.send((20, false)).unwrap();
        tx.send((5, true)).unwrap();
        drop(tx);
        // The thread took the first batch and is stuck writing it.
        let first = queue.next().unwrap();
        assert_eq!(first, (10, true));
        let residue = queue.abandoned_residue().unwrap();
        assert_eq!(
            residue,
            BatchResidue {
                batches: 3,
                rows: 35,
                unbacked_rows: 15
            }
        );
    }

    #[test]
    fn offload_queue_finished_batch_is_not_counted() {
        let (tx, rx) = std::sync::mpsc::sync_channel::<(u64, bool)>(4);
        let queue = OffloadQueue::new(rx, measure_pair);
        tx.send((7, true)).unwrap();
        drop(tx);
        let _ = queue.next().unwrap();
        queue.finished();
        assert_eq!(queue.abandoned_residue().unwrap(), BatchResidue::default());
        assert!(
            queue.next().is_none(),
            "a closed, empty queue ends the thread loop"
        );
    }

    #[test]
    fn offload_queue_reports_unknown_while_the_lock_is_held() {
        let (_tx, rx) = std::sync::mpsc::sync_channel::<(u64, bool)>(1);
        let queue = OffloadQueue::new(rx, measure_pair);
        let held = queue.queue.lock().unwrap();
        assert!(queue.abandoned_residue().is_none());
        drop(held);
        assert!(queue.abandoned_residue().is_some());
    }

    #[test]
    fn offload_queue_survives_a_poisoned_lock() {
        let (tx, rx) = std::sync::mpsc::sync_channel::<(u64, bool)>(2);
        let queue = OffloadQueue::new(rx, measure_pair);
        let poisoner = queue.clone();
        let _ = std::thread::spawn(move || {
            let _guard = poisoner.queue.lock().unwrap();
            panic!("poison the queue lock");
        })
        .join();
        tx.send((3, true)).unwrap();
        assert_eq!(queue.abandoned_residue().unwrap().unbacked_rows, 3);
        tx.send((4, false)).unwrap();
        assert_eq!(queue.next(), Some((4, false)));
    }

    #[test]
    fn count_abandoned_rows_and_fields_cover_known_and_unknown() {
        count_abandoned_rows(RESCUE_ABANDONED_SOURCE, None);
        count_abandoned_rows(
            WRITER_ABANDONED_SOURCE,
            Some(BatchResidue {
                batches: 1,
                rows: 9,
                unbacked_rows: 9,
            }),
        );
        assert_eq!(residue_fields(None), (-1, -1, -1));
        assert_eq!(
            residue_fields(Some(BatchResidue {
                batches: 2,
                rows: 30,
                unbacked_rows: 12
            })),
            (2, 30, 12)
        );
        seed_shutdown_loss_baselines();
    }

    /// The ring tail spends part of `main`'s lane budget, so offload grace +
    /// pre-join margin + tail must still fit. `main.rs`'s constants are
    /// private, so they are read from source (one scan of `main.rs` through
    /// `std::fs`, not `include_str!`, matching the house budget rule).
    #[test]
    fn the_ring_tail_budget_fits_the_lane_budget() {
        let main_rs = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/main.rs"),
        )
        .expect("main.rs must be readable");
        let budget: u64 = main_rs
            .split_once("const DHAN_LANE_SHUTDOWN_FLUSH_BUDGET_SECS: u64 = ")
            .and_then(|(_, rest)| rest.split_once(';'))
            .and_then(|(n, _)| n.trim().parse().ok())
            .expect("main.rs must declare the lane shutdown flush budget");
        let margin = 2 * tickvault_storage::depth_persistence::ILP_REQUEST_TIMEOUT_SECS;
        assert!(
            super::super::OFFLOAD_SHUTDOWN_GRACE_SECS + margin + SHUTDOWN_RING_TAIL_BUDGET_SECS
                <= budget,
            "the ring tail no longer fits main's lane budget ({budget} s)"
        );
    }
}
