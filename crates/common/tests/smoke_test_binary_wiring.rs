//! S7-Step4 / Phase 8.5: Lockdown test for the smoke_test binary.
//!
//! Prevents accidental deletion of the smoke test binary that the
//! AWS deploy workflow depends on. Checks the file exists, contains
//! the required sentinel string, and is wired into `crates/app`.

use std::path::Path;

#[test]
fn test_smoke_test_binary_exists() {
    let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .map(std::path::PathBuf::from)
        .expect("workspace root must exist above crates/common"); // APPROVED: test
    let smoke_path = workspace_root
        .join("crates")
        .join("app")
        .join("src")
        .join("bin")
        .join("smoke_test.rs");
    assert!(
        smoke_path.exists(),
        "S7-Step4: smoke_test.rs missing — AWS deploy workflow depends on it"
    );
}

#[test]
fn test_smoke_test_binary_has_required_checks() {
    let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .map(std::path::PathBuf::from)
        .expect("workspace root"); // APPROVED: test
    let content = std::fs::read_to_string(workspace_root.join("crates/app/src/bin/smoke_test.rs"))
        .expect("smoke_test.rs must be readable"); // APPROVED: test

    // Must exercise all 5 checks referenced in the AWS deploy workflow.
    for sentinel in &[
        "config file exists",
        "config parse + validate",
        "sandbox window check",
        "spill dir is writable",
        "Dhan locked facts",
    ] {
        assert!(
            content.contains(sentinel),
            "smoke_test.rs missing sentinel '{sentinel}' — deploy workflow invariant"
        );
    }
}
