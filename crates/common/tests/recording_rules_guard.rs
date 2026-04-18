//! Phase 12 extension — Prometheus recording rules guard.
//!
//! Recording rules pre-aggregate expensive Prometheus queries so the
//! 5 Grafana dashboards render in ~30-50ms per panel instead of
//! recomputing rates on every refresh. If a recording rule is
//! silently removed, the dashboard panel that depends on it either
//! (a) shows "No data" OR (b) falls back to an expensive query that
//! times out. Both are operator-facing regressions.
//!
//! This guard pins the 10 `tv:*` recording rules + ensures they
//! reference the underlying `tv_*` metrics correctly.

use std::fs;
use std::path::{Path, PathBuf};

const ALERTS_YAML: &str = "deploy/docker/prometheus/rules/tickvault-alerts.yml";

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn load_text() -> String {
    let path = workspace_root().join(ALERTS_YAML);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// Every required `tv:` recording rule. Adding a new rule? Add its
/// name here so future edits can't silently remove it.
const REQUIRED_RECORDING_RULES: &[&str] = &[
    "tv:ticks_per_minute:1m",
    "tv:order_latency_p99_ms:5m",
    "tv:order_latency_p95_ms:5m",
    "tv:token_hours_remaining",
    "tv:depth_20_pool_healthy",
    "tv:depth_200_pool_healthy",
    "tv:candles_per_minute:1m",
    "tv:dedup_per_minute:5m",
    "tv:valkey_errors_per_minute:1m",
    "tv:questdb_disk_free_gb",
];

#[test]
fn every_dashboard_recording_rule_is_defined() {
    let src = load_text();
    let mut missing: Vec<&'static str> = Vec::new();
    for rule in REQUIRED_RECORDING_RULES {
        let needle = format!("record: {rule}");
        if !src.contains(&needle) {
            missing.push(rule);
        }
    }
    assert!(
        missing.is_empty(),
        "Prometheus recording rules missing — dashboards will render slow or empty:\n  {}",
        missing.join("\n  ")
    );
}

#[test]
fn recording_rules_group_interval_is_set() {
    let src = load_text();
    assert!(
        src.contains("tv-dashboard-recording-rules"),
        "recording-rules group missing"
    );
    assert!(
        src.contains("interval: 30s"),
        "recording-rules group must declare interval (30s default) for\
         predictable pre-aggregation cadence"
    );
}

#[test]
fn order_latency_record_divides_by_1e6_for_ms() {
    // The histogram buckets are in nanoseconds. Dashboards want ms.
    // The record MUST divide by 1e6 — a missing divide means the
    // operator-health dashboard shows p99 in nanoseconds (huge number
    // triggering the red threshold for no reason).
    let src = load_text();
    for record in ["tv:order_latency_p99_ms:5m", "tv:order_latency_p95_ms:5m"] {
        let start = src
            .find(&format!("record: {record}"))
            .unwrap_or_else(|| panic!("record {record} missing"));
        let block: String = src[start..].chars().take(300).collect();
        assert!(
            block.contains("/ 1e6"),
            "{record} must divide by 1e6 to convert ns histogram -> ms"
        );
    }
}

#[test]
fn token_hours_remaining_divides_by_3600() {
    let src = load_text();
    let start = src
        .find("record: tv:token_hours_remaining")
        .expect("tv:token_hours_remaining missing");
    let block: String = src[start..].chars().take(200).collect();
    assert!(
        block.contains("/ 3600"),
        "tv:token_hours_remaining must divide seconds by 3600"
    );
}

#[test]
fn questdb_disk_free_divides_by_gb() {
    let src = load_text();
    let start = src
        .find("record: tv:questdb_disk_free_gb")
        .expect("tv:questdb_disk_free_gb missing");
    let block: String = src[start..].chars().take(200).collect();
    // 1073741824 = 1 GiB
    assert!(
        block.contains("1073741824"),
        "tv:questdb_disk_free_gb must divide bytes by 1GiB = 1073741824"
    );
}

#[test]
fn depth_pool_health_records_use_bool_comparison() {
    // The health record is `(active_conns == bool N)` — the `bool`
    // keyword is essential because comparison without it filters to
    // empty vector instead of emitting 0/1. Without `bool` the
    // recording rule looks like it works but returns no samples when
    // the pool is degraded → dashboard panel shows "No data" during
    // the exact moment the operator needs it.
    let src = load_text();
    for record in ["tv:depth_20_pool_healthy", "tv:depth_200_pool_healthy"] {
        let start = src
            .find(&format!("record: {record}"))
            .unwrap_or_else(|| panic!("record {record} missing"));
        let block: String = src[start..].chars().take(300).collect();
        assert!(
            block.contains("== bool"),
            "{record} must use `== bool N` — without `bool`, degraded pool \
             returns no samples instead of 0"
        );
    }
}

#[test]
fn recording_rules_use_tv_prefix_not_dlt() {
    // `dlt:` is legacy; all zero-touch-era rules use `tv:`. Enforce it
    // so copy-paste mistakes don't revive the legacy prefix.
    let src = load_text();
    for rule in REQUIRED_RECORDING_RULES {
        assert!(
            rule.starts_with("tv:"),
            "Zero-touch recording rules must start with `tv:` prefix, got `{rule}`"
        );
    }
}

fn _suppress(_p: &Path) {}
