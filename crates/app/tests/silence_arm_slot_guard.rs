//! SILENCE ARM SLOT-CELL GUARD (2026-11-17).
//!
//! Live on 17 Nov 2026 the drain's 30 s silence arm took about 2.9 ms per
//! firing, and about 85% of it was one subscribe-reconcile hash probe per
//! tracked instrument. The arm now hands each report's detector slot to
//! `ReconcileSweep::note_at_slot`, which reads a dense per-slot cell instead
//! of probing. This guard reads the production source and fails when the arm
//! goes back to the per-report probe, or when the cell table and the lane's
//! detector stop being sized from the same constant.
//!
//! The behaviour halves: `proptest_note_at_slot_matches_note`
//! (`subscribe_reconcile`), `proptest_scan_silence_named_matches_frozen_oracle`
//! (`dhan_feed_stack/silence_slot_oracle_tests.rs`) and the DHAT gate
//! `dhat_silence_scan.rs`.
//!
//! US1-D (2026-11-24): the drain no longer runs `scan_silence_named_at`; the
//! 30 s timer begins a sliced sweep and the idle arm steps it
//! (`dhan_feed_stack/silence_sweep.rs`). The step function is held to the
//! same rule (`the_sliced_silence_step_judges_reconcile_by_slot_not_by_probe`),
//! and the one-go form stays pinned as the sliced sweep's oracle.
//!
//! HONEST LIMIT: a text scan of a few function bodies and two constants.

use std::path::PathBuf;

fn read(rel: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(rel);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// Drops `//` line comments, so prose about the old call cannot satisfy or
/// trip the scan.
fn strip_line_comments(src: &str) -> String {
    src.lines()
        .map(|line| match line.find("//") {
            Some(at) => &line[..at],
            None => line,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The body of `fn scan_silence_named_at` up to the next method.
fn arm_body(stack: &str) -> &str {
    fn_body(stack, "fn scan_silence_named_at(")
}

/// The body of the function that starts at `head`, up to the next method.
fn fn_body<'a>(src: &'a str, head: &str) -> &'a str {
    let start = src
        .find(head)
        .unwrap_or_else(|| panic!("`{head}` must exist"));
    let rest = &src[start..];
    let end = rest[1..]
        .find("\n    fn ")
        .or_else(|| rest[1..].find("\n    pub fn "))
        .map_or(rest.len(), |e| e + 1);
    &rest[..end]
}

/// Whether `src` calls `.note` followed by optional whitespace and `(`, under
/// any argument spelling. `.note_at_slot(` does not match (an `_` follows).
fn has_bare_note_call(src: &str) -> bool {
    src.match_indices(".note")
        .any(|(at, m)| src[at + m.len()..].trim_start().starts_with('('))
}

#[test]
fn the_bare_note_scan_matches_every_spelling_and_not_note_at_slot() {
    for bad in [
        "sweep.note(&report);",
        "sweep.note(&r);",
        "s.note (&*report)",
        "x.note(\n &report)",
    ] {
        assert!(has_bare_note_call(bad), "{bad}");
    }
    for good in ["sweep.note_at_slot(slot, &report);", "notes(", "note(x)"] {
        assert!(!has_bare_note_call(good), "{good}");
    }
}

#[test]
fn the_silence_arm_judges_reconcile_by_slot_not_by_probe() {
    let stack = strip_line_comments(&read("src/dhan_feed_stack.rs"));
    let body = arm_body(&stack);
    assert!(
        body.contains("scan_silence_slots("),
        "scan_silence_named_at must scan with scan_silence_slots so each report carries its slot"
    );
    assert!(
        body.contains("note_at_slot("),
        "scan_silence_named_at must judge reconcile with note_at_slot (slot cells, no probe)"
    );
    assert!(
        !has_bare_note_call(body),
        "scan_silence_named_at went back to one reconcile hash probe per report (a `.note(` call)"
    );
}

#[test]
fn the_sliced_silence_step_judges_reconcile_by_slot_not_by_probe() {
    let sweep = strip_line_comments(&read("src/dhan_feed_stack/silence_sweep.rs"));
    let production = sweep
        .split("#[cfg(test)]\nmod tests")
        .next()
        .unwrap_or(&sweep);
    let scan = fn_body(production, "fn scan_silence_sweep_slots(");
    assert!(
        scan.contains("scan_silence_slot_range("),
        "the silence step must scan its slice with scan_silence_slot_range"
    );
    assert!(
        scan.contains("note_at_slot("),
        "the silence step must judge reconcile with note_at_slot (slot cells, no probe)"
    );
    assert!(
        !has_bare_note_call(production),
        "the sliced silence sweep went back to one reconcile hash probe per report"
    );
    let step = fn_body(production, "pub fn step_silence_sweep(");
    assert!(
        step.contains("self.scan_silence_sweep_slots("),
        "step_silence_sweep must step through scan_silence_sweep_slots"
    );
    // The drain begins the sliced sweep and never runs the one-go scan.
    let stack = strip_line_comments(&read("src/dhan_feed_stack.rs"));
    let stack_prod = stack.split("#[cfg(test)]").next().unwrap_or(&stack);
    let drain = &stack_prod[stack_prod
        .find("async fn run_frame_drain(")
        .expect("run_frame_drain must exist")..];
    assert!(drain.contains("ingest.begin_silence_sweep(now_millis)"));
    assert!(
        !drain.contains("scan_silence_named("),
        "the drain went back to the one-go O(n) scan on its timer arm"
    );
}

#[test]
fn the_lane_attaches_the_book_with_the_builder_and_sizes_the_detector_at_the_cell_count() {
    let stack = strip_line_comments(&read("src/dhan_feed_stack.rs"));
    assert!(
        stack.contains(".with_subscribe_reconcile(Arc::clone(&subscribe_reconcile))"),
        "the production lane attaches the reconcile book through with_subscribe_reconcile"
    );
    assert!(
        stack.contains(".with_detector_capacity(AGGREGATOR_MAX_SLOTS)"),
        "the lane's detector must be sized at AGGREGATOR_MAX_SLOTS, the cell count"
    );
    let reconcile = strip_line_comments(&read("src/subscribe_reconcile.rs"));
    assert!(
        reconcile.contains("pub const RECONCILE_CELL_SLOTS: usize = AGGREGATOR_MAX_SLOTS;"),
        "the reconcile cell table must be sized from AGGREGATOR_MAX_SLOTS"
    );
}
