//! Fix A no-op close (2026-06-30): production QuestDB-liveness wiring.
//!
//! PR #1268 ("reconnect-in-place on Dhan reset class") added the pure
//! `is_bare_reset_class(healths, token_valid, questdb_reachable)` gate in
//! `crates/app/src/main.rs`, which lets the pool watchdog reconnect IN PLACE
//! (instead of `process::exit(2)` → 775-SID re-subscribe → Dhan 429 restart
//! storm) on a benign bare-Dhan-TCP-reset class. That gate requires
//! `questdb_reachable == true`.
//!
//! The bug this guard closes: `questdb_reachable` is backed by an
//! `AtomicBool` on `SystemHealthStatus` that was ONLY ever set `true` in
//! `#[cfg(test)]` code. In production `health.questdb_reachable()` was
//! therefore permanently `false` → `is_bare_reset_class` always returned
//! `false` → the reconnect-in-place branch was DEAD CODE → the watchdog
//! still exited and the storm still happened. CI stayed green because the
//! unit tests pass the flag as a literal argument.
//!
//! The fix wires a REAL signal: `spawn_pool_watchdog_task` now probes
//! QuestDB every 5s (the same tick that already pushes
//! `set_websocket_connections`) and calls `health.set_questdb_reachable(...)`
//! with the live result — independent of tick flow, so a Dhan bare-RST storm
//! (which stops ticks while QuestDB stays up) cannot freeze the signal.
//!
//! These tests ratchet, mirroring `health_counter_fix7_guard.rs`:
//! 1. `set_questdb_reachable` / `questdb_reachable` round-trip on
//!    `SystemHealthStatus` (the API still exists and is honest).
//! 2. `spawn_pool_watchdog_task` in `crates/app/src/main.rs` actually CALLS
//!    `set_questdb_reachable` from a NON-test (production) path — i.e. there
//!    is at least one production call site, so the gate is no longer a no-op.
//! 3. The Halt arm reads `health.questdb_reachable()` to feed the classifier.

use std::path::PathBuf;
use std::sync::Arc;

use tickvault_api::state::SystemHealthStatus;

#[test]
fn test_questdb_reachable_round_trips() {
    let health = Arc::new(SystemHealthStatus::new());
    // Default is unreachable (a fresh process has not yet probed QuestDB).
    assert!(
        !health.questdb_reachable(),
        "fresh health status must default to QuestDB unreachable"
    );
    health.set_questdb_reachable(true);
    assert!(
        health.questdb_reachable(),
        "setter must flip the flag to reachable"
    );
    health.set_questdb_reachable(false);
    assert!(
        !health.questdb_reachable(),
        "setter must flip the flag back to unreachable (a real DB death must show)"
    );
}

/// Reads the production `main.rs` source and asserts the dead-signal
/// regression cannot recur: `spawn_pool_watchdog_task` must take a QuestDB
/// config AND call `set_questdb_reachable`, AND the Halt arm must read
/// `health.questdb_reachable()` to feed the bare-reset classifier.
#[test]
fn test_pool_watchdog_sets_questdb_reachable_from_production() {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.pop(); // crates/api -> crates
    path.push("app/src/main.rs");
    let src =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));

    // The whole `set_questdb_reachable` call-site search must find at least
    // one occurrence OUTSIDE a `#[cfg(test)]` module — main.rs has no
    // `#[cfg(test)]` setter, so any hit here is a production call site.
    assert!(
        src.contains("set_questdb_reachable"),
        "Fix-A-no-op-close regression: `spawn_pool_watchdog_task` no longer \
         calls `set_questdb_reachable`. Without a production writer, \
         `health.questdb_reachable()` stays permanently false and the \
         reconnect-in-place gate (`is_bare_reset_class`) becomes dead code — \
         the watchdog will exit and re-trigger the Dhan 429 restart storm."
    );

    // The watchdog must accept a QuestDB config so it can probe liveness.
    assert!(
        src.contains("fn spawn_pool_watchdog_task(")
            && src.contains("questdb_config: tickvault_common::config::QuestDbConfig"),
        "Fix-A-no-op-close regression: `spawn_pool_watchdog_task` no longer \
         takes a `QuestDbConfig`. Without it the watchdog cannot probe \
         QuestDB liveness to maintain the `questdb_reachable` signal."
    );

    // The watchdog must actually probe QuestDB liveness via the canonical
    // boot probe (reused, not hand-rolled), so the signal reflects reality.
    assert!(
        src.contains("wait_for_questdb_ready"),
        "Fix-A-no-op-close regression: the watchdog accepts the QuestDB \
         config but does not probe QuestDB liveness — a config passed but \
         never used is the same as not wiring the signal at all."
    );

    // And the Halt arm must READ the signal back to feed the classifier, so
    // the production gate actually consumes the wired value.
    assert!(
        src.contains("health.questdb_reachable()") && src.contains("is_bare_reset_class("),
        "Fix A regression: the pool-watchdog Halt arm no longer reads \
         `health.questdb_reachable()` to feed `is_bare_reset_class`. The \
         wired signal must be consumed by the gate or it is inert."
    );
}
