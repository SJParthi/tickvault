//! Boot-time depth-200 smoke test (PR-B, 2026-05-02).
//!
//! Spawned once at boot, this module watches a shared `Arc<AtomicU64>` that
//! the depth-200 receiver tasks increment on every frame. It classifies the
//! outcome at the deadline:
//!
//! | In market hours? | Frames received within window? | Outcome |
//! |---|---|---|
//! | No                | (any)                          | `Skipped` (off-hours, frames not expected) |
//! | Yes               | ≥ 1                            | `Passed` (Severity::Info) |
//! | Yes               | 0                              | `Failed` (Severity::Critical, `DEPTH200-SMOKE-01`) |
//!
//! ## Why this exists
//!
//! Pre-PR-#427 we used a separate SELF token for depth-200; auth failures
//! showed up as `Protocol(ResetWithoutClosingHandshake)` reconnect storms.
//! Post-PR-#427 depth-200 reuses the shared APP token. A single positive
//! ping at boot ("Dhan accepted the APP token AND streamed at least one
//! frame") confirms Ticket #5610706's server-side gate removal end-to-end.
//!
//! The existing `MarketOpenStreamingConfirmation` event covers the same
//! ground at 09:15:30 IST per trading day; this smoke test fills the gap
//! for **mid-day in-market boots** (operator restart at e.g. 11:00 IST)
//! by giving a Critical-level signal within `SMOKE_DEADLINE_SECS` of boot
//! instead of waiting up to 4.5 hours for the next market-open self-test.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tickvault_common::error_code::ErrorCode;
use tickvault_common::market_hours::is_within_market_hours_ist;
use tracing::{error, info};

/// Deadline (seconds) for receiving the first depth-200 frame at boot.
///
/// Dhan's depth-200 connections handshake within ~5–10s. Subscribe ack
/// + first frame typically within another 5s. 60s gives a comfortable
/// margin without holding back the boot summary too long.
pub const SMOKE_DEADLINE_SECS: u64 = 60;

/// Poll interval (seconds) for the smoke test loop.
///
/// 1s is fast enough to fire `Passed` shortly after the first frame and
/// slow enough that the loop is essentially free CPU-wise (60 iterations
/// max).
pub const SMOKE_POLL_INTERVAL_SECS: u64 = 1;

/// Outcome of the boot-time depth-200 smoke test.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SmokeOutcome {
    /// Boot happened outside [09:00, 15:30] IST. Frames not expected;
    /// no signal either way is meaningful.
    Skipped,
    /// In-market boot, ≥ 1 frame observed within the deadline.
    Passed { frames_received: u64 },
    /// In-market boot, 0 frames within the deadline. `DEPTH200-SMOKE-01`.
    Failed,
}

/// Pure classifier — given the frame count and current market-hours
/// state at the deadline, return the outcome.
///
/// Separating this from the polling task makes the contract testable
/// without any tokio runtime.
#[must_use]
pub fn classify_smoke_outcome(frames_received: u64, in_market_hours: bool) -> SmokeOutcome {
    if !in_market_hours {
        return SmokeOutcome::Skipped;
    }
    if frames_received == 0 {
        return SmokeOutcome::Failed;
    }
    SmokeOutcome::Passed { frames_received }
}

/// Spawns the smoke-test task. Returns immediately; the task runs in the
/// background and emits exactly ONE log line at the deadline.
///
/// The caller wires `frames_counter` into every depth-200 receiver task
/// (see `Depth200MinimalConnInputs::depth_200_frame_counter`). The
/// counter MUST be incremented on every frame, NOT just on first frame —
/// this lets observability tools (Grafana, doctor) read it as a running
/// total without a separate Prom counter.
pub fn spawn_depth_200_boot_smoke_test(frames_counter: Arc<AtomicU64>) {
    tokio::spawn(async move {
        run_smoke_test_loop(frames_counter, SMOKE_DEADLINE_SECS).await;
    });
}

/// Runs the smoke-test loop until either a frame arrives or the deadline
/// expires. Public for unit testing — production callers go through
/// [`spawn_depth_200_boot_smoke_test`] which spawns this on the runtime.
pub async fn run_smoke_test_loop(frames_counter: Arc<AtomicU64>, deadline_secs: u64) {
    let poll_interval = Duration::from_secs(SMOKE_POLL_INTERVAL_SECS);
    let max_polls = deadline_secs.saturating_div(SMOKE_POLL_INTERVAL_SECS);

    for _ in 0..max_polls {
        tokio::time::sleep(poll_interval).await;
        if frames_counter.load(Ordering::Relaxed) > 0 {
            // Early exit on first frame — emit Passed immediately.
            let frames = frames_counter.load(Ordering::Relaxed);
            let in_market = is_within_market_hours_ist();
            emit_outcome(classify_smoke_outcome(frames, in_market));
            return;
        }
    }

    // Deadline reached.
    let frames = frames_counter.load(Ordering::Relaxed);
    let in_market = is_within_market_hours_ist();
    emit_outcome(classify_smoke_outcome(frames, in_market));
}

fn emit_outcome(outcome: SmokeOutcome) {
    match outcome {
        SmokeOutcome::Skipped => {
            info!(
                deadline_secs = SMOKE_DEADLINE_SECS,
                "depth-200 boot smoke test skipped — boot occurred outside market hours; \
                 frames not expected, signal not meaningful"
            );
        }
        SmokeOutcome::Passed { frames_received } => {
            info!(
                frames_received,
                deadline_secs = SMOKE_DEADLINE_SECS,
                "depth-200 boot smoke test PASSED — Dhan accepted the shared APP token \
                 and streamed at least one frame from full-depth-api.dhan.co \
                 (Ticket #5610706 server-side gate removal verified live)"
            );
        }
        SmokeOutcome::Failed => {
            error!(
                code = ErrorCode::Depth200Smoke01NoFramesAtBoot.code_str(),
                deadline_secs = SMOKE_DEADLINE_SECS,
                "depth-200 boot smoke test FAILED — zero frames received from \
                 full-depth-api.dhan.co within deadline. Possible causes: \
                 (a) Dhan reverted the Ticket #5610706 server-side gate \
                 (depth-200 rejecting APP tokens again), (b) all subscribed \
                 SecurityIds are too far OTM (server-side filtering), \
                 (c) WebSocket pool failed to subscribe. Check \
                 tv_websocket_connections_active{{kind=\"depth-200\"}} and \
                 errors.jsonl.* for subscribe ack failures"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_off_hours_returns_skipped_regardless_of_frame_count() {
        assert_eq!(classify_smoke_outcome(0, false), SmokeOutcome::Skipped);
        assert_eq!(classify_smoke_outcome(1, false), SmokeOutcome::Skipped);
        assert_eq!(classify_smoke_outcome(1_000, false), SmokeOutcome::Skipped);
    }

    #[test]
    fn classify_in_market_zero_frames_returns_failed() {
        assert_eq!(classify_smoke_outcome(0, true), SmokeOutcome::Failed);
    }

    #[test]
    fn classify_in_market_at_least_one_frame_returns_passed() {
        assert_eq!(
            classify_smoke_outcome(1, true),
            SmokeOutcome::Passed { frames_received: 1 }
        );
        assert_eq!(
            classify_smoke_outcome(42, true),
            SmokeOutcome::Passed {
                frames_received: 42
            }
        );
    }

    #[test]
    fn smoke_outcome_is_partial_eq_for_test_assertions() {
        // Belt-and-suspenders: PartialEq + Eq are derived; test asserts
        // we can match without `if let` chains.
        let a = SmokeOutcome::Passed { frames_received: 5 };
        let b = SmokeOutcome::Passed { frames_received: 5 };
        let c = SmokeOutcome::Passed { frames_received: 6 };
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn smoke_deadline_is_at_least_30s_to_allow_handshake() {
        // Dhan handshake is typically 5–10s; deadline must be at least
        // 30s to avoid spurious Failed events on slow networks.
        assert!(
            SMOKE_DEADLINE_SECS >= 30,
            "deadline too aggressive — handshake may not complete: {SMOKE_DEADLINE_SECS}s"
        );
    }

    #[test]
    fn smoke_deadline_is_at_most_120s_to_keep_signal_timely() {
        // > 2 minutes makes the signal less actionable; operator
        // wants to know within boot's first cycle.
        assert!(
            SMOKE_DEADLINE_SECS <= 120,
            "deadline too lax — signal will lag operator action: {SMOKE_DEADLINE_SECS}s"
        );
    }

    #[test]
    fn smoke_poll_interval_divides_deadline_cleanly() {
        // Defensive: if the interval doesn't divide, the loop's
        // `saturating_div` rounds down and could exit slightly early.
        // Not catastrophic but indicates a constant misconfiguration.
        assert_eq!(
            SMOKE_DEADLINE_SECS % SMOKE_POLL_INTERVAL_SECS,
            0,
            "poll interval ({SMOKE_POLL_INTERVAL_SECS}s) must divide deadline \
             ({SMOKE_DEADLINE_SECS}s) cleanly"
        );
    }

    // Note: time-based tokio tests use real time (no `start_paused`)
    // because the workspace's `tokio` dev-deps don't enable `test-util`.
    // We use very short deadlines to keep CI fast.

    #[tokio::test]
    async fn run_smoke_test_loop_exits_early_on_first_frame() {
        let counter = Arc::new(AtomicU64::new(0));
        let counter_clone = Arc::clone(&counter);

        // Increment counter BEFORE starting the loop — the very first
        // poll iteration should observe the count and exit.
        counter_clone.store(7, Ordering::Relaxed);

        let start = std::time::Instant::now();
        // 5s deadline (real time). Loop should exit after the first
        // 1s poll observes the non-zero count.
        run_smoke_test_loop(counter, 5).await;
        let elapsed = start.elapsed();

        assert!(
            elapsed < Duration::from_secs(3),
            "loop should exit shortly after first poll, not run to deadline: {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn run_smoke_test_loop_runs_to_deadline_with_zero_frames() {
        let counter = Arc::new(AtomicU64::new(0));
        let start = std::time::Instant::now();
        // Tiny deadline (2s = 2 polls) keeps CI fast.
        run_smoke_test_loop(counter, 2).await;
        let elapsed = start.elapsed();

        assert!(
            elapsed >= Duration::from_secs(2),
            "loop should run to deadline with zero frames: {elapsed:?}"
        );
    }
}
