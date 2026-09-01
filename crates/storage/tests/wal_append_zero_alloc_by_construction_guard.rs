//! The capture-at-receipt append must stay allocation-free BY CONSTRUCTION.
//!
//! # Why a source guard and not a DHAT gate
//!
//! `WsFrameSpill::append_with_seq_at` is the FIRST link in the no-loss chain:
//! it puts a raw frame on the durable path BEFORE anything parses or
//! broadcasts it, at roughly 5,000 frames/sec. Sixteen DHAT gates exist in
//! this workspace and, as of 2026-09-01, NONE of them covered it.
//!
//! A DHAT gate is the obvious instrument and it is the wrong one here. The
//! append hands the record to a writer THREAD through a bounded channel, and
//! `dhat` measures the whole process heap — so a gate would be measuring the
//! writer's file I/O concurrently with the caller, on every attempt. That is
//! not a phantom allocation the shared `measure_with_phantom_retry` helper can
//! retry away; it recurs every run and scales with the workload, which is
//! exactly the shape that makes a gate flaky. A flaky gate gets `#[ignore]`d,
//! and then it enforces nothing.
//!
//! Isolating the caller would need a constructor that keeps the channel
//! receiver alive without spawning a writer. That seam does not exist, and
//! manufacturing a `pub` test-only constructor on production code is a hazard
//! this repository has already recorded once (`DepthWriter::for_test` was
//! `pub` and set the free-space floor to 0, which production then used as a
//! swap placeholder).
//!
//! So this pins the property directly instead of measuring a proxy for it.
//! It is deterministic, has no thread noise, and cannot be defeated by a
//! runner's timing.

use std::path::PathBuf;

fn spill_source() -> String {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/ws_frame_spill.rs");
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("cannot read {}: {e}", p.display()))
}

/// The append's HAPPY path: from the fn signature to the `Spilled` arm.
///
/// Deliberately excludes the drop arms below it — those run only when a frame
/// is already being lost, and they legitimately format an `error!` naming the
/// code that pages. Gating them would be wrong.
fn happy_path_region(src: &str) -> String {
    let start = src
        .find("pub fn append_with_seq_at")
        .expect("append_with_seq_at must exist — it is the durable floor's entry point");
    let rest = &src[start..];
    let end = rest
        .find("Ok(()) => AppendOutcome::Spilled,")
        .expect("the Spilled arm must exist — it is the happy path this guard bounds");
    rest[..end].to_string()
}

#[test]
fn the_wal_append_happy_path_allocates_nothing() {
    let src = spill_source();
    let region = happy_path_region(&src);

    // Anti-vacuity floor: an empty or truncated region would let every
    // assertion below pass having scanned nothing.
    assert!(
        region.len() > 200,
        "the scanned region is only {} bytes — the function moved or the \
         anchors changed; refusing to pass vacuously",
        region.len()
    );
    assert!(
        region.contains("WalRecord {") && region.contains("try_send"),
        "the scanned region no longer contains the record construction and the \
         send — the anchors have drifted and this guard is measuring the wrong \
         code"
    );

    // Allocation-shaped tokens. `frame.into()` is a MOVE for `Bytes` and for
    // `Vec<u8>`; a copy would appear as one of these.
    const BANNED: &[&str] = &[
        "to_string()",
        "format!",
        "Vec::new",
        "vec![",
        "to_vec()",
        ".clone()",
        "String::from",
        ".collect()",
        "Box::new",
        "String::new",
    ];
    let mut found = Vec::new();
    for tok in BANNED {
        if region.contains(tok) {
            found.push(*tok);
        }
    }
    assert!(
        found.is_empty(),
        "WsFrameSpill::append_with_seq_at's HAPPY path contains {found:?}.\n\
         \n\
         This is the capture-at-receipt write — the first link in the no-loss \
         chain, at ~5,000 frames/sec. An allocation here is one per frame on \
         the path whose entire purpose is that a frame survives a crash. The \
         drop arms BELOW the `Spilled` arm are deliberately outside this \
         region and may format freely; they run only when a frame is already \
         lost."
    );
}

#[test]
fn the_frame_parameter_stays_a_move_not_a_copy() {
    let src = spill_source();
    let region = happy_path_region(&src);

    assert!(
        region.contains("frame: impl Into<Bytes>"),
        "append_with_seq_at must take `impl Into<Bytes>`. Changing it to \
         `&[u8]` or `Vec<u8>`-by-value-then-copy would turn every frame into \
         a heap copy on the durable floor — invisible in a row count, in a \
         test assertion, and in a review diff of the call site."
    );
    assert!(
        region.contains("frame: frame.into()"),
        "the frame must reach the record via `.into()`, which is a MOVE for \
         both `Bytes` and `Vec<u8>`. Any other construction should be \
         re-justified here before this assertion is changed."
    );
}

/// Bite-proof for the scanner itself.
///
/// A guard whose extractor silently returns the wrong slice reports green for
/// the wrong reason. This proves the region really is bounded at the `Spilled`
/// arm — that the drop arms, which DO format, are outside it.
#[test]
fn the_region_stops_before_the_drop_arms_that_legitimately_format() {
    let src = spill_source();
    let region = happy_path_region(&src);

    assert!(
        !region.contains("WsSpill02FrameDropped"),
        "the scanned region reaches into the drop arm — the extractor is \
         bounding the wrong slice, and the banned-token assertion above would \
         fire on an `error!` that is entirely correct"
    );
    assert!(
        src.contains("WsSpill02FrameDropped"),
        "the drop arm must still exist somewhere in the file — if it is gone, \
         a dropped frame no longer pages"
    );
}
