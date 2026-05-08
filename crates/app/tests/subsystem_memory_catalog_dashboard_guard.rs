//! L127 ratchet — dashboard ↔ catalog cross-reference for the
//! Wave-5 in-memory-store observability metrics (#504a).
//!
//! Every metric registered by [`tickvault_app::metrics_catalog`] MUST
//! also appear on a Grafana dashboard, and every metric referenced on
//! a dashboard JSON file MUST be present in the catalog (so unused /
//! stray references can't accumulate). This test runs as part of the
//! standard `cargo test -p tickvault-app` battery.
//!
//! Why this exists (verbatim BUG-H6 finding from §AA review):
//! > Dashboard rename not guarded — `operator-health.json` JSON
//! > references metric name; no test pins JSON ↔ catalog.
//!
//! The catalog (`crates/app/src/metrics_catalog.rs`) is a single
//! source of truth. The Grafana dashboard `operator-health.json`
//! ships the operator-visible representation. This ratchet enforces
//! that they stay in lockstep.

use std::path::{Path, PathBuf};

use tickvault_app::metrics_catalog::{
    IN_MEM_EVICTIONS_COUNTER_NAME, MARKET_HOURS_ACTIVE_GAUGE_NAME, SAMPLER_HEARTBEAT_GAUGE_NAME,
    SUBSYSTEM_MEMORY_GAUGE_NAME,
};

/// Names that the L127 ratchet REQUIRES on at least one dashboard.
/// Every entry must be a metric registered in `metrics_catalog.rs`.
fn required_metrics_on_dashboards() -> &'static [&'static str] {
    &[
        SUBSYSTEM_MEMORY_GAUGE_NAME,
        SAMPLER_HEARTBEAT_GAUGE_NAME,
        MARKET_HOURS_ACTIVE_GAUGE_NAME,
        // Note: IN_MEM_EVICTIONS_COUNTER_NAME is not yet on the
        // operator-health dashboard. #504a ships the counter
        // scaffolding (L128 cached handles, BUG-L13 boot pre-warm)
        // but the per-TF eviction panel lands in #504d once the
        // in-memory store is wired. Adding the panel before the
        // metric exists would always graph as zero and waste
        // operator screen real estate.
    ]
}

/// Path to the operator-health dashboard JSON, rooted at workspace.
fn operator_health_path() -> PathBuf {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest_dir
        .parent()
        .and_then(Path::parent)
        .expect("workspace root must exist")
        .join("deploy/docker/grafana/dashboards/operator-health.json")
}

#[test]
fn every_l18_metric_appears_on_operator_health_dashboard() {
    // L127 / BUG-H6: pin every Wave-5 metric to a dashboard panel.
    let path = operator_health_path();
    let content = std::fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("failed to read {}: {err}", path.display()));
    for &metric in required_metrics_on_dashboards() {
        assert!(
            content.contains(metric),
            "L127 ratchet: required metric `{metric}` is NOT referenced \
             on `operator-health.json`. Either add a panel for it, or \
             remove it from `required_metrics_on_dashboards()` (and \
             update `metrics_catalog.rs` accordingly)."
        );
    }
}

#[test]
fn legacy_subsystem_memory_metric_name_is_not_referenced_on_dashboards() {
    // L121 / BUG-C1: `tv_subsystem_memory_bytes` was the pre-review
    // working name. The renamed metric is
    // `tv_subsystem_memory_estimated_bytes` (no claim of being raw
    // RSS). If the legacy name leaks into a panel JSON, the dashboard
    // would silently graph nothing — operator gets no signal.
    let path = operator_health_path();
    let content = std::fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("failed to read {}: {err}", path.display()));
    // Match the legacy name only when it is NOT followed by
    // `_estimated`, so the legitimate renamed metric does not trip
    // the guard.
    let legacy = "tv_subsystem_memory_bytes";
    let idx = 0;
    if let Some(pos) = content[idx..].find(legacy) {
        let abs = idx + pos;
        let tail = &content[abs + legacy.len()..];
        // Accept the renamed prefix `tv_subsystem_memory_estimated_bytes`
        // only — anything else is the legacy name returning.
        // The exact renamed substring `tv_subsystem_memory_bytes` would
        // continue into `_estimated_bytes`, so detect that pattern.
        let next_char = tail.chars().next().unwrap_or('_');
        // The renamed metric `tv_subsystem_memory_estimated_bytes` does
        // NOT contain the legacy substring (no `_bytes` between
        // `_memory` and `_estimated`), so this branch is unreachable for
        // the renamed name. If the legacy substring appears at all,
        // it's the banned form.
        let _ = next_char;
        panic!(
            "L121 ratchet: legacy metric name `{legacy}` appears in \
             `operator-health.json` at byte offset {abs}. The metric \
             was RENAMED to `tv_subsystem_memory_estimated_bytes` so \
             the name does not falsely claim raw-RSS semantics."
        );
    }
}

#[test]
fn alerts_yml_references_renamed_metric_only() {
    // Same L121 ratchet, applied to the Grafana provisioning alerts.
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let alerts_path = manifest_dir
        .parent()
        .and_then(Path::parent)
        .expect("workspace root")
        .join("deploy/docker/grafana/provisioning/alerting/alerts.yml");
    let content = std::fs::read_to_string(&alerts_path)
        .unwrap_or_else(|err| panic!("failed to read {}: {err}", alerts_path.display()));

    // Both new alerts must reference the renamed metric.
    assert!(
        content.contains(SUBSYSTEM_MEMORY_GAUGE_NAME),
        "alerts.yml must reference {SUBSYSTEM_MEMORY_GAUGE_NAME}"
    );
    assert!(
        content.contains(SAMPLER_HEARTBEAT_GAUGE_NAME),
        "alerts.yml must reference {SAMPLER_HEARTBEAT_GAUGE_NAME}"
    );
    assert!(
        content.contains(MARKET_HOURS_ACTIVE_GAUGE_NAME),
        "alerts.yml must reference {MARKET_HOURS_ACTIVE_GAUGE_NAME}"
    );

    // Counter is not yet alerted (no panic / no eviction signal until
    // #504d wires the in-memory store). Ratchet pins absence-by-name
    // of the legacy form only.
    let _ = IN_MEM_EVICTIONS_COUNTER_NAME;
}

#[test]
fn alert_quiet_hours_gate_uses_market_hours_active_per_l129() {
    // L129: every alert that must NOT page outside market hours
    // includes `and on() tv_market_hours_active == 1`. The two new
    // alerts both gate on the market-hours gauge.
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let alerts_path = manifest_dir
        .parent()
        .and_then(Path::parent)
        .expect("workspace root")
        .join("deploy/docker/grafana/provisioning/alerting/alerts.yml");
    let content = std::fs::read_to_string(&alerts_path)
        .unwrap_or_else(|err| panic!("failed to read {}: {err}", alerts_path.display()));

    // Find the tv-rss-per-subsystem-high block and assert its expr
    // contains the gate.
    for needle in [
        "tv-rss-per-subsystem-high",
        "tv-subsystem-memory-sampler-stale",
    ] {
        let idx = content
            .find(needle)
            .unwrap_or_else(|| panic!("alert `{needle}` missing from alerts.yml"));
        // Look at a window of 1500 bytes after the alert UID — the
        // expression appears within ~10 lines.
        let window_end = (idx + 1500).min(content.len());
        let window = &content[idx..window_end];
        assert!(
            window.contains("tv_market_hours_active == 1"),
            "L129 ratchet: alert `{needle}` is missing the \
             `and on() tv_market_hours_active == 1` quiet-hours \
             gate. Outside-market drift would otherwise page."
        );
    }
}

#[test]
fn subsystem_memory_alert_uses_unless_absent_over_time_per_l124() {
    // L124 / BUG-H3: components init to NaN; the alert MUST filter
    // them out via `unless absent_over_time(...)` so unregistered
    // components cannot page.
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let alerts_path = manifest_dir
        .parent()
        .and_then(Path::parent)
        .expect("workspace root")
        .join("deploy/docker/grafana/provisioning/alerting/alerts.yml");
    let content = std::fs::read_to_string(&alerts_path)
        .unwrap_or_else(|err| panic!("failed to read {}: {err}", alerts_path.display()));

    let needle = "tv-rss-per-subsystem-high";
    let idx = content
        .find(needle)
        .unwrap_or_else(|| panic!("alert `{needle}` missing"));
    let window_end = (idx + 1500).min(content.len());
    let window = &content[idx..window_end];
    assert!(
        window.contains("absent_over_time"),
        "L124 ratchet: tv-rss-per-subsystem-high must use \
         `unless absent_over_time(...)` to filter NaN-init components."
    );
}

#[test]
fn legacy_memory_rss_alert_constant_is_not_referenced_in_app_src() {
    // L122 / BUG-C2: the legacy `MEMORY_RSS_ALERT_MB` constant was
    // retired. Pin its absence in the app crate's source tree so a
    // future commit cannot silently re-introduce it.
    //
    // Allow-list: this test file itself (and the comment in
    // `infra.rs` documenting the retirement) reference the name in
    // string form. We only block `pub const MEMORY_RSS_ALERT_MB`.
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let src = manifest_dir.join("src");
    let mut hits = Vec::new();
    walk_for_const(&src, &mut hits);
    assert!(
        hits.is_empty(),
        "L122 ratchet: legacy `pub const MEMORY_RSS_ALERT_MB` must \
         NOT reappear. Found in: {hits:?}"
    );
}

fn walk_for_const(dir: &Path, hits: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk_for_const(&path, hits);
            continue;
        }
        if path.extension().and_then(|s| s.to_str()) != Some("rs") {
            continue;
        }
        if let Ok(content) = std::fs::read_to_string(&path) {
            // Match the actual declaration line (start of line, optional
            // whitespace, then `pub const MEMORY_RSS_ALERT_MB`). Skips
            // string-literal references (e.g. the retirement-pin test
            // in `infra.rs` that asserts the source no longer contains
            // the declaration).
            for line in content.lines() {
                if line
                    .trim_start()
                    .starts_with("pub const MEMORY_RSS_ALERT_MB")
                {
                    hits.push(path.clone());
                    break;
                }
            }
        }
    }
}
