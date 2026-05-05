//! Phase 4b dormancy guard — prevents accidental resurrection of the
//! legacy `StockMoversWriter` + `OptionMoversWriter` in production
//! paths after their 2026-05-03 retirement.
//!
//! ## What this ratchets
//!
//! The legacy movers writers were retired in the 2026-05-03 audit
//! (see `crates/app/src/main.rs:2728` "Audit-2026-05-03" comment).
//! Production paths now bind both writer params to `None`. The full
//! deletion of the writer code + the `DROP TABLE` of `stock_movers`
//! / `option_movers` is the operator-driven Phase 4b runbook
//! (`docs/runbooks/phase-4b-movers-cleanup.md`).
//!
//! Until the runbook completes, this test prevents anyone from
//! inadvertently re-instantiating the dead writers in `main.rs`,
//! `trading_pipeline.rs`, or any other production wiring file. Tests
//! ARE allowed to instantiate them (the writer code itself isn't
//! deleted yet, so the type still has unit tests).
//!
//! ## Why a source-scan ratchet
//!
//! `cargo test --workspace` already catches compile errors if the
//! writer types are deleted, but does NOT catch the regression
//! "someone re-binds the writer to `Some(StockMoversWriter::new(...))`
//! in main.rs". A source-scan ratchet catches the moment of
//! re-introduction, before it ships to a binary.
//!
//! The scan is deliberately narrow:
//! - Only flags `::new(` invocations on the writer types
//! - Only scans production source files (`crates/*/src/**/*.rs`)
//! - Skips test files (`/tests/`, `_test.rs`, `#[cfg(test)]` blocks)
//! - Skips comments (lines starting with `//`)

use std::path::PathBuf;

const BANNED_PATTERNS: &[&str] = &["StockMoversWriter::new(", "OptionMoversWriter::new("];

const SCAN_ROOTS: &[&str] = &[
    "crates/app/src",
    "crates/core/src",
    "crates/api/src",
    "crates/trading/src",
];

/// Walks every `.rs` file under `crates/<crate>/src/` (excluding
/// `tests/`, `benches/`, `_test.rs` files) and returns the first
/// production-path violation, if any.
fn scan_for_writer_resurrection() -> Option<String> {
    let workspace_root = workspace_root();
    for root in SCAN_ROOTS {
        let path = workspace_root.join(root);
        if !path.exists() {
            continue;
        }
        if let Some(violation) = walk_dir(&path) {
            return Some(violation);
        }
    }
    None
}

fn walk_dir(dir: &PathBuf) -> Option<String> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return None,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            // Skip nested test dirs.
            let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
            if name == "tests" || name == "benches" {
                continue;
            }
            if let Some(v) = walk_dir(&path) {
                return Some(v);
            }
            continue;
        }
        let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
        if !name.ends_with(".rs") {
            continue;
        }
        if name.ends_with("_test.rs") || name == "tests.rs" {
            continue;
        }
        if let Some(v) = scan_file(&path) {
            return Some(v);
        }
    }
    None
}

fn scan_file(path: &PathBuf) -> Option<String> {
    let content = std::fs::read_to_string(path).ok()?;
    let mut in_test_block = false;
    let mut brace_depth = 0_i32;
    for (lineno, raw_line) in content.lines().enumerate() {
        let line_num = lineno + 1;
        let trimmed = raw_line.trim_start();
        // Track #[cfg(test)] blocks via brace depth.
        if trimmed.contains("#[cfg(test)]") {
            in_test_block = true;
            brace_depth = 0;
            continue;
        }
        if in_test_block {
            for ch in raw_line.chars() {
                if ch == '{' {
                    brace_depth += 1;
                } else if ch == '}' {
                    brace_depth -= 1;
                    if brace_depth <= 0 {
                        in_test_block = false;
                        brace_depth = 0;
                    }
                }
            }
            continue;
        }
        // Skip pure comments.
        if trimmed.starts_with("//") || trimmed.starts_with("/*") || trimmed.starts_with("*") {
            continue;
        }
        // Strip inline comments before scanning.
        let code = match raw_line.split_once("//") {
            Some((before, _)) => before,
            None => raw_line,
        };
        for pattern in BANNED_PATTERNS {
            if code.contains(pattern) {
                return Some(format!(
                    "{}:{} — production-path code instantiates {}",
                    path.display(),
                    line_num,
                    pattern,
                ));
            }
        }
    }
    None
}

fn workspace_root() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    // CARGO_MANIFEST_DIR for tickvault-storage = .../crates/storage
    manifest
        .parent()
        .and_then(|p| p.parent())
        .map_or(manifest.clone(), |p| p.to_path_buf())
}

#[test]
fn legacy_movers_writers_must_not_be_instantiated_in_production() {
    if let Some(violation) = scan_for_writer_resurrection() {
        panic!(
            "Phase 4b dormancy guard tripped:\n\
             \n  {violation}\n\n\
             The legacy StockMoversWriter / OptionMoversWriter were retired \
             in the 2026-05-03 audit (main.rs:2728). Production code MUST \
             pass `None` for both writer params to `run_tick_processor`. \
             If you genuinely need to re-instantiate one (e.g. a regression \
             diagnostic), put the call inside `#[cfg(test)]` or a test file. \
             See docs/runbooks/phase-4b-movers-cleanup.md for the full \
             retirement plan."
        );
    }
}

#[test]
fn scan_for_writer_resurrection_returns_some_when_pattern_present() {
    // Self-test: the scan_file helper MUST detect a banned pattern in
    // a synthetic test fixture. Without this, a future refactor that
    // breaks the scanner would let the main ratchet pass green.
    let workspace_root = workspace_root();
    let tmp = workspace_root.join("target/movers_writer_dormant_guard_self_test.rs");
    std::fs::create_dir_all(tmp.parent().unwrap()).unwrap();
    std::fs::write(
        &tmp,
        "fn dummy() { let _ = StockMoversWriter::new(&cfg); }\n",
    )
    .unwrap();
    let result = scan_file(&tmp);
    let _ = std::fs::remove_file(&tmp);
    assert!(
        result.is_some(),
        "self-test failed: scan_file did not detect StockMoversWriter::new( in fixture"
    );
}

#[test]
fn scan_for_writer_resurrection_skips_comments() {
    let workspace_root = workspace_root();
    let tmp = workspace_root.join("target/movers_writer_dormant_guard_comment_test.rs");
    std::fs::create_dir_all(tmp.parent().unwrap()).unwrap();
    std::fs::write(
        &tmp,
        "// StockMoversWriter::new( in a comment must not trigger\n\
         fn ok() {}\n",
    )
    .unwrap();
    let result = scan_file(&tmp);
    let _ = std::fs::remove_file(&tmp);
    assert!(
        result.is_none(),
        "scan_file false-positive on comment: {result:?}"
    );
}

#[test]
fn banned_patterns_list_covers_both_writers() {
    assert!(
        BANNED_PATTERNS
            .iter()
            .any(|p| p.contains("StockMoversWriter"))
    );
    assert!(
        BANNED_PATTERNS
            .iter()
            .any(|p| p.contains("OptionMoversWriter"))
    );
    assert_eq!(BANNED_PATTERNS.len(), 2);
}
