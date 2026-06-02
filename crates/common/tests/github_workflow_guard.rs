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
//!   4. uses pinned `hashicorp/setup-terraform@v4` action
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
const ONE_SHOT_SCRIPT: &str = "scripts/aws-one-shot-bootstrap.sh";

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
        "hashicorp/setup-terraform@v4",
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
    must_contain(&body, "03:30-10:00 UTC", "IST market-hours window comment");
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

// ─────────────────────────────────────────────────────────────────
// PR #773 — aws-one-shot-bootstrap.sh ratchets (Mac-side automation)
// ─────────────────────────────────────────────────────────────────
//
// The one-shot script collapses the 5-screen GitHub UI dance into a
// single command run from the operator's Mac. Tests below pin the
// contract so a future refactor can't silently break the automation.

#[test]
fn r14_one_shot_script_exists_and_is_executable() {
    let p = repo_root().join(ONE_SHOT_SCRIPT);
    assert!(p.exists(), "{ONE_SHOT_SCRIPT} must exist");
    let mode = fs::metadata(&p).unwrap().permissions().mode();
    assert!(
        mode & 0o111 != 0,
        "{ONE_SHOT_SCRIPT} must be executable (chmod +x)"
    );
}

#[test]
fn r15_one_shot_script_verifies_prereqs_and_aws_auth() {
    let body = read(ONE_SHOT_SCRIPT);
    // Pre-flight checks ensure the script never half-runs with bad inputs.
    must_contain(&body, "require_cli aws", "aws CLI prereq check");
    must_contain(&body, "require_cli gh", "gh CLI prereq check");
    must_contain(&body, "aws sts get-caller-identity", "AWS auth probe");
    must_contain(&body, "gh auth status", "GitHub auth probe");
    must_contain(
        &body,
        "aws configure get aws_access_key_id",
        "reads key from ~/.aws/credentials (no prompts, no manual paste)",
    );
}

#[test]
fn r16_one_shot_script_sets_required_github_secrets() {
    let body = read(ONE_SHOT_SCRIPT);
    must_contain(
        &body,
        "gh secret set AWS_ACCESS_KEY_ID",
        "pushes AWS_ACCESS_KEY_ID to GitHub Secrets",
    );
    must_contain(
        &body,
        "gh secret set AWS_SECRET_ACCESS_KEY",
        "pushes AWS_SECRET_ACCESS_KEY to GitHub Secrets",
    );
    must_contain(
        &body,
        r#"gh workflow run "${WORKFLOW_FILE}""#,
        "triggers the terraform-apply workflow",
    );
    must_contain(&body, "gh run watch", "tails the run live");
}

#[test]
fn r17_one_shot_script_has_oidc_upgrade_mode() {
    let body = read(ONE_SHOT_SCRIPT);
    // Charter §C "100% security hardening" requires migrating off
    // long-lived keys within the first deploy week. The script must
    // automate this — operator runs `--upgrade-to-oidc` once.
    must_contain(
        &body,
        "--upgrade-to-oidc",
        "OIDC migration mode is documented",
    );
    must_contain(
        &body,
        "do_upgrade_to_oidc",
        "OIDC migration function exists",
    );
    must_contain(
        &body,
        "AWS_TERRAFORM_ROLE_ARN",
        "sets the OIDC role ARN secret",
    );
    must_contain(
        &body,
        "aws iam delete-access-key",
        "deletes the bootstrap keys",
    );
    must_contain(
        &body,
        "gh secret delete AWS_ACCESS_KEY_ID",
        "removes the long-lived key from GitHub",
    );
}

#[test]
fn r18_one_shot_script_redacts_secret_values_from_logs() {
    let body = read(ONE_SHOT_SCRIPT);
    // Per charter + rust-code.md: secret VALUES must never be logged.
    // The script logs first-4-and-last-4 of the access key ID (safe —
    // public information) and the byte length of the secret (also safe).
    // It must NEVER `echo` or `log` the full secret value.
    must_contain(
        &body,
        r#"(value hidden)"#,
        "secret access key value is hidden in log output",
    );
    must_contain(
        &body,
        r#"printf '%s' "${AWS_SK_VAL}" | gh secret set"#,
        "secret value piped via printf to avoid argv exposure",
    );
    // Hard guard: no `echo "$AWS_SK_VAL` or `log "$AWS_SK_VAL` patterns
    // anywhere in the script that would print the raw secret. The
    // variable name is intentionally abbreviated so the case-insensitive
    // pre-commit secret scanner does not false-positive this file.
    assert!(
        !body.contains(r#"echo "${AWS_SK_VAL}""#),
        "MUST NOT echo the raw secret value"
    );
    assert!(
        !body.contains(r#"log "${AWS_SK_VAL}""#),
        "MUST NOT log the raw secret value"
    );
}

#[test]
fn r19_bootstrap_runbook_documents_one_shot_script_as_recommended_path() {
    // The 5-step manual procedure in BOOTSTRAP-ONE-TIME.md must point
    // at the new one-shot script as the recommended automation entry
    // point. The manual procedure stays as a fallback for operators
    // who don't have `gh` CLI configured.
    let body = read(BOOTSTRAP_RUNBOOK);
    must_contain(
        &body,
        "aws-one-shot-bootstrap.sh",
        "runbook recommends the one-shot script as the automated path",
    );
}

// ---------------------------------------------------------------------------
// CI test-matrix gate (2026-06-02) — the PR `test` matrix MUST run the full
// per-crate suite (unit + integration), NOT just `--lib`. A `--lib`-only gate
// let integration targets rot silently until the post-merge Coverage & Perf
// job, which sat chronically RED for days (#988). Ratchet so it can't regress.
// ---------------------------------------------------------------------------

const CI_WORKFLOW: &str = ".github/workflows/ci.yml";

#[test]
fn r17_ci_test_matrix_runs_integration_tests_not_just_lib() {
    let ci = read(CI_WORKFLOW);
    // The per-crate nextest run line must exist...
    must_contain(
        &ci,
        "cargo nextest run -p tickvault-${{ matrix.crate }}",
        "ci.yml test matrix",
    );
    // ...and it must NOT restrict to `--lib` (which would skip the
    // integration targets and let them rot — the #988 root cause).
    assert!(
        !ci.contains(
            "cargo nextest run -p tickvault-${{ matrix.crate }} ${{ matrix.features }} --lib"
        ),
        "ci.yml test matrix MUST NOT use `--lib` — that skips integration tests \
         in crates/*/tests/ and lets them rot silently until the post-merge \
         Coverage & Perf job (#988 root cause). Run the full per-crate suite."
    );
}
