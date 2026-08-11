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

// `std::path::PathBuf` was dropped 2026-08-11 with the retired
// `spawn_pool_watchdog_task` absence-scan — the replacement pin is
// behavioural, so this file no longer reads any source file from disk.
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

/// PR-C2 (2026-07-13): the Fix #7 wiring ratchet was INVERTED to pin the
/// DELETION of `spawn_pool_watchdog_task` — a reappearing pool watchdog in
/// main.rs was a scope-lock violation while the Dhan live-WS lane was
/// retired.
///
/// RE-BLESSED 2026-08-11 — that absence-pin is RETIRED, and deliberately
/// NOT simply deleted; it is replaced below by a presence-pin covering the
/// same failure.
///
/// **Why the absence-pin had to go:** the operator authorized the Dhan
/// 16-connection live main-feed revival on 2026-08-09
/// (websocket-connection-scope-lock.md, "DHAN LIVE MAIN-FEED WS REVIVAL
/// AUTHORIZED" + the same-day second quote lifting the connection cap to
/// 16), approved 2026-08-11. Its precondition — "re-introducing the Dhan
/// live WS requires a fresh dated operator quote in the rule file first" —
/// is now SATISFIED, so the assertion would have blocked authorized work
/// for a reason that no longer holds. (Verified 2026-08-11: it does not
/// trip today either — the revival lands as
/// `crates/app/src/dhan_feed_stack.rs::spawn_dhan_feed_stack`, and the
/// per-connection supervision lives in
/// `crates/core/src/websocket/pool_supervisor.rs`, so no
/// `spawn_pool_watchdog_task` returns to main.rs at all. Retiring it is
/// about correctness of intent, not an imminent break.)
///
/// **What replaces it, and why this is the same protection:** this file
/// exists because of Fix #7 — `/health` reporting `websocket_connections:
/// 0` forever while sockets were live. The revival re-opens exactly that
/// exposure from the other side: with up to 16 connections running, a
/// `/health` that still reads 0 is a false-OK (audit Rule 11). `state.rs`
/// closes it with the `websocket_reported` gate (2026-08-09
/// observability-blind-spot audit): while NO producer has ever pushed a
/// count, `/health` renders `retired` rather than a fake `disconnected`;
/// the first real push flips the flag and re-arms the degrade predicate.
///
/// That gate only works if `set_websocket_connections` actually flips the
/// flag. If a future refactor stores the count without setting
/// `websocket_reported`, `/health` would render `retired` **permanently**
/// while all 16 sockets ran — silently, with a plausible-looking verdict.
/// That is strictly worse than the original Fix #7 bug, because `retired`
/// reads as intentional. So the pin moves from "the watchdog must be
/// absent" to "the counter's write path must arm the honesty gate".
#[test]
fn test_health_counter_write_arms_the_websocket_reported_gate() {
    let health = Arc::new(SystemHealthStatus::new());

    // Pre-revival state: nothing has ever reported, so `/health` is
    // entitled to render `retired` instead of a fabricated `disconnected`.
    assert!(
        !health.websocket_reported(),
        "a freshly-constructed SystemHealthStatus must report NO producer \
         yet — otherwise `/health` would claim the websocket subsystem is \
         live before any feed has pushed a single count."
    );

    // The revival's first pushed count must ARM the gate.
    health.set_websocket_connections(16);
    assert_eq!(health.websocket_connections(), 16);
    assert!(
        health.websocket_reported(),
        "Fix #7 regression (re-blessed 2026-08-11): \
         `set_websocket_connections` stored a count WITHOUT arming \
         `websocket_reported`. With the Dhan 16-connection revival live, \
         `/health` would render the websocket subsystem as `retired` \
         forever while every socket was actually running — a false-OK that \
         reads as intentional, so no operator would ever investigate it."
    );

    // ...and once armed, a genuine drop to zero must still be visible as a
    // real zero (not silently re-classified back to `retired`), which is
    // the degrade signal Fix #7 was created to preserve.
    health.set_websocket_connections(0);
    assert_eq!(health.websocket_connections(), 0);
    assert!(
        health.websocket_reported(),
        "once a producer has reported, the gate must STAY armed — allowing \
         it to fall back to `retired` on a drop to 0 would disguise a total \
         feed outage (16 → 0 connections) as a deliberate retirement."
    );
}
