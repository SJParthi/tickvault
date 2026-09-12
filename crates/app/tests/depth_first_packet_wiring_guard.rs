//! The first-packet tracker is only worth its hot-path cost if all four of
//! its call sites stay wired (2026-09-11).
//!
//! The module's own unit tests prove the ARITHMETIC. They prove nothing about
//! whether anything calls it, and a measurement module with no live call site
//! is the shape this repository has recorded repeatedly: `scan_silence` sat
//! seeded and observed with ZERO production readers for weeks, reading greener
//! than dead code because every part of it looked connected.
//!
//! Four sites, each failing differently if it drifts:
//!
//! | Site | What its loss costs |
//! |---|---|
//! | depth-20 dispatch | 250 of the 255 depth slots stop being measured |
//! | depth-200 dispatch | the 5 deep sockets stop being measured |
//! | frame drain | NOTHING is ever measured; every subscribe ages into `silent_window` |
//! | per-minute sweep | a dark contract is never counted, and the hot-path gate stays hot forever |
//!
//! The fourth is the subtle one. `pending_count` is what keeps the drain's
//! cost to one relaxed load; without the sweep an unanswered subscribe pins it
//! above zero for the rest of the session, so EVERY depth packet pays a hash
//! probe it did not need to.

use std::fs;

fn read(path: &str) -> String {
    fs::read_to_string(path).unwrap_or_else(|e| panic!("read {path}: {e}"))
}

/// `read`, with every `//` comment removed.
///
/// ⚠ Added 2026-09-11, and it closes a defect in BOTH directions — a 2026-09-11
/// audit found this file was the only one of its family scanning RAW text while
/// ten sibling guards under `crates/app/tests/` strip first:
///
/// * A deleted call site left behind as `// record_subscribe_at(...)` keeps
///   every `contains` assertion below GREEN while nothing is measured.
/// * The inverse cost is not theoretical: the sentence "See
///   `record_subscribe_at`." added to a dispatch site the same day moved the
///   `find` anchor several hundred bytes and turned
///   `the_stamp_sits_on_the_success_arm_and_not_on_a_refusal` RED against code
///   that was correct. A guard whose first act is a false positive is a guard
///   somebody deletes.
///
/// Line-comments only. A `/* */` block inside a string literal cannot be
/// stripped without a Rust lexer, and none of the scanned text uses block
/// comments; stated so the limit is on the record rather than assumed away.
fn read_without_comments(path: &str) -> String {
    read(path)
        .lines()
        .map(|line| match line.find("//") {
            Some(at) => &line[..at],
            None => line,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn both_depth_pools_stamp_the_clock_when_a_swap_is_queued() {
    let depth20 = read_without_comments("src/depth20_track.rs");
    assert!(
        depth20.contains(".record_subscribe_at("),
        "depth-20 dispatch no longer starts the first-packet clock — 250 of the \
         255 depth slots would stop being measured"
    );
    let depth200 = read_without_comments("src/depth_rebalance.rs");
    assert!(
        depth200.contains(".record_subscribe_at("),
        "depth-200 dispatch no longer starts the first-packet clock"
    );
}

#[test]
fn the_stamp_sits_on_the_success_arm_and_not_on_a_refusal() {
    // The stamp must be adjacent to the SENT counter, which is inside the
    // `Ok(())` arm. A stamp on a refused send has no subscribe behind it and
    // ages out into a `silent_window` that reports the market being quiet when
    // the truth is that nothing was ever asked for.
    for (path, sent_counter) in [
        ("src/depth20_track.rs", "DEPTH20_SWAPS_SENT"),
        ("src/depth_rebalance.rs", "REBALANCE_SWAPS_SENT"),
    ] {
        let body = read_without_comments(path);
        // The CALL, not the bare name: the name also appears in prose that
        // explains the stamp, and anchoring on prose measures the comment's
        // position rather than the code's.
        let stamp = body
            .find(".record_subscribe_at(")
            .unwrap_or_else(|| panic!("{path} has no first-packet stamp"));
        let sent = body
            .find(&format!("counter!({sent_counter}).increment(1)"))
            .unwrap_or_else(|| panic!("{path} has no {sent_counter} dispatch increment"));
        let gap = stamp.abs_diff(sent);
        assert!(
            gap < 600,
            "{path}: the first-packet stamp drifted {gap} bytes from the \
             {sent_counter} increment that marks the success arm — it may now \
             sit on a refusal path"
        );
    }
}

#[test]
fn the_drain_observes_once_per_packet_and_before_the_level_loop() {
    let drain = read_without_comments("src/dhan_feed_stack.rs");
    let observe = drain
        .find("global_depth_first_packet_tracker().observe_at")
        .expect("the frame drain no longer observes first packets — NOTHING is measured");
    // The level loop is what makes this per-PACKET rather than per-LEVEL. A
    // depth-200 frame carries 200 levels; observing inside the loop would
    // multiply a hot-path probe by two hundred for a question that has one
    // answer per packet.
    // ⚠ The `.expect` above IS this guard; the assertion that used to follow it
    // was VACUOUS and is removed (2026-09-11). It read `level_loop > 0`, where
    // `level_loop` is an offset INTO `drain[observe..]` — zero is unreachable
    // once the `find` has succeeded, so it could never fail and read as
    // coverage that did not exist.
    //
    // What the `.expect` really proves: a level loop opens somewhere AFTER the
    // observation. Move the observation inside the loop and no further loop
    // follows it, so the `find` returns `None` and this panics. That is the
    // realistic regression and it is caught. What it does NOT prove is
    // placement relative to the EARLIER, unrelated level loop in this file —
    // stated because an assertion nobody can fail is worse than none.
    drain[observe..]
        .find("for (idx, level) in levels.iter().enumerate()")
        .expect(
            "no depth level loop follows the first-packet observation — it has \
             moved INSIDE the loop, multiplying a hot-path probe by up to 200 \
             for a question with one answer per packet",
        );
}

/// Bite-test: proves the scans above can actually FAIL.
///
/// ⚠ Added 2026-09-11. Until then this file had six tests and no fixture, so a
/// parser or offset bug would have left every one of them passing green on a
/// tree with the wiring torn out — the inert-guard shape `rust_only_guard.rs`
/// and `browser_surface_and_toolchain_guard.rs` each carry a self-test to
/// prevent. A guard is a claim, and an unproven guard is an unproven claim.
#[test]
fn guard_self_test() {
    // The comment stripper must remove a commented-out call site...
    let commented = "    // x.record_subscribe_at(1, s, p, t, false);\n    let y = 1;";
    let stripped = commented
        .lines()
        .map(|line| match line.find("//") {
            Some(at) => &line[..at],
            None => line,
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        !stripped.contains(".record_subscribe_at("),
        "a commented-out call site must not satisfy the wiring assertions"
    );
    // ...and must NOT remove a live one on a line that also carries a comment.
    let live = "    x.record_subscribe_at(1, s, p, t, false); // stamp the clock";
    let live_stripped = match live.find("//") {
        Some(at) => &live[..at],
        None => live,
    };
    assert!(
        live_stripped.contains(".record_subscribe_at("),
        "stripping a trailing comment must not take the code with it"
    );
    // The anchor must be the CALL, not the bare name — prose mentioning the
    // function is what broke this guard once.
    let prose = "    // See `record_subscribe_at` for why.";
    let prose_stripped = match prose.find("//") {
        Some(at) => &prose[..at],
        None => prose,
    };
    assert!(!prose_stripped.contains("record_subscribe_at"));
    // And the real files must still satisfy what the fixtures describe.
    assert!(read_without_comments("src/depth20_track.rs").contains(".record_subscribe_at("));
}

#[test]
fn the_drain_skips_replayed_frames() {
    // A WAL frame was captured before the subscribe it would be judged
    // against, so measuring it reports a latency against a clock that never
    // ran. Same reasoning, and the same guard, as the ghost check beside it.
    let drain = read_without_comments("src/dhan_feed_stack.rs");
    let observe = drain
        .find("global_depth_first_packet_tracker().observe_at")
        .expect("no observe call site");
    let window_start = observe.saturating_sub(400);
    assert!(
        drain[window_start..observe].contains("frame.connection_index != u8::MAX"),
        "the first-packet observation is no longer gated on a LIVE frame — \
         replayed WAL frames would report fabricated latencies"
    );
}

#[test]
fn the_steering_loop_sweeps_and_pre_registers() {
    let loop_body = read_without_comments("src/depth_rebalance.rs");
    assert!(
        loop_body.contains("sweep_expired_at"),
        "the per-minute sweep is gone: a dark contract is never counted, AND \
         the drain's hot-path gate stays above zero for the rest of the \
         session, so every depth packet pays a probe it does not need"
    );
    assert!(
        loop_body.contains("pre_register_first_packet_counters"),
        "the outcome series are no longer seeded at zero — a counter whose \
         first increment is the event it reports is the sample an exporter \
         drops (the 2026-08-28 seeding defect)"
    );
}

#[test]
fn the_sweep_runs_on_the_steering_task_and_never_on_the_drain() {
    // O(pending) work on the frame drain is the coupling this repository has
    // removed twice — the drain is the only task emptying the socket, and
    // Dhan skips a slow consumer forward with no sequence number.
    let drain = read_without_comments("src/dhan_feed_stack.rs");
    assert!(
        !drain.contains("sweep_expired_at"),
        "the O(pending) sweep must never run on the frame drain"
    );
}
