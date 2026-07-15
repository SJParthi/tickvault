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

/// Reads the production `main.rs` source and asserts the /health
/// `questdb_reachable` flag keeps a PRODUCTION writer.
///
/// PR-C2 (2026-07-13): the Dhan pool watchdog — the original writer this
/// guard pinned — is DELETED with the Dhan live-WS lane (operator
/// retirement directive; websocket-connection-scope-lock.md "2026-07-13
/// Amendment"). The flag's writer re-homed into the process-shared
/// observability task (`run_slow_boot_observability`), whose 2s QuestDB
/// /exec ping is now CADENCE-driven (a select! interval arm) and writes
/// `health.set_questdb_reachable(connected)` on every ping. Without a
/// production writer, `GET /health` (and overall_status) would report
/// QuestDB down FOREVER — the Rule-11 false-degraded this guard exists
/// to prevent.
#[test]
fn test_observability_task_sets_questdb_reachable_from_production() {
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.pop(); // crates/api -> crates
    path.push("app/src/main.rs");
    let src =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));

    // A production call site must exist — main.rs has no #[cfg(test)]
    // setter, so any hit is a production write.
    assert!(
        src.contains("health.set_questdb_reachable(connected)"),
        "PR-C2 regression: the observability task no longer writes \
         `health.set_questdb_reachable(connected)`. Without a production \
         writer, `health.questdb_reachable()` stays permanently false and \
         GET /health reports QuestDB down forever (Rule-11 false-degraded)."
    );

    // The writer must live in the surviving observability task, which must
    // take the health handle so the wiring is real, not vestigial.
    assert!(
        src.contains("async fn run_slow_boot_observability(")
            && src.contains("health: tickvault_api::state::SharedHealthStatus"),
        "PR-C2 regression: `run_slow_boot_observability` no longer accepts \
         the SharedHealthStatus handle — the questdb_reachable signal has \
         lost its writer wiring."
    );

    // The ping must be cadence-driven (interval arm), not tick-gated: with
    // the Dhan live WS retired the tick broadcast has no publisher, so a
    // recv()-gated ping would never fire and the flag would go blind.
    assert!(
        src.contains("qdb_ping_ticker.tick()"),
        "PR-C2 regression: the QuestDB health ping is no longer driven by \
         its own interval — a tick-gated ping starves on the publisher-less \
         broadcast and the /health flag goes permanently stale."
    );
}
