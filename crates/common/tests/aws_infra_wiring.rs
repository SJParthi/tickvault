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
    // Operator-lock 2026-06-30 (daily-universe-scope-expansion-2026-05-27.md §7
    // Quote 7): r8g.large ONLY (Graviton4, 16 GiB) — DOUBLED RAM from m8g.large
    // for the both-feeds + larger-universe workload. SUPERSEDES the 2026-05-29
    // m8g.large + 2026-05-27 t4g.large + 2026-05-18 t4g.medium locks. Any
    // reintroduction of c7i.xlarge / c8g.xlarge / m8g.large / t4g.* as the
    // PINNED type fails this test.
    assert!(
        content.contains("\"r8g.large\""),
        "variables.tf must pin instance_type to r8g.large (operator lock 2026-06-30, see daily-universe-scope-expansion-2026-05-27.md §7)"
    );
    assert!(
        content.contains("var.instance_type == \"r8g.large\""),
        "variables.tf must VALIDATE instance_type pinning"
    );
    // Negative asserts — block the retired stacks from ever returning as the
    // validated default (m8g.large/t4g.medium/t4g.large may still appear in
    // SUPERSEDES comments, so we only forbid them as the validation condition).
    assert!(
        !content.contains("var.instance_type == \"m8g.large\""),
        "m8g.large retired as the pinned type (operator lock 2026-06-30, doubled RAM to r8g.large)"
    );
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
fn test_deploy_aws_does_not_auto_seed_prod_secrets() {
    // Single-prod-env consolidation (operator 2026-06-30): the operator
    // populates /tickvault/prod/* MANUALLY. The deploy workflow MUST NOT
    // auto-seed prod — the retired seed-staging-ssm workflow + the deploy's
    // auto-seed step are gone. The deploy's prod-secret step is READ-ONLY:
    // it never copies from another env and never overwrites an operator value.
    assert!(
        !workspace_root()
            .join(".github/workflows/seed-staging-ssm.yml")
            .exists(),
        "the staging-secret auto-seed workflow must be retired (single prod env)"
    );
    let wf = std::fs::read_to_string(workspace_root().join(".github/workflows/deploy-aws.yml"))
        .expect("deploy-aws.yml must be readable"); // APPROVED: test
    // No auto-seed: the deploy must NOT INVOKE `aws ssm put-parameter` anywhere
    // (it only NAMES the manual command in an operator-facing warning string).
    // A real invocation would clobber operator-owned /tickvault/prod/* values.
    assert!(
        !wf.contains("aws ssm put-parameter --"),
        "deploy-aws.yml must NOT invoke `aws ssm put-parameter` — the operator \
         populates /tickvault/prod/* manually; the deploy is read-only there."
    );
    // The read-only prod pre-flight must exist and target the prod prefix.
    assert!(
        wf.contains("/tickvault/prod/$key"),
        "deploy-aws.yml must read-only pre-flight the /tickvault/prod/* boot-halt keys"
    );
}

#[test]
fn test_cloudwatch_operator_dashboard_exists() {
    // The CloudWatch-only runtime has NO Grafana — the operator dashboard is a
    // native CloudWatch dashboard (deploy/aws/terraform/dashboard.tf). It must
    // exist + chart only metrics the CW agent actually scrapes (the allowlist
    // in user-data prometheus.yaml) + reference real alarm resources, so a
    // terraform apply doesn't fail and no widget is empty.
    let dash = std::fs::read_to_string(workspace_root().join("deploy/aws/terraform/dashboard.tf"))
        .expect("dashboard.tf must be readable"); // APPROVED: test
    assert!(
        dash.contains("resource \"aws_cloudwatch_dashboard\" \"operator\""),
        "dashboard.tf must declare the operator CloudWatch dashboard"
    );
    // A few signal metrics that MUST be charted (and are in the scrape allowlist).
    for metric in &[
        "tv_realtime_guarantee_score",
        "tv_questdb_disconnected_seconds",
        "tv_token_remaining_seconds",
        "tv_aggregator_seals_emitted_total",
    ] {
        assert!(
            dash.contains(metric),
            "operator dashboard must chart {metric} (a scraped Tickvault/Prod metric)"
        );
    }
    // The dashboard_url output lets the operator jump straight to it.
    let outputs = std::fs::read_to_string(workspace_root().join("deploy/aws/terraform/outputs.tf"))
        .expect("outputs.tf must be readable"); // APPROVED: test
    assert!(
        outputs.contains("dashboard_url"),
        "outputs.tf must expose dashboard_url for one-click console access"
    );
}

#[test]
fn test_single_prod_env_iam_systemd_and_dry_run_locked() {
    // Single-prod-env consolidation (operator 2026-06-30): dev/staging were
    // collapsed into ONE real env, `prod`. The app reads its secrets under the
    // SSM prefix selected by TV_ENVIRONMENT (systemd unit) = "prod", and the
    // instance IAM role (named with var.environment = prod) grants
    // /tickvault/${var.environment}/* — so the box can read /tickvault/prod/*.
    // The retired /tickvault/staging/* grant must be GONE.
    //
    // Hard safety invariant (the operator has insisted on this dozens of times):
    // this is the NO-real-orders data-pull phase, so production.toml MUST lock
    // dry_run = true. This test fails the build if a future edit flips
    // production.toml back to dry_run = false (which would arm real orders).
    let main_tf = std::fs::read_to_string(workspace_root().join("deploy/aws/terraform/main.tf"))
        .expect("main.tf must be readable"); // APPROVED: test
    // IAM grants the prod prefix via var.environment (= prod) and must NOT
    // retain a literal staging grant.
    assert!(
        main_tf.contains("parameter/tickvault/${var.environment}/*"),
        "main.tf instance IAM role must allow ssm:GetParameter on \
         /tickvault/${{var.environment}}/* (= /tickvault/prod/*) — the app's \
         TV_ENVIRONMENT=prod SSM prefix."
    );
    assert!(
        !main_tf.contains("parameter/tickvault/staging/*"),
        "the retired /tickvault/staging/* IAM grant must be removed \
         (single prod env, operator 2026-06-30)."
    );

    // The systemd unit must select the single prod env.
    let unit = std::fs::read_to_string(workspace_root().join("deploy/systemd/tickvault.service"))
        .expect("tickvault.service must be readable"); // APPROVED: test
    assert!(
        unit.contains("TV_ENVIRONMENT=prod"),
        "tickvault.service must set TV_ENVIRONMENT=prod (single real env). \
         NO real orders — production.toml locks dry_run=true."
    );
    assert!(
        !unit.contains("TV_ENVIRONMENT=staging"),
        "the retired TV_ENVIRONMENT=staging must be removed from tickvault.service"
    );

    // HARD SAFETY: production.toml locks dry_run = true (NO real orders). A
    // build-failing assertion so dry_run can never silently flip to false again.
    let prod_cfg = std::fs::read_to_string(workspace_root().join("config/production.toml"))
        .expect("config/production.toml must be readable"); // APPROVED: test
    assert!(
        prod_cfg.contains("dry_run = true"),
        "config/production.toml MUST set dry_run = true — this is the no-real-orders \
         data-pull phase. Flipping to live trading is a separate deliberate change."
    );
    assert!(
        !prod_cfg.contains("dry_run = false"),
        "config/production.toml MUST NOT set dry_run = false — that arms REAL orders."
    );

    // The retired dev/staging configs must be gone; base.toml + production.toml
    // are the only env configs.
    assert!(
        !workspace_root().join("config/staging.toml").exists(),
        "config/staging.toml must be retired (single prod env, operator 2026-06-30)"
    );
    assert!(
        !workspace_root().join("config/dev.toml").exists(),
        "there must be no config/dev.toml (single prod env)"
    );
}

#[test]
fn test_terraform_eventbridge_schedules_match_budget() {
    let content = std::fs::read_to_string(workspace_root().join("deploy/aws/terraform/main.tf"))
        .expect("main.tf must be readable"); // APPROVED: test
    // Weekday start: 08:30 IST = 03:00 UTC Mon-Fri (operator narrowed the
    // window back to 08:30-16:30 on 2026-06-05 — "make the aws instance start
    // and stop from 8.30 am till 4.30 pm"; supersedes the 2026-06-02
    // 08:00-17:00 widening).
    assert!(
        content.contains("cron(0 3 ? * MON-FRI *)"),
        "main.tf missing weekday start schedule (03:00 UTC Mon-Fri = 08:30 IST)"
    );
    // Weekday stop: 16:30 IST = 11:00 UTC Mon-Fri.
    assert!(
        content.contains("cron(0 11 ? * MON-FRI *)"),
        "main.tf missing weekday stop schedule (11:00 UTC Mon-Fri = 16:30 IST)"
    );
    // Negative asserts — the prior 7-day daily crons (every day, no MON-FRI
    // restriction) must NOT return. The weekday-only `? * MON-FRI` form is
    // distinct from the retired 7-day `* * ?` form.
    assert!(
        !content.contains("cron(30 2 * * ? *)"),
        "7-day daily start cron retired (weekday-only now)"
    );
    assert!(
        !content.contains("cron(30 11 * * ? *)"),
        "7-day daily stop cron retired (weekday-only now)"
    );
}

#[test]
fn test_cowork_can_dispatch_aws_workflows() {
    // Claude Cowork (claude-mobile-command) has actions:write so it can dispatch
    // aws-control / deploy on request. (The staging-secret seed workflow was
    // retired in the single-prod-env consolidation — the operator populates
    // /tickvault/prod/* manually, so there is no auto-seed workflow to self-fire.)
    let mobile = std::fs::read_to_string(
        workspace_root().join(".github/workflows/claude-mobile-command.yml"),
    )
    .expect("claude-mobile-command.yml must be readable"); // APPROVED: test
    assert!(
        mobile.contains("actions: write"),
        "claude-mobile-command must have actions:write so Cowork can dispatch AWS workflows"
    );
}

#[test]
fn test_aws_control_workflow_covers_all_operations() {
    // Full automation: a single dispatchable workflow operates the whole AWS
    // deployment (start/stop/reboot/status/restart-app/restart-questdb/
    // seed-secrets/deploy) from anywhere — GitHub web/mobile or a Claude session
    // — with zero terminal commands. Must exist, expose every action, guard
    // market hours on session-interrupting actions, and never echo a secret.
    let wf = std::fs::read_to_string(workspace_root().join(".github/workflows/aws-control.yml"))
        .expect("aws-control.yml must be readable"); // APPROVED: test
    for action in &[
        "status",
        "start",
        "stop",
        "reboot",
        "restart-app",
        "restart-questdb",
        "seed-secrets",
        "deploy",
    ] {
        assert!(
            wf.contains(&format!("action == '{action}'")),
            "aws-control.yml must implement the '{action}' action"
        );
    }
    assert!(
        wf.contains("Market-hours guard"),
        "aws-control.yml must guard session-interrupting actions against market hours"
    );
    assert!(
        !wf.contains("echo \"$value\"") && !wf.contains("echo $value"),
        "aws-control.yml must NEVER echo a decrypted secret value"
    );
}

#[test]
fn test_terraform_apply_replans_so_archive_file_zips_exist() {
    // Regression 2026-05-30 (terraform-apply #40/#43/#46): apply ran on a
    // SEPARATE runner from plan and only downloaded `tfplan`, so the Lambda
    // `data "archive_file"` .zip files built during plan were missing ->
    // `reading ZIP file (./.budget-killswitch.zip): no such file or directory`.
    // The fix re-plans on the apply runner (`apply -auto-approve` WITHOUT the
    // saved tfplan) so archive_file rebuilds the zips locally. Guard pins that
    // the apply step no longer applies the saved plan file.
    let content =
        std::fs::read_to_string(workspace_root().join(".github/workflows/terraform-apply.yml"))
            .expect("terraform-apply.yml must be readable"); // APPROVED: test
    assert!(
        !content.contains("apply -auto-approve -no-color tfplan"),
        "terraform-apply.yml must NOT apply the saved tfplan — the apply runner \
         lacks the archive_file .zip outputs built on the plan runner. Re-plan \
         on the apply runner instead."
    );
    assert!(
        content.contains("apply -auto-approve -no-color"),
        "terraform-apply.yml must still run terraform apply -auto-approve"
    );
}

#[test]
fn test_terraform_instance_role_has_ssm_managed_core() {
    // Regression 2026-05-30 (deploy-aws run #98): `aws ssm send-command` failed
    // with `InvalidInstanceId: Instances not in a valid state for account`. The
    // EC2 instance role had inline ssm:GetParameter perms (for the app to read
    // secrets) but NOT AmazonSSMManagedInstanceCore, so the SSM *agent* could
    // not register the box as a managed instance. The deploy pushes the binary
    // via SSM, so this attachment is mandatory. (Session Manager tunnels in
    // docs/runbooks/aws-access-from-anywhere.md also depend on it.)
    let content = std::fs::read_to_string(workspace_root().join("deploy/aws/terraform/main.tf"))
        .expect("main.tf must be readable"); // APPROVED: test
    assert!(
        content.contains("AmazonSSMManagedInstanceCore"),
        "main.tf must attach AmazonSSMManagedInstanceCore to the EC2 instance \
         role — required for `aws ssm send-command` (the deploy mechanism) to \
         reach the box; without it send-command fails InvalidInstanceId."
    );
}

#[test]
fn test_deploy_aws_resolves_account_id_at_runtime() {
    // Regression 2026-05-30 (deploy-aws run #98): the Telegram-notify steps used
    // ${{ secrets.AWS_ACCOUNT_ID }} which was empty -> SNS publish failed with
    // `Invalid parameter: AccountId`. The account id is now derived at runtime
    // via `aws sts get-caller-identity`, removing the manual-secret dependency.
    let content =
        std::fs::read_to_string(workspace_root().join(".github/workflows/deploy-aws.yml"))
            .expect("deploy-aws.yml must be readable"); // APPROVED: test
    assert!(
        content.contains("aws sts get-caller-identity --query Account"),
        "deploy-aws.yml must derive the AWS account id at runtime"
    );
    assert!(
        content.contains("steps.acct.outputs.id"),
        "deploy-aws.yml SNS topic ARNs must use the runtime-derived account id"
    );
    assert!(
        !content.contains("secrets.AWS_ACCOUNT_ID"),
        "deploy-aws.yml must NOT reference the empty AWS_ACCOUNT_ID secret \
         (it caused the SNS `Invalid parameter: AccountId` failure)."
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
fn test_deploy_aws_workflow_refreshes_repo_and_systemd_unit() {
    // Regression 2026-05-30 pre-flight audit: the deploy SSM command shipped a
    // new binary + synced config, but NEVER refreshed the box's repo clone or
    // the systemd unit. The clone is shallow from first boot, so the box would
    // keep its first-boot systemd unit FOREVER. That unit carries TV_ENVIRONMENT
    // — a stale unit would load a stale config. Under the single-prod-env model
    // (operator 2026-06-30) the unit sets TV_ENVIRONMENT=prod and production.toml
    // locks dry_run=true (NO real orders), so refreshing the unit keeps the box
    // on the correct env. The deploy must git-pull the clone AND re-copy the
    // systemd unit before daemon-reload + restart.
    let content =
        std::fs::read_to_string(workspace_root().join(".github/workflows/deploy-aws.yml"))
            .expect("deploy-aws.yml must be readable"); // APPROVED: test
    assert!(
        content.contains("git -C repo reset --hard origin/main"),
        "deploy-aws.yml SSM command must git-refresh the box's repo clone to \
         origin/main before copying config/systemd — otherwise the box runs \
         its stale first-boot files forever."
    );
    assert!(
        content.contains(
            "cp -f repo/deploy/systemd/tickvault.service /etc/systemd/system/tickvault.service"
        ),
        "deploy-aws.yml SSM command must refresh the systemd unit from the repo \
         (carries TV_ENVIRONMENT=prod; production.toml locks dry_run=true — a \
         stale unit could load a stale config)."
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
    // The 09:00-15:45 IST no-deploy band in UTC is 03:30-10:15 (operator lock
    // 2026-06-03: "only between 9 am till 3.45 pm it should not happen").
    assert!(
        content.contains("330") && content.contains("1015"),
        "deploy-aws.yml must encode the 09:00-15:45 IST window in UTC (330-1015)"
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
fn test_aws_autopilot_selfheal_workflow_exists() {
    // Operator demand 2026-05-31: "make it an extreme complete comprehensive
    // automated process ... never get stuck with manual configurations like
    // run deploy, configure static elastic IP." The aws-autopilot workflow +
    // script self-check the running deployment every 15 min and auto-heal
    // (start stopped box in market hours, re-associate EIP, restart app,
    // bring QuestDB up), Telegram-alerting only what needs human eyes.
    require_file_exists(
        ".github/workflows/aws-autopilot.yml",
        "AWS self-check + auto-heal workflow (operator demand 2026-05-31)",
    );
    require_file_exists("scripts/aws-autopilot.sh", "AWS autopilot self-heal script");
    let wf = std::fs::read_to_string(workspace_root().join(".github/workflows/aws-autopilot.yml"))
        .expect("aws-autopilot.yml must be readable"); // APPROVED: test
    // Runs on a schedule (every 15 min during the box's up-window) — not
    // only on-demand, so it self-heals without a human.
    assert!(
        wf.contains("schedule:") && wf.contains("*/15"),
        "aws-autopilot.yml must run on a 15-minute schedule (self-heal without a human)"
    );
    assert!(
        wf.contains("scripts/aws-autopilot.sh"),
        "aws-autopilot.yml must invoke scripts/aws-autopilot.sh"
    );

    let sh = std::fs::read_to_string(workspace_root().join("scripts/aws-autopilot.sh"))
        .expect("aws-autopilot.sh must be readable"); // APPROVED: test
    // The auto-heal actions the operator explicitly wanted to never do by hand.
    assert!(
        sh.contains("associate-address"),
        "autopilot must auto-re-associate the Elastic IP (operator: 'configure static elastic IP')"
    );
    // BP-09 (2026-07-01): a bare `systemctl restart` is a NO-OP on a
    // StartLimit-`failed` unit (a Dhan-RST crash-loop trips this), so the
    // script now `reset-failed`s to clear the latched start-limit BEFORE
    // `start` — recovering both a plain enabled-but-inactive unit AND a
    // stuck `failed` crash-loop with one path. The auto-restart intent is
    // preserved; the mechanism is `reset-failed` + `start`, not `restart`.
    assert!(
        sh.contains("systemctl reset-failed tickvault") && sh.contains("systemctl start tickvault"),
        "autopilot must auto-restart the app (reset-failed to clear any crash-loop start-limit, then start)"
    );
    assert!(
        sh.contains("start-instances"),
        "autopilot must auto-start a stopped box during market hours"
    );
    assert!(
        sh.contains("describe-instance-information"),
        "autopilot must verify the box is an SSM managed node"
    );
    // 2026-06-02 fix: the self-start window must be the FULL 08:30-16:30 IST
    // up-window, not just market hours (09:00-15:35). The pre-market gap
    // (08:30-09:00) is when the universe is fetched + WS subscribes — a stopped
    // box there is a real problem. Using is_market_window left autopilot
    // waiting until 09:00 to rescue a failed 08:30 EventBridge start.
    assert!(
        sh.contains("is_box_up_window"),
        "autopilot must self-start a stopped box across the WHOLE 08:30-16:30 IST \
         up-window (is_box_up_window), not only market hours — otherwise a failed \
         08:30 EventBridge start is not rescued until 09:00 (2026-06-02 incident)"
    );
    assert!(
        sh.contains("830") && sh.contains("1630"),
        "is_box_up_window must encode the 08:30-16:30 IST bounds (830/1630)"
    );
    // 2026-06-02 diagnostic (the 'A' fix): when the box is stopped during the
    // up-window, autopilot must report WHY the EventBridge auto-start failed
    // (rule missing / disabled / automation errored) so the operator doesn't
    // have to guess from the AWS console.
    assert!(
        sh.contains("diagnose_eventbridge_start"),
        "autopilot must diagnose the EventBridge daily-start rule when the box is \
         found stopped during the up-window"
    );
    assert!(
        sh.contains("tv-prod-daily-start") && sh.contains("describe-rule"),
        "the EventBridge diagnostic must inspect the tv-prod-daily-start rule state"
    );
}

#[test]
fn test_deploy_aws_self_starts_stopped_instance_and_skips_offhours_cleanly() {
    // 2026-06-02 incident: the box did not auto-start at 08:30 IST, so every
    // `deploy-aws` run failed at `aws ssm send-command` with
    // `InvalidInstanceId: Instances not in a valid state` and paged the
    // operator ("DLT deploy FAILED") on every overnight push. Two fixes pinned
    // here:
    //   C) the deploy job self-starts a stopped box when it SHOULD be up
    //      (up-window or manual dispatch) and waits for the SSM agent Online
    //      before send-command — this self-heals a missed EventBridge start.
    //   B) an off-hours auto push to a deliberately-stopped box skips cleanly
    //      via DEPLOY_SKIP_REASON, suppressing the failure Telegram (no 2 AM
    //      spam) while staying non-success so the 08:45 cron redeploys.
    let content =
        std::fs::read_to_string(workspace_root().join(".github/workflows/deploy-aws.yml"))
            .expect("deploy-aws.yml must be readable"); // APPROVED: test
    // C: self-start + SSM-online wait before the SSM deploy.
    assert!(
        content.contains("Ensure instance running"),
        "deploy-aws.yml must ensure the instance is running before send-command \
         (a stopped box -> InvalidInstanceId -> failed deploy)"
    );
    assert!(
        content.contains("aws ec2 start-instances") && content.contains("PingStatus"),
        "deploy-aws.yml must self-start a stopped box and wait for the SSM agent \
         PingStatus=Online before deploying"
    );
    // B: intentional off-hours skip flag + the failure-notify gate on it.
    assert!(
        content.contains("DEPLOY_SKIP_REASON=intentional_stopped"),
        "deploy-aws.yml must set DEPLOY_SKIP_REASON for an off-hours skip"
    );
    assert!(
        content.contains("env.DEPLOY_SKIP_REASON == ''"),
        "deploy-aws.yml failure-notify + auto-stop + fetch-output steps must be \
         gated on DEPLOY_SKIP_REASON being empty so ANY intentional defer \
         (off-hours stopped box OR market-hours swap abort) does NOT page the \
         operator (anti-spam)"
    );
}

#[test]
fn test_deploy_aws_regates_market_hours_at_swap_time() {
    // ROOT-CAUSE FIX 2026-06-02 (incident #2): PR #971 merged at 09:11 IST
    // (4 min before the 09:15 open), passed the START-time guard_market_hours,
    // then the ~4-5 min ARM build pushed the SSM binary swap + systemctl
    // restart to ~09:16 IST — INSIDE market open — blanking the live feed for
    // ~2 min at the bell. The start-time guard cannot see a build that crosses
    // the open boundary. This pins the SWAP-time re-gate: immediately before
    // the SSM swap, deploy-aws re-checks IST market hours and ABORTS (defers
    // to the 15:46 cron) instead of restarting the live app mid-session.
    let content =
        std::fs::read_to_string(workspace_root().join(".github/workflows/deploy-aws.yml"))
            .expect("deploy-aws.yml must be readable"); // APPROVED: test
    assert!(
        content.contains("Re-gate market hours at SWAP time"),
        "deploy-aws.yml must re-check market hours immediately before the SSM \
         binary swap (a build that started pre-open can cross the 09:00 boundary)"
    );
    assert!(
        content.contains("market_hours_swap_abort"),
        "the swap-time re-gate must set DEPLOY_SKIP_REASON=market_hours_swap_abort \
         so the deferred run does not page the operator + the 15:46 cron redeploys"
    );
    // The swap-time gate must encode the 09:00-15:45 IST window (900/1545) and
    // honour the same operator override as the start-time guard.
    assert!(
        content.contains("900") && content.contains("1545"),
        "swap-time re-gate must encode the 09:00-15:45 IST market window (900/1545)"
    );
    assert!(
        content.contains("confirm_market_hours"),
        "swap-time re-gate must keep the workflow_dispatch confirm_market_hours override"
    );
}

#[test]
fn test_deploy_aws_after_close_workflow_fires_in_premarket_and_postmarket_only() {
    // Operator lock 2026-06-03: "always deploy whenever the instance auto-starts
    // at 8 am ... only between 9 am till 3.45 pm it should not happen." The ONLY
    // no-deploy band is 09:00-15:45 IST; every other time auto-deploys main.
    //   - instance-start cron = 03:00 UTC = 08:30 IST (self-starts a stopped box)
    //   - pre-market cron      = 03:15 UTC = 08:45 IST (backup, before 09:15 open)
    //   - post-market cron     = 10:16 UTC = 15:46 IST (just after the 15:45 band)
    let path = ".github/workflows/deploy-aws-after-close.yml";
    require_file_exists(
        path,
        "auto-deploy at 08:30 instance-start + outside the 09:00-15:45 IST band \
         (operator lock 2026-06-03)",
    );
    let content = std::fs::read_to_string(workspace_root().join(path))
        .expect("deploy-aws-after-close.yml must be readable"); // APPROVED: test

    assert!(
        content.contains("0 3 * * 1-5"),
        "deploy-aws-after-close.yml must include instance-start cron \
         03:00 UTC Mon-Fri (08:30 IST) — self-starts the box + deploys even if \
         the EventBridge instance-start fails (2026-06-03 incident fix)"
    );
    assert!(
        content.contains("15 3 * * 1-5"),
        "deploy-aws-after-close.yml must include pre-market cron \
         03:15 UTC Mon-Fri (08:45 IST)"
    );
    assert!(
        content.contains("16 10 * * 1-5"),
        "deploy-aws-after-close.yml must include post-market cron \
         10:16 UTC Mon-Fri (15:46 IST — just after the 15:45 no-deploy band)"
    );
    // Must dispatch deploy-aws.yml — that's the whole point.
    assert!(
        content.contains("gh workflow run deploy-aws.yml"),
        "deploy-aws-after-close.yml must dispatch deploy-aws.yml via gh CLI"
    );
    // Idempotency: must compare main HEAD against last successful deploy SHA
    // so quiet days don't trigger a spurious 5s app restart.
    assert!(
        content.contains("head_sha"),
        "deploy-aws-after-close.yml must compare against last successful \
         deploy head_sha (idempotency — no spurious restart on quiet days)"
    );
    // Required permission to dispatch another workflow.
    assert!(
        content.contains("actions: write"),
        "deploy-aws-after-close.yml must request actions:write to dispatch deploy-aws.yml"
    );
}

#[test]
fn test_deploy_aws_binary_size_cap_is_30mb() {
    // Regression 2026-05-30: deploy-aws #97 failed because the release binary
    // grew to ~18MB (3 AWS SDKs + reqwest/rustls + tokio + questdb + rkyv,
    // all statically linked) against a stale 15MB cap that lived ONLY in this
    // workflow (CI never enforced it). Binary size has zero runtime/latency
    // impact and shrinking via opt-level="z" would hurt the O(1) hot path, so
    // the cap was raised to 30MB. This guard pins the new value so a future
    // edit can't silently drop it back to a blocking number.
    let content =
        std::fs::read_to_string(workspace_root().join(".github/workflows/deploy-aws.yml"))
            .expect("deploy-aws.yml must be readable"); // APPROVED: test
    assert!(
        content.contains("31457280"),
        "deploy-aws.yml binary size cap must be 30MB (31457280 bytes) — the \
         ~18MB release binary (3 AWS SDKs + reqwest/rustls/tokio/questdb/rkyv) \
         exceeds the old 15MB cap; shrinking would hurt O(1) latency."
    );
    assert!(
        !content.contains("15728640"),
        "deploy-aws.yml must NOT retain the old 15MB cap (15728640) — it \
         blocks every deploy of the legitimately-grown binary."
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

#[test]
fn test_start_watchdog_lambda_monitors_the_morning_start() {
    // 2026-06-02: AWS-native watchdog that answers "who monitors the 08:30
    // start?". A ping Lambda at 08:30 IST + a check Lambda at 08:45 IST that
    // pages if the box did not come up. Runs IN AWS (not on a GitHub runner),
    // so it alerts even if GitHub Actions is down — the gap that hid the
    // 2026-06-02 silent start failure.
    require_file_exists(
        "deploy/aws/lambda/start-watchdog/handler.py",
        "start-watchdog Lambda handler (ping + check)",
    );
    require_file_exists(
        "deploy/aws/terraform/start-watchdog-lambda.tf",
        "start-watchdog Lambda terraform (IAM + 2 EventBridge schedules)",
    );
    let tf = std::fs::read_to_string(
        workspace_root().join("deploy/aws/terraform/start-watchdog-lambda.tf"),
    )
    .expect("start-watchdog-lambda.tf must be readable"); // APPROVED: test
    // Ping at 08:30 IST (03:00 UTC) + check at 08:45 IST (03:15 UTC), weekdays.
    assert!(
        tf.contains("cron(0 3 ? * MON-FRI *)"),
        "ping schedule must be 03:00 UTC Mon-Fri (08:30 IST)"
    );
    assert!(
        tf.contains("cron(15 3 ? * MON-FRI *)"),
        "check schedule must be 03:15 UTC Mon-Fri (08:45 IST)"
    );
    // Least-privilege: describe EC2 + publish to the alerts topic.
    assert!(
        tf.contains("ec2:DescribeInstances") && tf.contains("aws_sns_topic.tv_alerts.arn"),
        "watchdog Lambda must describe EC2 + publish to tv_alerts"
    );
    let py = std::fs::read_to_string(
        workspace_root().join("deploy/aws/lambda/start-watchdog/handler.py"),
    )
    .expect("handler.py must be readable"); // APPROVED: test
    assert!(
        py.contains("describe_instances") && py.contains("\"running\""),
        "handler must check the instance is running"
    );
}

// ---------------------------------------------------------------------------
// Deploy-watchdog Lambda (2026-06-02) — AWS-native safety-net for the
// post-merge auto-deploy. Operator decision "AWS watchdog safety-net": keep
// the GitHub-cron deploy, add an EventBridge->Lambda that covers cron misses.
// Ratchet the wiring so it can't silently rot (charter "extreme check").
// ---------------------------------------------------------------------------

#[test]
fn test_deploy_watchdog_lambda_is_wired() {
    require_file_exists(
        "deploy/aws/terraform/deploy-watchdog-lambda.tf",
        "Deploy-watchdog Lambda terraform (EventBridge + IAM + function)",
    );
    require_file_exists(
        "deploy/aws/lambda/deploy-watchdog/handler.py",
        "Deploy-watchdog Lambda handler",
    );
    let tf = std::fs::read_to_string(
        workspace_root().join("deploy/aws/terraform/deploy-watchdog-lambda.tf"),
    )
    .expect("deploy-watchdog-lambda.tf must be readable"); // APPROVED: test

    // Two weekday EventBridge schedules, 5 min after each GitHub deploy cron:
    // 08:50 IST = 03:20 UTC, 15:51 IST = 10:21 UTC.
    assert!(
        tf.contains("cron(20 3 ? * MON-FRI *)"),
        "deploy-watchdog must run 08:50 IST (03:20 UTC Mon-Fri) — 5 min after the pre-market cron"
    );
    assert!(
        tf.contains("cron(21 10 ? * MON-FRI *)"),
        "deploy-watchdog must run 15:51 IST (10:21 UTC Mon-Fri) — 5 min after the 15:46 post-market cron"
    );
    // It reads the repo-scoped GitHub token (same param as operator-control)
    // and can publish to the alerts topic.
    assert!(
        tf.contains("/operator/github-token") && tf.contains("ssm:GetParameter"),
        "deploy-watchdog must read the repo-scoped GitHub token from SSM"
    );
    assert!(
        tf.contains("aws_sns_topic.tv_alerts.arn"),
        "deploy-watchdog must be able to page the operator via the tv_alerts SNS topic"
    );
    // INSTANT deploy on instance start (operator directive 2026-06-02): an
    // EC2 state-change event rule fires the watchdog the moment the box enters
    // `running`, so the latest main deploys right after the 08:00 auto-start —
    // not at the 08:45 cron. This is the PRIMARY morning deploy trigger.
    assert!(
        tf.contains("EC2 Instance State-change Notification") && tf.contains("\"running\""),
        "deploy-watchdog must have an EventBridge event-pattern rule that fires \
         on the tv-app EC2 instance entering 'running' — the instant-deploy-on-start \
         trigger (operator directive 2026-06-02)."
    );
    assert!(
        tf.contains("aws_instance.tv_app.id"),
        "the instance-start rule must scope to THIS instance (aws_instance.tv_app.id), \
         not fire on every EC2 in the account."
    );
    assert!(
        tf.contains("window = \"instance-start\""),
        "the instance-start target must pass window=\"instance-start\" so the handler \
         logs/labels the instant trigger distinctly."
    );
}

#[test]
fn test_deploy_watchdog_handler_only_dispatches_when_positively_stale() {
    // The handler's safety contract: NEVER dispatch on uncertainty. The pure
    // `is_stale` must return False when either sha is unknown (the no-false-OK
    // inverse — we never "fix" what we can't confirm is broken).
    let h = std::fs::read_to_string(
        workspace_root().join("deploy/aws/lambda/deploy-watchdog/handler.py"),
    )
    .expect("deploy-watchdog handler.py must be readable"); // APPROVED: test
    assert!(
        h.contains("def is_stale("),
        "handler must expose the pure is_stale decision function"
    );
    assert!(
        h.contains("if not desired_sha or not deployed_sha:") && h.contains("return False"),
        "is_stale MUST return False when either sha is unknown — never dispatch on uncertainty"
    );
    assert!(
        h.contains("deploy-aws-after-close.yml"),
        "watchdog must dispatch the idempotent + market-hours-guarded auto-deploy workflow"
    );
}
