//! The receipt time of the newest frame an EARLIER process wrote to the WAL
//! (plan item K4, 2026-10-26).
//!
//! A process that boots mid-session needs to know how long the feed was
//! unwatched: from the previous process's last received frame to now. The
//! capture-at-receipt WAL holds that answer. Every v3/v4 record carries the
//! frame's receipt instant (UTC epoch nanos), so the newest record on disk,
//! read before this process appends anything, is the previous process's last
//! receipt.
//!
//! ## How "newest" is found
//!
//! The same way the sequence seed finds the high-water mark
//! (`ws_frame_spill::highest_frame_seq_on_disk`): for the live directory and
//! its `replaying/`, `settling/` and `archive/` subfolders, walk the record
//! headers of the newest segment (stepping back over at most
//! [`SEGMENTS_PER_DIR`] empty or torn ones), keep the record with the greatest
//! frame sequence, then read that one record whole and check its CRC before
//! trusting its receipt.
//!
//! ## Cost
//!
//! Boot only, once: one header walk per directory (O(records in one segment),
//! payloads seeked past, never read) plus one whole-record read. Never on the
//! frame path.
//!
//! ## Honest limits
//!
//! - The greatest SEQUENCE is not always the greatest receipt: sixteen sockets
//!   race to the writer, so the two can differ by the reorder slack (well
//!   under a second). The answer is the previous process's last receipt to
//!   within that.
//! - A record from before v3 has no receipt, and a WAL whose segments were all
//!   uploaded and pruned has no record at all: both give `None`, and the boot
//!   writes no downtime row rather than guess.
//! - It must run after the WAL directory is claimed and before this process's
//!   writer appends its first frame; otherwise it reads this process's own
//!   frame. `main.rs` calls it right after `lock_wal_dir`.

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use crate::ws_frame_spill::{
    ARCHIVE_SUBDIR, REPLAYING_SUBDIR, RecordProbe, WAL_RECEIPT_UNKNOWN_NANOS,
    header_walk_highest_frame_seq, plausible_receipt_nanos, record_at_capped, wal_segments_in,
};

/// Newest segments examined per directory, stepping back over empty or torn
/// ones (the sequence seed uses the same bound).
pub const SEGMENTS_PER_DIR: usize = 4;

/// Largest frame a probed record may declare before the probe refuses it
/// rather than allocate (matches the sequence probe's own cap).
const PROBE_MAX_FRAME_BYTES: usize = 64 * 1024 * 1024;

/// v3/v4 header bytes up to and including the receipt field:
/// magic(4) + ws_type(1) + frame_seq(8) + received_at_nanos(8).
const RECEIPT_HEADER_BYTES: usize = 21;

/// Byte range of the receipt inside a v3/v4 record.
const RECEIPT_OFFSET: usize = 13;

/// The receipt instant (UTC epoch nanos) of the newest intact v3/v4 record
/// under `wal_dir`, or `None` when there is none (no WAL, every segment
/// pruned, only pre-v3 records, a corrupt newest record, or an implausible
/// receipt).
#[must_use]
pub fn newest_receipt_nanos_on_disk(wal_dir: &Path) -> Option<i64> {
    let (seq, path, offset) = newest_record_on_disk(wal_dir)?;
    receipt_of_record(&path, offset, seq)
}

/// `(frame_seq, segment, offset)` of the record with the greatest header
/// sequence across the four WAL directories. Header-only walk.
fn newest_record_on_disk(wal_dir: &Path) -> Option<(u64, PathBuf, u64)> {
    let mut best: Option<(u64, PathBuf, u64)> = None;
    for dir in [
        wal_dir.to_path_buf(),
        wal_dir.join(REPLAYING_SUBDIR),
        wal_dir.join(crate::wal_replay_settle::SETTLING_SUBDIR),
        wal_dir.join(ARCHIVE_SUBDIR),
    ] {
        let mut segs = wal_segments_in(&dir);
        // Names are zero-padded rotation nanos: lexicographic is chronological.
        segs.sort_by(|a, b| a.file_name().cmp(&b.file_name())); // O(1) EXEMPT: boot-time probe, cold path
        for seg in segs.iter().rev().take(SEGMENTS_PER_DIR) {
            let (seq, offset) = header_walk_highest_frame_seq(seg);
            if seq > 0 {
                if best.as_ref().is_none_or(|(b, _, _)| seq > *b) {
                    best = Some((seq, seg.clone(), offset));
                }
                break;
            }
        }
    }
    best
}

/// Reads the record at `offset` whole, checks its CRC and that it carries
/// `expected_seq`, then returns its receipt when the record is v3/v4 and the
/// receipt is plausible.
fn receipt_of_record(path: &Path, offset: u64, expected_seq: u64) -> Option<i64> {
    let mut f = File::open(path).ok()?;
    match record_at_capped(&mut f, offset, PROBE_MAX_FRAME_BYTES) {
        RecordProbe::Intact { frame_seq, .. } if frame_seq == expected_seq => {}
        _ => return None,
    }
    std::io::Seek::seek(&mut f, std::io::SeekFrom::Start(offset)).ok()?;
    let mut head = [0u8; RECEIPT_HEADER_BYTES];
    f.read_exact(&mut head).ok()?;
    if &head[0..4] != b"TVW3" && &head[0..4] != b"TVW4" {
        return None;
    }
    let bytes: [u8; 8] = head[RECEIPT_OFFSET..RECEIPT_OFFSET + 8].try_into().ok()?;
    let receipt = plausible_receipt_nanos(i64::from_le_bytes(bytes));
    (receipt != WAL_RECEIPT_UNKNOWN_NANOS).then_some(receipt)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ws_frame_spill::{WalEndpoint, WsFrameSpill, WsType};

    /// A unique temp folder, removed on drop.
    struct TmpDir(PathBuf);

    impl TmpDir {
        fn new() -> Self {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let p = std::env::temp_dir().join(format!("tv-wal-last-receipt-{nanos}-{n}"));
            std::fs::create_dir_all(&p).unwrap();
            Self(p)
        }
    }

    impl Drop for TmpDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    #[test]
    fn test_newest_receipt_nanos_on_disk_none_for_missing_dir() {
        let dir = TmpDir::new();
        assert_eq!(
            newest_receipt_nanos_on_disk(&dir.0.as_path().join("does-not-exist")),
            None
        );
    }

    #[test]
    fn test_newest_receipt_nanos_on_disk_none_for_empty_dir() {
        let dir = TmpDir::new();
        assert_eq!(newest_receipt_nanos_on_disk(dir.0.as_path()), None);
    }

    #[test]
    fn test_newest_receipt_nanos_on_disk_and_header_walk_highest_frame_seq_read_the_last_frame() {
        let dir = TmpDir::new();
        // Two frames with known receipts (2026-10-26 ~04:00 and ~04:01 UTC).
        let first = 1_792_987_200_000_000_000_i64;
        let second = first + 60_000_000_000;
        {
            let spill = WsFrameSpill::new(dir.0.as_path()).expect("spill");
            let s1 = crate::ws_frame_spill::next_frame_seq();
            spill.append_with_seq_at(
                WsType::LiveFeed,
                vec![1u8; 16],
                s1,
                first,
                WalEndpoint::MainFeed,
            );
            let s2 = crate::ws_frame_spill::next_frame_seq();
            spill.append_with_seq_at(
                WsType::LiveFeed,
                vec![2u8; 16],
                s2,
                second,
                WalEndpoint::MainFeed,
            );
            spill.shutdown(std::time::Duration::from_secs(5));
        }
        assert_eq!(newest_receipt_nanos_on_disk(dir.0.as_path()), Some(second));
    }

    #[test]
    fn test_newest_receipt_nanos_on_disk_none_for_garbage_segment() {
        let dir = TmpDir::new();
        std::fs::write(
            dir.0.as_path().join("00000000000000000001.wal"),
            b"not a wal record",
        )
        .unwrap();
        assert_eq!(newest_receipt_nanos_on_disk(dir.0.as_path()), None);
    }
}
