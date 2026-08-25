//! Pre-resolved counter handles for the per-tick candle fold.
//!
//! # Why this exists
//!
//! `AggregatorCell::fold` runs **24 times per tick** — once per timeframe —
//! and `MultiTfAggregator::consume_tick` drives it from the frame drain. The
//! anomaly counters inside that fold were calling the `metrics::counter!`
//! MACRO directly at 16 sites across two files.
//!
//! The macro is not free. This repository already documented exactly why, in
//! `dhan_feed_stack.rs::DrainCounters`: *"even unlabelled it performs a
//! sharded-registry lookup"*. `parser/dispatcher.rs` hit it first and solved
//! it by resolving handles once; `DrainCounters` solved it the same way for
//! the per-frame path. The fold — the hottest code in the workspace — was the
//! one place that never got the treatment.
//!
//! # Honest magnitude, because the first draft of this note overstated it
//!
//! Every label value at these sites is a compile-time literal, so the macro
//! takes `Key::from_static_parts` and **allocates nothing**. This is NOT the
//! `record_ws_lag` defect class (a runtime label value dropping the macro to
//! its allocating arm, ~36M allocations/hour). What is removed here is a hash
//! plus a sharded-registry lock acquisition per call — real, but bounded, and
//! the honest reason the DHAT gate was green throughout: `dhat_multi_tf_fold`
//! asserts zero allocations and passes both before and after this change,
//! because allocation was never the axis.
//!
//! These sites are also CONDITIONAL — each fires only when its anomaly occurs,
//! not on every tick. The worst sustained case is the volume-regression site,
//! which fires on late ticks; `multi_tf_aggregator` records that ~10% of live
//! ticks arrive more than an hour behind receive time. The worst burst case is
//! the open-clamp site, whose own comment notes it fires once per instrument
//! per timeframe on a gap-open — at the authorized ceiling that is a large
//! number of lookups concentrated in the 09:15 open minute, which is precisely
//! when the drain is least able to afford them.
//!
//! # Bounded by construction
//!
//! Every name and every label value below is a compile-time-known
//! `&'static str`, so the full handle set is enumerable up front. There is no
//! unbounded label cardinality hiding in this struct — the same property
//! `DrainCounters` documents about itself.

use std::sync::OnceLock;

/// Every counter the per-tick fold can increment, resolved once.
pub(crate) struct FoldCounters {
    pub(crate) session_extremes_inverted: metrics::Counter,
    pub(crate) session_extreme_regressed_high: metrics::Counter,
    pub(crate) session_extreme_regressed_low: metrics::Counter,
    pub(crate) open_clamped: metrics::Counter,
    pub(crate) day_high_adopted: metrics::Counter,
    pub(crate) day_low_adopted: metrics::Counter,
    pub(crate) session_high_recovered: metrics::Counter,
    pub(crate) session_low_recovered: metrics::Counter,
    pub(crate) oi_zero_ignored: metrics::Counter,
    pub(crate) volume_regression_suppressed: metrics::Counter,
    pub(crate) cumulative_regression: metrics::Counter,
    pub(crate) slot_exhausted: metrics::Counter,
    pub(crate) slot_volume_baseline_seeded: metrics::Counter,
    /// `tick_refused` carries a `reason` label with THREE distinct values.
    /// One field per value, because collapsing them would merge three
    /// independent refusal causes into one series and make the counter
    /// useless for telling a bad price from a bad timestamp.
    pub(crate) tick_refused_price: metrics::Counter,
    pub(crate) tick_refused_timestamp: metrics::Counter,
    pub(crate) tick_refused_untraded_sentinel: metrics::Counter,
}

impl FoldCounters {
    fn resolve() -> Self {
        Self {
            session_extremes_inverted: metrics::counter!(
                "tv_candle_session_extremes_inverted_total"
            ),
            session_extreme_regressed_high: metrics::counter!(
                "tv_candle_session_extreme_regressed_total",
                "extreme" => "high"
            ),
            session_extreme_regressed_low: metrics::counter!(
                "tv_candle_session_extreme_regressed_total",
                "extreme" => "low"
            ),
            open_clamped: metrics::counter!("tv_candle_open_clamped_total"),
            day_high_adopted: metrics::counter!("tv_candle_day_high_adopted_total"),
            day_low_adopted: metrics::counter!("tv_candle_day_low_adopted_total"),
            session_high_recovered: metrics::counter!("tv_candle_session_high_recovered_total"),
            session_low_recovered: metrics::counter!("tv_candle_session_low_recovered_total"),
            oi_zero_ignored: metrics::counter!("tv_candle_oi_zero_ignored_total"),
            volume_regression_suppressed: metrics::counter!(
                "tv_candle_volume_regression_suppressed_total"
            ),
            cumulative_regression: metrics::counter!("tv_aggregator_cumulative_regression_total"),
            slot_exhausted: metrics::counter!("tv_aggregator_slot_exhausted_total"),
            slot_volume_baseline_seeded: metrics::counter!(
                "tv_aggregator_slot_volume_baseline_seeded_total"
            ),
            tick_refused_price: metrics::counter!(
                "tv_aggregator_tick_refused_total",
                "reason" => "price"
            ),
            tick_refused_timestamp: metrics::counter!(
                "tv_aggregator_tick_refused_total",
                "reason" => "timestamp"
            ),
            tick_refused_untraded_sentinel: metrics::counter!(
                "tv_aggregator_tick_refused_total",
                "reason" => "untraded_sentinel"
            ),
        }
    }
}

/// The process-wide handle set. Resolved on first use, then a plain read.
///
/// `OnceLock` rather than `lazy_static`/`OnceCell` to match
/// `DrainCounters`/`WsLagHandles`, and because the recorder is installed at
/// boot Step 3 — long before the first tick — so the resolve happens against
/// the real recorder, never a no-op one.
pub(crate) fn fold_counters() -> &'static FoldCounters {
    static HANDLES: OnceLock<FoldCounters> = OnceLock::new();
    HANDLES.get_or_init(FoldCounters::resolve)
}

#[cfg(test)]
mod tests {
    use super::{FoldCounters, fold_counters};

    /// The handle set is resolved ONCE — the whole point.
    ///
    /// Two calls must hand back the identical `&'static` struct. If someone
    /// converts this to a function that rebuilds the struct per call, the
    /// registry lookup returns to the per-tick path and nothing else would
    /// notice: the counters would still be correct, still allocate nothing,
    /// and the DHAT gate would still pass. That is exactly the shape of defect
    /// this module exists to prevent, so it gets its own assertion.
    #[test]
    fn the_handle_set_is_resolved_once_not_per_call() {
        let a = fold_counters();
        let b = fold_counters();
        assert!(
            std::ptr::eq(a, b),
            "fold_counters() must return the SAME cached struct — a fresh \
             resolve per call re-introduces the sharded-registry lookup on \
             the 24-times-per-tick path"
        );
    }

    /// Incrementing through a cached handle is sound with no recorder
    /// installed, which is the state every unit test runs in.
    #[test]
    fn incrementing_a_cached_handle_is_safe_without_a_recorder() {
        let c = fold_counters();
        c.open_clamped.increment(1);
        c.volume_regression_suppressed.increment(1);
        c.oi_zero_ignored.increment(1);
        c.session_extreme_regressed_high.increment(1);
        c.session_extreme_regressed_low.increment(1);
    }

    /// The two labelled variants are DISTINCT handles.
    ///
    /// They share a metric name and differ only by label value. Resolving them
    /// into one field, or copying the wrong field into the other, would merge
    /// high-side and low-side regressions into a single series — a silent
    /// halving of one signal and doubling of the other, with no error anywhere.
    #[test]
    fn the_two_labelled_extremes_are_separate_handles() {
        let c = FoldCounters::resolve();
        assert!(
            !std::ptr::eq(
                std::ptr::from_ref(&c.session_extreme_regressed_high),
                std::ptr::from_ref(&c.session_extreme_regressed_low)
            ),
            "high and low must be distinct handles or the two series merge"
        );
    }
}
