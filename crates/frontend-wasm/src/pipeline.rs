//! Pipeline orchestrator — registry + aggregator + active indicator instances.
//!
//! Manages the full tick → candle → indicator computation flow.
//! All hot-path operations (on_tick) are O(1) per active indicator.

use crate::aggregator::{Candle, TickAggregator, Timeframe};
use crate::indicator::{Compute, DisplayMode, OutputStyle, ParamKind};
use crate::indicators;
use crate::registry::IndicatorRegistry;

// ---------------------------------------------------------------------------
// Active Indicator
// ---------------------------------------------------------------------------

/// An active indicator instance with cached outputs.
pub(crate) struct ActiveIndicator {
    pub(crate) id: String,
    pub(crate) instance_id: u32,
    pub(crate) compute: Box<dyn Compute>,
    pub(crate) latest_outputs: Vec<f64>,
}

// ---------------------------------------------------------------------------
// Pipeline
// ---------------------------------------------------------------------------

/// Orchestrates tick → candle aggregation → indicator computation.
///
/// # Performance
/// - `on_tick()`: O(n) where n = active indicators (each is O(1))
/// - `add_indicator()`/`remove_indicator()`: not on hot path
/// - `list_indicators_json()`: not on hot path (UI only)
pub(crate) struct Pipeline {
    registry: IndicatorRegistry,
    aggregator: TickAggregator,
    active: Vec<ActiveIndicator>,
    next_id: u32,
    last_closed: Option<Candle>,
}

impl Pipeline {
    /// Creates a new pipeline with all indicators registered and 1-minute default.
    pub(crate) fn new() -> Self {
        let mut registry = IndicatorRegistry::new();
        indicators::register_all(&mut registry);
        Self {
            registry,
            aggregator: TickAggregator::new(Timeframe::M1),
            active: Vec::new(),
            next_id: 1,
            last_closed: None,
        }
    }

    /// Returns JSON array of indicator metadata for the UI picker.
    pub(crate) fn list_indicators_json(&self) -> String {
        let metas = self.registry.list();
        let mut json = String::with_capacity(2048);
        json.push('[');
        for (i, meta) in metas.iter().enumerate() {
            if i > 0 {
                json.push(',');
            }
            json.push('{');

            // Core fields
            let display = match meta.display {
                DisplayMode::Overlay => "overlay",
                DisplayMode::Pane => "pane",
            };
            json.push_str(&format!(
                r#""id":"{}","display_name":"{}","display":"{}""#,
                meta.id, meta.display_name, display
            ));

            // Outputs
            json.push_str(r#","outputs":["#);
            for (j, out) in meta.outputs.iter().enumerate() {
                if j > 0 {
                    json.push(',');
                }
                let style = match out.style {
                    OutputStyle::Line => "line",
                    OutputStyle::Histogram => "histogram",
                    OutputStyle::StepLine => "stepline",
                    OutputStyle::Marker => "marker",
                    OutputStyle::Band => "band",
                    OutputStyle::Rectangle => "rectangle",
                };
                json.push_str(&format!(
                    r#"{{"name":"{}","color":"{}","style":"{}"}}"#,
                    out.name, out.color, style
                ));
            }
            json.push(']');

            // Params
            json.push_str(r#","params":["#);
            for (j, param) in meta.params.iter().enumerate() {
                if j > 0 {
                    json.push(',');
                }
                match param.kind {
                    ParamKind::Int(default, min, max) => {
                        json.push_str(&format!(
                            r#"{{"name":"{}","kind":"int","default":{},"min":{},"max":{}}}"#,
                            param.name, default, min, max
                        ));
                    }
                    ParamKind::Float(default, min, max) => {
                        json.push_str(&format!(
                            r#"{{"name":"{}","kind":"float","default":{},"min":{},"max":{}}}"#,
                            param.name, default, min, max
                        ));
                    }
                    ParamKind::Bool(default) => {
                        json.push_str(&format!(
                            r#"{{"name":"{}","kind":"bool","default":{}}}"#,
                            param.name, default
                        ));
                    }
                }
            }
            json.push_str("]}");
        }
        json.push(']');
        json
    }

    /// Adds an indicator instance. Returns instance ID or None if not found.
    pub(crate) fn add_indicator(&mut self, id: &str, params: &[f64]) -> Option<u32> {
        let compute = self.registry.create_instance(id, params)?;
        let output_count = compute.output_count();
        let instance_id = self.next_id;
        self.next_id += 1;
        self.active.push(ActiveIndicator {
            id: id.to_owned(),
            instance_id,
            compute,
            latest_outputs: vec![f64::NAN; output_count],
        });
        Some(instance_id)
    }

    /// Removes an indicator instance by ID. Returns true if found.
    pub(crate) fn remove_indicator(&mut self, instance_id: u32) -> bool {
        if let Some(pos) = self
            .active
            .iter()
            .position(|a| a.instance_id == instance_id)
        {
            self.active.remove(pos);
            true
        } else {
            false
        }
    }

    /// Processes a tick. Returns true if a candle closed.
    ///
    /// When true, all active indicators have been updated with the closed candle.
    /// Query outputs via `indicator_output()`.
    ///
    /// # Performance
    /// O(n) where n = active indicators. Each indicator is O(1) by contract.
    pub(crate) fn on_tick(&mut self, epoch_secs: u64, price: f64, volume: f64) -> bool {
        let closed = self.aggregator.on_tick(epoch_secs, price, volume);
        if let Some(candle) = closed {
            for ai in &mut self.active {
                let outputs = ai.compute.on_candle(
                    candle.open,
                    candle.high,
                    candle.low,
                    candle.close,
                    candle.volume,
                );
                ai.latest_outputs.copy_from_slice(outputs);
            }
            self.last_closed = Some(candle);
            true
        } else {
            false
        }
    }

    /// Returns the last closed candle (test-only).
    #[cfg(test)]
    pub(crate) fn last_closed_candle(&self) -> Option<&Candle> {
        self.last_closed.as_ref()
    }

    /// Returns the current (building) candle as [time, open, high, low, close, volume].
    pub(crate) fn current_candle_values(&self) -> Option<[f64; 6]> {
        self.aggregator
            .current_candle()
            .map(|c| [c.time as f64, c.open, c.high, c.low, c.close, c.volume])
    }

    /// Returns the closed candle as [time, open, high, low, close, volume].
    pub(crate) fn closed_candle_values(&self) -> Option<[f64; 6]> {
        self.last_closed
            .map(|c| [c.time as f64, c.open, c.high, c.low, c.close, c.volume])
    }

    /// Returns outputs for a specific active indicator instance.
    pub(crate) fn indicator_output(&self, instance_id: u32) -> Option<&[f64]> {
        self.active
            .iter()
            .find(|a| a.instance_id == instance_id)
            .map(|a| a.latest_outputs.as_slice())
    }

    /// Returns the indicator ID string for a given instance.
    pub(crate) fn indicator_id(&self, instance_id: u32) -> Option<&str> {
        self.active
            .iter()
            .find(|a| a.instance_id == instance_id)
            .map(|a| a.id.as_str())
    }

    /// Returns all active instance IDs.
    pub(crate) fn active_instance_ids(&self) -> Vec<u32> {
        self.active.iter().map(|a| a.instance_id).collect()
    }

    /// Number of active indicator instances.
    pub(crate) fn active_count(&self) -> usize {
        self.active.len()
    }

    /// Changes the timeframe and resets all state.
    pub(crate) fn set_timeframe(&mut self, seconds: u64) {
        if let Some(tf) = Timeframe::from_seconds(seconds) {
            self.aggregator.set_timeframe(tf);
            for ai in &mut self.active {
                ai.compute.reset();
                for v in &mut ai.latest_outputs {
                    *v = f64::NAN;
                }
            }
            self.last_closed = None;
        }
    }

    /// Resets the aggregator and all indicator state.
    pub(crate) fn reset(&mut self) {
        self.aggregator.reset();
        for ai in &mut self.active {
            ai.compute.reset();
            for v in &mut ai.latest_outputs {
                *v = f64::NAN;
            }
        }
        self.last_closed = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aggregator::Timeframe;

    #[test]
    fn test_pipeline_add_remove_indicators() {
        let mut pipeline = Pipeline::new();

        let id1 = pipeline.add_indicator("supertrend", &[10.0, 3.0]);
        assert!(id1.is_some());
        assert_eq!(pipeline.active_count(), 1);

        let id2 = pipeline.add_indicator("macd_mtf", &[]);
        assert!(id2.is_some());
        assert_eq!(pipeline.active_count(), 2);

        assert!(pipeline.remove_indicator(id1.unwrap()));
        assert_eq!(pipeline.active_count(), 1);

        assert!(!pipeline.remove_indicator(999));
    }

    #[test]
    fn test_pipeline_on_tick_flow() {
        let mut pipeline = Pipeline::new();
        pipeline.add_indicator("supertrend", &[10.0, 3.0]);

        let base = Timeframe::M1.align_timestamp(1772858700);

        // Feed ticks in same minute — no candle close
        for i in 0..30 {
            let closed = pipeline.on_tick(base + i, 25000.0 + (i as f64), 1000.0);
            assert!(!closed);
        }

        // Tick in next minute — closes previous candle
        let closed = pipeline.on_tick(base + 61, 25050.0, 2000.0);
        assert!(closed);
        assert!(pipeline.last_closed_candle().is_some());

        // Check indicator output exists
        let ids = pipeline.active_instance_ids();
        let output = pipeline.indicator_output(ids[0]);
        assert!(output.is_some());
    }

    #[test]
    fn test_pipeline_set_timeframe_resets() {
        let mut pipeline = Pipeline::new();
        pipeline.add_indicator("supertrend", &[]);

        let base = Timeframe::M1.align_timestamp(1772858700);
        pipeline.on_tick(base, 25000.0, 1000.0);
        pipeline.on_tick(base + 61, 25050.0, 2000.0);

        pipeline.set_timeframe(300); // 5 min
        assert!(pipeline.last_closed_candle().is_none());
        assert!(pipeline.current_candle_values().is_none());
    }

    #[test]
    fn test_pipeline_nonexistent_indicator() {
        let mut pipeline = Pipeline::new();
        let result = pipeline.add_indicator("nonexistent", &[]);
        assert!(result.is_none());
        assert_eq!(pipeline.active_count(), 0);
    }

    #[test]
    fn test_pipeline_indicator_id_lookup() {
        let mut pipeline = Pipeline::new();
        let id = pipeline.add_indicator("supertrend", &[]).unwrap();
        assert_eq!(pipeline.indicator_id(id), Some("supertrend"));
        assert_eq!(pipeline.indicator_id(999), None);
    }

    #[test]
    fn test_pipeline_list_indicators_json() {
        let pipeline = Pipeline::new();
        let json = pipeline.list_indicators_json();
        assert!(json.starts_with('['));
        assert!(json.ends_with(']'));
        assert!(json.contains("supertrend"));
        assert!(json.contains("smc"));
        assert!(json.contains("squeeze_momentum"));
        assert!(json.contains("macd_mtf"));
    }

    #[test]
    fn test_pipeline_multiple_indicators_compute() {
        let mut pipeline = Pipeline::new();
        pipeline.add_indicator("supertrend", &[10.0, 3.0]);
        pipeline.add_indicator("macd_mtf", &[]);

        let base = Timeframe::M1.align_timestamp(1772858700);

        // Feed enough ticks to produce candles
        for minute in 0..20 {
            let t = base + (minute * 60) + 30;
            let price = 25000.0 + (minute as f64 * 10.0);
            pipeline.on_tick(t, price, 1000.0);

            if minute > 0 {
                // Each new minute closes the previous candle
                let t2 = base + (minute * 60) + 1;
                pipeline.on_tick(t2, price + 5.0, 1100.0);
            }
        }

        // Both indicators should have outputs
        let ids = pipeline.active_instance_ids();
        assert_eq!(ids.len(), 2);
        for id in &ids {
            let output = pipeline.indicator_output(*id);
            assert!(output.is_some());
            assert!(!output.unwrap().is_empty());
        }
    }
}
