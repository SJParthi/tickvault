//! How a WAL shutdown ended (plan item K2, 2026-10-26), kept out of
//! `ws_frame_spill.rs` so the parent stays under its size pin.

/// How [`WsFrameSpill::shutdown`] ended (plan item K2, 2026-10-26).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum WalShutdownOutcome {
    /// Queue empty and the writer exited inside the budget.
    Drained,
    /// The budget expired: `queued` records never left the channel, and an
    /// abandoned writer still held `unflushed` records in its buffer. Either
    /// part may be zero (a writer stuck with nothing buffered loses nothing,
    /// but the drain was still not completed).
    Abandoned { queued: usize, unflushed: u64 },
}

impl WalShutdownOutcome {
    /// Records this shutdown lost (the value `shutdown` returns).
    pub(super) fn lost_records(self) -> usize {
        match self {
            Self::Drained => 0,
            Self::Abandoned { queued, unflushed } => {
                queued.saturating_add(usize::try_from(unflushed).unwrap_or(usize::MAX))
            }
        }
    }
}
