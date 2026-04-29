//! Real-time cross-reference guard — `ErrorCode` enum MUST stay in sync with
//! `.claude/rules/**/*.md` files.
//!
//! Phase 1 of `.claude/plans/active-plan.md`. The goal is mechanical: the
//! moment anyone adds a new gap code to the rules OR a new variant to
//! [`ErrorCode`] without touching the other, the build fails.
//!
//! Two assertions per run:
//!
//! 1. **Forward check** — every `ErrorCode::code_str()` appears in at least
//!    one markdown file under `.claude/rules/`. Prevents ghost variants
//!    that have no documentation.
//!
//! 2. **Reverse check** — every code-shaped string (`I-P0-01`, `OMS-GAP-03`,
//!    `DH-904`, `DATA-807`, `GAP-NET-01`, etc.) that appears in
//!    `.claude/rules/**/*.md` has a matching [`ErrorCode`] variant. Prevents
//!    undocumented codes accumulating in rule text without typed enforcement.
//!
//! An explicit allow-list handles the few code strings the rules mention
//! for historical / pedagogical reasons that intentionally don't have an
//! enum variant yet (e.g. codes reserved for a future phase). Adding to the
//! allow-list requires a comment justifying the exemption.

use std::collections::{BTreeSet, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use tickvault_common::error_code::ErrorCode;

/// Code prefixes the reverse-check recognises as tracked by `ErrorCode`.
///
/// Codes that appear in rule text but do NOT start with one of these
/// prefixes are ignored by the reverse-check (they're not in our taxonomy).
const TRACKED_PREFIXES: &[&str] = &[
    "I-P0-",
    "I-P1-",
    "I-P2-",
    "GAP-NET-",
    "GAP-SEC-",
    "OMS-GAP-",
    "WS-GAP-",
    "RISK-GAP-",
    "AUTH-GAP-",
    "STORAGE-GAP-",
    "DH-",
    "DATA-",
];

/// Codes mentioned in rule files that intentionally don't have an enum
/// variant yet. Every entry must have a justification comment.
const REVERSE_CHECK_ALLOWLIST: &[&str] = &[
    // `DH-805`: appears in `historical-candles-cross-verify.md` as a
    //   historical typo for the Data-API 805 code (too-many-connections).
    //   The canonical code is `Data805TooManyConnections` / `DATA-805`.
    //   Keeping the typo as an allowlist entry until the doc is corrected.
    "DH-805",
    // `DH-900`: appears in `release-notes.md` as a reference to the
    //   `DH-9xx` error-code family introduced in API v2.0, not a specific
    //   code. No individual variant corresponds to it.
    "DH-900",
    // RESERVED Wave-4 stubs in `wave-4-error-codes.md`. These codes are
    // documented as planned but not yet promoted to ErrorCode variants.
    // Each will be removed from this allowlist when its sub-PR ships.
    // `AUTH-GAP-04` — TOTP secret rotated externally (Wave-4-E2).
    "AUTH-GAP-04",
    // `DH-911` — Dhan API silent black-hole (Wave-4-E2).
    "DH-911",
    // `STORAGE-GAP-05` — disk-full pre-flight failed (Wave-4-E2).
    "STORAGE-GAP-05",
];

#[test]
fn every_error_code_variant_appears_in_a_rule_file() {
    let rule_text = load_all_rule_text();
    let mut missing: Vec<&'static str> = Vec::new();

    for code in ErrorCode::all() {
        if rule_text_mentions(&rule_text, code.code_str()) {
            continue;
        }
        missing.push(code.code_str());
    }

    assert!(
        missing.is_empty(),
        "ErrorCode variants with no mention in .claude/rules/**/*.md:\n  {}\n\n\
         Either add rule documentation for these codes OR remove the variant.",
        missing.join("\n  ")
    );
}

/// True if `code_str` is documented in the combined rule text.
///
/// Handles the two naming conventions the rules use:
/// - `DH-XXX`, `I-PX-YY`, `OMS-GAP-YY`, etc. — appear verbatim.
/// - `DATA-8YY` — rules reference these as the bare backticked number
///   (e.g. `` `807` `` in `dhan/annexure-enums.md`). We detect this form
///   via a backtick-wrapped exact-digit match.
fn rule_text_mentions(rule_text: &str, code_str: &str) -> bool {
    if rule_text.contains(code_str) {
        return true;
    }
    if let Some(rest) = code_str.strip_prefix("DATA-") {
        let backticked = format!("`{rest}`");
        if rule_text.contains(&backticked) {
            return true;
        }
    }
    false
}

#[test]
fn every_rule_file_code_has_an_enum_variant() {
    let rule_text = load_all_rule_text();
    let known: HashSet<&'static str> = ErrorCode::all().iter().map(|c| c.code_str()).collect();
    let allow: HashSet<&'static str> = REVERSE_CHECK_ALLOWLIST.iter().copied().collect();
    let mut missing: BTreeSet<String> = BTreeSet::new();

    for candidate in extract_code_strings(&rule_text) {
        if known.contains(candidate.as_str()) || allow.contains(candidate.as_str()) {
            continue;
        }
        missing.insert(candidate);
    }

    assert!(
        missing.is_empty(),
        "Codes referenced in .claude/rules/**/*.md without a matching ErrorCode variant:\n  {}\n\n\
         Either add a variant to crates/common/src/error_code.rs OR add to \
         REVERSE_CHECK_ALLOWLIST with justification.",
        missing.iter().cloned().collect::<Vec<_>>().join("\n  ")
    );
}

#[test]
fn every_runbook_path_exists_on_disk() {
    let root = workspace_root();
    let mut missing: Vec<String> = Vec::new();

    for code in ErrorCode::all() {
        let path = root.join(code.runbook_path());
        if !path.exists() {
            missing.push(format!("{} -> {}", code.code_str(), code.runbook_path()));
        }
    }

    assert!(
        missing.is_empty(),
        "ErrorCode variants pointing at non-existent runbook files:\n  {}",
        missing.join("\n  ")
    );
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."))
}

fn load_all_rule_text() -> String {
    let root = workspace_root();
    let rules_dir = root.join(".claude").join("rules");
    let mut combined = String::new();
    collect_markdown_contents(&rules_dir, &mut combined);
    combined
}

fn collect_markdown_contents(dir: &Path, out: &mut String) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_markdown_contents(&path, out);
            continue;
        }
        if path.extension().and_then(|s| s.to_str()) != Some("md") {
            continue;
        }
        if let Ok(body) = fs::read_to_string(&path) {
            out.push_str(&body);
            out.push('\n');
        }
    }
}

/// Extracts every substring that looks like a tracked code
/// (one of TRACKED_PREFIXES followed by alphanumerics or hyphens).
fn extract_code_strings(text: &str) -> BTreeSet<String> {
    let mut found: BTreeSet<String> = BTreeSet::new();
    for prefix in TRACKED_PREFIXES {
        let mut cursor = 0;
        while let Some(idx) = text[cursor..].find(prefix) {
            let absolute = cursor + idx;
            // Must be at word boundary (start of text OR non-alphanumeric before).
            if absolute > 0 {
                let prev = text.as_bytes()[absolute - 1];
                if prev.is_ascii_alphanumeric() || prev == b'-' || prev == b'_' {
                    cursor = absolute + prefix.len();
                    continue;
                }
            }
            let tail = &text[absolute + prefix.len()..];
            let len: usize = tail
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '-')
                .map(|c| c.len_utf8())
                .sum();
            if len == 0 {
                cursor = absolute + prefix.len();
                continue;
            }
            let full = &text[absolute..absolute + prefix.len() + len];
            // Must end at a word boundary (next char non-alphanumeric).
            let end = absolute + prefix.len() + len;
            let ends_cleanly = end == text.len() || !text.as_bytes()[end].is_ascii_alphanumeric();
            if ends_cleanly && looks_like_a_code(full, prefix) {
                found.insert(full.to_string());
            }
            cursor = end;
        }
    }
    found
}

/// True if `candidate` is plausibly a real error code (has at least one
/// digit after the prefix, avoiding sentence fragments like "DH-" in prose).
fn looks_like_a_code(candidate: &str, prefix: &str) -> bool {
    let tail = &candidate[prefix.len()..];
    if tail.is_empty() {
        return false;
    }
    // Require tail to start with a digit (ruling out "DH-ApiCall" and similar).
    let starts_with_digit = tail.chars().next().is_some_and(|c| c.is_ascii_digit());
    if !starts_with_digit {
        return false;
    }
    // Sanity upper bound on length to skip spurious long matches.
    candidate.len() <= 32
}
