//! Frame-base ratchet and capped record-probe tests (2026-10-20, plan item
//! PR W review round 3), and the bucket-boundary reorder tests (round 4, A:
//! a frame shed just below a `2^38` boundary inside a segment whose first
//! record is just above it). A child of `ws_frame_spill::tests`, kept in its
//! own file so the parent stays under its size pin, and the round-5 mirror
//! (a frame shed just ABOVE a boundary, written as the last record of a
//! segment whose range ends just below it); it reuses the parent's
//! test helpers (`tmp_dir`, `write_wm_segment`, `backdate`,
//! `all_applied_through`, `mark_unapplied`, `deferred_at`).

use super::*;

/// 2026-10-20 (PR W review round 3, M1): after the ratchet, a mint is
/// above the ratcheted value even though the wall clock reads far
/// behind it (the ratchet target here is an hour ahead of the clock, the
/// shape a backward clock step leaves).
#[test]
fn test_ratchet_frame_seq_to_makes_every_later_mint_greater() {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX))
        .unwrap_or(0);
    let ahead = round_up_to_frame_base(now + 3_600 * 1_000_000_000);
    assert!(ratchet_frame_seq_to(ahead) >= ahead);
    assert!(current_frame_seq() >= ahead, "the counter holds the target");
    let minted = next_frame_seq();
    assert!(minted > ahead, "{minted} must be above the ratchet {ahead}");
    // Never lowers it.
    let before = current_frame_seq();
    assert!(ratchet_frame_seq_to(1 << PACKET_INDEX_BITS) >= before);
    assert!(current_frame_seq() >= before);
}

#[test]
fn test_round_up_to_frame_base_rounds_up_and_saturates() {
    let base = 1u64 << PACKET_INDEX_BITS;
    assert_eq!(round_up_to_frame_base(0), 0);
    assert_eq!(round_up_to_frame_base(1), base);
    assert_eq!(round_up_to_frame_base(base), base, "aligned stays");
    assert_eq!(round_up_to_frame_base(base + 1), 2 * base);
    let top = (u64::MAX >> PACKET_INDEX_BITS) << PACKET_INDEX_BITS;
    assert_eq!(round_up_to_frame_base(u64::MAX), top, "saturates");
    assert_eq!(round_up_to_frame_base(top), top);
}

/// 2026-10-20 (PR W review round 3, L9): a header whose frame length is
/// past the cap is refused before anything is allocated for it.
#[test]
fn test_record_at_capped_refuses_a_length_past_the_cap() {
    let dir = tmp_dir("record-at-capped");
    let p = write_wm_segment(&dir, 42, 1, WalEndpoint::MainFeed);
    let len = std::fs::metadata(&p).unwrap().len();
    let mut f = File::open(&p).unwrap();
    assert!(matches!(
        record_at_capped(&mut f, 0, usize::MAX),
        RecordProbe::Intact { record_len, .. } if record_len == len
    ));
    assert!(matches!(record_at(&mut f, 0), RecordProbe::Intact { .. }));
    assert_eq!(record_at_capped(&mut f, 0, 0), RecordProbe::End);
    let _ = std::fs::remove_dir_all(&dir);
}

/// 2026-10-20 (PR W review round 4): `wal_unapplied_ranges` skips a segment
/// file shorter than any record as holding no frame. Its copy of the
/// smallest record length must equal the writer's (this file can read the
/// private constant; the parent has no room under its size pin to export
/// it), and must be the smallest of the four versions.
#[test]
fn test_wal_smallest_record_bytes_matches_the_writer() {
    let smallest = [
        crate::ws_frame_spill::WAL_MIN_RECORD_V1,
        crate::ws_frame_spill::WAL_MIN_RECORD_V2,
        crate::ws_frame_spill::WAL_MIN_RECORD_V3,
        crate::ws_frame_spill::WAL_MIN_RECORD_V4,
    ]
    .into_iter()
    .min()
    .unwrap_or(0);
    assert_eq!(
        crate::wal_unapplied_ranges::WAL_SMALLEST_RECORD_BYTES,
        smallest as u64
    );
}

/// Writes one segment holding a record at each of `seqs`, in the given
/// (possibly reordered) order, named by the FIRST record's sequence as
/// the writer names it.
fn write_seq_segment(dir: &Path, seqs: &[u64], endpoint: WalEndpoint) -> PathBuf {
    let mut bytes = Vec::new();
    for &seq in seqs {
        bytes.extend_from_slice(&encode_v4_record(
            WsType::LiveFeed,
            seq,
            7,
            endpoint,
            &[0xAB; 32],
        ));
    }
    let path = dir.join(format!("ws-frames-{:020}.wal", seqs[0]));
    std::fs::write(&path, bytes).unwrap();
    path
}

/// PR W review round 4 (A): the shed frame `s` sits just below a `2^38`
/// bucket boundary, inside segment B whose first record is just above
/// it (reordered across sockets). Returns `(s, [A, B, C, D])`; C is
/// applied and clear of every mark, D is the newest.
fn write_boundary_segments(dir: &Path) -> (u64, [PathBuf; 4]) {
    use crate::wal_applied_watermark::{REPLAY_REORDER_SLACK_SEQ, UNAPPLIED_BUCKET_SHIFT};
    let boundary = ((wm_seq(1_000) >> UNAPPLIED_BUCKET_SHIFT) + 1) << UNAPPLIED_BUCKET_SHIFT;
    let frame = 1u64 << PACKET_INDEX_BITS;
    let s = (boundary - REPLAY_REORDER_SLACK_SEQ / 2) >> PACKET_INDEX_BITS << PACKET_INDEX_BITS;
    let b = boundary + frame;
    let c = (boundary + 10 * REPLAY_REORDER_SLACK_SEQ) >> PACKET_INDEX_BITS << PACKET_INDEX_BITS;
    let d = (boundary + 20 * REPLAY_REORDER_SLACK_SEQ) >> PACKET_INDEX_BITS << PACKET_INDEX_BITS;
    let a_seg = write_wm_segment(dir, 0, 3, WalEndpoint::MainFeed);
    let b_seg = write_seq_segment(dir, &[b, s, b + frame], WalEndpoint::MainFeed);
    let c_seg = write_seq_segment(dir, &[c, c + frame], WalEndpoint::MainFeed);
    let d_seg = write_seq_segment(dir, &[d, d + frame], WalEndpoint::MainFeed);
    (s, [a_seg, b_seg, c_seg, d_seg])
}

/// PR W review round 4 (A): a shed bucket mark on `s` keeps segment B,
/// whose first record is just above the bucket boundary, from both
/// prunes. Before, B's range started in the next bucket and B read
/// applied.
#[test]
fn test_regression_prune_keeps_a_segment_holding_a_frame_shed_below_the_boundary() {
    let dir = tmp_dir("wm-prune-boundary");
    let now = SystemTime::now();
    let (s, [a, b, c, d]) = write_boundary_segments(&dir);
    for p in [&a, &b, &c, &d] {
        backdate(p, now, 400_000);
    }
    let mut snap = all_applied_through(wm_seq(9_000));
    mark_unapplied(&mut snap, s);
    let age = prune_active_segments_at(&dir, 172_800, u64::MAX, now, Some(&snap));
    assert!(b.exists(), "B holds the shed frame: kept");
    assert!(a.exists(), "A's range spans the shed bucket: kept");
    assert!(!c.exists(), "C is applied and clear: expired");
    assert!(d.exists(), "D is the newest (unknown): kept");
    assert_eq!(age.deleted, 1);
    let bytes = prune_active_segments_at(&dir, 172_800, 0, now, Some(&snap));
    assert_eq!(bytes.size_deleted, 0);
    assert!(b.exists(), "the byte pass keeps B as well");
    let _ = std::fs::remove_dir_all(&dir);
}

/// PR W review round 4 (A): the same for a deferred-depth mark: the prune
/// keeps B, and the after-close pass's segment lookup names B for the
/// shed bucket so its frame is written back before the mark clears.
#[test]
fn test_regression_deferred_mark_reaches_a_segment_starting_above_the_boundary() {
    // The lookup is in the deferred table's own bucket units (PR W review
    // round 5: this read `UNAPPLIED_BUCKET_SHIFT`, equal today only).
    use crate::wal_deferred_depth::DEFERRED_BUCKET_SHIFT;
    let dir = tmp_dir("deferred-boundary");
    let now = SystemTime::now();
    let (s, [a, b, c, d]) = write_boundary_segments(&dir);
    for p in [&a, &b, &c, &d] {
        backdate(p, now, 400_000);
    }
    let spans = deferred_segment_spans(&dir).expect("listable");
    let found = segments_for_bucket(&spans, s >> DEFERRED_BUCKET_SHIFT);
    assert!(
        found.paths.contains(&b),
        "the after-close pass must read B for the shed bucket"
    );
    assert!(!found.paths.contains(&c), "C is more than the slack above");
    let applied = all_applied_through(wm_seq(9_000));
    let deferred = deferred_at(s);
    let age = prune_active_segments_deferred_at(
        &dir,
        172_800,
        u64::MAX,
        now,
        Some(&applied),
        Some(&deferred),
        false,
        UploadGate::NotRequired,
    );
    assert!(b.exists(), "B holds shed depth: kept");
    assert!(!c.exists(), "C is clear of the mark: expired");
    assert_eq!(age.deferred_kept, 2, "A and B");
    let _ = std::fs::remove_dir_all(&dir);
}

/// PR W review round 5, the mirror of [`write_boundary_segments`]: the shed
/// frame `f` sits just ABOVE a `2^38` boundary and is segment A's last
/// record, and B's first record `g` sits just below the boundary (minted
/// earlier on another socket, written later). A's range `[a, g − 1]` ends in
/// the bucket below `f`'s. Returns `(f, [Z, A, B, C, D])`; Z is older and
/// clear, C sits in the next bucket and is clear, D is the newest.
fn write_mirror_boundary_segments(dir: &Path) -> (u64, [PathBuf; 5]) {
    use crate::wal_applied_watermark::{REPLAY_REORDER_SLACK_SEQ, UNAPPLIED_BUCKET_SHIFT};
    let slack = REPLAY_REORDER_SLACK_SEQ;
    let bucket = 1u64 << UNAPPLIED_BUCKET_SHIFT;
    let boundary = ((wm_seq(1_000) >> UNAPPLIED_BUCKET_SHIFT) + 1) << UNAPPLIED_BUCKET_SHIFT;
    let frame = 1u64 << PACKET_INDEX_BITS;
    let base = |x: u64| x >> PACKET_INDEX_BITS << PACKET_INDEX_BITS;
    let f = boundary + frame;
    let g = base(boundary - slack / 2);
    let a = base(boundary - 5 * slack);
    let c = base(boundary + bucket + 10 * slack);
    let d = base(boundary + bucket + 20 * slack);
    assert!(g < boundary && f - g < slack);
    let z_seg = write_wm_segment(dir, 0, 3, WalEndpoint::MainFeed);
    let a_seg = write_seq_segment(dir, &[a, a + frame, f], WalEndpoint::MainFeed);
    let b_seg = write_seq_segment(dir, &[g, g + frame], WalEndpoint::MainFeed);
    let c_seg = write_seq_segment(dir, &[c, c + frame], WalEndpoint::MainFeed);
    let d_seg = write_seq_segment(dir, &[d, d + frame], WalEndpoint::MainFeed);
    (f, [z_seg, a_seg, b_seg, c_seg, d_seg])
}

/// PR W review round 5: a shed bucket mark on `f` keeps segment A, whose
/// range ends just below the bucket boundary, from both prunes. Before, A's
/// bucket read stopped at the bucket below `f`'s and A read applied.
#[test]
fn test_regression_prune_keeps_a_segment_whose_last_record_is_shed_above_the_boundary() {
    let dir = tmp_dir("wm-prune-mirror-boundary");
    let now = SystemTime::now();
    let (f, [z, a, b, c, d]) = write_mirror_boundary_segments(&dir);
    for p in [&z, &a, &b, &c, &d] {
        backdate(p, now, 400_000);
    }
    let mut snap = all_applied_through(wm_seq(9_000));
    mark_unapplied(&mut snap, f);
    let age = prune_active_segments_at(&dir, 172_800, u64::MAX, now, Some(&snap));
    assert!(
        a.exists(),
        "A holds the shed frame as its last record: kept"
    );
    assert!(b.exists(), "B's range spans the shed bucket: kept");
    assert!(!z.exists(), "Z is applied and clear: expired");
    assert!(!c.exists(), "C is applied and clear: expired");
    assert!(d.exists(), "D is the newest (unknown): kept");
    assert_eq!(age.deleted, 2);
    let bytes = prune_active_segments_at(&dir, 172_800, 0, now, Some(&snap));
    assert_eq!(bytes.size_deleted, 0);
    assert!(a.exists(), "the byte pass keeps A as well");
    let _ = std::fs::remove_dir_all(&dir);
}

/// PR W review round 5: the same for a deferred-depth mark: the prune keeps
/// A, and the after-close pass's segment lookup names A for the shed bucket
/// so its frame is written back before the mark clears.
#[test]
fn test_regression_deferred_mark_reaches_a_segment_ending_below_the_boundary() {
    use crate::wal_deferred_depth::DEFERRED_BUCKET_SHIFT;
    let dir = tmp_dir("deferred-mirror-boundary");
    let now = SystemTime::now();
    let (f, [z, a, b, c, d]) = write_mirror_boundary_segments(&dir);
    for p in [&z, &a, &b, &c, &d] {
        backdate(p, now, 400_000);
    }
    let spans = deferred_segment_spans(&dir).expect("listable");
    let found = segments_for_bucket(&spans, f >> DEFERRED_BUCKET_SHIFT);
    assert!(
        found.paths.contains(&a),
        "the after-close pass must read A for the shed bucket"
    );
    assert!(found.paths.contains(&b), "B spans the bucket");
    assert!(
        !found.paths.contains(&z),
        "Z ends more than the slack below"
    );
    assert!(!found.paths.contains(&c), "C starts in the next bucket");
    let applied = all_applied_through(wm_seq(9_000));
    let deferred = deferred_at(f);
    let age = prune_active_segments_deferred_at(
        &dir,
        172_800,
        u64::MAX,
        now,
        Some(&applied),
        Some(&deferred),
        false,
        UploadGate::NotRequired,
    );
    assert!(a.exists(), "A holds shed depth: kept");
    assert!(!c.exists(), "C is clear of the mark: expired");
    assert_eq!(age.deferred_kept, 2, "A and B");
    let _ = std::fs::remove_dir_all(&dir);
}

/// PR W review round 5: an unreadable segment is placed between its
/// readable neighbours with the same reach, so a bucket within one slack
/// above the next readable segment's first record is reported uncertain.
#[test]
fn test_segments_for_bucket_places_an_unreadable_segment_with_the_reach() {
    use crate::wal_applied_watermark::REPLAY_REORDER_SLACK_SEQ;
    use crate::wal_deferred_depth::DEFERRED_BUCKET_SHIFT;
    let boundary = ((wm_seq(1_000) >> DEFERRED_BUCKET_SHIFT) + 1) << DEFERRED_BUCKET_SHIFT;
    let span = |lo: u64, hi: Option<u64>, name: &str| SegmentSpan {
        path: PathBuf::from(name),
        lo,
        hi,
        is_open: false,
    };
    let next = boundary - REPLAY_REORDER_SLACK_SEQ / 2;
    let spans = [
        span(boundary - 9 * REPLAY_REORDER_SLACK_SEQ, None, "p"),
        span(0, None, "u"),
        span(next, Some(next + 4 * REPLAY_REORDER_SLACK_SEQ), "n"),
    ];
    let found = segments_for_bucket(&spans, boundary >> DEFERRED_BUCKET_SHIFT);
    assert!(
        found.uncertain,
        "the unreadable segment can hold the bucket"
    );
}

/// Plan item W2 (2026-10-27): a collision lost the shed frame in segment A.
/// The overflow height is the top of that frame's bucket, so A is kept,
/// while B and C (applied, aged, with a verified copy, and wholly above the
/// height) age out. Before W2 every range read as deferred while the table
/// was overflowed, and B and C were pinned until an after-close pass cleared
/// the flag.
#[test]
fn test_prune_deletes_an_applied_uploaded_segment_above_the_overflow_height() {
    use crate::wal_deferred_depth::{
        DEFERRED_BUCKET_SHIFT, DEFERRED_BUCKETS, DeferredDepth, ShedDepth,
    };
    let dir = tmp_dir("deferred-overflow-height");
    let now = SystemTime::now();
    let a = write_wm_segment(&dir, 0, 3, WalEndpoint::MainFeed);
    let b = write_wm_segment(&dir, 1_000, 3, WalEndpoint::MainFeed);
    let c = write_wm_segment(&dir, 2_000, 3, WalEndpoint::MainFeed);
    let d = write_wm_segment(&dir, 3_000, 3, WalEndpoint::MainFeed);
    for p in [&a, &b, &c] {
        backdate(p, now, 400_000);
        mark_uploaded(&dir, p, len_of(p));
    }
    // An older bucket holds the slot, so the shed in A cannot be recorded.
    let lost = wm_seq(1);
    let marks = DeferredDepth::new_for_tests();
    marks.note_shed(
        lost - ((DEFERRED_BUCKETS as u64) << DEFERRED_BUCKET_SHIFT),
        ShedDepth::Inline,
    );
    marks.note_shed(lost, ShedDepth::Inline);
    let deferred = marks.snapshot();
    let height = deferred.overflow_height().expect("a known height");
    assert!(height < wm_seq(1_000), "B starts above the height");
    let markers = crate::raw_frame_upload::markers_dir(&dir);
    let out = prune_active_segments_deferred_at(
        &dir,
        172_800,
        u64::MAX,
        now,
        Some(&all_applied_through(wm_seq(5_000))),
        Some(&deferred),
        false,
        UploadGate::Required {
            markers_dir: &markers,
        },
    );
    assert!(a.exists(), "A holds the frame the overflow lost: kept");
    assert!(!b.exists(), "B is above the height: it ages out");
    assert!(!c.exists(), "C is above the height: it ages out");
    assert!(d.exists(), "D is the newest: kept");
    assert_eq!(out.deleted, 2);
    assert_eq!(out.deferred_kept, 1, "only A");
    let _ = std::fs::remove_dir_all(&dir);
}
