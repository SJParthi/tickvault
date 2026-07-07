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

/// Extract ONE terraform resource block by name, bounded at ITS closing
/// brace (a top-level `\n}`) — never a fixed byte window, which overruns
/// into the neighboring resource and lets the neighbor's identical literals
/// satisfy assertions vacuously (the 2026-07-06 mutation-verified hole).
fn resource_block<'a>(tf: &'a str, block_name: &str) -> &'a str {
    let at = tf
        .find(&format!(
            "resource \"aws_cloudwatch_metric_alarm\" \"{block_name}\""
        ))
        .unwrap_or_else(|| panic!("app-alarms.tf: resource {block_name} not found"));
    let end = tf[at..].find("\n}").map_or(tf.len(), |rel| at + rel + 2);
    &tf[at..end]
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
    // The scan is bounded at the resource block's OWN closing brace
    // (2026-07-06 fix): a fixed byte window overran into the NEIGHBOR
    // resource, whose identical `treat_missing_data` literal satisfied the
    // assertion even after the alarm's own line was deleted (mutation-
    // verified vacuous pass — terraform then defaults to "missing", the
    // exact 2026-06-02 stuck-FIRING class this guard claims to prevent).
    for block_name in ["groww_ws_inactive", "groww_stall_restart_storm"] {
        let block = resource_block(&tf, block_name);
        assert!(
            block.contains("treat_missing_data  = \"notBreaching\""),
            "app-alarms.tf: {block_name} must use treat_missing_data = notBreaching \
             (the 2026-06-02 stuck-FIRING fix class)"
        );
    }

    // Delta-semantics honesty (2026-07-06 fix): the CloudWatch agent's
    // Prometheus/EMF pipeline DELTA-converts counter metrics (each datapoint
    // is the per-scrape increase, never the cumulative count), so the storm
    // alarm must aggregate with Sum over a window — a Maximum-vs-cumulative
    // threshold >1 can NEVER fire on its documented condition and un-fires
    // mid-storm once the in-process backoff throttles kills apart.
    {
        let block = resource_block(&tf, "groww_stall_restart_storm");
        assert!(
            block.contains("statistic           = \"Sum\""),
            "app-alarms.tf: groww_stall_restart_storm must use statistic = Sum — \
             the CW agent delta-converts Prometheus counters, so Maximum >= 3 \
             never sees a cumulative count (behaviorally dead alarm)"
        );
        assert!(
            !block.contains("\"Maximum\""),
            "app-alarms.tf: groww_stall_restart_storm must NOT regress to Maximum \
             on a delta-converted counter"
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
fn resource_block_extractor_is_bounded_at_own_closing_brace() {
    // Anti-vacuous self-test (2026-07-06): the block extractor must stop at
    // the alarm's OWN closing brace — an overrunning window that swallows
    // the neighboring resource would let the neighbor's identical
    // `treat_missing_data` / `statistic` literals satisfy the assertions
    // after the alarm's own lines were deleted.
    let root = repo_root();
    let tf = read(&root.join("deploy/aws/terraform/app-alarms.tf"));
    for block_name in ["groww_ws_inactive", "groww_stall_restart_storm"] {
        let block = resource_block(&tf, block_name);
        assert_eq!(
            block
                .matches("resource \"aws_cloudwatch_metric_alarm\"")
                .count(),
            1,
            "{block_name}: extracted block must contain exactly ONE resource \
             declaration (no neighbor overrun): {block}"
        );
        assert!(
            block.trim_end().ends_with('}'),
            "{block_name}: extracted block must end at its own closing brace"
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
