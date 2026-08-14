//! Ratchet: every `[[bin]]` in `crates/aws-lambdas/Cargo.toml` must appear in
//! the `BINS` list of EVERY "Build Rust lambda zips" step in
//! `.github/workflows/terraform-apply.yml`.
//!
//! # Why this exists (2026-08-09 incident, Verified)
//!
//! `dhan-token-minter` was added as a `[[bin]]` target and referenced by
//! `deploy/aws/terraform/dhan-token-minter-lambda.tf`, but was never added to
//! the workflow's `BINS` list. `cargo lambda build -p tickvault-aws-lambdas`
//! builds every bin in the crate, so the BUILD was green; only the `cp` loop
//! is driven by `BINS`, so the zip was silently never copied into
//! `.lambda-zips/`.
//!
//! Terraform reads that zip with `filename = "./.lambda-zips/<bin>.zip"`, and
//! a missing file is not caught at plan time — it fails during APPLY:
//!
//! ```text
//! Error: reading ZIP file (./.lambda-zips/dhan-token-minter.zip):
//!        open ./.lambda-zips/dhan-token-minter.zip: no such file or directory
//! ```
//!
//! That is the worst possible moment to fail. The 2026-08-09 apply had already
//! replaced the EC2 instance, re-attached the Elastic IP and modified 29
//! resources before it hit this error and aborted, leaving the workflow red
//! with infrastructure half-converged. A missing entry in a shell string is
//! therefore a PRODUCTION-INFRASTRUCTURE hazard, not a cosmetic drift, and it
//! belongs in a build-failing gate rather than in review attention.
//!
//! # Honest envelope
//!
//! This guard proves the two lists AGREE. It does NOT prove that a listed bin
//! actually compiles, that the zip is well-formed, or that terraform
//! references it — those are covered by the build itself and by apply.
//! It is deliberately direction-symmetric: a bin in `BINS` that does not exist
//! in Cargo.toml also fails, because the `cp` loop would abort the step under
//! `set -euo pipefail`.

use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    // CARGO_MANIFEST_DIR = <root>/crates/common
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/common must sit two levels below the workspace root")
        .to_path_buf()
}

/// Parse `[[bin]]` / `name = "..."` pairs out of the aws-lambdas manifest.
///
/// Deliberately a small hand-rolled scan rather than a TOML dependency: this
/// crate's dev-dependencies are pinned and adding one for a guard test would
/// need operator approval per the CARGO rules in CLAUDE.md.
fn declared_bin_names(manifest: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut in_bin_section = false;

    for raw in manifest.lines() {
        let line = raw.trim();

        if line.starts_with('[') {
            // Any new section header ends the previous [[bin]] block.
            in_bin_section = line == "[[bin]]";
            continue;
        }

        if !in_bin_section {
            continue;
        }

        if let Some(rest) = line.strip_prefix("name") {
            let rest = rest.trim_start();
            if let Some(rest) = rest.strip_prefix('=') {
                let value = rest.trim().trim_matches('"');
                if !value.is_empty() {
                    names.push(value.to_string());
                }
            }
        }
    }

    names
}

/// Extract every `BINS="..."` assignment in the workflow.
///
/// Returns one entry per occurrence — the workflow defines the build step
/// twice (the plan job and the apply job) and BOTH must carry every bin.
/// Checking only the first would let the apply-side list rot undetected,
/// which is precisely the side that fails destructively.
fn workflow_bins_lists(workflow: &str) -> Vec<Vec<String>> {
    let mut lists = Vec::new();

    for raw in workflow.lines() {
        let line = raw.trim();
        let Some(rest) = line.strip_prefix("BINS=") else {
            continue;
        };
        let value = rest.trim().trim_matches('"');
        lists.push(value.split_whitespace().map(str::to_string).collect());
    }

    lists
}

fn load(rel: &str) -> String {
    let path = repo_root().join(rel);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()))
}

#[test]
fn every_declared_lambda_bin_is_zipped_by_every_build_step() {
    let declared = declared_bin_names(&load("crates/aws-lambdas/Cargo.toml"));
    let lists = workflow_bins_lists(&load(".github/workflows/terraform-apply.yml"));

    assert!(
        !declared.is_empty(),
        "parsed zero [[bin]] targets from crates/aws-lambdas/Cargo.toml — the \
         parser is broken, not the manifest; this guard would pass vacuously"
    );
    assert!(
        !lists.is_empty(),
        "parsed zero BINS= assignments from terraform-apply.yml — the parser \
         is broken or the build step was renamed; this guard would pass vacuously"
    );

    for (idx, listed) in lists.iter().enumerate() {
        for bin in &declared {
            assert!(
                listed.contains(bin),
                "BINS list #{} in .github/workflows/terraform-apply.yml is missing \
                 the lambda bin `{bin}` declared in crates/aws-lambdas/Cargo.toml.\n\
                 \n\
                 The build would still be GREEN — cargo lambda builds the whole \
                 crate — but the zip is never copied to .lambda-zips/, and \
                 terraform fails during APPLY with:\n\
                 \n    Error: reading ZIP file (./.lambda-zips/{bin}.zip): \
                 no such file or directory\n\
                 \n\
                 by which point real infrastructure has already been changed. \
                 Add `{bin}` to the BINS string.",
                idx + 1
            );
        }
    }
}

#[test]
fn every_zipped_bin_is_actually_declared() {
    // The reverse direction: `cp target/lambda/<bin>/bootstrap.zip` runs under
    // `set -euo pipefail`, so a stale name left in BINS after a bin is renamed
    // or removed hard-fails the build step.
    let declared = declared_bin_names(&load("crates/aws-lambdas/Cargo.toml"));
    let lists = workflow_bins_lists(&load(".github/workflows/terraform-apply.yml"));

    for (idx, listed) in lists.iter().enumerate() {
        for bin in listed {
            assert!(
                declared.contains(bin),
                "BINS list #{} names `{bin}`, which is not a [[bin]] target in \
                 crates/aws-lambdas/Cargo.toml. The `cp` loop would fail the \
                 build step (set -euo pipefail). Remove it or restore the target.",
                idx + 1
            );
        }
    }
}

#[test]
fn both_build_steps_carry_identical_lists() {
    let lists = workflow_bins_lists(&load(".github/workflows/terraform-apply.yml"));
    assert_eq!(
        lists.len(),
        2,
        "expected exactly 2 BINS= assignments (the plan job and the apply job); \
         found {}. If a build step was added or removed, update this guard \
         deliberately — do not relax it.",
        lists.len()
    );

    let mut first = lists[0].clone();
    let mut second = lists[1].clone();
    first.sort();
    second.sort();
    assert_eq!(
        first, second,
        "the plan-job and apply-job BINS lists have diverged. They must be \
         identical: the plan job produces the zips whose digest terraform \
         plans against, and the apply job re-produces them."
    );
}

#[test]
fn the_2026_08_09_regression_is_pinned() {
    // Non-vacuity: name the specific bin whose absence caused the incident, so
    // that a future edit which drops it fails with the history attached rather
    // than only through the generic loop above.
    let lists = workflow_bins_lists(&load(".github/workflows/terraform-apply.yml"));
    for (idx, listed) in lists.iter().enumerate() {
        assert!(
            listed.iter().any(|b| b == "dhan-token-minter"),
            "BINS list #{} lost `dhan-token-minter`. Its absence aborted the \
             2026-08-09 terraform apply mid-run, after the EC2 rebuild had \
             already happened. See §10 of \
             .claude/rules/project/groww-shared-token-minter-2026-07-02.md.",
            idx + 1
        );
    }
}

#[test]
fn parser_self_test_detects_a_missing_entry() {
    // Guard-the-guard: prove the parsers actually discriminate, so a silent
    // parsing failure can never render the assertions above vacuous.
    let manifest = "[package]\nname = \"x\"\n\n[[bin]]\nname = \"alpha\"\npath = \"a.rs\"\n\n[[bin]]\nname = \"beta\"\n";
    let parsed = declared_bin_names(manifest);
    assert_eq!(parsed, vec!["alpha".to_string(), "beta".to_string()]);

    // A `name =` under [package] must NOT be picked up as a bin.
    assert!(!parsed.contains(&"x".to_string()));

    let workflow = "      run: |\n          BINS=\"alpha\"\n          echo hi\n          BINS=\"alpha beta\"\n";
    let lists = workflow_bins_lists(workflow);
    assert_eq!(lists.len(), 2);
    assert_eq!(lists[0], vec!["alpha".to_string()]);
    assert_eq!(lists[1], vec!["alpha".to_string(), "beta".to_string()]);

    // The real assertion shape: list #1 is missing `beta`.
    assert!(!lists[0].contains(&"beta".to_string()));
}
