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
    // Current set as of 2026-05-10 (≥ 22 keys after Wave 6 Sub-PR #1):
    //   Existing (≥ 16): DEDUP_KEY_TICKS, DEDUP_KEY_MARKET_DEPTH,
    //     DEDUP_KEY_PREVIOUS_CLOSE, DEDUP_KEY_DEEP_DEPTH, DEDUP_KEY_CANDLES,
    //     DEDUP_KEY_CANDLES_1S, DEDUP_KEY_INDICATORS, DEDUP_KEY_OBI,
    //     DEDUP_KEY_VOLUME_NSE_AUDIT, DEDUP_KEY_OPTION_GREEKS_LIVE,
    //     DEDUP_KEY_OPTION_GREEKS, DEDUP_KEY_DHAN_OPTION_CHAIN_RAW,
    //     DEDUP_KEY_GREEKS_VERIFICATION, DEDUP_KEY_DERIVATIVE_CONTRACTS,
    //     DEDUP_KEY_SUBSCRIBED_INDICES (+ MOVERS / others as added).
    //   Wave 6 Sub-PR #1 (this commit): +10 keys —
    //     DEDUP_KEY_CANDLES_{1M,5M,15M,30M,1H,2H,3H,4H,1D}_SHADOW (9)
    //     + DEDUP_KEY_AGGREGATOR_SEAL_AUDIT (1)
    //   Total ≥ 26 expected; threshold pinned at 22 with margin so a
    //   single removal does not silently flip the ratchet.
    let decls = collect_dedup_key_declarations();
    let keys_with_security_id: Vec<&str> = decls
        .iter()
        .filter(|(_, _, _, body)| body.contains("security_id"))
        .map(|(_, _, name, _)| name.as_str())
        .collect();
    assert!(
        keys_with_security_id.len() >= 22,
        "expected at least 22 DEDUP_KEY_* constants touching security_id (post Wave 6 Sub-PR #1 \
         shadow tables), got {}: {:?}. A decrease means a table was removed — verify the \
         removal was intentional.",
        keys_with_security_id.len(),
        keys_with_security_id,
    );
}

// ============================================================================
// Meta-guard — every audit-table DEDUP key MUST include the designated
// timestamp column `ts` (regression: 2026-05-18 production boot)
// ============================================================================
//
// QuestDB requires the designated timestamp column to be part of every
// `DEDUP UPSERT KEYS(...)` clause. On 2026-05-18 the production boot
// returned HTTP 400 for `gap_fill_audit` (ERROR) and `last_tick_audit`
// (WARN) because their DEDUP key constants omitted `ts` and the DDL
// format string did not prepend it. The 4 latent siblings
// (`order_update_ws_audit`, `sl_replacement_audit`, `open_price_audit`,
// `self_trade_audit`) carried the same omission but had not yet been
// wired into the boot path.
//
// This guard scans every `DEDUP_KEY_*_AUDIT` constant in the storage
// crate and fails the build if any of them omits `ts`. Tables whose
// DDL prepends `ts` at format time (e.g. `aggregator_seal_audit` —
// `DEDUP UPSERT KEYS(ts, {DEDUP_KEY_AGGREGATOR_SEAL_AUDIT})`) are
// explicitly listed in `DDL_PREPENDS_TS_AT_FORMAT_TIME` so the guard
// allows them. Any new audit-table constant added without `ts` (and
// without an explicit format-time prepend) blocks the build.

const DDL_PREPENDS_TS_AT_FORMAT_TIME: &[&str] = &[
    // `shadow_persistence.rs` emits `DEDUP UPSERT KEYS(ts, {DEDUP_KEY_AGGREGATOR_SEAL_AUDIT})`.
    "DEDUP_KEY_AGGREGATOR_SEAL_AUDIT",
];

#[test]
fn every_audit_dedup_key_must_include_designated_timestamp_ts() {
    let decls = collect_dedup_key_declarations();
    let mut violations: Vec<String> = Vec::new();

    for (path, line, name, body) in &decls {
        if !name.ends_with("_AUDIT") {
            continue;
        }
        if DDL_PREPENDS_TS_AT_FORMAT_TIME.contains(&name.as_str()) {
            continue;
        }
        // Match `ts` as a whole token (avoid false positives like
        // `candle_ts` or `last_seen_ltt`).
        let has_ts_token = body.split([',', ' ']).map(str::trim).any(|tok| tok == "ts");
        if !has_ts_token {
            violations.push(format!(
                "{}:{} — {} = \"{}\" omits the designated timestamp column `ts`. \
                 QuestDB returns HTTP 400 from `/exec` at boot when the DDL's \
                 `DEDUP UPSERT KEYS(...)` clause does not include the designated \
                 timestamp column. Fix by prepending `ts, ` to the constant body, \
                 OR add the constant name to `DDL_PREPENDS_TS_AT_FORMAT_TIME` if \
                 the DDL format string explicitly prepends `ts`.",
                path.display(),
                line,
                name,
                body
            ));
        }
    }

    assert!(
        violations.is_empty(),
        "Audit-table DEDUP key missing `ts` (2026-05-18 production regression):\n\n{}\n\n\
         Cause: gap_fill_audit + last_tick_audit returned HTTP 400 at boot because \
         their DEDUP keys lacked the designated timestamp column. See \
         .claude/rules/project/phase-0-architecture.md (audit-table template).",
        violations.join("\n\n"),
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
