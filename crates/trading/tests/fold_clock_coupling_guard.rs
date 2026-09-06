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
        cell.contains("fold_clock_ist_secs(tick.exchange_timestamp, tick.received_at_nanos)"),
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

/// A tick with NO receipt falls back to the exchange stamp -- never to zero,
/// never to a clock read.
///
/// This is the half that keeps the split fail-soft: v1/v2 WAL records and
/// Ticker-mode packets genuinely have no receipt, and a fold that dropped them
/// would lose real ticks to enforce a rule about a value that does not exist.
#[test]
fn a_tick_without_a_receipt_falls_back_to_its_trade_stamp() {
    use tickvault_trading::candles::tf_index::fold_clock_ist_secs;

    const STAMP: u32 = 1_800_000_000;
    for absent in [0_i64, -1, i64::MIN] {
        assert_eq!(
            fold_clock_ist_secs(STAMP, absent),
            STAMP,
            "an absent receipt ({absent}) must fall back to the trade stamp"
        );
    }
}

/// The delta guard only ever WIDENS toward the exchange stamp, never invents a
/// third value.
#[test]
fn an_implausible_receipt_never_produces_a_third_clock() {
    use tickvault_trading::candles::tf_index::fold_clock_ist_secs;

    const STAMP: u32 = 1_800_000_000;
    // A receipt hours late (a replayed frame re-stamped by an old binary) and
    // one hours early (a stepped clock) must BOTH yield the trade stamp.
    for receipt_ist_secs in [
        i64::from(STAMP) + 86_400,
        i64::from(STAMP) - 86_400,
        i64::from(STAMP) + 301,
        i64::from(STAMP) - 11,
    ] {
        let receipt_utc_nanos = (receipt_ist_secs
            - i64::from(tickvault_common::constants::IST_UTC_OFFSET_SECONDS))
            * 1_000_000_000;
        assert_eq!(
            fold_clock_ist_secs(STAMP, receipt_utc_nanos),
            STAMP,
            "a receipt {receipt_ist_secs} outside the plausible delta must fall \
             back to the trade stamp, not to a blended or clamped third value"
        );
    }

    // And one INSIDE the band is genuinely preferred, or the guard above is
    // vacuous because nothing is ever preferred.
    let inside_ist = i64::from(STAMP) + 120;
    let inside_utc_nanos = (inside_ist
        - i64::from(tickvault_common::constants::IST_UTC_OFFSET_SECONDS))
        * 1_000_000_000;
    assert_eq!(
        fold_clock_ist_secs(STAMP, inside_utc_nanos),
        u32::try_from(inside_ist).unwrap(),
        "a receipt inside the band MUST be preferred -- if it is not, the whole \
         W2 change is inert and every test above passes vacuously"
    );
}
