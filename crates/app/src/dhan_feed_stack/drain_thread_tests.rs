//! Plan item DT-S1 (2026-10-30): the REAL `run_frame_drain`, spawned through
//! `drain_thread::spawn_frame_drain_with` on the shared runtime and on its
//! own OS thread, produces the same outcome and byte-identical `ticks` rows.
//! A sibling file, to keep `dhan_feed_stack.rs` under its size pin.

use super::*;
use crate::drain_thread::{DrainThreadPlan, spawn_frame_drain_with};
use tickvault_common::feed::Feed;

/// 09:16:40 IST today, IST epoch seconds: inside the candle session.
fn todays_in_session_ltt() -> u32 {
    let ist_now = chrono::Utc::now().timestamp().saturating_add(i64::from(
        tickvault_common::constants::IST_UTC_OFFSET_SECONDS,
    ));
    let ist_midnight = ist_now - ist_now.rem_euclid(86_400);
    u32::try_from((ist_midnight + 33_400).max(0)).unwrap_or(0)
}

fn ticker_packet(security_id: u32, ltp: f32, ltt: u32) -> [u8; 16] {
    let mut p = [0u8; 16];
    p[0] = 2;
    p[1] = 16;
    p[3] = 0; // IDX_I
    p[4..8].copy_from_slice(&security_id.to_le_bytes());
    p[8..12].copy_from_slice(&ltp.to_le_bytes());
    p[12..16].copy_from_slice(&ltt.to_le_bytes());
    p
}

/// A fixed stream: four instruments, prices and trade seconds moving, and
/// one unparseable frame every 97th, so every counter has something in it.
fn frames() -> Vec<CapturedFrame> {
    let ltt = todays_in_session_ltt();
    let base_nanos = i64::from(ltt) * 1_000_000_000;
    (0..2_000_u32)
        .map(|i| {
            let bytes = if i % 97 == 96 {
                bytes::Bytes::from_static(&[0xEE, 0x03, 0x00])
            } else {
                bytes::Bytes::copy_from_slice(&ticker_packet(
                    [13, 25, 51, 21][(i % 4) as usize],
                    23_000.0 + (i % 50) as f32 * 0.05,
                    ltt + i / 40,
                ))
            };
            CapturedFrame {
                seq: u64::from(i) + 1,
                endpoint: DhanEndpointType::MainFeed,
                connection_index: 0,
                received_at: std::time::Instant::now(),
                received_at_nanos: base_nanos + i64::from(i) * 1_000_000,
                wal_backed: true,
                bytes,
            }
        })
        .collect()
}

/// Runs the real drain over [`frames`] at `plan`; returns its counters, the
/// rows still pending, and every byte the writer handed off.
async fn run(plan: DrainThreadPlan) -> ([u64; 7], u64, Vec<u8>, bool) {
    let (producer, sink, batches) = TickWriter::for_test(Feed::Dhan).split_for_offload();
    drop(sink); // the batches are read off the queue below
    let collector = std::thread::spawn(move || {
        let mut all = Vec::new();
        while let Ok(batch) = batches.recv() {
            all.extend_from_slice(batch.ilp_bytes());
        }
        all
    });
    let stream = frames();
    let (tx, rx) = tickvault_core::websocket::frame_ring::frame_ring::<CapturedFrame>(4_096);
    for frame in stream {
        tx.try_send(frame).expect("ring room");
    }
    drop(tx); // the drain ends when the ring is empty and closed
    let join = spawn_frame_drain_with(
        run_frame_drain(
            rx,
            LiveIngest::new(producer, 8),
            Arc::new(RingByteBudget::new(MAIN_FEED_RING_MAX_BYTES)),
            Arc::new(RingByteBudget::new(DEPTH_RING_MAX_BYTES)),
            Arc::new(RingByteBudget::new(DEPTH_RING_MAX_BYTES)),
            Arc::new(tokio::sync::Notify::new()),
            Arc::new(tickvault_common::feed_health::FeedHealthRegistry::new()),
            tokio::sync::mpsc::channel(1).1,
        ),
        plan,
        None,
        None,
    );
    let own = join.is_own_thread();
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(30), join.join())
        .await
        .expect("the drain must end once the ring is closed and empty")
        .expect("the drain must return its outcome");
    let counters = [
        outcome.frames_seen,
        outcome.folded,
        outcome.depth_unconsumed,
        outcome.depth_rows,
        outcome.depth_refused,
        outcome.depth_dropped,
        outcome.unparseable,
    ];
    let pending = outcome.ingest.pending_rows();
    drop(outcome); // drops the producer, so the collector's queue closes
    (counters, pending, collector.join().expect("collector"), own)
}

#[test]
fn test_drain_on_its_own_thread_matches_the_shared_runtime_byte_for_byte() {
    // The drain adopts the process-global backup set; held so a concurrent
    // backup test never sees this drain take its publication.
    let _guard = crate::main_feed_backup::TEST_PUBLISH_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("test runtime");
    let (shared_counts, shared_pending, shared_rows, shared_own) =
        rt.block_on(run(DrainThreadPlan::Shared));
    let (own_counts, own_pending, own_rows, own_own) = rt.block_on(run(DrainThreadPlan::Own(None)));
    assert!(!shared_own && own_own, "each run must use its placement");
    // Non-vacuity: frames folded, rows written, the bad frames refused.
    assert_eq!(shared_counts[0], 2_000, "every frame is seen");
    assert!(shared_counts[1] > 1_900, "frames folded: {shared_counts:?}");
    assert!(
        shared_counts[6] >= 20,
        "unparseable counted: {shared_counts:?}"
    );
    assert!(!shared_rows.is_empty(), "the drain must hand rows off");
    assert_eq!(
        shared_counts, own_counts,
        "drain counters differ by placement"
    );
    assert_eq!(shared_pending, own_pending);
    assert_eq!(
        shared_rows.len(),
        own_rows.len(),
        "row bytes differ in length by placement"
    );
    assert!(
        shared_rows == own_rows,
        "the ticks rows must be byte-identical on both placements"
    );
}
