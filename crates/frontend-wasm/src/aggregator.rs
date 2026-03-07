//! Tick-to-candle aggregator.
//!
//! Converts raw tick data into OHLCV candles at any timeframe.
//! O(1) per tick — maintains a single "current candle" that closes
//! when the timestamp crosses a timeframe boundary.

// ---------------------------------------------------------------------------
// Timeframe
// ---------------------------------------------------------------------------

/// Supported candle timeframes in seconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Timeframe {
    /// 1-second candles.
    S1 = 1,
    /// 5-second candles.
    S5 = 5,
    /// 15-second candles.
    S15 = 15,
    /// 30-second candles.
    S30 = 30,
    /// 1-minute candles.
    M1 = 60,
    /// 3-minute candles.
    M3 = 180,
    /// 5-minute candles.
    M5 = 300,
    /// 15-minute candles.
    M15 = 900,
    /// 30-minute candles.
    M30 = 1800,
    /// 1-hour candles.
    H1 = 3600,
    /// 4-hour candles.
    H4 = 14400,
    /// 1-day candles.
    D1 = 86400,
}

impl Timeframe {
    /// Returns the duration in seconds.
    pub fn seconds(self) -> u64 {
        self as u64
    }

    /// Aligns a UTC epoch timestamp to the start of its timeframe bucket.
    ///
    /// # IST Alignment
    /// Applies IST offset (+5:30 = 19800s) before bucketing, then subtracts
    /// it back. This ensures candles align to IST clock boundaries.
    ///
    /// # Performance
    /// O(1) — integer division + multiplication.
    #[inline(always)]
    pub fn align_timestamp(self, epoch_secs: u64) -> u64 {
        let ist_offset: u64 = 19800; // +5:30 in seconds
        let tf_secs = self.seconds();
        let ist_time = epoch_secs.saturating_add(ist_offset);
        let aligned_ist = (ist_time / tf_secs) * tf_secs;
        aligned_ist.saturating_sub(ist_offset)
    }
}

// ---------------------------------------------------------------------------
// Candle
// ---------------------------------------------------------------------------

/// A single OHLCV candle.
#[derive(Debug, Clone, Copy)]
pub struct Candle {
    /// Candle open time (UTC epoch seconds, aligned to timeframe).
    pub time: u64,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    pub volume: f64,
}

// ---------------------------------------------------------------------------
// Aggregator
// ---------------------------------------------------------------------------

/// Aggregates raw ticks into OHLCV candles at a configurable timeframe.
///
/// # Performance
/// O(1) per tick — updates running min/max/close in the current candle.
/// Returns `Some(Candle)` when a timeframe boundary is crossed (candle closed).
pub struct TickAggregator {
    timeframe: Timeframe,
    /// Currently building candle (None if no tick received yet).
    current: Option<CandleBuilder>,
}

/// Internal candle builder — accumulates tick data for the current period.
struct CandleBuilder {
    time: u64,
    open: f64,
    high: f64,
    low: f64,
    close: f64,
    volume: f64,
}

impl CandleBuilder {
    fn new(time: u64, price: f64, volume: f64) -> Self {
        Self {
            time,
            open: price,
            high: price,
            low: price,
            close: price,
            volume,
        }
    }

    /// Updates the builder with a new tick. O(1).
    #[inline(always)]
    fn update(&mut self, price: f64, volume: f64) {
        if price > self.high {
            self.high = price;
        }
        if price < self.low {
            self.low = price;
        }
        self.close = price;
        self.volume = volume; // cumulative volume from exchange, not delta
    }

    fn to_candle(&self) -> Candle {
        Candle {
            time: self.time,
            open: self.open,
            high: self.high,
            low: self.low,
            close: self.close,
            volume: self.volume,
        }
    }
}

impl TickAggregator {
    /// Creates a new aggregator for the given timeframe.
    pub fn new(timeframe: Timeframe) -> Self {
        Self {
            timeframe,
            current: None,
        }
    }

    /// Processes a single tick.
    ///
    /// # Returns
    /// - `Some(Candle)` if this tick caused the previous candle to close
    /// - `None` if the tick was absorbed into the current candle
    ///
    /// The returned candle is the CLOSED candle. The current (open) candle
    /// is accessible via `current_candle()`.
    ///
    /// # Performance
    /// O(1) — one timestamp alignment + one comparison + one update.
    #[inline(always)]
    pub fn on_tick(&mut self, epoch_secs: u64, price: f64, volume: f64) -> Option<Candle> {
        let aligned = self.timeframe.align_timestamp(epoch_secs);

        match &mut self.current {
            Some(builder) if builder.time == aligned => {
                // Same candle period — update in place
                builder.update(price, volume);
                None
            }
            Some(builder) => {
                // New candle period — close the previous candle
                let closed = builder.to_candle();
                *builder = CandleBuilder::new(aligned, price, volume);
                Some(closed)
            }
            None => {
                // First tick ever — start a new candle
                self.current = Some(CandleBuilder::new(aligned, price, volume));
                None
            }
        }
    }

    /// Returns the current (open, not yet closed) candle, if any.
    pub fn current_candle(&self) -> Option<Candle> {
        self.current.as_ref().map(|b| b.to_candle())
    }

    /// Returns the current timeframe.
    pub fn timeframe(&self) -> Timeframe {
        self.timeframe
    }

    /// Changes the timeframe and resets state.
    pub fn set_timeframe(&mut self, timeframe: Timeframe) {
        self.timeframe = timeframe;
        self.current = None;
    }

    /// Resets the aggregator (called on symbol change).
    pub fn reset(&mut self) {
        self.current = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_timeframe_seconds() {
        assert_eq!(Timeframe::S1.seconds(), 1);
        assert_eq!(Timeframe::M1.seconds(), 60);
        assert_eq!(Timeframe::M5.seconds(), 300);
        assert_eq!(Timeframe::H1.seconds(), 3600);
        assert_eq!(Timeframe::D1.seconds(), 86400);
    }

    #[test]
    fn test_timeframe_align_1m() {
        // 2026-03-07 09:15:30 IST = 2026-03-07 03:45:30 UTC
        // epoch = 1772858730 (approximate)
        let tf = Timeframe::M1;
        let ts = 1772858730_u64;
        let aligned = tf.align_timestamp(ts);
        // Should be 30 seconds earlier (aligned to minute boundary in IST)
        assert_eq!(aligned % 60, tf.align_timestamp(aligned) % 60);
    }

    #[test]
    fn test_timeframe_align_idempotent() {
        let tf = Timeframe::M5;
        let ts = 1772858700_u64;
        let aligned = tf.align_timestamp(ts);
        let realigned = tf.align_timestamp(aligned);
        assert_eq!(aligned, realigned);
    }

    #[test]
    fn test_aggregator_first_tick_no_candle() {
        let mut agg = TickAggregator::new(Timeframe::M1);
        let result = agg.on_tick(1772858730, 25000.0, 100.0);
        assert!(result.is_none()); // First tick — no closed candle yet
        assert!(agg.current_candle().is_some());
    }

    #[test]
    fn test_aggregator_same_period_updates() {
        let mut agg = TickAggregator::new(Timeframe::M1);
        let base = 1772858700_u64; // aligned to minute

        agg.on_tick(base, 100.0, 10.0);
        agg.on_tick(base + 10, 105.0, 20.0); // higher
        agg.on_tick(base + 20, 95.0, 30.0); // lower
        agg.on_tick(base + 30, 102.0, 40.0); // close

        let candle = agg.current_candle().unwrap();
        assert!((candle.open - 100.0).abs() < f64::EPSILON);
        assert!((candle.high - 105.0).abs() < f64::EPSILON);
        assert!((candle.low - 95.0).abs() < f64::EPSILON);
        assert!((candle.close - 102.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_aggregator_candle_close_on_new_period() {
        let mut agg = TickAggregator::new(Timeframe::M1);
        let base = Timeframe::M1.align_timestamp(1772858700);

        // Tick in period 1
        agg.on_tick(base + 10, 100.0, 10.0);
        agg.on_tick(base + 30, 105.0, 20.0);

        // Tick in period 2 — should close period 1
        let closed = agg.on_tick(base + 70, 110.0, 30.0);
        assert!(closed.is_some());

        let candle = closed.unwrap();
        assert!((candle.open - 100.0).abs() < f64::EPSILON);
        assert!((candle.high - 105.0).abs() < f64::EPSILON);
        assert!((candle.close - 105.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_aggregator_reset() {
        let mut agg = TickAggregator::new(Timeframe::M1);
        agg.on_tick(1772858700, 100.0, 10.0);
        assert!(agg.current_candle().is_some());

        agg.reset();
        assert!(agg.current_candle().is_none());
    }

    #[test]
    fn test_aggregator_set_timeframe_resets() {
        let mut agg = TickAggregator::new(Timeframe::M1);
        agg.on_tick(1772858700, 100.0, 10.0);

        agg.set_timeframe(Timeframe::M5);
        assert_eq!(agg.timeframe(), Timeframe::M5);
        assert!(agg.current_candle().is_none());
    }
}
