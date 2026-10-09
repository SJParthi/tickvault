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
//!   4. uses pinned `hashicorp/setup-terraform` v4 action
//!   5. uses pinned `aws-actions/configure-aws-credentials` v4 action
//!      (2026-10-09: both are pinned to a 40-character commit SHA with the
//!      tag as a trailing comment, not to the movable tag itself)
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
    // 2026-10-09 (stress audit RO-5/SEC-2): was `must_contain("…@v4")`. A tag
    // can be moved by whoever controls the action's repository, and this job
    // holds production credentials, so the pin is now a commit SHA with the
    // tag kept as a comment.
    assert_pinned_to_sha_with_tag(&body, "hashicorp/setup-terraform", "v4");
    must_contain(
        &body,
        "terraform_version: 1.9.8",
        "terraform version pinned",
    );
}

#[test]
fn r5_uses_pinned_configure_aws_credentials_action() {
    let body = read(WORKFLOW);
    // 2026-10-09: SHA form, same reason as r4.
    assert_pinned_to_sha_with_tag(&body, "aws-actions/configure-aws-credentials", "v4");
}

/// Every `uses: <action>@…` line in `body` names a 40-character commit SHA
/// and carries `# <tag>` as its trailing comment; at least one such line
/// exists.
fn assert_pinned_to_sha_with_tag(body: &str, action: &str, tag: &str) {
    let lines: Vec<&str> = body
        .lines()
        .filter(|l| uses_value(l).is_some_and(|v| v.starts_with(&format!("{action}@"))))
        .collect();
    assert!(!lines.is_empty(), "{action}: no `uses:` line found");
    for line in lines {
        let value = uses_value(line).unwrap_or_default();
        assert!(
            is_full_sha_ref(value),
            "{action}: must be pinned to a 40-character commit SHA: {line:?}"
        );
        assert!(
            line.contains(&format!("# {tag}")),
            "{action}: the pin must carry `# {tag}` so a reader sees the version: {line:?}"
        );
    }
}

#[test]
fn r6_has_market_hours_guard_job() {
    let body = read(WORKFLOW);
    must_contain(&body, "guard-market-hours:", "guard-market-hours job");
    must_contain(&body, "03:30-10:15 UTC", "IST market-hours window comment");
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

/// Audit PR36a (2026-09-27): the operator console key was published to the
/// alerts topic as `${URL}#key=${KEY}`, and that topic also delivers to email
/// and SMS, so the key sat in plaintext mail and text history. `add-mask`
/// only hides the Actions log. No workflow may put the key, or a `#key=`
/// link, into any published message, and the portal step must not read the
/// stored key at all.
#[test]
fn r20_no_workflow_publishes_the_operator_key() {
    let dir = repo_root().join(".github/workflows");
    let mut scanned = 0usize;
    for entry in fs::read_dir(&dir).expect("workflows dir readable") {
        let path = entry.expect("dir entry").path();
        let is_yaml = matches!(
            path.extension().and_then(|e| e.to_str()),
            Some("yml") | Some("yaml")
        );
        if !is_yaml {
            continue;
        }
        scanned += 1;
        let body = fs::read_to_string(&path).expect("workflow readable");
        let name = path.display();
        assert!(
            !body.contains("#key="),
            "{name}: builds a #key= link; the operator key must never be sent (audit PR36a)"
        );
        for (i, line) in body.lines().enumerate() {
            if line.contains("--message") || line.contains("MESSAGE=") {
                assert!(
                    !line.contains("KEY}") && !line.contains("$KEY") && !line.contains("LINK}"),
                    "{name}:{}: a published message references the key: {line}",
                    i + 1
                );
            }
        }
    }
    assert!(scanned > 0, "no workflow files scanned");

    let body = read(WORKFLOW);
    let start = body
        .find("- name: Send the operator-portal link to Telegram")
        .expect("portal link step present");
    let rest = &body[start + 1..];
    let step = &rest[..rest.find("\n      - name:").unwrap_or(rest.len())];
    assert!(
        !step.contains("--with-decryption"),
        "the portal step must not read the stored operator key"
    );
    must_contain(
        step,
        "The key is never sent in a message.",
        "portal message tells the operator where the key is instead",
    );
    // Rotation (owner chose "Rotate after fix", 2026-09-27): only the exact
    // word "rotate" overwrites the stored key, and a failed overwrite fails
    // the step instead of reporting success.
    must_contain(step, "rotate_console_key", "rotation is a manual input");
    must_contain(
        step,
        "[ \"$ROTATE_CONSOLE_KEY\" = \"rotate\" ]",
        "rotation needs the exact word",
    );
    must_contain(step, "--overwrite", "rotation replaces the stored key");
    must_contain(
        step,
        "console key rotation FAILED",
        "a failed rotation is loud",
    );
    // Security review (PR36a): the first-time save and the key itself are
    // checked too, so neither an empty key nor a failed save is reported as
    // a live portal.
    must_contain(
        step,
        "[ ${#KEY} -ne 40 ]",
        "a key is stored only after its length is checked",
    );
    must_contain(
        step,
        "console key generation FAILED",
        "an empty or short key is loud",
    );
    must_contain(
        step,
        "console key creation FAILED",
        "a failed first-time save is loud",
    );
    must_contain(
        step,
        "could not read the stored key; the old key is still in place",
        "a rotation that fell into the create branch is not reported as done",
    );
    let rotate_input = body
        .find("      rotate_console_key:")
        .expect("rotate_console_key dispatch input declared");
    assert!(
        body[rotate_input..].contains("default: ''"),
        "rotation defaults to off"
    );
}

/// Audit PR36b (2026-09-27): a manual deploy could be started from any
/// branch and reach the prod environment, shipping a commit that never
/// passed All Green. The deploy workflow's preflight refuses a manual run
/// that is not on main or whose commit has no successful All Green (its own,
/// or the head of the pull request squash-merged as it), fail-closed on any
/// API error, and reports whether the prod environment has a
/// deployment-branch rule (the barrier a branch cannot delete).
#[test]
fn r21_manual_deploy_needs_main_and_all_green() {
    let body = read(DEPLOY_AWS_WORKFLOW);
    let start = body
        .find("- name: Refuse a manual or tag deploy of a commit that did not pass All Green")
        .expect("manual-deploy gate step present");
    let pre = &body[..start];
    assert!(
        pre.rfind("\n  preflight:") > pre.rfind("\n  build:"),
        "the gate sits in the preflight job, before the build"
    );
    let rest = &body[start + 1..];
    let step = &rest[..rest.find("\n      - name:").unwrap_or(rest.len())];
    must_contain(
        step,
        "if: github.event_name == 'workflow_dispatch'",
        "the gate runs on every manual run",
    );
    must_contain(
        step,
        "[ \"$RUN_REF\" != \"refs/heads/main\" ]",
        "only main can be deployed by hand",
    );
    // Audit PR36c (2026-10-03): a pushed v*.*.* tag runs the same gate, and
    // its commit must already be on main.
    must_contain(
        step,
        "if: github.event_name == 'workflow_dispatch' || startsWith(github.ref, 'refs/tags/')",
        "the gate runs on every manual run AND every tag run",
    );
    must_contain(
        step,
        "compare/main...${RUN_SHA}",
        "a tag's commit is compared with main",
    );
    must_contain(
        step,
        "identical|behind)",
        "only a commit already on main (main's head or an ancestor) passes",
    );
    must_contain(step, "check_name=All%20Green", "reads the All Green check");
    must_contain(
        step,
        ".app.slug == \"github-actions\"",
        "only an All Green posted by GitHub Actions counts",
    );
    must_contain(
        step,
        ".merge_commit_sha == \\\"${RUN_SHA}\\\"",
        "the pull request must be the one merged as this commit",
    );
    must_contain(step, "could not read", "an API failure refuses the deploy");
    assert!(
        !step.contains("continue-on-error"),
        "the gate must be able to fail the run"
    );
    let job = &body[pre.rfind("\n  preflight:").expect("preflight job")..];
    let job = &job[..job.find("\n  build:").expect("build job after preflight")];
    must_contain(job, "checks: read", "preflight may read check runs");
    must_contain(
        job,
        "pull-requests: read",
        "preflight may read pull requests",
    );
    must_contain(
        job,
        "- name: Report whether the prod environment is limited to main",
        "the prod environment's branch rule is reported on every run",
    );
    must_contain(
        job,
        "prod environment accepts runs from ANY branch",
        "a missing branch rule is a visible warning",
    );
}

// ---------------------------------------------------------------------------
// Mutation lane (audit H2, 2026-10-06). The weekly full sweep is ~18-hour
// class and the job was capped at 60 minutes, so it could never finish. The
// sweep is sharded and each shard gets GitHub's long job timeout; a push run
// mutates only the changed lines.
// ---------------------------------------------------------------------------

const MUTATION_WORKFLOW: &str = ".github/workflows/mutation.yml";

#[test]
fn mutation_weekly_sweep_is_sharded_with_a_timeout_it_can_finish_in() {
    let wf = read(MUTATION_WORKFLOW);
    must_contain(
        &wf,
        "fromJSON('[0, 1, 2, 3, 4, 5, 6, 7]')",
        "mutation sweep shard matrix",
    );
    must_contain(
        &wf,
        "MUTATION_SHARDS: ${{ github.event_name == 'push' && 1 || 8 }}",
        "mutation shard count matches the matrix",
    );
    must_contain(
        &wf,
        "--shard \"${{ matrix.shard }}/${MUTATION_SHARDS}\"",
        "cargo mutants runs one shard per job",
    );
    must_contain(
        &wf,
        "timeout-minutes: ${{ github.event_name == 'push' && 60 || 350 }}",
        "scheduled shards get the long timeout",
    );
    must_contain(
        &wf,
        "fail-fast: false",
        "one shard's result never cancels the others",
    );
    must_contain(
        &wf,
        "name: mutation-results-shard-${{ matrix.shard }}",
        "each shard uploads its own results",
    );
    assert!(
        !wf.contains("    timeout-minutes: 60\n"),
        "a flat 60-minute cap on the mutation job is back; the weekly sweep cannot finish in it"
    );
}

#[test]
fn mutation_push_run_mutates_only_the_changed_lines() {
    let wf = read(MUTATION_WORKFLOW);
    for (needle, label) in [
        (
            "git diff \"$before\" HEAD -- crates/core crates/trading crates/common > mutants.diff",
            "push run diff is limited to the critical crates",
        ),
        (
            "echo \"in_diff=--in-diff mutants.diff\" >> \"$GITHUB_OUTPUT\"",
            "push run passes the diff to cargo mutants",
        ),
        (
            "${{ steps.changed.outputs.in_diff }}",
            "cargo mutants receives the in-diff argument",
        ),
        (
            "--in-place",
            "the unmutated baseline keeps .git (2026-10-04)",
        ),
    ] {
        assert!(
            wf.contains(needle),
            "{label}: expected to contain {needle:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// cargo-careful runner disk (2026-10-06, the issue #1836 class). The weekly
// run and its re-run died linking with "Free space left: 0 MB" and an lld
// "Bus error". The job now carries the sanitizer jobs' three disk measures.
// ---------------------------------------------------------------------------

#[test]
fn careful_job_frees_runner_disk_and_drops_dependency_debug_info() {
    let wf = read(".github/workflows/safety.yml");
    let start = wf.find("\n  careful:\n").expect("careful job present");
    let end = wf[start..]
        .find("\n  sanitizer-asan:\n")
        .map(|i| start + i)
        .expect("asan job follows careful");
    let job = &wf[start..end];
    for (needle, label) in [
        (
            "Free runner disk space",
            "frees the preinstalled toolchains",
        ),
        (
            "sudo rm -rf /usr/share/dotnet",
            "the cleanup actually removes something",
        ),
        (
            "CARGO_PROFILE_DEV_DEBUG: line-tables-only",
            "line tables for our crates",
        ),
        (
            "CARGO_RESOLVER_FEATURE_UNIFICATION: workspace",
            "one feature set for every -p",
        ),
        (
            "Report runner disk (issue 1836)",
            "df after the build either way",
        ),
    ] {
        assert!(
            job.contains(needle),
            "careful job {label}: expected {needle:?}"
        );
    }
    let no_dep_debug = job
        .matches("--config 'profile.dev.package.\"*\".debug=false'")
        .count();
    assert_eq!(
        no_dep_debug, 2,
        "both careful cargo calls (build and test) must drop dependency debug info, else the test step relinks with full debug info"
    );
}

// ---------------------------------------------------------------------------
// Stress audit 2026-10-09: RO-5/SEC-2 (action pins), OPS-2 (reset-failed),
// RO-1 (no market-data deletion in a workflow).
// ---------------------------------------------------------------------------

/// Every workflow file under `.github/workflows/`, as `(path, text)`.
fn workflow_files() -> Vec<(String, String)> {
    let dir = repo_root().join(".github/workflows");
    let mut out: Vec<(String, String)> = fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("read_dir {} failed: {e}", dir.display()))
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "yml" || x == "yaml"))
        .map(|p| {
            let rel = format!(
                ".github/workflows/{}",
                p.file_name().unwrap_or_default().to_string_lossy()
            );
            let text = fs::read_to_string(&p).unwrap_or_else(|e| panic!("read {rel}: {e}"));
            (rel, text)
        })
        .collect();
    out.sort();
    out
}

/// The value of a `uses:` key on this line (`- uses: x` or `uses: x`), with
/// quotes and any trailing ` # comment` removed. `None` for every other line,
/// comment lines included.
fn uses_value(line: &str) -> Option<&str> {
    let t = line.trim_start();
    if t.starts_with('#') {
        return None;
    }
    let t = t.strip_prefix("- ").map_or(t, str::trim_start);
    let v = t.strip_prefix("uses:")?.trim();
    let v = v.split(" #").next().unwrap_or(v).trim();
    Some(v.trim_matches(|c| c == '"' || c == '\''))
}

/// `owner/repo[/path]@<40 lowercase hex>`.
fn is_full_sha_ref(value: &str) -> bool {
    let Some((name, git_ref)) = value.rsplit_once('@') else {
        return false;
    };
    !name.is_empty()
        && git_ref.len() == 40
        && git_ref
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Remote actions still allowed on a tag or branch, as `action@ref`.
///
/// SHRINK-ONLY: entries may be removed, never added, and
/// [`UNPINNED_ACTION_CEILING`] may only be lowered. Empty since 2026-10-09:
/// every remote action resolved to a commit with `git ls-remote`.
const UNPINNED_ACTION_ALLOWLIST: &[&str] = &[];

/// Upper bound on [`UNPINNED_ACTION_ALLOWLIST`]. May only go down.
const UNPINNED_ACTION_CEILING: usize = 0;

/// The `uses:` lines across `files` that name a remote action by anything
/// other than a full commit SHA (a `docker://` image needs an `@sha256:`
/// digest), minus the allowlist. Local `./` actions are ours and exempt.
fn unpinned_action_lines(files: &[(String, String)], allow: &[&str]) -> Vec<String> {
    let mut out = Vec::new();
    for (path, text) in files {
        for (i, line) in text.lines().enumerate() {
            let Some(v) = uses_value(line) else {
                continue;
            };
            if v.starts_with("./") || allow.contains(&v) {
                continue;
            }
            let pinned = match v.strip_prefix("docker://") {
                Some(image) => image.contains("@sha256:"),
                None => is_full_sha_ref(v),
            };
            if !pinned {
                out.push(format!("{path}:{}: {v}", i + 1));
            }
        }
    }
    out
}

// Regression: 2026-10-09 — actions holding production credentials (AWS keys,
// the GitHub token) were pinned to movable tags, so whoever could move the
// tag could run code with those credentials.
#[test]
fn test_regression_every_remote_action_is_pinned_to_a_commit_sha() {
    assert!(
        UNPINNED_ACTION_ALLOWLIST.len() <= UNPINNED_ACTION_CEILING,
        "the unpinned-action allowlist may only shrink"
    );
    let files = workflow_files();
    assert!(files.len() > 5, "found only {} workflows", files.len());
    let used: usize = files
        .iter()
        .map(|(_, t)| t.lines().filter(|l| uses_value(l).is_some()).count())
        .sum();
    assert!(
        used > 50,
        "found only {used} `uses:` lines; the scan is broken"
    );
    let bad = unpinned_action_lines(&files, UNPINNED_ACTION_ALLOWLIST);
    assert!(
        bad.is_empty(),
        "remote actions must be pinned to a 40-character commit SHA with the tag as \
         a trailing comment (resolve it with `git ls-remote <repo> refs/tags/<tag>` \
         and use the peeled `^{{}}` commit for an annotated tag):\n{}",
        bad.join("\n")
    );
    for entry in UNPINNED_ACTION_ALLOWLIST {
        assert!(
            files
                .iter()
                .any(|(_, t)| t.lines().any(|l| uses_value(l) == Some(*entry))),
            "allowlist entry {entry} is no longer used: remove it"
        );
    }
}

#[test]
fn unpinned_action_scan_bites() {
    let wf = |t: &str| vec![("w.yml".to_string(), t.to_string())];
    let sha = "3d3c42e5aac5ba805825da76410c181273ba90b1";
    assert_eq!(
        unpinned_action_lines(&wf("      - uses: actions/checkout@v7\n"), &[]).len(),
        1
    );
    assert_eq!(
        unpinned_action_lines(&wf("        uses: \"a/b@main\"  # note\n"), &[]).len(),
        1
    );
    assert_eq!(
        unpinned_action_lines(&wf("        uses: a/b@3d3c42e5aac5\n"), &[]).len(),
        1,
        "a short SHA is not a pin"
    );
    assert_eq!(
        unpinned_action_lines(&wf("        uses: docker://alpine:3\n"), &[]).len(),
        1
    );
    assert!(
        unpinned_action_lines(
            &wf(&format!(
                "      - uses: actions/checkout@{sha} # v7.0.1\n        uses: ./local\n      # uses: x/y@v1\n"
            )),
            &[]
        )
        .is_empty()
    );
    assert!(unpinned_action_lines(&wf("  uses: a/b@v1\n"), &["a/b@v1"]).is_empty());
}

// Regression: 2026-10-09 — the normal deploy swap restarted the app without
// `systemctl reset-failed` first, so a unit that had crash-looped into its
// start limit refused the restart and the FIX could not be deployed; the
// rollback then reinstalled the crashing binary.
#[test]
fn test_regression_deploy_swap_resets_a_failed_unit_before_restarting() {
    let wf = read(DEPLOY_AWS_WORKFLOW);
    let lines: Vec<&str> = wf.lines().collect();
    let restart = lines
        .iter()
        .position(|l| {
            !l.trim_start().starts_with('#')
                && l.contains("\"systemctl restart tickvault --no-block")
        })
        .expect("deploy-aws.yml must restart the app with --no-block");
    let from = restart.saturating_sub(DEPLOY_RESET_FAILED_WINDOW_LINES);
    let reset = lines[from..restart].iter().any(|l| {
        !l.trim_start().starts_with('#') && l.contains("systemctl reset-failed tickvault")
    });
    assert!(
        reset,
        "deploy-aws.yml must run `systemctl reset-failed tickvault` in the {} SSM \
         command(s) before `systemctl restart tickvault --no-block`",
        DEPLOY_RESET_FAILED_WINDOW_LINES
    );
}

// Regression: 2026-10-09 — the same missing `reset-failed` sat on the two
// other workflow paths that restart the app (the instance downsize and the
// operator restart / IP-change buttons in aws-control). A crash-locked unit
// refused each of those restarts too.
#[test]
fn test_regression_every_workflow_restart_resets_a_failed_unit_first() {
    let mut missing = Vec::new();
    let mut restarts = 0usize;
    for (rel, text) in workflow_files() {
        let lines: Vec<&str> = text.lines().collect();
        for (i, line) in lines.iter().enumerate() {
            if line.trim_start().starts_with('#') {
                continue;
            }
            // The app unit only: `tickvault-host-tuning` is a different unit.
            let Some(at) = line
                .match_indices("systemctl restart tickvault")
                .find_map(|(at, m)| {
                    let next = line[at + m.len()..].chars().next();
                    (!next.is_some_and(|c| c == '-' || c.is_ascii_alphanumeric())).then_some(at)
                })
            else {
                continue;
            };
            restarts += 1;
            let same_line = line[..at].contains("systemctl reset-failed tickvault");
            let from = i.saturating_sub(DEPLOY_RESET_FAILED_WINDOW_LINES);
            let before = lines[from..i].iter().any(|l| {
                !l.trim_start().starts_with('#') && l.contains("systemctl reset-failed tickvault")
            });
            if !same_line && !before {
                missing.push(format!("{rel}:{}", i + 1));
            }
        }
    }
    assert!(
        restarts >= WORKFLOW_APP_RESTART_FLOOR,
        "expected at least {WORKFLOW_APP_RESTART_FLOOR} workflow app restarts, found {restarts}"
    );
    assert!(
        missing.is_empty(),
        "every workflow `systemctl restart tickvault` must follow \
         `systemctl reset-failed tickvault` (same command or the {} before it): {missing:?}",
        DEPLOY_RESET_FAILED_WINDOW_LINES
    );
}

/// Workflow lines that restart the app today (deploy, downsize, two in
/// aws-control). A floor, so a rename that hides them fails loudly.
const WORKFLOW_APP_RESTART_FLOOR: usize = 4;

/// How many lines before the restart the reset may sit (the marker element
/// sits directly before it today).
const DEPLOY_RESET_FAILED_WINDOW_LINES: usize = 2;

/// Non-comment lines of `text` that delete raw capture, spill or dead-letter
/// data with `rm`, or drop or truncate a market-data table (named, or through
/// a shell variable).
fn market_data_deletions(text: &str) -> Vec<String> {
    const RAW_DIRS: &[&str] = &["ws_wal", "spill", "dlq"];
    const TABLES: &[&str] = &["ticks", "market_depth", "candles", "top_volume"];
    let mut out = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let t = line.trim_start();
        if t.starts_with('#') {
            continue;
        }
        let words: Vec<&str> = t.split_whitespace().collect();
        let rm_with_flags = words
            .windows(2)
            .any(|w| w[0] == "rm" && w[1].starts_with('-'));
        if rm_with_flags && RAW_DIRS.iter().any(|d| t.contains(d)) {
            out.push(format!("{}: {t}", i + 1));
            continue;
        }
        let upper = t.to_ascii_uppercase();
        let drops = upper.contains("DROP TABLE") || upper.contains("TRUNCATE TABLE");
        if drops && (TABLES.iter().any(|n| t.contains(n)) || t.contains('$')) {
            out.push(format!("{}: {t}", i + 1));
        }
    }
    out
}

// Regression: 2026-10-09 — emergency-fs-recover.yml ran `rm -rf` on ws_wal,
// spill and dlq and dropped ticks, market_depth and every candles_<tf> with
// no verified S3 copy. Quotes 21/22 are spent and Quote 28 (2026-09-29)
// forbids deleting captured market data without a verified copy.
#[test]
fn test_regression_no_workflow_deletes_market_data_without_a_verified_copy() {
    let mut bad = Vec::new();
    for (path, text) in workflow_files() {
        for hit in market_data_deletions(&text) {
            bad.push(format!("{path}:{hit}"));
        }
    }
    assert!(
        bad.is_empty(),
        "a workflow deletes captured market data. Quote 28 (2026-09-29) forbids it \
         without a verified copy; a deletion needs a fresh dated operator quote in \
         daily-universe-scope-expansion-2026-05-27.md first:\n{}",
        bad.join("\n")
    );
}

#[test]
fn market_data_deletion_scan_bites() {
    for line in [
        "sudo rm -rf /opt/tickvault/data/spill /opt/tickvault/data/dlq",
        "  sudo rm -rf /opt/tickvault/data/ws_wal",
        "sudo rm -f /opt/tickvault/data/spill-hold/*.ilp",
        r#"R=$(q "DROP TABLE IF EXISTS $t")"#,
        r#"curl ... "query=DROP TABLE ticks""#,
        r#"q "truncate table candles_1m""#,
    ] {
        assert_eq!(market_data_deletions(line).len(), 1, "{line}");
    }
    for line in [
        "# sudo rm -rf /opt/tickvault/data/ws_wal (history)",
        "rm -rf scripts/groww-sidecar",
        "- NEVER run `git reset --hard`, `rm -rf`, `DROP TABLE`, or",
        "sudo du -sBG /opt/tickvault/data/spill /opt/tickvault/data/ws_wal",
    ] {
        assert!(market_data_deletions(line).is_empty(), "{line}");
    }
}
