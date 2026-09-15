//! STAGE-D P7.4 — bounded healthy WAL workload checks.
//!
//! The 100,000-frame case checks admission, drop accounting and the writer's
//! persisted-record counter. The 20,000-frame LiveFeed/OrderUpdate case also
//! compares every replayed payload and type. The 50-instance case checks ten
//! exact replayed payloads and increasing frame sequences in every directory.
//!
//! These are in-process tests on temporary directories. They do not inject
//! process crashes, measure resource leaks, prove power-loss durability or
//! establish production latency.

#![cfg(test)]

#[path = "support/owned_wal_replay.rs"]
mod owned_wal_replay;
use owned_wal_replay::{assert_complete_replay, claim_wal};

use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tickvault_storage::ws_frame_spill::{AppendOutcome, WsFrameSpill, WsType};

fn chaos_tmp(tag: &str) -> std::path::PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let p = std::env::temp_dir().join(format!(
        "tv-wal-spill-chaos-{tag}-{}-{nanos}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).expect("create chaos temp dir"); // APPROVED: test
    p
}

/// Poll `persisted_count()` until it reaches `target` or a deadline
/// expires. Returns the observed count.
fn wait_until_persisted_at_least(spill: &WsFrameSpill, target: u64, budget: Duration) -> u64 {
    let deadline = Instant::now() + budget;
    while Instant::now() < deadline {
        let count = spill.persisted_count();
        if count >= target {
            return count;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    spill.persisted_count()
}

/// **P7.4 healthy-ops contract** — 100,000 live-feed frames in a tight
/// loop MUST land in the WAL with zero drops. This proves the
/// `tv_ws_frame_spill_drop_critical` metric is a true safety floor
/// (only fires when the disk writer is actually dead).
#[test]
fn chaos_healthy_ops_burst_100k_frames_zero_drops() {
    let dir = chaos_tmp("healthy-100k");
    let spill = Arc::new(WsFrameSpill::new(&dir).expect("spill new"));

    const N: usize = 100_000;
    let mut observed_dropped = 0u64;

    for i in 0..N {
        // 16-byte ticker-sized payload — tiny, so the writer thread has
        // zero back-pressure from disk. If `drop_critical` fires under
        // this workload the WAL has a real bug (not a test artifact).
        let mut frame = vec![0u8; 16];
        frame[0..4].copy_from_slice(&(i as u32).to_le_bytes());
        let outcome = spill.append(WsType::LiveFeed, frame);
        if outcome == AppendOutcome::Dropped {
            observed_dropped += 1;
        }
    }

    assert_eq!(
        observed_dropped, 0,
        "healthy-ops contract violated: {observed_dropped} of {N} frames dropped \
         (expected 0). If this fails, tv_ws_frame_spill_drop_critical will fire \
         in production on the first busy market open — P0 bug."
    );
    assert_eq!(
        spill.drop_critical_count(),
        0,
        "drop_critical counter must stay at zero in healthy ops"
    );

    // Writer thread must persist every frame within a generous
    // window. 100k small frames hit disk in well under 2 seconds.
    let final_count = wait_until_persisted_at_least(&spill, N as u64, Duration::from_secs(10));
    assert_eq!(
        final_count, N as u64,
        "writer thread must persist every enqueued frame; got {final_count}/{N}"
    );

    drop(spill);
    std::thread::sleep(Duration::from_millis(50));
    let _ = std::fs::remove_dir_all(&dir);
}

/// **P7.4 mixed-type burst** — interleave the surviving WsType variants
/// (LiveFeed + OrderUpdate, after PR #4 retired Depth20/Depth200) in a
/// single burst. Every frame lands, nothing drops, replay returns
/// frames with the correct type tags in FIFO order.
#[test]
fn chaos_mixed_type_burst_preserves_every_frame_across_types() {
    let dir = chaos_tmp("mixed-20k");
    let types = [WsType::LiveFeed, WsType::OrderUpdate];

    const N: usize = 20_000;

    {
        let spill = Arc::new(WsFrameSpill::new(&dir).expect("spill new"));

        let mut dropped = 0u64;
        for i in 0..N {
            let ws_type = types[i % types.len()];
            // Each ws_type gets a distinctive 8-byte payload so we can
            // audit per-type counts on replay.
            let mut frame = vec![0u8; 8];
            frame[0] = ws_type.as_u8();
            frame[1..5].copy_from_slice(&(i as u32).to_le_bytes());
            if spill.append(ws_type, frame) == AppendOutcome::Dropped {
                dropped += 1;
            }
        }
        assert_eq!(dropped, 0, "mixed burst must not drop");
        assert_eq!(spill.drop_critical_count(), 0);

        assert_eq!(
            wait_until_persisted_at_least(&spill, N as u64, Duration::from_secs(10)),
            N as u64
        );
        drop(spill);
        std::thread::sleep(Duration::from_millis(50));
    }

    let dir_owner = claim_wal(&dir);
    let recovered_batch = assert_complete_replay(&dir_owner);
    let recovered = &recovered_batch.frames;
    assert_eq!(
        recovered.len(),
        N,
        "replay must return every persisted frame"
    );

    // Per-type counts must each be exactly N/2 (here 20000 / 2 = 10000).
    let mut live = 0usize;
    let mut ord = 0usize;
    let mut truedata = 0usize;
    for (index, rec) in recovered.iter().enumerate() {
        let expected_type = types[index % types.len()];
        let mut expected_frame = vec![0u8; 8];
        expected_frame[0] = expected_type.as_u8();
        expected_frame[1..5].copy_from_slice(&(index as u32).to_le_bytes());
        assert_eq!(rec.ws_type, expected_type, "type at FIFO index {index}");
        assert_eq!(
            rec.frame, expected_frame,
            "complete payload at FIFO index {index}"
        );
        match rec.ws_type {
            WsType::LiveFeed => live += 1,
            WsType::OrderUpdate => ord += 1,
            WsType::TruedataFeed => truedata += 1,
        }
    }
    assert!(
        recovered
            .windows(2)
            .all(|pair| pair[0].frame_seq < pair[1].frame_seq)
    );
    assert_eq!(live, 10_000, "LiveFeed count");
    assert_eq!(ord, 10_000, "OrderUpdate count");
    assert_eq!(
        truedata, 0,
        "this fixture appends no TrueData frames — a non-zero count means \
         replay mis-routed a transport tag"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// Create and close 50 writers in separate temporary directories. Every
/// cycle must return its complete payloads through owned replay. This tests
/// repeated ownership handoff, not process crashes or measured resource leaks.
#[test]
fn chaos_rapid_spill_churn_50_cycles_preserves_every_payload() {
    let root = chaos_tmp("churn");

    for cycle in 0..50 {
        let dir = root.join(format!("cycle-{cycle}"));
        let spill = WsFrameSpill::new(&dir).expect("spill new in churn");
        for i in 0..10u32 {
            assert_eq!(
                spill.append(WsType::LiveFeed, i.to_le_bytes().to_vec()),
                AppendOutcome::Spilled
            );
        }
        assert_eq!(
            wait_until_persisted_at_least(&spill, 10, Duration::from_secs(2)),
            10,
            "cycle {cycle} must flush every admitted record"
        );
        drop(spill);

        let owner = claim_wal(&dir);
        let batch = assert_complete_replay(&owner);
        assert_eq!(
            batch.frames.len(),
            10,
            "cycle {cycle} must replay ten records"
        );
        for (index, record) in batch.frames.iter().enumerate() {
            assert_eq!(
                record.ws_type,
                WsType::LiveFeed,
                "cycle {cycle}, index {index}"
            );
            assert_eq!(
                record.frame,
                (index as u32).to_le_bytes(),
                "complete payload at cycle {cycle}, FIFO index {index}"
            );
        }
        assert!(
            batch
                .frames
                .windows(2)
                .all(|pair| pair[0].frame_seq < pair[1].frame_seq),
            "cycle {cycle} must preserve increasing frame identities"
        );
    }

    let _ = std::fs::remove_dir_all(&root);
}
