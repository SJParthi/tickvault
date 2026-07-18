//! B9 deploy provenance — source-scan ratchet.
//!
//! Pins every wiring point of the provenance chain documented in
//! `.claude/rules/project/deploy-provenance.md`:
//!
//! ```text
//! TICKVAULT_BUILD_GIT_SHA/git ─▶ build.rs ─▶ BUILD_GIT_SHA ─▶ /health + boot Telegram
//! deploy-aws.yml ─▶ SSM binary-git-sha ─▶ portal footer + watchdog metric
//!                                          ─▶ 24h binary-sha-stale alarm
//! terraform ─▶ portal_git_sha var/output + binary_git_sha_ssm_param output
//! ```
//!
//! Deleting or renaming ANY link in that chain fails the build here first,
//! so a silent partial rollback is impossible (a deliberate rollback must
//! also revert this guard + the rule file in the same commit).

#![cfg(test)]

use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates/common parent")
        .parent()
        .expect("repo root")
        .to_path_buf()
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {} failed: {e}", path.display()))
}

/// 1+2 — the build script resolves the dedicated TICKVAULT_BUILD_GIT_SHA
/// env var (NEVER GitHub's own GITHUB_SHA — a rerun-if-env-changed on
/// GITHUB_SHA kills the target cache in every CI workflow, 2026-07-03
/// adversarial-review HIGH fix) and embeds TICKVAULT_GIT_SHA, and
/// build_info exposes it as BUILD_GIT_SHA.
#[test]
fn build_script_embeds_git_sha() {
    let root = repo_root();
    let build_rs = read(&root.join("crates/common/build.rs"));
    assert!(
        build_rs.contains("TICKVAULT_BUILD_GIT_SHA"),
        "build.rs must resolve the dedicated TICKVAULT_BUILD_GIT_SHA env var"
    );
    assert!(
        !build_rs.contains("rerun-if-env-changed=GITHUB_SHA"),
        "build.rs must NOT register rerun-if-env-changed on GITHUB_SHA — \
         it changes every commit in EVERY workflow and invalidates the \
         restored target cache (full workspace rebuild per commit)"
    );
    assert!(
        build_rs.contains("TICKVAULT_GIT_SHA"),
        "build.rs must emit cargo:rustc-env=TICKVAULT_GIT_SHA"
    );

    let build_info = read(&root.join("crates/common/src/build_info.rs"));
    assert!(
        build_info.contains("BUILD_GIT_SHA"),
        "build_info.rs must expose BUILD_GIT_SHA"
    );
    assert!(
        build_info.contains("env!(\"TICKVAULT_GIT_SHA\")"),
        "BUILD_GIT_SHA must come from the build-script env"
    );
}

/// 3 — /health reports the embedded SHA.
#[test]
fn health_endpoint_reports_git_sha() {
    let root = repo_root();
    let health = read(&root.join("crates/api/src/handlers/health.rs"));
    assert!(
        health.contains("git_sha"),
        "/health HealthResponse must carry the git_sha field"
    );
    assert!(
        health.contains("BUILD_GIT_SHA"),
        "/health git_sha must be populated from build_info::BUILD_GIT_SHA"
    );
}

/// 4 — the boot Telegram carries the short SHA.
#[test]
fn boot_telegram_carries_build_sha() {
    let root = repo_root();
    let events = read(&root.join("crates/core/src/notification/events.rs"));
    assert!(
        events.contains("build_git_sha_short"),
        "StartupComplete to_message must render build_info::build_git_sha_short()"
    );
    assert!(
        events.contains("Build: {build}"),
        "StartupComplete message must carry the 'Build: <sha>' line"
    );
}

/// 5 — the deploy workflow records the deployed SHA to SSM after a
/// verified swap (non-fatally), and the OIDC role can write it.
#[test]
fn deploy_workflow_records_binary_sha_to_ssm() {
    let root = repo_root();
    let workflow = read(&root.join(".github/workflows/deploy-aws.yml"));
    assert!(
        workflow.contains("/tickvault/prod/deploy/binary-git-sha"),
        "deploy-aws.yml must write the binary-git-sha SSM param post-swap"
    );
    assert!(
        workflow.contains("ssm put-parameter"),
        "deploy-aws.yml must use aws ssm put-parameter for the provenance write"
    );
    assert!(
        workflow.contains("provenance param write failed"),
        "the SSM provenance write must be NON-FATAL with a visible warning"
    );
    assert!(
        workflow.contains("TICKVAULT_BUILD_GIT_SHA"),
        "deploy-aws.yml build steps must set TICKVAULT_BUILD_GIT_SHA so the \
         shipped + smoke binaries embed the exact deploy sha"
    );
    assert!(
        workflow.contains("[0-9a-f]{40}"),
        "the SSM provenance write must hex-validate GITHUB_SHA before \
         recording it (refuse non-hex, still non-fatal)"
    );

    let oidc = read(&root.join("deploy/aws/terraform/oidc.tf"));
    assert!(
        oidc.contains("ssm:PutParameter") && oidc.contains("deploy/binary-git-sha"),
        "oidc.tf must grant ssm:PutParameter on exactly the binary-git-sha param"
    );
}

/// 6 — the operator portal renders the provenance footer.
/// (2026-07-18 rust-only phase 2b-3: the python handler.py is DELETED — the
/// pins now target the Rust port, same contract, never weakened.)
#[test]
fn portal_footer_renders_provenance_line() {
    let root = repo_root();
    let handler = read(&root.join("crates/aws-lambdas/src/operator_control.rs"));
    assert!(
        handler.contains("· portal") && handler.contains("· main"),
        "portal handler must format 'binary <7> · portal <7> · main <7>'"
    );
    assert!(
        handler.contains("fn provenance_line"),
        "portal handler must keep the pure provenance_line formatter"
    );
    assert!(
        handler.contains("^[0-9a-f]{7,40}$") && handler.contains("fn safe_provenance_sha"),
        "portal handler must hex-validate provenance SHAs (anchored \
         HEX_SHA_RE via safe_provenance_sha) before rendering — a poisoned \
         SSM param / GitHub response must never smuggle markup into the \
         footer"
    );

    let tf = read(&root.join("deploy/aws/terraform/operator-control-lambda.tf"));
    assert!(
        tf.contains("PORTAL_GIT_SHA") && tf.contains("BINARY_SHA_PARAM"),
        "operator-control terraform must inject PORTAL_GIT_SHA + BINARY_SHA_PARAM env"
    );
}

/// 7 — the watchdog publishes the mismatch metric, and terraform pins the
/// 24h staleness alarm on it.
#[test]
fn watchdog_publishes_binary_mismatch_metric() {
    let root = repo_root();
    let handler = read(&root.join("deploy/aws/lambda/deploy-watchdog/handler.py"));
    assert!(
        handler.contains("tv_binary_main_sha_mismatch"),
        "deploy-watchdog must publish the tv_binary_main_sha_mismatch metric"
    );
    assert!(
        handler.contains("binary_mismatch_value"),
        "deploy-watchdog must keep the pure binary_mismatch_value decision fn"
    );

    let tf = read(&root.join("deploy/aws/terraform/deploy-watchdog-lambda.tf"));
    assert!(
        tf.contains("tv_binary_main_sha_mismatch"),
        "deploy-watchdog terraform must alarm on tv_binary_main_sha_mismatch"
    );
    assert!(
        tf.contains("binary-sha-stale"),
        "the tv-<env>-binary-sha-stale alarm must exist"
    );
    assert!(
        tf.contains("notBreaching"),
        "the staleness alarm must treat missing data as notBreaching (box off / weekends never page)"
    );
}

/// 8 — terraform exposes the provenance outputs + CI wires the variable.
#[test]
fn terraform_outputs_expose_provenance() {
    let root = repo_root();
    let outputs = read(&root.join("deploy/aws/terraform/outputs.tf"));
    assert!(
        outputs.contains("portal_git_sha"),
        "outputs.tf must expose portal_git_sha"
    );
    assert!(
        outputs.contains("binary_git_sha_ssm_param"),
        "outputs.tf must expose binary_git_sha_ssm_param"
    );

    let variables = read(&root.join("deploy/aws/terraform/variables.tf"));
    assert!(
        variables.contains("portal_git_sha"),
        "variables.tf must define portal_git_sha (default \"unknown\")"
    );

    let tf_apply = read(&root.join(".github/workflows/terraform-apply.yml"));
    assert!(
        tf_apply.contains("TF_VAR_portal_git_sha"),
        "terraform-apply.yml must export TF_VAR_portal_git_sha for plan + apply"
    );
}

/// 9 — the rule file documenting the chain exists and names the chain.
#[test]
fn rule_file_exists_and_documents_chain() {
    let root = repo_root();
    let path = root.join(".claude/rules/project/deploy-provenance.md");
    assert!(
        path.exists(),
        ".claude/rules/project/deploy-provenance.md must exist"
    );
    let body = read(&path);
    for needle in [
        "TICKVAULT_GIT_SHA",
        "BUILD_GIT_SHA",
        "binary-git-sha",
        "tv_binary_main_sha_mismatch",
        "portal_git_sha",
    ] {
        assert!(
            body.contains(needle),
            "deploy-provenance.md must document '{needle}'"
        );
    }
}
