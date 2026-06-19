//! Groww 1-minute candle aggregator — second feed (operator lock §32).
//!
//! Pure, in-memory, O(1)-per-tick fold of Groww live ticks into 1-minute OHLCV
//! candles. Self-contained (no I/O, no storage/network deps) so it is fully
//! offline-unit-tested; the bridge PR maps the emitted [`Groww1mCandle`] to
//! `tickvault_storage::groww_candle_persistence::GrowwCandle1mRow` and persists
//! it to `groww_candles_1m`.
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

use tickvault_common::types::ExchangeSegment;

/// Nanoseconds in one minute — the candle bucket width.
const NANOS_PER_MINUTE: i64 = 60_000_000_000;

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

/// The open (in-progress) bucket for one instrument.
#[derive(Clone, Copy, Debug)]
struct MinuteBucket {
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
}

impl MinuteBucket {
    fn seal(&self, security_id: i64, segment: ExchangeSegment) -> Groww1mCandle {
        Groww1mCandle {
            security_id,
            segment,
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
}

/// Floors an IST-nanos timestamp to its minute boundary. O(1).
#[must_use]
pub fn floor_to_minute_nanos(ts_ist_nanos: i64) -> i64 {
    ts_ist_nanos - ts_ist_nanos.rem_euclid(NANOS_PER_MINUTE)
}

/// In-memory 1-minute candle aggregator for the Groww feed. One open bucket per
/// `(security_id, segment)`. O(1) per tick.
#[derive(Debug, Default)]
pub struct Groww1mAggregator {
    // Composite `(security_id, segment)` key per I-P1-11 (segment-aware) — never
    // `security_id` alone.
    buckets: HashMap<(i64, ExchangeSegment), MinuteBucket>,
    late_ticks: u64,
}

impl Groww1mAggregator {
    #[must_use]
    pub fn new() -> Self {
        Self {
            buckets: HashMap::new(),
            late_ticks: 0,
        }
    }

    /// Fold one tick. Returns a sealed [`Groww1mCandle`] when this tick crosses
    /// into a new minute for that instrument (sealing the prior minute); `None`
    /// otherwise (folded in place, or discarded as late). O(1).
    pub fn on_tick(&mut self, tick: &Groww1mTick) -> Option<Groww1mCandle> {
        let minute_start = floor_to_minute_nanos(tick.ts_ist_nanos);
        let key = (tick.security_id, tick.segment);

        match self.buckets.get_mut(&key) {
            None => {
                self.buckets
                    .insert(key, new_bucket(tick, minute_start, tick.cum_volume));
                None
            }
            Some(bucket) => {
                if minute_start == bucket.minute_start_nanos {
                    bucket.high = bucket.high.max(tick.ltp);
                    bucket.low = bucket.low.min(tick.ltp);
                    bucket.close = tick.ltp;
                    bucket.last_cum_volume = tick.cum_volume;
                    bucket.tick_count = bucket.tick_count.saturating_add(1);
                    None
                } else if minute_start > bucket.minute_start_nanos {
                    let candle = bucket.seal(tick.security_id, tick.segment);
                    // New minute baseline = the sealed minute's closing cumulative.
                    *bucket = new_bucket(tick, minute_start, bucket.last_cum_volume);
                    Some(candle)
                } else {
                    // Earlier minute than the open bucket → out-of-order/late.
                    self.late_ticks = self.late_ticks.saturating_add(1);
                    None
                }
            }
        }
    }

    /// Seals and drains every open bucket (e.g. at session end / IST midnight),
    /// returning their candles. O(n) over open instruments. Order unspecified.
    pub fn force_seal_all(&mut self) -> Vec<Groww1mCandle> {
        let mut out = Vec::with_capacity(self.buckets.len());
        for ((security_id, segment), bucket) in self.buckets.drain() {
            out.push(bucket.seal(security_id, segment));
        }
        out
    }

    /// Count of out-of-order ticks discarded so far (observability).
    #[must_use]
    pub const fn late_tick_count(&self) -> u64 {
        self.late_ticks
    }

    /// Number of instruments with an open (unsealed) bucket (observability).
    #[must_use]
    pub fn open_bucket_count(&self) -> usize {
        self.buckets.len()
    }
}

/// Builds a fresh single-tick bucket. `baseline_cum_volume` is the per-minute
/// volume baseline (prior minute's closing cumulative, or this tick's cumulative
/// for an instrument's first bucket).
fn new_bucket(tick: &Groww1mTick, minute_start: i64, baseline_cum_volume: i64) -> MinuteBucket {
    MinuteBucket {
        minute_start_nanos: minute_start,
        open: tick.ltp,
        high: tick.ltp,
        low: tick.ltp,
        close: tick.ltp,
        baseline_cum_volume,
        last_cum_volume: tick.cum_volume,
        tick_count: 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
