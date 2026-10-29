//! M2 (2026-10-26): one catch-up round, split at its WAL segments.
//!
//! # Why
//!
//! A round used to fold every frame it read, wait for the writers to ack all
//! of them, and only then settle ALL the staged segments. One rescue handed
//! back to the WAL anywhere in the round refused the whole round, and since
//! the round's segments sort first, the next boot re-read exactly the same
//! ones. Measured live: from 21 to 26 Oct 2026 every boot and every catch-up
//! re-read the same 5 segments (2,798,676 frames) and confirmed none of them,
//! while 1,696 segments (225 GB) stayed pinned.
//!
//! Split at segments, a round folds and acks one segment at a time and
//! settles each as soon as ITS rows are acked. A failure stops the round at
//! that segment; the segments before it are already retired. Every boot that
//! lands one segment shrinks the backlog.
//!
//! # Cost
//!
//! Cold path (boot and evening catch-up only). Building the split is one pass
//! over the round's frames with a cursor over its segments: O(frames +
//! segments), one `Vec` of at most one entry per segment. Each segment then
//! costs one flush and one ack wait instead of one per round.

use std::ffi::OsString;

/// One segment of a round: its file name (`None` when the mapping from
/// frames to segments was not available, which settles the whole staging
/// folder as before) and the `staged` index one past its last folded frame.
pub(super) type SegmentChunkEnd = (Option<OsString>, usize);

/// Closes every segment whose last batch frame comes before `batch_idx`,
/// recording `staged_len` as its end in the staged (folded) frames.
///
/// Called at the top of each iteration of the staging loop with that
/// frame's batch index, and once after the loop with `usize::MAX`. A
/// segment that returned no frame closes at the same staged index as the
/// one before it, so its chunk is empty and it settles with no fold.
/// O(1) amortised: `cursor` only moves forward.
pub(super) fn close_segments_before(
    segment_ends: &[(Option<OsString>, usize)],
    cursor: &mut usize,
    batch_idx: usize,
    staged_len: usize,
    out: &mut Vec<SegmentChunkEnd>,
) {
    while let Some((name, end)) = segment_ends.get(*cursor) {
        if *end > batch_idx {
            break;
        }
        out.push((name.clone(), staged_len));
        *cursor += 1;
    }
}

/// The round's chunks, checked: every staged frame must sit in exactly one
/// chunk and the ends must not go backwards. Anything else (no segment map,
/// a map that does not reach the last staged frame) returns ONE chunk with
/// no name, which folds the whole round and settles the whole staging
/// folder: the pre-M2 behaviour, never a partial settle on a wrong map.
pub(super) fn checked_chunks(
    chunks: Vec<SegmentChunkEnd>,
    staged_len: usize,
) -> Vec<SegmentChunkEnd> {
    let ordered = chunks.windows(2).all(|w| w[0].1 <= w[1].1);
    let named = chunks.iter().all(|(name, _)| name.is_some());
    let covers = chunks.last().is_some_and(|(_, end)| *end == staged_len);
    if ordered && named && covers {
        chunks
    } else {
        vec![(None, staged_len)]
    }
}

/// The gap indexes of `staged[start..end]`, rebased to that slice.
pub(super) fn chunk_gaps(staged_gaps: &[usize], start: usize, end: usize, out: &mut Vec<usize>) {
    out.clear();
    out.extend(
        staged_gaps
            .iter()
            .filter(|&&g| g >= start && g < end)
            .map(|&g| g - start),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(s: &str) -> Option<OsString> {
        Some(OsString::from(s))
    }

    /// Replays the staging loop's calls on a batch where `live[i]` says
    /// whether batch frame i is folded (staged).
    fn split(ends: &[(Option<OsString>, usize)], live: &[bool]) -> Vec<SegmentChunkEnd> {
        let mut out = Vec::new();
        let mut cursor = 0usize;
        let mut staged = 0usize;
        for (i, &l) in live.iter().enumerate() {
            close_segments_before(ends, &mut cursor, i, staged, &mut out);
            if l {
                staged += 1;
            }
        }
        close_segments_before(ends, &mut cursor, usize::MAX, staged, &mut out);
        checked_chunks(out, staged)
    }

    #[test]
    fn test_close_segments_before_maps_each_segment_to_its_staged_end() {
        // Three segments of 3, 0 and 2 frames; batch frame 1 is not folded.
        let ends = vec![(name("a"), 3), (name("b"), 3), (name("c"), 5)];
        let chunks = split(&ends, &[true, false, true, true, true]);
        assert_eq!(chunks, vec![(name("a"), 2), (name("b"), 2), (name("c"), 4)]);
    }

    #[test]
    fn test_checked_chunks_falls_back_to_one_whole_round_chunk_on_a_bad_map() {
        // No map at all.
        assert_eq!(checked_chunks(Vec::new(), 7), vec![(None, 7)]);
        // A map that stops short of the last staged frame.
        assert_eq!(checked_chunks(vec![(name("a"), 3)], 7), vec![(None, 7)]);
        // A segment with no file name.
        assert_eq!(checked_chunks(vec![(None, 7)], 7), vec![(None, 7)]);
        // Ends that go backwards.
        let back = vec![(name("a"), 5), (name("b"), 3), (name("c"), 7)];
        assert_eq!(checked_chunks(back, 7), vec![(None, 7)]);
    }

    #[test]
    fn test_chunk_gaps_rebases_to_the_slice() {
        let mut out = Vec::new();
        chunk_gaps(&[0, 4, 5, 9], 4, 9, &mut out);
        assert_eq!(out, vec![0, 1]);
    }

    proptest::proptest! {
        /// Every staged frame lands in exactly one chunk, chunks follow
        /// segment order, and a chunk never reaches past its own segment's
        /// last frame (the property the per-segment settle relies on: a
        /// settled segment's frames were all folded and acked before it).
        #[test]
        fn proptest_chunks_partition_the_staged_frames_by_segment(
            sizes in proptest::collection::vec(0usize..6, 1..8),
            mask in proptest::collection::vec(proptest::bool::ANY, 48),
        ) {
            let mut ends = Vec::new();
            let mut owner = Vec::new();
            let mut total = 0usize;
            for (k, &n) in sizes.iter().enumerate() {
                total += n;
                ends.push((name(&format!("s{k}")), total));
                owner.extend(std::iter::repeat_n(k, n));
            }
            let live: Vec<bool> = (0..total).map(|i| mask[i % mask.len()]).collect();
            let chunks = split(&ends, &live);
            proptest::prop_assert_eq!(chunks.len(), sizes.len());
            let mut staged_owner = Vec::new();
            for (i, &l) in live.iter().enumerate() {
                if l {
                    staged_owner.push(owner[i]);
                }
            }
            let mut start = 0usize;
            for (k, (_, end)) in chunks.iter().enumerate() {
                proptest::prop_assert!(*end >= start);
                for &o in &staged_owner[start..*end] {
                    proptest::prop_assert_eq!(o, k, "a chunk holds only its own segment's frames");
                }
                start = *end;
            }
            proptest::prop_assert_eq!(start, staged_owner.len());
        }
    }
}

/// M2 (2026-10-26): the replay pacer's "rows held" check, on a real writer
/// whose hand-off queue is full. Lives here, beside the other M2 replay
/// change, to keep `dhan_feed_stack.rs` under its size pin.
#[cfg(test)]
mod pacer_tests {
    use super::super::*;
    use tickvault_common::feed::Feed;

    /// One IDX_I ticker frame traded and received now.
    fn ticker_now(seq: u64) -> (u64, i64, WalEndpoint, bytes::Bytes) {
        let now_utc = chrono::Utc::now().timestamp();
        let now_ist = now_utc + i64::from(tickvault_common::constants::IST_UTC_OFFSET_SECONDS);
        let mut p = [0u8; 16];
        p[0] = 2;
        p[1] = 16;
        p[4..8].copy_from_slice(&13u32.to_le_bytes());
        p[8..12].copy_from_slice(&24_000.0_f32.to_le_bytes());
        p[12..16].copy_from_slice(&u32::try_from(now_ist).unwrap_or(0).to_le_bytes());
        (
            seq,
            now_utc * 1_000_000_000,
            WalEndpoint::MainFeed,
            bytes::Bytes::copy_from_slice(&p),
        )
    }

    /// Live, 19 to 26 Oct 2026: `pace_waits` = 0 on all eight boot replays,
    /// yet the writer width-capped and rescued rows on every one. The pacer
    /// read `LiveIngest::pending_rows`, which `flush_ticks` zeroes even when
    /// a full queue hands the rows back, so it never saw them held.
    #[test]
    fn test_regression_m2_replay_pacer_sees_rows_a_full_queue_gave_back() {
        let (producer, _sink, _rx) = TickWriter::for_test(Feed::Dhan).split_for_offload();
        let mut ingest = LiveIngest::new(producer, 4);
        // Fill the hand-off queue (nothing consumes `_rx`), then one more.
        let depth = tickvault_storage::tick_persistence::FLUSH_QUEUE_DEPTH;
        for seq in 1..=(depth as u64 + 1) {
            let _outcome = refold_wal_frames(&mut ingest, &[ticker_now(seq)], &[], None);
            assert!(ingest.writer.pending() > 0, "the frame appended a row");
            ingest.flush_ticks();
        }
        assert!(
            ingest.writer.pending() > 0,
            "the full queue gave the last batch back to the writer"
        );
        assert_eq!(
            ingest.pending_rows(),
            0,
            "the lane's own counter reads 0 here: the value the old pacer trusted"
        );
        assert!(
            replay_rows_held(&ingest, false) > 0,
            "the pacer must see the rows the queue gave back, or it never waits"
        );
        assert!(replay_rows_held(&ingest, true) > 0);
    }
}
