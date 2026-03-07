//! Squeeze Momentum indicator (LazyBear variant).
//!
//! Detects market compression (Bollinger Bands inside Keltner Channels)
//! and measures momentum via linear regression of price vs midline.
//!
//! # Outputs (2 values per candle)
//! - `[0]` = Momentum value (histogram height)
//! - `[1]` = Squeeze state (1.0 = squeeze on, 0.0 = squeeze off)
//!
//! # Parameters
//! - bb_length: Bollinger Bands period (default: 20)
//! - bb_mult: Bollinger Bands multiplier (default: 2.0)
//! - kc_length: Keltner Channel period (default: 20)
//! - kc_mult: Keltner Channel multiplier (default: 1.5)

use crate::indicator::*;
use crate::registry::IndicatorFactory;
use crate::ring_buffer::RingBuffer;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const OUTPUT_COUNT: usize = 2;
const IDX_MOMENTUM: usize = 0;
const IDX_SQUEEZE: usize = 1;

const DEFAULT_BB_LENGTH: usize = 20;
const DEFAULT_BB_MULT: f64 = 2.0;
const DEFAULT_KC_LENGTH: usize = 20;
const DEFAULT_KC_MULT: f64 = 1.5;

// ---------------------------------------------------------------------------
// Metadata
// ---------------------------------------------------------------------------

static OUTPUTS: &[OutputDef] = &[
    OutputDef {
        name: "Momentum",
        color: "#26a69a",
        style: OutputStyle::Histogram,
    },
    OutputDef {
        name: "Squeeze",
        color: "#ff0000",
        style: OutputStyle::Marker,
    },
];

static PARAMS: &[ParamDef] = &[
    ParamDef {
        name: "BB Length",
        kind: ParamKind::Int(DEFAULT_BB_LENGTH as i64, 1, 200),
    },
    ParamDef {
        name: "BB Mult",
        kind: ParamKind::Float(DEFAULT_BB_MULT, 0.1, 10.0),
    },
    ParamDef {
        name: "KC Length",
        kind: ParamKind::Int(DEFAULT_KC_LENGTH as i64, 1, 200),
    },
    ParamDef {
        name: "KC Mult",
        kind: ParamKind::Float(DEFAULT_KC_MULT, 0.1, 10.0),
    },
];

static META: IndicatorMeta = IndicatorMeta {
    id: "squeeze_momentum",
    display_name: "Squeeze Momentum [LB]",
    display: DisplayMode::Pane,
    outputs: OUTPUTS,
    params: PARAMS,
};

// ---------------------------------------------------------------------------
// Compute
// ---------------------------------------------------------------------------

pub struct SqueezeMomentumCompute {
    bb_length: usize,
    bb_mult: f64,
    kc_length: usize,
    kc_mult: f64,
    /// Close prices for BB/KC calculations.
    close_buffer: RingBuffer,
    /// High prices for KC (True Range).
    high_buffer: RingBuffer,
    /// Low prices for KC (True Range).
    low_buffer: RingBuffer,
    /// True Range values for KC ATR.
    tr_buffer: RingBuffer,
    /// Previous close for TR calculation.
    prev_close: f64,
    candle_count: usize,
    output: [f64; OUTPUT_COUNT],
}

impl SqueezeMomentumCompute {
    fn new(bb_length: usize, bb_mult: f64, kc_length: usize, kc_mult: f64) -> Self {
        let max_len = bb_length.max(kc_length);
        Self {
            bb_length,
            bb_mult,
            kc_length,
            kc_mult,
            close_buffer: RingBuffer::new(max_len),
            high_buffer: RingBuffer::new(max_len),
            low_buffer: RingBuffer::new(max_len),
            tr_buffer: RingBuffer::new(kc_length),
            prev_close: f64::NAN,
            candle_count: 0,
            output: [f64::NAN; OUTPUT_COUNT],
        }
    }

    /// Computes standard deviation of last `n` close values. O(n).
    fn std_dev(&self, n: usize) -> f64 {
        if self.close_buffer.len() < n {
            return f64::NAN;
        }
        let mean = {
            let mut sum = 0.0;
            for i in 0..n {
                sum += self.close_buffer.get(i);
            }
            sum / n as f64
        };
        let mut sum_sq = 0.0;
        for i in 0..n {
            let diff = self.close_buffer.get(i) - mean;
            sum_sq += diff * diff;
        }
        (sum_sq / n as f64).sqrt()
    }

    /// Computes SMA of last `n` close values. O(n).
    fn sma_close(&self, n: usize) -> f64 {
        if self.close_buffer.len() < n {
            return f64::NAN;
        }
        let mut sum = 0.0;
        for i in 0..n {
            sum += self.close_buffer.get(i);
        }
        sum / n as f64
    }

    /// Computes linear regression value (deviation from midline).
    /// This is the momentum measure — simplified to close minus midline of
    /// highest-high and lowest-low channel averaged with SMA.
    fn linear_regression_value(&self) -> f64 {
        if self.close_buffer.len() < self.bb_length {
            return f64::NAN;
        }

        // Highest high and lowest low over KC length
        let mut highest = f64::NEG_INFINITY;
        let mut lowest = f64::INFINITY;
        for i in 0..self.kc_length.min(self.high_buffer.len()) {
            let h = self.high_buffer.get(i);
            let l = self.low_buffer.get(i);
            if h > highest {
                highest = h;
            }
            if l < lowest {
                lowest = l;
            }
        }

        let donchian_mid = (highest + lowest) / 2.0;
        let sma = self.sma_close(self.bb_length);
        let midline = (donchian_mid + sma) / 2.0;

        self.close_buffer.get(0) - midline
    }
}

impl Compute for SqueezeMomentumCompute {
    fn on_candle(&mut self, _open: f64, high: f64, low: f64, close: f64, _volume: f64) -> &[f64] {
        self.candle_count += 1;

        // True Range
        let tr = if self.prev_close.is_nan() {
            high - low
        } else {
            let hl = high - low;
            let hc = (high - self.prev_close).abs();
            let lc = (low - self.prev_close).abs();
            if hl >= hc && hl >= lc {
                hl
            } else if hc >= lc {
                hc
            } else {
                lc
            }
        };

        self.close_buffer.push(close);
        self.high_buffer.push(high);
        self.low_buffer.push(low);
        self.tr_buffer.push(tr);
        self.prev_close = close;

        let max_len = self.bb_length.max(self.kc_length);
        if self.candle_count < max_len {
            self.output = [f64::NAN; OUTPUT_COUNT];
            return &self.output;
        }

        // Bollinger Bands
        let bb_sma = self.sma_close(self.bb_length);
        let bb_std = self.std_dev(self.bb_length);
        let bb_upper = bb_sma + self.bb_mult * bb_std;
        let bb_lower = bb_sma - self.bb_mult * bb_std;

        // Keltner Channel (using SMA + ATR)
        let kc_sma = self.sma_close(self.kc_length);
        let kc_atr = self.tr_buffer.mean();
        let kc_upper = kc_sma + self.kc_mult * kc_atr;
        let kc_lower = kc_sma - self.kc_mult * kc_atr;

        // Squeeze detection: BB inside KC
        let squeeze_on = bb_lower > kc_lower && bb_upper < kc_upper;

        // Momentum: linear regression value
        let momentum = self.linear_regression_value();

        self.output[IDX_MOMENTUM] = momentum;
        self.output[IDX_SQUEEZE] = if squeeze_on { 1.0 } else { 0.0 };

        &self.output
    }

    fn reset(&mut self) {
        self.close_buffer.reset();
        self.high_buffer.reset();
        self.low_buffer.reset();
        self.tr_buffer.reset();
        self.prev_close = f64::NAN;
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

pub struct SqueezeMomentumFactory;

impl IndicatorFactory for SqueezeMomentumFactory {
    fn metadata(&self) -> &IndicatorMeta {
        &META
    }

    fn create(&self, params: &[f64]) -> Box<dyn Compute> {
        let bb_length = params
            .first()
            .map(|p| *p as usize)
            .unwrap_or(DEFAULT_BB_LENGTH);
        let bb_mult = params.get(1).copied().unwrap_or(DEFAULT_BB_MULT);
        let kc_length = params
            .get(2)
            .map(|p| *p as usize)
            .unwrap_or(DEFAULT_KC_LENGTH);
        let kc_mult = params.get(3).copied().unwrap_or(DEFAULT_KC_MULT);

        Box::new(SqueezeMomentumCompute::new(
            bb_length, bb_mult, kc_length, kc_mult,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_squeeze_metadata() {
        let factory = SqueezeMomentumFactory;
        let meta = factory.metadata();
        assert_eq!(meta.id, "squeeze_momentum");
        assert_eq!(meta.display, DisplayMode::Pane);
        assert_eq!(meta.outputs.len(), 2);
        assert_eq!(meta.params.len(), 4);
    }

    #[test]
    fn test_squeeze_warmup() {
        let factory = SqueezeMomentumFactory;
        let mut sq = factory.create(&[20.0, 2.0, 20.0, 1.5]);

        for i in 0..19 {
            let price = 25000.0 + (i as f64 * 5.0);
            let out = sq.on_candle(price, price + 20.0, price - 20.0, price, 1000.0);
            assert!(out[IDX_MOMENTUM].is_nan(), "candle {i} should be NaN");
        }
    }

    #[test]
    fn test_squeeze_produces_values_after_warmup() {
        let factory = SqueezeMomentumFactory;
        let mut sq = factory.create(&[5.0, 2.0, 5.0, 1.5]); // Short period

        for i in 0..10 {
            let price = 25000.0 + (i as f64 * 10.0);
            sq.on_candle(price, price + 30.0, price - 30.0, price + 5.0, 1000.0);
        }

        let out = sq.on_candle(25100.0, 25130.0, 25070.0, 25110.0, 1000.0);
        assert!(
            out[IDX_MOMENTUM].is_finite(),
            "momentum should be finite after warmup"
        );
    }

    #[test]
    fn test_squeeze_reset() {
        let factory = SqueezeMomentumFactory;
        let mut sq = factory.create(&[5.0, 2.0, 5.0, 1.5]);

        for i in 0..10 {
            let price = 25000.0 + (i as f64 * 10.0);
            sq.on_candle(price, price + 30.0, price - 30.0, price, 1000.0);
        }

        sq.reset();
        let out = sq.on_candle(25000.0, 25020.0, 24980.0, 25010.0, 1000.0);
        assert!(out[IDX_MOMENTUM].is_nan(), "should be NaN after reset");
    }
}
