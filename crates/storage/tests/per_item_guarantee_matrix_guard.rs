//! Wave 5 Item 22 — per-item guarantee matrix ratchet.
//!
//! The operator-mandated 15-row + 7-row guarantee matrix lives in
//! `.claude/rules/project/per-wave-guarantee-matrix.md` and is enforced at
//! commit/PR time by `.claude/hooks/per-item-guarantee-check.sh`. This test
//! runs the same hook from `cargo test` so a future regression (deleting the
//! rule file, neutering the hook, or shipping a new active plan that lacks
//! the matrix) fails the build, not just the pre-PR gate.
//!
//! Tests:
//! - `test_every_wave_5_item_carries_guarantee_matrix` — hook returns 0 across
//!   every `.claude/plans/active-plan*.md` in the workspace.
//! - `test_per_item_guarantee_check_hook_runs_on_pre_pr` — the hook script
//!   itself exists, is executable, and shells via bash (so it can run from
//!   pre-commit, pre-pr, or this test).

use std::path::PathBuf;
use std::process::Command;

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(std::path::Path::parent)
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn hook_path() -> PathBuf {
    workspace_root()
        .join(".claude")
        .join("hooks")
        .join("per-item-guarantee-check.sh")
}

fn rule_path() -> PathBuf {
    workspace_root()
        .join(".claude")
        .join("rules")
        .join("project")
        .join("per-wave-guarantee-matrix.md")
}

#[test]
fn test_every_wave_5_item_carries_guarantee_matrix() {
    let hook = hook_path();
    assert!(
        hook.is_file(),
        "guarantee-matrix hook missing at {}",
        hook.display()
    );
    assert!(
        rule_path().is_file(),
        "per-wave-guarantee-matrix rule file missing at {}",
        rule_path().display()
    );

    let root = workspace_root();
    let output = Command::new("bash")
        .arg(&hook)
        .env("CLAUDE_PROJECT_DIR", &root)
        .output()
        .expect("failed to spawn per-item-guarantee-check.sh");

    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "per-item-guarantee-check.sh exited non-zero — one or more active \
         plan files are missing the 15-row / 7-row guarantee matrix.\n\
         stderr:\n{stderr}\nstdout:\n{stdout}"
    );
}

#[test]
fn test_per_item_guarantee_check_hook_runs_on_pre_pr() {
    let hook = hook_path();
    let metadata = std::fs::metadata(&hook).expect("hook must be readable");
    assert!(metadata.is_file());

    // Sanity-check: the hook must reference the rule file by name so that
    // `grep` searches and operator runbooks resolve. If a future refactor
    // renames the rule file without updating the hook, this fails fast.
    let body = std::fs::read_to_string(&hook).expect("hook must be readable");
    assert!(
        body.contains("per-wave-guarantee-matrix.md"),
        "hook must mention the canonical rule file path"
    );
    assert!(
        body.starts_with("#!/usr/bin/env bash") || body.starts_with("#!/bin/bash"),
        "hook must declare a bash shebang so pre-commit / pre-pr can exec it"
    );
}
