//! Ratchet tests: full Telegram + audit visibility on every WS event,
//! across all 4 WebSocket types (main feed / depth-20 / depth-200 /
//! order-update).
//!
//! # Why this exists (Parthiban directive 2026-04-21)
//!
//! Every disconnect, reconnect, and connect event on every WebSocket
//! MUST fire a Telegram notification, REGARDLESS of market hours.
//! Full audit trail. The 2026-04-21 morning Telegram noise was caused
//! by a SEPARATE false-positive: the order-update watchdog firing
//! every 11 minutes on a healthy idle socket because Dhan's
//! order-update server does not ping on the same cadence as the
//! market feed. That is fixed by raising the watchdog threshold to
//! 1800s — NOT by suppressing alerts off-hours.
//!
//! This file enforces:
//!
//! 1. Every `NotificationEvent` variant exists for every WS event type
//!    on every WS transport (connect / disconnect / reconnect).
//! 2. Every emission site calls `notifier.notify(...)`.
//! 3. First-retry latency is ≤ 500ms on every WS type (fast recovery).
//! 4. The order-update watchdog threshold is ≥ 1800s (no false
//!    positives on legitimate idle windows).
//! 5. Subscription replay on reconnect is preserved (main feed
//!    `cached_subscription_messages.iter()` in `connect_and_subscribe`).

#![allow(clippy::assertions_on_constants)]

use std::fs;
use std::path::PathBuf;

fn repo_path(rel: &str) -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop();
    p.pop();
    p.push(rel);
    p
}

fn read(rel: &str) -> String {
    let path = repo_path(rel);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// Returns the PRODUCTION region of a source file — everything before the
/// first `#[cfg(test)]` marker.
///
/// 2026-07-06 hostile-review medium A: ratchets that pin production call
/// sites MUST scan this half only. The in-file test module calls the same
/// pure helpers (`should_page_outage(...)` etc.), so a whole-file substring
/// count stayed green even after the sole production call site was deleted
/// — a vacuous ratchet.
fn production_region(src: &str) -> &str {
    src.split("#[cfg(test)]").next().unwrap_or(src)
}

// ---------------------------------------------------------------------------
// (1) Every event variant exists in events.rs
// ---------------------------------------------------------------------------

#[test]
fn events_rs_declares_all_ws_event_variants() {
    let src = read("crates/core/src/notification/events.rs");
    // Main feed
    assert!(
        src.contains("WebSocketConnected"),
        "events.rs must declare WebSocketConnected"
    );
    assert!(
        src.contains("WebSocketDisconnected"),
        "events.rs must declare WebSocketDisconnected"
    );
    assert!(
        src.contains("WebSocketReconnected"),
        "events.rs must declare WebSocketReconnected"
    );
    // Order-update
    assert!(
        src.contains("OrderUpdateConnected"),
        "events.rs must declare OrderUpdateConnected"
    );
    assert!(
        src.contains("OrderUpdateDisconnected"),
        "events.rs must declare OrderUpdateDisconnected"
    );
    assert!(
        src.contains("OrderUpdateReconnected"),
        "events.rs must declare OrderUpdateReconnected (added 2026-04-21)"
    );
}

#[test]
fn events_rs_formats_all_reconnect_variants_in_to_message() {
    let src = read("crates/core/src/notification/events.rs");
    assert!(
        src.contains("Self::OrderUpdateReconnected"),
        "events.rs to_message() must handle OrderUpdateReconnected."
    );
}

// ---------------------------------------------------------------------------
// (2) Every emission site wires notifier.notify(...)
// ---------------------------------------------------------------------------

#[test]
fn main_feed_wires_notify_on_disconnect_and_reconnect() {
    let src = read("crates/core/src/websocket/connection.rs");
    // At least one notifier call emitting WebSocketDisconnected.
    assert!(
        src.matches("WebSocketDisconnected").count() >= 3,
        "connection.rs must emit WebSocketDisconnected at every disconnect site \
         (non-reconnectable / 807 token / generic-read / connect-failed)."
    );
    assert!(
        src.contains("WebSocketReconnected"),
        "connection.rs must emit WebSocketReconnected on successful reconnect."
    );
    // No in_hours gating should re-appear; main feed MUST emit unconditionally.
    assert!(
        !src.contains("if is_within_market_hours_ist()") && !src.contains("if in_hours {"),
        "connection.rs must NOT gate disconnect/reconnect Telegram by market \
         hours — Parthiban directive 2026-04-21 requires full audit trail on \
         every WS event regardless of hours."
    );
}

// REMOVED 2026-06-02 (CI rot fix): depth_20 / depth_200 reconnect-notify
// source-scan ratchets. The depth WebSocket module
// (`crates/core/src/websocket/depth_connection.rs`) was DELETED in
// AWS-lifecycle PR #4 — only the main-feed + order-update WebSockets remain
// per `.claude/rules/project/websocket-connection-scope-lock.md`. Both tests
// read a file that no longer exists. The main-feed + order-update
// reconnect-notify ratchets are retained.

#[test]
fn order_update_connection_fires_reconnect_notify() {
    let src = read("crates/core/src/websocket/order_update_connection.rs");
    assert!(
        src.contains("OrderUpdateReconnected"),
        "order_update_connection.rs must emit OrderUpdateReconnected \
         from the 60s stability arm inside connect_and_listen."
    );
    assert!(
        src.contains("failures_before_attempt"),
        "order_update_connection.rs must pass failures_before_attempt into \
         connect_and_listen so it can distinguish fresh-connect from recovery."
    );
    // Ensure the market-hours gating is NOT reintroduced on the generic
    // error / threshold-hit logs.
    assert!(
        !src.contains("if is_within_market_hours_ist()"),
        "order_update_connection.rs must NOT gate alerts by market hours — \
         Parthiban directive 2026-04-21."
    );
}

// ---------------------------------------------------------------------------
// (2b) 2026-07-06 incident ratchets — reachable HIGH page + honest recovery.
// Root causes verified live (14:05:49 IST, dead token, Dhan TCP-RST ~10ms
// after login, 39+ in-market failures, zero HIGH pages):
//   RC1: the only OrderUpdateDisconnected emit site was dead code behind a
//        never-returning function (WS-GAP-04 removed the legacy `return`).
//   RC2: OrderUpdateReconnected fired on the CONNECT edge, 10ms before Dhan
//        killed each socket — false "[LOW] reconnected x8" storms.
// ---------------------------------------------------------------------------

#[test]
fn order_update_high_page_is_emitted_inside_reconnect_loop() {
    let src = read("crates/core/src/websocket/order_update_connection.rs");
    // 2026-07-06 hostile-review medium A: every pin below is scoped to the
    // PRODUCTION region — the old whole-file `matches(...).count() >= 2`
    // stayed green with the sole production call site deleted, because the
    // in-file test module calls the same pure fn ~15 times.
    let prod = production_region(&src);
    assert!(
        prod.contains("NotificationEvent::OrderUpdateDisconnected"),
        "PRODUCTION code (the non-test region of order_update_connection.rs) \
         must emit the [HIGH] OrderUpdateDisconnected page from INSIDE the \
         reconnect loop (via emit_in_market_outage_page) — the main.rs \
         task-exit emit was dead code (2026-07-06 incident)."
    );
    assert_eq!(
        prod.matches("fn should_page_outage(").count(),
        1,
        "should_page_outage must be defined exactly once in production code."
    );
    let page_decision_calls = prod
        .matches("should_page_outage(within_hours, consecutive_failures, outage_paged)")
        .count();
    assert_eq!(
        page_decision_calls, 2,
        "the edge-triggered, market-hours-aware page decision (audit-findings \
         Rules 3+4) must be evaluated with the REAL loop state \
         (within_hours, consecutive_failures, outage_paged) at BOTH \
         production arms — transport-error AND sub-stability clean close. \
         Got {page_decision_calls} production call sites."
    );
    assert_eq!(
        prod.matches("outage_paged = true;").count(),
        2,
        "each production page arm must set the once-per-episode latch \
         (`outage_paged = true;`) immediately before emitting — removing \
         either latch reintroduces per-failure page spam (Rule 4)."
    );
    assert!(
        prod.contains("WsGap10OrderUpdateOutage"),
        "the in-loop outage error! must carry code = WS-GAP-10 so the \
         tag-guard + triage chain route it."
    );
}

#[test]
fn order_update_clean_close_counts_as_failure_and_can_page() {
    // 2026-07-06 hostile-review fix (same day): Dhan's documented
    // auth-rejection delivery is a clean Close frame (Ok(()) exit), not only
    // a TCP reset. The original streak_after_clean_close reset the streak to
    // 0 on an un-paged clean close, making the 3-failure HIGH page
    // UNREACHABLE for a pure clean-close dead-token regime and letting a
    // mixed regime (<=2 errors then one clean close, repeating) perpetually
    // defeat the threshold.
    let src = read("crates/core/src/websocket/order_update_connection.rs");
    // 2026-07-06 hostile-review medium A: scope to the production region so
    // test-module calls can never satisfy these pins vacuously.
    let prod = production_region(&src);
    assert!(
        prod.contains("fn streak_after_clean_close(prev_streak: u32, stability_reached: bool)"),
        "streak_after_clean_close must key on STABILITY SURVIVAL, not the \
         paged latch — a sub-60s clean close is a failure regardless of \
         delivery mode."
    );
    assert_eq!(
        prod.matches("if should_page_outage(within_hours, consecutive_failures, outage_paged)")
            .count(),
        2,
        "the [HIGH] page decision must be evaluated in BOTH production \
         reconnect-loop arms — the transport-error arm AND the \
         sub-stability clean-close (Ok) arm — else a pure clean-close \
         outage grows the streak but never pages."
    );
    let emit_defs = prod.matches("fn emit_in_market_outage_page(").count();
    assert_eq!(
        emit_defs, 1,
        "emit_in_market_outage_page must be defined exactly once in \
         production code."
    );
    let emit_calls = prod.matches("emit_in_market_outage_page(").count() - emit_defs;
    assert_eq!(
        emit_calls, 2,
        "both production arms must route through the shared \
         emit_in_market_outage_page helper (2 call sites — transport-error \
         + clean-close) so the Telegram wording and the WS-GAP-10 coded \
         error! can never diverge between regimes. Got {emit_calls} \
         production call sites."
    );
}

#[test]
fn order_update_reconnected_is_stability_gated_not_connect_edge() {
    let src = read("crates/core/src/websocket/order_update_connection.rs");
    let full_token = "NotificationEvent::OrderUpdateReconnected";
    assert_eq!(
        src.matches(full_token).count(),
        1,
        "exactly ONE OrderUpdateReconnected emission must exist — the \
         connect-edge emission was DELETED 2026-07-06 (it fired 10ms before \
         Dhan killed each dead-token socket → false-recovery storms)."
    );
    let pin_pos = src
        .find("tokio::pin!(stability_sleep)")
        .expect("the 60s stability sleep must be pinned before the read loop");
    let emit_pos = src
        .find(full_token)
        .expect("checked non-zero above — count == 1");
    assert!(
        emit_pos > pin_pos,
        "the OrderUpdateReconnected emission must live AFTER the stability \
         sleep pin (i.e. inside the 60s-survival select arm), never at the \
         connect edge."
    );
    assert!(
        src.contains("ORDER_UPDATE_RECONNECT_STABILITY_SECS: u64 = 60"),
        "the stability window must stay pinned at 60s — far beyond the \
         observed ~10ms die-after-login window, far below the 14400s \
         activity watchdog."
    );
    // 2026-07-06 hostile-review medium B pins:
    assert!(
        src.contains(", if !*stability_reached =>"),
        "the stability select arm must keep the `if !*stability_reached` \
         re-poll guard — a completed tokio Sleep must never be re-polled \
         (would panic); the guard is load-bearing."
    );
    assert!(
        src.contains("time::sleep(stability_window())"),
        "the production stability sleep must be built from the pure \
         stability_window() source so the paused-time boundary tests \
         (59s not stable / 60s stable) exercise the exact window \
         production uses."
    );
}

#[test]
fn main_rs_does_not_emit_order_update_disconnected_after_task_await() {
    let src = read("crates/app/src/main.rs");
    assert_eq!(
        src.matches("NotificationEvent::OrderUpdateDisconnected")
            .count(),
        0,
        "main.rs must NOT emit OrderUpdateDisconnected — the post-await site \
         was unreachable (run_order_update_connection never returns since \
         WS-GAP-04); the reachable page lives inside the reconnect loop."
    );
    assert!(
        src.contains("task_exited_unreachable"),
        "main.rs must keep the defensive coded error! at the order-update \
         spawn site so a future refactor that breaks the never-return loop \
         contract surfaces loudly."
    );
}

// ---------------------------------------------------------------------------
// (3) Main.rs wires reconnect_notifier into the surviving connection functions
//     (main-feed + order-update — the only 2 WS types per the scope lock).
// ---------------------------------------------------------------------------

#[test]
fn main_rs_passes_reconnect_notifier_to_order_update_call_sites() {
    // Updated 2026-06-02 (CI rot fix): depth-20 / depth-200 were deleted in
    // AWS-lifecycle PR #4, so the former "all 3 connection functions" is now
    // the order-update connection (fast + slow boot call sites). Order-update
    // has two call sites; both must pass a reconnect notifier.
    let src = read("crates/app/src/main.rs");
    assert!(
        src.matches("reconnect_notifier").count() >= 2,
        "main.rs must pass a reconnect notifier into every order-update \
         connection call site (fast boot + slow boot)."
    );
}

// ---------------------------------------------------------------------------
// (4) Fast first-retry latency on all 4 WS types
// ---------------------------------------------------------------------------

// REMOVED 2026-06-02 (CI rot fix): depth_first_retry_is_at_most_500ms — the
// depth WebSocket module was deleted in AWS-lifecycle PR #4. The main-feed +
// order-update first-retry latency ratchets are retained.

#[test]
fn order_update_first_retry_is_at_most_500ms() {
    use tickvault_common::constants::ORDER_UPDATE_RECONNECT_INITIAL_DELAY_MS;
    assert!(
        ORDER_UPDATE_RECONNECT_INITIAL_DELAY_MS <= 500,
        "ORDER_UPDATE_RECONNECT_INITIAL_DELAY_MS must be ≤ 500ms for fast \
         first-retry parity with main feed + depth. Currently {ORDER_UPDATE_RECONNECT_INITIAL_DELAY_MS}ms."
    );
}

// ---------------------------------------------------------------------------
// (5) Order-update watchdog threshold ≥ 1800s (the real false-positive fix)
// ---------------------------------------------------------------------------

#[test]
fn order_update_watchdog_threshold_is_at_least_1800_secs() {
    use tickvault_core::websocket::activity_watchdog::WATCHDOG_THRESHOLD_ORDER_UPDATE_SECS;
    assert!(
        WATCHDOG_THRESHOLD_ORDER_UPDATE_SECS >= 1800,
        "WATCHDOG_THRESHOLD_ORDER_UPDATE_SECS must be ≥ 1800s. Production \
         evidence (2026-04-21): the previous 660s bound fired every 11 min \
         on idle dry-run accounts because Dhan's order-update server does \
         not ping on the market-feed cadence. TCP RST still catches real \
         dead sockets via Some(Err(..)). Current value: {WATCHDOG_THRESHOLD_ORDER_UPDATE_SECS}s."
    );
}

// ---------------------------------------------------------------------------
// (6) Subscription replay invariant on main feed reconnect
// ---------------------------------------------------------------------------

#[test]
fn main_feed_replays_cached_subscription_on_every_reconnect() {
    let src = read("crates/core/src/websocket/connection.rs");
    assert!(
        src.contains("self.cached_subscription_messages.iter()"),
        "connection.rs connect_and_subscribe must iterate \
         cached_subscription_messages. Without this, a reconnect comes up \
         subscribed to ZERO instruments and tick stream stays silent — \
         silent data loss on every Dhan pre-market TCP reset."
    );
    assert!(
        src.contains("self.connect_and_subscribe().await"),
        "connection.rs reconnect loop must call connect_and_subscribe \
         (not a subscribe-less connect variant)."
    );
}

// ---------------------------------------------------------------------------
// (7) Main feed responds to Dhan's ping frame (protocol adherence)
// ---------------------------------------------------------------------------

#[test]
fn main_feed_responds_to_dhan_ping_with_bounded_pong() {
    let src = read("crates/core/src/websocket/connection.rs");
    assert!(
        src.contains("Message::Ping"),
        "connection.rs must explicitly handle Dhan server Ping frames \
         (Dhan pings every 10s per live-market-feed rule §16)."
    );
    assert!(
        src.contains("Message::Pong(data)"),
        "connection.rs must reply to every Ping with a Pong carrying the \
         same payload (RFC 6455 ping/pong semantics). Dhan closes idle \
         sockets after 40s of no pong."
    );
    assert!(
        src.contains("pong_timeout") || src.contains("pong_timeout_secs"),
        "connection.rs must bound the Pong send by a timeout so a stuck \
         TCP buffer is detected as a dead socket and triggers reconnect."
    );
}

// ---------------------------------------------------------------------------
// (8) RETIRED 2026-07-14 (operator Dhan noise lock —
// dhan-rest-only-noise-lock-2026-07-14.md): the no-tick watchdog + its
// NoLiveTicksDuringMarketHours Critical page were DELETED. The watchdog's
// heartbeat was fed ONLY by the Dhan tick pipeline (retired 2026-07-13);
// Groww stall detection is FEED-STALL-01 + the market-hours-liveness alarm.
// The four wiring ratchets that lived here died with the feature.
// ---------------------------------------------------------------------------
