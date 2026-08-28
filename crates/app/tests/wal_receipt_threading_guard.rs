//! The persisted WAL receipt must actually REACH the replay fold.
//!
//! # What this exists to stop
//!
//! On 2026-08-28 the WAL record format grew to `TVW3`, adding an 8-byte
//! `received_at_nanos` to every frame, specifically so boot replay would stop
//! re-deriving an arrival instant it had thrown away.
//!
//! The format shipped. The plumbing did not. `main.rs` reduced each
//! `ReplayedFrame` to `(frame_seq, frame)` and dropped the receipt on the
//! floor, and `refold_wal_frames` then hardcoded `WAL_RECEIPT_UNKNOWN_NANOS`
//! for every frame — while `ReplayedFrame::received_at_nanos`'s own doc said,
//! verbatim, "A replay consumer MUST prefer this over a fresh clock read."
//! A workspace grep for the field returned ZERO consumers: written to disk on
//! every frame, discarded at its only reader.
//!
//! That is not a wasted field. `tick_persistence::row_timestamp_ist_nanos`
//! DERIVES a never-traded tick's `ts` from the receipt, and `ts` is the first
//! column of the `ticks` DEDUP key — so the live write and the replay write of
//! the SAME observation carried different keys and became two rows instead of
//! one, across the ~950k sentinel-timestamp rows a session carries,
//! compounding on every replay.
//!
//! # Why a source scan rather than a behavioural test
//!
//! The round-trip itself is already proven behaviourally
//! (`ws_frame_spill::tvw3_roundtrip_preserves_received_at`), and the fold's
//! use of a receipt is proven in `multi_tf_aggregator`. Neither could see the
//! defect, because the break was a value being dropped BETWEEN them — in
//! plumbing that has no return value to assert on. So this pins the two hops
//! the behavioural tests structurally cannot reach.

#![cfg(test)]

use std::path::PathBuf;

fn read_src(rel: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(rel);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {} failed: {e}", path.display()))
}

/// Strips `//` line comments so a rationale block naming a symbol can never
/// satisfy an assertion about the CODE using it — the exact way a source-scan
/// guard goes quietly vacuous.
fn code_only(src: &str) -> String {
    src.lines()
        .map(|l| l.split_once("//").map_or(l, |(before, _)| before))
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn boot_carries_the_wal_receipt_into_the_replay_vector() {
    let code = code_only(&read_src("src/main.rs"));

    assert!(
        code.contains("rec.received_at_nanos"),
        "main.rs must carry each replayed frame's PERSISTED receipt into the \
         replay vector. Dropping it here is invisible everywhere else: the WAL \
         round-trip test still passes, the fold test still passes, and the \
         value simply never arrives."
    );

    let vec_decl = code
        .find("ws_wal_replay_live_feed: Vec<")
        .expect("the live-feed replay vector must exist");
    let decl_line = &code[vec_decl
        ..code[vec_decl..]
            .find('\n')
            .map_or(code.len(), |n| vec_decl + n)];
    assert!(
        decl_line.contains("i64"),
        "the replay vector must carry the receipt alongside the frame; found: {decl_line}"
    );
}

#[test]
fn the_replay_fold_uses_the_per_frame_receipt_not_the_unknown_sentinel() {
    let src = read_src("src/dhan_feed_stack.rs");
    let body = src
        .split("pub fn refold_wal_frames")
        .nth(1)
        .expect("refold_wal_frames must exist");
    // Bound the scan to the function body, so a later function's use of the
    // sentinel cannot make this pass or fail by accident.
    let body = &body[..body.find("\n}\n").map_or(body.len(), |n| n)];
    let code = code_only(body);

    assert!(
        code.contains("wal_received_at_nanos"),
        "the fold must destructure the per-frame receipt out of the replay tuple"
    );
    assert!(
        code.contains("dispatch_frame(") && code.contains("*wal_received_at_nanos"),
        "and must hand THAT receipt to dispatch_frame -- passing a constant \
         here is what made replay bucket differently from the live write of \
         the same bytes"
    );
    assert!(
        !code.contains("WAL_RECEIPT_UNKNOWN_NANOS"),
        "the fold must not substitute the UNKNOWN sentinel for a receipt it \
         actually has. v1/v2 records already arrive carrying the sentinel from \
         the reader, which is correct for them and only for them."
    );
}
