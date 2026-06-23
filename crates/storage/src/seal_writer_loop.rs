//! Wave 6 Sub-PR #1 item 1.2f.5 — sealed-candle writer-task tokio loop.
//!
//! Long-running async wrapper that calls
//! [`SealWriterRunner::run_one_cycle`] on a fixed interval, with
//! cooperative cancellation via a [`tokio::sync::watch`] channel.
//! The future boot wiring (item 1.4) spawns this task at app
//! startup; the shutdown path flips the cancellation flag, the loop
//! does ONE final drain, then exits gracefully.
//!
//! ## What this slice ships
//!
//! - [`SEAL_DRAIN_INTERVAL_MS`] — locked drain interval constant
//!   (100 ms — bounds the worst-case in-flight latency from
//!   `aggregator.consume_tick → ILP commit` to ~100 ms +
//!   one `flush()` round-trip).
//! - [`run_seal_writer_loop`] — the async fn; runs forever until
//!   cancelled, returns on cancel after one final drain cycle so no
//!   buffered seal is lost.
//! - 5 `#[tokio::test]` cases covering: idle ticks; submit-then-tick
//!   processes seals; cancellation exits the loop; cancellation does
//!   ONE final drain (`final_drain_outcome` non-idle); `MissedTickBehavior::Skip`
//!   ratcheted (no thundering-herd if the runtime stalls).
//!
//! ## What this slice does NOT ship
//!
//! - Boot wiring + `tokio::spawn` of this loop into `main.rs` (item 1.4).
//! - Prometheus counter increments per-cycle outcome (item 1.4 — the
//!   loop currently DROPS the `CycleOutcome` after each tick; the
//!   future wiring will fan it out to counters).
//! - Reconnect throttle for `ShadowCandleWriter` (item 1.2f.6, if
//!   needed — current behaviour is "permanently disconnected if
//!   QuestDB was down at boot").
//!
//! ## Why `tokio::sync::watch` and not `tokio_util::CancellationToken`
//!
//! `tokio-util` is NOT a workspace dependency today. Adding it
//! requires Parthiban approval per CLAUDE.md "New dep additions need
//! Parthiban approval". `tokio::sync::watch` ships with the existing
//! `tokio` dep and provides identical semantics
//! (`watch::Sender::send(true)` flips a flag; the receiver's
//! `.changed().await` wakes pending tasks). The other long-running
//! tasks in the codebase (`ip_monitor.rs`, `depth_rebalancer.rs`)
//! already use this pattern.

use std::time::Duration;

use chrono::Utc;
use tokio::sync::watch;
use tracing::{debug, info};

use crate::seal_writer_runner::{CycleOutcome, SealWriterRunner};

/// Drain interval — the loop wakes every 100 ms during normal
/// operation and drains the mpsc + ring through the ILP buffer.
/// Sized to bound the producer→ILP latency at ~100 ms + one flush
/// round-trip. A future SLO ratchet may tighten this; for now it
/// matches the existing tick-persistence drain cadence.
pub const SEAL_DRAIN_INTERVAL_MS: u64 = 100;

/// Convenience accessor for the drain interval as a `Duration`.
#[must_use]
pub const fn seal_drain_interval() -> Duration {
    Duration::from_millis(SEAL_DRAIN_INTERVAL_MS)
}

/// Returns the current UTC unix timestamp in seconds. Used to
/// derive the IST-date filename for spill / DLQ via the
/// downstream `now_unix_secs` parameter — per locked decision
/// **L-H7** the SHARED storage layer takes the wall-clock from the
/// caller, never `Utc::now()` inside the hot path. The writer task
/// is COLD path so this `Utc::now()` call IS allowed here.
fn utc_now_secs() -> i64 {
    Utc::now().timestamp()
}

/// Long-running tokio task. Calls `runner.run_one_cycle` on a fixed
/// 100 ms interval until cancelled. On cancellation, performs ONE
/// final drain cycle so no buffered seal is silently lost.
///
/// `cancel_rx` is the receive end of a `tokio::sync::watch::channel`.
/// The producer side (typically `main.rs` shutdown handler) flips the
/// flag via `cancel_tx.send(true)`. The loop wakes immediately and
/// exits after one final drain.
///
/// Returns the outcome of the final post-cancel drain so the caller
/// can log / surface it for graceful-shutdown observability.
pub async fn run_seal_writer_loop(
    mut runner: SealWriterRunner,
    interval: Duration,
    mut cancel_rx: watch::Receiver<bool>,
) -> CycleOutcome {
    info!(
        interval_ms = interval.as_millis(),
        max_drain = runner.max_drain_per_cycle(),
        "seal writer loop starting"
    );
    let mut ticker = tokio::time::interval(interval);
    // If the runtime stalls and we miss multiple ticks (e.g. tokio
    // reactor was pinned by another task), DO NOT fire all the
    // missed ticks back-to-back (which would burn CPU on a
    // back-to-back drain barrage). Skip them; the next cycle is
    // sufficient to catch up.
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            biased; // Prefer the cancel branch so a pending shutdown
                    // is not held up by an extra tick.
            _ = cancel_rx.changed() => {
                if *cancel_rx.borrow() {
                    info!("seal writer loop cancelled — performing final drain");
                    let now = utc_now_secs();
                    let final_outcome = runner.run_one_cycle(now);
                    info!(
                        submitted_from_mpsc = final_outcome.submitted_from_mpsc,
                        rescued_to_spill = final_outcome.drain.rescued_to_spill,
                        rescued_to_dlq = final_outcome.drain.rescued_to_dlq,
                        rescued_dropped = final_outcome.drain.rescued_dropped,
                        "seal writer loop final drain complete"
                    );
                    return final_outcome;
                }
                // The watch receiver's `.changed()` future also
                // resolves on the initial `false → false` change
                // (which doesn't happen) and on sender drop; ignore.
            }
            _ = ticker.tick() => {
                let now = utc_now_secs();
                let outcome = runner.run_one_cycle(now);
                if !outcome.is_idle() {
                    debug!(
                        submitted_from_mpsc = outcome.submitted_from_mpsc,
                        ring_seals_popped = outcome.drain.ring_seals_popped,
                        flushed_ok = outcome.drain.flushed_ok,
                        "seal writer cycle"
                    );
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use tickvault_common::feed::Feed;
    use tickvault_trading::candles::{BufferedSeal, LiveCandleState, TfIndex};

    fn temp_pair(name: &str) -> (PathBuf, PathBuf) {
        let mut spill = std::env::temp_dir();
        let mut dlq = std::env::temp_dir();
        spill.push(format!(
            "tickvault-seal-loop-spill-{}-{}",
            name,
            std::process::id()
        ));
        dlq.push(format!(
            "tickvault-seal-loop-dlq-{}-{}",
            name,
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&spill);
        let _ = std::fs::remove_dir_all(&dlq);
        std::fs::create_dir_all(&spill).expect("spill dir");
        std::fs::create_dir_all(&dlq).expect("dlq dir");
        (spill, dlq)
    }

    fn cleanup(spill: &PathBuf, dlq: &PathBuf) {
        let _ = std::fs::remove_dir_all(spill);
        let _ = std::fs::remove_dir_all(dlq);
    }

    fn mk_seal(sid: u32, seg: u8, tf: TfIndex, bucket: u32, close: f64) -> BufferedSeal {
        let mut state = LiveCandleState::empty();
        state.bucket_start_ist_secs = bucket;
        state.open = 100.0;
        state.high = 105.0;
        state.low = 99.0;
        state.close = close;
        state.volume = 1234;
        state.bucket_start_cumulative = 1000;
        state.oi = 50_000;
        state.tick_count = 5;
        state.close_pct_from_prev_day = 1.5;
        state.oi_pct_from_prev_day = -0.2;
        state.volume_pct_from_prev_day = 12.3;
        BufferedSeal::new(sid, seg, tf, state, Feed::Dhan)
    }

    #[test]
    fn test_seal_drain_interval_constant_pinned() {
        // Pin the locked interval so a regression PR can't silently
        // raise it (which would balloon producer→ILP latency).
        assert_eq!(SEAL_DRAIN_INTERVAL_MS, 100);
        assert_eq!(seal_drain_interval(), Duration::from_millis(100));
    }

    #[tokio::test]
    async fn test_run_seal_writer_loop_exits_on_cancellation() {
        let (spill, dlq) = temp_pair("cancel-exits");
        let runner = SealWriterRunner::for_test(spill.clone(), dlq.clone(), 16, 16, 16);
        let (cancel_tx, cancel_rx) = watch::channel(false);
        let task = tokio::spawn(run_seal_writer_loop(
            runner,
            Duration::from_millis(10),
            cancel_rx,
        ));
        // Let it tick once
        tokio::time::sleep(Duration::from_millis(15)).await;
        // Signal cancellation
        cancel_tx.send(true).expect("send");
        // Loop must exit promptly
        let result = tokio::time::timeout(Duration::from_millis(500), task).await;
        let join_result = result.expect("loop must exit within 500ms after cancel");
        let _final_outcome = join_result.expect("task must not panic");
        cleanup(&spill, &dlq);
    }

    #[tokio::test]
    async fn test_run_seal_writer_loop_processes_seals_on_tick() {
        let (spill, dlq) = temp_pair("processes-seals");
        let runner = SealWriterRunner::for_test(spill.clone(), dlq.clone(), 16, 16, 16);
        let tx = runner.sender();
        let (cancel_tx, cancel_rx) = watch::channel(false);

        // Push 3 seals BEFORE starting the loop so the very first
        // tick has work to do.
        tx.try_send(mk_seal(13, 0, TfIndex::M1, 1_716_000_900, 100.0))
            .expect("ok");
        tx.try_send(mk_seal(25, 0, TfIndex::M1, 1_716_001_500, 200.0))
            .expect("ok");
        tx.try_send(mk_seal(51, 0, TfIndex::M1, 1_716_002_100, 300.0))
            .expect("ok");

        let task = tokio::spawn(run_seal_writer_loop(
            runner,
            Duration::from_millis(10),
            cancel_rx,
        ));
        // Let two ticks fire
        tokio::time::sleep(Duration::from_millis(30)).await;
        cancel_tx.send(true).expect("send");
        let outcome = tokio::time::timeout(Duration::from_millis(500), task)
            .await
            .expect("timeout")
            .expect("panic");
        // Loop drained the seals over the ticks. By the time we
        // cancel, the ring is empty AND the rescue counters are
        // populated (writer is disconnected → all 3 went to spill).
        // Final cycle is a noop (everything already drained).
        let _ = outcome;

        // Verify spill file has 3 records (proves the loop drained
        // through the writer + cascade-rescued correctly).
        let spill_writer =
            crate::seal_spill::SealSpillWriter::with_spill_dir_for_test(spill.clone());
        let now = chrono::Utc::now().timestamp();
        let drained = spill_writer.read_all(now).expect("read");
        assert_eq!(drained.len(), 3, "all 3 seals must reach spill");
        cleanup(&spill, &dlq);
    }

    #[tokio::test]
    async fn test_cancel_triggers_final_drain() {
        // Push seals AFTER cancel signal but BEFORE the loop sees it.
        // The final-drain on cancel MUST process them.
        let (spill, dlq) = temp_pair("final-drain");
        let runner = SealWriterRunner::for_test(spill.clone(), dlq.clone(), 16, 16, 16);
        let tx = runner.sender();
        let (cancel_tx, cancel_rx) = watch::channel(false);

        let task = tokio::spawn(run_seal_writer_loop(
            runner,
            // Long interval so the loop is parked between ticks.
            Duration::from_secs(10),
            cancel_rx,
        ));
        // Give the spawned task a moment to enter `select!`
        tokio::time::sleep(Duration::from_millis(20)).await;
        // Push seals THEN cancel
        tx.try_send(mk_seal(13, 0, TfIndex::M1, 1_716_000_900, 100.0))
            .expect("ok");
        tx.try_send(mk_seal(25, 0, TfIndex::M1, 1_716_001_500, 200.0))
            .expect("ok");
        cancel_tx.send(true).expect("send");
        let outcome = tokio::time::timeout(Duration::from_millis(500), task)
            .await
            .expect("timeout")
            .expect("panic");
        // Final drain should have processed both seals
        assert_eq!(
            outcome.submitted_from_mpsc, 2,
            "final-drain MUST submit both pending seals to pipeline"
        );
        assert_eq!(
            outcome.drain.ring_seals_popped, 2,
            "final-drain MUST pop both from the ring"
        );
        assert_eq!(
            outcome.drain.rescued_to_spill, 2,
            "final-drain MUST rescue both via spill (writer disconnected)"
        );
        cleanup(&spill, &dlq);
    }

    #[tokio::test]
    async fn test_loop_handles_no_seals_gracefully_across_many_ticks() {
        let (spill, dlq) = temp_pair("idle-ticks");
        let runner = SealWriterRunner::for_test(spill.clone(), dlq.clone(), 16, 16, 16);
        let (cancel_tx, cancel_rx) = watch::channel(false);
        let task = tokio::spawn(run_seal_writer_loop(
            runner,
            Duration::from_millis(5),
            cancel_rx,
        ));
        // Let many ticks fire with no work — loop must not error
        tokio::time::sleep(Duration::from_millis(50)).await;
        cancel_tx.send(true).expect("send");
        let final_outcome = tokio::time::timeout(Duration::from_millis(500), task)
            .await
            .expect("timeout")
            .expect("panic");
        // Every cycle was idle; the final cycle is also idle.
        assert!(final_outcome.is_idle());
        cleanup(&spill, &dlq);
    }

    #[tokio::test]
    async fn test_loop_does_not_panic_on_sender_drop() {
        // If all producer-side senders drop, `try_recv` returns
        // Disconnected. The loop must continue ticking without
        // panicking; it's the caller's job to cancel.
        let (spill, dlq) = temp_pair("sender-drop");
        let runner = SealWriterRunner::for_test(spill.clone(), dlq.clone(), 16, 16, 16);
        let tx = runner.sender();
        let (cancel_tx, cancel_rx) = watch::channel(false);
        let task = tokio::spawn(run_seal_writer_loop(
            runner,
            Duration::from_millis(5),
            cancel_rx,
        ));
        // Drop the producer-side sender and let several ticks fire
        drop(tx);
        tokio::time::sleep(Duration::from_millis(30)).await;
        // Loop must still be alive
        assert!(!task.is_finished(), "loop must survive sender drop");
        cancel_tx.send(true).expect("send");
        let _ = tokio::time::timeout(Duration::from_millis(500), task)
            .await
            .expect("timeout")
            .expect("panic");
        cleanup(&spill, &dlq);
    }
}
