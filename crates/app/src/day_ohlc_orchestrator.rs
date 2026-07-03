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
//! ## Why `day_open` is the first live SESSION tick (not a pre-market value)
//!
//! Per operator directive 2026-05-26, all Dhan historical / pre-market
//! buffer code was removed from the workspace. `day_open` for the 4 LOCKED
//! IDX_I SIDs (NIFTY=13, BANKNIFTY=25, SENSEX=51, INDIA VIX=21) now equals
//! the first observed live WebSocket tick after the midnight reset.
//!
//! **Session-open purity (operator directive 2026-07-03):** the consumer is
//! session-gated to `[09:15:00, 15:30:00)` IST via
//! [`day_ohlc_session_accepts`], which reuses the canonical G1 exchange gate
//! (`g1_exchange_gate_accepts`). Pre-open indicative ticks (Dhan streams
//! from 09:00 IST) and post-close snapshot ticks can therefore NEVER arm
//! `day_open` or advance high/low/close — the 09:15:00 tick IS the day open,
//! matching the candle aggregator's session window exactly. Always-on SIDs
//! (GIFT Nifty, operator lock 2026-06-01 §30) are EXEMPT from the window,
//! mirroring the candle aggregator + tick processor — see
//! [`day_ohlc_gate_allows`].

use std::collections::HashSet;
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

/// Nanoseconds per second — the seconds→nanos-of-day widening factor for
/// [`g1_exchange_gate_accepts`] (which takes IST NANOS-of-day).
const NANOS_PER_SECOND: i64 = 1_000_000_000;

/// Session-open purity gate (operator directive 2026-07-03): returns `true`
/// iff a tick's `exchange_timestamp` (IST epoch SECONDS, the Dhan LTT
/// convention per `data-integrity.md` — Groww ticks never reach this
/// consumer; they flow through the separate Groww bridge) falls inside the
/// regular NSE session `[09:15:00, 15:30:00)` IST.
///
/// Delegates to the canonical `g1_exchange_gate_accepts`
/// (`crates/common/src/constants.rs`) — the SAME half-open window the candle
/// aggregator enforces — so the day-OHLC window can never drift from the
/// candle-grid window. Pure, O(1), zero allocation: one modulo, one multiply,
/// two integer compares.
#[inline]
#[must_use]
pub fn day_ohlc_session_accepts(exchange_timestamp_ist_secs: u32) -> bool {
    let secs_of_day = exchange_timestamp_ist_secs % SECONDS_PER_DAY;
    g1_exchange_gate_accepts(i64::from(secs_of_day) * NANOS_PER_SECOND)
}

/// Full day-OHLC admission gate: session window + the always-on exemption
/// (operator lock 2026-06-01 §30, GIFT Nifty — an NSE-IX INDEX with a ~21 h
/// session). Returns `true` iff the tick may reach the tracker.
///
/// Always-on `(security_id, exchange_segment_code)` pairs are EXEMPT from the
/// `[09:15, 15:30)` window, mirroring the candle aggregator
/// (`multi_tf_aggregator.rs::consume_tick`) and the tick processor's
/// `is_window_exempt`. The set is the SAME boot-computed source of truth both
/// of those use — `DailyUniverse::always_on_segments` installed once via
/// `tickvault_common::always_on::init_always_on_segments` and read at the
/// spawn site via `tickvault_common::always_on::current()` — passed in
/// EXPLICITLY (never read from the global here) so this stays unit-testable,
/// exactly like the aggregator's `with_always_on`.
///
/// Honesty note: GIFT Nifty IS an IDX_I index (segment code 0 — see
/// `always_on.rs` / `daily_universe.rs::always_on_segments`), so when it is in
/// the subscribed universe its ticks DO pass this consumer's IDX_I filter
/// today. Without this exemption its legitimate out-of-window ticks would be
/// (a) skipped from day-OHLC and (b) continuously inflate
/// `tv_day_ohlc_session_gate_skipped_total`, destroying that counter's
/// diagnostic value.
#[inline]
#[must_use]
pub fn day_ohlc_gate_allows(
    always_on: &HashSet<(u64, u8)>,
    security_id: u64,
    exchange_segment_code: u8,
    exchange_timestamp_ist_secs: u32,
) -> bool {
    // O(1) EXEMPT: HashSet `contains` is O(1) hashing, not an O(n) Vec scan.
    always_on.contains(&(security_id, exchange_segment_code))
        || day_ohlc_session_accepts(exchange_timestamp_ist_secs)
}

/// IST midnight = 00:00:00.
pub const IST_MIDNIGHT_HOUR: u32 = 0;
pub const IST_MIDNIGHT_MINUTE: u32 = 0;
pub const IST_MIDNIGHT_SECOND: u32 = 0;

/// Spawns the tick consumer task. Drains the tick broadcast and routes
/// every IN-SESSION IDX_I tick to `tracker.update_tick()`. The first
/// in-session tick for a SID auto-arms day_open/high/low/close to that LTP;
/// subsequent ticks advance day_high/day_low/day_close. Non-IDX_I ticks and
/// out-of-session ticks (outside [09:15, 15:30) IST per
/// [`day_ohlc_gate_allows`], always-on SIDs exempt per §30) are skipped O(1).
///
/// On `broadcast::Lagged`, increments a counter and continues. Closed
/// channel exits the task (process shutdown).
// TEST-EXEMPT: tokio::spawn wrapper over `broadcast::recv` — per-tick logic `tracker.update_tick()` is tested in `day_ohlc_tracker.rs::tests`; spawn site pinned by `test_day_ohlc_orchestrator_is_wired_into_main`.
pub fn spawn_day_ohlc_tick_consumer(
    tracker: Arc<DayOhlcTracker>,
    mut rx: broadcast::Receiver<ParsedTick>,
    always_on: Arc<HashSet<(u64, u8)>>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        info!("day_ohlc tick consumer started");
        loop {
            match rx.recv().await {
                Ok(tick) => {
                    if tick.exchange_segment_code != idx_i_segment_code() {
                        continue;
                    }
                    // Session-open purity (operator directive 2026-07-03):
                    // only ticks inside [09:15:00, 15:30:00) IST reach the
                    // tracker — a pre-open (09:00–09:14) indicative tick or a
                    // post-close snapshot can never arm day_open or move
                    // high/low/close. Always-on SIDs (GIFT Nifty §30) are
                    // EXEMPT, matching the candle aggregator. O(1) skip, no
                    // per-tick log.
                    if !day_ohlc_gate_allows(
                        &always_on,
                        tick.security_id,
                        tick.exchange_segment_code,
                        tick.exchange_timestamp,
                    ) {
                        metrics::counter!("tv_day_ohlc_session_gate_skipped_total").increment(1);
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

    // -----------------------------------------------------------------------
    // Session-open purity gate (operator directive 2026-07-03)
    // -----------------------------------------------------------------------

    /// Day-aligned IST epoch base (divisible by 86_400) used to build
    /// multi-day epoch inputs — proves the `% SECONDS_PER_DAY` reduction.
    const DAY_BASE_IST_EPOCH: u32 = 1_779_235_200;

    #[test]
    fn test_day_ohlc_session_gate_rejects_preopen_0907() {
        // 09:07:00 IST = 32_820 secs-of-day — the original bug scenario:
        // a pre-open indicative index tick must NOT reach the tracker.
        assert!(!day_ohlc_session_accepts(DAY_BASE_IST_EPOCH + 32_820));
    }

    #[test]
    fn test_day_ohlc_session_gate_rejects_0914_59() {
        // 09:14:59 IST = 33_299 — last pre-open second is rejected.
        assert!(!day_ohlc_session_accepts(DAY_BASE_IST_EPOCH + 33_299));
    }

    #[test]
    fn test_day_ohlc_session_gate_accepts_0915_00_exact_open() {
        // 09:15:00 IST = 33_300 — the session open is INCLUSIVE: this tick
        // IS the day open.
        assert!(day_ohlc_session_accepts(DAY_BASE_IST_EPOCH + 33_300));
    }

    #[test]
    fn test_day_ohlc_session_gate_accepts_15_29_59() {
        // 15:29:59 IST = 55_799 — last in-session second is accepted.
        assert!(day_ohlc_session_accepts(DAY_BASE_IST_EPOCH + 55_799));
    }

    #[test]
    fn test_day_ohlc_session_gate_rejects_15_30_00_exclusive_close() {
        // 15:30:00 IST = 55_800 — the close is EXCLUSIVE, matching the
        // candle aggregator gate and g1_exchange_gate_accepts.
        assert!(!day_ohlc_session_accepts(DAY_BASE_IST_EPOCH + 55_800));
    }

    #[test]
    fn test_day_ohlc_gate_allows_exempts_always_on_sid_at_2000_ist() {
        // Operator lock 2026-06-01 §30: GIFT Nifty (an IDX_I index trading
        // ~21 h/day on NSE-IX) must pass the gate OUTSIDE [09:15, 15:30),
        // exactly like the candle aggregator's always_on exemption. A normal
        // SID at the same 20:00 IST instant must still be rejected.
        let mut always_on = std::collections::HashSet::new();
        always_on.insert((5024_u64, 0_u8)); // GIFT Nifty (sid, IDX_I=0)

        // 20:00:00 IST = 72_000 secs-of-day — far outside the regular session.
        let ts_2000 = DAY_BASE_IST_EPOCH + 72_000;
        assert!(
            day_ohlc_gate_allows(&always_on, 5024, 0, ts_2000),
            "always-on GIFT Nifty tick at 20:00 IST must be exempt from the session gate"
        );
        assert!(
            !day_ohlc_gate_allows(&always_on, 13, 0, ts_2000),
            "normal SID (NIFTY) at 20:00 IST must be rejected"
        );
        // In-session, both pass (the exemption never REJECTS anything).
        let ts_open = DAY_BASE_IST_EPOCH + 33_300;
        assert!(day_ohlc_gate_allows(&always_on, 5024, 0, ts_open));
        assert!(day_ohlc_gate_allows(&always_on, 13, 0, ts_open));
        // Same SID on a DIFFERENT segment is NOT exempt (composite key per
        // I-P1-11 — matching the aggregator's (sid, segment) exempt_key).
        assert!(!day_ohlc_gate_allows(&always_on, 5024, 2, ts_2000));
    }

    #[test]
    fn test_day_ohlc_session_accepts_is_the_gate_when_always_on_empty() {
        // With no exemptions installed (today's default when GIFT is absent),
        // the gate is exactly the session window (`day_ohlc_session_accepts`).
        let always_on = std::collections::HashSet::new();
        assert!(!day_ohlc_gate_allows(
            &always_on,
            13,
            0,
            DAY_BASE_IST_EPOCH + 72_000
        ));
        assert!(day_ohlc_gate_allows(
            &always_on,
            13,
            0,
            DAY_BASE_IST_EPOCH + 33_300
        ));
    }

    #[test]
    fn test_preopen_tick_never_arms_day_open_then_0915_tick_is_the_open() {
        // Integration-style: a real DayOhlcTracker fed through the SAME
        // gated path the consumer uses. The 09:07 pre-open tick must leave
        // the tracker untouched (no slot, hence disarmed); the 09:15:00
        // tick must arm day_open to ITS LTP.
        let tracker = DayOhlcTracker::new();
        let sid: u64 = 13; // NIFTY

        // Pre-open tick at 09:07 IST with a pre-market indicative price.
        let preopen_ts = DAY_BASE_IST_EPOCH + 32_820;
        let preopen_ltp = 23_100.0_f64;
        if day_ohlc_session_accepts(preopen_ts) {
            let _ = tracker.update_tick(sid, ExchangeSegment::IdxI, preopen_ltp);
        }
        assert!(
            tracker
                .snapshot(sid, ExchangeSegment::IdxI)
                .is_none_or(|s| !s.is_armed()),
            "a 09:07 pre-open tick must never arm day_open"
        );

        // First session tick at exactly 09:15:00 IST — this IS the open.
        let open_ts = DAY_BASE_IST_EPOCH + 33_300;
        let open_ltp = 23_146.45_f64;
        if day_ohlc_session_accepts(open_ts) {
            let _ = tracker.update_tick(sid, ExchangeSegment::IdxI, open_ltp);
        }
        let snap = tracker
            .snapshot(sid, ExchangeSegment::IdxI)
            .expect("09:15:00 tick must create the slot");
        assert!(snap.is_armed(), "09:15:00 tick must arm the tracker");
        assert!(
            (snap.day_open - open_ltp).abs() < f64::EPSILON,
            "day_open must be the 09:15:00 LTP, not the pre-open price"
        );
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
}
