//! Ratchet: every `*.rs` filename in a CLAUDE.md TABLE ROW must actually exist.
//!
//! Why this guard exists (2026-08-08). CLAUDE.md is the codebase map every
//! Claude Code / Cowork session reads at startup per the SESSION PROTOCOL. On
//! 2026-08-08 an audit found that **all 7 rows** of its `crates/storage` table
//! named files that had been deleted — `tick_persistence.rs`,
//! `candle_persistence.rs`, `instrument_persistence.rs`,
//! `calendar_persistence.rs`, `materialized_views.rs`,
//! `deep_depth_persistence.rs`, `indicator_snapshot_persistence.rs`. The
//! tick-chain modules died in the 2026-07-17 stage-2 dead-WS sweep (PR #1631),
//! roughly three weeks before the drift was noticed.
//!
//! The table also advertised a "QuestDB ILP writer (zero-alloc hot path)" for
//! ticks, which no longer exists in any form (live market-data feeds retired
//! 2026-07-13 / 2026-07-15) — so the map misdescribed the ARCHITECTURE, not
//! just the paths. A stale map is worse than no map: it sends a fresh session
//! confidently to the wrong place.
//!
//! SCOPE — deliberately TABLE ROWS ONLY (lines beginning with `|`). CLAUDE.md
//! legitimately mentions deleted files in prose: dated correction banners,
//! historical notes, and retirement records all name modules that are GONE on
//! purpose, and house convention never rewrites dated history. Restricting the
//! scan to table rows targets exactly the "map" surface — the part a session
//! navigates by — while leaving the audit trail alone.

#![cfg(test)]

use std::collections::BTreeSet;
use std::path::PathBuf;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// Collect every distinct `` `something.rs` `` token appearing in a markdown
/// TABLE ROW of CLAUDE.md.
fn rs_files_named_in_table_rows(claude_md: &str) -> BTreeSet<String> {
    let mut found = BTreeSet::new();
    for line in claude_md.lines() {
        // Table rows only — see the SCOPE note in the module docs.
        if !line.trim_start().starts_with('|') {
            continue;
        }
        // Hand-rolled backtick-span scan: no regex dependency in this crate.
        let mut rest = line;
        while let Some(open) = rest.find('`') {
            let after_open = &rest[open.saturating_add(1)..];
            let Some(close) = after_open.find('`') else {
                break;
            };
            let span = &after_open[..close];
            // A cell may hold `a.rs` / `b.rs` alternatives — split on
            // whitespace and slashes so each filename is checked.
            for token in span.split(['/', ' ', ',']) {
                let token = token.trim();
                // Strip a `::member` suffix before the `.rs` test.
                //
                // 2026-08-21: without this the guard was blind to its OWN most
                // common citation style. The non-O(1) table cites
                // `path/to/file.rs::field`, and that token does not END with
                // ".rs" -- so all 22 such citations were skipped, including two
                // rows naming modules deleted hours earlier. The guard reported
                // a clean run while pointing at code that was gone, which is
                // precisely the failure it exists to catch, in its own scanner.
                let token = match token.find("::") {
                    Some(at) => &token[..at],
                    None => token,
                };
                if !token.ends_with(".rs") || token.is_empty() {
                    continue;
                }
                // Skip GLOB patterns. CLAUDE.md legitimately cites families of
                // files by pattern (e.g. `crates/*/tests/dhat_*.rs`,
                // `chaos_*.rs` in the TESTING STRATEGY table). Those are not
                // filenames and must not be existence-checked.
                if token.contains('*') || token.contains('?') {
                    continue;
                }
                found.insert(token.to_string());
            }
            rest = &after_open[close.saturating_add(1)..];
        }
    }
    found
}

/// Recursively test whether a file with this exact name exists under `crates/`.
fn exists_under_crates(root: &PathBuf, filename: &str) -> bool {
    fn walk(dir: &PathBuf, filename: &str, depth: usize) -> bool {
        if depth > 12 {
            return false;
        }
        let Ok(entries) = std::fs::read_dir(dir) else {
            return false;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                // Skip build output — it can contain generated copies.
                if path.file_name().is_some_and(|n| n == "target") {
                    continue;
                }
                if walk(&path, filename, depth.saturating_add(1)) {
                    return true;
                }
            } else if path.file_name().is_some_and(|n| n == filename) {
                return true;
            }
        }
        false
    }
    walk(&root.join("crates"), filename, 0)
}

#[test]
fn every_rs_file_named_in_claude_md_table_exists() {
    let root = repo_root();
    let claude_md = std::fs::read_to_string(root.join("CLAUDE.md"))
        .unwrap_or_else(|e| panic!("cannot read CLAUDE.md: {e}"));

    let named = rs_files_named_in_table_rows(&claude_md);

    // Non-vacuity: the codebase-map tables reference dozens of modules. If this
    // count collapses, the scanner (or the map) has been gutted and the guard
    // would pass while checking nothing — the false-OK class this repo forbids
    // (audit-findings Rule 11).
    assert!(
        named.len() >= 40,
        "CLAUDE.md table scan found only {} .rs filenames — expected >= 40. \
         Either the codebase-map tables were removed or this scanner broke. \
         A guard that checks nothing is worse than no guard.",
        named.len()
    );

    let missing: Vec<&String> = named
        .iter()
        .filter(|f| !exists_under_crates(&root, f))
        .collect();

    assert!(
        missing.is_empty(),
        "CLAUDE.md names {} file(s) in a TABLE ROW that do not exist anywhere \
         under crates/: {:?}\n\n\
         CLAUDE.md is the codebase map every session reads at startup. A row \
         pointing at a deleted module sends the next session to the wrong \
         place. Fix the table row (do NOT delete this guard).\n\
         If the file was intentionally removed, replace the row with the real \
         module and record the deletion in a dated note — prose is exempt from \
         this scan precisely so history can stay honest.",
        missing.len(),
        missing
    );
}

#[test]
fn scanner_detects_a_planted_ghost_row() {
    // Bite-proof: the scanner must actually catch a bad row, and must ignore
    // the same filename when it appears in prose rather than a table.
    let planted = "\
| File | Contains |\n\
|------|----------|\n\
| `definitely_not_a_real_module_xyz.rs` | ghost |\n";
    let found = rs_files_named_in_table_rows(planted);
    assert!(
        found.contains("definitely_not_a_real_module_xyz.rs"),
        "scanner failed to extract a filename from a table row"
    );

    let prose_only = "> Historical note: `definitely_not_a_real_module_xyz.rs` was deleted.\n";
    let found_prose = rs_files_named_in_table_rows(prose_only);
    assert!(
        found_prose.is_empty(),
        "scanner must IGNORE prose mentions (dated history names deleted files \
         on purpose), but extracted: {found_prose:?}"
    );
}

/// A `path.rs::member` citation must be extracted, not skipped.
///
/// This is the guard's OWN dominant citation style: the non-O(1) table in
/// CLAUDE.md names 22 paths that way. Until 2026-08-21 the scanner tested
/// `token.ends_with(".rs")` on the whole token, and `file.rs::field` does not
/// end with ".rs" -- so every one of them was silently skipped. Two rows
/// naming modules deleted the same day passed a green run.
///
/// The negative half matters as much: a `::member` suffix must not become a
/// way to smuggle a ghost path past the guard.
#[test]
fn scanner_sees_through_a_member_suffix() {
    let row = "| `crates/trading/src/candles/seal_ring.rs::push` | a real file |\n";
    let found = rs_files_named_in_table_rows(row);
    assert!(
        found.contains("seal_ring.rs"),
        "a `path.rs::member` citation must yield the filename: {found:?}"
    );

    // Bite proof: the same shape naming a file that does NOT exist must be
    // extracted too, so `every_rs_file_named_in_claude_md_table_exists` fails
    // on it rather than waving it through.
    let ghost = "| `crates/app/src/definitely_not_a_real_module_xyz.rs::field` | ghost |\n";
    let found_ghost = rs_files_named_in_table_rows(ghost);
    assert!(
        found_ghost.contains("definitely_not_a_real_module_xyz.rs"),
        "a ghost path with a `::member` suffix must still be caught: {found_ghost:?}"
    );
}

#[test]
fn scanner_skips_glob_patterns() {
    // CLAUDE.md's TESTING STRATEGY table cites file FAMILIES by pattern, e.g.
    // `crates/*/tests/dhat_*.rs` and `chaos_*.rs`. These are not filenames;
    // existence-checking them would fail the guard forever for no defect.
    // (This is precisely how the guard failed on first run — recorded so the
    // exemption is never mistaken for laziness.)
    let row = "| Zero-alloc | `dhat` | `crates/*/tests/dhat_*.rs` | verification |\n";
    let found = rs_files_named_in_table_rows(row);
    assert!(
        found.is_empty(),
        "glob patterns must be skipped, but scanner extracted: {found:?}"
    );

    // A concrete filename in the same cell shape must STILL be caught, so the
    // exemption cannot be abused to smuggle a real ghost past the guard.
    let mixed = "| x | `crates/trading/src/candles/seal_ring.rs` | real file |\n";
    let found_mixed = rs_files_named_in_table_rows(mixed);
    assert!(
        found_mixed.contains("seal_ring.rs"),
        "concrete filenames must still be extracted from path-shaped cells: {found_mixed:?}"
    );
}

#[test]
fn scanner_handles_multi_file_cells() {
    // Several real rows list alternatives in one cell, e.g.
    // `seal_writer_loop.rs` / `seal_writer_task.rs`. Both must be extracted.
    let row = "| `seal_writer_loop.rs` / `seal_writer_task.rs` | writer loop |\n";
    let found = rs_files_named_in_table_rows(row);
    assert!(
        found.contains("seal_writer_loop.rs") && found.contains("seal_writer_task.rs"),
        "multi-file cell not fully extracted: {found:?}"
    );
}
