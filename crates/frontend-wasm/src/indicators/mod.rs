//! Indicator implementations.
//!
//! Each indicator is a self-contained module implementing the `Compute` trait.
//! Add new indicators here — the registry auto-discovers them via `register_all()`.

pub mod macd_mtf;
pub mod smc;
pub mod squeeze_momentum;
pub mod supertrend;

use crate::registry::IndicatorRegistry;

/// Registers all built-in indicators into the given registry.
///
/// Called once at WASM module initialization. O(k) where k = number of indicators.
pub fn register_all(registry: &mut IndicatorRegistry) {
    registry.register(Box::new(supertrend::SupertrendFactory));
    registry.register(Box::new(smc::SmcFactory));
    registry.register(Box::new(squeeze_momentum::SqueezeMomentumFactory));
    registry.register(Box::new(macd_mtf::MacdMtfFactory));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_register_all_populates_registry() {
        let mut registry = IndicatorRegistry::new();
        register_all(&mut registry);
        assert_eq!(registry.count(), 4);
        assert!(registry.get("supertrend").is_some());
        assert!(registry.get("smc").is_some());
        assert!(registry.get("squeeze_momentum").is_some());
        assert!(registry.get("macd_mtf").is_some());
    }

    #[test]
    fn test_all_indicators_create_successfully() {
        let mut registry = IndicatorRegistry::new();
        register_all(&mut registry);

        for meta in registry.list() {
            let instance = registry.create_instance(meta.id, &[]);
            assert!(
                instance.is_some(),
                "failed to create instance for '{}'",
                meta.id
            );
            assert!(
                instance.unwrap().output_count() > 0,
                "indicator '{}' has zero outputs",
                meta.id
            );
        }
    }
}
