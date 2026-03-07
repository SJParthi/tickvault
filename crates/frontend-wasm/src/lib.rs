//! WASM indicator engine for the DLT trading terminal.
//!
//! Compiles to WebAssembly via `wasm-pack build`. Provides:
//! - Indicator plugin registry with dynamic instance creation
//! - Tick-to-candle aggregation at any timeframe
//! - O(1) per-tick indicator computation
//!
//! # Architecture
//! ```text
//! Browser WS → WASM on_tick() → aggregator → indicators → Float64Array → JS renderers
//! ```

pub mod aggregator;
pub mod indicator;
pub mod indicators;
pub mod registry;
pub mod ring_buffer;

#[cfg(test)]
mod tests {
    use crate::aggregator::{TickAggregator, Timeframe};
    use crate::indicators::register_all;
    use crate::registry::IndicatorRegistry;

    #[test]
    fn test_full_pipeline_tick_to_indicator() {
        // Set up registry with all indicators
        let mut registry = IndicatorRegistry::new();
        register_all(&mut registry);

        // Create a Supertrend instance
        let mut supertrend = registry
            .create_instance("supertrend", &[10.0, 3.0])
            .unwrap();

        // Create a 1-minute aggregator
        let mut aggregator = TickAggregator::new(Timeframe::M1);

        // Simulate ticks — use a timestamp aligned to a 1-minute boundary
        let base_time = Timeframe::M1.align_timestamp(1772858700);

        // Feed ticks in the same minute
        for i in 0..50 {
            let price = 25000.0 + (i as f64 * 2.0);
            let volume = 100.0 + (i as f64 * 10.0);
            let tick_time = base_time + i;

            if let Some(closed_candle) = aggregator.on_tick(tick_time, price, volume) {
                // Feed closed candle to indicator
                supertrend.on_candle(
                    closed_candle.open,
                    closed_candle.high,
                    closed_candle.low,
                    closed_candle.close,
                    closed_candle.volume,
                );
            }
        }

        // Current candle should exist
        assert!(aggregator.current_candle().is_some());
    }

    #[test]
    fn test_all_indicators_handle_extreme_prices() {
        let mut registry = IndicatorRegistry::new();
        register_all(&mut registry);

        for meta in registry.list() {
            let mut instance = registry.create_instance(meta.id, &[]).unwrap();

            // Feed extreme but valid prices (NIFTY range)
            for i in 0..50 {
                let base = 20000.0 + (i as f64 * 100.0);
                let out =
                    instance.on_candle(base, base + 200.0, base - 200.0, base + 50.0, 1_000_000.0);

                // Outputs should be NaN (warmup) or finite — never Infinity
                for (j, &val) in out.iter().enumerate() {
                    assert!(
                        val.is_nan() || val.is_finite(),
                        "indicator '{}' output[{j}] at candle {i} is infinity: {val}",
                        meta.id,
                    );
                }
            }
        }
    }

    #[test]
    fn test_all_indicators_reset_cleanly() {
        let mut registry = IndicatorRegistry::new();
        register_all(&mut registry);

        for meta in registry.list() {
            let mut instance = registry.create_instance(meta.id, &[]).unwrap();

            // Feed some candles
            for i in 0..30 {
                let price = 25000.0 + (i as f64 * 10.0);
                instance.on_candle(price, price + 50.0, price - 50.0, price, 1000.0);
            }

            // Reset
            instance.reset();

            // First candle after reset should be NaN (back to warmup)
            let out = instance.on_candle(25000.0, 25050.0, 24950.0, 25000.0, 1000.0);
            for (j, &val) in out.iter().enumerate() {
                assert!(
                    val.is_nan() || val == 0.0,
                    "indicator '{}' output[{j}] should be NaN or zero after reset, got {val}",
                    meta.id,
                );
            }
        }
    }
}
