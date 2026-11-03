//! Exact threshold evidence beside the unchanged historical histogram view.
//!
//! One recorded event belongs to one disjoint raw bin. Both strict counts
//! come from the same raw snapshot/window; no separately read total is used.

use super::{STAGE_BUCKETS, bucket_index};

/// The requested strict upper limit; equality is a violating sample.
pub const STRICT_THRESHOLD_NANOS: u64 = 1_000;
/// Persisted interpretation of the disjoint raw histogram classification.
pub const STRICT_MEASUREMENT_VERSION: i64 = 1;
/// The historical first bin is split; every later bin keeps its old range.
pub const RAW_STAGE_BUCKETS: usize = STAGE_BUCKETS + 1;

/// Exact classification counts for one raw snapshot or one observed window.
/// These describe recorded samples, not elapsed-time coverage or freshness.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct StrictSampleCounts {
    /// The sum of all disjoint raw bins in this same window.
    pub samples: u64,
    /// Samples at or above 1,000 ns, including equality.
    pub samples_at_or_above_1000ns: u64,
}

impl StrictSampleCounts {
    pub(crate) fn from_columns(
        version: Option<i64>,
        samples: Option<i64>,
        at_or_above: Option<i64>,
    ) -> Option<Self> {
        if version != Some(STRICT_MEASUREMENT_VERSION) {
            return None;
        }
        let counts = Self {
            samples: u64::try_from(samples?).ok()?,
            samples_at_or_above_1000ns: u64::try_from(at_or_above?).ok()?,
        };
        (counts.samples_at_or_above_1000ns <= counts.samples).then_some(counts)
    }

    /// Whether the nonempty population's nearest-rank p99 is below 1,000 ns.
    /// Invalid or empty evidence is unknown. O(1), no allocation.
    #[must_use]
    pub const fn p99_below_1000ns(self) -> Option<bool> {
        if self.samples == 0 || self.samples_at_or_above_1000ns > self.samples {
            None
        } else {
            Some(self.samples_at_or_above_1000ns <= self.samples / 100)
        }
    }

    pub(crate) fn checked_add(self, other: Self) -> Option<Self> {
        if self.samples_at_or_above_1000ns > self.samples
            || other.samples_at_or_above_1000ns > other.samples
        {
            return None;
        }
        Some(Self {
            samples: self.samples.checked_add(other.samples)?,
            samples_at_or_above_1000ns: self
                .samples_at_or_above_1000ns
                .checked_add(other.samples_at_or_above_1000ns)?,
        })
    }

    /// A strict pair must not be independently saturated into LONG values.
    pub(crate) fn ilp_counts(self) -> Option<(i64, i64)> {
        if self.samples_at_or_above_1000ns > self.samples {
            return None;
        }
        Some((
            i64::try_from(self.samples).ok()?,
            i64::try_from(self.samples_at_or_above_1000ns).ok()?,
        ))
    }
}

pub(super) const fn raw_bucket_index(nanos: u64) -> usize {
    if nanos < STRICT_THRESHOLD_NANOS {
        0
    } else {
        bucket_index(nanos) + 1
    }
}

pub(super) fn legacy_buckets(raw: &[u64; RAW_STAGE_BUCKETS]) -> [u64; STAGE_BUCKETS] {
    let mut legacy = [0; STAGE_BUCKETS];
    legacy[0] = raw[0].saturating_add(raw[1]);
    legacy[1..].copy_from_slice(&raw[2..]);
    legacy
}

fn counts(raw: &[u64; RAW_STAGE_BUCKETS]) -> Option<StrictSampleCounts> {
    let samples = raw.iter().try_fold(0u64, |sum, n| sum.checked_add(*n))?;
    Some(StrictSampleCounts {
        samples,
        samples_at_or_above_1000ns: samples.checked_sub(raw[0])?,
    })
}

pub(super) fn strict_window(
    now: &[u64; RAW_STAGE_BUCKETS],
    previous: &[u64; RAW_STAGE_BUCKETS],
) -> Option<StrictSampleCounts> {
    let mut window = [0; RAW_STAGE_BUCKETS];
    for ((out, current), before) in window.iter_mut().zip(now).zip(previous) {
        // A reset/wrap is not a measured zero. The caller installs the new
        // baseline after this unknown window so later windows can recover.
        *out = current.checked_sub(*before)?;
    }
    counts(&window)
}
