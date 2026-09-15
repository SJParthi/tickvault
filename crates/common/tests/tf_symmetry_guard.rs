//! The shared active candle specification, runtime config and shipped TOML
//! must describe one exact set. Trading and metrics test their own typed views
//! of this specification without adding a dependency cycle to common.

use tickvault_common::candle_timeframes::ACTIVE_CANDLE_TIMEFRAMES;
use tickvault_common::config::TimeframesConfig;

const EXPECTED: [&str; 10] = [
    "1s", "3s", "5s", "1m", "3m", "5m", "10m", "15m", "30m", "60m",
];

#[test]
fn common_spec_and_default_match_the_requested_ten_frames() {
    let actual: Vec<_> = ACTIVE_CANDLE_TIMEFRAMES
        .iter()
        .map(|spec| spec.label)
        .collect();
    assert_eq!(actual, EXPECTED);
    assert_eq!(TimeframesConfig::default_list(), EXPECTED);
    assert!(TimeframesConfig::default().contains_seconds_tf());
}

#[test]
fn shipped_base_config_declares_the_same_unique_active_set() {
    let base: toml::Value =
        toml::from_str(include_str!("../../../config/base.toml")).expect("valid base TOML");
    let actual: Vec<_> = base["engine"]["timeframes"]["list"]
        .as_array()
        .expect("timeframe list")
        .iter()
        .map(|value| value.as_str().expect("timeframe label"))
        .collect();
    assert_eq!(actual, EXPECTED);
    let config: TimeframesConfig = base["engine"]["timeframes"]
        .clone()
        .try_into()
        .expect("shipped set accepted by real configuration deserializer");
    assert_eq!(config.list, EXPECTED);
}

#[test]
fn every_previous_candle_only_label_is_excluded_from_active_config() {
    let config = TimeframesConfig::default();
    for retired in [
        "2s", "4s", "6s", "7s", "8s", "9s", "10s", "11s", "12s", "13s", "14s", "15s", "30s", "2m",
        "1d",
    ] {
        assert!(
            !config.list.iter().any(|label| label == retired),
            "{retired}"
        );
    }
}
