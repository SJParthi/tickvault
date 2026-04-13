//! S3-6: Sanity check for cargo-deny configuration.
//!
//! The full `cargo deny check` runs in CI and as a best-effort pre-push
//! gate. This test runs unconditionally on every `cargo test` and verifies
//! that `deny.toml` at the workspace root exists and contains the key
//! section headers. It does NOT replace `cargo deny check` — it only
//! prevents `deny.toml` from being silently emptied or deleted during
//! unrelated edits.
//!
//! If this test fails, either:
//! - `deny.toml` was removed (restore it from git history)
//! - `deny.toml` was emptied (restore the section headers and re-run
//!   `cargo deny check` locally to fill in the details)
//!
//! Adding new sections? Update `REQUIRED_SECTIONS` below.

use std::path::Path;

const REQUIRED_SECTIONS: &[&str] = &["[graph]", "[advisories]", "[licenses]"];

fn read_deny_toml() -> String {
    let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .map(std::path::PathBuf::from)
        .expect("workspace root must exist above crates/common"); // APPROVED: test
    let path = workspace_root.join("deny.toml");
    std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "S3-6: deny.toml not found at {} — did the file get deleted? Error: {e}",
            path.display()
        )
    })
}

#[test]
fn deny_config_exists_and_has_required_sections() {
    let content = read_deny_toml();
    for section in REQUIRED_SECTIONS {
        assert!(
            content.contains(section),
            "S3-6: deny.toml is missing required section {section}. \
             Do NOT weaken cargo-deny enforcement without replacing the \
             equivalent check elsewhere."
        );
    }
}

#[test]
fn deny_config_targets_include_linux_and_mac() {
    let content = read_deny_toml();
    assert!(
        content.contains("x86_64-unknown-linux-gnu"),
        "S3-6: deny.toml must target Linux (AWS production)"
    );
    assert!(
        content.contains("aarch64-apple-darwin"),
        "S3-6: deny.toml must target macOS aarch64 (local development)"
    );
}

#[test]
fn deny_config_not_empty() {
    let content = read_deny_toml();
    assert!(
        content.len() > 200,
        "S3-6: deny.toml is suspiciously short ({} bytes) — did someone \
         gut it? Restore from git history.",
        content.len()
    );
}
