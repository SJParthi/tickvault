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
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};

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

/// The Dhan-lane lifecycle state machine (D2b). The single source of truth for
/// where the runtime Dhan lane is — stored as an `AtomicU8` in
/// [`FeedRuntimeState`] (M9 of the cold-start design) so the boot-ON inline
/// start AND the runtime activation watcher read/write the SAME state.
///
/// `Running` is entered ONLY after `start_dhan_lane` returns `Ok` — closing the
/// "phantom running" false-OK where a lane could report running with no pool.
/// `Stopping → Off` happens ONLY after teardown awaits every WS/order-update/
/// renewal handle join (no double pool). `Stopping → Starting` is rejected.
///
/// The numeric discriminants are the `tv_dhan_lane_state` gauge values
/// (0=Off, 1=Starting, 2=Running, 3=Stopping) — stable wire format.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum LaneState {
    /// No pool, no Dhan tasks; the dormant watcher polls the enable flag.
    Off = 0,
    /// A `start_dhan_lane()` cold-start is in-flight (auth → universe → pool).
    /// Reported as NOT running (no false-OK while the lane is coming up).
    Starting = 1,
    /// Pool spawned, ticks flowing, order-update + renewal + lane watchdogs up.
    Running = 2,
    /// A `stop_dhan_lane()` teardown is in-flight; pool conns NOT yet joined.
    Stopping = 3,
}

impl LaneState {
    /// Stable wire-format byte for the `tv_dhan_lane_state` gauge + the atomic.
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    /// Inverse of [`Self::as_u8`]. Unknown bytes fail-closed to `Off` (the safe
    /// "no lane" state) — there is no panic path, mirroring the no-panic
    /// discipline of the Dhan binary-protocol `from_byte` helpers.
    #[must_use]
    pub const fn from_u8(byte: u8) -> Self {
        match byte {
            1 => Self::Starting,
            2 => Self::Running,
            3 => Self::Stopping,
            // 0 and every unknown byte fail-closed to Off.
            _ => Self::Off,
        }
    }

    /// Whether this state means the lane is genuinely live (a real pool exists).
    /// ONLY `Running` is live — `Starting` is honestly NOT-yet-running (no
    /// false-OK), and `Stopping`/`Off` are not running.
    #[must_use]
    pub const fn is_running(self) -> bool {
        matches!(self, Self::Running)
    }
}

/// The events the lane FSM reacts to. Pure data — the FSM
/// ([`next_lane_state`]) maps `(state, event) -> Option<LaneState>` with no I/O.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LaneEvent {
    /// The watcher observed desired-ON while `Off`; a cold-start begins.
    StartRequested,
    /// `start_dhan_lane` returned `Ok(handles)` — the lane is genuinely live.
    StartSucceeded,
    /// `start_dhan_lane` returned an error (auth / universe / pool stage).
    StartFailed,
    /// The in-flight start task was cancelled (desired-OFF mid-`Starting`).
    StartCancelled,
    /// The watcher observed a GATED desired-OFF while `Running`; teardown begins.
    StopRequested,
    /// Teardown awaited every lane handle join — the lane is fully down.
    StopJoined,
    /// `stop_dhan_lane` re-asserted the disable gate immediately before the
    /// irreversible WS-close (H5), found it re-closed (an order opened
    /// mid-teardown), and aborted the teardown → the lane stays up.
    StopAborted,
}

/// The pure, total Dhan-lane FSM transition function (D2b). Returns the NEXT
/// state for a legal `(current, event)` pair, or `None` for an illegal one —
/// never panics. This mirrors `reconcile_lane_action`'s pure-and-total style so
/// the watcher loop stays a trivial poll around a unit-tested decision.
///
/// Legal transitions (everything else is `None`):
/// - `Off + StartRequested      -> Starting`
/// - `Starting + StartSucceeded -> Running`
/// - `Starting + StartFailed    -> Off`  (no half-running lane)
/// - `Starting + StartCancelled -> Off`
/// - `Running + StopRequested   -> Stopping`
/// - `Stopping + StopJoined     -> Off`   (ONLY after all handles join — H6)
/// - `Stopping + StopAborted    -> Running` (gate re-closed mid-teardown — H5)
///
/// Explicitly REJECTED (returns `None`):
/// - `Running + StartRequested`  (idempotent — already running)
/// - `Off + StartSucceeded` / `Off + StopRequested` / `Off + StopJoined`
/// - `Stopping + StartRequested` (no Start while a teardown is draining — H6;
///   Start is legal ONLY from a confirmed-empty `Off`)
#[must_use]
pub fn next_lane_state(current: LaneState, event: LaneEvent) -> Option<LaneState> {
    use LaneEvent::{
        StartCancelled, StartFailed, StartRequested, StartSucceeded, StopAborted, StopJoined,
        StopRequested,
    };
    match (current, event) {
        (LaneState::Off, StartRequested) => Some(LaneState::Starting),
        (LaneState::Starting, StartSucceeded) => Some(LaneState::Running),
        (LaneState::Starting, StartFailed | StartCancelled) => Some(LaneState::Off),
        (LaneState::Running, StopRequested) => Some(LaneState::Stopping),
        (LaneState::Stopping, StopJoined) => Some(LaneState::Off),
        (LaneState::Stopping, StopAborted) => Some(LaneState::Running),
        // Every other pair is illegal — including Running+StartRequested
        // (idempotent), Stopping+StartRequested (H6 — no double pool), and any
        // event fired from a state that cannot accept it.
        _ => None,
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
    /// PR-E: whether the Dhan main-feed lane was spawned this process (Dhan
    /// enabled at boot). Mirrors `groww_lane_running` for honest reporting.
    pub dhan_lane_running: bool,
    /// PR-E: whether DISABLING Dhan at runtime is currently permitted. `true`
    /// in the no-orders data-pull phase (`dry_run`); `false` once live trading
    /// is on, so the toggle can never blind the system mid-trade.
    pub dhan_disable_allowed: bool,
}

/// §34 PR-3 (Item 12): one per-connection health row for the `/feeds` panel.
/// All evidence is FILE-derived (per-conn status + tick capture files under
/// `data/groww/shards/cNN/`) — honest, no invented liveness.
#[derive(Clone, Debug, serde::Serialize)]
pub struct GrowwScaleConnRow {
    /// Zero-based connection id (matches the shard cutter / `GROWW_CONN_ID`).
    pub conn_id: usize,
    /// `true` when the ladder currently wants this connection running
    /// (`conn_id < desired_conns`).
    pub desired: bool,
    /// `true` when the sidecar wrote its connect+subscribe PROOF status file.
    pub subscribed_proof: bool,
    /// Bytes captured into this conn's NDJSON tick file (0 = nothing yet).
    pub tick_file_bytes: u64,
    /// Seconds since this conn's tick file was last appended; `null` = never.
    pub last_capture_age_secs: Option<u64>,
}

/// §34 PR-3 (Item 12): the Groww auto-scale panel snapshot the ladder
/// publishes every evaluation tick (~30s). Served on `GET /api/feeds/health`
/// as the `groww_scale` object when scale mode is enabled.
#[derive(Clone, Debug, serde::Serialize)]
pub struct GrowwScaleSnapshot {
    /// Ladder FSM state label (`probing` / `holding` / `advancing` /
    /// `rolling_back` / `halted_at_ceiling` / `halted_at_plateau`).
    pub ladder_state: &'static str,
    /// Connections the ladder currently wants running.
    pub desired_conns: usize,
    /// Configured ceiling (post probe-override when probe mode is on).
    pub target_conns: usize,
    /// Cap-probe mode active (2 x 600 verdict run).
    pub probe_mode: bool,
    /// This evaluation ran in weekend SMOKE mode (market closed; tick gates
    /// honestly skipped — machinery validation only, NOT a live validation).
    pub smoke: bool,
    /// One row per potential connection (0..target_conns).
    pub connections: Vec<GrowwScaleConnRow>,
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
    /// Set once by the boot wiring when the Groww bridge task is actually
    /// spawned (i.e. `groww_enabled` was true at boot). Read by the API to tell
    /// the operator honestly whether a runtime toggle will take effect.
    groww_lane_running: AtomicBool,
    /// PR-E: set once by the boot wiring when the Dhan main-feed pool is spawned.
    dhan_lane_running: AtomicBool,
    /// PR-2: set TRUE exactly where the inline Dhan boot spine spawned the
    /// main-feed WS pool at boot (i.e. `dhan_enabled` was true AT BOOT). This is
    /// the "a real pool exists for PR-E's in-loop dormancy to resume" sentinel,
    /// distinct from `dhan_lane_running` (a transient UI flag the watcher
    /// toggles). On a boot-OFF run (Dhan disabled at boot, e.g. Groww-only) NO
    /// pool is spawned, so this stays `false` and a runtime enable must NOT mark
    /// the lane running — that would be a false-OK (`dhan_lane_running=true` with
    /// zero connections / zero ticks). Default `false`.
    dhan_pool_present: AtomicBool,
    /// PR-E: gate on the Dhan *disable* direction (orders-live safety). Seeded
    /// from `dry_run` at boot; defaults `true` (no-orders phase).
    dhan_disable_allowed: AtomicBool,
    /// D2b: the Dhan-lane lifecycle FSM state (`LaneState` byte). The single
    /// source of truth for where the runtime lane is (M9). The boot-ON inline
    /// start, the dormant runtime watcher, and the `tv_dhan_lane_state` gauge
    /// all read/write THIS atomic, so the lane state can never disagree between
    /// the boot path and the watcher. Defaults `Off` (no lane yet).
    dhan_lane_state: AtomicU8,
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
    /// §34 PR-3 (Item 12): latest Groww auto-scale panel snapshot, published by
    /// the ladder every ~30s eval tick and read by `GET /api/feeds/health`.
    /// Cold path both sides (30s writer / operator-page reader) — a Mutex'd
    /// `Option<Arc<_>>` mirrors the `live_lane_token_manager` pattern; the lock
    /// is never held across an `.await` and never touched per-tick.
    groww_scale_snapshot: Mutex<Option<Arc<GrowwScaleSnapshot>>>,
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
            // No pool until the inline Dhan boot spine spawns one (boot-ON only).
            dhan_pool_present: AtomicBool::new(false),
            dhan_disable_allowed: AtomicBool::new(true),
            // D2b: the lane FSM starts Off; the boot-ON inline start drives it
            // Off→Starting→Running, exactly like a runtime enable does.
            dhan_lane_state: AtomicU8::new(LaneState::Off.as_u8()),
            // D2c (C4): no lane manager until `start_dhan_lane` installs one.
            live_lane_token_manager: Mutex::new(None),
            // §34 PR-3: no scale snapshot until the ladder publishes one.
            groww_scale_snapshot: Mutex::new(None),
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

    /// PR-2: the inline Dhan boot spine marks that it actually spawned the
    /// main-feed WS pool at boot (the boot-ON case). Called from the SAME boot
    /// block that calls [`Self::mark_dhan_lane_running`]. Idempotent.
    pub fn mark_dhan_pool_present(&self) {
        self.dhan_pool_present.store(true, Ordering::Relaxed);
    }

    /// PR-2: does a real Dhan main-feed pool exist this process (spawned at
    /// boot)? The Dhan activation watcher reads this to refuse marking the lane
    /// running on a runtime enable when NO pool was ever spawned (boot-OFF run) —
    /// closing the false-OK where `dhan_lane_running` would claim `true` with zero
    /// connections. `true` only after a boot-ON Dhan boot spine ran.
    #[must_use]
    pub fn is_dhan_pool_present(&self) -> bool {
        self.dhan_pool_present.load(Ordering::Relaxed)
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
    /// §34 PR-3 (Item 12): publish the latest Groww auto-scale panel snapshot.
    /// Called by the ladder every evaluation tick (~30s, cold path). A poisoned
    /// lock (a panicked writer) is survived by skipping the publish — the panel
    /// simply shows the previous snapshot.
    pub fn set_groww_scale_snapshot(&self, snapshot: Arc<GrowwScaleSnapshot>) {
        if let Ok(mut slot) = self.groww_scale_snapshot.lock() {
            *slot = Some(snapshot);
        }
    }

    /// §34 PR-3 (Item 12): the latest Groww auto-scale panel snapshot, or
    /// `None` when scale mode is off / the ladder has not published yet.
    #[must_use]
    pub fn groww_scale_snapshot(&self) -> Option<Arc<GrowwScaleSnapshot>> {
        self.groww_scale_snapshot
            .lock()
            .ok()
            .and_then(|slot| slot.clone())
    }

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

    /// D2b: read the current Dhan-lane FSM state (the single source of truth).
    /// `Relaxed`: the lane state is observed by the watcher poll + the API
    /// snapshot, neither of which has an ordering dependency with other state.
    #[must_use]
    pub fn dhan_lane_state(&self) -> LaneState {
        LaneState::from_u8(self.dhan_lane_state.load(Ordering::Relaxed))
    }

    /// D2b: force the Dhan-lane FSM to a specific state. Use ONLY for the
    /// boot-ON inline path (which sets `Running` after the spine spawns the
    /// pool) and tests. The runtime watcher uses [`Self::advance_dhan_lane`],
    /// which goes through the pure FSM. Idempotent.
    pub fn set_dhan_lane_state(&self, state: LaneState) {
        self.dhan_lane_state.store(state.as_u8(), Ordering::Relaxed);
    }

    /// D2b: apply a lane [`LaneEvent`] through the pure [`next_lane_state`] FSM,
    /// atomically (compare-and-swap on the observed current state). Returns
    /// `Some(new_state)` if the transition was legal AND won the CAS race;
    /// `None` if the event is illegal from the observed state OR another writer
    /// raced us (the caller re-reads + retries, mirroring the watcher poll).
    ///
    /// This is the ONLY mutator the runtime watcher uses, so an illegal
    /// transition (e.g. `Stopping → Starting`) can never be applied — the FSM
    /// rejects it and the atomic is left untouched (H6 double-pool prevention).
    pub fn advance_dhan_lane(&self, event: LaneEvent) -> Option<LaneState> {
        let current = self.dhan_lane_state();
        let next = next_lane_state(current, event)?;
        // CAS so two concurrent advancers can't both "win" the same transition.
        self.dhan_lane_state
            .compare_exchange(
                current.as_u8(),
                next.as_u8(),
                Ordering::Relaxed,
                Ordering::Relaxed,
            )
            .ok()
            .map(|_| next)
    }

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

    /// D2c (C4): clear the live lane-owned `TokenManager`. Called at every
    /// lane→`Off` transition (operator disable, watchdog-Halt, and the
    /// already-torn-down convergence) so the gauges fall back to the global handle
    /// while no lane runs (boot-OFF / Groww-only) instead of reading a dead manager.
    /// Idempotent. Off-hot-path; poison-tolerant (see [`Self::set_live_token_manager`]).
    pub fn clear_live_token_manager(&self) {
        match self.live_lane_token_manager.lock() {
            Ok(mut slot) => *slot = None,
            Err(poisoned) => *poisoned.into_inner() = None,
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
            .field("groww_lane_running", &self.groww_lane_running)
            .field("dhan_lane_running", &self.dhan_lane_running)
            .field("dhan_pool_present", &self.dhan_pool_present)
            .field("dhan_disable_allowed", &self.dhan_disable_allowed)
            .field("dhan_lane_state", &self.dhan_lane_state)
            .field(
                "live_lane_token_manager_present",
                &live_token_manager_present,
            )
            .field("groww_scale_snapshot", &self.groww_scale_snapshot)
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
            dhan_enabled: true,
            groww_enabled: true,
            ..Default::default()
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
    fn test_clear_live_token_manager_is_idempotent_when_empty() {
        // Clearing an already-empty slot is a no-op (the watchdog-Halt and the
        // already-torn-down convergence can both fire the clear).
        let state = FeedRuntimeState::default();
        state.clear_live_token_manager();
        state.clear_live_token_manager();
        assert!(state.live_token_manager().is_none());
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
    fn test_dhan_pool_present_sentinel() {
        // PR-2 false-OK fix: tests mark_dhan_pool_present + is_dhan_pool_present.
        // Default/boot-OFF: no pool present, so a runtime enable must NOT mark the
        // lane running (the watcher gates on this). Boot-ON: the inline spine marks
        // it, so the watcher may mark the lane running. This is the sentinel that
        // distinguishes "a real pool exists to resume" from "nothing to resume".
        let state = FeedRuntimeState::default();
        assert!(
            !state.is_dhan_pool_present(),
            "default/boot-OFF: no pool spawned"
        );
        state.mark_dhan_pool_present();
        assert!(
            state.is_dhan_pool_present(),
            "boot-ON: inline spine marked the pool present"
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

    // ---------------------------------------------------------------------
    // D2b — LaneState FSM unit tests (pure-total transition coverage + the
    // atomic advance/CAS path). The watcher loops around `next_lane_state`,
    // so every legal + illegal transition is pinned here.
    // ---------------------------------------------------------------------

    #[test]
    fn next_lane_state_off_to_starting_on_start() {
        assert_eq!(
            next_lane_state(LaneState::Off, LaneEvent::StartRequested),
            Some(LaneState::Starting)
        );
    }

    #[test]
    fn next_lane_state_starting_to_running_on_success() {
        // Running is entered ONLY after start succeeds — the phantom-running fix.
        assert_eq!(
            next_lane_state(LaneState::Starting, LaneEvent::StartSucceeded),
            Some(LaneState::Running)
        );
    }

    #[test]
    fn next_lane_state_starting_to_off_on_failure() {
        assert_eq!(
            next_lane_state(LaneState::Starting, LaneEvent::StartFailed),
            Some(LaneState::Off),
            "a failed cold-start returns the FSM to Off — never a half-running lane"
        );
    }

    #[test]
    fn next_lane_state_starting_to_off_on_cancel() {
        assert_eq!(
            next_lane_state(LaneState::Starting, LaneEvent::StartCancelled),
            Some(LaneState::Off),
            "a cancelled (desired-OFF mid-start) cold-start returns the FSM to Off"
        );
    }

    #[test]
    fn next_lane_state_running_to_stopping_on_disable() {
        assert_eq!(
            next_lane_state(LaneState::Running, LaneEvent::StopRequested),
            Some(LaneState::Stopping)
        );
    }

    #[test]
    fn next_lane_state_stopping_to_off_only_after_handles_join() {
        // H6: Stopping→Off ONLY on StopJoined (after the teardown awaits every
        // handle join), never on a bare Notify-fire.
        assert_eq!(
            next_lane_state(LaneState::Stopping, LaneEvent::StopJoined),
            Some(LaneState::Off)
        );
    }

    #[test]
    fn next_lane_state_stopping_to_running_on_gate_reclose() {
        // H5: the disable gate re-closed mid-teardown (an order opened) → the
        // teardown aborts and the lane returns to Running, never blinded.
        assert_eq!(
            next_lane_state(LaneState::Stopping, LaneEvent::StopAborted),
            Some(LaneState::Running)
        );
    }

    #[test]
    fn next_lane_state_rejects_running_to_starting() {
        // Idempotent: a Start while already Running must be a no-op (None).
        assert_eq!(
            next_lane_state(LaneState::Running, LaneEvent::StartRequested),
            None
        );
    }

    #[test]
    fn next_lane_state_rejects_off_to_running() {
        // Must pass through Starting — Off→Running directly is illegal.
        assert_eq!(
            next_lane_state(LaneState::Off, LaneEvent::StartSucceeded),
            None
        );
    }

    #[test]
    fn next_lane_state_rejects_off_to_stopping() {
        // Nothing to tear down from Off.
        assert_eq!(
            next_lane_state(LaneState::Off, LaneEvent::StopRequested),
            None
        );
        assert_eq!(next_lane_state(LaneState::Off, LaneEvent::StopJoined), None);
    }

    #[test]
    fn next_lane_state_rejects_stopping_to_starting() {
        // H6 (double-pool prevention): NO Start while a teardown is draining.
        // Start is legal ONLY from a confirmed-empty Off.
        assert_eq!(
            next_lane_state(LaneState::Stopping, LaneEvent::StartRequested),
            None,
            "Stopping→Starting must be rejected to prevent a double pool"
        );
    }

    #[test]
    fn next_lane_state_is_total_never_panics() {
        // Property: every (state, event) pair returns Some|None and never
        // panics. Exhaustively iterate the small finite product.
        let states = [
            LaneState::Off,
            LaneState::Starting,
            LaneState::Running,
            LaneState::Stopping,
        ];
        let events = [
            LaneEvent::StartRequested,
            LaneEvent::StartSucceeded,
            LaneEvent::StartFailed,
            LaneEvent::StartCancelled,
            LaneEvent::StopRequested,
            LaneEvent::StopJoined,
            LaneEvent::StopAborted,
        ];
        let mut legal = 0usize;
        for s in states {
            for e in events {
                // The call itself must not panic; we only count outcomes.
                if next_lane_state(s, e).is_some() {
                    legal += 1;
                }
            }
        }
        // Exactly the 7 documented legal transitions.
        assert_eq!(legal, 7, "exactly 7 legal transitions in the FSM");
    }

    #[test]
    fn lane_state_byte_round_trips_and_unknown_fails_closed_to_off() {
        for s in [
            LaneState::Off,
            LaneState::Starting,
            LaneState::Running,
            LaneState::Stopping,
        ] {
            assert_eq!(LaneState::from_u8(s.as_u8()), s);
        }
        // Unknown bytes fail-closed to Off (no panic).
        assert_eq!(LaneState::from_u8(7), LaneState::Off);
        assert_eq!(LaneState::from_u8(255), LaneState::Off);
        // Only Running is "live".
        assert!(LaneState::Running.is_running());
        assert!(!LaneState::Starting.is_running());
        assert!(!LaneState::Off.is_running());
        assert!(!LaneState::Stopping.is_running());
    }

    #[test]
    fn advance_dhan_lane_applies_the_pure_fsm_atomically() {
        let state = FeedRuntimeState::default();
        // Default lane state is Off.
        assert_eq!(state.dhan_lane_state(), LaneState::Off);
        // Off → Starting → Running (the boot-ON / runtime-enable path).
        assert_eq!(
            state.advance_dhan_lane(LaneEvent::StartRequested),
            Some(LaneState::Starting)
        );
        assert_eq!(state.dhan_lane_state(), LaneState::Starting);
        assert_eq!(
            state.advance_dhan_lane(LaneEvent::StartSucceeded),
            Some(LaneState::Running)
        );
        assert_eq!(state.dhan_lane_state(), LaneState::Running);
        // Running → Stopping → Off (gated disable + teardown join).
        assert_eq!(
            state.advance_dhan_lane(LaneEvent::StopRequested),
            Some(LaneState::Stopping)
        );
        assert_eq!(
            state.advance_dhan_lane(LaneEvent::StopJoined),
            Some(LaneState::Off)
        );
        assert_eq!(state.dhan_lane_state(), LaneState::Off);
    }

    #[test]
    fn advance_dhan_lane_rejects_illegal_event_and_leaves_state_untouched() {
        let state = FeedRuntimeState::default();
        state.set_dhan_lane_state(LaneState::Stopping);
        // H6: a Start while Stopping is rejected by the FSM — the atomic is
        // NOT mutated (no double pool).
        assert_eq!(state.advance_dhan_lane(LaneEvent::StartRequested), None);
        assert_eq!(
            state.dhan_lane_state(),
            LaneState::Stopping,
            "an illegal advance must leave the lane state untouched"
        );
    }

    #[test]
    fn test_dhan_lane_state_and_set_dhan_lane_state_round_trip() {
        // Direct coverage of the `dhan_lane_state` reader + `set_dhan_lane_state`
        // forced writer (used by the boot-ON inline path + tests).
        let state = FeedRuntimeState::default();
        assert_eq!(state.dhan_lane_state(), LaneState::Off);
        for s in [
            LaneState::Starting,
            LaneState::Running,
            LaneState::Stopping,
            LaneState::Off,
        ] {
            state.set_dhan_lane_state(s);
            assert_eq!(state.dhan_lane_state(), s);
        }
    }

    #[test]
    fn test_lane_state_as_u8_from_u8_and_is_running_accessors() {
        // Direct coverage of LaneState::as_u8 / from_u8 / is_running.
        assert_eq!(LaneState::Off.as_u8(), 0);
        assert_eq!(LaneState::Starting.as_u8(), 1);
        assert_eq!(LaneState::Running.as_u8(), 2);
        assert_eq!(LaneState::Stopping.as_u8(), 3);
        assert_eq!(LaneState::from_u8(2), LaneState::Running);
        assert!(LaneState::Running.is_running());
        assert!(!LaneState::Off.is_running());
    }

    #[test]
    fn advance_dhan_lane_gate_reclose_returns_running_from_stopping() {
        // H5: gate re-closed mid-teardown → StopAborted drives Stopping→Running.
        let state = FeedRuntimeState::default();
        state.set_dhan_lane_state(LaneState::Stopping);
        assert_eq!(
            state.advance_dhan_lane(LaneEvent::StopAborted),
            Some(LaneState::Running)
        );
        assert_eq!(state.dhan_lane_state(), LaneState::Running);
    }
}
