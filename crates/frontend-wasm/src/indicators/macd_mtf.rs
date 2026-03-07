//! CM Ultimate MACD Multi-Timeframe indicator.
//!
//! Standard MACD with fast/slow EMA and signal line.
//! The "MTF" aspect is handled by the pipeline manager which feeds
//! candles from different timeframe aggregators.
//!
//! # Outputs (4 values per candle)
//! - `[0]` = MACD line (fast EMA - slow EMA)
//! - `[1]` = Signal line (EMA of MACD)
//! - `[2]` = Histogram (MACD - Signal)
//! - `[3]` = Zero line (always 0.0, for rendering reference)
//!
//! # Parameters
//! - fast_length: Fast EMA period (default: 12)
//! - slow_length: Slow EMA period (default: 26)
//! - signal_length: Signal EMA period (default: 9)

use crate::indicator::*;
use crate::registry::IndicatorFactory;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const OUTPUT_COUNT: usize = 4;
const IDX_MACD: usize = 0;
const IDX_SIGNAL: usize = 1;
const IDX_HISTOGRAM: usize = 2;
const IDX_ZERO: usize = 3;

const DEFAULT_FAST: usize = 12;
const DEFAULT_SLOW: usize = 26;
const DEFAULT_SIGNAL: usize = 9;

// ---------------------------------------------------------------------------
// Metadata
// ---------------------------------------------------------------------------

static OUTPUTS: &[OutputDef] = &[
    OutputDef {
        name: "MACD",
        color: "#2962ff",
        style: OutputStyle::Line,
    },
    OutputDef {
        name: "Signal",
        color: "#ff6d00",
        style: OutputStyle::Line,
    },
    OutputDef {
        name: "Histogram",
        color: "#26a69a",
        style: OutputStyle::Histogram,
    },
    OutputDef {
        name: "Zero",
        color: "#787b86",
        style: OutputStyle::Line,
    },
];

static PARAMS: &[ParamDef] = &[
    ParamDef {
        name: "Fast Length",
        kind: ParamKind::Int(DEFAULT_FAST as i64, 1, 200),
    },
    ParamDef {
        name: "Slow Length",
        kind: ParamKind::Int(DEFAULT_SLOW as i64, 1, 200),
    },
    ParamDef {
        name: "Signal Length",
        kind: ParamKind::Int(DEFAULT_SIGNAL as i64, 1, 200),
    },
];

static META: IndicatorMeta = IndicatorMeta {
    id: "macd_mtf",
    display_name: "CM Ultimate MACD MTF",
    display: DisplayMode::Pane,
    outputs: OUTPUTS,
    params: PARAMS,
};

// ---------------------------------------------------------------------------
// EMA helper
// ---------------------------------------------------------------------------

/// Incremental EMA calculator. O(1) per update.
struct Ema {
    period: usize,
    multiplier: f64,
    value: f64,
    count: usize,
    sum: f64,
}

impl Ema {
    fn new(period: usize) -> Self {
        Self {
            period,
            multiplier: 2.0 / (period as f64 + 1.0),
            value: f64::NAN,
            count: 0,
            sum: 0.0,
        }
    }

    /// Updates EMA with a new value. Returns current EMA.
    ///
    /// Uses SMA for the first `period` values as seed, then switches to EMA.
    /// O(1) per call.
    #[inline(always)]
    fn update(&mut self, price: f64) -> f64 {
        self.count += 1;
        if self.count <= self.period {
            self.sum += price;
            if self.count == self.period {
                self.value = self.sum / self.period as f64;
            }
        } else {
            self.value = (price - self.value) * self.multiplier + self.value;
        }
        self.value
    }

    fn reset(&mut self) {
        self.value = f64::NAN;
        self.count = 0;
        self.sum = 0.0;
    }
}

// ---------------------------------------------------------------------------
// Compute
// ---------------------------------------------------------------------------

pub struct MacdMtfCompute {
    fast_ema: Ema,
    slow_ema: Ema,
    signal_ema: Ema,
    slow_length: usize,
    candle_count: usize,
    output: [f64; OUTPUT_COUNT],
}

impl MacdMtfCompute {
    fn new(fast_length: usize, slow_length: usize, signal_length: usize) -> Self {
        Self {
            fast_ema: Ema::new(fast_length),
            slow_ema: Ema::new(slow_length),
            signal_ema: Ema::new(signal_length),
            slow_length,
            candle_count: 0,
            output: [f64::NAN; OUTPUT_COUNT],
        }
    }
}

impl Compute for MacdMtfCompute {
    fn on_candle(&mut self, _open: f64, _high: f64, _low: f64, close: f64, _volume: f64) -> &[f64] {
        self.candle_count += 1;

        let fast_val = self.fast_ema.update(close);
        let slow_val = self.slow_ema.update(close);

        if self.candle_count < self.slow_length {
            self.output = [f64::NAN; OUTPUT_COUNT];
            return &self.output;
        }

        let macd = fast_val - slow_val;
        let signal = self.signal_ema.update(macd);

        self.output[IDX_MACD] = macd;
        self.output[IDX_SIGNAL] = signal;
        self.output[IDX_HISTOGRAM] = if signal.is_finite() {
            macd - signal
        } else {
            f64::NAN
        };
        self.output[IDX_ZERO] = 0.0;

        &self.output
    }

    fn reset(&mut self) {
        self.fast_ema.reset();
        self.slow_ema.reset();
        self.signal_ema.reset();
        self.candle_count = 0;
        self.output = [f64::NAN; OUTPUT_COUNT];
    }

    fn output_count(&self) -> usize {
        OUTPUT_COUNT
    }
}

// ---------------------------------------------------------------------------
// Factory
// ---------------------------------------------------------------------------

pub struct MacdMtfFactory;

impl IndicatorFactory for MacdMtfFactory {
    fn metadata(&self) -> &IndicatorMeta {
        &META
    }

    fn create(&self, params: &[f64]) -> Box<dyn Compute> {
        let fast = params.first().map(|p| *p as usize).unwrap_or(DEFAULT_FAST);
        let slow = params.get(1).map(|p| *p as usize).unwrap_or(DEFAULT_SLOW);
        let signal = params.get(2).map(|p| *p as usize).unwrap_or(DEFAULT_SIGNAL);

        Box::new(MacdMtfCompute::new(fast, slow, signal))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_macd_metadata() {
        let factory = MacdMtfFactory;
        let meta = factory.metadata();
        assert_eq!(meta.id, "macd_mtf");
        assert_eq!(meta.display, DisplayMode::Pane);
        assert_eq!(meta.outputs.len(), 4);
    }

    #[test]
    fn test_ema_basic() {
        let mut ema = Ema::new(3);
        ema.update(10.0);
        ema.update(20.0);
        let val = ema.update(30.0); // SMA seed = 20.0
        assert!((val - 20.0).abs() < f64::EPSILON);

        let val = ema.update(40.0); // EMA = (40 - 20) * 0.5 + 20 = 30
        assert!((val - 30.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_macd_warmup() {
        let factory = MacdMtfFactory;
        let mut macd = factory.create(&[12.0, 26.0, 9.0]);

        for i in 0..25 {
            let price = 25000.0 + (i as f64 * 10.0);
            let out = macd.on_candle(price, price + 10.0, price - 10.0, price, 1000.0);
            assert!(out[IDX_MACD].is_nan(), "candle {i} should be NaN");
        }
    }

    #[test]
    fn test_macd_produces_values() {
        let factory = MacdMtfFactory;
        let mut macd = factory.create(&[3.0, 5.0, 3.0]); // Short periods

        for i in 0..10 {
            let price = 25000.0 + (i as f64 * 10.0);
            macd.on_candle(price, price + 10.0, price - 10.0, price, 1000.0);
        }

        let out = macd.on_candle(25100.0, 25110.0, 25090.0, 25100.0, 1000.0);
        assert!(out[IDX_MACD].is_finite(), "MACD should be finite");
        assert!((out[IDX_ZERO] - 0.0).abs() < f64::EPSILON, "zero line = 0");
    }

    #[test]
    fn test_macd_reset() {
        let factory = MacdMtfFactory;
        let mut macd = factory.create(&[3.0, 5.0, 3.0]);

        for i in 0..10 {
            let price = 25000.0 + (i as f64 * 10.0);
            macd.on_candle(price, price + 10.0, price - 10.0, price, 1000.0);
        }

        macd.reset();
        let out = macd.on_candle(25000.0, 25010.0, 24990.0, 25000.0, 1000.0);
        assert!(out[IDX_MACD].is_nan(), "should be NaN after reset");
    }
}
