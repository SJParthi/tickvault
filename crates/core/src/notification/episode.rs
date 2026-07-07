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
pub const EPISODE_EDIT_FAILURES_FALLBACK_THRESHOLD: u8 = 2;

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
}

impl EpisodeFamily {
    /// Feed badge leading the bubble body (plain emoji + provider word —
    /// the 10 Telegram commandments hold: no library names, no file paths).
    #[must_use]
    pub const fn badge(self) -> &'static str {
        match self {
            Self::MainFeedWs | Self::OrderUpdateWs => "\u{1f537} DHAN", // 🔷
        }
    }

    /// Plain-English description of the affected feed.
    #[must_use]
    pub const fn feed_desc(self) -> &'static str {
        match self {
            Self::MainFeedWs => "Live price feed",
            Self::OrderUpdateWs => "Order confirmations feed",
        }
    }

    /// Stable wire label for the snapshot file.
    #[must_use]
    pub const fn snapshot_label(self) -> &'static str {
        match self {
            Self::MainFeedWs => "main_feed_ws",
            Self::OrderUpdateWs => "order_update_ws",
        }
    }

    /// Inverse of [`Self::snapshot_label`]. Unknown labels → `None`
    /// (fail-open — the snapshot entry is dropped, never a panic).
    #[must_use]
    pub fn from_snapshot_label(label: &str) -> Option<Self> {
        match label {
            "main_feed_ws" => Some(Self::MainFeedWs),
            "order_update_ws" => Some(Self::OrderUpdateWs),
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
}

/// Episode timing knobs. Defaults come from the pinned constants.
#[derive(Debug, Clone, Copy)]
pub struct EpisodeConfig {
    pub edit_min_interval_secs: u64,
    pub stability_secs: u64,
    pub reopen_secs: u64,
}

impl Default for EpisodeConfig {
    fn default() -> Self {
        Self {
            edit_min_interval_secs: EPISODE_EDIT_MIN_INTERVAL_SECS,
            stability_secs: EPISODE_STABILITY_SECS,
            reopen_secs: EPISODE_REOPEN_SECS,
        }
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
    /// Nothing to do. Reachable ONLY for `Resolve` with no live state and
    /// no fresh tombstone (a recovery for an episode we never opened).
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
    _severity: Severity,
    now_ms: u64,
    cfg: &EpisodeConfig,
) -> EpisodeAction {
    match state {
        Some(st) => {
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
            // A recovery for an episode we never tracked — nothing to edit,
            // nothing degraded to announce.
            EpisodeRole::Resolve => EpisodeAction::Ignore,
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
}

/// In-memory episode registry — one live [`EpisodeState`] per key plus
/// recent [`ClosedTombstone`]s for the flap-reopen fold.
#[derive(Debug, Default)]
pub struct EpisodeRegistry {
    inner: Mutex<HashMap<EpisodeKey, EpisodeState>>,
    tombstones: Mutex<HashMap<EpisodeKey, ClosedTombstone>>,
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
                EpisodeAction::Ignore => {
                    // Resolve with no state — synthesize a throwaway
                    // snapshot; the caller returns without rendering.
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

    /// Stability promotion — called from the drain ticker every ~10s.
    ///
    /// Promotes `Recovering` episodes quiet for `cfg.stability_secs` to
    /// CLOSED: removes them, writes a [`ClosedTombstone`] (kept
    /// `cfg.reopen_secs`, then GC'd here too) and returns the closed states
    /// so the caller can issue the final green close edit.
    #[must_use]
    pub fn tick(&self, now_ms: u64, cfg: &EpisodeConfig) -> Vec<EpisodeState> {
        let mut inner = self.lock_inner();
        let mut tombstones = self.lock_tombstones();
        let stability_ms = cfg.stability_secs.saturating_mul(MS_PER_SEC);
        let closed_keys: Vec<EpisodeKey> = inner
            .iter()
            .filter(|(_, st)| {
                st.phase == EpisodePhase::Recovering
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
                        },
                    );
                }
                closed.push(st);
            }
        }

        // GC stale tombstones past the reopen window.
        let reopen_ms = cfg.reopen_secs.saturating_mul(MS_PER_SEC);
        tombstones.retain(|_, t| now_ms.saturating_sub(t.closed_at_ms) <= reopen_ms);
        closed
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
        desc = state.key.family.feed_desc(),
        dur = format_duration_human(down_secs),
        att = state.attempts,
    )
}

// ---------------------------------------------------------------------------
// Snapshot codec (pure; fail-open)
// ---------------------------------------------------------------------------

/// Pure JSON codec for the advisory episode snapshot file.
///
/// Serialized fields ONLY: `{family, conn, message_id, opened_at_ms,
/// occurrences, attempts, explained, phase}` — no reasons, no secrets.
pub mod episode_snapshot {
    use super::{
        EPISODE_REHYDRATE_MAX_AGE_SECS, EpisodeFamily, EpisodeKey, EpisodePhase, EpisodeState,
        MS_PER_SEC, Severity,
    };

    /// Encodes live episode states to the snapshot JSON string.
    #[must_use]
    pub fn encode(entries: &[EpisodeState]) -> String {
        let items: Vec<serde_json::Value> = entries
            .iter()
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
            let mut st = EpisodeState::open(
                EpisodeKey { family, conn },
                // Severity is not persisted — conservative High default
                // (both WS families page High on open today).
                Severity::High,
                opened_at_ms,
            );
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
        let closed = reg.tick(NOW + 1000 + 30_000, &cfg());
        assert!(closed.is_empty(), "must not close inside stability window");
        // After stability: closed + tombstoned.
        let closed = reg.tick(NOW + 1000 + EPISODE_STABILITY_SECS * 1000, &cfg());
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
    fn test_resolve_with_no_state_no_tombstone_is_ignore() {
        let action = next_episode_action(
            None,
            None,
            EpisodeRole::Resolve,
            Severity::Medium,
            NOW,
            &cfg(),
        );
        assert_eq!(action, EpisodeAction::Ignore);
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
            let tomb = ClosedTombstone { message_id: tomb_id, closed_at_ms: tomb_closed_ms };
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
                rendered.matches('\n').count() <= EPISODE_STEADY_MAX_LINES - 1,
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
}
