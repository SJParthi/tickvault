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

use std::sync::atomic::{AtomicBool, Ordering};

use tickvault_common::config::FeedsConfig;

/// The market-data feeds that can be reported / toggled.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Feed {
    /// Dhan (feed #1) — the primary trading feed. Status-only at runtime.
    Dhan,
    /// Groww (feed #2) — runtime-toggleable (pause/resume live).
    Groww,
}

impl Feed {
    /// The stable wire-format label (`"dhan"` / `"groww"`).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Dhan => "dhan",
            Self::Groww => "groww",
        }
    }

    /// Parse a feed name from the URL path. Returns `None` for anything that is
    /// not exactly a known feed label (case-sensitive — the API is machine-facing).
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "dhan" => Some(Self::Dhan),
            "groww" => Some(Self::Groww),
            _ => None,
        }
    }

    /// Whether this feed may be toggled at runtime. Only Groww in slice 1 —
    /// disabling the primary Dhan feed mid-session is unsafe (config+restart only).
    #[must_use]
    pub const fn is_runtime_toggleable(self) -> bool {
        matches!(self, Self::Groww)
    }
}

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
}

/// Lock-free per-feed runtime enable/disable flags. Seeded from `config.feeds` at
/// boot; flipped live by the feed-toggle API. One shared `Arc` instance binds the
/// API and the feed lanes together.
#[derive(Debug)]
pub struct FeedRuntimeState {
    dhan: AtomicBool,
    groww: AtomicBool,
    /// Set once by the boot wiring when the Groww bridge task is actually
    /// spawned (i.e. `groww_enabled` was true at boot). Read by the API to tell
    /// the operator honestly whether a runtime toggle will take effect.
    groww_lane_running: AtomicBool,
}

impl FeedRuntimeState {
    /// Build from the boot config — `dhan_enabled` / `groww_enabled` seed the
    /// atomics. This is the ONLY constructor production uses; the runtime API
    /// only ever flips the atomics afterward, never re-seeds from config.
    #[must_use]
    pub fn from_config(feeds: &FeedsConfig) -> Self {
        Self {
            dhan: AtomicBool::new(feeds.dhan_enabled),
            groww: AtomicBool::new(feeds.groww_enabled),
            // The lane is not running until the boot wiring spawns it.
            groww_lane_running: AtomicBool::new(false),
        }
    }

    /// Called once by the boot wiring when the Groww bridge task is spawned, so
    /// the API can report honestly whether a runtime toggle will take effect.
    pub fn mark_groww_lane_running(&self) {
        self.groww_lane_running.store(true, Ordering::Relaxed);
    }

    /// Whether the Groww bridge task was spawned this process (see [`FeedStatus`]).
    #[must_use]
    pub fn is_groww_lane_running(&self) -> bool {
        self.groww_lane_running.load(Ordering::Relaxed)
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
    fn test_only_groww_is_runtime_toggleable() {
        assert!(Feed::Groww.is_runtime_toggleable());
        assert!(
            !Feed::Dhan.is_runtime_toggleable(),
            "Dhan must NOT be runtime-disable-able (config+restart only)"
        );
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
            }
        );
        state.set_enabled(Feed::Groww, true);
        assert_eq!(
            state.snapshot(),
            FeedStatus {
                dhan_enabled: true,
                groww_enabled: true,
                groww_lane_running: false,
            }
        );
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
}
