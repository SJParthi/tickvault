//! Idle seal of the WAL writer's open segment, on request (operator Quote 31,
//! 2026-10-21).
//!
//! The WAL writer (`ws_frame_spill`) rotates a segment only when it fills. After
//! the close no frame arrives to fill the session's last segment, so it stays
//! open, and the after-close depth pass carries every bucket that overlaps an
//! open segment. That bucket waited a whole day. The pass now asks the writer
//! to close (flush, sync and release) that segment in its next lull: a 200 ms
//! poll with an empty channel. The next record opens a fresh segment, so a
//! request never loses or splits a frame.
//!
//! Kept out of `ws_frame_spill.rs`, which is at its size pin.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

/// WAL directories whose writer should seal its open segment at the next
/// lull ([`request_idle_seal`]). One entry per production WAL; tests add one
/// per temp directory. `IDLE_SEAL_ANY` keeps the writer's poll to one load.
// APPROVED: never on the per-frame append path — the Vec is a static, not a per-frame allocation.
static IDLE_SEAL_DIRS: std::sync::Mutex<Vec<PathBuf>> = std::sync::Mutex::new(Vec::new());
static IDLE_SEAL_ANY: AtomicBool = AtomicBool::new(false);

/// Asks the WAL writer to close the segment it holds open at its next lull
/// (a poll with no frame), so every frame written so far sits in a closed,
/// replayable segment. Operator Quote 31 (2026-10-21): after the close no
/// frame arrives to fill and rotate the last segment, and the after-close
/// depth pass cannot write back a bucket that overlaps an open segment.
/// Idempotent and O(1); a request while frames still flow waits for a lull,
/// and with nothing open it is a no-op.
pub fn request_idle_seal(wal_dir: &Path) {
    if let Ok(mut dirs) = IDLE_SEAL_DIRS.lock() {
        if !dirs.iter().any(|d| d == wal_dir) {
            dirs.push(wal_dir.to_path_buf()); // APPROVED: cold, once per after-close round
        }
        IDLE_SEAL_ANY.store(true, Ordering::Release);
    }
}

/// Takes a pending seal request for `wal_dir`. One load when nothing is
/// pending; the lock only while some request is. Writer thread, once per
/// lull poll, never per frame.
pub(crate) fn take_idle_seal_request(wal_dir: &Path) -> bool {
    if !IDLE_SEAL_ANY.load(Ordering::Acquire) {
        return false;
    }
    let Ok(mut dirs) = IDLE_SEAL_DIRS.lock() else {
        return false;
    };
    let before = dirs.len();
    dirs.retain(|d| d != wal_dir); // O(1) EXEMPT: at most one entry per live WAL dir, cold
    if dirs.is_empty() {
        IDLE_SEAL_ANY.store(false, Ordering::Release);
    }
    dirs.len() != before
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ws_frame_spill::{WsFrameSpill, WsType, is_open_segment};
    use std::time::Duration;

    fn tmp_dir(name: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let p = std::env::temp_dir().join(format!("tv-idle-seal-{name}-{nanos}"));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn wait_until_persisted(spill: &WsFrameSpill, target: u64) {
        for _ in 0..500 {
            if spill.persisted_count() >= target {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("spill did not persist {target} frames");
    }

    fn segments(d: &Path) -> Vec<PathBuf> {
        let mut v: Vec<PathBuf> = std::fs::read_dir(d)
            .unwrap()
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("wal"))
            .collect();
        v.sort();
        v
    }

    /// A request is scoped to its directory and consumed once.
    #[test]
    fn take_idle_seal_request_is_per_directory_and_one_shot() {
        let a = tmp_dir("req-a");
        let b = tmp_dir("req-b");
        assert!(!take_idle_seal_request(&a), "nothing requested");
        request_idle_seal(&a);
        request_idle_seal(&a);
        assert!(!take_idle_seal_request(&b), "another directory's request");
        assert!(take_idle_seal_request(&a));
        assert!(
            !take_idle_seal_request(&a),
            "consumed once, even if asked twice"
        );
        let _ = std::fs::remove_dir_all(&a);
        let _ = std::fs::remove_dir_all(&b);
    }

    /// Quote 31: a request closes the writer's open segment in a lull WITHOUT
    /// stopping the writer, so the after-close depth pass can read it; the
    /// next frame opens a new segment.
    #[test]
    fn test_request_idle_seal_closes_the_open_segment_and_the_next_frame_opens_a_new_one() {
        let dir = tmp_dir("seal");
        let spill = WsFrameSpill::new(&dir).unwrap();
        spill.append(WsType::LiveFeed, vec![7u8; 16]);
        wait_until_persisted(&spill, 1);
        let first = segments(&dir);
        assert_eq!(first.len(), 1);
        assert!(is_open_segment(&first[0]));

        request_idle_seal(&dir);
        for _ in 0..300 {
            if !is_open_segment(&first[0]) {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            !is_open_segment(&first[0]),
            "the lull must seal the open segment"
        );
        assert!(
            std::fs::metadata(&first[0]).unwrap().len() > 0,
            "the sealed segment keeps its frame"
        );

        spill.append(WsType::LiveFeed, vec![8u8; 16]);
        wait_until_persisted(&spill, 2);
        let after = segments(&dir);
        assert_eq!(after.len(), 2, "the next frame opens a new segment");
        assert!(is_open_segment(&after[1]));
        assert!(!is_open_segment(&after[0]));
        drop(spill);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
