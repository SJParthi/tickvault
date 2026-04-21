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
    // Depth-20
    assert!(
        src.contains("DepthTwentyConnected"),
        "events.rs must declare DepthTwentyConnected"
    );
    assert!(
        src.contains("DepthTwentyDisconnected"),
        "events.rs must declare DepthTwentyDisconnected"
    );
    assert!(
        src.contains("DepthTwentyReconnected"),
        "events.rs must declare DepthTwentyReconnected (added 2026-04-21)"
    );
    // Depth-200
    assert!(
        src.contains("DepthTwoHundredConnected"),
        "events.rs must declare DepthTwoHundredConnected"
    );
    assert!(
        src.contains("DepthTwoHundredDisconnected"),
        "events.rs must declare DepthTwoHundredDisconnected"
    );
    assert!(
        src.contains("DepthTwoHundredReconnected"),
        "events.rs must declare DepthTwoHundredReconnected (added 2026-04-21)"
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
        src.contains("Self::DepthTwentyReconnected"),
        "events.rs to_message() must handle DepthTwentyReconnected — otherwise Telegram fires an empty message."
    );
    assert!(
        src.contains("Self::DepthTwoHundredReconnected"),
        "events.rs to_message() must handle DepthTwoHundredReconnected."
    );
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

#[test]
fn depth_20_connection_fires_reconnect_notify() {
    let src = read("crates/core/src/websocket/depth_connection.rs");
    assert!(
        src.contains("DepthTwentyReconnected"),
        "depth_connection.rs must emit DepthTwentyReconnected inside \
         run_twenty_depth_connection on every successful reconnect."
    );
    assert!(
        src.contains("failures_before_attempt > 0"),
        "depth_connection.rs must guard the reconnect emission on \
         failures_before_attempt > 0 so fresh-boot does NOT fire."
    );
}

#[test]
fn depth_200_connection_fires_reconnect_notify() {
    let src = read("crates/core/src/websocket/depth_connection.rs");
    assert!(
        src.contains("DepthTwoHundredReconnected"),
        "depth_connection.rs must emit DepthTwoHundredReconnected inside \
         run_two_hundred_depth_connection on every successful reconnect."
    );
}

#[test]
fn order_update_connection_fires_reconnect_notify() {
    let src = read("crates/core/src/websocket/order_update_connection.rs");
    assert!(
        src.contains("OrderUpdateReconnected"),
        "order_update_connection.rs must emit OrderUpdateReconnected \
         inside connect_and_listen right after successful connect + login."
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
// (3) Main.rs wires reconnect_notifier into all 3 connection functions
// ---------------------------------------------------------------------------

#[test]
fn main_rs_passes_reconnect_notifier_to_all_three_connection_functions() {
    let src = read("crates/app/src/main.rs");
    assert!(
        src.contains("d20_reconnect_notifier"),
        "main.rs must construct + pass d20_reconnect_notifier into \
         run_twenty_depth_connection."
    );
    assert!(
        src.contains("d200_reconnect_notifier"),
        "main.rs must construct + pass d200_reconnect_notifier into \
         run_two_hundred_depth_connection."
    );
    // Order-update has two call sites (fast + slow boot paths); both
    // must pass a reconnect notifier.
    assert!(
        src.matches("reconnect_notifier").count() >= 3,
        "main.rs must pass a reconnect notifier into every call site of the \
         3 depth / order-update connection functions (depth-20, depth-200, \
         order-update fast boot, order-update slow boot)."
    );
}

// ---------------------------------------------------------------------------
// (4) Fast first-retry latency on all 4 WS types
// ---------------------------------------------------------------------------

#[test]
fn depth_first_retry_is_at_most_500ms() {
    let src = read("crates/core/src/websocket/depth_connection.rs");
    assert!(
        src.contains("const DEPTH_RECONNECT_INITIAL_MS: u64 = 500;"),
        "DEPTH_RECONNECT_INITIAL_MS must be exactly 500ms for uniform fast \
         first-retry parity with the main feed. Regression reinstates the \
         slower 1000ms first-retry latency."
    );
}

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
// (8) No-Live-Ticks-During-Market-Hours watchdog wiring (Parthiban
// directive 2026-04-21). This watchdog catches the "WS connected but
// Dhan stopped streaming" silent failure that triggered today's
// investigation. Ratcheted at four points:
//   (a) module exists,
//   (b) heartbeat is updated in the tick processor hot path,
//   (c) main.rs spawns the watchdog in BOTH boot paths,
//   (d) the alert event variant is declared CRITICAL severity.
// ---------------------------------------------------------------------------

#[test]
fn no_tick_watchdog_module_exists() {
    let src = read("crates/core/src/pipeline/no_tick_watchdog.rs");
    assert!(
        src.contains("pub fn spawn_no_tick_watchdog"),
        "no_tick_watchdog module must expose spawn_no_tick_watchdog. \
         Without it, the silent-data-loss failure mode that hit \
         2026-04-21 morning has no detector."
    );
    assert!(
        src.contains("NO_TICK_THRESHOLD_SECS"),
        "no_tick_watchdog must expose a tuneable threshold constant."
    );
    assert!(
        src.contains("is_within_market_hours_ist()"),
        "no_tick_watchdog must use the shared market-hours helper \
         (Rule 3) — never poll alerts post-market."
    );
    assert!(
        src.contains("currently_alerting"),
        "no_tick_watchdog must be edge-triggered (Rule 4) — \
         track currently_alerting state to suppress repeat fires."
    );
}

#[test]
fn tick_processor_updates_heartbeat_on_every_tick() {
    let src = read("crates/core/src/pipeline/tick_processor.rs");
    assert!(
        src.contains("tick_heartbeat: Option<std::sync::Arc<std::sync::atomic::AtomicI64>>"),
        "tick_processor must accept a tick_heartbeat parameter so the \
         no-tick watchdog can read the latest tick timestamp."
    );
    // Heartbeat update must appear in BOTH Tick and TickWithDepth branches.
    let store_count = src.matches("hb.store(").count();
    assert!(
        store_count >= 2,
        "tick_processor must update tick_heartbeat in BOTH ParsedFrame::Tick \
         AND ParsedFrame::TickWithDepth branches (got {store_count} hb.store calls). \
         Missing one branch = blind spot during depth-only tick periods."
    );
}

#[test]
fn main_rs_spawns_watchdog_in_both_boot_paths() {
    let src = read("crates/app/src/main.rs");
    assert!(
        src.contains("fast_tick_heartbeat"),
        "main.rs FAST BOOT must construct fast_tick_heartbeat."
    );
    assert!(
        src.contains("slow_tick_heartbeat"),
        "main.rs SLOW BOOT must construct slow_tick_heartbeat."
    );
    let spawn_count = src.matches("spawn_no_tick_watchdog(").count();
    assert!(
        spawn_count >= 2,
        "main.rs must spawn the no-tick watchdog from BOTH fast boot AND \
         slow boot paths (got {spawn_count} spawn calls). Production runs \
         take exactly ONE path; missing the spawn in either = blind spot."
    );
}

#[test]
fn no_tick_alert_event_is_critical_severity() {
    let src = read("crates/core/src/notification/events.rs");
    assert!(
        src.contains("NoLiveTicksDuringMarketHours"),
        "events.rs must declare NoLiveTicksDuringMarketHours variant."
    );
    assert!(
        src.contains("Self::NoLiveTicksDuringMarketHours { .. } => Severity::Critical"),
        "NoLiveTicksDuringMarketHours must be Critical severity (the \
         operator MUST be paged immediately — silent data loss during \
         market hours is the worst-case failure for a trading system)."
    );
}
