//! Every Lambda in the terraform has a declared log group with a retention
//! (audit PR M / M4, 2026-10-26).
//!
//! A Lambda with no declared log group gets one from the runtime on its first
//! invoke, with "never expire" retention, and nothing in terraform shows it.
//! Read live on 2026-10-26: `/aws/lambda/tv-prod-questdb-console-front` and
//! `/aws/lambda/tv-prod-questdb-console-proxy` had no retention. The same
//! shape was found for `operator-control` on 2026-07-03 (`log-retention.tf`).
//! This guard stops the next Lambda arriving the same way.
//!
//! HONEST LIMIT: a source scan of `deploy/aws/terraform/*.tf`. It reads a
//! `function_name` written as `"tv-${var.environment}-<name>"` and does not
//! see a Lambda named any other way.

use std::path::{Path, PathBuf};

fn terraform_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../deploy/aws/terraform")
}

fn terraform_sources() -> Vec<(String, String)> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(terraform_dir())
        .expect("terraform dir")
        .flatten()
    {
        let path = entry.path();
        if path.extension().is_some_and(|e| e == "tf") {
            let text = std::fs::read_to_string(&path).expect("read .tf");
            out.push((path.display().to_string(), text));
        }
    }
    out
}

const PREFIX: &str = "\"tv-${var.environment}-";

/// The `<name>` of every `function_name = "tv-${var.environment}-<name>"`.
fn lambda_names(sources: &[(String, String)]) -> Vec<String> {
    let mut names = Vec::new();
    for (_, text) in sources {
        for line in text.lines() {
            let line = line.trim();
            if !line.starts_with("function_name") {
                continue;
            }
            if let Some(rest) = line.split_once(PREFIX).map(|(_, r)| r) {
                if let Some(name) = rest.split('"').next() {
                    names.push(name.to_owned());
                }
            }
        }
    }
    names.sort();
    names.dedup();
    names
}

/// Bodies of every `resource "aws_cloudwatch_log_group"` block.
fn log_group_blocks(sources: &[(String, String)]) -> Vec<String> {
    let mut blocks = Vec::new();
    for (_, text) in sources {
        let mut rest = text.as_str();
        while let Some(at) = rest.find("resource \"aws_cloudwatch_log_group\"") {
            let tail = &rest[at..];
            let end = tail.find("\n}\n").map_or(tail.len(), |e| e + 3);
            blocks.push(tail[..end].to_owned());
            rest = &tail[end..];
        }
    }
    blocks
}

fn has_retained_group(blocks: &[String], lambda: &str) -> bool {
    let wanted = format!("\"/aws/lambda/tv-${{var.environment}}-{lambda}\"");
    blocks
        .iter()
        .any(|b| b.contains(&wanted) && b.contains("retention_in_days"))
}

// Regression: 2026-10-26 — the two QuestDB console Lambdas had no declared log
// group and kept their logs forever. Bite-checked by removing their two blocks
// from log-retention.tf: this test fails naming both.
#[test]
fn test_regression_every_lambda_has_a_retained_log_group() {
    let sources = terraform_sources();
    let names = lambda_names(&sources);
    assert!(
        names.len() >= 10,
        "the scan found {} Lambdas; the terraform has at least ten, so the scan is broken",
        names.len()
    );
    let blocks = log_group_blocks(&sources);
    let missing: Vec<&String> = names
        .iter()
        .filter(|n| !has_retained_group(&blocks, n))
        .collect();
    assert!(
        missing.is_empty(),
        "Lambdas with no declared log group carrying retention_in_days (the runtime \
         would create one that never expires): {missing:?}"
    );
}

#[test]
fn test_the_questdb_console_groups_are_declared_with_retention() {
    let blocks = log_group_blocks(&terraform_sources());
    for lambda in ["questdb-console-front", "questdb-console-proxy"] {
        assert!(has_retained_group(&blocks, lambda), "{lambda}");
    }
}

#[test]
fn test_scan_helpers_on_a_fixture() {
    let fixture = vec![(
        "x.tf".to_owned(),
        "resource \"aws_lambda_function\" \"a\" {\n  function_name = \"tv-${var.environment}-alpha\"\n}\n\
         resource \"aws_cloudwatch_log_group\" \"a\" {\n  name = \"/aws/lambda/tv-${var.environment}-alpha\"\n}\n"
            .to_owned(),
    )];
    assert_eq!(lambda_names(&fixture), vec!["alpha".to_owned()]);
    let blocks = log_group_blocks(&fixture);
    assert_eq!(blocks.len(), 1);
    assert!(!has_retained_group(&blocks, "alpha"), "no retention line");
}
