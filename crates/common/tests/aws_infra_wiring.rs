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
    // Operator-lock 2026-05-29 (daily-universe-scope-expansion-2026-05-27.md §7
    // Quote 5): m8g.large ONLY (Graviton4, 8 GiB). SUPERSEDES the 2026-05-18
    // t4g.medium + 2026-05-27 t4g.large locks. Any reintroduction of
    // c7i.xlarge / c8g.xlarge / t4g.* as the PINNED type fails this test.
    assert!(
        content.contains("\"m8g.large\""),
        "variables.tf must pin instance_type to m8g.large (operator lock 2026-05-29, see daily-universe-scope-expansion-2026-05-27.md §7)"
    );
    assert!(
        content.contains("var.instance_type == \"m8g.large\""),
        "variables.tf must VALIDATE instance_type pinning"
    );
    // Negative asserts — block the retired stacks from ever returning as the
    // validated default (t4g.medium/t4g.large may still appear in SUPERSEDES
    // comments, so we only forbid them as the validation condition).
    assert!(
        !content.contains("var.instance_type == \"t4g.medium\""),
        "t4g.medium retired as the pinned type (operator lock 2026-05-29)"
    );
    assert!(
        !content.contains("c7i.xlarge"),
        "c7i.xlarge retired in operator-lock 2026-05-18"
    );
    assert!(
        !content.contains("c8g.xlarge"),
        "c8g.xlarge retired in operator-lock 2026-05-18"
    );
}

#[test]
fn test_terraform_instance_iam_allows_staging_ssm_prefix() {
    // The app reads its secrets under the SSM prefix selected by
    // TV_ENVIRONMENT (systemd unit) = "staging" for the 3-month data-pull
    // phase. The instance IAM role is named with var.environment (prod), so
    // its SSM Resource must EXPLICITLY also allow /tickvault/staging/* —
    // otherwise the box is IAM-denied reading /tickvault/staging/dhan/* and
    // boot fails at auth. Regression: 2026-05-30 pre-flight audit caught the
    // app(staging) vs IAM(prod) SSM-prefix mismatch before first deploy.
    let content = std::fs::read_to_string(workspace_root().join("deploy/aws/terraform/main.tf"))
        .expect("main.tf must be readable"); // APPROVED: test
    assert!(
        content.contains("parameter/tickvault/staging/*"),
        "main.tf instance IAM role must allow ssm:GetParameter on \
         /tickvault/staging/* — the app's TV_ENVIRONMENT=staging SSM prefix. \
         Without it the box is IAM-denied reading its Dhan/Telegram secrets."
    );
    // The systemd unit must agree — TV_ENVIRONMENT=staging is the source of the
    // staging SSM prefix the IAM line above grants.
    let unit = std::fs::read_to_string(workspace_root().join("deploy/systemd/tickvault.service"))
        .expect("tickvault.service must be readable"); // APPROVED: test
    assert!(
        unit.contains("TV_ENVIRONMENT=staging"),
        "tickvault.service must set TV_ENVIRONMENT=staging (data-pull, no real \
         orders). If this flips to prod, the IAM staging grant + this test must \
         be revisited together (production.toml arms real money)."
    );
}

#[test]
fn test_terraform_eventbridge_schedules_match_budget() {
    let content = std::fs::read_to_string(workspace_root().join("deploy/aws/terraform/main.tf"))
        .expect("main.tf must be readable"); // APPROVED: test
    // Weekday start: 08:30 IST = 03:00 UTC Mon-Fri (operator-lock 2026-05-29,
    // daily-universe-scope-expansion-2026-05-27.md §7 Quote 5 — trading
    // weekdays only, 08:30-16:30 IST; SUPERSEDES the 2026-05-18 7-day cron).
    assert!(
        content.contains("cron(0 3 ? * MON-FRI *)"),
        "main.tf missing weekday start schedule (03:00 UTC Mon-Fri = 08:30 IST)"
    );
    // Weekday stop: 16:30 IST = 11:00 UTC Mon-Fri.
    assert!(
        content.contains("cron(0 11 ? * MON-FRI *)"),
        "main.tf missing weekday stop schedule (11:00 UTC Mon-Fri = 16:30 IST)"
    );
    // Negative asserts — the prior 7-day daily crons (02:30/11:30 UTC every
    // day) were replaced by the weekday-only pair per operator lock 2026-05-29.
    assert!(
        !content.contains("cron(30 2 * * ? *)"),
        "7-day daily start cron retired in operator-lock 2026-05-29 (weekday-only now)"
    );
    assert!(
        !content.contains("cron(30 11 * * ? *)"),
        "7-day daily stop cron retired in operator-lock 2026-05-29 (weekday-only now)"
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
        content.contains("SJParthi/tickvault"),
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
    let alarm_count = content.matches("aws_sns_topic.tv_alerts.arn").count();
    assert!(
        alarm_count >= 5,
        "alarms.tf must wire >= 5 alarms to tv_alerts SNS (found {alarm_count})"
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
        "deploy/systemd/tickvault.service",
        "systemd unit for auto-restart and sd_notify watchdog",
    );
}

#[test]
fn test_systemd_unit_restart_policy() {
    let content =
        std::fs::read_to_string(workspace_root().join("deploy/systemd/tickvault.service"))
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
