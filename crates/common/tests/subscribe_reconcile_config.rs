//! Parsing checks for the default-off main-feed reconciliation action.
//! These tests inspect configuration data without starting a feed or executor.

use figment::Figment;
use figment::providers::{Format, Toml};
use serde::Deserialize;
use tickvault_common::config::DhanUniverseConfig;

#[derive(Deserialize)]
struct Wrapper {
    dhan_universe: DhanUniverseConfig,
}

fn parse_universe(source: &str) -> Result<DhanUniverseConfig, Box<figment::Error>> {
    Figment::new()
        .merge(Toml::string(source))
        .extract::<Wrapper>()
        .map(|wrapper| wrapper.dhan_universe)
        .map_err(Box::new)
}

#[test]
fn legacy_universe_config_keeps_reconciliation_actions_off() {
    let legacy = parse_universe("[dhan_universe]\nenabled = true\n")
        .expect("a universe config predating reconciliation must still parse");
    assert!(legacy.enabled);
    assert!(!legacy.subscribe_reconcile_auto_resubscribe);
    assert!(!DhanUniverseConfig::default().subscribe_reconcile_auto_resubscribe);
}

#[test]
fn reconciliation_action_setting_requires_an_explicit_boolean() {
    for enabled in [false, true] {
        let source = format!("[dhan_universe]\nsubscribe_reconcile_auto_resubscribe = {enabled}\n");
        let config = parse_universe(&source).expect("explicit booleans must parse");
        assert_eq!(config.subscribe_reconcile_auto_resubscribe, enabled);
    }
    assert!(
        parse_universe("[dhan_universe]\nsubscribe_reconcile_auto_resubscribe = 1\n").is_err(),
        "a numeric value must not silently enable reconciliation"
    );
}

#[test]
fn shipped_base_config_keeps_reconciliation_actions_off() {
    let config = parse_universe(include_str!("../../../config/base.toml"))
        .expect("the shipped universe config must parse");
    assert!(!config.subscribe_reconcile_auto_resubscribe);
}
