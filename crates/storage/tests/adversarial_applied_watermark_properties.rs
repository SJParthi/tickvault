//! Adversarial properties of the WAL applied-watermark file (`applied.tvaw`)
//! as read at boot by `AppliedSnapshot::from_bytes` / `load`.
//!
//! A wrongly ACCEPTED file is a data-loss path: a watermark that reads higher
//! than the one written makes boot replay treat unread WAL segments as
//! already applied and skip them. These properties pin that the reader
//! accepts only an exact, checksummed, plausible file:
//!
//! 1. Arbitrary bytes of any length never panic; anything accepted has the
//!    exact length, magic, version and a CRC-32 (computed here by an
//!    independent bitwise reference) that matches.
//! 2. A buffer with a valid CRC and a random body is accepted exactly when
//!    both watermarks are at most one day past "now", and every field it
//!    returns equals a reference decode of the bytes (so a load never
//!    invents a watermark, a flag or a bucket).
//! 3. Any one or two flipped bits of a valid file are refused (the file is
//!    ~16,700 bits, inside CRC-32's distance-3 length, so every 1- and
//!    2-bit error is detected); a flipped file can therefore never raise
//!    the watermark.
//! 4. A file truncated or extended by any amount is refused.
//!
//! The layout (magic `TVAW`, version 1, 40-byte header, 128 x 16-byte
//! bucket body, 4-byte CRC) is restated here and pinned against the
//! module's public `APPLIED_WATERMARK_LEN`, so a format change fails loudly.
//! Honest limit: the ranges sidecar and the directory tag check in `load`
//! are not exercised here (they need a real WAL directory).

use proptest::prelude::*;
use proptest::test_runner::Config as ProptestConfig;
use tickvault_storage::wal_applied_watermark::{
    APPLIED_WATERMARK_LEN, AppliedSnapshot, UNAPPLIED_BUCKETS,
};

const MAGIC: &[u8; 4] = b"TVAW";
const VERSION: u8 = 1;
const HEADER: usize = 40;
const BODY: usize = UNAPPLIED_BUCKETS * 16;
const ONE_DAY_NANOS: u64 = 86_400 * 1_000_000_000;
/// 2026-10-30 12:00 IST in UTC nanoseconds: a fixed "now".
const NOW_NANOS: u64 = 1_793_342_400 * 1_000_000_000;

/// Bitwise CRC-32 (IEEE 802.3, reflected, init and final xor all ones),
/// written independently of the module's table-driven one.
fn crc32_reference(bytes: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &b in bytes {
        crc ^= u32::from(b);
        for _ in 0..8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

fn word(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().expect("8 bytes"))
}

/// A file with the right header, the given body fields, and a fresh CRC.
fn valid_file(flags: u8, hwm_ticks: u64, hwm_depth: u64, rest: &[u8]) -> Vec<u8> {
    let mut b = Vec::with_capacity(APPLIED_WATERMARK_LEN);
    b.extend_from_slice(MAGIC);
    b.push(VERSION);
    b.push(flags);
    b.extend_from_slice(&rest[0..2]); // ranges generation
    b.extend_from_slice(&hwm_ticks.to_le_bytes());
    b.extend_from_slice(&hwm_depth.to_le_bytes());
    b.extend_from_slice(&rest[2..18]); // persisted_at, dir_tag
    b.extend_from_slice(&rest[18..18 + BODY]);
    let crc = crc32_reference(&b);
    b.extend_from_slice(&crc.to_le_bytes());
    b
}

fn rest_bytes() -> impl Strategy<Value = Vec<u8>> {
    prop::collection::vec(any::<u8>(), 18 + BODY)
}

fn watermark() -> impl Strategy<Value = u64> {
    prop_oneof![
        any::<u64>(),
        0..NOW_NANOS + ONE_DAY_NANOS,
        Just(NOW_NANOS + ONE_DAY_NANOS),
        Just(NOW_NANOS + ONE_DAY_NANOS + 1),
        Just(u64::MAX),
        Just(0),
    ]
}

#[test]
fn test_layout_restated_here_matches_the_module() {
    assert_eq!(APPLIED_WATERMARK_LEN, HEADER + BODY + 4);
    // A valid all-zero file is accepted and reads back as zeros.
    let file = valid_file(0, 0, 0, &vec![0; 18 + BODY]);
    let snap = AppliedSnapshot::from_bytes(&file, NOW_NANOS).expect("valid zero file");
    assert_eq!((snap.hwm_ticks, snap.hwm_depth), (0, 0));
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 1024, ..ProptestConfig::default() })]

    /// Property 1.
    #[test]
    fn prop_arbitrary_bytes_accepted_only_when_exact_and_checksummed(
        raw in prop_oneof![
            prop::collection::vec(any::<u8>(), 0..64),
            prop::collection::vec(any::<u8>(), APPLIED_WATERMARK_LEN),
            prop::collection::vec(any::<u8>(), APPLIED_WATERMARK_LEN - 1..APPLIED_WATERMARK_LEN + 2),
        ],
        now in any::<u64>(),
    ) {
        if AppliedSnapshot::from_bytes(&raw, now).is_some() {
            prop_assert_eq!(raw.len(), APPLIED_WATERMARK_LEN);
            prop_assert_eq!(&raw[0..4], MAGIC);
            prop_assert_eq!(raw[4], VERSION);
            let crc = u32::from_le_bytes(raw[HEADER + BODY..].try_into().expect("4"));
            prop_assert_eq!(crc32_reference(&raw[..HEADER + BODY]), crc);
        }
    }

    /// Property 2.
    #[test]
    fn prop_valid_file_decodes_exactly_or_is_refused_as_implausible(
        flags in any::<u8>(),
        hwm_ticks in watermark(),
        hwm_depth in watermark(),
        rest in rest_bytes(),
    ) {
        let file = valid_file(flags, hwm_ticks, hwm_depth, &rest);
        let plausible = hwm_ticks <= NOW_NANOS + ONE_DAY_NANOS && hwm_depth <= NOW_NANOS + ONE_DAY_NANOS;
        match AppliedSnapshot::from_bytes(&file, NOW_NANOS) {
            None => prop_assert!(!plausible, "a plausible checksummed file was refused"),
            Some(snap) => {
                prop_assert!(plausible, "an implausible watermark was accepted");
                prop_assert_eq!(snap.hwm_ticks, hwm_ticks);
                prop_assert_eq!(snap.hwm_depth, hwm_depth);
                prop_assert_eq!(snap.depth_untracked, flags & 0b01 != 0);
                prop_assert_eq!(snap.overflowed, flags & 0b10 != 0);
                prop_assert_eq!(snap.persisted_at_nanos, word(&file, 24));
                prop_assert_eq!(snap.dir_tag, word(&file, 32));
                for (i, slot) in snap.buckets.iter().enumerate() {
                    let at = HEADER + i * 16;
                    prop_assert_eq!(*slot, (word(&file, at), word(&file, at + 8)));
                }
            }
        }
    }

    /// Property 3: one or two flipped bits are always refused.
    #[test]
    fn prop_one_or_two_bit_flips_are_refused(
        hwm in 0..NOW_NANOS,
        rest in rest_bytes(),
        bits in prop::collection::btree_set(0..APPLIED_WATERMARK_LEN * 8, 1..=2),
    ) {
        let mut file = valid_file(0, hwm, hwm, &rest);
        prop_assert!(AppliedSnapshot::from_bytes(&file, NOW_NANOS).is_some());
        for bit in &bits {
            file[bit / 8] ^= 1 << (bit % 8);
        }
        prop_assert!(
            AppliedSnapshot::from_bytes(&file, NOW_NANOS).is_none(),
            "flipped bits {:?} accepted", bits
        );
    }

    /// Property 4: any truncation or extension is refused.
    #[test]
    fn prop_resized_file_is_refused(
        rest in rest_bytes(),
        cut in 0usize..APPLIED_WATERMARK_LEN,
        extra in prop::collection::vec(any::<u8>(), 1..16),
    ) {
        let file = valid_file(0, 1, 1, &rest);
        prop_assert!(AppliedSnapshot::from_bytes(&file[..cut], NOW_NANOS).is_none());
        let mut longer = file.clone();
        longer.extend_from_slice(&extra);
        prop_assert!(AppliedSnapshot::from_bytes(&longer, NOW_NANOS).is_none());
    }
}
