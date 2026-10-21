//! Why a boot WAL catch-up drain stopped, named honestly (2026-10-20, plan
//! item PR W).
//!
//! The catch-up summary line's `stop_reason` fell through to `"drained"` for
//! every stop it did not name. A drain whose last replay pass came back empty
//! because the pass hit its frame cap, its memory guard or the disk floor, or
//! deferred segments past its byte budget, read as `"drained"` while segments
//! were still waiting on disk; so did a drain that stopped because a batch was
//! never acknowledged, or because the replay could not read. Only a drain that
//! really emptied the backlog (`catchup_drained`) may say `"drained"` now.
//!
//! Pure and O(1): a few branch tests, once per boot, no allocation.
//!
//! The lane's wiring is held by the compiler, not by a source scan: every
//! item here is `pub(super)` and used only by the lane, so removing one of
//! its uses (falling back to a literal `"drained"`, or dropping an early-stop
//! assignment) leaves the item unused, and `dead_code` under the CI clippy
//! run (`-D warnings`) fails the build.

/// The early stop the lane records before it knows a more precise one.
pub(super) const STOP_INCOMPLETE: &str = "incomplete";
/// The replay could not read another batch.
pub(super) const STOP_REPLAY_FAILED: &str = "replay_failed";
/// A replayed batch was never acknowledged by the sinks.
pub(super) const STOP_ACK_TIMEOUT: &str = "ack_timeout";

/// The reason for an early stop when the final replay pass returned no
/// frames: `frame_cap`, `memory`, `disk` or `deferred`, in that order of
/// precedence; `None` when that pass found the backlog empty.
#[must_use]
pub(super) const fn empty_pass_stop_reason(
    stopped_for_frame_cap: bool,
    stopped_for_memory: bool,
    stopped_for_disk: bool,
    deferred_segments: u64,
) -> Option<&'static str> {
    if stopped_for_frame_cap {
        Some("frame_cap")
    } else if stopped_for_memory {
        Some("memory")
    } else if stopped_for_disk {
        Some("disk")
    } else if deferred_segments > 0 {
        Some("deferred")
    } else {
        None
    }
}

/// [`empty_pass_stop_reason`] read from a replay batch's own flags. A helper
/// so the lane's call fits on one line of `dhan_feed_stack.rs`, which sits at
/// its size pin.
#[must_use]
pub(super) const fn batch_stop_reason(
    batch: &tickvault_storage::ws_frame_spill::WalReplayBatch,
) -> Option<&'static str> {
    empty_pass_stop_reason(
        batch.stopped_for_frame_cap,
        batch.stopped_for_memory,
        batch.stopped_for_disk,
        batch.deferred_segments,
    )
}

/// What the summary reports after the named stops (shutdown, memory, apply
/// lag, round cap, clock) were ruled out: `"drained"` only when the drain
/// really emptied the backlog, otherwise the early stop the loop recorded.
#[must_use]
pub(super) const fn residual_stop_reason(drained: bool, early_stop: &'static str) -> &'static str {
    if drained { "drained" } else { early_stop }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_batch_stop_reason_reads_the_batch_flags() {
        use tickvault_storage::ws_frame_spill::WalReplayBatch;
        assert_eq!(batch_stop_reason(&WalReplayBatch::default()), None);
        let disk = WalReplayBatch {
            stopped_for_disk: true,
            ..WalReplayBatch::default()
        };
        assert_eq!(batch_stop_reason(&disk), Some("disk"));
        let deferred = WalReplayBatch {
            deferred_segments: 2,
            ..WalReplayBatch::default()
        };
        assert_eq!(batch_stop_reason(&deferred), Some("deferred"));
        let cap = WalReplayBatch {
            stopped_for_frame_cap: true,
            stopped_for_disk: true,
            ..WalReplayBatch::default()
        };
        assert_eq!(batch_stop_reason(&cap), Some("frame_cap"));
    }

    #[test]
    fn test_empty_pass_stop_reason_names_every_early_stop() {
        assert_eq!(
            empty_pass_stop_reason(true, false, false, 0),
            Some("frame_cap")
        );
        assert_eq!(
            empty_pass_stop_reason(false, true, false, 0),
            Some("memory")
        );
        assert_eq!(empty_pass_stop_reason(false, false, true, 0), Some("disk"));
        assert_eq!(
            empty_pass_stop_reason(false, false, false, 3),
            Some("deferred")
        );
        assert_eq!(empty_pass_stop_reason(false, false, false, 0), None);
        // Precedence when several hold at once: the cap first.
        assert_eq!(
            empty_pass_stop_reason(true, true, true, 9),
            Some("frame_cap")
        );
    }

    /// Regression: 2026-10-20 — a catch-up whose last pass stopped at its
    /// frame cap reported `stop_reason = "drained"` while segments waited.
    #[test]
    fn test_catchup_stop_reason_frame_cap_is_not_drained() {
        let early = empty_pass_stop_reason(true, false, false, 0).unwrap_or(STOP_INCOMPLETE);
        assert_eq!(residual_stop_reason(false, early), "frame_cap");
        assert_ne!(residual_stop_reason(false, early), "drained");
        assert_eq!(residual_stop_reason(true, STOP_INCOMPLETE), "drained");
        assert_eq!(residual_stop_reason(false, STOP_ACK_TIMEOUT), "ack_timeout");
        assert_eq!(
            residual_stop_reason(false, STOP_REPLAY_FAILED),
            "replay_failed"
        );
    }

    /// Every early stop the lane can record, and every empty-pass verdict,
    /// reads as itself unless the drain really emptied the backlog; none of
    /// them reads `"drained"` on its own. Exhaustive over the four inputs
    /// that matter (three flags and whether segments were deferred).
    #[test]
    fn test_residual_stop_reason_is_drained_only_when_the_backlog_emptied() {
        let early_stops = [STOP_INCOMPLETE, STOP_REPLAY_FAILED, STOP_ACK_TIMEOUT];
        for &early in &early_stops {
            assert_eq!(residual_stop_reason(true, early), "drained");
            assert_eq!(residual_stop_reason(false, early), early);
            assert_ne!(residual_stop_reason(false, early), "drained");
        }
        for bits in 0_u8..16 {
            let (cap, mem, disk) = (bits & 1 != 0, bits & 2 != 0, bits & 4 != 0);
            let deferred = u64::from(bits & 8 != 0);
            for &prior in &early_stops {
                let early = empty_pass_stop_reason(cap, mem, disk, deferred).unwrap_or(prior);
                let reported = residual_stop_reason(false, early);
                assert_ne!(reported, "drained", "bits {bits:04b}, prior {prior}");
                if cap || mem || disk || deferred > 0 {
                    assert_ne!(reported, prior, "a named stop wins, bits {bits:04b}");
                } else {
                    assert_eq!(reported, prior, "an empty backlog keeps the prior stop");
                }
            }
        }
        // The three names are distinct and none is the drained name.
        assert_ne!(STOP_INCOMPLETE, STOP_REPLAY_FAILED);
        assert_ne!(STOP_INCOMPLETE, STOP_ACK_TIMEOUT);
        assert_ne!(STOP_REPLAY_FAILED, STOP_ACK_TIMEOUT);
        assert!(early_stops.iter().all(|s| *s != "drained"));
    }
}
