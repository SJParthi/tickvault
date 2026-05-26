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
//! ## Why `day_open` is the first live tick (not a pre-market value)
//!
//! Per operator directive 2026-05-26, all Dhan historical / pre-market
//! buffer code was removed from the workspace. `day_open` for the 4 LOCKED
//! IDX_I SIDs (NIFTY=13, BANKNIFTY=25, SENSEX=51, INDIA VIX=21) now equals
//! the first observed live WebSocket tick after the midnight reset.
//!
//! For trading-day correctness this is the LTP of the first tick that
//! arrives after 09:15:00 IST. For non-trading windows it is whatever LTP
//! Dhan happens to stream first (typically the pre-open snapshot tick).

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::broadcast::{self, error::RecvError};
use tracing::{info, warn};

use tickvault_common::constants::{INDIA_VIX_SECURITY_ID, IST_UTC_OFFSET_SECONDS, SECONDS_PER_DAY};
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
}
