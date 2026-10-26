//! Where each `ticks` row lands against the rows already written (plan item
//! M1, 2026-10-26).
//!
//! `ticks` is `PARTITION BY HOUR` on `ts`, and `ts` is the exchange trade time
//! (`.claude/rules/project/data-integrity.md`). A row older than the newest
//! row already written is an out-of-order insert: QuestDB merges it into the
//! partition that already holds later rows, rewriting that partition from the
//! insertion point. A row in an OLDER hour partition rewrites a partition that
//! is already complete.
//!
//! Measured on the live box (two raw frame segments from `raw-frames/`,
//! 2026-10-26 11:25 and 14:32 IST, 725,460 and 724,056 tick packets): only
//! 25-27% of rows are within one second of their receipt, 39-41% are more
//! than a minute old, and 4.9-5.6% fall into an older hour partition. 83-85%
//! repeat the instrument's previous trade time (a quote update with no new
//! trade). The same day the root disk wrote 116 MiB/s in session against
//! about 1 MB/s of tick rows.
//!
//! This module only COUNTS the shape so each session shows it: one
//! `fetch_max` and one pre-resolved counter add per row, O(1), no allocation,
//! no lock. It changes nothing about what is written. Local `/metrics` only:
//! `tv_ticks_ts_placement_total{placement}`, no alarm, no EMF name.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicI64, Ordering};

use metrics::Counter;

/// Counter name, one series per [`Placement`].
pub const TICKS_TS_PLACEMENT_COUNTER: &str = "tv_ticks_ts_placement_total";

/// Nanoseconds in one `ticks` partition (`PARTITION BY HOUR`).
pub const TICKS_PARTITION_NANOS: i64 = 3_600 * 1_000_000_000;

/// Where a row's `ts` falls against the newest `ts` already written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placement {
    /// At or after the newest row: an append.
    InOrder,
    /// Before the newest row, in the same hour partition.
    LateSameHour,
    /// In an hour partition older than the newest row's.
    OlderPartition,
}

impl Placement {
    /// The label value.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InOrder => "in_order",
            Self::LateSameHour => "late_same_hour",
            Self::OlderPartition => "older_partition",
        }
    }

    const ALL: [Self; 3] = [Self::InOrder, Self::LateSameHour, Self::OlderPartition];

    const fn index(self) -> usize {
        match self {
            Self::InOrder => 0,
            Self::LateSameHour => 1,
            Self::OlderPartition => 2,
        }
    }
}

/// Classifies `ts` against `newest` (the newest `ts` written before it). Pure.
/// O(1).
#[must_use]
pub fn classify(ts_ist_nanos: i64, newest_ist_nanos: i64) -> Placement {
    if ts_ist_nanos >= newest_ist_nanos {
        Placement::InOrder
    } else if ts_ist_nanos.div_euclid(TICKS_PARTITION_NANOS)
        == newest_ist_nanos.div_euclid(TICKS_PARTITION_NANOS)
    {
        Placement::LateSameHour
    } else {
        Placement::OlderPartition
    }
}

/// Newest `ts` noted so far, process-wide (one Dhan tick writer).
static NEWEST_TS_NANOS: AtomicI64 = AtomicI64::new(i64::MIN);

fn handles() -> &'static [Counter; 3] {
    static HANDLES: OnceLock<[Counter; 3]> = OnceLock::new();
    HANDLES.get_or_init(|| {
        Placement::ALL.map(|p| {
            let c = metrics::counter!(TICKS_TS_PLACEMENT_COUNTER, "placement" => p.as_str());
            c.increment(0);
            c
        })
    })
}

/// Registers every series at 0. Called when a tick writer is built, so the
/// series exist before the first row.
pub fn register_baseline() {
    for handle in handles() {
        handle.increment(0);
    }
}

/// Notes one appended `ticks` row and returns its placement. One `fetch_max`
/// and one counter add. O(1), no allocation after [`register_baseline`].
pub fn note_row(ts_ist_nanos: i64) -> Placement {
    let newest = NEWEST_TS_NANOS.fetch_max(ts_ist_nanos, Ordering::Relaxed);
    let placement = classify(ts_ist_nanos, newest);
    handles()[placement.index()].increment(1);
    placement
}

#[cfg(test)]
mod tests {
    use super::*;

    const H: i64 = TICKS_PARTITION_NANOS;

    #[test]
    fn test_classify_in_order_late_same_hour_and_older_partition() {
        let newest = 10 * H + 30 * 60 * 1_000_000_000; // 10:30
        assert_eq!(classify(newest, newest), Placement::InOrder);
        assert_eq!(classify(newest + 1, newest), Placement::InOrder);
        assert_eq!(classify(10 * H, newest), Placement::LateSameHour);
        assert_eq!(classify(10 * H - 1, newest), Placement::OlderPartition);
        assert_eq!(classify(9 * H, newest), Placement::OlderPartition);
        // The first row of a process is always an append.
        assert_eq!(classify(0, i64::MIN), Placement::InOrder);
    }

    #[test]
    fn test_placement_as_str_labels_are_distinct() {
        let labels = Placement::ALL.map(Placement::as_str);
        assert_eq!(labels, ["in_order", "late_same_hour", "older_partition"]);
    }

    /// Regression (2026-10-26, plan item M1): a repeat quote whose trade time
    /// is from an earlier hour is counted as an older-partition insert, the
    /// shape that made the disk write 116 MiB/s for 1 MB/s of rows.
    #[test]
    fn test_regression_note_row_counts_a_stale_trade_time_as_older_partition() {
        register_baseline();
        // Above any real stamp another test in this binary notes (the high-water
        // mark is process-wide), so their rows cannot move it past ours.
        let base = (i64::MAX / 2).div_euclid(H) * H;
        assert_eq!(note_row(base + 5 * 60 * 1_000_000_000), Placement::InOrder);
        assert_eq!(note_row(base + 60 * 1_000_000_000), Placement::LateSameHour);
        assert_eq!(note_row(base - 1), Placement::OlderPartition);
        assert_eq!(
            note_row(base + 6 * 60 * 1_000_000_000),
            Placement::InOrder,
            "a later row is an append again"
        );
    }
}
