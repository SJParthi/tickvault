//! Meta-guard — every `error!` site that mentions a tracked error code in
//! its message MUST also carry a `code = ...` structured field.
//!
//! Phase 1 of `.claude/plans/active-plan.md`. This lets Loki alert rules
//! and Claude Code triage hooks group events by the stable `code` field
//! instead of regexing the free-text message. The message wording can
//! change over time; the `code` field MUST not.
//!
//! Mechanics:
//! - Scan every `.rs` file under `crates/*/src/` (excluding tests).
//! - For each `error!(...)` / `tracing::error!(...)` block, look at the
//!   string literal (the message). If it contains a tracked code prefix
//!   (like `I-P1-11:`, `OMS-GAP-02:`, etc.), the call MUST also contain
//!   `code =` within the same macro invocation.
//! - Violations are listed with file:line.
//!
//! Escape hatch: preceding `// APPROVED:` comment skips the check.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use tickvault_common::error_code::ErrorCode;

#[test]
fn every_error_macro_tagged_with_a_known_code_carries_code_field() {
    let root = workspace_root();
    let mut violations: Vec<String> = Vec::new();
    let tagged_prefixes = compute_tagged_prefixes();

    // 2026-08-10 anti-vacuity: count what was actually read. Before this, an
    // unreadable or moved directory made scan_dir return silently, so the guard
    // passed having inspected ZERO files while reporting green.
    let mut scanned_files: usize = 0;
    for crate_name in ["common", "storage", "core", "trading", "api", "app"] {
        let src_dir = root.join("crates").join(crate_name).join("src");
        assert!(
            src_dir.is_dir(),
            "error-code tag guard is BLIND: {src_dir:?} is not a directory. A \
             missing crate root used to be skipped silently. Repoint the crate \
             list or fix the workspace root."
        );
        scan_dir(
            &src_dir,
            &tagged_prefixes,
            &mut violations,
            &mut scanned_files,
        );
    }

    // The real corpus is ~291 .rs files across the six crates; 100 leaves 3x
    // headroom while still failing loudly if the walk collapses.
    assert!(
        scanned_files > 100,
        "error-code tag guard scanned only {scanned_files} files — it is \
         effectively enforcing NOTHING. Expected >100. This assert exists \
         because the guard previously passed while scanning zero."
    );

    assert!(
        violations.is_empty(),
        "error!/tracing::error! sites mention a tracked error code in the \
         message but do not carry a `code = ErrorCode::X.code_str()` field:\n{}\n\n\
         Add the `code` field at the top of the macro call, or prefix the \
         preceding line with `// APPROVED: <reason>` to skip this guard.",
        violations.join("\n")
    );
}

fn compute_tagged_prefixes() -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for code in ErrorCode::all() {
        out.insert(format!("{}:", code.code_str()));
        out.insert(format!("{} ", code.code_str()));
    }
    out
}

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        // 2026-08-10: was `.unwrap_or_else(|| PathBuf::from("."))` — a silent CWD
        // fallback. If the layout ever changed, every crate path missed, the walk
        // found nothing, and the guard passed. Fail loudly instead.
        .expect("workspace root must exist above crates/common")
}

fn scan_dir(
    dir: &Path,
    tagged: &BTreeSet<String>,
    violations: &mut Vec<String>,
    scanned: &mut usize,
) {
    // 2026-08-10: both of the swallows below used to convert a corpus-read
    // failure into "nothing to check, pass". A guard cannot report on files it
    // never managed to open, so read failures now panic with the path.
    let entries = fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("error-code tag guard corpus unreadable {dir:?}: {e}"));
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            scan_dir(&path, tagged, violations, scanned);
            continue;
        }
        if path.extension().and_then(|s| s.to_str()) != Some("rs") {
            continue;
        }
        let contents = fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("error-code tag guard cannot read {path:?}: {e}"));
        *scanned = scanned.saturating_add(1);
        scan_file(&path, &contents, tagged, violations);
    }
}

fn scan_file(path: &Path, contents: &str, tagged: &BTreeSet<String>, violations: &mut Vec<String>) {
    let lines: Vec<&str> = contents.lines().collect();
    let mut idx = 0;
    while idx < lines.len() {
        let line = lines[idx];
        let is_error_macro = line.contains("error!(") || line.contains("tracing::error!(");
        if !is_error_macro {
            idx += 1;
            continue;
        }
        // Collect the macro body (possibly multi-line) by counting parens.
        let (body, end_idx) = collect_macro_body(&lines, idx);
        let mentions_tagged_code = tagged.iter().any(|p| body.contains(p.as_str()));
        if !mentions_tagged_code {
            idx = end_idx + 1;
            continue;
        }
        let carries_code_field = body.contains("code =") || body.contains("code=");
        if carries_code_field {
            idx = end_idx + 1;
            continue;
        }
        // APPROVED-comment escape hatch on the line immediately preceding
        // the macro start.
        let prev = idx
            .checked_sub(1)
            .and_then(|i| lines.get(i))
            .copied()
            .unwrap_or("");
        if prev.trim_start().starts_with("// APPROVED:") {
            idx = end_idx + 1;
            continue;
        }
        violations.push(format!(
            "  {}:{} — error! contains a tracked code prefix but no `code =` field",
            path.display(),
            idx + 1
        ));
        idx = end_idx + 1;
    }
}

/// Returns the full macro body (concatenated) and the 0-based index of the
/// line that closes the macro.
fn collect_macro_body(lines: &[&str], start: usize) -> (String, usize) {
    let mut body = String::new();
    let mut depth: i32 = 0;
    let mut seen_open = false;
    for (offset, raw) in lines.iter().enumerate().skip(start) {
        for ch in raw.chars() {
            match ch {
                '(' => {
                    depth += 1;
                    seen_open = true;
                }
                ')' => depth -= 1,
                _ => {}
            }
        }
        body.push_str(raw);
        body.push('\n');
        if seen_open && depth == 0 {
            return (body, offset);
        }
    }
    (body, lines.len().saturating_sub(1))
}

#[test]
fn tagged_prefix_set_is_non_empty() {
    let tagged = compute_tagged_prefixes();
    assert!(!tagged.is_empty());
    // Every ErrorCode should contribute exactly 2 entries (": " and " ").
    assert_eq!(tagged.len(), ErrorCode::all().len() * 2);
}

/// 2026-08-10. `tagged_prefix_set_is_non_empty` above is a genuine check of the
/// PREFIX SET, but it is NOT a non-vacuity check of this guard: it asserts on the
/// `ErrorCode` enum, which the test itself derives, and would keep passing even
/// if the file walk read nothing at all. That distinction is exactly how a
/// sibling guard in this repo stayed green for an unknown period while scanning
/// zero files.
///
/// This test asserts the thing that actually matters — that the corpus the guard
/// walks EXISTS and is substantial.
#[test]
fn scan_corpus_exists_and_is_substantial() {
    let root = workspace_root();
    let mut total = 0usize;
    for crate_name in ["common", "storage", "core", "trading", "api", "app"] {
        let src_dir = root.join("crates").join(crate_name).join("src");
        assert!(src_dir.is_dir(), "scan corpus missing: {src_dir:?}");
        let mut violations: Vec<String> = Vec::new();
        let tagged = BTreeSet::new();
        scan_dir(&src_dir, &tagged, &mut violations, &mut total);
    }
    assert!(
        total > 100,
        "scan corpus collapsed to {total} files — the guard would enforce nothing"
    );
}
