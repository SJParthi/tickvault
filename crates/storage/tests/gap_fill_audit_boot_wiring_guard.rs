//! Phase 0 Item 8+9 (PR-D5, 2026-05-17) — boot-wiring ratchet for the
//! `ensure_gap_fill_audit_table` DDL helper.
//!
//! Background: PR-A (2026-05-17) shipped `append_gap_fill_audit_row` +
//! `ensure_gap_fill_audit_table` in `gap_fill_audit_persistence.rs`,
//! but only the row-writer was reachable from boot. The table-creation
//! helper was defined + unit-tested but never called from
//! `crates/app/src/main.rs` — Rule 13 (`audit-findings-2026-04-17.md`)
//! anti-pattern: "method exists + is tested but is never called in
//! production". PR-D4 (2026-05-17) then began writing audit rows on
//! every disconnect-resolved bar; without this DDL wired, the first
//! INSERT would silently fail until QuestDB happened to have the
//! table from some other path.
//!
//! PR-D5 wired the call into the boot-time `tokio::join!` (the same
//! block that already handles `ensure_phase2_audit_table`,
//! `ensure_boot_audit_table`, etc.). This ratchet pins that wiring so
//! a future refactor can't silently remove the call and re-introduce
//! the Rule 13 anti-pattern.

use std::fs;
use std::path::{Path, PathBuf};

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
}

#[test]
fn ensure_gap_fill_audit_table_called_from_main_rs() {
    let main_rs = workspace_root().join("crates/app/src/main.rs");
    let body =
        fs::read_to_string(&main_rs).unwrap_or_else(|e| panic!("read {}: {e}", main_rs.display()));

    assert!(
        body.contains("ensure_gap_fill_audit_table"),
        "Rule 13 regression: `ensure_gap_fill_audit_table` is no longer \
         referenced from `crates/app/src/main.rs`. PR-D4's per-bar \
         `append_gap_fill_audit_row` writes will silently fail with \
         'table not found' until something else creates the table. \
         Re-add the call to the boot-time `tokio::join!` DDL block \
         alongside the other `ensure_*_audit_table` helpers."
    );
}
