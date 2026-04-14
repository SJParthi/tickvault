//! S6-Step8: Lockdown test — the Tech Stack Bible was deleted by
//! Parthiban's explicit "fully delete the bible stack bro" decision.
//! This test prevents accidental reintroduction.
//!
//! If anyone restores `docs/architecture/tech-stack-bible.md`, this
//! test fails the build. To re-introduce it (which would create a
//! second source of truth alongside `Cargo.toml` — exactly the
//! drift problem we deleted it to fix), edit this test in the same
//! commit, which forces a visible review.

use std::path::Path;

#[test]
fn test_bible_file_does_not_exist() {
    let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .map(std::path::PathBuf::from)
        .expect("workspace root must exist above crates/common"); // APPROVED: test
    let bible_path = workspace_root
        .join("docs")
        .join("architecture")
        .join("tech-stack-bible.md");
    assert!(
        !bible_path.exists(),
        "S6-Step8 LOCKDOWN: tech-stack-bible.md was deleted per Parthiban's \
         explicit decision (2026-04-14) — Cargo.toml is the executable single \
         source of truth for versions. If you need to restore it, edit this \
         test in the same commit so the change is visible. Path: {}",
        bible_path.display()
    );
}

#[test]
fn test_no_bible_references_in_active_claudemd() {
    // The CLAUDE.md authority chain MUST NOT cite the Bible as authoritative.
    // Historical mentions in deletion notes are allowed — the check is for
    // the literal phrase "Tech Stack Bible V6 > this file" which was the
    // old authority-chain string.
    let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .map(std::path::PathBuf::from)
        .expect("workspace root must exist above crates/common"); // APPROVED: test
    let claudemd = workspace_root.join("CLAUDE.md");
    let content =
        std::fs::read_to_string(&claudemd).unwrap_or_else(|e| panic!("CLAUDE.md not found: {e}")); // APPROVED: test
    assert!(
        !content.contains("Tech Stack Bible V6 > this file"),
        "S6-Step8 LOCKDOWN: CLAUDE.md authority chain still cites the Bible. \
         Replace with 'Cargo.toml workspace deps + deny.toml + dhan_locked_facts.rs'."
    );
    // The "read Bible at startup" anti-pattern must also be gone.
    assert!(
        !content.contains("Do NOT read Bible at startup"),
        "S6-Step8 LOCKDOWN: CLAUDE.md still references the Bible startup rule."
    );
}
