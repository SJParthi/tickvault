//! Detects a WAL record written more than one reorder slack below a record
//! already written by the same writer (PR W review round 7, 2026-10-21).
//!
//! Every replay and prune decision judges a segment from its FIRST record and
//! assumes no record in it sits more than `REPLAY_REORDER_SLACK_SEQ` (one
//! second of sequence) below a record written before it. Sixteen socket read
//! tasks mint sequences from one counter and reach the writer in any order, so
//! the assumption holds only while no task stalls for a second between minting
//! a frame and handing it to the WAL. Until now nothing checked it.
//!
//! A record that breaks it is counted on `tv_wal_reorder_slack_exceeded_total`
//! (local `/metrics` only, no alarm) and its span `[seq, highest written]` is
//! added to the unapplied ranges, so the next boot replays the segment holding
//! it and the prune keeps it: the failure direction is "replay more". One
//! uncoded `warn!` per episode, throttled to powers of two.
//!
//! Kept out of `ws_frame_spill.rs`, which is at its size pin.

use crate::wal_applied_watermark::{REPLAY_REORDER_SLACK_SEQ, applied_watermark};
use tracing::warn;

/// Counter of records written more than one reorder slack below the highest
/// record the same writer had already written.
pub const WAL_REORDER_SLACK_EXCEEDED_COUNTER: &str = "tv_wal_reorder_slack_exceeded_total";

/// Per-writer state: the highest sequence written and the last span handed to
/// the unapplied ranges. Fixed size, no allocation.
#[derive(Debug, Default)]
pub(crate) struct ReorderWatch {
    high: u64,
    covered: Option<(u64, u64)>,
    exceeded: u64,
}

impl ReorderWatch {
    /// Seeds the counter so the series exists before its first, rare, episode.
    pub(crate) fn new() -> Self {
        metrics::counter!(WAL_REORDER_SLACK_EXCEEDED_COUNTER).increment(0);
        Self::default()
    }

    /// Observes one written record. O(1): one compare and one max per record;
    /// only a violation takes the ranges lock (and writes the sidecar when a
    /// new span is needed). Returns whether `seq` broke the slack.
    pub(crate) fn observe(&mut self, seq: u64) -> bool {
        let high = self.high;
        self.high = high.max(seq);
        if seq == 0 || seq.saturating_add(REPLAY_REORDER_SLACK_SEQ) >= high {
            return false;
        }
        self.exceeded = self.exceeded.saturating_add(1);
        metrics::counter!(WAL_REORDER_SLACK_EXCEEDED_COUNTER).increment(1);
        if self.exceeded.is_power_of_two() {
            warn!(
                source = "wal_reorder_slack_exceeded",
                seq,
                highest_written = high,
                exceeded = self.exceeded,
                "WAL record written more than one second of sequence below a record \
                 already written; its span is marked unapplied so the next boot replays it"
            );
        }
        if self.covered.is_none_or(|(lo, hi)| seq < lo || high > hi) {
            let lo = self.covered.map_or(seq, |(lo, _)| lo.min(seq));
            let _changed = applied_watermark().add_unapplied_range(lo, high);
            self.covered = Some((lo, high));
        }
        true
    }

    /// Records counted as breaking the slack so far.
    #[cfg(test)]
    pub(crate) fn exceeded(&self) -> u64 {
        self.exceeded
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: u64 = REPLAY_REORDER_SLACK_SEQ;

    #[test]
    fn observe_counts_only_records_more_than_one_slack_below_the_highest() {
        let mut w = ReorderWatch::new();
        let base = 1_800_000_000_000_000_000;
        assert!(!w.observe(base));
        assert!(!w.observe(base + 3 * S));
        // Exactly one slack below: allowed.
        assert!(!w.observe(base + 2 * S));
        // One past the slack: a violation.
        assert!(w.observe(base + 2 * S - 1));
        assert_eq!(w.exceeded(), 1);
        // The highest never falls.
        assert!(w.observe(base + S));
        assert_eq!(w.exceeded(), 2);
    }

    #[test]
    fn observe_marks_the_span_unapplied_and_widens_it_only_when_needed() {
        let mut w = ReorderWatch::new();
        let base = 1_700_000_000_000_000_000;
        w.observe(base + 10 * S);
        assert!(w.observe(base));
        assert_eq!(w.covered, Some((base, base + 10 * S)));
        // Inside the covered span: no new range.
        assert!(w.observe(base + S));
        assert_eq!(w.covered, Some((base, base + 10 * S)));
        // A higher record widens the top on the next violation.
        w.observe(base + 20 * S);
        assert!(w.observe(base + 2 * S));
        assert_eq!(w.covered, Some((base, base + 20 * S)));
        assert!(
            applied_watermark()
                .snapshot()
                .range_has_unapplied(base + 3 * S, base + 4 * S),
            "the span is protected"
        );
    }

    #[test]
    fn observe_ignores_a_zero_sequence() {
        let mut w = ReorderWatch::new();
        w.observe(5 * S);
        assert!(!w.observe(0));
        assert_eq!(w.exceeded(), 0);
    }
}
