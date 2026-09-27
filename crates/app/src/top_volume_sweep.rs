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
//! The work is the SAME work; what changed is its SHAPE on the drain. The total
//! is O(k) in contracts that traded since audit PR4c (2026-09-26), when the
//! board's sliced merge sort (O(k log k)) became a sliced LSD radix sort on the
//! integer rank key ([`SliceRadixSort`]). O(k) is the floor, not a shortfall:
//! the output is one sorted row per traded contract. On the drain:
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
//! - Since audit PR4c-2 (2026-09-26) a window is ranked once, after its candle
//!   closes ([`WindowCloseClock`]), and each contract's volume is read from
//!   that window's own candle bar in the fold. There is no baseline and no
//!   roll. The board reads the bar when the key is visited: a tick that
//!   reaches the fold after the close margin but before the visit is in the
//!   board if its contract was already on the job's list, and one that
//!   arrives after the visit is in the candle and not in the board.
//! - **A late tick for a contract NOT already on the job's list** (its only
//!   trade in the window arrives after the list was taken, i.e. more than the
//!   one-second close margin behind the newest tick of the feed, as a lagging
//!   socket can deliver) is in that window's candle and on no board: the next
//!   job reads the next window's bar and finds none. Not counted, because a
//!   visit cannot tell it apart from a key left over from a window before the
//!   capture window. The candle table is the record of it.
//! - A window that closes while its cadence's previous job is still queued or
//!   running WAITS: the close clock reports windows oldest first and is only
//!   advanced when a board can be queued, so windows that close together are
//!   each ranked. Only when it falls more than
//!   [`WINDOW_CLOSE_CATCH_UP_WINDOWS`] behind are the oldest skipped, counted
//!   on [`TOP_VOLUME_SWEEP_DEFERRED_COUNTER`] and logged; the candle tables
//!   still carry their volume.
//! - The fold keeps only a contract's open bar and its last sealed one, and a
//!   contract that trades every second seals over a 1-second window's bar
//!   before any sweep can reach it. So every seal of a board frame also
//!   leaves the window's volume in the leaderboard
//!   ([`crate::volume_leaderboard::SEALED_WINDOW_HISTORY`] windows deep),
//!   and the board reads it there. A contract whose window is gone from both
//!   is left off the board and counted, not ranked on a guess.
//! - The candle columns are read when the row is projected, not at the close.
//!   The fold keeps the open bar and the last sealed one, so a job that
//!   projects after its window has rolled out of the fold stores those columns
//!   empty. Counted on [`TOP_VOLUME_SWEEP_CANDLE_MISS_COUNTER`] and logged.

use std::collections::VecDeque;

use tickvault_storage::top_volume_rank_persistence::{SnapshotCadence, TopVolumeLabelMap};

use crate::volume_leaderboard::{GainerWalk, RankedContract, SEALED_WINDOW_HISTORY, WINDOW_COUNT};

/// Rows a single step of the sweep may touch. At the measured ~50–150 ns per
/// row (one to three hash probes and a row build), one step is tens of
/// microseconds — the longest a queued frame waits behind the sweep.
pub const TOP_VOLUME_SWEEP_STEP_ROWS: usize = 512;

/// Counts windows that got no board: a cadence closed while its previous job
/// was still queued or running, or the drain only noticed a later close.
pub const TOP_VOLUME_SWEEP_DEFERRED_COUNTER: &str = "tv_top_volume_sweep_deferred_total";

/// Seconds the CANDLE clock (the fold's watermark, the newest exchange second
/// it has consumed) must run past a window's close before the window is
/// ranked. Dhan stamps a tick with a whole exchange second, so a tick of the
/// window's last second can still arrive after the first tick of the next
/// second; one second of margin lets it land in the bar before the board
/// reads that bar.
pub const WINDOW_CLOSE_LATENESS_SECS: u32 = 1;

/// Seconds the WALL clock must run past a window's close before the window is
/// ranked even though no tick moved the candle clock: a quiet contract set, a
/// feed gap, or the minutes before the first tick of the day.
pub const WINDOW_CLOSE_GRACE_SECS: u32 = 5;

/// The instant, IST epoch seconds, up to which windows count as closed:
/// the candle clock less [`WINDOW_CLOSE_LATENESS_SECS`], capped AT the wall
/// clock, and never earlier than the wall clock less
/// [`WINDOW_CLOSE_GRACE_SECS`].
///
/// The cap is the wall itself, with no lead: one future-dated tick moves the
/// watermark for the rest of the day, and any lead would let it rank a window
/// whose end has not happened yet, from a partial bar. Capped at the wall, a
/// board is never ranked before its window has ended by our own clock, and
/// the candle clock only ever makes a close EARLIER than the grace would.
///
/// # Complexity
/// O(1): two saturating subtractions and two compares.
#[must_use]
pub const fn window_close_reference_secs(watermark_secs: u32, wall_secs: u32) -> u32 {
    let by_candles = watermark_secs.saturating_sub(WINDOW_CLOSE_LATENESS_SECS);
    let by_candles = if by_candles > wall_secs {
        wall_secs
    } else {
        by_candles
    };
    let by_wall = wall_secs.saturating_sub(WINDOW_CLOSE_GRACE_SECS);
    if by_candles > by_wall {
        by_candles
    } else {
        by_wall
    }
}

/// Closed windows of one cadence the clock still catches up on, oldest
/// first. Older ones are skipped and counted: by the time the clock is that
/// far behind, a contract that trades every window may have sealed over the
/// oldest one's bar in the leaderboard's sealed-window history
/// ([`SEALED_WINDOW_HISTORY`] deep), so its board could no longer be read
/// in full.
pub const WINDOW_CLOSE_CATCH_UP_WINDOWS: u32 = SEALED_WINDOW_HISTORY as u32 - 1;

const _: () = assert!(
    WINDOW_CLOSE_CATCH_UP_WINDOWS >= 1,
    "the clock must be able to report at least the newest closed window"
);

/// A window the clock just saw close.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowClosed {
    /// The window's open, IST epoch seconds.
    pub window_open_ist_secs: u32,
    /// Earlier windows of the same IST day that closed and will never be
    /// reported: the clock fell more than [`WINDOW_CLOSE_CATCH_UP_WINDOWS`]
    /// windows behind. They get no board.
    pub skipped: u64,
}

/// Per-cadence record of the next window to close (audit PR4c-2).
///
/// The boards are ranked once per window, when its candle closes — the
/// operator's 2026-09-26 rule — so something has to notice the close. This
/// is that something: fed the reference instant from
/// [`window_close_reference_secs`], it reports each window once.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct WindowCloseClock {
    next_open: [Option<u32>; WINDOW_COUNT],
}

impl WindowCloseClock {
    /// A clock that has seen nothing; its first reading only sets it.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            next_open: [None; WINDOW_COUNT],
        }
    }

    /// Reports the OLDEST window of `cadence` that closed at or before
    /// `reference_secs` and has not been reported yet, and moves past it. One
    /// window per call: when several closed at once (a frame whose ticks
    /// crossed two seconds, or a caller that could not queue a board for a
    /// while), each later call reports the next, so no window with trades in
    /// it is dropped for arriving together with another. Only when more than
    /// [`WINDOW_CLOSE_CATCH_UP_WINDOWS`] are waiting are the oldest skipped,
    /// and counted in `skipped`.
    ///
    /// The FIRST reading of a cadence reports nothing and only records where
    /// the clock stands: a process that boots mid-session would otherwise
    /// rank a window it saw only the tail of. A reading that moved backwards
    /// (the wall clock stepped back) reports nothing. A reading on a later
    /// IST day restarts at that day's newest closed window, counting nothing.
    ///
    /// # Complexity
    /// O(1): a division and a few compares. No allocation.
    pub fn advance(
        &mut self,
        cadence: SnapshotCadence,
        reference_secs: u32,
    ) -> Option<WindowClosed> {
        const SECS_PER_DAY: u32 = 86_400;
        let slot = cadence.slot()?;
        let period = u32::try_from(cadence.interval_secs()).ok()?;
        if period == 0 {
            return None;
        }
        let current_open = reference_secs - reference_secs % period;
        let closed_open = current_open.checked_sub(period)?;
        let entry = self.next_open.get_mut(slot)?;
        let Some(next) = *entry else {
            *entry = Some(current_open);
            return None;
        };
        if closed_open < next {
            return None;
        }
        // `next ..= closed_open` have closed unreported; `behind` of them are
        // older than the newest.
        let behind = (closed_open - next) / period;
        let (oldest, skipped) = if closed_open / SECS_PER_DAY != next / SECS_PER_DAY {
            (closed_open, 0)
        } else if behind >= WINDOW_CLOSE_CATCH_UP_WINDOWS {
            let oldest = closed_open - (WINDOW_CLOSE_CATCH_UP_WINDOWS - 1) * period;
            (oldest, u64::from((oldest - next) / period))
        } else {
            (next, 0)
        };
        *entry = Some(oldest.saturating_add(period));
        Some(WindowClosed {
            window_open_ist_secs: oldest,
            skipped,
        })
    }

    /// Forgets every cadence, as at boot. The daily reset calls it so the new
    /// day starts from its own first reading.
    pub fn clear(&mut self) {
        self.next_open = [None; WINDOW_COUNT];
    }
}

/// Counts projected rows whose candle bar was no longer in the fold when the
/// row was projected, so its candle columns were stored empty.
pub const TOP_VOLUME_SWEEP_CANDLE_MISS_COUNTER: &str = "tv_top_volume_candle_bar_missing_total";

/// Words in a radix sort key, most significant first.
pub const RADIX_WORDS: usize = 3;

/// Byte digits in a radix sort key: eight per word.
const RADIX_DIGITS: usize = RADIX_WORDS * 8;

/// Buckets per digit: one per byte value.
const RADIX_BUCKETS: usize = 256;

/// An LSD radix sort that runs a bounded amount of work per call (audit
/// PR4c, 2026-09-26).
///
/// The sort key is `[u64; RADIX_WORDS]`, compared word by word, most
/// significant first, ascending. The sort visits one byte of the key per pass,
/// least significant first, and scatters stably through a second buffer, so
/// the result is the key order with every tie resolved by the key itself.
/// When the key encodes a TOTAL order over the elements, the result is the
/// same sequence `sort_unstable_by` produces with that order (pinned by the
/// property tests below and by `board_radix_key`'s own test).
///
/// # Complexity
///
/// **O(n × live digits)** in total, where a "live" digit is a key byte that
/// is not the same in every element. A first scan finds them with one OR and
/// one AND per word, so a byte that never varies (for example the high bytes
/// of a small security id, or a segment every row shares) costs nothing
/// after that scan. At most [`RADIX_DIGITS`] passes, each of two walks, so
/// the sort is O(n) with a constant bounded by the key width, where the merge
/// sort it replaced was O(n log n). Each call does about `budget` element
/// visits, plus at most one 256-entry prefix sum.
///
/// Zero allocation once sized: both buffers keep their capacity and the
/// counters are a fixed array.
#[derive(Debug)]
pub struct SliceRadixSort<T: Copy> {
    aux: Vec<T>,
    counts: [usize; RADIX_BUCKETS],
    or_mask: [u64; RADIX_WORDS],
    and_mask: [u64; RADIX_WORDS],
    phase: RadixPhase,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RadixPhase {
    /// Copies the data into the second buffer and records which bytes vary.
    Scan {
        next: usize,
    },
    /// Counts the elements per byte value of one digit.
    Count {
        digit: usize,
        next: usize,
    },
    /// Scatters the elements, in order, into their bucket positions.
    Scatter {
        digit: usize,
        next: usize,
    },
    Done,
}

/// The byte of `key` at `digit`, digit 0 being the least significant byte of
/// the least significant word.
#[inline]
fn radix_byte(key: &[u64; RADIX_WORDS], digit: usize) -> usize {
    let word = RADIX_WORDS.saturating_sub(1).saturating_sub(digit / 8);
    let shift = (digit % 8) * 8;
    key.get(word).map_or(0, |w| ((w >> shift) & 0xFF) as usize)
}

impl<T: Copy> SliceRadixSort<T> {
    /// A sorter whose second buffer holds `capacity` elements without growing.
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            aux: Vec::with_capacity(capacity),
            counts: [0; RADIX_BUCKETS],
            or_mask: [0; RADIX_WORDS],
            and_mask: [u64::MAX; RADIX_WORDS],
            phase: RadixPhase::Done,
        }
    }

    /// Starts a new sort. The data passed to the following `step` calls must
    /// not change until the sort finishes.
    pub fn reset(&mut self) {
        self.aux.clear();
        self.or_mask = [0; RADIX_WORDS];
        self.and_mask = [u64::MAX; RADIX_WORDS];
        self.phase = RadixPhase::Scan { next: 0 };
    }

    /// The first digit at or after `from` whose byte is not the same in every
    /// element. O(`RADIX_DIGITS`).
    fn next_live_digit(&self, from: usize) -> Option<usize> {
        (from..RADIX_DIGITS).find(|&digit| {
            let word = RADIX_WORDS.saturating_sub(1).saturating_sub(digit / 8);
            let shift = (digit % 8) * 8;
            let varies = match (self.or_mask.get(word), self.and_mask.get(word)) {
                (Some(or), Some(and)) => or ^ and,
                _ => 0,
            };
            (varies >> shift) & 0xFF != 0
        })
    }

    /// The phase that follows a finished pass over the digits before `from`.
    fn begin_digit_from(&mut self, from: usize) -> RadixPhase {
        match self.next_live_digit(from) {
            Some(digit) => {
                self.counts = [0; RADIX_BUCKETS];
                RadixPhase::Count { digit, next: 0 }
            }
            None => RadixPhase::Done,
        }
    }

    /// Advances the sort by about `budget` element visits. Returns `true` when
    /// `data` is fully sorted by `key`.
    pub fn step<K>(&mut self, data: &mut Vec<T>, budget: usize, key: K) -> bool
    where
        K: Fn(&T) -> [u64; RADIX_WORDS],
    {
        let len = data.len();
        let mut work = 0usize;
        loop {
            match self.phase {
                RadixPhase::Done => return true,
                RadixPhase::Scan { next } => {
                    if next >= len {
                        self.phase = if len <= 1 {
                            RadixPhase::Done
                        } else {
                            self.begin_digit_from(0)
                        };
                        continue;
                    }
                    let end = next
                        .saturating_add(budget.saturating_sub(work).max(1))
                        .min(len);
                    if let Some(chunk) = data.get(next..end) {
                        for element in chunk {
                            let words = key(element);
                            for ((or, and), word) in self
                                .or_mask
                                .iter_mut()
                                .zip(self.and_mask.iter_mut())
                                .zip(words)
                            {
                                *or |= word;
                                *and &= word;
                            }
                            self.aux.push(*element);
                        }
                    }
                    work = work.saturating_add(end - next);
                    self.phase = RadixPhase::Scan { next: end };
                }
                RadixPhase::Count { digit, next } => {
                    if next >= len {
                        // Exclusive prefix sum: each bucket's first position.
                        let mut position = 0usize;
                        for count in &mut self.counts {
                            let this = *count;
                            *count = position;
                            position = position.saturating_add(this);
                        }
                        work = work.saturating_add(RADIX_BUCKETS);
                        self.phase = RadixPhase::Scatter { digit, next: 0 };
                        continue;
                    }
                    let end = next
                        .saturating_add(budget.saturating_sub(work).max(1))
                        .min(len);
                    if let Some(chunk) = data.get(next..end) {
                        for element in chunk {
                            if let Some(count) =
                                self.counts.get_mut(radix_byte(&key(element), digit))
                            {
                                *count = count.saturating_add(1);
                            }
                        }
                    }
                    work = work.saturating_add(end - next);
                    self.phase = RadixPhase::Count { digit, next: end };
                }
                RadixPhase::Scatter { digit, next } => {
                    if next >= len {
                        // The pass is complete in `aux`; the next pass reads it.
                        std::mem::swap(data, &mut self.aux);
                        self.phase = self.begin_digit_from(digit.saturating_add(1));
                        continue;
                    }
                    let end = next
                        .saturating_add(budget.saturating_sub(work).max(1))
                        .min(len);
                    if let Some(chunk) = data.get(next..end) {
                        for element in chunk {
                            let bucket = radix_byte(&key(element), digit);
                            if let Some(slot) = self.counts.get_mut(bucket) {
                                if let Some(target) = self.aux.get_mut(*slot) {
                                    *target = *element;
                                }
                                *slot = slot.saturating_add(1);
                            }
                        }
                    }
                    work = work.saturating_add(end - next);
                    self.phase = RadixPhase::Scatter { digit, next: end };
                }
            }
            if work >= budget {
                return self.phase == RadixPhase::Done;
            }
        }
    }
}

/// One cadence fire waiting to be ranked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SweepJob {
    /// The cadence that fired.
    pub cadence: SnapshotCadence,
    /// The window's CLOSE instant, IST nanoseconds (audit PR4c-2; it was the
    /// timer fire instant, which sat on the same grid). The snapshot's `ts` —
    /// the window's open — is derived from THIS, never from when a later step
    /// happens to run.
    pub ts_ist_nanos: i64,
    /// A `top_volume` writer exists, so rows are projected and handed off.
    pub wants_rows: bool,
    /// The depth-200 candidates are due from this cadence.
    pub wants_candidates: bool,
}

impl SweepJob {
    /// The open of the window this job ranks, IST epoch seconds: the instant
    /// floored to the cadence grid, less one period — the same window the
    /// projection stamps as the row's `ts`, so the volume and the row can
    /// never name different windows. Saturates at 0 for an instant before the
    /// epoch, which no production job carries.
    #[must_use]
    pub fn window_open_ist_secs(&self) -> u32 {
        let close_secs = u32::try_from(self.ts_ist_nanos.div_euclid(1_000_000_000)).unwrap_or(0);
        let period = u32::try_from(self.cadence.interval_secs()).unwrap_or(0);
        if period == 0 {
            return close_secs;
        }
        (close_secs - close_secs % period).saturating_sub(period)
    }
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
    /// The sliced radix sorter for `rows` (audit PR4c).
    pub sort: SliceRadixSort<RankedContract>,
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
            sort: SliceRadixSort::with_capacity(row_capacity),
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

    /// Counts `windows` closed windows that were never ranked because the
    /// drain only noticed a LATER close (a stalled drain or a feed gap long
    /// enough that neither clock was polled). Same counter as
    /// [`Self::record_deferred`]: both mean "this window has no board".
    /// Returns the running total when this call crossed a power of two.
    pub fn record_windows_skipped(&mut self, windows: u64) -> Option<u64> {
        if windows == 0 {
            return None;
        }
        let before = self.deferred;
        self.deferred = before.saturating_add(windows);
        self.deferred_counter.increment(windows);
        (before.checked_ilog2() != self.deferred.checked_ilog2()).then_some(self.deferred)
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

    use std::cmp::Ordering;

    fn total(a: &(u64, u64), b: &(u64, u64)) -> Ordering {
        b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1))
    }

    /// `total` as a radix key: the first component descending, the second
    /// ascending.
    fn total_key(a: &(u64, u64)) -> [u64; RADIX_WORDS] {
        [!a.0, a.1, 0]
    }

    fn run_to_end(data: &mut Vec<(u64, u64)>, budget: usize) -> usize {
        let mut sorter = SliceRadixSort::with_capacity(data.len());
        sorter.reset();
        let mut steps = 0usize;
        while !sorter.step(data, budget, total_key) {
            steps += 1;
            assert!(steps < 1_000_000, "the sliced sort must terminate");
        }
        steps
    }

    proptest! {
        #[test]
        fn slice_radix_sort_step_matches_sort_unstable_by(
            keys in proptest::collection::vec(any::<u64>(), 0..3_000),
            small in any::<bool>(),
            budget in 1usize..2_000,
        ) {
            // Distinct second component: the order is TOTAL, as `board_order`
            // is. `small` folds the keys into a narrow range so ties on the
            // first component are common and the tie-break is exercised.
            let mut data: Vec<(u64, u64)> = keys
                .iter()
                .enumerate()
                .map(|(i, k)| (if small { k % 50 } else { *k }, (i as u64).wrapping_mul(0x9E37_79B9)))
                .collect();
            let mut expected = data.clone();
            expected.sort_unstable_by(total);
            let _steps = run_to_end(&mut data, budget);
            prop_assert_eq!(data, expected);
        }
    }

    #[test]
    fn slice_radix_sort_step_bounds_the_work_of_one_call() {
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
    fn slice_radix_sort_step_skips_the_digits_that_never_vary() {
        // Keys below 256 and a constant second component: exactly ONE byte
        // varies, so after the scan the sort is one count and one scatter —
        // three walks of the data in all, not one per key byte.
        let mut data: Vec<(u64, u64)> = (0..1_000u64).map(|i| ((i * 7) % 256, 5)).collect();
        let mut sorter = SliceRadixSort::with_capacity(data.len());
        sorter.reset();
        // A budget large enough for the whole job in one call.
        assert!(sorter.step(&mut data, usize::MAX, total_key));
        assert_eq!(sorter.next_live_digit(0), Some(16));
        assert_eq!(sorter.next_live_digit(17), None);
        assert!(
            data.windows(2)
                .all(|w| total(&w[0], &w[1]) != Ordering::Greater)
        );
    }

    #[test]
    fn slice_radix_sort_step_keeps_both_buffers_capacity() {
        let mut data: Vec<(u64, u64)> = Vec::with_capacity(4_096);
        data.extend((0..4_000u64).map(|i| (i % 13, i)));
        let mut sorter = SliceRadixSort::with_capacity(4_096);
        sorter.reset();
        while !sorter.step(&mut data, 300, total_key) {}
        assert!(data.capacity() >= 4_096);
        assert!(sorter.aux.capacity() >= 4_096);
        assert!(
            data.windows(2)
                .all(|w| total(&w[0], &w[1]) != Ordering::Greater)
        );
    }

    #[test]
    fn slice_radix_sort_step_handles_empty_and_single_inputs() {
        let mut sorter = SliceRadixSort::with_capacity(4);
        let mut empty: Vec<(u64, u64)> = Vec::new();
        sorter.reset();
        assert!(sorter.step(&mut empty, 1, total_key));
        let mut one = vec![(3u64, 9u64)];
        sorter.reset();
        assert!(sorter.step(&mut one, 1, total_key) || sorter.step(&mut one, 1, total_key));
        assert_eq!(one, vec![(3, 9)]);
    }

    #[test]
    fn test_radix_byte_reads_least_significant_word_first() {
        let key = [0x0102, 0x0304, 0x0506];
        assert_eq!(radix_byte(&key, 0), 0x06);
        assert_eq!(radix_byte(&key, 1), 0x05);
        assert_eq!(radix_byte(&key, 8), 0x04);
        assert_eq!(radix_byte(&key, 16), 0x02);
        assert_eq!(radix_byte(&key, 23), 0x00);
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
        let sorter: SliceRadixSort<u64> = SliceRadixSort::with_capacity(64);
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

    /// 09:15:00 IST on an arbitrary day, as IST epoch seconds: a whole
    /// minute, so every cadence's grid lines up on it.
    const OPEN_0915: u32 = 1_790_000_000 - 1_790_000_000 % 86_400 + 9 * 3_600 + 15 * 60;

    #[test]
    fn test_window_close_reference_secs_follows_the_candle_clock_inside_its_bounds() {
        // Candles behind the wall but past its grace: the candle clock less
        // the lateness margin.
        assert_eq!(window_close_reference_secs(1_000, 1_001), 999);
        // The watermark at the wall: capped at the wall, never past it.
        assert_eq!(window_close_reference_secs(1_002, 1_001), 1_001);
        // A future-dated tick poisons the watermark: capped at the wall.
        assert_eq!(window_close_reference_secs(9_999, 1_000), 1_000);
        assert_eq!(window_close_reference_secs(u32::MAX, 1_000), 1_000);
        // No tick moved the candle clock: the wall less the grace closes it.
        assert_eq!(
            window_close_reference_secs(0, 1_000),
            1_000 - WINDOW_CLOSE_GRACE_SECS
        );
        // Saturates rather than wrapping at the bottom of the range.
        assert_eq!(window_close_reference_secs(0, 0), 0);
        assert_eq!(
            window_close_reference_secs(u32::MAX, u32::MAX),
            u32::MAX - WINDOW_CLOSE_LATENESS_SECS,
            "no overflow at the top of the range"
        );
    }

    #[test]
    fn test_window_close_clock_new_first_reading_only_sets_the_clock() {
        let mut clock = WindowCloseClock::new();
        assert_eq!(clock, WindowCloseClock::default());
        assert_eq!(
            clock.advance(SnapshotCadence::OneSecond, OPEN_0915 + 30),
            None,
            "a clock that booted mid-window ranks nothing it saw only the tail of"
        );
        assert_eq!(
            clock.advance(SnapshotCadence::OneSecond, OPEN_0915 + 30),
            None,
            "the same second again closes nothing"
        );
        assert_eq!(
            clock.advance(SnapshotCadence::OneSecond, OPEN_0915 + 31),
            Some(WindowClosed {
                window_open_ist_secs: OPEN_0915 + 30,
                skipped: 0,
            })
        );
    }

    /// Hot-path review of audit PR4c-2: a far-future watermark must never
    /// close a window whose end is after the wall clock, on any cadence, at
    /// any wall second of a minute.
    #[test]
    fn test_far_future_watermark_never_closes_a_window_ending_after_the_wall() {
        for cadence in SnapshotCadence::ALL {
            let period = u32::try_from(cadence.interval_secs()).expect("period fits");
            let mut clock = WindowCloseClock::new();
            let _ = clock.advance(cadence, OPEN_0915);
            for offset in 1..=180 {
                let wall = OPEN_0915 + offset;
                let reference = window_close_reference_secs(u32::MAX, wall);
                if let Some(closed) = clock.advance(cadence, reference) {
                    assert!(
                        closed.window_open_ist_secs + period <= wall,
                        "{} window opening at {} ends after the wall {wall}",
                        cadence.as_str(),
                        closed.window_open_ist_secs
                    );
                }
            }
        }
    }

    #[test]
    fn test_window_close_clock_advance_reports_each_window_once_per_cadence() {
        let mut clock = WindowCloseClock::new();
        for cadence in SnapshotCadence::ALL {
            assert_eq!(clock.advance(cadence, OPEN_0915), None);
        }
        // Three seconds later: the 1s and 3s windows opened at 09:15:00 have
        // closed; the 5s and 1m windows have not. Three 1s windows closed at
        // once: each is reported, oldest first, one per call.
        let t = OPEN_0915 + 3;
        for open in OPEN_0915..t {
            assert_eq!(
                clock.advance(SnapshotCadence::OneSecond, t),
                Some(WindowClosed {
                    window_open_ist_secs: open,
                    skipped: 0,
                }),
                "windows that closed together are each ranked, oldest first"
            );
        }
        assert_eq!(clock.advance(SnapshotCadence::OneSecond, t), None);
        assert_eq!(
            clock.advance(SnapshotCadence::ThreeSecond, t),
            Some(WindowClosed {
                window_open_ist_secs: OPEN_0915,
                skipped: 0,
            })
        );
        assert_eq!(clock.advance(SnapshotCadence::FiveSecond, t), None);
        assert_eq!(clock.advance(SnapshotCadence::OneMinute, t), None);
        assert_eq!(
            clock.advance(SnapshotCadence::OneMinute, OPEN_0915 + 60),
            Some(WindowClosed {
                window_open_ist_secs: OPEN_0915,
                skipped: 0,
            })
        );
        // A reading that stepped backwards reports nothing and moves nothing.
        assert_eq!(clock.advance(SnapshotCadence::OneSecond, OPEN_0915), None);
        assert_eq!(
            clock.advance(SnapshotCadence::OneSecond, t + 1),
            Some(WindowClosed {
                window_open_ist_secs: t,
                skipped: 0,
            })
        );
    }

    /// Review of audit PR4c-2: a frame whose ticks move the candle clock
    /// across two seconds closes two windows; both are ranked. Only when the
    /// clock is more than the catch-up bound behind are the oldest skipped.
    #[test]
    fn test_window_close_clock_catches_up_oldest_first_within_the_bound() {
        let mut clock = WindowCloseClock::new();
        let _ = clock.advance(SnapshotCadence::OneSecond, OPEN_0915);
        // Windows 09:15:00 and :01 close together.
        let two = OPEN_0915 + 2;
        assert_eq!(
            clock
                .advance(SnapshotCadence::OneSecond, two)
                .map(|c| c.window_open_ist_secs),
            Some(OPEN_0915)
        );
        assert_eq!(
            clock
                .advance(SnapshotCadence::OneSecond, two)
                .map(|c| c.window_open_ist_secs),
            Some(OPEN_0915 + 1)
        );
        assert_eq!(clock.advance(SnapshotCadence::OneSecond, two), None);
        // Ten seconds of no reading: :02 .. :11 closed. Only the newest
        // WINDOW_CLOSE_CATCH_UP_WINDOWS are ranked; the rest are counted.
        let later = OPEN_0915 + 12;
        let first = clock
            .advance(SnapshotCadence::OneSecond, later)
            .expect("windows closed");
        let kept = WINDOW_CLOSE_CATCH_UP_WINDOWS;
        assert_eq!(first.window_open_ist_secs, later - kept);
        assert_eq!(first.skipped, u64::from(10 - kept));
        for offset in (1..kept).rev() {
            assert_eq!(
                clock.advance(SnapshotCadence::OneSecond, later),
                Some(WindowClosed {
                    window_open_ist_secs: later - offset,
                    skipped: 0,
                })
            );
        }
        assert_eq!(clock.advance(SnapshotCadence::OneSecond, later), None);
    }

    #[test]
    fn test_window_close_clock_advance_does_not_count_windows_across_a_day() {
        let mut clock = WindowCloseClock::new();
        let _ = clock.advance(SnapshotCadence::OneMinute, OPEN_0915);
        let next_day = OPEN_0915 + 86_400 + 120;
        let closed = clock
            .advance(SnapshotCadence::OneMinute, next_day)
            .expect("a later window closed");
        assert_eq!(closed.window_open_ist_secs, next_day - 60);
        assert_eq!(closed.skipped, 0, "the night is not a skipped window");
    }

    #[test]
    fn test_window_close_clock_clear_forgets_every_cadence() {
        let mut clock = WindowCloseClock::new();
        let _ = clock.advance(SnapshotCadence::OneSecond, OPEN_0915);
        clock.clear();
        assert_eq!(clock, WindowCloseClock::new());
        assert_eq!(
            clock.advance(SnapshotCadence::OneSecond, OPEN_0915 + 10),
            None,
            "after a clear the next reading is a first reading again"
        );
    }

    #[test]
    fn test_window_open_ist_secs_is_the_window_before_the_close_instant() {
        let close = |cadence, secs: u32| SweepJob {
            cadence,
            ts_ist_nanos: i64::from(secs) * 1_000_000_000,
            wants_rows: true,
            wants_candidates: false,
        };
        assert_eq!(
            close(SnapshotCadence::OneSecond, OPEN_0915 + 1).window_open_ist_secs(),
            OPEN_0915
        );
        assert_eq!(
            close(SnapshotCadence::OneMinute, OPEN_0915 + 60).window_open_ist_secs(),
            OPEN_0915
        );
        // An instant inside a window names the window before the one it is in.
        assert_eq!(
            close(SnapshotCadence::FiveSecond, OPEN_0915 + 7).window_open_ist_secs(),
            OPEN_0915
        );
        let before_epoch = SweepJob {
            ts_ist_nanos: -5,
            ..close(SnapshotCadence::OneSecond, 0)
        };
        assert_eq!(before_epoch.window_open_ist_secs(), 0, "saturates at 0");
    }

    #[test]
    fn test_record_windows_skipped_shares_the_deferred_count_and_reports_on_powers_of_two() {
        let mut sweep = TopVolumeSweep::with_capacity(4, 4);
        assert_eq!(sweep.record_windows_skipped(0), None, "nothing skipped");
        assert_eq!(sweep.record_windows_skipped(1), Some(1));
        assert_eq!(sweep.record_deferred(), 2, "one counter for both causes");
        assert_eq!(sweep.record_windows_skipped(1), None, "3 is inside [2, 4)");
        assert_eq!(sweep.record_windows_skipped(10), Some(13));
        assert_eq!(sweep.record_windows_skipped(u64::MAX), Some(u64::MAX));
    }
}
