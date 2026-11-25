//! Z11a (2026-10-02) regressions for the WAL reader: a walk resyncs past
//! mid-segment damage and recovers the intact records after it, and (plan
//! item LF-8) says whether it STOPPED early (`SegmentRead::stopped_early`).
//! A child of `ws_frame_spill::tests`, kept in its own file so the parent
//! stays under its size pin; it reuses the parent's test helpers
//! (`tmp_dir`, `encode_v4_record`).

use super::*;

/// A segment of v4 records, sequence `k + 1` and a payload of `fill(k)`;
/// returns the bytes and each record's start offset.
fn z11_segment(payloads: &[Vec<u8>]) -> (Vec<u8>, Vec<usize>) {
    let mut bytes = Vec::new();
    let mut offsets = Vec::new();
    for (k, p) in payloads.iter().enumerate() {
        offsets.push(bytes.len());
        bytes.extend_from_slice(&encode_v4_record(
            WsType::LiveFeed,
            k as u64 + 1,
            1_000 + k as i64,
            WalEndpoint::MainFeed,
            p,
        ));
    }
    (bytes, offsets)
}

/// Payloads that are runs of one byte, never spelling `TVW`.
fn z11_plain_payloads(n: usize) -> Vec<Vec<u8>> {
    (0..n).map(|k| vec![(k % 64) as u8 + 1; 32]).collect()
}

fn z11_walk(name: &str, bytes: &[u8]) -> SegmentRead {
    let dir = tmp_dir(name);
    let path = dir.join("00000000000000000001.wal");
    std::fs::write(&path, bytes).unwrap();
    let read = replay_segment_core(&path, None, &|_, _| true, true).expect("readable");
    let _ = std::fs::remove_dir_all(&dir);
    read
}

/// The pin for Z11a: one flipped byte in record 40 of 100 used to end
/// the walk and abandon records 41..100 that were intact on disk.
#[test]
fn test_regression_resync_recovers_records_after_a_mid_segment_crc_flip() {
    let payloads = z11_plain_payloads(100);
    let (mut bytes, offsets) = z11_segment(&payloads);
    // Inside record 40's frame (the v4 header is 26 bytes).
    bytes[offsets[40] + 26 + 5] ^= 0xFF;
    let read = z11_walk("resync-crc", &bytes);
    assert_eq!(read.frames.len(), 99, "only the damaged record is lost");
    assert!(read.damaged, "skipped bytes are damage");
    assert!(!read.stopped_early, "the walk resynced and read to the end");
    let seqs: Vec<u64> = read.frames.iter().map(|f| f.frame_seq).collect();
    let want: Vec<u64> = (1..=100).filter(|s| *s != 41).collect();
    assert_eq!(seqs, want);
    for f in &read.frames {
        assert_eq!(f.frame, payloads[(f.frame_seq - 1) as usize]);
        assert_eq!(
            f.after_gap,
            f.frame_seq == 42,
            "exactly the first frame after the skipped bytes follows a gap"
        );
    }
    assert_eq!(
        resync_from(&bytes, offsets[40] + 1),
        Some(offsets[41]),
        "the skipped stretch is exactly record 40's bytes"
    );
}

/// A corrupt length that points past EOF used to read as a torn tail:
/// no counter, `damaged = false`, and every later record silently gone.
#[test]
fn test_regression_corrupt_length_past_eof_is_counted_not_a_silent_tail() {
    let payloads = z11_plain_payloads(10);
    let (mut bytes, offsets) = z11_segment(&payloads);
    // v4 length field sits at offset 22 of the record.
    bytes[offsets[4] + 22..offsets[4] + 26].copy_from_slice(&0x7FFF_FFFFu32.to_le_bytes());
    let read = z11_walk("resync-len-mid", &bytes);
    assert!(
        read.damaged,
        "a lying length with records after it is damage"
    );
    assert_eq!(read.frames.len(), 9, "the records after it are recovered");
    assert!(read.frames.iter().all(|f| f.frame_seq != 5));

    // The same corruption on the LAST record: nothing follows, but the
    // length is above every frame cap, so no writer produced it.
    let (mut last, offsets) = z11_segment(&payloads);
    last[offsets[9] + 22..offsets[9] + 26].copy_from_slice(&0x7FFF_FFFFu32.to_le_bytes());
    let read = z11_walk("resync-len-last", &last);
    assert_eq!(read.frames.len(), 9);
    assert!(read.damaged, "an impossible length at the tail is counted");
    assert!(
        read.stopped_early,
        "nothing readable after it: the walk stopped (SegmentRead::stopped_early)"
    );
}

/// What an interrupted writer leaves stays silent and undamaged.
#[test]
fn torn_tail_stays_silent_and_undamaged() {
    let payloads = z11_plain_payloads(10);
    let (bytes, _) = z11_segment(&payloads);
    for cut in [3usize, 20, 40] {
        let read = z11_walk("resync-torn", &bytes[..bytes.len() - cut]);
        assert_eq!(read.frames.len(), 9, "cut {cut}");
        assert!(!read.damaged, "cut {cut}: a torn tail is not damage");
        assert!(!read.trailing_gap, "cut {cut}");
        assert!(!read.stopped_early, "cut {cut}: a torn tail is no stop");
        assert!(read.frames.iter().all(|f| !f.after_gap), "cut {cut}");
    }
}

/// A record whose magic at offset 0 is damaged, with readable records
/// after it, is resynced rather than declared wholly unreadable.
#[test]
fn a_damaged_first_magic_resyncs_to_the_second_record() {
    let payloads = z11_plain_payloads(10);
    let (mut bytes, _) = z11_segment(&payloads);
    bytes[0] = b'X';
    let read = z11_walk("resync-offset0", &bytes);
    assert_eq!(read.frames.len(), 9);
    assert!(read.damaged);
    assert_eq!(read.frames[0].frame_seq, 2);
    assert!(read.frames[0].after_gap);
    assert!(read.frames[0].first_in_segment);
    assert!(!read.stopped_early);
}

/// Magic bytes inside a payload, with a plausible header, never resync
/// into a false record: the CRC refuses them.
#[test]
fn magic_bytes_inside_a_payload_never_produce_a_false_record() {
    let mut payloads = z11_plain_payloads(6);
    // A fake v4 header (valid magic, type, small length) with a bad CRC.
    let mut fake = Vec::new();
    fake.extend_from_slice(b"TVW4");
    fake.push(WsType::LiveFeed.as_u8());
    fake.extend_from_slice(&[0u8; 17]);
    fake.extend_from_slice(&4u32.to_le_bytes());
    fake.extend_from_slice(&[9, 9, 9, 9, 0xDE, 0xAD, 0xBE, 0xEF]);
    payloads[2] = fake.clone();
    payloads[3] = fake;
    let (mut bytes, offsets) = z11_segment(&payloads);
    // Damage record 2's own magic, so the scan walks into its payload.
    bytes[offsets[2]] = b'X';
    let read = z11_walk("resync-fake", &bytes);
    let seqs: Vec<u64> = read.frames.iter().map(|f| f.frame_seq).collect();
    assert_eq!(seqs, vec![1, 2, 4, 5, 6], "no fabricated record");
    for f in &read.frames {
        assert_eq!(f.frame, payloads[(f.frame_seq - 1) as usize]);
    }
    assert_eq!(resync_from(b"TVW4 not a record at all, just text", 0), None);
}

proptest::proptest! {
    #![proptest_config(proptest::prelude::ProptestConfig::with_cases(256))]

    /// Every returned frame is one that was written, and every record
    /// lying wholly before the first flip or wholly after the last one is
    /// recovered.
    #[test]
    fn resync_returns_only_written_frames(
        payloads in proptest::collection::vec(proptest::collection::vec(proptest::num::u8::ANY, 0..120), 1..40),
        flips in proptest::collection::vec((0.0f64..1.0, 1u8..=255), 0..4),
    ) {
        let (mut bytes, offsets) = z11_segment(&payloads);
        let mut flipped: Vec<usize> = Vec::new();
        for (at, x) in &flips {
            let pos = ((*at * bytes.len() as f64) as usize).min(bytes.len() - 1);
            bytes[pos] ^= *x;
            flipped.push(pos);
        }
        let read = z11_walk("resync-prop", &bytes);
        for f in &read.frames {
            let k = (f.frame_seq - 1) as usize;
            proptest::prop_assert!(k < payloads.len(), "fabricated seq {}", f.frame_seq);
            proptest::prop_assert_eq!(&f.frame, &payloads[k]);
        }
        let first = flipped.iter().copied().min();
        let last = flipped.iter().copied().max();
        for (k, start) in offsets.iter().enumerate() {
            let end = offsets.get(k + 1).copied().unwrap_or(bytes.len());
            let before = first.is_none_or(|f| end <= f);
            let after = last.is_some_and(|l| *start > l);
            if before || after {
                proptest::prop_assert!(
                    read.frames.iter().any(|f| f.frame_seq == k as u64 + 1),
                    "intact record {} not recovered", k + 1
                );
            }
        }
        if flipped.is_empty() {
            proptest::prop_assert!(!read.damaged);
            proptest::prop_assert_eq!(read.frames.len(), payloads.len());
        }
    }
}
