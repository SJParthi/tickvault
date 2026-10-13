//! Joining the two top-volume writer threads at shutdown (2026-10-13,
//! top_volume spill plan item 3), split out of `dhan_feed_stack.rs`, which is
//! over its size pin (`source_size_ratchet_guard`).

use super::*;

impl LiveIngest {
    /// Waits for the top-volume snapshot writer to finish its queue.
    ///
    /// The THIRD writer to get shutdown accounting, and the last: ticks and
    /// depth have had it since 2026-08-28, and this one was spawned with its
    /// handle discarded until 2026-09-13.
    ///
    /// Shares the SAME deadline as the other two joins for the reason recorded
    /// at `shutdown_offload_writer`: `main` gives this whole task ONE
    /// `DHAN_LANE_SHUTDOWN_FLUSH_BUDGET_SECS`, and independent graces would SUM
    /// past it — which is the arithmetic the compile-time assert in `main.rs`
    /// exists to keep honest. All three queues are closed before any join
    /// begins, so the threads drain in parallel and the total is the MAX of the
    /// three waits, never the sum. Adding this third join therefore costs zero
    /// shutdown budget.
    ///
    /// Idempotent, and a no-op on a lane whose writer never spawned.
    pub fn shutdown_top_volume_writer(&mut self, deadline: std::time::Instant) {
        // TWO threads since 2026-10-13 (top_volume spill plan item 3): the
        // formatter, then the flush thread. The formatter holds the flush
        // queue's only sender, so the flush thread can finish only after the
        // formatter has. Both are polled against the SAME deadline, so the
        // second wait spends only what the first left.
        //
        // The shutdown signal goes FIRST (2026-10-13, PR-B review): from here
        // both threads spill instead of sending, so the up-to-four queued
        // batches go to disk at append speed instead of each waiting on a slow
        // QuestDB. Only a batch already mid-flush can still hold the flush
        // thread (one request timeout), which the deadline below bounds.
        if let Some(shutdown) = self.top_volume_shutdown.as_ref() {
            shutdown.request();
        }
        let threads = [
            (self.top_volume_thread.take(), "formatter"),
            (self.top_volume_flush_thread.take(), "flush"),
        ];
        for (slot, thread) in threads {
            let Some(handle) = slot else {
                continue;
            };
            // POLLED to the deadline, then joined only once the thread has
            // actually finished — the `shutdown_rescue_writer` shape, adopted
            // here on 2026-09-13 because this function did not have it.
            //
            // `JoinHandle::join` has no timeout, so a writer wedged on a hung
            // socket hangs the box's shutdown until systemd's `TimeoutStopSec`
            // SIGKILLs it — trading lost snapshot rows for a lost shutdown,
            // which is the worse half of the trade. A point-in-time check is
            // not a bound: it only refuses to START waiting once the budget is
            // already spent. This is the LAST of the three joins against one
            // shared budget, so the two before it routinely leave very little.
            //
            // `is_finished()` is non-blocking, so the `join()` below cannot
            // block: it runs only on a thread that has already exited.
            while !handle.is_finished() && std::time::Instant::now() < deadline {
                std::thread::sleep(RESCUE_JOIN_POLL);
            }
            if !handle.is_finished() {
                metrics::counter!(OFFLOAD_SHUTDOWN_INCOMPLETE_COUNTER, "writer" => "top_volume")
                    .increment(1);
                let why = if thread == "flush" {
                    "the flush thread was still inside a QuestDB request when the \
                     lane's shared shutdown deadline passed (its own request \
                     timeout, or a budget the tick and depth joins already spent)"
                } else {
                    "the formatter was still formatting or spilling its queue when \
                     the lane's shared shutdown deadline passed (its own disk \
                     writes, or a budget the tick and depth joins already spent)"
                };
                error!(
                    code = ErrorCode::HotPath02WriterQueueDrop.code_str(),
                    thread,
                    why,
                    "the top-volume writer was abandoned at shutdown, so the final \
                     ranking batches of the session may have reached neither \
                     QuestDB nor the top_volume spill folder. These are ranking \
                     SNAPSHOTS, not ticks: no trade data is lost."
                );
                // An abandoned formatter still holds the flush queue open, so
                // the flush thread cannot finish either: counted once, here.
                return;
            }
            if handle.join().is_err() {
                metrics::counter!(OFFLOAD_SHUTDOWN_INCOMPLETE_COUNTER, "writer" => "top_volume")
                    .increment(1);
                error!(
                    code = ErrorCode::HotPath02WriterQueueDrop.code_str(),
                    thread,
                    "the top-volume writer thread PANICKED — the final ranking \
                     batches of the session may not have reached QuestDB."
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2026-10-13 (top_volume spill plan item 3): the top-volume writer is two
    /// threads, the formatter and the flush thread, and shutdown joins BOTH
    /// against the one shared deadline. A thread still running at the deadline
    /// is abandoned and counted, never waited on without a bound.
    #[test]
    fn test_shutdown_joins_both_threads_within_the_deadline() {
        let mut ingest = LiveIngest::new(TickWriter::for_test(Feed::Dhan), 4);
        let flush_done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = std::sync::Arc::clone(&flush_done);
        ingest = ingest
            .with_top_volume_writer(
                tickvault_storage::top_volume_rank_persistence::TopVolumeRowProducer::for_test().0,
                std::thread::spawn(|| {}),
            )
            .with_top_volume_flush_thread(
                Some(std::thread::spawn(move || {
                    flag.store(true, std::sync::atomic::Ordering::SeqCst);
                })),
                tickvault_storage::top_volume_rank_persistence::TopVolumeShutdown::default(),
            );
        assert!(ingest.top_volume_flush_thread.is_some());
        let signal = ingest.top_volume_shutdown.clone().expect("held");
        assert!(!signal.is_requested());
        ingest.close_offload_queues();
        let deadline = std::time::Instant::now() + OFFLOAD_SHUTDOWN_GRACE;
        ingest.shutdown_top_volume_writer(deadline);
        assert!(
            ingest.top_volume_thread.is_none(),
            "the formatter was joined"
        );
        assert!(
            ingest.top_volume_flush_thread.is_none(),
            "the flush thread was joined"
        );
        assert!(flush_done.load(std::sync::atomic::Ordering::SeqCst));
        assert!(
            signal.is_requested(),
            "shutdown must ask both threads to spill before it waits on them"
        );

        // A flush thread that outlives the deadline is ABANDONED, not waited
        // on: the call returns at the deadline.
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let mut stuck = LiveIngest::new(TickWriter::for_test(Feed::Dhan), 4)
            .with_top_volume_flush_thread(
                Some(std::thread::spawn(move || {
                    let _ = release_rx.recv();
                })),
                tickvault_storage::top_volume_rank_persistence::TopVolumeShutdown::default(),
            );
        let started = std::time::Instant::now();
        stuck.shutdown_top_volume_writer(started + std::time::Duration::from_millis(50));
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "bounded by the deadline"
        );
        assert!(stuck.top_volume_flush_thread.is_none());
        drop(release_tx);
    }

    #[test]
    fn test_with_top_volume_flush_thread_holds_the_handle_for_shutdown() {
        let ingest = LiveIngest::new(TickWriter::for_test(Feed::Dhan), 4);
        assert!(ingest.top_volume_flush_thread.is_none());
        let ingest = ingest.with_top_volume_flush_thread(
            Some(std::thread::spawn(|| {})),
            tickvault_storage::top_volume_rank_persistence::TopVolumeShutdown::default(),
        );
        assert!(ingest.top_volume_flush_thread.is_some());
        assert!(ingest.top_volume_shutdown.is_some());
        // A failed flush-thread spawn still hands over the shutdown signal.
        let no_thread = LiveIngest::new(TickWriter::for_test(Feed::Dhan), 4)
            .with_top_volume_flush_thread(
                None,
                tickvault_storage::top_volume_rank_persistence::TopVolumeShutdown::default(),
            );
        assert!(no_thread.top_volume_flush_thread.is_none());
        assert!(no_thread.top_volume_shutdown.is_some());
    }
}
