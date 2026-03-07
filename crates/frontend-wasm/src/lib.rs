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

use wasm_bindgen::prelude::*;

pub mod aggregator;
pub mod indicator;
pub mod indicators;
pub(crate) mod pipeline;
pub mod registry;
pub mod ring_buffer;

// ---------------------------------------------------------------------------
// WASM Engine — JS-callable interface
// ---------------------------------------------------------------------------

/// Main WASM engine exposed to JavaScript via wasm_bindgen.
///
/// Wraps the internal pipeline: registry + aggregator + active indicators.
/// All hot-path operations (on_tick) are O(1) per active indicator.
#[wasm_bindgen]
pub struct WasmEngine {
    inner: pipeline::Pipeline,
}

#[wasm_bindgen]
impl WasmEngine {
    /// Creates a new engine with all indicators registered.
    #[wasm_bindgen(constructor)]
    pub fn new() -> Self {
        Self {
            inner: pipeline::Pipeline::new(),
        }
    }

    /// Returns JSON array of available indicator metadata for the UI picker.
    pub fn list_indicators(&self) -> String {
        self.inner.list_indicators_json()
    }

    /// Adds an indicator instance. Returns instance ID (>= 0) or -1 if not found.
    pub fn add_indicator(&mut self, id: &str, params: &[f64]) -> i32 {
        match self.inner.add_indicator(id, params) {
            Some(iid) => iid as i32,
            None => -1,
        }
    }

    /// Removes an indicator instance by ID. Returns true if found.
    pub fn remove_indicator(&mut self, instance_id: u32) -> bool {
        self.inner.remove_indicator(instance_id)
    }

    /// Processes a tick. Returns true if a candle closed.
    ///
    /// When true, call `closed_candle()` and `indicator_output()` to get results.
    ///
    /// # Parameters
    /// - `epoch_secs`: UTC epoch seconds (f64 for JS compatibility)
    /// - `price`: Last traded price
    /// - `volume`: Cumulative volume
    pub fn on_tick(&mut self, epoch_secs: f64, price: f64, volume: f64) -> bool {
        self.inner.on_tick(epoch_secs as u64, price, volume)
    }

    /// Returns the last closed candle as [time, open, high, low, close, volume].
    /// Empty array if no candle has closed yet.
    pub fn closed_candle(&self) -> Box<[f64]> {
        match self.inner.closed_candle_values() {
            Some(arr) => Box::new(arr),
            None => Box::new([]),
        }
    }

    /// Returns the current (building) candle as [time, open, high, low, close, volume].
    /// Empty array if no tick received yet.
    pub fn current_candle(&self) -> Box<[f64]> {
        match self.inner.current_candle_values() {
            Some(arr) => Box::new(arr),
            None => Box::new([]),
        }
    }

    /// Returns outputs for a specific active indicator instance.
    /// Empty array if instance not found.
    pub fn indicator_output(&self, instance_id: u32) -> Box<[f64]> {
        match self.inner.indicator_output(instance_id) {
            Some(outputs) => outputs.into(),
            None => Box::new([]),
        }
    }

    /// Returns the indicator type ID for a given instance.
    pub fn indicator_id(&self, instance_id: u32) -> Option<String> {
        self.inner.indicator_id(instance_id).map(|s| s.to_owned())
    }

    /// Returns all active instance IDs.
    pub fn active_instance_ids(&self) -> Box<[u32]> {
        self.inner.active_instance_ids().into_boxed_slice()
    }

    /// Number of active indicator instances.
    pub fn active_count(&self) -> u32 {
        self.inner.active_count() as u32
    }

    /// Changes the aggregation timeframe (in seconds). Resets all state.
    pub fn set_timeframe(&mut self, seconds: u32) {
        self.inner.set_timeframe(seconds as u64);
    }

    /// Resets all state (aggregator + indicators).
    pub fn reset(&mut self) {
        self.inner.reset();
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use crate::aggregator::{TickAggregator, Timeframe};
    use crate::indicators::register_all;
    use crate::pipeline::Pipeline;
    use crate::registry::IndicatorRegistry;

    #[test]
    fn test_full_pipeline_tick_to_indicator() {
        let mut registry = IndicatorRegistry::new();
        register_all(&mut registry);

        let mut supertrend = registry
            .create_instance("supertrend", &[10.0, 3.0])
            .unwrap();

        let mut aggregator = TickAggregator::new(Timeframe::M1);
        let base_time = Timeframe::M1.align_timestamp(1772858700);

        for i in 0..50 {
            let price = 25000.0 + (i as f64 * 2.0);
            let volume = 100.0 + (i as f64 * 10.0);
            let tick_time = base_time + i;

            if let Some(closed_candle) = aggregator.on_tick(tick_time, price, volume) {
                supertrend.on_candle(
                    closed_candle.open,
                    closed_candle.high,
                    closed_candle.low,
                    closed_candle.close,
                    closed_candle.volume,
                );
            }
        }

        assert!(aggregator.current_candle().is_some());
    }

    #[test]
    fn test_all_indicators_handle_extreme_prices() {
        let mut registry = IndicatorRegistry::new();
        register_all(&mut registry);

        for meta in registry.list() {
            let mut instance = registry.create_instance(meta.id, &[]).unwrap();

            for i in 0..50 {
                let base = 20000.0 + (i as f64 * 100.0);
                let out =
                    instance.on_candle(base, base + 200.0, base - 200.0, base + 50.0, 1_000_000.0);

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

            for i in 0..30 {
                let price = 25000.0 + (i as f64 * 10.0);
                instance.on_candle(price, price + 50.0, price - 50.0, price, 1000.0);
            }

            instance.reset();

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

    #[test]
    fn test_wasm_engine_api() {
        let mut engine = super::WasmEngine::new();

        let json = engine.list_indicators();
        assert!(json.contains("supertrend"));

        let id = engine.add_indicator("supertrend", &[10.0, 3.0]);
        assert!(id >= 0);
        assert_eq!(engine.active_count(), 1);

        let bad = engine.add_indicator("nonexistent", &[]);
        assert_eq!(bad, -1);

        let base = Timeframe::M1.align_timestamp(1772858700) as f64;
        for i in 0..30 {
            let closed = engine.on_tick(base + i as f64, 25000.0 + i as f64, 1000.0);
            assert!(!closed);
        }

        let closed = engine.on_tick(base + 61.0, 25050.0, 2000.0);
        assert!(closed);

        let candle = engine.closed_candle();
        assert_eq!(candle.len(), 6);

        let output = engine.indicator_output(id as u32);
        assert!(!output.is_empty());

        assert!(engine.remove_indicator(id as u32));
        assert_eq!(engine.active_count(), 0);
    }

    #[test]
    fn test_pipeline_integration() {
        let mut pipeline = Pipeline::new();
        pipeline.add_indicator("supertrend", &[10.0, 3.0]);
        pipeline.add_indicator("macd_mtf", &[]);

        let base = Timeframe::M1.align_timestamp(1772858700);

        for minute in 0..35 {
            let t = base + (minute * 60);
            let price = 25000.0 + (minute as f64 * 10.0);
            pipeline.on_tick(t + 1, price, 1000.0);
            pipeline.on_tick(t + 30, price + 20.0, 1500.0);
        }

        let ids = pipeline.active_instance_ids();
        assert_eq!(ids.len(), 2);

        for id in &ids {
            let output = pipeline.indicator_output(*id);
            assert!(output.is_some());
        }
    }
}
