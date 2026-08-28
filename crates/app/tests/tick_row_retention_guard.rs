//! A tick we cannot FOLD is not a tick we cannot STORE.
//!
//! This guard pins the one line in `dhan_feed_stack.rs` that decides whether a
//! refused tick keeps its row in `ticks` or is discarded outright:
//!
//! ```ignore
//! let hard_refusal = stats.refused_price || stats.refused_timestamp;
//! ```
//!
//! # Why a guard exists for one boolean
//!
//! Because the same mistake has now been made FOUR times, each time by adding
//! a condition to that line, and each time it cost real market data:
//!
//! | date | condition wrongly made hard | measured cost |
//! |---|---|---|
//! | 2026-08-20 | price sentinel `0.0` | ~22,000 ticks/session |
//! | 2026-08-26 | timestamp sentinel `0` | 825,783 ticks/session |
//! | 2026-08-28 | out-of-band timestamp | 2,008,916 ticks/session |
//! | 2026-08-28 | slot exhaustion | unmeasured — the counter was not shipped |
//!
//! The reasoning that produced all four was identical and is always plausible:
//! "we cannot place this tick in a candle bucket, so refuse it". The step that
//! does not follow is discarding the ROW, because `ticks` does not need a
//! bucket — it needs a timestamp the writer can stamp and a price it can
//! store, and every one of those four cases had both.
//!
//! # Why only two conditions may ever be hard
//!
//! A hard refusal is correct in exactly one situation: writing the row would
//! put CORRUPT data in the table. That is true of a non-finite or out-of-range
//! price, and of a timestamp outside the plausible band with no receipt time
//! to stamp the row from instead — the latter would mint a 1970 or year-2106
//! partition that retention and archival, both keyed on the trading day, can
//! never reach.
//!
//! It is NOT true of slot exhaustion, and the gate ORDER in
//! `multi_tf_aggregator::consume_tick` is the proof: timestamp band, then
//! price, then trading day, then session window, and only THEN the slot
//! lookup. A tick that reaches the slot check has already passed every
//! validity gate the writer cares about. Slot exhaustion describes OUR
//! capacity, not the tick.

use std::path::PathBuf;

fn feed_stack_src() -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/dhan_feed_stack.rs");
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// The `hard_refusal` binding, with `//` comments stripped.
///
/// Comment-stripping is load-bearing: the reasoning for this rule is written
/// directly above the line, and every one of the four conditions is NAMED in
/// that prose. A scan that did not strip comments would match the explanation
/// instead of the code and pass while the code was wrong — which is precisely
/// the class of vacuous guard this file exists to avoid being.
fn hard_refusal_expr(src: &str) -> String {
    let start = src
        .find("let hard_refusal =")
        .expect("dhan_feed_stack.rs must bind `hard_refusal`");
    let end = start
        + src[start..]
            .find(';')
            .expect("the `hard_refusal` binding must terminate");
    src[start..end]
        .lines()
        .map(|line| line.split_once("//").map_or(line, |(before, _)| before))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Fields that may appear in `hard_refusal`. Adding one is a decision that
/// costs rows, so it must be made here, deliberately, and not by editing a
/// boolean in passing.
const MAY_BE_HARD: &[&str] = &["refused_price", "refused_timestamp"];

/// Fields that must NEVER appear: the tick is valid, only the fold is not.
const MUST_NEVER_BE_HARD: &[&str] = &[
    "slot_exhausted",
    "out_of_session",
    "untraded_sentinel",
    "untraded_timestamp",
    "stale_trading_day",
    "out_of_band_timestamp",
];

#[test]
fn a_foldable_failure_never_costs_the_row() {
    let expr = hard_refusal_expr(&feed_stack_src());

    for field in MUST_NEVER_BE_HARD {
        assert!(
            !expr.contains(field),
            "`{field}` appears in `hard_refusal`, so a tick refused for that \
             reason is DISCARDED — no row in `ticks` at all, not merely a \
             missing candle.\n\n\
             That is only correct when writing the row would store CORRUPT \
             data. It is not correct here: by the time this field is set the \
             tick has already passed the timestamp band, the price check, the \
             trading-day gate and the session window, in that order. What \
             failed is our ability to place it in a bucket, which `ticks` does \
             not need.\n\n\
             This exact mistake has been made four times and cost between \
             22,000 and 2,008,916 ticks per session each time. If this one is \
             genuinely different, add it to MAY_BE_HARD with the reason \
             writing the row would corrupt the table.\n\n\
             Found: {expr}"
        );
    }
}

#[test]
fn only_the_two_corrupting_conditions_are_hard() {
    let expr = hard_refusal_expr(&feed_stack_src());

    for field in MAY_BE_HARD {
        assert!(
            expr.contains(field),
            "`{field}` is missing from `hard_refusal`. Both listed conditions \
             MUST stay hard: a non-finite price and an unstampable timestamp \
             each put corrupt data in `ticks`, and a corrupt row is worse than \
             a lost one because a lost row is counted and a corrupt row is \
             trusted.\n\nFound: {expr}"
        );
    }
}

/// Self-test: the extractor must actually see the fields, in both directions.
///
/// Without this, a helper that returned an empty string would leave both tests
/// above permanently green — the loss measured, the measurement discarded, the
/// dashboard green. That is the shape this repo keeps paying for.
#[test]
fn guard_self_test_reads_code_and_ignores_prose() {
    let good = "        let hard_refusal = stats.refused_price || stats.refused_timestamp;";
    let bad = "        let hard_refusal = stats.refused_price || stats.slot_exhausted;";
    let commented = "        // slot_exhausted used to live in hard_refusal\n\
                      let hard_refusal = stats.refused_price || stats.refused_timestamp;";

    assert!(hard_refusal_expr(good).contains("refused_price"));
    assert!(!hard_refusal_expr(good).contains("slot_exhausted"));
    assert!(hard_refusal_expr(bad).contains("slot_exhausted"));
    assert!(
        !hard_refusal_expr(commented).contains("slot_exhausted"),
        "a mention inside a `//` comment must not count as code — otherwise \
         the prose explaining this rule would fail the rule"
    );
}

/// The real file must be non-vacuous: the binding exists and names something.
#[test]
fn scanner_is_not_vacuous_against_the_real_file() {
    let expr = hard_refusal_expr(&feed_stack_src());
    assert!(
        expr.contains("stats."),
        "extracted `hard_refusal` expression looks empty — the scanner is not \
         reading the real binding. Found: {expr}"
    );
}
