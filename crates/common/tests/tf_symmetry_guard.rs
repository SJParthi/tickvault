//! PR #517 (Wave-5 TF reduction) — symmetry ratchet across the 4
//! TF-bearing surfaces.
//!
//! The active timeframe set is mirrored across:
//! 1. `tickvault_app::metrics_catalog::Tf::ALL` (Prometheus label set)
//! 2. `tickvault_common::config::TimeframesConfig::default_list`
//!    (runtime config)
//! 3. `tickvault_trading::candles::cascade_fanout::CascadeFanout`
//!    (RAM cascade engines)
//! 4. `tickvault_storage::materialized_views::VIEW_DEFS`
//!    (QuestDB matview DDL)
//!
//! Adding or removing a TF MUST update ALL FOUR sites in lock-step,
//! per the operator directive: "if we add the timeframes then it
//! should be applicable for both ticks candles in memory and db
//! same vice versa also reverse also applicable".
//!
//! These tests live in `tickvault-common` (not the app crate) so
//! they don't pull in the app binary's tokio runtime when running
//! `cargo test -p tickvault-common`.
//!
//! Note: `metrics_catalog` and `cascade_fanout` symmetry checks live
//! IN the source crates (where their internal modules are reachable).
//! This file owns the cross-crate config-side symmetry checks.

use tickvault_common::config::TimeframesConfig;

/// PR #517 canonical TF set, ordered ascending. Re-add a TF here ONLY
/// after coordinated updates to the 4 surfaces above.
const PR517_CANONICAL_TF_SET: &[&str] =
    &["1m", "5m", "15m", "30m", "1h", "2h", "3h", "4h", "1d"];

#[test]
fn timeframes_config_default_matches_pr517_canonical_set() {
    let cfg = TimeframesConfig::default();
    let actual: Vec<&str> = cfg.list.iter().map(String::as_str).collect();
    assert_eq!(
        actual, PR517_CANONICAL_TF_SET,
        "TimeframesConfig::default_list drifted from the PR #517 canonical \
         9-TF set. Adding a TF requires symmetric updates to: \
         metrics_catalog::Tf, cascade_fanout::CascadeFanout, \
         materialized_views::VIEW_DEFS."
    );
}

#[test]
fn timeframes_config_default_has_no_retired_pr517_tfs() {
    let retired = [
        "2m", "3m", "4m", "6m", "7m", "8m", "9m", "10m", "11m", "12m", "13m", "14m",
    ];
    let cfg = TimeframesConfig::default();
    for tf in retired {
        assert!(
            !cfg.list.iter().any(|s| s == tf),
            "Retired TF {tf} reappeared in TimeframesConfig::default_list — \
             re-add via coordinated PR (config + cascade engine + matview DDL + \
             enum + symmetry ratchet)."
        );
    }
}

#[test]
fn timeframes_config_default_has_no_seconds_tfs() {
    let cfg = TimeframesConfig::default();
    assert!(
        !cfg.contains_seconds_tf(),
        "Wave-5 §K-L7 retired all seconds-resolution timeframes; \
         the default list MUST NOT contain any. Got: {:?}",
        cfg.list
    );
}

#[test]
fn timeframes_config_default_count_is_9() {
    let cfg = TimeframesConfig::default();
    assert_eq!(
        cfg.list.len(),
        9,
        "PR #517: default TF count is 9 (was 21). Found {}.",
        cfg.list.len()
    );
}
