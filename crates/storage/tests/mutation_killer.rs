//! Mutation-killer tests for `tickvault-storage`.
//!
//! Each test targets a specific mutation that `cargo-mutants` could
//! introduce in the hot/cold code paths of the WAL and persistence
//! layers: boundary conditions on segment rotation, comparison
//! inversions on dedup keys, arithmetic sign flips on buffer
//! watermarks, return-value substitutions on error paths.
//!
//! If `cargo-mutants` can silently mutate any of these functions
//! without flipping a test from pass to fail, there IS a test gap
//! and ANY of the tests here must catch it.
//!
//! Expansion of the Stage-D mutation-testing battery from
//! `crates/trading/tests/mutation_killer.rs` to the storage crate.

#![cfg(test)]

use tickvault_storage::ws_frame_spill::{AppendOutcome, WsFrameSpill, WsType, replay_all};

/// Build a unique temp dir for each test so parallel `cargo test`
/// instances never collide on the WAL directory.
fn tmp(tag: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let p = std::env::temp_dir().join(format!("tv-mut-{tag}-{}-{nanos}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).unwrap();
    p
}

// ============================================================================
// WsType enum — mutation targets: variant → variant swap, None → Some swap
// ============================================================================

#[test]
fn mutation_ws_type_from_u8_zero_returns_none() {
    assert!(WsType::from_u8(0).is_none());
}

#[test]
fn mutation_ws_type_from_u8_one_returns_live_feed_not_depth() {
    // If a mutant swaps variant arms (e.g. Depth20 → LiveFeed), this fails.
    assert_eq!(WsType::from_u8(1), Some(WsType::LiveFeed));
    assert_ne!(WsType::from_u8(1), Some(WsType::Depth20));
}

#[test]
fn mutation_ws_type_from_u8_two_returns_depth20_not_depth200() {
    assert_eq!(WsType::from_u8(2), Some(WsType::Depth20));
    assert_ne!(WsType::from_u8(2), Some(WsType::Depth200));
}

#[test]
fn mutation_ws_type_from_u8_three_returns_depth200_not_order_update() {
    assert_eq!(WsType::from_u8(3), Some(WsType::Depth200));
    assert_ne!(WsType::from_u8(3), Some(WsType::OrderUpdate));
}

#[test]
fn mutation_ws_type_from_u8_four_returns_order_update_not_live_feed() {
    assert_eq!(WsType::from_u8(4), Some(WsType::OrderUpdate));
    assert_ne!(WsType::from_u8(4), Some(WsType::LiveFeed));
}

#[test]
fn mutation_ws_type_from_u8_five_returns_none_not_live_feed() {
    // Mutant swapping the unknown arm to Some(LiveFeed) is caught here.
    assert_eq!(WsType::from_u8(5), None);
    assert_eq!(WsType::from_u8(255), None);
}

#[test]
fn mutation_ws_type_as_u8_values_do_not_collide() {
    // Any mutation that flips the discriminant so two variants share
    // a value would break dedup + replay. Assert all 4 are distinct.
    let vals = [
        WsType::LiveFeed.as_u8(),
        WsType::Depth20.as_u8(),
        WsType::Depth200.as_u8(),
        WsType::OrderUpdate.as_u8(),
    ];
    let mut sorted = vals;
    sorted.sort_unstable();
    assert_eq!(sorted, [1, 2, 3, 4]);
}

// ============================================================================
// AppendOutcome — mutation targets: Spilled → Dropped swap, both → same
// ============================================================================

#[test]
fn mutation_append_outcome_spilled_and_dropped_are_distinct() {
    // PartialEq mutation check — a mutant making Spilled == Dropped
    // would silently destroy the drop_critical counter semantics.
    assert_ne!(AppendOutcome::Spilled, AppendOutcome::Dropped);
}

#[test]
fn mutation_append_returns_spilled_in_healthy_ops() {
    let dir = tmp("healthy");
    let spill = WsFrameSpill::new(&dir).unwrap();
    // The healthy path MUST return Spilled. A mutant flipping the
    // Ok branch to Dropped would silently make drop_critical fire
    // on every append.
    assert_eq!(
        spill.append(WsType::LiveFeed, vec![0u8; 16]),
        AppendOutcome::Spilled
    );
    drop(spill);
    let _ = std::fs::remove_dir_all(&dir);
}

// ============================================================================
// Replay round-trip — mutation targets: offset arithmetic, CRC compute
// ============================================================================

#[test]
fn mutation_replay_preserves_exact_frame_bytes_not_truncated() {
    let dir = tmp("bytes-exact");
    {
        let spill = WsFrameSpill::new(&dir).unwrap();
        let payload = vec![0xAA, 0xBB, 0xCC, 0xDD, 0xEE];
        spill.append(WsType::LiveFeed, payload.clone());
        // Wait for the writer thread to persist.
        for _ in 0..200 {
            if spill.persisted_count() >= 1 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        drop(spill);
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let recovered = replay_all(&dir).unwrap();
    assert_eq!(recovered.len(), 1);
    // A mutant off-by-one in the payload slice would truncate the
    // first or last byte — assert the full payload is preserved.
    assert_eq!(recovered[0].frame, vec![0xAA, 0xBB, 0xCC, 0xDD, 0xEE]);
    assert_eq!(recovered[0].frame.len(), 5);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn mutation_replay_preserves_ws_type_tag_not_swapped() {
    let dir = tmp("tag-exact");
    {
        let spill = WsFrameSpill::new(&dir).unwrap();
        spill.append(WsType::OrderUpdate, vec![0x42]);
        for _ in 0..200 {
            if spill.persisted_count() >= 1 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        drop(spill);
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let recovered = replay_all(&dir).unwrap();
    assert_eq!(recovered.len(), 1);
    // A mutant tag swap (OrderUpdate → Depth20) would silently mis-
    // route the replay into the wrong drain helper.
    assert_eq!(recovered[0].ws_type, WsType::OrderUpdate);
    assert_ne!(recovered[0].ws_type, WsType::Depth20);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn mutation_replay_fifo_order_preserved_not_reversed() {
    let dir = tmp("fifo-order");
    {
        let spill = WsFrameSpill::new(&dir).unwrap();
        for i in 0..5u8 {
            spill.append(WsType::LiveFeed, vec![i]);
        }
        for _ in 0..200 {
            if spill.persisted_count() >= 5 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        drop(spill);
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let recovered = replay_all(&dir).unwrap();
    assert_eq!(recovered.len(), 5);
    // A mutant reversing the iterator would give [4,3,2,1,0] — the
    // FIFO contract is the basis for dedup idempotency.
    for (i, rec) in recovered.iter().enumerate() {
        assert_eq!(
            rec.frame[0], i as u8,
            "FIFO order violated at index {i}: got {:?}",
            rec.frame
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

// ============================================================================
// drop_critical counter — mutation targets: saturating_add flip, init value
// ============================================================================

#[test]
fn mutation_drop_critical_starts_at_zero_not_one() {
    let dir = tmp("dropcrit-init");
    let spill = WsFrameSpill::new(&dir).unwrap();
    // A mutant initialising drop_critical to 1 would silently break
    // the "MUST be zero in healthy ops" contract.
    assert_eq!(spill.drop_critical_count(), 0);
    drop(spill);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn mutation_persisted_count_monotonic_not_decreasing() {
    let dir = tmp("persisted-mono");
    let spill = WsFrameSpill::new(&dir).unwrap();
    let before = spill.persisted_count();
    for _ in 0..10 {
        spill.append(WsType::LiveFeed, vec![0u8; 8]);
    }
    for _ in 0..200 {
        if spill.persisted_count() >= before + 10 {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let after = spill.persisted_count();
    // A mutant flipping `fetch_add` to `fetch_sub` is caught here.
    assert!(
        after >= before,
        "persisted_count must be monotonic: {before} -> {after}"
    );
    assert!(
        after >= before + 10,
        "expected at least 10 new persists, got {after} - {before} = {}",
        after - before
    );
    drop(spill);
    let _ = std::fs::remove_dir_all(&dir);
}

// ============================================================================
// Replay tolerance — mutation targets: error path swap, empty-dir guard
// ============================================================================

#[test]
fn mutation_replay_empty_dir_returns_empty_vec_not_err() {
    let dir = tmp("empty");
    // A mutant that turns `Ok(Vec::new())` into `Err(...)` would
    // break first-boot (no WAL dir yet).
    let recovered = replay_all(&dir).unwrap();
    assert!(recovered.is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn mutation_replay_nonexistent_dir_returns_empty_vec_not_err() {
    let dir = std::env::temp_dir().join(format!(
        "tv-mut-nonexistent-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    // A mutant that panics / errors on missing dir breaks first boot.
    let recovered = replay_all(&dir).unwrap();
    assert!(recovered.is_empty());
}
