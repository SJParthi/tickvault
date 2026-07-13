//! Bounded, chunked, backpressured WAL frame re-injection (STAGE-C.2b).
//!
//! C3 fix (2026-07-03): the boot-time WAL replay used to drain its entire
//! recovered-frame `Vec` through a synchronous `try_send` loop into the
//! pool's frame channel (`FRAME_CHANNEL_CAPACITY` = 131,072) — everything
//! past the capacity was silently dropped (observed `dropped=1,127,801` at
//! 10:35 IST on 2026-07-03), the re-injection was marked NOT-clean, so
//! `confirm_replayed()` never archived the staged WAL segments and they
//! re-replayed AND GREW on every restart: a self-feeding storm.
//!
//! [`reinject_wal_frames`] replaces both call sites (fast boot + the
//! `start_dhan_lane` slow-boot mirror) with:
//!
//! 1. **Backpressure, never drop** — `sender.send(frame).await` waits for
//!    the tick-processor consumer while the channel is open. This is a
//!    COLD, once-per-boot recovery path (NOT the per-tick hot path), so
//!    awaiting is correct.
//! 2. **Chunked yielding** — every [`WAL_REINJECT_CHUNK_SIZE`] delivered
//!    frames the injector calls `tokio::task::yield_now()` so the live WS
//!    read loop and the tick processor keep getting scheduled; a 1M+ frame
//!    replay must never monopolize the runtime.
//! 3. **Bounded stall protection** — each send is wrapped in
//!    `tokio::time::timeout`. Only a truly dead (channel closed) or wedged
//!    (zero progress for the timeout) consumer aborts the run; the abort is
//!    typed-paged (WS-REINJECT-01) and the remaining frames stay staged in
//!    the WAL `replaying/` directory for re-replay next boot — fail-closed,
//!    no silent loss.
//!
//! A fully delivered replay returns [`ReinjectOutcome::clean`] `== true`,
//! which lets the boot path call `confirm_replayed()` and archive the WAL
//! segments — breaking the re-replay-grows-forever loop.
//!
//! O(1) per frame: `Vec` items are MOVED into the channel (no byte clone),
//! no per-frame allocation, no `format!` in the loop.
//!
//! Runbook: `.claude/rules/project/ws-reinject-error-codes.md`.

use std::time::Duration;

use bytes::Bytes;
use tickvault_common::constants::WAL_REINJECT_PROGRESS_LOG_CHUNKS;
use tickvault_common::error_code::ErrorCode;
use tokio::sync::mpsc::Sender;
use tracing::{error, info};

/// Outcome of one [`reinject_wal_frames`] run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReinjectOutcome {
    /// Frames delivered into the channel (backpressured, never dropped).
    pub injected: u64,
    /// Frames NOT delivered because the run aborted (consumer dead or
    /// wedged). Includes the single in-flight frame whose send failed or
    /// timed out. These frames remain staged in the WAL `replaying/`
    /// directory and re-replay next boot.
    pub aborted_remaining: u64,
    /// True when every frame was delivered (`aborted_remaining == 0`).
    /// Only a clean run may `confirm_replayed()` / archive WAL segments.
    pub clean: bool,
}

/// Re-inject WAL-replayed frames into the pool's frame channel with
/// bounded, chunked backpressure (STAGE-C.2b, cold boot-recovery path).
///
/// * `chunk_size` — yield to the runtime after this many delivered frames
///   (prod call sites pass `WAL_REINJECT_CHUNK_SIZE`). Clamped to ≥ 1.
/// * `send_timeout` — per-send stall bound (prod call sites pass
///   `WAL_REINJECT_SEND_TIMEOUT_SECS`); parameterized so tests can use a
///   tiny timeout for the wedged-consumer path.
///
/// On abort (channel closed OR per-send timeout) the helper emits
/// `error!(code = "WS-REINJECT-01", …)`, increments
/// `tv_ws_frame_wal_reinject_aborted_total{reason}` +
/// `tv_ws_frame_wal_reinjected_dropped_total{ws_type="live_feed"}`
/// (semantic continuity with the pre-fix drop counter), and returns
/// `clean == false` so the caller skips `confirm_replayed()`.
///
/// ORDERING INVARIANT (C3 review CRITICAL, 2026-07-03): production callers
/// MUST spawn the frame-channel consumer (`run_tick_processor`) BEFORE
/// awaiting this helper. Without a live consumer, any replay larger than
/// `FRAME_CHANNEL_CAPACITY` fills the channel, the next send stalls for the
/// full `send_timeout`, and the run aborts NOT-clean — the WAL never
/// archives and the re-replay storm loop is NOT broken.
// TEST-EXEMPT: PR-C2 (2026-07-13) retired both main.rs re-injection call sites (and their ratchets) with the Dhan live-WS lane; the helper is retained un-consumed pending the Phase C module cleanup — behavior was pinned by the retired ratchets while consumers existed.
pub async fn reinject_wal_frames(
    sender: &Sender<(u64, Bytes)>,
    frames: Vec<(u64, Bytes)>,
    chunk_size: usize,
    send_timeout: Duration,
) -> ReinjectOutcome {
    let total = frames.len() as u64;
    let chunk = chunk_size.max(1);
    let mut injected: u64 = 0;
    let mut since_yield: usize = 0;
    let mut chunks_delivered: u64 = 0;

    for frame in frames {
        let abort_reason: &'static str =
            match tokio::time::timeout(send_timeout, sender.send(frame)).await {
                Ok(Ok(())) => {
                    injected += 1;
                    since_yield += 1;
                    if since_yield >= chunk {
                        since_yield = 0;
                        chunks_delivered += 1;
                        metrics::counter!("tv_ws_frame_wal_reinject_chunks_total").increment(1);
                        // Boot-latency honesty: a huge WAL drains inline
                        // before systemd readiness, so surface periodic
                        // progress (every ~131K frames at prod constants)
                        // for an operator tailing logs during a long boot.
                        if chunks_delivered.is_multiple_of(WAL_REINJECT_PROGRESS_LOG_CHUNKS) {
                            info!(injected, total, "WAL re-injection progress");
                        }
                        // Let the live WS read loop + tick processor run —
                        // a huge replay must not monopolize the runtime.
                        tokio::task::yield_now().await;
                    }
                    continue;
                }
                // Consumer's Receiver dropped — channel permanently closed.
                Ok(Err(_send_error)) => "channel_closed",
                // Zero progress for the whole timeout — consumer wedged.
                Err(_elapsed) => "send_timeout",
            };

        let aborted_remaining = total.saturating_sub(injected);
        error!(
            code = ErrorCode::WsReinject01Aborted.code_str(),
            reason = abort_reason,
            injected,
            aborted_remaining,
            "WS-REINJECT-01: WAL re-injection aborted — frame-channel consumer dead or \
             wedged; remaining frames stay staged in WAL replaying/ and re-replay next boot"
        );
        metrics::counter!("tv_ws_frame_wal_reinject_aborted_total", "reason" => abort_reason)
            .increment(1);
        // Semantic continuity: aborted frames are the bounded, WAL-preserved
        // successor of the pre-fix try_send drops (same operator dashboard).
        metrics::counter!(
            "tv_ws_frame_wal_reinjected_dropped_total",
            "ws_type" => "live_feed"
        )
        .increment(aborted_remaining);
        return ReinjectOutcome {
            injected,
            aborted_remaining,
            clean: false,
        };
    }

    ReinjectOutcome {
        injected,
        aborted_remaining: 0,
        clean: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc;

    /// Small static payload — the helper must move frames, so tests use
    /// `Bytes::from_static` (zero-alloc, cheap to construct in bulk).
    fn frame(seq: u64) -> (u64, Bytes) {
        (seq, Bytes::from_static(b"x"))
    }

    fn make_frames(count: u64) -> Vec<(u64, Bytes)> {
        (0..count).map(frame).collect()
    }

    /// Generous test timeout — must never fire in the healthy-path tests.
    const TEST_SEND_TIMEOUT: Duration = Duration::from_secs(30);

    /// (a) 1,000,000 frames through a FRAME_CHANNEL_CAPACITY-cap channel
    /// with an active draining consumer: ALL delivered, zero drops. This is
    /// >7x the channel capacity — the exact shape that dropped 1,127,801
    /// frames pre-fix.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_million_frames_all_delivered_zero_drops() {
        const TOTAL: u64 = 1_000_000;
        let (tx, mut rx) =
            mpsc::channel::<(u64, Bytes)>(tickvault_common::constants::FRAME_CHANNEL_CAPACITY);
        let consumer = tokio::spawn(async move {
            let mut received: u64 = 0;
            while rx.recv().await.is_some() {
                received += 1;
            }
            received
        });

        let outcome = reinject_wal_frames(
            &tx,
            make_frames(TOTAL),
            tickvault_common::constants::WAL_REINJECT_CHUNK_SIZE,
            TEST_SEND_TIMEOUT,
        )
        .await;

        assert_eq!(outcome.injected, TOTAL);
        assert_eq!(outcome.aborted_remaining, 0);
        assert!(outcome.clean);

        drop(tx); // close the channel so the consumer's recv() ends
        let received = consumer.await.expect("consumer task");
        assert_eq!(received, TOTAL, "every frame must reach the consumer");
    }

    /// (b1) Receiver dropped BEFORE the run: every send fails immediately —
    /// clean == false, nothing injected, everything aborted, no panic.
    #[tokio::test]
    async fn test_receiver_dropped_before_run_aborts_immediately() {
        let (tx, rx) = mpsc::channel::<(u64, Bytes)>(8);
        drop(rx);

        let outcome = reinject_wal_frames(&tx, make_frames(100), 8, TEST_SEND_TIMEOUT).await;

        assert!(!outcome.clean);
        assert_eq!(outcome.injected, 0);
        assert_eq!(outcome.aborted_remaining, 100);
    }

    /// (b2) Consumer dropped MID-replay: some frames delivered, then send
    /// returns Err — outcome accounts for every frame, no panic.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_consumer_dropped_mid_replay_aborts_cleanly() {
        const TOTAL: u64 = 1_000;
        let (tx, mut rx) = mpsc::channel::<(u64, Bytes)>(8);
        let consumer = tokio::spawn(async move {
            // Drain a handful, then die (drop the receiver).
            for _ in 0..50u32 {
                if rx.recv().await.is_none() {
                    break;
                }
            }
            drop(rx);
        });

        let outcome = reinject_wal_frames(&tx, make_frames(TOTAL), 8, TEST_SEND_TIMEOUT).await;

        consumer.await.expect("consumer task");
        assert!(!outcome.clean, "mid-replay consumer death must abort");
        assert!(outcome.injected < TOTAL);
        assert_eq!(
            outcome.injected + outcome.aborted_remaining,
            TOTAL,
            "every frame is either injected or counted as aborted"
        );
    }

    /// (c) Wedged consumer (receiver alive but never recv): the bounded
    /// per-send timeout fires — abort path taken, clean == false. The
    /// channel buffer (4) absorbs exactly 4 sends first.
    #[tokio::test]
    async fn test_wedged_consumer_times_out_and_aborts() {
        let (tx, rx) = mpsc::channel::<(u64, Bytes)>(4);

        let outcome = reinject_wal_frames(&tx, make_frames(10), 4, Duration::from_millis(50)).await;

        assert!(!outcome.clean);
        assert_eq!(outcome.injected, 4, "buffer absorbs exactly its capacity");
        assert_eq!(outcome.aborted_remaining, 6);
        drop(rx); // keep the receiver alive across the run, then release
    }

    /// (c2) C3 review CRITICAL, documented as a test: if NO consumer is
    /// ever spawned (the receiver exists but nothing drains — the exact
    /// shape of the pre-reorder production bug, where `run_tick_processor`
    /// was spawned AFTER the reinject await), a replay LARGER than the
    /// channel capacity fills exactly `capacity` slots, the next send
    /// stalls for the full timeout, and the run aborts NOT-clean — so the
    /// WAL never confirms and the storm loop is NOT broken. This is WHY
    /// production MUST spawn the consumer BEFORE awaiting the helper
    /// (ratcheted by `ratchet_tick_processor_spawns_before_reinject_await`).
    #[tokio::test]
    async fn test_no_consumer_times_out_and_aborts() {
        const CAPACITY: usize = 64;
        const TOTAL: u64 = 200; // > capacity, mirrors replay >> channel
        let (tx, rx) = mpsc::channel::<(u64, Bytes)>(CAPACITY);

        let outcome =
            reinject_wal_frames(&tx, make_frames(TOTAL), 16, Duration::from_millis(50)).await;

        assert!(!outcome.clean, "no consumer must abort NOT-clean");
        assert_eq!(
            outcome.injected, CAPACITY as u64,
            "exactly the channel capacity is absorbed, then the stall aborts"
        );
        assert!(outcome.aborted_remaining > 0);
        assert_eq!(outcome.injected + outcome.aborted_remaining, TOTAL);
        drop(rx); // receiver stayed alive (never polled) across the run
    }

    /// (c3) Robustness to a slight spawn race: the reinject future starts
    /// FIRST (frames > capacity), and the draining consumer is spawned
    /// ~50ms AFTER. The generous per-send timeout rides out the gap, the
    /// late consumer drains, and the run completes clean with zero aborts.
    /// (Production still spawns consumer-first; this proves the helper
    /// tolerates the inverse ordering briefly rather than aborting.)
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_consumer_spawned_after_call_still_drains() {
        const CAPACITY: usize = 64;
        const TOTAL: u64 = 5_000; // > capacity so the injector MUST stall
        let (tx, mut rx) = mpsc::channel::<(u64, Bytes)>(CAPACITY);

        let reinject = tokio::spawn(async move {
            let outcome = reinject_wal_frames(&tx, make_frames(TOTAL), 16, TEST_SEND_TIMEOUT).await;
            // tx drops here, closing the channel for the consumer below.
            outcome
        });

        // Let the reinject future run (and park on a full channel) first.
        tokio::time::sleep(Duration::from_millis(50)).await;

        let consumer = tokio::spawn(async move {
            let mut received: u64 = 0;
            while rx.recv().await.is_some() {
                received += 1;
            }
            received
        });

        let outcome = reinject.await.expect("reinject task");
        assert!(outcome.clean, "late-spawned consumer must still drain all");
        assert_eq!(outcome.injected, TOTAL);
        assert_eq!(outcome.aborted_remaining, 0);
        assert_eq!(consumer.await.expect("consumer task"), TOTAL);
    }

    /// (d) Empty vec: trivially clean, nothing injected, nothing aborted.
    /// (Named after the pub fn for the pub-fn-test-guard ratchet.)
    #[tokio::test]
    async fn test_reinject_wal_frames_empty_vec_is_clean_noop() {
        let (tx, _rx) = mpsc::channel::<(u64, Bytes)>(8);

        let outcome = reinject_wal_frames(&tx, Vec::new(), 8, TEST_SEND_TIMEOUT).await;

        assert!(outcome.clean);
        assert_eq!(outcome.injected, 0);
        assert_eq!(outcome.aborted_remaining, 0);
    }

    /// (e) Chunk-boundary exactness: frames == chunk_size and chunk_size ± 1
    /// all deliver cleanly with a draining consumer.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_chunk_boundary_exactness() {
        const CHUNK: usize = 16;
        for total in [CHUNK as u64 - 1, CHUNK as u64, CHUNK as u64 + 1] {
            let (tx, mut rx) = mpsc::channel::<(u64, Bytes)>(4);
            let consumer = tokio::spawn(async move {
                let mut received: u64 = 0;
                while rx.recv().await.is_some() {
                    received += 1;
                }
                received
            });

            let outcome =
                reinject_wal_frames(&tx, make_frames(total), CHUNK, TEST_SEND_TIMEOUT).await;

            assert!(outcome.clean, "total={total} must be clean");
            assert_eq!(outcome.injected, total);
            assert_eq!(outcome.aborted_remaining, 0);

            drop(tx);
            let received = consumer.await.expect("consumer task");
            assert_eq!(received, total, "total={total}");
        }
    }

    /// (e2) chunk_size == 0 is clamped to 1 (no panic, no infinite loop).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_zero_chunk_size_is_clamped() {
        let (tx, mut rx) = mpsc::channel::<(u64, Bytes)>(4);
        let consumer = tokio::spawn(async move {
            let mut received: u64 = 0;
            while rx.recv().await.is_some() {
                received += 1;
            }
            received
        });

        let outcome = reinject_wal_frames(&tx, make_frames(10), 0, TEST_SEND_TIMEOUT).await;

        assert!(outcome.clean);
        assert_eq!(outcome.injected, 10);
        drop(tx);
        assert_eq!(consumer.await.expect("consumer task"), 10);
    }

    /// (f) LIVENESS: while a 500K-frame replay is injected, a separate
    /// "live" producer sends frames into the SAME channel — the live frames
    /// interleave with (are not starved behind) the replay. Proves the
    /// chunked-yield + backpressure design keeps the live path scheduled.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_live_frames_interleave_with_large_replay() {
        const REPLAY_TOTAL: u64 = 500_000;
        const LIVE_TOTAL: u64 = 10;
        /// Live frames are tagged with sequence numbers far above the
        /// replay range so the consumer can classify arrivals.
        const LIVE_SEQ_BASE: u64 = u64::MAX - LIVE_TOTAL;

        let (tx, mut rx) = mpsc::channel::<(u64, Bytes)>(CHANNEL_CAP as usize);

        // Consumer: drains everything, recording the arrival index of the
        // FIRST live frame plus per-class counts.
        let consumer = tokio::spawn(async move {
            let mut arrival_index: u64 = 0;
            let mut first_live_arrival: Option<u64> = None;
            let mut live_received: u64 = 0;
            let mut replay_received: u64 = 0;
            while let Some((seq, _payload)) = rx.recv().await {
                if seq >= LIVE_SEQ_BASE {
                    live_received += 1;
                    if first_live_arrival.is_none() {
                        first_live_arrival = Some(arrival_index);
                    }
                } else {
                    replay_received += 1;
                }
                arrival_index += 1;
            }
            (first_live_arrival, live_received, replay_received)
        });

        // Live producer: separate task, sends into the SAME channel — the
        // first frame immediately, then one every 5ms.
        let live_tx = tx.clone();
        let live_producer = tokio::spawn(async move {
            for i in 0..LIVE_TOTAL {
                live_tx
                    .send((LIVE_SEQ_BASE + i, Bytes::from_static(b"live")))
                    .await
                    .expect("live send");
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        });

        let outcome = reinject_wal_frames(
            &tx,
            make_frames(REPLAY_TOTAL),
            CHUNK as usize, // frequent yields relative to the 4,096-slot channel
            TEST_SEND_TIMEOUT,
        )
        .await;

        assert!(outcome.clean);
        assert_eq!(outcome.injected, REPLAY_TOTAL);

        live_producer.await.expect("live producer task");
        drop(tx);
        let (first_live_arrival, live_received, replay_received) =
            consumer.await.expect("consumer task");

        assert_eq!(replay_received, REPLAY_TOTAL, "no replay frame lost");
        assert_eq!(live_received, LIVE_TOTAL, "no live frame lost");
        let first = first_live_arrival.expect("at least one live frame arrived");
        // Deterministic, meaningful liveness bound (C3 review MEDIUM — the
        // old `first < total/2` bound of 250,005 was near-vacuous): the live
        // producer's first send is issued at replay start; mpsc waiter
        // fairness is FIFO, so the live frame lands behind at most the
        // frames already buffered (channel capacity 4,096) plus a few
        // yield windows (chunk 1,024) of scheduling slack before the live
        // task first runs. 3 chunks + capacity = 7,168 of 500,010 — a
        // comfortable margin that still proves the live path is NOT
        // starved behind hundreds of thousands of replay frames.
        const CHANNEL_CAP: u64 = 4_096;
        const CHUNK: u64 = 1_024;
        let bound = 3 * CHUNK + CHANNEL_CAP;
        assert!(
            first < bound,
            "live path starved: first live frame arrived at index {first}, \
             bound {bound} (replay still had {} frames remaining)",
            REPLAY_TOTAL.saturating_sub(first)
        );
    }

    // RETIRED (PR-C2, 2026-07-13 — Dhan live-WS lane deletion, operator
    // retirement directive per websocket-connection-scope-lock.md
    // "2026-07-13 Amendment" §B): ratchet_main_rs_uses_bounded_reinject_helper died with the wiring it pinned —
    // both STAGE-C.2b re-injection call sites (fast boot + start_dhan_lane)
    // were deleted with the lane; main.rs now drains residual LiveFeed WAL
    // frames loudly at boot (counted + confirm_replayed) because no Dhan
    // frame channel / tick processor exists to re-inject into. The
    // `reinject_wal_frames` helper is retained un-consumed pending the
    // Phase C module cleanup.

    // RETIRED (PR-C2, 2026-07-13 — Dhan live-WS lane deletion, operator
    // retirement directive per websocket-connection-scope-lock.md
    // "2026-07-13 Amendment" §B): ratchet_tick_processor_spawns_before_reinject_await died with the wiring it pinned —
    // both STAGE-C.2b re-injection call sites (fast boot + start_dhan_lane)
    // were deleted with the lane; main.rs now drains residual LiveFeed WAL
    // frames loudly at boot (counted + confirm_replayed) because no Dhan
    // frame channel / tick processor exists to re-inject into. The
    // `reinject_wal_frames` helper is retained un-consumed pending the
    // Phase C module cleanup.
}
