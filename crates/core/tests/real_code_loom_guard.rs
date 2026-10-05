//! Pins how many loom tests model a hand-written copy instead of driving the
//! real code (audit H3, 2026-10-05).
//!
//! A loom test that rebuilds the production pattern by hand cannot fail when
//! the production code changes: it checks loom, not us. The audit found five
//! loom files and every one was a copy; two now drive the real types
//! (`loom_circuit_breaker.rs` drives `OrderCircuitBreaker`,
//! `loom_ghost_register.rs` drives `GhostRegister`). The other three model a
//! pattern whose real code loom cannot run (tokio timers, `Notify`, `mpsc`) or
//! whose real code was deleted.
//!
//! The rule this guard enforces, for every `crates/*/tests/loom_*.rs`:
//!
//! - it imports a workspace crate (`use tickvault_...`) so it drives real
//!   code, OR
//! - its header says `PATTERN MODEL` and gives the reason.
//!
//! The number of pattern-model files may only shrink. Converting one to real
//! code means lowering `MAX_PATTERN_MODEL_FILES` in the same change; adding a
//! new pattern-model file fails the build.
//!
//! Note: this file must not be named `loom_*.rs`; the CI loom lane lists those
//! files explicitly and runs each with `--features loom`.

use std::fs;
use std::path::{Path, PathBuf};

/// Loom files that model a pattern rather than driving production code.
/// Shrink-only. Today: activity watchdog, tick dedup, socket decoupling.
const MAX_PATTERN_MODEL_FILES: usize = 3;

/// The header marker a pattern-model file must carry.
const PATTERN_MODEL_MARKER: &str = "PATTERN MODEL";

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .map(PathBuf::from)
        .expect("workspace root must exist above crates/core") // APPROVED: test
}

/// Every `crates/*/tests/loom_*.rs`, sorted.
fn loom_files() -> Vec<PathBuf> {
    let crates_dir = workspace_root().join("crates");
    let mut out = Vec::new();
    let crates = fs::read_dir(&crates_dir).expect("crates/ must be readable"); // APPROVED: test
    for krate in crates.flatten() {
        let tests_dir = krate.path().join("tests");
        let Ok(entries) = fs::read_dir(&tests_dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if name.starts_with("loom_") && name.ends_with(".rs") {
                out.push(path);
            }
        }
    }
    out.sort();
    out
}

/// True when the file imports a workspace crate outside a comment line.
fn drives_real_code(source: &str) -> bool {
    source.lines().any(|line| {
        let trimmed = line.trim_start();
        !trimmed.starts_with("//") && trimmed.starts_with("use tickvault_")
    })
}

/// True when the file's `//!` header carries the pattern-model marker.
fn declares_pattern_model(source: &str) -> bool {
    source
        .lines()
        .take_while(|line| line.starts_with("//!") || line.trim().is_empty())
        .any(|line| line.contains(PATTERN_MODEL_MARKER))
}

#[test]
fn every_loom_file_drives_real_code_or_says_why_not() {
    let files = loom_files();
    assert!(
        files.len() >= 5,
        "found only {} loom files; the scan is probably looking in the wrong place",
        files.len()
    );

    let mut unlabelled = Vec::new();
    let mut pattern_models = Vec::new();
    for path in &files {
        let source = fs::read_to_string(path).expect("loom file must be readable"); // APPROVED: test
        if drives_real_code(&source) {
            continue;
        }
        if declares_pattern_model(&source) {
            pattern_models.push(path.display().to_string());
        } else {
            unlabelled.push(path.display().to_string());
        }
    }

    assert!(
        unlabelled.is_empty(),
        "these loom files neither import a tickvault crate nor carry a `{PATTERN_MODEL_MARKER}` \
         header saying why they cannot drive the real code: {unlabelled:?}"
    );
    assert!(
        pattern_models.len() <= MAX_PATTERN_MODEL_FILES,
        "{} pattern-model loom files, budget {MAX_PATTERN_MODEL_FILES} (shrink-only): {pattern_models:?}. \
         Make the new loom test drive the real type through the crate's `sync` shim.",
        pattern_models.len()
    );
    assert_eq!(
        pattern_models.len(),
        MAX_PATTERN_MODEL_FILES,
        "only {} pattern-model loom files remain; lower MAX_PATTERN_MODEL_FILES to match so \
         the budget cannot grow back: {pattern_models:?}",
        pattern_models.len()
    );
}

#[test]
fn the_classifiers_tell_real_code_from_a_pattern_model() {
    let real = "//! header\n\n#[cfg(feature = \"loom\")]\nmod t {\n    use tickvault_core::x;\n}\n";
    let commented = "//! use tickvault_core::x;\nmod t {}\n";
    let labelled = "//! A model.\n//! PATTERN MODEL (why): loom cannot run tokio.\n\nmod t {}\n";
    let marker_in_body = "//! A model.\n\nmod t {\n    // PATTERN MODEL\n}\n";

    assert!(drives_real_code(real));
    assert!(!drives_real_code(commented));
    assert!(declares_pattern_model(labelled));
    assert!(
        !declares_pattern_model(marker_in_body),
        "the marker counts only in the file header"
    );
}
