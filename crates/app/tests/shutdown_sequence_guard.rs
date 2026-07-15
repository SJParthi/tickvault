//! Audit finding #12 (2026-04-17): shutdown sequence coverage guard.
//!
//! This test DOES NOT claim every spawned task in `main.rs` listens on
//! the `shutdown_notify` — that would require a massive refactor
//! (~39 spawn sites, most are observability tasks that safely get
//! force-killed).
//!
//! What it DOES claim, mechanically, is that the shutdown sequence
//! in `run_shutdown_fast` (and both boot paths' shutdown logic):
//!
//!   1. Fires `shutdown_notify.notify_waiters()` so long-lived tasks
//!      that DO listen (pool watchdog, depth rebalancer, token
//!      renewal watchdog) stop cleanly.
//!   2. Signals `pool.request_graceful_shutdown()` so every live
//!      WebSocket sends a RequestCode 12 Disconnect to Dhan.
//!   3. Flushes the tick + depth + live candle writers via
//!      `flush_on_shutdown()` in `run_tick_processor` (which is
//!      awaited before the process exits).
//!
//! Any of these going missing = data loss on SIGTERM. Source-scan
//! guard fires the build if someone deletes one of these calls.

#![cfg(test)]

use std::path::PathBuf;

fn main_rs() -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/main.rs");
    std::fs::read_to_string(&path).expect("crates/app/src/main.rs must be readable")
}

fn tick_processor_rs() -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates parent")
        .join("core/src/pipeline/tick_processor.rs");
    std::fs::read_to_string(&path).expect("tick_processor.rs must be readable")
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
// Tick processor must force-flush ALL writers on shutdown
// ============================================================================

#[test]
fn tick_processor_flushes_tick_writer_on_shutdown() {
    let src = tick_processor_rs();
    // Match the call whether the writer is bound as `writer` or `tw`.
    let patterns = ["writer.flush_on_shutdown()", "tw.flush_on_shutdown()"];
    let hit = patterns.iter().any(|p| src.contains(p));
    assert!(
        hit,
        "Audit finding #12/#3: run_tick_processor MUST call \
         flush_on_shutdown() on the tick writer before exit. Without this, \
         up to 300K ticks sitting in the ring buffer are lost on SIGTERM."
    );
}

#[test]
fn tick_processor_flushes_candle_writer_on_shutdown() {
    let src = tick_processor_rs();
    // flush_on_shutdown is called on live_candle_writer inside the candle
    // aggregator shutdown block. Match either the direct call or the
    // `cw.flush_on_shutdown` pattern.
    let hit = src.contains("cw.flush_on_shutdown()") || src.contains("flush_on_shutdown();");
    assert!(
        hit,
        "Audit finding #12/#3: run_tick_processor MUST flush the live \
         candle writer on shutdown. Without this, the last few seconds of \
         live candles are lost."
    );
}

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
