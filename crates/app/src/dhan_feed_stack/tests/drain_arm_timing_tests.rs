//! 2026-11-16 drain tests for the `drain_arm_*` hot-path stages. A child of
//! `dhan_feed_stack.rs`'s `mod tests`, in its own file to keep
//! `dhan_feed_stack.rs` under its size pin (`source_size_ratchet_guard`).
//!
//! The exact per-arm check is the source-scan guard
//! `crates/app/tests/drain_arm_timing_guard.rs`; this is its behaviour half.

use super::*;

/// Regression: 2026-11-16. On the box, `ring_dwell` and `receipt_to_fold`
/// read over 2 ms in 292 of 376 minutes while `drain_main_frame` and
/// `drain_frame_post` stayed under 0.4 ms: frames waited behind drain work
/// no stage timed. Running the real drain with a seed batch and an open,
/// empty ring fires the flush, catch-up and silence timers (their first tick
/// is immediate), the seed arm and the idle arm (the queued seed batch);
/// each must record its own stage before the shutdown ends the drain.
///
/// The stages are process-wide and other drain tests fire the same timer
/// arms, so a run in parallel can move a stage this drain did not; the
/// source-scan guard is the exact per-arm check. Run alone, this test fails
/// when any arm's timer is removed.
#[tokio::test]
async fn test_regression_every_non_frame_drain_arm_records_its_own_stage() {
    use tickvault_storage::hot_path_telemetry::{DRAIN_ARM_STAGES, stage_snapshot};
    let ingest = LiveIngest::new(TickWriter::for_test(Feed::Dhan), 4).with_detector_capacity(64);
    let (tx, rx) = tickvault_core::websocket::frame_ring::frame_ring::<CapturedFrame>(8);
    let (seed_tx, seed_rx) = tokio::sync::mpsc::channel(1);
    let seed: Vec<SubscribeInstrument> = (0..3_u64)
        .map(|i| SubscribeInstrument {
            security_id: 71_000 + i,
            segment: ExchangeSegment::NseFno,
        })
        .collect();
    seed_tx.send(seed).await.expect("the seed channel has room");
    let shutdown = Arc::new(tokio::sync::Notify::new());
    let before = DRAIN_ARM_STAGES.map(|s| stage_snapshot(s).count);
    let moved = || {
        DRAIN_ARM_STAGES
            .iter()
            .zip(before)
            .filter(|(s, b)| stage_snapshot(**s).count <= *b)
            .map(|(s, _)| s.as_str())
            .collect::<Vec<_>>()
    };
    let driver = async {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let still = moved();
            if still.is_empty() || std::time::Instant::now() >= deadline {
                shutdown.notify_one();
                return still;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    };
    let drain = run_frame_drain(
        rx,
        ingest,
        Arc::new(RingByteBudget::new(MAIN_FEED_RING_MAX_BYTES)),
        Arc::new(RingByteBudget::new(DEPTH_RING_MAX_BYTES)),
        Arc::new(RingByteBudget::new(DEPTH_RING_MAX_BYTES)),
        Arc::clone(&shutdown),
        Arc::new(tickvault_common::feed_health::FeedHealthRegistry::new()),
        seed_rx,
    );
    let (unmoved, _outcome) = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        tokio::join!(driver, drain)
    })
    .await
    .expect("the drain must end on the shutdown signal");
    drop(tx);
    drop(seed_tx);
    assert!(
        unmoved.is_empty(),
        "every non-frame drain arm must record its own stage; these did not: {unmoved:?}"
    );
}
