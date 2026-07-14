//! Fix #7 (2026-04-24): main-feed `websocket_connections` counter wiring.
//!
//! Before Fix #7 the counter `SystemHealthStatus::websocket_connections`
//! was never updated by the main-feed pool watchdog — only depth-20,
//! depth-200 and order-update had setter call sites. Consequence: the
//! `/health` endpoint and the 09:15:30 IST streaming-confirmation
//! heartbeat reported `websocket_connections: 0` forever, even with
//! all 5 sockets live.
//!
//! These tests ratchet:
//! 1. The `set_websocket_connections` API still exists and is
//!    increment/decrement-friendly (state.rs).
//! 2. The `spawn_pool_watchdog_task` signature in `crates/app/src/main.rs`
//!    includes `health` so the watchdog can push counts (source scan —
//!    full behavioural coverage needs a live WebSocket mock which is
//!    out of scope for a unit test crate).

use std::path::PathBuf;
use std::sync::Arc;

use tickvault_api::state::SystemHealthStatus;

#[test]
fn test_active_counter_increments_on_connect() {
    // Simulate five connections coming up in sequence.
    let health = Arc::new(SystemHealthStatus::new());
    assert_eq!(health.websocket_connections(), 0);
    for i in 1..=5u64 {
        health.set_websocket_connections(i);
        assert_eq!(
            health.websocket_connections(),
            i,
            "counter must reflect live socket count"
        );
    }
}

#[test]
fn test_active_counter_decrements_on_disconnect() {
    // Simulate five connections then one RST — count drops to 4.
    let health = Arc::new(SystemHealthStatus::new());
    health.set_websocket_connections(5);
    assert_eq!(health.websocket_connections(), 5);
    health.set_websocket_connections(4);
    assert_eq!(health.websocket_connections(), 4);
    health.set_websocket_connections(0);
    assert_eq!(
        health.websocket_connections(),
        0,
        "counter must go to 0 when all sockets drop"
    );
}

#[test]
fn test_active_counter_is_lock_free_atomic() {
    // Hot-path invariant: the counter is an atomic, so concurrent
    // reads/writes never block. This test fires N threads doing writes
    // and reads simultaneously and asserts no deadlock / panic.
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::thread;

    let health = Arc::new(SystemHealthStatus::new());
    let writes = Arc::new(AtomicU64::new(0));

    let mut handles = Vec::new();
    for _ in 0..4 {
        let h = Arc::clone(&health);
        let w = Arc::clone(&writes);
        handles.push(thread::spawn(move || {
            for i in 0..1000u64 {
                h.set_websocket_connections(i % 6);
                // Read back — must not panic.
                let _ = h.websocket_connections();
                w.fetch_add(1, Ordering::Relaxed);
            }
        }));
    }
    for h in handles {
        h.join().expect("no thread may panic");
    }
    assert_eq!(writes.load(Ordering::Relaxed), 4000);
}

/// PR-C2 (2026-07-13): the Fix #7 wiring ratchet is INVERTED. The Dhan
/// live-WS lane — and with it `spawn_pool_watchdog_task` + the main-feed
/// `set_websocket_connections` writes — is DELETED per the operator's
/// 2026-07-13 retirement directive (websocket-connection-scope-lock.md
/// "2026-07-13 Amendment"). `/health` honestly reports 0 main-feed
/// connections for the retired feed. This test now pins the DELETION:
/// a reappearing pool watchdog in main.rs is a scope-lock violation
/// (re-introducing the Dhan live WS requires a fresh dated operator
/// quote in the rule file first).
#[test]
fn test_pool_watchdog_task_stays_deleted_with_the_lane() {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.pop(); // crates/api -> crates
    path.push("app/src/main.rs");
    let src =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));

    assert!(
        !src.contains("fn spawn_pool_watchdog_task("),
        "PR-C2 regression: `spawn_pool_watchdog_task` reappeared in main.rs — \
         the Dhan live-WS lane was deleted 2026-07-13; re-introducing it \
         requires a fresh dated operator quote in \
         websocket-connection-scope-lock.md first."
    );
}
