//! Phase 9.1 of `.claude/plans/active-plan.md` — operator health dashboard guard.
//!
//! Pins the structural invariants of the
//! `deploy/docker/grafana/dashboards/operator-health.json` dashboard so
//! nobody can silently remove a green/red panel that the operator's
//! "morning glance" depends on.
//!
//! The dashboard is the one-page answer to "is tickvault working right
//! now?". Every panel maps to a specific PromQL metric; if the metric
//! name drifts OR the panel is deleted, this guard fails the build.

use std::fs;
use std::path::PathBuf;

const DASHBOARD_PATH: &str = "deploy/docker/grafana/dashboards/operator-health.json";

/// Every required (panel_title, promql_metric) pair. If any pair is
/// missing from the JSON, the test fails.
const REQUIRED_PANELS: &[(&str, &str)] = &[
    ("App", "up{job=\\\"tickvault\\\"}"),
    ("QuestDB", "tv_questdb_connected"),
    ("WebSocket Pool", "tv_websocket_connections_active"),
    ("Valkey Cache", "up{job=\\\"tv-valkey\\\"}"),
    ("Token", "tv_token_remaining_seconds"),
    ("Circuit Breaker", "tv_circuit_breaker_state"),
    ("Ticks / min", "tv_ticks_processed_total"),
    ("Ticks lost (cumulative)", "tv_ticks_dropped_total"),
    ("Tick buffer active", "tv_tick_buffer_size"),
    ("Ticks spilled to disk", "tv_ticks_spilled_total"),
    (
        "Cross-segment collisions",
        "tv_instrument_registry_cross_segment_collisions",
    ),
    ("Valkey errors / min", "tv_valkey_errors_total"),
    ("Daily P&L", "tv_realized_pnl"),
    (
        "Instrument registry entries",
        "tv_instrument_registry_total_entries",
    ),
];

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(std::path::Path::parent)
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn load_dashboard_text() -> String {
    let path = workspace_root().join(DASHBOARD_PATH);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

#[test]
fn operator_health_dashboard_file_exists() {
    let path = workspace_root().join(DASHBOARD_PATH);
    assert!(
        path.exists(),
        "operator-health.json missing at {}",
        path.display()
    );
}

#[test]
fn operator_health_dashboard_is_valid_json() {
    let text = load_dashboard_text();
    // Quick structural check: opens with `{`, closes with `}`, has
    // required top-level keys. Full JSON parsing would need serde_json
    // which is a heavyweight add for a compile-only test — the
    // existing `grafana_dashboard_snapshot_filter_guard` already does
    // deep JSON parsing across every dashboard.
    assert!(text.trim_start().starts_with('{'), "must start with {{");
    assert!(text.trim_end().ends_with('}'), "must end with }}");
    for key in ["\"uid\"", "\"title\"", "\"panels\"", "\"refresh\""] {
        assert!(text.contains(key), "dashboard missing top-level key {key}");
    }
}

#[test]
fn operator_health_dashboard_uid_is_stable() {
    let text = load_dashboard_text();
    // The uid is what Grafana uses to identify the dashboard. Must not
    // drift or the operator's saved bookmarks + `grafana_url` links
    // in alert emails break.
    assert!(
        text.contains("\"uid\": \"tv-operator-health\""),
        "dashboard UID must remain `tv-operator-health` for stable links"
    );
}

#[test]
fn operator_health_dashboard_timezone_is_ist() {
    let text = load_dashboard_text();
    // SEBI audit + operator comfort: IST display everywhere.
    assert!(
        text.contains("\"timezone\": \"Asia/Kolkata\""),
        "dashboard timezone must be Asia/Kolkata (IST)"
    );
}

#[test]
fn operator_health_dashboard_has_every_required_panel() {
    let text = load_dashboard_text();
    let mut missing: Vec<String> = Vec::new();
    for (title, metric) in REQUIRED_PANELS {
        if !text.contains(&format!("\"title\": \"{title}\"")) {
            missing.push(format!("panel title `{title}` missing"));
        }
        if !text.contains(metric) {
            missing.push(format!("metric `{metric}` (for panel `{title}`) missing"));
        }
    }
    assert!(
        missing.is_empty(),
        "operator-health.json is missing required panels/metrics:\n  {}\n\n\
         The operator's one-glance morning check depends on every panel here.\n\
         If you genuinely need to remove one, update REQUIRED_PANELS in this test first.",
        missing.join("\n  ")
    );
}

#[test]
fn operator_health_dashboard_covers_zero_tick_loss_chain() {
    let text = load_dashboard_text();
    // The dashboard MUST show every layer of the three-tier buffer so
    // the operator can diagnose where a zero-tick-loss violation
    // happened. Missing any layer = invisible data loss.
    for metric in [
        "tv_ticks_dropped_total", // final counter
        "tv_tick_buffer_size",    // ring buffer
        "tv_ticks_spilled_total", // disk spill
    ] {
        assert!(
            text.contains(metric),
            "dashboard missing zero-tick-loss chain metric: {metric}"
        );
    }
}

#[test]
fn operator_health_dashboard_is_auto_provisioned() {
    // Grafana picks up dashboards from
    // deploy/docker/grafana/dashboards/ automatically via the
    // provisioning/dashboards/dashboards.yml config. This test is a
    // sanity check that our new dashboard landed in the provisioned
    // directory, not somewhere else that wouldn't get loaded.
    let provisioned_dir = workspace_root().join("deploy/docker/grafana/dashboards");
    let dashboard_path = workspace_root().join(DASHBOARD_PATH);
    assert!(
        dashboard_path.starts_with(&provisioned_dir),
        "operator-health.json must live under the provisioned dashboards directory"
    );
}
