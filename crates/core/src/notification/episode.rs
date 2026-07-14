//! Telegram UX Overhaul (2026-07-07) — pure episode core.
//!
//! One incident = ONE Telegram bubble. A WebSocket outage that used to emit
//! 40 separate messages now opens a single "first page" bubble whose live
//! counters are EDITED in place; recovery flips the same bubble to a
//! "confirming…" phase and — after a stability window — to ONE green close
//! line. A flap shortly after close re-opens the SAME bubble.
//!
//! # Purity contract
//!
//! Everything in this file is a pure, total function of its inputs:
//! - NO I/O (no HTTP, no filesystem)
//! - NO clock reads (`now_ms` is always an injected parameter)
//! - NO panics on any input (ratcheted by
//!   `proptest_fsm_total_never_ignores_high_critical`)
//!
//! The transport shell (the Telegram send/edit HTTP calls, the snapshot
//! file writer) lives in `service.rs` and is a thin wrapper around this core.
//!
//! # Never-drop pin (operator law)
//!
//! A `Severity::High` / `Severity::Critical` event with role `Open` or
//! `Progress` can NEVER map to [`EpisodeAction::Ignore`]. Suppression is
//! never a decision — only transport can fail, and the transport shell
//! terminates every failure at the existing TELEGRAM-01 loudness.
//!
//! # Performance
//!
//! Cold path only — the episode machinery runs inside `notify()`'s spawned
//! task. The tick hot path is untouched. `EpisodeKey` is `Copy` so the
//! per-event `episode_key()` probe on the dispatch bypass arm is
//! allocation-free (DHAT-pinned by `dhat_telegram_dispatcher.rs`).

use std::collections::HashMap;
use std::sync::Mutex;

use super::events::Severity;

// ---------------------------------------------------------------------------
// Pinned constants (ratcheted by crates/core/tests/episode_edit_wiring_guard.rs)
// ---------------------------------------------------------------------------

/// Minimum seconds between two live edits of the same bubble. Bounds the
/// Telegram edit traffic to ~3 edits/min/episode under a pathological
/// 1-event/sec storm; folded events still advance the counters.
pub const EPISODE_EDIT_MIN_INTERVAL_SECS: u64 = 20;

/// Seconds a `Recovering` episode must stay quiet (no new Open/Progress)
/// before the bubble flips to the final green close line. Prevents a flap
/// from instantly re-greening the bubble.
pub const EPISODE_STABILITY_SECS: u64 = 60;

/// Seconds after a close during which a fresh disconnect folds back into
/// the SAME closed bubble (tombstone reopen) instead of opening a new one.
pub const EPISODE_REOPEN_SECS: u64 = 120;

/// Steady-state render ceiling — characters. Live-edited bubbles stay
/// glanceable; the full explanation lives on the first page only.
pub const EPISODE_STEADY_MAX_CHARS: usize = 320;

/// Steady-state render ceiling — lines.
pub const EPISODE_STEADY_MAX_LINES: usize = 3;

/// First-page render ceiling — equals the Telegram chunk limit
/// (`service.rs::TELEGRAM_CHUNK_LIMIT_CHARS`) so the first page is ALWAYS a
/// single chunk and its message id unambiguously names the bubble.
pub const EPISODE_FIRST_PAGE_MAX_CHARS: usize = 3800;

/// Maximum age (seconds) of a persisted episode snapshot entry at rehydrate.
/// Older entries are dropped fail-open — a fresh first page (one duplicate
/// bubble) beats editing a bubble from another era.
pub const EPISODE_REHYDRATE_MAX_AGE_SECS: u64 = 7200;

/// Consecutive transient edit failures after which the fallback ladder
/// sends a FRESH bubble (duplicate-over-drop) instead of retrying edits.
/// Applies to episodes whose peak severity is BELOW High — a High/Critical
/// episode falls back on the very first exhausted transient (the shell
/// checks `severity_peak`), because a structurally-final event (e.g. the
/// once-per-outage order-update page) may never re-drive the ladder.
pub const EPISODE_EDIT_FAILURES_FALLBACK_THRESHOLD: u8 = 2;

/// Seconds a `Down` episode may sit with NO events before it stops being
/// live (hostile-review fix 2026-07-07). Covers the restart edge: a
/// rehydrated Down episode whose recovery event never arrives (a clean
/// first connect emits no reconnect event) would otherwise show DOWN
/// forever AND swallow a later same-day outage as a silent edit with no
/// fresh page and no SMS. Expiry closes the bubble neutrally and writes
/// NO tombstone, so the next outage opens a FRESH first page (with the
/// SNS-SMS leg).
pub const EPISODE_DOWN_STALE_EXPIRE_SECS: u64 = 1800;

/// Boot-bubble render ceiling — characters (2026-07-09 operator escalation:
/// ONE consolidated boot bubble per boot). Larger than the WS steady ceiling
/// because the checklist carries one line per boot component, still far
/// below one Telegram chunk.
pub const BOOT_BUBBLE_MAX_CHARS: usize = 1200;

/// Seconds after `BootMilestone::Complete` before the boot bubble retires
/// SILENTLY (registry state + checklist cleared, NO edit). Late stragglers
/// inside the window still fold into the completed bubble; a mapped event
/// AFTER retirement (mid-day lane cold start / feed re-enable) opens a
/// fresh mini-checklist bubble instead of editing the morning's message.
pub const BOOT_BUBBLE_RETIRE_SECS: u64 = 900;

const MS_PER_SEC: u64 = 1000;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// Which incident family a Telegram episode bubble tracks.
///
/// Extensible; only the two live WebSocket lifecycle families today — they
/// cover the observed 40-message disconnect storms.
#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq)]
pub enum EpisodeFamily {
    /// Dhan main-feed WebSocket lifecycle (disconnect / reconnect).
    MainFeedWs,
    /// Dhan order-update WebSocket lifecycle.
    OrderUpdateWs,
    /// The consolidated boot bubble (2026-07-09 operator escalation — the
    /// ~8-message boot spray folds into ONE live-edited checklist bubble).
    /// Single fixed key ([`BOOT_EPISODE_KEY`]); excluded from the tick
    /// promote/expire scans and from the cross-restart snapshot.
    Boot,
}

impl EpisodeFamily {
    /// Feed badge leading the bubble body (plain emoji + provider word —
    /// the 10 Telegram commandments hold: no library names, no file paths).
    #[must_use]
    pub const fn badge(self) -> &'static str {
        match self {
            Self::MainFeedWs | Self::OrderUpdateWs => "\u{1f537} DHAN", // 🔷
            Self::Boot => "\u{1f680}",                                  // 🚀
        }
    }

    /// Plain-English description of the affected feed.
    #[must_use]
    pub const fn feed_desc(self) -> &'static str {
        match self {
            Self::MainFeedWs => "Live price feed",
            Self::OrderUpdateWs => "Order confirmations feed",
            Self::Boot => "System boot",
        }
    }

    /// Stable wire label for the snapshot file.
    #[must_use]
    pub const fn snapshot_label(self) -> &'static str {
        match self {
            Self::MainFeedWs => "main_feed_ws",
            Self::OrderUpdateWs => "order_update_ws",
            Self::Boot => "boot",
        }
    }

    /// Inverse of [`Self::snapshot_label`]. Unknown labels → `None`
    /// (fail-open — the snapshot entry is dropped, never a panic).
    #[must_use]
    pub fn from_snapshot_label(label: &str) -> Option<Self> {
        match label {
            "main_feed_ws" => Some(Self::MainFeedWs),
            "order_update_ws" => Some(Self::OrderUpdateWs),
            "boot" => Some(Self::Boot),
            _ => None,
        }
    }
}

/// Composite episode identity — `(family, connection index)`.
#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq)]
pub struct EpisodeKey {
    pub family: EpisodeFamily,
    pub conn: u8,
}

/// The single fixed key for the consolidated boot bubble.
pub const BOOT_EPISODE_KEY: EpisodeKey = EpisodeKey {
    family: EpisodeFamily::Boot,
    conn: 0,
};

/// Per-family episode timing knobs.
///
/// - WS families keep the pinned [`EpisodeConfig::default`] values
///   (ratcheted by `episode_edit_wiring_guard.rs`).
/// - `Boot` runs a ZERO edit throttle: milestones are bounded (≤ ~12 per
///   process), the fnv1a hash-skip kills identical re-renders, and the
///   operator must see each component flip ✅ promptly.
#[must_use]
pub fn episode_config_for(family: EpisodeFamily) -> EpisodeConfig {
    match family {
        EpisodeFamily::MainFeedWs | EpisodeFamily::OrderUpdateWs => EpisodeConfig::default(),
        EpisodeFamily::Boot => EpisodeConfig {
            edit_min_interval_secs: 0,
            ..EpisodeConfig::default()
        },
    }
}

/// The role an incoming event plays inside an episode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EpisodeRole {
    /// A degradation signal (disconnect). Opens an episode when none exists.
    Open,
    /// A repeat degradation signal folding into an existing episode.
    Progress,
    /// A recovery signal (reconnect) — enters the Recovering phase; the
    /// green close waits for the stability window.
    Resolve,
}

/// Live phase of an open episode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EpisodePhase {
    /// The feed is down / flapping.
    Down,
    /// A recovery arrived; waiting out [`EPISODE_STABILITY_SECS`] before
    /// the green close.
    Recovering,
}

impl EpisodePhase {
    /// Stable wire label for the snapshot file.
    #[must_use]
    pub const fn snapshot_label(self) -> &'static str {
        match self {
            Self::Down => "down",
            Self::Recovering => "recovering",
        }
    }

    /// Inverse of [`Self::snapshot_label`]; unknown → `None` (fail-open).
    #[must_use]
    pub fn from_snapshot_label(label: &str) -> Option<Self> {
        match label {
            "down" => Some(Self::Down),
            "recovering" => Some(Self::Recovering),
            _ => None,
        }
    }
}

/// Mutable state of one live episode bubble.
#[derive(Debug, Clone)]
pub struct EpisodeState {
    pub key: EpisodeKey,
    /// Telegram message id of the bubble, captured from the first-page send.
    /// `None` = first page not yet delivered (or 200-with-unparseable-body):
    /// subsequent events take [`EpisodeAction::SendNewFallback`].
    pub message_id: Option<i64>,
    /// Epoch ms when the episode opened (first event).
    pub opened_at_ms: u64,
    /// Epoch ms of the most recent event folded into this episode.
    pub last_event_ms: u64,
    /// Number of degradation events (drops) folded in.
    pub occurrences: u32,
    /// Reconnect attempts observed (best-effort — from event context when
    /// available, else the folded event count).
    pub attempts: u32,
    /// Highest severity observed across the episode.
    pub severity_peak: Severity,
    /// `true` once the first-page explanation paragraph has been sent.
    /// PERSISTED so a restart never re-sends the boilerplate.
    pub explained: bool,
    /// FNV-1a hash of the last successfully-applied render — skips no-op
    /// edits (Telegram would 400 "message is not modified" otherwise).
    pub last_render_hash: u64,
    /// Epoch ms of the last successfully-applied edit (throttle anchor).
    pub last_edit_ms: u64,
    /// Consecutive transient edit failures (resets on success).
    pub edit_failures: u8,
    /// Down vs Recovering.
    pub phase: EpisodePhase,
}

impl EpisodeState {
    /// Fresh episode opened at `now_ms`.
    #[must_use]
    pub fn open(key: EpisodeKey, severity: Severity, now_ms: u64) -> Self {
        Self {
            key,
            message_id: None,
            opened_at_ms: now_ms,
            last_event_ms: now_ms,
            occurrences: 1,
            attempts: 0,
            severity_peak: severity,
            explained: false,
            last_render_hash: 0,
            last_edit_ms: 0,
            edit_failures: 0,
            phase: EpisodePhase::Down,
        }
    }
}

/// Tombstone of a closed episode — enables the flap-reopen fold.
#[derive(Debug, Clone, Copy)]
pub struct ClosedTombstone {
    pub message_id: i64,
    pub closed_at_ms: u64,
    /// Peak severity of the closed episode. A High/Critical reopen may
    /// only fold into a bubble that itself already paged at High/Critical
    /// — folding a HIGH event into a Low-peak tombstone would produce no
    /// push and no SMS (hostile-review fix 2026-07-07).
    pub severity_peak: Severity,
}

/// Episode timing knobs. Defaults come from the pinned constants.
#[derive(Debug, Clone, Copy)]
pub struct EpisodeConfig {
    pub edit_min_interval_secs: u64,
    pub stability_secs: u64,
    pub reopen_secs: u64,
    /// Seconds a Down episode may sit event-less before stale expiry.
    pub down_stale_expire_secs: u64,
}

impl Default for EpisodeConfig {
    fn default() -> Self {
        Self {
            edit_min_interval_secs: EPISODE_EDIT_MIN_INTERVAL_SECS,
            stability_secs: EPISODE_STABILITY_SECS,
            reopen_secs: EPISODE_REOPEN_SECS,
            down_stale_expire_secs: EPISODE_DOWN_STALE_EXPIRE_SECS,
        }
    }
}

// ---------------------------------------------------------------------------
// Boot bubble (2026-07-09) — milestones + checklist + render
// ---------------------------------------------------------------------------

/// State of the Dhan price-feed line on the boot checklist.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootDhanFeed {
    /// The main-feed pool reached Connected.
    Online {
        connected: u32,
        total: u32,
        /// Age (secs) of the freshest REAL tick, `None` = no real tick yet
        /// (the render keeps the honest "no real prices yet" caveat).
        last_real_tick_age_secs: Option<u32>,
    },
    /// Off-hours boot — connections deliberately wait for market open.
    DeferredOffHours,
}

/// One typed boot milestone folded into the [`BootChecklist`].
///
/// All variants are `Copy` (the `&'static str` mode rides `StartupComplete`'s
/// existing static label) so the per-event extraction never allocates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootMilestone {
    /// Infrastructure services health check completed.
    Services { healthy: u32, total: u32 },
    /// Dhan JWT acquired.
    DhanAuth,
    /// Instrument universe loaded (`count` = contracts + underlyings).
    Instruments { count: u32 },
    /// Dhan main-feed pool online.
    DhanFeedOnline {
        connected: u32,
        total: u32,
        last_real_tick_age_secs: Option<u32>,
    },
    /// Dhan main-feed deferred until the next market open (off-hours boot).
    DhanFeedDeferredOffHours,
    /// Order-update WebSocket task connected.
    OrderUpdateConnected,
    /// Order-update WebSocket auth handshake confirmed by the server.
    OrderUpdateAuthenticated,
    /// Groww feed token read + accepted.
    GrowwAuth,
    /// Groww feed instrument set resolved.
    GrowwInstruments { subscribed: u32 },
    /// Groww socket connected + subscribed (awaiting first tick).
    GrowwConnected { market_open: bool },
    /// Boot finished (`StartupComplete`).
    Complete { mode: &'static str },
}

/// The one-per-boot live checklist behind the consolidated boot bubble.
///
/// Every slot is an `Option` so [`Self::fold`] is idempotent AND
/// commutative — duplicate or out-of-order milestones (boot emits from
/// several spawned tasks) converge to the same render.
#[derive(Debug, Clone, Default)]
pub struct BootChecklist {
    /// Epoch ms of the first observed milestone (or the flavor init).
    pub started_at_ms: u64,
    /// Epoch ms of `Complete` (first one wins).
    pub complete_at_ms: Option<u64>,
    /// Run mode carried by `Complete` (e.g. "sandbox" / "LIVE").
    pub mode: Option<&'static str>,
    /// `true` iff the on-disk previous-boot sha and the built-in sha are
    /// BOTH valid lowercase hex AND differ (never claimed on `unknown`).
    pub new_code: bool,
    /// Short build sha for the header; empty/`unknown` renders plainly.
    pub build_sha_short: String,
    /// Which feed lines to show as ⏳-pending from the first page:
    /// `(dhan_enabled, groww_enabled)`. `None` = lazy mode (only lines
    /// with data render).
    pub expectations: Option<(bool, bool)>,
    pub services: Option<(u32, u32)>,
    pub dhan_auth: bool,
    pub instruments: Option<u32>,
    pub dhan_feed: Option<BootDhanFeed>,
    pub order_update_connected: bool,
    pub order_update_authenticated: bool,
    pub groww_auth: bool,
    pub groww_instruments: Option<u32>,
    /// `Some(market_open)` once the Groww socket connected + subscribed.
    pub groww_connected: Option<bool>,
    /// A render is pending delivery (set on fold; cleared by the shell on
    /// a successfully-applied send/edit or a byte-identical hash-skip).
    /// The drain ticker re-drives a dirty checklist so a failed FINAL
    /// edit (no further milestone will ever arrive) is retried ≤10s.
    pub dirty: bool,
    /// `true` when this checklist opened AFTER a completed boot bubble
    /// already retired in this process — i.e. a mid-day feed/component
    /// restart, NOT a system boot (hostile-review fix 2026-07-09). A mini
    /// checklist renders a neutral "Feed update" header (never "tickvault
    /// starting"), no "Boot finishing…" footer, and retires on an idle
    /// window instead of requiring `Complete` (which never re-fires).
    pub mini: bool,
    /// Epoch ms of the most recent fold — drives the mini idle retirement.
    pub last_fold_ms: u64,
}

impl BootChecklist {
    /// Fresh checklist anchored at `now_ms`.
    #[must_use]
    pub fn new(now_ms: u64, new_code: bool, build_sha_short: String) -> Self {
        Self {
            started_at_ms: now_ms,
            last_fold_ms: now_ms,
            new_code,
            build_sha_short,
            ..Self::default()
        }
    }

    /// Folds one milestone in. Idempotent (re-folding the same milestone
    /// is a no-op beyond `dirty`) and commutative (independent slots).
    pub fn fold(&mut self, milestone: BootMilestone, now_ms: u64) {
        match milestone {
            BootMilestone::Services { healthy, total } => {
                self.services = Some((healthy, total));
            }
            BootMilestone::DhanAuth => self.dhan_auth = true,
            BootMilestone::Instruments { count } => self.instruments = Some(count),
            BootMilestone::DhanFeedOnline {
                connected,
                total,
                last_real_tick_age_secs,
            } => {
                self.dhan_feed = Some(BootDhanFeed::Online {
                    connected,
                    total,
                    last_real_tick_age_secs,
                });
            }
            BootMilestone::DhanFeedDeferredOffHours => {
                self.dhan_feed = Some(BootDhanFeed::DeferredOffHours);
            }
            BootMilestone::OrderUpdateConnected => self.order_update_connected = true,
            BootMilestone::OrderUpdateAuthenticated => {
                self.order_update_connected = true;
                self.order_update_authenticated = true;
            }
            BootMilestone::GrowwAuth => self.groww_auth = true,
            BootMilestone::GrowwInstruments { subscribed } => {
                self.groww_instruments = Some(subscribed);
            }
            BootMilestone::GrowwConnected { market_open } => {
                self.groww_connected = Some(market_open);
            }
            BootMilestone::Complete { mode } => {
                // First Complete wins (idempotence under duplicates).
                if self.complete_at_ms.is_none() {
                    self.complete_at_ms = Some(now_ms);
                }
                self.mode.get_or_insert(mode);
            }
        }
        self.dirty = true;
        self.last_fold_ms = self.last_fold_ms.max(now_ms);
    }

    /// `true` once `Complete` folded in.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.complete_at_ms.is_some()
    }
}

/// What the transport shell must do for an incoming episode event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EpisodeAction {
    /// Send the full first page as a fresh message (this IS the preserved
    /// High/Critical immediate bypass send; the SNS-SMS leg rides it).
    SendFirstPage,
    /// Edit the existing bubble in place. `close` = final green line.
    Edit { message_id: i64, close: bool },
    /// Fold silently — inside the per-episode edit throttle window.
    /// Counters still advance; the next eligible edit carries them.
    EditThrottled,
    /// The bubble has no message id (first send failed / id unparseable) —
    /// send a fresh message (duplicate-over-drop).
    SendNewFallback,
    /// A recovery for an episode we never tracked (snapshot lost across a
    /// restart, or a cross-task ordering race) — deliver the event through
    /// the LEGACY immediate lane instead of dropping it (hostile-review
    /// fix 2026-07-07: a bare Ignore silently dropped the boot-time
    /// reconnect line).
    SendLegacy,
    /// Nothing to do. Reachable ONLY for `Resolve` against a FRESH
    /// tombstone — the episode just closed with the green line, so the
    /// recovery is already announced; a second line would be noise.
    Ignore,
}

// ---------------------------------------------------------------------------
// THE pure total FSM
// ---------------------------------------------------------------------------

/// Decides the transport action for one incoming episode event.
///
/// Total: never panics for any input combination. Never returns
/// [`EpisodeAction::Ignore`] for a High/Critical `Open`/`Progress` event
/// (the never-drop pin — proptest-ratcheted).
#[must_use]
pub fn next_episode_action(
    state: Option<&EpisodeState>,
    tombstone: Option<&ClosedTombstone>,
    role: EpisodeRole,
    severity: Severity,
    now_ms: u64,
    cfg: &EpisodeConfig,
) -> EpisodeAction {
    match state {
        Some(st) => {
            // Severity-escalation edge (hostile-review fix 2026-07-07): an
            // episode whose peak is below High (e.g. the pre-open Low
            // off-hours storm) crossing into High/Critical MUST page like
            // a fresh incident — an in-place edit produces no Telegram
            // push notification and the SNS-SMS leg never fires.
            // SendFirstPage replaces the bubble and rides the preserved
            // High/Critical bypass semantics (including the SMS leg), so
            // an in-market HIGH outage always reaches the operator's phone.
            if matches!(role, EpisodeRole::Open | EpisodeRole::Progress)
                && severity >= Severity::High
                && st.severity_peak < Severity::High
            {
                return EpisodeAction::SendFirstPage;
            }
            let Some(message_id) = st.message_id else {
                // First page never landed — fresh send, never a drop.
                return EpisodeAction::SendNewFallback;
            };
            match role {
                EpisodeRole::Open | EpisodeRole::Progress => {
                    let throttle_ms = cfg.edit_min_interval_secs.saturating_mul(MS_PER_SEC);
                    // A revert out of Recovering is a phase change the
                    // operator must see — never throttled.
                    if st.phase == EpisodePhase::Down
                        && now_ms.saturating_sub(st.last_edit_ms) < throttle_ms
                    {
                        EpisodeAction::EditThrottled
                    } else {
                        EpisodeAction::Edit {
                            message_id,
                            close: false,
                        }
                    }
                }
                // Recovery: edit to "confirming…" — NO instant green; the
                // registry tick promotes to close after the stability window.
                EpisodeRole::Resolve => EpisodeAction::Edit {
                    message_id,
                    close: false,
                },
            }
        }
        None => match role {
            EpisodeRole::Open | EpisodeRole::Progress => {
                if let Some(t) = tombstone
                    && now_ms.saturating_sub(t.closed_at_ms)
                        <= cfg.reopen_secs.saturating_mul(MS_PER_SEC)
                    // A High/Critical reopen may fold ONLY into a bubble
                    // that itself paged at High/Critical — a Low-peak
                    // tombstone swallowing a HIGH event would produce no
                    // push and no SMS (escalation gap, 2026-07-07 fix).
                    && !(severity >= Severity::High && t.severity_peak < Severity::High)
                {
                    // Flap folds into the SAME closed bubble. If the edit
                    // later fails permanent, the ladder sends fresh.
                    EpisodeAction::Edit {
                        message_id: t.message_id,
                        close: false,
                    }
                } else {
                    EpisodeAction::SendFirstPage
                }
            }
            EpisodeRole::Resolve => {
                if let Some(t) = tombstone
                    && now_ms.saturating_sub(t.closed_at_ms)
                        <= cfg.reopen_secs.saturating_mul(MS_PER_SEC)
                {
                    // Just closed green — the recovery is already
                    // announced by the close line; nothing to add.
                    EpisodeAction::Ignore
                } else {
                    // Recovery for an episode we never tracked — deliver
                    // it through the legacy immediate lane, never drop it.
                    EpisodeAction::SendLegacy
                }
            }
        },
    }
}

// ---------------------------------------------------------------------------
// Registry (cold-path state holder; poisoned-mutex recovery via into_inner)
// ---------------------------------------------------------------------------

/// Outcome of [`EpisodeRegistry::apply_event`] — the FSM action plus a
/// post-mutation snapshot of the episode state for rendering.
#[derive(Debug, Clone)]
pub struct EpisodeDecision {
    pub action: EpisodeAction,
    pub state: EpisodeState,
    /// `true` when a tombstone was folded back into a live episode.
    pub reopened: bool,
    /// `true` when a live sub-High episode escalated into High/Critical
    /// and the action is a fresh first page (SMS leg rides it).
    pub escalated: bool,
}

/// Outcome of [`EpisodeRegistry::tick`].
#[derive(Debug, Default)]
pub struct EpisodeTickOutcome {
    /// `Recovering` episodes promoted to the final green close line.
    pub closed: Vec<EpisodeState>,
    /// `Down` episodes expired after `down_stale_expire_secs` without any
    /// event — closed neutrally, NO tombstone (a later outage must open a
    /// FRESH first page with the SMS leg, never fold into a stale bubble).
    pub expired: Vec<EpisodeState>,
}

/// In-memory episode registry — one live [`EpisodeState`] per key plus
/// recent [`ClosedTombstone`]s for the flap-reopen fold.
#[derive(Debug, Default)]
pub struct EpisodeRegistry {
    inner: Mutex<HashMap<EpisodeKey, EpisodeState>>,
    tombstones: Mutex<HashMap<EpisodeKey, ClosedTombstone>>,
    /// The one-per-boot checklist behind the consolidated boot bubble
    /// (2026-07-09). LOCK ORDER: never held together with `inner` /
    /// `tombstones` — every method takes one lock at a time.
    boot: Mutex<Option<BootChecklist>>,
    /// Latched `true` once a COMPLETED boot bubble retired in this
    /// process. Any boot-keyed milestone arriving after that is a mid-day
    /// feed/component restart — the fresh checklist it opens is a MINI
    /// bubble (neutral header, idle retirement), never a false
    /// "tickvault starting" claim (hostile-review fix 2026-07-09).
    boot_completed_retired: std::sync::atomic::AtomicBool,
}

impl EpisodeRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn lock_inner(&self) -> std::sync::MutexGuard<'_, HashMap<EpisodeKey, EpisodeState>> {
        // Poisoned-mutex recovery via into_inner — the coalescer house
        // pattern: a panicked prior holder leaves consistent data.
        match self.inner.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    fn lock_tombstones(&self) -> std::sync::MutexGuard<'_, HashMap<EpisodeKey, ClosedTombstone>> {
        match self.tombstones.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    fn lock_boot(&self) -> std::sync::MutexGuard<'_, Option<BootChecklist>> {
        match self.boot.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    // -- Boot bubble (2026-07-09) -------------------------------------------

    /// Fresh checklist honoring the completed-boot latch: created AFTER a
    /// completed boot bubble retired ⇒ a MINI (feed-update) checklist.
    fn fresh_boot_checklist(&self, now_ms: u64) -> BootChecklist {
        let mut cl = BootChecklist::new(now_ms, false, String::new());
        cl.mini = self
            .boot_completed_retired
            .load(std::sync::atomic::Ordering::Relaxed);
        cl
    }

    /// Sets the deploy-flavor fields (creates the checklist when absent so
    /// the flavor is never lost to milestone-vs-init ordering).
    pub fn set_boot_flavor(&self, new_code: bool, build_sha_short: &str, now_ms: u64) {
        let mut boot = self.lock_boot();
        let cl = boot.get_or_insert_with(|| self.fresh_boot_checklist(now_ms));
        cl.new_code = new_code;
        cl.build_sha_short = build_sha_short.to_string();
    }

    /// Declares which feed lines the checklist shows as ⏳-pending from the
    /// first page (`None` until called → lazy render of data-only lines).
    pub fn set_boot_expectations(&self, dhan: bool, groww: bool, now_ms: u64) {
        let mut boot = self.lock_boot();
        let cl = boot.get_or_insert_with(|| self.fresh_boot_checklist(now_ms));
        cl.expectations = Some((dhan, groww));
    }

    /// Folds one boot milestone into the checklist, then runs the SAME
    /// pure FSM (role `Open`, severity fixed Low) for the message-id /
    /// fallback mechanics. Returns the decision + a post-fold snapshot.
    #[must_use]
    pub fn apply_boot_milestone(
        &self,
        milestone: BootMilestone,
        now_ms: u64,
        cfg: &EpisodeConfig,
    ) -> (EpisodeDecision, BootChecklist) {
        let checklist = {
            let mut boot = self.lock_boot();
            let cl = boot.get_or_insert_with(|| self.fresh_boot_checklist(now_ms));
            cl.fold(milestone, now_ms);
            cl.clone()
        };
        let decision = self.apply_event(
            BOOT_EPISODE_KEY,
            EpisodeRole::Open,
            Severity::Low,
            0,
            now_ms,
            cfg,
        );
        (decision, checklist)
    }

    /// Post-fold checklist snapshot (`None` when no boot bubble is live).
    #[must_use]
    pub fn boot_checklist(&self) -> Option<BootChecklist> {
        self.lock_boot().clone()
    }

    /// Clears the pending-delivery flag after a successfully-applied
    /// send/edit (or a byte-identical hash-skip).
    pub fn mark_boot_delivered(&self) {
        if let Some(cl) = self.lock_boot().as_mut() {
            cl.dirty = false;
        }
    }

    /// Ticker re-drive probe: a dirty checklist plus the current bubble
    /// message id (`None` id = the first page never landed → fresh send).
    #[must_use]
    pub fn boot_redrive_candidate(&self) -> Option<(BootChecklist, Option<i64>)> {
        let checklist = {
            let boot = self.lock_boot();
            match boot.as_ref() {
                Some(cl) if cl.dirty => cl.clone(),
                _ => return None,
            }
        };
        let message_id = self
            .lock_inner()
            .get(&BOOT_EPISODE_KEY)
            .and_then(|st| st.message_id);
        Some((checklist, message_id))
    }

    /// Silently retires a fully-delivered boot bubble (no edit, no
    /// tombstone). Returns `true` when retirement happened.
    ///
    /// - The one-per-boot MAIN bubble retires [`BOOT_BUBBLE_RETIRE_SECS`]
    ///   after `Complete`; an incomplete boot never retires in-process —
    ///   the bubble honestly keeps showing ⏳ while the untouched
    ///   CRITICAL/HIGH failure pages own loudness. Retiring it latches
    ///   `boot_completed_retired`, so later boot-keyed events open MINI
    ///   (feed-update) bubbles.
    /// - A MINI bubble never sees `Complete` (`StartupComplete` fires
    ///   once per process), so it retires after the SAME window of
    ///   delivered idleness since its last fold (hostile-review fix
    ///   2026-07-09 — no permanently-stuck mid-day bubble).
    #[must_use]
    pub fn retire_boot(&self, now_ms: u64) -> bool {
        let window_ms = BOOT_BUBBLE_RETIRE_SECS.saturating_mul(MS_PER_SEC);
        let (retire, completed) = {
            let boot = self.lock_boot();
            match boot.as_ref() {
                Some(cl) if !cl.dirty => {
                    let complete_elapsed = cl
                        .complete_at_ms
                        .is_some_and(|t| now_ms.saturating_sub(t) >= window_ms);
                    let mini_idle = cl.mini && now_ms.saturating_sub(cl.last_fold_ms) >= window_ms;
                    (complete_elapsed || mini_idle, cl.is_complete())
                }
                _ => (false, false),
            }
        };
        if retire {
            if completed {
                self.boot_completed_retired
                    .store(true, std::sync::atomic::Ordering::Relaxed);
            }
            *self.lock_boot() = None;
            let _ = self.lock_inner().remove(&BOOT_EPISODE_KEY);
            let _ = self.lock_tombstones().remove(&BOOT_EPISODE_KEY);
        }
        retire
    }

    /// Applies one incoming event: runs the pure FSM against the current
    /// state, mutates the registry accordingly, and returns the action plus
    /// a post-mutation state snapshot for rendering.
    ///
    /// `attempts_hint` carries reconnect-attempt context from the event when
    /// available (0 = unknown → the folded event count is used instead).
    #[must_use]
    pub fn apply_event(
        &self,
        key: EpisodeKey,
        role: EpisodeRole,
        severity: Severity,
        attempts_hint: u32,
        now_ms: u64,
        cfg: &EpisodeConfig,
    ) -> EpisodeDecision {
        let mut inner = self.lock_inner();
        let mut tombstones = self.lock_tombstones();
        let action = next_episode_action(
            inner.get(&key),
            tombstones.get(&key),
            role,
            severity,
            now_ms,
            cfg,
        );
        // A fresh first page decided WHILE live state exists == the
        // severity-escalation edge (Low-peak episode crossed into High).
        let escalated = matches!(action, EpisodeAction::SendFirstPage) && inner.contains_key(&key);

        let mut reopened = false;
        let state = match inner.get_mut(&key) {
            Some(st) => {
                st.last_event_ms = now_ms;
                st.severity_peak = st.severity_peak.max(severity);
                match role {
                    EpisodeRole::Open | EpisodeRole::Progress => {
                        st.occurrences = st.occurrences.saturating_add(1);
                        st.phase = EpisodePhase::Down;
                    }
                    EpisodeRole::Resolve => {
                        st.phase = EpisodePhase::Recovering;
                    }
                }
                if attempts_hint > 0 {
                    st.attempts = st.attempts.max(attempts_hint);
                } else {
                    st.attempts = st.attempts.saturating_add(1);
                }
                st.clone()
            }
            None => match action {
                EpisodeAction::Ignore | EpisodeAction::SendLegacy => {
                    // Resolve with no state — synthesize a throwaway
                    // snapshot; the caller renders nothing (Ignore) or
                    // routes the event through the legacy immediate lane
                    // (SendLegacy) without creating episode state.
                    EpisodeState::open(key, severity, now_ms)
                }
                _ => {
                    let mut st = EpisodeState::open(key, severity, now_ms);
                    if let EpisodeAction::Edit { message_id, .. } = action {
                        // Tombstone reopen: same bubble, already explained.
                        st.message_id = Some(message_id);
                        st.explained = true;
                        reopened = true;
                        tombstones.remove(&key);
                    }
                    if attempts_hint > 0 {
                        st.attempts = attempts_hint;
                    }
                    inner.insert(key, st.clone());
                    st
                }
            },
        };
        EpisodeDecision {
            action,
            state,
            reopened,
            escalated,
        }
    }

    /// Records the Telegram message id captured from a fresh send and marks
    /// the explanation paragraph as delivered.
    pub fn record_sent(&self, key: EpisodeKey, message_id: Option<i64>, now_ms: u64) {
        let mut inner = self.lock_inner();
        if let Some(st) = inner.get_mut(&key) {
            if message_id.is_some() {
                st.message_id = message_id;
            }
            st.explained = true;
            st.last_edit_ms = now_ms;
            st.edit_failures = 0;
        }
    }

    /// Records a successfully-applied edit (or a benign not-modified noop).
    pub fn record_edit_applied(&self, key: EpisodeKey, now_ms: u64, render_hash: u64) {
        let mut inner = self.lock_inner();
        if let Some(st) = inner.get_mut(&key) {
            st.last_edit_ms = now_ms;
            st.last_render_hash = render_hash;
            st.edit_failures = 0;
        }
    }

    /// Records a transient edit failure; returns the new consecutive count.
    #[must_use]
    pub fn record_edit_failure(&self, key: EpisodeKey) -> u8 {
        let mut inner = self.lock_inner();
        match inner.get_mut(&key) {
            Some(st) => {
                st.edit_failures = st.edit_failures.saturating_add(1);
                st.edit_failures
            }
            None => 0,
        }
    }

    /// Replaces the bubble message id after a fallback fresh send.
    pub fn replace_message_id(&self, key: EpisodeKey, message_id: Option<i64>, now_ms: u64) {
        let mut inner = self.lock_inner();
        if let Some(st) = inner.get_mut(&key) {
            st.message_id = message_id;
            st.last_edit_ms = now_ms;
            st.edit_failures = 0;
        }
    }

    /// Returns the render hash last applied for `key` (0 = none).
    #[must_use]
    pub fn last_render_hash(&self, key: EpisodeKey) -> u64 {
        self.lock_inner()
            .get(&key)
            .map_or(0, |st| st.last_render_hash)
    }

    /// Stability promotion + stale-Down expiry — called from the drain
    /// ticker every ~10s.
    ///
    /// Promotes `Recovering` episodes quiet for `cfg.stability_secs` to
    /// CLOSED: removes them, writes a [`ClosedTombstone`] (kept
    /// `cfg.reopen_secs`, then GC'd here too) and returns the closed states
    /// so the caller can issue the final green close edit.
    ///
    /// Additionally EXPIRES `Down` episodes that received NO event for
    /// `cfg.down_stale_expire_secs` (hostile-review fix 2026-07-07 — the
    /// restart edge): expired states are removed with NO tombstone, so a
    /// later outage the same day opens a FRESH first page (with the
    /// SNS-SMS leg) instead of silently editing an hours-old bubble.
    #[must_use]
    pub fn tick(&self, now_ms: u64, cfg: &EpisodeConfig) -> EpisodeTickOutcome {
        let mut inner = self.lock_inner();
        let mut tombstones = self.lock_tombstones();
        let stability_ms = cfg.stability_secs.saturating_mul(MS_PER_SEC);
        let closed_keys: Vec<EpisodeKey> = inner
            .iter()
            .filter(|(_, st)| {
                // Boot bubbles never green-promote (they close via the
                // checklist's Complete render + silent retire_boot).
                st.key.family != EpisodeFamily::Boot
                    && st.phase == EpisodePhase::Recovering
                    && now_ms.saturating_sub(st.last_event_ms) >= stability_ms
            })
            .map(|(k, _)| *k)
            .collect();

        let mut closed: Vec<EpisodeState> = Vec::with_capacity(closed_keys.len());
        for key in closed_keys {
            if let Some(st) = inner.remove(&key) {
                if let Some(id) = st.message_id {
                    tombstones.insert(
                        key,
                        ClosedTombstone {
                            message_id: id,
                            closed_at_ms: now_ms,
                            severity_peak: st.severity_peak,
                        },
                    );
                }
                closed.push(st);
            }
        }

        // Stale-Down expiry: an event-less Down episode is no longer a
        // live incident view (typical cause: restart whose clean first
        // connect emits no recovery event). NO tombstone on purpose.
        let stale_ms = cfg.down_stale_expire_secs.saturating_mul(MS_PER_SEC);
        let expired_keys: Vec<EpisodeKey> = inner
            .iter()
            .filter(|(_, st)| {
                // Boot bubbles never stale-expire — a hung boot keeps
                // honestly showing ⏳ (retire_boot owns the happy path).
                st.key.family != EpisodeFamily::Boot
                    && st.phase == EpisodePhase::Down
                    && now_ms.saturating_sub(st.last_event_ms) >= stale_ms
            })
            .map(|(k, _)| *k)
            .collect();
        let mut expired: Vec<EpisodeState> = Vec::with_capacity(expired_keys.len());
        for key in expired_keys {
            if let Some(st) = inner.remove(&key) {
                expired.push(st);
            }
        }

        // GC stale tombstones past the reopen window.
        let reopen_ms = cfg.reopen_secs.saturating_mul(MS_PER_SEC);
        tombstones.retain(|_, t| now_ms.saturating_sub(t.closed_at_ms) <= reopen_ms);
        EpisodeTickOutcome { closed, expired }
    }

    /// Snapshot of every live episode (for the persistence shell).
    #[must_use]
    pub fn snapshot(&self) -> Vec<EpisodeState> {
        self.lock_inner().values().cloned().collect()
    }

    /// Rehydrates the registry from decoded snapshot entries (boot path).
    pub fn rehydrate(&self, entries: Vec<EpisodeState>) {
        let mut inner = self.lock_inner();
        for st in entries {
            inner.entry(st.key).or_insert(st);
        }
    }

    /// Number of live episodes (test + observability helper).
    #[must_use]
    pub fn live_count(&self) -> usize {
        self.lock_inner().len()
    }
}

// ---------------------------------------------------------------------------
// FNV-1a render hash (skip no-op edits)
// ---------------------------------------------------------------------------

/// FNV-1a 64-bit hash of a rendered message — used to skip edits whose text
/// is byte-identical to the last applied render.
#[must_use]
pub fn fnv1a_hash(s: &str) -> u64 {
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = FNV_OFFSET;
    for byte in s.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

// ---------------------------------------------------------------------------
// Pure renderers
// ---------------------------------------------------------------------------

/// Render context injected by the shell — the pure core never reads a clock.
#[derive(Debug, Clone, Copy)]
pub struct EpisodeRenderCtx {
    /// Wall-clock epoch milliseconds at render time (injected).
    pub now_ms: u64,
}

/// Formats an epoch-ms instant as IST 12-hour wall-clock ("10:02 AM").
/// Pure — no clock reads. Commandment 9: IST 12-hour, never 24h/ISO.
#[must_use]
pub fn format_ist_12h(epoch_ms: u64) -> String {
    let secs = (epoch_ms / MS_PER_SEC) as i64;
    let ist = chrono::FixedOffset::east_opt(tickvault_common::constants::IST_UTC_OFFSET_SECONDS);
    match (
        ist,
        chrono::DateTime::<chrono::Utc>::from_timestamp(secs, 0),
    ) {
        (Some(offset), Some(utc)) => utc.with_timezone(&offset).format("%-I:%M %p").to_string(),
        // Unreachable in practice (offset is a valid constant; epoch-ms
        // divided by 1000 always fits) — honest fallback, never a panic.
        _ => "?:?? --".to_string(),
    }
}

/// Human duration — "28m 40s", "1h 05m", "45s".
#[must_use]
pub fn format_duration_human(total_secs: u64) -> String {
    const SECS_PER_MIN: u64 = 60;
    const SECS_PER_HOUR: u64 = 3600;
    if total_secs >= SECS_PER_HOUR {
        let hours = total_secs / SECS_PER_HOUR;
        let mins = (total_secs % SECS_PER_HOUR) / SECS_PER_MIN;
        format!("{hours}h {mins:02}m")
    } else if total_secs >= SECS_PER_MIN {
        let mins = total_secs / SECS_PER_MIN;
        let secs = total_secs % SECS_PER_MIN;
        format!("{mins}m {secs:02}s")
    } else {
        format!("{total_secs}s")
    }
}

/// Truncates to at most `max_chars` characters, preferring a newline
/// boundary. Never splits a char; never panics.
fn truncate_at_newline_boundary(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut out = String::new();
    let mut kept = 0_usize;
    for line in text.split_inclusive('\n') {
        let line_chars = line.chars().count();
        if kept.saturating_add(line_chars) > max_chars {
            break;
        }
        out.push_str(line);
        kept = kept.saturating_add(line_chars);
    }
    if out.is_empty() {
        // Single huge line — hard char cut.
        out = text.chars().take(max_chars).collect();
    }
    out
}

/// Renders the episode FIRST page — the existing full event body (the
/// explanation paragraph, "Likely source" block, etc.), defensively
/// truncated at a newline boundary to [`EPISODE_FIRST_PAGE_MAX_CHARS`] so
/// the first page is ALWAYS a single Telegram chunk.
#[must_use]
pub fn render_episode_first_page(event_body: &str) -> String {
    truncate_at_newline_boundary(event_body, EPISODE_FIRST_PAGE_MAX_CHARS)
}

fn render_status_lines(state: &EpisodeState, now_line: &str) -> String {
    let badge = state.key.family.badge();
    let desc = state.key.family.feed_desc();
    let since = format_ist_12h(state.opened_at_ms);
    let text = format!(
        "{badge} — {desc} DOWN\nSince {since} IST · {occ} drops · {att} reconnect attempts\n{now_line}",
        occ = state.occurrences,
        att = state.attempts,
    );
    // Defensive ceiling — never panics, never chunks.
    let mut out: String = text.chars().take(EPISODE_STEADY_MAX_CHARS).collect();
    // Keep the line ceiling honest even if inputs somehow carried newlines.
    let max_newlines = EPISODE_STEADY_MAX_LINES.saturating_sub(1);
    if out.matches('\n').count() > max_newlines {
        out = out
            .split('\n')
            .take(EPISODE_STEADY_MAX_LINES)
            .collect::<Vec<_>>()
            .join("\n");
    }
    out
}

/// Steady-state render — at most [`EPISODE_STEADY_MAX_LINES`] lines and
/// [`EPISODE_STEADY_MAX_CHARS`] chars, live counters only, no boilerplate.
#[must_use]
pub fn render_episode_steady(state: &EpisodeState, _ctx: &EpisodeRenderCtx) -> String {
    render_status_lines(state, "Now: reconnecting automatically")
}

/// Recovering render — same live counters, amber "confirming" tail.
#[must_use]
pub fn render_episode_recovering(state: &EpisodeState, _ctx: &EpisodeRenderCtx) -> String {
    render_status_lines(state, "Now: reconnected — confirming…")
}

/// The feed description for the recovered / stale-closed close lines —
/// carries the SAME broker badge the DOWN bubble led with (operator
/// directive 2026-07-14: recovered / edge-cleared messages must carry the
/// same tag as their trigger). The Boot family keeps its plain
/// description (🚀 is a milestone marker, not a broker tag).
fn badged_feed_desc(family: EpisodeFamily) -> String {
    match family {
        EpisodeFamily::Boot => family.feed_desc().to_string(),
        // "{badge} — {desc}" — the SAME format the DOWN bubble's status
        // lines lead with (render_status_lines), so the close line reads
        // identically at a glance.
        f => format!("{} — {}", f.badge(), f.feed_desc()),
    }
}

/// Neutral close line for a stale-expired Down episode — the incident
/// stopped reporting (typical cause: a restart whose clean first connect
/// emits no recovery event). ONE line, plain English, IST 12-hour time.
#[must_use]
pub fn render_episode_stale_closed(state: &EpisodeState) -> String {
    let last = format_ist_12h(state.last_event_ms);
    format!(
        "\u{26aa} {desc} alert closed — no updates since {last} IST. Any new problem will open a fresh alert.",
        desc = badged_feed_desc(state.key.family),
    )
}

/// Final green close line — ONE line, no newlines.
#[must_use]
pub fn render_episode_recovered(state: &EpisodeState, ctx: &EpisodeRenderCtx) -> String {
    let down_secs = ctx
        .now_ms
        .saturating_sub(state.opened_at_ms)
        .checked_div(MS_PER_SEC)
        .unwrap_or(0);
    let opened = format_ist_12h(state.opened_at_ms);
    let closed = format_ist_12h(ctx.now_ms);
    format!(
        "\u{2705} Recovered — {desc} back. Down {dur}, {att} attempts ({opened}–{closed} IST)",
        desc = badged_feed_desc(state.key.family),
        dur = format_duration_human(down_secs),
        att = state.attempts,
    )
}

/// Renders the consolidated boot bubble (2026-07-09) — a fixed-template
/// checklist that the shell live-edits in place as milestones fold in.
///
/// Commandments hold: plain English, IST 12-hour times, emoji status, no
/// library names, no file paths. The `Build:` short-sha is the conscious
/// B9 operator-approved override of the no-version-numbers rule. Ceiling:
/// [`BOOT_BUBBLE_MAX_CHARS`] via the newline-boundary truncation.
#[must_use]
pub fn render_boot_checklist(cl: &BootChecklist, _ctx: &EpisodeRenderCtx) -> String {
    let sha_ok = !cl.build_sha_short.is_empty() && cl.build_sha_short != "unknown";
    // Never a false "NEW CODE" claim without a valid sha pair upstream.
    let new_code = cl.new_code && sha_ok;
    let build = if sha_ok {
        if new_code && cl.is_complete() {
            format!(" (build {}, new code)", cl.build_sha_short)
        } else {
            format!(" (build {})", cl.build_sha_short)
        }
    } else {
        String::new()
    };
    let header = match cl.complete_at_ms {
        Some(done_ms) => {
            // The literal phrase "tickvault started" is load-bearing — the
            // operator's morning-readiness instructions reference it.
            format!(
                "<b>\u{2705} tickvault started — boot complete {when}{build}</b>",
                when = format_ist_12h(done_ms),
            )
        }
        None if cl.mini => {
            // Mid-day feed/component restart AFTER a completed boot
            // retired (hostile-review fix 2026-07-09): the system is NOT
            // starting — claiming "tickvault starting" here would be a
            // false signal. Neutral header, timestamped to the latest
            // update; retires on the idle window (see `retire_boot`).
            format!(
                "<b>\u{1f504} Feed update {when}</b>",
                when = format_ist_12h(cl.last_fold_ms),
            )
        }
        None => {
            let deploy = if new_code {
                " — NEW CODE deployed"
            } else {
                ""
            };
            format!("<b>\u{1f680} tickvault starting{deploy}{build}</b>")
        }
    };

    let (expect_dhan, expect_groww) = cl.expectations.unwrap_or((false, false));
    let mut lines: Vec<String> = Vec::new();
    let started = format_ist_12h(cl.started_at_ms);
    if !cl.mini {
        match cl.services {
            Some((healthy, total)) => {
                lines.push(format!(
                    "\u{2705} Started {started} — services {healthy} of {total}"
                ));
            }
            None => lines.push(format!("\u{2705} Started {started}")),
        }
    }
    if cl.dhan_auth {
        lines.push("\u{2705} Dhan login".to_string());
    } else if expect_dhan {
        lines.push("\u{23f3} Dhan login".to_string());
    }
    match cl.instruments {
        Some(count) => lines.push(format!("\u{2705} Instruments — {count} tracked")),
        None if expect_dhan => lines.push("\u{23f3} Instruments".to_string()),
        None => {}
    }
    match cl.dhan_feed {
        Some(BootDhanFeed::Online {
            connected,
            total,
            last_real_tick_age_secs,
        }) => {
            // No-false-OK: connected without a REAL price keeps the caveat.
            let tail = match last_real_tick_age_secs {
                Some(age) => format!(", last real price {age}s ago"),
                None => ", \u{26a0}\u{fe0f} no real prices yet".to_string(),
            };
            lines.push(format!(
                "\u{2705} Dhan price feed — {connected} of {total} connections{tail}"
            ));
        }
        Some(BootDhanFeed::DeferredOffHours) => {
            lines.push("\u{1f319} Dhan price feed — waiting for market open".to_string());
        }
        None if expect_dhan => lines.push("\u{23f3} Dhan price feed".to_string()),
        None => {}
    }
    if cl.order_update_authenticated {
        lines.push("\u{2705} Order confirmations — connected and confirmed".to_string());
    } else if cl.order_update_connected {
        lines.push("\u{2705} Order confirmations — connected".to_string());
    } else if expect_dhan {
        lines.push("\u{23f3} Order confirmations".to_string());
    }
    let groww_has_data =
        cl.groww_auth || cl.groww_instruments.is_some() || cl.groww_connected.is_some();
    if groww_has_data {
        let mut parts: Vec<String> = Vec::new();
        if cl.groww_auth {
            parts.push("signed in".to_string());
        }
        if let Some(subscribed) = cl.groww_instruments {
            parts.push(format!("{subscribed} instruments"));
        }
        if let Some(market_open) = cl.groww_connected {
            parts.push(if market_open {
                "connected — waiting for first price".to_string()
            } else {
                "connected (market closed — quiet is normal)".to_string()
            });
        }
        lines.push(format!("\u{2705} Groww feed — {}", parts.join(" · ")));
    } else if expect_groww {
        lines.push("\u{23f3} Groww feed".to_string());
    }
    match cl.complete_at_ms {
        Some(done_ms) => {
            let took_secs = done_ms
                .saturating_sub(cl.started_at_ms)
                .checked_div(MS_PER_SEC)
                .unwrap_or(0);
            let mode_part = cl
                .mode
                .map_or_else(String::new, |m| format!("Mode: {m} \u{b7} "));
            lines.push(format!(
                "{mode_part}Boot took {took}. You're good to go.",
                took = format_duration_human(took_secs),
            ));
        }
        None if cl.mini => {
            // No "Boot finishing…" promise on a mini bubble — nothing is
            // finishing (`StartupComplete` never re-fires mid-process),
            // so the footer would be a broken promise. The component
            // lines above ARE the whole message.
        }
        None => {
            lines.push("\u{23f3} Boot finishing\u{2026}".to_string());
            lines.push(String::new());
            lines.push("This message updates itself as each part comes up.".to_string());
        }
    }
    let text = format!("{header}\n\n{}", lines.join("\n"));
    truncate_at_newline_boundary(&text, BOOT_BUBBLE_MAX_CHARS)
}

// ---------------------------------------------------------------------------
// Snapshot codec (pure; fail-open)
// ---------------------------------------------------------------------------

/// Pure JSON codec for the advisory episode snapshot file.
///
/// Serialized fields ONLY: `{family, conn, message_id, opened_at_ms,
/// occurrences, attempts, explained, phase, severity_peak}` — no reasons,
/// no secrets.
pub mod episode_snapshot {
    use super::{
        EPISODE_REHYDRATE_MAX_AGE_SECS, EpisodeFamily, EpisodeKey, EpisodePhase, EpisodeState,
        MS_PER_SEC, Severity,
    };

    /// Encodes live episode states to the snapshot JSON string.
    ///
    /// The `Boot` family is FILTERED OUT (2026-07-09): a new process must
    /// never rehydrate + edit the previous process's boot bubble — every
    /// boot opens its own fresh checklist bubble.
    #[must_use]
    pub fn encode(entries: &[EpisodeState]) -> String {
        let items: Vec<serde_json::Value> = entries
            .iter()
            .filter(|st| st.key.family != EpisodeFamily::Boot)
            .map(|st| {
                serde_json::json!({
                    "family": st.key.family.snapshot_label(),
                    "conn": st.key.conn,
                    "message_id": st.message_id,
                    "opened_at_ms": st.opened_at_ms,
                    "occurrences": st.occurrences,
                    "attempts": st.attempts,
                    "explained": st.explained,
                    "phase": st.phase.snapshot_label(),
                    "severity_peak": st.severity_peak.as_label(),
                })
            })
            .collect();
        serde_json::Value::Array(items).to_string()
    }

    /// Decodes a snapshot, dropping entries that are older than
    /// [`EPISODE_REHYDRATE_MAX_AGE_SECS`] OR from a previous IST day.
    /// Corrupt JSON / unknown labels → dropped fail-open (empty vec worst
    /// case) — never a panic, never gates boot.
    #[must_use]
    pub fn decode(json: &str, now_ms: u64, today_ist: chrono::NaiveDate) -> Vec<EpisodeState> {
        let Ok(value) = serde_json::from_str::<serde_json::Value>(json) else {
            return Vec::new();
        };
        let Some(items) = value.as_array() else {
            return Vec::new();
        };
        let max_age_ms = EPISODE_REHYDRATE_MAX_AGE_SECS.saturating_mul(MS_PER_SEC);
        let mut out = Vec::new();
        for item in items {
            let Some(family) = item
                .get("family")
                .and_then(|v| v.as_str())
                .and_then(EpisodeFamily::from_snapshot_label)
            else {
                continue;
            };
            // Belt-and-braces: even a hand-written "boot" entry is dropped
            // (encode already filters the family out).
            if family == EpisodeFamily::Boot {
                continue;
            }
            let Some(conn) = item
                .get("conn")
                .and_then(serde_json::Value::as_u64)
                .and_then(|v| u8::try_from(v).ok())
            else {
                continue;
            };
            let Some(opened_at_ms) = item.get("opened_at_ms").and_then(serde_json::Value::as_u64)
            else {
                continue;
            };
            // Age bound (robustness graft).
            if now_ms.saturating_sub(opened_at_ms) > max_age_ms {
                continue;
            }
            // Same-IST-day bound (robustness graft) — an episode from a
            // previous trading day is never resumed.
            if !is_same_ist_day(opened_at_ms, today_ist) {
                continue;
            }
            let phase = item
                .get("phase")
                .and_then(|v| v.as_str())
                .and_then(EpisodePhase::from_snapshot_label)
                .unwrap_or(EpisodePhase::Down);
            let severity_peak = item
                .get("severity_peak")
                .and_then(|v| v.as_str())
                .and_then(severity_from_label)
                // Missing/unknown label (legacy file) → Low, so a later
                // High/Critical event pages FRESH via the escalation edge
                // (duplicate-over-drop: a repeat page beats silence).
                .unwrap_or(Severity::Low);
            let mut st =
                EpisodeState::open(EpisodeKey { family, conn }, severity_peak, opened_at_ms);
            st.message_id = item.get("message_id").and_then(serde_json::Value::as_i64);
            st.occurrences = item
                .get("occurrences")
                .and_then(serde_json::Value::as_u64)
                .and_then(|v| u32::try_from(v).ok())
                .unwrap_or(1);
            st.attempts = item
                .get("attempts")
                .and_then(serde_json::Value::as_u64)
                .and_then(|v| u32::try_from(v).ok())
                .unwrap_or(0);
            st.explained = item
                .get("explained")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(true);
            st.phase = phase;
            st.last_event_ms = opened_at_ms;
            out.push(st);
        }
        out
    }

    /// Inverse of [`Severity::as_label`] for the snapshot file only.
    /// Unknown → `None` (caller defaults Low — fail-loud on re-page).
    fn severity_from_label(label: &str) -> Option<Severity> {
        match label {
            "info" => Some(Severity::Info),
            "low" => Some(Severity::Low),
            "medium" => Some(Severity::Medium),
            "high" => Some(Severity::High),
            "critical" => Some(Severity::Critical),
            _ => None,
        }
    }

    fn is_same_ist_day(epoch_ms: u64, today_ist: chrono::NaiveDate) -> bool {
        let secs = (epoch_ms / MS_PER_SEC) as i64;
        let Some(offset) =
            chrono::FixedOffset::east_opt(tickvault_common::constants::IST_UTC_OFFSET_SECONDS)
        else {
            return false;
        };
        let Some(utc) = chrono::DateTime::<chrono::Utc>::from_timestamp(secs, 0) else {
            return false;
        };
        utc.with_timezone(&offset).date_naive() == today_ist
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::arithmetic_side_effects)] // APPROVED: test code
mod tests {
    use super::*;

    const NOW: u64 = 1_751_900_000_000; // arbitrary epoch ms anchor

    fn key() -> EpisodeKey {
        EpisodeKey {
            family: EpisodeFamily::MainFeedWs,
            conn: 0,
        }
    }

    fn cfg() -> EpisodeConfig {
        EpisodeConfig::default()
    }

    fn live_state(message_id: Option<i64>) -> EpisodeState {
        let mut st = EpisodeState::open(key(), Severity::High, NOW);
        st.message_id = message_id;
        st
    }

    // -- FSM --------------------------------------------------------------

    #[test]
    fn test_first_high_event_opens_episode_with_send_new() {
        for sev in [Severity::High, Severity::Critical] {
            let action = next_episode_action(None, None, EpisodeRole::Open, sev, NOW, &cfg());
            assert_eq!(
                action,
                EpisodeAction::SendFirstPage,
                "no state + Open at {sev:?} must open with the preserved bypass first page"
            );
        }
    }

    #[test]
    fn test_repeat_progress_edits_not_sends() {
        let mut st = live_state(Some(42));
        st.last_edit_ms = NOW - 60_000; // outside throttle
        let action = next_episode_action(
            Some(&st),
            None,
            EpisodeRole::Progress,
            Severity::High,
            NOW,
            &cfg(),
        );
        assert_eq!(
            action,
            EpisodeAction::Edit {
                message_id: 42,
                close: false
            },
            "repeat events must edit the SAME bubble, never a second first page"
        );
    }

    #[test]
    fn test_resolve_enters_recovering_not_instant_green() {
        let st = live_state(Some(42));
        let action = next_episode_action(
            Some(&st),
            None,
            EpisodeRole::Resolve,
            Severity::Medium,
            NOW,
            &cfg(),
        );
        // Edit with close:false — never an instant green close.
        assert_eq!(
            action,
            EpisodeAction::Edit {
                message_id: 42,
                close: false
            }
        );
        // Registry-side: phase flips to Recovering.
        let reg = EpisodeRegistry::new();
        let _ = reg.apply_event(key(), EpisodeRole::Open, Severity::High, 0, NOW, &cfg());
        reg.record_sent(key(), Some(42), NOW);
        let d = reg.apply_event(
            key(),
            EpisodeRole::Resolve,
            Severity::Medium,
            0,
            NOW + 1000,
            &cfg(),
        );
        assert_eq!(d.state.phase, EpisodePhase::Recovering);
    }

    #[test]
    fn test_recovering_promotes_to_closed_after_60s_stability_tick() {
        let reg = EpisodeRegistry::new();
        let _ = reg.apply_event(key(), EpisodeRole::Open, Severity::High, 0, NOW, &cfg());
        reg.record_sent(key(), Some(42), NOW);
        let _ = reg.apply_event(
            key(),
            EpisodeRole::Resolve,
            Severity::Medium,
            0,
            NOW + 1000,
            &cfg(),
        );
        // Before stability: no close.
        let outcome = reg.tick(NOW + 1000 + 30_000, &cfg());
        assert!(
            outcome.closed.is_empty(),
            "must not close inside stability window"
        );
        assert!(outcome.expired.is_empty());
        // After stability: closed + tombstoned.
        let outcome = reg.tick(NOW + 1000 + EPISODE_STABILITY_SECS * 1000, &cfg());
        let closed = outcome.closed;
        assert_eq!(closed.len(), 1);
        assert_eq!(closed[0].message_id, Some(42));
        assert_eq!(reg.live_count(), 0);
        // Reopen inside the window folds into the tombstone bubble.
        let now2 = NOW + 1000 + EPISODE_STABILITY_SECS * 1000 + 5_000;
        let d = reg.apply_event(key(), EpisodeRole::Open, Severity::High, 0, now2, &cfg());
        assert_eq!(
            d.action,
            EpisodeAction::Edit {
                message_id: 42,
                close: false
            }
        );
        assert!(d.reopened);
    }

    #[test]
    fn test_flap_during_recovering_reverts_same_bubble_to_down() {
        let reg = EpisodeRegistry::new();
        let _ = reg.apply_event(key(), EpisodeRole::Open, Severity::High, 0, NOW, &cfg());
        reg.record_sent(key(), Some(7), NOW);
        let _ = reg.apply_event(
            key(),
            EpisodeRole::Resolve,
            Severity::Medium,
            0,
            NOW + 1000,
            &cfg(),
        );
        // Flap: disconnect again during Recovering — SAME message id, Down.
        let d = reg.apply_event(
            key(),
            EpisodeRole::Open,
            Severity::High,
            0,
            NOW + 2000,
            &cfg(),
        );
        assert_eq!(
            d.action,
            EpisodeAction::Edit {
                message_id: 7,
                close: false
            }
        );
        assert_eq!(d.state.phase, EpisodePhase::Down);
    }

    #[test]
    fn test_reopen_within_120s_reuses_tombstone_message_id() {
        let t = ClosedTombstone {
            message_id: 99,
            closed_at_ms: NOW,
            severity_peak: Severity::High,
        };
        // Inside the reopen window → SAME bubble.
        let action = next_episode_action(
            None,
            Some(&t),
            EpisodeRole::Open,
            Severity::High,
            NOW + EPISODE_REOPEN_SECS * 1000,
            &cfg(),
        );
        assert_eq!(
            action,
            EpisodeAction::Edit {
                message_id: 99,
                close: false
            }
        );
        // Past the window → fresh first page.
        let action = next_episode_action(
            None,
            Some(&t),
            EpisodeRole::Open,
            Severity::High,
            NOW + EPISODE_REOPEN_SECS * 1000 + 1,
            &cfg(),
        );
        assert_eq!(action, EpisodeAction::SendFirstPage);
    }

    #[test]
    fn test_edit_throttle_folds_without_network_action() {
        let mut st = live_state(Some(5));
        st.last_edit_ms = NOW;
        let action = next_episode_action(
            Some(&st),
            None,
            EpisodeRole::Progress,
            Severity::High,
            NOW + (EPISODE_EDIT_MIN_INTERVAL_SECS * 1000) - 1,
            &cfg(),
        );
        assert_eq!(action, EpisodeAction::EditThrottled);
        // Counters still advance in the registry even when throttled.
        let reg = EpisodeRegistry::new();
        let _ = reg.apply_event(key(), EpisodeRole::Open, Severity::High, 0, NOW, &cfg());
        reg.record_sent(key(), Some(5), NOW);
        let d = reg.apply_event(
            key(),
            EpisodeRole::Progress,
            Severity::High,
            0,
            NOW + 1000,
            &cfg(),
        );
        assert_eq!(d.action, EpisodeAction::EditThrottled);
        assert_eq!(d.state.occurrences, 2);
    }

    #[test]
    fn test_no_message_id_falls_back_to_send_new() {
        let st = live_state(None);
        for role in [
            EpisodeRole::Open,
            EpisodeRole::Progress,
            EpisodeRole::Resolve,
        ] {
            let action = next_episode_action(Some(&st), None, role, Severity::High, NOW, &cfg());
            assert_eq!(action, EpisodeAction::SendNewFallback, "role {role:?}");
        }
    }

    #[test]
    fn test_resolve_with_no_state_routes_send_legacy_and_fresh_tombstone_ignores() {
        // No state, no tombstone — a recovery for an episode we never
        // tracked (restart lost the snapshot) routes through the LEGACY
        // immediate lane instead of being dropped (hostile-review fix).
        let action = next_episode_action(
            None,
            None,
            EpisodeRole::Resolve,
            Severity::Medium,
            NOW,
            &cfg(),
        );
        assert_eq!(action, EpisodeAction::SendLegacy);
        // Fresh tombstone — the green close already announced the
        // recovery; a second line would be noise.
        let t = ClosedTombstone {
            message_id: 9,
            closed_at_ms: NOW,
            severity_peak: Severity::High,
        };
        let action = next_episode_action(
            None,
            Some(&t),
            EpisodeRole::Resolve,
            Severity::Medium,
            NOW + 1000,
            &cfg(),
        );
        assert_eq!(action, EpisodeAction::Ignore);
        // Stale tombstone (past the reopen window) — legacy lane again.
        let action = next_episode_action(
            None,
            Some(&t),
            EpisodeRole::Resolve,
            Severity::Medium,
            NOW + EPISODE_REOPEN_SECS * 1000 + 1,
            &cfg(),
        );
        assert_eq!(action, EpisodeAction::SendLegacy);
        // Registry side: SendLegacy never creates episode state.
        let reg = EpisodeRegistry::new();
        let d = reg.apply_event(
            key(),
            EpisodeRole::Resolve,
            Severity::Medium,
            0,
            NOW,
            &cfg(),
        );
        assert_eq!(d.action, EpisodeAction::SendLegacy);
        assert_eq!(reg.live_count(), 0, "SendLegacy must not open an episode");
    }

    #[test]
    fn test_next_episode_action_escalation_low_peak_to_high_pages_fresh() {
        // Hostile-review fix: a Low-opened episode (pre-open off-hours
        // storm) crossing into High MUST page fresh — never a silent edit.
        let mut st = EpisodeState::open(key(), Severity::Low, NOW);
        st.message_id = Some(42);
        st.last_edit_ms = NOW; // even inside the edit throttle window
        for role in [EpisodeRole::Open, EpisodeRole::Progress] {
            let action =
                next_episode_action(Some(&st), None, role, Severity::High, NOW + 1000, &cfg());
            assert_eq!(
                action,
                EpisodeAction::SendFirstPage,
                "Low→High escalation must take the fresh-page bypass ({role:?})"
            );
        }
        // Same-severity repeats keep folding as edits/throttles.
        let action = next_episode_action(
            Some(&st),
            None,
            EpisodeRole::Progress,
            Severity::Low,
            NOW + 1000,
            &cfg(),
        );
        assert_eq!(action, EpisodeAction::EditThrottled);
        // A High-peak episode never re-escalates on repeat High events.
        let mut high = EpisodeState::open(key(), Severity::High, NOW);
        high.message_id = Some(43);
        high.last_edit_ms = 0;
        let action = next_episode_action(
            Some(&high),
            None,
            EpisodeRole::Progress,
            Severity::High,
            NOW + 60_000,
            &cfg(),
        );
        assert_eq!(
            action,
            EpisodeAction::Edit {
                message_id: 43,
                close: false
            }
        );
    }

    #[test]
    fn test_apply_event_escalation_marks_decision_and_updates_peak() {
        let reg = EpisodeRegistry::new();
        let _ = reg.apply_event(key(), EpisodeRole::Open, Severity::Low, 0, NOW, &cfg());
        reg.record_sent(key(), Some(42), NOW);
        let d = reg.apply_event(
            key(),
            EpisodeRole::Open,
            Severity::High,
            0,
            NOW + 1000,
            &cfg(),
        );
        assert_eq!(d.action, EpisodeAction::SendFirstPage);
        assert!(d.escalated, "escalation edge must be flagged");
        assert_eq!(d.state.severity_peak, Severity::High);
        assert_eq!(d.state.occurrences, 2, "the escalating drop still folds");
        // A plain first open is NOT an escalation.
        let reg2 = EpisodeRegistry::new();
        let d2 = reg2.apply_event(key(), EpisodeRole::Open, Severity::High, 0, NOW, &cfg());
        assert_eq!(d2.action, EpisodeAction::SendFirstPage);
        assert!(!d2.escalated);
    }

    #[test]
    fn test_tombstone_high_reopen_over_low_peak_pages_fresh() {
        // A HIGH disconnect inside the reopen window of a LOW-peak closed
        // bubble must NOT fold silently — fresh first page (with SMS leg).
        let low_tomb = ClosedTombstone {
            message_id: 7,
            closed_at_ms: NOW,
            severity_peak: Severity::Low,
        };
        let action = next_episode_action(
            None,
            Some(&low_tomb),
            EpisodeRole::Open,
            Severity::High,
            NOW + 1000,
            &cfg(),
        );
        assert_eq!(action, EpisodeAction::SendFirstPage);
        // Low reopen into the same Low tombstone still folds.
        let action = next_episode_action(
            None,
            Some(&low_tomb),
            EpisodeRole::Open,
            Severity::Low,
            NOW + 1000,
            &cfg(),
        );
        assert_eq!(
            action,
            EpisodeAction::Edit {
                message_id: 7,
                close: false
            }
        );
    }

    #[test]
    fn test_tick_expires_stale_down_and_live_count_drops() {
        // Hostile-review fix: a Down episode with no events for
        // EPISODE_DOWN_STALE_EXPIRE_SECS expires (no tombstone) so a later
        // outage opens a FRESH first page instead of editing a stale bubble.
        let reg = EpisodeRegistry::new();
        let _ = reg.apply_event(key(), EpisodeRole::Open, Severity::High, 0, NOW, &cfg());
        reg.record_sent(key(), Some(42), NOW);
        assert_eq!(reg.live_count(), 1);
        // Just under the bound: still live.
        let outcome = reg.tick(NOW + EPISODE_DOWN_STALE_EXPIRE_SECS * 1000 - 1, &cfg());
        assert!(outcome.expired.is_empty());
        assert_eq!(reg.live_count(), 1);
        // At the bound: expired, no tombstone.
        let expire_at = NOW + EPISODE_DOWN_STALE_EXPIRE_SECS * 1000;
        let outcome = reg.tick(expire_at, &cfg());
        assert_eq!(outcome.expired.len(), 1);
        assert_eq!(outcome.expired[0].message_id, Some(42));
        assert!(outcome.closed.is_empty());
        assert_eq!(reg.live_count(), 0, "expired episode leaves the registry");
        // The NEXT outage opens a fresh first page — never a stale fold.
        let d = reg.apply_event(
            key(),
            EpisodeRole::Open,
            Severity::High,
            0,
            expire_at + 1000,
            &cfg(),
        );
        assert_eq!(d.action, EpisodeAction::SendFirstPage);
    }

    // The never-drop pin: FSM is total AND High/Critical Open/Progress
    // never maps to Ignore, for arbitrary state/tombstone/clock inputs.
    proptest::proptest! {
        #[test]
        fn proptest_fsm_total_never_ignores_high_critical(
            has_state in proptest::bool::ANY,
            message_id in proptest::option::of(proptest::num::i64::ANY),
            last_edit_ms in proptest::num::u64::ANY,
            phase_recovering in proptest::bool::ANY,
            has_tombstone in proptest::bool::ANY,
            tomb_id in proptest::num::i64::ANY,
            tomb_closed_ms in proptest::num::u64::ANY,
            role_idx in 0_u8..3,
            sev_idx in 0_u8..5,
            now_ms in proptest::num::u64::ANY,
        ) {
            let severity = match sev_idx {
                0 => Severity::Info,
                1 => Severity::Low,
                2 => Severity::Medium,
                3 => Severity::High,
                _ => Severity::Critical,
            };
            let role = match role_idx {
                0 => EpisodeRole::Open,
                1 => EpisodeRole::Progress,
                _ => EpisodeRole::Resolve,
            };
            let mut st = EpisodeState::open(
                EpisodeKey { family: EpisodeFamily::OrderUpdateWs, conn: 0 },
                severity,
                now_ms,
            );
            st.message_id = message_id;
            st.last_edit_ms = last_edit_ms;
            st.phase = if phase_recovering { EpisodePhase::Recovering } else { EpisodePhase::Down };
            let state = has_state.then_some(&st);
            let tomb = ClosedTombstone { message_id: tomb_id, closed_at_ms: tomb_closed_ms, severity_peak: severity };
            let tombstone = has_tombstone.then_some(&tomb);

            // Total: never panics.
            let action = next_episode_action(state, tombstone, role, severity, now_ms, &EpisodeConfig::default());

            // Never-drop pin.
            if severity >= Severity::High
                && matches!(role, EpisodeRole::Open | EpisodeRole::Progress)
            {
                proptest::prop_assert_ne!(action, EpisodeAction::Ignore);
            }
        }
    }

    // -- Renderers ----------------------------------------------------------

    #[test]
    fn test_steady_render_max_3_lines_320_chars_adversarial() {
        let mut st = live_state(Some(1));
        st.occurrences = u32::MAX;
        st.attempts = u32::MAX;
        let ctx = EpisodeRenderCtx { now_ms: NOW };
        for rendered in [
            render_episode_steady(&st, &ctx),
            render_episode_recovering(&st, &ctx),
        ] {
            assert!(
                rendered.matches('\n').count() < EPISODE_STEADY_MAX_LINES,
                "steady render exceeded {EPISODE_STEADY_MAX_LINES} lines: {rendered:?}"
            );
            assert!(
                rendered.chars().count() <= EPISODE_STEADY_MAX_CHARS,
                "steady render exceeded {EPISODE_STEADY_MAX_CHARS} chars: {rendered:?}"
            );
        }
    }

    #[test]
    fn test_first_page_render_always_single_chunk() {
        // 10KB adversarial body — must clamp to a single Telegram chunk.
        let huge = "line of cause text that repeats forever\n".repeat(300);
        let page = render_episode_first_page(&huge);
        assert!(page.chars().count() <= EPISODE_FIRST_PAGE_MAX_CHARS);
        let chunks = crate::notification::service::split_message_for_telegram(&page);
        assert_eq!(chunks.len(), 1, "first page must always be one chunk");
        // Small bodies pass through untouched.
        assert_eq!(render_episode_first_page("small"), "small");
    }

    #[test]
    fn test_first_page_truncates_at_newline_boundary() {
        let body = format!("{}\n{}", "a".repeat(3000), "b".repeat(3000));
        let page = render_episode_first_page(&body);
        // Only the first line fits — cut must land on the newline boundary.
        assert_eq!(page, format!("{}\n", "a".repeat(3000)));
    }

    #[test]
    fn test_explanation_paragraph_only_on_first_render() {
        // The first page carries the event body verbatim (incl. the
        // "Likely source:" explanation block); the steady render never does.
        let body = "feed dropped\nLikely source: the provider — their side reset\nConfirm: check";
        let first = render_episode_first_page(body);
        assert!(first.contains("Likely source:"));
        let st = live_state(Some(1));
        let ctx = EpisodeRenderCtx { now_ms: NOW };
        let steady = render_episode_steady(&st, &ctx);
        assert!(!steady.contains("Likely source:"));
        assert!(!steady.contains("Confirm:"));
    }

    #[test]
    fn test_recovery_render_one_line_green_ist_12h() {
        let mut st = live_state(Some(1));
        st.attempts = 31;
        let ctx = EpisodeRenderCtx {
            now_ms: NOW + 28 * 60_000 + 40_000,
        };
        let line = render_episode_recovered(&st, &ctx);
        assert!(line.starts_with('\u{2705}'), "must start green: {line:?}");
        assert_eq!(line.matches('\n').count(), 0, "one line only: {line:?}");
        // IST 12-hour timestamp present (commandment 9).
        let re = regex_lite_12h(&line);
        assert!(re, "12h IST time missing: {line:?}");
        assert!(line.contains("28m 40s"), "human duration: {line:?}");
        assert!(line.contains("31 attempts"));
    }

    /// Tiny hand-rolled matcher for `H:MM AM|PM` (no regex dep).
    fn regex_lite_12h(s: &str) -> bool {
        let bytes: Vec<char> = s.chars().collect();
        for i in 0..bytes.len() {
            if bytes[i].is_ascii_digit() {
                // H or HH then ':' then MM then ' ' then AM/PM
                let rest: String = bytes[i..].iter().collect();
                let mut parts = rest.splitn(2, ':');
                let hours = parts.next().unwrap_or("");
                let tail = parts.next().unwrap_or("");
                if !hours.is_empty()
                    && hours.len() <= 2
                    && hours.chars().all(|c| c.is_ascii_digit())
                    && tail.len() >= 5
                    && tail[..2].chars().all(|c| c.is_ascii_digit())
                    && (tail[2..].starts_with(" AM") || tail[2..].starts_with(" PM"))
                {
                    return true;
                }
            }
        }
        false
    }

    #[test]
    fn test_episode_renders_pass_commandments_banned_strings() {
        // Commandments 1-4: plain English — no library names, no file
        // paths, no transport verbs, no version numbers.
        let banned = [
            "rkyv",
            "papaya",
            "mpsc",
            ".rs",
            "data/",
            "QuestDB",
            "editMessageText",
        ];
        let mut st = live_state(Some(1));
        st.occurrences = 7;
        st.attempts = 31;
        let ctx = EpisodeRenderCtx {
            now_ms: NOW + 60_000,
        };
        let renders = [
            render_episode_steady(&st, &ctx),
            render_episode_recovering(&st, &ctx),
            render_episode_recovered(&st, &ctx),
        ];
        for r in &renders {
            for b in banned {
                assert!(!r.contains(b), "banned string {b:?} in render: {r:?}");
            }
            // x.y.z version pattern scan (e.g. "1.2.3").
            let has_version = r
                .split(|c: char| !c.is_ascii_digit() && c != '.')
                .any(|tok| {
                    let dots = tok.matches('.').count();
                    dots >= 2 && tok.chars().all(|c| c.is_ascii_digit() || c == '.')
                });
            assert!(!has_version, "version-like token in render: {r:?}");
        }
    }

    #[test]
    fn test_format_duration_human_bands() {
        assert_eq!(format_duration_human(45), "45s");
        assert_eq!(format_duration_human(28 * 60 + 40), "28m 40s");
        assert_eq!(format_duration_human(3600 + 5 * 60), "1h 05m");
        assert_eq!(format_duration_human(0), "0s");
    }

    #[test]
    fn test_format_ist_12h_known_instant() {
        // 2026-07-07 04:32:00 UTC = 10:02 AM IST.
        let epoch_ms = 1_783_485_120_000_u64;
        let rendered = format_ist_12h(epoch_ms);
        assert!(
            rendered.ends_with("AM") || rendered.ends_with("PM"),
            "12h suffix: {rendered:?}"
        );
        assert!(rendered.contains(':'));
    }

    #[test]
    fn test_fnv1a_hash_distinguishes_and_is_stable() {
        assert_eq!(fnv1a_hash("abc"), fnv1a_hash("abc"));
        assert_ne!(fnv1a_hash("abc"), fnv1a_hash("abd"));
        // Known FNV-1a vector: empty string → offset basis.
        assert_eq!(fnv1a_hash(""), 0xcbf2_9ce4_8422_2325);
    }

    // -- Snapshot codec -----------------------------------------------------

    #[test]
    fn test_snapshot_roundtrip_stale_day_age_and_corrupt_fail_open() {
        let today = ist_date_of(NOW);
        let mut fresh = live_state(Some(42));
        fresh.occurrences = 7;
        fresh.attempts = 31;
        fresh.explained = true;
        fresh.phase = EpisodePhase::Recovering;

        // Round-trip identity on the persisted fields.
        let json = episode_snapshot::encode(&[fresh.clone()]);
        let decoded = episode_snapshot::decode(&json, NOW + 1000, today);
        assert_eq!(decoded.len(), 1);
        let d = &decoded[0];
        assert_eq!(d.key, fresh.key);
        assert_eq!(d.message_id, Some(42));
        assert_eq!(d.occurrences, 7);
        assert_eq!(d.attempts, 31);
        assert!(d.explained);
        assert_eq!(d.phase, EpisodePhase::Recovering);
        assert_eq!(
            d.severity_peak,
            Severity::High,
            "severity_peak persists so escalation still pages after restart"
        );

        // Legacy entry with NO severity_peak label → Low default, so a
        // later High event pages fresh (duplicate-over-drop).
        let legacy = format!(
            r#"[{{"family":"main_feed_ws","conn":0,"opened_at_ms":{NOW},"message_id":9}}]"#
        );
        let decoded_legacy = episode_snapshot::decode(&legacy, NOW + 1000, today);
        assert_eq!(decoded_legacy.len(), 1);
        assert_eq!(decoded_legacy[0].severity_peak, Severity::Low);

        // Age bound: > EPISODE_REHYDRATE_MAX_AGE_SECS old → dropped.
        let stale_now = NOW + (EPISODE_REHYDRATE_MAX_AGE_SECS + 1) * 1000;
        let decoded = episode_snapshot::decode(&json, stale_now, ist_date_of(stale_now));
        assert!(decoded.is_empty(), "stale-age entry must be dropped");

        // Day bound: previous IST day → dropped even when age is fine.
        let tomorrow = today.succ_opt().unwrap();
        let decoded = episode_snapshot::decode(&json, NOW + 1000, tomorrow);
        assert!(decoded.is_empty(), "previous-IST-day entry must be dropped");

        // Corrupt JSON → empty, no panic.
        assert!(episode_snapshot::decode("{{{{not json", NOW, today).is_empty());
        assert!(episode_snapshot::decode("{\"a\":1}", NOW, today).is_empty());
        // Unknown family label → dropped fail-open.
        let alien = r#"[{"family":"alien_ws","conn":0,"opened_at_ms":1751900000000}]"#;
        assert!(episode_snapshot::decode(alien, NOW, today).is_empty());
    }

    fn ist_date_of(epoch_ms: u64) -> chrono::NaiveDate {
        let offset =
            chrono::FixedOffset::east_opt(tickvault_common::constants::IST_UTC_OFFSET_SECONDS)
                .unwrap();
        chrono::DateTime::<chrono::Utc>::from_timestamp((epoch_ms / 1000) as i64, 0)
            .unwrap()
            .with_timezone(&offset)
            .date_naive()
    }

    // -- Registry extras ------------------------------------------------------

    #[test]
    fn test_registry_record_edit_failure_and_fallback_bookkeeping() {
        let reg = EpisodeRegistry::new();
        let _ = reg.apply_event(key(), EpisodeRole::Open, Severity::High, 0, NOW, &cfg());
        reg.record_sent(key(), Some(10), NOW);
        assert_eq!(reg.record_edit_failure(key()), 1);
        assert_eq!(reg.record_edit_failure(key()), 2);
        reg.replace_message_id(key(), Some(11), NOW + 1000);
        let snap = reg.snapshot();
        assert_eq!(snap.len(), 1);
        assert_eq!(snap[0].message_id, Some(11));
        assert_eq!(snap[0].edit_failures, 0, "fallback resets failures");
        // Hash bookkeeping.
        reg.record_edit_applied(key(), NOW + 2000, 777);
        assert_eq!(reg.last_render_hash(key()), 777);
        // Unknown key is safe.
        let other = EpisodeKey {
            family: EpisodeFamily::OrderUpdateWs,
            conn: 3,
        };
        assert_eq!(reg.record_edit_failure(other), 0);
        assert_eq!(reg.last_render_hash(other), 0);
    }

    #[test]
    fn test_registry_rehydrate_and_attempts_hint() {
        let reg = EpisodeRegistry::new();
        let st = live_state(Some(5));
        reg.rehydrate(vec![st]);
        assert_eq!(reg.live_count(), 1);
        // Rehydrated state edits, not re-sends.
        let d = reg.apply_event(
            key(),
            EpisodeRole::Progress,
            Severity::High,
            17,
            NOW + 100_000,
            &cfg(),
        );
        assert_eq!(
            d.action,
            EpisodeAction::Edit {
                message_id: 5,
                close: false
            }
        );
        assert_eq!(d.state.attempts, 17, "attempts hint wins when provided");
    }

    // -- Name-matched pub-fn coverage (pub-fn-test-guard contract) ----------

    #[test]
    fn test_record_sent_sets_message_id_explained_and_resets_failures() {
        let reg = EpisodeRegistry::new();
        let _ = reg.apply_event(key(), EpisodeRole::Open, Severity::High, 0, NOW, &cfg());
        let _ = reg.record_edit_failure(key());
        reg.record_sent(key(), Some(64), NOW + 100);
        let snap = reg.snapshot();
        assert_eq!(snap[0].message_id, Some(64));
        assert!(snap[0].explained);
        assert_eq!(snap[0].edit_failures, 0);
        assert_eq!(snap[0].last_edit_ms, NOW + 100);
        // A failed send (None id) still marks explained, keeps id None.
        let reg2 = EpisodeRegistry::new();
        let _ = reg2.apply_event(key(), EpisodeRole::Open, Severity::High, 0, NOW, &cfg());
        reg2.record_sent(key(), None, NOW + 100);
        assert_eq!(reg2.snapshot()[0].message_id, None);
        assert!(reg2.snapshot()[0].explained);
    }

    #[test]
    fn test_record_edit_applied_and_last_render_hash_bookkeeping() {
        let reg = EpisodeRegistry::new();
        let _ = reg.apply_event(key(), EpisodeRole::Open, Severity::High, 0, NOW, &cfg());
        assert_eq!(reg.last_render_hash(key()), 0, "no render applied yet");
        let _ = reg.record_edit_failure(key());
        reg.record_edit_applied(key(), NOW + 500, 4242);
        assert_eq!(reg.last_render_hash(key()), 4242);
        let snap = reg.snapshot();
        assert_eq!(snap[0].last_edit_ms, NOW + 500);
        assert_eq!(snap[0].edit_failures, 0, "success resets the failure run");
    }

    #[test]
    fn test_replace_message_id_swaps_bubble_and_resets_failures() {
        let reg = EpisodeRegistry::new();
        let _ = reg.apply_event(key(), EpisodeRole::Open, Severity::High, 0, NOW, &cfg());
        reg.record_sent(key(), Some(1), NOW);
        let _ = reg.record_edit_failure(key());
        reg.replace_message_id(key(), Some(2), NOW + 1000);
        let snap = reg.snapshot();
        assert_eq!(snap[0].message_id, Some(2));
        assert_eq!(snap[0].edit_failures, 0);
    }

    #[test]
    fn test_from_snapshot_label_family_phase_roundtrip_unknown_none() {
        for family in [
            EpisodeFamily::MainFeedWs,
            EpisodeFamily::OrderUpdateWs,
            EpisodeFamily::Boot,
        ] {
            assert_eq!(
                EpisodeFamily::from_snapshot_label(family.snapshot_label()),
                Some(family)
            );
        }
        assert_eq!(EpisodeFamily::from_snapshot_label("alien_ws"), None);
        for phase in [EpisodePhase::Down, EpisodePhase::Recovering] {
            assert_eq!(
                EpisodePhase::from_snapshot_label(phase.snapshot_label()),
                Some(phase)
            );
        }
        assert_eq!(EpisodePhase::from_snapshot_label("half-open"), None);
    }

    #[test]
    fn test_render_episode_first_page_small_and_huge() {
        assert_eq!(render_episode_first_page("tiny body"), "tiny body");
        let huge = "x".repeat(EPISODE_FIRST_PAGE_MAX_CHARS + 500);
        assert_eq!(
            render_episode_first_page(&huge).chars().count(),
            EPISODE_FIRST_PAGE_MAX_CHARS
        );
    }

    #[test]
    fn test_render_episode_steady_shows_counters() {
        let mut st = live_state(Some(1));
        st.occurrences = 7;
        st.attempts = 31;
        let ctx = EpisodeRenderCtx { now_ms: NOW };
        let rendered = render_episode_steady(&st, &ctx);
        assert!(rendered.contains("7 drops"));
        assert!(rendered.contains("31 reconnect attempts"));
        assert!(rendered.contains("reconnecting automatically"));
    }

    #[test]
    fn test_render_episode_recovering_shows_confirming() {
        let st = live_state(Some(1));
        let ctx = EpisodeRenderCtx { now_ms: NOW };
        let rendered = render_episode_recovering(&st, &ctx);
        assert!(rendered.contains("reconnected — confirming"));
    }

    #[test]
    fn test_render_episode_recovered_contains_duration_and_attempts() {
        let mut st = live_state(Some(1));
        st.attempts = 4;
        let ctx = EpisodeRenderCtx {
            now_ms: NOW + 90_000,
        };
        let line = render_episode_recovered(&st, &ctx);
        assert!(line.contains("1m 30s"));
        assert!(line.contains("4 attempts"));
    }

    #[test]
    fn test_render_episode_stale_closed_one_line_neutral() {
        let mut st = live_state(Some(1));
        st.last_event_ms = NOW;
        let line = render_episode_stale_closed(&st);
        assert!(line.starts_with('\u{26aa}'), "neutral marker: {line:?}");
        assert_eq!(line.matches('\n').count(), 0, "one line only");
        assert!(line.contains("IST"), "IST timestamp per commandment 9");
        assert!(line.contains("fresh alert"));
        // Commandments: no library names / paths / jargon.
        for banned in ["rkyv", "papaya", "mpsc", ".rs", "data/", "editMessageText"] {
            assert!(!line.contains(banned), "banned {banned:?} in {line:?}");
        }
    }

    #[test]
    fn test_render_episode_recovered_carries_feed_badge() {
        // Operator directive 2026-07-14: the recovered close line carries
        // the SAME broker badge the DOWN bubble led with — a recovery must
        // never lose the tag its trigger carried (ratchet).
        for family in [EpisodeFamily::MainFeedWs, EpisodeFamily::OrderUpdateWs] {
            let mut st = EpisodeState::open(EpisodeKey { family, conn: 1 }, Severity::High, NOW);
            st.attempts = 4;
            let ctx = EpisodeRenderCtx {
                now_ms: NOW + 90_000,
            };
            let line = render_episode_recovered(&st, &ctx);
            assert!(
                line.contains("\u{1f537} DHAN — "),
                "recovered line must carry the same badge format as the DOWN bubble: {line:?}"
            );
            assert!(line.starts_with('\u{2705}'), "green stays first: {line:?}");
        }
        // The boot bubble is a milestone checklist, not a broker feed —
        // its close line stays badge-free.
        let boot = EpisodeState::open(BOOT_EPISODE_KEY, Severity::High, NOW);
        let ctx = EpisodeRenderCtx {
            now_ms: NOW + 90_000,
        };
        let line = render_episode_recovered(&boot, &ctx);
        assert!(
            !line.contains("DHAN") && !line.contains("GROWW"),
            "boot close line must not carry a broker badge: {line:?}"
        );
    }

    #[test]
    fn test_render_episode_stale_closed_carries_feed_badge() {
        // Same-tag rule for the neutral stale-close line (2026-07-14).
        let mut st = live_state(Some(1));
        st.last_event_ms = NOW;
        let line = render_episode_stale_closed(&st);
        assert!(
            line.contains("\u{1f537} DHAN — "),
            "stale-close line must carry the same badge format as the DOWN bubble: {line:?}"
        );
        assert!(
            line.starts_with('\u{26aa}'),
            "neutral marker stays first: {line:?}"
        );
    }

    // -- Boot bubble (2026-07-09) -------------------------------------------

    fn full_checklist(now: u64) -> BootChecklist {
        let mut cl = BootChecklist::new(now, true, "a1b2c3d".to_string());
        cl.expectations = Some((true, true));
        cl.fold(
            BootMilestone::Services {
                healthy: 3,
                total: 3,
            },
            now,
        );
        cl.fold(BootMilestone::DhanAuth, now + 1000);
        cl.fold(BootMilestone::Instruments { count: 1046 }, now + 2000);
        cl.fold(
            BootMilestone::DhanFeedOnline {
                connected: 1,
                total: 1,
                last_real_tick_age_secs: Some(2),
            },
            now + 3000,
        );
        cl.fold(BootMilestone::OrderUpdateConnected, now + 4000);
        cl.fold(BootMilestone::OrderUpdateAuthenticated, now + 4500);
        cl.fold(BootMilestone::GrowwAuth, now + 5000);
        cl.fold(
            BootMilestone::GrowwInstruments { subscribed: 768 },
            now + 5500,
        );
        cl.fold(
            BootMilestone::GrowwConnected { market_open: true },
            now + 6000,
        );
        cl
    }

    #[test]
    fn test_boot_checklist_fold_idempotent_and_order_free() {
        // Same milestones, shuffled order + duplicates → identical render.
        let now = NOW;
        let milestones = [
            BootMilestone::DhanAuth,
            BootMilestone::Instruments { count: 1046 },
            BootMilestone::OrderUpdateAuthenticated,
            BootMilestone::GrowwAuth,
            BootMilestone::Services {
                healthy: 3,
                total: 3,
            },
        ];
        let mut forward = BootChecklist::new(now, false, "a1b2c3d".to_string());
        for m in milestones {
            forward.fold(m, now + 1000);
            forward.fold(m, now + 2000); // duplicate — idempotent
        }
        let mut reversed = BootChecklist::new(now, false, "a1b2c3d".to_string());
        for m in milestones.iter().rev() {
            reversed.fold(*m, now + 3000);
        }
        let ctx = EpisodeRenderCtx { now_ms: now + 9000 };
        assert_eq!(
            render_boot_checklist(&forward, &ctx),
            render_boot_checklist(&reversed, &ctx),
            "fold must be idempotent + commutative"
        );
        assert!(forward.dirty, "fold marks a pending render");
        // Complete: first fold wins (duplicate keeps the original instant).
        forward.fold(BootMilestone::Complete { mode: "sandbox" }, now + 10_000);
        forward.fold(BootMilestone::Complete { mode: "LIVE" }, now + 20_000);
        assert_eq!(forward.complete_at_ms, Some(now + 10_000));
        assert_eq!(forward.mode, Some("sandbox"));
        assert!(forward.is_complete());
    }

    #[test]
    fn test_render_boot_checklist_first_mid_final_under_ceiling() {
        let now = NOW;
        let ctx = EpisodeRenderCtx { now_ms: now };
        // First page: only Services folded, expectations set → ⏳ lines.
        let mut first = BootChecklist::new(now, true, "a1b2c3d".to_string());
        first.expectations = Some((true, true));
        first.fold(
            BootMilestone::Services {
                healthy: 3,
                total: 3,
            },
            now,
        );
        let page = render_boot_checklist(&first, &ctx);
        assert!(page.contains("tickvault starting — NEW CODE deployed"));
        assert!(page.contains("(build a1b2c3d)"));
        assert!(page.contains("\u{23f3} Dhan login"));
        assert!(page.contains("\u{23f3} Groww feed"));
        assert!(page.contains("Boot finishing"));
        assert!(page.contains("updates itself"));
        // Mid-boot: some ✅, some ⏳, fixed template order (Dhan login
        // before Instruments before the feed lines, regardless of arrival).
        let mid = full_checklist(now);
        let mid_render = render_boot_checklist(&mid, &ctx);
        let login_idx = mid_render.find("Dhan login").expect("login line");
        let instr_idx = mid_render
            .find("Instruments — 1046 tracked")
            .expect("instr line");
        let feed_idx = mid_render.find("Dhan price feed").expect("feed line");
        assert!(login_idx < instr_idx && instr_idx < feed_idx, "fixed order");
        assert!(mid_render.contains("last real price 2s ago"));
        assert!(mid_render.contains("Order confirmations — connected and confirmed"));
        assert!(mid_render.contains("Groww feed — signed in \u{b7} 768 instruments"));
        // Final: Complete folds in → checklist header + footer.
        let mut done = full_checklist(now);
        done.fold(BootMilestone::Complete { mode: "sandbox" }, now + 94_000);
        let final_render = render_boot_checklist(&done, &ctx);
        assert!(final_render.contains("(build a1b2c3d, new code)"));
        assert!(final_render.contains("Mode: sandbox"));
        assert!(final_render.contains("Boot took 1m 34s"));
        assert!(final_render.contains("You're good to go."));
        assert!(!final_render.contains("Boot finishing"));
        // Ceiling + commandments on every phase.
        for r in [&page, &mid_render, &final_render] {
            assert!(
                r.chars().count() <= BOOT_BUBBLE_MAX_CHARS,
                "boot render exceeded {BOOT_BUBBLE_MAX_CHARS} chars"
            );
            for banned in ["rkyv", "papaya", "mpsc", ".rs", "data/", "editMessageText"] {
                assert!(!r.contains(banned), "banned {banned:?} in {r:?}");
            }
        }
    }

    #[test]
    fn test_render_boot_checklist_lazy_mode_without_expectations() {
        // Expectations unset (milestone raced set_boot_expectations) —
        // only data-carrying lines render, no phantom ⏳ component lines.
        let now = NOW;
        let mut cl = BootChecklist::new(now, false, "a1b2c3d".to_string());
        cl.fold(BootMilestone::DhanAuth, now);
        let r = render_boot_checklist(&cl, &EpisodeRenderCtx { now_ms: now });
        assert!(r.contains("\u{2705} Dhan login"));
        assert!(!r.contains("\u{23f3} Instruments"));
        assert!(!r.contains("\u{23f3} Groww feed"));
        assert!(r.contains("Boot finishing"), "generic pending line stays");
        // No false "NEW CODE" flavor when new_code=false.
        assert!(!r.contains("NEW CODE"));
    }

    #[test]
    fn test_render_boot_checklist_honest_no_real_ticks_line() {
        let now = NOW;
        let mut cl = BootChecklist::new(now, false, "a1b2c3d".to_string());
        cl.fold(
            BootMilestone::DhanFeedOnline {
                connected: 1,
                total: 1,
                last_real_tick_age_secs: None,
            },
            now,
        );
        let r = render_boot_checklist(&cl, &EpisodeRenderCtx { now_ms: now });
        assert!(
            r.contains("no real prices yet"),
            "connected without a real price must keep the caveat: {r:?}"
        );
    }

    #[test]
    fn test_render_boot_checklist_deferred_off_hours_line() {
        let now = NOW;
        let mut cl = BootChecklist::new(now, false, "a1b2c3d".to_string());
        cl.fold(BootMilestone::DhanFeedDeferredOffHours, now);
        let r = render_boot_checklist(&cl, &EpisodeRenderCtx { now_ms: now });
        assert!(
            r.contains("\u{1f319} Dhan price feed — waiting for market open"),
            "off-hours boot must show the honest deferred line: {r:?}"
        );
    }

    #[test]
    fn test_render_boot_checklist_keeps_tickvault_started_phrase() {
        // The morning-readiness instructions grep for the literal phrase
        // "tickvault started" — the completed header must carry it, and an
        // unknown build sha must render plainly (never a false NEW CODE).
        let now = NOW;
        let mut cl = BootChecklist::new(now, true, "unknown".to_string());
        cl.fold(BootMilestone::Complete { mode: "sandbox" }, now + 5000);
        let r = render_boot_checklist(&cl, &EpisodeRenderCtx { now_ms: now });
        assert!(r.contains("tickvault started"));
        assert!(!r.contains("build"), "unknown sha renders plainly: {r:?}");
        assert!(!r.contains("new code"));
    }

    #[test]
    fn test_episode_config_for_boot_zero_throttle_ws_default() {
        // Ratchet: Boot gets the 0s edit throttle; the WS families keep
        // the pinned 2026-07-07 defaults untouched.
        let boot = episode_config_for(EpisodeFamily::Boot);
        assert_eq!(boot.edit_min_interval_secs, 0);
        assert_eq!(boot.stability_secs, EPISODE_STABILITY_SECS);
        for family in [EpisodeFamily::MainFeedWs, EpisodeFamily::OrderUpdateWs] {
            let cfg = episode_config_for(family);
            assert_eq!(cfg.edit_min_interval_secs, EPISODE_EDIT_MIN_INTERVAL_SECS);
            assert_eq!(cfg.stability_secs, EPISODE_STABILITY_SECS);
            assert_eq!(cfg.reopen_secs, EPISODE_REOPEN_SECS);
            assert_eq!(cfg.down_stale_expire_secs, EPISODE_DOWN_STALE_EXPIRE_SECS);
        }
    }

    #[test]
    fn test_snapshot_excludes_boot_family() {
        // A boot episode must never survive a restart via the snapshot —
        // encode filters it; decode drops a hand-written "boot" entry.
        let boot_state = EpisodeState::open(BOOT_EPISODE_KEY, Severity::Low, NOW);
        let ws_state = live_state(Some(9));
        let json = episode_snapshot::encode(&[boot_state, ws_state]);
        assert!(
            !json.contains("\"boot\""),
            "encode must filter Boot: {json}"
        );
        let handwritten =
            format!(r#"[{{"family":"boot","conn":0,"opened_at_ms":{NOW},"message_id":5}}]"#);
        let decoded = episode_snapshot::decode(&handwritten, NOW + 1000, ist_date_of(NOW + 1000));
        assert!(decoded.is_empty(), "decode must drop Boot entries");
    }

    #[test]
    fn test_tick_never_closes_or_expires_boot_family() {
        let reg = EpisodeRegistry::new();
        let cfg = episode_config_for(EpisodeFamily::Boot);
        let _ = reg.apply_boot_milestone(BootMilestone::DhanAuth, NOW, &cfg);
        reg.record_sent(BOOT_EPISODE_KEY, Some(42), NOW);
        // Way past both the stability and stale-expire windows.
        let far = NOW + (EPISODE_DOWN_STALE_EXPIRE_SECS + 3600) * 1000;
        let outcome = reg.tick(far, &EpisodeConfig::default());
        assert!(outcome.closed.is_empty(), "Boot never green-promotes");
        assert!(outcome.expired.is_empty(), "Boot never stale-expires");
        assert_eq!(reg.live_count(), 1, "the boot bubble stays live");
    }

    #[test]
    fn test_retire_boot_only_after_complete_plus_window() {
        let reg = EpisodeRegistry::new();
        let cfg = episode_config_for(EpisodeFamily::Boot);
        let _ = reg.apply_boot_milestone(BootMilestone::DhanAuth, NOW, &cfg);
        reg.record_sent(BOOT_EPISODE_KEY, Some(42), NOW);
        reg.mark_boot_delivered();
        // Incomplete boot never retires — even hours later.
        assert!(!reg.retire_boot(NOW + 7200 * 1000));
        assert_eq!(reg.live_count(), 1);
        // Complete + delivered, inside the window: still live.
        let (_, _) = reg.apply_boot_milestone(
            BootMilestone::Complete { mode: "sandbox" },
            NOW + 10_000,
            &cfg,
        );
        reg.mark_boot_delivered();
        assert!(!reg.retire_boot(NOW + 10_000 + BOOT_BUBBLE_RETIRE_SECS * 1000 - 1));
        assert_eq!(reg.live_count(), 1);
        // Past the window: retires silently (state + checklist cleared).
        assert!(reg.retire_boot(NOW + 10_000 + BOOT_BUBBLE_RETIRE_SECS * 1000));
        assert_eq!(reg.live_count(), 0);
        assert!(reg.boot_checklist().is_none());
        // A post-retire milestone opens a FRESH mini bubble.
        let (decision, cl) = reg.apply_boot_milestone(
            BootMilestone::GrowwConnected { market_open: true },
            NOW + 20_000 + BOOT_BUBBLE_RETIRE_SECS * 1000,
            &cfg,
        );
        assert_eq!(decision.action, EpisodeAction::SendFirstPage);
        assert!(!cl.is_complete(), "fresh checklist, not the retired one");
        assert!(cl.mini, "post-retire checklist is a MINI (feed update)");
        // A dirty (undelivered) completed checklist must NOT retire.
        let reg2 = EpisodeRegistry::new();
        let _ = reg2.apply_boot_milestone(BootMilestone::Complete { mode: "LIVE" }, NOW, &cfg);
        assert!(
            !reg2.retire_boot(NOW + (BOOT_BUBBLE_RETIRE_SECS + 60) * 1000),
            "dirty checklist stays for the ticker re-drive"
        );
    }

    #[test]
    fn test_post_retire_boot_event_opens_fresh_mini_bubble() {
        // Hostile-review fix 2026-07-09: a mid-day feed reconnect / lane
        // cold-start re-firing boot-keyed events AFTER the morning bubble
        // retired must render an honest "Feed update" mini bubble — never
        // a false "tickvault starting" that can neither complete nor
        // retire.
        let reg = EpisodeRegistry::new();
        let cfg = episode_config_for(EpisodeFamily::Boot);
        let _ = reg.apply_boot_milestone(BootMilestone::Complete { mode: "sandbox" }, NOW, &cfg);
        reg.mark_boot_delivered();
        assert!(reg.retire_boot(NOW + BOOT_BUBBLE_RETIRE_SECS * 1000));
        // A Groww reconnect edge 3 hours later.
        let mid_day = NOW + 3 * 3600 * 1000;
        let (_, cl) = reg.apply_boot_milestone(
            BootMilestone::GrowwConnected { market_open: true },
            mid_day,
            &cfg,
        );
        assert!(cl.mini);
        let r = render_boot_checklist(&cl, &EpisodeRenderCtx { now_ms: mid_day });
        assert!(r.contains("\u{1f504} Feed update"), "neutral header: {r:?}");
        assert!(r.contains("Groww feed"), "component line renders: {r:?}");
        assert!(!r.contains("tickvault starting"), "no false boot claim");
        assert!(!r.contains("Boot finishing"), "no broken promise footer");
        assert!(!r.contains("updates itself"), "no broken promise footer");
        assert!(!r.contains("Started"), "no phantom system start line");
        // A second component event inside the window folds into the SAME
        // mini bubble (anti-spam for a restart burst).
        reg.record_sent(BOOT_EPISODE_KEY, Some(88), mid_day);
        reg.mark_boot_delivered();
        let (d2, cl2) = reg.apply_boot_milestone(BootMilestone::DhanAuth, mid_day + 60_000, &cfg);
        assert_eq!(
            d2.action,
            EpisodeAction::Edit {
                message_id: 88,
                close: false
            }
        );
        assert!(cl2.mini && cl2.groww_connected.is_some() && cl2.dhan_auth);
        reg.mark_boot_delivered();
        // The mini bubble retires on the idle window despite never seeing
        // `Complete` — no permanently-stuck bubble.
        assert!(!reg.retire_boot(mid_day + 60_000 + BOOT_BUBBLE_RETIRE_SECS * 1000 - 1));
        assert!(reg.retire_boot(mid_day + 60_000 + BOOT_BUBBLE_RETIRE_SECS * 1000));
        assert!(reg.boot_checklist().is_none());
        assert_eq!(reg.live_count(), 0);
    }

    #[test]
    fn test_is_complete_only_after_complete_fold() {
        let mut cl = BootChecklist::new(NOW, false, "a1b2c3d".to_string());
        assert!(!cl.is_complete());
        cl.fold(BootMilestone::DhanAuth, NOW + 100);
        assert!(!cl.is_complete(), "component milestones never complete");
        cl.fold(BootMilestone::Complete { mode: "sandbox" }, NOW + 200);
        assert!(cl.is_complete());
    }

    #[test]
    fn test_set_boot_flavor_creates_and_updates_checklist() {
        let reg = EpisodeRegistry::new();
        // Creates the checklist when absent (flavor never lost to
        // milestone-vs-init ordering)…
        reg.set_boot_flavor(true, "a1b2c3d", NOW);
        let cl = reg.boot_checklist().expect("flavor creates the checklist");
        assert!(cl.new_code);
        assert_eq!(cl.build_sha_short, "a1b2c3d");
        assert!(!cl.mini, "boot-time flavor init is the MAIN bubble");
        // …and updates an existing one in place.
        reg.set_boot_flavor(false, "unknown", NOW + 100);
        let cl2 = reg.boot_checklist().expect("checklist still live");
        assert!(!cl2.new_code);
        assert_eq!(cl2.build_sha_short, "unknown");
    }

    #[test]
    fn test_mark_boot_delivered_and_boot_redrive_candidate_dirty_flow() {
        let reg = EpisodeRegistry::new();
        let cfg = episode_config_for(EpisodeFamily::Boot);
        assert!(
            reg.boot_redrive_candidate().is_none(),
            "no checklist = no candidate"
        );
        let _ = reg.apply_boot_milestone(BootMilestone::DhanAuth, NOW, &cfg);
        let (cl, id) = reg
            .boot_redrive_candidate()
            .expect("undelivered fold is a candidate");
        assert!(cl.dirty);
        assert_eq!(id, None, "first page never landed → fresh send");
        reg.record_sent(BOOT_EPISODE_KEY, Some(7), NOW);
        let (_, id2) = reg.boot_redrive_candidate().expect("still dirty");
        assert_eq!(id2, Some(7), "message id rides the candidate");
        reg.mark_boot_delivered();
        assert!(
            reg.boot_redrive_candidate().is_none(),
            "mark_boot_delivered clears the dirty flag"
        );
    }

    #[test]
    fn test_apply_boot_milestone_registry_flow_and_redrive_bookkeeping() {
        // First milestone opens (SendFirstPage); once the id is recorded,
        // later milestones edit the SAME bubble; boot_redrive_candidate +
        // mark_boot_delivered manage the dirty flag; set_boot_flavor +
        // set_boot_expectations survive any call order.
        let reg = EpisodeRegistry::new();
        let cfg = episode_config_for(EpisodeFamily::Boot);
        reg.set_boot_flavor(true, "a1b2c3d", NOW);
        reg.set_boot_expectations(true, true, NOW);
        let (d1, cl1) = reg.apply_boot_milestone(BootMilestone::DhanAuth, NOW + 100, &cfg);
        assert_eq!(d1.action, EpisodeAction::SendFirstPage);
        assert!(cl1.new_code && cl1.build_sha_short == "a1b2c3d");
        assert_eq!(cl1.expectations, Some((true, true)));
        assert!(cl1.dirty);
        reg.record_sent(BOOT_EPISODE_KEY, Some(64), NOW + 100);
        reg.mark_boot_delivered();
        assert!(reg.boot_redrive_candidate().is_none(), "delivered = clean");
        let (d2, _) =
            reg.apply_boot_milestone(BootMilestone::Instruments { count: 4 }, NOW + 200, &cfg);
        assert_eq!(
            d2.action,
            EpisodeAction::Edit {
                message_id: 64,
                close: false
            },
            "boot throttle is 0 — the very next milestone edits in place"
        );
        let (cl, id) = reg
            .boot_redrive_candidate()
            .expect("undelivered fold is a re-drive candidate");
        assert_eq!(id, Some(64));
        assert_eq!(cl.instruments, Some(4));
        reg.mark_boot_delivered();
        assert!(reg.boot_redrive_candidate().is_none());
    }
}
