//! Runtime per-feed enable/disable state (feed-toggle API, operator
//! AskUserQuestion 2026-06-19: "Feed-toggle API endpoint").
//!
//! A single [`FeedRuntimeState`] is built at boot from `config.feeds` and shared
//! (as one `Arc`) into BOTH the Axum API (`AppState`) and the Groww bridge, so an
//! authenticated `POST /api/feeds/{feed}` toggle is seen LIVE by the feed lane —
//! pause/resume a feed mid-session with no restart. The flags are lock-free
//! `AtomicBool`s: O(1), zero-alloc reads on the lane's hot loop.
//!
//! **Honest envelope (slice 1):** only Groww is runtime-toggleable. Disabling the
//! primary Dhan trading feed mid-session is unsafe, so the endpoint rejects
//! `POST /api/feeds/dhan` (Dhan stays config+restart, per Step C). Dhan's flag is
//! still reported by `GET /api/feeds` for visibility.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tickvault_common::config::FeedsConfig;
// SP1: `Feed` now lives in `common` (the shared layer every crate depends on),
// so writers/aggregators/parity can use the SAME enum + label fn. Re-exported
// here so existing `api::feed_state::Feed` call sites keep compiling unchanged.
pub use tickvault_common::feed::Feed;

/// A point-in-time snapshot of every feed's enabled state — the `GET /api/feeds`
/// response payload (serialised by the handler).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FeedStatus {
    pub dhan_enabled: bool,
    pub groww_enabled: bool,
    /// Whether the Groww lane (the bridge task) was actually SPAWNED this process.
    /// The bridge only spawns when `groww_enabled` is true AT BOOT. So if you
    /// flip `groww_enabled` ON via the API but this is `false`, the toggle is
    /// recorded but NO lane acts on it — set `groww_enabled=true` in config and
    /// RESTART to start the lane (the API toggle is pause/resume for a *running*
    /// lane). Exposing this avoids a misleading "enabled but nothing happens".
    pub groww_lane_running: bool,
    /// PR-E: whether the Dhan main-feed lane was spawned this process (Dhan
    /// enabled at boot). Mirrors `groww_lane_running` for honest reporting.
    pub dhan_lane_running: bool,
    /// PR-E: whether DISABLING Dhan at runtime is currently permitted. `true`
    /// in the no-orders data-pull phase (`dry_run`); `false` once live trading
    /// is on, so the toggle can never blind the system mid-trade.
    pub dhan_disable_allowed: bool,
}

/// Lock-free per-feed runtime enable/disable flags. Seeded from `config.feeds` at
/// boot; flipped live by the feed-toggle API. One shared `Arc` instance binds the
/// API and the feed lanes together. PR-E: the `dhan`/`groww` flags are
/// `Arc<AtomicBool>` so the SAME atomic can be handed to the `core` Dhan
/// WebSocket loop (which cannot depend on this crate) via [`Self::dhan_flag`].
#[derive(Debug)]
pub struct FeedRuntimeState {
    dhan: Arc<AtomicBool>,
    groww: Arc<AtomicBool>,
    /// Set once by the boot wiring when the Groww bridge task is actually
    /// spawned (i.e. `groww_enabled` was true at boot). Read by the API to tell
    /// the operator honestly whether a runtime toggle will take effect.
    groww_lane_running: AtomicBool,
    /// PR-E: set once by the boot wiring when the Dhan main-feed pool is spawned.
    dhan_lane_running: AtomicBool,
    /// PR-E: gate on the Dhan *disable* direction (orders-live safety). Seeded
    /// from `dry_run` at boot; defaults `true` (no-orders phase).
    dhan_disable_allowed: AtomicBool,
}

impl FeedRuntimeState {
    /// Build from the boot config — `dhan_enabled` / `groww_enabled` seed the
    /// atomics. This is the ONLY constructor production uses; the runtime API
    /// only ever flips the atomics afterward, never re-seeds from config.
    /// PR-E: `dhan_disable_allowed` defaults `true`; the boot wiring narrows it
    /// to `dry_run` via [`Self::set_dhan_disable_allowed`].
    #[must_use]
    pub fn from_config(feeds: &FeedsConfig) -> Self {
        Self {
            dhan: Arc::new(AtomicBool::new(feeds.dhan_enabled)),
            groww: Arc::new(AtomicBool::new(feeds.groww_enabled)),
            // The lane is not running until the boot wiring spawns it.
            groww_lane_running: AtomicBool::new(false),
            dhan_lane_running: AtomicBool::new(false),
            dhan_disable_allowed: AtomicBool::new(true),
        }
    }

    /// PR-E: a clone of the shared Dhan enable atomic, for the `core` Dhan
    /// WebSocket pool/connection (`with_feed_enable_flag`). The SAME atomic the
    /// API toggle flips — so a webpage toggle is observed live by the feed loop.
    #[must_use]
    pub fn dhan_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.dhan)
    }

    /// PR-E: boot wiring marks the Dhan main-feed lane as spawned.
    pub fn mark_dhan_lane_running(&self) {
        self.dhan_lane_running.store(true, Ordering::Relaxed);
    }

    /// PR-E: whether the Dhan lane is running this process.
    #[must_use]
    pub fn is_dhan_lane_running(&self) -> bool {
        self.dhan_lane_running.load(Ordering::Relaxed)
    }

    /// PR-2: set the Dhan lane-running flag BOTH ways. `mark_dhan_lane_running`
    /// (the inline boot's one-way `true`) stays; this is the two-way setter the
    /// dormant Dhan activation watcher (`dhan_activation`) uses to keep the feed
    /// page honest across runtime toggles — `true` once the lane is (re)activated,
    /// `false` on a runtime disable — so the page never shows a stale "running"
    /// after a teardown (mirrors `set_groww_lane_running`).
    pub fn set_dhan_lane_running(&self, running: bool) {
        // Relaxed: UI-status-only flag; no ordering dependency with other shared state.
        self.dhan_lane_running.store(running, Ordering::Relaxed);
    }

    /// PR-E: narrow the Dhan-disable safety gate (boot wires this to `dry_run`).
    pub fn set_dhan_disable_allowed(&self, allowed: bool) {
        self.dhan_disable_allowed.store(allowed, Ordering::Relaxed);
    }

    /// PR-E: may the operator DISABLE Dhan right now? `false` once live trading
    /// is on — the handler rejects a Dhan-off toggle so the feed can't be killed
    /// mid-trade. Enabling Dhan is always allowed.
    #[must_use]
    pub fn can_disable_dhan(&self) -> bool {
        self.dhan_disable_allowed.load(Ordering::Relaxed)
    }

    /// Called once by the boot wiring when the Groww bridge task is spawned, so
    /// the API can report honestly whether a runtime toggle will take effect.
    pub fn mark_groww_lane_running(&self) {
        self.set_groww_lane_running(true);
    }

    /// Set the Groww lane-running flag both ways. The activation watcher
    /// (`groww_activation`) sets it `true` on the enable rising-edge once the
    /// tables + watch-list are ready, and `false` on the disable falling-edge —
    /// so the feed page reports "running" iff the lane is actually live, never a
    /// false-OK and never a stale DEGRADED after a runtime cold-start.
    pub fn set_groww_lane_running(&self, running: bool) {
        // Relaxed: UI-status-only flag; no ordering dependency with other shared state.
        self.groww_lane_running.store(running, Ordering::Relaxed);
    }

    /// Whether the Groww bridge task was spawned this process (see [`FeedStatus`]).
    #[must_use]
    pub fn is_groww_lane_running(&self) -> bool {
        self.groww_lane_running.load(Ordering::Relaxed)
    }

    /// Generic per-feed lane-running accessor (Feed::ALL-driven — the health
    /// endpoint iterates feeds). Exhaustive match → a new feed forces an arm here.
    #[must_use]
    pub fn lane_running(&self, feed: Feed) -> bool {
        match feed {
            Feed::Dhan => self.is_dhan_lane_running(),
            Feed::Groww => self.is_groww_lane_running(),
        }
    }

    /// O(1) lock-free read — does this feed's lane currently run? The Groww
    /// bridge calls this every loop iteration.
    #[must_use]
    pub fn is_enabled(&self, feed: Feed) -> bool {
        match feed {
            Feed::Dhan => self.dhan.load(Ordering::Relaxed),
            Feed::Groww => self.groww.load(Ordering::Relaxed),
        }
    }

    /// Flip a feed's runtime flag. Returns the new value. Relaxed ordering — a
    /// toggle is advisory (the lane observes it on its next loop), not a barrier.
    pub fn set_enabled(&self, feed: Feed, enabled: bool) -> bool {
        match feed {
            Feed::Dhan => self.dhan.store(enabled, Ordering::Relaxed),
            Feed::Groww => self.groww.store(enabled, Ordering::Relaxed),
        }
        enabled
    }

    /// Snapshot every feed's state for the status endpoint.
    #[must_use]
    pub fn snapshot(&self) -> FeedStatus {
        FeedStatus {
            dhan_enabled: self.is_enabled(Feed::Dhan),
            groww_enabled: self.is_enabled(Feed::Groww),
            groww_lane_running: self.is_groww_lane_running(),
            dhan_lane_running: self.is_dhan_lane_running(),
            dhan_disable_allowed: self.can_disable_dhan(),
        }
    }
}

impl Default for FeedRuntimeState {
    /// Default mirrors `FeedsConfig::default()` — Dhan ON, Groww OFF. Used by the
    /// 4-arg `AppState::new` (tests); production injects the config-seeded one via
    /// `AppState::new_with_feed_runtime`.
    fn default() -> Self {
        Self::from_config(&FeedsConfig::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_feed_as_str_and_parse_round_trip() {
        for feed in [Feed::Dhan, Feed::Groww] {
            assert_eq!(Feed::parse(feed.as_str()), Some(feed));
        }
        assert_eq!(Feed::parse("DHAN"), None, "parse is case-sensitive");
        assert_eq!(Feed::parse("groww_live"), None);
        assert_eq!(Feed::parse(""), None);
    }

    #[test]
    fn test_both_feeds_runtime_toggleable_after_pr_e() {
        // PR-E (2026-06-21): Dhan is now runtime-toggleable too (operator-
        // authorized); the Dhan-disable direction is safety-gated separately.
        assert!(Feed::Groww.is_runtime_toggleable());
        assert!(Feed::Dhan.is_runtime_toggleable());
    }

    #[test]
    fn test_from_config_seeds_atomics() {
        let feeds = FeedsConfig {
            dhan_enabled: true,
            groww_enabled: true,
        };
        let state = FeedRuntimeState::from_config(&feeds);
        assert!(state.is_enabled(Feed::Dhan));
        assert!(state.is_enabled(Feed::Groww));
    }

    #[test]
    fn test_default_is_dhan_on_groww_off() {
        let state = FeedRuntimeState::default();
        assert!(state.is_enabled(Feed::Dhan));
        assert!(!state.is_enabled(Feed::Groww));
    }

    #[test]
    fn test_is_enabled_reads_the_current_flag() {
        let state = FeedRuntimeState::default();
        assert!(state.is_enabled(Feed::Dhan));
        assert!(!state.is_enabled(Feed::Groww));
        state.set_enabled(Feed::Groww, true);
        assert!(state.is_enabled(Feed::Groww));
    }

    #[test]
    fn test_set_enabled_flips_and_returns_new_value() {
        let state = FeedRuntimeState::default();
        assert!(!state.is_enabled(Feed::Groww));
        assert!(state.set_enabled(Feed::Groww, true));
        assert!(state.is_enabled(Feed::Groww));
        assert!(!state.set_enabled(Feed::Groww, false));
        assert!(!state.is_enabled(Feed::Groww));
    }

    #[test]
    fn test_toggling_groww_does_not_touch_dhan() {
        let state = FeedRuntimeState::default();
        state.set_enabled(Feed::Groww, true);
        assert!(
            state.is_enabled(Feed::Dhan),
            "Dhan unchanged by Groww toggle"
        );
        state.set_enabled(Feed::Dhan, false);
        assert!(
            state.is_enabled(Feed::Groww),
            "Groww unchanged by Dhan toggle"
        );
    }

    #[test]
    fn test_snapshot_reflects_current_state() {
        let state = FeedRuntimeState::default();
        assert_eq!(
            state.snapshot(),
            FeedStatus {
                dhan_enabled: true,
                groww_enabled: false,
                groww_lane_running: false,
                dhan_lane_running: false,
                dhan_disable_allowed: true,
            }
        );
        state.set_enabled(Feed::Groww, true);
        assert_eq!(
            state.snapshot(),
            FeedStatus {
                dhan_enabled: true,
                groww_enabled: true,
                groww_lane_running: false,
                dhan_lane_running: false,
                dhan_disable_allowed: true,
            }
        );
    }

    #[test]
    fn test_dhan_is_runtime_toggleable_after_pr_e() {
        assert!(Feed::Dhan.is_runtime_toggleable());
        assert!(Feed::Groww.is_runtime_toggleable());
    }

    #[test]
    fn test_dhan_flag_shares_the_same_atomic_the_toggle_flips() {
        // The flag handed to the core Dhan WS loop MUST be the very atomic the
        // API toggle flips — otherwise a webpage toggle would never reach the feed.
        let state = FeedRuntimeState::default();
        let flag = state.dhan_flag();
        assert!(flag.load(Ordering::Relaxed), "seeded enabled");
        state.set_enabled(Feed::Dhan, false);
        assert!(
            !flag.load(Ordering::Relaxed),
            "toggling Dhan off is observed on the shared flag"
        );
        state.set_enabled(Feed::Dhan, true);
        assert!(flag.load(Ordering::Relaxed), "re-enable observed too");
    }

    #[test]
    fn test_dhan_disable_safety_gate() {
        // tests set_dhan_disable_allowed + can_disable_dhan
        let state = FeedRuntimeState::default();
        assert!(state.can_disable_dhan(), "default no-orders phase: allowed");
        state.set_dhan_disable_allowed(false);
        assert!(!state.can_disable_dhan(), "live trading: disable refused");
    }

    #[test]
    fn test_dhan_lane_running_marker() {
        // tests mark_dhan_lane_running + is_dhan_lane_running
        let state = FeedRuntimeState::default();
        assert!(
            !state.is_dhan_lane_running(),
            "not marked until boot wiring"
        );
        state.mark_dhan_lane_running();
        assert!(
            state.is_dhan_lane_running(),
            "boot wiring marked it running"
        );
    }

    #[test]
    fn test_set_dhan_lane_running_toggles_both_ways() {
        // PR-2: the Dhan activation watcher sets the flag true on the enable
        // (re)activation and false on a runtime disable. Both directions must
        // round-trip — the disable path is what clears a stale "running" so the
        // feed page never lies after a teardown (mirrors set_groww_lane_running).
        let state = FeedRuntimeState::default();
        // Default mirrors prod: NOT running until the boot wiring / watcher marks it.
        assert!(!state.is_dhan_lane_running(), "default not running");
        state.set_dhan_lane_running(true);
        assert!(state.is_dhan_lane_running(), "set true => running");
        assert!(state.snapshot().dhan_lane_running);
        state.set_dhan_lane_running(false);
        assert!(!state.is_dhan_lane_running(), "set false => stopped");
        assert!(!state.snapshot().dhan_lane_running);
    }

    #[test]
    fn test_mark_groww_lane_running_then_is_groww_lane_running_true() {
        // Honest signal (3-agent hostile review): the lane is NOT running until
        // the boot wiring spawns it. A runtime enable on a non-running lane is
        // recorded but has no effect — the API exposes this so it never lies.
        let state = FeedRuntimeState::default();
        assert!(!state.is_groww_lane_running(), "not running until marked");
        assert!(!state.snapshot().groww_lane_running);
        state.mark_groww_lane_running();
        assert!(state.is_groww_lane_running());
        assert!(state.snapshot().groww_lane_running);
    }

    #[test]
    fn test_set_groww_lane_running_toggles_both_ways() {
        // The activation watcher sets the flag true on the enable rising edge
        // (after the watch-list builds) and false on the disable falling edge.
        // Both directions must round-trip — the disable path is what clears a
        // stale "running" so the feed page never lies after a teardown.
        let state = FeedRuntimeState::default();
        assert!(!state.is_groww_lane_running(), "default not running");
        state.set_groww_lane_running(true);
        assert!(state.is_groww_lane_running(), "set true => running");
        assert!(state.snapshot().groww_lane_running);
        state.set_groww_lane_running(false);
        assert!(!state.is_groww_lane_running(), "set false => stopped");
        assert!(!state.snapshot().groww_lane_running);
    }

    #[test]
    fn test_lane_running_generic_accessor_matches_per_feed() {
        // The Feed::ALL-driven generic accessor must agree with the per-feed ones.
        let state = FeedRuntimeState::default();
        assert_eq!(state.lane_running(Feed::Dhan), state.is_dhan_lane_running());
        assert_eq!(
            state.lane_running(Feed::Groww),
            state.is_groww_lane_running()
        );
        state.mark_dhan_lane_running();
        assert!(state.lane_running(Feed::Dhan));
        assert!(!state.lane_running(Feed::Groww));
    }
}
