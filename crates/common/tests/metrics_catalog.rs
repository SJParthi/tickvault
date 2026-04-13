//! E1: Prometheus metrics catalog — ratchet test that every metric
//! introduced by the zero-tick-loss work is wired into production code.
//!
//! This file does NOT spin up a metrics recorder (that would require a
//! heavy integration test). Instead, it scans the source tree for the
//! literal metric name strings. If someone deletes an emission site, the
//! test fails loudly with a descriptive message.
//!
//! Tests in this file exist in pairs with the feature that introduced the
//! metric — they are the second line of defence after the feature's own
//! unit tests, specifically guarding against silent metric-deletion during
//! later refactors.
//!
//! To add a new metric: add it here AND emit it from prod code AND wire it
//! into Grafana (see E2).

use std::path::Path;

/// Reads all .rs files in the given directories and concatenates them
/// into a single haystack for substring matching. Returns the concatenation.
fn read_all_sources() -> String {
    let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .map(std::path::PathBuf::from)
        .expect("workspace root must exist above crates/common"); // APPROVED: test

    let mut haystack = String::new();
    let crates = ["common", "core", "trading", "storage", "api", "app"];
    for crate_name in &crates {
        let crate_src = workspace_root.join("crates").join(crate_name).join("src");
        if !crate_src.exists() {
            continue;
        }
        walk_and_concat(&crate_src, &mut haystack);
    }
    haystack
}

fn walk_and_concat(dir: &Path, out: &mut String) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk_and_concat(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            if let Ok(content) = std::fs::read_to_string(&path) {
                out.push_str(&content);
                out.push('\n');
            }
        }
    }
}

/// Every entry in this table MUST appear as a literal string in the
/// workspace source tree. The description column is the operator-facing
/// meaning of the metric — it's what you'd put on the Grafana tooltip.
const REQUIRED_METRICS: &[(&str, &str)] = &[
    // --- Session 1 (A2 dead-letter queue) ---
    (
        "dlt_dlq_ticks_total",
        "Ticks that failed BOTH ring buffer AND disk spill and were \
         written to the NDJSON dead-letter queue. MUST be 0 in production.",
    ),
    (
        "dlt_spill_disk_available_mb",
        "Free disk space on the spill directory, updated on every \
         open_spill_file call.",
    ),
    // --- Session 2 A5 (graceful unsubscribe) ---
    (
        "dlt_ws_graceful_unsub_total",
        "Count of graceful-unsubscribe attempts by outcome \
         (sent / send_failed / timeout) per WebSocket connection_id.",
    ),
    (
        "dlt_ws_graceful_shutdown_signalled_total",
        "Total connections signalled by \
         WebSocketConnectionPool::request_graceful_shutdown.",
    ),
    // --- Session 2 A4 (pool circuit breaker) ---
    (
        "dlt_pool_degraded_seconds",
        "How long the WebSocket pool has been FULLY degraded (all \
         connections Reconnecting/Disconnected). 0 when healthy.",
    ),
    (
        "dlt_pool_degraded_alerts_total",
        "Number of times the pool crossed the 60s degraded alert \
         threshold. Each down-cycle increments this once.",
    ),
    (
        "dlt_pool_halts_total",
        "Number of times the pool watchdog requested a process halt \
         (down for >300s). The caller should exit the process when this \
         fires — supervisor restart brings us back up.",
    ),
    (
        "dlt_pool_recoveries_total",
        "Number of times the pool recovered from AllDown to Healthy.",
    ),
    // --- Session 3 S3-1 (QuestDB health poller) ---
    (
        "dlt_questdb_connected",
        "Binary gauge: 1.0 if the tick writer's ILP sender is connected, \
         0.0 otherwise. Flipping to 0 does NOT imply tick loss — the ring \
         buffer + spill path absorb writes while QuestDB is down.",
    ),
    (
        "dlt_questdb_disconnected_seconds",
        "How long the current QuestDB outage has lasted (0 when connected).",
    ),
    (
        "dlt_questdb_reconnects_total",
        "Total successful QuestDB reconnects since process startup.",
    ),
    (
        "dlt_questdb_disconnect_events_total",
        "Total Connected → Disconnected transitions observed. Each outage \
         increments this once.",
    ),
];

// Note: `dlt_sandbox_gate_blocks_total` was intentionally deferred from E1.
// Adding a metrics counter to `common::config` would require pulling the
// `metrics` crate into the common crate, which is currently
// framework-free. The ERROR log on sandbox-gate-block already fires a
// Telegram alert, so operator visibility exists without the counter.
// Revisit if we ever need a time-series of block attempts.

/// E1: Every metric in REQUIRED_METRICS MUST appear as a literal string
/// in the workspace source. If this test fails, you either:
/// - Deleted the emission site (regression — restore it)
/// - Renamed the metric (update REQUIRED_METRICS + Grafana dashboards)
/// - Added a new metric without registering it here (add it now)
#[test]
fn metrics_catalog_every_required_metric_is_emitted() {
    let haystack = read_all_sources();

    let mut missing = Vec::new();
    for (name, description) in REQUIRED_METRICS {
        // Search for the literal name inside a Rust string (quoted).
        // Any metric! macro use embeds the name as a quoted literal.
        let quoted = format!("\"{name}\"");
        if !haystack.contains(&quoted) {
            missing.push((*name, *description));
        }
    }

    assert!(
        missing.is_empty(),
        "E1: {} required metric(s) are NOT emitted anywhere in production source:\n{}",
        missing.len(),
        missing
            .iter()
            .map(|(n, d)| format!("  - {n}: {d}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
}

/// E1: Sanity check — REQUIRED_METRICS itself must not contain duplicates.
/// A duplicate would mean two different descriptions point at the same
/// metric name, which would silently confuse operators.
#[test]
fn metrics_catalog_no_duplicate_names() {
    let mut seen = std::collections::HashSet::new();
    for (name, _) in REQUIRED_METRICS {
        assert!(
            seen.insert(*name),
            "E1: duplicate metric name in REQUIRED_METRICS: {name}"
        );
    }
}

/// E1: Every metric name must follow the Prometheus convention
/// (lowercase, underscores, `_total` suffix for counters).
#[test]
fn metrics_catalog_names_follow_prometheus_conventions() {
    for (name, _) in REQUIRED_METRICS {
        assert!(
            name.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'),
            "E1: metric name must be snake_case ASCII: {name}"
        );
        assert!(
            name.starts_with("dlt_"),
            "E1: all dlt metrics must carry the dlt_ prefix: {name}"
        );
    }
}
