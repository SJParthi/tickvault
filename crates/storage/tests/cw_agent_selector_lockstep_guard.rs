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
//! build on drift, on the Trap-A liveness heartbeat
//! (`tv_rest_1m_fire_heartbeat`, 2026-07-15 — the re-pointed market-hours
//! liveness alarm's gauge) disappearing from the selector, or on any of the
//! 4 retired Groww-live metric names sneaking back in.

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
fn cw_agent_selector_ships_heartbeat_and_drops_groww_live_metrics() {
    // 2026-07-15 (Groww live-feed retirement, Trap-A lockstep): the selector
    // must ship the re-pointed liveness alarm's heartbeat gauge and must NOT
    // resurrect the 4 retired Groww-live names (a dead name in the EMF list
    // implies coverage no producer can ever publish again).
    let root = repo_root();
    let live = extract_selector_regex(
        &read(&root.join("deploy/aws/cloudwatch-agent.json")),
        "deploy/aws/cloudwatch-agent.json",
    );
    assert!(
        live.contains("tv_rest_1m_fire_heartbeat"),
        "cw-agent metric_selectors must ship tv_rest_1m_fire_heartbeat — the \
         market-hours liveness alarm (treat_missing_data=breaching) is blind \
         without it (false-SOS ~09:25 IST daily)"
    );
    for retired in [
        "tv_groww_ws_active",
        "tv_feed_last_tick_age_seconds",
        "tv_feed_sidecar_stall_restart_total",
        "tv_groww_exchange_lag_p99_seconds",
    ] {
        assert!(
            !live.contains(retired),
            "cw-agent metric_selectors resurrected retired Groww-live metric \
             {retired} (producer deleted 2026-07-15 — a dead name implies \
             coverage that can never be published again)"
        );
    }
}

// RETIRED (2026-07-15 — Groww live-feed retirement):
// groww_feed_down_alarms_exist_and_are_registered died with the two alarms
// it pinned — groww_ws_inactive + groww_stall_restart_storm left
// app-alarms.tf (their gauge/counter producers, the Groww bridge + sidecar
// stall watchdog, were deleted; dated notes in app-alarms.tf).

#[test]
fn resource_block_extractor_is_bounded_at_own_closing_brace() {
    // Anti-vacuous self-test (2026-07-06): the block extractor must stop at
    // the alarm's OWN closing brace — an overrunning window that swallows
    // the neighboring resource would let the neighbor's identical
    // `treat_missing_data` / `statistic` literals satisfy the assertions
    // after the alarm's own lines were deleted.
    let root = repo_root();
    let tf = read(&root.join("deploy/aws/terraform/app-alarms.tf"));
    for block_name in ["questdb_disconnected", "disk_watcher_respawn"] {
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
