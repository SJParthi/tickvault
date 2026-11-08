//! Plan item PR RD, RD-3 (2026-11-08): a depth batch that landed nowhere
//! during a replay no longer holds its WAL segment's ticks. The depth writer
//! marks the lost range in the deferred-depth table; the replay confirm may
//! then archive the segment, and these tests prove the archive is safe: the
//! archive prune keeps the segment and the after-close pass's work list finds
//! it there. A child of `ws_frame_spill::tests`, kept in its own file so the
//! parent stays under its size pin; it reuses the parent's test helpers
//! (`tmp_dir`, `write_wm_segment`, `wm_seq`, `backdate`).

use super::*;
use crate::wal_deferred_depth::{DEFERRED_BUCKET_SHIFT, SHED_DEPTH_ALL};

/// Seconds past the `wm_seq` base for this file's segments: about 580 days
/// on, far from every other test's marks in this binary.
const RD3_SECS: u64 = 50_000_000;

/// Two archived, aged segments: the one whose depth was lost, and the next
/// one, which bounds it.
fn archived_pair(tag: &str, now: SystemTime) -> (PathBuf, PathBuf, PathBuf) {
    let dir = tmp_dir(tag);
    let archive = dir.join(ARCHIVE_SUBDIR);
    std::fs::create_dir_all(&archive).unwrap();
    let lost = write_wm_segment(&archive, RD3_SECS, 3, WalEndpoint::MainFeed);
    let next = write_wm_segment(&archive, RD3_SECS + 1_000, 3, WalEndpoint::MainFeed);
    for p in [&lost, &next] {
        backdate(p, now, 400_000);
    }
    (dir, lost, next)
}

// Regression: 2026-11-08 (PR RD, RD-3). A depth rescue failure during the 7 Nov
// and Muhurat replays failed the whole ack, so the same 5 segments were
// re-offered every boot. The fix lets the segment be archived for its ticks;
// this pins that the archive then keeps it for the depth. Bite: drop the
// `defer_lost_range` call in `note_rescue_outcome_depth` and the mark
// assertion and the prune-keeps assertion fail.
#[test]
fn test_regression_rd3_a_lost_depth_range_keeps_its_archived_segment_and_reaches_the_pass() {
    let now = SystemTime::now();
    let (lo, hi) = (wm_seq(RD3_SECS), wm_seq(RD3_SECS + 2));

    // Control: with no marks the same prune deletes both segments.
    let (control, c_lost, c_next) = archived_pair("rd3-control", now);
    let out = prune_archived_segments_deferred_at(
        &control,
        172_800,
        0,
        now,
        Some(&crate::wal_deferred_depth::DeferredDepthSnapshot::default()),
        false,
        UploadGate::NotRequired,
    );
    assert!(!c_lost.exists() && !c_next.exists(), "{out:?}");
    let _ = std::fs::remove_dir_all(&control);

    // The production failure arm: ILP write and spill rescue both failed.
    let wm = crate::wal_applied_watermark::applied_watermark();
    let before = wm.unlanded_snapshot();
    crate::depth_persistence::note_rescue_outcome_depth(false, (lo, hi), true);
    let after = wm.unlanded_snapshot();
    assert!(after.depth > before.depth, "the loss is counted");
    assert!(
        after.depth_deferred > before.depth_deferred,
        "and counted deferred, so the replay confirm may archive the ticks"
    );

    // `lost_depth_marks()` is the process table in production; under unit
    // tests it is a separate one, so this binary's other prunes stay clean.
    let marks = crate::wal_deferred_depth::lost_depth_marks().snapshot();
    let bucket = lo >> DEFERRED_BUCKET_SHIFT;
    assert!(
        marks
            .marked_buckets()
            .iter()
            .any(|&(id, kinds)| id == bucket && kinds == SHED_DEPTH_ALL),
        "the lost range's bucket is marked for both depth kinds"
    );

    let (dir, lost, _next) = archived_pair("rd3-deferred", now);
    let out = prune_archived_segments_deferred_at(
        &dir,
        172_800,
        0,
        now,
        Some(&marks),
        false,
        UploadGate::NotRequired,
    );
    assert!(
        lost.exists(),
        "the archive prune keeps the deferred segment: {out:?}"
    );
    assert!(out.deferred_kept >= 1);

    // The after-close pass's work list finds the segment in `archive/`.
    let spans = deferred_segment_spans(&dir).expect("listable");
    let span = spans
        .iter()
        .find(|s| s.path == lost)
        .expect("the archived segment is listed");
    assert!(marks.segment_is_deferred(span.lo, span.hi));
    let found = segments_for_bucket(&spans, bucket);
    assert!(found.paths.contains(&lost), "{found:?}");
    assert!(!found.touches_open && !found.uncertain, "{found:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// A sequence-less depth batch (range start 0) cannot be deferred: it is
/// counted as an undeferred loss, so the replay confirm still holds.
#[test]
fn test_rd3_note_rescue_outcome_depth_does_not_defer_a_sequence_less_loss() {
    let wm = crate::wal_applied_watermark::applied_watermark();
    let before = wm.unlanded_snapshot();
    crate::depth_persistence::note_rescue_outcome_depth(false, (0, 0), true);
    let after = wm.unlanded_snapshot();
    assert!(after.depth > before.depth);
    assert_eq!(
        after.depth.saturating_sub(before.depth),
        after
            .depth_deferred
            .saturating_sub(before.depth_deferred)
            .saturating_add(1),
        "exactly this loss is undeferred (other tests may add deferred ones)"
    );
}
