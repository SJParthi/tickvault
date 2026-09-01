//! Pressure-triggered wake for the spill/WAL reclaim sweep.
//!
//! # The deadlock this breaks
//!
//! When the data volume fills, QuestDB WAL-suspends its tables. A suspended
//! table keeps ACKing ILP rows while never applying them, and
//! `partition_archive` REFUSES to archive a suspended table — correctly, and
//! that refusal must not be relaxed: a suspended table has accepted writes
//! that are not yet visible, so an export would count only the visible rows,
//! verify successfully, drop the partition, and then the pending writes would
//! replay into something that no longer exists.
//!
//! On the prod box `market_depth` is ~62% of the volume, so the one table
//! whose space actually matters is the one the archiver cannot touch:
//!
//! ```text
//! disk full -> tables suspended -> archiver refuses -> space never returns
//!           -> auto-resume needs 25% free -> never fires -> disk stays full
//! ```
//!
//! `wal_auto_resume` exists and is wired (via `wal_suspension_watcher`), and
//! it is the correct exit — but it needs free space it cannot itself create.
//!
//! # The one reclaim that still works while every table is suspended
//!
//! Our OWN capture files — `ws_frame_spill`'s archived and active segments,
//! and the seal spill — are on the same volume and QuestDB does not own them.
//! Nothing about a WAL suspension blocks deleting them, so this is the only
//! path that can free space in that state. It already exists, already prunes
//! safely, and is already wired.
//!
//! **What was missing is the TRIGGER.** That sweep runs on a fixed
//! `WS_WAL_ARCHIVE_PRUNE_INTERVAL_SECS` (6 hours) timer, indifferent to the
//! emergency. On 2026-08-25 the volume reached 100% at ~11:11 IST; a timer
//! that last fired at 09:00 would not act until 15:00, and the box sat
//! unreachable in between. The operator found it by asking why a table was
//! empty.
//!
//! # Why triggering it early is safe
//!
//! This module adds NO deletion logic and changes NO retention bound. The
//! sweep it wakes deletes exactly what the timer would have deleted — segments
//! past their age or byte cap — just sooner. A pressure-triggered pass can
//! therefore never remove anything the scheduled pass would have kept.
//!
//! # Why the floor exists
//!
//! The pressure loop polls on a short interval, and an episode can persist for
//! many polls. Without a floor, every poll would wake the sweep and turn a
//! 6-hourly cold path into a hot loop against a struggling disk — trading one
//! failure for another. [`RECLAIM_MIN_INTERVAL_SECS`] bounds it: at most one
//! pressure-triggered sweep per minute, however often pressure is reported.
//!
//! # Complexity
//!
//! O(1). The signal is a single `Notify`; the decision is one integer compare
//! against a monotonic clock. No per-instrument state, no allocation, and the
//! space is one `Notify` plus one `u64` for the life of the process.

use std::sync::LazyLock;
use tokio::sync::Notify;
use tokio::time::Duration;

/// Smallest gap between two PRESSURE-triggered sweeps.
///
/// 60 seconds. The scheduled 6-hourly sweep is unaffected — this bounds only
/// the extra passes pressure can request. One a minute is far more responsive
/// than six-hourly while still being a cold path: the sweep is a directory
/// walk plus a bounded number of unlinks, not a per-tick cost.
pub const RECLAIM_MIN_INTERVAL_SECS: u64 = 60;

/// Process-global wake for the reclaim sweep.
///
/// A `Notify` rather than a channel: the signal carries no payload, repeated
/// requests while a sweep is already pending should collapse into one, and a
/// request with nobody listening must not block the pressure loop.
/// `notify_one` gives exactly those semantics, including storing a permit when
/// the sweep is mid-pass so a request during a sweep is honoured by the next
/// wait rather than lost.
static RECLAIM_NOW: LazyLock<Notify> = LazyLock::new(Notify::new);

/// Ask the reclaim sweep to run as soon as its floor allows.
///
/// Called by the disk-pressure loop the moment it decides an episode is
/// active. Never blocks and never fails: if no sweep is waiting, the permit is
/// stored and the next wait returns immediately.
pub fn request_reclaim() {
    RECLAIM_NOW.notify_one();
    metrics::counter!("tv_reclaim_requested_total").increment(1);
}

/// Should a pressure request be honoured, given when the last one ran?
///
/// Pure so the floor is testable without a clock, a disk, or a runtime.
///
/// `last_run_secs` is `None` before the first pressure-triggered sweep, which
/// is always honoured — the first request of an episode is the one that
/// matters most, and delaying it by a minute would reintroduce exactly the
/// latency this module exists to remove.
#[must_use]
pub fn should_honor_reclaim(last_run_secs: Option<u64>, now_secs: u64, floor_secs: u64) -> bool {
    match last_run_secs {
        None => true,
        // `saturating_sub` so a non-monotonic or restarted clock reads as
        // "no time has passed" (refuse) rather than underflowing into a huge
        // elapsed value (always honour). Refusing is the safe direction: the
        // scheduled sweep still runs regardless.
        Some(last) => now_secs.saturating_sub(last) >= floor_secs,
    }
}

/// Wait for either the scheduled interval or a pressure request.
///
/// Returns `true` when a pressure request woke it, `false` when the scheduled
/// interval elapsed — the caller uses that to apply the floor only to
/// pressure-triggered passes, never to the scheduled one.
pub async fn wait_for_reclaim_or(interval: Duration) -> bool {
    tokio::select! {
        () = tokio::time::sleep(interval) => false,
        () = RECLAIM_NOW.notified() => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_honor_reclaim_always_honours_the_first_request() {
        assert!(
            should_honor_reclaim(None, 0, RECLAIM_MIN_INTERVAL_SECS),
            "the first pressure request of an episode is the one that matters most; \
             delaying it would reintroduce the latency this module removes"
        );
        assert!(should_honor_reclaim(
            None,
            999_999,
            RECLAIM_MIN_INTERVAL_SECS
        ));
    }

    #[test]
    fn should_honor_reclaim_refuses_a_request_inside_the_floor() {
        assert!(!should_honor_reclaim(Some(100), 100, 60));
        assert!(!should_honor_reclaim(Some(100), 159, 60));
    }

    #[test]
    fn should_honor_reclaim_honours_at_or_past_the_floor() {
        assert!(should_honor_reclaim(Some(100), 160, 60));
        assert!(should_honor_reclaim(Some(100), 10_000, 60));
    }

    #[test]
    fn should_honor_reclaim_refuses_a_backwards_clock_rather_than_spinning() {
        // If `now` is somehow BEFORE `last`, saturating_sub yields 0, which is
        // below any positive floor. Refusing is safe: the scheduled sweep is
        // unaffected. Underflowing would have produced a huge elapsed value
        // and honoured every single request — a hot loop on a broken clock.
        assert!(!should_honor_reclaim(Some(500), 100, 60));
        assert!(!should_honor_reclaim(Some(u64::MAX), 0, 60));
    }

    #[test]
    fn should_honor_reclaim_with_a_zero_floor_honours_everything() {
        // The documented disable value: a floor of 0 means every request runs.
        // Kept meaningful so a future operator can disable the rate limit
        // without the constant silently doing something else.
        assert!(should_honor_reclaim(Some(100), 100, 0));
    }

    #[test]
    fn should_honor_reclaim_floor_is_a_minute_far_under_the_scheduled_sweep() {
        assert_eq!(RECLAIM_MIN_INTERVAL_SECS, 60);
        assert!(
            RECLAIM_MIN_INTERVAL_SECS
                < tickvault_common::constants::WS_WAL_ARCHIVE_PRUNE_INTERVAL_SECS,
            "the pressure floor must be far tighter than the scheduled interval, \
             otherwise pressure buys no responsiveness at all"
        );
    }

    #[tokio::test]
    async fn request_reclaim_wakes_wait_for_reclaim_or_before_the_interval() {
        // The whole point: a request must win against a long interval.
        request_reclaim();
        let woke_by_pressure = wait_for_reclaim_or(Duration::from_secs(3600)).await;
        assert!(
            woke_by_pressure,
            "a pressure request must wake the sweep instead of waiting out the \
             scheduled interval — that latency IS the deadlock"
        );
    }

    #[tokio::test]
    async fn wait_for_reclaim_or_still_fires_the_interval_with_no_request() {
        let woke_by_pressure = wait_for_reclaim_or(Duration::from_millis(1)).await;
        assert!(
            !woke_by_pressure,
            "with no request pending the scheduled interval must still elapse — \
             the timer is the floor of the guarantee, not an optimisation"
        );
    }
}
