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
//! HONEST LIMIT: a text scan of one function body and two constants.

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
    let start = stack
        .find("fn scan_silence_named_at(")
        .expect("scan_silence_named_at must exist in dhan_feed_stack.rs");
    let rest = &stack[start..];
    let end = rest[1..]
        .find("\n    fn ")
        .or_else(|| rest[1..].find("\n    pub fn "))
        .map_or(rest.len(), |e| e + 1);
    &rest[..end]
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
        !body.contains(".note(&report)"),
        "scan_silence_named_at went back to one reconcile hash probe per report"
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
