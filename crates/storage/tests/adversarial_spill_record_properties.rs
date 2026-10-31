//! Adversarial properties of the on-disk sealed-candle spill records and the
//! tick-spill line splitter.
//!
//! Surfaces (bytes that come back from disk after a crash, a full volume or
//! a bad sector):
//!
//! * `seal_spill::decode_spill_record` / `SerializedSeal::{to_bytes,
//!   from_bytes}`: one fixed 128-byte record, CRC-32 over bytes 4..128 at
//!   bytes 0..4 (format 5 onward), version at byte 7.
//! * `SealSpillWriter::read_all` over a day file that was torn (truncated)
//!   or had a record damaged.
//! * `tick_spill_replay::split_at_line_boundary`, which splits a refused ILP
//!   chunk into whole lines.
//!
//! Properties:
//!
//! 1. Round trip: `decode(to_bytes(s))` is `Seal(s)` for every seal
//!    (compared by re-encoded bytes, so NaN fields count too).
//! 2. Any 1, 2 or 3 distinct bit flips of a valid record never decode to a
//!    seal. CRC-32 (IEEE) has Hamming distance 4 for messages this short
//!    (992 bits), and a flip in the version byte lands on an unreadable
//!    version, so a decoded seal after such a flip would be a real defect.
//! 3. Arbitrary 128 bytes never panic, and anything that does decode
//!    re-encodes to a record that decodes to the same seal.
//! 4. `from_bytes` on any slice shorter than a record is `None`.
//! 5. A day file truncated at any byte reads back exactly the whole records
//!    before the cut, in order, and nothing else; a file with one damaged
//!    record reads back every other record.
//! 6. `split_at_line_boundary` returns a point strictly inside the range,
//!    right after a newline, and `None` exactly when no such point exists
//!    (reference: a linear scan).
//!
//! Honest limits: four or more flipped bits can in principle collide with the
//! CRC (probability about 2^-32 per pattern) and are not asserted; the
//! splitter's "nearest the middle" choice is not asserted, only that the
//! split is a real boundary.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use proptest::prelude::*;
use proptest::test_runner::Config as ProptestConfig;
use tickvault_common::feed::Feed;
use tickvault_storage::seal_spill::{
    SEAL_SPILL_RECORD_SIZE, SealSpillWriter, SerializedSeal, SpillRecordRead, decode_spill_record,
};
use tickvault_storage::tick_spill_replay::split_at_line_boundary;

/// 2026-10-30 12:00:00 IST, a fixed "now" so every case writes the same day
/// file name inside its own directory.
const NOW_UNIX_SECS: i64 = 1_793_342_400;

static CASE: AtomicU64 = AtomicU64::new(0);

/// A fresh, empty directory for one case; removed by [`CaseDir::drop`].
struct CaseDir(PathBuf);

impl CaseDir {
    fn new(tag: &str) -> Self {
        let n = CASE.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("tv-adv-spill-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create case dir");
        Self(dir)
    }
}

impl Drop for CaseDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn any_seal() -> impl Strategy<Value = SerializedSeal> {
    (
        (
            any::<u64>(),
            any::<u8>(),
            any::<u8>(),
            0..Feed::ALL.len(),
            any::<u32>(),
            any::<u32>(),
            any::<u64>(),
            any::<u64>(),
            any::<i64>(),
        ),
        (
            any::<f64>(),
            any::<f64>(),
            any::<f64>(),
            any::<f64>(),
            any::<f64>(),
            any::<f64>(),
            any::<u32>(),
            any::<u32>(),
            any::<f64>(),
            any::<f64>(),
            any::<f64>(),
        ),
    )
        .prop_map(
            |(
                (id, segment, tf, feed, bucket, ticks, volume, cumulative, oi),
                (
                    open,
                    high,
                    low,
                    close,
                    close_pct,
                    prev_close,
                    buy,
                    sell,
                    open_pct,
                    change_pct,
                    gap_pct,
                ),
            )| SerializedSeal {
                security_id: id,
                exchange_segment_code: segment,
                tf_ordinal: tf,
                feed: Feed::ALL[feed],
                bucket_start_ist_secs: bucket,
                tick_count: ticks,
                volume,
                bucket_start_cumulative: cumulative,
                oi,
                open,
                high,
                low,
                close,
                close_pct_from_prev_day: close_pct,
                bucket_open_prev_close: prev_close,
                total_buy_qty: buy,
                total_sell_qty: sell,
                open_pct,
                change_pct,
                open_gap_pct: gap_pct,
            },
        )
}

/// A plain seal with distinct values per index, for the file tests.
fn indexed_seal(i: u32) -> SerializedSeal {
    SerializedSeal {
        security_id: 1_000 + u64::from(i),
        exchange_segment_code: 2,
        tf_ordinal: 1,
        feed: Feed::Dhan,
        bucket_start_ist_secs: 33_300 + i * 60,
        tick_count: i + 1,
        volume: u64::from(i) * 75,
        bucket_start_cumulative: 0,
        oi: 0,
        open: 100.0 + f64::from(i),
        high: 101.0 + f64::from(i),
        low: 99.0 + f64::from(i),
        close: 100.5 + f64::from(i),
        close_pct_from_prev_day: 0.5,
        bucket_open_prev_close: 99.5,
        total_buy_qty: i,
        total_sell_qty: i * 2,
        open_pct: 0.1,
        change_pct: 0.2,
        open_gap_pct: 0.3,
    }
}

fn record(bytes: &[u8]) -> [u8; SEAL_SPILL_RECORD_SIZE] {
    bytes.try_into().expect("a whole record")
}

fn decoded_bytes(read: SpillRecordRead) -> Option<[u8; SEAL_SPILL_RECORD_SIZE]> {
    match read {
        SpillRecordRead::Seal(seal) => Some(seal.to_bytes()),
        SpillRecordRead::OtherVersion | SpillRecordRead::ChecksumMismatch => None,
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 2048, ..ProptestConfig::default() })]

    /// Property 1: every seal survives encode then decode, byte for byte.
    #[test]
    fn prop_seal_record_round_trips(seal in any_seal()) {
        let bytes = seal.to_bytes();
        let back = decoded_bytes(decode_spill_record(&bytes));
        prop_assert_eq!(back, Some(bytes));
    }

    /// Property 2: one, two or three distinct flipped bits never decode.
    #[test]
    fn prop_up_to_three_bit_flips_never_decode(
        seal in any_seal(),
        bits in prop::collection::btree_set(0..SEAL_SPILL_RECORD_SIZE * 8, 1..=3),
    ) {
        let mut bytes = seal.to_bytes();
        for bit in &bits {
            bytes[bit / 8] ^= 1 << (bit % 8);
        }
        let read = decode_spill_record(&bytes);
        prop_assert!(
            !matches!(read, SpillRecordRead::Seal(_)),
            "flipped bits {:?} still decoded", bits
        );
        prop_assert!(SerializedSeal::from_bytes(&bytes).is_none() || bits.iter().any(|b| b / 8 == 7));
    }

    /// Property 3: arbitrary bytes never panic; a decoded seal re-encodes to
    /// a record that decodes to the same seal.
    #[test]
    fn prop_arbitrary_record_never_panics(raw in prop::collection::vec(any::<u8>(), SEAL_SPILL_RECORD_SIZE)) {
        let bytes = record(&raw);
        if let SpillRecordRead::Seal(seal) = decode_spill_record(&bytes) {
            let again = seal.to_bytes();
            prop_assert_eq!(decoded_bytes(decode_spill_record(&again)), Some(again));
        }
        let _ = SerializedSeal::from_bytes(&raw);
    }

    /// Property 4: a short slice is never a record.
    #[test]
    fn prop_short_slice_is_none(raw in prop::collection::vec(any::<u8>(), 0..SEAL_SPILL_RECORD_SIZE)) {
        prop_assert!(SerializedSeal::from_bytes(&raw).is_none());
    }

    /// Property 6: the splitter returns a real line boundary strictly inside
    /// the range, and `None` exactly when there is none.
    #[test]
    fn prop_split_at_line_boundary_matches_reference(
        bytes in prop::collection::vec(prop_oneof![Just(b'\n'), Just(b'a'), any::<u8>()], 0..96),
        start in 0usize..110,
        len in 0usize..110,
    ) {
        let range = start..start.saturating_add(len);
        let got = split_at_line_boundary(&bytes, range.clone());
        let end = range.end.min(bytes.len());
        let reference_exists = (start.saturating_add(1)..end).any(|s| bytes[s - 1] == b'\n');
        match got {
            Some(s) => {
                prop_assert!(s > start && s < end, "split {} outside ({}, {})", s, start, end);
                prop_assert_eq!(bytes[s - 1], b'\n');
            }
            None => prop_assert!(!reference_exists, "a boundary exists in {:?}", range),
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 64, ..ProptestConfig::default() })]

    /// Property 5a: a day file torn at any byte reads back exactly the whole
    /// records before the cut, in order.
    #[test]
    fn prop_torn_day_file_reads_the_whole_record_prefix(count in 1u32..12, cut_seed in any::<u64>()) {
        let dir = CaseDir::new("torn");
        let writer = SealSpillWriter::with_spill_dir_for_test(dir.0.clone());
        let seals: Vec<SerializedSeal> = (0..count).map(indexed_seal).collect();
        for seal in &seals {
            writer.append_seal(seal, NOW_UNIX_SECS).expect("append");
        }
        let path = writer.spill_path(NOW_UNIX_SECS);
        let full = std::fs::read(&path).expect("read day file");
        prop_assert_eq!(full.len(), seals.len() * SEAL_SPILL_RECORD_SIZE);
        let cut = usize::try_from(cut_seed % (full.len() as u64 + 1)).expect("fits");
        std::fs::write(&path, &full[..cut]).expect("truncate");
        let reader = SealSpillWriter::with_spill_dir_for_test(dir.0.clone());
        let back = reader.read_all(NOW_UNIX_SECS).expect("read_all");
        prop_assert_eq!(&back[..], &seals[..cut / SEAL_SPILL_RECORD_SIZE]);
    }

    /// Property 5b: one damaged record is refused and every other record is
    /// still read, in order.
    #[test]
    fn prop_damaged_record_is_refused_others_kept(
        count in 1u32..12,
        victim_seed in any::<u32>(),
        offset in 0usize..SEAL_SPILL_RECORD_SIZE,
        mask in 1u8..=255,
    ) {
        let dir = CaseDir::new("damaged");
        let writer = SealSpillWriter::with_spill_dir_for_test(dir.0.clone());
        let seals: Vec<SerializedSeal> = (0..count).map(indexed_seal).collect();
        for seal in &seals {
            writer.append_seal(seal, NOW_UNIX_SECS).expect("append");
        }
        let path = writer.spill_path(NOW_UNIX_SECS);
        let mut bytes = std::fs::read(&path).expect("read day file");
        let victim = (victim_seed % count) as usize;
        bytes[victim * SEAL_SPILL_RECORD_SIZE + offset] ^= mask;
        std::fs::write(&path, &bytes).expect("rewrite");
        let reader = SealSpillWriter::with_spill_dir_for_test(dir.0.clone());
        let back = reader.read_all(NOW_UNIX_SECS).expect("read_all");
        let mut expected = seals.clone();
        expected.remove(victim);
        prop_assert_eq!(back, expected);
    }
}

/// A missing day file reads as empty, not as an error.
#[test]
fn test_missing_day_file_reads_empty() {
    let dir = CaseDir::new("missing");
    let reader = SealSpillWriter::with_spill_dir_for_test(dir.0.clone());
    assert!(reader.read_all(NOW_UNIX_SECS).expect("read_all").is_empty());
}

/// Every possible value of the version byte on an otherwise valid record:
/// only the live version decodes.
#[test]
fn test_only_the_live_version_byte_decodes() {
    let bytes = indexed_seal(3).to_bytes();
    let live = bytes[7];
    for version in 0..=u8::MAX {
        let mut edited = bytes;
        edited[7] = version;
        let read = decode_spill_record(&edited);
        if version == live {
            assert!(
                matches!(read, SpillRecordRead::Seal(_)),
                "live version {version}"
            );
        } else {
            assert!(
                !matches!(read, SpillRecordRead::Seal(_)),
                "version {version} decoded"
            );
        }
    }
}

/// The splitter's extreme ranges: empty, inverted, past the end, and a range
/// starting at `usize::MAX`, never panic.
#[test]
fn test_split_at_line_boundary_extreme_ranges() {
    let bytes = b"a\nb\nc\n";
    assert_eq!(split_at_line_boundary(bytes, 0..0), None);
    #[allow(clippy::reversed_empty_ranges)]
    // APPROVED: an inverted range is exactly the adversarial input under test.
    let inverted = 4..2;
    assert_eq!(split_at_line_boundary(bytes, inverted), None);
    assert_eq!(split_at_line_boundary(bytes, usize::MAX..usize::MAX), None);
    assert_eq!(split_at_line_boundary(b"", 0..10), None);
    assert_eq!(split_at_line_boundary(b"\n", 0..1), None);
    let s = split_at_line_boundary(bytes, 0..usize::MAX).expect("a split");
    assert!(s > 0 && s < bytes.len() && bytes[s - 1] == b'\n');
}
