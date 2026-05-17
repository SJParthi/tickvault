//! Phase 0 Item 8+9 — Gap-fill scheduler receive loop (PR-C, 2026-05-17).
//!
//! This module owns the consumer side of the disconnect-event broadcast
//! channel produced by [`crate::websocket::connection`]. On each
//! [`crate::websocket::disconnect_event::DisconnectResolvedEvent`]:
//!
//! 1. Log the event at INFO with structured fields (connection index,
//!    outage window in IST seconds, planned bar count).
//! 2. Call the pure planner
//!    [`crate::historical::gap_fill_planner::plan_gap_fill_bars`] to
//!    compute which 1m bars would need refilling.
//! 3. Increment Prometheus counters so the operator's "is gap-fill
//!    seeing events?" dashboard panel has signal.
//!
//! ## What PR-C does NOT yet do
//!
//! - Fetch via Dhan REST `/v2/charts/intraday` — PR-D ships the
//!   `fetch_intraday_with_retry` pub wrapper + per-bar `tokio::time::sleep_until`
//!   + DH-904 backoff ladder + `historical_candles` UPSERT.
//! - Audit row writes — PR-D wires `gap_fill_audit_persistence::append_gap_fill_audit_row`
//!   (already shipped in PR-A) per bar attempt.
//! - Telegram event emission — PR-D wires the `GapFillCompleted` /
//!   `Partial` / `Failed` variants shipped in PR-A.
//!
//! ## Why this is NOT a skeleton (per `audit-findings-2026-04-17.md` Rule 14)
//!
//! PR-B's aborted skeleton attempt shipped pub fns with `// TODO PR-C`
//! stubs and a heartbeat counter on an inert work loop. PR-C ships a
//! REAL work loop: the planner is called, planned-bar counts are
//! emitted to Prometheus, and the operator can see real signal from
//! day one. Every pub fn has a real call site (boot wiring in main.rs).
//! No false-OK heartbeat, no `enabled` config flag, no inert pub
//! surface.
//!
//! ## Shutdown
//!
//! Uses the codebase convention `Arc<Notify>` per
//! `audit-findings-2026-04-17.md` Rule 16. The boot path passes
//! `Arc::clone(&shutdown_notify)` (the same handle shared with the
//! pool watchdog). Per Rule 16, the task MUST be parked on
//! `.notified()` before `notify_waiters()` fires — `tokio::select!`
//! parks on every iteration so this is satisfied by construction.
//!
//! ## Z+ layers shipped in PR-C
//!
//! | Layer | Mechanism |
//! |---|---|
//! | L1 DETECT | `broadcast::Receiver::recv()` |
//! | L2 VERIFY | `Lagged(n)` arm fires Critical with `code = ErrorCode::GapFill04EventChannelLagged.code_str()` |
//! | L5 AUDIT | `tv_gap_fill_events_received_total` + `tv_gap_fill_planned_bars_total` |
//! | L6 RECOVER | Per Rule 11, Lagged is NEVER silent — operator is paged |

use std::sync::Arc;

use tokio::sync::Notify;
use tokio::sync::broadcast;
use tracing::{error, info};

use tickvault_common::constants::{SECONDS_PER_DAY, TICK_PERSIST_END_SECS_OF_DAY_IST};
use tickvault_common::error_code::ErrorCode;

use crate::historical::gap_fill_planner::plan_gap_fill_bars;
use crate::websocket::disconnect_event::DisconnectResolvedEvent;

/// Run the gap-fill scheduler receive loop until shutdown.
///
/// Subscribes (the caller passes the `Receiver` directly so the
/// broadcast channel's `Sender` half stays in the connection layer).
/// Processes events sequentially — PR-D adds per-bar concurrent fetch
/// tasks once the REST path lands.
///
/// # Call site
///
/// Boot wiring in `crates/app/src/main.rs` — after the WS pool spawns,
/// the pool's broadcast `Sender` is passed to each `WebSocketConnection`
/// via `with_disconnect_event_sender(tx)`, and the matching `Receiver`
/// is passed to this function via `tokio::spawn`.
///
/// # Shutdown
///
/// Returns when `shutdown_notify.notify_waiters()` fires AND the task
/// is parked on `shutdown.notified()` (per Rule 16). Both conditions
/// hold because the `tokio::select!` arm parks on every iteration.
pub async fn run_gap_fill_scheduler(
    mut disconnect_rx: broadcast::Receiver<DisconnectResolvedEvent>,
    shutdown_notify: Arc<Notify>,
) {
    info!(
        channel_capacity = crate::websocket::disconnect_event::DISCONNECT_EVENT_CHANNEL_CAPACITY,
        "gap-fill scheduler started (PR-C: log + plan + Prom counters; PR-D adds REST fetch + UPSERT)"
    );

    let events_received = metrics::counter!("tv_gap_fill_events_received_total");
    let events_lagged = metrics::counter!("tv_gap_fill_event_channel_lagged_total");
    let planned_bars = metrics::counter!("tv_gap_fill_planned_bars_total");

    loop {
        tokio::select! {
            recv_result = disconnect_rx.recv() => {
                match recv_result {
                    Ok(event) => {
                        events_received.increment(1);
                        let bars = compute_planned_bars(&event);
                        planned_bars.increment(bars.len() as u64);
                        info!(
                            connection_index = event.connection_index,
                            outage_start_secs = event.outage_start_secs,
                            outage_end_secs = event.outage_end_secs,
                            outage_duration_secs =
                                event.outage_end_secs.saturating_sub(event.outage_start_secs),
                            planned_bar_count = bars.len(),
                            "gap-fill scheduler received disconnect event (PR-C: planned; PR-D will fetch)"
                        );
                    }
                    Err(broadcast::error::RecvError::Lagged(dropped)) => {
                        events_lagged.increment(dropped);
                        // Per audit-findings Rule 11 (no false-OK signals):
                        // Lagged is NEVER silent. PR-D will add a
                        // reconciliation pass; PR-C surfaces the loss
                        // to the operator so they can manually inspect.
                        error!(
                            code = ErrorCode::GapFill04EventChannelLagged.code_str(),
                            dropped_event_count = dropped,
                            "GAP-FILL-04: disconnect event broadcast lagged; events dropped — reconciliation pass deferred to PR-D"
                        );
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        info!("gap-fill scheduler: disconnect event channel closed; exiting");
                        return;
                    }
                }
            }
            () = shutdown_notify.notified() => {
                info!("gap-fill scheduler: shutdown signalled; exiting");
                return;
            }
        }
    }
}

/// Compute the planned bar timestamps for a disconnect-resolved event.
///
/// Pure function — wraps [`plan_gap_fill_bars`] with the IST-derived
/// market-close cutoff for the trading day the event resolved on.
///
/// Returns `Vec<i64>` of IST epoch SECONDS, one per 1m bar that PR-D
/// will fetch + UPSERT. Empty if:
/// - Outage entirely sub-minute (no full minute fits inside).
/// - Outage entirely post-15:30 IST (market close).
/// - Outage spans midnight (planner clamps to today's market close).
///
/// O(1) EXEMPT: cold path — runs once per WS reconnect cycle (~5/day).
#[must_use]
pub fn compute_planned_bars(event: &DisconnectResolvedEvent) -> Vec<i64> {
    // Derive today's market-close as IST epoch seconds: the trading-day
    // anchor is the IST midnight that immediately precedes
    // `outage_end_secs`, plus the 15:30 secs-of-day constant.
    //
    // O(1) EXEMPT: arithmetic, no allocation outside the planner's Vec.
    let day_anchor = day_anchor_ist_secs(event.outage_end_secs);
    let market_close_secs = day_anchor.saturating_add(i64::from(TICK_PERSIST_END_SECS_OF_DAY_IST));
    plan_gap_fill_bars(
        event.outage_start_secs,
        event.outage_end_secs,
        market_close_secs,
    )
}

/// IST midnight (00:00:00) of the trading day containing `ist_secs`.
///
/// Pure function. Used to anchor today's `market_close_secs` for the
/// planner so a disconnect resolved at 15:32 IST clamps to today's
/// 15:30 IST not yesterday's.
#[must_use]
fn day_anchor_ist_secs(ist_secs: i64) -> i64 {
    let secs_in_day = i64::from(SECONDS_PER_DAY);
    ist_secs.saturating_sub(ist_secs.rem_euclid(secs_in_day))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// Returns an "IST epoch seconds" value per Dhan convention
    /// (`data-integrity.md` "WebSocket Timestamp Rule"): the value is
    /// the raw unix timestamp of `year-month-day 00:00:00` interpreted
    /// directly AS IST midnight (no +5:30 offset added). Under this
    /// convention IST midnight = exact multiple of 86400 since epoch,
    /// which is what `day_anchor_ist_secs` relies on via `rem_euclid`.
    fn ist_midnight_secs(year: i32, month: u32, day: u32) -> i64 {
        use chrono::{NaiveDate, NaiveDateTime, NaiveTime};
        let naive = NaiveDateTime::new(
            NaiveDate::from_ymd_opt(year, month, day).unwrap(),
            NaiveTime::from_hms_opt(0, 0, 0).unwrap(),
        );
        naive.and_utc().timestamp()
    }

    #[test]
    fn test_day_anchor_at_midnight_returns_same() {
        let midnight = ist_midnight_secs(2026, 5, 17);
        assert_eq!(day_anchor_ist_secs(midnight), midnight);
    }

    #[test]
    fn test_day_anchor_at_noon_returns_midnight() {
        let midnight = ist_midnight_secs(2026, 5, 17);
        let noon = midnight + 12 * 3600;
        assert_eq!(day_anchor_ist_secs(noon), midnight);
    }

    #[test]
    fn test_compute_planned_bars_three_minute_outage_midday() {
        // 11:00:00 → 11:03:00 IST → 3 full 1m bars expected.
        let midnight = ist_midnight_secs(2026, 5, 17);
        let event = DisconnectResolvedEvent {
            connection_index: 0,
            outage_start_secs: midnight + 11 * 3600,
            outage_end_secs: midnight + 11 * 3600 + 180,
        };
        let bars = compute_planned_bars(&event);
        assert_eq!(bars.len(), 3, "3 minute outage = 3 planned bars");
    }

    #[test]
    fn test_compute_planned_bars_outage_after_market_close_returns_empty() {
        // 16:00:00 → 16:05:00 IST — entirely post-15:30 close.
        let midnight = ist_midnight_secs(2026, 5, 17);
        let event = DisconnectResolvedEvent {
            connection_index: 1,
            outage_start_secs: midnight + 16 * 3600,
            outage_end_secs: midnight + 16 * 3600 + 300,
        };
        let bars = compute_planned_bars(&event);
        assert!(
            bars.is_empty(),
            "outage entirely post-close must plan zero bars; got {bars:?}"
        );
    }

    #[test]
    fn test_compute_planned_bars_outage_spans_market_close_includes_only_pre_close() {
        // 15:29:00 → 15:31:00 IST — 15:29 bar eligible, 15:30 excluded.
        let midnight = ist_midnight_secs(2026, 5, 17);
        let event = DisconnectResolvedEvent {
            connection_index: 2,
            outage_start_secs: midnight + 15 * 3600 + 29 * 60,
            outage_end_secs: midnight + 15 * 3600 + 31 * 60,
        };
        let bars = compute_planned_bars(&event);
        assert_eq!(bars.len(), 1, "exactly the 15:29 bar must be planned");
    }

    #[tokio::test]
    async fn test_scheduler_returns_on_shutdown_signal() {
        let (_tx, rx) = crate::websocket::disconnect_event::create_disconnect_event_channel();
        let shutdown = Arc::new(Notify::new());
        let task_shutdown = Arc::clone(&shutdown);
        let handle = tokio::spawn(async move {
            run_gap_fill_scheduler(rx, task_shutdown).await;
        });
        // Yield once so the task reaches its `select!` and parks on
        // `.notified()` BEFORE we fire (per Rule 16).
        tokio::time::sleep(Duration::from_millis(50)).await;
        shutdown.notify_waiters();
        // Should exit within 2s.
        let outcome = tokio::time::timeout(Duration::from_secs(2), handle).await;
        assert!(outcome.is_ok(), "scheduler must exit within 2s of shutdown");
    }

    #[tokio::test]
    async fn test_scheduler_processes_event_then_returns_on_shutdown() {
        let (tx, rx) = crate::websocket::disconnect_event::create_disconnect_event_channel();
        let shutdown = Arc::new(Notify::new());
        let task_shutdown = Arc::clone(&shutdown);
        let handle = tokio::spawn(async move {
            run_gap_fill_scheduler(rx, task_shutdown).await;
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        // Send a real event mid-flight; scheduler must drain it then
        // honour shutdown.
        let midnight = ist_midnight_secs(2026, 5, 17);
        tx.send(DisconnectResolvedEvent {
            connection_index: 0,
            outage_start_secs: midnight + 11 * 3600,
            outage_end_secs: midnight + 11 * 3600 + 120,
        })
        .expect("send must succeed");
        tokio::time::sleep(Duration::from_millis(100)).await;
        shutdown.notify_waiters();
        let outcome = tokio::time::timeout(Duration::from_secs(2), handle).await;
        assert!(outcome.is_ok());
    }

    #[tokio::test]
    async fn test_scheduler_exits_on_channel_close() {
        let (tx, rx) = crate::websocket::disconnect_event::create_disconnect_event_channel();
        let shutdown = Arc::new(Notify::new());
        let task_shutdown = Arc::clone(&shutdown);
        let handle = tokio::spawn(async move {
            run_gap_fill_scheduler(rx, task_shutdown).await;
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        // Drop the Sender; scheduler must exit on `Err(Closed)`.
        drop(tx);
        let outcome = tokio::time::timeout(Duration::from_secs(2), handle).await;
        assert!(
            outcome.is_ok(),
            "scheduler must exit within 2s of channel close"
        );
    }
}
