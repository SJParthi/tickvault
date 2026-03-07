//! Supertrend indicator.
//!
//! Computes Supertrend overlay using ATR (Average True Range) and HL/2 midpoint.
//! Classic TradingView formula: Supertrend(period, multiplier).
//!
//! # Outputs (3 values per candle)
//! - `[0]` = Supertrend line value
//! - `[1]` = Direction (1.0 = bullish/green, -1.0 = bearish/red)
//! - `[2]` = Signal (1.0 = buy, -1.0 = sell, 0.0 = no change)
//!
//! # Parameters
//! - period: ATR lookback period (default: 10)
//! - multiplier: ATR multiplier (default: 3.0)

use crate::indicator::*;
use crate::registry::IndicatorFactory;
use crate::ring_buffer::RingBuffer;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Number of output values per candle.
const OUTPUT_COUNT: usize = 3;

/// Output index: Supertrend line value.
const IDX_LINE: usize = 0;
/// Output index: Direction (1.0 = up, -1.0 = down).
const IDX_DIRECTION: usize = 1;
/// Output index: Signal (1.0 = buy, -1.0 = sell, 0.0 = hold).
const IDX_SIGNAL: usize = 2;

/// Default ATR period.
const DEFAULT_PERIOD: usize = 10;
/// Default ATR multiplier.
const DEFAULT_MULTIPLIER: f64 = 3.0;

// ---------------------------------------------------------------------------
// Metadata
// ---------------------------------------------------------------------------

static OUTPUTS: &[OutputDef] = &[
    OutputDef {
        name: "Supertrend",
        color: "#26a69a",
        style: OutputStyle::StepLine,
    },
    OutputDef {
        name: "Direction",
        color: "#ffffff",
        style: OutputStyle::Line,
    },
    OutputDef {
        name: "Signal",
        color: "#ffab00",
        style: OutputStyle::Marker,
    },
];

static PARAMS: &[ParamDef] = &[
    ParamDef {
        name: "Period",
        kind: ParamKind::Int(DEFAULT_PERIOD as i64, 1, 200),
    },
    ParamDef {
        name: "Multiplier",
        kind: ParamKind::Float(DEFAULT_MULTIPLIER, 0.1, 20.0),
    },
];

static META: IndicatorMeta = IndicatorMeta {
    id: "supertrend",
    display_name: "Supertrend",
    display: DisplayMode::Overlay,
    outputs: OUTPUTS,
    params: PARAMS,
};

// ---------------------------------------------------------------------------
// Compute Implementation
// ---------------------------------------------------------------------------

/// Supertrend indicator compute engine.
///
/// Uses Wilder's smoothed ATR (RMA) for band calculation.
/// O(1) per candle — ring buffer for TR values, EMA-style ATR update.
pub struct SupertrendCompute {
    /// ATR lookback period.
    period: usize,
    /// ATR multiplier for band width.
    multiplier: f64,
    /// True Range ring buffer for ATR calculation.
    tr_buffer: RingBuffer,
    /// Current ATR value (Wilder's smoothing / RMA).
    atr: f64,
    /// Previous close (for True Range calculation).
    prev_close: f64,
    /// Upper band value (basic, before final band logic).
    upper_band: f64,
    /// Lower band value (basic, before final band logic).
    lower_band: f64,
    /// Final upper band (clamped).
    final_upper: f64,
    /// Final lower band (clamped).
    final_lower: f64,
    /// Current direction: true = up (bullish), false = down (bearish).
    direction_up: bool,
    /// Previous direction for signal detection.
    prev_direction_up: bool,
    /// Candle count for warmup detection.
    candle_count: usize,
    /// Pre-allocated output buffer.
    output: [f64; OUTPUT_COUNT],
}

impl SupertrendCompute {
    fn new(period: usize, multiplier: f64) -> Self {
        Self {
            period,
            multiplier,
            tr_buffer: RingBuffer::new(period),
            atr: 0.0,
            prev_close: f64::NAN,
            upper_band: 0.0,
            lower_band: 0.0,
            final_upper: f64::MAX,
            final_lower: f64::MIN,
            direction_up: true,
            prev_direction_up: true,
            candle_count: 0,
            output: [f64::NAN; OUTPUT_COUNT],
        }
    }
}

impl Compute for SupertrendCompute {
    fn on_candle(&mut self, _open: f64, high: f64, low: f64, close: f64, _volume: f64) -> &[f64] {
        self.candle_count += 1;

        // --- True Range ---
        let tr = if self.prev_close.is_nan() {
            high - low
        } else {
            let hl = high - low;
            let hc = (high - self.prev_close).abs();
            let lc = (low - self.prev_close).abs();
            // max of three values
            if hl >= hc && hl >= lc {
                hl
            } else if hc >= lc {
                hc
            } else {
                lc
            }
        };
        self.tr_buffer.push(tr);

        // --- ATR (Wilder's smoothing / RMA) ---
        if self.candle_count <= self.period {
            // Warmup: use SMA for initial ATR
            self.atr = self.tr_buffer.mean();
        } else {
            // Wilder's smoothing: ATR = (prev_ATR * (period-1) + TR) / period
            let period_f = self.period as f64;
            self.atr = (self.atr * (period_f - 1.0) + tr) / period_f;
        }

        // --- Basic Bands ---
        let hl2 = (high + low) / 2.0;
        let band_offset = self.multiplier * self.atr;
        self.upper_band = hl2 + band_offset;
        self.lower_band = hl2 - band_offset;

        // --- Final Bands (clamped) ---
        // Upper band: use lower of current basic upper and previous final upper
        // (band can only move down, never up — prevents whipsaw)
        if self.upper_band < self.final_upper || self.prev_close > self.final_upper {
            self.final_upper = self.upper_band;
        }
        // Lower band: use higher of current basic lower and previous final lower
        // (band can only move up, never down)
        if self.lower_band > self.final_lower || self.prev_close < self.final_lower {
            self.final_lower = self.lower_band;
        }

        // --- Direction ---
        self.prev_direction_up = self.direction_up;
        if close > self.final_upper {
            self.direction_up = true;
        } else if close < self.final_lower {
            self.direction_up = false;
        }
        // else: direction unchanged

        // --- Output ---
        if self.candle_count > self.period {
            // Supertrend line = lower band when bullish, upper band when bearish
            self.output[IDX_LINE] = if self.direction_up {
                self.final_lower
            } else {
                self.final_upper
            };
            self.output[IDX_DIRECTION] = if self.direction_up { 1.0 } else { -1.0 };

            // Signal: direction change
            if self.direction_up && !self.prev_direction_up {
                self.output[IDX_SIGNAL] = 1.0; // Buy signal
            } else if !self.direction_up && self.prev_direction_up {
                self.output[IDX_SIGNAL] = -1.0; // Sell signal
            } else {
                self.output[IDX_SIGNAL] = 0.0; // No signal
            }
        } else {
            // Still warming up
            self.output = [f64::NAN; OUTPUT_COUNT];
        }

        self.prev_close = close;
        &self.output
    }

    fn reset(&mut self) {
        self.tr_buffer.reset();
        self.atr = 0.0;
        self.prev_close = f64::NAN;
        self.upper_band = 0.0;
        self.lower_band = 0.0;
        self.final_upper = f64::MAX;
        self.final_lower = f64::MIN;
        self.direction_up = true;
        self.prev_direction_up = true;
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

/// Factory for creating Supertrend compute instances.
pub struct SupertrendFactory;

impl IndicatorFactory for SupertrendFactory {
    fn metadata(&self) -> &IndicatorMeta {
        &META
    }

    fn create(&self, params: &[f64]) -> Box<dyn Compute> {
        let period = params
            .first()
            .map(|p| *p as usize)
            .unwrap_or(DEFAULT_PERIOD);
        let multiplier = params.get(1).copied().unwrap_or(DEFAULT_MULTIPLIER);

        Box::new(SupertrendCompute::new(period, multiplier))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_supertrend_metadata() {
        let factory = SupertrendFactory;
        let meta = factory.metadata();
        assert_eq!(meta.id, "supertrend");
        assert_eq!(meta.display, DisplayMode::Overlay);
        assert_eq!(meta.outputs.len(), 3);
        assert_eq!(meta.params.len(), 2);
    }

    #[test]
    fn test_supertrend_output_count() {
        let factory = SupertrendFactory;
        let instance = factory.create(&[10.0, 3.0]);
        assert_eq!(instance.output_count(), 3);
    }

    #[test]
    fn test_supertrend_warmup_period() {
        let factory = SupertrendFactory;
        let mut st = factory.create(&[10.0, 3.0]);

        // First 10 candles should return NaN (warming up ATR)
        for i in 0..10 {
            let price = 25000.0 + (i as f64 * 10.0);
            let out = st.on_candle(price, price + 50.0, price - 50.0, price + 10.0, 1000.0);
            assert!(
                out[IDX_LINE].is_nan(),
                "candle {i} should be NaN during warmup"
            );
        }

        // Candle 11 should have a value
        let out = st.on_candle(25100.0, 25150.0, 25050.0, 25110.0, 1000.0);
        assert!(out[IDX_LINE].is_finite(), "candle 11 should have a value");
    }

    #[test]
    fn test_supertrend_bullish_trend() {
        let factory = SupertrendFactory;
        let mut st = factory.create(&[3.0, 1.0]); // Small period for testing

        // Rising prices
        let prices: &[(f64, f64, f64, f64)] = &[
            (100.0, 110.0, 90.0, 105.0),
            (105.0, 115.0, 95.0, 110.0),
            (110.0, 120.0, 100.0, 115.0),
            (115.0, 125.0, 105.0, 120.0),
            (120.0, 130.0, 110.0, 125.0),
        ];

        let mut last_output = [f64::NAN; 3];
        for &(o, h, l, c) in prices {
            let out = st.on_candle(o, h, l, c, 1000.0);
            last_output.copy_from_slice(out);
        }

        // In a strong uptrend, direction should be bullish
        if last_output[IDX_DIRECTION].is_finite() {
            assert!(
                last_output[IDX_DIRECTION] > 0.0,
                "expected bullish direction in uptrend, got {}",
                last_output[IDX_DIRECTION]
            );
        }
    }

    #[test]
    fn test_supertrend_reset() {
        let factory = SupertrendFactory;
        let mut st = factory.create(&[10.0, 3.0]);

        st.on_candle(100.0, 110.0, 90.0, 105.0, 1000.0);
        st.reset();

        // After reset, should be back to warmup
        let out = st.on_candle(100.0, 110.0, 90.0, 105.0, 1000.0);
        assert!(out[IDX_LINE].is_nan());
    }

    #[test]
    fn test_supertrend_default_params() {
        let factory = SupertrendFactory;
        let instance = factory.create(&[]); // No params — use defaults
        assert_eq!(instance.output_count(), 3);
    }
}
