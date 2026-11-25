//! The frame drain's live-fill tap carries the chain thread's wake (§28.6,
//! owner card 2026-11-13 "Yes, instant wake (Recommended)").
//!
//! The behaviour halves are unit tests: `LiveFillTap::from_link` attaches the
//! link's wake and a queued tick unparks a parked thread at once
//! (`test_tap_offer_wakes_a_parked_chain_thread_at_once_with_wake_from_link`),
//! and `start_chain` hands the SAME `Arc<ChainWake>` to the link and to the
//! thread it spawns (`test_start_chain_spawns_the_thread_and_take_tap_sender_take_paper_link`).
//! What neither can stage is the frame drain's own boot wiring inside
//! `dhan_feed_stack.rs` (it runs only in a full lane start), so this guard
//! reads it: the tap built from the boot's `TapLink` must be built with
//! `LiveFillTap::from_link`, never `LiveFillTap::new(link.tx)`, which drops
//! the wake and turns every idle wait back into the old 1 ms / 5 ms nap with
//! every other test still green.

use std::path::PathBuf;

fn read(rel: &str) -> String {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    std::fs::read_to_string(root.join(rel)).unwrap_or_else(|e| panic!("read {rel}: {e}"))
}

/// The block from the boot's `take_tap_sender()` to the end of its `if let`
/// body (the first `}` at the block's indentation).
fn tap_block(stack: &str) -> Option<&str> {
    let start = stack.find("crate::live_fill::take_tap_sender()")?;
    let rest = &stack[start..];
    let end = rest.find("\n    }\n")?;
    Some(&rest[..end])
}

/// The tap the drain arms is built from the whole link, so its wake rides.
fn wiring_holds(stack: &str) -> Result<(), String> {
    let block = tap_block(stack).ok_or(
        "dhan_feed_stack.rs: the boot no longer takes the tap link with take_tap_sender()",
    )?;
    let squeezed: String = block.split_whitespace().collect();
    if !squeezed.contains("with_live_fill_tap(live_fill_tap::LiveFillTap::from_link(link))") {
        return Err(format!(
            "dhan_feed_stack.rs: the frame drain must arm its tap with \
             `LiveFillTap::from_link(link)` so the chain thread's wake is attached; got:\n{block}"
        ));
    }
    if squeezed.contains("LiveFillTap::new(") || squeezed.contains("link.tx") {
        return Err(format!(
            "dhan_feed_stack.rs: the tap is built from parts of the link, which can drop \
             its wake; build it with `LiveFillTap::from_link(link)`:\n{block}"
        ));
    }
    Ok(())
}

#[test]
fn frame_drain_arms_the_live_fill_tap_with_the_chain_wake() {
    let stack = read("src/dhan_feed_stack.rs");
    if let Err(e) = wiring_holds(&stack) {
        panic!("{e}");
    }
}

/// Bite test: the pre-§28.6 wiring (tap from the sender and loss only) and a
/// missing block are both refused.
#[test]
fn wake_wiring_check_refuses_a_tap_built_without_the_wake() {
    let old = "    if let Some(link) = crate::live_fill::take_tap_sender() {\n        \
               ingest = ingest.with_live_fill_tap(\n            \
               live_fill_tap::LiveFillTap::new(link.tx).with_loss_out(link.loss),\n        \
               );\n    }\n";
    assert!(wiring_holds(old).is_err(), "the old wiring drops the wake");
    let good = "    if let Some(link) = crate::live_fill::take_tap_sender() {\n        \
                ingest = ingest.with_live_fill_tap(live_fill_tap::LiveFillTap::from_link(link));\n    \
                }\n";
    assert!(wiring_holds(good).is_ok());
    assert!(
        wiring_holds("fn main() {}").is_err(),
        "a removed block is refused"
    );
}

/// The production main-feed arm wakes the chain once per FRAME, after
/// `drain_main_feed_frame` returns (§28.6 review, 2026-11-13). `offer` only
/// records the wake, so without this call the chain thread would sleep to
/// its 1 ms / 5 ms bound on every tick.
///
/// The flush must come AFTER the frame's stage timings (the clock read
/// `frame_work_end` and the last stage record, `record_receipt_to_fold_nanos`),
/// as the first statement after them (PR C review, 2026-11-13): before, it
/// ran ahead of the clock read, so the futex wake (8.2 to 8.7 µs per edge)
/// and any preemption by the woken thread were counted in
/// `drain_main_frame`, `drain_main_tick` and `receipt_to_fold`, the stages the
/// daily capacity verdict holds under 1 µs.
fn frame_flush_holds(stack: &str) -> Result<(), String> {
    const FLUSH: &str = "ingest.flush_live_fill_wake();";
    let call = "let outcome = drain_main_feed_frame(";
    let start = stack.find(call).ok_or(
        "dhan_feed_stack.rs: the drain no longer binds `let outcome = drain_main_feed_frame(`",
    )?;
    let rest = &stack[start..];
    let clock = rest.find("let frame_work_end = std::time::Instant::now();").ok_or(
        "dhan_feed_stack.rs: the main-feed arm no longer reads `frame_work_end` after the drain",
    )?;
    if rest[..clock].contains(FLUSH) {
        return Err(
            "dhan_feed_stack.rs: the chain wake is flushed before `frame_work_end` is read, \
             so its futex wake is timed as fold work; flush it after the stage timings"
                .to_owned(),
        );
    }
    let last = rest[clock..]
        .find("record_receipt_to_fold_nanos(")
        .map(|i| clock + i)
        .ok_or("dhan_feed_stack.rs: the main-feed arm no longer records `receipt_to_fold`")?;
    if rest[clock..last].contains(FLUSH) {
        return Err(
            "dhan_feed_stack.rs: the chain wake is flushed between the stage timings; \
             flush it after `record_receipt_to_fold_nanos`"
                .to_owned(),
        );
    }
    let end = rest[last..]
        .find(");")
        .map(|i| last + i)
        .ok_or("dhan_feed_stack.rs: unterminated `record_receipt_to_fold_nanos` call")?;
    // The first statement after the last stage timing must be the flush.
    let after = rest[end + 2..]
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with("//"))
        .unwrap_or("");
    if after != FLUSH {
        return Err(format!(
            "dhan_feed_stack.rs: the statement right after the main-feed arm's \
             `record_receipt_to_fold_nanos` call must be `{FLUSH}`, got `{after}`"
        ));
    }
    if stack.matches("ingest.flush_live_fill_wake();").count() != 1 {
        return Err("dhan_feed_stack.rs: flush the chain wake exactly once per frame".to_owned());
    }
    Ok(())
}

#[test]
fn main_feed_arm_flushes_the_chain_wake_once_per_frame() {
    let stack = read("src/dhan_feed_stack.rs");
    if let Err(e) = frame_flush_holds(&stack) {
        panic!("{e}");
    }
    let tap = read("src/dhan_feed_stack/live_fill_tap.rs");
    let offer = tap
        .find("pub fn offer(")
        .and_then(|i| tap[i..].find("pub fn flush_wake(").map(|j| &tap[i..i + j]))
        .expect("live_fill_tap.rs: offer body");
    assert!(
        !offer.contains(".wake()"),
        "live_fill_tap.rs: `offer` must not wake per tick; the frame's `flush_wake` does"
    );
}

/// Bite test: a drain with no flush, a flush inside the timed span (the
/// placement before the PR C review), or a flush placed later is refused.
#[test]
fn frame_flush_check_refuses_a_missing_timed_or_late_flush() {
    const DRAIN: &str =
        "let outcome = drain_main_feed_frame(\n    &mut ingest, &frame, a, b, c,\n);\n";
    const CLOCK: &str = "let frame_work_end = std::time::Instant::now();\n";
    const STAGES: &str = "record_stage(DrainMainFrame, w);\n\
                          record_receipt_to_fold_nanos(\n    u64::try_from(\n        x.as_nanos(),\n    )\n    \
                          .unwrap_or(u64::MAX),\n);\n";
    const FLUSH: &str = "// one wake per frame\ningest.flush_live_fill_wake();\n";
    let good = format!("{DRAIN}{CLOCK}{STAGES}{FLUSH}let x = 1;\n");
    assert!(frame_flush_holds(&good).is_ok(), "{good}");
    let missing = format!("{DRAIN}{CLOCK}{STAGES}let x = 1;\n");
    assert!(frame_flush_holds(&missing).is_err());
    let timed = format!("{DRAIN}{FLUSH}{CLOCK}{STAGES}let x = 1;\n");
    assert!(
        frame_flush_holds(&timed).is_err(),
        "a flush before the clock read is timed as fold work"
    );
    let between = format!(
        "{DRAIN}{CLOCK}record_stage(DrainMainFrame, w);\n{FLUSH}\
         record_receipt_to_fold_nanos(\n    1,\n);\nlet x = 1;\n"
    );
    assert!(frame_flush_holds(&between).is_err());
    let late = format!("{DRAIN}{CLOCK}{STAGES}let x = 1;\n{FLUSH}");
    assert!(frame_flush_holds(&late).is_err());
    let twice = format!("{DRAIN}{CLOCK}{STAGES}{FLUSH}{FLUSH}");
    assert!(frame_flush_holds(&twice).is_err(), "once per frame");
}

/// `live_fill/wake_edge` (one futex wake plus a context switch) is documented
/// as NOT gated, but `scripts/bench-gate.sh` applies its 5% regression arm to
/// every Criterion bench it finds. So the bench must run only on request
/// (`TV_BENCH_WAKE_EDGE`), and no CI workflow may set that variable.
fn wake_edge_is_opt_in(bench: &str) -> Result<(), String> {
    let squeezed: String = bench.split_whitespace().collect();
    let gate = "ifstd::env::var_os(WAKE_EDGE_ENV).is_some(){bench_wake_edge(c,&parsed);}";
    if !squeezed.contains(gate) {
        return Err(
            "benches/live_fill.rs: `bench_wake_edge` must run only inside \
             `if std::env::var_os(WAKE_EDGE_ENV).is_some()`"
                .to_owned(),
        );
    }
    if squeezed.matches("bench_wake_edge(c,").count() != 1 {
        return Err(
            "benches/live_fill.rs: `bench_wake_edge` is called outside its gate".to_owned(),
        );
    }
    if !squeezed.contains("constWAKE_EDGE_ENV:&str=\"TV_BENCH_WAKE_EDGE\";") {
        return Err(
            "benches/live_fill.rs: the gate variable must be TV_BENCH_WAKE_EDGE".to_owned(),
        );
    }
    Ok(())
}

#[test]
fn wake_edge_bench_runs_only_on_request_so_the_nightly_never_gates_it() {
    let bench = read("benches/live_fill.rs");
    if let Err(e) = wake_edge_is_opt_in(&bench) {
        panic!("{e}");
    }
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../.github/workflows");
    let entries = std::fs::read_dir(&root).unwrap_or_else(|e| panic!("read {root:?}: {e}"));
    let mut scanned = 0;
    for entry in entries {
        let path = entry.expect("dir entry").path();
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        scanned += 1;
        assert!(
            !text.contains("TV_BENCH_WAKE_EDGE"),
            "{path:?} sets TV_BENCH_WAKE_EDGE: the nightly would gate the wake-edge bench"
        );
    }
    assert!(scanned > 0, "no workflow files read from {root:?}");
}

/// Bite test: the ungated call (as before the fix) is refused.
#[test]
fn wake_edge_opt_in_check_refuses_an_ungated_call() {
    let ungated = "const WAKE_EDGE_ENV: &str = \"TV_BENCH_WAKE_EDGE\";\n\
                   fn b(c: &mut Criterion) {\n    bench_wake_edge(c, &parsed);\n}\n";
    assert!(wake_edge_is_opt_in(ungated).is_err());
    let gated = "const WAKE_EDGE_ENV: &str = \"TV_BENCH_WAKE_EDGE\";\n\
                 fn b(c: &mut Criterion) {\n    if std::env::var_os(WAKE_EDGE_ENV).is_some() {\n        \
                 bench_wake_edge(c, &parsed);\n    }\n}\n";
    assert!(wake_edge_is_opt_in(gated).is_ok());
}
