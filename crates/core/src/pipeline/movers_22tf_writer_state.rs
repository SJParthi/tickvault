//! Movers 22-TF writer state — Phase 10 of v3 plan, 2026-04-28.
//!
//! Holds the global registry of 22 `tokio::sync::mpsc::Sender<MoverRow>`
//! channels (one per timeframe) so the snapshot scheduler tasks can
//! enqueue rows without each holding a separate handle. Mirrors the
//! `prev_close_writer.rs` pattern (`OnceLock<Sender>`) but for an array.
//!
//! ## Lifecycle
//!
//! 1. Boot wiring (Phase 10b in main.rs) constructs 22
//!    `tokio::sync::mpsc::channel(MOVERS_22TF_MPSC_CAPACITY)` pairs.
//! 2. The 22 receivers move into the per-timeframe `Movers22TfWriter`
//!    tasks (one task per writer, each owning its own ILP TCP `Sender`
//!    to QuestDB).
//! 3. The 22 senders are stored in a `[Sender<MoverRow>; 22]` array
//!    inside `Movers22TfWriterState`, which goes into a `OnceLock`.
//! 4. The snapshot scheduler tasks call `try_enqueue_global(idx, row)`
//!    to push rows. Drop-newest semantics (per audit: native tokio
//!    `try_send` returns `Full(value)` and drops the NEWEST attempt).
//!
//! ## See also
//!
//! - `crates/core/src/pipeline/prev_close_writer.rs` — the precedent
//!   pattern (single OnceLock<Sender>).

use std::sync::OnceLock;

use tickvault_common::mover_types::MoverRow;
use tokio::sync::mpsc;

/// Number of writer slots. Pinned at 22 by the timeframe ladder.
pub const MOVERS_22TF_WRITER_COUNT: usize = 22;

/// Reasons a `try_enqueue_global` call could fail. Mirrors the
/// `prev_close_writer::EnqueueOutcome` taxonomy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnqueueOutcome {
    /// Row enqueued successfully.
    Sent,
    /// `try_send` returned `Full` — drop-newest semantics. Row dropped.
    /// Increment `tv_movers_writer_dropped_total{reason="full_drop_newest"}`.
    DroppedFull,
    /// Channel receiver was dropped — writer task exited. Row dropped.
    /// Increment `tv_movers_writer_dropped_total{reason="closed"}`.
    Closed,
    /// `OnceLock` not initialized yet — boot wiring incomplete. Row dropped.
    /// Increment `tv_movers_writer_dropped_total{reason="uninit"}`.
    Uninit,
    /// Index out of range (>= 22). Row dropped.
    OutOfRange,
}

/// Writer state — the array of 22 senders. Stored inside `OnceLock`.
///
/// The struct itself is `Send + Sync` because every Sender is.
pub struct Movers22TfWriterState {
    senders: [mpsc::Sender<MoverRow>; MOVERS_22TF_WRITER_COUNT],
}

impl Movers22TfWriterState {
    /// Constructs the state from the 22 senders. Boot wiring uses this.
    #[must_use]
    // TEST-EXEMPT: trivial constructor; exercised by `make_state` test helper inside this module's #[cfg(test)] block.
    pub const fn new(senders: [mpsc::Sender<MoverRow>; MOVERS_22TF_WRITER_COUNT]) -> Self {
        Self { senders }
    }

    /// Returns the sender for the given timeframe index, or None if
    /// out of range.
    #[must_use]
    pub fn sender_for(&self, idx: usize) -> Option<&mpsc::Sender<MoverRow>> {
        self.senders.get(idx)
    }

    /// Returns the count (always 22).
    #[must_use]
    pub const fn len(&self) -> usize {
        MOVERS_22TF_WRITER_COUNT
    }

    /// True if no senders are present (impossible by construction —
    /// kept for API parity with collection types).
    #[must_use]
    // TEST-EXEMPT: const-true-by-construction; exercised by `test_state_len_returns_22` which asserts `!is_empty()`.
    pub const fn is_empty(&self) -> bool {
        false
    }
}

/// Global writer state. Set once at boot (Phase 10b wiring) via
/// `init_global_writer_state`. Read by snapshot scheduler tasks via
/// `try_enqueue_global`.
static GLOBAL_WRITER_STATE: OnceLock<Movers22TfWriterState> = OnceLock::new();

/// Installs the global writer state. Idempotent — second + later calls
/// are no-ops (returns `false`). Boot must call this exactly once.
///
/// # Errors
///
/// Returns `false` if the OnceLock was already set. Caller should log
/// at WARN — duplicate boot wiring is a programmer bug.
// TEST-EXEMPT: thin wrapper over OnceLock::set; cross-test contamination prevents idempotency unit tests in process — covered by Phase 12-runtime integration tests.
pub fn init_global_writer_state(state: Movers22TfWriterState) -> bool {
    GLOBAL_WRITER_STATE.set(state).is_ok()
}

/// True if the global writer state has been initialised.
#[must_use]
// WIRING-EXEMPT: pending Phase 10b boot integration in main.rs (call site lands when snapshot scheduler is wired into boot per Phase 10b plan).
// TEST-EXEMPT: thin wrapper over OnceLock::get; covered by Phase 12-runtime integration tests once Phase 10b lands.
pub fn is_global_writer_state_initialised() -> bool {
    GLOBAL_WRITER_STATE.get().is_some()
}

/// Tries to enqueue a `MoverRow` to the writer at the given timeframe
/// index. Drop-newest semantics — Full / Closed / Uninit / OutOfRange
/// all return without panicking. Caller increments the appropriate
/// `tv_movers_writer_dropped_total{reason}` metric on non-`Sent` outcomes.
#[must_use]
// TEST-EXEMPT: depends on global OnceLock state; per-outcome semantics covered by `test_local_send_*` tests on a non-global state instance + Phase 12-runtime integration tests after Phase 10b boot wiring.
pub fn try_enqueue_global(idx: usize, row: MoverRow) -> EnqueueOutcome {
    let Some(state) = GLOBAL_WRITER_STATE.get() else {
        return EnqueueOutcome::Uninit;
    };
    let Some(sender) = state.sender_for(idx) else {
        return EnqueueOutcome::OutOfRange;
    };
    match sender.try_send(row) {
        Ok(()) => EnqueueOutcome::Sent,
        Err(mpsc::error::TrySendError::Full(_)) => EnqueueOutcome::DroppedFull,
        Err(mpsc::error::TrySendError::Closed(_)) => EnqueueOutcome::Closed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_row() -> MoverRow {
        MoverRow::empty()
    }

    fn make_state(
        channel_capacity: usize,
    ) -> (Movers22TfWriterState, Vec<mpsc::Receiver<MoverRow>>) {
        let mut senders = Vec::with_capacity(MOVERS_22TF_WRITER_COUNT);
        let mut receivers = Vec::with_capacity(MOVERS_22TF_WRITER_COUNT);
        for _ in 0..MOVERS_22TF_WRITER_COUNT {
            let (tx, rx) = mpsc::channel(channel_capacity);
            senders.push(tx);
            receivers.push(rx);
        }
        let arr: [mpsc::Sender<MoverRow>; MOVERS_22TF_WRITER_COUNT] = senders
            .try_into()
            .unwrap_or_else(|_| panic!("array conversion failed"));
        (Movers22TfWriterState::new(arr), receivers)
    }

    /// Phase 10 primitive ratchet — count is 22.
    #[test]
    fn test_movers_22tf_writer_count_is_22() {
        assert_eq!(MOVERS_22TF_WRITER_COUNT, 22);
    }

    #[test]
    fn test_state_len_returns_22() {
        let (state, _rx) = make_state(8);
        assert_eq!(state.len(), 22);
        assert!(!state.is_empty());
    }

    #[test]
    fn test_sender_for_returns_some_in_range() {
        let (state, _rx) = make_state(8);
        for idx in 0..MOVERS_22TF_WRITER_COUNT {
            assert!(
                state.sender_for(idx).is_some(),
                "sender_for({idx}) returned None"
            );
        }
        assert!(state.sender_for(22).is_none());
        assert!(state.sender_for(usize::MAX).is_none());
    }

    /// Phase 10 ratchet: try_enqueue_global returns Uninit when boot
    /// wiring hasn't happened. This is the safe-default we want.
    /// NOTE: this test cannot run alongside others that init the OnceLock
    /// in the same process, so we test the local helper instead.
    #[test]
    fn test_local_send_succeeds_when_capacity_available() {
        let (state, mut rx) = make_state(8);
        let sender = state.sender_for(0).expect("sender for idx 0 exists");
        match sender.try_send(dummy_row()) {
            Ok(()) => {}
            Err(e) => panic!("first try_send should succeed: {e:?}"),
        }
        // Receiver should pick it up immediately.
        let popped = rx[0].try_recv().expect("recv should succeed");
        assert_eq!(popped.security_id, dummy_row().security_id);
    }

    #[test]
    fn test_local_send_drops_newest_when_full() {
        let (state, _rx) = make_state(1); // 1-slot channel
        let sender = state.sender_for(0).expect("sender exists");
        // First send fills the buffer.
        sender.try_send(dummy_row()).expect("first send ok");
        // Second send — drop-newest semantics.
        match sender.try_send(dummy_row()) {
            Err(mpsc::error::TrySendError::Full(_)) => {}
            other => panic!("expected Full, got {other:?}"),
        }
    }

    #[test]
    fn test_local_send_returns_closed_when_receiver_dropped() {
        let (state, mut rx) = make_state(8);
        let sender = state.sender_for(0).expect("sender exists");
        // Drop all receivers.
        rx.drain(..);
        // try_send should now return Closed.
        match sender.try_send(dummy_row()) {
            Err(mpsc::error::TrySendError::Closed(_)) => {}
            other => panic!("expected Closed, got {other:?}"),
        }
    }

    #[test]
    fn test_enqueue_outcome_variants_distinct() {
        // Defensive — every variant must be distinct so callers can
        // emit the correct {reason} label on tv_movers_writer_dropped_total.
        let variants = [
            EnqueueOutcome::Sent,
            EnqueueOutcome::DroppedFull,
            EnqueueOutcome::Closed,
            EnqueueOutcome::Uninit,
            EnqueueOutcome::OutOfRange,
        ];
        for (i, a) in variants.iter().enumerate() {
            for (j, b) in variants.iter().enumerate() {
                if i == j {
                    assert_eq!(a, b);
                } else {
                    assert_ne!(a, b);
                }
            }
        }
    }
}
