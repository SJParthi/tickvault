//! The five sites that decide a tick's candle identity must read ONE clock.
//!
//! # The trap this pins, in the plan's own words
//!
//! W2 of `active-plan-receipt-clock-preopen.md`:
//!
//! > The bucketing clock is ONE line ... but **eleven other sites read the same
//! > field**, and four of them must move WITH it or they disagree with the
//! > bucket they are guarding ... Moving the bucket clock alone leaves ordering
//! > deciding on one clock and bucketing on another.
//!
//! All five now read `fold_secs`, derived once per tick from
//! `fold_clock_ist_secs`. Nothing pinned that, so a future edit could move any
//! single one back to the raw exchange stamp and every existing test would
//! still pass -- each site is individually correct on either clock, and only
//! their AGREEMENT is the property.
//!
//! # What a split actually costs
//!
//! | Site reverted alone | Consequence |
//! |---|---|
//! | bucket | the bar is chosen on one clock while its own session gate admits on another |
//! | watermark advance | the stale-trading-day gate compares a receipt-clock tick against an exchange-clock watermark -- the exact midnight-crossing shape the code comment says it removed |
//! | session gate | a print admitted by the bucket is refused by the gate, or the reverse |
//! | `close_ts_ist_secs` | the close is owned by a different packet than the one the bucket believes is last |
//! | `tick_is_newest` | open-interest and close take their value from disagreeing orderings |
//!
//! None of those is a crash. Every one is a quietly wrong candle.

use std::path::Path;
use tickvault_common::source_scan::strip_rust_comments;

fn src(rel: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(rel);
    let raw = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
    // Comments here DISCUSS both clocks at length; scanning raw text would let
    // a rationale block satisfy an assertion about the CODE.
    strip_rust_comments(&raw)
}

/// The fold clock is derived ONCE per tick, above the timeframe loop.
///
/// Not style: an O(1) audit caught an earlier draft recomputing it 48x per
/// tick, and the module records that as the same hoisting defect it had
/// already logged once before.
#[test]
fn the_fold_clock_is_derived_once_per_tick_not_per_timeframe() {
    let agg = src("src/candles/multi_tf_aggregator.rs");
    let derivations = agg.matches("fold_clock_ist_secs(").count();
    assert_eq!(
        derivations, 1,
        "`multi_tf_aggregator` must derive the fold clock EXACTLY once per tick \
         (found {derivations}). More than one derivation means it is being \
         recomputed inside a loop -- the 48x-per-tick hoisting defect this \
         module has already recorded twice."
    );
}

/// The three gates in `consume` all read the derived value.
#[test]
fn the_watermark_and_both_session_gates_read_the_fold_clock() {
    let agg = src("src/candles/multi_tf_aggregator.rs");

    assert!(
        agg.contains("let fold_secs = fold_clock_ist_secs("),
        "the per-tick fold clock must be bound to `fold_secs`"
    );
    assert!(
        agg.contains("if fold_secs > self.watermark_secs"),
        "the watermark must advance on the FOLD clock. Advancing it on the \
         exchange stamp while the stale-trading-day gate compares a fold-clock \
         value re-opens the midnight-crossing rejection the code comment says \
         it removed."
    );
    assert!(
        agg.contains("fold_secs / 86_400 < self.watermark_secs / 86_400"),
        "the stale-trading-day gate must compare LIKE WITH LIKE -- a fold-clock \
         day against a fold-clock watermark."
    );
    assert!(
        agg.contains("let secs_of_day = fold_secs % 86_400"),
        "the seconds-of-day session gate must read the fold clock, or a print \
         the bucket accepts is refused here (or the reverse)."
    );
}

/// The bucket and both close-ordering guards read the derived value.
#[test]
fn the_bucket_and_the_close_ordering_guards_read_the_fold_clock() {
    let cell = src("src/candles/aggregator_cell.rs");

    assert!(
        cell.contains("fold_clock_ist_secs(tick.exchange_timestamp)"),
        "the bucket must be chosen on the fold clock"
    );
    assert!(
        cell.contains("close_ts_ist_secs: fold_secs"),
        "`close_ts_ist_secs` must be stamped from the fold clock, or the close \
         is owned by a different packet than the bucket believes is last"
    );
    assert!(
        cell.contains("let tick_is_newest = fold_secs >= state.close_ts_ist_secs"),
        "the close-ordering guard must compare fold clock against fold clock. \
         Comparing an exchange stamp against a receipt-clock `close_ts` makes \
         every late-delivered tick look older than it is, so open interest and \
         close take their value from a packet that is not the newest."
    );
}

/// The fold clock IS the exchange stamp, for every input, with no exception.
///
/// ## Why this replaced two receipt-behaviour tests (2026-09-18)
///
/// Until the ts-bucketing directive this file pinned a delta-bounded hybrid:
/// a receipt within `[-10 s, +300 s]` of the trade won, anything outside fell
/// back. Two tests pinned each half. Both are now gone, because the property
/// they guarded no longer exists — and deleting them without a replacement
/// would leave the fold clock's VALUE unpinned entirely, which is how a
/// "consistency" refactor quietly reintroduces the receipt.
///
/// This is the stronger pin, not a weaker one: the hybrid needed two tests and
/// a band; the identity needs one test and admits no band at all.
#[test]
fn the_fold_clock_is_the_exchange_stamp_for_every_input() {
    use tickvault_trading::candles::tf_index::fold_clock_ist_secs;

    // Every shape the wire can produce, including the ones the old hybrid
    // treated specially: the session open, a mid-session second, the epoch
    // floor and the u32 ceiling.
    for stamp in [0_u32, 1, 1_800_000_000, 1_800_000_301, u32::MAX] {
        assert_eq!(
            fold_clock_ist_secs(stamp),
            stamp,
            "the fold clock must be the exchange stamp verbatim — operator \
             directive 2026-09-18, recorded in \
             `websocket-connection-scope-lock.md` section \"2026-09-18 \
             (SECOND)\". A blended, clamped or receipt-derived value here \
             re-opens the divergence between `ticks` and every candle."
        );
    }
}

/// The receipt cannot reach the fold clock, because there is nowhere to put it.
///
/// This is the mechanical half, and it is why the argument was REMOVED rather
/// than ignored. A two-argument signature whose second argument is unused
/// reads, at fourteen call sites, as though the receipt still matters — and a
/// future edit could start honouring it again with no call site changing.
/// Taking the argument away makes that a compile error instead of a review
/// question.
#[test]
fn the_fold_clock_takes_no_receipt_argument() {
    let src = src("src/candles/tf_index.rs");

    assert!(
        src.contains("pub const fn fold_clock_ist_secs(exchange_timestamp: u32) -> u32"),
        "the fold clock must take the exchange stamp ALONE. Re-adding a \
         receipt parameter is the whole 2026-09-18 directive undone, and it \
         would pass every other test in this file."
    );
    assert!(
        !src.contains("MAX_PLAUSIBLE_RECEIPT_LAG_SECS"),
        "the delta band is deleted, not merely unused. A named bound that \
         nothing enforces is the documented-mechanism-that-does-not-exist \
         class this repository keeps having to correct."
    );
}

/// The two DAY gates still read the receipt, and must keep doing so.
///
/// They are not the fold clock and were never part of this directive: their
/// question is "did the vendor stamp this for a different trading day than the
/// one we are living in", which needs both clocks by construction. A cleanup
/// that removed `received_at_nanos` from `multi_tf_aggregator` "because we
/// bucket on ts now" would delete the only defence against a snapshot filing
/// into a closed day — measured mean 5 hours, max 34 days.
#[test]
fn the_cross_day_gates_still_compare_the_fold_day_against_the_receipt_day() {
    let agg = src("src/candles/multi_tf_aggregator.rs");

    assert!(
        agg.contains("let fold_day = i64::from(fold_secs) / 86_400"),
        "the cross-day gates must still derive a fold day"
    );
    assert!(
        agg.contains("let receipt_day = receipt_ist_secs / 86_400"),
        "the cross-day gates must still derive a RECEIPT day. Without it, \
         `stale_trading_day` and `future_trading_day` compare the exchange \
         stamp against itself and can never fire."
    );
    assert!(
        agg.contains("if fold_day > receipt_day") && agg.contains("if fold_day < receipt_day"),
        "BOTH directions must survive: a stamp from a closed day and a stamp \
         from a future day are different defects with different counters."
    );
}
