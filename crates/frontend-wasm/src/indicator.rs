//! Indicator trait and metadata types.
//!
//! Every indicator implements `Compute`. The trait enforces O(1) per-candle
//! processing via pre-allocated output buffers.

// ---------------------------------------------------------------------------
// Display Mode
// ---------------------------------------------------------------------------

/// Where the indicator renders on the chart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisplayMode {
    /// Drawn on the main price chart (e.g., Supertrend, moving averages).
    Overlay,
    /// Drawn in a separate pane below the chart (e.g., MACD, RSI).
    Pane,
}

// ---------------------------------------------------------------------------
// Output Definition
// ---------------------------------------------------------------------------

/// Describes a single output series from an indicator.
#[derive(Debug, Clone)]
pub struct OutputDef {
    /// Human-readable name (e.g., "MACD Line", "Signal").
    pub name: &'static str,
    /// CSS color string (e.g., "#26a69a", "rgba(255,0,0,0.5)").
    pub color: &'static str,
    /// Rendering style hint.
    pub style: OutputStyle,
}

/// How a single output series should be rendered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputStyle {
    /// Continuous line (e.g., moving average, MACD line).
    Line,
    /// Filled histogram bars (e.g., volume, squeeze momentum).
    Histogram,
    /// Step line that changes color (e.g., Supertrend).
    StepLine,
    /// Markers/labels at specific points (e.g., Buy/Sell signals).
    Marker,
    /// Shaded band between two values (e.g., Bollinger Bands).
    Band,
    /// Rectangular zones (e.g., order blocks, FVGs).
    Rectangle,
}

// ---------------------------------------------------------------------------
// Parameter Definition
// ---------------------------------------------------------------------------

/// Describes a user-configurable parameter for an indicator.
#[derive(Debug, Clone)]
pub struct ParamDef {
    /// Parameter name shown in UI (e.g., "Period", "Multiplier").
    pub name: &'static str,
    /// Parameter type and constraints.
    pub kind: ParamKind,
}

/// Parameter type with default and bounds.
#[derive(Debug, Clone, Copy)]
pub enum ParamKind {
    /// Integer parameter with (default, min, max).
    Int(i64, i64, i64),
    /// Float parameter with (default, min, max).
    Float(f64, f64, f64),
    /// Boolean toggle with default.
    Bool(bool),
}

// ---------------------------------------------------------------------------
// Indicator Metadata
// ---------------------------------------------------------------------------

/// Static metadata describing an indicator's capabilities and UI requirements.
///
/// Returned by each indicator's factory function. Used by the UI to build
/// the indicator picker dropdown and parameter editor.
#[derive(Debug, Clone)]
pub struct IndicatorMeta {
    /// Unique identifier (e.g., "supertrend", "smc", "squeeze_momentum").
    pub id: &'static str,
    /// Human-readable display name (e.g., "Supertrend", "Smart Money Concepts").
    pub display_name: &'static str,
    /// Where this indicator renders.
    pub display: DisplayMode,
    /// Output series definitions (determines renderer behavior).
    pub outputs: &'static [OutputDef],
    /// User-configurable parameters.
    pub params: &'static [ParamDef],
}

// ---------------------------------------------------------------------------
// Compute Trait
// ---------------------------------------------------------------------------

/// Core computation trait for all indicators.
///
/// # O(1) Contract
/// Implementations MUST process each candle in O(1) time and space.
/// Use fixed-size ring buffers for lookback windows. No heap allocation
/// in `on_candle()`. No Vec growth. No HashMap lookups.
///
/// # Output Buffer
/// `on_candle()` returns a slice into a pre-allocated buffer owned by
/// the indicator instance. The slice length equals `output_count()` and
/// remains constant for the lifetime of the instance.
pub trait Compute {
    /// Process one OHLCV candle and return computed output values.
    ///
    /// # Returns
    /// Slice of `output_count()` f64 values. NaN indicates "no value yet"
    /// (indicator still warming up / insufficient lookback data).
    ///
    /// # Performance
    /// MUST be O(1). Pre-allocate all buffers in `new()`.
    fn on_candle(&mut self, open: f64, high: f64, low: f64, close: f64, volume: f64) -> &[f64];

    /// Reset all internal state (called on symbol or timeframe change).
    fn reset(&mut self);

    /// Number of output values per candle (fixed at creation time).
    fn output_count(&self) -> usize;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_display_mode_equality() {
        assert_eq!(DisplayMode::Overlay, DisplayMode::Overlay);
        assert_ne!(DisplayMode::Overlay, DisplayMode::Pane);
    }

    #[test]
    fn test_output_style_equality() {
        assert_eq!(OutputStyle::Line, OutputStyle::Line);
        assert_ne!(OutputStyle::Line, OutputStyle::Histogram);
    }

    #[test]
    fn test_param_kind_int_values() {
        let p = ParamKind::Int(10, 1, 100);
        match p {
            ParamKind::Int(default, min, max) => {
                assert_eq!(default, 10);
                assert_eq!(min, 1);
                assert_eq!(max, 100);
            }
            _ => panic!("expected Int"),
        }
    }

    #[test]
    fn test_param_kind_float_values() {
        let p = ParamKind::Float(3.0, 0.1, 10.0);
        match p {
            ParamKind::Float(default, min, max) => {
                assert!((default - 3.0).abs() < f64::EPSILON);
                assert!((min - 0.1).abs() < f64::EPSILON);
                assert!((max - 10.0).abs() < f64::EPSILON);
            }
            _ => panic!("expected Float"),
        }
    }

    #[test]
    fn test_param_kind_bool_default() {
        let p = ParamKind::Bool(true);
        match p {
            ParamKind::Bool(default) => assert!(default),
            _ => panic!("expected Bool"),
        }
    }

    #[test]
    fn test_indicator_meta_construction() {
        static OUTPUTS: &[OutputDef] = &[OutputDef {
            name: "value",
            color: "#26a69a",
            style: OutputStyle::Line,
        }];
        static PARAMS: &[ParamDef] = &[ParamDef {
            name: "period",
            kind: ParamKind::Int(14, 1, 200),
        }];
        let meta = IndicatorMeta {
            id: "test",
            display_name: "Test Indicator",
            display: DisplayMode::Pane,
            outputs: OUTPUTS,
            params: PARAMS,
        };
        assert_eq!(meta.id, "test");
        assert_eq!(meta.display, DisplayMode::Pane);
        assert_eq!(meta.outputs.len(), 1);
        assert_eq!(meta.params.len(), 1);
    }
}
