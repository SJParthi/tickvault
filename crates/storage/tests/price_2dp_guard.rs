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

// RETIRED (stage-2 dead-WS sweep, 2026-07-17):
// `tick_persistence_price_columns_are_rounded_to_2dp` pinned the
// `round_to_2dp(...)` wraps at the `RawTickFields` build site in
// `tick_persistence.rs::build_tick_row_seq` — that file (and the shared
// `tick_row_builder.rs` it delegated to) was DELETED with the dead Dhan
// tick chain, so there is no tick-persist price write site left to pin.
// The operator 2-decimal contract STANDS for every surviving price write
// path: the candle seal row pin below (`candles_*`, the LIVE path) and the
// REST-leg persistence modules' own rounding pins.

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

// RETIRED (stage-2 dead-WS sweep, 2026-07-17):
// `round_to_2dp_helper_is_available_in_storage` pinned the `round_to_2dp`
// re-export inside the deleted `tick_persistence.rs`. The canonical
// primitive lives in `tickvault_common::price_precision::round_to_2dp`
// (unchanged); surviving storage write sites import it directly.

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
    let _ = base.join("shadow_seal_columns.rs");
}
