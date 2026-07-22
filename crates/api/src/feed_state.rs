//! Runtime per-feed enable/disable state (feed-toggle API, operator
//! AskUserQuestion 2026-06-19: "Feed-toggle API endpoint").
//!
//! A single [`FeedRuntimeState`] is built at boot from `config.feeds` and shared
//! (as one `Arc`) into BOTH the Axum API (`AppState`) and the Groww bridge, so an
//! authenticated `POST /api/feeds/{feed}` toggle is seen LIVE by the feed lane —
//! pause/resume a feed mid-session with no restart. The flags are lock-free
//! `AtomicBool`s: O(1), zero-alloc reads on the lane's hot loop.
//!
//! **Honest envelope (PR-E / PR-2):** BOTH Dhan and Groww are runtime-toggleable.
//! The Dhan *disable* direction is safety-gated — `POST /api/feeds/dhan {enabled:
//! false}` is permitted only while `can_disable_dhan()` is true (the no-orders
//! data-pull phase); once live trading is on it returns `409 CONFLICT` so the feed
//! can never be blinded mid-trade. Enabling Dhan is always allowed. For a Dhan
//! feed that was ENABLED at boot, a toggle is a true live pause/resume (PR-E
//! in-loop dormancy). For a Dhan feed that was OFF at boot, no main-feed pool was
//! spawned, so the `dhan_pool_present` sentinel stays false and a runtime enable
//! records the desired flag WITHOUT marking the lane running (no false-OK) — a
//! restart with `dhan_enabled=true` is the documented path to actually stream.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use tickvault_common::config::FeedsConfig;
// D2c (C4 follow-up): the live lane-owned TokenManager handle. Stored here so the
// PROCESS-level health gauges (self-test token-headroom + SLO token-freshness) can
// read the CURRENT lane manager instead of the dead boot-time global `OnceLock`
// after a runtime stop→re-start. `api` already depends on `core`.
use tickvault_core::auth::TokenManager;
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
///
/// `Debug` is hand-written (not derived) because the `live_lane_token_manager`
/// slot holds a `TokenManager`, which is intentionally NOT `Debug` (it owns
/// credentials). The manual impl renders that slot as a presence flag only —
/// never the manager's contents — so no secret can leak through a debug format.
pub struct FeedRuntimeState {
    dhan: Arc<AtomicBool>,
    groww: Arc<AtomicBool>,
    /// The IMMUTABLE RAW TOML value of `feeds.dhan_enabled` (Phase A,
    /// operator directive 2026-07-13) — captured BEFORE the feed-state
    /// overlay is applied (round-2 review MEDIUM, 2026-07-13). Distinct
    /// from the mutable `dhan` runtime atomic: with the Dhan live WS lane
    /// retired by config (`dhan_enabled = false` in base + production),
    /// the runtime cold-start supervisor refuses a lane start — so the
    /// `/api/feeds` handler must refuse a Dhan ENABLE at the API layer too
    /// (409), instead of flipping a flag that can never take effect (a
    /// Rule-11 false-OK: /feeds would show ON while nothing can start).
    /// "Retired" is a statement about the CONFIG, never about the last
    /// webpage toggle: seeding this from the POST-overlay effective value
    /// would let a persisted runtime-OFF overlay permanently 409-lock a
    /// config-ON boot with a misleading "config change + restart" message
    /// (breaking the PR-E disable→restart→re-enable round trip).
    dhan_config_enabled: bool,
    /// Set once by the boot wiring when the Groww bridge task is actually
    /// spawned (i.e. `groww_enabled` was true at boot). Read by the API to tell
    /// the operator honestly whether a runtime toggle will take effect.
    groww_lane_running: AtomicBool,
    /// PR-E: set once by the boot wiring when the Dhan main-feed pool is spawned.
    dhan_lane_running: AtomicBool,
    /// PR-E: gate on the Dhan *disable* direction (orders-live safety). Seeded
    /// from `dry_run` at boot; defaults `true` (no-orders phase).
    dhan_disable_allowed: AtomicBool,
    /// D2c (C4 follow-up): the CURRENTLY-live lane-owned `TokenManager`. SET when
    /// `start_dhan_lane` succeeds (overwriting any prior, dead manager) and CLEARED
    /// at every lane→`Off` transition. The two PROCESS-level health gauges
    /// (self-test token-headroom + SLO token-freshness) prefer THIS handle and
    /// fall back to the global `OnceLock` only when no lane is running (boot-OFF /
    /// Groww-only) — so after a runtime stop→re-start they read the NEW manager's
    /// remaining seconds, not the stale boot-time manager's.
    ///
    /// A `std::sync::Mutex` is correct here: set/clear happen on the cold-path lane
    /// lifecycle (start/stop), and the read happens on the periodic self-test (once
    /// per trading day) + SLO (every 10s) cadences — NEVER on the per-tick hot path,
    /// so the lock is uncontended and has zero hot-path impact. `Option<Arc<_>>` is
    /// cheap to clone out under the lock; the lock is never held across an `.await`.
    live_lane_token_manager: Mutex<Option<Arc<TokenManager>>>,
    // (2026-07-15: the §34 Groww auto-scale panel snapshot slot was deleted
    // with the scale ladder — the /feeds health payload no longer carries a
    // `groww_scale` object.)
}

impl FeedRuntimeState {
    /// Build from the boot config — `dhan_enabled` / `groww_enabled` seed the
    /// atomics. This is the ONLY constructor production uses; the runtime API
    /// only ever flips the atomics afterward, never re-seeds from config.
    /// PR-E: `dhan_disable_allowed` defaults `true`; the boot wiring narrows it
    /// to `dry_run` via [`Self::set_dhan_disable_allowed`].
    #[must_use]
    pub fn from_config(feeds: &FeedsConfig) -> Self {
        // No-overlay path (tests / `Default`): the effective and RAW Dhan
        // config values coincide, so the raw seed is `feeds.dhan_enabled`.
        Self::from_config_with_dhan_config(feeds, feeds.dhan_enabled)
    }

    /// Build from the boot config + the RAW (pre-overlay) TOML value of
    /// `feeds.dhan_enabled`. Production (main.rs) uses THIS constructor:
    /// `feeds` is the POST-overlay EFFECTIVE state (it seeds the runtime
    /// atomics), while `dhan_config_enabled_raw` is captured BEFORE
    /// `overlay_feeds` is applied — the Phase A "lane retired" truth is a
    /// CONFIG statement, so a persisted runtime-OFF overlay
    /// (data/feed-state.json) on a config-ON boot must never 409-lock the
    /// PR-E disable→restart→re-enable round trip, and no overlay shape can
    /// un-retire a config-OFF lane (round-2 review MEDIUM, 2026-07-13; the
    /// overlay AND-gate in `feed_state_persist` already narrows the
    /// EFFECTIVE value — this keeps the refusal gate keyed off the config
    /// alone, in BOTH directions).
    #[must_use]
    pub fn from_config_with_dhan_config(
        feeds: &FeedsConfig,
        dhan_config_enabled_raw: bool,
    ) -> Self {
        Self {
            dhan: Arc::new(AtomicBool::new(feeds.dhan_enabled)),
            groww: Arc::new(AtomicBool::new(feeds.groww_enabled)),
            // Phase A (2026-07-13): the RAW TOML Dhan value is retained
            // immutably so the API can refuse a runtime enable of the
            // retired lane (raw config-off is authoritative; round-2 FIX A).
            dhan_config_enabled: dhan_config_enabled_raw,
            // The lane is not running until the boot wiring spawns it.
            groww_lane_running: AtomicBool::new(false),
            dhan_lane_running: AtomicBool::new(false),
            // No pool until the inline Dhan boot spine spawns one (boot-ON only).
            dhan_disable_allowed: AtomicBool::new(true),
            // D2b: the lane FSM starts Off; the boot-ON inline start drives it
            // Off→Starting→Running, exactly like a runtime enable does.
            // D2c (C4): no lane manager until `start_dhan_lane` installs one.
            live_lane_token_manager: Mutex::new(None),
            // §34 PR-3: no scale snapshot until the ladder publishes one.
        }
    }

    /// PR-E: whether the Dhan lane is running this process.
    #[must_use]
    pub fn is_dhan_lane_running(&self) -> bool {
        self.dhan_lane_running.load(Ordering::Relaxed)
    }

    /// PR-2: set the Dhan lane-running flag BOTH ways. This is the two-way setter the
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

    /// Phase A (operator directive 2026-07-13): was `feeds.dhan_enabled`
    /// TRUE in the RAW boot TOML (BEFORE the feed-state overlay — round-2
    /// FIX A)? `false` means the Dhan live WS lane is retired for this
    /// process — the runtime cold-start supervisor refuses a lane start, so
    /// the `/api/feeds` handler refuses the enable direction (409) instead
    /// of recording a flag that can never take effect. `true` with the
    /// runtime flag off means dormant-not-retired (a persisted runtime-OFF
    /// overlay): the toggle re-enable stays allowed. Immutable for the
    /// process lifetime (config change + restart is the only way to change
    /// it).
    #[must_use]
    pub fn is_dhan_config_enabled(&self) -> bool {
        self.dhan_config_enabled
    }

    /// PR-E: may the operator DISABLE Dhan right now? `false` once live trading
    /// is on — the handler rejects a Dhan-off toggle so the feed can't be killed
    /// mid-trade. Enabling Dhan is always allowed.
    #[must_use]
    pub fn can_disable_dhan(&self) -> bool {
        self.dhan_disable_allowed.load(Ordering::Relaxed)
    }

    // PR-C2 (2026-07-13): the D2b `LaneState` FSM (enum + events +
    // `next_lane_state` + the `dhan_lane_state` atomic and its accessors) is
    // DELETED with the Dhan live-WS lane — no runtime cold-start exists to
    // drive it. `dhan_lane_running` / `dhan_pool_present` (plain bools) stay
    // for the /feeds page truthfulness.

    /// D2c (C4): install the CURRENTLY-live lane-owned `TokenManager`. Called by
    /// `start_dhan_lane` after a successful cold-start, OVERWRITING any prior
    /// (now-dead) manager. The two PROCESS-level health gauges read this via
    /// [`Self::live_token_manager`] so, after a runtime stop→re-start, they report
    /// the NEW manager's remaining-seconds rather than the stale boot-time global.
    ///
    /// Off-hot-path (lane lifecycle only); the `Mutex` is uncontended. On a poisoned
    /// lock we recover the inner guard (`into_inner`) rather than panic — losing the
    /// live handle is benign (the gauges fall back to the global), and a health-slot
    /// write must never abort the lane.
    // Needs a real `Arc<TokenManager>` to exercise, but `core`'s only constructor
    // (`new_for_test`) is `#[cfg(test)] pub(crate)` — unreachable from this crate.
    // The set path is one line; its read/clear companions (`live_token_manager` /
    // `clear_live_token_manager`) ARE unit-tested, and the live call site is
    // `start_dhan_lane` (boot-deploy follow exercises it).
    // TEST-EXEMPT: cross-crate test-constructor barrier (see comment above).
    pub fn set_live_token_manager(&self, manager: Arc<TokenManager>) {
        match self.live_lane_token_manager.lock() {
            Ok(mut slot) => *slot = Some(manager),
            Err(poisoned) => *poisoned.into_inner() = Some(manager),
        }
    }

    /// D2c (C4): the CURRENTLY-live lane-owned `TokenManager`, or `None` when no
    /// lane is running. The health gauges prefer this and fall back to the global
    /// `OnceLock` only on `None`, so a runtime stop→re-start reports the live
    /// manager's headroom. Clones the `Arc` out under the lock (cheap, no `.await`
    /// held). Called on the periodic self-test/SLO cadence, never per-tick.
    #[must_use]
    pub fn live_token_manager(&self) -> Option<Arc<TokenManager>> {
        match self.live_lane_token_manager.lock() {
            Ok(slot) => slot.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
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

impl std::fmt::Debug for FeedRuntimeState {
    /// Manual impl: renders the live-token-manager slot as a presence flag only
    /// (`TokenManager` is not `Debug` — it owns credentials), so no secret can
    /// leak through a `{:?}` format. Every other field is shown directly.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let live_token_manager_present = self
            .live_lane_token_manager
            .lock()
            .map_or(true, |slot| slot.is_some());
        f.debug_struct("FeedRuntimeState")
            .field("dhan", &self.dhan)
            .field("groww", &self.groww)
            .field("dhan_config_enabled", &self.dhan_config_enabled)
            .field("groww_lane_running", &self.groww_lane_running)
            .field("dhan_lane_running", &self.dhan_lane_running)
            .field("dhan_disable_allowed", &self.dhan_disable_allowed)
            .field(
                "live_lane_token_manager_present",
                &live_token_manager_present,
            )
            .finish()
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
            gdf_enabled: false,
            gdf: Default::default(),
            dhan_enabled: true,
            groww_enabled: true,
            ..Default::default()
        };
        let state = FeedRuntimeState::from_config(&feeds);
        assert!(state.is_enabled(Feed::Dhan));
        assert!(state.is_enabled(Feed::Groww));
        assert!(state.is_dhan_config_enabled());
    }

    #[test]
    fn test_is_dhan_config_enabled_immutable_across_runtime_toggles() {
        // Phase A (2026-07-13): the boot-config Dhan value is IMMUTABLE —
        // runtime toggles flip the atomic, never the config snapshot the
        // API refusal gate reads.
        let feeds = FeedsConfig {
            gdf_enabled: false,
            gdf: Default::default(),
            dhan_enabled: false,
            groww_enabled: true,
            ..Default::default()
        };
        let state = FeedRuntimeState::from_config(&feeds);
        assert!(!state.is_dhan_config_enabled());
        state.set_enabled(Feed::Dhan, true);
        assert!(
            !state.is_dhan_config_enabled(),
            "runtime enable must not mutate the boot-config snapshot"
        );
    }

    /// Round-2 FIX A (2026-07-13): the 409/refusal gate is seeded from the
    /// RAW pre-overlay TOML value, and the feed-state overlay can flip it in
    /// NEITHER direction.
    /// test coverage (pub-fn-test-guard): from_config_with_dhan_config
    #[test]
    fn test_dhan_config_enabled_seeded_from_raw_toml_not_overlay() {
        // config-ON boot + persisted runtime-OFF overlay: the EFFECTIVE
        // feeds carry dhan=false (seeds the runtime atomic OFF) while the
        // RAW TOML said true — the lane is dormant, NOT retired, so the
        // refusal gate must stay open (pre-Phase-A round trip restored).
        let effective_off = FeedsConfig {
            gdf_enabled: false,
            gdf: Default::default(),
            dhan_enabled: false,
            groww_enabled: true,
            ..Default::default()
        };
        let state = FeedRuntimeState::from_config_with_dhan_config(&effective_off, true);
        assert!(
            !state.is_enabled(Feed::Dhan),
            "the runtime atomic seeds from the EFFECTIVE (post-overlay) value"
        );
        assert!(
            state.is_dhan_config_enabled(),
            "the refusal gate reads the RAW TOML value — a runtime-OFF \
             overlay must not 409-lock a config-ON lane"
        );

        // Defensive inverse (production-impossible: the overlay AND-gate can
        // never widen raw=false to effective=true) — pins that the gate is
        // keyed off the raw value ALONE, so no overlay shape can un-retire a
        // config-OFF lane either.
        let effective_on = FeedsConfig {
            gdf_enabled: false,
            gdf: Default::default(),
            dhan_enabled: true,
            ..Default::default()
        };
        let state = FeedRuntimeState::from_config_with_dhan_config(&effective_on, false);
        assert!(state.is_enabled(Feed::Dhan));
        assert!(
            !state.is_dhan_config_enabled(),
            "raw=false keeps the lane retired regardless of the effective value"
        );
    }

    #[test]
    fn test_default_is_dhan_on_groww_off() {
        let state = FeedRuntimeState::default();
        assert!(state.is_enabled(Feed::Dhan));
        assert!(!state.is_enabled(Feed::Groww));
    }

    #[test]
    fn test_live_token_manager_defaults_none() {
        // D2c (C4): a fresh state has no lane manager — the gauges must fall
        // back to the global `OnceLock` (boot-OFF / Groww-only / pre-start).
        let state = FeedRuntimeState::default();
        assert!(
            state.live_token_manager().is_none(),
            "live lane token-manager slot must start empty"
        );
    }

    #[test]
    fn test_debug_renders_live_token_manager_presence_flag_only() {
        // The manual Debug impl must expose only a presence flag, never the
        // manager's contents (it owns credentials). Empty slot → present=false.
        let state = FeedRuntimeState::default();
        let dbg = format!("{state:?}");
        assert!(
            dbg.contains("live_lane_token_manager_present: false"),
            "Debug must show the presence flag (false when empty); got: {dbg}"
        );
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
    fn test_dhan_disable_safety_gate() {
        // tests set_dhan_disable_allowed + can_disable_dhan
        let state = FeedRuntimeState::default();
        assert!(state.can_disable_dhan(), "default no-orders phase: allowed");
        state.set_dhan_disable_allowed(false);
        assert!(!state.can_disable_dhan(), "live trading: disable refused");
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

    // ---------------------------------------------------------------------
    // D2b — LaneState FSM unit tests (pure-total transition coverage + the
    // atomic advance/CAS path). The watcher loops around `next_lane_state`,
    // so every legal + illegal transition is pinned here.
    // ---------------------------------------------------------------------
}
