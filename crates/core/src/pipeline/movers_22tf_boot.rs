//! Boot helper for the movers 22-tf pipeline — Phase 10b-2 of v3 plan.
//!
//! Encapsulates the wiring that brings up the full 22-timeframe snapshot
//! pipeline at app startup. Provides one entry point
//! (`build_movers_22tf_pipeline`) that:
//!
//! 1. Constructs the shared `Movers22TfTracker` (papaya-backed).
//! 2. Allocates 22 `tokio::sync::mpsc::channel(8192)` pairs (one per
//!    timeframe).
//! 3. Returns the senders array (boot integration installs into
//!    `init_global_writer_state`) + receivers vec (consumer tasks
//!    take ownership) + the shared tracker (ticker processor + 22
//!    snapshot tasks share via Arc::clone).
//!
//! ## Why a separate boot helper?
//!
//! `crates/app/src/main.rs` is 8K+ lines. A single misplaced edit risks
//! breaking the existing boot sequence. By isolating the wiring math
//! into a testable helper, the main.rs insertion is a single function
//! call that's easy to audit + revert.
//!
//! ## See also
//!
//! - `crates/core/src/pipeline/movers_22tf_writer_state.rs` — the
//!   `OnceLock<[Sender; 22]>` registry.
//! - `crates/core/src/pipeline/movers_22tf_tracker.rs` — the papaya
//!   tracker.
//! - `crates/core/src/pipeline/movers_22tf_scheduler.rs` — the
//!   per-timeframe cadence primitives.

use std::sync::Arc;

use tickvault_common::mover_types::MoverRow;
use tokio::sync::mpsc;

use crate::pipeline::movers_22tf_scheduler::MOVERS_22TF_MPSC_CAPACITY;
use crate::pipeline::movers_22tf_tracker::Movers22TfTracker;
use crate::pipeline::movers_22tf_writer_state::{MOVERS_22TF_WRITER_COUNT, Movers22TfWriterState};

/// Output of `build_movers_22tf_pipeline` — everything the boot wiring
/// needs to install in main.rs.
pub struct Movers22TfPipeline {
    /// Shared papaya-backed tracker. Cloned via Arc into:
    /// - tick_processor (single writer; calls `update_security_state`)
    /// - 22 snapshot tasks (concurrent readers; call `snapshot_into`)
    pub tracker: Movers22TfTracker,
    /// Writer-state registry holding the 22 senders. Pass to
    /// `init_global_writer_state(state)` immediately after construction.
    pub writer_state: Movers22TfWriterState,
    /// 22 receivers — boot wiring takes ownership of each, spawning a
    /// per-timeframe consumer task that drains the channel into the
    /// `Movers22TfWriter` ILP buffer.
    pub receivers: Vec<mpsc::Receiver<MoverRow>>,
}

/// Constructs the full movers 22-tf runtime: tracker + 22 channels +
/// writer-state registry. Pure function (no I/O); the caller installs
/// each piece into the running tokio runtime.
///
/// Call exactly once at boot, after `ensure_movers_22tf_tables` has
/// completed but before spawning the 22 consumer tasks + the snapshot
/// scheduler tasks.
#[must_use]
pub fn build_movers_22tf_pipeline() -> Movers22TfPipeline {
    let tracker = Movers22TfTracker::new();

    let mut senders: Vec<mpsc::Sender<MoverRow>> = Vec::with_capacity(MOVERS_22TF_WRITER_COUNT);
    let mut receivers: Vec<mpsc::Receiver<MoverRow>> = Vec::with_capacity(MOVERS_22TF_WRITER_COUNT);
    for _ in 0..MOVERS_22TF_WRITER_COUNT {
        let (tx, rx) = mpsc::channel::<MoverRow>(MOVERS_22TF_MPSC_CAPACITY);
        senders.push(tx);
        receivers.push(rx);
    }

    // Convert Vec<Sender; 22> -> [Sender; 22] for the typed array.
    let senders_array: [mpsc::Sender<MoverRow>; MOVERS_22TF_WRITER_COUNT] =
        senders.try_into().unwrap_or_else(|_| {
            // O(1) EXEMPT: structural invariant — the loop above pushed
            // exactly MOVERS_22TF_WRITER_COUNT entries, so try_into is
            // infallible. The unreachable branch keeps the type system
            // happy without a panic in production.
            unreachable!("MOVERS_22TF_WRITER_COUNT push count is fixed")
        });
    let writer_state = Movers22TfWriterState::new(senders_array);

    Movers22TfPipeline {
        tracker,
        writer_state,
        receivers,
    }
}

/// Returns a clonable handle to the tracker for downstream consumers
/// (tick processor, snapshot tasks, observability metrics emitters).
/// Wraps the tracker in `Arc` for ergonomic sharing.
#[must_use]
pub fn arc_tracker(pipeline: &Movers22TfPipeline) -> Arc<Movers22TfTracker> {
    Arc::new(pipeline.tracker.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tickvault_common::types::ExchangeSegment;

    /// Phase 10b-2 ratchet: pipeline build allocates exactly 22
    /// senders + 22 receivers.
    #[test]
    fn test_build_movers_22tf_pipeline_creates_22_channels() {
        let pipeline = build_movers_22tf_pipeline();
        assert_eq!(pipeline.receivers.len(), MOVERS_22TF_WRITER_COUNT);
        assert_eq!(pipeline.writer_state.len(), MOVERS_22TF_WRITER_COUNT);
        assert_eq!(MOVERS_22TF_WRITER_COUNT, 22);
    }

    /// Phase 10b-2 ratchet: tracker starts empty + writer state has
    /// senders for every index in [0, 22).
    #[test]
    fn test_build_movers_22tf_pipeline_writer_state_has_all_22_senders() {
        let pipeline = build_movers_22tf_pipeline();
        for idx in 0..MOVERS_22TF_WRITER_COUNT {
            assert!(
                pipeline.writer_state.sender_for(idx).is_some(),
                "writer_state.sender_for({idx}) should be Some"
            );
        }
        assert!(pipeline.writer_state.sender_for(22).is_none());
    }

    /// Phase 10b-2 ratchet: arc_tracker returns a clonable Arc that
    /// shares state with the pipeline tracker.
    #[test]
    fn test_arc_tracker_shares_state() {
        let pipeline = build_movers_22tf_pipeline();
        let arc = arc_tracker(&pipeline);
        let state = crate::pipeline::movers_22tf_tracker::SecurityState {
            ltp: 19500.0,
            prev_close: 19400.0,
            ..crate::pipeline::movers_22tf_tracker::SecurityState::empty()
        };
        // Update via the pipeline's tracker — Arc<>'d clone should see it.
        pipeline
            .tracker
            .update_security_state(13, ExchangeSegment::IdxI, state);
        assert_eq!(arc.len(), 1);
        assert_eq!(arc.get(13, ExchangeSegment::IdxI).unwrap().ltp, 19500.0);
    }

    /// Phase 10b-2 ratchet: pipeline channels actually flow data —
    /// sending on one writer-state sender lands on the matching receiver.
    #[tokio::test]
    async fn test_build_movers_22tf_pipeline_channels_flow_data() {
        let mut pipeline = build_movers_22tf_pipeline();
        let row = MoverRow::empty();

        // Send via writer_state sender at index 0.
        let sender = pipeline.writer_state.sender_for(0).unwrap();
        sender
            .try_send(row)
            .expect("first send to slot 0 should succeed");

        // Receive on the matching receiver at index 0.
        let received = pipeline.receivers[0]
            .try_recv()
            .expect("recv should succeed");
        assert_eq!(received.security_id, row.security_id);
    }
}
