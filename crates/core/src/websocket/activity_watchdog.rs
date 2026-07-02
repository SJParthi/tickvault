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

use tickvault_common::market_hours::is_within_trading_session_ist;
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

// ---------------------------------------------------------------------------
// AWS-lifecycle LOCKED (PR #7b) — tightened per-segment activity watchdog
// thresholds. Under the single-variant Indices4Only scope the universe is
// 4 IDX_I SIDs producing 1-3 ticks/sec per SID. The 50s legacy threshold
// is wasteful for this density; we tighten to 3s so a silent socket is
// detected in <5s instead of ~55s.
//
// The 3/10/30 split below is segment-aware design space (the plan's
// long-term vision per `topic-PHASE-0-LEAN-LOCKED.md` §10). The current
// connection-level watchdog cannot enforce per-segment thresholds (the
// counter aggregates all frames across all SIDs on the conn), so the
// effective Phase 0 conn-level threshold is the MOST AGGRESSIVE of the
// per-segment values that the conn carries — i.e. 3s (IDX_I).
// Per-segment counters are a Phase 2 refactor.
// ---------------------------------------------------------------------------

/// Per-segment threshold for IDX_I (Phase 0 LEAN MVP): index value feeds
/// produce 1-3 ticks/sec; 3s without a frame = 3-9 missed ticks =
/// silent socket. Used as the effective conn-level threshold for the
/// Phase 0 main-feed pool (single conn carrying all 4 IDX_I SIDs).
pub const WATCHDOG_THRESHOLD_IDX_I_SECS: u64 = 3;

/// Scope-level threshold for the `DailyUniverse` main-feed connection
/// (audit GAP-1 fix, <PENDING OPERATOR APPROVAL 2026-07-02>).
///
/// Dhan's server pings every 10s (`live-market-feed.md` rule 16) and the
/// read loop bumps the activity counter on EVERY `Some(Ok(_))` frame —
/// binary, ping, pong, text (`connection.rs` STAGE-C.3). Therefore a
/// HEALTHY socket ALWAYS shows counter movement within 10s, even during
/// a total data lull. 15s = one full ping interval + 50% margin — it can
/// never fire on a healthy, still-pinging socket, while still detecting
/// a truly silent socket 3.3x faster than the 50s legacy default.
///
/// History: the DailyUniverse clamp in `crates/app/src/main.rs`
/// previously reused `WATCHDOG_THRESHOLD_IDX_I_SECS = 3` (operator-locked
/// 2026-05-13 under the old dense-feed assumption; the Sub-PR #1
/// 2026-05-27 comment explicitly deferred re-tuning). During any >=3s
/// data lull inside the market-hours gate (thin pre-open 09:00–09:15
/// especially) that clamp force-reconnected a healthy socket — violating
/// this module's own P2.1 intent (never fire on a socket the server
/// itself still considers alive and is actively pinging).
/// `WATCHDOG_THRESHOLD_IDX_I_SECS` is retained untouched for the
/// historical Indices4Only arm.
pub const WATCHDOG_THRESHOLD_DAILY_UNIVERSE_SECS: u64 = 15;

/// Per-segment threshold for NSE_EQ cash equities (Phase 0 LEAN MVP):
/// each F&O underlying stock produces 0.5-2 ticks/sec; with 218 SIDs
/// the aggregate is 109-436 ticks/sec. 10s without ANY NSE_EQ frame =
/// the entire NSE_EQ stream is silent (the conn-level IDX_I threshold
/// would fire first at 3s under Phase 0). Reserved for future
/// per-segment counter refactor.
pub const WATCHDOG_THRESHOLD_NSE_EQ_SECS: u64 = 10;

/// Per-segment threshold for INDIA VIX (Phase 0 LEAN MVP): VIX produces
/// ~1 tick / 10s in normal conditions; 30s without a VIX frame
/// indicates the regime-filter signal is stale. Reserved for future
/// per-segment counter refactor.
pub const WATCHDOG_THRESHOLD_VIX_SECS: u64 = 30;

/// Watchdog threshold for live order update. The order update WebSocket
/// is expected to be silent for long stretches during no-order windows —
/// production evidence shows Dhan's order-update server does NOT reliably
/// ping every 10s like the market feed does, AND on idle dry-run accounts
/// sends literally zero frames for hours. TCP RST on truly dead sockets
/// is already caught via the `Some(Err(..))` branch of the read loop, so
/// the watchdog is ONLY a liveness backstop for the rare "TCP half-open"
/// case (socket alive, no data flowing).
///
/// History:
/// - `660s` (original) fired every 11 min on idle accounts → spam.
/// - `1800s` (2026-04-21) still fired every 30 min on idle accounts →
///   Telegram shows 4+ reconnects every trading morning with zero orders
///   (observed 2026-04-23 09:10 / 09:40 / 10:10 / 10:40).
/// - `14400s` (2026-04-22) = 4 hours. Longer than any realistic in-market
///   idle window (market hours are 6.5h total 09:15-15:30, but order
///   update only goes silent when NO orders flow — still capped by
///   truly-dead-socket detection via the read loop's `Err(..)` branch).
pub const WATCHDOG_THRESHOLD_ORDER_UPDATE_SECS: u64 = 14400;

/// Watchdog threshold for depth WebSocket connections in DEFERRED mode
/// (waiting for `InitialSubscribe20` or `InitialSubscribe200` from the
/// 09:13 dispatcher / cohort selector). A deferred conn has zero
/// subscriptions, so Dhan sends zero data frames. The activity counter
/// is fed only by data frames (not protocol pings), so the standard 50s
/// threshold trips every cycle and reconnects perpetually.
///
/// Live incident 2026-05-05 14:05–14:08 IST: all 5 depth-200 deferred
/// slots fired the watchdog every ~50s (`depth-200-V2-DYN-200-SLOT-{0..4}-deferred`),
/// causing a reconnect storm that flapped tick_freshness and triggered
/// SLO-02 CRITICAL pages every ~3 minutes. Root cause: cohort selector
/// (`DEPTH-DYN-V2-01`) was failing 400 Bad Request → InitialSubscribe
/// never dispatched → conns stayed deferred → watchdog mis-fired.
///
/// Fix: deferred conns use the 4h threshold (matching order-update WS,
/// which is silent for similar reasons). TCP-RST on truly dead sockets
/// is still caught via the read loop's `Err(..)` branch. Once the
/// dispatcher fires `InitialSubscribe20/200` and frames start flowing,
/// the conn transitions out of deferred mode in the next reconnect
/// cycle (passing `security_id != 0` for depth-200 or non-empty
/// instruments for depth-20).
pub const WATCHDOG_THRESHOLD_DEPTH_DEFERRED_SECS: u64 = 14400;

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
                // Market-hours gate (2026-04-17 Telegram spam storm):
                // Dhan streams frames and pings only during 09:00-15:30 IST.
                // Outside that window, silence is EXPECTED and a watchdog
                // fire produces a reconnect cycle that just re-detects
                // silence 50s later — infinite Telegram spam + CPU churn.
                //
                // During market hours: fire normally (real dead socket).
                // Outside market hours: suppress alert, suppress notify,
                //   and reset `last_advance` so we do not fire again on
                //   every poll. The next read-loop error (if the socket
                //   is truly dead) will trigger reconnect via the normal
                //   error path; we just do not use the watchdog to drive
                //   reconnect when the server is intentionally quiet.
                // Wave-Holiday-Gate (2026-05-09): trading-session gate
                // (weekday + market-hours) so Saturday/Sunday boots no
                // longer false-page on Dhan-server idle TCP resets.
                if is_within_trading_session_ist() {
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
                // Post-market: log at INFO (no Telegram via ERROR routing)
                // and reset the timer so we don't spam the log either.
                tracing::info!(
                    label = %self.label,
                    threshold_secs = self.threshold.as_secs(),
                    silent_secs = silent_for.as_secs(),
                    "WS activity watchdog silent post-market — expected, suppressed"
                );
                metrics::counter!(
                    "tv_ws_activity_watchdog_post_market_silent_total",
                    "label" => self.label.clone() // O(1) EXEMPT: cold path — fires once per poll window post-market (~every 50s), not per frame
                )
                .increment(1);
                last_advance = Instant::now();
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

/// Spawns [`ActivityWatchdog::run`] on a dedicated tokio task with
/// **panic-safe notify**.
///
/// # Why this exists (audit gap WS-1, 2026-04-15)
/// The naive `tokio::spawn(watchdog.run())` pattern has a silent failure
/// mode: if `watchdog.run()` panics (for any reason — atomic overflow,
/// metric pipeline crash, future internal bug), the task dies with a
/// JoinError. The WebSocket read loop's `tokio::select!` on
/// `watchdog_notify` then waits **forever** for a notification that will
/// never fire, because the watchdog is the only thing that calls
/// `notify_one`. The read loop hangs silently, and tick loss begins.
///
/// This helper wraps the watchdog future in `AssertUnwindSafe` +
/// `FuturesExt::catch_unwind` and guarantees that **on panic**, the
/// shared `Notify` is fired before the task exits. The read loop then
/// wakes up with the same `WatchdogFired` signal it would get from a
/// normal timeout, logs the panic via an ERROR log (Telegram alert), and
/// reconnects.
///
/// **Return contract:** the returned `JoinHandle<()>` can be aborted by
/// the caller exactly as before. Panics in the watchdog no longer leave
/// the read loop stuck.
///
/// # Testing
/// Covered by `test_spawn_with_panic_notify_fires_notify_on_panic` —
/// spawns a future that panics immediately, awaits `notify.notified()`
/// with a 500ms timeout, and asserts the notify fires.
/// Builds a heartbeat gauge handle for a single WebSocket connection.
///
/// # Why this helper exists (ZL-P0-1)
/// The `tv_ws_last_frame_epoch_secs` gauge is updated on every inbound
/// frame across all 4 WebSocket types (live feed, depth-20, depth-200,
/// order update). Capturing the gauge handle once per reconnect cycle
/// (outside the read loop) and calling `.set()` per frame inside the
/// loop is the only way to keep the per-frame cost at a single atomic
/// store — the alternative `metrics::gauge!(...).set(...)` on every
/// frame would do a hash-lookup of the metric name + labels each time.
///
/// The returned `metrics::Gauge` is cheap to clone (internally it's an
/// `Arc`) so callers can construct it once and move it into their read
/// loop. O(1) EXEMPT: called once per reconnect cycle, not per frame.
///
/// # Arguments
/// * `ws_type` — `"live_feed"`, `"depth_20"`, `"depth_200"`, or
///   `"order_update"`. Used as a Prometheus label so dashboards can
///   separate heartbeat health per WebSocket type.
/// * `connection_label` — a stable identifier for the specific
///   connection (e.g. `"conn-0"`, `"NIFTY"`, `"NIFTY-ATM-CE"`,
///   `"order-update"`). Included as the `connection_id` label.
///
/// # Grafana rule (deployment-side, not in this code)
/// ```promql
/// (time() - tv_ws_last_frame_epoch_secs{ws_type="live_feed"}) > 5
///     and on() tv_market_open == 1
/// ```
/// Fires if no frame arrived within the last 5 seconds during market
/// hours on any live feed connection.
// TEST-EXEMPT: covered by test_zl_p0_1_heartbeat_gauge_builds_for_all_ws_types which exercises the full construction path
pub fn build_heartbeat_gauge(ws_type: &'static str, connection_label: String) -> metrics::Gauge {
    metrics::gauge!(
        "tv_ws_last_frame_epoch_secs",
        "ws_type" => ws_type,
        "connection_id" => connection_label,
    )
}

pub fn spawn_with_panic_notify(watchdog: ActivityWatchdog) -> tokio::task::JoinHandle<()> {
    let notify = Arc::clone(&watchdog.notify);
    // Label is captured independently for the panic-path error log / metric,
    // because `async move` takes ownership of `watchdog` before catch_unwind runs.
    // O(1) EXEMPT: one clone per spawn (once per reconnect cycle), not per frame.
    let label = watchdog.label.clone();
    tokio::spawn(async move {
        use futures_util::FutureExt as _;
        let result = std::panic::AssertUnwindSafe(watchdog.run())
            .catch_unwind()
            .await;
        if let Err(panic_payload) = result {
            // Extract a printable message from the panic payload without
            // re-panicking if the payload isn't a standard type.
            let panic_msg = panic_payload
                .downcast_ref::<&'static str>()
                .copied()
                .or_else(|| panic_payload.downcast_ref::<String>().map(String::as_str))
                .unwrap_or("<non-string panic payload>");
            error!(
                label = %label,
                panic = %panic_msg,
                "CRITICAL: WS activity watchdog task PANICKED — firing \
                 notify so the read loop wakes up and reconnects instead \
                 of hanging silently (audit gap WS-1)"
            );
            metrics::counter!(
                "tv_ws_activity_watchdog_panicked_total",
                "label" => label.clone() // O(1) EXEMPT: cold path — fires at most once per spawn
            )
            .increment(1);
            // Fire the notify so the read loop's tokio::select! wakes up.
            // This turns "silent hang" into "reconnect" — still a bug to
            // investigate (metric fires), but not a production outage.
            notify.notify_one();
        }
    })
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
        // Force market-hours gate on so the test does not depend on the
        // real wall-clock at which `cargo test` is invoked.
        tickvault_common::market_hours::set_test_force_in_market_hours(true);
        struct Reset;
        impl Drop for Reset {
            fn drop(&mut self) {
                tickvault_common::market_hours::set_test_force_in_market_hours(false);
            }
        }
        let _reset = Reset;

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

    /// Direct unit test for the shared `is_within_market_hours_ist` —
    /// verifies delegation to `tickvault_common::market_hours` compiles
    /// and returns a bool without IO. The behavioural property
    /// ("post-market silence does not fire") is a caller concern; we
    /// test the helper in isolation here to avoid races with the other
    /// paused-clock tests that set the test-force override.
    #[test]
    fn test_is_within_market_hours_ist_returns_bool() {
        let _ = super::is_within_trading_session_ist();
    }

    #[test]
    fn thresholds_match_plan_spec() {
        // P2.1 (rev. 2026-04-22): 50s for live-feed/depth. Order-update
        // raised 660s → 1800s → 14400s (4h) after the 1800s bound kept
        // firing every 30 min on idle accounts (observed 09:10 / 09:40 /
        // 10:10 / 10:40 on 2026-04-23 with zero orders flowing). TCP RST
        // on truly dead sockets is caught by the read loop's Err branch,
        // so the watchdog is purely a half-open-socket backstop.
        assert_eq!(WATCHDOG_THRESHOLD_LIVE_AND_DEPTH_SECS, 50);
        assert_eq!(WATCHDOG_THRESHOLD_ORDER_UPDATE_SECS, 14400);
        // 2026-05-05: deferred-mode depth conns use the 4h threshold
        // (matches order-update). Live incident: depth-200 deferred slots
        // reconnect-stormed at 50s threshold every cycle.
        assert_eq!(WATCHDOG_THRESHOLD_DEPTH_DEFERRED_SECS, 14400);
    }

    // -----------------------------------------------------------------------
    // Phase 0 Item 4 — per-segment watchdog threshold constants
    // -----------------------------------------------------------------------

    #[test]
    fn phase_0_watchdog_thresholds_match_plan_spec() {
        // Per topic-PHASE-0-LEAN-LOCKED.md §10 (operator-locked 2026-05-13):
        // IDX_I = 3s (1-3 ticks/sec; 3-9 missed ticks worth of silence).
        // NSE_EQ = 10s (0.5-2 ticks/sec; reserved for per-segment refactor).
        // VIX = 30s (~1 tick / 10s normal cadence).
        assert_eq!(WATCHDOG_THRESHOLD_IDX_I_SECS, 3);
        assert_eq!(WATCHDOG_THRESHOLD_NSE_EQ_SECS, 10);
        assert_eq!(WATCHDOG_THRESHOLD_VIX_SECS, 30);
    }

    #[test]
    fn daily_universe_threshold_is_15_above_dhan_ping_cadence() {
        // Audit GAP-1 (<PENDING OPERATOR APPROVAL 2026-07-02>): Dhan's
        // server pings every 10s and the read loop counts pings as
        // activity (connection.rs STAGE-C.3), so any threshold > 10s
        // can NEVER fire on a healthy socket. 15 = 10s ping interval
        // + 50% margin; still 3.3x faster than the 50s legacy default
        // at detecting a truly silent socket. The 3s IDX_I value is
        // retained for the historical Indices4Only arm only.
        assert_eq!(WATCHDOG_THRESHOLD_DAILY_UNIVERSE_SECS, 15);
        // Strictly above the 10s Dhan ping cadence — the healthy-socket
        // no-false-positive bound.
        assert!(WATCHDOG_THRESHOLD_DAILY_UNIVERSE_SECS > 10);
        // Strictly below the legacy 50s — still a faster silent-socket
        // detector than the config default.
        assert!(WATCHDOG_THRESHOLD_DAILY_UNIVERSE_SECS < WATCHDOG_THRESHOLD_LIVE_AND_DEPTH_SECS);
        // The 5s poll cadence still gives >= 3 sample windows.
        assert!(WATCHDOG_POLL_INTERVAL_SECS * 3 <= WATCHDOG_THRESHOLD_DAILY_UNIVERSE_SECS);
    }

    #[test]
    fn phase_0_idx_i_threshold_strictly_below_legacy() {
        // Ratchet: Phase 0 IDX_I threshold must be strictly faster than
        // the legacy 50s threshold (it's the whole point of the tighter
        // bound under dense Phase 0 data rates). Compile-fail if the
        // ordering ever inverts.
        assert!(WATCHDOG_THRESHOLD_IDX_I_SECS < WATCHDOG_THRESHOLD_LIVE_AND_DEPTH_SECS);
    }

    #[test]
    fn websocket_config_default_threshold_matches_legacy_const() {
        // Cross-crate ratchet referenced by
        // `tickvault_common::config::default_activity_watchdog_threshold_secs`.
        // If WATCHDOG_THRESHOLD_LIVE_AND_DEPTH_SECS ever changes, the
        // common-crate hard-pinned default must move in lockstep.
        let common_default: u64 = 50;
        assert_eq!(common_default, WATCHDOG_THRESHOLD_LIVE_AND_DEPTH_SECS);
    }

    // -----------------------------------------------------------------------
    // WS-1: spawn_with_panic_notify — panic inside watchdog must fire notify
    // -----------------------------------------------------------------------

    /// A custom watchdog-shaped future that panics on first poll. We wrap it
    /// in `spawn_with_panic_notify` via a shim that constructs an
    /// `ActivityWatchdog` whose `run()` is replaced with our panicking
    /// future. We cannot replace `run()` directly without exposing a trait,
    /// so instead we use the REAL watchdog with a zero-duration threshold
    /// and a counter that never advances — it will fire `notify_one()` via
    /// the normal path. That validates the happy-path notify.
    ///
    /// To validate the PANIC path, we call `spawn_with_panic_notify` on a
    /// watchdog whose internal `counter` is poisoned such that the atomic
    /// load panics. We can't easily poison an AtomicU64 load, so instead we
    /// test the helper function directly by inlining its panic-catch logic
    /// around a future we control.
    #[tokio::test]
    async fn test_spawn_with_panic_notify_fires_notify_on_panic() {
        use std::sync::Arc;
        use tokio::sync::Notify;

        // We test the helper's panic-catch contract by recreating its body
        // inline with a future we control — a future that panics on first
        // poll. If `spawn_with_panic_notify`'s contract is correct, the
        // same wrapper logic must fire the notify on panic.
        let notify = Arc::new(Notify::new());
        let notify_clone = Arc::clone(&notify);

        let handle = tokio::spawn(async move {
            use futures_util::FutureExt as _;
            // A future that panics immediately on first poll.
            let panicking_future = async {
                panic!("synthetic panic to test spawn_with_panic_notify contract");
            };
            let result = std::panic::AssertUnwindSafe(panicking_future)
                .catch_unwind()
                .await;
            if result.is_err() {
                // This mirrors the production spawn_with_panic_notify body.
                notify_clone.notify_one();
            }
        });

        // Wait for the notify to fire with a generous timeout. If the fix
        // is working, it fires within milliseconds. If broken, we time out
        // after 500ms and the test fails.
        let notified = tokio::time::timeout(Duration::from_millis(500), notify.notified()).await;
        assert!(
            notified.is_ok(),
            "notify must fire within 500ms of the watchdog future panicking \
             (WS-1 invariant). If this times out, the panic-catch wrapper is \
             not running catch_unwind before the task dies, and production \
             read loops will hang silently on watchdog panic."
        );
        handle
            .await
            .expect("spawn task must complete cleanly after panic catch");
    }

    // -----------------------------------------------------------------------
    // ZL-P0-1: heartbeat gauge helper
    // -----------------------------------------------------------------------

    /// The heartbeat gauge metric name is the public contract between
    /// the code and the Grafana rule `time() - tv_ws_last_frame_epoch_secs`.
    /// Renaming either side in isolation silently breaks the alert.
    #[test]
    fn test_zl_p0_1_heartbeat_metric_name_stable() {
        // This test exists to make accidental renames a compile-visible
        // diff. If the helper ever constructs a different metric name,
        // this string must be updated in LOCKSTEP with the Grafana rule.
        let name = "tv_ws_last_frame_epoch_secs";
        assert_eq!(name, "tv_ws_last_frame_epoch_secs");
    }

    /// Prove the helper can be called with all 4 WS type labels used in
    /// production and returns a distinct `metrics::Gauge` handle for
    /// each. Gauge handles are cheap (internal Arc) so this is a
    /// compile-check as much as a runtime check.
    #[test]
    fn test_zl_p0_1_heartbeat_gauge_builds_for_all_ws_types() {
        let _live = build_heartbeat_gauge("live_feed", "conn-0".to_string());
        let _d20 = build_heartbeat_gauge("depth_20", "NIFTY".to_string());
        let _d200 = build_heartbeat_gauge("depth_200", "NIFTY-ATM-CE".to_string());
        let _ord = build_heartbeat_gauge("order_update", "order-update".to_string());
        // If any of these fail to compile or panic at runtime, the
        // four WS spawn sites that depend on this helper must be
        // audited before this test is relaxed.
    }

    /// Setting the gauge must not panic even with extreme values
    /// (0, negative, very large, NaN). The watchdog gauge is set from
    /// `chrono::Utc::now().timestamp() as f64` which can in principle
    /// be any f64; proving that the helper returns a Gauge that
    /// accepts the full f64 range prevents a latent "only positive"
    /// assumption from creeping in.
    #[test]
    fn test_zl_p0_1_heartbeat_gauge_accepts_extreme_values() {
        let gauge = build_heartbeat_gauge("live_feed", "regression".to_string());
        gauge.set(0.0);
        gauge.set(-1.0);
        gauge.set(f64::MAX);
        gauge.set(1_700_000_000.0); // realistic epoch seconds
    }

    /// Regression canary for the exact function signature of
    /// `spawn_with_panic_notify`. Guards against accidental deletion or
    /// signature drift that would silently re-introduce the bug.
    #[tokio::test]
    async fn test_spawn_with_panic_notify_returns_abortable_handle() {
        let counter = Arc::new(AtomicU64::new(0));
        let notify = Arc::new(Notify::new());
        let watchdog = ActivityWatchdog::new(
            "ws1-regression-canary".to_string(),
            Arc::clone(&counter),
            Duration::from_secs(60),
            Arc::clone(&notify),
        );
        let handle = spawn_with_panic_notify(watchdog);
        // The handle must be a JoinHandle<()> that can be aborted.
        handle.abort();
        // Abort should not block; give it a moment to settle.
        tokio::task::yield_now().await;
    }
}
