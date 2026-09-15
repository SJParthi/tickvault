//! WAL constructor refusal and healthy writer ownership/replay checks.
//!
//! A regular file cannot be a WAL directory. The separate Linux-only procfs
//! case requires an existing virtual parent that refuses directory creation;
//! unexpected writability fails instead of becoming a passing "no panic" result.
//!
//! The surviving-Arc and repeated-cycle cases exercise healthy writer lifetime,
//! admission, owned replay and complete payload preservation. Dropping one Arc
//! does not simulate a dead writer while another Arc still owns it.
//!
//! These cases do not fill a filesystem or establish ENOSPC recovery. The real
//! Unix file-size-limit/EFBIG fixture is in chaos_disk_full_ulimit.rs; neither
//! fixture establishes power-loss durability.

#![cfg(test)]

#[path = "support/owned_wal_replay.rs"]
mod owned_wal_replay;
use owned_wal_replay::{assert_complete_replay, claim_wal};

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tickvault_storage::ws_frame_spill::{AppendOutcome, WsFrameSpill, WsType};

fn chaos_tmp(tag: &str) -> std::path::PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let p = std::env::temp_dir().join(format!(
        "tv-wal-disk-chaos-{tag}-{}-{nanos}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&p);
    p
}

/// **Path A** — `wal_dir` points at a regular file. `create_dir_all`
/// fails. `WsFrameSpill::new` surfaces `Err` without panic.
#[test]
fn chaos_wal_dir_is_regular_file_surfaces_err_without_panic() {
    let parent = chaos_tmp("path-a-parent");
    std::fs::create_dir_all(&parent).expect("create parent"); // APPROVED: test
    let file_path = parent.join("not-a-dir");
    std::fs::write(&file_path, b"sentinel").expect("write sentinel file"); // APPROVED: test

    // Now try to use the FILE as a wal_dir. `create_dir_all` inside
    // `WsFrameSpill::new` must fail because the path already exists
    // as a regular file.
    let outcome = std::panic::catch_unwind(|| WsFrameSpill::new(&file_path));
    assert!(
        outcome.is_ok(),
        "WsFrameSpill::new must NEVER panic on a bad wal_dir"
    );
    let result = outcome.expect("catch_unwind outer");
    assert!(
        result.is_err(),
        "WsFrameSpill::new must return Err when wal_dir is a regular file"
    );

    let _ = std::fs::remove_dir_all(&parent);
}

/// **Path B** — Linux procfs refuses creating an ordinary WAL directory,
/// even for root. Require a real procfs parent and fail if the supposedly
/// unavailable path opens. Other platforms do not execute this Linux fixture.
#[cfg(target_os = "linux")]
#[test]
fn chaos_wal_dir_in_unwritable_parent_surfaces_err_without_panic() {
    let parent = std::path::Path::new("/proc/self");
    assert!(
        parent.is_dir(),
        "Linux procfs is required to exercise this directory-creation refusal"
    );
    let path = parent.join(format!("tv_ws_wal_refusal_{}", std::process::id()));
    assert!(
        !path.exists(),
        "the isolated refusal path must start absent"
    );

    let outcome = std::panic::catch_unwind(|| WsFrameSpill::new(&path));
    assert!(
        outcome.is_ok(),
        "WsFrameSpill::new must return an error instead of panicking"
    );
    match outcome.expect("catch_unwind outer") {
        Ok(spill) => {
            drop(spill);
            let cleanup = std::fs::remove_dir_all(&path);
            panic!(
                "fault prerequisite was not established: the procfs WAL path opened; \
                 this is not passing refusal evidence (cleanup={cleanup:?})"
            );
        }
        Err(error) => assert!(
            format!("{error:#}").contains("exclusive WAL directory ownership unavailable"),
            "expected directory-creation refusal, got {error:#}"
        ),
    }
}

/// **Path C** — senders dropped mid-flight. This is the post-writer-
/// crash state that a real disk-full failure leaves behind. We
/// simulate it by creating a `WsFrameSpill`, cloning an `Arc`, then
/// dropping the original. At this point the clone is still live but
/// the crossbeam channel's sender half (inside the spill) is only
/// dropped when the LAST Arc drops — so this path actually tests the
/// healthy clone behaviour. The HONEST observable we can assert is:
/// after every Arc is dropped, no panic occurs on later replay.
#[test]
fn chaos_spill_dropped_with_in_flight_arcs_no_panic_on_replay() {
    let dir = chaos_tmp("path-c");
    std::fs::create_dir_all(&dir).expect("create"); // APPROVED: test
    {
        let spill = Arc::new(WsFrameSpill::new(&dir).expect("spill new"));
        assert_eq!(
            spill.append(WsType::LiveFeed, b"keepalive".to_vec()),
            AppendOutcome::Spilled
        );
        let clone = Arc::clone(&spill);
        drop(spill);
        // Clone still holds a sender — append must still succeed.
        let outcome = clone.append(WsType::LiveFeed, b"after-drop".to_vec());
        assert!(
            outcome == AppendOutcome::Spilled,
            "a healthy surviving clone must still admit its frame"
        );
        drop(clone);
    }
    // Writer thread drains and exits cleanly.
    std::thread::sleep(Duration::from_millis(100));

    // Take the now-free directory claim and retain the full fenced result.
    // A refusal or a panic must not be mistaken for an empty recovery.
    let replay_result = std::panic::catch_unwind(|| {
        let owner = claim_wal(&dir);
        let batch = assert_complete_replay(&owner);
        assert_eq!(
            batch.frames.len(),
            2,
            "both admitted clone payloads must survive"
        );
        for (record, expected) in batch
            .frames
            .iter()
            .zip([b"keepalive".as_slice(), b"after-drop".as_slice()])
        {
            assert_eq!(record.ws_type, WsType::LiveFeed);
            assert_eq!(record.frame, expected);
        }
        assert!(batch.frames[0].frame_seq < batch.frames[1].frame_seq);
    });
    assert!(
        replay_result.is_ok(),
        "owned replay must finish all payload checks after the last clone drops"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// **Integration** — stress: 10 rapid-fire create/drop/replay cycles
/// with realistic frame traffic. No panic across any cycle. Catches
/// any race condition in writer-thread shutdown that a real disk-full
/// scenario might expose.
#[test]
fn chaos_rapid_disk_io_cycles_no_panic() {
    let root = chaos_tmp("integration");
    std::fs::create_dir_all(&root).expect("create root"); // APPROVED: test

    for cycle in 0..10 {
        let dir = root.join(format!("cycle-{cycle}"));
        let spill = WsFrameSpill::new(&dir).expect("new");
        for i in 0..500 {
            let frame = vec![0u8; 64];
            assert_eq!(
                spill.append(
                    if i % 2 == 0 {
                        WsType::LiveFeed
                    } else {
                        WsType::OrderUpdate
                    },
                    frame,
                ),
                AppendOutcome::Spilled
            );
        }
        drop(spill);
        std::thread::sleep(Duration::from_millis(20));
        let replay_result = std::panic::catch_unwind(|| {
            let owner = claim_wal(&dir);
            let batch = assert_complete_replay(&owner);
            assert_eq!(
                batch.frames.len(),
                500,
                "cycle {cycle} must recover every admitted frame"
            );
            for (index, record) in batch.frames.iter().enumerate() {
                let expected_type = if index % 2 == 0 {
                    WsType::LiveFeed
                } else {
                    WsType::OrderUpdate
                };
                assert_eq!(record.ws_type, expected_type);
                assert_eq!(record.frame, vec![0u8; 64]);
            }
            assert!(
                batch
                    .frames
                    .windows(2)
                    .all(|pair| pair[0].frame_seq < pair[1].frame_seq)
            );
        });
        assert!(
            replay_result.is_ok(),
            "cycle {cycle} must not swallow a replay panic or refusal"
        );
    }

    let _ = std::fs::remove_dir_all(&root);
}
