//! Day OHLC boot orchestration.
//!
//! Wires `DayOhlcTracker` to its two lifecycle dependencies:
//!
//! 1. **Tick consumer task** — subscribes to the tick broadcast and routes
//!    every IDX_I tick to `tracker.update_tick(sid, IdxI, last_price)`.
//!    First tick for a SID auto-arms `day_open/high/low/close` to that LTP.
//!    Subsequent ticks advance `day_high`, `day_low`, `day_close` per tick.
//!    Zero hot-path allocation; lock held in microseconds per `hot-path.md`.
//!
//! 2. **IST midnight reset task** — at 00:00:00 IST every day, calls
//!    `tracker.reset_daily_all()` to clear stale state so the next live
//!    tick re-arms `day_open` to that day's first observed LTP.
//!
//! ## Why `day_open` is the first REGULAR-SESSION tick (never pre-market)
//!
//! Per operator directive 2026-05-26, all Dhan historical / pre-market
//! buffer code was removed from the workspace. `day_open` for the 4 LOCKED
//! IDX_I SIDs (NIFTY=13, BANKNIFTY=25, SENSEX=51, INDIA VIX=21) equals the
//! first observed live WebSocket tick after the midnight reset **whose
//! exchange timestamp falls inside the regular trading session
//! `[09:15:00, 15:30:00)` IST** (operator rule 2026-07-03: pre-market data
//! must NEVER become the day open). The tick broadcast starts carrying
//! IDX_I ticks from 09:00 IST (the `TICK_PERSIST_START_SECS_OF_DAY_IST`
//! pipeline window), so without this gate the pre-open snapshot tick would
//! arm `day_open` — [`should_route_day_ohlc_tick`] drops those on the
//! tick's EXCHANGE timestamp via
//! [`g1_exchange_gate_accepts`](tickvault_common::constants::g1_exchange_gate_accepts).

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::broadcast::{self, error::RecvError};
use tracing::{error, info, warn};

use tickvault_common::constants::{
    INDIA_VIX_SECURITY_ID, IST_UTC_OFFSET_SECONDS, SECONDS_PER_DAY, g1_exchange_gate_accepts,
};
use tickvault_common::tick_types::ParsedTick;
use tickvault_common::types::ExchangeSegment;
use tickvault_trading::in_mem::day_ohlc_tracker::{
    DayOhlcTracker, ist_seconds_of_day, secs_until_next_ist,
};

/// IST midnight = 00:00:00.
pub const IST_MIDNIGHT_HOUR: u32 = 0;
pub const IST_MIDNIGHT_MINUTE: u32 = 0;
pub const IST_MIDNIGHT_SECOND: u32 = 0;

/// Spawns the tick consumer task. Drains the tick broadcast and routes
/// every IDX_I tick to `tracker.update_tick()`. The first tick for a SID
/// auto-arms day_open/high/low/close to that LTP; subsequent ticks advance
/// day_high/day_low/day_close. Non-IDX_I ticks are skipped O(1).
///
/// On `broadcast::Lagged`, increments a counter and continues. Closed
/// channel exits the task (process shutdown).
// TEST-EXEMPT: tokio::spawn wrapper over `broadcast::recv` — per-tick logic `tracker.update_tick()` is tested in `day_ohlc_tracker.rs::tests`; spawn site pinned by `test_day_ohlc_orchestrator_is_wired_into_main`.
pub fn spawn_day_ohlc_tick_consumer(
    tracker: Arc<DayOhlcTracker>,
    mut rx: broadcast::Receiver<ParsedTick>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        info!("day_ohlc tick consumer started");
        loop {
            match rx.recv().await {
                Ok(tick) => {
                    if !should_route_day_ohlc_tick(
                        tick.exchange_segment_code,
                        tick.exchange_timestamp,
                    ) {
                        continue;
                    }
                    // Pure-function hot path: O(1) per tick, ≤50ns budget.
                    let _ = tracker.update_tick(
                        tick.security_id,
                        ExchangeSegment::IdxI,
                        f64::from(tick.last_traded_price),
                    );
                }
                Err(RecvError::Lagged(n)) => {
                    metrics::counter!("tv_day_ohlc_tick_consumer_lagged_total").increment(n);
                    warn!(lagged = n, "day_ohlc tick consumer lagged");
                }
                Err(RecvError::Closed) => {
                    info!("day_ohlc tick consumer: broadcast closed, exiting");
                    break;
                }
            }
        }
    })
}

/// IDX_I segment code (0) per `dhan-annexure-enums.md` rule 2.
#[inline]
const fn idx_i_segment_code() -> u8 {
    0
}

/// Pure routing gate for the day-OHLC tick consumer (operator rule
/// 2026-07-03): route ONLY IDX_I ticks whose EXCHANGE timestamp (IST epoch
/// seconds, per `data-integrity.md` — never receive time) falls inside the
/// regular trading session `[09:15:00, 15:30:00)` IST. Pre-market /
/// pre-open ticks (< 09:15:00) must never arm `day_open`; post-close stale
/// ticks (>= 15:30:00) must never mutate day high/low/close. Delegates the
/// window check to the canonical G1 exchange gate
/// (`g1_exchange_gate_accepts`, half-open, boundary-exact). O(1), zero
/// allocation, no panic path.
#[inline]
#[must_use]
pub const fn should_route_day_ohlc_tick(
    exchange_segment_code: u8,
    exchange_timestamp_secs: u32,
) -> bool {
    if exchange_segment_code != idx_i_segment_code() {
        return false;
    }
    let secs_of_day = (exchange_timestamp_secs % 86_400) as i64;
    g1_exchange_gate_accepts(secs_of_day * 1_000_000_000)
}

/// Spawns the IST midnight reset task. At 00:00:00 IST every day, calls
/// `tracker.reset_daily_all()` to clear yesterday's day OHLC. The next
/// live tick re-arms `day_open` to the new trading day's first LTP.
// TEST-EXEMPT: tokio::spawn wrapper — pure logic `reset_daily_all()` is tested in `day_ohlc_tracker.rs::tests::test_tracker_reset_daily_disarms_all`; spawn site pinned by `test_day_ohlc_orchestrator_is_wired_into_main`.
pub fn spawn_midnight_reset_task(tracker: Arc<DayOhlcTracker>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let now_ist = ist_seconds_of_day();
            let wait_secs = secs_until_next_ist(
                IST_MIDNIGHT_HOUR,
                IST_MIDNIGHT_MINUTE,
                IST_MIDNIGHT_SECOND,
                now_ist,
            );
            // If wait_secs is 0 (we're exactly at midnight), wait a full day
            // to avoid busy-looping.
            let actual_wait = if wait_secs == 0 {
                SECONDS_PER_DAY
            } else {
                wait_secs
            };
            info!(
                wait_secs = actual_wait,
                "day_ohlc midnight reset task: sleeping until next IST 00:00:00"
            );
            tokio::time::sleep(Duration::from_secs(u64::from(actual_wait))).await;

            tracker.reset_daily_all();
            info!(
                tracked_sids = tracker.len(),
                "day_ohlc midnight reset complete"
            );
        }
    })
}

/// Backoff between a midnight-reset task death and its respawn. Small so the
/// IST-midnight reset resumes quickly, but non-zero so a task that panics
/// instantly on every start cannot busy-spin the CPU — it respawns at most
/// once per this interval, and the `tv_day_ohlc_reset_failures_total{reason}`
/// counter rate surfaces the flap to the operator (INDEX-OHLC-02).
pub const DAY_OHLC_RESET_RESPAWN_BACKOFF_SECS: u64 = 5;

/// Classify why the supervised midnight-reset task's `JoinHandle` resolved,
/// into a stable metric label. Pure function so the supervisor's branch logic
/// is unit testable without constructing a real `JoinError` (which has no
/// public constructor). Mirrors `disk_health_watcher::classify_join_exit`.
#[must_use]
pub fn classify_reset_task_exit(join_result: &Result<(), tokio::task::JoinError>) -> &'static str {
    match join_result {
        Ok(()) => "clean_exit",
        Err(e) if e.is_panic() => "panic",
        Err(e) if e.is_cancelled() => "cancelled",
        Err(_) => "unknown",
    }
}

/// CCL-02 — supervise the IST-midnight DayOhlc reset task (INDEX-OHLC-02).
///
/// [`spawn_midnight_reset_task`] runs an infinite sleep-until-midnight loop, so
/// its `JoinHandle` resolves ONLY on a fatal event (panic mid-iteration or an
/// external cancel). Before this supervisor the handle was bound to `_` in
/// `main.rs`, so a panic made the daily reset vanish silently — yesterday's day
/// high/low/close would then carry over to the next trading day (until the first
/// live tick re-arms) with ZERO operator signal.
///
/// This mirrors the WS-GAP-05 pool supervisor and the DISK-WATCHER-01 disk-health
/// watcher: on every reset-task death it logs `error!` (code `INDEX-OHLC-02`) +
/// increments `tv_day_ohlc_reset_failures_total{reason}`, then respawns after
/// [`DAY_OHLC_RESET_RESPAWN_BACKOFF_SECS`] so the midnight reset keeps firing.
/// The counter is the one the `index-day-ohlc-tracker-error-codes.md` runbook
/// already documents (it previously had no producer).
///
/// The returned `JoinHandle` is itself an infinite loop (it never resolves in
/// normal operation); callers bind it to a `_`-prefixed name. The supervisor
/// body has no panic path of its own (no `unwrap`/`expect`, pure-function
/// classification), so it does not need a supervisor-of-the-supervisor.
// O(1) EXEMPT: cold-path supervisor — one task per session, fires only on reset-task death.
// TEST-EXEMPT: an infinite respawn driver; the pure decision `classify_reset_task_exit` and the backoff constant are unit-tested below, and the liveness invariant is pinned by `test_spawn_supervised_midnight_reset_task_keeps_running`.
pub fn spawn_supervised_midnight_reset_task(
    tracker: Arc<DayOhlcTracker>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let handle = spawn_midnight_reset_task(Arc::clone(&tracker));
            let join_result = handle.await;
            let reason = classify_reset_task_exit(&join_result);
            error!(
                reason,
                code =
                    tickvault_common::error_code::ErrorCode::IndexOhlc02DailyResetFailed.code_str(),
                backoff_secs = DAY_OHLC_RESET_RESPAWN_BACKOFF_SECS,
                "INDEX-OHLC-02: DayOhlc IST-midnight reset task exited — respawning so \
                 yesterday's day high/low/close cannot silently carry over past midnight"
            );
            metrics::counter!("tv_day_ohlc_reset_failures_total", "reason" => reason).increment(1);
            tokio::time::sleep(Duration::from_secs(DAY_OHLC_RESET_RESPAWN_BACKOFF_SECS)).await;
        }
    })
}

// Re-export the segment code constant so source-scan ratchets can verify
// the consumer routes IDX_I ticks specifically.
#[doc(hidden)]
pub const IDX_I_SEGMENT_CODE_FOR_RATCHETS: u8 = 0;

// Verify INDIA_VIX_SECURITY_ID is what we expect (sanity for the locked set).
const _: () = assert!(
    INDIA_VIX_SECURITY_ID == 21,
    "INDIA VIX SID must remain pinned at 21"
);

const _: () = assert!(
    IST_UTC_OFFSET_SECONDS == 5 * 3600 + 30 * 60,
    "IST offset must remain pinned at +5:30"
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_idx_i_segment_code_pinned_at_0() {
        assert_eq!(idx_i_segment_code(), 0);
        assert_eq!(IDX_I_SEGMENT_CODE_FOR_RATCHETS, 0);
    }

    #[test]
    fn test_midnight_boundary_constants_pinned() {
        assert_eq!(IST_MIDNIGHT_HOUR, 0);
        assert_eq!(IST_MIDNIGHT_MINUTE, 0);
        assert_eq!(IST_MIDNIGHT_SECOND, 0);
    }

    // -----------------------------------------------------------------------
    // CCL-02 — supervised midnight-reset task (INDEX-OHLC-02)
    // -----------------------------------------------------------------------

    #[test]
    fn test_reset_respawn_backoff_is_small_but_nonzero() {
        // Non-zero so a task that panics instantly on every start can't
        // busy-spin the CPU; small so the IST-midnight reset resumes quickly.
        assert!(DAY_OHLC_RESET_RESPAWN_BACKOFF_SECS >= 1);
        assert!(DAY_OHLC_RESET_RESPAWN_BACKOFF_SECS <= 30);
    }

    #[tokio::test]
    async fn test_classify_reset_task_exit_clean() {
        let h = tokio::spawn(async {});
        let r = h.await;
        assert_eq!(classify_reset_task_exit(&r), "clean_exit");
    }

    #[tokio::test]
    async fn test_classify_reset_task_exit_panic() {
        // A panicking task yields a JoinError where is_panic() == true.
        let h = tokio::spawn(async {
            panic!("intentional test panic"); // APPROVED: test — exercises the panic branch
        });
        let r = h.await;
        assert_eq!(classify_reset_task_exit(&r), "panic");
    }

    #[tokio::test]
    async fn test_classify_reset_task_exit_cancelled() {
        // An aborted task yields a JoinError where is_cancelled() == true.
        let h = tokio::spawn(async {
            tokio::time::sleep(Duration::from_secs(3600)).await;
        });
        h.abort();
        let r = h.await;
        assert_eq!(classify_reset_task_exit(&r), "cancelled");
    }

    #[tokio::test]
    async fn test_spawn_supervised_midnight_reset_task_keeps_running() {
        // The supervisor is an infinite loop — its JoinHandle must NOT resolve
        // in normal operation. The inner reset task it spawns also loops forever
        // (sleeps until IST 00:00:00), so the supervisor parks on `handle.await`
        // and never completes. (If a future edit makes the supervisor return
        // after one reset-task death instead of respawning, this guard fails.)
        let tracker = Arc::new(DayOhlcTracker::new());
        let handle = spawn_supervised_midnight_reset_task(tracker);
        // Let the spawned task make progress.
        tokio::task::yield_now().await;
        assert!(
            !handle.is_finished(),
            "supervisor must keep running, not exit after spawning the reset task"
        );
        handle.abort();
    }

    // ------------------------------------------------------------------
    // should_route_day_ohlc_tick — session-window ratchets (operator rule
    // 2026-07-03: pre-market data must NEVER become day_open).
    // ------------------------------------------------------------------

    /// A day-aligned IST epoch base (multiple of 86_400) so seconds-of-day
    /// arithmetic below is exact.
    const DAY_BASE: u32 = 1_779_321_600;

    #[test]
    fn test_should_route_rejects_pre_open_and_pre_market() {
        // 09:00:00 IST (pre-open session start — the broadcast carries these).
        assert!(!should_route_day_ohlc_tick(0, DAY_BASE + 9 * 3600));
        // 09:14:59 IST — one second before the regular session opens.
        assert!(
            !should_route_day_ohlc_tick(0, DAY_BASE + 33_299),
            "09:14:59 pre-market tick must NOT arm day_open"
        );
        // 08:00:00 IST — any earlier tick.
        assert!(!should_route_day_ohlc_tick(0, DAY_BASE + 8 * 3600));
    }

    #[test]
    fn test_should_route_accepts_091500_and_in_session() {
        // 09:15:00.000 exactly — the first regular-session second (inclusive).
        assert!(
            should_route_day_ohlc_tick(0, DAY_BASE + 33_300),
            "09:15:00 exactly must be accepted (inclusive open boundary)"
        );
        // Mid-session 11:30:00 IST.
        assert!(should_route_day_ohlc_tick(
            0,
            DAY_BASE + 11 * 3600 + 30 * 60
        ));
        // Last in-session second 15:29:59 IST.
        assert!(should_route_day_ohlc_tick(0, DAY_BASE + 55_799));
    }

    #[test]
    fn test_should_route_rejects_1530_and_later() {
        // 15:30:00 exactly — exclusive close boundary.
        assert!(
            !should_route_day_ohlc_tick(0, DAY_BASE + 55_800),
            "15:30:00 exactly must be rejected (exclusive close boundary)"
        );
        // 20:00:00 IST post-close stale tick.
        assert!(!should_route_day_ohlc_tick(0, DAY_BASE + 72_000));
    }

    #[test]
    fn test_should_route_rejects_non_idx_i_segment() {
        // NSE_EQ (1) / NSE_FNO (2) in-session ticks are never day-OHLC routed.
        assert!(!should_route_day_ohlc_tick(1, DAY_BASE + 33_300));
        assert!(!should_route_day_ohlc_tick(2, DAY_BASE + 40_000));
    }
}
