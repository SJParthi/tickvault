//! Wave-5 PR #509 — static-universe cleanup ratchet guards.
//!
//! Asserts that dead modules and helpers retired under the indices-only
//! scope (per `.claude/plans/active-plan-in-memory-store-aws-instance.md`
//! §R.1) do NOT come back. Each test is a single-purpose source-tree
//! scan — fast, no compile, no runtime state.

use std::path::PathBuf;

fn workspace_root() -> PathBuf {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest_dir
        .ancestors()
        .find(|p| p.join("Cargo.toml").exists() && p.join("crates").exists())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            manifest_dir
                .parent()
                .and_then(|p| p.parent())
                .map(PathBuf::from)
                .unwrap_or(manifest_dir)
        })
}

#[test]
fn test_live_tick_atm_resolver_file_does_not_exist() {
    let path = workspace_root().join("crates/core/src/instrument/live_tick_atm_resolver.rs");
    assert!(
        !path.exists(),
        "live_tick_atm_resolver.rs MUST be deleted under indices-only scope (PR #509). \
         Found at {}. Stock F&O ATM resolution is dead — see plan §R.1.",
        path.display()
    );
}

#[test]
fn test_no_pub_mod_live_tick_atm_resolver_in_instrument_mod() {
    let path = workspace_root().join("crates/core/src/instrument/mod.rs");
    let src =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {}", path.display(), e));
    assert!(
        !src.contains("pub mod live_tick_atm_resolver"),
        "instrument/mod.rs MUST NOT declare `pub mod live_tick_atm_resolver` \
         (file was deleted in PR #509)."
    );
}
