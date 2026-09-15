//! Integration-fixture access to the production owned, fenced replay path.
//! No test bypasses a resource fence or confirms an unacknowledged generation.

#![cfg(test)]

use std::path::Path;
use tickvault_storage::ws_frame_spill::{WalMaintenance, WalReplayBatch, lock_wal_dir};

pub fn claim_wal(directory: &Path) -> WalMaintenance {
    let guard = lock_wal_dir(directory)
        .expect("claim the fixture WAL only after its writer has exited")
        .expect("replay requires an exclusive directory guard");
    WalMaintenance::from_guard(&guard).expect("bind maintenance to the held directory claim")
}

pub fn assert_complete_replay(owner: &WalMaintenance) -> WalReplayBatch {
    let batch = owner
        .replay_fenced()
        .expect("owned fenced replay must decode the healthy fixture");
    assert!(
        !batch.stopped_for_disk && !batch.stopped_for_memory && !batch.stopped_for_frame_cap,
        "fixture replay was refused: disk={}, memory={}, frame_cap={}; this is not an empty success",
        batch.stopped_for_disk,
        batch.stopped_for_memory,
        batch.stopped_for_frame_cap
    );
    assert_eq!(
        batch.deferred_segments, 0,
        "the whole bounded fixture must be replayed"
    );
    assert_eq!(
        (
            batch.skipped_segments,
            batch.skipped_frames,
            batch.skipped_bytes
        ),
        (0, 0, 0),
        "these fixtures provide no exact sink ACK permitting any original to be skipped"
    );
    assert_eq!(
        batch.bytes_replayed,
        batch
            .frames
            .iter()
            .map(|frame| frame.frame.len() as u64)
            .sum::<u64>(),
        "replay byte accounting must match the complete returned payloads"
    );
    batch
}
