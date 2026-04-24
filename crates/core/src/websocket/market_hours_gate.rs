//! Off-hours WebSocket connect gate.
//!
//! **Directive:** Parthiban, 2026-04-24 — "inside market hours it should
//! never ever happen right dude". Pre-market Dhan's WebSocket servers
//! (`api-feed.dhan.co`, `api-order-update.dhan.co`, etc.) TCP-RST every
//! idle connection. Our reconnect loop then re-dials, Dhan RSTs again,
//! the cycle repeats for ~1.5 hours until 09:00 IST. This burns CPU,
//! bandwidth, and Telegram alerts for zero data, AND risks bleeding the
//! reconnect storm into market open where tick loss is unacceptable.
//!
//! The fix is to not connect at all until market open. This module
//! provides the async gate that every WebSocket entry-point must call
//! before opening its first TCP socket.
//!
//! # When this fires
//! - App boots at 06:00 IST → `defer_until_market_open_ist` sleeps ~3h
//!   until 09:00 IST, then returns. Main feed + order update sockets
//!   open at 09:00 with zero flap.
//! - App boots at 10:00 IST (inside market hours) → helper returns
//!   immediately. No behaviour change from the pre-2026-04-24 code.
//! - App boots at 16:00 IST (post-market) → helper sleeps until the
//!   next day's 09:00 IST. Same rationale — Dhan resets idle sockets
//!   post-market too.
//!
//! # Scope — does NOT replace existing reconnect throttling
//! This gate applies ONCE, at task start, before the first connect.
//! Once a connection is up and then drops mid-session, the existing
//! reconnect loops (in `connection.rs`, `order_update_connection.rs`,
//! `depth_connection.rs`) own the reconnect cadence. Their existing
//! 3-consecutive-failure off-hours give-up logic still fires.

use std::time::Duration;

use tracing::info;

use tickvault_common::market_hours::{is_within_market_hours_ist, secs_until_next_market_open_ist};

/// Metric name emitted when a WebSocket task defers its boot connect.
/// Label `ws_kind` carries one of `main_feed`, `order_update`, `depth_20`,
/// `depth_200` so Prometheus can attribute the defer to a specific
/// subsystem.
pub const WS_BOOT_DEFER_METRIC: &str = "tv_websocket_boot_connect_deferred_total";

/// Metric name emitted when the boot defer completes and the task is
/// released to open its first TCP socket.
pub const WS_BOOT_DEFER_RELEASED_METRIC: &str = "tv_websocket_boot_connect_defer_released_total";

/// Sleeps until the next 09:00 IST market open if the current wall clock
/// is outside `[09:00, 15:30)` IST. Returns immediately when inside
/// market hours or when the test override is set.
///
/// # Arguments
/// * `ws_kind` — short tag for logs + metric label (e.g. `"main_feed"`,
///   `"order_update"`). Must be `'static` so the tracing call site can
///   use it without allocation on the hot path.
/// * `task_label` — additional context for the log (e.g. connection
///   index, contract name). Can be empty.
///
/// # Behaviour
/// - In market hours → returns immediately, emits no log, no metric.
/// - Off hours → logs `info!`, increments the defer counter, sleeps the
///   computed duration, increments the release counter, returns.
///
/// # Cancellation safety
/// The wait is a single `tokio::time::sleep`, which is cancel-safe.
/// Callers that want to cancel the defer (e.g. on graceful shutdown)
/// should race this future against their shutdown signal via
/// `tokio::select!`.
// TEST-EXEMPT: covered by defer_returns_immediately_during_market_hours below + two source-scanning regression guards
pub async fn defer_until_market_open_ist(ws_kind: &'static str, task_label: &str) {
    if is_within_market_hours_ist() {
        return;
    }

    let secs = secs_until_next_market_open_ist();
    // secs == 0 would mean "in market hours", which the early return above
    // already handled. Defensive saturating addition so a bad clock never
    // produces a zero-length sleep that silently disables the gate.
    let sleep_secs = secs.max(1);

    info!(
        ws_kind,
        task_label,
        sleep_secs,
        "WebSocket boot connect deferred — outside market hours [09:00, 15:30) IST. \
         Pre/post-market Dhan TCP-RSTs every idle socket, so we wait until the \
         next 09:00 IST before opening any TCP connection."
    );
    metrics::counter!(WS_BOOT_DEFER_METRIC, "ws_kind" => ws_kind).increment(1);

    tokio::time::sleep(Duration::from_secs(sleep_secs)).await;

    info!(
        ws_kind,
        task_label,
        slept_secs = sleep_secs,
        "WebSocket boot connect gate released — market hours entered, proceeding to connect"
    );
    metrics::counter!(WS_BOOT_DEFER_RELEASED_METRIC, "ws_kind" => ws_kind).increment(1);
}

#[cfg(test)]
mod tests {
    use super::*;
    use tickvault_common::market_hours::set_test_force_in_market_hours;

    /// When forced in-market, the helper must return immediately — verified
    /// by bounding the elapsed wall-clock time to well under one second.
    #[tokio::test]
    async fn defer_returns_immediately_during_market_hours() {
        set_test_force_in_market_hours(true);
        let start = std::time::Instant::now();
        defer_until_market_open_ist("test_kind", "test_label").await;
        let elapsed = start.elapsed();
        set_test_force_in_market_hours(false);
        assert!(
            elapsed < Duration::from_millis(200),
            "defer_until_market_open_ist must return immediately in market hours, \
             took {:?}",
            elapsed
        );
    }

    /// The metric name constants must stay stable — Prometheus dashboards
    /// and alert rules reference them by literal string.
    #[test]
    fn metric_names_are_stable() {
        assert_eq!(
            WS_BOOT_DEFER_METRIC, "tv_websocket_boot_connect_deferred_total",
            "metric name MUST NOT change — Grafana dashboards reference it by literal"
        );
        assert_eq!(
            WS_BOOT_DEFER_RELEASED_METRIC, "tv_websocket_boot_connect_defer_released_total",
            "metric name MUST NOT change — Grafana dashboards reference it by literal"
        );
    }

    /// Regression guard: `connection_pool::spawn_all` MUST call
    /// `defer_until_market_open_ist("main_feed", ...)` inside the per-task
    /// `tokio::spawn` closure. If anyone removes the gate, this test fails
    /// at `cargo test` time — before the flap ever reaches prod again.
    #[test]
    fn regression_main_feed_pool_wires_the_defer_gate() {
        let src = include_str!("../../src/websocket/connection_pool.rs");
        assert!(
            src.contains("market_hours_gate::defer_until_market_open_ist"),
            "connection_pool.rs MUST call \
             crate::websocket::market_hours_gate::defer_until_market_open_ist \
             before conn.run().await. Off-hours connect flap regression guard."
        );
        assert!(
            src.contains("\"main_feed\""),
            "connection_pool.rs MUST tag the defer call with ws_kind=\"main_feed\" \
             so Prometheus can attribute the defer to the main feed pool."
        );
    }

    /// Regression guard: `order_update_connection::run_order_update_connection`
    /// MUST call the defer gate at task entry.
    #[test]
    fn regression_order_update_wires_the_defer_gate() {
        let src = include_str!("../../src/websocket/order_update_connection.rs");
        assert!(
            src.contains("market_hours_gate::defer_until_market_open_ist"),
            "order_update_connection.rs MUST call \
             crate::websocket::market_hours_gate::defer_until_market_open_ist \
             at the top of run_order_update_connection. Off-hours connect flap \
             regression guard."
        );
        assert!(
            src.contains("\"order_update\""),
            "order_update_connection.rs MUST tag the defer call with \
             ws_kind=\"order_update\"."
        );
    }
}
