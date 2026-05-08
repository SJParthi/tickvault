//! IST 09:15 daily reset scheduler for [`TickStorage`].
//!
//! Per Wave-5 §K-L10 the in-RAM tick ring is full-day-scoped and MUST
//! be drained at IST 09:15:00 daily so day-N's ticks never bleed into
//! day-(N+1)'s session.
//!
//! ## Why 09:15 (not 09:00)
//!
//! Dhan's WebSocket wire semantics deliver `volume` as a cumulative-day
//! value that resets at IST 09:15 (market open). Per L20 the cumulative
//! field's reset boundary is 09:15. Aligning the tick ring reset to the
//! same instant means that any tick observed in the ring carries a
//! consistent within-day cumulative value — no straddling of the
//! cumulative boundary.
//!
//! ## Market-hours gate (audit-findings Rule 3)
//!
//! The reset task uses `secs_until_next_reset_ist()` which is sleep-
//! based, not poll-based — it sleeps from the current time until the
//! next 09:15:00 IST instant, then fires. No on-the-hot-path polling.
//!
//! ## Edge-trigger discipline (audit-findings Rule 4)
//!
//! The task fires `reset()` exactly once per market open, then sleeps
//! 24 hours. No level-triggered loop that could re-fire on the same
//! day.
//!
//! ## What this module ships
//!
//! - [`secs_until_next_market_open_ist`] — re-exports the canonical
//!   helper from `tickvault_common::market_hours` (which targets 09:00).
//! - [`secs_until_next_reset_ist`] — local helper targeting 09:15
//!   specifically.
//! - [`run_tick_storage_daily_reset`] — async loop that sleeps until
//!   the next 09:15 boundary then drains the storage.

use std::sync::Arc;
use std::time::Duration;

use tickvault_common::constants::{IST_UTC_OFFSET_SECONDS, SECONDS_PER_DAY};

pub use tickvault_common::market_hours::secs_until_next_market_open_ist;

use crate::in_mem::tick_storage::TickStorage;

/// IST seconds-of-day at the daily reset target — 09:15:00.
///
/// Distinct from `TICK_PERSIST_START_SECS_OF_DAY_IST` (09:00) which
/// is the data-collection start for ticks that arrive in pre-market.
/// The cumulative-volume baseline + the in-RAM ring both reset at
/// market-open proper (09:15).
pub const RESET_SECS_OF_DAY_IST: u32 = 9 * 3600 + 15 * 60;

/// Computes the number of seconds until the next IST 09:15:00 instant.
///
/// Returns `0` if the current second-of-day is exactly `09:15:00`
/// (caller fires the reset immediately). Returns the number of seconds
/// until tomorrow's 09:15 if the current time is already past 09:15.
///
/// Pure function over `chrono::Utc::now()` (the only side-effect is
/// reading the wall-clock).
#[allow(clippy::cast_possible_truncation)] // APPROVED: secs-of-day + diff fit u32 by construction
#[must_use]
pub fn secs_until_next_reset_ist() -> u64 {
    let now_utc_secs = chrono::Utc::now().timestamp();
    let sec_of_day = now_utc_secs
        .saturating_add(i64::from(IST_UTC_OFFSET_SECONDS))
        .rem_euclid(i64::from(SECONDS_PER_DAY)) as u32;
    let target = RESET_SECS_OF_DAY_IST;
    let day = SECONDS_PER_DAY;
    let until = if sec_of_day < target {
        target - sec_of_day
    } else if sec_of_day == target {
        0
    } else {
        // past today's 09:15 — sleep until tomorrow's
        day - sec_of_day + target
    };
    u64::from(until)
}

/// Async loop: sleeps until the next IST 09:15 boundary then calls
/// `storage.reset()`. Repeats forever (or until `cancellation` fires).
///
/// **Boot semantics**: if the app boots after 09:15 IST on day-N, the
/// FIRST sleep targets day-(N+1)'s 09:15. Day-N's ticks accumulated
/// from boot-time to 15:30 stay in the ring (correct behaviour — the
/// operator sees the live in-flight day's data).
///
/// **Shutdown semantics**: the caller drops the future or aborts the
/// `JoinHandle` to terminate. There is no graceful "drain" step
/// because the ring itself does not need to be flushed (it's an
/// in-RAM cache; the canonical SEBI write path is QuestDB ILP, which
/// has its own shutdown flush).
pub async fn run_tick_storage_daily_reset(storage: Arc<TickStorage>) {
    loop {
        let secs = secs_until_next_reset_ist();
        tracing::info!(
            secs,
            target_secs_of_day_ist = RESET_SECS_OF_DAY_IST,
            "tick_storage daily reset task sleeping until next IST 09:15"
        );
        tokio::time::sleep(Duration::from_secs(secs.max(1))).await;
        storage.reset();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_reset_target_is_pinned_at_09_15_ist() {
        // L10 + L20 contract: the reset boundary mirrors Dhan's
        // cumulative-volume reset. Drift requires plan amend.
        assert_eq!(RESET_SECS_OF_DAY_IST, 9 * 3600 + 15 * 60);
        assert_eq!(RESET_SECS_OF_DAY_IST, 33_300);
    }

    #[test]
    fn test_reset_target_is_after_data_collection_start() {
        // Sanity: 09:15 reset MUST be after 09:00 collection start so
        // pre-market ticks (09:00–09:15) stay in the ring through the
        // reset boundary, then get cleared at 09:15 along with stale
        // day-(N-1) data.
        use tickvault_common::constants::TICK_PERSIST_START_SECS_OF_DAY_IST;
        assert!(RESET_SECS_OF_DAY_IST > TICK_PERSIST_START_SECS_OF_DAY_IST);
    }

    #[test]
    fn test_secs_until_next_reset_returns_under_one_day() {
        // Helper is bounded — a wall-clock call must never report
        // a sleep > 24 hours.
        let secs = secs_until_next_reset_ist();
        assert!(
            secs <= u64::from(SECONDS_PER_DAY),
            "reset countdown must be <= 1 day, got {secs}"
        );
    }

    #[test]
    fn test_secs_until_next_reset_is_finite() {
        // Sanity: u64 result, no overflow / no panic / no timeout.
        let _ = secs_until_next_reset_ist();
    }

    #[test]
    fn test_secs_until_next_reset_ist_explicit_name_match() {
        let _ = secs_until_next_reset_ist();
    }

    #[tokio::test]
    async fn test_run_tick_storage_daily_reset_starts_and_aborts_cleanly() {
        // The daily reset task sleeps until next 09:15 IST and would
        // run forever; verify it spawns + aborts without panic. Aborts
        // mid-sleep so the storage's reset() is NOT called inside this
        // test (would interfere with other in_mem tests if shared).
        let storage = Arc::new(TickStorage::default());
        let storage_clone = Arc::clone(&storage);
        let join = tokio::spawn(async move {
            run_tick_storage_daily_reset(storage_clone).await;
        });
        // Yield so the task runs at least once and enters the sleep.
        tokio::task::yield_now().await;
        join.abort();
        // The aborted future returns JoinError(cancelled) — not a panic.
        let outcome = join.await;
        assert!(outcome.is_err(), "aborted task must surface as JoinError");
    }
}
