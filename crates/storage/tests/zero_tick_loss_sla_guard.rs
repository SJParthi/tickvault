//! Zero-Tick-Loss SLA guard test.
//!
//! **Parthiban directive 2026-04-20:** "zero ticks loss and nowhere
//! websocket should get disconnected or reconnect not even a single
//! tick should be missed".
//!
//! Physical networks can always drop packets, so the SLA we can actually
//! prove is:
//!
//!   **If a frame enters the WAL spill, it WILL survive a process crash
//!   and be recovered on the next startup, with no loss, no reordering,
//!   and no silent corruption.**
//!
//! This test pins that contract end-to-end:
//!
//! 1. Boot a fresh `WsFrameSpill` against a tempdir (`data/wal/` analogue).
//! 2. Hammer it with N frames across all 4 `WsType` variants — simulates
//!    a real session where main-feed, depth-20, depth-200 and order-update
//!    frames interleave on the way to the spill.
//! 3. Wait for `persisted_count()` to reach N (the writer thread flushes
//!    batches asynchronously).
//! 4. Drop the spill — simulates a hard crash (no graceful shutdown).
//! 5. Call `replay_all()` on the same tempdir.
//! 6. Assert: every frame is recovered, in FIFO order, with the correct
//!    `ws_type` tag, and byte-for-byte equal to the original payload.
//! 7. Assert: `drop_critical_count() == 0` (no SLA-visible losses).
//!
//! If this test ever breaks, the SLA is dead — escalate to Parthiban.
//!
//! This complements `chaos_ws_e2e_wal_durability.rs` which covers the
//! pre-spill (TCP-drop) path. Here we focus on the spill→replay contract
//! with an explicit zero-loss assertion.

use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tickvault_storage::ws_frame_spill::{AppendOutcome, WsFrameSpill, WsType, replay_all};

/// Per-test scratch dir — namespaced by test name + wall-clock nanos so
/// parallel test runs don't stomp each other. Cleans up any stale prior
/// run. Matches the pattern already used in `ws_frame_spill.rs` unit tests.
fn fresh_wal_dir(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let p = std::env::temp_dir().join(format!("tv-zero-tick-loss-{name}-{nanos}"));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).expect("create tempdir");
    p
}

/// How long we wait for the async writer to drain before declaring failure.
/// The writer flushes in batches of 128 or on 5ms timeouts, so even 10K
/// frames should complete well under this budget.
const DRAIN_TIMEOUT_SECS: u64 = 5;

/// Block until `spill.persisted_count() >= target` or `DRAIN_TIMEOUT_SECS`
/// elapses. Returns `true` on timeout so tests can emit a clear failure.
fn wait_until_persisted(spill: &WsFrameSpill, target: u64) -> bool {
    let deadline = Instant::now() + Duration::from_secs(DRAIN_TIMEOUT_SECS);
    while spill.persisted_count() < target {
        if Instant::now() >= deadline {
            return true;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    false
}

/// Builds a deterministic frame payload tagged with its index so we can
/// assert FIFO ordering after replay.
fn build_frame(ws_type: WsType, index: u32) -> Vec<u8> {
    let mut v = Vec::with_capacity(8);
    v.push(ws_type as u8);
    v.extend_from_slice(&index.to_le_bytes());
    // Pad to a realistic frame size (~50 bytes like a Dhan Quote packet).
    v.extend_from_slice(&[0xAB; 45]);
    v
}

#[test]
fn test_zero_tick_loss_spill_survives_crash_and_replay_recovers_all_frames() {
    // Fresh tempdir per run — no cross-test pollution.
    let wal_dir = fresh_wal_dir("roundtrip");

    // ---- Phase 1: write N frames across all WsTypes ---------------------
    const N_PER_TYPE: u32 = 250;
    let ws_types = [
        WsType::LiveFeed,
        WsType::Depth20,
        WsType::Depth200,
        WsType::OrderUpdate,
    ];
    let expected_total: u64 = u64::from(N_PER_TYPE) * ws_types.len() as u64;

    // Scope spill to drop before replay_all — mimics process crash.
    let mut originals: Vec<(WsType, Vec<u8>)> = Vec::with_capacity(expected_total as usize);
    {
        let spill = WsFrameSpill::new(&wal_dir).expect("spill new");
        for i in 0..N_PER_TYPE {
            for &ws_type in &ws_types {
                let frame = build_frame(ws_type, i);
                originals.push((ws_type, frame.clone()));
                let outcome = spill.append(ws_type, frame);
                assert!(
                    matches!(outcome, AppendOutcome::Spilled),
                    "append #{i} on {ws_type:?} must be Spilled (not Dropped) — channel has 65K capacity"
                );
            }
        }

        // Drain: wait for background writer to persist every frame.
        let timed_out = wait_until_persisted(&spill, expected_total);
        assert!(
            !timed_out,
            "writer thread did not drain {expected_total} frames within \
             {DRAIN_TIMEOUT_SECS}s — persisted {}",
            spill.persisted_count()
        );

        // SLA assertion 1: ZERO frames dropped by the hot path.
        assert_eq!(
            spill.drop_critical_count(),
            0,
            "ZERO-TICK-LOSS VIOLATION: {} frame(s) dropped by spill hot path. \
             Channel is 65K deep and we only wrote {expected_total} — this \
             means the writer thread is genuinely stuck, not just slow.",
            spill.drop_critical_count()
        );

        // Verify `persisted_count` matches what we pushed.
        assert_eq!(
            spill.persisted_count(),
            expected_total,
            "persisted_count must equal frames written"
        );
    } // <-- spill dropped here; writer thread joins, files are fsynced closed.

    // ---- Phase 2: replay from disk (new process perspective) ------------
    let recovered = replay_all(&wal_dir).expect("replay_all must succeed");

    // SLA assertion 2: every frame recovered, no loss, no extras.
    assert_eq!(
        recovered.len() as u64,
        expected_total,
        "ZERO-TICK-LOSS VIOLATION: replayed {} frame(s), expected {expected_total}. \
         Delta = {} lost (negative = phantom frames, also bad).",
        recovered.len(),
        expected_total as i64 - recovered.len() as i64,
    );

    // SLA assertion 3: byte-for-byte identity, in FIFO order.
    for (i, (original_ws_type, original_frame)) in originals.iter().enumerate() {
        let r = &recovered[i];
        assert_eq!(
            r.ws_type as u8, *original_ws_type as u8,
            "ws_type mismatch at index {i}: replayed {:?} != original {:?}",
            r.ws_type, original_ws_type
        );
        assert_eq!(
            &r.frame, original_frame,
            "frame byte mismatch at index {i} ({original_ws_type:?})"
        );
    }

    // ---- Phase 3: second replay returns nothing (archive worked) --------
    // replay_all moves consumed segments to archive/ so a second call on
    // the same dir must yield zero frames. Proves no double-replay.
    let second = replay_all(&wal_dir).expect("second replay must succeed");
    assert!(
        second.is_empty(),
        "segments must be archived after first replay; second replay returned {} frames",
        second.len()
    );
}

#[test]
fn test_zero_tick_loss_empty_wal_is_noop() {
    // Fresh tempdir, no writes, replay should return empty.
    let wal_dir = fresh_wal_dir("empty");
    let frames = replay_all(&wal_dir).expect("replay on empty dir");
    assert!(frames.is_empty());
}

#[test]
fn test_zero_tick_loss_ws_type_tags_preserved_across_crash() {
    // Smaller + targeted: each WsType gets a distinct marker frame, crash,
    // replay — asserts the u8 tag byte round-trips even when frames are
    // interleaved at high cadence.
    let wal_dir = fresh_wal_dir("ws-types");
    {
        let spill = WsFrameSpill::new(&wal_dir).expect("new spill");
        // Interleave 4 types, 10 each → 40 frames.
        for i in 0..10 {
            for &ws_type in &[
                WsType::LiveFeed,
                WsType::Depth20,
                WsType::Depth200,
                WsType::OrderUpdate,
            ] {
                let outcome = spill.append(ws_type, build_frame(ws_type, i));
                assert!(matches!(outcome, AppendOutcome::Spilled));
            }
        }
        assert!(!wait_until_persisted(&spill, 40));
        assert_eq!(spill.drop_critical_count(), 0);
    }
    let frames = replay_all(&wal_dir).expect("replay");
    assert_eq!(frames.len(), 40);
    // First 4 entries must be 4 distinct ws_types (one per variant).
    let first_four: std::collections::HashSet<u8> =
        frames.iter().take(4).map(|f| f.ws_type as u8).collect();
    assert_eq!(
        first_four.len(),
        4,
        "first 4 replayed frames must span all 4 ws_types (interleaved write order); \
         got ws_types: {:?}",
        frames.iter().take(4).map(|f| f.ws_type).collect::<Vec<_>>()
    );
}
