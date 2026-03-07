//! Indicator plugin registry.
//!
//! Stores indicator factories by name. O(1) lookup via HashMap.
//! Each factory creates independent compute instances with their own state.

use std::collections::HashMap;

use crate::indicator::{Compute, IndicatorMeta};

// ---------------------------------------------------------------------------
// Factory Trait
// ---------------------------------------------------------------------------

/// Factory for creating indicator compute instances.
///
/// Each call to `create()` produces an independent instance with its own
/// pre-allocated buffers and state. Multiple instances of the same indicator
/// (e.g., Supertrend with different periods) are fully independent.
pub trait IndicatorFactory {
    /// Returns static metadata for the UI (name, outputs, params).
    fn metadata(&self) -> &IndicatorMeta;

    /// Creates a new compute instance with the given parameters.
    ///
    /// # Parameters
    /// `params` slice matches the order of `metadata().params`. If shorter
    /// than expected, missing params use their defaults.
    fn create(&self, params: &[f64]) -> Box<dyn Compute>;
}

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

/// Central registry of all available indicators.
///
/// # Performance
/// - `register()`: O(1) amortized (HashMap insert, startup only)
/// - `get()`: O(1) (HashMap lookup)
/// - `list()`: O(n) (collects metadata, UI only)
pub struct IndicatorRegistry {
    factories: HashMap<&'static str, Box<dyn IndicatorFactory>>,
}

impl IndicatorRegistry {
    /// Creates an empty registry.
    pub fn new() -> Self {
        Self {
            factories: HashMap::new(),
        }
    }

    /// Registers an indicator factory by its unique ID.
    ///
    /// # Panics (debug only)
    /// Debug-asserts that the ID is not already registered.
    pub fn register(&mut self, factory: Box<dyn IndicatorFactory>) {
        let id = factory.metadata().id;
        debug_assert!(
            !self.factories.contains_key(id),
            "indicator already registered: {id}"
        );
        self.factories.insert(id, factory);
    }

    /// Returns the factory for the given indicator ID.
    pub fn get(&self, id: &str) -> Option<&dyn IndicatorFactory> {
        self.factories.get(id).map(|f| f.as_ref())
    }

    /// Creates a compute instance for the given indicator ID with params.
    pub fn create_instance(&self, id: &str, params: &[f64]) -> Option<Box<dyn Compute>> {
        self.factories.get(id).map(|f| f.create(params))
    }

    /// Returns metadata for all registered indicators (for UI picker).
    pub fn list(&self) -> Vec<&IndicatorMeta> {
        self.factories.values().map(|f| f.metadata()).collect()
    }

    /// Number of registered indicators.
    pub fn count(&self) -> usize {
        self.factories.len()
    }
}

impl Default for IndicatorRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::indicator::*;

    // --- Test indicator for registry tests ---

    struct TestCompute {
        output: [f64; 1],
        count: usize,
    }

    impl Compute for TestCompute {
        fn on_candle(&mut self, _o: f64, _h: f64, _l: f64, close: f64, _v: f64) -> &[f64] {
            self.count += 1;
            self.output[0] = close;
            &self.output
        }
        fn reset(&mut self) {
            self.count = 0;
            self.output[0] = f64::NAN;
        }
        fn output_count(&self) -> usize {
            1
        }
    }

    static TEST_OUTPUTS: &[OutputDef] = &[OutputDef {
        name: "value",
        color: "#ffffff",
        style: OutputStyle::Line,
    }];

    static TEST_PARAMS: &[ParamDef] = &[ParamDef {
        name: "period",
        kind: ParamKind::Int(14, 1, 200),
    }];

    static TEST_META: IndicatorMeta = IndicatorMeta {
        id: "test_indicator",
        display_name: "Test",
        display: DisplayMode::Pane,
        outputs: TEST_OUTPUTS,
        params: TEST_PARAMS,
    };

    struct TestFactory;

    impl IndicatorFactory for TestFactory {
        fn metadata(&self) -> &IndicatorMeta {
            &TEST_META
        }
        fn create(&self, _params: &[f64]) -> Box<dyn Compute> {
            Box::new(TestCompute {
                output: [f64::NAN],
                count: 0,
            })
        }
    }

    #[test]
    fn test_empty_registry() {
        let reg = IndicatorRegistry::new();
        assert_eq!(reg.count(), 0);
        assert!(reg.list().is_empty());
        assert!(reg.get("nonexistent").is_none());
    }

    #[test]
    fn test_register_and_get() {
        let mut reg = IndicatorRegistry::new();
        reg.register(Box::new(TestFactory));
        assert_eq!(reg.count(), 1);
        assert!(reg.get("test_indicator").is_some());
        assert!(reg.get("nonexistent").is_none());
    }

    #[test]
    fn test_create_instance() {
        let mut reg = IndicatorRegistry::new();
        reg.register(Box::new(TestFactory));

        let mut instance = reg.create_instance("test_indicator", &[14.0]).unwrap();
        assert_eq!(instance.output_count(), 1);

        let output = instance.on_candle(100.0, 105.0, 95.0, 102.0, 1000.0);
        assert!((output[0] - 102.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_create_instance_nonexistent() {
        let reg = IndicatorRegistry::new();
        assert!(reg.create_instance("nonexistent", &[]).is_none());
    }

    #[test]
    fn test_list_metadata() {
        let mut reg = IndicatorRegistry::new();
        reg.register(Box::new(TestFactory));

        let list = reg.list();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].id, "test_indicator");
        assert_eq!(list[0].display_name, "Test");
    }

    #[test]
    fn test_multiple_instances_are_independent() {
        let mut reg = IndicatorRegistry::new();
        reg.register(Box::new(TestFactory));

        let mut inst1 = reg.create_instance("test_indicator", &[]).unwrap();
        let mut inst2 = reg.create_instance("test_indicator", &[]).unwrap();

        inst1.on_candle(100.0, 105.0, 95.0, 102.0, 1000.0);
        let out2 = inst2.on_candle(200.0, 210.0, 190.0, 205.0, 2000.0);

        // inst2 should not be affected by inst1
        assert!((out2[0] - 205.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_default_creates_empty() {
        let reg = IndicatorRegistry::default();
        assert_eq!(reg.count(), 0);
    }
}
