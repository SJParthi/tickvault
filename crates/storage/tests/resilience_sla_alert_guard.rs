//! Phase 11 of `.claude/plans/active-plan.md` — WS + QuestDB + Valkey
//! resilience-SLA alert guard.
//!
//! Parthiban's requirement: "nowhere websocket should get disconnected or
//! reconnect ... questdb should never ever fail or always disconnect — I
//! need guarantee for both of these always". We can't physically prevent
//! third-party failures, but we CAN guarantee:
//!
//! 1. The measurement instruments (Prometheus metrics) exist.
//! 2. The SLA alerts fire the moment any of the three subsystems
//!    degrade below the contract.
//!
//! This guard pins BOTH — so future edits that silently remove a metric
//! emission OR the matching alert rule fail the build.
//!
//! Pinned alerts:
//!
//! - WebSocketDisconnected — main feed pool degraded > N seconds
//! - HighWebSocketReconnectRate — reconnect loop detection
//! - WebSocketBackpressure — SPSC channel lagging → ticks queued
//! - QuestDbDown — ILP writer unable to reach QuestDB
//! - ValkeyDown — cache fallback active
//! - HighValkeyErrorRate — cache errors exceed threshold
//!
//! Together these cover every layer of the three-tier resilience
//! story — network -> WebSocket pool -> storage -> cache.

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

fn load_alerts_yaml() -> String {
    let path = workspace_root().join(ALERTS_YAML);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// Every required SLA alert name. Adding a new resilience alert?
/// Add its name here so future edits can't silently remove it.
const REQUIRED_ALERT_NAMES: &[&str] = &[
    // Original 6 — WS / QuestDB / Valkey
    "WebSocketDisconnected",
    "HighWebSocketReconnectRate",
    "WebSocketBackpressure",
    "QuestDbDown",
    "ValkeyDown",
    "HighValkeyErrorRate",
    // Defense-in-depth auth + IP + collision alerts (added 2026-04-18
    // so these fire without needing the opt-in Loki profile)
    "TokenRenewalFailureBurst",
    "AuthCircuitBreakerOpen",
    "IpMismatchDetected",
    "InstrumentRegistryCollisionDrift",
];

#[test]
fn every_resilience_sla_alert_is_pinned() {
    let text = load_alerts_yaml();
    let mut missing: Vec<&'static str> = Vec::new();
    for alert in REQUIRED_ALERT_NAMES {
        let needle = format!("- alert: {alert}");
        if !text.contains(&needle) {
            missing.push(alert);
        }
    }
    assert!(
        missing.is_empty(),
        "Required SLA alert rules missing from Prometheus config:\n  {}\n\n\
         These alerts are the only mechanical signal the operator receives \
         when the WS / QuestDB / Valkey resilience contract is violated. \
         Do not delete them.",
        missing.join("\n  ")
    );
}

#[test]
fn websocket_disconnected_alert_uses_correct_metric() {
    let text = load_alerts_yaml();
    // Find the WebSocketDisconnected block and verify it references
    // the pool-degraded metric or the active-connections gauge.
    let start = text
        .find("- alert: WebSocketDisconnected")
        .unwrap_or_else(|| panic!("WebSocketDisconnected alert missing"));
    let block: String = text[start..].chars().take(1200).collect();
    let uses_active = block.contains("tv_websocket_connections_active");
    let uses_pool = block.contains("tv_pool_degraded");
    assert!(
        uses_active || uses_pool,
        "WebSocketDisconnected rule must reference tv_websocket_connections_active \
         or tv_pool_degraded_seconds, got:\n{}",
        &block[..block.len().min(400)]
    );
}

#[test]
fn questdb_down_alert_uses_correct_metric() {
    let text = load_alerts_yaml();
    let start = text
        .find("- alert: QuestDbDown")
        .unwrap_or_else(|| panic!("QuestDbDown alert missing"));
    let block: String = text[start..].chars().take(1200).collect();
    assert!(
        block.contains("tv_questdb_connected")
            || block.contains("tv_questdb_disconnected")
            || block.contains("tv_questdb_alive"),
        "QuestDbDown rule must reference a QuestDB liveness metric \
         (tv_questdb_connected / tv_questdb_disconnected / tv_questdb_alive)"
    );
}

#[test]
fn valkey_down_alert_uses_correct_metric() {
    let text = load_alerts_yaml();
    let start = text
        .find("- alert: ValkeyDown")
        .unwrap_or_else(|| panic!("ValkeyDown alert missing"));
    let block: String = text[start..].chars().take(1200).collect();
    assert!(
        block.contains("up{") && (block.contains("valkey") || block.contains("tv-valkey")),
        "ValkeyDown rule must reference the valkey up metric"
    );
}

#[test]
fn every_resilience_alert_has_severity_label() {
    let text = load_alerts_yaml();
    for alert in REQUIRED_ALERT_NAMES {
        let needle = format!("- alert: {alert}");
        let Some(idx) = text.find(&needle) else {
            continue; // caught by every_resilience_sla_alert_is_pinned
        };
        // Scan the next ~50 lines looking for `severity:` label.
        let block: String = text[idx..].chars().take(1500).collect();
        let has_severity = block.contains("severity:");
        assert!(
            has_severity,
            "{alert} alert is missing a severity label — Alertmanager \
             routing depends on severity for Telegram dedup + SMS escalation"
        );
    }
}

#[test]
fn tick_persistence_emits_questdb_connection_gauge() {
    // Source scan — the gauge name referenced by QuestDbDown alert
    // must actually be emitted somewhere in the storage crate.
    // Source scan — the gauge name referenced by QuestDbDown alert
    // must be emitted somewhere in the storage crate. Historically the
    // emission was in tick_persistence.rs but was moved to the
    // dedicated questdb_health.rs liveness poller, so scan the full
    // crate source tree.
    let src_dir = workspace_root().join("crates/storage/src");
    let mut combined = String::new();
    fn walk(dir: &Path, out: &mut String) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                walk(&p, out);
                continue;
            }
            if p.extension().and_then(|s| s.to_str()) != Some("rs") {
                continue;
            }
            if let Ok(body) = fs::read_to_string(&p) {
                out.push_str(&body);
                out.push('\n');
            }
        }
    }
    walk(&src_dir, &mut combined);
    assert!(
        combined.contains("tv_questdb_connected")
            || combined.contains("tv_questdb_disconnected")
            || combined.contains("tv_questdb_alive"),
        "QuestDB liveness gauge not emitted anywhere under crates/storage/src/ — \
         the QuestDbDown alert rule will never fire"
    );
}
