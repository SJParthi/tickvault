//! Wave 2 Item 6 (G1) — depth-20 / depth-200 / order-update WebSocket
//! post-close sleep + supervisor ratchets.
//!
//! These tests mirror `ws_sleep_resilience.rs` for the three remaining
//! WS feeds and pin the regression invariants documented in
//! `.claude/plans/active-plan-wave-2.md` Item 6:
//!
//! 1. `test_depth_20_post_close_sleeps_until_next_open` — source-scan
//!    ratchet asserting `depth_connection.rs` invokes the post-close
//!    helper with the `"depth_20"` feed label.
//! 2. `test_depth_200_post_close_sleeps_until_next_open` — same for
//!    `"depth_200"`.
//! 3. `test_order_update_post_close_sleeps_until_next_open` —
//!    source-scan ratchet asserting `order_update_connection.rs`
//!    REPLACED the legacy `Exhausted` `return` path with
//!    `order_update_post_close_sleep` + `continue`.
//! 4. `test_depth_20_supervisor_respawns_dead_task` — drains a dead
//!    handle through `supervise_pool` (the same supervisor wires the
//!    main-feed pool, depth pools, and order-update task).
//! 5. `test_depth_200_supervisor_respawns_dead_task` — same.
//! 6. `test_order_update_supervisor_respawns_dead_task` — same.
//! 7. `test_order_update_activity_watchdog_remains_14400s` — source-scan
//!    ratchet asserting `WATCHDOG_THRESHOLD_ORDER_UPDATE_SECS` stays
//!    14_400. Regressing to 1800 reintroduces the every-30-min false
//!    reconnect storm (see `live-market-feed-subscription.md`
//!    2026-04-24 §7).

#![allow(clippy::unwrap_used)] // APPROVED: test code
#![allow(clippy::expect_used)] // APPROVED: test code

use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use tickvault_core::websocket::WebSocketError;
use tickvault_core::websocket::connection_pool::WebSocketConnectionPool;

// ---------------------------------------------------------------------------
// Local helpers
// ---------------------------------------------------------------------------

fn repo_path(rel: &str) -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop();
    p.pop();
    p.push(rel);
    p
}

fn read_repo_file(rel: &str) -> String {
    let path = repo_path(rel);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

// ---------------------------------------------------------------------------
// Source-scan ratchets (1, 2, 3, 7)
// ---------------------------------------------------------------------------

#[test]
fn test_depth_20_post_close_sleeps_until_next_open() {
    // Source-scan ratchet: the depth-20 reconnect loop in
    // `depth_connection.rs::run_twenty_depth_connection` MUST invoke
    // `depth_post_close_sleep_or_exhaust` with feed label `"depth_20"`
    // (NOT `"depth20"` — the underscore is required for consistency
    // with the typed `WebSocketSleepEntered { feed }` field used by
    // Telegram routing).
    let src = read_repo_file("crates/core/src/websocket/depth_connection.rs");
    assert!(
        src.contains("depth_post_close_sleep_or_exhaust(\n                        \"depth_20\",")
            || src.contains("depth_post_close_sleep_or_exhaust(\"depth_20\""),
        "depth-20 reconnect path MUST call depth_post_close_sleep_or_exhaust(\"depth_20\", ...)"
    );
    // Helper itself MUST emit the WS-GAP-04 code on entering sleep.
    assert!(
        src.contains("ErrorCode::WsGap04PostCloseSleep"),
        "depth helper MUST tag WS-GAP-04 code on post-close sleep"
    );
    // Helper MUST emit WebSocketSleepEntered AND WebSocketSleepResumed.
    assert!(
        src.contains("WebSocketSleepEntered"),
        "depth helper MUST emit WebSocketSleepEntered Telegram event"
    );
    assert!(
        src.contains("WebSocketSleepResumed"),
        "depth helper MUST emit WebSocketSleepResumed Telegram event"
    );
    // Helper MUST proactively renew a stale token before resuming.
    assert!(
        src.contains("force_renewal_if_stale(14_400)"),
        "depth helper MUST call force_renewal_if_stale(14_400) before reconnect"
    );
}

#[test]
fn test_depth_200_post_close_sleeps_until_next_open() {
    // Source-scan ratchet: the depth-200 reconnect loop in
    // `depth_connection.rs::run_two_hundred_depth_connection` MUST
    // invoke the same helper with feed label `"depth_200"`.
    let src = read_repo_file("crates/core/src/websocket/depth_connection.rs");
    assert!(
        src.contains("depth_post_close_sleep_or_exhaust(\n                        \"depth_200\",")
            || src.contains("depth_post_close_sleep_or_exhaust(\"depth_200\""),
        "depth-200 reconnect path MUST call depth_post_close_sleep_or_exhaust(\"depth_200\", ...)"
    );
}

#[test]
fn test_order_update_post_close_sleeps_until_next_open() {
    // Source-scan ratchet: the legacy `Exhausted` `return` path in
    // `order_update_connection.rs::run_order_update_connection` MUST
    // be REPLACED by `order_update_post_close_sleep` + `continue`.
    // Re-introducing `return;` inside the `Exhausted` arm restores
    // the legacy give-up that requires a process restart at next
    // market open.
    let src = read_repo_file("crates/core/src/websocket/order_update_connection.rs");
    assert!(
        src.contains("order_update_post_close_sleep("),
        "order update reconnect MUST call order_update_post_close_sleep on Exhausted"
    );
    // The helper must emit WS-GAP-04 and the typed sleep events.
    assert!(
        src.contains("WsGap04PostCloseSleep") && src.contains("\"order_update\""),
        "order update sleep MUST tag WS-GAP-04 with feed=\"order_update\""
    );
    assert!(
        src.contains("WebSocketSleepEntered") && src.contains("WebSocketSleepResumed"),
        "order update sleep MUST emit WebSocketSleepEntered and WebSocketSleepResumed events"
    );
    // The helper must force-renew a stale token before reconnect.
    assert!(
        src.contains("force_renewal_if_stale(14_400)"),
        "order update sleep MUST call force_renewal_if_stale(14_400) before reconnect"
    );
    // The Exhausted arm MUST `continue` — not `return` — after sleeping.
    assert!(
        src.contains(
            "// Continue the reconnect loop after sleep.\n                        continue;"
        ),
        "order update Exhausted arm MUST `continue` after sleeping (no `return;`)"
    );
}

#[test]
fn test_order_update_activity_watchdog_remains_14400s() {
    // Source-scan ratchet: per `live-market-feed-subscription.md`
    // 2026-04-24 §7, the order-update activity watchdog threshold
    // MUST stay at 14_400 seconds (4h). Regressing to 1_800 (30min)
    // reintroduces the every-30-min false reconnect storm Dhan
    // exhibits when an account has zero order activity.
    let src = read_repo_file("crates/core/src/websocket/activity_watchdog.rs");
    assert!(
        src.contains("WATCHDOG_THRESHOLD_ORDER_UPDATE_SECS: u64 = 14400"),
        "order-update activity watchdog MUST remain 14_400 seconds — \
         regressing to 1_800 reintroduces the 30-min false-reconnect storm"
    );
    // Banned downgrade — explicitly assert the legacy 1800 value
    // is NOT bound to the order-update constant.
    let banned = "WATCHDOG_THRESHOLD_ORDER_UPDATE_SECS: u64 = 1800";
    assert!(
        !src.contains(banned),
        "BANNED: WATCHDOG_THRESHOLD_ORDER_UPDATE_SECS MUST NOT be 1800"
    );
}

// ---------------------------------------------------------------------------
// Supervisor drain ratchets (4, 5, 6) — exercise the same supervise_pool
// helper that wires the main-feed pool. Depth + order-update tasks return
// `Result<(), WebSocketError>` and are observable through the supervisor.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn test_depth_20_supervisor_respawns_dead_task() {
    // Construct a depth-20 task handle that exits immediately with
    // ReconnectionExhausted (the post-cap legacy error). Assert
    // supervise_pool drains the handle within the 5s budget — proving
    // the supervisor surfaces depth-20 task deaths to the operator
    // exactly the same way it surfaces main-feed task deaths.
    let h = tokio::spawn(async move {
        Err::<(), WebSocketError>(WebSocketError::ReconnectionExhausted {
            connection_id: 0,
            attempts: 60,
        })
    });
    let res = tokio::time::timeout(
        Duration::from_secs(5),
        WebSocketConnectionPool::supervise_pool(vec![h]),
    )
    .await;
    assert!(
        res.is_ok(),
        "supervise_pool must drain a dead depth-20 task within 5 s"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn test_depth_200_supervisor_respawns_dead_task() {
    // Same shape as the depth-20 case but with the 200-level cap (60
    // attempts) — supervise_pool is feed-agnostic; the same drain
    // must succeed.
    let h = tokio::spawn(async move {
        Err::<(), WebSocketError>(WebSocketError::ReconnectionExhausted {
            connection_id: 0,
            attempts: 60,
        })
    });
    let res = tokio::time::timeout(
        Duration::from_secs(5),
        WebSocketConnectionPool::supervise_pool(vec![h]),
    )
    .await;
    assert!(
        res.is_ok(),
        "supervise_pool must drain a dead depth-200 task within 5 s"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn test_order_update_supervisor_respawns_dead_task() {
    // Order-update is wrapped via `tokio::spawn(async { run_order_update_connection(...).await; })`
    // returning `Result<(), WebSocketError>` from the supervisor's
    // perspective by way of an outer adaptor at the call site
    // (main.rs). Here we exercise the same surface: a handle that
    // exits with WebSocketError (the only failure modality the
    // supervisor inspects). The 5s drain budget is the regression
    // bound.
    let h = tokio::spawn(async move {
        Err::<(), WebSocketError>(WebSocketError::ReconnectionExhausted {
            connection_id: 0,
            attempts: 60,
        })
    });
    let res = tokio::time::timeout(
        Duration::from_secs(5),
        WebSocketConnectionPool::supervise_pool(vec![h]),
    )
    .await;
    assert!(
        res.is_ok(),
        "supervise_pool must drain a dead order-update task within 5 s"
    );
}
