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
    /// **INCREMENTAL volume** within this 1-second bucket.
    /// Computed as `current_tick_cumulative - bucket_start_cumulative`.
    /// Item 28 (Wave 5): fixes pre-2026-05-01 bug where this field stored cumulative-snapshot
    /// (`tick.volume`), making `sum(volume)` cascade in materialized_views.rs:315 produce
    /// 60× over-count. Mat view `sum(volume)` is now MATHEMATICALLY CORRECT.
    pub volume: u32,
    /// **Bucket-start cumulative volume** — the cumulative-volume value at the start of THIS
    /// 1-second bucket. Set ONCE on bucket creation (from per-security tracker), never updated.
    /// Used to compute incremental volume on each subsequent tick: `volume = tick.volume - bucket_start_cumulative`.
    /// For the first-ever tick of a session: bucket_start_cumulative = 0 (cumulative starts at 0).
    /// For subsequent buckets: bucket_start_cumulative = previous bucket's last cumulative
    /// (carried via the per-security tracker, ensuring no inter-bucket volume is lost when a
    /// bucket has no ticks).
    pub bucket_start_cumulative: u32,
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
    ///
    /// `bucket_start_cumulative` is the cumulative volume at the END of the previous bucket
    /// for this security (or 0 if this is the first-ever tick or after a session reset).
    /// Initial volume = `tick.volume - bucket_start_cumulative` (incremental from bucket start).
    #[inline(always)]
    fn from_tick(tick: &ParsedTick, bucket_start_cumulative: u32) -> Self {
        Self {
            timestamp_secs: tick.exchange_timestamp,
            open: tick.last_traded_price,
            high: tick.last_traded_price,
            low: tick.last_traded_price,
            close: tick.last_traded_price,
            // Item 28: incremental volume from bucket start (NOT cumulative snapshot).
            volume: tick.volume.saturating_sub(bucket_start_cumulative),
            bucket_start_cumulative,
            tick_count: 1,
            iv: tick.iv,
            delta: tick.delta,
            gamma: tick.gamma,
            theta: tick.theta,
            vega: tick.vega,
        }
    }

    /// Updates the candle with a new tick in the same second.
    ///
    /// Recomputes `volume` as `tick.volume - self.bucket_start_cumulative` (incremental).
    /// `self.bucket_start_cumulative` is set once on bucket creation; it stays constant
    /// across ticks within the same bucket.
    #[inline(always)]
    fn update(&mut self, tick: &ParsedTick) {
        if tick.last_traded_price > self.high {
            self.high = tick.last_traded_price;
        }
        if tick.last_traded_price < self.low {
            self.low = tick.last_traded_price;
        }
        self.close = tick.last_traded_price;
        // Item 28: recompute incremental volume from bucket start.
        // saturating_sub guards against rare out-of-order tick where current cumulative < bucket start.
        self.volume = tick.volume.saturating_sub(self.bucket_start_cumulative);
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
///
/// # I-P1-11 segment-aware key invariant
///
/// The map key is the COMPOSITE `(u32 security_id, u8 exchange_segment_code)`,
/// NOT `security_id` alone. Dhan's instrument master can reuse the same numeric
/// `security_id` across different `ExchangeSegment` values (e.g. FINNIFTY's
/// IDX_I index value `27` and an NSE_EQ instrument with id `27` both existed
/// live on 2026-04-17). A `HashMap<u32, _>` would silently merge OHLCV across
/// the two segments, polluting candles. The composite key prevents this.
/// Ratchets: `test_candle_aggregator_keyed_on_security_id_and_segment`,
/// `test_two_instruments_same_id_different_segment_do_not_merge_ohlcv`.
pub struct CandleAggregator {
    /// Active candles keyed by `(security_id, segment_code)`.
    /// I-P1-11: composite key — DO NOT regress to `HashMap<u32, _>`.
    candles: HashMap<(u32, u8), LiveCandle>,
    /// Buffer for completed candles (drained by caller after sweep).
    completed: Vec<CompletedCandle>,
    /// Total candles completed since startup.
    total_completed: u64,
    /// **Per-security last-cumulative-volume tracker** — Item 28 (Wave 5) Choice A.
    ///
    /// Holds the LATEST cumulative volume seen for each `(security_id, segment_code)`,
    /// updated on EVERY tick regardless of bucket boundary. Used to seed the
    /// `bucket_start_cumulative` of NEW candles when a bucket boundary is crossed,
    /// ensuring that volume traded between buckets (when intermediate buckets had no ticks)
    /// is correctly attributed to the bucket where the next tick arrives.
    ///
    /// Why this is necessary: naive `last - first` per bucket misses inter-bucket volume.
    /// Example: bucket B has tick at cumulative=1500 then bucket C has no ticks then
    /// bucket D has tick at cumulative=1800. Naive D.volume = 1800 - 1800 = 0 (WRONG).
    /// With this tracker: D's bucket_start_cumulative = 1500 (from previous tick), so
    /// D.volume = 1800 - 1500 = 300 (CORRECT).
    ///
    /// Session-reset detection: if `tick.volume < tracker.get(key)`, treat as session
    /// boundary (NSE F&O cumulative resets at 09:15:00 IST next day). bucket_start = 0.
    /// (Item 26 L1 monotonicity guard fires CRITICAL if this happens within a single
    /// trading day; that runtime check is in a follow-up sub-PR.)
    last_cumulative_volume: HashMap<(u32, u8), u32>,
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
            // Item 28 Choice A: same capacity as candles HashMap; one entry per
            // active security carried across bucket boundaries.
            last_cumulative_volume: HashMap::with_capacity(CANDLE_MAP_INITIAL_CAPACITY),
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

        // Item 28 Choice A: compute bucket_start_cumulative from per-security tracker.
        // - First-ever tick for this security: tracker is empty → bucket_start = 0.
        // - Subsequent buckets: tracker holds cumulative from previous tick (which may
        //   be in the same bucket OR the previous bucket, but always represents
        //   "cumulative at end of last seen activity").
        // - Session reset detection: if current tick's cumulative is LESS than tracker,
        //   it's a session boundary (cumulative resets at 09:15:00 IST next day);
        //   reset bucket_start to 0. (Within a single trading day, decreasing volume
        //   would violate cumulative semantic — Item 26 L1 catches that as CRITICAL
        //   in a separate sub-PR.)
        let prev_cum = self.last_cumulative_volume.get(&key).copied().unwrap_or(0);
        let bucket_start = if tick.volume < prev_cum {
            0 // Session reset (next trading day)
        } else {
            prev_cum
        };

        if let Some(candle) = self.candles.get_mut(&key) {
            if tick.exchange_timestamp == candle.timestamp_secs {
                // Same second — update OHLCV in place. Candle's stored
                // bucket_start_cumulative is unchanged; volume re-computed as incremental.
                candle.update(tick);
            } else {
                // New second — complete the old candle (its volume is final incremental)
                // and start a new bucket with bucket_start = previous-tick cumulative.
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
                *candle = LiveCandle::from_tick(tick, bucket_start);
            }
        } else {
            // First tick for this security — start a new candle.
            // bucket_start = 0 here (tracker was empty), so candle.volume = tick.volume
            // (correctly representing cumulative-since-session-open for the first bucket).
            self.candles
                .insert(key, LiveCandle::from_tick(tick, bucket_start));
        }

        // ALWAYS update the tracker AFTER processing the tick, so the NEXT tick
        // (whether in the same bucket or a new one) sees the current cumulative.
        self.last_cumulative_volume.insert(key, tick.volume);
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
    fn volume_is_incremental_within_bucket_post_item_28() {
        // Item 28 (Wave 5) fix: candle volume is INCREMENTAL from bucket_start_cumulative,
        // NOT the cumulative-snapshot of the latest tick.
        //
        // For the first bucket of a security: bucket_start_cumulative = 0, so
        // candle.volume == latest_tick.volume (== cumulative-since-session-open).
        //
        // The "drop from 500 to 300" in this test simulates a session reset OR
        // an out-of-order tick. With saturating_sub, the candle correctly recomputes
        // from bucket_start = 0 → candle.volume = 300.
        let mut agg = CandleAggregator::new();
        agg.update(&make_tick(100, 2, 500.0, 1000, 100)); // 1st bucket, 1st tick: bucket_start=0, vol=100-0=100
        agg.update(&make_tick(100, 2, 510.0, 1000, 500)); // same bucket, 2nd tick: vol=500-0=500
        agg.update(&make_tick(100, 2, 505.0, 1000, 300)); // same bucket, 3rd tick: vol=300-0=300 (saturating_sub safe)

        let candle = agg.candles.get(&(100, 2)).expect("candle must exist");
        assert_eq!(
            candle.volume, 300,
            "Item 28: volume should be incremental from bucket_start_cumulative=0; for first bucket, equals latest tick's cumulative."
        );
        assert_eq!(
            candle.bucket_start_cumulative, 0,
            "first-ever bucket for this security should have bucket_start_cumulative = 0"
        );
    }

    #[test]
    fn volume_carries_across_bucket_boundary_correctly_item_28() {
        // Item 28 critical regression: the bug fix's central claim.
        //
        // Bucket A (ts=1000): cumulative goes 1000 → 1500. A.volume = 1500 (incremental from 0).
        // Bucket B (ts=1001): cumulative goes 1500 → 1800. B.volume = 1800-1500 = 300.
        //
        // Pre-Item 28 (snapshot bug): B.volume would be 1800. sum across A+B = 1500+1800 = 3300.
        // Post-Item 28: B.volume = 300. sum across A+B = 1500+300 = 1800 (CORRECT — matches final cumulative).
        let mut agg = CandleAggregator::new();
        agg.update(&make_tick(100, 2, 500.0, 1000, 1000));
        agg.update(&make_tick(100, 2, 502.0, 1000, 1500)); // A closes at cum=1500
        agg.update(&make_tick(100, 2, 503.0, 1001, 1800)); // B opens with bucket_start=1500

        // Bucket A is now in completed buffer
        let completed = agg.drain_completed();
        assert_eq!(completed.len(), 1, "bucket A should be completed");
        assert_eq!(completed[0].timestamp_secs, 1000);
        assert_eq!(
            completed[0].volume, 1500,
            "Item 28: bucket A volume = incremental from session start (bucket_start=0) to A's close = 1500"
        );

        // Bucket B is still active in self.candles
        let candle_b = agg.candles.get(&(100, 2)).expect("bucket B must exist");
        assert_eq!(candle_b.timestamp_secs, 1001);
        assert_eq!(
            candle_b.bucket_start_cumulative, 1500,
            "Item 28: bucket B's start = previous bucket's last cumulative (1500), NOT 0 and NOT 1800"
        );
        assert_eq!(
            candle_b.volume, 300,
            "Item 28: bucket B volume = 1800 - 1500 = 300 (incremental WITHIN this bucket, not cumulative)"
        );
    }

    #[test]
    fn volume_attributes_skipped_bucket_volume_to_next_bucket_item_28() {
        // Item 28 inter-bucket attribution: when an intermediate bucket has no ticks,
        // its volume is correctly attributed to the bucket where the next tick arrives.
        //
        // Bucket A (ts=1000): cum 1000 → 1500.
        // Bucket B (ts=1001): NO TICKS (security went silent for 1 second).
        // Bucket C (ts=1002): cum reaches 2300 (1500 → 1800 happened during B silently, then 1800 → 2300 in C).
        //
        // Without per-security tracker (naive last-first per bucket): C.volume = 0 (only one tick = no delta).
        //                                                              Total volume lost = 800.
        // With per-security tracker (Item 28): C.bucket_start = 1500 (carried from end of A). C.volume = 2300 - 1500 = 800.
        //                                       Volume conservation: A + C = 1500 + 800 = 2300 (matches final cumulative).
        let mut agg = CandleAggregator::new();
        agg.update(&make_tick(100, 2, 500.0, 1000, 1000));
        agg.update(&make_tick(100, 2, 502.0, 1000, 1500)); // A closes at cum=1500
        // No ticks in bucket 1001 — security went silent
        agg.update(&make_tick(100, 2, 506.0, 1002, 2300)); // C opens 2 seconds later

        // Snapshot bucket C's volume BEFORE draining completed (avoids overlapping borrows).
        let c_volume = {
            let candle_c = agg.candles.get(&(100, 2)).expect("bucket C must exist");
            assert_eq!(candle_c.timestamp_secs, 1002);
            assert_eq!(
                candle_c.bucket_start_cumulative, 1500,
                "Item 28: bucket C's start = end of bucket A (1500), even though B had no ticks"
            );
            assert_eq!(
                candle_c.volume, 800,
                "Item 28: bucket C volume = 2300 - 1500 = 800 (captures inter-bucket trades from silent bucket B)"
            );
            candle_c.volume
        };

        // Verify volume conservation: A.volume + C.volume should equal C's final cumulative
        let completed = agg.drain_completed();
        let a_volume = completed[0].volume; // bucket A
        assert_eq!(
            a_volume + c_volume,
            2300,
            "Item 28 volume conservation: sum of all bucket-incremental volumes = final cumulative"
        );
    }

    #[test]
    fn volume_session_reset_resets_bucket_start_to_zero_item_28() {
        // Item 28 session-reset detection: when current tick's cumulative is LESS than
        // the per-security tracker, treat as new trading-day session reset.
        // bucket_start_cumulative resets to 0 → first tick of new day correctly captures
        // its cumulative-since-09:15 IST.
        //
        // (Item 26 L1 will fire CRITICAL alert if a decrease is observed within a single
        // trading day — that's a separate sub-PR.)
        let mut agg = CandleAggregator::new();
        // Day 1 trading
        agg.update(&make_tick(100, 2, 500.0, 1000, 50_000)); // bucket A, day 1: cum=50K
        agg.update(&make_tick(100, 2, 502.0, 1000, 75_000)); // same bucket, cum=75K (incremental=75K)
        // Day 2 first tick — cumulative reset, smaller value than tracker
        agg.update(&make_tick(100, 2, 510.0, 86_400 + 1000, 500)); // new bucket (different ts), cum=500 < 75K (reset)

        let candle = agg.candles.get(&(100, 2)).expect("candle must exist");
        assert_eq!(
            candle.bucket_start_cumulative, 0,
            "Item 28: session reset detected (tick.volume=500 < tracker=75000); bucket_start reset to 0"
        );
        assert_eq!(
            candle.volume, 500,
            "Item 28: first bucket of new day volume = cumulative since 09:15 IST today = 500"
        );
    }

    #[test]
    fn first_ever_tick_volume_equals_cumulative_item_28() {
        // Item 28 first-ever-tick semantic: when no prior tick exists for a security,
        // tracker is empty (`unwrap_or(0)`) → bucket_start = 0 → candle.volume = tick.volume.
        // This is mathematically correct: first cumulative observed = volume traded since
        // session open (assuming Dhan publishes from 09:15 IST onward).
        let mut agg = CandleAggregator::new();
        agg.update(&make_tick(999, 2, 1000.0, 33_300, 12_345)); // first tick ever for security 999

        let candle = agg.candles.get(&(999, 2)).expect("candle must exist");
        assert_eq!(
            candle.bucket_start_cumulative, 0,
            "first-ever tick: bucket_start_cumulative = 0 (no prior cumulative known)"
        );
        assert_eq!(
            candle.volume, 12345,
            "Item 28: first-ever bucket volume = tick.volume (== cumulative since 09:15 IST)"
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

    // ------------------------------------------------------------------
    // Wave 5 Item 7 — I-P1-11 segment-aware key ratchet tests.
    // ------------------------------------------------------------------

    /// Pins the composite key shape `(security_id, segment_code)`. Failing this
    /// test means someone regressed the aggregator to `HashMap<u32, _>`, which
    /// would silently merge OHLCV across segments per the I-P1-11 collision
    /// (Dhan reuses security_id 27 for FINNIFTY IDX_I + NSE_EQ id 27).
    #[test]
    fn test_candle_aggregator_keyed_on_security_id_and_segment() {
        let mut agg = CandleAggregator::new();
        // Same security_id (27), different segments (IDX_I=0, NSE_EQ=1).
        agg.update(&make_tick(27, 0, 25_650.0, 1000, 0)); // IDX_I — index value
        agg.update(&make_tick(27, 1, 1_234.5, 1000, 100)); // NSE_EQ — equity
        // If the key were just `u32`, the second tick would overwrite the first
        // and active_count would be 1. The composite key keeps both alive.
        assert_eq!(
            agg.active_count(),
            2,
            "composite key (security_id, segment_code) MUST keep colliding ids separated"
        );
        // Each segment's candle is reachable on its own composite key.
        assert!(agg.candles.contains_key(&(27, 0)));
        assert!(agg.candles.contains_key(&(27, 1)));
    }

    /// I-P1-11 OHLCV non-merge guarantee. Two ticks with the same security_id
    /// in the same second-bucket but on different segments must NOT update each
    /// other's open/high/low/close. The IDX_I index value at 25,650 must not
    /// pollute the NSE_EQ candle at 1,234.5 and vice versa.
    #[test]
    fn test_two_instruments_same_id_different_segment_do_not_merge_ohlcv() {
        let mut agg = CandleAggregator::new();
        // IDX_I tick path: open at 25,650, high to 25,700, low to 25,600.
        agg.update(&make_tick(27, 0, 25_650.0, 1000, 0));
        agg.update(&make_tick(27, 0, 25_700.0, 1000, 0)); // index OI is 0
        agg.update(&make_tick(27, 0, 25_600.0, 1000, 0));
        // NSE_EQ tick path: open at 1,234.5, distinct OHLCV.
        agg.update(&make_tick(27, 1, 1_234.5, 1000, 100));
        agg.update(&make_tick(27, 1, 1_240.0, 1000, 200));
        agg.update(&make_tick(27, 1, 1_230.0, 1000, 300));

        let idx = agg.candles.get(&(27, 0)).expect("IDX_I candle must exist");
        let eq = agg.candles.get(&(27, 1)).expect("NSE_EQ candle must exist");

        // IDX_I OHLCV stays in 25,000s — NEVER pulled toward equity range.
        assert_eq!(idx.open, 25_650.0);
        assert_eq!(idx.high, 25_700.0);
        assert_eq!(idx.low, 25_600.0);
        assert_eq!(idx.close, 25_600.0);

        // NSE_EQ OHLCV stays in 1,200s — NEVER pulled toward index range.
        assert_eq!(eq.open, 1_234.5);
        assert_eq!(eq.high, 1_240.0);
        assert_eq!(eq.low, 1_230.0);
        assert_eq!(eq.close, 1_230.0);

        // Volume tracking is also segment-isolated.
        assert_eq!(idx.volume, 0, "IDX_I volume isolated from NSE_EQ");
        assert_eq!(eq.volume, 300, "NSE_EQ volume isolated from IDX_I");
    }
}
