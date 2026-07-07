//! CloudWatch-agent metric-selector LOCKSTEP guard + Groww feed-down alarm
//! pins (operator directive 2026-07-06 — Groww feed-down alerting).
//!
//! The cw-agent `metric_selectors` regex exists in TWO tracked copies that
//! MUST stay byte-identical:
//!   1. `deploy/aws/cloudwatch-agent.json` — the LIVE copy, shipped to the
//!      box on EVERY deploy (`deploy-aws.yml` copies it + runs fetch-config)
//!   2. `deploy/aws/terraform/user-data.sh.tftpl` — the first-boot heredoc
//!
//! If the two drift, a fresh instance selects a DIFFERENT metric set than a
//! deployed one — an alarm can silently point at a metric that never reaches
//! CloudWatch (false-OK class, audit-findings Rule 11). This guard fails the
//! build on drift, on any of the three 2026-07-06 metric names disappearing,
//! or on the two Groww feed-down alarms (or their output-list registrations)
//! being removed from `app-alarms.tf`.

#![cfg(test)]

use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates/storage parent")
        .parent()
        .expect("repo root")
        .to_path_buf()
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {} failed: {e}", path.display()))
}

/// Extract the `metric_selectors` regex literal (`"^(...)$"`) from a file.
fn extract_selector_regex(content: &str, name: &str) -> String {
    let marker = "\"metric_selectors\":";
    let at = content
        .find(marker)
        .unwrap_or_else(|| panic!("{name}: no metric_selectors key found"));
    let rest = &content[at + marker.len()..];
    let start = rest
        .find("\"^(")
        .unwrap_or_else(|| panic!("{name}: metric_selectors regex literal not found"));
    let tail = &rest[start + 1..];
    let end = tail
        .find(")$\"")
        .unwrap_or_else(|| panic!("{name}: metric_selectors regex not `)$`-anchored"));
    tail[..end + 2].to_string()
}

#[test]
fn cw_agent_selector_copies_are_byte_identical() {
    let root = repo_root();
    let live = extract_selector_regex(
        &read(&root.join("deploy/aws/cloudwatch-agent.json")),
        "deploy/aws/cloudwatch-agent.json",
    );
    let boot = extract_selector_regex(
        &read(&root.join("deploy/aws/terraform/user-data.sh.tftpl")),
        "deploy/aws/terraform/user-data.sh.tftpl",
    );
    assert_eq!(
        live, boot,
        "cw-agent metric_selectors drifted between deploy/aws/cloudwatch-agent.json \
         (live, per-deploy) and deploy/aws/terraform/user-data.sh.tftpl (first-boot) — \
         the two copies MUST stay byte-identical or a fresh instance selects a \
         different metric set than a deployed one"
    );
}

#[test]
fn cw_agent_selector_ships_groww_feed_down_metrics() {
    let root = repo_root();
    let live = extract_selector_regex(
        &read(&root.join("deploy/aws/cloudwatch-agent.json")),
        "deploy/aws/cloudwatch-agent.json",
    );
    for name in [
        "tv_groww_ws_active",
        "tv_feed_last_tick_age_seconds",
        "tv_feed_sidecar_stall_restart_total",
    ] {
        assert!(
            live.contains(name),
            "cw-agent metric_selectors must ship {name} (2026-07-06 groww feed-down \
             alerting) — an alarm on a metric that never reaches CloudWatch is a \
             false-OK"
        );
    }
}

#[test]
fn groww_feed_down_alarms_exist_and_are_registered() {
    let root = repo_root();
    let tf = read(&root.join("deploy/aws/terraform/app-alarms.tf"));

    // Alarm resources + their metric wiring.
    for (resource, alarm_name, metric) in [
        (
            "resource \"aws_cloudwatch_metric_alarm\" \"groww_ws_inactive\"",
            "tv-${var.environment}-groww-ws-inactive",
            "tv_groww_ws_active",
        ),
        (
            "resource \"aws_cloudwatch_metric_alarm\" \"groww_stall_restart_storm\"",
            "tv-${var.environment}-groww-stall-restart-storm",
            "tv_feed_sidecar_stall_restart_total",
        ),
    ] {
        assert!(
            tf.contains(resource),
            "app-alarms.tf: missing {resource} (2026-07-06 groww feed-down alerting)"
        );
        assert!(
            tf.contains(alarm_name),
            "app-alarms.tf: missing alarm_name {alarm_name}"
        );
        assert!(
            tf.contains(metric),
            "app-alarms.tf: alarm must point at metric {metric}"
        );
    }

    // Missing-data honesty: both alarms must never stick FIRING across the
    // scheduled 16:30 IST box stop / weekends / deploy gaps.
    for block_name in ["groww_ws_inactive", "groww_stall_restart_storm"] {
        let at = tf
            .find(&format!(
                "resource \"aws_cloudwatch_metric_alarm\" \"{block_name}\""
            ))
            .expect("resource located above");
        let block = &tf[at..(at + 1500).min(tf.len())];
        assert!(
            block.contains("treat_missing_data  = \"notBreaching\""),
            "app-alarms.tf: {block_name} must use treat_missing_data = notBreaching \
             (the 2026-06-02 stuck-FIRING fix class)"
        );
    }

    // Output-list registration (the operator-facing alarm inventory).
    for entry in [
        "aws_cloudwatch_metric_alarm.groww_ws_inactive.alarm_name",
        "aws_cloudwatch_metric_alarm.groww_stall_restart_storm.alarm_name",
    ] {
        assert!(
            tf.contains(entry),
            "app-alarms.tf: {entry} must be registered in the app_cloudwatch_alarms output"
        );
    }
}

#[test]
fn selector_extractor_is_not_vacuous() {
    // Anti-vacuous self-test: the extractor must actually pull a `tv_`-metric
    // alternation, not an empty string (a broken extractor comparing "" == ""
    // would pass the lockstep test forever).
    let root = repo_root();
    let live = extract_selector_regex(
        &read(&root.join("deploy/aws/cloudwatch-agent.json")),
        "deploy/aws/cloudwatch-agent.json",
    );
    assert!(
        live.starts_with("^(") && live.ends_with(")$"),
        "extractor must return the anchored regex, got: {live}"
    );
    assert!(
        live.matches("tv_").count() >= 10,
        "selector regex should carry the tv_ metric alternation, got: {live}"
    );
}
