//! WebSocket connection pool for Dhan Live Market Feed.
//!
//! Always creates the maximum allowed number of WebSocket connections
//! (default 5, capped at `MAX_WEBSOCKET_CONNECTIONS`), distributing
//! instruments round-robin across all connections. Empty connections
//! stay alive for Phase 2 dynamic rebalancing.
//!
//! Dhan limit: 5,000 instruments per connection, 25,000 total.
//! Each connection runs independently on its own tokio task.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use tokio::sync::mpsc;
use tracing::{error, info};

use tickvault_common::config::{DhanConfig, WebSocketConfig};
use tickvault_common::constants::{
    IST_UTC_OFFSET_SECONDS, MAX_INSTRUMENTS_PER_WEBSOCKET_CONNECTION, MAX_WEBSOCKET_CONNECTIONS,
    SECONDS_PER_DAY, TICK_PERSIST_END_SECS_OF_DAY_IST, TICK_PERSIST_START_SECS_OF_DAY_IST,
};
use tickvault_common::types::FeedMode;
use tickvault_storage::ws_frame_spill::WsFrameSpill;

/// Bug B fix (2026-04-20): market-hours gate for pool watchdog verdicts.
///
/// Returns `true` iff the current wall-clock IST time is inside the
/// trading window `[09:00, 15:30) IST`. Outside this window the pool
/// watchdog MUST NOT escalate Degraded/Halt to ERROR or exit the
/// process — Dhan routinely resets idle TCP connections in the 00:00
/// to 09:00 window, which is normal and should not trigger supervisor
/// restart loops.
///
/// Follows the same pattern as
/// `depth_rebalancer::is_within_market_hours_ist` (audit-findings
/// Rule 3 — "all background workers must be market-hours aware").
#[must_use]
fn pool_watchdog_is_within_market_hours() -> bool {
    let now_utc = chrono::Utc::now().timestamp();
    let now_ist = now_utc.saturating_add(i64::from(IST_UTC_OFFSET_SECONDS));
    let sec_of_day = now_ist.rem_euclid(i64::from(SECONDS_PER_DAY)) as u32;
    // O(1) EXEMPT: Range::contains is two integer comparisons, NOT Vec::contains.
    (TICK_PERSIST_START_SECS_OF_DAY_IST..TICK_PERSIST_END_SECS_OF_DAY_IST).contains(&sec_of_day)
}

use crate::auth::{TokenHandle, TokenManager};
use crate::websocket::connection::WebSocketConnection;
use crate::websocket::pool_watchdog::{PoolWatchdog, WatchdogVerdict};
use crate::websocket::types::{ConnectionHealth, InstrumentSubscription, WebSocketError};

// ---------------------------------------------------------------------------
// W2#8 (WS-GAP-05, 2026-07-10) — pool-slot supervised respawn
// ---------------------------------------------------------------------------

/// Base backoff before re-entering `run()` after an unexpected clean exit —
/// the "respawned within ~5s" WS-GAP-05 runbook contract
/// (`.claude/rules/project/wave-2-error-codes.md`).
const POOL_RESPAWN_BASE_BACKOFF_SECS: u64 = 5;

/// Backoff ceiling for a PERSISTENT unexpected-clean-exit loop (e.g. Dhan
/// politely Close-framing every session). Matches the WS-GAP-08 /
/// `WS_RATE_LIMIT_BACKOFF_CAP_MS` 300s ceiling class: a pathological
/// server-side close loop degrades to one reconnect per 5 minutes — loud
/// (one coded ERROR per cycle), bounded, never a storm, never given up
/// (the FEED-STALL-01 never-give-up-in-market philosophy).
const POOL_RESPAWN_MAX_BACKOFF_SECS: u64 = 300;

/// A `run()` session that survived at least this long resets the respawn
/// backoff streak to 0 — mirrors the WS-GAP-10 order-update 60s
/// reconnect-stability window, so a once-a-day polite close always
/// recovers at the 5s base instead of a stale escalated backoff.
const POOL_RESPAWN_STABILITY_RESET_SECS: u64 = 60;

/// W2#8: verdict for a pool-slot `run()` exit — should the slot task
/// re-enter `run()` (respawn) or terminate?
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PoolSlotExitVerdict {
    /// Deliberate or unrecoverable exit — the slot task returns. Covers:
    /// graceful shutdown / lane teardown (`shutdown_requested`), every
    /// `Err` (`NonReconnectableDisconnect` is the deliberate anti-storm
    /// stop for Dhan 805-class codes; `ReconnectionExhausted` is a
    /// configured finite budget — respawning either would defeat a
    /// deliberate bound; the pool watchdog's genuine-fatal Halt owns
    /// process-level recovery), and a clean exit with the live frame
    /// channel CLOSED (WS-GAP-07 — the tick consumer is dead, so a fresh
    /// session would reconnect to Dhan and immediately exit again).
    Terminal,
    /// Unexpected clean exit (Dhan server Close frame / TCP stream-end
    /// with no shutdown requested and a live consumer) — re-enter
    /// `run()` after a storm-bounded backoff. Pre-W2#8 this class left
    /// the slot dead until the watchdog's 300s Halt + 15-min WS-GAP-09
    /// ride-out (which assumed a live reconnect loop and therefore
    /// no-op'd) escalated to a full process restart.
    Respawn,
}

/// W2#8: pure classifier for a pool-slot `run()` exit. See
/// [`PoolSlotExitVerdict`] for the per-arm rationale.
///
/// Panics / cancellation are NOT inputs here: a panic propagates out of
/// the slot task itself (release profile is `panic = "abort"`, so in the
/// production binary it aborts the process — the TICK-FLUSH-01 honesty
/// precedent; recovery is next-boot WAL replay), and a teardown `abort()`
/// cancels the WHOLE slot task including this loop — cancellation can
/// never respawn, so a torn-down lane can never be resurrected.
fn classify_pool_slot_exit(
    exit: &Result<(), WebSocketError>,
    shutdown_requested: bool,
    live_channel_closed: bool,
) -> PoolSlotExitVerdict {
    if shutdown_requested {
        return PoolSlotExitVerdict::Terminal;
    }
    match exit {
        Err(_) => PoolSlotExitVerdict::Terminal,
        Ok(()) if live_channel_closed => PoolSlotExitVerdict::Terminal,
        Ok(()) => PoolSlotExitVerdict::Respawn,
    }
}

/// W2#8: pure storm-bounded respawn backoff — `min(5 × 2^n, 300)` seconds
/// for the n-th CONSECUTIVE quick exit (5, 10, 20, 40, 80, 160, 300, 300…).
/// Overflow-safe via `checked_shl` saturation to the cap.
fn compute_pool_respawn_backoff_secs(consecutive_quick_exits: u32) -> u64 {
    let doubled = if consecutive_quick_exits >= 32 {
        POOL_RESPAWN_MAX_BACKOFF_SECS
    } else {
        POOL_RESPAWN_BASE_BACKOFF_SECS
            .checked_shl(consecutive_quick_exits)
            .unwrap_or(POOL_RESPAWN_MAX_BACKOFF_SECS)
    };
    doubled.min(POOL_RESPAWN_MAX_BACKOFF_SECS)
}

/// W2#8 (WS-GAP-05): the supervised pool-slot loop — the body of every
/// per-connection task spawned by [`WebSocketConnectionPool::spawn_all`].
///
/// Replace-not-add by construction: the "respawn" is a serialized
/// re-entry of `conn.run()` inside the SAME tokio task on the SAME slot
/// `Arc` — never a second task, never a second socket, so the 2-WebSocket
/// lock (`websocket-connection-scope-lock.md`) and the lane-FSM handle
/// ownership (`DhanLaneRunHandles.ws_handles` H8 Drop floor,
/// `PreLaneAbortGuard`, `teardown_dhan_lane_tasks` abort+drain) are all
/// byte-identical to pre-W2#8. Subscription restoration on respawn is the
/// SAME `connect_and_subscribe` re-send of the slot's assigned instruments
/// that every in-loop reconnect already performs, and the
/// `SubscribeRxGuard` slot reinstall gives the fresh session the
/// subscribe-command receiver exactly as it does across reconnects.
///
/// Cold path — runs once per slot exit, never per tick.
// O(1) EXEMPT: cold-path slot supervisor — one iteration per session exit.
async fn run_supervised_pool_slot(conn: Arc<WebSocketConnection>) -> Result<(), WebSocketError> {
    let mut consecutive_quick_exits: u32 = 0;
    loop {
        let session_started = std::time::Instant::now();
        let exit = conn.run().await;
        let verdict = classify_pool_slot_exit(
            &exit,
            conn.is_shutdown_requested(),
            conn.live_frame_channel_closed(),
        );
        match verdict {
            PoolSlotExitVerdict::Terminal => return exit,
            PoolSlotExitVerdict::Respawn => {
                // Stability reset: a session that lived ≥ 60s is a real
                // recovery — the next backoff starts back at the 5s base.
                if session_started.elapsed().as_secs() >= POOL_RESPAWN_STABILITY_RESET_SECS {
                    consecutive_quick_exits = 0;
                }
                let backoff_secs = compute_pool_respawn_backoff_secs(consecutive_quick_exits);
                consecutive_quick_exits = consecutive_quick_exits.saturating_add(1);
                tracing::error!(
                    connection_id = conn.connection_id(),
                    backoff_secs,
                    consecutive_quick_exits,
                    code = tickvault_common::error_code::ErrorCode::WsGap05PoolRespawn.code_str(),
                    "WS-GAP-05 pool slot exited cleanly WITHOUT a shutdown request \
                     (server Close frame / stream-end) — respawning the slot's \
                     connection loop after backoff"
                );
                metrics::counter!(
                    "tv_ws_pool_respawn_total",
                    "reason" => "unexpected_clean_exit"
                )
                .increment(1);
                tokio::time::sleep(std::time::Duration::from_secs(backoff_secs)).await;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Connection Pool
// ---------------------------------------------------------------------------

/// Pool of WebSocket connections distributing instruments across connections.
///
/// Always creates the maximum allowed number of connections (default 5),
/// distributing instruments round-robin. Empty connections stay alive for
/// Phase 2 dynamic rebalancing. Respects per-connection and total capacity.
pub struct WebSocketConnectionPool {
    /// Active connections (up to 5).
    connections: Vec<Arc<WebSocketConnection>>,

    /// Receiver for raw binary frames from all connections.
    /// TICK-SEQ-01: each item is `(frame_seq, frame)` — the read-loop capture
    /// sequence rides alongside the frame so `capture_seq` is replay-stable.
    frame_receiver: mpsc::Receiver<(u64, bytes::Bytes)>,

    /// STAGE-C.2b: Retained clone of the shared frame sender. Held on the
    /// pool so boot-time WAL replay can inject recovered LiveFeed frames
    /// into the downstream consumer through the same mpsc the live
    /// connections write to. Without this, replayed frames would only be
    /// archived, never re-played into the live pipeline.
    frame_sender: mpsc::Sender<(u64, bytes::Bytes)>,

    /// Stagger delay between connection spawns (milliseconds). 0 = no stagger.
    connection_stagger_ms: u64,

    /// A4: Pool-level circuit breaker watchdog. Tracks whether all
    /// connections have been simultaneously down for > POOL_DEGRADED_ALERT_SECS
    /// (60s) and > POOL_HALT_SECS (300s). Interior-mutable so the read-only
    /// `poll_watchdog()` API works from a `&self` reference shared across
    /// the pool's background task and its owner.
    watchdog: std::sync::Mutex<PoolWatchdog>,

    /// A4 log-coalescing state — the last A4 verdict kind (`a4_log_kind`)
    /// that produced a `poll_watchdog` log line. The Degraded/Halt arms
    /// re-log only when this value changes, so a long all-down cycle no
    /// longer prints one line every 5s poll. Cold-path only.
    last_a4_log_kind: std::sync::atomic::AtomicU8,

    /// O1-B (2026-04-17): Per-connection runtime subscribe-command senders.
    /// Index `i` matches `connections[i]`. `None` means "no subscribe
    /// channel was wired for this connection" (e.g. test pools or the
    /// pre-O1-B legacy boot path). `dispatch_subscribe()` picks the
    /// connection with the most spare instrument capacity and uses its
    /// sender. Empty Vec when no channels installed.
    subscribe_senders:
        std::sync::Mutex<Vec<Option<mpsc::Sender<crate::websocket::connection::SubscribeCommand>>>>,
}

impl std::fmt::Debug for WebSocketConnectionPool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebSocketConnectionPool")
            .field("connection_count", &self.connections.len())
            .finish()
    }
}

// ---------------------------------------------------------------------------
// A4 log coalescing
// ---------------------------------------------------------------------------

/// A4 verdict kind that is not edge-coalesced — `poll_watchdog` logs the
/// Healthy/Recovered/Degrading arms without edge-gating (Recovered already
/// fires exactly once; the other two are silent).
const A4_LOG_KIND_NONE: u8 = 0;
/// A4 `Degraded` verdict kind (all connections down > 60s).
const A4_LOG_KIND_DEGRADED: u8 = 1;
/// A4 `Halt` verdict kind (all connections down > 300s).
const A4_LOG_KIND_HALT: u8 = 2;

/// Maps a watchdog verdict to its A4 log kind. `poll_watchdog` records the
/// last kind it logged and re-logs the Degraded/Halt arms only when this
/// value changes — so a multi-hour all-down cycle yields one log line per
/// transition instead of one every 5s watchdog poll. The continuous signal
/// stays available via the `tv_pool_degraded_seconds` gauge.
fn a4_log_kind(verdict: &WatchdogVerdict) -> u8 {
    match verdict {
        WatchdogVerdict::Degraded { .. } => A4_LOG_KIND_DEGRADED,
        WatchdogVerdict::Halt { .. } => A4_LOG_KIND_HALT,
        WatchdogVerdict::Healthy
        | WatchdogVerdict::Recovered { .. }
        | WatchdogVerdict::Degrading { .. } => A4_LOG_KIND_NONE,
    }
}

impl WebSocketConnectionPool {
    /// Creates a new connection pool and distributes instruments across connections.
    ///
    /// # Arguments
    /// * `token_handle` — Shared atomic token for O(1) reads.
    /// * `client_id` — Dhan client ID (from SSM).
    /// * `dhan_config` — Dhan API configuration.
    /// * `ws_config` — WebSocket keep-alive and reconnection config.
    /// * `instruments` — Full list of instruments to subscribe.
    /// * `feed_mode` — Desired feed granularity.
    ///
    /// # Errors
    /// Returns `CapacityExceeded` if instruments exceed dynamic capacity
    /// (`max_per_conn × num_connections`, default 5,000 × 5 = 25,000).
    pub fn new(
        token_handle: TokenHandle,
        client_id: String,
        dhan_config: DhanConfig,
        ws_config: WebSocketConfig,
        instruments: Vec<InstrumentSubscription>,
        feed_mode: FeedMode,
        notifier: Option<std::sync::Arc<crate::notification::NotificationService>>,
    ) -> Result<Self, WebSocketError> {
        Self::new_with_optional_wal(
            token_handle,
            client_id,
            dhan_config,
            ws_config,
            instruments,
            feed_mode,
            notifier,
            None,
            None,
            None,
            None,
        )
    }

    /// STAGE-C: builder variant that attaches a shared [`WsFrameSpill`] to
    /// every connection in the pool. Every raw binary frame is appended to
    /// the WAL before the try_send to the frame channel, guaranteeing
    /// durability even if the downstream consumer stalls. Test call sites
    /// keep using the 7-arg `new()` — only production paths that need
    /// durable spill wire this variant.
    #[allow(clippy::too_many_arguments)] // APPROVED: STAGE-C builder variant with wal_spill
    // TEST-EXEMPT: integration-level — requires live Dhan WebSocket endpoint; thin delegate over new() which IS tested
    pub fn new_with_optional_wal(
        token_handle: TokenHandle,
        client_id: String,
        dhan_config: DhanConfig,
        ws_config: WebSocketConfig,
        instruments: Vec<InstrumentSubscription>,
        feed_mode: FeedMode,
        notifier: Option<std::sync::Arc<crate::notification::NotificationService>>,
        wal_spill: Option<Arc<WsFrameSpill>>,
        ws_audit_tx: Option<crate::websocket::connection::WsEventAuditSender>,
        // PR-E (2026-06-21): shared runtime Dhan feed-enable flag. `None` =
        // always-on (every existing caller / test). When `Some`, every
        // connection in the pool reads it and parks dormant while disabled.
        feed_enable_flag: Option<Arc<AtomicBool>>,
        // D2c (closes C4): LANE-OWNED `TokenManager` for the sleep-wake
        // renewal. `None` = global `OnceLock` fallback (boot-ON / fast arm).
        // When `Some` (runtime cold-start via `start_dhan_lane`), every
        // connection in the pool targets THIS manager on wake-renewal so a
        // stop→re-start renews the manager the live pool is actually using.
        lane_token_manager: Option<Arc<TokenManager>>,
    ) -> Result<Self, WebSocketError> {
        let total = instruments.len();

        // Compute connection parameters first — needed for dynamic capacity check.
        let max_per_conn = dhan_config
            .max_instruments_per_connection
            .min(MAX_INSTRUMENTS_PER_WEBSOCKET_CONNECTION);
        let num_connections = dhan_config
            .max_websocket_connections
            .min(MAX_WEBSOCKET_CONNECTIONS);

        // Dynamic capacity: effective limit depends on config, not hardcoded 25K.
        // If max_per_conn=2000, num_connections=5 → effective capacity = 10,000.
        let effective_capacity = max_per_conn.saturating_mul(num_connections);
        if total > effective_capacity {
            return Err(WebSocketError::CapacityExceeded {
                requested: total,
                capacity: effective_capacity,
            });
        }

        // Shared channel: all connections send frames to a single receiver.
        // Buffer size: 131,072 (FRAME_CHANNEL_CAPACITY) — sized for 25K instruments.
        // At 10K ticks/sec = 13 seconds backpressure-free headroom.
        // On overflow: backpressure (blocks WS read, never drops frames).
        let (frame_sender, frame_receiver) =
            mpsc::channel(tickvault_common::constants::FRAME_CHANNEL_CAPACITY);

        // O(1) EXEMPT: begin — pool constructor runs once at startup, not per tick
        // Distribute instruments round-robin across connections.
        let mut connection_instruments: Vec<Vec<InstrumentSubscription>> =
            (0..num_connections).map(|_| Vec::new()).collect();

        for (idx, instrument) in instruments.into_iter().enumerate() {
            // SAFETY: num_connections guaranteed > 0 by config validation + .min(MAX_WEBSOCKET_CONNECTIONS=5)
            let slot = idx.checked_rem(num_connections).unwrap_or(0);
            connection_instruments[slot].push(instrument);
        }

        let connections: Vec<Arc<WebSocketConnection>> = connection_instruments
            .into_iter()
            .enumerate()
            .map(|(id, assigned_instruments)| {
                let mut conn = WebSocketConnection::new(
                    id as u8,
                    token_handle.clone(),
                    client_id.clone(),
                    dhan_config.clone(),
                    ws_config.clone(),
                    assigned_instruments,
                    feed_mode,
                    frame_sender.clone(),
                    notifier.clone(),
                );
                if let Some(spill) = wal_spill.clone() {
                    conn = conn.with_wal_spill(spill);
                }
                // 2026-06-12: attach the WS-event audit channel + this pool's
                // size so every connection stamps forensic rows with its
                // (ws_type=MainFeed, connection_index) — future-proof for the
                // 5-main-feed scenario (pool_size reflects num_connections).
                if let Some(tx) = ws_audit_tx.clone() {
                    conn = conn.with_ws_audit(
                        tx,
                        num_connections as i64,
                        tickvault_common::ws_event_types::WsType::MainFeed,
                    );
                }
                // PR-E: attach the shared runtime feed-enable flag so each
                // connection can pause/resume on the operator toggle.
                if let Some(flag) = feed_enable_flag.clone() {
                    conn = conn.with_feed_enable_flag(flag);
                }
                // D2c (C4): attach the LANE-OWNED TokenManager so each
                // connection's sleep-wake renewal targets the live lane manager
                // (not the stale global) after a runtime stop→re-start.
                if let Some(tm) = lane_token_manager.clone() {
                    conn = conn.with_token_manager(tm);
                }
                Arc::new(conn)
            })
            .collect();
        // O(1) EXEMPT: end

        info!(
            num_connections = connections.len(),
            total_instruments = total,
            "WebSocket connection pool created"
        );

        Ok(Self {
            connections,
            frame_receiver,
            frame_sender,
            connection_stagger_ms: ws_config.connection_stagger_ms,
            watchdog: std::sync::Mutex::new(PoolWatchdog::new()),
            last_a4_log_kind: std::sync::atomic::AtomicU8::new(A4_LOG_KIND_NONE),
            // O1-B: empty until `install_subscribe_channels()` is called.
            // APPROVED: cold path — pool constructor, runs once at boot.
            subscribe_senders: std::sync::Mutex::new(Vec::with_capacity(MAX_WEBSOCKET_CONNECTIONS)),
        })
    }

    /// STAGE-C.2b: Returns a clone of the shared frame sender for boot-time
    /// WAL replay injection. Cheap (clone is an Arc bump). The caller
    /// typically uses this to push recovered LiveFeed frames back into the
    /// same pipeline the live connections write to, so the tick processor
    /// sees replayed frames as if they had just arrived — preserving
    /// the zero-tick-loss guarantee across crashes.
    ///
    /// Safe to call at any time; the sender is cheap to clone and drops
    /// cleanly when the caller is done injecting. Covered end-to-end by
    /// the STAGE-C.2b replay-injection integration flow in main.rs.
    // TEST-EXEMPT: trivial Arc-bump clone accessor — `mpsc::Sender::clone` is tokio-tested; integration covered by main.rs replay-injection path
    pub fn frame_sender_clone(&self) -> mpsc::Sender<(u64, bytes::Bytes)> {
        self.frame_sender.clone() // APPROVED: cold path — Arc bump for boot-time WAL replay, not per-tick
    }

    /// Spawns all connections as independent tokio tasks with staggered startup.
    ///
    /// Each connection manages its own lifecycle (connect, subscribe, ping,
    /// read, reconnect). Connections are spawned with a configurable delay
    /// between each to avoid hitting Dhan's server simultaneously.
    /// Returns task handles for monitoring/cancellation.
    // O(1) EXEMPT: begin — spawn runs once per session
    pub async fn spawn_all(&self) -> Vec<tokio::task::JoinHandle<Result<(), WebSocketError>>> {
        let stagger = std::time::Duration::from_millis(self.connection_stagger_ms);
        let mut handles = Vec::with_capacity(self.connections.len());

        for (idx, conn) in self.connections.iter().enumerate() {
            if idx > 0 && !stagger.is_zero() {
                info!(
                    connection_id = idx,
                    stagger_ms = self.connection_stagger_ms,
                    "Waiting before spawning next WebSocket connection"
                );
                tokio::time::sleep(stagger).await;
            }

            let conn = Arc::clone(conn);
            let defer_label = format!("conn={idx}");
            handles.push(tokio::spawn(async move {
                // Off-hours boot gate: if the app starts outside
                // [09:00, 15:30) IST, sleep until the next 09:00 IST
                // BEFORE opening any TCP socket. Eliminates the pre-market
                // disconnect/reconnect flap Parthiban reported on
                // 2026-04-24 at 07:40 IST.
                crate::websocket::market_hours_gate::defer_until_market_open_ist(
                    "main_feed",
                    &defer_label,
                )
                .await;
                // W2#8 (WS-GAP-05, 2026-07-10): supervised slot loop —
                // re-enters `conn.run()` in THIS SAME task (replace-not-add)
                // on an unexpected clean exit (server Close frame /
                // stream-end), with a storm-bounded 5s→300s backoff. Every
                // Err, every shutdown-requested exit, and a WS-GAP-07
                // consumer-dead exit stay terminal; a teardown `abort()`
                // cancels the whole loop so a torn-down lane is never
                // resurrected.
                run_supervised_pool_slot(conn).await
            }));

            info!(
                connection_id = idx,
                total = self.connections.len(),
                "Spawned WebSocket connection (task started; may defer connect if off-hours)"
            );
        }

        handles
    }
    // O(1) EXEMPT: end

    /// Returns the frame receiver for downstream binary frame processing.
    ///
    /// The caller owns this receiver and reads raw binary frames from
    /// all active connections. Each frame is a complete Dhan binary packet.
    pub fn take_frame_receiver(&mut self) -> mpsc::Receiver<(u64, bytes::Bytes)> {
        // Replace with a dummy channel — receiver can only be taken once.
        let (_, dummy) = mpsc::channel(1);
        std::mem::replace(&mut self.frame_receiver, dummy)
    }

    /// Returns health snapshots for all connections.
    pub fn health(&self) -> Vec<ConnectionHealth> {
        self.connections.iter().map(|conn| conn.health()).collect() // O(1) EXEMPT: monitoring, not per tick
    }

    /// Number of connections in the pool.
    pub fn connection_count(&self) -> usize {
        self.connections.len()
    }

    /// O1-B (2026-04-17): Install runtime subscribe-command channels on
    /// every connection in the pool. Creates one bounded mpsc per
    /// connection (capacity 8 — Phase 2 plus a safety margin), gives the
    /// receiver to the connection, retains the sender on the pool for
    /// `dispatch_subscribe`. Idempotent: calling twice re-creates all
    /// channels (and the prior senders are dropped, which closes the old
    /// receivers cleanly).
    ///
    /// Spawn `spawn_all` AFTER this so the read loop sees the receiver
    /// when it calls `subscribe_cmd_rx.lock().take()`. If called after
    /// spawn, the new receiver is only seen on the NEXT reconnect cycle.
    ///
    /// Cold path — runs once at boot. O(N) over connection count.
    // TEST-EXEMPT: O1-B pool installation; integration covered via main.rs both boot paths
    pub async fn install_subscribe_channels(&self) {
        let mut new_senders = Vec::with_capacity(self.connections.len()); // O(1) EXEMPT: cold path
        for conn in &self.connections {
            let (tx, rx) = mpsc::channel::<crate::websocket::connection::SubscribeCommand>(8);
            let _previous = conn.install_subscribe_channel(rx).await;
            new_senders.push(Some(tx));
        }
        // Replace any previously installed senders.
        if let Ok(mut guard) = self.subscribe_senders.lock() {
            *guard = new_senders;
        }
    }

    /// O1-B: Dispatches a `SubscribeCommand` to the connection with the
    /// most spare instrument capacity. Returns `Some(connection_id)` on
    /// success, `None` if no channels are installed or every connection
    /// is at the per-connection cap.
    ///
    /// Capacity is read from `health()` snapshots and compared against
    /// the dhan-config cap captured by each connection — no global cap
    /// stored on the pool.
    // TEST-EXEMPT: O1-B pool dispatch; covered by phase2_scheduler integration in main.rs
    pub fn dispatch_subscribe(
        &self,
        cmd: crate::websocket::connection::SubscribeCommand,
    ) -> Option<usize> {
        // O(1) EXEMPT: cold path — runs once per Phase 2 trigger or
        // operator override, not per tick.
        //
        // Audit finding #9 (2026-04-17): every `None` return path below
        // ERROR-logs + increments a counter so a missed Phase 2 dispatch
        // is visible to the operator. Previously the function returned
        // None silently, so the caller couldn't distinguish "no channels
        // installed" from "dispatch succeeded but all full".
        let healths = self.health();
        let Ok(senders_guard) = self.subscribe_senders.lock() else {
            metrics::counter!("tv_dispatch_subscribe_errors_total", "reason" => "lock_poisoned")
                .increment(1);
            error!(
                "dispatch_subscribe: senders lock poisoned — pool rebalance impossible. \
                 Restart required."
            );
            return None;
        };
        if senders_guard.is_empty() {
            metrics::counter!(
                "tv_dispatch_subscribe_errors_total",
                "reason" => "channels_not_installed"
            )
            .increment(1);
            error!(
                "dispatch_subscribe: no subscribe channels installed — \
                 install_subscribe_channels() was not called at boot. \
                 Phase 2 stock F&O subscription will not happen."
            );
            return None;
        }
        // Pick the connection with the smallest current subscribed_count.
        let target_idx = match healths
            .iter()
            .enumerate()
            .min_by_key(|(_, h)| h.subscribed_count)
            .map(|(i, _)| i)
        {
            Some(i) => i,
            None => {
                metrics::counter!(
                    "tv_dispatch_subscribe_errors_total",
                    "reason" => "empty_healths"
                )
                .increment(1);
                error!(
                    "dispatch_subscribe: no connection health snapshots — \
                     pool has zero connections spawned."
                );
                return None;
            }
        };
        let sender = match senders_guard.get(target_idx).and_then(|s| s.as_ref()) {
            Some(s) => s,
            None => {
                metrics::counter!(
                    "tv_dispatch_subscribe_errors_total",
                    "reason" => "sender_missing"
                )
                .increment(1);
                error!(
                    target_idx,
                    "dispatch_subscribe: sender slot missing — pool/sender state out of sync."
                );
                return None;
            }
        };
        match sender.try_send(cmd) {
            Ok(()) => Some(target_idx),
            Err(err) => {
                metrics::counter!(
                    "tv_dispatch_subscribe_errors_total",
                    "reason" => "channel_full"
                )
                .increment(1);
                error!(
                    target_idx,
                    ?err,
                    "dispatch_subscribe: channel send failed — connection \
                     at capacity or closed. Phase 2 subscription dropped."
                );
                None
            }
        }
    }

    /// Total instruments across all connections.
    pub fn total_instruments(&self) -> usize {
        self.connections
            .iter()
            .map(|conn| conn.health().subscribed_count)
            .sum()
    }

    /// A4: Pool-level circuit breaker poll. Call this from a background
    /// task every ~5 seconds (or whatever your health-poll cadence is).
    /// Reads all connection states, feeds them to the internal watchdog,
    /// and returns the verdict. Side effects:
    ///
    /// - `WatchdogVerdict::Degraded` → fires Telegram CRITICAL (if a
    ///   notifier is wired upstream by the caller) and increments
    ///   `tv_pool_degraded_seconds_total`
    /// - `WatchdogVerdict::Halt` → caller MUST exit the process with a
    ///   non-zero status so the supervisor (systemd / Docker restart
    ///   policy) brings it back up. The pool cannot self-exit because
    ///   it doesn't own the runtime.
    /// - `WatchdogVerdict::Recovered` → INFO log + metric reset
    /// - `WatchdogVerdict::Degrading` → metric gauge update only
    ///
    /// The watchdog itself is in `pool_watchdog.rs` and is a pure state
    /// machine — this method only wires it to metrics and logs. Thresholds:
    /// - 60s all-down: CRITICAL alert fired exactly once per down-cycle
    /// - 300s all-down: Halt requested (caller decides how to exit)
    ///
    /// Cold path — runs on a watchdog cadence, not per tick.
    // TEST-EXEMPT: covered by test_pool_poll_watchdog_* integration tests and pool_watchdog unit tests
    #[allow(clippy::expect_used)] // APPROVED: lock poison on watchdog is unrecoverable
    pub fn poll_watchdog(&self) -> WatchdogVerdict {
        let healths = self.health();
        let now = std::time::Instant::now();
        // APPROVED: lock poison is unrecoverable — the process is corrupted anyway
        let mut wd = self.watchdog.lock().expect("pool watchdog lock poisoned");
        let verdict = wd.tick(&healths, now);
        drop(wd);

        // A4 log coalescing: re-log the Degraded/Halt arms only when the
        // verdict kind changes, so a multi-hour all-down cycle no longer
        // prints one line every 5s poll. The metrics/gauges below still
        // update every poll — only the human-readable log is edge-gated.
        let a4_kind = a4_log_kind(&verdict);
        let a4_log_edge = self
            .last_a4_log_kind
            .swap(a4_kind, std::sync::atomic::Ordering::Relaxed)
            != a4_kind;

        match verdict {
            WatchdogVerdict::Healthy => {
                metrics::gauge!("tv_pool_degraded_seconds").set(0.0);
            }
            WatchdogVerdict::Recovered { was_down_for } => {
                info!(
                    down_for_secs = was_down_for.as_secs(),
                    "A4: WebSocket pool RECOVERED — at least one connection is live again"
                );
                metrics::counter!("tv_pool_recoveries_total").increment(1);
                metrics::gauge!("tv_pool_degraded_seconds").set(0.0);
            }
            WatchdogVerdict::Degrading { down_for } => {
                metrics::gauge!("tv_pool_degraded_seconds").set(down_for.as_secs_f64());
            }
            WatchdogVerdict::Degraded { down_for } => {
                // Bug B fix (2026-04-20): gate Degraded ERROR by market hours.
                // Pre-market (< 09:00 IST) Dhan idle-resets all TCP connections;
                // firing ERROR → Telegram fires a false alarm during the normal
                // 07:00-09:00 reset window and the operator's inbox is spammed.
                // Outside market hours we log at INFO level only (dashboard +
                // metric still updated so operators can see the signal).
                // A4 log coalescing: gated by `a4_log_edge` so this fires
                // once per down-cycle transition, not every 5s poll.
                if a4_log_edge {
                    if pool_watchdog_is_within_market_hours() {
                        tracing::error!(
                            down_for_secs = down_for.as_secs(),
                            "A4 CRITICAL: WebSocket pool has been FULLY DEGRADED for >60s — ALL connections \
                             are Reconnecting/Disconnected, no market data flowing. Investigate Dhan \
                             server status, token validity, network reachability."
                        );
                    } else {
                        tracing::info!(
                            down_for_secs = down_for.as_secs(),
                            "A4: pool degraded outside market hours (09:00-15:30 IST) — downgraded to INFO \
                             (Dhan routinely resets idle connections pre-market)"
                        );
                    }
                }
                metrics::counter!("tv_pool_degraded_alerts_total").increment(1);
                metrics::gauge!("tv_pool_degraded_seconds").set(down_for.as_secs_f64());
            }
            WatchdogVerdict::Halt { down_for } => {
                // Bug B fix (2026-04-20): gate Halt ERROR by market hours.
                // The 08:38 AM IST halt that blocked the 09:15 IST restart was
                // caused by this ERROR firing pre-market. Outside market hours
                // we demote Halt to INFO — the downstream supervisor no longer
                // force-exits the process (see main.rs:3789 for the matching
                // verdict handler which also checks market-hours post-fix).
                // A4 log coalescing: gated by `a4_log_edge` so this fires
                // once per down-cycle transition, not every 5s poll.
                if a4_log_edge {
                    if pool_watchdog_is_within_market_hours() {
                        tracing::error!(
                            down_for_secs = down_for.as_secs(),
                            "A4 FATAL: WebSocket pool has been FULLY DEGRADED for >300s — initiating process \
                             halt so supervisor can restart us. If this fires repeatedly, Dhan is likely \
                             unreachable or the account/token is locked."
                        );
                    } else {
                        tracing::info!(
                            down_for_secs = down_for.as_secs(),
                            "A4: pool halt verdict outside market hours — downgraded to INFO (no process exit)"
                        );
                    }
                }
                metrics::counter!("tv_pool_halts_total").increment(1);
                metrics::gauge!("tv_pool_degraded_seconds").set(down_for.as_secs_f64());
            }
        }

        verdict
    }

    /// Resets the pool's internal watchdog to its freshly-constructed
    /// `Healthy` state (see `PoolWatchdog::reset`).
    ///
    /// Called by `spawn_pool_watchdog_task` on every **off-hours** poll so
    /// the intentional pre-market DEFERRAL window (no TCP opened until
    /// 09:00 IST) never accumulates a stale `AllDown { since }` that would
    /// trip the 300s `Halt` the instant `is_within_market_hours_ist()` flips
    /// true at 09:00:00. Without this, the first in-hours poll saw ~1055s of
    /// carried-over pre-market down-time and force-exited the process,
    /// paging `[HIGH] WS POOL HALT` + `[HIGH] FAST BOOT` every market open.
    ///
    /// Cold path — runs at most once per 5s off-hours poll, never per tick.
    /// The timer-restart semantics are unit-tested in `pool_watchdog.rs`
    /// (`test_watchdog_reset_*`); the pool wrapper is covered by
    /// `test_pool_reset_watchdog_is_callable_and_keeps_pool_pollable`.
    #[allow(clippy::expect_used)] // APPROVED: lock poison on watchdog is unrecoverable
    pub fn reset_watchdog(&self) {
        // APPROVED: lock poison is unrecoverable — the process is corrupted anyway
        let mut wd = self.watchdog.lock().expect("pool watchdog lock poisoned");
        wd.reset();
    }

    /// A5: Graceful shutdown — requests each connection to send a
    /// `RequestCode: 12` (Disconnect) to Dhan and close its socket cleanly.
    ///
    /// Iterates every connection and calls `request_graceful_shutdown()`,
    /// which is non-blocking (atomic store + notify). The actual Disconnect
    /// JSON is sent by the per-connection read loop within its own
    /// `tokio::select!`. Connections that are already in `Disconnected` or
    /// `Reconnecting` state skip the network send (their read loop isn't
    /// running) — the flag still gets set so subsequent runs don't reconnect.
    ///
    /// Returns the number of connections that were signalled (including ones
    /// that were already dead). Caller is responsible for awaiting the spawn
    /// handles with its own timeout.
    ///
    /// Cold path — runs once per process at SIGTERM.
    // TEST-EXEMPT: covered by test_pool_graceful_shutdown_signals_all and integration tests
    pub fn request_graceful_shutdown(&self) -> usize {
        use crate::websocket::types::ConnectionState;

        let mut live = 0_usize;
        let mut dead = 0_usize;
        for conn in &self.connections {
            let state = conn.health().state;
            match state {
                ConnectionState::Connected | ConnectionState::Connecting => {
                    live = live.saturating_add(1);
                }
                ConnectionState::Disconnected | ConnectionState::Reconnecting => {
                    dead = dead.saturating_add(1);
                }
            }
            // Always set the flag so the outer `run()` loop will not
            // reconnect, even if the read loop isn't listening right now.
            conn.request_graceful_shutdown();
        }

        info!(
            live_connections = live,
            dead_connections = dead,
            total = self.connections.len(),
            "A5: graceful shutdown signalled to all WebSocket connections"
        );
        metrics::counter!("tv_ws_graceful_shutdown_signalled_total")
            .increment(self.connections.len() as u64);

        self.connections.len()
    }

    /// Wave 2 Item 5.2 (WS-GAP-05) — pool supervisor.
    ///
    /// Awaits the spawned `JoinHandle`s and logs ERROR (with the
    /// `WS-GAP-05` code) + increments `tv_ws_pool_respawn_total` for
    /// every per-connection task that exits unexpectedly (panic, error
    /// return, or aborted). Called by `main.rs` AFTER `spawn_all()` so
    /// the operator gets a Telegram alert if a connection task dies
    /// silently.
    ///
    /// Design note: the per-connection task owns its own reconnect loop
    /// (`wait_with_backoff` + Wave 2 Item 5 sleep path), and since W2#8
    /// (2026-07-10) the slot task ALSO self-respawns via
    /// `run_supervised_pool_slot` on an unexpected clean exit (server
    /// Close frame / stream-end) — see the WS-GAP-05 runbook. A
    /// `JoinHandle` therefore resolves ONLY on real terminal events
    /// (panic — process-aborting in release, `NonReconnectableDisconnect`,
    /// `ReconnectionExhausted` under a finite budget, graceful shutdown,
    /// or a teardown abort). This drain surfaces those via metrics +
    /// ERROR logs at teardown so nothing exits silently.
    ///
    /// Returns when all handles have exited.
    // O(1) EXEMPT: cold-path supervisor — runs once per session.
    pub async fn supervise_pool(handles: Vec<tokio::task::JoinHandle<Result<(), WebSocketError>>>) {
        let total = handles.len();
        if total == 0 {
            tracing::warn!("WS-GAP-05 pool supervisor invoked with 0 handles — nothing to watch");
            return;
        }
        // Use FuturesUnordered so we react to whichever handle exits
        // first, in O(1) per event.
        // O(1) EXEMPT: cold-path supervisor — runs once per session.
        use futures_util::stream::{FuturesUnordered, StreamExt};
        let mut pending: FuturesUnordered<_> = FuturesUnordered::new();
        for h in handles {
            pending.push(h);
        }
        let mut idx: usize = 0;
        while let Some(join_result) = pending.next().await {
            idx = idx.saturating_add(1);
            match join_result {
                Ok(Ok(())) => {
                    tracing::info!(
                        slot = idx,
                        total,
                        "WS pool task exited cleanly (graceful shutdown)"
                    );
                }
                Ok(Err(ws_err)) => {
                    tracing::error!(
                        slot = idx,
                        total,
                        ?ws_err,
                        code =
                            tickvault_common::error_code::ErrorCode::WsGap05PoolRespawn.code_str(),
                        "WS-GAP-05 pool task exited with WebSocketError — supervisor recorded"
                    );
                    metrics::counter!("tv_ws_pool_respawn_total", "reason" => "ws_error")
                        .increment(1);
                }
                Err(join_err) => {
                    let label = if join_err.is_panic() {
                        "panic"
                    } else if join_err.is_cancelled() {
                        "cancelled"
                    } else {
                        "unknown"
                    };
                    tracing::error!(
                        slot = idx,
                        total,
                        kind = label,
                        code =
                            tickvault_common::error_code::ErrorCode::WsGap05PoolRespawn.code_str(),
                        "WS-GAP-05 pool task did not exit cleanly — supervisor recorded"
                    );
                    metrics::counter!("tv_ws_pool_respawn_total", "reason" => label).increment(1);
                }
            }
        }
        tracing::info!(total, "WS-GAP-05 pool supervisor — all handles drained");
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::arithmetic_side_effects)] // APPROVED: test code
mod tests {
    use super::*;
    use crate::websocket::types::ConnectionState;
    use tickvault_common::types::ExchangeSegment;

    fn make_test_dhan_config() -> DhanConfig {
        DhanConfig {
            websocket_url: "wss://api-feed.dhan.co".to_string(),
            order_update_websocket_url: "wss://api-order-update.dhan.co".to_string(),
            rest_api_base_url: "https://api.dhan.co/v2".to_string(),
            auth_base_url: "https://auth.dhan.co".to_string(),
            instrument_csv_url: "https://example.com/csv".to_string(),
            instrument_csv_fallback_url: "https://example.com/csv-fallback".to_string(),
            max_instruments_per_connection: 5000,
            max_websocket_connections: 5,
            sandbox_base_url: String::new(),
        }
    }

    fn make_test_ws_config() -> WebSocketConfig {
        WebSocketConfig {
            ping_interval_secs: 10,
            pong_timeout_secs: 10,
            max_consecutive_pong_failures: 2,
            reconnect_initial_delay_ms: 500,
            reconnect_max_delay_ms: 30000,
            reconnect_max_attempts: 10,
            subscription_batch_size: 100,
            connection_stagger_ms: 0, // No stagger in tests for speed
            activity_watchdog_threshold_secs: 50,
        }
    }

    fn make_test_token_handle() -> TokenHandle {
        Arc::new(arc_swap::ArcSwap::new(Arc::new(None)))
    }

    fn make_instruments(count: usize) -> Vec<InstrumentSubscription> {
        (0..count)
            .map(|i| InstrumentSubscription::new(ExchangeSegment::NseFno, (i as u64) + 1000))
            .collect()
    }

    /// Extract `CapacityExceeded` fields from a `WebSocketError`, panicking if
    /// the variant doesn't match. Consolidates 3 identical panic sites.
    #[track_caller]
    fn unwrap_capacity_exceeded(err: WebSocketError) -> (usize, usize) {
        match err {
            WebSocketError::CapacityExceeded {
                requested,
                capacity,
            } => (requested, capacity),
            other => panic!("expected CapacityExceeded, got {other:?}"),
        }
    }

    #[test]
    fn test_pool_empty_instruments_creates_max_connections() {
        let pool = WebSocketConnectionPool::new(
            make_test_token_handle(),
            "test-client".to_string(),
            make_test_dhan_config(),
            make_test_ws_config(),
            vec![],
            FeedMode::Ticker,
            None,
        )
        .unwrap();
        assert_eq!(pool.connection_count(), 5);
        assert_eq!(pool.total_instruments(), 0);
    }

    #[test]
    fn test_pool_under_5000_still_creates_max_connections() {
        let pool = WebSocketConnectionPool::new(
            make_test_token_handle(),
            "test-client".to_string(),
            make_test_dhan_config(),
            make_test_ws_config(),
            make_instruments(3000),
            FeedMode::Quote,
            None,
        )
        .unwrap();
        assert_eq!(pool.connection_count(), 5);
        assert_eq!(pool.total_instruments(), 3000);
    }

    #[test]
    fn test_pool_exactly_5000_creates_max_connections() {
        let pool = WebSocketConnectionPool::new(
            make_test_token_handle(),
            "test-client".to_string(),
            make_test_dhan_config(),
            make_test_ws_config(),
            make_instruments(5000),
            FeedMode::Full,
            None,
        )
        .unwrap();
        assert_eq!(pool.connection_count(), 5);
    }

    #[test]
    fn test_pool_5001_creates_max_connections() {
        let pool = WebSocketConnectionPool::new(
            make_test_token_handle(),
            "test-client".to_string(),
            make_test_dhan_config(),
            make_test_ws_config(),
            make_instruments(5001),
            FeedMode::Ticker,
            None,
        )
        .unwrap();
        assert_eq!(pool.connection_count(), 5);
        assert_eq!(pool.total_instruments(), 5001);
    }

    // ----------------------------------------------------------------------
    // AWS-lifecycle LOCKED (PR #7b) — 1-conn main-feed pool.
    //
    // Under the only legal scope (`Indices4Only`), main.rs overrides
    // `dhan_config.max_websocket_connections` via
    // `effective_main_feed_pool_size(scope, configured)` to
    // `PHASE_0_MAIN_FEED_CONNECTION_COUNT = 1` BEFORE calling the pool
    // constructor. These ratchets verify that:
    //   * a 1-conn pool builds correctly with 222 SIDs (the Phase 0 universe)
    //   * ALL 222 SIDs land on conn 0 (round-robin with N=1 = "all on 0")
    //   * the other 4 connection objects are NOT created (no false-positive
    //     "idle conn" watchdog signals)
    // ----------------------------------------------------------------------

    #[test]
    fn test_pool_one_main_feed_conn_builds_correctly_with_phase_0_universe() {
        // Phase 0 universe = 222 SIDs ≤ 5000-per-conn cap → fits on 1 conn.
        let mut dhan = make_test_dhan_config();
        dhan.max_websocket_connections =
            tickvault_common::constants::PHASE_0_MAIN_FEED_CONNECTION_COUNT;
        let pool = WebSocketConnectionPool::new(
            make_test_token_handle(),
            "test-client".to_string(),
            dhan,
            make_test_ws_config(),
            make_instruments(222),
            FeedMode::Ticker,
            None,
        )
        .expect("Phase 0 1-conn pool with 222 instruments must build");
        assert_eq!(
            pool.connection_count(),
            tickvault_common::constants::PHASE_0_MAIN_FEED_CONNECTION_COUNT,
            "Phase 0 pool must spawn exactly 1 main-feed conn",
        );
        assert_eq!(
            pool.total_instruments(),
            222,
            "all 222 Phase 0 SIDs must be assigned",
        );
    }

    #[test]
    fn test_pool_one_main_feed_conn_assigns_all_instruments_to_conn_zero() {
        // Round-robin distribution with N=1 collapses to "all on slot 0".
        let mut dhan = make_test_dhan_config();
        dhan.max_websocket_connections = 1;
        let pool = WebSocketConnectionPool::new(
            make_test_token_handle(),
            "test-client".to_string(),
            dhan,
            make_test_ws_config(),
            make_instruments(222),
            FeedMode::Quote,
            None,
        )
        .unwrap();

        // Single-conn pool → exactly 1 health entry → that entry holds 222.
        let health = pool.health();
        assert_eq!(
            health.len(),
            1,
            "Phase 0 pool must expose exactly 1 health entry",
        );
        assert_eq!(
            health[0].subscribed_count, 222,
            "all 222 SIDs must be assigned to conn 0",
        );
    }

    #[test]
    fn test_pool_one_main_feed_conn_capacity_limit_still_5000() {
        // The per-conn capacity cap is still 5000 even when only 1 conn
        // spawns. CapacityExceeded must fire at 5001 SIDs under N=1.
        let mut dhan = make_test_dhan_config();
        dhan.max_websocket_connections = 1;
        let err = WebSocketConnectionPool::new(
            make_test_token_handle(),
            "test-client".to_string(),
            dhan,
            make_test_ws_config(),
            make_instruments(5001),
            FeedMode::Ticker,
            None,
        )
        .expect_err("5001 instruments on 1 conn must exceed effective capacity");
        match err {
            WebSocketError::CapacityExceeded {
                requested,
                capacity,
            } => {
                assert_eq!(requested, 5001);
                assert_eq!(capacity, 5000, "single-conn effective capacity = 5000");
            }
            other => panic!("expected CapacityExceeded, got {other:?}"),
        }
    }

    #[test]
    fn test_pool_25000_uses_five_connections() {
        let pool = WebSocketConnectionPool::new(
            make_test_token_handle(),
            "test-client".to_string(),
            make_test_dhan_config(),
            make_test_ws_config(),
            make_instruments(25000),
            FeedMode::Full,
            None,
        )
        .unwrap();
        assert_eq!(pool.connection_count(), 5);
        assert_eq!(pool.total_instruments(), 25000);
    }

    #[test]
    fn test_pool_exceeds_capacity_returns_error() {
        let result = WebSocketConnectionPool::new(
            make_test_token_handle(),
            "test-client".to_string(),
            make_test_dhan_config(),
            make_test_ws_config(),
            make_instruments(25001),
            FeedMode::Ticker,
            None,
        );
        assert!(result.is_err());
        let (requested, capacity) = unwrap_capacity_exceeded(result.unwrap_err());
        assert_eq!(requested, 25001);
        assert_eq!(capacity, 25000);
    }

    #[test]
    fn test_pool_round_robin_distribution() {
        // 10001 instruments distributed round-robin across 5 connections
        let pool = WebSocketConnectionPool::new(
            make_test_token_handle(),
            "test-client".to_string(),
            make_test_dhan_config(),
            make_test_ws_config(),
            make_instruments(10001),
            FeedMode::Ticker,
            None,
        )
        .unwrap();
        assert_eq!(pool.connection_count(), 5);

        let healths = pool.health();
        // Round-robin across 5: 2001 + 2000 + 2000 + 2000 + 2000
        let total: usize = healths.iter().map(|h| h.subscribed_count).sum();
        assert_eq!(total, 10001);
        // Verify no connection exceeds max_per_conn (5000)
        for h in &healths {
            assert!(h.subscribed_count <= 5000);
        }
    }

    #[test]
    fn test_pool_single_instrument() {
        let pool = WebSocketConnectionPool::new(
            make_test_token_handle(),
            "test-client".to_string(),
            make_test_dhan_config(),
            make_test_ws_config(),
            make_instruments(1),
            FeedMode::Ticker,
            None,
        )
        .unwrap();
        assert_eq!(pool.connection_count(), 5);
        assert_eq!(pool.total_instruments(), 1);
    }

    #[test]
    fn test_pool_boundary_4999_max_connections() {
        let pool = WebSocketConnectionPool::new(
            make_test_token_handle(),
            "test-client".to_string(),
            make_test_dhan_config(),
            make_test_ws_config(),
            make_instruments(4999),
            FeedMode::Quote,
            None,
        )
        .unwrap();
        assert_eq!(pool.connection_count(), 5);
    }

    #[test]
    fn test_pool_boundary_10000_max_connections() {
        let pool = WebSocketConnectionPool::new(
            make_test_token_handle(),
            "test-client".to_string(),
            make_test_dhan_config(),
            make_test_ws_config(),
            make_instruments(10000),
            FeedMode::Ticker,
            None,
        )
        .unwrap();
        assert_eq!(pool.connection_count(), 5);
    }

    #[test]
    fn test_pool_boundary_15000_max_connections() {
        let pool = WebSocketConnectionPool::new(
            make_test_token_handle(),
            "test-client".to_string(),
            make_test_dhan_config(),
            make_test_ws_config(),
            make_instruments(15000),
            FeedMode::Full,
            None,
        )
        .unwrap();
        assert_eq!(pool.connection_count(), 5);
    }

    #[test]
    fn test_pool_boundary_20000_max_connections() {
        let pool = WebSocketConnectionPool::new(
            make_test_token_handle(),
            "test-client".to_string(),
            make_test_dhan_config(),
            make_test_ws_config(),
            make_instruments(20000),
            FeedMode::Ticker,
            None,
        )
        .unwrap();
        assert_eq!(pool.connection_count(), 5);
    }

    #[test]
    fn test_pool_take_frame_receiver_once() {
        let mut pool = WebSocketConnectionPool::new(
            make_test_token_handle(),
            "test-client".to_string(),
            make_test_dhan_config(),
            make_test_ws_config(),
            make_instruments(100),
            FeedMode::Ticker,
            None,
        )
        .unwrap();

        // First take succeeds — we get the real receiver
        let _receiver = pool.take_frame_receiver();

        // Second take gets a dummy channel (capacity 1, no senders)
        let mut dummy_receiver = pool.take_frame_receiver();
        // Dummy receiver should return None immediately since no sender exists
        assert!(
            dummy_receiver.try_recv().is_err(),
            "Second take should return empty dummy receiver"
        );
    }

    #[test]
    fn test_pool_debug_impl() {
        let pool = WebSocketConnectionPool::new(
            make_test_token_handle(),
            "test-client".to_string(),
            make_test_dhan_config(),
            make_test_ws_config(),
            make_instruments(5001),
            FeedMode::Ticker,
            None,
        )
        .unwrap();
        let debug_str = format!("{pool:?}");
        assert!(debug_str.contains("WebSocketConnectionPool"));
        assert!(debug_str.contains("connection_count"));
        assert!(debug_str.contains("5")); // Always 5 connections
    }

    #[test]
    fn test_pool_config_override_smaller_max_per_connection() {
        // Config says 2000 per connection instead of default 5000
        let config = DhanConfig {
            max_instruments_per_connection: 2000,
            ..make_test_dhan_config()
        };
        let pool = WebSocketConnectionPool::new(
            make_test_token_handle(),
            "test-client".to_string(),
            config,
            make_test_ws_config(),
            make_instruments(5000),
            FeedMode::Full,
            None,
        )
        .unwrap();
        // Always max connections (5), instruments distributed round-robin
        assert_eq!(pool.connection_count(), 5);
        assert_eq!(pool.total_instruments(), 5000);
    }

    #[test]
    fn test_pool_total_instruments_matches_after_distribution() {
        // Test that round-robin doesn't lose or gain instruments
        for count in [
            1, 100, 4999, 5000, 5001, 10000, 10001, 15000, 20000, 24999, 25000,
        ] {
            let pool = WebSocketConnectionPool::new(
                make_test_token_handle(),
                "test-client".to_string(),
                make_test_dhan_config(),
                make_test_ws_config(),
                make_instruments(count),
                FeedMode::Ticker,
                None,
            )
            .unwrap();
            assert_eq!(
                pool.total_instruments(),
                count,
                "Mismatch for {count} instruments"
            );
        }
    }

    #[test]
    fn test_pool_health_returns_all_connections() {
        let pool = WebSocketConnectionPool::new(
            make_test_token_handle(),
            "test-client".to_string(),
            make_test_dhan_config(),
            make_test_ws_config(),
            make_instruments(12000),
            FeedMode::Full,
            None,
        )
        .unwrap();
        let healths = pool.health();
        assert_eq!(healths.len(), pool.connection_count());
        for (idx, health) in healths.iter().enumerate() {
            assert_eq!(health.connection_id, idx as u8);
            assert_eq!(health.state, ConnectionState::Disconnected);
        }
    }

    // --- New tests for always-max-connections + dynamic capacity ---

    #[test]
    fn test_pool_config_max_connections_3() {
        // Config limits to 3 connections
        let config = DhanConfig {
            max_websocket_connections: 3,
            ..make_test_dhan_config()
        };
        let pool = WebSocketConnectionPool::new(
            make_test_token_handle(),
            "test-client".to_string(),
            config,
            make_test_ws_config(),
            make_instruments(5000),
            FeedMode::Ticker,
            None,
        )
        .unwrap();
        assert_eq!(pool.connection_count(), 3);
        assert_eq!(pool.total_instruments(), 5000);
    }

    #[test]
    fn test_pool_config_max_per_conn_2000_exceeds() {
        // max_per_conn=2000, max_conns=5 → effective capacity = 10,000
        let config = DhanConfig {
            max_instruments_per_connection: 2000,
            ..make_test_dhan_config()
        };
        let result = WebSocketConnectionPool::new(
            make_test_token_handle(),
            "test-client".to_string(),
            config,
            make_test_ws_config(),
            make_instruments(15000),
            FeedMode::Ticker,
            None,
        );
        assert!(result.is_err());
        let (requested, capacity) = unwrap_capacity_exceeded(result.unwrap_err());
        assert_eq!(requested, 15000);
        assert_eq!(capacity, 10000); // 2000 × 5
    }

    #[test]
    fn test_pool_config_max_conns_100_capped_at_5() {
        // Config says 100, but MAX_WEBSOCKET_CONNECTIONS caps at 5
        let config = DhanConfig {
            max_websocket_connections: 100,
            ..make_test_dhan_config()
        };
        let pool = WebSocketConnectionPool::new(
            make_test_token_handle(),
            "test-client".to_string(),
            config,
            make_test_ws_config(),
            make_instruments(5000),
            FeedMode::Ticker,
            None,
        )
        .unwrap();
        assert_eq!(pool.connection_count(), 5);
    }

    // --- Property-based test ---

    mod proptest_pool {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            #[test]
            fn test_pool_no_instrument_loss_any_count(count in 0usize..=25000) {
                let instruments = make_instruments(count);
                let pool = WebSocketConnectionPool::new(
                    make_test_token_handle(),
                    "test".to_string(),
                    make_test_dhan_config(),
                    make_test_ws_config(),
                    instruments,
                    FeedMode::Ticker,
                    None,
                ).unwrap();
                prop_assert_eq!(pool.total_instruments(), count);
                prop_assert_eq!(pool.connection_count(), 5);
            }
        }
    }

    // --- spawn_all() Tests ---

    /// RAII guard: pin `is_within_market_hours_ist()` to `true` for the
    /// lifetime of the guard.
    ///
    /// The off-hours boot gate added in `market_hours_gate.rs` (2026-04-24,
    /// fix-offhours-ws-connect-flap) otherwise sleeps each `spawn_all` task
    /// until the next 09:00 IST, which would hang every `test_spawn_all_*`
    /// when CI runs outside market hours (most of the 24-hour day).
    /// Wrapping each test with this guard forces the gate to short-circuit
    /// and preserves the pre-fix test semantics (tasks run immediately).
    ///
    /// **Race-safe:** a process-global refcount ensures the override stays
    /// `true` until the LAST concurrent guard drops. Without this,
    /// `cargo test` running tests in parallel would see test A's drop
    /// flip the atomic to `false` while test B's tasks are still parked
    /// at the gate, re-introducing the exact hang this guard exists to
    /// prevent.
    static MARKET_HOURS_GUARD_REFCOUNT: std::sync::atomic::AtomicUsize =
        std::sync::atomic::AtomicUsize::new(0);
    struct MarketHoursTestGuard;
    impl MarketHoursTestGuard {
        fn new() -> Self {
            MARKET_HOURS_GUARD_REFCOUNT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            tickvault_common::market_hours::set_test_force_in_market_hours(true);
            Self
        }
    }
    impl Drop for MarketHoursTestGuard {
        fn drop(&mut self) {
            let prev =
                MARKET_HOURS_GUARD_REFCOUNT.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
            // Only clear the override when we were the LAST guard holding it.
            if prev == 1 {
                tickvault_common::market_hours::set_test_force_in_market_hours(false);
            }
        }
    }

    /// Session 8 final-sweep helper — drain all spawned WebSocket tasks for
    /// test cleanup without hanging. Signals graceful shutdown first, then
    /// aborts each handle so the task exits even if the deeper connection
    /// loop ignores `reconnect_max_attempts: 0`. Without this, every
    /// `test_spawn_all_*` variant hangs forever because the connection loop
    /// retries indefinitely against a fake Dhan endpoint.
    async fn drain_handles_or_timeout(
        pool: &WebSocketConnectionPool,
        handles: Vec<tokio::task::JoinHandle<Result<(), WebSocketError>>>,
    ) {
        pool.request_graceful_shutdown();
        for handle in handles {
            handle.abort();
            let _ = tokio::time::timeout(std::time::Duration::from_millis(500), handle).await;
        }
    }

    #[tokio::test]
    async fn test_spawn_all_returns_correct_number_of_handles() {
        let _guard = MarketHoursTestGuard::new();
        let pool = WebSocketConnectionPool::new(
            make_test_token_handle(), // No token → connections will fail
            "test-client".to_string(),
            make_test_dhan_config(),
            WebSocketConfig {
                reconnect_max_attempts: 1, // one attempt, then exhaust (0 = retry forever in prod)
                reconnect_initial_delay_ms: 1,
                reconnect_max_delay_ms: 1,
                ..make_test_ws_config()
            },
            make_instruments(100),
            FeedMode::Ticker,
            None,
        )
        .unwrap();

        let handles = pool.spawn_all().await;
        assert_eq!(
            handles.len(),
            pool.connection_count(),
            "spawn_all must return one handle per connection"
        );

        // Drain with timeout — see drain_handles_or_timeout doc for context.
        drain_handles_or_timeout(&pool, handles).await;
    }

    #[tokio::test]
    async fn test_spawn_all_empty_pool_returns_handles() {
        let _guard = MarketHoursTestGuard::new();
        let pool = WebSocketConnectionPool::new(
            make_test_token_handle(),
            "test-client".to_string(),
            make_test_dhan_config(),
            WebSocketConfig {
                reconnect_max_attempts: 1, // one attempt, then exhaust (0 = retry forever in prod)
                reconnect_initial_delay_ms: 1,
                reconnect_max_delay_ms: 1,
                ..make_test_ws_config()
            },
            vec![], // no instruments
            FeedMode::Ticker,
            None,
        )
        .unwrap();

        let handles = pool.spawn_all().await;
        assert_eq!(handles.len(), 5, "empty pool still has 5 connections");

        drain_handles_or_timeout(&pool, handles).await;
    }

    #[tokio::test]
    async fn test_spawn_all_tasks_return_reconnection_exhausted() {
        let _guard = MarketHoursTestGuard::new();
        let pool = WebSocketConnectionPool::new(
            make_test_token_handle(),
            "test-client".to_string(),
            make_test_dhan_config(),
            WebSocketConfig {
                reconnect_max_attempts: 2,
                reconnect_initial_delay_ms: 1,
                reconnect_max_delay_ms: 1,
                ..make_test_ws_config()
            },
            make_instruments(10),
            FeedMode::Ticker,
            None,
        )
        .unwrap();

        let handles = pool.spawn_all().await;

        for handle in handles {
            let join_result = handle.await.unwrap();
            match join_result {
                Err(WebSocketError::ReconnectionExhausted { attempts, .. }) => {
                    assert_eq!(attempts, 2);
                }
                other => panic!("Expected ReconnectionExhausted, got {other:?}"),
            }
        }
    }

    // --- Additional coverage tests ---

    #[test]
    fn test_pool_config_max_connections_1() {
        // Config limits to 1 connection — all instruments on one connection.
        let config = DhanConfig {
            max_websocket_connections: 1,
            ..make_test_dhan_config()
        };
        let pool = WebSocketConnectionPool::new(
            make_test_token_handle(),
            "test-client".to_string(),
            config,
            make_test_ws_config(),
            make_instruments(100),
            FeedMode::Ticker,
            None,
        )
        .unwrap();
        assert_eq!(pool.connection_count(), 1);
        assert_eq!(pool.total_instruments(), 100);

        let healths = pool.health();
        assert_eq!(healths.len(), 1);
        assert_eq!(healths[0].subscribed_count, 100);
        assert_eq!(healths[0].connection_id, 0);
    }

    #[test]
    fn test_pool_config_max_per_conn_1() {
        // max_per_conn=1 means effective capacity = 1 * 5 = 5.
        let config = DhanConfig {
            max_instruments_per_connection: 1,
            ..make_test_dhan_config()
        };
        let pool = WebSocketConnectionPool::new(
            make_test_token_handle(),
            "test-client".to_string(),
            config,
            make_test_ws_config(),
            make_instruments(5),
            FeedMode::Ticker,
            None,
        )
        .unwrap();
        assert_eq!(pool.connection_count(), 5);
        assert_eq!(pool.total_instruments(), 5);
        // Each connection should have exactly 1 instrument
        for h in pool.health() {
            assert_eq!(h.subscribed_count, 1);
        }
    }

    #[test]
    fn test_pool_config_max_per_conn_1_exceeds() {
        // max_per_conn=1, max_conns=5 → capacity=5. 6 instruments should fail.
        let config = DhanConfig {
            max_instruments_per_connection: 1,
            ..make_test_dhan_config()
        };
        let result = WebSocketConnectionPool::new(
            make_test_token_handle(),
            "test-client".to_string(),
            config,
            make_test_ws_config(),
            make_instruments(6),
            FeedMode::Ticker,
            None,
        );
        assert!(result.is_err());
        let (requested, capacity) = unwrap_capacity_exceeded(result.unwrap_err());
        assert_eq!(requested, 6);
        assert_eq!(capacity, 5);
    }

    #[test]
    fn test_pool_health_initial_state_all_disconnected() {
        // All connections start in Disconnected state.
        let pool = WebSocketConnectionPool::new(
            make_test_token_handle(),
            "test-client".to_string(),
            make_test_dhan_config(),
            make_test_ws_config(),
            make_instruments(500),
            FeedMode::Full,
            None,
        )
        .unwrap();
        for h in pool.health() {
            assert_eq!(h.state, ConnectionState::Disconnected);
            assert_eq!(h.total_reconnections, 0);
        }
    }

    #[test]
    fn test_pool_health_connection_ids_sequential() {
        // Connection IDs should be 0, 1, 2, 3, 4.
        let pool = WebSocketConnectionPool::new(
            make_test_token_handle(),
            "test-client".to_string(),
            make_test_dhan_config(),
            make_test_ws_config(),
            make_instruments(100),
            FeedMode::Ticker,
            None,
        )
        .unwrap();
        let healths = pool.health();
        for (idx, h) in healths.iter().enumerate() {
            assert_eq!(h.connection_id, idx as u8);
        }
    }

    #[test]
    fn test_pool_take_frame_receiver_returns_working_receiver() {
        // The first take should return a receiver connected to the real senders.
        let mut pool = WebSocketConnectionPool::new(
            make_test_token_handle(),
            "test-client".to_string(),
            make_test_dhan_config(),
            make_test_ws_config(),
            make_instruments(10),
            FeedMode::Ticker,
            None,
        )
        .unwrap();

        let mut receiver = pool.take_frame_receiver();
        // No data has been sent yet, so try_recv should return Empty (not Disconnected)
        match receiver.try_recv() {
            Err(mpsc::error::TryRecvError::Empty) => {} // expected
            Err(mpsc::error::TryRecvError::Disconnected) => {
                // Also acceptable — senders may not be held open
            }
            Ok(_) => panic!("Should not receive data before connection starts"),
        }
    }

    #[test]
    fn test_pool_debug_format_with_various_sizes() {
        for count in [0, 1, 100, 5000] {
            let config = DhanConfig {
                max_websocket_connections: 3,
                ..make_test_dhan_config()
            };
            let pool = WebSocketConnectionPool::new(
                make_test_token_handle(),
                "test-client".to_string(),
                config,
                make_test_ws_config(),
                make_instruments(count),
                FeedMode::Ticker,
                None,
            )
            .unwrap();
            let debug_str = format!("{pool:?}");
            assert!(debug_str.contains("3"));
            assert!(debug_str.contains("WebSocketConnectionPool"));
        }
    }

    #[test]
    fn test_pool_round_robin_even_distribution() {
        // 10 instruments across 5 connections → 2 each.
        let pool = WebSocketConnectionPool::new(
            make_test_token_handle(),
            "test-client".to_string(),
            make_test_dhan_config(),
            make_test_ws_config(),
            make_instruments(10),
            FeedMode::Ticker,
            None,
        )
        .unwrap();
        for h in pool.health() {
            assert_eq!(h.subscribed_count, 2);
        }
    }

    #[test]
    fn test_pool_round_robin_uneven_distribution() {
        // 7 instruments across 5 connections → 2, 2, 1, 1, 1.
        let pool = WebSocketConnectionPool::new(
            make_test_token_handle(),
            "test-client".to_string(),
            make_test_dhan_config(),
            make_test_ws_config(),
            make_instruments(7),
            FeedMode::Ticker,
            None,
        )
        .unwrap();
        let healths = pool.health();
        let counts: Vec<usize> = healths.iter().map(|h| h.subscribed_count).collect();
        assert_eq!(counts, vec![2, 2, 1, 1, 1]);
    }

    /// 2026-05-02 — pin the equal-split guarantee for the all-expiries
    /// universe (operator confirmed ~10-11K NIFTY+BANKNIFTY+SENSEX
    /// contracts after `subscription_planner` Section 3 revert). Asserts
    /// that for any instrument count `N` distributed across 5 conns,
    /// the per-conn count differs by AT MOST 1.
    ///
    /// This is the property the operator asked about explicitly:
    /// "split equally across entire 5 connections". Round-robin
    /// (`idx % num_connections`) provides this guarantee mechanically;
    /// this test pins it for several realistic post-revert universe
    /// sizes so a future regression away from round-robin is caught.
    #[test]
    fn test_pool_round_robin_split_within_one_for_all_expiries_universe() {
        // Realistic instrument counts after the 2026-05-02 all-expiries
        // revert. Lower bound = ~10K, upper bound = ~11.3K (+ headroom
        // for daily strike additions).
        for n in [10_000_usize, 10_300, 10_500, 10_900, 11_300] {
            let pool = WebSocketConnectionPool::new(
                make_test_token_handle(),
                "test-client".to_string(),
                make_test_dhan_config(),
                make_test_ws_config(),
                make_instruments(n),
                FeedMode::Ticker,
                None,
            )
            .unwrap_or_else(|e| panic!("pool init failed for n={n}: {e:?}"));
            let counts: Vec<usize> = pool.health().iter().map(|h| h.subscribed_count).collect();
            assert_eq!(counts.len(), 5, "must always be 5 conns");
            let total: usize = counts.iter().sum();
            assert_eq!(total, n, "no instruments dropped during distribution");
            let max = counts.iter().max().copied().unwrap_or(0);
            let min = counts.iter().min().copied().unwrap_or(0);
            assert!(
                max - min <= 1,
                "n={n}: per-conn count must differ by at most 1, got max={max} min={min}, counts={counts:?}"
            );
        }
    }

    #[test]
    fn test_pool_config_exceeds_max_websocket_connections_constant() {
        // MAX_WEBSOCKET_CONNECTIONS is 5. Config of 10 should be capped.
        let config = DhanConfig {
            max_websocket_connections: 10,
            ..make_test_dhan_config()
        };
        let pool = WebSocketConnectionPool::new(
            make_test_token_handle(),
            "test-client".to_string(),
            config,
            make_test_ws_config(),
            make_instruments(100),
            FeedMode::Ticker,
            None,
        )
        .unwrap();
        assert_eq!(pool.connection_count(), 5);
    }

    #[test]
    fn test_pool_config_exceeds_max_instruments_per_connection_constant() {
        // MAX_INSTRUMENTS_PER_WEBSOCKET_CONNECTION is 5000. Config of 10000 should be capped.
        let config = DhanConfig {
            max_instruments_per_connection: 10000,
            ..make_test_dhan_config()
        };
        let pool = WebSocketConnectionPool::new(
            make_test_token_handle(),
            "test-client".to_string(),
            config,
            make_test_ws_config(),
            make_instruments(25000),
            FeedMode::Ticker,
            None,
        )
        .unwrap();
        assert_eq!(pool.connection_count(), 5);
        // Effective capacity is still 25000 (5000 * 5), not 50000 (10000 * 5)
        assert_eq!(pool.total_instruments(), 25000);
    }

    #[tokio::test]
    async fn test_spawn_all_with_single_connection() {
        let _guard = MarketHoursTestGuard::new();
        let config = DhanConfig {
            max_websocket_connections: 1,
            ..make_test_dhan_config()
        };
        let pool = WebSocketConnectionPool::new(
            make_test_token_handle(),
            "test-client".to_string(),
            config,
            WebSocketConfig {
                reconnect_max_attempts: 1, // one attempt, then exhaust (0 = retry forever in prod)
                reconnect_initial_delay_ms: 1,
                reconnect_max_delay_ms: 1,
                ..make_test_ws_config()
            },
            make_instruments(5),
            FeedMode::Ticker,
            None,
        )
        .unwrap();

        let handles = pool.spawn_all().await;
        assert_eq!(handles.len(), 1);

        drain_handles_or_timeout(&pool, handles).await;
    }

    #[tokio::test]
    async fn test_spawn_all_with_stagger_returns_correct_handles() {
        let _guard = MarketHoursTestGuard::new();
        let pool = WebSocketConnectionPool::new(
            make_test_token_handle(),
            "test-client".to_string(),
            make_test_dhan_config(),
            WebSocketConfig {
                reconnect_max_attempts: 1, // one attempt, then exhaust (0 = retry forever in prod)
                reconnect_initial_delay_ms: 1,
                reconnect_max_delay_ms: 1,
                connection_stagger_ms: 50, // 50ms stagger for fast test
                ..make_test_ws_config()
            },
            make_instruments(10),
            FeedMode::Ticker,
            None,
        )
        .unwrap();

        let start = tokio::time::Instant::now();
        let handles = pool.spawn_all().await;
        let elapsed = start.elapsed();

        assert_eq!(handles.len(), 5);
        // 4 gaps of 50ms = 200ms minimum
        assert!(
            elapsed >= std::time::Duration::from_millis(200),
            "Stagger should cause at least 200ms delay for 5 connections, got {elapsed:?}",
        );

        drain_handles_or_timeout(&pool, handles).await;
    }

    // =======================================================================
    // A4: Pool-level circuit breaker (watchdog integration)
    // =======================================================================

    /// A4: A freshly-constructed pool has all 5 connections in `Disconnected`
    /// state. The first watchdog poll MUST transition to Degrading (starting
    /// the all-down cycle), not stay Healthy.
    #[test]
    fn test_pool_poll_watchdog_initial_state_is_degrading() {
        let pool = WebSocketConnectionPool::new(
            make_test_token_handle(),
            "test-client".to_string(),
            make_test_dhan_config(),
            make_test_ws_config(),
            make_instruments(100),
            FeedMode::Ticker,
            None,
        )
        .unwrap();

        let verdict = pool.poll_watchdog();
        assert!(
            matches!(verdict, WatchdogVerdict::Degrading { .. }),
            "fresh pool with all-Disconnected connections must transition \
             into Degrading on first poll, got {verdict:?}"
        );
    }

    /// Pre-market deferral fix (2026-06-03): `reset_watchdog` returns the
    /// pool's internal watchdog to `Healthy` so the intentional pre-market
    /// DEFERRED window (no TCP opened until 09:00 IST) never accumulates a
    /// stale `AllDown { since }` that trips the 300s Halt at 09:00:00. The
    /// timer-restart semantics are unit-tested in `pool_watchdog.rs`
    /// (`test_watchdog_reset_*`); here we verify the pool wrapper delegates
    /// without panicking, is idempotent, and leaves the pool pollable.
    #[test]
    fn test_pool_reset_watchdog_is_callable_and_keeps_pool_pollable() {
        let pool = WebSocketConnectionPool::new(
            make_test_token_handle(),
            "test-client".to_string(),
            make_test_dhan_config(),
            make_test_ws_config(),
            make_instruments(100),
            FeedMode::Ticker,
            None,
        )
        .unwrap();

        // Fresh pool = all Disconnected → first poll enters the all-down cycle.
        assert!(matches!(
            pool.poll_watchdog(),
            WatchdogVerdict::Degrading { .. }
        ));

        // Reset must not panic and must be idempotent.
        pool.reset_watchdog();
        pool.reset_watchdog();

        // After reset the pool is still pollable; an all-down pool re-enters
        // Degrading with a freshly-restarted down-window.
        assert!(matches!(
            pool.poll_watchdog(),
            WatchdogVerdict::Degrading { .. }
        ));
    }

    /// A4: Repeated polls on a dead pool stay in Degrading (until 60s
    /// threshold). We can't simulate 60s of wall-clock in a unit test, so
    /// we just verify the verdict type is stable across multiple polls.
    #[test]
    fn test_pool_poll_watchdog_stable_across_polls() {
        let pool = WebSocketConnectionPool::new(
            make_test_token_handle(),
            "test-client".to_string(),
            make_test_dhan_config(),
            make_test_ws_config(),
            make_instruments(100),
            FeedMode::Ticker,
            None,
        )
        .unwrap();

        for _ in 0..5 {
            let v = pool.poll_watchdog();
            assert!(
                matches!(v, WatchdogVerdict::Degrading { .. }),
                "dead pool must stay in Degrading, got {v:?}"
            );
        }
    }

    /// A4 log coalescing: `a4_log_kind` must map every watchdog verdict to a
    /// stable, distinct kind so the swap-based edge check in `poll_watchdog`
    /// re-logs Degraded/Halt exactly once per down-cycle transition.
    #[test]
    fn test_a4_log_kind_maps_each_verdict() {
        use std::time::Duration;

        assert_eq!(a4_log_kind(&WatchdogVerdict::Healthy), A4_LOG_KIND_NONE);
        assert_eq!(
            a4_log_kind(&WatchdogVerdict::Recovered {
                was_down_for: Duration::from_secs(1),
            }),
            A4_LOG_KIND_NONE,
        );
        assert_eq!(
            a4_log_kind(&WatchdogVerdict::Degrading {
                down_for: Duration::from_secs(1),
            }),
            A4_LOG_KIND_NONE,
        );
        assert_eq!(
            a4_log_kind(&WatchdogVerdict::Degraded {
                down_for: Duration::from_secs(61),
            }),
            A4_LOG_KIND_DEGRADED,
        );
        assert_eq!(
            a4_log_kind(&WatchdogVerdict::Halt {
                down_for: Duration::from_secs(301),
            }),
            A4_LOG_KIND_HALT,
        );

        // The three kinds must be distinct or the `swap(..) != kind` edge
        // check would miss a Degraded -> Halt transition.
        assert_ne!(A4_LOG_KIND_NONE, A4_LOG_KIND_DEGRADED);
        assert_ne!(A4_LOG_KIND_DEGRADED, A4_LOG_KIND_HALT);
        assert_ne!(A4_LOG_KIND_NONE, A4_LOG_KIND_HALT);
    }

    // =======================================================================
    // A5: Pool-level graceful shutdown
    // =======================================================================

    /// A5: `request_graceful_shutdown` sets the flag on every connection in
    /// the pool and reports the total count.
    #[test]
    fn test_pool_graceful_shutdown_signals_all_connections() {
        let pool = WebSocketConnectionPool::new(
            make_test_token_handle(),
            "test-client".to_string(),
            make_test_dhan_config(),
            make_test_ws_config(),
            make_instruments(10),
            FeedMode::Ticker,
            None,
        )
        .unwrap();

        let signalled = pool.request_graceful_shutdown();
        assert_eq!(
            signalled,
            pool.connection_count(),
            "must signal every connection (live or dead)"
        );

        // Every underlying connection must now report shutdown_requested == true.
        for conn in &pool.connections {
            assert!(
                conn.is_shutdown_requested(),
                "conn {} must have shutdown flag set",
                conn.connection_id()
            );
        }
    }

    /// A5: Signalling a pool whose connections are all dead (default state)
    /// must still succeed — graceful shutdown is best-effort.
    #[test]
    fn test_pool_graceful_shutdown_skips_dead_connections_safely() {
        let pool = WebSocketConnectionPool::new(
            make_test_token_handle(),
            "test-client".to_string(),
            make_test_dhan_config(),
            make_test_ws_config(),
            vec![],
            FeedMode::Ticker,
            None,
        )
        .unwrap();

        // All connections are Disconnected at construction time.
        for h in pool.health() {
            assert_eq!(h.state, ConnectionState::Disconnected);
        }

        // Must not panic and must report the correct total.
        let signalled = pool.request_graceful_shutdown();
        assert_eq!(signalled, 5, "pool always signals all 5 slots");
    }

    #[tokio::test]
    async fn test_spawn_all_zero_stagger_is_instant() {
        let _guard = MarketHoursTestGuard::new();
        let pool = WebSocketConnectionPool::new(
            make_test_token_handle(),
            "test-client".to_string(),
            make_test_dhan_config(),
            WebSocketConfig {
                reconnect_max_attempts: 1, // one attempt, then exhaust (0 = retry forever in prod)
                reconnect_initial_delay_ms: 1,
                reconnect_max_delay_ms: 1,
                connection_stagger_ms: 0,
                ..make_test_ws_config()
            },
            make_instruments(10),
            FeedMode::Ticker,
            None,
        )
        .unwrap();

        let start = tokio::time::Instant::now();
        let handles = pool.spawn_all().await;
        let elapsed = start.elapsed();

        assert_eq!(handles.len(), 5);
        assert!(
            elapsed < std::time::Duration::from_millis(50),
            "Zero stagger should be near-instant, got {elapsed:?}",
        );

        drain_handles_or_timeout(&pool, handles).await;
    }

    // -------------------------------------------------------------
    // Wave 2 Item 5.2 (WS-GAP-05) — supervise_pool tests.
    // -------------------------------------------------------------

    #[tokio::test]
    async fn test_supervise_pool_returns_immediately_with_zero_handles() {
        // Empty handle list must return without hanging.
        let started = std::time::Instant::now();
        WebSocketConnectionPool::supervise_pool(Vec::new()).await;
        assert!(started.elapsed() < std::time::Duration::from_millis(500));
    }

    #[tokio::test]
    async fn test_supervise_pool_drains_clean_exit_handle() {
        // A handle that returns Ok(()) must drain to clean termination.
        let handle: tokio::task::JoinHandle<Result<(), WebSocketError>> =
            tokio::spawn(async { Ok(()) });
        WebSocketConnectionPool::supervise_pool(vec![handle]).await;
        // Reaching this assertion proves supervise_pool returned.
    }

    #[tokio::test]
    async fn test_supervise_pool_handles_panicked_task_without_panicking_supervisor() {
        // A panicking handle must NOT propagate the panic to the supervisor.
        let handle: tokio::task::JoinHandle<Result<(), WebSocketError>> =
            tokio::spawn(async { panic!("synthetic panic — Wave 2 Item 5.2 test") });
        WebSocketConnectionPool::supervise_pool(vec![handle]).await;
        // Reaching this assertion proves the supervisor kept its composure.
    }

    // ----------------------------------------------------------------------
    // W2#8 (WS-GAP-05, 2026-07-10) — pool-slot supervised respawn.
    // ----------------------------------------------------------------------

    /// Builds a `WebSocketConnection` that fails fast: unroutable URL +
    /// `reconnect_max_attempts: 1` (the connection.rs
    /// `test_connection_run_zero_max_attempts` pattern) so `run()` exits
    /// with `ReconnectionExhausted` in milliseconds.
    fn make_fast_exit_connection(
        feed_enable_flag: Option<Arc<AtomicBool>>,
    ) -> (
        Arc<WebSocketConnection>,
        mpsc::Receiver<(u64, bytes::Bytes)>,
    ) {
        let (tx, rx) = mpsc::channel(8);
        let mut conn = WebSocketConnection::new(
            1,
            make_test_token_handle(),
            "test-client".to_string(),
            DhanConfig {
                websocket_url: "wss://127.0.0.1:19999".to_string(),
                ..make_test_dhan_config()
            },
            WebSocketConfig {
                reconnect_max_attempts: 1,
                reconnect_initial_delay_ms: 1,
                reconnect_max_delay_ms: 1,
                ..make_test_ws_config()
            },
            vec![],
            FeedMode::Ticker,
            tx,
            None,
        );
        if let Some(flag) = feed_enable_flag {
            conn = conn.with_feed_enable_flag(flag);
        }
        (Arc::new(conn), rx)
    }

    // --- classify_pool_slot_exit: every arm ---

    #[test]
    fn test_classify_terminal_on_shutdown_even_for_clean_exit() {
        // A graceful shutdown / lane teardown must NEVER respawn — the
        // no-resurrect-after-teardown contract.
        assert_eq!(
            classify_pool_slot_exit(&Ok(()), true, false),
            PoolSlotExitVerdict::Terminal
        );
    }

    #[test]
    fn test_classify_terminal_on_shutdown_with_err() {
        assert_eq!(
            classify_pool_slot_exit(
                &Err(WebSocketError::ReconnectionExhausted {
                    connection_id: 1,
                    attempts: 1,
                }),
                true,
                false
            ),
            PoolSlotExitVerdict::Terminal
        );
    }

    #[test]
    fn test_classify_terminal_on_reconnection_exhausted() {
        // A configured finite retry budget stays a hard bound — respawning
        // would defeat it through the back door.
        assert_eq!(
            classify_pool_slot_exit(
                &Err(WebSocketError::ReconnectionExhausted {
                    connection_id: 1,
                    attempts: 3,
                }),
                false,
                false
            ),
            PoolSlotExitVerdict::Terminal
        );
    }

    #[test]
    fn test_classify_terminal_on_non_reconnectable_disconnect() {
        // Dhan 805-class codes deliberately STOP retrying (anti reconnect
        // storm) — the respawn loop must not undo that design.
        assert_eq!(
            classify_pool_slot_exit(
                &Err(WebSocketError::NonReconnectableDisconnect {
                    code: crate::websocket::types::DisconnectCode::from_u16(805),
                }),
                false,
                false
            ),
            PoolSlotExitVerdict::Terminal
        );
    }

    #[test]
    fn test_classify_terminal_on_clean_exit_with_dead_consumer() {
        // WS-GAP-07: the tick consumer is gone — a fresh session would
        // reconnect to Dhan and immediately exit again (connect churn with
        // zero benefit). Terminal.
        assert_eq!(
            classify_pool_slot_exit(&Ok(()), false, true),
            PoolSlotExitVerdict::Terminal
        );
    }

    #[test]
    fn test_classify_respawn_on_unexpected_clean_exit() {
        // Server Close frame / stream-end with no shutdown and a live
        // consumer — THE gap this PR closes.
        assert_eq!(
            classify_pool_slot_exit(&Ok(()), false, false),
            PoolSlotExitVerdict::Respawn
        );
    }

    // --- compute_pool_respawn_backoff_secs: base / doubling / cap ---

    #[test]
    fn test_respawn_backoff_base_is_5s_per_ws_gap_05_contract() {
        assert_eq!(compute_pool_respawn_backoff_secs(0), 5);
    }

    #[test]
    fn test_respawn_backoff_doubles_then_caps_at_300() {
        assert_eq!(compute_pool_respawn_backoff_secs(1), 10);
        assert_eq!(compute_pool_respawn_backoff_secs(2), 20);
        assert_eq!(compute_pool_respawn_backoff_secs(3), 40);
        assert_eq!(compute_pool_respawn_backoff_secs(4), 80);
        assert_eq!(compute_pool_respawn_backoff_secs(5), 160);
        assert_eq!(compute_pool_respawn_backoff_secs(6), 300);
        assert_eq!(compute_pool_respawn_backoff_secs(7), 300);
    }

    #[test]
    fn test_respawn_backoff_never_overflows_at_large_streaks() {
        assert_eq!(compute_pool_respawn_backoff_secs(31), 300);
        assert_eq!(compute_pool_respawn_backoff_secs(32), 300);
        assert_eq!(compute_pool_respawn_backoff_secs(u32::MAX), 300);
    }

    #[test]
    fn test_respawn_backoff_is_monotone_nondecreasing() {
        let mut prev = 0;
        for n in 0..40 {
            let cur = compute_pool_respawn_backoff_secs(n);
            assert!(cur >= prev, "backoff must never decrease (n={n})");
            prev = cur;
        }
    }

    // --- run_supervised_pool_slot: terminal / cancel behaviour ---

    #[tokio::test]
    async fn test_supervised_slot_terminal_err_no_respawn() {
        // A `run()` Err (here: exhausted finite budget against an
        // unroutable URL) must terminate the slot loop — never respawn.
        let (conn, _rx) = make_fast_exit_connection(None);
        let result = run_supervised_pool_slot(conn).await;
        assert!(
            matches!(result, Err(WebSocketError::ReconnectionExhausted { .. })),
            "expected terminal ReconnectionExhausted, got {result:?}"
        );
    }

    #[tokio::test]
    async fn test_supervised_slot_terminal_ok_on_shutdown_never_resurrects() {
        // Feed disabled + graceful shutdown pre-set: `run()` exits the
        // dormant gate with Ok(()) and `shutdown_requested == true` — the
        // slot loop must return, NOT respawn (the graceful half of the
        // no-resurrect-after-teardown contract; the abort half is the
        // cancellation test below).
        let flag = Arc::new(AtomicBool::new(false)); // feed disabled
        let (conn, _rx) = make_fast_exit_connection(Some(flag));
        conn.request_graceful_shutdown();
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            run_supervised_pool_slot(conn),
        )
        .await
        .expect("slot loop must terminate promptly on shutdown, not respawn");
        assert!(result.is_ok(), "shutdown exit is Ok(()), got {result:?}");
    }

    #[tokio::test]
    async fn test_supervised_slot_abort_is_cancelled_no_respawn() {
        // Teardown aborts the slot handle → the WHOLE wrapper (respawn
        // loop included) is cancelled. A cancelled slot can never respawn
        // — this is the abort half of no-resurrect-after-teardown
        // (`teardown_dhan_lane_tasks` step 3b aborts exactly these
        // handles).
        let flag = Arc::new(AtomicBool::new(false)); // feed disabled → parks dormant
        let (conn, _rx) = make_fast_exit_connection(Some(flag));
        let handle = tokio::spawn(run_supervised_pool_slot(conn));
        // Give the task a moment to park in the dormant gate.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        handle.abort();
        let join = handle.await;
        assert!(
            join.err().is_some_and(|e| e.is_cancelled()),
            "aborted slot task must resolve as cancelled"
        );
    }

    #[tokio::test]
    async fn test_spawn_all_handle_count_equals_connection_count() {
        // Replace-not-add baseline: exactly ONE task per pool slot, and the
        // W2#8 respawn is a re-entry INSIDE that task — the handle count is
        // the structural ceiling on live connection tasks (2-WS lock).
        let pool = WebSocketConnectionPool::new(
            make_test_token_handle(),
            "test-client".to_string(),
            DhanConfig {
                websocket_url: "wss://127.0.0.1:19999".to_string(),
                ..make_test_dhan_config()
            },
            make_test_ws_config(),
            vec![],
            FeedMode::Ticker,
            None,
        )
        .unwrap();
        let handles = pool.spawn_all().await;
        assert_eq!(handles.len(), pool.connection_count());
        for h in &handles {
            h.abort();
        }
    }

    #[test]
    fn ratchet_spawn_all_uses_supervised_slot_loop() {
        // Source-scan ratchet: the ONLY per-slot spawn site must route
        // through the supervised respawn loop, and the loop must classify
        // via the pure verdict fn + carry the WS-GAP-05 code + counter.
        // Removing any of these silently regresses the WS-GAP-05 respawn
        // contract back to detect-only.
        let src = include_str!("connection_pool.rs");
        let spawn_all_region = src
            .split("pub async fn spawn_all")
            .nth(1)
            .expect("spawn_all must exist");
        let spawn_all_region = &spawn_all_region[..spawn_all_region
            .find("pub fn take_frame_receiver")
            .expect("take_frame_receiver must follow spawn_all")];
        assert!(
            spawn_all_region.contains("run_supervised_pool_slot(conn).await"),
            "spawn_all's task body must call run_supervised_pool_slot"
        );
        let slot_region = src
            .split("async fn run_supervised_pool_slot")
            .nth(1)
            .expect("run_supervised_pool_slot must exist");
        assert!(
            slot_region.contains("classify_pool_slot_exit("),
            "slot loop must classify exits via classify_pool_slot_exit"
        );
        assert!(
            slot_region.contains("tv_ws_pool_respawn_total"),
            "slot loop respawn arm must increment tv_ws_pool_respawn_total"
        );
        assert!(
            slot_region.contains("WsGap05PoolRespawn.code_str()"),
            "slot loop respawn arm must carry the WS-GAP-05 error code"
        );
        assert!(
            slot_region.contains("compute_pool_respawn_backoff_secs("),
            "slot loop must apply the storm-bounded respawn backoff"
        );
    }
}
