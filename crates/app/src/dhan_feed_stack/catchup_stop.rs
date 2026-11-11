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
/// The partition archiver held the old-hour writers out for longer than the
/// bounded wait (PR AA, owner Quote 51): the segments wait for the next drain.
pub(super) const STOP_ARCHIVE_QUIESCE: &str = "archive_quiesce";

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

/// Whether a quiesce stop needs a line of its own. The catch-up summary is
/// written only after a completed round or a named stop, so a drain the
/// archive lock stopped in round 0 (the usual case, the first chunk of an
/// evening drain) left only a counter (PR AA finding 16).
#[must_use]
pub(super) fn quiesce_stop_needs_own_line(rounds: u32, early_stop: &str) -> bool {
    rounds == 0 && early_stop == STOP_ARCHIVE_QUIESCE
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression, PR AA finding 16: a quiesce stop in round 0 logged no
    /// `stop_reason`. It now gets its own line; a later round's stop is
    /// already named by the summary, and other stops never take this line.
    #[test]
    fn test_regression_round_zero_quiesce_stop_is_logged() {
        assert!(quiesce_stop_needs_own_line(0, STOP_ARCHIVE_QUIESCE));
        assert!(!quiesce_stop_needs_own_line(1, STOP_ARCHIVE_QUIESCE));
        for other in [STOP_INCOMPLETE, STOP_REPLAY_FAILED, STOP_ACK_TIMEOUT] {
            assert!(!quiesce_stop_needs_own_line(0, other));
        }
        let code = lane_code();
        let stop = code
            .find("catchup_early_stop = catchup_stop::STOP_ARCHIVE_QUIESCE;")
            .expect("the quiesce stop");
        let tail = &code[stop..];
        let line = tail
            .find("quiesce_stop_needs_own_line(rounds, catchup_early_stop)")
            .expect("the round-0 line");
        let brk = tail.find("break;").expect("the stop breaks");
        assert!(line < brk, "the line is written before the drain stops");
        assert!(tail[line..brk].contains("stop_reason = catchup_early_stop"));
    }

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

    /// PR AA step 1: a drain the archive quiesce lock stopped reports
    /// `archive_quiesce`, never `drained`.
    #[test]
    fn test_catchup_stops_with_archive_quiesce_reason() {
        assert_eq!(
            residual_stop_reason(false, STOP_ARCHIVE_QUIESCE),
            "archive_quiesce"
        );
        assert_eq!(residual_stop_reason(true, STOP_ARCHIVE_QUIESCE), "drained");
    }

    /// The lane's code, with its comments removed.
    fn lane_code() -> String {
        include_str!("../dhan_feed_stack.rs")
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// PR AA step 1: each catch-up chunk takes the archive quiesce lock
    /// BEFORE its refold, and a miss stops the drain with `archive_quiesce`
    /// before the segment settles.
    #[test]
    fn test_catchup_chunk_takes_the_quiesce_lock_before_its_refold() {
        let code = lane_code();
        let chunk = code
            .find("if !chunk.is_empty() {")
            .expect("the catch-up chunk loop must exist");
        let body = &code[chunk..];
        let lock = body
            .find("aq::QuiesceWriter::CatchUp")
            .expect("each chunk must take the quiesce lock");
        let refold = body
            .find("refold_wal_frames(&mut ingest, chunk")
            .expect("the chunk refold must exist");
        let ack = body
            .find("replay_rows_landed(&mut ingest, \"catchup\"")
            .expect("the chunk ack must exist");
        let settle = body
            .find("settle_confirm_segments(")
            .expect("the segment settle must exist");
        assert!(lock < refold && refold < ack && ack < settle);
        assert!(
            body[lock..refold].contains("round_quiesce_held = true;"),
            "a miss must record the stop before any refold"
        );
        assert!(
            code.contains("catchup_early_stop = catchup_stop::STOP_ARCHIVE_QUIESCE;"),
            "a quiesce-held round must name its stop"
        );
        // PR AA review: the chunk waits to the drain's own deadline (not a
        // fixed 60 s), and the replay is registered before the boot refold,
        // so the archiver starts no new partition while it runs.
        assert!(
            body[..lock].contains("aq::archive_quiesce_for_replay_until(")
                && body[lock..refold].contains("catchup_deadline,"),
            "each chunk waits until the drain's deadline"
        );
        let intent = code
            .find("unwrap_or_else(tickvault_storage::archive_quiesce::intend_old_hour_replay)")
            .expect("the replay must be registered");
        let boot_lock = code
            .find("QuiesceWriter::BootRefold")
            .expect("the boot refold lock");
        assert!(
            intent < boot_lock,
            "registered before the boot refold waits"
        );
    }

    /// Regression, PR AA finding 6: the replay used to be registered only
    /// inside the lane, after the token wait, the seeding and the seal-writer
    /// wait, while both archive loops were already running; on an evening
    /// restart the archiver took the lock first and the drain could miss
    /// its deadline. `main.rs` now registers it BEFORE either archive loop
    /// spawns and hands it to the lane, which reuses it at the refold.
    #[test]
    fn test_regression_boot_replay_is_registered_before_the_archive_loops_spawn() {
        let main_code: String = include_str!("../main.rs")
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        let intent = main_code
            .find("dhan_feed_stack::boot_replay_intent(&ws_wal_replay_live_feed)")
            .expect("main.rs must register the boot replay");
        for archive_loop in [
            "spawn_supervised_disk_pressure_loop(",
            "spawn_supervised_daily_archive_loop(",
        ] {
            let spawn = main_code
                .find(archive_loop)
                .expect("the archive loop spawn must exist");
            assert!(
                intent < spawn,
                "the replay must be registered before {archive_loop}"
            );
        }
        let handed = main_code
            .find("boot_replay_intent,")
            .expect("main.rs must hand the registration to the lane");
        let lane_spawn = main_code
            .find("dhan_feed_stack::spawn_dhan_feed_stack(")
            .expect("the lane spawn must exist");
        assert!(intent < handed && handed > lane_spawn);
        // The lane reuses it at the refold, and keeps it through the dial
        // wait only up to the authentication grace.
        let lane = lane_code();
        assert!(lane.contains(".boot_replay_intent\n            .take()"));
        let dial = lane
            .find("wait_for_dial_preconditions(&params.instance_lock_held")
            .expect("the dial wait");
        assert!(
            lane[dial..].contains("if params.boot_replay_intent.is_some() =>"),
            "the dial wait releases the registration at the grace"
        );
    }

    /// PR AA finding 6: with nothing staged there is no registration, so the
    /// archiver is never held back on a clean boot. (That a registration
    /// holds the archiver out is pinned in `archive_quiesce`'s own tests.)
    #[test]
    fn test_boot_replay_intent_only_for_staged_frames() {
        let none: [u8; 0] = [];
        assert!(super::super::boot_replay_intent(&none).is_none());
        let intent = super::super::boot_replay_intent(&[1u8]);
        assert!(intent.is_some(), "staged frames register the replay");
        drop(intent);
    }

    /// PR AA step 1: the boot refold takes the lock (try-only in session)
    /// before it folds, folds nothing on a miss, and lets go right after its
    /// flush and ack, before the catch-up drain.
    #[test]
    fn test_boot_refold_takes_the_quiesce_lock_and_releases_it_before_catchup() {
        let code = lane_code();
        let lock = code
            .find("QuiesceWriter::BootRefold")
            .expect("the boot refold must take the quiesce lock");
        let miss = code
            .find("} else if boot_quiesce.is_none() {")
            .expect("a miss must fold nothing");
        let refold = code
            .find("refold_wal_frames(&mut ingest, boot_frames")
            .expect("the boot refold must exist");
        let ack = code
            .find("replay_rows_landed(&mut ingest, \"boot_refold\"")
            .expect("the boot ack must exist");
        let release = code
            .find("drop(boot_quiesce);")
            .expect("the lock must be released explicitly");
        let catchup = code
            .find("let catchup_budget_secs")
            .expect("the catch-up drain must exist");
        assert!(lock < miss && miss < refold && refold < ack && ack < release);
        assert!(
            release < catchup,
            "never held into the catch-up or the live lane"
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
        let early_stops = [
            STOP_INCOMPLETE,
            STOP_REPLAY_FAILED,
            STOP_ACK_TIMEOUT,
            STOP_ARCHIVE_QUIESCE,
        ];
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
        for other in [STOP_INCOMPLETE, STOP_REPLAY_FAILED, STOP_ACK_TIMEOUT] {
            assert_ne!(STOP_ARCHIVE_QUIESCE, other);
        }
        assert!(early_stops.iter().all(|s| *s != "drained"));
    }
}
