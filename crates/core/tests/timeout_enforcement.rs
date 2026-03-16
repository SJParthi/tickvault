//! Timeout enforcement tests.
//!
//! Verifies that async operations respect deadlines and do not
//! hang indefinitely. Critical for a trading system where stuck
//! operations can block order execution.
//!
//! NOTE: Upper-bound timing assertions are skipped under instrumented
//! builds where execution is 10-100x slower.

use std::time::Duration;

/// Returns true when running under instrumented builds.
fn skip_perf_assertions() -> bool {
    std::env::var("DLT_SKIP_PERF_ASSERTIONS").is_ok()
}

use tokio::time::timeout;

// ---------------------------------------------------------------------------
// Channel receive timeout
// ---------------------------------------------------------------------------

#[tokio::test]
async fn timeout_mpsc_recv_respects_deadline() {
    let (_tx, mut rx) = tokio::sync::mpsc::channel::<u32>(16);
    // No sender will ever send — recv should hang forever without timeout

    let result = timeout(Duration::from_millis(50), rx.recv()).await;
    assert!(result.is_err(), "recv should time out when no data arrives");
}

#[tokio::test]
async fn timeout_mpsc_recv_succeeds_before_deadline() {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<u32>(16);

    // Send immediately
    tx.send(42).await.unwrap();

    let result = timeout(Duration::from_secs(1), rx.recv()).await;
    assert!(result.is_ok(), "recv should complete before deadline");
    assert_eq!(result.unwrap(), Some(42));
}

// ---------------------------------------------------------------------------
// Broadcast receive timeout
// ---------------------------------------------------------------------------

#[tokio::test]
async fn timeout_broadcast_recv_respects_deadline() {
    let (_tx, mut rx) = tokio::sync::broadcast::channel::<u32>(16);

    let result = timeout(Duration::from_millis(50), rx.recv()).await;
    assert!(result.is_err(), "broadcast recv should time out");
}

// ---------------------------------------------------------------------------
// Sleep / Timer accuracy
// ---------------------------------------------------------------------------

#[tokio::test]
async fn timeout_tokio_sleep_completes_within_bounds() {
    let target = Duration::from_millis(50);
    let start = std::time::Instant::now();

    tokio::time::sleep(target).await;

    let elapsed = start.elapsed();
    // Upper bound: skip under instrumentation (cargo-careful adds overhead)
    if !skip_perf_assertions() {
        assert!(
            elapsed < target * 3,
            "sleep took {elapsed:?}, expected ~{target:?}"
        );
    }
    // Lower bound: should not complete too early (always valid)
    assert!(
        elapsed >= target - Duration::from_millis(5),
        "sleep completed too early: {elapsed:?}"
    );
}

// ---------------------------------------------------------------------------
// Select with timeout — pattern used in tick processor
// ---------------------------------------------------------------------------

#[tokio::test]
async fn timeout_select_with_deadline() {
    let (_tx, mut rx) = tokio::sync::mpsc::channel::<u32>(16);
    let deadline = Duration::from_millis(50);

    let start = std::time::Instant::now();
    tokio::select! {
        msg = rx.recv() => {
            // Should not happen — no sender
            panic!("unexpected message: {msg:?}");
        }
        () = tokio::time::sleep(deadline) => {
            // Expected — timeout triggered
        }
    }
    let elapsed = start.elapsed();
    assert!(
        elapsed >= Duration::from_millis(40),
        "select returned too early: {elapsed:?}"
    );
}

// ---------------------------------------------------------------------------
// Graceful shutdown timeout
// ---------------------------------------------------------------------------

#[tokio::test]
async fn timeout_task_cancellation_on_shutdown() {
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::broadcast::channel::<()>(1);

    let handle = tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = shutdown_rx.recv() => {
                    return "shutdown";
                }
                () = tokio::time::sleep(Duration::from_secs(3600)) => {
                    // Would run forever without shutdown
                }
            }
        }
    });

    // Signal shutdown
    shutdown_tx.send(()).unwrap();

    // Task should complete quickly
    let result = timeout(Duration::from_millis(100), handle).await;
    assert!(result.is_ok(), "task should respond to shutdown signal");
    assert_eq!(result.unwrap().unwrap(), "shutdown");
}

// ---------------------------------------------------------------------------
// Nested timeout (inner timeout shorter than outer)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn timeout_nested_inner_fires_first() {
    let inner_deadline = Duration::from_millis(25);
    let outer_deadline = Duration::from_millis(100);

    let result = timeout(outer_deadline, async {
        timeout(inner_deadline, async {
            // Never completes
            tokio::time::sleep(Duration::from_secs(3600)).await;
            42_u32
        })
        .await
    })
    .await;

    // Outer should succeed (inner timed out first)
    assert!(result.is_ok(), "outer timeout should not fire");
    // Inner should report timeout
    assert!(result.unwrap().is_err(), "inner timeout should fire");
}
