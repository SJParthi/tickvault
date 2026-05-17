//! Phase 0 Item 8+9 — Disconnect event broadcast type (PR-C, 2026-05-17).
//!
//! Typed payload from the WebSocket connection layer to the gap-fill
//! subsystem. One event per successful reconnect cycle. Carries the
//! outage window so [`crate::historical::gap_fill_planner::plan_gap_fill_bars`]
//! can compute which 1m bars to refill.
//!
//! ## Why a typed event (not just a `String`)
//!
//! The scheduler's planner takes `(outage_start_secs, outage_end_secs,
//! market_close_secs)` — three IST epoch seconds. Encoding them in a
//! struct here means the connection layer's `Sender::send()` call site
//! is type-checked at compile time; no string parsing in the hot
//! reconnect path.
//!
//! ## Why a separate module
//!
//! Producer (`connection.rs`) and consumer (`historical/gap_fill_scheduler.rs`)
//! live in different sibling modules. Defining the type in either side
//! creates a cross-module dependency that grows organically into a
//! cycle. A dedicated module owns the contract.
//!
//! ## Architectural lock
//!
//! Per `audit-findings-2026-04-17.md` Rule 15 (locked decisions table
//! in `.claude/plans/active-plan-item-8-9-gap-fill.md`):
//! - `broadcast::channel(64)` — 5 conns × 2s flap × 25s drain budget.
//!   Lagged fires `GapFill04EventChannelLagged` Critical.
//! - `Arc<Notify>` shutdown convention — NOT `CancellationToken`.

use tokio::sync::broadcast;

/// Capacity of the disconnect-event broadcast channel.
///
/// 64 is intentional: even under a 5-conn pool flap-storm (2.5
/// events/sec sustained), 25s of receiver downtime is absorbed
/// before Lagged fires. Beyond that, the scheduler's Lagged arm
/// emits `GapFill04EventChannelLagged` Critical + invokes a
/// catchup reconciliation pass (PR-D).
pub const DISCONNECT_EVENT_CHANNEL_CAPACITY: usize = 64;

/// One event per successful WebSocket reconnect cycle.
///
/// Cloned cheaply across broadcast subscribers (Copy, 24 bytes).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DisconnectResolvedEvent {
    /// Pool index of the connection that resolved (0..=4 for main feed).
    pub connection_index: u8,
    /// IST epoch seconds — best-estimate disconnect start. Currently
    /// the post-reconnect site fills this with `reconnect_end_secs -
    /// estimated_outage_secs` where `estimated_outage_secs` is computed
    /// from the most recent backoff_total_secs. PR-D refines using
    /// last-seen-tick timestamps.
    pub outage_start_secs: i64,
    /// IST epoch seconds — when the reconnect + subscribe completed.
    pub outage_end_secs: i64,
}

/// Construct the disconnect-event broadcast channel.
///
/// Used by `crates/app/src/main.rs` at boot. The `Sender` half is
/// cloned into each `WebSocketConnection` via
/// `WebSocketConnection::with_disconnect_event_sender()`. The
/// `Receiver` half is owned by
/// `crate::historical::gap_fill_scheduler::run_gap_fill_scheduler`.
#[must_use]
pub fn create_disconnect_event_channel() -> (
    broadcast::Sender<DisconnectResolvedEvent>,
    broadcast::Receiver<DisconnectResolvedEvent>,
) {
    broadcast::channel(DISCONNECT_EVENT_CHANNEL_CAPACITY)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_event_is_copy_clone_debug_eq() {
        let event = DisconnectResolvedEvent {
            connection_index: 2,
            outage_start_secs: 1_700_000_000,
            outage_end_secs: 1_700_000_120,
        };
        let cloned = event;
        assert_eq!(event, cloned);
        assert!(format!("{event:?}").contains("DisconnectResolvedEvent"));
    }

    #[test]
    fn test_channel_capacity_constant_is_64() {
        assert_eq!(DISCONNECT_EVENT_CHANNEL_CAPACITY, 64);
    }

    #[tokio::test]
    async fn test_send_recv_roundtrip() {
        let (tx, mut rx) = create_disconnect_event_channel();
        let event = DisconnectResolvedEvent {
            connection_index: 0,
            outage_start_secs: 100,
            outage_end_secs: 220,
        };
        tx.send(event)
            .expect("send must succeed with a live subscriber");
        assert_eq!(rx.recv().await.unwrap(), event);
    }
}
