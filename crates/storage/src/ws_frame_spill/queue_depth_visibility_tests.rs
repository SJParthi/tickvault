//! Queue-depth visibility source scans, moved verbatim out of
//! `ws_frame_spill.rs` on 2026-10-26 (plan item K2) so the parent stays under
//! its size pin.

/// The writer loop must publish the queue depth AND its session high-water.
///
/// Source-scanned rather than behavioural because the loop is a thread with
/// no return value and the gauges are process-global; a behavioural test
/// would have to install a recorder and race the thread. What can go wrong
/// here is a refactor deleting the lines, and a scan catches exactly that.
/// The writer loop body, bounded by the next top-level fn.
///
/// NOT split on `#[cfg(test)]` like the other guards in this repo: that
/// marker appears INSIDE `writer_loop` itself (the `maybe_test_panic`
/// hook), so the usual split truncates the production text mid-function
/// and the scan silently sees nothing. Caught by these tests failing on
/// their first run against code that was already correct.
fn writer_loop_src() -> &'static str {
    let src = include_str!("../ws_frame_spill.rs");
    let from = src.find("fn writer_loop").expect("writer_loop must exist");
    let len = src[from..]
        .find("fn open_segment_resilient")
        .expect("the next top-level fn must still follow writer_loop");
    &src[from..from + len]
}

#[test]
fn the_writer_publishes_its_queue_depth_and_high_water() {
    let production = writer_loop_src();

    assert!(
        production.contains("tv_ws_frame_spill_queue_depth"),
        "the queued-but-unwritten window must be observable — it is the \
         frames an abort loses, and nothing else counts them"
    );
    assert!(
        production.contains("tv_ws_frame_spill_queue_high_water"),
        "the PEAK is the load-bearing half: a 30s scrape of a decaying \
         gauge misses the burst that matters"
    );
    assert!(
        production.contains("let queued = rx.len();"),
        "depth must be read from the channel itself, not inferred"
    );
}

/// The gauges must be resolved OUTSIDE the loop and read at the BATCH
/// boundary, never per frame.
///
/// This is the `record_ws_lag` lesson, which allocated twice per tick —
/// ~36M allocations/hour — on a path its own docs called allocation-free.
/// Three correct comments did not stop it shipping; a test does.
#[test]
fn the_depth_gauge_is_not_read_on_the_per_frame_path() {
    let loop_body = writer_loop_src();

    // The handles are resolved before the `loop {`; the reads happen after
    // the inner drain. If either moved into the 0..256 drain, the gauge
    // write would land once per FRAME instead of once per batch.
    let resolve_at = loop_body
        .find("metrics::gauge!(\"tv_ws_frame_spill_queue_depth\")")
        .expect("depth gauge must be resolved in writer_loop");
    let loop_at = loop_body.find("\n    loop {").expect("the drain loop");
    assert!(
        resolve_at < loop_at,
        "the gauge handle is resolved INSIDE the loop — re-resolving a \
         handle per iteration is how a metric write becomes a cost"
    );

    let drain_at = loop_at
        + loop_body[loop_at..]
            .find("for _ in 0..256")
            .expect("the batch drain");
    let read_at = loop_at
        + loop_body[loop_at..]
            .find("let queued = rx.len();")
            .expect("the depth read");
    assert!(
        read_at > drain_at,
        "the depth is read BEFORE the batch drain — it must sit at the \
         batch boundary so it costs one write per ~257 records, not one \
         per frame"
    );
}

#[test]
fn refusal_line_due_logs_the_first_and_every_power_of_two() {
    use super::refusal_line_due;
    // The first refusal MUST log (it is what pages); the flood must not.
    assert!(refusal_line_due(1));
    assert!(refusal_line_due(2));
    assert!(!refusal_line_due(3));
    assert!(refusal_line_due(4));
    assert!(!refusal_line_due(1_000));
    assert!(refusal_line_due(1 << 20));
    // 2026-09-04: 2,000,238 refusals in one session would have produced
    // 21 lines instead of two million.
    let lines = (1..=2_000_238u64).filter(|n| refusal_line_due(*n)).count();
    assert_eq!(lines, 21);
    assert!(!refusal_line_due(0), "a zero count is not a refusal");
}

#[test]
fn refusal_line_due_stops_doubling_at_the_stride_so_a_long_storm_is_never_silent() {
    use super::{REFUSAL_LINE_STRIDE, refusal_line_due};
    // Past the ladder the line repeats at a FIXED stride. Under the pure
    // power-of-two rule the counter is never reset, so after a 2M-refusal
    // episode the next line was 2^21 and the one after that 2^22 - a
    // 2.1-MILLION-frame silence inside an outage that is still losing
    // frames.
    assert!(refusal_line_due(REFUSAL_LINE_STRIDE));
    assert!(refusal_line_due(REFUSAL_LINE_STRIDE * 3));
    assert!(refusal_line_due(REFUSAL_LINE_STRIDE * 7));
    assert!(!refusal_line_due(REFUSAL_LINE_STRIDE * 3 + 1));
    // 3 x stride is NOT a power of two: this case logged nothing before.
    assert!(!(REFUSAL_LINE_STRIDE * 3).is_power_of_two());

    // The worst-case silent gap anywhere past the ladder is one stride.
    let start = REFUSAL_LINE_STRIDE * 5 + 1;
    let mut silent = 0u64;
    for n in start..=(start + REFUSAL_LINE_STRIDE) {
        if refusal_line_due(n) {
            break;
        }
        silent += 1;
    }
    assert!(
        silent < REFUSAL_LINE_STRIDE,
        "silent run of {silent} frames exceeds the stride"
    );
}
