//! STAGE-C.3: Per-connection WebSocket activity watchdog.
//!
//! Each WebSocket connection publishes a shared atomic `activity_counter`
//! that is bumped on every inbound frame (data, ping, pong, text). The
//! watchdog task polls that counter at a fixed cadence and fires a
//! `tokio::sync::Notify` when the counter has not advanced for longer than
//! the configured `threshold`. The WS read loop selects on the notify and
//! returns `Err(WebSocketError::WatchdogFired)`, which the outer `run()`
//! loop treats as a reconnectable error.
//!
//! **This is the ONLY deadline in the WS pipeline.** The read loop itself
//! has no client-side read timeout — Dhan's server closes idle sockets
//! after 40s of no pong, and the watchdog is a sidecar check that only
//! fires when the socket is *truly* dead (no frames AND no pings for
//! longer than the server's own timeout).
//!
//! # Thresholds
//! - Live Market Feed / Depth-20 / Depth-200: 50s
//!   (40s Dhan server ping timeout + 10s safety margin)
//! - Live Order Update: 660s
//!   (600s idle-between-orders tolerance + 60s safety margin)
//!
//! # Guarantees
//! - O(1) per poll: one atomic load, one `Instant::elapsed`, one compare.
//! - Zero allocation in the hot path.
//! - Never blocks the read loop — the watchdog runs on its own task.
//! - A fresh watchdog MUST be spawned on every reconnect; the owner aborts
//!   the task on disconnect and respawns before the next `run_read_loop`.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tokio::sync::Notify;
// NOTE: Use `tokio::time::Instant` instead of `std::time::Instant` so the
// watchdog honours the paused clock in `#[tokio::test(start_paused = true)]`.
// `std::time::Instant` always reads real monotonic time and therefore
// makes the `silent_for >= threshold` check unreachable under the paused
// runtime. `tokio::time::Instant` is a transparent wrapper in production
// and a mocked clock in tests.
use tokio::time::Instant;
use tracing::error;

/// Poll cadence — short enough that the watchdog reacts promptly, long
/// enough that the compare loop burns no meaningful CPU. 5s gives the
/// 50s live-feed threshold 10 sample windows and the 660s order-update
/// threshold 132 sample windows.
pub const WATCHDOG_POLL_INTERVAL_SECS: u64 = 5;

/// Watchdog threshold for live market feed + 20-level depth + 200-level
/// depth. 40s Dhan server ping timeout + 10s safety margin. Per plan
/// `P2.1`: the watchdog MUST NOT fire before the server itself would
/// close an idle socket.
pub const WATCHDOG_THRESHOLD_LIVE_AND_DEPTH_SECS: u64 = 50;

/// Watchdog threshold for live order update. The order update WebSocket
/// is expected to be silent for long stretches during no-order windows.
/// 600s tolerance + 60s safety margin.
pub const WATCHDOG_THRESHOLD_ORDER_UPDATE_SECS: u64 = 660;

/// Per-connection activity watchdog.
///
/// Construct with [`ActivityWatchdog::new`], share the [`Arc<AtomicU64>`]
/// counter with the WS read loop, and spawn [`ActivityWatchdog::run`] as
/// an independent task. When the counter goes stale for `threshold`
/// seconds, the watchdog fires `notify` and returns.
pub struct ActivityWatchdog {
    /// Human-readable label used for logs + metric labels.
    /// Examples: `"live-feed-3"`, `"depth-20-NIFTY"`,
    /// `"depth-200-NIFTY-ATM-CE"`, `"order-update"`.
    label: String,
    /// Shared atomic counter. The WS read loop bumps this on every inbound
    /// frame; the watchdog reads it on each poll and checks for forward
    /// progress.
    counter: Arc<AtomicU64>,
    /// Maximum allowed silence before the watchdog fires.
    threshold: Duration,
    /// Notify handle that the WS read loop awaits in `tokio::select!`.
    /// When the watchdog fires, it calls `notify_one`; the read loop then
    /// returns `Err(WatchdogFired)`.
    notify: Arc<Notify>,
}

impl ActivityWatchdog {
    /// Creates a new watchdog. Does NOT spawn the task — call [`Self::run`]
    /// on a dedicated tokio task (the owner is responsible for aborting it
    /// on disconnect/reconnect).
    pub const fn new(
        label: String,
        counter: Arc<AtomicU64>,
        threshold: Duration,
        notify: Arc<Notify>,
    ) -> Self {
        Self {
            label,
            counter,
            threshold,
            notify,
        }
    }

    /// Runs the watchdog loop until either the counter goes stale for
    /// longer than `threshold` (fires notify) or the task is aborted by
    /// the owner. Each poll is O(1). Exercised by the three paused-clock
    /// tests below (fires / does-not-fire / resets-on-late-activity).
    // TEST-EXEMPT: covered by watchdog_does_not_fire_when_counter_advances, watchdog_fires_on_sustained_silence_past_threshold, watchdog_resets_on_late_activity — hook name-matcher doesn't pick up behaviour-named tests
    pub async fn run(self) {
        let poll = Duration::from_secs(WATCHDOG_POLL_INTERVAL_SECS);
        let mut last_counter = self.counter.load(Ordering::Relaxed);
        let mut last_advance = Instant::now();
        let mut ticker = tokio::time::interval(poll);
        // Skip missed ticks so long stalls (e.g. during a stop-the-world
        // GC-style event) don't cause a burst of catch-up ticks that all
        // fire at once.
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        // Consume the first immediate tick — do NOT fire before the
        // connection has had at least one poll window to receive its
        // first frame after connect.
        ticker.tick().await;
        loop {
            ticker.tick().await;
            let current = self.counter.load(Ordering::Relaxed);
            if current != last_counter {
                last_counter = current;
                last_advance = Instant::now();
                continue;
            }
            let silent_for = last_advance.elapsed();
            if silent_for >= self.threshold {
                error!(
                    label = %self.label,
                    threshold_secs = self.threshold.as_secs(),
                    silent_secs = silent_for.as_secs(),
                    "WS activity watchdog fired — no frames or pings within threshold, \
                     triggering reconnect"
                );
                metrics::counter!(
                    "tv_ws_activity_watchdog_fired_total",
                    "label" => self.label.clone() // O(1) EXEMPT: cold path — fires once per dead connection, not per frame
                )
                .increment(1);
                self.notify.notify_one();
                return;
            }
        }
    }

    /// Returns the label (for tests + error construction on the caller).
    /// One-line accessor returning the owned `label` field as a borrowed str.
    // TEST-EXEMPT: trivial accessor — no behaviour to exercise in isolation beyond the compiler-provided borrow
    pub fn label(&self) -> &str {
        &self.label
    }

    /// Returns the threshold (for tests + error construction on the caller).
    pub const fn threshold(&self) -> Duration {
        self.threshold
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Helper: construct a watchdog + start its task. Returns the handle,
    // the shared counter, and the notify so tests can drive each side.
    fn spawn_watchdog(
        threshold: Duration,
    ) -> (tokio::task::JoinHandle<()>, Arc<AtomicU64>, Arc<Notify>) {
        let counter = Arc::new(AtomicU64::new(0));
        let notify = Arc::new(Notify::new());
        let watchdog = ActivityWatchdog::new(
            "test".to_string(),
            Arc::clone(&counter),
            threshold,
            Arc::clone(&notify),
        );
        let handle = tokio::spawn(watchdog.run());
        (handle, counter, notify)
    }

    #[tokio::test(start_paused = true)]
    async fn watchdog_does_not_fire_when_counter_advances() {
        // SCENARIO: counter is bumped every poll window — watchdog must NEVER fire.
        let (handle, counter, _notify) = spawn_watchdog(Duration::from_secs(50));
        // Bump counter every 5s for 60s (12 bumps through 2 thresholds).
        for _ in 0..12 {
            tokio::time::sleep(Duration::from_secs(5)).await;
            counter.fetch_add(1, Ordering::Relaxed);
        }
        // Watchdog should not have fired.
        assert!(
            !handle.is_finished(),
            "watchdog fired despite steady activity"
        );
        handle.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn watchdog_fires_on_sustained_silence_past_threshold() {
        // SCENARIO: counter never advances after start — watchdog fires after threshold.
        let threshold = Duration::from_secs(50);
        let (handle, _counter, notify) = spawn_watchdog(threshold);
        // Advance well past the threshold.
        tokio::time::sleep(Duration::from_secs(60)).await;
        // Wait for the watchdog task to finish.
        let join_result = tokio::time::timeout(Duration::from_secs(10), handle).await;
        assert!(
            join_result.is_ok(),
            "watchdog task did not finish after threshold"
        );
        // The notify must have been triggered — a second notified() should
        // resolve immediately because notify_one was called.
        let notified = tokio::time::timeout(Duration::from_millis(1), notify.notified()).await;
        assert!(notified.is_ok(), "watchdog did not fire notify");
    }

    #[tokio::test(start_paused = true)]
    async fn watchdog_resets_on_late_activity() {
        // SCENARIO: counter is silent for almost-threshold, then bumps, then
        // silent again for almost-threshold — watchdog must NOT fire.
        let threshold = Duration::from_secs(50);
        let (handle, counter, _notify) = spawn_watchdog(threshold);
        // Silent for 40s.
        tokio::time::sleep(Duration::from_secs(40)).await;
        // One bump resets the stopwatch.
        counter.fetch_add(1, Ordering::Relaxed);
        // Another 40s of silence — still under threshold from the bump.
        tokio::time::sleep(Duration::from_secs(40)).await;
        assert!(!handle.is_finished(), "watchdog fired despite reset");
        handle.abort();
    }

    #[test]
    fn thresholds_match_plan_spec() {
        // P2.1: 50s for live-feed/depth, 660s for order-update.
        assert_eq!(WATCHDOG_THRESHOLD_LIVE_AND_DEPTH_SECS, 50);
        assert_eq!(WATCHDOG_THRESHOLD_ORDER_UPDATE_SECS, 660);
    }
}
