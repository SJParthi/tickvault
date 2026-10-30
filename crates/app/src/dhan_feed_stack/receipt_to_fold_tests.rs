//! Plan item DT0 (2026-10-29): the drain records the `receipt_to_fold`
//! hot-path stage (a main-feed frame's socket receipt → folded) once per
//! main-feed frame. A sibling file, to keep `dhan_feed_stack.rs` under its
//! size pin.

use super::*;
use tickvault_common::feed::Feed;
use tickvault_storage::hot_path_telemetry::{
    FINE_BUCKETS, Stage, receipt_to_fold_fine_snapshot, stage_snapshot,
};

/// 09:16:40 IST today, as epoch seconds (inside the candle session window).
fn todays_in_session_ltt() -> u32 {
    let ist_now = chrono::Utc::now().timestamp().saturating_add(i64::from(
        tickvault_common::constants::IST_UTC_OFFSET_SECONDS,
    ));
    let ist_midnight = ist_now - ist_now.rem_euclid(86_400);
    u32::try_from((ist_midnight + 33_400).max(0)).unwrap_or(0)
}

/// One IDX_I ticker packet.
fn ticker_packet(security_id: u32, ltp: f32, ltt: u32) -> [u8; 16] {
    let mut p = [0u8; 16];
    p[0] = 2; // ticker
    p[1] = 16;
    p[3] = 0; // IDX_I
    p[4..8].copy_from_slice(&security_id.to_le_bytes());
    p[8..12].copy_from_slice(&ltp.to_le_bytes());
    p[12..16].copy_from_slice(&ltt.to_le_bytes());
    p
}

/// Every main-feed frame the drain folds records one `receipt_to_fold`
/// sample measured from the frame's OWN receipt instant. The frames are
/// stamped 20 s in the past, so each lands in the fine histogram's top
/// (above 16.8 ms) bucket, which no other test in this binary fills by the
/// hundreds; the stage is a process-wide static, so the deltas are checked
/// as AT LEAST one sample per frame.
#[tokio::test]
async fn test_receipt_to_fold_stage_records_one_sample_per_frame() {
    const FRAMES: u64 = 5;
    let Some(received_at) =
        std::time::Instant::now().checked_sub(std::time::Duration::from_secs(20))
    else {
        // A host up for under 20 s cannot express the past instant.
        return;
    };
    let ingest = LiveIngest::new(TickWriter::for_test(Feed::Dhan), 4);
    let (tx, rx) = tickvault_core::websocket::frame_ring::frame_ring::<CapturedFrame>(8);
    let ltt = todays_in_session_ltt();
    for k in 0..FRAMES {
        tx.try_send(CapturedFrame {
            seq: 900 + k,
            endpoint: DhanEndpointType::MainFeed,
            connection_index: 0,
            received_at,
            received_at_nanos: tickvault_storage::ws_frame_spill::receipt_nanos_from(
                std::time::Instant::now(),
            ),
            wal_backed: true,
            bytes: bytes::Bytes::copy_from_slice(&ticker_packet(
                13,
                23_146.45,
                ltt.saturating_add(u32::try_from(k).unwrap_or(0)),
            )),
        })
        .expect("the ring must accept a frame");
    }
    drop(tx);
    let before = stage_snapshot(Stage::ReceiptToFold);
    let fine_before = receipt_to_fold_fine_snapshot();
    let drained = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        run_frame_drain(
            rx,
            ingest,
            Arc::new(RingByteBudget::new(MAIN_FEED_RING_MAX_BYTES)),
            Arc::new(RingByteBudget::new(DEPTH_RING_MAX_BYTES)),
            Arc::new(RingByteBudget::new(DEPTH_RING_MAX_BYTES)),
            Arc::new(tokio::sync::Notify::new()),
            Arc::new(tickvault_common::feed_health::FeedHealthRegistry::new()),
            tokio::sync::mpsc::channel(1).1,
        ),
    )
    .await
    .expect("the drain must end when the ring closes");
    let after = stage_snapshot(Stage::ReceiptToFold);
    let fine_after = receipt_to_fold_fine_snapshot();
    assert_eq!(drained.folded, FRAMES, "every frame folded");
    assert!(
        after.count.saturating_sub(before.count) >= FRAMES,
        "each main-feed frame records one receipt_to_fold sample: {} -> {}",
        before.count,
        after.count
    );
    assert!(
        after.over_budget.saturating_sub(before.over_budget) >= FRAMES,
        "a frame received 20 s ago is over the 1 ms budget"
    );
    let top = FINE_BUCKETS - 1;
    assert!(
        fine_after[top].saturating_sub(fine_before[top]) >= FRAMES,
        "measured from the frame's receipt, not from the drain's pick-up"
    );
}
