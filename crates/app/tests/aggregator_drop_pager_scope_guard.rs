//! AGGREGATOR-DROP-01 pager scope guard.
//!
//! # What this pins, and why it is worth a build-failing test
//!
//! The 30-second aggregator read-out in `dhan_feed_stack.rs` emits an `error!`
//! carrying `code = AGGREGATOR-DROP-01`. That code is metric-filtered in
//! `deploy/aws/terraform/error-code-alarms.tf` as
//! `{ $.code = "AGGREGATOR-DROP-01" && $.level = "ERROR" }` with
//! `threshold = 1`, `datapoints_to_alarm = 1`, `evaluation_periods = 3` and —
//! decisively — `ok_recovery = false`. One ERROR line in a 5-minute period
//! raises the alarm, and the alarm never sends an OK, so it stays latched
//! until a human clears it.
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
//! # The rule
//!
//! The two vendor-clock reasons are reported on a SEPARATE `warn!`. Every one
//! of the 23 metric filters in this repository requires `$.level = "ERROR"`,
//! so a `warn!` reaches CloudWatch Logs and the dashboards while paging nobody.
//! In both cases the ROW IS WRITTEN and only the candle bucket is skipped, so
//! they are a data-quality trend, never a tick-loss count.

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

    // The pager's own payload must not carry the two vendor-clock fields.
    // Scoped to the region between the two gates so an unrelated mention
    // elsewhere in this large file cannot fail the test spuriously.
    let start = src.find(PAGER_GATE).expect("pager gate located above");
    let end = src.find(WARN_GATE).expect("warn gate located above");
    assert!(
        start < end,
        "the warn! block must follow the pager block; found the warn gate at \
         byte {end} before the pager gate at byte {start}"
    );
    let pager_block = &src[start..end];

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
    let start = src
        .find(WARN_GATE)
        .expect("warn gate must exist — see the sibling test for why");
    let block = &src[start..];
    // Bound the region to the warn! invocation itself.
    let block = &block[..block.len().min(1600)];

    assert!(
        block.contains("warn!("),
        "the vendor-clock reasons must be emitted with warn!, not error!. \
         Every metric filter in deploy/aws/terraform requires \
         $.level = \"ERROR\", so ERROR here means a page."
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
    let start = src.find(WARN_GATE).expect("warn gate must exist");
    let tail = &src[start..];
    let tail = &tail[..tail.len().min(2400)];

    assert!(
        tail.contains("last_refusals = now;"),
        "the refusal baseline must advance after both blocks. If it advances \
         only inside a block, a quiet cycle leaves the deltas accumulating, \
         and the next line that fires reports a figure spanning many cycles \
         while claiming to describe the last 30 seconds."
    );
}
