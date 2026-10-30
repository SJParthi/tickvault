//! Adversarial properties of WAL segment replay (torn, bit-flipped and
//! garbage-laden segments).
//!
//! Surface: `ws_frame_spill::replay_all`, which reads back every raw frame
//! the sockets captured, after a crash, a power cut or a bad sector. The
//! existing `ws_frame_wal_replay` fuzz target checks only that replay never
//! panics; these properties check what it RETURNS.
//!
//! One golden segment of [`FRAME_COUNT`] frames (main-feed binary frames of
//! 1 to 1,500 bytes and order-update JSON) is written once through the real
//! writer, then each case copies its bytes into a fresh directory, damages
//! them, and replays:
//!
//! 1. Untouched: every frame comes back, in order, byte for byte, with its
//!    type and sequence.
//! 2. Truncated at any byte: the result is a PREFIX of the originals (never
//!    a fabricated or reordered frame), its length never decreases as the
//!    cut moves later, and the full length gives every frame.
//! 3. One bit flipped anywhere: the result is a subsequence of the
//!    originals (each replayed frame identical to an original, order kept,
//!    none twice) and at most one frame is lost (the resync scan finds the
//!    next intact record).
//! 4. A run of random bytes inserted anywhere: subsequence again, at most
//!    one frame lost.
//!
//! Honest limits: garbage that happens to form a record with a valid CRC is
//! not excluded (about 2^-32 per candidate); multi-bit damage spread across
//! several records is covered by the "subsequence" half only.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use proptest::prelude::*;
use proptest::test_runner::Config as ProptestConfig;
use tickvault_storage::ws_frame_spill::{
    AppendOutcome, ReplayedFrame, WsFrameSpill, WsType, replay_all,
};

/// Frames in the golden segment.
const FRAME_COUNT: u32 = 24;

static CASE: AtomicU64 = AtomicU64::new(0);

/// A fresh directory for one case; removed on drop.
struct CaseDir(PathBuf);

impl CaseDir {
    fn new(tag: &str) -> Self {
        let n = CASE.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("tv-adv-wal-{tag}-{}-{n}", std::process::id()));
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

/// What a replayed frame must match: type byte, payload and sequence.
#[derive(Clone, Debug, PartialEq, Eq)]
struct FrameId {
    ws_type: u8,
    frame: Vec<u8>,
    frame_seq: u64,
}

fn id_of(frame: &ReplayedFrame) -> FrameId {
    FrameId {
        ws_type: frame.ws_type.as_u8(),
        frame: frame.frame.clone(),
        frame_seq: frame.frame_seq,
    }
}

/// Deterministic frame `i`: odd indices are order-update JSON, even ones
/// are binary main-feed frames whose length cycles from 1 to 1,500 bytes.
fn golden_frame(i: u32) -> (WsType, Vec<u8>) {
    if i % 2 == 1 {
        let json = format!(
            r#"{{"Data":{{"OrderNo":"ADV{i:06}","Status":"TRADED"}},"Type":"order_alert"}}"#
        );
        (WsType::OrderUpdate, json.into_bytes())
    } else {
        let len = 1 + (i as usize * 263) % 1_500;
        let frame = (0..len)
            .map(|b| u8::try_from((b * 31 + i as usize) % 251).expect("< 256"))
            .collect();
        (WsType::LiveFeed, frame)
    }
}

/// The golden segment: its file name, its bytes, and the frames it holds.
struct Golden {
    name: String,
    bytes: Vec<u8>,
    frames: Vec<FrameId>,
}

fn golden() -> &'static Golden {
    static GOLDEN: OnceLock<Golden> = OnceLock::new();
    GOLDEN.get_or_init(|| {
        let dir = CaseDir::new("golden");
        {
            let spill = WsFrameSpill::new(&dir.0).expect("WsFrameSpill::new");
            for i in 0..FRAME_COUNT {
                let (ws_type, frame) = golden_frame(i);
                assert_eq!(spill.append(ws_type, frame), AppendOutcome::Spilled);
            }
            // The writer thread drains the queue; drop joins it.
            std::thread::sleep(Duration::from_millis(200));
            drop(spill);
            std::thread::sleep(Duration::from_millis(50));
        }
        let segments: Vec<PathBuf> = std::fs::read_dir(&dir.0)
            .expect("read golden dir")
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("wal"))
            .collect();
        assert_eq!(segments.len(), 1, "one golden segment: {segments:?}");
        let name = segments[0]
            .file_name()
            .and_then(|n| n.to_str())
            .expect("utf-8 name")
            .to_owned();
        let bytes = std::fs::read(&segments[0]).expect("read golden segment");
        let frames: Vec<FrameId> = replay_all(&dir.0)
            .expect("replay golden")
            .iter()
            .map(id_of)
            .collect();
        assert_eq!(frames.len(), FRAME_COUNT as usize, "golden replays whole");
        for (i, frame) in frames.iter().enumerate() {
            let (ws_type, payload) = golden_frame(u32::try_from(i).expect("small"));
            assert_eq!(frame.ws_type, ws_type.as_u8(), "frame {i} type");
            assert_eq!(frame.frame, payload, "frame {i} payload");
        }
        Golden {
            name,
            bytes,
            frames,
        }
    })
}

/// Writes `bytes` as the golden segment's name in a fresh dir and replays.
fn replay_bytes(tag: &str, bytes: &[u8]) -> Vec<FrameId> {
    let dir = CaseDir::new(tag);
    write_segment(&dir.0, &golden().name, bytes);
    replay_all(&dir.0)
        .expect("replay never errors on a readable dir")
        .iter()
        .map(id_of)
        .collect()
}

fn write_segment(dir: &Path, name: &str, bytes: &[u8]) {
    std::fs::write(dir.join(name), bytes).expect("write segment");
}

/// `got` is a subsequence of `all`: every frame identical to an original,
/// in the original order, none twice. Returns how many originals are missing.
fn missing_if_subsequence(got: &[FrameId], all: &[FrameId]) -> Option<usize> {
    let mut next = 0;
    for frame in got {
        let at = all[next..].iter().position(|f| f == frame)?;
        next += at + 1;
    }
    Some(all.len() - got.len())
}

#[test]
fn test_untouched_golden_segment_replays_every_frame() {
    let g = golden();
    assert_eq!(replay_bytes("untouched", &g.bytes), g.frames);
}

/// Every cut point, exhaustively (the segment is a few KiB).
#[test]
fn test_truncation_at_every_byte_yields_a_growing_prefix() {
    let g = golden();
    let mut previous = 0;
    for cut in 0..=g.bytes.len() {
        let got = replay_bytes("cut", &g.bytes[..cut]);
        assert!(
            got.len() <= g.frames.len() && got[..] == g.frames[..got.len()],
            "cut {cut}: not a prefix of the originals"
        );
        assert!(
            got.len() >= previous,
            "cut {cut}: {} frames, fewer than {previous} at an earlier cut",
            got.len()
        );
        previous = got.len();
    }
    assert_eq!(previous, g.frames.len(), "the whole segment replays whole");
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 512, ..ProptestConfig::default() })]

    #[test]
    fn prop_one_bit_flip_loses_at_most_one_frame(at in any::<prop::sample::Index>(), bit in 0u8..8) {
        let g = golden();
        let mut bytes = g.bytes.clone();
        let offset = at.index(bytes.len());
        bytes[offset] ^= 1 << bit;
        let got = replay_bytes("flip", &bytes);
        let missing = missing_if_subsequence(&got, &g.frames);
        prop_assert!(missing.is_some(), "byte {} bit {}: a replayed frame is not an original", offset, bit);
        prop_assert!(missing.unwrap_or(usize::MAX) <= 1, "byte {} bit {}: {:?} frames lost", offset, bit, missing);
    }

    #[test]
    fn prop_inserted_garbage_loses_at_most_one_frame(
        at in any::<prop::sample::Index>(),
        garbage in prop::collection::vec(any::<u8>(), 1..64),
    ) {
        let g = golden();
        let offset = at.index(g.bytes.len() + 1);
        let mut bytes = g.bytes[..offset].to_vec();
        bytes.extend_from_slice(&garbage);
        bytes.extend_from_slice(&g.bytes[offset..]);
        let got = replay_bytes("garbage", &bytes);
        let missing = missing_if_subsequence(&got, &g.frames);
        prop_assert!(missing.is_some(), "insert at {}: a replayed frame is not an original", offset);
        prop_assert!(missing.unwrap_or(usize::MAX) <= 1, "insert at {}: {:?} frames lost", offset, missing);
    }

    /// Arbitrary bytes under a real segment name: never panics, never errors,
    /// and nothing is returned that the bytes do not hold.
    #[test]
    fn prop_arbitrary_segment_bytes_are_handled(raw in prop::collection::vec(any::<u8>(), 0..2048)) {
        let got = replay_bytes("arbitrary", &raw);
        for frame in &got {
            prop_assert!(
                raw.windows(frame.frame.len().max(1)).any(|w| w == &frame.frame[..]) || frame.frame.is_empty(),
                "a replayed payload is not in the input bytes"
            );
        }
    }
}
