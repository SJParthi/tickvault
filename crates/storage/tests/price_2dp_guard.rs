//! Operator 2-decimal contract guard (2026-05-29) — every persisted
//! PRICE column must be rounded through `round_to_2dp` at its write site.
//!
//! Background: the operator requires every persisted price/percentage
//! DOUBLE column to carry at most two digits after the decimal point.
//! Prices arrive from Dhan as 2-dp f32 and `f32_to_f64_clean` preserves
//! that, so the `ticks` price columns were usually clean — but
//! exchange-computed values like `avg_price` (VWAP) can carry >2 digits,
//! and a future code change could introduce a raw `column_f64` price
//! write that skips rounding. This source-scan guard fails the build if
//! any known price column is written without `round_to_2dp` wrapping.
//!
//! Scope: PRICE columns only. Greeks (delta/theta/gamma/vega) and IV are
//! DELIBERATELY EXCLUDED (operator decision 2026-05-29 — delta is 0-1,
//! IV is a fraction; 2dp would destroy them). The `*_pct_from_prev_day`
//! columns are rounded upstream at compute time
//! (`pct_stamping::compute_*`), so they need no write-site wrap.
//!
//! See: `.claude/rules/project/data-integrity.md` (Price Precision),
//! `crates/common/src/price_precision.rs::round_to_2dp`.

#![cfg(test)]

use std::path::{Path, PathBuf};

fn storage_src(file: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join(file);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("price-2dp guard: cannot read {} ({e})", path.display()))
}

/// Strip line/doc comments so the guard pins live code, not comments that
/// merely mention a column name.
fn code_only(content: &str) -> String {
    content
        .lines()
        .filter(|l| {
            let t = l.trim_start();
            !t.starts_with("//")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The live tick-persist price columns. Each must have its VALUE constructed
/// through `round_to_2dp(...)` at the `RawTickFields` build site in
/// `tick_persistence.rs::build_tick_row_seq` (C1 convergence moved the literal
/// `column_f64(...)` writes into the shared `tick_row_builder`, but the operator
/// 2-decimal contract is applied here, at value-construction time, BEFORE the
/// value enters the feed-agnostic `RawTickFields`).
const TICK_PRICE_COLUMNS: &[&str] = &["ltp", "open", "high", "low", "close", "avg_price"];

#[test]
fn tick_persistence_price_columns_are_rounded_to_2dp() {
    let code = code_only(&storage_src("tick_persistence.rs"));
    // Post-C1 the Dhan `build_tick_row_seq` constructs a `RawTickFields` whose
    // price values are pre-rounded, then delegates to the shared builder. The
    // 2-decimal contract lives at this value-construction site. Accepted shapes
    // (whitespace normalised) for each price column `<col>`:
    //   <col>: round_to_2dp(                  — required-for-both field (ltp)
    //   <col>: Some(round_to_2dp(             — per-feed Option field (open/…/avg_price)
    let normalised: String = code.split_whitespace().collect::<Vec<_>>().join(" ");
    for col in TICK_PRICE_COLUMNS {
        let required_field = format!("{col}: round_to_2dp(");
        let optional_field = format!("{col}: Some(round_to_2dp(");
        assert!(
            normalised.contains(&required_field) || normalised.contains(&optional_field),
            "tick_persistence.rs::build_tick_row_seq must construct the price field \
             `{col}` through `round_to_2dp(...)` (operator 2-decimal contract \
             2026-05-29) before it enters RawTickFields. Found a `{col}:` field set \
             without rounding. Wrap it: round_to_2dp(f32_to_f64_clean(...)). Greeks/IV \
             are exempt; this guard covers PRICE columns only."
        );
    }
}

#[test]
fn candle_seal_row_rounds_ohlc_to_2dp() {
    // The candle seal row (open/high/low/close) is the OTHER price write
    // path (→ candles_<tf> tables). It must round through round_to_2dp in
    // `ShadowSealRow::from_buffered_seal`.
    let code = code_only(&storage_src("shadow_seal_columns.rs"));
    let normalised: String = code.split_whitespace().collect::<Vec<_>>().join(" ");
    for field in ["open", "high", "low", "close"] {
        let needle = format!("{field}: round_to_2dp(");
        assert!(
            normalised.contains(&needle),
            "shadow_seal_columns.rs::from_buffered_seal must round the candle \
             `{field}` through `round_to_2dp(...)` (operator 2-decimal \
             contract 2026-05-29). Found `{field}:` set without rounding."
        );
    }
}

#[test]
fn round_to_2dp_helper_is_available_in_storage() {
    // Pin that the local re-export exists so the wraps above resolve.
    let code = storage_src("tick_persistence.rs");
    assert!(
        code.contains("pub fn round_to_2dp"),
        "tick_persistence.rs must expose a `round_to_2dp` re-export of the \
         canonical common primitive."
    );
}

// ---------------------------------------------------------------------------
// Self-test for the helper
// ---------------------------------------------------------------------------

#[test]
fn self_test_code_only_strips_comments() {
    let sample = "// column_f64(\"ltp\", raw) in a comment\nlet x = 1;";
    let stripped = code_only(sample);
    assert!(!stripped.contains("in a comment"));
    assert!(stripped.contains("let x = 1;"));
}

#[allow(dead_code)]
fn _assert_paths_exist() {
    // compile-time-ish sanity that the files are where we scan.
    let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let _ = base.join("tick_persistence.rs");
    let _ = base.join("shadow_seal_columns.rs");
}
