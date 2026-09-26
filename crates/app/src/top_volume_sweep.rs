//! The top-volume sweep, taken in SLICES so the frame drain never stalls on it
//! (audit PR4, 2026-09-26).
//!
//! # Why this exists
//!
//! Until 2026-09-26 each of the four cadence timer arms on the frame drain
//! ranked the whole board in one call: a walk over every contract that traded
//! in the window, a sort, the gainer filter and the row projection. MEASURED
//! at **2.95 ms** for the ranking alone where every one of 20,220 contracts
//! traded (`rank_sweep_cost_at_the_authorized_ceiling`), plus the projection
//! after it — and the drain reads no frame while it runs. The owner's ask was
//! that nothing blocks the tick loop, p99 included.
//!
//! # What changed, and what did not
//!
//! The work is the SAME work and the total is the same order — O(k log k) in
//! contracts that traded, which is inherent: the output is one sorted row per
//! traded contract. What changed is its SHAPE on the drain:
//!
//! - the timer arm pays **O(1)**: it swaps each family's work list into an
//!   in-flight slot ([`crate::volume_leaderboard::VolumeLeaderboard::begin_sweep`])
//!   and queues a [`SweepJob`];
//! - an idle arm placed LAST in the drain's `biased` select advances the job
//!   by at most [`TOP_VOLUME_SWEEP_STEP_ROWS`] rows per call, so a queued frame
//!   waits for at most one step — a constant, not the size of the board.
//!
//! # Why not a separate ranking thread
//!
//! The plan said "a dedicated ranking thread". It was not taken because the
//! projection reads the candle fold (`bar_for_window`) for every row, and the
//! aggregator is single-owner `&mut` on the drain by a documented decision —
//! sharing it would put a lock or an epoch on the per-tick fold. Slicing keeps
//! every structure single-owner and gives the drain the property that was
//! asked for: a bounded stall.
//!
//! # Honest consequences
//!
//! - A contract's window now ends when its key is visited, not at the timer
//!   instant — up to one sweep's duration later. Volume is cumulative and each
//!   visit rolls the baseline it read, so nothing is lost or counted twice; a
//!   trade in that gap is reported in this window instead of the next.
//! - If a cadence fires while its previous job is still queued or running, the
//!   fire is DEFERRED and its window is DROPPED: the fire rolls that window's
//!   baselines, so the next job measures only its own window instead of two
//!   windows under one `ts`. The skipped window has no rows; the candle tables
//!   still carry its volume. Counted on [`TOP_VOLUME_SWEEP_DEFERRED_COUNTER`]
//!   and logged.
//! - The candle columns are read when the row is projected, not at the fire.
//!   The fold keeps the open bar and the last sealed one, so a job that
//!   projects after its window has rolled out of the fold stores those columns
//!   empty. Counted on [`TOP_VOLUME_SWEEP_CANDLE_MISS_COUNTER`] and logged.

use std::cmp::Ordering;
use std::collections::VecDeque;

use tickvault_storage::top_volume_rank_persistence::{SnapshotCadence, TopVolumeLabelMap};

use crate::volume_leaderboard::{GainerWalk, RankedContract, WINDOW_COUNT};

/// Rows a single step of the sweep may touch. At the measured ~50–150 ns per
/// row (one to three hash probes and a row build), one step is tens of
/// microseconds — the longest a queued frame waits behind the sweep.
pub const TOP_VOLUME_SWEEP_STEP_ROWS: usize = 512;

/// Counts cadence fires that found their previous job still queued or
/// running, so their window was skipped.
pub const TOP_VOLUME_SWEEP_DEFERRED_COUNTER: &str = "tv_top_volume_sweep_deferred_total";

/// Counts projected rows whose candle bar was no longer in the fold when the
/// row was projected, so its candle columns were stored empty.
pub const TOP_VOLUME_SWEEP_CANDLE_MISS_COUNTER: &str = "tv_top_volume_candle_bar_missing_total";

/// Length of the runs the sliced sort sorts in place before merging.
const SORT_RUN: usize = 256;

/// A merge sort that runs a bounded amount of work per call.
///
/// Sorts runs of [`SORT_RUN`] elements in place, then merges them bottom-up
/// through a second buffer, swapping the two at the end of each pass. With a
/// comparator that is a TOTAL order over distinct elements the result is the
/// same sequence `sort_unstable_by` produces — pinned by the property test
/// below. Both buffers keep their capacity, so a sort allocates nothing once
/// the buffers are sized.
#[derive(Debug)]
pub struct SliceSort<T: Copy> {
    aux: Vec<T>,
    phase: SortPhase,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SortPhase {
    Runs {
        next: usize,
    },
    Merge {
        width: usize,
        start: usize,
        left: usize,
        right: usize,
    },
    Done,
}

impl<T: Copy> SliceSort<T> {
    /// A sorter whose merge buffer holds `capacity` elements without growing.
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            aux: Vec::with_capacity(capacity),
            phase: SortPhase::Done,
        }
    }

    /// Starts a new sort. The data passed to the following `step` calls must
    /// not change length until the sort finishes.
    pub fn reset(&mut self) {
        self.aux.clear();
        self.phase = SortPhase::Runs { next: 0 };
    }

    /// Advances the sort by about `budget` element moves. Returns `true` when
    /// `data` is fully sorted.
    pub fn step<F>(&mut self, data: &mut Vec<T>, budget: usize, cmp: F) -> bool
    where
        F: Fn(&T, &T) -> Ordering,
    {
        let len = data.len();
        let mut work = 0usize;
        loop {
            match self.phase {
                SortPhase::Done => return true,
                SortPhase::Runs { next } => {
                    if next >= len {
                        if len <= SORT_RUN {
                            self.phase = SortPhase::Done;
                            return true;
                        }
                        self.aux.clear();
                        self.phase = SortPhase::Merge {
                            width: SORT_RUN,
                            start: 0,
                            left: 0,
                            right: SORT_RUN,
                        };
                        continue;
                    }
                    let end = next.saturating_add(SORT_RUN).min(len);
                    if let Some(run) = data.get_mut(next..end) {
                        run.sort_unstable_by(&cmp);
                    }
                    work = work.saturating_add(end - next);
                    self.phase = SortPhase::Runs { next: end };
                }
                SortPhase::Merge {
                    width,
                    start,
                    mut left,
                    mut right,
                } => {
                    if start >= len {
                        // One pass merged: the merged sequence is in `aux`.
                        std::mem::swap(data, &mut self.aux);
                        self.aux.clear();
                        let doubled = width.saturating_mul(2);
                        if doubled >= len {
                            self.phase = SortPhase::Done;
                            return true;
                        }
                        self.phase = SortPhase::Merge {
                            width: doubled,
                            start: 0,
                            left: 0,
                            right: doubled.min(len),
                        };
                        continue;
                    }
                    let mid = start.saturating_add(width).min(len);
                    let end = start.saturating_add(width.saturating_mul(2)).min(len);
                    while work < budget && (left < mid || right < end) {
                        let take_left = match (data.get(left), data.get(right)) {
                            (Some(l), Some(r)) if left < mid && right < end => {
                                cmp(l, r) != Ordering::Greater
                            }
                            _ => left < mid,
                        };
                        let from = if take_left { left } else { right };
                        if let Some(value) = data.get(from) {
                            self.aux.push(*value);
                        }
                        if take_left {
                            left = left.saturating_add(1);
                        } else {
                            right = right.saturating_add(1);
                        }
                        work = work.saturating_add(1);
                    }
                    self.phase = if left >= mid && right >= end {
                        SortPhase::Merge {
                            width,
                            start: end,
                            left: end,
                            right: end.saturating_add(width).min(len),
                        }
                    } else {
                        SortPhase::Merge {
                            width,
                            start,
                            left,
                            right,
                        }
                    };
                }
            }
            if work >= budget {
                return self.phase == SortPhase::Done;
            }
        }
    }
}

/// One cadence fire waiting to be ranked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SweepJob {
    /// The cadence that fired.
    pub cadence: SnapshotCadence,
    /// The fire instant, IST nanoseconds — the snapshot's `ts` is derived from
    /// THIS, never from when a later step happens to run, so a sliced job
    /// writes the same grid cell an inline one would have.
    pub ts_ist_nanos: i64,
    /// A `top_volume` writer exists, so rows are projected and handed off.
    pub wants_rows: bool,
    /// The depth-200 candidates are due from this cadence.
    pub wants_candidates: bool,
}

/// Where the running job is, per family.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SweepPhase {
    /// Visiting the family's in-flight keys into `rows`.
    Collect,
    /// Sorting `rows` into board order.
    Sort,
    /// Walking the sorted rows for gainer-eligible contracts.
    Gainers,
    /// Publishing the depth-20 and depth-200 candidate lists.
    Publish,
    /// Projecting and staging rows `[pos..]`.
    Project {
        /// First row not yet projected.
        pos: usize,
    },
}

/// The job being worked, with its per-family progress.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActiveSweep {
    /// The job.
    pub job: SweepJob,
    /// Index into the drain's ranked-family list.
    pub family_idx: usize,
    /// Progress within that family.
    pub phase: SweepPhase,
    /// Rows staged so far across families.
    pub appended: usize,
    /// Rows refused so far across families.
    pub refused: usize,
}

/// A job that finished, for the drain's per-cadence accounting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SweepDone {
    /// The cadence the job ranked.
    pub cadence: SnapshotCadence,
    /// Rows staged for the writer.
    pub appended: usize,
    /// Rows refused.
    pub refused: usize,
}

/// The queue and reusable buffers of the sliced sweep. Owned by the drain.
#[derive(Debug)]
pub struct TopVolumeSweep {
    queue: VecDeque<SweepJob>,
    /// Bit per cadence slot: a job for that cadence is queued or running.
    busy_mask: u8,
    /// The job being worked, if any.
    pub active: Option<ActiveSweep>,
    /// The collected rows of the family being worked, sorted in place.
    pub rows: Vec<RankedContract>,
    /// The sliced sorter for `rows`.
    pub sort: SliceSort<RankedContract>,
    /// The sliced gainer walk over `rows`.
    pub gainers: GainerWalk,
    /// The label snapshot the running family's projection reads, taken on
    /// its first chunk and handed to the writer with the batch.
    pub labels: Option<std::sync::Arc<TopVolumeLabelMap>>,
    deferred: u64,
    deferred_counter: metrics::Counter,
    candle_misses: u64,
    candle_miss_counter: metrics::Counter,
}

impl TopVolumeSweep {
    /// Buffers sized for `row_capacity` rows, allocated once here.
    #[must_use]
    pub fn with_capacity(row_capacity: usize, gainer_capacity: usize) -> Self {
        Self {
            // One job per cadence at most — see `try_enqueue`.
            queue: VecDeque::with_capacity(WINDOW_COUNT),
            busy_mask: 0,
            active: None,
            rows: Vec::with_capacity(row_capacity),
            sort: SliceSort::with_capacity(row_capacity),
            gainers: GainerWalk::with_capacity(gainer_capacity),
            labels: None,
            deferred: 0,
            deferred_counter: metrics::counter!(TOP_VOLUME_SWEEP_DEFERRED_COUNTER),
            candle_misses: 0,
            candle_miss_counter: metrics::counter!(TOP_VOLUME_SWEEP_CANDLE_MISS_COUNTER),
        }
    }

    /// Whether a job for `cadence` is queued or running.
    #[must_use]
    pub fn is_busy(&self, cadence: SnapshotCadence) -> bool {
        cadence
            .slot()
            .is_some_and(|slot| self.busy_mask & (1u8 << slot) != 0)
    }

    /// Whether any job is queued or running — the drain's idle-arm guard.
    #[must_use]
    pub fn has_work(&self) -> bool {
        self.active.is_some() || !self.queue.is_empty()
    }

    /// Queues a job. The caller has already checked [`Self::is_busy`] and
    /// swapped the work lists in; one job per cadence keeps the queue within
    /// its pre-sized capacity.
    pub fn enqueue(&mut self, job: SweepJob) {
        if let Some(slot) = job.cadence.slot() {
            self.busy_mask |= 1u8 << slot;
        }
        self.queue.push_back(job);
    }

    /// Counts a fire that found its cadence busy, and returns the running
    /// total so the caller can log on powers of two.
    pub fn record_deferred(&mut self) -> u64 {
        self.deferred = self.deferred.saturating_add(1);
        self.deferred_counter.increment(1);
        self.deferred
    }

    /// Counts `misses` rows projected without a candle bar. Returns the new
    /// session total when this call crossed a power of two, so the caller
    /// logs the onset and the magnitude without logging every chunk.
    pub fn record_candle_misses(&mut self, misses: usize) -> Option<u64> {
        if misses == 0 {
            return None;
        }
        let before = self.candle_misses;
        self.candle_misses = before.saturating_add(misses as u64);
        self.candle_miss_counter.increment(misses as u64);
        (before.checked_ilog2() != self.candle_misses.checked_ilog2()).then_some(self.candle_misses)
    }

    /// Takes the next queued job as the active one, if nothing is running.
    /// Returns whether a job is now active.
    pub fn activate_next(&mut self) -> bool {
        if self.active.is_none()
            && let Some(job) = self.queue.pop_front()
        {
            self.rows.clear();
            self.active = Some(ActiveSweep {
                job,
                family_idx: 0,
                phase: SweepPhase::Collect,
                appended: 0,
                refused: 0,
            });
        }
        self.active.is_some()
    }

    /// Ends the active job and frees its cadence.
    pub fn finish_active(&mut self) -> Option<SweepDone> {
        let active = self.active.take()?;
        if let Some(slot) = active.job.cadence.slot() {
            self.busy_mask &= !(1u8 << slot);
        }
        self.rows.clear();
        self.labels = None;
        Some(SweepDone {
            cadence: active.job.cadence,
            appended: active.appended,
            refused: active.refused,
        })
    }

    /// Drops every queued and running job — the daily reset, which also
    /// empties the in-flight lists the jobs would have visited.
    pub fn clear(&mut self) {
        self.queue.clear();
        self.active = None;
        self.busy_mask = 0;
        self.rows.clear();
        self.labels = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn total(a: &(u64, u64), b: &(u64, u64)) -> Ordering {
        b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1))
    }

    fn run_to_end(data: &mut Vec<(u64, u64)>, budget: usize) -> usize {
        let mut sorter = SliceSort::with_capacity(data.len());
        sorter.reset();
        let mut steps = 0usize;
        while !sorter.step(data, budget, total) {
            steps += 1;
            assert!(steps < 1_000_000, "the sliced sort must terminate");
        }
        steps
    }

    proptest! {
        #[test]
        fn slice_sort_step_matches_sort_unstable_by(
            keys in proptest::collection::vec(0u64..50, 0..3_000),
            budget in 1usize..2_000,
        ) {
            // Distinct second component: the order is TOTAL, as `board_order` is.
            let mut data: Vec<(u64, u64)> =
                keys.iter().enumerate().map(|(i, k)| (*k, i as u64)).collect();
            let mut expected = data.clone();
            expected.sort_unstable_by(total);
            let _steps = run_to_end(&mut data, budget);
            prop_assert_eq!(data, expected);
        }
    }

    #[test]
    fn slice_sort_step_bounds_the_work_of_one_call() {
        let mut data: Vec<(u64, u64)> = (0..20_000u64).map(|i| (i % 97, i)).collect();
        let mut expected = data.clone();
        expected.sort_unstable_by(total);
        let steps = run_to_end(&mut data, 512);
        assert_eq!(data, expected);
        // 20,000 rows at 512 per call cannot finish in one call: the sort
        // really is spread over many steps, which is the whole point.
        assert!(steps > 20_000 / 512, "took {steps} steps");
    }

    #[test]
    fn slice_sort_step_keeps_both_buffers_capacity() {
        let mut data: Vec<(u64, u64)> = Vec::with_capacity(4_096);
        data.extend((0..4_000u64).map(|i| (i % 13, i)));
        let mut sorter = SliceSort::with_capacity(4_096);
        sorter.reset();
        while !sorter.step(&mut data, 300, total) {}
        assert!(data.capacity() >= 4_096);
        assert!(sorter.aux.capacity() >= 4_096);
        assert!(
            data.windows(2)
                .all(|w| total(&w[0], &w[1]) != Ordering::Greater)
        );
    }

    #[test]
    fn a_busy_cadence_is_refused_until_its_job_finishes() {
        let mut sweep = TopVolumeSweep::with_capacity(16, 8);
        let job = SweepJob {
            cadence: SnapshotCadence::OneSecond,
            ts_ist_nanos: 1,
            wants_rows: true,
            wants_candidates: false,
        };
        assert!(!sweep.is_busy(SnapshotCadence::OneSecond));
        sweep.enqueue(job);
        assert!(sweep.is_busy(SnapshotCadence::OneSecond));
        assert!(!sweep.is_busy(SnapshotCadence::OneMinute));
        assert!(sweep.has_work());
        assert!(sweep.activate_next());
        assert!(sweep.is_busy(SnapshotCadence::OneSecond));
        let done = sweep.finish_active().expect("a job was active");
        assert_eq!(done.cadence, SnapshotCadence::OneSecond);
        assert!(!sweep.is_busy(SnapshotCadence::OneSecond));
        assert!(!sweep.has_work());
        assert_eq!(sweep.record_deferred(), 1);
    }

    #[test]
    fn clear_drops_queued_and_active_jobs() {
        let mut sweep = TopVolumeSweep::with_capacity(16, 8);
        for cadence in [SnapshotCadence::OneSecond, SnapshotCadence::OneMinute] {
            sweep.enqueue(SweepJob {
                cadence,
                ts_ist_nanos: 1,
                wants_rows: false,
                wants_candidates: true,
            });
        }
        assert!(sweep.activate_next());
        sweep.clear();
        assert!(!sweep.has_work());
        assert!(!sweep.is_busy(SnapshotCadence::OneMinute));
    }

    fn job(cadence: SnapshotCadence) -> SweepJob {
        SweepJob {
            cadence,
            ts_ist_nanos: 7,
            wants_rows: true,
            wants_candidates: true,
        }
    }

    #[test]
    fn test_with_capacity_sizes_every_buffer_up_front() {
        let sweep = TopVolumeSweep::with_capacity(1_000, 300);
        assert!(sweep.rows.capacity() >= 1_000);
        assert!(sweep.sort.aux.capacity() >= 1_000);
        assert!(sweep.queue.capacity() >= WINDOW_COUNT);
        assert!(sweep.active.is_none());
        assert!(sweep.labels.is_none());
        let sorter: SliceSort<u64> = SliceSort::with_capacity(64);
        assert!(sorter.aux.capacity() >= 64);
    }

    #[test]
    fn test_is_busy_is_per_cadence() {
        let mut sweep = TopVolumeSweep::with_capacity(4, 4);
        sweep.enqueue(job(SnapshotCadence::ThreeSecond));
        assert!(sweep.is_busy(SnapshotCadence::ThreeSecond));
        assert!(!sweep.is_busy(SnapshotCadence::OneSecond));
        assert!(!sweep.is_busy(SnapshotCadence::FiveSecond));
        assert!(!sweep.is_busy(SnapshotCadence::OneMinute));
    }

    #[test]
    fn test_has_work_covers_queued_and_active_jobs() {
        let mut sweep = TopVolumeSweep::with_capacity(4, 4);
        assert!(!sweep.has_work());
        sweep.enqueue(job(SnapshotCadence::OneSecond));
        assert!(sweep.has_work(), "a queued job is work");
        assert!(sweep.activate_next());
        assert!(sweep.queue.is_empty());
        assert!(sweep.has_work(), "an active job is work");
        let _ = sweep.finish_active();
        assert!(!sweep.has_work());
    }

    #[test]
    fn test_enqueue_keeps_fire_order_and_stays_within_capacity() {
        let mut sweep = TopVolumeSweep::with_capacity(4, 4);
        let cap = sweep.queue.capacity();
        let order = [
            SnapshotCadence::OneMinute,
            SnapshotCadence::OneSecond,
            SnapshotCadence::FiveSecond,
            SnapshotCadence::ThreeSecond,
        ];
        for cadence in order {
            sweep.enqueue(job(cadence));
        }
        assert_eq!(
            sweep.queue.capacity(),
            cap,
            "one job per cadence never regrows"
        );
        for cadence in order {
            assert!(sweep.activate_next());
            let done = sweep.finish_active().expect("active");
            assert_eq!(done.cadence, cadence);
        }
    }

    #[test]
    fn test_record_deferred_counts_every_refused_fire() {
        let mut sweep = TopVolumeSweep::with_capacity(4, 4);
        assert_eq!(sweep.record_deferred(), 1);
        assert_eq!(sweep.record_deferred(), 2);
        assert_eq!(sweep.record_deferred(), 3);
    }

    #[test]
    fn test_activate_next_does_not_replace_a_running_job() {
        let mut sweep = TopVolumeSweep::with_capacity(4, 4);
        assert!(!sweep.activate_next(), "nothing queued");
        sweep.enqueue(job(SnapshotCadence::OneSecond));
        sweep.enqueue(job(SnapshotCadence::OneMinute));
        assert!(sweep.activate_next());
        assert!(sweep.activate_next(), "still the first job");
        assert_eq!(
            sweep.active.as_ref().map(|a| a.job.cadence),
            Some(SnapshotCadence::OneSecond)
        );
        assert_eq!(sweep.queue.len(), 1);
        assert_eq!(
            sweep.active.as_ref().map(|a| a.phase),
            Some(SweepPhase::Collect)
        );
    }

    #[test]
    fn test_record_candle_misses_reports_on_powers_of_two() {
        let mut sweep = TopVolumeSweep::with_capacity(4, 4);
        assert_eq!(sweep.record_candle_misses(0), None, "no misses, no report");
        assert_eq!(sweep.record_candle_misses(1), Some(1));
        assert_eq!(sweep.record_candle_misses(1), Some(2));
        assert_eq!(sweep.record_candle_misses(1), None, "3 is inside [2, 4)");
        assert_eq!(sweep.record_candle_misses(5), Some(8));
        assert_eq!(sweep.record_candle_misses(3), None, "11 is inside [8, 16)");
    }

    #[test]
    fn test_finish_active_frees_the_cadence_and_reports_counts() {
        let mut sweep = TopVolumeSweep::with_capacity(4, 4);
        assert!(sweep.finish_active().is_none(), "nothing active");
        sweep.enqueue(job(SnapshotCadence::FiveSecond));
        assert!(sweep.activate_next());
        if let Some(active) = sweep.active.as_mut() {
            active.appended = 11;
            active.refused = 2;
        }
        let done = sweep.finish_active().expect("active");
        assert_eq!((done.appended, done.refused), (11, 2));
        assert!(!sweep.is_busy(SnapshotCadence::FiveSecond));
        assert!(sweep.rows.is_empty());
        assert!(sweep.labels.is_none());
    }
}
