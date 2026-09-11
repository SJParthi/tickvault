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

#[test]
fn both_depth_pools_stamp_the_clock_when_a_swap_is_queued() {
    let depth20 = read("src/depth20_track.rs");
    assert!(
        depth20.contains("record_subscribe_at"),
        "depth-20 dispatch no longer starts the first-packet clock — 250 of the \
         255 depth slots would stop being measured"
    );
    let depth200 = read("src/depth_rebalance.rs");
    assert!(
        depth200.contains("record_subscribe_at"),
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
        let body = read(path);
        let stamp = body
            .find("record_subscribe_at")
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
    let drain = read("src/dhan_feed_stack.rs");
    let observe = drain
        .find("global_depth_first_packet_tracker().observe_at")
        .expect("the frame drain no longer observes first packets — NOTHING is measured");
    // The level loop is what makes this per-PACKET rather than per-LEVEL. A
    // depth-200 frame carries 200 levels; observing inside the loop would
    // multiply a hot-path probe by two hundred for a question that has one
    // answer per packet.
    let level_loop = drain[observe..]
        .find("for (idx, level) in levels.iter().enumerate()")
        .expect("the depth level loop moved; re-verify the observe placement by hand");
    assert!(
        level_loop > 0,
        "the first-packet observation must precede the level loop"
    );
}

#[test]
fn the_drain_skips_replayed_frames() {
    // A WAL frame was captured before the subscribe it would be judged
    // against, so measuring it reports a latency against a clock that never
    // ran. Same reasoning, and the same guard, as the ghost check beside it.
    let drain = read("src/dhan_feed_stack.rs");
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
    let loop_body = read("src/depth_rebalance.rs");
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
    let drain = read("src/dhan_feed_stack.rs");
    assert!(
        !drain.contains("sweep_expired_at"),
        "the O(pending) sweep must never run on the frame drain"
    );
}
