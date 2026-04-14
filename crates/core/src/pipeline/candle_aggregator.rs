//! Real-time 1-second candle aggregation from live ticks.
//!
//! Aggregates `ParsedTick` into 1-second OHLCV candles in-memory. Completed candles
//! are flushed to QuestDB via ILP. Higher timeframes (5m, 15m) are handled by
//! QuestDB materialized views — this module only produces the base 1s candles.
//!
//! # Performance
//! - O(1) per tick: HashMap lookup + OHLCV update (all inline arithmetic)
//! - Pre-allocated HashMap (no reallocation on hot path)
//! - `LiveCandle` is Copy, compact struct (OHLCV + Greeks)

use std::collections::HashMap;

use tickvault_common::tick_types::ParsedTick;

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
/// Copy for zero-allocation hot path.
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
    /// Implied volatility (latest snapshot from Greeks pipeline, f64::NAN if not available).
    pub iv: f64,
    /// Delta (latest snapshot from Greeks pipeline, f64::NAN if not available).
    pub delta: f64,
    /// Gamma (latest snapshot from Greeks pipeline, f64::NAN if not available).
    pub gamma: f64,
    /// Theta (latest snapshot from Greeks pipeline, f64::NAN if not available).
    pub theta: f64,
    /// Vega (latest snapshot from Greeks pipeline, f64::NAN if not available).
    pub vega: f64,
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
            iv: tick.iv,
            delta: tick.delta,
            gamma: tick.gamma,
            theta: tick.theta,
            vega: tick.vega,
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
        // Update Greeks from latest tick snapshot (only if available).
        if !tick.iv.is_nan() {
            self.iv = tick.iv;
            self.delta = tick.delta;
            self.gamma = tick.gamma;
            self.theta = tick.theta;
            self.vega = tick.vega;
        }
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
    /// Candle timestamp (second boundary, IST epoch seconds from Dhan WebSocket).
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
    /// Implied volatility (latest snapshot, f64::NAN if not available).
    pub iv: f64,
    /// Delta (latest snapshot, f64::NAN if not available).
    pub delta: f64,
    /// Gamma (latest snapshot, f64::NAN if not available).
    pub gamma: f64,
    /// Theta (latest snapshot, f64::NAN if not available).
    pub theta: f64,
    /// Vega (latest snapshot, f64::NAN if not available).
    pub vega: f64,
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
                    iv: candle.iv,
                    delta: candle.delta,
                    gamma: candle.gamma,
                    theta: candle.theta,
                    vega: candle.vega,
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
                    iv: candle.iv,
                    delta: candle.delta,
                    gamma: candle.gamma,
                    theta: candle.theta,
                    vega: candle.vega,
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
                iv: candle.iv,
                delta: candle.delta,
                gamma: candle.gamma,
                theta: candle.theta,
                vega: candle.vega,
            });
            self.total_completed = self.total_completed.saturating_add(1);
        }
        self.candles.clear();
    }

    /// Returns a slice of completed candles ready for persistence.
    ///
    /// Call [`clear_completed()`](Self::clear_completed) after processing to free the buffer
    /// while preserving its pre-allocated capacity (avoids reallocation on the next cycle).
    pub fn completed_slice(&self) -> &[CompletedCandle] {
        &self.completed
    }

    /// Clears the completed buffer, preserving pre-allocated capacity.
    ///
    /// Unlike `std::mem::take` (which replaces with a zero-capacity Vec and discards the
    /// pre-allocated buffer), `clear()` keeps the existing allocation for reuse.
    pub fn clear_completed(&mut self) {
        self.completed.clear();
    }

    /// Returns the number of active (in-progress) candles.
    pub fn active_count(&self) -> usize {
        self.candles.len()
    }

    /// Returns the total number of candles completed since startup.
    pub fn total_completed(&self) -> u64 {
        self.total_completed
    }

    /// Drains and returns completed candles (test-only convenience).
    ///
    /// Uses `std::mem::take` which discards the pre-allocated capacity.
    /// Production code must use [`completed_slice()`](Self::completed_slice) +
    /// [`clear_completed()`](Self::clear_completed) to preserve capacity.
    #[cfg(test)]
    pub fn drain_completed(&mut self) -> Vec<CompletedCandle> {
        std::mem::take(&mut self.completed)
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
        // 28 bytes (OHLCV + meta) + 40 bytes (5 × f64 Greeks) + padding = 72 bytes
        assert!(
            std::mem::size_of::<LiveCandle>() <= 80,
            "LiveCandle must be compact (OHLCV + Greeks)"
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
            iv: f64::NAN,
            delta: f64::NAN,
            gamma: f64::NAN,
            theta: f64::NAN,
            vega: f64::NAN,
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

    // -----------------------------------------------------------------------
    // Functional: OHLCV correctness with known price sequence
    // -----------------------------------------------------------------------

    #[test]
    fn test_functional_candle_ohlcv_correctness() {
        let mut agg = CandleAggregator::new();

        // Feed a known sequence: O=100, H=120, L=90, C=105, V=5000, ticks=5
        // Timestamp 1000 (second boundary).
        agg.update(&make_tick(42, 2, 100.0, 1000, 1000)); // Open=100
        agg.update(&make_tick(42, 2, 110.0, 1000, 2000)); // Mid
        agg.update(&make_tick(42, 2, 120.0, 1000, 3000)); // High=120
        agg.update(&make_tick(42, 2, 90.0, 1000, 4000)); // Low=90
        agg.update(&make_tick(42, 2, 105.0, 1000, 5000)); // Close=105

        // Trigger completion by advancing to next second.
        agg.update(&make_tick(42, 2, 106.0, 1001, 5100));

        let completed = agg.drain_completed();
        assert_eq!(completed.len(), 1);
        let c = &completed[0];

        // Verify every OHLCV field exactly.
        assert_eq!(c.security_id, 42);
        assert_eq!(c.exchange_segment_code, 2);
        assert_eq!(c.timestamp_secs, 1000);
        assert_eq!(c.open, 100.0, "open must be the FIRST tick's price");
        assert_eq!(c.high, 120.0, "high must be the maximum price");
        assert_eq!(c.low, 90.0, "low must be the minimum price");
        assert_eq!(c.close, 105.0, "close must be the LAST tick's price");
        assert_eq!(c.volume, 5000, "volume must be the last snapshot");
        assert_eq!(
            c.tick_count, 5,
            "tick_count must equal number of ticks in that second"
        );

        // Verify the new active candle started correctly.
        let active = agg.candles.get(&(42, 2)).expect("active candle must exist");
        assert_eq!(active.open, 106.0);
        assert_eq!(active.timestamp_secs, 1001);
        assert_eq!(active.tick_count, 1);
    }

    // -----------------------------------------------------------------------
    // Functional: sweep_stale emits correct candle data
    // -----------------------------------------------------------------------

    #[test]
    fn test_functional_candle_sweep_stale() {
        let mut agg = CandleAggregator::new();

        // Security 100: active candle at timestamp 1000 with known OHLCV.
        agg.update(&make_tick(100, 2, 200.0, 1000, 500));
        agg.update(&make_tick(100, 2, 210.0, 1000, 600)); // High
        agg.update(&make_tick(100, 2, 195.0, 1000, 700)); // Low

        // Security 200: active candle at timestamp 1008 (recent — should NOT be swept).
        agg.update(&make_tick(200, 2, 300.0, 1008, 1000));

        // Sweep at timestamp 1010 — candle at 1000 is stale (>5s), candle at 1008 is not.
        agg.sweep_stale(1010);

        // Only security 100 should be swept.
        assert_eq!(
            agg.active_count(),
            1,
            "only recent candle should remain active"
        );
        assert!(
            agg.candles.contains_key(&(200, 2)),
            "security 200 should still be active"
        );

        let completed = agg.drain_completed();
        assert_eq!(completed.len(), 1);
        let c = &completed[0];

        // Verify the swept candle has correct OHLCV.
        assert_eq!(c.security_id, 100);
        assert_eq!(c.timestamp_secs, 1000);
        assert_eq!(c.open, 200.0);
        assert_eq!(c.high, 210.0);
        assert_eq!(c.low, 195.0);
        assert_eq!(c.close, 195.0); // Last tick was the low
        assert_eq!(c.volume, 700);
        assert_eq!(c.tick_count, 3);
    }

    // -----------------------------------------------------------------------
    // Functional: same ticks produce correct candles across multiple seconds
    // (simulates what higher-timeframe rollup would consume)
    // -----------------------------------------------------------------------

    #[test]
    fn test_functional_multiple_timeframes() {
        // This aggregator produces 1-second candles. Higher timeframes (1m, 5m)
        // are QuestDB materialized views that consume these 1s candles.
        // Verify that 60 consecutive 1s candles have correct per-second OHLCV
        // that would correctly roll up to a 1-minute candle.
        let mut agg = CandleAggregator::new();

        let base_ts = 1_700_000_000_u32;
        let security_id = 100_u32;
        let segment = 2_u8;

        // Simulate 60 seconds of ticks, each second has 3 ticks.
        // Price pattern: starts at 100, rises to 160 linearly.
        for second in 0..60_u32 {
            let ts = base_ts + second;
            let base_price = 100.0 + second as f32;
            agg.update(&make_tick(
                security_id,
                segment,
                base_price,
                ts,
                second * 100,
            ));
            agg.update(&make_tick(
                security_id,
                segment,
                base_price + 0.5,
                ts,
                second * 100 + 50,
            ));
            agg.update(&make_tick(
                security_id,
                segment,
                base_price - 0.25,
                ts,
                second * 100 + 75,
            ));
        }

        // Flush to get all candles including the last active one.
        agg.flush_all();
        let candles = agg.drain_completed();

        // 60 seconds: 59 completed by second transitions + 1 flushed = 60 total.
        assert_eq!(
            candles.len(),
            60,
            "must produce exactly 60 one-second candles"
        );

        // Verify first candle (second 0): O=100.0, H=100.5, L=99.75, C=99.75
        let first = candles
            .iter()
            .find(|c| c.timestamp_secs == base_ts)
            .expect("first candle");
        assert_eq!(first.open, 100.0);
        assert_eq!(first.high, 100.5);
        assert_eq!(first.low, 99.75);
        assert_eq!(first.close, 99.75);
        assert_eq!(first.tick_count, 3);

        // Verify last candle (second 59): O=159.0, H=159.5, L=158.75, C=158.75
        let last = candles
            .iter()
            .find(|c| c.timestamp_secs == base_ts + 59)
            .expect("last candle");
        assert_eq!(last.open, 159.0);
        assert_eq!(last.high, 159.5);
        assert_eq!(last.low, 158.75);
        assert_eq!(last.close, 158.75);
        assert_eq!(last.tick_count, 3);

        // Simulate 1-minute rollup: overall min/max across all 60 candles.
        let minute_open = candles
            .iter()
            .min_by_key(|c| c.timestamp_secs)
            .expect("non-empty")
            .open;
        let minute_close = candles
            .iter()
            .max_by_key(|c| c.timestamp_secs)
            .expect("non-empty")
            .close;
        let minute_high = candles
            .iter()
            .map(|c| c.high)
            .fold(f32::NEG_INFINITY, f32::max);
        let minute_low = candles.iter().map(|c| c.low).fold(f32::INFINITY, f32::min);

        // 1-minute candle correctness (what QuestDB materialized view would compute).
        assert_eq!(minute_open, 100.0, "1-minute open = first 1s open");
        assert_eq!(minute_close, 158.75, "1-minute close = last 1s close");
        assert_eq!(minute_high, 159.5, "1-minute high = max of all 1s highs");
        assert_eq!(minute_low, 99.75, "1-minute low = min of all 1s lows");

        // 5-minute rollup: first 300 seconds. We only have 60, so verify subset.
        // All 60 candles are in the same 5-minute window.
        let five_min_candles: Vec<_> = candles
            .iter()
            .filter(|c| c.timestamp_secs >= base_ts && c.timestamp_secs < base_ts + 300)
            .collect();
        assert_eq!(
            five_min_candles.len(),
            60,
            "all 60 candles fit in one 5-minute window"
        );
    }

    // -----------------------------------------------------------------------
    // Coverage: CandleAggregator::new(), Default, accessor methods
    // -----------------------------------------------------------------------

    #[test]
    fn test_new_aggregator_is_empty() {
        let agg = CandleAggregator::new();
        assert_eq!(agg.active_count(), 0);
        assert_eq!(agg.total_completed(), 0);
        assert!(agg.completed_slice().is_empty());
    }

    #[test]
    fn test_default_matches_new() {
        let from_new = CandleAggregator::new();
        let from_default = CandleAggregator::default();
        assert_eq!(from_new.active_count(), from_default.active_count());
        assert_eq!(from_new.total_completed(), from_default.total_completed());
    }

    #[test]
    fn test_clear_completed_preserves_capacity() {
        let mut agg = CandleAggregator::new();
        agg.update(&make_tick(1, 2, 100.0, 1000, 500));
        agg.update(&make_tick(1, 2, 101.0, 1001, 600));
        assert!(!agg.completed_slice().is_empty());
        agg.clear_completed();
        assert!(agg.completed_slice().is_empty());
    }

    #[test]
    fn test_flush_all_emits_all_active_candles() {
        let mut agg = CandleAggregator::new();
        agg.update(&make_tick(1, 2, 100.0, 1000, 500));
        agg.update(&make_tick(2, 2, 200.0, 1000, 600));
        agg.update(&make_tick(3, 1, 300.0, 1000, 700));
        assert_eq!(agg.active_count(), 3);

        agg.flush_all();
        assert_eq!(agg.active_count(), 0);
        assert_eq!(agg.completed_slice().len(), 3);
        assert_eq!(agg.total_completed(), 3);
    }

    #[test]
    fn test_update_first_tick_creates_active_candle() {
        let mut agg = CandleAggregator::new();
        agg.update(&make_tick(42, 2, 100.0, 1000, 500));
        assert_eq!(agg.active_count(), 1);
        assert_eq!(agg.total_completed(), 0);
        assert!(agg.completed_slice().is_empty());
    }

    #[test]
    fn test_update_same_second_updates_in_place() {
        let mut agg = CandleAggregator::new();
        agg.update(&make_tick(42, 2, 100.0, 1000, 500));
        agg.update(&make_tick(42, 2, 110.0, 1000, 600)); // same second
        assert_eq!(agg.active_count(), 1);
        assert_eq!(agg.total_completed(), 0);
    }

    #[test]
    fn test_drain_completed_returns_and_clears() {
        let mut agg = CandleAggregator::new();
        agg.update(&make_tick(42, 2, 100.0, 1000, 500));
        agg.update(&make_tick(42, 2, 101.0, 1001, 600)); // completes 1000
        let drained = agg.drain_completed();
        assert_eq!(drained.len(), 1);
        assert!(agg.completed_slice().is_empty());
    }

    #[test]
    fn test_sweep_stale_no_stale_candles() {
        let mut agg = CandleAggregator::new();
        agg.update(&make_tick(1, 2, 100.0, 1000, 500));
        agg.sweep_stale(1002); // only 2s old, threshold is 5s
        assert_eq!(agg.active_count(), 1);
        assert!(agg.completed_slice().is_empty());
    }

    #[test]
    fn test_completed_candle_fields_copy() {
        let candle = CompletedCandle {
            security_id: 42,
            exchange_segment_code: 2,
            timestamp_secs: 1000,
            open: 100.0,
            high: 110.0,
            low: 90.0,
            close: 105.0,
            volume: 5000,
            tick_count: 5,
            iv: f64::NAN,
            delta: f64::NAN,
            gamma: f64::NAN,
            theta: f64::NAN,
            vega: f64::NAN,
        };
        let copied = candle;
        assert_eq!(copied.security_id, candle.security_id);
        assert_eq!(copied.open, candle.open);
        assert_eq!(copied.high, candle.high);
        assert_eq!(copied.low, candle.low);
        assert_eq!(copied.close, candle.close);
    }
}
