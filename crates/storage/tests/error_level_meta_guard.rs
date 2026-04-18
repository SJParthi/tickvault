//! Phase 0 / Rule 5 meta-guard — flush/persist/broadcast failures must be `error!`.
//!
//! Scans the workspace source tree and fails the build if any `warn!` macro
//! contains a known-bad phrase that should ALWAYS fire as `error!` (routes to
//! Telegram via the NotificationService webhook).
//!
//! This is a permanent ratchet: the phrases below were all WARN before the
//! 2026-04-18 escalation pass. If a future change regresses any site back to
//! `warn!`, this test fails and the build is blocked.
//!
//! To exempt a legitimate informational warn with one of these phrases, put an
//! `// APPROVED: <reason>` comment on the immediately-preceding line and the
//! scan will skip the match.

use std::fs;
use std::path::{Path, PathBuf};

/// Phrases that MUST appear inside `error!` (never `warn!`) because they
/// indicate explicit data-loss visibility for the operator.
///
/// Each entry is a lower-cased substring of the log message. Matching is
/// case-sensitive on the source (log messages in the codebase are lower-case
/// English). Phrases are specific enough to avoid false positives (we do NOT
/// match on the word "flush" alone, only in the context of "failed").
const REQUIRE_ERROR_LEVEL_PHRASES: &[&str] = &[
    // Tick persistence
    "flush during ring buffer drain failed",
    "ring buffer final flush failed",
    "bufwriter flush failed before drain",
    "flush during spill drain failed",
    "spill drain final flush failed",
    // Depth persistence
    "flush during depth ring buffer drain failed",
    "depth ring buffer final flush failed",
    "flush during depth spill drain failed",
    "depth spill drain final flush failed",
    // Candle persistence
    "live candle flush failed",
    "candle ring drain flush failed",
    "candle ring drain final flush failed",
    "candle bufwriter flush failed before drain",
    "candle spill drain flush failed",
    "candle spill drain final flush failed",
    // App main boot / shutdown flushes
    "failed to persist 20-level depth",
    "depth writer flush on shutdown failed",
    "obi writer flush on shutdown failed",
    "cold-path tick persistence write failed",
    "cold-path candle flush failed",
    // Historical fetcher
    "failed to flush remaining candles to questdb",
    // Movers (tick processor shutdown)
    "stock movers flush on shutdown failed",
    "option movers flush on shutdown failed",
    // Stale-spill recovery flushes (post-restart recovery path)
    "flush during stale spill drain failed",
    "stale spill drain final flush failed",
    "flush during stale depth spill drain failed",
    "stale depth spill drain final flush failed",
    "stale candle spill drain flush failed",
    "stale candle spill drain final flush failed",
];

#[test]
fn flush_persist_broadcast_failures_must_use_error_level() {
    let root = workspace_root();
    let mut violations: Vec<String> = Vec::new();

    for crate_name in ["common", "storage", "core", "trading", "api", "app"] {
        let src_dir = root.join("crates").join(crate_name).join("src");
        scan_rust_files_recursive(&src_dir, &mut violations);
    }

    assert!(
        violations.is_empty(),
        "Rule 5 violation — flush/persist/broadcast failures must be `error!`, not `warn!`:\n{}",
        violations.join("\n")
    );
}

fn workspace_root() -> PathBuf {
    // CARGO_MANIFEST_DIR points at crates/storage; workspace root is two up.
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .unwrap_or(manifest)
}

fn scan_rust_files_recursive(dir: &Path, violations: &mut Vec<String>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            scan_rust_files_recursive(&path, violations);
            continue;
        }
        if path.extension().and_then(|s| s.to_str()) != Some("rs") {
            continue;
        }
        let Ok(contents) = fs::read_to_string(&path) else {
            continue;
        };
        scan_file(&path, &contents, violations);
    }
}

fn scan_file(path: &Path, contents: &str, violations: &mut Vec<String>) {
    let lines: Vec<&str> = contents.lines().collect();
    for (idx, line) in lines.iter().enumerate() {
        let trimmed = line.trim_start();
        if !trimmed.contains("warn!(") {
            continue;
        }
        let lc = line.to_lowercase();
        for phrase in REQUIRE_ERROR_LEVEL_PHRASES {
            if !lc.contains(phrase) {
                continue;
            }
            // APPROVED-comment escape hatch on the preceding line.
            let prev = idx
                .checked_sub(1)
                .and_then(|i| lines.get(i))
                .copied()
                .unwrap_or("");
            if prev.trim_start().starts_with("// APPROVED:") {
                continue;
            }
            violations.push(format!(
                "  {}:{} — contains Rule-5 phrase in warn!: `{}`",
                path.display(),
                idx + 1,
                phrase
            ));
        }
    }
}

#[test]
fn phrases_list_is_non_empty_and_lowercase() {
    assert!(
        !REQUIRE_ERROR_LEVEL_PHRASES.is_empty(),
        "meta-guard phrase list must not be empty"
    );
    for phrase in REQUIRE_ERROR_LEVEL_PHRASES {
        assert_eq!(
            *phrase,
            phrase.to_lowercase(),
            "phrases are compared case-insensitively — store them lower-cased"
        );
    }
}
