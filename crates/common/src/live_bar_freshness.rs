//! Live-bar decision-freshness primitive (the §38.8 decision-freshness gate).
//!
//! Backfill/sweep-repaired REST 1m rows are RECORD-COMPLETENESS data — never
//! trading-decision inputs. Any future strategy consumer MUST fail closed on
//! stale bars: a bar older than the configured freshness threshold means NO
//! trade for that minute. This module is the pure, O(1), zero-alloc primitive
//! that gate consults.

/// Returns `true` when the bar is fresh enough for a trading decision.
///
/// `bar_ts_secs` is the bar's retrieval/close timestamp (epoch seconds),
/// `now_secs` the current clock, `threshold_secs` the configured freshness
/// bound. Boundary is inclusive (`age == threshold` is fresh). A bar stamped
/// in the future (clock skew) counts fresh via `saturating_sub` (age = 0) —
/// clock-skew policing belongs to BOOT-03, not this gate.
#[inline]
#[must_use]
pub fn live_bar_is_fresh(bar_ts_secs: u64, now_secs: u64, threshold_secs: u64) -> bool {
    now_secs.saturating_sub(bar_ts_secs) <= threshold_secs
}

#[cfg(test)]
mod tests {
    use super::live_bar_is_fresh;
    use proptest::prelude::*;

    #[test]
    fn zero_age_is_fresh() {
        assert!(live_bar_is_fresh(1_000, 1_000, 60));
    }

    #[test]
    fn beyond_threshold_is_stale() {
        assert!(!live_bar_is_fresh(1_000, 1_061, 60));
    }

    #[test]
    fn exactly_at_threshold_is_fresh() {
        assert!(live_bar_is_fresh(1_000, 1_060, 60));
    }

    #[test]
    fn zero_threshold_only_same_second_is_fresh() {
        assert!(live_bar_is_fresh(1_000, 1_000, 0));
        assert!(!live_bar_is_fresh(1_000, 1_001, 0));
    }

    #[test]
    fn future_bar_counts_fresh_via_saturating_sub() {
        assert!(live_bar_is_fresh(2_000, 1_000, 0));
    }

    #[test]
    fn u64_max_inputs_never_panic() {
        assert!(live_bar_is_fresh(u64::MAX, u64::MAX, 0));
        assert!(!live_bar_is_fresh(0, u64::MAX, 60));
        assert!(live_bar_is_fresh(0, u64::MAX, u64::MAX));
    }

    proptest! {
        #[test]
        fn freshness_matches_saturating_age(bar in any::<u64>(), now in any::<u64>(), thr in any::<u64>()) {
            let expected = now.saturating_sub(bar) <= thr;
            prop_assert_eq!(live_bar_is_fresh(bar, now, thr), expected);
        }
    }
}
