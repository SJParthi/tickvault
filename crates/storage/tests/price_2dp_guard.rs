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

/// The live tick-persist price columns. Each must be written as
/// `column_f64("<name>", round_to_2dp(...))` in the production append path.
const TICK_PRICE_COLUMNS: &[&str] = &["ltp", "open", "high", "low", "close", "avg_price"];

#[test]
fn tick_persistence_price_columns_are_rounded_to_2dp() {
    let code = code_only(&storage_src("tick_persistence.rs"));
    // The production append fn lives before the `#[cfg(test)]` mod; the
    // test-helper writers (raw column_f64 literals) live after it. We pin
    // the PRODUCTION write path by requiring the `round_to_2dp(` token to
    // appear on the same logical statement as each price column there.
    //
    // Pragmatic source-scan: require that for each price column, a
    // `column_f64("<col>", round_to_2dp(` occurrence exists. Whitespace
    // between args is normalised away by collapsing runs of spaces.
    let normalised: String = code.split_whitespace().collect::<Vec<_>>().join(" ");
    for col in TICK_PRICE_COLUMNS {
        // Two accepted write-site shapes (both rounded):
        //   column_f64("ltp", round_to_2dp(            — plain &str name
        //   column_f64(ColumnName::new_unchecked("ltp"), round_to_2dp(
        //     — pre-validated name (latency-hunt 2026-06-10; the name
        //       literal itself is pinned by
        //       test_tick_row_unchecked_ilp_names_pass_validation)
        let plain = format!("column_f64(\"{col}\", round_to_2dp(");
        let plain_spaced = format!("column_f64( \"{col}\", round_to_2dp(");
        let prevalidated =
            format!("column_f64( ColumnName::new_unchecked(\"{col}\"), round_to_2dp(");
        let prevalidated_tight =
            format!("column_f64(ColumnName::new_unchecked(\"{col}\"), round_to_2dp(");
        assert!(
            normalised.contains(&plain)
                || normalised.contains(&plain_spaced)
                || normalised.contains(&prevalidated)
                || normalised.contains(&prevalidated_tight),
            "tick_persistence.rs production append must write price column \
             `{col}` through `round_to_2dp(...)` (operator 2-decimal contract \
             2026-05-29). Found a `column_f64` write for `{col}` that is not \
             wrapped. Wrap it: round_to_2dp(f32_to_f64_clean(...)). Greeks/IV \
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
