//! Live-feed purity guard (Parthiban directive 2026-04-17).
//!
//! Scans the workspace source tree and fails the build if anyone
//! re-introduces the historical→ticks backfill path. This is the
//! second line of defence behind the pre-commit banned-pattern hook —
//! catches violations even when the hook is bypassed (e.g. CI against
//! a branch that landed before the hook).
//!
//! Ground rule: the `ticks` QuestDB table holds LIVE-feed-sourced ticks
//! ONLY. Historical REST-API data NEVER crosses into `ticks` (the entire
//! Dhan historical fetch chain was deleted in PRs #803-#807;
//! `historical_candles` table is gone). Stage-2 dead-WS sweep
//! (2026-07-17): the live tick WRITE path itself (`tick_processor.rs` +
//! `tick_persistence.rs`) was deleted with the dead Dhan tick chain — the
//! table is now read-only (SEBI-retained), which makes this ban strictly
//! stronger: NO writer may be re-introduced from any historical/synth
//! path (the banned-symbol needles below stay as the tripwire).
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
    // 2026-08-10: was `else { return out; }` — an unreadable directory silently
    // became "nothing to check, pass". Corpus-read failure now fails LOUD.
    let entries = std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("LIVE-FEED-PURITY guard corpus unreadable {dir:?}: {e}"));
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
    // 2026-08-10 REPOINT. The previous three targets —
    // crates/core/src/{historical,rest,backfill} — were DELETED from the tree,
    // and because the scan loop did `if !dir.is_dir() { continue; }` this guard
    // passed while reading ZERO files. It had been asleep for an unknown period.
    //
    // That was not theoretical: `crates/storage/src/tick_persistence.rs` (which
    // owns `append_tick(`) was REBUILT for the Dhan revival while this guard was
    // blind, and `TickPersistenceWriter` now appears in 5 source files. Those
    // live-feed homes are legitimate — the invariant is that the REST/historical
    // flow must never reach them.
    //
    // Targets are now the REST/historical flow that ACTUALLY EXISTS. They are
    // files, not directories, so the scan handles both. Every one is asserted to
    // exist: a deleted target must fail loudly and be repointed deliberately,
    // never skipped silently.
    vec![
        root.join("crates/app/src/spot_1m_rest_boot.rs"),
        root.join("crates/app/src/rest_candle_fold.rs"),
        root.join("crates/app/src/dhan_rest_stack.rs"),
        root.join("crates/app/src/groww_rest_burst.rs"),
        root.join("crates/storage/src/spot_1m_rest_persistence.rs"),
        root.join("crates/storage/src/option_contract_1m_rest_persistence.rs"),
        root.join("crates/storage/src/rest_fetch_audit_persistence.rs"),
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

    let mut scanned_files: usize = 0;
    for target in historical_flow_paths() {
        assert!(
            target.exists(),
            "LIVE-FEED-PURITY guard is BLIND: scan target {target:?} does not \
             exist. A missing target used to be skipped silently, which let this \
             guard pass while reading ZERO files. Repoint historical_flow_paths() \
             at the current REST/historical flow, or remove the entry \
             deliberately — never leave it dangling."
        );
        let files = if target.is_dir() {
            read_rust_files(&target)
        } else {
            let content = std::fs::read_to_string(&target).unwrap_or_else(|e| {
                panic!("LIVE-FEED-PURITY guard cannot read scan target {target:?}: {e}")
            });
            vec![(target.clone(), content)]
        };
        scanned_files = scanned_files.saturating_add(files.len());
        for (path, content) in files {
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
                         The entire Dhan historical fetch chain was \
                         deleted in PR-E (2026-05-26).",
                        path.display(),
                        line_idx,
                        banned
                    ));
                }
            }
        }
    }

    // Anti-vacuity: a guard that scanned nothing must FAIL, not pass. This is the
    // assertion whose absence let the guard sleep.
    assert!(
        scanned_files > 0,
        "LIVE-FEED-PURITY guard scanned ZERO files — it is enforcing NOTHING. \
         This assert exists because the guard previously passed in exactly that \
         state for an unknown period."
    );

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
    // PR-E (2026-05-26) deleted the entire `crates/core/src/historical/`
    // directory. Absent file = strictly stronger guarantee than
    // "file exists without `pub mod backfill`". Tolerate both states:
    // - file absent (current state — historical/ deleted)
    // - file present but does NOT contain `pub mod backfill`
    let historical_mod = workspace_root().join("crates/core/src/historical/mod.rs");
    match std::fs::read_to_string(&historical_mod) {
        Err(_) => {
            // historical/ directory absent — backfill module cannot
            // possibly exist. Test passes trivially.
        }
        Ok(content) => {
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
    }
}

// PR-C (2026-05-26): cross_verify module DELETED entirely — purity guard
// against `cross_verify.rs writing to ticks` is no longer needed.

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
fn self_test_historical_flow_paths_all_exist_on_disk() {
    // 2026-08-10 REPLACED A TAUTOLOGY. This previously asserted that the
    // hardcoded vec returned by historical_flow_paths() contained a path ending
    // in "historical" — true regardless of whether that path existed on disk. It
    // was the check meant to PROVE the guard was working, and it is precisely why
    // the guard's blindness went unnoticed: it reported healthy while every real
    // target had been deleted. A fake non-vacuity test is worse than none,
    // because it converts "unverified" into "verified".
    //
    // It now asserts the thing that actually matters: the targets EXIST.
    let paths = historical_flow_paths();
    assert!(
        !paths.is_empty(),
        "historical_flow_paths() is empty — the guard would scan nothing"
    );
    for p in paths {
        assert!(
            p.exists(),
            "historical_flow_paths() names {p:?}, which does not exist on disk — \
             the guard would silently scan nothing for this target"
        );
    }
}
