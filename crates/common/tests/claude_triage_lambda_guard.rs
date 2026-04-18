//! Phase 8.2 guard — the claude-triage Lambda scaffold must stay wired.
//!
//! Pins the structural invariants of the CloudWatch-alarm → SNS →
//! Lambda → SSM RunCommand → claude CLI chain. The Lambda is OFF by
//! default (`enable_claude_triage_lambda = false`) so terraform creates
//! nothing — this guard just ensures the scaffolding is coherent
//! enough to activate on day-1 when the operator provisions EC2.

use std::fs;
use std::path::{Path, PathBuf};

const HANDLER_PY: &str = "deploy/aws/lambda/claude-triage/handler.py";
const README_MD: &str = "deploy/aws/lambda/claude-triage/README.md";
const LAMBDA_TF: &str = "deploy/aws/terraform/claude-triage-lambda.tf";

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
fn claude_triage_lambda_files_exist() {
    for rel in [HANDLER_PY, README_MD, LAMBDA_TF] {
        let path = workspace_root().join(rel);
        assert!(
            path.exists(),
            "{} missing — Phase 8.2 scaffold is broken",
            path.display()
        );
    }
}

#[test]
fn handler_exposes_lambda_handler_entry() {
    let src = load_text(HANDLER_PY);
    assert!(
        src.contains("def lambda_handler"),
        "handler.py missing lambda_handler entry"
    );
    assert!(
        src.contains("def _parse_sns_alarm"),
        "handler.py missing _parse_sns_alarm"
    );
    assert!(
        src.contains("def _build_claude_prompt"),
        "handler.py missing _build_claude_prompt"
    );
    assert!(
        src.contains("def _invoke_claude_via_ssm"),
        "handler.py missing _invoke_claude_via_ssm"
    );
}

#[test]
fn handler_uses_only_boto3_and_stdlib() {
    let src = load_text(HANDLER_PY);
    // boto3 is pre-installed on AWS Lambda Python runtime, so it's
    // allowed. No other non-stdlib imports permitted (keeps the deploy
    // package small — AWS Lambda has a 50 MB uploaded size limit).
    let allow_list = [
        "from __future__",
        "import json",
        "import logging",
        "import os",
        "import boto3",
        "from typing",
    ];
    for line in src.lines() {
        let trimmed = line.trim_start();
        if !(trimmed.starts_with("import ") || trimmed.starts_with("from ")) {
            continue;
        }
        if line != trimmed {
            continue; // skip indented imports
        }
        let ok = allow_list.iter().any(|a| trimmed.starts_with(a));
        assert!(
            ok,
            "handler.py has a non-allowed top-level import: `{trimmed}`. \
             Lambda deploy package must stay slim — stdlib + boto3 only."
        );
    }
}

#[test]
fn terraform_gates_lambda_behind_opt_in_variable() {
    let src = load_text(LAMBDA_TF);
    assert!(
        src.contains("variable \"enable_claude_triage_lambda\""),
        "claude-triage-lambda.tf missing enable_claude_triage_lambda variable"
    );
    assert!(
        src.contains("default     = false") || src.contains("default = false"),
        "enable_claude_triage_lambda must default to false so resources \
         are NOT created until the operator opts in"
    );
}

#[test]
fn terraform_every_resource_is_count_gated() {
    // Every aws_* resource in this file MUST use `count = var.enable_... ? 1 : 0`
    // — otherwise terraform creates it unconditionally.
    let src = load_text(LAMBDA_TF);
    let mut offending: Vec<String> = Vec::new();

    // Find every `resource "<type>" "<name>" {` block and verify the
    // next ~5 lines contain a `count =` gate.
    for (idx, line) in src.lines().enumerate() {
        let trimmed = line.trim_start();
        if !trimmed.starts_with("resource \"") {
            continue;
        }
        // Scan next 6 lines for the count gate.
        let window: String = src.lines().skip(idx).take(6).collect::<Vec<_>>().join("\n");
        if !window.contains("count") {
            offending.push(trimmed.to_string());
        }
    }
    assert!(
        offending.is_empty(),
        "claude-triage-lambda.tf has resources without a `count` gate:\n  {}",
        offending.join("\n  ")
    );
}

#[test]
fn terraform_references_existing_sns_topic() {
    let src = load_text(LAMBDA_TF);
    // Must subscribe to aws_sns_topic.tv_alerts which is defined in main.tf.
    assert!(
        src.contains("aws_sns_topic.tv_alerts.arn"),
        "claude-triage Lambda must subscribe to the existing tv_alerts SNS topic"
    );
}

#[test]
fn terraform_iam_role_uses_scoped_permissions() {
    let src = load_text(LAMBDA_TF);
    // The IAM role must NOT grant wildcard actions like "*" or "ssm:*".
    // Wildcard permissions are a security anti-pattern. The role should
    // only grant ssm:SendCommand/ssm:GetCommandInvocation + CloudWatch
    // logs write.
    for banned in ["\"*\"", "\"ssm:*\"", "\"ec2:*\"", "\"iam:*\""] {
        assert!(
            !src.contains(banned),
            "claude-triage IAM role grants banned wildcard permission {banned}"
        );
    }
    assert!(
        src.contains("ssm:SendCommand"),
        "claude-triage IAM role must grant ssm:SendCommand"
    );
}
