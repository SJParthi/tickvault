//! Common 1-minute OHLCV fold cell — the ONE per-instrument candle-fold logic
//! shared by EVERY feed (SP3 of the common-feed-engine convergence, operator lock
//! 2026-06-22 "make everything as common runtime dynamic scalable").
//!
//! This is the pure, `Copy`, O(1)-per-tick fold of a single instrument's ticks
//! into a 1-minute candle. It holds NO instrument identity (the container owns the
//! `(security_id, segment)` key) and does NO I/O — so it is fully offline-tested
//! and reused by both the Groww aggregator and the Dhan multi-TF engine.
//!
//! ## Per-feed behaviour is a [`FoldStrategy`] parameter — NOT forked code
//!
//! The 3-agent adversarial review (2026-06-22) proved the two feeds' fold logic
//! differs in exactly three policies. Each is a field of [`FoldStrategy`], so the
//! cell code stays common while each feed keeps its own correct behaviour:
//!
//! 1. **`late_policy`** — a tick whose minute is EARLIER than the open bucket:
//!    Groww [`LatePolicy::Discard`]s it (out-of-order, count + drop); Dhan
//!    [`LatePolicy::Refold`]s a 1-bucket-late tick back into the just-sealed
//!    minute's H/L/C and re-emits it (the `AmendedLate` semantics).
//! 2. **i64 cumulative volume** — volume is always `i64` (Groww's cumulative day
//!    volume exceeds `u32` intraday for liquid stocks), folded as the cross-minute
//!    delta `last_cum − baseline_cum`, saturated at 0 on a cumulative reset.
//! 3. **baseline carry** — the per-minute volume baseline is the PREVIOUS minute's
//!    closing cumulative (or the instrument's first tick's cumulative for the very
//!    first bucket). It is a REQUIRED constructor argument (no default) so a caller
//!    can never silently produce `volume = full_cumulative` (the PREVOI-01 class).

/// What to do with a tick whose minute is EARLIER than the open bucket.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LatePolicy {
    /// Re-fold a 1-bucket-late tick into the just-sealed minute's H/L/C and
    /// re-emit (Dhan `AmendedLate`). A tick ≥2 buckets late is still discarded.
    Refold,
    /// Discard the late tick (count it), never touching a sealed candle (Groww).
    Discard,
}

/// Per-feed fold policy (DATA, not forked code).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FoldStrategy {
    pub late_policy: LatePolicy,
}

/// A sealed 1-minute candle's values (no instrument identity — the container
/// attaches `(security_id, segment)`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FoldedCandle {
    pub minute_start_ist_nanos: i64,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    /// Per-minute volume (cross-minute cumulative delta, saturated ≥ 0).
    pub volume: i64,
    pub tick_count: i64,
}

/// The outcome of folding one tick into the cell.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum FoldOutcome {
    /// Folded into the open bucket; nothing sealed.
    Updated,
    /// The tick crossed into a new minute — the PRIOR minute is sealed and
    /// returned; the cell now holds the new minute's opening bucket.
    Sealed(FoldedCandle),
    /// A 1-bucket-late tick re-folded the just-sealed minute (`Refold` policy);
    /// the amended candle is returned for an UPSERT in place.
    AmendedLate(FoldedCandle),
    /// A late tick was discarded (`Discard` policy, or ≥2 buckets late).
    Discarded,
}

/// Nanoseconds in one minute — the candle bucket width.
pub const NANOS_PER_MINUTE: i64 = 60_000_000_000;

/// Floors an IST-nanos timestamp to its minute boundary. O(1). `rem_euclid`
/// keeps it correct for negative / pre-epoch nanos.
#[must_use]
pub fn floor_to_minute_nanos(ts_ist_nanos: i64) -> i64 {
    ts_ist_nanos - ts_ist_nanos.rem_euclid(NANOS_PER_MINUTE)
}

/// One instrument's open 1-minute fold cell. `Copy` + no heap → zero-alloc, O(1).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct OneMinFoldCell {
    minute_start_nanos: i64,
    open: f64,
    high: f64,
    low: f64,
    close: f64,
    /// Cumulative volume at the close of the PREVIOUS minute (or this minute's
    /// first tick for the very first bucket) — the per-minute volume baseline.
    baseline_cum_volume: i64,
    last_cum_volume: i64,
    tick_count: i64,
    /// The most recently sealed minute (for the `Refold` 1-bucket-late check).
    last_sealed_minute: Option<i64>,
    last_sealed: Option<FoldedCandle>,
}

impl OneMinFoldCell {
    /// Open a fresh cell from an instrument's first tick. `baseline_cum_volume` is
    /// REQUIRED (no default): for a brand-new instrument pass `cum_volume` (the
    /// in-minute delta); the container passes the prior minute's closing cumulative
    /// on a minute roll.
    #[must_use]
    pub fn new(ts_ist_nanos: i64, ltp: f64, cum_volume: i64, baseline_cum_volume: i64) -> Self {
        Self {
            // Floor internally so a caller can never mis-key the bucket (matches
            // the Groww aggregator's on_tick flooring).
            minute_start_nanos: floor_to_minute_nanos(ts_ist_nanos),
            open: ltp,
            high: ltp,
            low: ltp,
            close: ltp,
            baseline_cum_volume,
            last_cum_volume: cum_volume,
            tick_count: 1,
            last_sealed_minute: None,
            last_sealed: None,
        }
    }

    /// The candle the open bucket would seal to right now (for `force_seal`).
    #[must_use]
    pub fn snapshot(&self) -> FoldedCandle {
        FoldedCandle {
            minute_start_ist_nanos: self.minute_start_nanos,
            open: self.open,
            high: self.high,
            low: self.low,
            close: self.close,
            volume: self
                .last_cum_volume
                .saturating_sub(self.baseline_cum_volume)
                .max(0),
            tick_count: self.tick_count,
        }
    }

    fn fold_in_place(&mut self, ltp: f64, cum_volume: i64) {
        self.high = self.high.max(ltp);
        self.low = self.low.min(ltp);
        self.close = ltp;
        self.last_cum_volume = cum_volume;
        self.tick_count = self.tick_count.saturating_add(1);
    }

    /// Fold one tick (raw IST-nanos timestamp — floored internally). Per-feed
    /// `strategy` decides the late-tick policy. O(1), zero-alloc.
    pub fn consume(
        &mut self,
        ts_ist_nanos: i64,
        ltp: f64,
        cum_volume: i64,
        strategy: FoldStrategy,
    ) -> FoldOutcome {
        let minute_start = floor_to_minute_nanos(ts_ist_nanos);
        if minute_start == self.minute_start_nanos {
            self.fold_in_place(ltp, cum_volume);
            FoldOutcome::Updated
        } else if minute_start > self.minute_start_nanos {
            // Crossed into a new minute → seal the prior, open a fresh bucket whose
            // baseline is the sealed minute's closing cumulative.
            let sealed = self.snapshot();
            let new_baseline = self.last_cum_volume;
            *self = Self::new(minute_start, ltp, cum_volume, new_baseline);
            self.last_sealed_minute = Some(sealed.minute_start_ist_nanos);
            self.last_sealed = Some(sealed);
            FoldOutcome::Sealed(sealed)
        } else {
            // Earlier minute than the open bucket → late.
            match strategy.late_policy {
                LatePolicy::Refold if self.last_sealed_minute == Some(minute_start) => {
                    // Exactly 1 bucket late → re-fold into the sealed minute + re-emit.
                    let mut amended = self.last_sealed.unwrap_or_else(|| self.snapshot());
                    amended.high = amended.high.max(ltp);
                    amended.low = amended.low.min(ltp);
                    amended.close = ltp;
                    amended.tick_count = amended.tick_count.saturating_add(1);
                    self.last_sealed = Some(amended);
                    FoldOutcome::AmendedLate(amended)
                }
                _ => FoldOutcome::Discarded,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const M1: i64 = 1_780_000_020_000_000_000;
    const DISCARD: FoldStrategy = FoldStrategy {
        late_policy: LatePolicy::Discard,
    };
    const REFOLD: FoldStrategy = FoldStrategy {
        late_policy: LatePolicy::Refold,
    };

    /// Coverage-friendly outcome extractor: BOTH arms are exercised
    /// (`test_outcome_extractors_reject_non_matching_variants` covers the
    /// `None` arm), so seal-asserting tests carry no unreachable
    /// multi-line `panic!` arm that llvm-cov flags as a missed line.
    fn as_sealed(out: FoldOutcome) -> Option<FoldedCandle> {
        match out {
            FoldOutcome::Sealed(c) => Some(c),
            _ => None,
        }
    }

    /// See [`as_sealed`] — same coverage rationale for `AmendedLate`.
    fn as_amended(out: FoldOutcome) -> Option<FoldedCandle> {
        match out {
            FoldOutcome::AmendedLate(c) => Some(c),
            _ => None,
        }
    }

    #[test]
    fn test_outcome_extractors_reject_non_matching_variants() {
        assert!(as_sealed(FoldOutcome::Discarded).is_none());
        assert!(as_sealed(FoldOutcome::Updated).is_none());
        assert!(as_amended(FoldOutcome::Discarded).is_none());
        assert!(as_amended(FoldOutcome::Updated).is_none());
    }

    // ── GOLDEN: the common cell reproduces the existing Groww1mAggregator EXACTLY
    // (same scenarios + values as crates/core/src/feed/groww/aggregator_1m.rs tests),
    // so Groww can switch to this cell with provably-identical candles. ──

    #[test]
    fn test_golden_groww_same_minute_three_tick_fold() {
        // Groww test_force_seal_all_seals_single_minute_fold: O=100 H=105 L=99
        // C=99 vol=10 ticks=3 (baseline = first cum 10, last cum 20).
        let mut cell = OneMinFoldCell::new(M1, 100.0, 10, 10);
        assert_eq!(cell.consume(M1, 105.0, 14, DISCARD), FoldOutcome::Updated);
        assert_eq!(cell.consume(M1, 99.0, 20, DISCARD), FoldOutcome::Updated);
        let c = cell.snapshot();
        assert_eq!(c.open, 100.0);
        assert_eq!(c.high, 105.0);
        assert_eq!(c.low, 99.0);
        assert_eq!(c.close, 99.0);
        assert_eq!(c.volume, 10);
        assert_eq!(c.tick_count, 3);
    }

    #[test]
    fn test_golden_groww_minute_boundary_seal_and_baseline_carry() {
        // Groww test_on_tick_minute_boundary_emits_prior_candle.
        let mut cell = OneMinFoldCell::new(M1, 100.0, 100, 100); // baseline 100
        cell.consume(M1 + 30_000_000_000, 110.0, 150, DISCARD); // fold, last cum 150
        let out = cell.consume(M1 + NANOS_PER_MINUTE, 111.0, 170, DISCARD);
        let sealed = as_sealed(out).expect("expected seal");
        assert_eq!(sealed.minute_start_ist_nanos, M1);
        assert_eq!(sealed.open, 100.0);
        assert_eq!(sealed.high, 110.0);
        assert_eq!(sealed.close, 110.0);
        assert_eq!(sealed.volume, 50); // 150 − 100
        // minute-2 baseline carried = 150; seal it.
        let c2 = cell.snapshot();
        assert_eq!(c2.minute_start_ist_nanos, M1 + NANOS_PER_MINUTE);
        assert_eq!(c2.open, 111.0);
        assert_eq!(c2.volume, 170 - 150); // 20
    }

    #[test]
    fn test_golden_groww_discard_late_out_of_order_tick() {
        // Groww test_late_tick_count_increments_on_out_of_order_tick.
        let mut cell = OneMinFoldCell::new(M1 + NANOS_PER_MINUTE, 100.0, 10, 10);
        let out = cell.consume(M1, 99.0, 5, DISCARD); // minute-1 tick arrives late
        assert_eq!(out, FoldOutcome::Discarded);
    }

    #[test]
    fn test_golden_groww_volume_saturates_at_zero_on_reset() {
        // Groww test_volume_saturates_at_zero_on_cumulative_reset.
        let mut cell = OneMinFoldCell::new(M1, 100.0, 1_000, 1_000);
        let out = cell.consume(M1 + NANOS_PER_MINUTE, 100.0, 5, DISCARD);
        let sealed = as_sealed(out).expect("seal");
        assert_eq!(sealed.volume, 0, "saturated, never negative");
    }

    // ── Dhan Refold policy ──

    #[test]
    fn test_refold_amends_one_bucket_late_tick() {
        let mut cell = OneMinFoldCell::new(M1, 100.0, 100, 100);
        // Seal minute 1 by crossing into minute 2.
        let sealed_out = cell.consume(M1 + NANOS_PER_MINUTE, 90.0, 100, REFOLD);
        let sealed = as_sealed(sealed_out).expect("seal");
        assert_eq!(sealed.high, 100.0);
        // A minute-1 tick arrives 1 bucket late with a NEW high → re-fold + re-emit.
        let out = cell.consume(M1, 120.0, 100, REFOLD);
        let amended = as_amended(out).expect("expected amend");
        assert_eq!(amended.minute_start_ist_nanos, M1);
        assert_eq!(
            amended.high, 120.0,
            "late tick lifted the sealed minute's high"
        );
        assert_eq!(amended.close, 120.0);
    }

    #[test]
    fn test_refold_discards_two_buckets_late() {
        let mut cell = OneMinFoldCell::new(M1 + NANOS_PER_MINUTE, 100.0, 100, 100);
        cell.consume(M1 + 2 * NANOS_PER_MINUTE, 100.0, 100, REFOLD); // seal minute 2
        // minute-0 tick is ≥2 buckets late → discarded even under Refold.
        let out = cell.consume(M1, 999.0, 100, REFOLD);
        assert_eq!(out, FoldOutcome::Discarded);
    }

    #[test]
    fn test_cell_is_copy_zero_alloc() {
        // Compile-time proof the cell is Copy (no heap on the hot path).
        fn assert_copy<T: Copy>() {}
        assert_copy::<OneMinFoldCell>();
        assert_copy::<FoldOutcome>();
    }

    #[test]
    fn test_floor_to_minute_nanos() {
        assert_eq!(floor_to_minute_nanos(M1), M1);
        assert_eq!(floor_to_minute_nanos(M1 + 37_000_000_000), M1);
        assert_eq!(
            floor_to_minute_nanos(M1 + NANOS_PER_MINUTE + 1),
            M1 + NANOS_PER_MINUTE
        );
    }
}
