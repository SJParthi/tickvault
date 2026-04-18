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

    for crate_name in ["common", "storage", "core", "trading", "api", "app"] {
        let src_dir = root.join("crates").join(crate_name).join("src");
        scan_dir(&src_dir, &tagged_prefixes, &mut violations);
    }

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
        .unwrap_or_else(|| PathBuf::from("."))
}

fn scan_dir(dir: &Path, tagged: &BTreeSet<String>, violations: &mut Vec<String>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            scan_dir(&path, tagged, violations);
            continue;
        }
        if path.extension().and_then(|s| s.to_str()) != Some("rs") {
            continue;
        }
        let Ok(contents) = fs::read_to_string(&path) else {
            continue;
        };
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
