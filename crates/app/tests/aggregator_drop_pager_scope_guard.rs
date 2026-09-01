//! AGGREGATOR-DROP-01 pager scope guard.
//!
//! # What this pins, and why it is worth a build-failing test
//!
//! The 30-second aggregator read-out in `dhan_feed_stack.rs` emits an `error!`
//! carrying `code = AGGREGATOR-DROP-01`. That code is metric-filtered in
//! `deploy/aws/terraform/error-code-alarms.tf` as
//! `{ $.code = "AGGREGATOR-DROP-01" && $.level = "ERROR" }` with
//! `threshold = 1`, `datapoints_to_alarm = 1`, `evaluation_periods = 3` and —
//! decisively — `ok_recovery = false`. One ERROR line in a period raises the
//! alarm, and the alarm never sends an OK, so it stays latched until a human
//! clears it.
//!
//! That makes the emit gate a load-bearing production decision, not a logging
//! preference. It may only fire on the three reasons that are rare and mean
//! something is wrong:
//!
//!   * `refused_price`           — hard: no candle and no row
//!   * `refused_timestamp`       — hard: no candle and no row
//!   * `refused_slot_exhausted`  — candle-only, but capacity is exhausted
//!
//! # The near-miss this guard exists to prevent (2026-09-01)
//!
//! A change split two further reasons — `stale_trading_day` and
//! `out_of_band_timestamp` — out of the blended `out_of_session` field so they
//! would finally have an operator surface. That part was right: the read-out
//! deliberately skips `out_of_session`, so both had been invisible.
//!
//! The mistake was adding them to *this* gate. Measured on the 2026-08-31
//! production session:
//!
//!   * this pager fired **0 times** in the whole session — verified
//!     non-vacuously, since the same window carries 1,589 `HOT-PATH-02` and
//!     4,845 `WS-GAP-03` ERROR events in the same log group;
//!   * `out_of_band_timestamp` ran ~2,000,000 per session, roughly 85/s.
//!
//! So widening the gate would have converted a pager that is silent on a
//! healthy day into one that emits in virtually every 30-second window,
//! latching the alarm for the entire session — and with `ok_recovery = false`
//! it would never clear. The permanent-loss signal the pager exists for would
//! have been buried under vendor-clock noise, in a change whose stated purpose
//! was to reduce operator noise.
//!
//! No existing test could have caught it: the counters were correct, the
//! classification was correct, and the split itself was correct. Only the
//! coupling between a log level and a CloudWatch metric filter was wrong, and
//! that coupling lives in two files that no compiler checks against each other.
//!
//! # Why `warn!` is safe HERE — the narrow version, which is the true one
//!
//! An earlier draft of this header claimed "all 23 metric filters require
//! `$.level = \"ERROR\"`, so a warn cannot page". **That is false**, and false
//! in the reassuring direction. MEASURED across `deploy/aws/terraform`: there
//! are **32** filter patterns and **23** require ERROR. Nine do not.
//! `error-code-alarms.tf:184` is `pattern = "\"DH-906\""` — a bare TERM filter
//! with no level predicate, which matches that string at any level; others
//! match `$.tv_*` metric fields.
//!
//! The honest rule is therefore: **a `warn!` is safe only while its text and
//! its fields carry no filtered token.** The line this guard protects carries
//! none — checked field by field. A future line added beside it must be
//! checked the same way, not waved through on a generalisation.
//!
//! Nor does the split reach a dashboard: `dashboard.tf` has no log-insights
//! widgets, and the metric side folds every reason into one summed series. The
//! split's only surface is the raw log text. That is a real limitation and it
//! is stated rather than implied away.

use std::fs;
use std::path::PathBuf;

fn feed_stack_src() -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/dhan_feed_stack.rs");
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()))
}

/// The exact gate the pager must keep. Written as a literal so that widening
/// it is a visible diff on this line, not an invisible behavioural change.
const PAGER_GATE: &str = "if d_price > 0 || d_ts > 0 || d_slot > 0 {";

/// The separate, non-paging gate the two vendor-clock reasons belong on.
const WARN_GATE: &str = "if d_stale > 0 || d_oob > 0 {";

/// Both gates sit at the same indentation inside the `tokio::select!` arm, so
/// a block ends at the first closing brace on that indentation.
const BLOCK_CLOSE: &str = "\n                }";

/// The byte range of the block introduced by `gate`, from the gate to its
/// closing brace.
///
/// Earlier versions of this file bounded these regions with fixed byte counts
/// (1600 and 2400). That was brittle in the worst way: adding roughly eight
/// lines of comment to the source would have pulled an unrelated `error!` into
/// the window and failed a test with a message that was simply untrue. A guard
/// whose first failure is a false one teaches the reader to delete it.
fn block_of<'a>(src: &'a str, gate: &str) -> &'a str {
    let start = src
        .find(gate)
        .unwrap_or_else(|| panic!("gate not found in source: {gate:?}"));
    let rest = &src[start..];
    let end = rest
        .find(BLOCK_CLOSE)
        .unwrap_or_else(|| panic!("no closing brace found for gate {gate:?}"))
        + BLOCK_CLOSE.len();
    &rest[..end]
}

#[test]
fn the_pager_gate_excludes_the_two_vendor_clock_reasons() {
    let src = feed_stack_src();

    // Non-vacuity first: if either gate cannot be found the rest of this test
    // would pass by accident, which is the failure mode it is meant to stop.
    assert!(
        src.contains(PAGER_GATE),
        "the AGGREGATOR-DROP-01 emit gate is no longer {PAGER_GATE:?}. If the \
         gate was widened, read this file's header: that alarm has \
         threshold=1 / dta=1 / ok_recovery=false, and out_of_band_timestamp \
         runs ~85/s, so widening it latches the alarm for the whole session \
         and it never clears."
    );
    assert!(
        src.contains(WARN_GATE),
        "the non-paging warn! gate {WARN_GATE:?} is missing — the two \
         vendor-clock reasons must still be reported somewhere, just never at \
         ERROR level. Deleting it silently restores the blind spot the split \
         was written to close."
    );

    let pager_block = block_of(&src, PAGER_GATE);
    for field in ["refused_stale_trading_day", "refused_out_of_band_timestamp"] {
        assert!(
            !pager_block.contains(field),
            "{field:?} appears inside the AGGREGATOR-DROP-01 pager block. \
             Both reasons are candle-only — the row IS written — and both run \
             at vendor-clock rates. They belong on the warn! beneath, never on \
             a line whose code is metric-filtered at ERROR level."
        );
    }
}

#[test]
fn the_warn_block_reports_both_vendor_clock_reasons_and_never_pages() {
    let src = feed_stack_src();
    let block = block_of(&src, WARN_GATE);

    assert!(
        block.contains("warn!("),
        "the vendor-clock reasons must be emitted with warn!, not error!. The \
         AGGREGATOR-DROP-01 filter matches on the code AND $.level = \"ERROR\", \
         so ERROR here means a page."
    );
    assert!(
        !block.contains("error!("),
        "an error! appeared in the vendor-clock block — that is the exact \
         change this guard exists to refuse."
    );
    assert!(
        !block.contains("ErrorCode::AggregatorDrop01"),
        "the vendor-clock block must not carry the AGGREGATOR-DROP-01 code: \
         the metric filter matches on the code AND the level, so tagging it \
         here re-creates the pager coupling even at warn level if the level \
         predicate is ever relaxed."
    );
    // Not every filter in this repository requires ERROR — `dh-906` is a bare
    // term filter matching any level. So the warn must also carry no filtered
    // token in its text or fields.
    assert!(
        !block.contains("DH-906"),
        "the vendor-clock warn! carries the literal DH-906. \
         error-code-alarms.tf filters that string with NO level predicate, so \
         a warn containing it pages exactly like an error would."
    );
    for field in ["refused_stale_trading_day", "refused_out_of_band_timestamp"] {
        assert!(
            block.contains(field),
            "{field:?} is missing from the warn! block — it would then have no \
             operator surface at all, which is the blind spot the 2026-09-01 \
             split was written to close."
        );
    }
}

#[test]
fn the_refusal_baseline_advances_outside_both_blocks() {
    let src = feed_stack_src();

    // The property is POSITIONAL: the assignment must sit after the warn
    // block's closing brace, not merely somewhere nearby. An earlier version
    // of this test asserted only that the assignment appeared within a fixed
    // byte window after the gate — a window that CONTAINED the warn block, so
    // moving the assignment back inside it (restoring the original bug) still
    // passed. The test's own name was the only thing asserting the property.
    let warn_start = src.find(WARN_GATE).expect("warn gate must exist");
    let warn_block = block_of(&src, WARN_GATE);
    let after_warn_block = warn_start + warn_block.len();

    let assign = "last_refusals = now;";
    let pos = src[after_warn_block..].find(assign).unwrap_or_else(|| {
        panic!(
            "{assign:?} does not appear AFTER the warn block's closing brace. \
             If it moved inside either block, a quiet cycle leaves the deltas \
             accumulating, and the next line that fires reports a figure \
             spanning many cycles while claiming to describe the last 30 \
             seconds."
        )
    });

    // Bite-proof the positional claim: the assignment must also not appear
    // inside either block, which the search above cannot see on its own.
    let pager_block = block_of(&src, PAGER_GATE);
    assert!(
        !pager_block.contains(assign) && !warn_block.contains(assign),
        "the refusal baseline is assigned INSIDE one of the emit blocks. It \
         must advance every cycle, whether or not either line fired."
    );

    // And it must be the outer-indentation assignment (16 spaces), not one
    // nested deeper in some later construct.
    let abs = after_warn_block + pos;
    let line_start = src[..abs].rfind('\n').map(|i| i + 1).unwrap_or(0);
    let indent = abs - line_start;
    assert_eq!(
        indent, 16,
        "the baseline assignment is at indentation {indent}, expected 16 (the \
         select-arm's outer level). A deeper indentation means it sits inside \
         some conditional and no longer advances unconditionally."
    );
}
