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
/// Returns every `DEDUP_KEY_*` constant with its `"..."` body, across all
/// storage src files. Robust to rustfmt wrapping the declaration across lines
/// (`const X: &str =\n    "...";`): when the declaration line has no quote, the
/// body is found on a following continuation line.
fn collect_dedup_key_declarations() -> Vec<(PathBuf, usize, String, String)> {
    let mut out = Vec::new();
    for (path, content) in read_rust_files(&storage_src_dir()) {
        let lines: Vec<&str> = content.lines().collect();
        for (idx, line) in lines.iter().enumerate() {
            let trimmed = line.trim_start();
            if !trimmed.contains("DEDUP_KEY_") {
                continue;
            }
            // Only match declarations: `const DEDUP_KEY_X: &str = "..."`.
            if !trimmed.contains("const ") || !trimmed.contains(": &str") {
                continue;
            }
            // Extract the constant name — between `const ` and `:`.
            let after_const = line.split("const ").nth(1).unwrap_or("");
            let name = after_const
                .split(':')
                .next()
                .unwrap_or("")
                .trim()
                .to_string();
            // The quoted body may be on this line OR a continuation line. Scan
            // forward up to a few lines until the first `"..."` literal appears.
            let mut body: Option<String> = None;
            for probe in lines.iter().skip(idx).take(4) {
                if let Some(s) = probe.find('"') {
                    let start = s + 1;
                    if let Some(e) = probe[start..].find('"') {
                        body = Some(probe[start..start + e].to_string());
                        break;
                    }
                }
            }
            if let Some(body) = body {
                out.push((path.clone(), idx.saturating_add(1), name, body));
            }
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
    // #T1a (2026-05-20): the 9 Wave-6 `DEDUP_KEY_CANDLES_*_SHADOW`
    // constants collapsed into a single shared `DEDUP_KEY_CANDLES`.
    // #T2a (2026-05-20): the QuestDB audit-table cleanup deleted 10
    // `*_audit_persistence.rs` modules, removing their `DEDUP_KEY_*`
    // constants (aggregator_seal, bar_correction, decision, open_price,
    // order_update_ws, pnl, self_trade, signal, sl_replacement,
    // volume_nse).
    // #T3 (2026-05-20): instrument_persistence.rs deleted — removes
    // DEDUP_KEY_DERIVATIVE_CONTRACTS + DEDUP_KEY_SUBSCRIBED_INDICES.
    // #T4 (2026-05-20): market_depth / previous_close / indicator_snapshots
    // / obi_snapshots tables dropped — removes DEDUP_KEY_MARKET_DEPTH,
    // DEDUP_KEY_PREVIOUS_CLOSE, DEDUP_KEY_INDICATORS, DEDUP_KEY_OBI.
    // #T5 (2026-05-26): PR-E removed candle_persistence.rs alongside
    // the historical_candles table, eliminating its DEDUP_KEY_CANDLES
    // declaration. Set drops to 2; threshold lowered accordingly.
    //
    // Current set touching `security_id` (2 declaration sites):
    //   DEDUP_KEY_TICKS (tick_persistence.rs),
    //   DEDUP_KEY_CANDLES (shadow_persistence.rs — shared by all 21
    //   plain candle tables).
    //
    // The 2026-05-27 daily-universe lifecycle contracts added
    // `DEDUP_KEY_INSTRUMENT_LIFECYCLE` + `DEDUP_KEY_INSTRUMENT_LIFECYCLE_AUDIT`
    // (both segment-paired per I-P1-11), so the threshold rose from 2 to 4
    // as anticipated. Current set touching `security_id`:
    //   DEDUP_KEY_TICKS (tick_persistence.rs),
    //   DEDUP_KEY_CANDLES (shadow_persistence.rs),
    //   DEDUP_KEY_INSTRUMENT_LIFECYCLE (instrument_lifecycle_persistence.rs),
    //   DEDUP_KEY_INSTRUMENT_LIFECYCLE_AUDIT (instrument_lifecycle_persistence.rs).
    let decls = collect_dedup_key_declarations();
    let keys_with_security_id: Vec<&str> = decls
        .iter()
        .filter(|(_, _, _, body)| body.contains("security_id"))
        .map(|(_, _, name, _)| name.as_str())
        .collect();
    assert!(
        keys_with_security_id.len() >= 4,
        "expected at least 4 DEDUP_KEY_* constants touching security_id \
         (DEDUP_KEY_TICKS, DEDUP_KEY_CANDLES, DEDUP_KEY_INSTRUMENT_LIFECYCLE, \
         DEDUP_KEY_INSTRUMENT_LIFECYCLE_AUDIT), got {}: {:?}. A decrease \
         means a table was removed — verify the removal was intentional.",
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
// returned HTTP 400 for audit tables (e.g. `last_tick_audit` WARN)
// because their DEDUP key constants omitted `ts` and the DDL
// format string did not prepend it. The 4 latent siblings
// (`order_update_ws_audit`, `sl_replacement_audit`, `open_price_audit`,
// `self_trade_audit`) carried the same omission but had not yet been
// wired into the boot path.
//
// This guard scans every `DEDUP_KEY_*_AUDIT` constant in the storage
// crate and fails the build if any of them omits `ts`. Tables whose
// DDL prepends `ts` at format time may be listed in
// `DDL_PREPENDS_TS_AT_FORMAT_TIME` so the guard allows them. Any new
// audit-table constant added without `ts` (and without an explicit
// format-time prepend) blocks the build.

// #T2a (2026-05-20): the sole entry `DEDUP_KEY_AGGREGATOR_SEAL_AUDIT`
// was removed when the `aggregator_seal_audit` table was dropped by the
// QuestDB table cleanup. The allowlist is now empty.
const DDL_PREPENDS_TS_AT_FORMAT_TIME: &[&str] = &[];

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
         Cause: last_tick_audit returned HTTP 400 at boot because \
         their DEDUP keys lacked the designated timestamp column. See \
         .claude/rules/project/phase-0-architecture.md (audit-table template).",
        violations.join("\n\n"),
    );
}

// ============================================================================
// Meta-guard — per-feed MARKET-DATA DEDUP keys MUST include `feed`
// (per-feed identity, operator 2026-06-23)
// ============================================================================
//
// A row that is genuinely ONE feed's market data (a Dhan tick vs a Groww tick,
// a Dhan 1m candle vs a Groww 1m candle, a Dhan prev-day OHLC vs a Groww one,
// a Dhan WS lifecycle event vs a Groww one) MUST carry `feed` in its DEDUP key
// so the two feeds never collapse into one row. This ratchets that contract:
// removing `feed` from any of these keys fails the build.
//
// NOT listed here (intentionally — per-feed value is meaningless/absent):
//   - cross_verify_1m_audit / groww_cross_verify_1m_audit (Dhan-only / Groww-only
//     by table; feed is a label, not a key)
//   - tick_conservation_audit (single combined cross-feed reconciliation)
//   - the instrument-master / universe tables (one universe shared by both feeds)
const FEED_KEYED_MARKET_DATA_KEYS: &[&str] = &[
    "DEDUP_KEY_TICKS",
    "DEDUP_KEY_CANDLES",
    "DEDUP_KEY_PREV_DAY_OHLCV",
    "DEDUP_KEY_WS_EVENT_AUDIT",
];

#[test]
fn per_feed_market_data_dedup_keys_must_include_feed() {
    let decls = collect_dedup_key_declarations();
    for expected in FEED_KEYED_MARKET_DATA_KEYS {
        let found = decls.iter().find(|(_, _, name, _)| name == expected);
        let (path, line, _name, body) = found.unwrap_or_else(|| {
            panic!(
                "per-feed DEDUP key `{expected}` not found in storage src — was it \
                 renamed/removed? It MUST exist and include `feed` (operator \
                 2026-06-23 per-feed identity)."
            )
        });
        assert!(
            body.contains("feed"),
            "{}:{} — {} = \"{}\" MUST include `feed` in its DEDUP key so a Dhan \
             and a Groww row for the same instrument/minute stay DISTINCT rows \
             (per-feed identity, operator 2026-06-23). Removing `feed` would \
             collapse the two feeds into one row.",
            path.display(),
            line,
            expected,
            body
        );
    }
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
