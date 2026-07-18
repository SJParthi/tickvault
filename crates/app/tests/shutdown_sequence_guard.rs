//! Audit finding #12 (2026-04-17): shutdown sequence coverage guard.
//!
//! This test DOES NOT claim every spawned task in `main.rs` listens on
//! the `shutdown_notify` — that would require a massive refactor
//! (~39 spawn sites, most are observability tasks that safely get
//! force-killed).
//!
//! What it originally claimed shrank with the runtime: the lane-scoped
//! notify/graceful-disconnect pins retired in PR-C2 (2026-07-13, Dhan
//! live-WS lane deletion) and the `run_tick_processor` writer-flush pins
//! retired in the stage-2 dead-WS sweep (2026-07-17, tick chain deletion).
//! The SURVIVING claim: the signal-wait helper in `main.rs` covers both
//! SIGTERM (unix prod) and Ctrl-C/SIGINT (dev), so an operator can always
//! stop the process cleanly.

#![cfg(test)]

use std::path::PathBuf;

fn main_rs() -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/main.rs");
    std::fs::read_to_string(&path).expect("crates/app/src/main.rs must be readable")
}

// ============================================================================
// Shutdown sequence must call notify_waiters
// ============================================================================

// RETIRED (PR-C2, 2026-07-13 — Dhan live-WS lane deletion, operator
// retirement directive per websocket-connection-scope-lock.md "2026-07-13
// Amendment" §B): `shutdown_sequence_fires_notify_waiters` and
// `shutdown_sequence_signals_graceful_websocket_disconnect` pinned the
// lane-scoped shutdown fan-out — `shutdown_notify.notify_waiters()` woke
// the pool watchdog/lane listeners, and `pool.request_graceful_shutdown()`
// sent RequestCode 12 per main-feed socket so the next boot never hit
// Dhan's 5-connection budget. The pool, its watchdog, and the lane
// teardown are DELETED — no main-feed socket exists to disconnect, so the
// SIGTERM false-Halt / connection-budget classes are structurally gone.
// The surviving flush guards (tick processor writers) stay pinned below.

// ============================================================================
// Tick processor flush guards — RETIRED with the tick chain
// ============================================================================

// RETIRED (stage-2 dead-WS sweep, 2026-07-17):
// `tick_processor_flushes_tick_writer_on_shutdown` and
// `tick_processor_flushes_candle_writer_on_shutdown` pinned
// `flush_on_shutdown()` in `run_tick_processor` — the subject file
// (`crates/core/src/pipeline/tick_processor.rs`) was DELETED with the dead
// Dhan tick chain (zero production spawn sites since PR-C2/C3; the Groww
// live feed retired 2026-07-15), so there is no tick/candle writer left to
// flush on SIGTERM. The seal chain's shutdown flushing is pinned by its own
// guards (seal_writer_runner / shadow_persistence tests).

// ============================================================================
// Shutdown notifier is a single Arc<Notify> — not reconstructed per-task
// ============================================================================

// RETIRED (PR-C2, 2026-07-13): `shutdown_notify_is_arc_notify` pinned the
// boot-once `Arc<Notify>` construction that fanned the lane shutdown — it
// died with the lane teardown above. The Rule-16 Arc<Notify> convention
// itself stands (audit-findings-2026-04-17.md Rule 16) for any future
// long-lived listener chain.

// ============================================================================
// run_shutdown_fast is wired to wait_for_shutdown_signal
// ============================================================================

#[test]
fn wait_for_shutdown_signal_covers_sigterm_and_ctrl_c() {
    let src = main_rs();
    // The signal-wait helper must handle both SIGTERM (unix prod) and
    // Ctrl-C (dev / mac). Missing either = the app stays alive after
    // an operator Ctrl-C.
    assert!(
        src.contains("ctrl_c") || src.contains("Ctrl-C") || src.contains("SIGINT"),
        "wait_for_shutdown_signal must accept Ctrl-C / SIGINT"
    );
    assert!(
        src.contains("terminate()") || src.contains("SIGTERM"),
        "wait_for_shutdown_signal must accept SIGTERM (unix prod)"
    );
}
