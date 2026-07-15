//! Wave 2 Item 6 (G1) — order-update WebSocket post-close sleep ratchets.
//!
//! These tests pin the surviving regression invariants documented in
//! `.claude/plans/active-plan-wave-2.md` Item 6:
//!
//! 1. `test_order_update_post_close_sleeps_until_next_open` —
//!    source-scan ratchet asserting `order_update_connection.rs`
//!    REPLACED the legacy `Exhausted` `return` path with
//!    `order_update_post_close_sleep` + `continue`.
//! 2. `test_order_update_activity_watchdog_remains_14400s` — source-scan
//!    ratchet asserting `WATCHDOG_THRESHOLD_ORDER_UPDATE_SECS` stays
//!    14_400. Regressing to 1800 reintroduces the every-30-min false
//!    reconnect storm (see `live-market-feed-subscription.md`
//!    2026-04-24 §7).
//!
//! PR-C2 trim (2026-07-13): the three `supervise_pool` drain ratchets
//! (depth-20 / depth-200 / order-update handle drains) retired with
//! `WebSocketConnectionPool` — deleted alongside the Dhan live main-feed
//! lane (operator retirement directive —
//! `websocket-connection-scope-lock.md` "2026-07-13 Amendment"). The
//! order-update task is now spawned + supervised by `dhan_rest_stack`
//! (pinned by the app-side ratchets). The depth-feed source-scan ratchets
//! were already removed 2026-06-02 (AWS-lifecycle PR #4).

#![allow(clippy::unwrap_used)] // APPROVED: test code
#![allow(clippy::expect_used)] // APPROVED: test code

use std::fs;
use std::path::PathBuf;

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

// REMOVED 2026-06-02 (CI rot fix): the depth-20 / depth-200 post-close
// source-scan ratchets. The depth WebSocket modules
// (`crates/core/src/websocket/depth_connection.rs`,
// `run_twenty_depth_connection`, `run_two_hundred_depth_connection`) were
// DELETED in the AWS-lifecycle PR #4 — only the main-feed + order-update
// WebSockets remain forever per
// `.claude/rules/project/websocket-connection-scope-lock.md`. The two
// source-scan tests below read a file that no longer exists and so panicked.
// The order-update post-close ratchet (which guards the SAME shared helper
// pattern on the surviving feed) is retained below.

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
