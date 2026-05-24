//! Guard test — `.github/workflows/terraform-apply.yml` pins the contract
//! for fully-automated `terraform apply` per operator charter §C
//! ("100% no manual inputs").
//!
//! Per PR #771 (5-PR roadmap, post #770): GitHub Actions auto-fires
//! `terraform apply` on push to main when `deploy/aws/terraform/**`
//! changes. This source-scan ratchet pins the 8 must-haves of the
//! workflow so a future regression can't silently break the automation
//! chain:
//!
//!   1. workflow file exists
//!   2. triggers on push to main with `deploy/aws/terraform/**` path filter
//!   3. has both terraform-plan and terraform-apply jobs
//!   4. uses pinned `hashicorp/setup-terraform@v3` action
//!   5. uses pinned `aws-actions/configure-aws-credentials@v4` action
//!   6. has the market-hours guard job
//!   7. has `aws sns publish` for both success and failure notify paths
//!   8. injects S3 backend bucket name via `-backend-config="bucket=tv-terraform-state-`
//!
//! Plus 2 ratchets for the companion script and runbook:
//!
//!   9. `scripts/aws-bootstrap-state-backend.sh` exists, executable, idempotent (uses head-bucket guard)
//!  10. `deploy/aws/terraform/BOOTSTRAP-ONE-TIME.md` exists and names the
//!      access-key cleanup step (charter §C "100% security hardening")
//!
//! If any link breaks, this test fails the build first — preventing the
//! silent regression to "operator types terraform apply by hand".

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

fn repo_root() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest.parent().unwrap().parent().unwrap().to_path_buf()
}

fn read(path: &str) -> String {
    let p = repo_root().join(path);
    fs::read_to_string(&p).unwrap_or_else(|e| panic!("failed to read {}: {e}", p.display()))
}

fn must_contain(haystack: &str, needle: &str, label: &str) {
    assert!(
        haystack.contains(needle),
        "{label}: expected to contain {needle:?}"
    );
}

const WORKFLOW: &str = ".github/workflows/terraform-apply.yml";
const DEPLOY_AWS_WORKFLOW: &str = ".github/workflows/deploy-aws.yml";
const BOOTSTRAP_SCRIPT: &str = "scripts/aws-bootstrap-state-backend.sh";
const BOOTSTRAP_RUNBOOK: &str = "deploy/aws/terraform/BOOTSTRAP-ONE-TIME.md";

#[test]
fn r1_workflow_file_exists() {
    let p = repo_root().join(WORKFLOW);
    assert!(
        p.exists(),
        "{WORKFLOW} must exist — operator charter §C requires auto-fired terraform apply"
    );
}

#[test]
fn r2_triggers_on_main_push_with_terraform_path_filter() {
    let body = read(WORKFLOW);
    must_contain(&body, "branches: [main]", "trigger branches");
    must_contain(
        &body,
        "deploy/aws/terraform/**",
        "path filter for terraform changes",
    );
}

#[test]
fn r3_has_both_plan_and_apply_jobs() {
    let body = read(WORKFLOW);
    must_contain(&body, "terraform-plan:", "terraform-plan job");
    must_contain(&body, "terraform-apply:", "terraform-apply job");
}

#[test]
fn r3b_has_preflight_job_that_gates_downstream_on_bootstrap_secrets() {
    // Per the operator-charter, the workflow MUST skip cleanly (not fail)
    // when the one-time AWS bootstrap secrets are absent. Otherwise the
    // PR check goes red on every contributor's first push to the repo,
    // blocking merges. The preflight job sets `bootstrap_ready=true|false`
    // and ALL downstream jobs gate on it via `needs.preflight.outputs.bootstrap_ready == 'true'`.
    let body = read(WORKFLOW);
    must_contain(&body, "preflight:", "preflight job exists");
    must_contain(
        &body,
        "bootstrap_ready",
        "preflight emits bootstrap_ready output",
    );
    must_contain(
        &body,
        "needs.preflight.outputs.bootstrap_ready == 'true'",
        "downstream jobs gated on preflight result",
    );
    // The 3 downstream jobs (terraform-plan, guard-market-hours, terraform-apply)
    // PLUS the notify job MUST all reference the gate. Count occurrences.
    let gate_refs = body
        .matches("needs.preflight.outputs.bootstrap_ready == 'true'")
        .count();
    assert!(
        gate_refs >= 4,
        "expected at least 4 jobs gated on preflight (plan, guard, apply, notify); found {gate_refs}"
    );
}

#[test]
fn r4_uses_pinned_setup_terraform_action() {
    let body = read(WORKFLOW);
    must_contain(
        &body,
        "hashicorp/setup-terraform@v3",
        "setup-terraform pinned action",
    );
    must_contain(
        &body,
        "terraform_version: 1.9.8",
        "terraform version pinned",
    );
}

#[test]
fn r5_uses_pinned_configure_aws_credentials_action() {
    let body = read(WORKFLOW);
    must_contain(
        &body,
        "aws-actions/configure-aws-credentials@v4",
        "configure-aws-credentials pinned action",
    );
}

#[test]
fn r6_has_market_hours_guard_job() {
    let body = read(WORKFLOW);
    must_contain(&body, "guard-market-hours:", "guard-market-hours job");
    must_contain(&body, "03:45-10:00 UTC", "IST market-hours window comment");
    must_contain(&body, "confirm_market_hours", "override input present");
}

#[test]
fn r7_has_sns_publish_for_success_and_failure() {
    let body = read(WORKFLOW);
    must_contain(&body, "Notify on success", "success notify step");
    must_contain(&body, "Notify on failure", "failure notify step");
    let publish_count = body.matches("aws sns publish").count();
    assert!(
        publish_count >= 2,
        "expected at least 2 'aws sns publish' invocations (success + failure), got {publish_count}"
    );
}

#[test]
fn r8_injects_s3_backend_bucket_via_backend_config_flag() {
    let body = read(WORKFLOW);
    must_contain(
        &body,
        r#"-backend-config="bucket=tv-terraform-state-"#,
        "dynamic S3 backend bucket injection at terraform init time",
    );
    must_contain(
        &body,
        r#"-backend-config="dynamodb_table=tv-terraform-locks""#,
        "DynamoDB lock table backend config",
    );
}

#[test]
fn r9_bootstrap_script_exists_and_is_executable_and_idempotent() {
    let p = repo_root().join(BOOTSTRAP_SCRIPT);
    assert!(p.exists(), "{BOOTSTRAP_SCRIPT} must exist");
    let mode = fs::metadata(&p).unwrap().permissions().mode();
    assert!(
        mode & 0o111 != 0,
        "{BOOTSTRAP_SCRIPT} must be executable (chmod +x)"
    );
    let body = read(BOOTSTRAP_SCRIPT);
    // Idempotency = each create call is gated by a head/describe check.
    must_contain(
        &body,
        "aws s3api head-bucket",
        "S3 bucket existence check before create (idempotency)",
    );
    must_contain(
        &body,
        "aws dynamodb describe-table",
        "DynamoDB table existence check before create (idempotency)",
    );
}

#[test]
fn r10_bootstrap_runbook_exists_and_names_oidc_cleanup() {
    let p = repo_root().join(BOOTSTRAP_RUNBOOK);
    assert!(p.exists(), "{BOOTSTRAP_RUNBOOK} must exist");
    let body = read(BOOTSTRAP_RUNBOOK);
    must_contain(
        &body,
        "AWS_TERRAFORM_ROLE_ARN",
        "post-bootstrap OIDC migration step (charter §C security hardening)",
    );
    must_contain(
        &body,
        "Delete the access key",
        "explicit access-key cleanup step",
    );
    must_contain(&body, "40 seconds", "honest envelope total time");
}

// ─────────────────────────────────────────────────────────────────
// PR #772 — deploy-aws.yml auto-trigger + preflight ratchets
// ─────────────────────────────────────────────────────────────────
//
// Per the 5-PR roadmap, deploy-aws.yml must auto-fire on push to main
// with Rust-relevant paths, AND skip cleanly when AWS bootstrap secrets
// are absent (same pattern as terraform-apply.yml). Tests below pin
// that contract.

#[test]
fn r11_deploy_aws_has_push_to_main_trigger_with_rust_paths() {
    let body = read(DEPLOY_AWS_WORKFLOW);
    // The original workflow only triggered on tag push + workflow_dispatch.
    // PR #772 added push to main with path filters so any merge to main
    // touching Rust code auto-deploys (outside market hours).
    must_contain(&body, "branches: [main]", "main branch push trigger");
    must_contain(&body, "'crates/**'", "Rust source path filter");
    must_contain(&body, "'Cargo.toml'", "Cargo manifest path filter");
    must_contain(&body, "'Cargo.lock'", "Cargo.lock path filter");
    must_contain(
        &body,
        "'deploy/systemd/**'",
        "systemd unit path filter (binary integration)",
    );
    // Existing triggers must still be present
    must_contain(&body, "'v*.*.*'", "existing tag trigger preserved");
    must_contain(
        &body,
        "workflow_dispatch:",
        "existing manual dispatch preserved",
    );
}

#[test]
fn r12_deploy_aws_has_preflight_gate() {
    let body = read(DEPLOY_AWS_WORKFLOW);
    // Mirrors the terraform-apply.yml preflight pattern — gates downstream
    // jobs on bootstrap secret presence so pre-bootstrap PRs don't go red.
    must_contain(&body, "preflight:", "deploy-aws preflight job");
    must_contain(
        &body,
        "bootstrap_ready",
        "deploy-aws preflight emits bootstrap_ready",
    );
    // build, guard_market_hours, deploy must all gate on preflight
    let gate_refs = body
        .matches("needs.preflight.outputs.bootstrap_ready == 'true'")
        .count();
    assert!(
        gate_refs >= 3,
        "expected at least 3 jobs gated on preflight (build, guard, deploy); found {gate_refs}"
    );
}

#[test]
fn r13_deploy_aws_header_documents_push_trigger() {
    let body = read(DEPLOY_AWS_WORKFLOW);
    // The header comment must list all 3 triggers so future readers
    // can grep the file without needing to parse the YAML.
    must_contain(
        &body,
        "Push to main with Rust",
        "header documents push-to-main trigger",
    );
    must_contain(
        &body,
        "guard_market_hours",
        "header documents market-hours safety net",
    );
    must_contain(&body, "preflight job", "header documents preflight gate");
}
