//! Live-feed purity guard (Parthiban directive 2026-04-17).
//!
//! Scans the workspace source tree and fails the build if anyone
//! re-introduces the historical→ticks backfill path. This is the
//! second line of defence behind the pre-commit banned-pattern hook —
//! catches violations even when the hook is bypassed (e.g. CI against
//! a branch that landed before the hook).
//!
//! Ground rule: the `ticks` QuestDB table is populated EXCLUSIVELY by
//! `ParsedTick` frames originating from the WebSocket connection pool
//! (`crates/core/src/websocket/`) and processed through
//! `crates/core/src/pipeline/tick_processor.rs`. Historical REST-API
//! data belongs in `historical_candles`, never in `ticks`.
//!
//! See `.claude/rules/project/live-feed-purity.md` for the full rule.

#![cfg(test)]

use std::path::{Path, PathBuf};

/// Workspace root = `CARGO_MANIFEST_DIR` (crates/storage) ↑ ↑.
fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root must exist above crates/storage")
        .to_path_buf()
}

/// Recursively read every `.rs` file under `dir`, returning (path, contents).
fn read_rust_files(dir: &Path) -> Vec<(PathBuf, String)> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            out.extend(read_rust_files(&path));
        } else if path.extension().is_some_and(|e| e == "rs")
            && let Ok(content) = std::fs::read_to_string(&path)
        {
            out.push((path, content));
        }
    }
    out
}

/// Paths where live-feed-purity is enforced — historical data flow boundary.
/// Files under these paths MUST NOT touch tick-writer APIs.
fn historical_flow_paths() -> Vec<PathBuf> {
    let root = workspace_root();
    vec![
        root.join("crates/core/src/historical"),
        // Future-proofed — any new rest/backfill path would land here.
        root.join("crates/core/src/rest"),
        root.join("crates/core/src/backfill"),
    ]
}

// ============================================================================
// Purity guard — no backfill writes to `ticks`
// ============================================================================

/// Banned symbols in the historical flow. Any match = a synthetic tick
/// is about to be written (or has been written) into the `ticks` table.
const BANNED_IN_HISTORICAL_FLOW: &[&str] = &[
    "TickPersistenceWriter",
    "append_tick(",
    "BackfillWorker",
    "synthesize_ticks",
    "run_backfill",
    "GapBackfillRequest",
];

#[test]
fn live_feed_purity_no_tick_writer_in_historical_flow() {
    let mut violations: Vec<String> = Vec::new();

    for dir in historical_flow_paths() {
        if !dir.is_dir() {
            continue;
        }
        for (path, content) in read_rust_files(&dir) {
            for banned in BANNED_IN_HISTORICAL_FLOW {
                if let Some(line_idx) = content
                    .lines()
                    .enumerate()
                    .find(|(_, l)| l.contains(banned) && !l.trim_start().starts_with("//"))
                    .map(|(i, _)| i.saturating_add(1))
                {
                    violations.push(format!(
                        "{}:{} — banned symbol `{}` in historical data flow. \
                         Live-feed purity directive (2026-04-17): the `ticks` \
                         table is reserved for WebSocket-sourced ticks only. \
                         Historical REST data belongs in `historical_candles`.",
                        path.display(),
                        line_idx,
                        banned
                    ));
                }
            }
        }
    }

    assert!(
        violations.is_empty(),
        "LIVE-FEED-PURITY violation(s) — do NOT write synthetic ticks from \
         historical data into the `ticks` table.\n\n{}\n\n\
         Rule: .claude/rules/project/live-feed-purity.md",
        violations.join("\n\n")
    );
}

// ============================================================================
// Purity guard — the BackfillWorker file MUST stay deleted
// ============================================================================

#[test]
fn live_feed_purity_backfill_module_stays_deleted() {
    let backfill_file = workspace_root().join("crates/core/src/historical/backfill.rs");
    assert!(
        !backfill_file.exists(),
        "LIVE-FEED-PURITY violation — `{}` has been re-introduced. \
         Parthiban directive (2026-04-17): the BackfillWorker module is \
         permanently DELETED. Live feed and historical data are separate \
         functionalities. Rule: .claude/rules/project/live-feed-purity.md",
        backfill_file.display()
    );
}

#[test]
fn live_feed_purity_backfill_mod_declaration_stays_removed() {
    let historical_mod = workspace_root().join("crates/core/src/historical/mod.rs");
    let content = std::fs::read_to_string(&historical_mod)
        .expect("historical mod.rs must exist and be readable");
    let live_decl = content.lines().any(|line| {
        let trimmed = line.trim();
        !trimmed.starts_with("//") && trimmed.contains("pub mod backfill")
    });
    assert!(
        !live_decl,
        "LIVE-FEED-PURITY violation — `pub mod backfill` has been re-added \
         to `{}`. Parthiban directive (2026-04-17): the BackfillWorker \
         module is DELETED. Remove the declaration.",
        historical_mod.display()
    );
}

// ============================================================================
// Purity guard — the cross_verify module MAY only READ, never WRITE
// ============================================================================

#[test]
fn live_feed_purity_cross_verify_does_not_write_ticks() {
    let cross_verify = workspace_root().join("crates/core/src/historical/cross_verify.rs");
    if !cross_verify.is_file() {
        // Module optional — absence is fine.
        return;
    }
    let content = std::fs::read_to_string(&cross_verify).expect("readable");
    for banned in &["append_tick(", "TickPersistenceWriter::new"] {
        assert!(
            !content.contains(banned),
            "LIVE-FEED-PURITY violation — cross_verify.rs contains `{}`. \
             cross_verify READS both historical and live candles and compares \
             them; it must NEVER write to `ticks`.",
            banned
        );
    }
}

// ============================================================================
// Self-tests for the helpers
// ============================================================================

#[test]
fn self_test_workspace_root_contains_cargo_toml() {
    let cargo = workspace_root().join("Cargo.toml");
    assert!(
        cargo.is_file(),
        "workspace_root() must resolve to workspace"
    );
}

#[test]
fn self_test_historical_flow_paths_contain_historical_dir() {
    let paths = historical_flow_paths();
    assert!(paths.iter().any(|p| p.ends_with("historical")));
}
