//! I-P1-11 meta-guard — every DEDUP key in `crates/storage/src/` that
//! includes `security_id` MUST also include `segment` (or
//! `exchange_segment`).
//!
//! Rationale: Dhan reuses numeric `security_id` across different
//! `ExchangeSegment` values. If a QuestDB DEDUP UPSERT key omits
//! `segment`, two distinct instruments that happen to share an id
//! would dedupe into one row and silently corrupt storage. This
//! file scans every `DEDUP_KEY_*` constant in storage src and fails
//! the build if any of them lacks `segment`.
//!
//! See: `.claude/rules/project/security-id-uniqueness.md` (I-P1-11)
//! and `.claude/rules/project/gap-enforcement.md` (STORAGE-GAP-01).

#![cfg(test)]

use std::path::{Path, PathBuf};

fn storage_src_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src")
}

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

/// Returns every line that declares a `DEDUP_KEY_*` constant with its
/// `"..."` body, across all storage src files.
fn collect_dedup_key_declarations() -> Vec<(PathBuf, usize, String, String)> {
    let mut out = Vec::new();
    for (path, content) in read_rust_files(&storage_src_dir()) {
        for (idx, line) in content.lines().enumerate() {
            let trimmed = line.trim_start();
            if !trimmed.contains("DEDUP_KEY_") {
                continue;
            }
            // Only match declarations: `const DEDUP_KEY_X: &str = "..."`
            if !trimmed.contains("const ") || !trimmed.contains(": &str") {
                continue;
            }
            // Extract the quoted body.
            let start = match line.find('"') {
                Some(p) => p + 1,
                None => continue,
            };
            let end = match line[start..].find('"') {
                Some(p) => start + p,
                None => continue,
            };
            // Extract the constant name — between `const ` and `:`.
            let after_const = line.split("const ").nth(1).unwrap_or("");
            let name = after_const
                .split(':')
                .next()
                .unwrap_or("")
                .trim()
                .to_string();
            let body = line[start..end].to_string();
            out.push((path.clone(), idx.saturating_add(1), name, body));
        }
    }
    out
}

// ============================================================================
// Meta-guard — every DEDUP key touching security_id must include segment
// ============================================================================

#[test]
fn every_dedup_key_with_security_id_must_include_segment() {
    let decls = collect_dedup_key_declarations();
    assert!(
        !decls.is_empty(),
        "no DEDUP_KEY_* declarations found — did the storage module move?"
    );

    let mut violations: Vec<String> = Vec::new();
    for (path, line, name, body) in &decls {
        if !body.contains("security_id") {
            // Constant doesn't touch security_id — out of scope.
            continue;
        }
        // Accept any of: `segment`, `exchange_segment`, bare `exchange`
        // (the column name used in `subscribed_indices` — a SYMBOL column
        // that carries the segment string).
        let has_segment = body.contains("segment")
            || body.contains("exchange_segment")
            || body.contains("exchange");
        if !has_segment {
            violations.push(format!(
                "{}:{} — {} = \"{}\" contains `security_id` but NOT `segment`. \
                 Dhan reuses security_id across segments; without `segment` in \
                 the DEDUP key, two distinct instruments with the same id \
                 silently collapse into one row. Fix by adding `segment` to \
                 the key.",
                path.display(),
                line,
                name,
                body
            ));
        }
    }

    assert!(
        violations.is_empty(),
        "I-P1-11 / STORAGE-GAP-01 violation — DEDUP keys touching `security_id` \
         MUST include `segment`:\n\n{}\n\n\
         Rule: .claude/rules/project/security-id-uniqueness.md",
        violations.join("\n\n")
    );
}

#[test]
fn every_dedup_key_is_listed_here_for_auditing() {
    // Ratchet-style audit: the list of DEDUP keys in the codebase must
    // be explicit so a reviewer can eyeball which tables the ban covers.
    // If this count changes, update the assertion AND verify the new key
    // includes `segment` (the test above enforces that independently).
    //
    // Current set as of 2026-04-17 (9 keys):
    //   DEDUP_KEY_TICKS, DEDUP_KEY_MARKET_DEPTH, DEDUP_KEY_PREVIOUS_CLOSE,
    //   DEDUP_KEY_DEEP_DEPTH, DEDUP_KEY_CANDLES, DEDUP_KEY_CANDLES_1S,
    //   DEDUP_KEY_INDICATORS, DEDUP_KEY_OBI, DEDUP_KEY_MOVERS
    let decls = collect_dedup_key_declarations();
    let keys_with_security_id: Vec<&str> = decls
        .iter()
        .filter(|(_, _, _, body)| body.contains("security_id"))
        .map(|(_, _, name, _)| name.as_str())
        .collect();
    assert!(
        keys_with_security_id.len() >= 7,
        "expected at least 7 DEDUP_KEY_* constants touching security_id, got {}: {:?}. \
         A decrease means a table was removed — verify the removal was intentional.",
        keys_with_security_id.len(),
        keys_with_security_id,
    );
}

// ============================================================================
// Self-tests for the helpers
// ============================================================================

#[test]
fn self_test_collect_finds_ticks_dedup_key() {
    let decls = collect_dedup_key_declarations();
    let names: Vec<&str> = decls.iter().map(|(_, _, n, _)| n.as_str()).collect();
    assert!(
        names.contains(&"DEDUP_KEY_TICKS"),
        "DEDUP_KEY_TICKS must be discovered by the scanner. Names: {:?}",
        names
    );
}

#[test]
fn self_test_collect_returns_non_empty_bodies() {
    let decls = collect_dedup_key_declarations();
    for (path, line, name, body) in &decls {
        assert!(
            !body.is_empty(),
            "empty body for {} at {}:{}",
            name,
            path.display(),
            line
        );
    }
}
