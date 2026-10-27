//! Confirming parked live acks once they are durable (2026-10-27, plan item
//! PR W4).
//!
//! # What this closes
//!
//! While a QuestDB probe is unclean, every live ack PARKS instead of moving
//! the applied watermark, and the episode's stretch of sequences is written to
//! the unapplied ranges twice: at the start (the detection window since the
//! last clean probe) and at the end (the parked acks). Until now nothing ever
//! took those ranges out again except a later replay reading the segments, so
//! on a box whose backlog does not drain a lag-latched session pinned its
//! whole stretch of WAL for good (PR W review round 4, the "Follow-up").
//!
//! # The rule (the one `settling/` uses for replays)
//!
//! An episode's ranges are confirmed (cut back out of the unapplied ranges,
//! and the watermark raised to the highest parked ack) only when ALL hold:
//!
//! * [`WATERMARK_DURABILITY_LAG_SECS`] have passed since the clean probe that
//!   ended the episode (every parked ack is at or before that probe);
//! * at least [`WAL_REPLAY_SETTLE_CLEAN_PROBES`] clean probes were reported
//!   AFTER that probe;
//! * no suspicion episode began since it, and the sink is not suspect now.
//!
//! Three narrowings beyond the plan text, each "keep more":
//!
//! * **Lag only.** Every unclean probe of the episode must have been a
//!   COMPLETE view with no suspended table, unclean only because a table was
//!   latched for apply lag ([`ProbeVerdict::LagOnly`]). A suspended table may
//!   have its transactions skipped by a `RESUME WAL FROM TXN`, and a blind or
//!   partial probe says nothing, so those episodes keep their ranges until a
//!   replay reads them, exactly as before.
//! * **Own coverage only.** The cut removes only the sequences the episode
//!   itself added. If any OTHER range (a reorder violation, an overflow
//!   bound, the backlog guard, a settle recovery, a boot range) touched the
//!   episode's spans at any point, nothing is cut: the set does not record
//!   which source a sequence came from, so a cut could remove theirs.
//! * **One episode at a time.** A new episode beginning before the previous
//!   one is confirmed drops the previous one (kept, counted).
//!
//! # Cost
//!
//! Cold path only: the WAL-suspension watcher's probe report (60 s) and the
//! range mutations, all under the existing ranges lock. Fixed-size `Copy`
//! state (two spans, two flags, one mark), no allocation. The ack path is
//! unchanged: one lock of the existing ack gate per BATCH.
//!
//! # Honest limits
//!
//! * The state is in memory: a crash before confirmation keeps the ranges
//!   (replay more), never fewer.
//! * A confirmed lag episode is trusted as much as an ordinary ack: rows that
//!   QuestDB committed to its own WAL and is applying. A table that suspends
//!   LATER, after the confirmation, is caught only for the detection window
//!   of that later episode, as for every other ack.
//! * The 60 s lag is the watermark's own assumption about kernel writeback;
//!   it is not an fsync of QuestDB's files.

use crate::wal_applied_watermark::WATERMARK_DURABILITY_LAG_SECS;
use crate::wal_replay_settle::WAL_REPLAY_SETTLE_CLEAN_PROBES;

/// Counter: lag-only suspicion episodes whose parked acks were confirmed and
/// whose ranges were cut back out. Local `/metrics` only: no CloudWatch
/// filter, no EMF name, no alarm. Registered at 0 at boot.
pub const WAL_PARKED_ACKS_CONFIRMED_COUNTER: &str = "tv_wal_parked_acks_confirmed_total";

/// Counter: suspicion episodes whose ranges were KEPT (left for a replay to
/// read), labelled `reason` (see [`KEPT_REASONS`]). Local `/metrics` only, as
/// above. Registered at 0 for every reason at boot.
pub const WAL_PARKED_ACKS_KEPT_COUNTER: &str = "tv_wal_parked_acks_kept_total";

/// Every `reason` of [`WAL_PARKED_ACKS_KEPT_COUNTER`].
pub const KEPT_REASONS: [&str; 3] = [KEPT_UNTRUSTED_PROBE, KEPT_FOREIGN_RANGE, KEPT_NEW_EPISODE];

/// An unclean probe of the episode was not lag-only.
pub const KEPT_UNTRUSTED_PROBE: &str = "untrusted_probe";
/// Another range touched the episode's spans.
pub const KEPT_FOREIGN_RANGE: &str = "foreign_range";
/// A new episode began before the confirmation was due.
pub const KEPT_NEW_EPISODE: &str = "new_episode";

/// Registers both counters at 0 (every `reason` of the kept one), so a box
/// that never confirms shows 0 rather than an absent series. Once, at boot.
pub fn register_counters() {
    metrics::counter!(WAL_PARKED_ACKS_CONFIRMED_COUNTER).increment(0);
    for reason in KEPT_REASONS {
        metrics::counter!(WAL_PARKED_ACKS_KEPT_COUNTER, "reason" => reason).increment(0);
    }
}

/// What one QuestDB probe saw.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeVerdict {
    /// Every table visible, none suspended, none latched for apply lag.
    Clean,
    /// A complete view with no suspended table; unclean only because a table
    /// is latched (or not yet strictly released) for apply lag.
    LagOnly,
    /// A suspended table, a partial view, or a probe that could not see.
    Untrusted,
}

impl ProbeVerdict {
    /// The verdict from the watcher's three readings. O(1).
    #[must_use]
    pub const fn from_readings(complete: bool, suspended: usize, lag_untrusted: usize) -> Self {
        if !complete || suspended > 0 {
            Self::Untrusted
        } else if lag_untrusted > 0 {
            Self::LagOnly
        } else {
            Self::Clean
        }
    }

    /// `true` for [`Self::Clean`].
    #[must_use]
    pub const fn is_clean(self) -> bool {
        matches!(self, Self::Clean)
    }
}

/// Which acked sink a range belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sink {
    /// The `ticks` writer.
    Ticks,
    /// The `market_depth` writer.
    Depth,
}

/// Where the clean probe that ended the episode stood.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExitMark {
    /// Monotonic nanos of the exit probe.
    pub at_mono: u64,
    /// The clean-probe count INCLUDING the exit probe.
    pub clean_probes: u64,
    /// The episode count at the exit (this episode's own number).
    pub episodes: u64,
    /// Highest parked ack per sink (`0` = none parked).
    pub parked_ticks: u64,
    pub parked_depth: u64,
}

/// The decision at one clean probe after the exit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Nothing is pending.
    Idle,
    /// Pending, not yet due.
    Wait,
    /// Cut these spans out and raise the watermarks to the parked acks.
    Confirm {
        ticks: Option<(u64, u64)>,
        depth: Option<(u64, u64)>,
        parked_ticks: u64,
        parked_depth: u64,
    },
    /// Keep the ranges; the reason is one of [`KEPT_REASONS`].
    Keep(&'static str),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Idle,
    Open,
    Exited(ExitMark),
}

/// One episode's confirmation state. `Copy`, fixed size, never allocates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParkedConfirm {
    phase: Phase,
    ticks: Option<(u64, u64)>,
    depth: Option<(u64, u64)>,
    foreign: bool,
    untrusted: bool,
}

impl Default for ParkedConfirm {
    fn default() -> Self {
        Self::new()
    }
}

/// The union of the two own spans as at most two disjoint, non-adjacent
/// spans (one when they overlap or touch). O(1).
#[must_use]
pub(crate) fn own_union(
    (ticks, depth): (Option<(u64, u64)>, Option<(u64, u64)>),
) -> [Option<(u64, u64)>; 2] {
    match (ticks, depth) {
        (Some((a, b)), Some((c, d))) if a <= d.saturating_add(1) && c <= b.saturating_add(1) => {
            [Some((a.min(c), b.max(d))), None]
        }
        (t, d) => [t, d],
    }
}

/// `true` when `[a, b]` and `[c, d]` share a sequence.
const fn overlaps(a: u64, b: u64, c: u64, d: u64) -> bool {
    a <= d && c <= b
}

impl ParkedConfirm {
    /// Nothing pending.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            phase: Phase::Idle,
            ticks: None,
            depth: None,
            foreign: false,
            untrusted: false,
        }
    }

    /// `true` while an episode is open or waiting for confirmation.
    #[must_use]
    pub const fn is_tracking(&self) -> bool {
        !matches!(self.phase, Phase::Idle)
    }

    /// The episode's own spans `(ticks, depth)`.
    #[must_use]
    pub const fn spans(&self) -> (Option<(u64, u64)>, Option<(u64, u64)>) {
        (self.ticks, self.depth)
    }

    /// An episode begins with `verdict` (never [`ProbeVerdict::Clean`]).
    /// `preexisting` = another range already touches the start ranges.
    /// Returns [`KEPT_NEW_EPISODE`] when a previous episode was still
    /// pending: it is dropped and its ranges stay. O(1).
    pub fn begin(&mut self, verdict: ProbeVerdict, preexisting: bool) -> Option<&'static str> {
        let dropped = self.is_tracking().then_some(KEPT_NEW_EPISODE);
        *self = Self {
            phase: Phase::Open,
            ticks: None,
            depth: None,
            foreign: preexisting,
            untrusted: !matches!(verdict, ProbeVerdict::LagOnly),
        };
        dropped
    }

    /// A further unclean probe inside the open episode. O(1).
    pub fn observe_unclean(&mut self, verdict: ProbeVerdict) {
        if matches!(self.phase, Phase::Open) && !matches!(verdict, ProbeVerdict::LagOnly) {
            self.untrusted = true;
        }
    }

    /// The episode itself added `[lo, hi]` for `sink`. O(1).
    pub fn own(&mut self, sink: Sink, lo: u64, hi: u64) {
        if !self.is_tracking() || lo > hi {
            return;
        }
        let slot = match sink {
            Sink::Ticks => &mut self.ticks,
            Sink::Depth => &mut self.depth,
        };
        *slot = Some(match *slot {
            Some((a, b)) => (a.min(lo), b.max(hi)),
            None => (lo, hi),
        });
    }

    /// `true` when `[lo, hi]` touches either own span. O(1).
    #[must_use]
    pub fn touches(&self, lo: u64, hi: u64) -> bool {
        lo <= hi
            && [self.ticks, self.depth]
                .iter()
                .flatten()
                .any(|&(a, b)| overlaps(a, b, lo, hi))
    }

    /// Any OTHER range add of `[lo, hi]`, whether or not it changed the set.
    /// O(1).
    pub fn note_foreign_add(&mut self, lo: u64, hi: u64) {
        if self.is_tracking() && self.touches(lo, hi) {
            self.foreign = true;
        }
    }

    /// Another range was already present over a part of `[lo, hi]` that the
    /// episode is about to add and does not already own. O(1).
    pub fn note_foreign_present(&mut self) {
        if self.is_tracking() {
            self.foreign = true;
        }
    }

    /// The clean probe that ended the episode. O(1).
    pub fn exit(&mut self, mark: ExitMark) {
        if matches!(self.phase, Phase::Open) {
            self.phase = Phase::Exited(mark);
        }
    }

    /// The decision at a clean probe: `now_mono`, the clean-probe count
    /// including it, the episode count, and whether the sink is suspect.
    /// A `Confirm` or `Keep` returns the state to idle. O(1).
    pub fn decide(
        &mut self,
        now_mono: u64,
        clean_probes: u64,
        episodes: u64,
        suspect: bool,
    ) -> Decision {
        let Phase::Exited(mark) = self.phase else {
            return if self.is_tracking() {
                Decision::Wait
            } else {
                Decision::Idle
            };
        };
        let verdict = if self.untrusted {
            Decision::Keep(KEPT_UNTRUSTED_PROBE)
        } else if self.foreign {
            Decision::Keep(KEPT_FOREIGN_RANGE)
        } else if episodes != mark.episodes || suspect {
            Decision::Keep(KEPT_NEW_EPISODE)
        } else if now_mono.saturating_sub(mark.at_mono)
            < WATERMARK_DURABILITY_LAG_SECS.saturating_mul(1_000_000_000)
            || clean_probes
                < mark
                    .clean_probes
                    .saturating_add(WAL_REPLAY_SETTLE_CLEAN_PROBES)
        {
            return Decision::Wait;
        } else {
            Decision::Confirm {
                ticks: self.ticks,
                depth: self.depth,
                parked_ticks: mark.parked_ticks,
                parked_depth: mark.parked_depth,
            }
        };
        *self = Self::new();
        verdict
    }
}

#[cfg(test)]
#[path = "wal_parked_confirm_tests.rs"]
mod tests;
