//! Ratchet guard for the B7 coverage-honesty sweep (2026-07-03).
//!
//! The executable truth is `quality/crate-coverage-thresholds.toml` — ratcheted
//! per-crate LINE-coverage floors (app 63.3 … common 99.5; floors only move up,
//! 100% is the target), enforced post-merge by `scripts/coverage-gate.sh`. The
//! B7 directive replaced every FALSE "100% coverage enforced" documentation
//! claim with that honest wording, aligned `make coverage` to the SAME per-crate
//! gate as CI, and gave the QuestDB-backed Groww e2e a CI lane that can never
//! vacuously pass (audit-findings Rule 11: no false-OK signals).
//!
//! These source-scan tests pin all of that so the false claims cannot silently
//! return.

use std::path::PathBuf;

/// Repo root = crates/storage/../../
fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../.."))
}

fn read(rel: &str) -> String {
    let path = repo_root().join(rel);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read {} (B7 honesty guard): {e}", path.display()))
}

/// ci.yml must describe the coverage gate as the ratcheted per-crate floors —
/// never the false "per-crate 100% thresholds" (B7 coverage honesty).
#[test]
fn coverage_claims_in_ci_yml_are_honest() {
    let ci = read(".github/workflows/ci.yml");
    assert!(
        !ci.contains("per-crate 100% thresholds"),
        "ci.yml regressed to the FALSE 'per-crate 100% thresholds' claim — the real \
         gate enforces ratcheted per-crate floors (B7 coverage honesty / audit Rule 11)"
    );
    assert!(
        ci.contains("ratcheted per-crate floors"),
        "ci.yml coverage-and-perf comment must describe the gate honestly as \
         'ratcheted per-crate floors' (B7 coverage honesty)"
    );
}

/// CLAUDE.md must not claim a 100% minimum for all crates — the enforced
/// floors are 63.3–99.5 (target 100%).
#[test]
fn claude_md_does_not_claim_100pct_minimum_for_all_crates() {
    let claude_md = read("CLAUDE.md");
    assert!(
        !claude_md.contains("100% minimum for ALL crates"),
        "CLAUDE.md regressed to the FALSE '100% minimum for ALL crates' coverage \
         claim — the real floors are ratcheted per crate in \
         quality/crate-coverage-thresholds.toml (B7 coverage honesty / audit Rule 11)"
    );
}

/// guarantees.md must state the ratcheted floors and drop the false
/// "100% line coverage required per crate" claim.
#[test]
fn guarantees_doc_states_ratcheted_floors() {
    let doc = read("docs/architecture/guarantees.md");
    assert!(
        !doc.contains("100% line coverage required per crate"),
        "docs/architecture/guarantees.md regressed to the FALSE '100% line coverage \
         required per crate' claim (B7 coverage honesty / audit Rule 11)"
    );
    assert!(
        doc.to_lowercase().contains("ratcheted"),
        "docs/architecture/guarantees.md must describe the coverage gate as \
         RATCHETED per-crate floors (B7 coverage honesty)"
    );
}

/// `make coverage` must run the SAME per-crate gate as CI — never a flat
/// `--fail-under-lines` that contradicts the TOML floors.
#[test]
fn makefile_coverage_target_uses_the_real_per_crate_gate() {
    let makefile = read("Makefile");
    assert!(
        !makefile.contains("--fail-under-lines"),
        "Makefile regressed to a flat `--fail-under-lines` coverage gate — \
         `make coverage` must enforce the SAME per-crate TOML floors as CI via \
         scripts/coverage-gate.sh (B7 coverage honesty)"
    );
    assert!(
        makefile.contains("scripts/coverage-gate.sh"),
        "Makefile `coverage` target must invoke scripts/coverage-gate.sh so local \
         runs enforce the same per-crate floors as CI (B7 coverage honesty)"
    );
}

/// The Groww QuestDB e2e CI lane must exist with both anti-false-OK guards:
/// the no-SKIP grep and the PROVED-count assertion, run under TV_REQUIRE_QDB=1.
#[test]
fn groww_e2e_workflow_exists_with_no_skip_guard_and_proved_assertion() {
    let wf = read(".github/workflows/groww-e2e.yml");
    assert!(
        wf.contains("groww_live_pipeline_e2e"),
        "groww-e2e.yml must run the groww_live_pipeline_e2e integration test \
         (B7 Groww CI lane)"
    );
    assert!(
        wf.contains("TV_REQUIRE_QDB=1"),
        "groww-e2e.yml must set TV_REQUIRE_QDB=1 so an unreachable QuestDB fails \
         the lane instead of skipping (audit Rule 11: no false-OK signals)"
    );
    assert!(
        wf.contains("SKIP "),
        "groww-e2e.yml must grep the log for 'SKIP ' lines and fail on any — \
         a skipped e2e is a vacuous pass (audit Rule 11)"
    );
    assert!(
        wf.contains("PROVED:"),
        "groww-e2e.yml must assert on the count of 'PROVED:' lines (>=3) so a \
         silently-deleted test cannot produce a vacuous green (audit Rule 11)"
    );
}

/// The e2e test itself must carry the TV_REQUIRE_QDB hard-fail branch so the
/// CI lane's guarantee is enforced inside the test process too.
#[test]
fn groww_e2e_has_tv_require_qdb_hard_fail_branch() {
    let e2e = read("crates/app/tests/groww_live_pipeline_e2e.rs");
    assert!(
        e2e.contains("TV_REQUIRE_QDB"),
        "groww_live_pipeline_e2e.rs lost the TV_REQUIRE_QDB hard-fail branch — \
         with the env var set, an unreachable QuestDB must PANIC instead of \
         skipping (B7 Groww CI lane / audit Rule 11)"
    );
    assert!(
        e2e.contains("qdb_required"),
        "groww_live_pipeline_e2e.rs must keep the qdb_required() helper gating \
         the panic-vs-skip decision in every QuestDB-backed test (B7 Groww CI lane)"
    );
}
