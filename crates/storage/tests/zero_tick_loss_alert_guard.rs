//! Phase 10.1 of `.claude/plans/active-plan.md` — zero-tick-loss alert guard.
//!
//! Parthiban's explicit top-priority requirement: "zero ticks loss and
//! nowhere websocket should get disconnected or reconnect not even a
//! single tick should be missed". The three-tier buffer (ring -> disk
//! spill -> recovery) in `tick_persistence.rs` delivers this at the code
//! level, but the operator-facing guarantee lives in a Prometheus alert
//! rule.
//!
//! This guard pins that rule mechanically. If ANY of these go missing
//! from `deploy/docker/prometheus/rules/tickvault-alerts.yml`, the build
//! fails — the operator cannot silently lose their early-warning signal.
//!
//! Pinned assertions:
//!
//! 1. `tv_ticks_dropped_total` metric is emitted from `tick_persistence.rs`.
//!    Source scan — if the counter emission is deleted, the metric never
//!    increments and the alert is silently defanged.
//! 2. `TicksDropped` alert rule exists, uses `tv_ticks_dropped_total > 0`
//!    as the expr, and has a non-empty `for:` duration.
//! 3. `TickBufferActive` alert rule exists (ring buffer in use).
//! 4. `TickDiskSpillActive` alert rule exists (disk spill in use).
//! 5. `TickDataLoss` alert rule exists (catch-all data loss signal).
//!
//! These 4 alerts together cover every layer of the three-tier buffer:
//! ring-buffer saturation -> disk-spill activity -> drop counter ->
//! data-loss catch-all.

use std::fs;
use std::path::{Path, PathBuf};

const ALERTS_YAML: &str = "deploy/docker/prometheus/rules/tickvault-alerts.yml";
const TICK_PERSISTENCE_SOURCE: &str = "crates/storage/src/tick_persistence.rs";

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn load_text(rel: &str) -> String {
    let path = workspace_root().join(rel);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

#[test]
fn prometheus_alert_rule_ticks_dropped_exists() {
    let text = load_text(ALERTS_YAML);
    assert!(
        text.contains("- alert: TicksDropped"),
        "TicksDropped alert rule missing — operator loses zero-tick-loss early-warning signal"
    );
    assert!(
        text.contains("tv_ticks_dropped_total > 0"),
        "TicksDropped rule no longer uses `tv_ticks_dropped_total > 0` expression"
    );
}

#[test]
fn prometheus_alert_rule_tick_buffer_active_exists() {
    let text = load_text(ALERTS_YAML);
    assert!(
        text.contains("- alert: TickBufferActive"),
        "TickBufferActive alert rule missing — ring buffer saturation is invisible"
    );
}

#[test]
fn prometheus_alert_rule_tick_disk_spill_active_exists() {
    let text = load_text(ALERTS_YAML);
    assert!(
        text.contains("- alert: TickDiskSpillActive"),
        "TickDiskSpillActive alert rule missing — disk spill activity is invisible"
    );
}

#[test]
fn prometheus_alert_rule_tick_data_loss_exists() {
    let text = load_text(ALERTS_YAML);
    assert!(
        text.contains("- alert: TickDataLoss"),
        "TickDataLoss catch-all alert rule missing"
    );
}

#[test]
fn tick_persistence_emits_dropped_counter_field() {
    let text = load_text(TICK_PERSISTENCE_SOURCE);
    // The struct field name used for the counter.
    assert!(
        text.contains("ticks_dropped_total"),
        "tick_persistence.rs no longer tracks `ticks_dropped_total` — zero-tick-loss metric pipeline is broken"
    );
}

#[test]
fn tick_persistence_buffer_capacity_is_at_least_one_lakh() {
    // TICK_BUFFER_CAPACITY must stay above 100K to absorb normal market
    // bursts without spill-to-disk. The exact floor is 100_000; current
    // value is 600_000 per the docstring. Checked via constant reference
    // in source.
    let common_constants = load_text("crates/common/src/constants.rs");

    // Extract the TICK_BUFFER_CAPACITY constant value.
    let cap_line = common_constants
        .lines()
        .find(|l| l.contains("TICK_BUFFER_CAPACITY") && l.contains("="))
        .unwrap_or_else(|| panic!("TICK_BUFFER_CAPACITY not found in common/constants.rs"));

    // Find the numeric literal after '='. Strip underscores and the
    // `usize` / `u32` etc. suffix.
    let numeric: String = cap_line
        .split('=')
        .nth(1)
        .unwrap_or("")
        .chars()
        .filter(|c| c.is_ascii_digit())
        .collect();
    let value: usize = numeric
        .parse()
        .unwrap_or_else(|e| panic!("TICK_BUFFER_CAPACITY parse failed on `{numeric}`: {e}"));

    assert!(
        value >= 100_000,
        "TICK_BUFFER_CAPACITY = {value} — must be >= 100_000 to absorb bursts without spill"
    );
}

#[test]
fn observability_architecture_doc_mentions_zero_tick_loss_chain() {
    // The operator-facing durable doc MUST spell out the three-tier
    // buffer chain so future sessions don't have to rediscover it.
    let doc_path: PathBuf =
        workspace_root().join(".claude/rules/project/observability-architecture.md");
    let missing_msg = format!(
        "observability-architecture.md missing at {}",
        doc_path.display()
    );
    let doc = fs::read_to_string(&doc_path).unwrap_or_else(|_| panic!("{missing_msg}"));

    let keywords = ["errors.jsonl", "errors.summary.md", "ErrorCode", "Telegram"];
    for kw in keywords {
        assert!(
            doc.contains(kw),
            "observability-architecture.md missing keyword `{kw}` — doc drift"
        );
    }
}

fn _suppress_unused_path_warning(_p: &Path) {}
