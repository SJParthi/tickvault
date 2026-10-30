//! Fuzz target: on-disk sealed-candle spill records and the tick-spill line
//! splitter (bytes read back from disk after a crash or a bad sector).
//!
//! Properties beyond "no panic":
//!
//! * a seal built from the input encodes, decodes to the same bytes, and
//!   any one flipped bit of it never decodes;
//! * `decode_spill_record` on the first 128 bytes: a decoded seal re-encodes
//!   to a record that decodes to the same bytes;
//! * flipping any one bit of that re-encoded record never decodes (CRC-32);
//! * `split_at_line_boundary` on the input and a range taken from it returns
//!   a point strictly inside the range, right after a newline.

#![no_main]

use libfuzzer_sys::fuzz_target;
use tickvault_common::feed::Feed;
use tickvault_storage::seal_spill::{
    SEAL_SPILL_RECORD_SIZE, SerializedSeal, SpillRecordRead, decode_spill_record,
};
use tickvault_storage::tick_spill_replay::split_at_line_boundary;

/// Little-endian word `i` of the fuzz input (zero past the end).
fn word(data: &[u8], i: usize) -> u64 {
    let mut b = [0u8; 8];
    for (k, slot) in b.iter_mut().enumerate() {
        *slot = data.get(i * 8 + k).copied().unwrap_or(0);
    }
    u64::from_le_bytes(b)
}

/// A seal whose every field comes from the fuzz input, so the decode and
/// bit-flip properties run on valid records, not only on noise that fails
/// the checksum.
fn seal_from(data: &[u8]) -> SerializedSeal {
    let w = |i| word(data, i);
    let f = |i| f64::from_bits(word(data, i));
    SerializedSeal {
        security_id: w(0),
        exchange_segment_code: data.first().copied().unwrap_or(0),
        tf_ordinal: data.get(1).copied().unwrap_or(0),
        feed: Feed::ALL[usize::from(data.get(2).copied().unwrap_or(0)) % Feed::ALL.len()],
        bucket_start_ist_secs: w(1) as u32,
        tick_count: (w(1) >> 32) as u32,
        volume: w(2),
        bucket_start_cumulative: w(3),
        oi: w(4) as i64,
        open: f(5),
        high: f(6),
        low: f(7),
        close: f(8),
        close_pct_from_prev_day: f(9),
        bucket_open_prev_close: f(10),
        total_buy_qty: w(11) as u32,
        total_sell_qty: (w(11) >> 32) as u32,
        open_pct: f(12),
        change_pct: f(13),
        open_gap_pct: f(14),
    }
}

fuzz_target!(|data: &[u8]| {
    // Valid records built from the input: round trip and bit flips.
    let built = seal_from(data).to_bytes();
    match decode_spill_record(&built) {
        SpillRecordRead::Seal(back) => assert_eq!(back.to_bytes(), built),
        other => panic!("a freshly encoded seal refused: {other:?}"),
    }
    let flip = usize::try_from(word(data, 15) % (SEAL_SPILL_RECORD_SIZE as u64 * 8)).unwrap_or(0);
    let mut damaged = built;
    damaged[flip / 8] ^= 1 << (flip % 8);
    assert!(
        !matches!(decode_spill_record(&damaged), SpillRecordRead::Seal(_)),
        "bit {flip} flipped in a valid record and still decoded"
    );
    let _ = SerializedSeal::from_bytes(data);
    if let Some(head) = data.get(..SEAL_SPILL_RECORD_SIZE) {
        let mut record = [0u8; SEAL_SPILL_RECORD_SIZE];
        record.copy_from_slice(head);
        if let SpillRecordRead::Seal(seal) = decode_spill_record(&record) {
            let again = seal.to_bytes();
            match decode_spill_record(&again) {
                SpillRecordRead::Seal(back) => assert_eq!(back.to_bytes(), again),
                other => panic!("re-encoded seal refused: {other:?}"),
            }
            let bit = usize::from(data.get(SEAL_SPILL_RECORD_SIZE).copied().unwrap_or(0))
                % (SEAL_SPILL_RECORD_SIZE * 8);
            let mut flipped = again;
            flipped[bit / 8] ^= 1 << (bit % 8);
            assert!(
                !matches!(decode_spill_record(&flipped), SpillRecordRead::Seal(_)),
                "bit {bit} flipped and still decoded"
            );
        }
    }
    let (start, len) = match data {
        [a, b, ..] => (usize::from(*a), usize::from(*b)),
        _ => (0, data.len()),
    };
    let range = start..start.saturating_add(len);
    if let Some(split) = split_at_line_boundary(data, range.clone()) {
        let end = range.end.min(data.len());
        assert!(
            split > start && split < end,
            "split {split} outside {range:?}"
        );
        assert_eq!(data[split - 1], b'\n');
    }
});
