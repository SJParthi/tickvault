//! Groww 1-minute candle aggregator — second feed (operator lock §32).
//!
//! Pure, in-memory, O(1)-per-tick fold of Groww live ticks into 1-minute OHLCV
//! candles. Self-contained (no I/O, no storage/network deps) so it is fully
//! offline-unit-tested; the bridge PR maps the emitted [`Groww1mCandle`] to
//! `tickvault_storage::groww_candle_persistence::GrowwCandle1mRow` and persists
//! it to the shared `candles_1m` table (tagged `feed='groww'`).
//!
//! ## Design
//!
//! One open [`MinuteBucket`] per instrument, keyed by `(security_id,
//! ExchangeSegment)` — the I-P1-11 composite identity (segment-aware). Each tick:
//! - floor its IST-nanos timestamp to the minute (O(1) arithmetic);
//! - same minute as the open bucket → fold (high/low/close/volume/tick_count);
//! - a LATER minute → **seal** the open bucket (emit a [`Groww1mCandle`]) and
//!   start a fresh bucket from this tick;
//! - an EARLIER minute (out-of-order/late) → discard + count (never corrupts a
//!   sealed candle).
//!
//! ## Volume (honest convention)
//!
//! Groww ticks carry **cumulative day volume**. A minute's candle volume is the
//! cross-minute delta: `last_cum_this_minute − last_cum_previous_minute`. The
//! first bucket of an instrument (no previous minute) uses its own first tick's
//! cumulative as the baseline (in-minute delta). Saturated at 0 so a cumulative
//! reset never yields a negative volume. The EXACT match to Groww's backtest 1m
//! volume convention is verified in the parity-check PR — documented, not
//! assumed.

use std::collections::HashMap;

use tickvault_common::candle_fold::{
    FoldOutcome, FoldStrategy, FoldedCandle, LatePolicy, OneMinFoldCell,
};
use tickvault_common::types::ExchangeSegment;

/// SP3b: Groww folds through the COMMON cell with the Discard late policy (Groww
/// drops out-of-order ticks; it never re-folds a sealed minute). One source of
/// truth for the fold logic — see `tickvault_common::candle_fold`.
const GROWW_FOLD_STRATEGY: FoldStrategy = FoldStrategy {
    late_policy: LatePolicy::Discard,
};

/// A normalised Groww live tick fed into the aggregator. `cum_volume` is the
/// cumulative day volume (Groww semantics); `ltp` is the last traded price.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Groww1mTick {
    pub security_id: i64,
    pub segment: ExchangeSegment,
    /// IST epoch nanoseconds (already IST — NO offset applied here).
    pub ts_ist_nanos: i64,
    pub ltp: f64,
    pub cum_volume: i64,
}

/// A sealed 1-minute OHLCV candle emitted by the aggregator.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Groww1mCandle {
    pub security_id: i64,
    pub segment: ExchangeSegment,
    /// Minute-aligned IST epoch nanoseconds (the candle's designated timestamp).
    pub minute_start_ist_nanos: i64,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    /// Per-minute volume (cross-minute cumulative delta; see module docs).
    pub volume: i64,
    pub tick_count: i64,
}

/// Floors an IST-nanos timestamp to its minute boundary. O(1). Re-exported from
/// the common fold cell so the Groww module's public API is unchanged (SP3b).
#[must_use]
pub fn floor_to_minute_nanos(ts_ist_nanos: i64) -> i64 {
    tickvault_common::candle_fold::floor_to_minute_nanos(ts_ist_nanos)
}

/// Attach the instrument identity to a common [`FoldedCandle`].
#[inline]
fn to_groww_candle(security_id: i64, segment: ExchangeSegment, c: FoldedCandle) -> Groww1mCandle {
    Groww1mCandle {
        security_id,
        segment,
        minute_start_ist_nanos: c.minute_start_ist_nanos,
        open: c.open,
        high: c.high,
        low: c.low,
        close: c.close,
        volume: c.volume,
        tick_count: c.tick_count,
    }
}

/// In-memory 1-minute candle aggregator for the Groww feed. One open
/// [`OneMinFoldCell`] (the COMMON fold cell, SP3b) per `(security_id, segment)`.
/// O(1) per tick. The fold logic now lives in `tickvault_common::candle_fold` —
/// this is the thin per-feed container that holds the cells + the Discard policy.
#[derive(Debug, Default)]
pub struct Groww1mAggregator {
    // Composite `(security_id, segment)` key per I-P1-11 (segment-aware) — never
    // `security_id` alone.
    cells: HashMap<(i64, ExchangeSegment), OneMinFoldCell>,
    late_ticks: u64,
}

impl Groww1mAggregator {
    #[must_use]
    pub fn new() -> Self {
        Self {
            cells: HashMap::new(),
            late_ticks: 0,
        }
    }

    /// Fold one tick through the common cell. Returns a sealed [`Groww1mCandle`]
    /// when this tick crosses into a new minute for that instrument; `None`
    /// otherwise (folded in place, or discarded as late under the Discard policy).
    /// O(1).
    pub fn on_tick(&mut self, tick: &Groww1mTick) -> Option<Groww1mCandle> {
        let key = (tick.security_id, tick.segment);
        match self.cells.get_mut(&key) {
            None => {
                // First tick for this instrument — baseline = its own cumulative
                // (in-minute delta), matching the prior aggregator semantics.
                self.cells.insert(
                    key,
                    OneMinFoldCell::new(
                        tick.ts_ist_nanos,
                        tick.ltp,
                        tick.cum_volume,
                        tick.cum_volume,
                    ),
                );
                None
            }
            Some(cell) => {
                match cell.consume(
                    tick.ts_ist_nanos,
                    tick.ltp,
                    tick.cum_volume,
                    GROWW_FOLD_STRATEGY,
                ) {
                    FoldOutcome::Updated => None,
                    FoldOutcome::Sealed(c) => {
                        Some(to_groww_candle(tick.security_id, tick.segment, c))
                    }
                    // Discard policy never amends, but map defensively for totality.
                    FoldOutcome::AmendedLate(c) => {
                        Some(to_groww_candle(tick.security_id, tick.segment, c))
                    }
                    FoldOutcome::Discarded => {
                        self.late_ticks = self.late_ticks.saturating_add(1);
                        None
                    }
                }
            }
        }
    }

    /// Seals and drains every open cell (e.g. at session end / IST midnight),
    /// returning their candles. O(n) over open instruments. Order unspecified.
    pub fn force_seal_all(&mut self) -> Vec<Groww1mCandle> {
        let mut out = Vec::with_capacity(self.cells.len());
        for ((security_id, segment), cell) in self.cells.drain() {
            out.push(to_groww_candle(security_id, segment, cell.snapshot()));
        }
        out
    }

    /// Count of out-of-order ticks discarded so far (observability).
    #[must_use]
    pub const fn late_tick_count(&self) -> u64 {
        self.late_ticks
    }

    /// Number of instruments with an open (unsealed) cell (observability).
    #[must_use]
    pub fn open_bucket_count(&self) -> usize {
        self.cells.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tickvault_common::candle_fold::NANOS_PER_MINUTE;

    const M1: i64 = 1_780_000_020_000_000_000; // some minute-aligned ts
    const SEG: ExchangeSegment = ExchangeSegment::NseEquity;

    fn tick(ts: i64, ltp: f64, cum_volume: i64) -> Groww1mTick {
        Groww1mTick {
            security_id: 1_333,
            segment: SEG,
            ts_ist_nanos: ts,
            ltp,
            cum_volume,
        }
    }

    #[test]
    fn test_floor_to_minute_nanos() {
        assert_eq!(floor_to_minute_nanos(M1), M1);
        assert_eq!(floor_to_minute_nanos(M1 + 37_000_000_000), M1); // +37s → same minute
        assert_eq!(
            floor_to_minute_nanos(M1 + NANOS_PER_MINUTE + 1),
            M1 + NANOS_PER_MINUTE
        );
    }

    #[test]
    fn test_force_seal_all_seals_single_minute_fold() {
        let mut agg = Groww1mAggregator::new();
        // 3 ticks in the same minute — no seal emitted yet.
        assert!(agg.on_tick(&tick(M1, 100.0, 10)).is_none());
        assert!(agg.on_tick(&tick(M1 + 1_000_000_000, 105.0, 14)).is_none());
        assert!(agg.on_tick(&tick(M1 + 2_000_000_000, 99.0, 20)).is_none());
        assert_eq!(agg.open_bucket_count(), 1);

        let candles = agg.force_seal_all();
        assert_eq!(candles.len(), 1);
        let c = candles[0];
        assert_eq!(c.minute_start_ist_nanos, M1);
        assert_eq!(c.open, 100.0);
        assert_eq!(c.high, 105.0);
        assert_eq!(c.low, 99.0);
        assert_eq!(c.close, 99.0);
        assert_eq!(c.tick_count, 3);
        // First bucket: baseline = first cum (10); last cum (20) → 10.
        assert_eq!(c.volume, 10);
        assert_eq!(agg.open_bucket_count(), 0);
    }

    #[test]
    fn test_on_tick_minute_boundary_emits_prior_candle() {
        let mut agg = Groww1mAggregator::new();
        agg.on_tick(&tick(M1, 100.0, 100)); // minute 1 open, baseline 100
        agg.on_tick(&tick(M1 + 30_000_000_000, 110.0, 150)); // fold, last cum 150
        // Cross into minute 2 — seals minute 1.
        let sealed = agg
            .on_tick(&tick(M1 + NANOS_PER_MINUTE, 111.0, 170))
            .expect("minute-1 candle emitted on boundary");
        assert_eq!(sealed.minute_start_ist_nanos, M1);
        assert_eq!(sealed.open, 100.0);
        assert_eq!(sealed.high, 110.0);
        assert_eq!(sealed.close, 110.0);
        // minute-1 volume = last_cum(150) − baseline(100) = 50.
        assert_eq!(sealed.volume, 50);

        // minute-2 baseline = minute-1 closing cumulative (150); seal it.
        let c2 = agg.force_seal_all();
        assert_eq!(c2.len(), 1);
        assert_eq!(c2[0].minute_start_ist_nanos, M1 + NANOS_PER_MINUTE);
        assert_eq!(c2[0].volume, 170 - 150); // 20
        assert_eq!(c2[0].open, 111.0);
    }

    #[test]
    fn test_late_tick_count_increments_on_out_of_order_tick() {
        let mut agg = Groww1mAggregator::new();
        agg.on_tick(&tick(M1 + NANOS_PER_MINUTE, 100.0, 10)); // open minute 2
        // A tick from minute 1 arrives late → discarded, no candle, counted.
        assert!(agg.on_tick(&tick(M1, 99.0, 5)).is_none());
        assert_eq!(agg.late_tick_count(), 1);
        assert_eq!(agg.open_bucket_count(), 1);
    }

    #[test]
    fn test_open_bucket_count_keeps_two_instruments_isolated() {
        let mut agg = Groww1mAggregator::new();
        let a = Groww1mTick {
            security_id: 1,
            segment: ExchangeSegment::NseEquity,
            ts_ist_nanos: M1,
            ltp: 50.0,
            cum_volume: 5,
        };
        // Same numeric id, DIFFERENT segment — must be a distinct bucket (I-P1-11).
        let b = Groww1mTick {
            security_id: 1,
            segment: ExchangeSegment::NseFno,
            ts_ist_nanos: M1,
            ltp: 200.0,
            cum_volume: 9,
        };
        agg.on_tick(&a);
        agg.on_tick(&b);
        assert_eq!(agg.open_bucket_count(), 2, "segment-aware key keeps both");
        let candles = agg.force_seal_all();
        assert_eq!(candles.len(), 2);
    }

    #[test]
    fn test_volume_saturates_at_zero_on_cumulative_reset() {
        let mut agg = Groww1mAggregator::new();
        agg.on_tick(&tick(M1, 100.0, 1_000)); // baseline 1000
        // Cumulative "resets" lower (e.g. new day) → volume must not go negative.
        let sealed = agg
            .on_tick(&tick(M1 + NANOS_PER_MINUTE, 100.0, 5))
            .expect("seal");
        assert_eq!(sealed.volume, 0, "saturated, never negative");
    }

    #[test]
    fn test_new_aggregator_is_empty() {
        let agg = Groww1mAggregator::new();
        assert_eq!(agg.open_bucket_count(), 0);
        assert_eq!(agg.late_tick_count(), 0);
    }
}
