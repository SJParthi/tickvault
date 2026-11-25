//! The depth on/off switch (owner, 2026-10-21: "switch off both depth 20 and
//! depth 200 dude okay?"; `websocket-connection-scope-lock.md`, "2026-10-21 —
//! DEPTH-20 AND DEPTH-200 ARE SWITCHED OFF").
//!
//! `[dhan_universe] depth_sockets_enabled` is read once at boot and stored
//! here, so the depth attach can read it without a new parameter. While it is
//! false the attach treats both depth halves as done from the start: no
//! depth-20 or depth-200 socket is attached or dialled, and the main-feed half
//! of the same attach runs unchanged.
//!
//! The steering loop never starts then, and its heartbeat
//! (`tv_depth_rebalance_age_secs`) is what the in-session
//! `depth-steering-stalled` alarm reads, with missing data treated as a
//! breach. [`spawn_switched_off_heartbeat`] publishes 0 for it instead: there
//! is no steering to stall. It also publishes `tv_depth_sockets_enabled`
//! (local `/metrics` only) so a reader can tell "switched off" from "steering
//! fine".
//!
//! Cost: one relaxed atomic load per read (a few per attach loop pass, cold);
//! the heartbeat writes two gauges every 30 s. Never on the frame path.

use std::sync::atomic::{AtomicBool, Ordering};

use tracing::info;

/// Gauge: 1 when the depth sockets are switched on, 0 when off.
pub const DEPTH_SOCKETS_ENABLED_GAUGE: &str = "tv_depth_sockets_enabled";

/// `true` until boot stores the config value, so code that never calls
/// [`set_depth_sockets_enabled`] (tests, harnesses) keeps depth as it was.
static DEPTH_SOCKETS_ENABLED: AtomicBool = AtomicBool::new(true);

/// Stores the boot value of `[dhan_universe] depth_sockets_enabled`. O(1).
pub fn set_depth_sockets_enabled(enabled: bool) {
    DEPTH_SOCKETS_ENABLED.store(enabled, Ordering::Release);
}

/// Whether the depth sockets may be attached and dialled. O(1).
#[must_use]
pub fn depth_sockets_enabled() -> bool {
    DEPTH_SOCKETS_ENABLED.load(Ordering::Acquire)
}

/// While depth is switched off: publishes the steering heartbeat at 0 and the
/// switch gauge at 0 every [`crate::depth_rebalance::REBALANCE_HEARTBEAT_SECS`],
/// for the life of the process. Call once at boot, only when switched off.
// TEST-EXEMPT: an endless gauge loop; the switch it reports is unit-tested (test_set_depth_sockets_enabled_round_trips).
pub fn spawn_switched_off_heartbeat() -> tokio::task::JoinHandle<()> {
    info!(
        "depth-20 and depth-200 are switched off ([dhan_universe] \
         depth_sockets_enabled = false): no depth socket is dialled today"
    );
    tokio::spawn(async move {
        loop {
            metrics::gauge!(DEPTH_SOCKETS_ENABLED_GAUGE).set(0.0);
            metrics::gauge!(crate::depth_rebalance::REBALANCE_AGE_SECS).set(0.0);
            tokio::time::sleep(std::time::Duration::from_secs(
                crate::depth_rebalance::REBALANCE_HEARTBEAT_SECS,
            ))
            .await;
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_set_depth_sockets_enabled_round_trips() {
        // The one test that touches this global; it ends where it started.
        assert!(depth_sockets_enabled(), "unset reads as on (old behaviour)");
        set_depth_sockets_enabled(false);
        assert!(!depth_sockets_enabled());
        set_depth_sockets_enabled(true);
        assert!(depth_sockets_enabled());
    }

    #[test]
    fn the_attach_and_main_read_the_switch() {
        let stack = include_str!("dhan_feed_stack.rs");
        assert!(
            stack.contains("let mut depth_ready = !crate::depth_switch::depth_sockets_enabled();")
        );
        assert!(
            stack.contains("let mut depth_done = !crate::depth_switch::depth_sockets_enabled();")
        );
        let main = include_str!("main.rs");
        assert!(main.contains("tickvault_app::depth_switch::set_depth_sockets_enabled("));
        assert!(main.contains("tickvault_app::depth_switch::spawn_switched_off_heartbeat()"));
    }
}
