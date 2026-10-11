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

/// Production source of main.rs with `//` comment lines removed, so a comment
/// naming a call can never satisfy a pin.
fn main_rs_code() -> String {
    main_rs()
        .lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The body of `async fn wait_for_shutdown_signal()`, up to the next item.
fn wait_fn_body(code: &str) -> String {
    let start = code
        .find("async fn wait_for_shutdown_signal()")
        .expect("main.rs defines wait_for_shutdown_signal");
    let rest = &code[start..];
    let end = rest[1..]
        .find("\n#[cfg(test)]")
        .or_else(|| rest[1..].find("\nfn "))
        .map_or(rest.len(), |e| e + 1);
    rest[..end].to_string()
}

/// Regression: 2026-10-11 (audit CONC-5) — each wait built fresh signal
/// streams and dropped them on return. A tokio stream subscribes at the
/// current version, so a SIGTERM delivered between two waits reached no one,
/// and systemd SIGKILLed the process after TimeoutStopSec. The streams are
/// now created once, in the first wait, and kept in a static slot.
#[test]
fn shutdown_signal_streams_are_created_once_not_per_wait() {
    let code = main_rs_code();
    assert!(
        code.contains("static SHUTDOWN_SIGNALS:"),
        "the shutdown-signal streams must live in a static slot that outlives a wait"
    );
    let body = wait_fn_body(&code);
    let lock = body
        .find("SHUTDOWN_SIGNALS.lock()")
        .expect("the wait reads the persistent slot");
    let first_use = body
        .find("if slot.is_none()")
        .expect("the streams are created only when the slot is empty");
    let create = body
        .find("signal(SignalKind::terminate())")
        .expect("the wait still listens for SIGTERM");
    assert!(
        lock < first_use && first_use < create,
        "SIGTERM's stream must be created inside the first-use branch, after the slot is read"
    );
    assert_eq!(
        body.matches("signal(SignalKind::terminate())").count(),
        1,
        "exactly one SIGTERM stream creation, on first use"
    );
    assert!(
        body.contains(".terminate.recv()") && body.contains(".interrupt.recv()"),
        "the wait polls the persistent streams"
    );
}

/// Regression: 2026-10-11 (audit CONC-5) — the post-market drain sleep and
/// partition detach ran with no shutdown signal being listened for. They now
/// race the signal wait, which wins.
#[test]
fn post_market_housekeeping_is_preemptible_by_the_shutdown_signal() {
    let code = main_rs_code();
    let detach = code
        .find("detach_old_partitions()")
        .expect("the post-market detach still runs");
    let housekeeping = code
        .find("let post_market_housekeeping = async {")
        .expect("the post-market housekeeping is one future");
    assert!(
        housekeeping < detach,
        "the detach runs inside the housekeeping future"
    );
    let select_at = code[detach..]
        .find("tokio::select!")
        .map(|i| detach + i)
        .expect("the housekeeping future is raced against the signal");
    let select = &code[select_at..];
    let select = &select[..select.find("};").expect("select! ends")];
    assert!(select.contains("biased;"), "the signal arm is polled first");
    let signal = select
        .find("wait_for_shutdown_signal()")
        .expect("one arm waits for the shutdown signal");
    let work = select
        .find("post_market_housekeeping")
        .expect("the other arm runs the housekeeping");
    assert!(
        signal < work,
        "the signal arm comes first in the biased select"
    );
}

/// Audit CONC-4 (2026-10-11): a clean stop releases the dual-instance lock
/// after the WAL floor (nothing can still need it) and before the notifier
/// flush (a slow Telegram cannot eat its budget).
#[test]
fn shutdown_releases_the_instance_lock_after_the_wal_and_before_the_notifier() {
    let code = main_rs_code();
    let wal = code
        .find("spill.shutdown(")
        .expect("shutdown drains the WAL writer");
    let release = code
        .find("release_instance_lock_at_shutdown(")
        .expect("shutdown releases the dual-instance lock");
    let notifier = code
        .find("notifier.shutdown_flush()")
        .expect("shutdown flushes the notifier");
    assert!(wal < release, "the lock is released after the WAL floor");
    assert!(
        release < notifier,
        "the lock is released before the notifier flush"
    );
    assert!(
        code[release..notifier].contains("INSTANCE_LOCK_RELEASE_BUDGET"),
        "the release is bounded by INSTANCE_LOCK_RELEASE_BUDGET"
    );

    let stack = std::fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/dhan_rest_stack.rs"),
    )
    .expect("dhan_rest_stack.rs must be readable");
    let prod = stack.split("#[cfg(test)]").next().unwrap_or(&stack);
    assert!(
        !prod.contains("let _heartbeat_handle"),
        "the heartbeat handle must be kept, not discarded"
    );
    assert!(
        prod.contains("INSTANCE_LOCK_HEARTBEAT"),
        "the heartbeat is registered for the shutdown release"
    );
}
