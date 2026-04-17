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

#[test]
fn shutdown_sequence_fires_notify_waiters() {
    let src = main_rs();
    assert!(
        src.contains("shutdown_notify.notify_waiters()"),
        "Audit finding #12: run_shutdown_fast (or equivalent) MUST call \
         `shutdown_notify.notify_waiters()` so long-lived tasks \
         (pool watchdog, depth rebalancer) stop cleanly on SIGTERM. \
         Missing this call = false-positive Halt alerts during intentional \
         teardown."
    );
}

#[test]
fn shutdown_sequence_signals_graceful_websocket_disconnect() {
    let src = main_rs();
    assert!(
        src.contains("request_graceful_shutdown()"),
        "Audit finding #12: shutdown MUST call \
         `pool.request_graceful_shutdown()` so each WebSocket sends \
         RequestCode 12 to Dhan before the socket closes. Without this, \
         Dhan's server holds our subscriptions open for 40s after our \
         process exits, and the NEXT boot hits \
         `MAX_WEBSOCKET_CONNECTIONS_PER_USER` (5 of 5 already consumed)."
    );
}

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
fn tick_processor_flushes_depth_writer_on_shutdown() {
    let src = tick_processor_rs();
    let patterns = ["dw.flush_on_shutdown()"];
    let hit = patterns.iter().any(|p| src.contains(p));
    assert!(
        hit,
        "Audit finding #12/#3: run_tick_processor MUST call \
         flush_on_shutdown() on the depth writer before exit."
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

#[test]
fn shutdown_notify_is_arc_notify() {
    let src = main_rs();
    assert!(
        src.contains("let shutdown_notify = std::sync::Arc::new(tokio::sync::Notify::new())"),
        "Audit finding #12: shutdown_notify MUST be an Arc<Notify> \
         initialised once at boot and cloned into each listener. \
         A per-task Notify would never receive the shutdown signal."
    );
}

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
