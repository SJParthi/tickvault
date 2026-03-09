//! Real-time 1-second candle aggregation from live ticks.
//!
//! Aggregates `ParsedTick` into 1-second OHLCV candles in-memory. Completed candles
//! are flushed to QuestDB via ILP. Higher timeframes (5m, 15m) are handled by
//! QuestDB materialized views — this module only produces the base 1s candles.
//!
//! # Performance
//! - O(1) per tick: HashMap lookup + OHLCV update (all inline arithmetic)
//! - Pre-allocated HashMap (no reallocation on hot path)
//! - `LiveCandle` is 32 bytes (Copy, half cache line)

use std::collections::HashMap;

use dhan_live_trader_common::tick_types::ParsedTick;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Initial capacity for the candle HashMap. Sized for typical F&O universe (~2000 instruments).
const CANDLE_MAP_INITIAL_CAPACITY: usize = 4096;

/// Stale candle threshold in seconds. Candles older than this are swept and emitted
/// during periodic flush. Prevents unbounded accumulation if a security stops ticking.
const STALE_CANDLE_THRESHOLD_SECS: u32 = 5;

// ---------------------------------------------------------------------------
// LiveCandle — in-memory 1s OHLCV accumulator
// ---------------------------------------------------------------------------

/// A single in-memory 1-second candle being accumulated from live ticks.
///
/// 36 bytes — fits in a cache line. Copy for zero-allocation hot path.
#[derive(Debug, Clone, Copy)]
pub struct LiveCandle {
    /// Candle timestamp (exchange_timestamp truncated to second boundary).
    pub timestamp_secs: u32,
    /// Open price (first tick in this second).
    pub open: f32,
    /// High price.
    pub high: f32,
    /// Low price.
    pub low: f32,
    /// Close price (last tick in this second).
    pub close: f32,
    /// Cumulative volume snapshot from latest tick (NOT incremental).
    pub volume: u32,
    /// Number of ticks aggregated into this candle.
    pub tick_count: u32,
}

impl LiveCandle {
    /// Creates a new candle from the first tick in this second.
    #[inline(always)]
    fn from_tick(tick: &ParsedTick) -> Self {
        Self {
            timestamp_secs: tick.exchange_timestamp,
            open: tick.last_traded_price,
            high: tick.last_traded_price,
            low: tick.last_traded_price,
            close: tick.last_traded_price,
            volume: tick.volume,
            tick_count: 1,
        }
    }

    /// Updates the candle with a new tick in the same second.
    #[inline(always)]
    fn update(&mut self, tick: &ParsedTick) {
        if tick.last_traded_price > self.high {
            self.high = tick.last_traded_price;
        }
        if tick.last_traded_price < self.low {
            self.low = tick.last_traded_price;
        }
        self.close = tick.last_traded_price;
        self.volume = tick.volume; // Snapshot, not incremental
        self.tick_count = self.tick_count.saturating_add(1);
    }
}

// ---------------------------------------------------------------------------
// CompletedCandle — ready for persistence
// ---------------------------------------------------------------------------

/// A completed 1-second candle ready for QuestDB persistence.
#[derive(Debug, Clone, Copy)]
pub struct CompletedCandle {
    /// Dhan security identifier.
    pub security_id: u32,
    /// Binary exchange segment code.
    pub exchange_segment_code: u8,
    /// Candle timestamp (second boundary, IST-naive epoch seconds from Dhan).
    pub timestamp_secs: u32,
    /// Open price.
    pub open: f32,
    /// High price.
    pub high: f32,
    /// Low price.
    pub low: f32,
    /// Close price.
    pub close: f32,
    /// Volume snapshot.
    pub volume: u32,
    /// Number of ticks aggregated into this candle.
    pub tick_count: u32,
}

// ---------------------------------------------------------------------------
// CandleAggregator
// ---------------------------------------------------------------------------

/// Real-time 1-second candle aggregator.
///
/// Maintains a HashMap of `(security_id, exchange_segment_code)` → `LiveCandle`.
/// When a tick arrives for a new second, the previous candle is completed and emitted.
pub struct CandleAggregator {
    /// Active candles keyed by `(security_id, segment_code)`.
    candles: HashMap<(u32, u8), LiveCandle>,
    /// Buffer for completed candles (drained by caller after sweep).
    completed: Vec<CompletedCandle>,
    /// Total candles completed since startup.
    total_completed: u64,
}

impl Default for CandleAggregator {
    fn default() -> Self {
        Self::new()
    }
}

impl CandleAggregator {
    /// Creates a new aggregator with pre-allocated capacity.
    pub fn new() -> Self {
        Self {
            candles: HashMap::with_capacity(CANDLE_MAP_INITIAL_CAPACITY),
            completed: Vec::with_capacity(256),
            total_completed: 0,
        }
    }

    /// Updates the aggregator with a new tick.
    ///
    /// If the tick starts a new second for its security, the previous candle
    /// is completed and pushed to the internal buffer.
    ///
    /// # Performance
    /// O(1) — single HashMap lookup + inline arithmetic.
    #[inline]
    pub fn update(&mut self, tick: &ParsedTick) {
        let key = (tick.security_id, tick.exchange_segment_code);

        if let Some(candle) = self.candles.get_mut(&key) {
            if tick.exchange_timestamp == candle.timestamp_secs {
                // Same second — update OHLCV in place
                candle.update(tick);
            } else {
                // New second — complete the old candle and start fresh
                self.completed.push(CompletedCandle {
                    security_id: tick.security_id,
                    exchange_segment_code: tick.exchange_segment_code,
                    timestamp_secs: candle.timestamp_secs,
                    open: candle.open,
                    high: candle.high,
                    low: candle.low,
                    close: candle.close,
                    volume: candle.volume,
                    tick_count: candle.tick_count,
                });
                self.total_completed = self.total_completed.saturating_add(1);
                *candle = LiveCandle::from_tick(tick);
            }
        } else {
            // First tick for this security — start a new candle
            self.candles.insert(key, LiveCandle::from_tick(tick));
        }
    }

    /// Sweeps stale candles older than `current_timestamp - STALE_CANDLE_THRESHOLD_SECS`.
    ///
    /// Call this periodically (e.g., every 100ms in the flush check) to emit
    /// candles for securities that stopped ticking mid-second.
    pub fn sweep_stale(&mut self, current_timestamp_secs: u32) {
        let threshold = current_timestamp_secs.saturating_sub(STALE_CANDLE_THRESHOLD_SECS);

        self.candles.retain(|&(security_id, segment_code), candle| {
            if candle.timestamp_secs < threshold {
                self.completed.push(CompletedCandle {
                    security_id,
                    exchange_segment_code: segment_code,
                    timestamp_secs: candle.timestamp_secs,
                    open: candle.open,
                    high: candle.high,
                    low: candle.low,
                    close: candle.close,
                    volume: candle.volume,
                    tick_count: candle.tick_count,
                });
                self.total_completed = self.total_completed.saturating_add(1);
                false // Remove from map
            } else {
                true // Keep
            }
        });
    }

    /// Flushes all remaining candles (for graceful shutdown).
    pub fn flush_all(&mut self) {
        for (&(security_id, segment_code), candle) in &self.candles {
            self.completed.push(CompletedCandle {
                security_id,
                exchange_segment_code: segment_code,
                timestamp_secs: candle.timestamp_secs,
                open: candle.open,
                high: candle.high,
                low: candle.low,
                close: candle.close,
                volume: candle.volume,
                tick_count: candle.tick_count,
            });
            self.total_completed = self.total_completed.saturating_add(1);
        }
        self.candles.clear();
    }

    /// Drains and returns completed candles. Caller is responsible for persisting them.
    pub fn drain_completed(&mut self) -> Vec<CompletedCandle> {
        std::mem::take(&mut self.completed)
    }

    /// Returns the number of active (in-progress) candles.
    pub fn active_count(&self) -> usize {
        self.candles.len()
    }

    /// Returns the total number of candles completed since startup.
    pub fn total_completed(&self) -> u64 {
        self.total_completed
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::arithmetic_side_effects)] // APPROVED: test-only arithmetic
mod tests {
    use super::*;

    fn make_tick(
        security_id: u32,
        segment: u8,
        ltp: f32,
        timestamp: u32,
        volume: u32,
    ) -> ParsedTick {
        ParsedTick {
            security_id,
            exchange_segment_code: segment,
            last_traded_price: ltp,
            exchange_timestamp: timestamp,
            volume,
            ..Default::default()
        }
    }

    #[test]
    fn new_aggregator_is_empty() {
        let agg = CandleAggregator::new();
        assert_eq!(agg.active_count(), 0);
        assert_eq!(agg.total_completed(), 0);
    }

    #[test]
    fn first_tick_creates_candle() {
        let mut agg = CandleAggregator::new();
        agg.update(&make_tick(100, 2, 500.0, 1000, 100));
        assert_eq!(agg.active_count(), 1);
        assert_eq!(agg.total_completed(), 0);
    }

    #[test]
    fn same_second_updates_ohlcv() {
        let mut agg = CandleAggregator::new();
        agg.update(&make_tick(100, 2, 500.0, 1000, 100));
        agg.update(&make_tick(100, 2, 510.0, 1000, 200)); // New high
        agg.update(&make_tick(100, 2, 490.0, 1000, 300)); // New low
        agg.update(&make_tick(100, 2, 505.0, 1000, 400)); // Close

        assert_eq!(agg.active_count(), 1);
        let candle = agg.candles.get(&(100, 2)).expect("candle must exist");
        assert_eq!(candle.open, 500.0);
        assert_eq!(candle.high, 510.0);
        assert_eq!(candle.low, 490.0);
        assert_eq!(candle.close, 505.0);
        assert_eq!(candle.volume, 400);
    }

    #[test]
    fn new_second_completes_previous_candle() {
        let mut agg = CandleAggregator::new();
        agg.update(&make_tick(100, 2, 500.0, 1000, 100));
        agg.update(&make_tick(100, 2, 510.0, 1001, 200)); // New second

        assert_eq!(agg.total_completed(), 1);
        let completed = agg.drain_completed();
        assert_eq!(completed.len(), 1);
        assert_eq!(completed[0].security_id, 100);
        assert_eq!(completed[0].timestamp_secs, 1000);
        assert_eq!(completed[0].open, 500.0);
        assert_eq!(completed[0].close, 500.0); // Only one tick
    }

    #[test]
    fn multiple_securities_tracked_independently() {
        let mut agg = CandleAggregator::new();
        agg.update(&make_tick(100, 2, 500.0, 1000, 100));
        agg.update(&make_tick(200, 2, 300.0, 1000, 50));
        agg.update(&make_tick(300, 5, 1500.0, 1000, 75));

        assert_eq!(agg.active_count(), 3);
    }

    #[test]
    fn sweep_stale_removes_old_candles() {
        let mut agg = CandleAggregator::new();
        agg.update(&make_tick(100, 2, 500.0, 1000, 100));
        agg.update(&make_tick(200, 2, 300.0, 1010, 50));

        // Sweep at timestamp 1010 — candle at 1000 is stale (>5s old)
        agg.sweep_stale(1010);

        assert_eq!(agg.active_count(), 1); // Only security 200 remains
        let completed = agg.drain_completed();
        assert_eq!(completed.len(), 1);
        assert_eq!(completed[0].security_id, 100);
    }

    #[test]
    fn sweep_keeps_recent_candles() {
        let mut agg = CandleAggregator::new();
        agg.update(&make_tick(100, 2, 500.0, 1000, 100));

        // Sweep at timestamp 1003 — candle at 1000 is within threshold (5s)
        agg.sweep_stale(1003);

        assert_eq!(agg.active_count(), 1); // Still active
        let completed = agg.drain_completed();
        assert!(completed.is_empty());
    }

    #[test]
    fn flush_all_emits_everything() {
        let mut agg = CandleAggregator::new();
        agg.update(&make_tick(100, 2, 500.0, 1000, 100));
        agg.update(&make_tick(200, 2, 300.0, 1000, 50));
        agg.update(&make_tick(300, 5, 1500.0, 1000, 75));

        agg.flush_all();

        assert_eq!(agg.active_count(), 0);
        assert_eq!(agg.total_completed(), 3);
        let completed = agg.drain_completed();
        assert_eq!(completed.len(), 3);
    }

    #[test]
    fn drain_completed_clears_buffer() {
        let mut agg = CandleAggregator::new();
        agg.update(&make_tick(100, 2, 500.0, 1000, 100));
        agg.update(&make_tick(100, 2, 510.0, 1001, 200));

        let first = agg.drain_completed();
        assert_eq!(first.len(), 1);

        let second = agg.drain_completed();
        assert!(second.is_empty(), "drain must clear the buffer");
    }

    #[test]
    fn live_candle_size_is_compact() {
        assert!(
            std::mem::size_of::<LiveCandle>() <= 36,
            "LiveCandle must fit in a cache line"
        );
    }

    #[test]
    fn completed_candle_has_all_fields() {
        let mut agg = CandleAggregator::new();
        agg.update(&make_tick(42, 3, 100.5, 5000, 999));
        agg.update(&make_tick(42, 3, 105.0, 5000, 1000));
        agg.update(&make_tick(42, 3, 98.0, 5001, 1100)); // New second

        let completed = agg.drain_completed();
        assert_eq!(completed.len(), 1);
        let c = &completed[0];
        assert_eq!(c.security_id, 42);
        assert_eq!(c.exchange_segment_code, 3);
        assert_eq!(c.timestamp_secs, 5000);
        assert_eq!(c.open, 100.5);
        assert_eq!(c.high, 105.0);
        assert_eq!(c.low, 100.5);
        assert_eq!(c.close, 105.0);
        assert_eq!(c.volume, 1000);
    }

    #[test]
    fn stale_threshold_is_reasonable() {
        const _: () = assert!(STALE_CANDLE_THRESHOLD_SECS >= 2, "too aggressive");
        const _: () = assert!(STALE_CANDLE_THRESHOLD_SECS <= 30, "too lenient");
    }

    #[test]
    fn initial_capacity_is_reasonable() {
        const _: () = assert!(CANDLE_MAP_INITIAL_CAPACITY >= 1024);
        const _: () = assert!(CANDLE_MAP_INITIAL_CAPACITY <= 65536);
    }

    #[test]
    fn default_impl_matches_new() {
        let a = CandleAggregator::new();
        let b = CandleAggregator::default();
        assert_eq!(a.active_count(), b.active_count());
        assert_eq!(a.total_completed(), b.total_completed());
    }

    #[test]
    fn rapid_second_transitions() {
        let mut agg = CandleAggregator::new();
        // Simulate ticks across 10 seconds
        for ts in 1000..1010u32 {
            agg.update(&make_tick(100, 2, 500.0 + ts as f32, ts, ts * 10));
        }
        // 9 completed candles (first tick starts a candle, each subsequent second completes previous)
        assert_eq!(agg.total_completed(), 9);
        assert_eq!(agg.active_count(), 1);
    }

    #[test]
    fn same_security_different_segments_tracked_separately() {
        let mut agg = CandleAggregator::new();
        agg.update(&make_tick(100, 2, 500.0, 1000, 100)); // NSE_FNO
        agg.update(&make_tick(100, 1, 505.0, 1000, 200)); // NSE_EQ
        assert_eq!(agg.active_count(), 2);
    }

    #[test]
    fn volume_is_snapshot_not_incremental() {
        let mut agg = CandleAggregator::new();
        agg.update(&make_tick(100, 2, 500.0, 1000, 100));
        agg.update(&make_tick(100, 2, 510.0, 1000, 500)); // Volume jumps
        agg.update(&make_tick(100, 2, 505.0, 1000, 300)); // Volume drops (correction)

        let candle = agg.candles.get(&(100, 2)).expect("candle must exist");
        assert_eq!(
            candle.volume, 300,
            "volume should be latest snapshot, not cumulative"
        );
    }

    #[test]
    fn completed_candle_is_copy() {
        let c = CompletedCandle {
            security_id: 1,
            exchange_segment_code: 2,
            timestamp_secs: 1000,
            open: 100.0,
            high: 110.0,
            low: 95.0,
            close: 105.0,
            volume: 500,
            tick_count: 10,
        };
        let copy = c; // Copy
        assert_eq!(c.security_id, copy.security_id);
        assert_eq!(c.open, copy.open);
    }

    #[test]
    fn sweep_at_exact_threshold_boundary() {
        let mut agg = CandleAggregator::new();
        agg.update(&make_tick(100, 2, 500.0, 1000, 100));

        // Sweep at exactly threshold boundary (1000 + 5 = 1005)
        agg.sweep_stale(1005);
        assert_eq!(
            agg.active_count(),
            1,
            "at exact boundary, candle should remain"
        );

        // Sweep one second past threshold
        agg.sweep_stale(1006);
        assert_eq!(
            agg.active_count(),
            0,
            "past threshold, candle should be swept"
        );
    }

    #[test]
    fn multiple_sweeps_are_idempotent() {
        let mut agg = CandleAggregator::new();
        agg.update(&make_tick(100, 2, 500.0, 1000, 100));

        agg.sweep_stale(1010);
        let first = agg.drain_completed();
        assert_eq!(first.len(), 1);

        agg.sweep_stale(1020);
        let second = agg.drain_completed();
        assert!(second.is_empty(), "second sweep should find nothing");
    }

    #[test]
    fn flush_all_after_partial_drain() {
        let mut agg = CandleAggregator::new();
        agg.update(&make_tick(100, 2, 500.0, 1000, 100));
        agg.update(&make_tick(100, 2, 510.0, 1001, 200)); // Completes first candle
        agg.update(&make_tick(200, 2, 300.0, 1000, 50)); // Active candle

        // Drain the completed candle first
        let drained = agg.drain_completed();
        assert_eq!(drained.len(), 1);

        // Now flush remaining active candles
        agg.flush_all();
        let remaining = agg.drain_completed();
        assert_eq!(remaining.len(), 2); // Security 100 at ts 1001 + security 200
    }

    #[test]
    fn high_throughput_many_securities() {
        let mut agg = CandleAggregator::new();
        // Simulate 1000 securities each with 5 ticks across 3 seconds
        for sec in 0..1000u32 {
            for ts in 0..3u32 {
                agg.update(&make_tick(
                    sec,
                    2,
                    100.0 + ts as f32,
                    1000 + ts,
                    sec * 10 + ts,
                ));
            }
        }
        // Each security has 3 seconds → 2 completed candles per security
        assert_eq!(agg.total_completed(), 2000);
        assert_eq!(agg.active_count(), 1000);
    }
}
