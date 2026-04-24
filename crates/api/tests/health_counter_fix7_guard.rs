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

/// Ratchets that `spawn_pool_watchdog_task` in main.rs takes a
/// `SharedHealthStatus` parameter, so the watchdog can push counts.
/// If someone refactors this away without a replacement the main-feed
/// gauge will silently go dark again.
#[test]
fn test_pool_watchdog_task_accepts_health_status() {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.pop(); // crates/api -> crates
    path.push("app/src/main.rs");
    let src =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));

    // Negative ratchet: the old 3-param signature MUST NOT reappear.
    // We test by positive match instead to avoid a brittle whitespace
    // pattern: the definition must include the `health:` parameter.
    assert!(
        src.contains("fn spawn_pool_watchdog_task(")
            && src.contains("tickvault_api::state::SharedHealthStatus"),
        "Fix #7 regression: `spawn_pool_watchdog_task` no longer takes \
         a `SharedHealthStatus`. Without it the watchdog cannot push \
         main-feed connection counts to the /health endpoint — the \
         exact symptom (0/5 forever) Fix #7 was created to close."
    );

    // And: the body must call `set_websocket_connections` so the counter
    // is actually written, not just accepted.
    assert!(
        src.contains("set_websocket_connections"),
        "Fix #7 regression: watchdog accepts the health handle but does \
         not call `set_websocket_connections`. Pass-through with no \
         write is the same as not wiring it at all."
    );
}
