//! Ratchet: every f32→f64 conversion on a `ParsedTick` price field in
//! prod code MUST route through `tickvault_common::price_precision::
//! f32_to_f64_clean`, NOT bare `f64::from(...)`.
//!
//! Operator-spotted bug 2026-05-25: candles_1m rows showed
//! `23937.30078125`, `23924.400390625`, `23925.650390625` —
//! IEEE-754 widening artifacts from `f64::from(tick.last_traded_price)`
//! in `crates/trading/src/candles/aggregator_cell.rs` AND
//! `crates/trading/src/indicator/engine.rs`. Both files live outside
//! the original banned-pattern hook scope (`crates/storage/` only),
//! so the corruption silently flowed into every sealed candle AND
//! every SMA/EMA/RSI/MACD/BB indicator value.
//!
//! This ratchet ensures the bug cannot reappear in any future PR.

use std::fs;
use std::path::{Path, PathBuf};

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .map(PathBuf::from)
        .expect("workspace root must exist above crates/common") // APPROVED: test
}

fn collect_rs_sources(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            // Skip target/ + tests/ — tests legitimately use f64::from
            // for synthetic ticks built in unit tests.
            if matches!(name, "target" | "tests") {
                continue;
            }
            collect_rs_sources(&path, out);
            continue;
        }
        if path.extension().and_then(|s| s.to_str()) == Some("rs") {
            out.push(path);
        }
    }
}

/// True iff the file contains the substring outside any `#[cfg(test)]`
/// module or `#[test]` function. We do a coarse heuristic: any line
/// after the FIRST `#[cfg(test)]` marker in the file is considered
/// test code. This matches the canonical Rust convention of putting
/// tests at the bottom of a module in a single `#[cfg(test)] mod tests`
/// block. False positives are acceptable (a CHANGE that needs to use
/// the banned pattern in prod code outside a test block must use the
/// canonical `// APPROVED:` comment escape).
fn prod_lines_containing(body: &str, needle: &str) -> Vec<(usize, String)> {
    let mut hits = Vec::new();
    let mut in_test_block = false;
    for (line_no, line) in body.lines().enumerate() {
        if line.contains("#[cfg(test)]") {
            in_test_block = true;
        }
        if in_test_block {
            continue;
        }
        if line.contains(needle) {
            hits.push((line_no + 1, line.trim().to_string()));
        }
    }
    hits
}

#[test]
fn test_no_prod_code_uses_f64_from_on_tick_price_fields() {
    let banned_substrings = [
        "f64::from(tick.last_traded_price)",
        "f64::from(tick.day_open)",
        "f64::from(tick.day_high)",
        "f64::from(tick.day_low)",
        "f64::from(tick.day_close)",
        "f64::from(tick.average_traded_price)",
    ];

    let mut sources = Vec::new();
    for crate_name in &["storage", "trading", "core"] {
        let crate_src = workspace_root().join("crates").join(crate_name).join("src");
        collect_rs_sources(&crate_src, &mut sources);
    }
    assert!(
        !sources.is_empty(),
        "self-check: collected 0 source files — walker broken"
    );

    let mut violations: Vec<String> = Vec::new();
    for path in sources {
        let Ok(body) = fs::read_to_string(&path) else {
            continue;
        };
        for needle in &banned_substrings {
            for (line_no, line) in prod_lines_containing(&body, needle) {
                let rel = path
                    .strip_prefix(workspace_root())
                    .unwrap_or(&path)
                    .display();
                violations.push(format!("{rel}:{line_no}: {line}"));
            }
        }
    }

    assert!(
        violations.is_empty(),
        "data-integrity.md \"Price Precision Preservation\" ratchet: \
         f64::from(f32) on a ParsedTick price field widens IEEE-754 \
         (e.g. 23925.65_f32 → 23925.650390625_f64) and corrupts every \
         downstream candle/indicator/strategy value. Replace with \
         tickvault_common::price_precision::f32_to_f64_clean(...).\n\n\
         Violations:\n  {}",
        violations.join("\n  "),
    );
}

#[test]
fn test_common_price_precision_module_exists() {
    let module_path = workspace_root().join("crates/common/src/price_precision.rs");
    assert!(
        module_path.exists(),
        "Z+ data-integrity ratchet: crates/common/src/price_precision.rs is \
         missing. Without it, the aggregator + indicator + storage cannot \
         share a single clean f32→f64 conversion impl."
    );
    let body = fs::read_to_string(&module_path).unwrap_or_default(); // APPROVED: test
    assert!(
        body.contains("pub fn f32_to_f64_clean"),
        "price_precision module must export f32_to_f64_clean as pub fn"
    );
}

// RETIRED (stage-2 dead-WS sweep, 2026-07-17):
// `test_storage_tick_persistence_delegates_to_common` pinned the
// storage-local `f32_to_f64_clean` thin wrapper inside
// `crates/storage/src/tick_persistence.rs` — that file was DELETED with
// the dead Dhan tick chain, so there is no storage-local wrapper left to
// drift. The single source of truth is unchanged:
// `tickvault_common::price_precision::f32_to_f64_clean` (pinned above by
// `test_common_price_precision_module_exists`), and every surviving
// consumer imports it from common directly (the cross-crate `f64::from`
// ban above still ratchets the whole workspace).

#[test]
fn test_banned_pattern_hook_covers_trading_and_core_paths() {
    // The pre-commit hook must scan trading + core, not just storage.
    // Before 2026-05-25 it only scanned storage, which is how the
    // candle precision bug slipped past 1000s of tests.
    let body = fs::read_to_string(workspace_root().join(".claude/hooks/banned-pattern-scanner.sh"))
        .unwrap_or_default(); // APPROVED: test
    assert!(
        body.contains("scan_tick_price_precision"),
        "Z+ ratchet: banned-pattern-scanner.sh must define \
         scan_tick_price_precision (cross-crate price-field scanner)."
    );
    assert!(
        body.contains("crates/(storage|trading|core)"),
        "Z+ ratchet: the cross-crate scanner regex must cover storage, \
         trading, and core — the 3 crates that touch ParsedTick prices."
    );
    for field in [
        "f64::from(tick\\.last_traded_price)",
        "f64::from(tick\\.day_open)",
        "f64::from(tick\\.day_high)",
        "f64::from(tick\\.day_low)",
        "f64::from(tick\\.day_close)",
    ] {
        assert!(
            body.contains(field),
            "Z+ ratchet: banned-pattern-scanner.sh must reject the \
             specific pattern `{field}` in production code."
        );
    }
}

// 2026-06-28: test_option_chain_minute_snapshot_disabled_pending_data_api_entitlement
// REMOVED — the entire option_chain REST subsystem (and its
// [option_chain_minute_snapshot] config block) was deleted per operator
// directive 2026-06-28. There is no longer an enabled flag to pin off.
