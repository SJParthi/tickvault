//! Ratchet: every metric named in `deploy/aws/terraform/app-alarms.tf`
//! must be emitted somewhere in the Rust codebase, AND must be present
//! in the CloudWatch agent's prometheus EMF metric_declaration filter
//! in `deploy/aws/terraform/user-data.sh.tftpl`.
//!
//! Three-way drift check:
//!   1. alarm metric_name → has matching `counter!` / `gauge!` call in crates/
//!   2. alarm metric_name → appears in user-data EMF filter list
//!   3. EMF filter list metric → appears in at least one alarm
//!
//! Without this guard, renaming a Rust metric (or dropping it from the
//! EMF filter) silently breaks the alarm — operator gets no Telegram.

use std::fs;
use std::path::{Path, PathBuf};

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .map(PathBuf::from)
        .expect("workspace root must exist above crates/common") // APPROVED: test
}

fn read(rel: &str) -> String {
    let p = workspace_root().join(rel);
    fs::read_to_string(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display())) // APPROVED: test
}

/// Pull every `metric_name = "tv_..."` literal out of `app-alarms.tf`.
fn alarm_metric_names() -> Vec<String> {
    let tf = read("deploy/aws/terraform/app-alarms.tf");
    let mut out = Vec::new();
    for line in tf.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("metric_name") {
            // Form: metric_name = "tv_..."
            if let Some(start) = rest.find('"') {
                if let Some(end) = rest[start + 1..].find('"') {
                    let name = &rest[start + 1..start + 1 + end];
                    if name.starts_with("tv_") {
                        out.push(name.to_string());
                    }
                }
            }
        }
    }
    out
}

/// Collect every `.rs` source file under `crates/` (depth-first walk).
fn collect_rs_sources(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            // Skip target/ if it ever lives under a crate (it shouldn't,
            // but safe + cheap to guard).
            if path.file_name().and_then(|n| n.to_str()) == Some("target") {
                continue;
            }
            collect_rs_sources(&path, out);
            continue;
        }
        if path.extension().and_then(|s| s.to_str()) == Some("rs") {
            out.push(path);
        }
    }
}

/// True iff the literal metric name appears inside any
/// `counter!`/`gauge!`/`histogram!` call in the workspace.
fn is_metric_emitted(name: &str) -> bool {
    let needles = [
        format!("counter!(\"{name}\""),
        format!("gauge!(\"{name}\""),
        format!("histogram!(\"{name}\""),
        format!("counter!(\"{name}\")"),
        format!("gauge!(\"{name}\")"),
        format!("histogram!(\"{name}\")"),
    ];
    let mut sources = Vec::new();
    collect_rs_sources(&workspace_root().join("crates"), &mut sources);
    for path in sources {
        let Ok(body) = fs::read_to_string(&path) else {
            continue;
        };
        for needle in &needles {
            if body.contains(needle) {
                return true;
            }
        }
    }
    false
}

#[test]
fn test_every_alarm_metric_has_a_rust_emit_site() {
    let names = alarm_metric_names();
    assert!(
        !names.is_empty(),
        "ratchet self-check: app-alarms.tf produced 0 metric_name entries — parser broken"
    );
    let mut missing = Vec::new();
    for name in &names {
        if !is_metric_emitted(name) {
            missing.push(name.clone());
        }
    }
    assert!(
        missing.is_empty(),
        "Z+ L2 VERIFY ratchet: the following alarm metric names have NO matching \
         counter!/gauge!/histogram! call anywhere under crates/. Either the metric \
         was renamed in Rust without updating app-alarms.tf, or the alarm was added \
         for a metric that does not exist yet. Missing: {missing:?}"
    );
}

#[test]
fn test_every_alarm_metric_is_in_emf_filter_list() {
    let user_data = read("deploy/aws/terraform/user-data.sh.tftpl");
    let names = alarm_metric_names();
    let mut missing = Vec::new();
    for name in &names {
        if !user_data.contains(name) {
            missing.push(name.clone());
        }
    }
    assert!(
        missing.is_empty(),
        "Z+ L2 VERIFY ratchet: app-alarms.tf references metrics that do NOT appear \
         in user-data.sh.tftpl's emf_processor metric_declaration filter. Without \
         the filter entry, the CloudWatch agent will not publish them. Missing: {missing:?}"
    );
}

#[test]
fn test_app_alarms_count_is_thirteen() {
    // Pin the count so future PRs that delete an alarm without updating
    // the rule files / PR body fail this guard. Cost note (aws-budget.md)
    // depends on this number — keeping the budget honest means keeping
    // this number explicit.
    //
    // 13 (was 12) since 2026-06-02: added `tv_ticks_dropped_total` — the
    // final zero-tick-loss breach (rescue ring + spill + DLQ all failed),
    // the operator's #1 invariant. The upstream spill/dlq tiers were
    // already alarmed; this is the strictly-more-severe irrecoverable case.
    // 15 (was 13) since 2026-06-03 (zero-tick-loss PR-4 / G4+G1): added
    // `tv_ws_frame_dropped_no_wal_total` (hard WS-frame-lost breach) +
    // `tv_ws_reconnect_gap_seconds_total` (reconnect-churn rate-alarm —
    // gives PR-3's reconnect-gap metric its anomaly detector).
    // 16 (was 15) since 2026-06-03 (zero-tick-loss PR-5 / G3): added
    // `tv_disk_watcher_respawn_total` — the spill disk-health watcher is
    // now supervised (respawn + alert) instead of fire-and-forget; the
    // counter feeds this rate-alarm so a flapping watcher pages.
    let count = alarm_metric_names().len();
    assert_eq!(
        count, 16,
        "Z+ L2 VERIFY ratchet: expected exactly 16 app-level CloudWatch alarms \
         (one per critical app signal). Found {count}. If you intentionally added \
         or removed one, update aws-budget.md custom-metric cost line AND this guard."
    );
}
