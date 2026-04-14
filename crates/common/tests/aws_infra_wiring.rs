//! S10: Lockdown tests for the AWS deployment infrastructure files.
//!
//! These tests prevent accidental deletion or corruption of the
//! Phase 8 AWS deployment assets. They run on every commit via the
//! normal test suite and fail the build if any required file is
//! missing or structurally broken.
//!
//! Covered:
//!   - Terraform stack (6 files)
//!   - GitHub Actions deploy workflow
//!   - GitHub Actions nightly dep-freshness workflow
//!   - AWS Makefile targets (12 targets)
//!   - Two runbooks
//!   - systemd unit file

use std::path::Path;

fn workspace_root() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .map(std::path::PathBuf::from)
        .expect("workspace root must exist above crates/common") // APPROVED: test
}

fn require_file_exists(rel_path: &str, reason: &str) {
    let abs = workspace_root().join(rel_path);
    assert!(
        abs.exists(),
        "S10 LOCKDOWN: {rel_path} is missing. {reason}\n\
         Full path: {}",
        abs.display()
    );
}

// ---------------------------------------------------------------------------
// Terraform stack (Phase 8.1)
// ---------------------------------------------------------------------------

#[test]
fn test_terraform_stack_files_exist() {
    for (file, reason) in &[
        (
            "deploy/aws/terraform/versions.tf",
            "Terraform + AWS provider version pins",
        ),
        (
            "deploy/aws/terraform/variables.tf",
            "Input variables (region, instance_type, EBS, etc)",
        ),
        (
            "deploy/aws/terraform/main.tf",
            "VPC + EC2 + EIP + IAM + SNS + EventBridge",
        ),
        (
            "deploy/aws/terraform/outputs.tf",
            "instance_id + elastic_ip + ssm_session_command",
        ),
        (
            "deploy/aws/terraform/user-data.sh.tftpl",
            "First-boot script",
        ),
        (
            "deploy/aws/terraform/oidc.tf",
            "GitHub Actions OIDC role for deploy workflow",
        ),
        (
            "deploy/aws/terraform/alarms.tf",
            "5 CloudWatch alarms (Phase 8 infrastructure monitoring)",
        ),
        (
            "deploy/aws/terraform/README.md",
            "Operator quick-start for first apply",
        ),
    ] {
        require_file_exists(file, reason);
    }
}

#[test]
fn test_terraform_main_pins_to_ap_south_1() {
    let content =
        std::fs::read_to_string(workspace_root().join("deploy/aws/terraform/variables.tf"))
            .expect("variables.tf must be readable"); // APPROVED: test
    // The aws_region variable must default to ap-south-1 AND have a
    // validation that rejects anything else (Dhan static IP + Mumbai
    // latency are non-negotiable).
    assert!(
        content.contains("\"ap-south-1\""),
        "variables.tf must default aws_region to ap-south-1"
    );
    assert!(
        content.contains("var.aws_region == \"ap-south-1\""),
        "variables.tf must VALIDATE region pinning (not just default)"
    );
}

#[test]
fn test_terraform_instance_type_pinned() {
    let content =
        std::fs::read_to_string(workspace_root().join("deploy/aws/terraform/variables.tf"))
            .expect("variables.tf must be readable"); // APPROVED: test
    assert!(
        content.contains("\"c7i.xlarge\""),
        "variables.tf must pin instance_type to c7i.xlarge (budget cap)"
    );
    assert!(
        content.contains("var.instance_type == \"c7i.xlarge\""),
        "variables.tf must VALIDATE instance_type pinning"
    );
}

#[test]
fn test_terraform_eventbridge_schedules_match_budget() {
    let content = std::fs::read_to_string(workspace_root().join("deploy/aws/terraform/main.tf"))
        .expect("main.tf must be readable"); // APPROVED: test
    // Weekday start: 08:00 IST = 02:30 UTC Mon-Fri
    assert!(
        content.contains("cron(30 2 ? * MON-FRI *)"),
        "main.tf missing weekday start schedule (02:30 UTC Mon-Fri)"
    );
    // Weekday stop: 17:00 IST = 11:30 UTC Mon-Fri
    assert!(
        content.contains("cron(30 11 ? * MON-FRI *)"),
        "main.tf missing weekday stop schedule (11:30 UTC Mon-Fri)"
    );
    // Weekend start: 08:00 IST = 02:30 UTC Sat-Sun
    assert!(
        content.contains("cron(30 2 ? * SAT-SUN *)"),
        "main.tf missing weekend start schedule"
    );
    // Weekend stop: 13:00 IST = 07:30 UTC Sat-Sun
    assert!(
        content.contains("cron(30 7 ? * SAT-SUN *)"),
        "main.tf missing weekend stop schedule"
    );
}

#[test]
fn test_terraform_s3_lifecycle_matches_sebi_retention() {
    let content = std::fs::read_to_string(workspace_root().join("deploy/aws/terraform/main.tf"))
        .expect("main.tf must be readable"); // APPROVED: test
    // 5-year SEBI retention = 1825 days
    assert!(
        content.contains("days = 1825"),
        "main.tf S3 lifecycle must have 1825-day (5-year SEBI) expiration"
    );
    // 30-day transition to Intelligent-Tiering
    assert!(
        content.contains("INTELLIGENT_TIERING"),
        "main.tf S3 lifecycle must transition to Intelligent-Tiering"
    );
    // 1-year transition to Glacier IR
    assert!(
        content.contains("GLACIER_IR"),
        "main.tf S3 lifecycle must transition to Glacier IR"
    );
}

#[test]
fn test_terraform_oidc_role_restricts_to_repo() {
    let content = std::fs::read_to_string(workspace_root().join("deploy/aws/terraform/oidc.tf"))
        .expect("oidc.tf must be readable"); // APPROVED: test
    // OIDC role must bind to our specific repo (not a wildcard).
    assert!(
        content.contains("SJParthi/dhan-live-trader"),
        "oidc.tf must pin the GitHub repo full name"
    );
    assert!(
        content.contains("token.actions.githubusercontent.com"),
        "oidc.tf must reference the GitHub Actions OIDC audience"
    );
}

#[test]
fn test_terraform_alarms_publish_to_sns() {
    let content = std::fs::read_to_string(workspace_root().join("deploy/aws/terraform/alarms.tf"))
        .expect("alarms.tf must be readable"); // APPROVED: test
    // All 5 alarms must reference the SNS topic for Telegram fan-out.
    let alarm_count = content.matches("aws_sns_topic.dlt_alerts.arn").count();
    assert!(
        alarm_count >= 5,
        "alarms.tf must wire >= 5 alarms to dlt_alerts SNS (found {alarm_count})"
    );
}

// ---------------------------------------------------------------------------
// GitHub Actions workflows
// ---------------------------------------------------------------------------

#[test]
fn test_github_actions_workflows_exist() {
    require_file_exists(
        ".github/workflows/deploy-aws.yml",
        "Manual + tag-triggered AWS deploy workflow",
    );
    require_file_exists(
        ".github/workflows/dep-freshness-nightly.yml",
        "Nightly 02:00 IST dep-drift scan",
    );
}

#[test]
fn test_deploy_aws_workflow_has_market_hours_guard() {
    let content =
        std::fs::read_to_string(workspace_root().join(".github/workflows/deploy-aws.yml"))
            .expect("deploy-aws.yml must be readable"); // APPROVED: test
    assert!(
        content.contains("guard_market_hours"),
        "deploy-aws.yml must have guard_market_hours job"
    );
    // The 09:15-15:30 IST window in UTC is 03:45-10:00.
    assert!(
        content.contains("345") && content.contains("1000"),
        "deploy-aws.yml must encode the 09:15-15:30 IST window in UTC (345-1000)"
    );
}

#[test]
fn test_deploy_aws_workflow_uses_oidc() {
    let content =
        std::fs::read_to_string(workspace_root().join(".github/workflows/deploy-aws.yml"))
            .expect("deploy-aws.yml must be readable"); // APPROVED: test
    assert!(
        content.contains("id-token: write"),
        "deploy-aws.yml must request id-token write for OIDC"
    );
    assert!(
        content.contains("aws-actions/configure-aws-credentials"),
        "deploy-aws.yml must use aws-actions/configure-aws-credentials"
    );
    assert!(
        content.contains("role-to-assume"),
        "deploy-aws.yml must reference the OIDC role ARN"
    );
}

#[test]
fn test_dep_freshness_workflow_cron_schedule() {
    let content = std::fs::read_to_string(
        workspace_root().join(".github/workflows/dep-freshness-nightly.yml"),
    )
    .expect("dep-freshness-nightly.yml must be readable"); // APPROVED: test
    // 02:00 IST = 20:30 UTC previous day
    assert!(
        content.contains("30 20 * * *"),
        "dep-freshness-nightly.yml cron must be 20:30 UTC (02:00 IST)"
    );
}

// ---------------------------------------------------------------------------
// Makefile AWS targets (Phase 8.6 operator helpers)
// ---------------------------------------------------------------------------

#[test]
fn test_makefile_aws_targets_exist() {
    let content = std::fs::read_to_string(workspace_root().join("Makefile"))
        .expect("Makefile must be readable"); // APPROVED: test
    for target in &[
        "aws-bootstrap-check",
        "aws-ami",
        "aws-operator-cidr",
        "aws-keypair",
        "aws-init",
        "aws-plan",
        "aws-apply",
        "aws-outputs",
        "aws-ssm",
        "aws-ssm-command",
        "aws-cost",
    ] {
        assert!(
            content.contains(&format!("{target}:")),
            "Makefile missing target: {target}"
        );
    }
}

#[test]
fn test_makefile_aws_apply_requires_tf_vars() {
    let content = std::fs::read_to_string(workspace_root().join("Makefile"))
        .expect("Makefile must be readable"); // APPROVED: test
    assert!(
        content.contains("TF_VAR_ami_id") && content.contains("TF_VAR_operator_cidr"),
        "Makefile aws-apply target must guard against missing TF_VAR_* env vars"
    );
}

// ---------------------------------------------------------------------------
// Runbooks (Phase 8.3 + 8.4)
// ---------------------------------------------------------------------------

#[test]
fn test_runbooks_exist() {
    require_file_exists(
        "docs/runbooks/aws-deploy.md",
        "First-time AWS setup runbook",
    );
    require_file_exists(
        "docs/runbooks/aws-disaster-recovery.md",
        "Incident DR runbook",
    );
}

#[test]
fn test_dr_runbook_has_11_sections() {
    let content =
        std::fs::read_to_string(workspace_root().join("docs/runbooks/aws-disaster-recovery.md"))
            .expect("DR runbook must be readable"); // APPROVED: test
    // The runbook has 11 incident sections §1 through §11.
    for n in 1..=11 {
        assert!(
            content.contains(&format!("## §{n}.")),
            "DR runbook missing §{n}. section"
        );
    }
}

// ---------------------------------------------------------------------------
// systemd unit (Phase 8 supervisor layer)
// ---------------------------------------------------------------------------

#[test]
fn test_systemd_unit_exists() {
    require_file_exists(
        "deploy/systemd/dlt-app.service",
        "systemd unit for auto-restart and sd_notify watchdog",
    );
}

#[test]
fn test_systemd_unit_restart_policy() {
    let content = std::fs::read_to_string(workspace_root().join("deploy/systemd/dlt-app.service"))
        .expect("systemd unit must be readable"); // APPROVED: test
    assert!(
        content.contains("Restart=always") || content.contains("Restart=on-failure"),
        "systemd unit must auto-restart on crash"
    );
    assert!(
        content.contains("WatchdogSec="),
        "systemd unit must configure sd_notify watchdog"
    );
    assert!(
        content.contains("KillSignal=SIGTERM"),
        "systemd unit must send SIGTERM (graceful shutdown handler)"
    );
}
