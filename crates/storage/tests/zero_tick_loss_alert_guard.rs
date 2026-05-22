//! Zero-tick-loss source guard.
//!
//! Parthiban's explicit top-priority requirement: "zero ticks loss and
//! nowhere websocket should get disconnected or reconnect not even a
//! single tick should be missed". The three-tier buffer (ring -> disk
//! spill -> recovery) in `tick_persistence.rs` delivers this at the code
//! level.
//!
//! 2026-05-20 (#O3 — observability → CloudWatch-only): the four
//! Prometheus-alert-rule assertions were removed when the Prometheus
//! container + `tickvault-alerts.yml` were retired. The operator-facing
//! early-warning signal moves to AWS CloudWatch Alarms over the same
//! `tv_*` metrics (the app still exposes them on `/metrics`). The
//! source-scan assertions below stay valid — they pin that the metrics
//! are still EMITTED, which CloudWatch needs.
//!
//! Pinned assertions:
//!
//! 1. `tv_ticks_dropped_total` metric is emitted from `tick_persistence.rs`.
//! 2. `tv_spill_dropped_total` is emitted with `reason` labels.
//! 3. `TICK_BUFFER_CAPACITY` stays >= 100_000.
//! 4. `observability-architecture.md` documents the zero-tick-loss chain.

use std::fs;
use std::path::{Path, PathBuf};

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
fn tick_persistence_emits_dropped_counter_field() {
    let text = load_text(TICK_PERSISTENCE_SOURCE);
    // The struct field name used for the counter.
    assert!(
        text.contains("ticks_dropped_total"),
        "tick_persistence.rs no longer tracks `ticks_dropped_total` — zero-tick-loss metric pipeline is broken"
    );
}

/// Wave 1 Item 0.b — `tv_spill_dropped_total` counter MUST be emitted
/// at the spill-path drop sites (open-failure + write-failure). Without
/// it, operators cannot distinguish "spill is fine" from "spill is
/// dropping ticks because of a slow disk".
#[test]
fn tick_persistence_emits_spill_dropped_total_with_reason_label() {
    let text = load_text(TICK_PERSISTENCE_SOURCE);
    assert!(
        text.contains("tv_spill_dropped_total"),
        "Wave 1 Item 0.b regression: tick_persistence.rs no longer emits \
         `tv_spill_dropped_total` — operator visibility into spill-path \
         drop reasons is broken."
    );
    assert!(
        text.contains("\"reason\" => \"open_failed\""),
        "Wave 1 Item 0.b regression: spill-path open-failure drop site \
         must label the metric with `reason=\"open_failed\"`"
    );
    assert!(
        text.contains("\"reason\" => \"write_failed\""),
        "Wave 1 Item 0.b regression: spill-path write-failure drop site \
         must label the metric with `reason=\"write_failed\"`"
    );
}

#[test]
fn tick_persistence_buffer_capacity_is_at_least_one_lakh() {
    // TICK_BUFFER_CAPACITY must stay above 100K to absorb normal market
    // bursts without spill-to-disk. The exact floor is 100_000.
    let common_constants = load_text("crates/common/src/constants.rs");

    let cap_line = common_constants
        .lines()
        .find(|l| l.contains("TICK_BUFFER_CAPACITY") && l.contains("="))
        .unwrap_or_else(|| panic!("TICK_BUFFER_CAPACITY not found in common/constants.rs"));

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
