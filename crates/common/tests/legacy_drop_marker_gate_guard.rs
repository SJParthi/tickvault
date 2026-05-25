//! Ratchet for PR #798 (operator-locked 2026-05-25): the
//! `drop_legacy_candle_objects` cleanup MUST be gated by a one-shot
//! marker file. Repeating the 45+ DROP statements on every boot wastes
//! ~5s + 45 HTTP round-trips against QuestDB.
//!
//! Operator demand 2026-05-25 19:00 IST:
//! > "we need to remove all the stale unused unwanted not used
//! >  everything right dude am i right bro?"
//!
//! ## Pinned invariants
//!
//! 1. `LEGACY_DROP_MARKER_PATH` constant exists in shadow_persistence.rs
//! 2. Marker file is under `data/state/` (consistent with other markers)
//! 3. The function checks `marker_path.exists()` and returns early when true
//! 4. The function writes the marker after the DROP loop completes

use std::fs;
use std::path::{Path, PathBuf};

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .map(PathBuf::from)
        .expect("workspace root must exist above crates/common") // APPROVED: test
}

fn read_shadow_persistence() -> String {
    let p = workspace_root().join("crates/storage/src/shadow_persistence.rs");
    fs::read_to_string(&p).unwrap_or_else(|e| panic!("read {}: {e}", p.display())) // APPROVED: test
}

#[test]
fn test_marker_constant_defined() {
    let body = read_shadow_persistence();
    assert!(
        body.contains("LEGACY_DROP_MARKER_PATH"),
        "shadow_persistence.rs MUST define LEGACY_DROP_MARKER_PATH constant (PR #798)"
    );
}

#[test]
fn test_marker_path_uses_data_state_directory() {
    let body = read_shadow_persistence();
    assert!(
        body.contains(r#""data/state/legacy_candle_objects_dropped.marker""#),
        "marker path MUST be `data/state/legacy_candle_objects_dropped.marker` — \
         same directory as other one-shot markers"
    );
}

#[test]
fn test_marker_check_short_circuits_cleanup_loop() {
    let body = read_shadow_persistence();
    // The function MUST check marker_path.exists() and return early.
    // Pin the exact early-return pattern so future Claude sessions
    // can't accidentally remove it.
    assert!(
        body.contains("if marker_path.exists() {"),
        "drop_legacy_candle_objects MUST check `marker_path.exists()` to short-circuit \
         the DROP loop on subsequent boots"
    );
    assert!(
        body.contains("skipping (PR #798)"),
        "marker-present branch MUST log a 'skipping' message citing PR #798 so future \
         operators can grep for it"
    );
}

#[test]
fn test_marker_written_after_cleanup() {
    let body = read_shadow_persistence();
    // After the DROP loop completes, the function MUST call
    // `std::fs::write` to record the marker. Best-effort: warn on
    // failure, don't panic.
    assert!(
        body.contains("std::fs::write(") && body.contains("marker_path"),
        "drop_legacy_candle_objects MUST write the marker file after the DROP loop"
    );
}

#[test]
fn test_pr_798_marker_comment_present() {
    let body = read_shadow_persistence();
    assert!(
        body.contains("PR #798"),
        "shadow_persistence.rs MUST cite PR #798 marker comment so future Claude \
         sessions don't accidentally remove the marker-file gate"
    );
}
