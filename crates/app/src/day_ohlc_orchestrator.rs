//! Day OHLC boot orchestration — PR #8a Slice 1.
//!
//! Wires `DayOhlcTracker` to its three lifecycle dependencies:
//!
//! 1. **09:15:00 IST arm task** — once per trading day at the exact NSE
//!    opening boundary, reads `PreOpenCloses::backtrack_latest()` for each
//!    of the 4 LOCKED IDX_I SIDs (NIFTY=13, BANKNIFTY=25, SENSEX=51,
//!    INDIA VIX=21) and calls `arm_sid(sid, IdxI, equilibrium_close)`.
//!    Empty buffer → `INDEX-OHLC-01` Critical (operator-locked per
//!    `.claude/rules/project/index-day-ohlc-tracker-error-codes.md`).
//!
//! 2. **Tick consumer task** — subscribes to the tick broadcast and routes
//!    every IDX_I tick to `tracker.update_tick(sid, IdxI, last_price)`
//!    so `day_high`, `day_low`, `day_close` advance per tick. Zero hot-path
//!    allocation; lock held in microseconds per `hot-path.md`.
//!
//! 3. **IST midnight reset task** — at 00:00:00 IST every day, calls
//!    `tracker.reset_daily_all()` to clear stale state so the next 09:15:00
//!    IST `arm_sid` cleanly re-initialises from the new pre-market buffer.
//!    Failure → `INDEX-OHLC-02` (Severity::High, auto-recoverable on next
//!    09:15:00 arm).
//!
//! ## Why this lives in `crates/app/`
//!
//! `DayOhlcTracker` lives in `crates/trading/src/in_mem/` and
//! `PreOpenCloses` lives in `crates/core/src/instrument/`. The `trading`
//! crate only has `tickvault-core` as a `dev-dependency` so it can't
//! consume `preopen_price_buffer` in its library code. The `app` crate
//! depends on both `core` and `trading` so it's the natural home for
//! the orchestrator that wires them together.
//!
//! ## Operator contract (per `index-day-ohlc-tracker-error-codes.md`)
//!
//! > "09:15:00 IST open price MUST = NSE equilibrium open — NOT the first
//! > post-open tick LTP."
//!
//! After Slice 1 lands, `day_open` for the 09:15 1m candle for the 4 IDX_I
//! SIDs matches Dhan / NSE / TradingView exactly. Cross-verify at 15:31
//! IST will match on the OPEN field instead of CROSS-VERIFY-01-failing.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::broadcast::{self, error::RecvError};
use tracing::{error, info, warn};

use tickvault_common::constants::{INDIA_VIX_SECURITY_ID, IST_UTC_OFFSET_SECONDS, SECONDS_PER_DAY};
use tickvault_common::error_code::ErrorCode;
use tickvault_common::tick_types::ParsedTick;
use tickvault_common::trading_calendar::TradingCalendar;
use tickvault_common::types::ExchangeSegment;
use tickvault_core::instrument::preopen_price_buffer::{
    PREOPEN_INDEX_UNDERLYINGS, SharedPreOpenBuffer,
};
use tickvault_trading::in_mem::day_ohlc_tracker::{
    ArmOutcome, DayOhlcTracker, evaluate_arm_outcome, ist_seconds_of_day, secs_until_next_ist,
};

/// Market-open boundary (09:15:00 IST = 33_300 seconds of day).
pub const MARKET_OPEN_HOUR_IST: u32 = 9;
pub const MARKET_OPEN_MINUTE_IST: u32 = 15;
pub const MARKET_OPEN_SECOND_IST: u32 = 0;

/// IST midnight = 00:00:00.
pub const IST_MIDNIGHT_HOUR: u32 = 0;
pub const IST_MIDNIGHT_MINUTE: u32 = 0;
pub const IST_MIDNIGHT_SECOND: u32 = 0;

/// Backoff sleep when the arm task wakes on a non-trading day. Prevents
/// busy-spin when `secs_until_next_ist` returns 0 because we're exactly
/// at the boundary. 60s is short enough that any operator-triggered
/// shutdown propagates promptly.
pub const NON_TRADING_DAY_BACKOFF_SECS: u32 = 60;

/// Spawns the 09:15:00 IST arm task. Loops forever:
/// - Sleep until next 09:15:00 IST
/// - If today is a trading day, iterate `PREOPEN_INDEX_UNDERLYINGS` and
///   arm each SID from `PreOpenCloses::backtrack_latest()`
/// - Empty buffer for a SID → `error!` with `code = INDEX-OHLC-01`,
///   Critical severity (routed to Telegram via Loki/Alertmanager)
/// - Loop again
///
/// # Idempotency
///
/// If the task is restarted mid-session (e.g. process restart after 09:15:00
/// IST), it sleeps until the next day's 09:15:00. To recover today's
/// `day_open` after a mid-session restart, see PR #8b which adds QuestDB
/// persistence via `day_ohlc_audit` table.
// TEST-EXEMPT: tokio::spawn wrapper — pure logic in `arm_all_locked_idx_i` has 4 unit tests; spawn site pinned by `test_day_ohlc_orchestrator_is_wired_into_main`.
pub fn spawn_market_open_arm_task(
    tracker: Arc<DayOhlcTracker>,
    preopen_buffer: SharedPreOpenBuffer,
    trading_calendar: Arc<TradingCalendar>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let now_ist = ist_seconds_of_day();
            let wait_secs = secs_until_next_ist(
                MARKET_OPEN_HOUR_IST,
                MARKET_OPEN_MINUTE_IST,
                MARKET_OPEN_SECOND_IST,
                now_ist,
            );
            info!(
                wait_secs,
                "day_ohlc arm task: sleeping until next 09:15:00 IST"
            );
            tokio::time::sleep(Duration::from_secs(u64::from(wait_secs))).await;

            // Audit-findings Rule 3: market-hours-aware. Skip non-trading days.
            if !trading_calendar.is_trading_day_today() {
                info!("day_ohlc arm task: not a trading day — skipping arm");
                // Avoid busy-spin if secs_until_next_ist returned 0.
                tokio::time::sleep(Duration::from_secs(u64::from(NON_TRADING_DAY_BACKOFF_SECS)))
                    .await;
                continue;
            }

            arm_all_locked_idx_i(&tracker, &preopen_buffer).await;
        }
    })
}

/// Pure-function side: iterate the 4 LOCKED IDX_I SIDs and arm each from
/// the pre-open buffer. Logs per-SID outcome via `tracing` (which routes
/// through the existing 5-sink fan-out: stdout / file / errors.jsonl /
/// Telegram via Loki).
///
/// Exposed `pub` so tests can call it directly without spawning the timer.
pub async fn arm_all_locked_idx_i(tracker: &DayOhlcTracker, preopen_buffer: &SharedPreOpenBuffer) {
    let buffer = preopen_buffer.read().await;
    let mut armed_count = 0_usize;
    let mut empty_count = 0_usize;

    for &(symbol, security_id) in PREOPEN_INDEX_UNDERLYINGS {
        let backtrack = buffer
            .get(symbol)
            .and_then(|closes| closes.backtrack_latest());
        match evaluate_arm_outcome(security_id, backtrack) {
            ArmOutcome::Armed {
                security_id,
                day_open,
            } => {
                tracker.arm_sid(security_id, ExchangeSegment::IdxI, day_open);
                armed_count += 1;
                info!(
                    security_id,
                    symbol,
                    day_open,
                    "day_ohlc armed from pre-open equilibrium close at 09:15:00 IST"
                );
            }
            ArmOutcome::EmptyBuffer { security_id } => {
                empty_count += 1;
                // INDEX-OHLC-01: Critical per operator lock — `day_open`
                // will fall back to first-trade LTP if any tick arrives,
                // causing CROSS-VERIFY-01 mismatch at 15:31 IST.
                error!(
                    code = ErrorCode::IndexOhlc01PreopenEmptyAt0915.code_str(),
                    security_id,
                    symbol,
                    "pre-open buffer empty at 09:15:00 IST — day_open will fall back to first-trade LTP (INDEX-OHLC-01)"
                );
            }
        }
    }

    info!(
        armed_count,
        empty_count,
        total = PREOPEN_INDEX_UNDERLYINGS.len(),
        "day_ohlc arm cycle complete"
    );
}

/// Spawns the tick consumer task. Drains the tick broadcast and routes
/// every IDX_I tick to `tracker.update_tick()` so day_high/day_low/day_close
/// advance. Non-IDX_I ticks are skipped O(1). Unarmed SIDs (pre-09:15:00
/// IST) result in a silent no-op per the tracker contract.
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
                    if tick.exchange_segment_code != idx_i_segment_code() {
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
/// 09:15:00 IST arm cycle re-initialises from the new pre-market buffer.
///
/// Failure mode: if `reset_daily_all` panics or otherwise fails, the next
/// arm cycle still overwrites `day_open` correctly (idempotent), so the
/// only visible effect is `day_high`/`day_low`/`day_close` carrying over
/// stale values for ~9 hours until the next arm. This is the
/// `INDEX-OHLC-02` condition (Severity::High, auto-recoverable).
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
    use tickvault_core::instrument::preopen_price_buffer::new_shared_preopen_buffer;

    #[tokio::test]
    async fn test_arm_all_locked_idx_i_with_full_buffer_arms_all_4_sids() {
        let tracker = Arc::new(DayOhlcTracker::new());
        let buffer = new_shared_preopen_buffer();
        // Seed the buffer with a non-empty close for each of the 4 SIDs.
        {
            let mut guard = buffer.write().await;
            use tickvault_core::instrument::preopen_price_buffer::PreOpenCloses;
            for &(symbol, _) in PREOPEN_INDEX_UNDERLYINGS {
                let mut closes = PreOpenCloses::default();
                closes.record(12, 100.0); // close at minute 12 = 09:12
                guard.insert(symbol.to_string(), closes);
            }
        }

        arm_all_locked_idx_i(&tracker, &buffer).await;

        // All 4 SIDs should be armed.
        assert_eq!(tracker.len(), 4);
        for &(_, sid) in PREOPEN_INDEX_UNDERLYINGS {
            let snap = tracker.snapshot(sid, ExchangeSegment::IdxI);
            assert!(snap.is_some(), "SID {sid} should be armed");
            let ohlc = snap.unwrap();
            assert!((ohlc.day_open - 100.0).abs() < f64::EPSILON);
        }
    }

    #[tokio::test]
    async fn test_arm_all_with_empty_buffer_arms_zero_sids() {
        let tracker = Arc::new(DayOhlcTracker::new());
        let buffer = new_shared_preopen_buffer();
        // Buffer empty — backtrack_latest returns None for all 4.

        arm_all_locked_idx_i(&tracker, &buffer).await;

        // No SIDs armed. The error! log fires for each (INDEX-OHLC-01).
        assert_eq!(tracker.len(), 0);
        for &(_, sid) in PREOPEN_INDEX_UNDERLYINGS {
            assert!(tracker.snapshot(sid, ExchangeSegment::IdxI).is_none());
        }
    }

    #[tokio::test]
    async fn test_arm_all_partial_buffer_arms_subset() {
        let tracker = Arc::new(DayOhlcTracker::new());
        let buffer = new_shared_preopen_buffer();
        // Seed buffer for NIFTY + BANKNIFTY only; SENSEX + VIX empty.
        {
            let mut guard = buffer.write().await;
            use tickvault_core::instrument::preopen_price_buffer::PreOpenCloses;
            for &(symbol, _) in PREOPEN_INDEX_UNDERLYINGS {
                if symbol == "NIFTY" || symbol == "BANKNIFTY" {
                    let mut closes = PreOpenCloses::default();
                    closes.record(12, 25_650.0);
                    guard.insert(symbol.to_string(), closes);
                }
            }
        }

        arm_all_locked_idx_i(&tracker, &buffer).await;

        // NIFTY (13) + BANKNIFTY (25) armed. SENSEX (51) + VIX (21) not.
        assert!(tracker.snapshot(13, ExchangeSegment::IdxI).is_some());
        assert!(tracker.snapshot(25, ExchangeSegment::IdxI).is_some());
        assert!(tracker.snapshot(51, ExchangeSegment::IdxI).is_none());
        assert!(tracker.snapshot(21, ExchangeSegment::IdxI).is_none());
    }

    #[tokio::test]
    async fn test_arm_all_backtracks_through_earlier_minutes() {
        let tracker = Arc::new(DayOhlcTracker::new());
        let buffer = new_shared_preopen_buffer();
        // Seed only slot 5 (09:05) for NIFTY — backtrack from 12 → 5 should find it.
        {
            let mut guard = buffer.write().await;
            use tickvault_core::instrument::preopen_price_buffer::PreOpenCloses;
            let mut closes = PreOpenCloses::default();
            closes.record(5, 23_456.78);
            guard.insert("NIFTY".to_string(), closes);
        }

        arm_all_locked_idx_i(&tracker, &buffer).await;

        let snap = tracker.snapshot(13, ExchangeSegment::IdxI);
        assert!(snap.is_some());
        assert!((snap.unwrap().day_open - 23_456.78).abs() < f64::EPSILON);
    }

    #[test]
    fn test_idx_i_segment_code_pinned_at_0() {
        assert_eq!(idx_i_segment_code(), 0);
        assert_eq!(IDX_I_SEGMENT_CODE_FOR_RATCHETS, 0);
    }

    #[test]
    fn test_market_open_boundary_constants_pinned() {
        assert_eq!(MARKET_OPEN_HOUR_IST, 9);
        assert_eq!(MARKET_OPEN_MINUTE_IST, 15);
        assert_eq!(MARKET_OPEN_SECOND_IST, 0);
        assert_eq!(IST_MIDNIGHT_HOUR, 0);
        assert_eq!(IST_MIDNIGHT_MINUTE, 0);
        assert_eq!(IST_MIDNIGHT_SECOND, 0);
    }
}
