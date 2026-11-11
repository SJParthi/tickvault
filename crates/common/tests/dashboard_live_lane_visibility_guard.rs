//! Pins that every metric we PAY to ship to CloudWatch is READ by an alarm.
//!
//! # History
//!
//! Between 2026-08-11 and 2026-08-15 the Dhan live lane had twenty-one metric
//! series added to the CloudWatch agent's EMF allowlist, each billed at about
//! $0.30 a month, and not one of them was charted or alarmed. This guard was
//! written then and required every shipped name to be either charted on the
//! CloudWatch operator dashboard or read by an alarm.
//!
//! # Since 2026-11-11 (operator Quote 53)
//!
//! The owner ticked "Trim CloudWatch metrics" and "Delete the CloudWatch
//! dashboard" (recorded in
//! `docs/claude-rules-full/project/daily-universe-scope-expansion-2026-05-27.md`,
//! Quote 53). The dashboard is gone, so a chart no longer counts as a reader:
//! CloudWatch receives ONLY the metrics an alarm reads. Every metric stays on
//! the box's local `/metrics`, and the metrics log group still receives every
//! scrape, so the log metric filters (the loss groups) are unaffected.
//!
//! What is pinned:
//! 1. every EMF-selected name is read by an alarm (`metric_name = "..."`);
//! 2. the CloudWatch dashboard does not come back;
//! 3. the selector still carries the kept pages' metrics (non-vacuity).
//!
//! The file keeps its old name so the test-count ratchet and the history in
//! other files that cite it stay valid.

use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("metric visibility guard: cannot canonicalize repo root")
}

fn read(rel: &str) -> String {
    std::fs::read_to_string(repo_root().join(rel))
        .unwrap_or_else(|e| panic!("metric visibility guard: {rel} must be readable: {e}"))
}

/// The CloudWatch agent config the box actually loads.
const AGENT_CONFIG: &str = "deploy/aws/cloudwatch-agent.json";

/// Shipped metrics excused from the "read by an alarm" rule. EMPTY since
/// Quote 53: a shipped metric nobody alarms on is money for nothing, and the
/// metric is still on the box's local `/metrics`. Adding an entry needs a
/// fresh dated owner quote first.
const SHIPPED_WITHOUT_AN_ALARM: &[(&str, &str)] = &[];

/// Extract the EMF `metric_selectors` names from the deployed agent config.
fn emf_selected_names(config: &str) -> Vec<String> {
    // The UNION across EVERY `metric_selectors` entry, not just the first.
    let mut names: Vec<String> = Vec::new();
    let mut rest = config;
    while let Some(idx) = rest.find("metric_selectors") {
        rest = &rest[idx + "metric_selectors".len()..];
        let Some(open) = rest.find("[\"") else { break };
        let after = &rest[open + 2..];
        let Some(close) = after.find('"') else { break };
        // Each selector is an anchored regex: `^(a|b|c)$` or `^single$`.
        for token in after[..close].split('|') {
            let token = token
                .trim()
                .trim_start_matches("^(")
                .trim_start_matches('^')
                .trim_end_matches(")$")
                .trim_end_matches('$')
                .trim();
            if token.starts_with("tv_") {
                names.push(token.to_string());
            }
        }
        rest = &after[close..];
    }
    assert!(
        !names.is_empty(),
        "no tv_* names parsed out of {AGENT_CONFIG} — the extractor is broken, \
         and every check in this file would pass or fail for the wrong reason"
    );
    names.sort();
    names.dedup();
    names
}

/// Every `.tf` file under deploy/aws/terraform, comments stripped, joined.
fn terraform_without_comments() -> String {
    let dir = repo_root().join("deploy/aws/terraform");
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", dir.display()))
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "tf"))
        .collect();
    files.sort();
    files
        .iter()
        .filter_map(|p| std::fs::read_to_string(p).ok())
        .flat_map(|body| {
            body.lines()
                .map(|l| l.split('#').next().unwrap_or("").to_string())
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// `metric_name = "<name>"`, whitespace-insensitive (HCL aligns `=` to the
/// longest key of its own block, so the spacing varies).
fn alarm_reads(terraform: &str, name: &str) -> bool {
    let wanted = format!("metric_name = \"{name}\"");
    terraform
        .lines()
        .any(|line| line.split_whitespace().collect::<Vec<_>>().join(" ") == wanted)
}

#[test]
fn every_published_metric_is_visible_somewhere() {
    let selected = emf_selected_names(&read(AGENT_CONFIG));
    let terraform = terraform_without_comments();
    assert!(
        terraform.contains("aws_cloudwatch_metric_alarm"),
        "the terraform scan found no alarm definitions at all — this check \
         would pass or fail for the wrong reason"
    );
    let unread: Vec<&String> = selected
        .iter()
        .filter(|n| !alarm_reads(&terraform, n))
        .filter(|n| !SHIPPED_WITHOUT_AN_ALARM.iter().any(|(m, _)| m == n))
        .collect();
    assert!(
        unread.is_empty(),
        "these metrics are SHIPPED to CloudWatch (about $0.30 a month each) but \
         no alarm reads them. Operator Quote 53 (2026-11-11): CloudWatch receives \
         only the metrics an alarm reads; the box's local /metrics keeps them all. \
         Remove them from {AGENT_CONFIG}:\n  {unread:?}"
    );
}

#[test]
fn nothing_is_charted_that_can_never_publish() {
    // Quote 53 deleted the CloudWatch dashboard (reversing Quote 37's
    // "dashboard KEPT"). A dashboard that comes back without a fresh dated
    // quote is a REJECT.
    assert!(
        !repo_root()
            .join("deploy/aws/terraform/dashboard.tf")
            .exists(),
        "deploy/aws/terraform/dashboard.tf is back; Quote 53 deleted the \
         CloudWatch dashboard"
    );
    assert!(
        !terraform_without_comments().contains("aws_cloudwatch_dashboard"),
        "a CloudWatch dashboard resource is declared again; Quote 53 deleted it"
    );
}

#[test]
fn the_lane_row_actually_exists_and_is_substantial() {
    // Non-vacuity: the first check passes trivially against an empty selector,
    // which would also silence every kept page that reads an EMF metric.
    let selected = emf_selected_names(&read(AGENT_CONFIG));
    for required in [
        "tv_dhan_feed_stack_up",
        "tv_dhan_ws_park_total",
        "tv_ticks_dropped_total",
        "tv_ticks_spilled_total",
        "tv_dhan_feed_last_tick_age_secs",
        "tv_token_remaining_seconds",
        "tv_questdb_disconnected_seconds",
        "tv_spill_dir_free_bytes",
    ] {
        assert!(
            selected.iter().any(|s| s == required),
            "{required} must stay EMF-selected: a page Quote 53 keeps reads it"
        );
    }
    assert!(
        selected.len() >= 30,
        "only {} names parsed from the selector — the extractor is not matching",
        selected.len()
    );
}

#[test]
fn every_excuse_carries_a_real_reason() {
    // Quote 53: no shipped metric goes unalarmed, so the excuse list is empty.
    // If an owner quote ever re-opens it, each entry still needs a reason.
    for (metric, reason) in SHIPPED_WITHOUT_AN_ALARM {
        assert!(reason.len() > 80, "{metric} is excused without a reason");
        assert!(metric.starts_with("tv_"), "{metric} is not a metric name");
    }
    assert!(
        SHIPPED_WITHOUT_AN_ALARM.is_empty(),
        "Quote 53 allows no shipped metric without an alarm"
    );
}
