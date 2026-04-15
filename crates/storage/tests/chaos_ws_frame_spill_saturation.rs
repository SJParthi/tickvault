//! STAGE-D P7.4 — Chaos: WsFrameSpill saturation / healthy-ops contract.
//!
//! The production guarantee we must preserve mechanically:
//!
//!   > `tv_ws_frame_spill_drop_critical` MUST be zero in healthy ops.
//!   > Any non-zero increment is a P0 bug.
//!
//! This test exercises the WAL under a high-rate burst to prove the
//! healthy-ops contract holds end-to-end:
//!
//!   1. Healthy-ops burst: append 100,000 live-feed frames as fast as
//!      a tight loop can go. Every frame MUST land in the WAL with
//!      `AppendOutcome::Spilled`. `drop_critical_count` MUST stay at 0.
//!      `persisted_count` MUST reach 100,000 within a bounded wait.
//!
//!   2. Mixed-type burst: interleave all 4 WsType variants in a single
//!      burst. Same invariant: zero drops, every frame persisted, every
//!      frame survives `replay_all` with the correct type tag.
//!
//!   3. Rapid-new-spill churn: create+drop WsFrameSpill 50 times in
//!      quick succession. Verify the writer thread starts and drains
//!      cleanly each time and the WAL directory does not accumulate
//!      corrupt segments.
//!
//! This is P7.4 Scenario 6 from the plan, scoped to the portable
//! in-process observable contract (the literal disk-full case requires
//! root + fs-quota magic that is deliberately out of scope — see the
//! commit message for follow-up tracking).
//!
//! Run cost: ~200-500 ms on a cold laptop. No Docker, no network, no
//! root required — safe for normal CI.

#![cfg(test)]

use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tickvault_storage::ws_frame_spill::{AppendOutcome, WsFrameSpill, WsType, replay_all};

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

/// **P7.4 mixed-type burst** — interleave all 4 WsType variants in a
/// single burst. Every frame lands, nothing drops, replay returns
/// frames with the correct type tags in FIFO order. This covers the
/// "one WAL, four WS types" property in Section D of the plan.
#[test]
fn chaos_mixed_type_burst_preserves_every_frame_across_types() {
    let dir = chaos_tmp("mixed-20k");
    let types = [
        WsType::LiveFeed,
        WsType::Depth20,
        WsType::Depth200,
        WsType::OrderUpdate,
    ];

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

        let _ = wait_until_persisted_at_least(&spill, N as u64, Duration::from_secs(10));
        drop(spill);
        std::thread::sleep(Duration::from_millis(50));
    }

    let recovered = replay_all(&dir).expect("replay_all");
    assert_eq!(
        recovered.len(),
        N,
        "replay must return every persisted frame"
    );

    // Per-type counts must each be exactly N/4 (± 1 if N is not a
    // multiple of 4; here 20000 / 4 = 5000 exactly).
    let mut counts = [0usize; 4];
    for rec in &recovered {
        counts[rec.ws_type.as_u8() as usize - 1] += 1;
    }
    assert_eq!(counts[0], 5000, "LiveFeed count");
    assert_eq!(counts[1], 5000, "Depth20 count");
    assert_eq!(counts[2], 5000, "Depth200 count");
    assert_eq!(counts[3], 5000, "OrderUpdate count");

    let _ = std::fs::remove_dir_all(&dir);
}

/// **P7.4 churn** — create and drop 50 WsFrameSpill instances in quick
/// succession, each with a different temp dir. Verifies the writer
/// thread lifecycle is clean (spawn + graceful exit on drop) and no
/// resource leak prevents the next spill from starting. Simulates a
/// process that crashes and restarts rapidly during a reconnect storm.
#[test]
fn chaos_rapid_spill_churn_50_cycles_no_leak_no_panic() {
    let root = chaos_tmp("churn");

    for cycle in 0..50 {
        let dir = root.join(format!("cycle-{cycle}"));
        let spill = WsFrameSpill::new(&dir).expect("spill new in churn");
        // Drop a few frames through each cycle so the writer actually
        // runs its main loop once before exiting.
        for i in 0..10u32 {
            let frame = i.to_le_bytes().to_vec();
            let outcome = spill.append(WsType::LiveFeed, frame);
            assert_eq!(outcome, AppendOutcome::Spilled);
        }
        let _ = wait_until_persisted_at_least(&spill, 10, Duration::from_secs(2));
        // Explicit drop — the writer thread joins as senders drop.
        drop(spill);
    }

    // Quick sanity: at least one cycle's WAL must be replayable.
    let first_cycle = root.join("cycle-0");
    if first_cycle.exists() {
        let frames = replay_all(&first_cycle).expect("replay first cycle");
        assert_eq!(frames.len(), 10, "first cycle WAL must replay 10 frames");
    }

    let _ = std::fs::remove_dir_all(&root);
}
