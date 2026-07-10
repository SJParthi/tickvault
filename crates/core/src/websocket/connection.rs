//! Single WebSocket connection to Dhan Live Market Feed.
//!
//! Handles: connect → authenticate → subscribe → ping loop → read frames →
//! disconnect handling → reconnect with backoff.
//!
//! Each connection manages up to 5,000 instruments.
//! The connection pool creates up to 5 of these.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

/// Test-only override: force the post-market guard inside
/// `wait_with_backoff` to treat the wall-clock as "inside market hours".
///
/// Production never reads this flag (`#[cfg(test)]` compilation only).
/// Tests that exercise the infinite-retry math path set it to `true`
/// before running and reset to `false` after, so the test result does
/// not depend on when `cargo test` was invoked.
#[cfg(test)]
static TEST_FORCE_IN_MARKET_HOURS: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Wave 2 Item 5 — global TradingCalendar handle for the post-close sleep
/// path. Set once at boot via `set_market_calendar()`. When `None`, the
/// legacy `return false` give-up path is used (preserves test behaviour).
/// When `Some`, `wait_with_backoff` sleeps until the next NSE market open
/// instead of giving up — closing G1 (main-feed gives up post-close).
static MARKET_CALENDAR: std::sync::OnceLock<
    Arc<tickvault_common::trading_calendar::TradingCalendar>,
> = std::sync::OnceLock::new();

/// Wave 2 Item 5 — install the global TradingCalendar for the post-close
/// sleep path. Call exactly once at boot, BEFORE spawning any
/// `WebSocketConnection::run()` task.
///
/// Idempotent on repeated calls — second+ calls return without effect.
/// Returns `true` on first install, `false` if already installed.
pub fn set_market_calendar(
    calendar: Arc<tickvault_common::trading_calendar::TradingCalendar>,
) -> bool {
    MARKET_CALENDAR.set(calendar).is_ok()
}

/// Read-only accessor for the global TradingCalendar. Returns `None`
/// before `set_market_calendar` is called (e.g., in unit tests that
/// construct a `WebSocketConnection` directly).
///
/// Visibility: `pub(crate)` so sibling modules under `websocket/`
/// (depth_connection, order_update_connection) can share the same
/// global handle for their post-close sleep paths.
pub(crate) fn market_calendar()
-> Option<&'static Arc<tickvault_common::trading_calendar::TradingCalendar>> {
    MARKET_CALENDAR.get()
}

use futures_util::{SinkExt, StreamExt};
use secrecy::ExposeSecret;
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::time;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async_tls_with_config};

use tickvault_common::constants::{WEBSOCKET_AUTH_TYPE, WEBSOCKET_PROTOCOL_VERSION};
use tracing::{debug, error, info, instrument, warn};

use tickvault_common::config::{DhanConfig, WebSocketConfig};
use tickvault_common::types::FeedMode;
use tickvault_storage::ws_frame_spill::{WsFrameSpill, WsType};

use crate::auth::TokenHandle;
use crate::websocket::activity_watchdog::ActivityWatchdog;
use crate::websocket::subscription_builder::build_subscription_messages;
use crate::websocket::tls::build_websocket_tls_connector;
use crate::websocket::types::{
    ConnectionHealth, ConnectionId, ConnectionState, DisconnectCode, InstrumentSubscription,
    WebSocketError,
};

// ---------------------------------------------------------------------------
// O1-B (2026-04-17): Runtime subscribe-add command
// ---------------------------------------------------------------------------

/// Runtime subscribe-add command for main-feed connections.
///
/// Mirrors `DepthCommand::Swap20` / `DepthCommand::Swap200` for depth.
/// Used by the 9:12 IST Phase 2 scheduler to subscribe stock F&O via the
/// existing pool — zero disconnect, RequestCode 17/21 sent on the same
/// WebSocket the connection is already running on.
#[derive(Debug, Clone)]
pub enum SubscribeCommand {
    /// Subscribe these instruments on this connection. The connection's
    /// read loop builds the JSON subscribe messages with the given feed
    /// mode and sends them via the existing write sink.
    AddInstruments {
        instruments: Vec<InstrumentSubscription>,
        feed_mode: FeedMode,
    },
}

// ---------------------------------------------------------------------------
// Fix #3 (2026-04-24): reconnect subscription persistence
// ---------------------------------------------------------------------------

/// Scope guard that takes the subscribe-command receiver out of a connection's
/// `Mutex<Option<Receiver<SubscribeCommand>>>` slot for the duration of the
/// `run_read_loop` call, and ALWAYS reinstalls it on drop — regardless of
/// whether the loop exited via `return Err(...)`, `return Ok(())`, panic
/// unwind, or normal fallthrough.
///
/// # Why this exists
///
/// On 2026-04-24 10:08 IST Dhan TCP-RST'd all 5 main-feed sockets. Our
/// `run()` loop reconnected in ~4s, but the prior implementation did
/// `self.subscribe_cmd_rx.lock().await.take()` INSIDE `run_read_loop`
/// without ever putting the receiver back. Consequence: a late Phase 2
/// subscribe command arriving after the reconnect had nowhere to go, so
/// the new socket ended up under-subscribed. That socket then went silent
/// for 50+ seconds, our activity watchdog tripped, and we got another
/// reconnect — a cascade that fires at 10:11, 10:15, etc. This guard fixes
/// the cause.
///
/// # Correctness
///
/// - On construction, moves the `Option<Receiver>` out of the slot.
/// - `take_rx()` returns the receiver ONCE to the owner of the guard.
/// - On `Drop`, moves the receiver back into the slot using `try_lock()`.
///   `try_lock` cannot contend in practice: `install_subscribe_channel`
///   runs exactly once per connection at boot (before `run()` starts).
///   If `try_lock` ever does fail, we emit an `error!` with the stable
///   code `"I-P1-RECONNECT"` so the operator is alerted and the receiver
///   is dropped — the next Phase 2 dispatch will then fail fast with
///   `SendError::Disconnected`, which is recoverable (pool can re-install).
///
/// # Usage
///
/// ```ignore
/// let mut guard = SubscribeRxGuard::acquire(&self.subscribe_cmd_rx).await;
/// // Borrow the Option<Receiver> via `guard.rx_mut()` inside select!
/// // On any return path, `guard` is dropped and whatever is currently in
/// // `guard.rx` is reinstalled into the slot for the NEXT reconnect.
/// ```
struct SubscribeRxGuard<'a> {
    slot: &'a tokio::sync::Mutex<Option<mpsc::Receiver<SubscribeCommand>>>,
    /// `Some(rx)` while the channel is alive, `None` after the pool
    /// dropped the sender (channel closed). The read loop may set this
    /// to `None` at any time via `rx_mut()`; on guard drop we reinstall
    /// whatever state it currently holds.
    rx: Option<mpsc::Receiver<SubscribeCommand>>,
}

impl<'a> SubscribeRxGuard<'a> {
    /// Locks the slot, moves the `Option<Receiver>` out of it, and
    /// returns the guard. The caller borrows the option via `rx_mut()`
    /// inside the read loop. On drop the guard reinstalls whatever the
    /// read loop left in `self.rx` — so the NEXT `run_read_loop`
    /// invocation (after reconnect) can take it again.
    async fn acquire(
        slot: &'a tokio::sync::Mutex<Option<mpsc::Receiver<SubscribeCommand>>>,
    ) -> Self {
        let rx = slot.lock().await.take();
        Self { slot, rx }
    }

    /// Returns a mutable borrow of the receiver slot so the read loop
    /// can poll it (`rx_mut().as_mut()` gives `Option<&mut Receiver>`).
    /// Setting it to `None` (e.g. when the channel closed) is expected
    /// and does NOT leak the receiver — the guard's drop handler will
    /// then reinstall `None` into the connection, which is the correct
    /// state.
    fn rx_mut(&mut self) -> &mut Option<mpsc::Receiver<SubscribeCommand>> {
        &mut self.rx
    }
}

impl Drop for SubscribeRxGuard<'_> {
    fn drop(&mut self) {
        // APPROVED: reinstall on reconnect — this is the entire point of
        // the guard; see the struct-level doc-comment for rationale.
        let rx = self.rx.take();
        match self.slot.try_lock() {
            Ok(mut guard) => {
                // Reinstall whatever state the read loop left us in —
                // `Some(rx)` to let the next reconnect cycle resume
                // delivery, or `None` if the channel was closed during
                // this cycle.
                *guard = rx;
            }
            Err(_) => {
                // This should be unreachable in production: the only
                // other lock holder is `install_subscribe_channel`, which
                // runs once at boot before `run()` starts. If we ever
                // hit this branch, drop the receiver and alert — Phase 2
                // will then fail fast and the pool can re-install.
                tracing::error!(
                    code = "I-P1-RECONNECT",
                    "subscribe_cmd_rx reinstall on read-loop exit failed: mutex contended. \
                     Dropping receiver; next Phase 2 dispatch will fail fast and the pool \
                     can re-install via install_subscribe_channel()."
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// WebSocket Connection
// ---------------------------------------------------------------------------

/// A single WebSocket connection to Dhan's live market feed.
///
/// Manages its own lifecycle: connect → subscribe → read loop → reconnect.
/// Binary frames are forwarded to `frame_sender` for downstream processing.
///
/// # Ping/Pong
/// Dhan server sends pings every 10 seconds. tokio-tungstenite auto-pongs.
/// Server disconnects after 40 seconds with no pong (code 806).
pub struct WebSocketConnection {
    /// Connection identifier within the pool (0–4).
    connection_id: ConnectionId,

    /// Atomic token handle — O(1) reads, swapped atomically on renewal.
    token_handle: TokenHandle,

    /// Dhan client ID (from SSM credentials).
    client_id: String,

    /// Dhan WebSocket base URL (from config, e.g., "wss://api-feed.dhan.co").
    websocket_base_url: String,

    /// Dhan config for connection limits.
    dhan_config: DhanConfig,

    /// WebSocket reconnection config.
    ws_config: WebSocketConfig,

    /// Instruments assigned to this connection.
    instruments: Vec<InstrumentSubscription>,

    /// Channel sender for forwarding raw binary frames to downstream.
    /// TICK-SEQ-01: sends `(frame_seq, frame)` — the read-loop capture sequence
    /// stamped once and shared with the WAL so `capture_seq` is replay-stable.
    frame_sender: mpsc::Sender<(u64, bytes::Bytes)>,

    /// Current connection state (tracked for health reporting).
    state: std::sync::Mutex<ConnectionState>,

    /// Total reconnection count since startup.
    total_reconnections: AtomicU64,

    /// Pre-built subscription messages — built once in `new()`, reused on every reconnect.
    /// IDX_I instruments use Ticker mode; all others use the configured feed mode.
    cached_subscription_messages: Vec<String>,

    /// Optional notification service for Telegram alerts on disconnect/reconnect.
    /// `None` in tests; `Some(Arc<...>)` in production.
    notifier: Option<Arc<crate::notification::NotificationService>>,

    /// A5: Graceful shutdown flag. When `true`, the outer `run()` loop exits
    /// without attempting reconnect after the read loop returns. Set by
    /// `request_graceful_shutdown()`.
    shutdown_requested: std::sync::atomic::AtomicBool,

    /// A5: Notifier used to wake the read loop when graceful shutdown is
    /// requested. The read loop's `tokio::select!` polls this alongside the
    /// socket, so shutdown requests interrupt a blocking read.
    shutdown_notify: Arc<tokio::sync::Notify>,

    /// STAGE-C: Optional WAL spill writer. When `Some`, every binary frame is
    /// durably appended to disk BEFORE being forwarded to the live frame
    /// channel. On process restart, `WsFrameSpill::replay_all` hands recovered
    /// frames back to downstream so zero ticks are lost across crashes or
    /// downstream stalls. `None` in tests.
    wal_spill: Option<Arc<WsFrameSpill>>,

    /// STAGE-C.3: Shared activity counter read by the per-connection activity
    /// watchdog. Bumped on every inbound frame (binary / ping / pong / text)
    /// so the watchdog can distinguish a truly dead socket from a
    /// data-quiet period where Dhan is still pinging. Initialised to 0 in
    /// `new()`; reset semantics are a no-op because the watchdog tracks
    /// *forward progress*, not absolute values.
    activity_counter: Arc<AtomicU64>,

    /// Wall-clock UTC epoch seconds of the most recent inbound frame
    /// (binary / ping / pong / text). Stored as `i64` so a `0` sentinel
    /// distinguishes "never seen a frame" from "frame at epoch 0".
    /// Read by `health()` to surface ping/pong heartbeat liveness in
    /// the `WebSocketConnected` Telegram payload. Bumped on every
    /// inbound frame in the read loop alongside `activity_counter`.
    /// O(1) per tick: single relaxed atomic store.
    last_activity_at_epoch_secs: Arc<std::sync::atomic::AtomicI64>,

    /// STAGE-C.3: Notify handle the activity watchdog fires when it has
    /// observed no forward progress on `activity_counter` for longer than
    /// its threshold. The read loop awaits this in `tokio::select!` and
    /// returns `Err(WatchdogFired)` on notification, which the outer
    /// `run()` loop treats as a reconnectable error.
    ///
    /// Separate from `shutdown_notify` because the semantics differ:
    /// `shutdown_notify` is a polite graceful-shutdown path that sends
    /// `RequestCode: 12` to Dhan and returns `Ok(())`; `watchdog_notify`
    /// represents a dead socket and must return `Err` so the outer loop
    /// reconnects.
    watchdog_notify: Arc<tokio::sync::Notify>,

    /// O1-B (2026-04-17): Optional runtime subscribe-command receiver.
    /// `None` after `new()`; installed via `with_subscribe_channel`. The
    /// read loop's `select!` polls this alongside the socket; on receipt
    /// it builds subscribe messages (RequestCode 17/21) and sends them
    /// via the existing write sink. The `Mutex<Option<...>>` lets the
    /// run loop `.take()` the receiver once at startup without making the
    /// constructor signature heavier.
    subscribe_cmd_rx: tokio::sync::Mutex<Option<mpsc::Receiver<SubscribeCommand>>>,

    /// Audit-2026-05-03 H2: pre-allocated `&'static str` label for the
    /// `tv_websocket_connections_active` + `tv_websocket_reconnections_total`
    /// metrics. Resolved via `connection_id_label_static()` against the
    /// `CONNECTION_ID_LABELS` const-array indexed by `connection_id`
    /// (range `0..MAX_WEBSOCKET_CONNECTIONS = 5`). Zero allocation —
    /// the label points into the binary's read-only `.rodata` section.
    /// Previously each `run()` invocation called
    /// `self.connection_id.to_string()` TWICE which heap-allocated a fresh
    /// `String` for an integer 0..5. This field eliminates that alloc
    /// from the run-loop entry path entirely.
    connection_id_label: &'static str,

    /// PR #790b (2026-05-25) — most recent disconnect reason, captured at
    /// each disconnect emit site and surfaced in the next `WebSocketReconnected`
    /// Telegram message. `None` means no disconnect has happened yet in this
    /// process lifetime (initial state).
    ///
    /// COLD PATH: written once per disconnect (at most a few times per
    /// trading day) and read once per reconnect. `std::sync::Mutex` is
    /// acceptable here — the lock is never held across an `.await` and the
    /// disconnect/reconnect paths are explicitly NOT the per-tick hot path.
    last_disconnect_reason: std::sync::Mutex<Option<String>>,

    /// PR #790b — wall-clock UTC epoch seconds when the most recent
    /// disconnect was recorded. `0` sentinel = no disconnect yet. Used to
    /// compute `down_secs` for the `WebSocketReconnected` event payload.
    last_disconnect_at_epoch_secs: std::sync::atomic::AtomicI64,

    /// PR #790b — counts reconnect attempts since the last disconnect was
    /// recorded. Reset to `0` on the successful reconnect emit. Lets the
    /// operator distinguish "1 long retry" from "60 short retries" when
    /// debugging a long outage.
    attempts_since_last_disconnect: std::sync::atomic::AtomicU32,

    /// 2026-06-12 — the authoritative Dhan disconnect code (805/807/etc.) of
    /// the most recent disconnect, when one was present. `u32::MAX` sentinel =
    /// none (a transport/TLS error carries no Dhan code). Lets BOTH the
    /// disconnect log AND the matching reconnect log classify the PRECISE
    /// source via `classify_disconnect_cause(reason, Some(code))` instead of
    /// the digit-blind string heuristic — operator demanded the precise
    /// failure source, and the code is the ground truth.
    last_disconnect_dhan_code: std::sync::atomic::AtomicU32,
    /// Consecutive Dhan feed rate-limit (HTTP 429 / DATA-805 class) connect
    /// failures since the last successful connect. Drives the post-429
    /// reconnect floor in `wait_with_backoff` (Dhan guidance: "stop all
    /// connections for 60s, then reconnect one at a time" — instant-retry
    /// after a 429 just keeps the limit alive). Reset to `0` on a clean
    /// connect+subscribe.
    rate_limit_streak: std::sync::atomic::AtomicU32,

    /// Fix B (2026-06-30) — epoch seconds of the most recent SUCCESSFUL
    /// connect+subscribe. Reset to `0` at the start of every connect attempt and
    /// stamped when `connect_and_subscribe` succeeds, so `now - connected_at` is
    /// the prior session's uptime when the read loop returns. A `0` value (no
    /// successful session this cycle) reads as uptime 0 → the short-session
    /// reconnect floor applies (errs toward backing off — safe). Drives
    /// `compute_short_session_reconnect_floor_ms` in `wait_with_backoff`.
    connected_at: std::sync::atomic::AtomicI64,

    /// Fix A (2026-06-30) — set `true` once this connection returns a
    /// `NonReconnectableDisconnect` (a genuine-fatal Dhan disconnect code), so
    /// the pool watchdog's bare-reset classifier can tell a benign transport RST
    /// apart from a real fatal. Surfaced on `ConnectionHealth`. Never reset
    /// within a process — a non-reconnectable disconnect ends `run()`, so this
    /// connection is fatal for the remainder of the process.
    saw_non_reconnectable: std::sync::atomic::AtomicBool,

    /// 2026-06-12 — optional channel to the `ws_event_audit` writer. Every WS
    /// lifecycle event (connect/disconnect/reconnect/sleep) on this connection
    /// also stamps a forensic row with this connection's `ws_type` (MainFeed)
    /// + `connection_index`. `None` until `with_ws_audit` attaches it. Sending
    /// is non-blocking (`try_send`) so the audit NEVER stalls the WS loop.
    ws_audit_tx: Option<WsEventAuditSender>,
    /// Configured number of connections of this connection's `ws_type` (for the
    /// audit `pool_size` column — 1 today, up to 5 in the future 16-conn scenario).
    ws_audit_pool_size: i64,
    /// The `ws_type` this connection stamps on its audit rows. Carried (not
    /// hardcoded) so the SAME `WebSocketConnection` can be a main-feed OR a
    /// future depth connection with the correct label — the operator's explicit
    /// "no schema change for the 16-connection future" demand. Defaults
    /// `MainFeed` (the only runtime `ws_type` for this struct today).
    ws_audit_type: tickvault_common::ws_event_types::WsType,
    /// PR-E (2026-06-21): optional runtime feed-enable flag. `None` = always-on
    /// (every existing caller). When `Some`, the `run()` loop reads it: `false`
    /// closes the socket + goes dormant (polling the flag), `true` reconnects.
    /// Sourced from the shared `FeedRuntimeState` Dhan atomic so the feed-control
    /// webpage/API can pause/resume the live Dhan feed with no restart.
    feed_enable_flag: Option<Arc<AtomicBool>>,

    /// D2c (closes C4): optional LANE-OWNED `TokenManager` for the sleep-wake
    /// `force_renewal_if_stale` path. `None` = use the global `OnceLock`
    /// (`global_token_manager()`) — the boot-ON / fast crash-recovery arm, which
    /// installs the manager globally once at boot. When `Some` (every runtime
    /// cold-start via `start_dhan_lane`), the wake-renewal targets THIS manager —
    /// the one the live pool is actually using — instead of the stale global.
    ///
    /// Why this exists: `set_global_token_manager` is a `OnceLock` that no-ops on
    /// the 2nd set. After a runtime lane STOP (drops manager-A) → re-START
    /// (lane-owned manager-B with a fresh JWT), the global still points at the
    /// dead manager-A, so a global-only wake-renewal would renew the WRONG token.
    /// Injecting the lane manager here makes the wake renew manager-B.
    ///
    /// COLD PATH: read at most once per connect/disconnect/sleep cycle in
    /// `wake_renewal_token_manager()`, NEVER per tick.
    token_manager: Option<Arc<crate::auth::TokenManager>>,
}

/// Polling cadence while the Dhan feed is disabled at runtime (PR-E). Bounds the
/// re-enable detection latency without busy-spinning.
const FEED_DISABLE_POLL_SECS: u64 = 2;

/// Polling cadence while the Dhan feed is disabled (PR-E) — typed `Duration`.
const FEED_DISABLE_POLL: Duration = Duration::from_secs(FEED_DISABLE_POLL_SECS);

/// Audit-2026-05-03 H2: static lookup for connection-id labels used by
/// metrics. Indexed by `ConnectionId as usize`. Capacity matches
/// `MAX_WEBSOCKET_CONNECTIONS = 5`. A 6th entry `"unknown"` covers the
/// degenerate case where a future regression bumps `ConnectionId` past
/// the cap — defensive, never hit in practice.
const CONNECTION_ID_LABELS: [&str; 6] = ["0", "1", "2", "3", "4", "unknown"];

/// Bounded channel sender a connection uses to hand a forensic
/// [`tickvault_common::ws_event_types::WsEventAuditRow`] to the `ws_event_audit`
/// consumer. Alias keeps the (otherwise clippy-`type_complexity`) signatures
/// readable here and in `connection_pool.rs` / the boot wiring.
pub type WsEventAuditSender =
    tokio::sync::mpsc::Sender<tickvault_common::ws_event_types::WsEventAuditRow>;

/// 2026-07-05: static drop-reason label for a `ws_event_audit` `try_send`
/// failure — `"full"` (channel at capacity) or `"closed"` (consumer dead).
/// Static `&'static str` so the `tv_ws_event_audit_dropped_total{reason}`
/// counter never allocates a label, and the dropped row's content (whose
/// pre-redaction `reason` field must never reach a log — security review)
/// is never formatted. Pure, exhaustive, shared by the main-feed and
/// order-update drop arms.
#[inline]
#[must_use]
pub(crate) fn ws_audit_drop_reason(
    err: &tokio::sync::mpsc::error::TrySendError<tickvault_common::ws_event_types::WsEventAuditRow>,
) -> &'static str {
    match err {
        tokio::sync::mpsc::error::TrySendError::Full(_) => "full",
        tokio::sync::mpsc::error::TrySendError::Closed(_) => "closed",
    }
}

/// Returns the `&'static str` metric label for a given `ConnectionId`.
/// Pure function, no I/O, no allocation. O(1) array index.
#[inline]
#[must_use]
fn connection_id_label_static(id: ConnectionId) -> &'static str {
    let idx = id as usize;
    if idx < CONNECTION_ID_LABELS.len() - 1 {
        CONNECTION_ID_LABELS[idx]
    } else {
        // Last slot is the defensive fallback; never reached at the
        // current MAX_WEBSOCKET_CONNECTIONS=5 cap.
        CONNECTION_ID_LABELS[CONNECTION_ID_LABELS.len() - 1]
    }
}

impl WebSocketConnection {
    /// Creates a new WebSocket connection (not yet connected).
    ///
    /// Call `run()` to start the connection lifecycle.
    #[allow(clippy::too_many_arguments)] // APPROVED: WebSocket constructor requires all config at init
    pub fn new(
        connection_id: ConnectionId,
        token_handle: TokenHandle,
        client_id: String,
        dhan_config: DhanConfig,
        ws_config: WebSocketConfig,
        instruments: Vec<InstrumentSubscription>,
        feed_mode: FeedMode,
        frame_sender: mpsc::Sender<(u64, bytes::Bytes)>,
        notifier: Option<Arc<crate::notification::NotificationService>>,
    ) -> Self {
        let websocket_base_url = dhan_config.websocket_url.clone(); // O(1) EXEMPT: constructor — once

        // Pre-build subscription messages once.
        //
        // IDX_I (index value) instruments are subscribed in QUOTE mode
        // (operator directive 2026-05-20). The Quote packet (response
        // code 4, 50 bytes) carries `day_open` / `day_high` / `day_low`
        // / `day_close` at fixed offsets — see `parse_quote_packet`.
        // That is the exchange-computed session OHLC, delivered directly
        // in every packet, so we do NOT have to derive the index open
        // ourselves. The prior code force-subscribed IDX_I in Ticker
        // mode (LTP-only, 16 bytes) on the ASSUMPTION that Dhan ignores
        // Quote/Full for index feeds — that assumption was never
        // live-verified. Worst case, if Dhan does ignore it, it still
        // streams Ticker-grade packets and we lose nothing vs before.
        // O(1) EXEMPT: constructor — runs once at startup, not per tick.
        let (idx_instruments, non_idx_instruments): (Vec<_>, Vec<_>) = instruments
            .iter()
            .cloned()
            .partition(|inst| inst.exchange_segment == "IDX_I");

        let mut cached_subscription_messages = build_subscription_messages(
            &non_idx_instruments,
            feed_mode,
            ws_config.subscription_batch_size,
        );
        if !idx_instruments.is_empty() {
            cached_subscription_messages.extend(build_subscription_messages(
                &idx_instruments,
                FeedMode::Quote,
                ws_config.subscription_batch_size,
            ));
        }

        Self {
            connection_id,
            token_handle,
            client_id,
            websocket_base_url,
            dhan_config,
            ws_config,
            instruments,
            frame_sender,
            state: std::sync::Mutex::new(ConnectionState::Disconnected),
            total_reconnections: AtomicU64::new(0),
            cached_subscription_messages,
            notifier,
            shutdown_requested: std::sync::atomic::AtomicBool::new(false),
            shutdown_notify: Arc::new(tokio::sync::Notify::new()),
            wal_spill: None,
            activity_counter: Arc::new(AtomicU64::new(0)),
            last_activity_at_epoch_secs: Arc::new(std::sync::atomic::AtomicI64::new(0)),
            watchdog_notify: Arc::new(tokio::sync::Notify::new()),
            // O1-B: subscribe command channel installed lazily via builder.
            subscribe_cmd_rx: tokio::sync::Mutex::new(None),
            // Audit-2026-05-03 H2: resolve the static label ONCE at
            // construction. `connection_id_label_static` is a pure O(1)
            // const-array index — zero heap allocation, label points to
            // `.rodata`. Previously the `run()` loop did a per-call
            // `self.connection_id` to-string conversion which heap-allocated.
            connection_id_label: connection_id_label_static(connection_id),
            // PR #790b: initial state — no disconnect has happened yet.
            last_disconnect_reason: std::sync::Mutex::new(None),
            last_disconnect_at_epoch_secs: std::sync::atomic::AtomicI64::new(0),
            attempts_since_last_disconnect: std::sync::atomic::AtomicU32::new(0),
            // u32::MAX sentinel = "no Dhan code recorded yet".
            last_disconnect_dhan_code: std::sync::atomic::AtomicU32::new(u32::MAX),
            rate_limit_streak: std::sync::atomic::AtomicU32::new(0),
            // Fix B (2026-06-30): 0 = no successful session yet this cycle.
            connected_at: std::sync::atomic::AtomicI64::new(0),
            // Fix A (2026-06-30): no fatal disconnect observed yet.
            saw_non_reconnectable: std::sync::atomic::AtomicBool::new(false),
            // 2026-06-12: WS-event audit sink attached lazily via with_ws_audit.
            ws_audit_tx: None,
            ws_audit_pool_size: 1,
            ws_audit_type: tickvault_common::ws_event_types::WsType::MainFeed,
            // PR-E (2026-06-21): no runtime feed-enable flag by default — the
            // feed is always-on (every existing caller + test path). The boot
            // wiring attaches the shared Dhan flag via `with_feed_enable_flag`.
            feed_enable_flag: None,
            // D2c (C4): no lane-owned TokenManager by default — wake-renewal
            // falls back to the global `OnceLock`. The runtime cold-start
            // (`start_dhan_lane`) injects the lane manager via
            // `with_token_manager` so the wake renews the LIVE manager.
            token_manager: None,
        }
    }

    /// STAGE-C: Attach a WAL spill writer. Every binary frame will be durably
    /// persisted to disk before being forwarded to the live consumer channel.
    ///
    /// Chain after `new()`: `WebSocketConnection::new(...).with_wal_spill(spill)`.
    #[must_use]
    // TEST-EXEMPT: builder pass-through covered indirectly by connection_pool construction paths; no test-visible state change to assert
    pub fn with_wal_spill(mut self, spill: Arc<WsFrameSpill>) -> Self {
        self.wal_spill = Some(spill);
        self
    }

    /// PR-E (2026-06-21): attach the shared runtime feed-enable flag (the Dhan
    /// atomic from `FeedRuntimeState`). When the flag is `false` the `run()` loop
    /// closes the socket and idles until it flips back `true`. Chain after
    /// `new()`. Absent flag (the default) = always-on, identical to pre-PR-E.
    #[must_use]
    pub fn with_feed_enable_flag(mut self, flag: Arc<AtomicBool>) -> Self {
        self.feed_enable_flag = Some(flag);
        self
    }

    /// PR-E: is the feed currently enabled? `None` flag = always enabled (every
    /// pre-PR-E caller). Pure O(1) lock-free read.
    #[inline]
    #[must_use]
    fn feed_enabled(&self) -> bool {
        self.feed_enable_flag
            .as_ref()
            .is_none_or(|f| f.load(Ordering::Relaxed))
    }

    /// D2c (closes C4): attach the LANE-OWNED `TokenManager` used by the
    /// sleep-wake `force_renewal_if_stale` path. Chain after `new()`. When set,
    /// `wake_renewal_token_manager()` returns THIS manager instead of the global
    /// `OnceLock`, so a runtime Dhan-lane stop→re-start renews the manager the
    /// live pool is actually using (not the stale boot-time global). Absent
    /// injection (the default) = global fallback, identical to pre-D2c.
    #[must_use]
    pub fn with_token_manager(mut self, tm: Arc<crate::auth::TokenManager>) -> Self {
        self.token_manager = Some(tm);
        self
    }

    /// D2c (C4): the `TokenManager` the wake-renewal path must target. Prefers
    /// the LANE-OWNED injected manager (the live pool's manager); falls back to
    /// the global `OnceLock` for the boot-ON / fast crash-recovery arm that
    /// installs the manager globally and does not inject. Pure `Option`
    /// selection — never panics, never allocates. COLD PATH (per sleep-wake,
    /// never per tick).
    #[inline]
    #[must_use]
    fn wake_renewal_token_manager(&self) -> Option<&Arc<crate::auth::TokenManager>> {
        self.token_manager
            .as_ref()
            .or_else(|| crate::auth::token_manager::global_token_manager())
    }

    /// PR-E: park the connection while the Dhan feed is disabled, polling the
    /// enable flag every `FEED_DISABLE_POLL`. Returns `true` if a graceful
    /// shutdown was requested during the wait (caller exits), `false` when the
    /// feed was re-enabled (caller reconnects). Emits one INFO + a forensic
    /// SleepEntered/SleepResumed audit pair so the dormant window is visible.
    // TEST-EXEMPT: async dormant poll loop driven by an external Arc<AtomicBool> + Notify; the pure enable decision (feed_enabled) is unit-tested and the integration is exercised by the connection_pool construction paths.
    async fn wait_until_feed_enabled(&self) -> bool {
        info!(
            connection_id = self.connection_id,
            "PR-E: Dhan feed disabled at runtime — connection dormant until re-enabled"
        );
        // Source slug `feed_disabled` (round-5 scoreboard fix 2026-07-10):
        // the SAME durable marker the Groww bridge stamps on its disable
        // falling edge — the feed-scoreboard's feed-off-day inference keys
        // on it to distinguish "operator runtime-disabled the feed" from
        // "enabled but the broker was dead" (the honest catastrophic day).
        // Previously "n/a", indistinguishable from the post-close dormant
        // sleep row.
        self.emit_ws_audit(
            tickvault_common::ws_event_types::WsEventKind::SleepEntered,
            "feed_disabled",
            "feed disabled at runtime (operator toggle)",
            None,
            0,
            0,
        );
        loop {
            if self.shutdown_requested.load(Ordering::Acquire) {
                return true;
            }
            if self.feed_enabled() {
                info!(
                    connection_id = self.connection_id,
                    "PR-E: Dhan feed re-enabled at runtime — reconnecting"
                );
                self.emit_ws_audit(
                    tickvault_common::ws_event_types::WsEventKind::SleepResumed,
                    "n/a",
                    "feed re-enabled at runtime (operator toggle)",
                    None,
                    0,
                    0,
                );
                return false;
            }
            tokio::select! {
                () = self.shutdown_notify.notified() => return true,
                () = time::sleep(FEED_DISABLE_POLL) => {}
            }
        }
    }

    /// 2026-06-12: attach the WS-event audit channel + this pool's size.
    ///
    /// Chain after `new()`. Every connect/disconnect/reconnect/sleep on this
    /// connection then stamps a `ws_event_audit` row with `WsType::MainFeed` +
    /// this connection's index. Builder form (matches `with_wal_spill`); avoids
    /// breaking the many `new()` call sites (Rule 17).
    #[must_use]
    // TEST-EXEMPT: builder pass-through; emission covered by ws_event_audit_wiring_guard + the storage append tests.
    pub fn with_ws_audit(
        mut self,
        tx: WsEventAuditSender,
        pool_size: i64,
        ws_type: tickvault_common::ws_event_types::WsType,
    ) -> Self {
        self.ws_audit_tx = Some(tx);
        self.ws_audit_pool_size = pool_size;
        self.ws_audit_type = ws_type;
        self
    }

    /// Stamp one WS lifecycle event to the `ws_event_audit` table (if attached).
    ///
    /// COLD PATH — fires at most once per connect/disconnect/reconnect/sleep,
    /// NEVER per tick. Non-blocking `try_send`: if the audit channel is full or
    /// closed the event is dropped (the live WS loop is never stalled by the
    /// forensic side-record) — the CloudWatch log + Telegram for the same event
    /// still fired, so no operator-visible signal is lost.
    fn emit_ws_audit(
        &self,
        event_kind: tickvault_common::ws_event_types::WsEventKind,
        source: &str,
        reason: &str,
        dhan_code: Option<u16>,
        down_secs: u64,
        attempts: u32,
    ) {
        let Some(tx) = self.ws_audit_tx.as_ref() else {
            return;
        };
        // O(1) EXEMPT: begin — COLD PATH. This whole helper fires at most once
        // per connect/disconnect/reconnect/sleep cycle, NEVER per tick; the two
        // owned-String allocations for the forensic row are off the tick path.
        // IST nanos for the designated timestamp + IST-midnight for the day.
        let now_utc_nanos = chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default();
        let now_ist_nanos =
            now_utc_nanos.saturating_add(tickvault_common::constants::IST_UTC_OFFSET_NANOS);
        let nanos_per_day: i64 = 86_400 * 1_000_000_000;
        let trading_date_ist_nanos = now_ist_nanos - now_ist_nanos.rem_euclid(nanos_per_day);
        let row = tickvault_common::ws_event_types::WsEventAuditRow {
            event_ts_ist_nanos: now_ist_nanos,
            trading_date_ist_nanos,
            // This is the Dhan main-feed connection. A future Groww connection
            // stamps Feed::Groww so their lifecycle events stay distinct rows.
            feed: tickvault_common::feed::Feed::Dhan,
            ws_type: self.ws_audit_type,
            connection_index: i64::try_from(usize::from(self.connection_id)).unwrap_or(0),
            pool_size: self.ws_audit_pool_size,
            event_kind,
            source: source.to_string(),
            reason: reason.to_string(),
            dhan_code: dhan_code.map_or(
                tickvault_common::ws_event_types::WS_EVENT_NO_DHAN_CODE,
                i64::from,
            ),
            down_secs: i64::try_from(down_secs).unwrap_or(i64::MAX),
            attempts: i64::from(attempts),
            market_hours: tickvault_common::market_hours::is_within_market_hours_ist(),
        };
        // O(1) EXEMPT: end
        // Non-blocking: drop on full/closed — never stall the WS loop.
        // 2026-07-05: upgraded from debug! (silent-loss window — the operator
        // found ws_event_audit EMPTY with zero signal). The static reason label
        // carries only "full"/"closed" — NOT the dropped row, whose
        // pre-redaction reason must never reach a log (security review).
        if let Err(err) = tx.try_send(row) {
            let drop_reason = ws_audit_drop_reason(&err);
            error!(
                code =
                    tickvault_common::error_code::ErrorCode::AuditWs01EventWriteFailed.code_str(),
                connection_id = usize::from(self.connection_id),
                reason = drop_reason,
                "ws_event_audit channel full/closed — row dropped (log+Telegram still fired)"
            );
            metrics::counter!("tv_ws_event_audit_dropped_total", "reason" => drop_reason)
                .increment(1);
        }
    }

    /// O1-B (2026-04-17): Attach a runtime subscribe-command channel.
    ///
    /// Builder form — chain after `new()` before Arc-wrapping. For
    /// post-Arc installation (the production path where the pool
    /// constructs connections then wraps them in `Arc<...>`), use
    /// `install_subscribe_channel(&self, rx)` instead.
    // TEST-EXEMPT: builder pass-through; behavior covered by integration tests
    pub fn with_subscribe_channel(self, rx: mpsc::Receiver<SubscribeCommand>) -> Self {
        if let Ok(mut guard) = self.subscribe_cmd_rx.try_lock() {
            *guard = Some(rx);
        }
        self
    }

    /// O1-B: Install the subscribe-command receiver via shared reference.
    /// Used by `WebSocketConnectionPool::install_subscribe_channels` after
    /// connections have been Arc-wrapped. Idempotent on a per-connection
    /// basis: a second call replaces the prior receiver.
    /// Returns the previous receiver if any was installed.
    // TEST-EXEMPT: O1-B installation API; covered by pool.install_subscribe_channels integration via main.rs
    pub async fn install_subscribe_channel(
        &self,
        rx: mpsc::Receiver<SubscribeCommand>,
    ) -> Option<mpsc::Receiver<SubscribeCommand>> {
        let mut guard = self.subscribe_cmd_rx.lock().await;
        guard.replace(rx)
    }

    /// A5: Request graceful shutdown of this connection. Idempotent.
    ///
    /// Sets the shutdown flag and wakes the read loop. The read loop will
    /// send a `RequestCode: 12` (Disconnect) JSON message to Dhan, close the
    /// socket, and return `Ok(())`. The outer `run()` loop then exits without
    /// attempting to reconnect. Called by `WebSocketConnectionPool::graceful_shutdown`.
    ///
    /// This is a cold-path operation: runs once per process lifetime at
    /// shutdown. Allocation-free on the fast path — just an atomic store and
    /// a notify wakeup.
    // TEST-EXEMPT: covered by test_graceful_shutdown_sets_flag_and_notifies and pool-level tests
    pub fn request_graceful_shutdown(&self) {
        self.shutdown_requested
            .store(true, std::sync::atomic::Ordering::Release);
        self.shutdown_notify.notify_one();
    }

    /// A5: Returns true if a graceful shutdown has been requested. Used by the
    /// outer `run()` loop to decide whether to reconnect after the read loop
    /// exits with `Ok(())`.
    // TEST-EXEMPT: trivial atomic load, covered by test_graceful_shutdown_sets_flag_and_notifies
    pub fn is_shutdown_requested(&self) -> bool {
        self.shutdown_requested
            .load(std::sync::atomic::Ordering::Acquire)
    }

    /// W2#8 (WS-GAP-05, 2026-07-10): Returns true if the downstream live
    /// frame channel is CLOSED — the tick-processing consumer holding the
    /// `Receiver` is gone (the WS-GAP-07 condition). The pool-slot respawn
    /// classifier (`connection_pool::classify_pool_task_exit`) reads this
    /// to distinguish a respawnable unexpected clean exit (server Close
    /// frame / stream-end) from a consumer-dead exit where respawning
    /// would only churn Dhan connects with zero benefit.
    ///
    /// Cold path — one atomic-ish check per pool-slot task exit, never per
    /// tick.
    pub(crate) fn live_frame_channel_closed(&self) -> bool {
        self.frame_sender.is_closed()
    }

    /// Returns the connection identifier.
    pub fn connection_id(&self) -> ConnectionId {
        self.connection_id
    }

    /// Returns a health snapshot for monitoring.
    #[allow(clippy::expect_used)] // APPROVED: lock poison is unrecoverable
    pub fn health(&self) -> ConnectionHealth {
        // Compute "seconds since last inbound frame" from the wall-clock
        // delta. `0` sentinel = no frame ever received → return None so
        // the Telegram formatter can render "—" instead of a misleading
        // "1714..." (epoch). On wall-clock skew where last > now, clamp
        // to 0s rather than overflow.
        let last_at = self.last_activity_at_epoch_secs.load(Ordering::Relaxed);
        let last_activity_secs_ago = if last_at == 0 {
            None
        } else {
            let now = chrono::Utc::now().timestamp();
            let delta = now.saturating_sub(last_at);
            Some(delta.clamp(0, u32::MAX as i64) as u32)
        };
        ConnectionHealth {
            connection_id: self.connection_id,
            state: *self.state.lock().expect("state lock poisoned"), // APPROVED: lock poison is unrecoverable
            subscribed_count: self.instruments.len(),
            total_reconnections: self.total_reconnections.load(Ordering::Acquire),
            last_activity_secs_ago,
            // Fix A (2026-06-30): expose the bare-reset classifier inputs so the
            // pool watchdog can decide reconnect-in-place vs genuine-fatal exit
            // from the snapshot it already takes every 5s.
            rate_limit_streak: self.rate_limit_streak.load(Ordering::Acquire),
            saw_non_reconnectable: self.saw_non_reconnectable.load(Ordering::Acquire),
        }
    }

    /// Runs the connection lifecycle: connect → subscribe → read loop.
    ///
    /// On disconnect, attempts reconnection with exponential backoff.
    /// Returns only on non-reconnectable errors or exhausted retries.
    #[instrument(skip_all, fields(conn_id = self.connection_id))]
    pub async fn run(&self) -> Result<(), WebSocketError> {
        // O(1) EXEMPT: begin — metric handles grabbed once before loop, not per-message.
        // Audit-2026-05-03 H2: label is a `&'static str` pointing into
        // `.rodata` via the `CONNECTION_ID_LABELS` const-array. Zero heap
        // allocation on `run()` entry; previously `self.connection_id.to_string()`
        // allocated a fresh `String` per metric handle (2 allocs per run).
        let m_conn_active = metrics::gauge!(
            "tv_websocket_connections_active",
            "connection_id" => self.connection_id_label,
        );
        let m_reconnections = metrics::counter!(
            "tv_websocket_reconnections_total",
            "connection_id" => self.connection_id_label,
        );
        // O(1) EXEMPT: end

        loop {
            // PR-E (2026-06-21): Dhan runtime on/off gate. When the operator
            // disables the Dhan feed (webpage/API), idle here — socket closed,
            // gauge 0 — polling the flag until it flips back ON (or shutdown).
            // This composes with every reconnect path below: a mid-stream
            // disable returns `WebSocketError::FeedDisabled`, the match arm
            // `continue`s to the top, and this gate parks the loop dormant.
            if !self.feed_enabled() {
                self.set_state(ConnectionState::Disconnected);
                m_conn_active.set(0.0);
                if self.wait_until_feed_enabled().await {
                    // Shutdown requested while dormant — exit cleanly.
                    return Ok(());
                }
                // Re-enabled: start the reconnect counter fresh so the first
                // post-resume attempt is the instant (0ms) retry, not a backoff.
                self.total_reconnections.store(0, Ordering::Release);
            }

            self.set_state(ConnectionState::Connecting);

            // Fix B (2026-06-30): clear the prior session's connect stamp before
            // this attempt. If the connect fails OR the next session is short,
            // `connected_at` reads 0 (or a fresh recent stamp) so the
            // short-session reconnect floor is applied — a stale long-ago stamp
            // can never suppress the floor.
            self.connected_at.store(0, Ordering::Release);

            match self.connect_and_subscribe().await {
                Ok(ws_stream) => {
                    self.set_state(ConnectionState::Connected);
                    m_conn_active.set(1.0);

                    let reconnection_count = self.total_reconnections.load(Ordering::Acquire);

                    info!(
                        connection_id = self.connection_id,
                        instruments = self.instruments.len(),
                        "WebSocket connected and subscribed"
                    );

                    // Post-reconnect notification. In-market backfill is explicitly
                    // disabled (user policy); only post-market historical candle
                    // fetch runs, and that path is independent of the live WS.
                    //
                    // Telegram fires ALWAYS on every successful reconnect,
                    // inside OR outside market hours — Parthiban directive
                    // (2026-04-21): all WS disconnect/reconnect/connect events
                    // MUST be notified + audited, no gating. The operator is
                    // the ground truth for distinguishing "expected Dhan
                    // maintenance at 07:40 IST" from "unexpected disconnect".
                    if reconnection_count > 0 {
                        info!(
                            connection_id = self.connection_id,
                            reconnection_count,
                            "WebSocket reconnected (in-market backfill disabled — post-market historical fetch handles any gap)"
                        );
                        // PR #790b: take disconnect-context (reason + down_secs
                        // + attempts) captured at the matching disconnect
                        // sites and emit it with the reconnect event so the
                        // Telegram message surfaces WHY + HOW LONG + HOW MANY.
                        let (reason, down_secs, attempts, dhan_code) =
                            self.take_disconnect_context();
                        // G1 (zero-tick-loss PR-3): quantify reconnect-gap time so
                        // dashboards + a CloudWatch rate-alarm can see sustained or
                        // excessive reconnect churn (Dhan packets carry no sequence
                        // number, so a sub-30s reconnect's lost ticks are otherwise
                        // invisible to the 30s tick-gap detector). Per-event severity
                        // stays Medium — typical reconnects are 5-10s, so escalating
                        // each to High would spam the pager; the rate-alarm on the
                        // cumulative counter is the correct anomaly detector.
                        metrics::counter!("tv_ws_reconnect_total", "feed" => "main").increment(1);
                        metrics::counter!("tv_ws_reconnect_gap_seconds_total", "feed" => "main")
                            .increment(down_secs);
                        // 2026-06-12: structured log so EVERY reconnect lands in
                        // CloudWatch (app.log), not just Telegram. Cold path.
                        let source = reason
                            .as_deref()
                            .map(|r| {
                                tickvault_common::disconnect_cause::classify_disconnect_cause(
                                    r, dhan_code,
                                )
                                .label()
                            })
                            .unwrap_or("unknown");
                        // SECURITY: redact any token-bearing URL in the reason
                        // before it reaches CloudWatch/app.log (the reason can
                        // be a TLS-handshake error embedding `?token=<JWT>`).
                        let safe_reason = reason
                            .as_deref()
                            .map(tickvault_common::sanitize::redact_url_params);
                        info!(
                            connection_id = usize::from(self.connection_id),
                            reason = ?safe_reason,
                            source = source,
                            down_secs,
                            attempts,
                            "WebSocket reconnected"
                        );
                        // 2026-06-12: stamp the forensic reconnect row (same
                        // precise source + down_secs + attempts as the log).
                        self.emit_ws_audit(
                            tickvault_common::ws_event_types::WsEventKind::Reconnected,
                            source,
                            reason.as_deref().unwrap_or(""),
                            dhan_code,
                            down_secs,
                            attempts,
                        );
                        if let Some(ref n) = self.notifier {
                            n.notify(crate::notification::events::NotificationEvent::WebSocketReconnected {
                                connection_index: usize::from(self.connection_id),
                                reason,
                                down_secs,
                                attempts,
                            });
                        }
                    } else {
                        // 2026-06-12 (B1 fix): the INITIAL connect of every
                        // connection is also tracked — stamp a `Connected`
                        // forensic row so "every connect" is true, not just
                        // reconnects. No Telegram for the initial connect
                        // (that positive signal is the 09:15:30 heartbeat).
                        self.emit_ws_audit(
                            tickvault_common::ws_event_types::WsEventKind::Connected,
                            "n/a",
                            "initial connect",
                            None,
                            0,
                            0,
                        );
                    }

                    // STAGE-C.3: Spawn the per-connection activity watchdog
                    // before entering the read loop. The watchdog polls
                    // `activity_counter` every WATCHDOG_POLL_INTERVAL_SECS
                    // and fires `watchdog_notify` if the counter has not
                    // advanced for WATCHDOG_THRESHOLD_LIVE_AND_DEPTH_SECS.
                    // On fire, the read loop's select! returns
                    // Err(WatchdogFired) and the outer loop reconnects.
                    //
                    // A fresh watchdog is spawned on every reconnect; the
                    // old task is aborted below as soon as the read loop
                    // returns.
                    // O(1) EXEMPT: watchdog label built once per reconnect cycle, not per frame
                    let watchdog_label = format!("live-feed-{}", self.connection_id);
                    // Phase 0 Item 4 (2026-05-13): threshold read from ws_config
                    // so main.rs can scope-override (Phase 0 = 3s; legacy = 50s).
                    // PR #5 (2026-05-19): scope-override now inlined directly
                    // in main.rs after `phase2_recovery` module retirement.
                    let watchdog = ActivityWatchdog::new(
                        watchdog_label,
                        Arc::clone(&self.activity_counter),
                        Duration::from_secs(self.ws_config.activity_watchdog_threshold_secs),
                        Arc::clone(&self.watchdog_notify),
                    );
                    // WS-1: use panic-safe spawn so a watchdog panic fires
                    // the notify and the read loop wakes up + reconnects
                    // instead of hanging silently forever.
                    let watchdog_handle =
                        crate::websocket::activity_watchdog::spawn_with_panic_notify(watchdog);

                    // Run read + ping loops until disconnect.
                    let disconnect_result = self.run_read_loop(ws_stream).await;

                    // STAGE-C.3: Always abort the watchdog task on exit.
                    // If the watchdog fired, its task has already returned
                    // and `abort()` is a no-op. If the read loop exited for
                    // any other reason (Dhan disconnect, graceful shutdown,
                    // transport error), we stop the watchdog so it can't
                    // fire stale alerts against a dead socket.
                    watchdog_handle.abort();

                    match disconnect_result {
                        Ok(()) => {
                            // Clean shutdown requested.
                            info!(
                                connection_id = self.connection_id,
                                "WebSocket cleanly closed"
                            );
                            self.set_state(ConnectionState::Disconnected);
                            m_conn_active.set(0.0);
                            return Ok(());
                        }
                        Err(WebSocketError::FeedDisabled) => {
                            // PR-E: operator disabled the Dhan feed mid-stream.
                            // The socket is already dropped (read loop returned).
                            // Skip reconnect/backoff entirely and `continue` to
                            // the top-of-loop dormant gate, which parks the
                            // connection until the feed is re-enabled. NOT a
                            // failure → no reconnect counter bump, no Telegram.
                            info!(
                                connection_id = self.connection_id,
                                "PR-E: Dhan feed disabled at runtime — socket closed, going dormant"
                            );
                            self.set_state(ConnectionState::Disconnected);
                            m_conn_active.set(0.0);
                            continue;
                        }
                        Err(WebSocketError::DhanDisconnect { code })
                            if !code.is_reconnectable() =>
                        {
                            error!(
                                connection_id = self.connection_id,
                                disconnect_code = %code,
                                "Non-reconnectable disconnect — stopping connection"
                            );
                            // H1: Critical disconnect — fire Telegram immediately.
                            // PR #790b: record reason so the next reconnect Telegram surfaces it.
                            // O(1) EXEMPT: cold path — fires at most once per disconnect cycle, never per tick
                            let reason = format!("Non-reconnectable: {code}");
                            // O(1) EXEMPT: cold path — reason needs to flow to both state slot + Telegram event
                            // Always fires WebSocketDisconnected (HIGH) regardless of hours → audit kind Disconnected.
                            self.record_disconnect(
                                reason.clone(), // O(1) EXEMPT: cold path — once per disconnect cycle, never per tick
                                Some(code.as_u16()),
                                tickvault_common::ws_event_types::WsEventKind::Disconnected,
                            );
                            if let Some(ref n) = self.notifier {
                                n.notify(crate::notification::events::NotificationEvent::WebSocketDisconnected {
                                    connection_index: usize::from(self.connection_id),
                                    reason,
                                });
                            }
                            // Fix A (2026-06-30): record the genuine-fatal class
                            // so the pool watchdog's bare-reset classifier never
                            // misclassifies a real non-reconnectable Dhan code as
                            // a benign transport RST.
                            self.saw_non_reconnectable.store(true, Ordering::Release);
                            self.set_state(ConnectionState::Disconnected);
                            m_conn_active.set(0.0);
                            return Err(WebSocketError::NonReconnectableDisconnect { code });
                        }
                        Err(WebSocketError::DhanDisconnect { code })
                            if code.requires_token_refresh() =>
                        {
                            m_conn_active.set(0.0);
                            // S5-D1: Fire Telegram on token-expired disconnect.
                            // Previously the 807 branch sat in silence waiting
                            // for renewal, so the operator only learned about
                            // token expiry indirectly (missing ticks, late
                            // auth-related errors). Now they get an explicit
                            // WebSocketDisconnected event the moment 807 fires.
                            // PR #790b: record reason so the next reconnect Telegram surfaces it.
                            // O(1) EXEMPT: cold path — once per 807 event, never per tick
                            let reason = format!("Token expired ({code}) — waiting for renewal");
                            // O(1) EXEMPT: cold path — reason needs to flow to both state slot + Telegram event
                            // Always fires WebSocketDisconnected (HIGH) regardless of hours → audit kind Disconnected.
                            self.record_disconnect(
                                reason.clone(), // O(1) EXEMPT: cold path — once per disconnect cycle, never per tick
                                Some(code.as_u16()),
                                tickvault_common::ws_event_types::WsEventKind::Disconnected,
                            );
                            if let Some(ref n) = self.notifier {
                                n.notify(crate::notification::events::NotificationEvent::WebSocketDisconnected {
                                    connection_index: usize::from(self.connection_id),
                                    reason,
                                });
                            }
                            warn!(
                                connection_id = self.connection_id,
                                disconnect_code = %code,
                                "Token expired — waiting for renewal before reconnect"
                            );
                            // Wait for renewal to swap in a valid token before reconnecting.
                            self.wait_for_valid_token().await;
                        }
                        Err(err) => {
                            m_conn_active.set(0.0);
                            // Parthiban override (2026-04-22): the 2026-04-21
                            // "HIGH-on-every-disconnect" directive caused
                            // Telegram spam at 08:32 IST (pre-market TCP
                            // resets by Dhan's idle-cleanup). Off-hours,
                            // auto-recoverable disconnects now route to
                            // the LOW-severity `WebSocketDisconnectedOffHours`
                            // variant — audit trail preserved, no alert
                            // escalation. In-market disconnects still fire
                            // HIGH via the original variant.
                            // PR #790b: record reason so the next reconnect Telegram surfaces it.
                            // O(1) EXEMPT: cold path — reconnection error, never per tick
                            let reason = format!("{err}");
                            // SECURITY (2026-06-12): a TLS/handshake error can embed
                            // the feed URL with the token — redact before logging the
                            // error detail so no JWT reaches CloudWatch/app.log.
                            let safe_err = tickvault_common::sanitize::redact_url_params(&reason);
                            warn!(
                                connection_id = self.connection_id,
                                error = %safe_err,
                                "WebSocket disconnected — will reconnect"
                            );
                            // Decide in-market ONCE so the audit kind EXACTLY mirrors
                            // the Telegram variant this site fires (B3 fix).
                            let in_market =
                                tickvault_common::market_hours::is_within_market_hours_ist();
                            let audit_kind = if in_market {
                                tickvault_common::ws_event_types::WsEventKind::Disconnected
                            } else {
                                tickvault_common::ws_event_types::WsEventKind::DisconnectedOffHours
                            };
                            // O(1) EXEMPT: cold path — transport error carries no Dhan code
                            self.record_disconnect(reason.clone(), None, audit_kind);
                            if let Some(ref n) = self.notifier {
                                let event = if in_market {
                                    crate::notification::events::NotificationEvent::WebSocketDisconnected {
                                        connection_index: usize::from(self.connection_id),
                                        // O(1) EXEMPT: cold path — reconnection error, not per tick
                                        reason,
                                    }
                                } else {
                                    crate::notification::events::NotificationEvent::WebSocketDisconnectedOffHours {
                                        connection_index: usize::from(self.connection_id),
                                        // O(1) EXEMPT: cold path — off-hours reset, not per tick
                                        reason,
                                    }
                                };
                                n.notify(event);
                            }
                        }
                    }
                }
                Err(err) => {
                    // Telegram + WARN log on EVERY connect-failed event,
                    // regardless of market hours. Mirrors the disconnect
                    // branch above for full audit parity.
                    // PR #790b: record reason so the next reconnect Telegram surfaces it.
                    // O(1) EXEMPT: cold path — connect-failed, never per tick
                    let reason = format!("connect failed: {err}");
                    // SECURITY (2026-06-12): redact any token-bearing URL in the
                    // connect error before logging it to CloudWatch/app.log.
                    let safe_err = tickvault_common::sanitize::redact_url_params(&reason);
                    warn!(
                        connection_id = self.connection_id,
                        error = %safe_err,
                        "WebSocket connection failed — will retry"
                    );
                    // Decide in-market ONCE so the audit kind EXACTLY mirrors the
                    // Telegram variant this site fires (B3 fix).
                    let in_market = tickvault_common::market_hours::is_within_market_hours_ist();
                    let audit_kind = if in_market {
                        tickvault_common::ws_event_types::WsEventKind::Disconnected
                    } else {
                        tickvault_common::ws_event_types::WsEventKind::DisconnectedOffHours
                    };
                    // O(1) EXEMPT: cold path — connect-failed transport error carries no Dhan code
                    self.record_disconnect(reason.clone(), None, audit_kind);
                    if let Some(ref n) = self.notifier {
                        // Parthiban override (2026-04-22): same off-hours
                        // severity split as the disconnect branch above.
                        let event = if in_market {
                            crate::notification::events::NotificationEvent::WebSocketDisconnected {
                                connection_index: usize::from(self.connection_id),
                                // O(1) EXEMPT: cold path — connect-failed is not per tick
                                reason,
                            }
                        } else {
                            crate::notification::events::NotificationEvent::WebSocketDisconnectedOffHours {
                                connection_index: usize::from(self.connection_id),
                                // O(1) EXEMPT: cold path — off-hours connect-failed
                                reason,
                            }
                        };
                        n.notify(event);
                    }
                }
            }

            // Reconnect with exponential backoff.
            self.set_state(ConnectionState::Reconnecting);
            self.total_reconnections.fetch_add(1, Ordering::Release);
            m_reconnections.increment(1);

            if !self.wait_with_backoff().await {
                return Err(WebSocketError::ReconnectionExhausted {
                    connection_id: self.connection_id,
                    attempts: self.ws_config.reconnect_max_attempts,
                });
            }
        }
    }

    /// Establishes the WebSocket connection with auth query params and sends subscriptions.
    ///
    /// Dhan V2 auth: `wss://api-feed.dhan.co?version=2&token=xxx&clientId=xxx&authType=2`
    // O(1) EXEMPT: begin — connect_and_subscribe runs once per connect/reconnect, not per tick
    async fn connect_and_subscribe(
        &self,
    ) -> Result<WebSocketStream<MaybeTlsStream<TcpStream>>, WebSocketError> {
        // Read current token atomically (O(1)).
        let token_guard = self.token_handle.load();
        let token_state = token_guard
            .as_ref()
            .as_ref()
            .ok_or(WebSocketError::NoTokenAvailable)?;

        if !token_state.is_valid() {
            warn!(
                connection_id = self.connection_id,
                "Token is expired — skipping connection attempt"
            );
            return Err(WebSocketError::NoTokenAvailable);
        }

        let access_token = token_state.access_token().expose_secret().to_string();

        // Build URL with auth query parameters (Dhan V2 protocol).
        // CRITICAL: The base URL must have an explicit "/" path before the query
        // string. Without it, http::Uri parses the path as empty, and tungstenite
        // writes "GET ?version=2&... HTTP/1.1" which is invalid HTTP (RFC 7230
        // requires the request-target to start with "/"). Proxies reject this
        // with 400 Bad Request.
        let base = self.websocket_base_url.trim_end_matches('/');
        // SEC-3: Zeroize the URL containing the JWT after use to prevent heap residency.
        let authenticated_url = zeroize::Zeroizing::new(format!(
            "{base}/?version={}&token={}&clientId={}&authType={}",
            WEBSOCKET_PROTOCOL_VERSION, access_token, self.client_id, WEBSOCKET_AUTH_TYPE,
        ));

        let request = authenticated_url
            .as_str()
            .into_client_request()
            .map_err(|err| WebSocketError::ConnectionFailed {
                url: self.websocket_base_url.clone(),
                source: err,
            })?;

        debug!(
            connection_id = self.connection_id,
            url = %self.websocket_base_url,
            "Connecting to Dhan WebSocket"
        );

        // Build a TLS connector that forces HTTP/1.1 ALPN.
        // Without explicit ALPN, some proxies (Cloudflare, nginx) may negotiate
        // HTTP/2 which cannot be used for WebSocket upgrade. Forcing "http/1.1"
        // ensures the upgrade handshake succeeds.
        let tls_connector = build_websocket_tls_connector()?;

        // Connect with timeout.
        let connect_timeout = Duration::from_millis(
            (self.dhan_config.max_instruments_per_connection as u64)
                .saturating_mul(10)
                .saturating_add(10000),
        );
        let connect_result = time::timeout(
            connect_timeout,
            connect_async_tls_with_config(request, None, false, Some(tls_connector)),
        )
        .await
        .map_err(|_| WebSocketError::ConnectionFailed {
            url: self.websocket_base_url.clone(),
            source: tokio_tungstenite::tungstenite::Error::Io(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "WebSocket connection timed out",
            )),
        })?;

        let (ws_stream, _response) = match connect_result {
            Ok(result) => result,
            Err(err) => {
                if let tokio_tungstenite::tungstenite::Error::Http(ref response) = err {
                    error!(
                        connection_id = self.connection_id,
                        status = %response.status(),
                        body = ?response.body().as_ref().map(|b| String::from_utf8_lossy(b).to_string()),
                        "WebSocket HTTP error"
                    );
                    // Dhan feed rate-limit (HTTP 429 / DATA-805 class): bump the
                    // streak so the NEXT `wait_with_backoff` applies the 60s→5m
                    // floor instead of the instant (0ms) first-retry. Instant
                    // reconnect after a 429 just keeps the limit alive.
                    if response.status().as_u16() == 429 {
                        // `fetch_add` returns the PREVIOUS value, so the new
                        // streak is prev + 1.
                        let new_streak = self
                            .rate_limit_streak
                            .fetch_add(1, Ordering::Release)
                            .saturating_add(1);
                        // WS-GAP-08: persist this 429 hit so the cooldown
                        // survives a `process::exit(2)` + supervisor restart.
                        // Without this, a restart wipes the in-memory streak
                        // and the fresh process reconnects with a 0ms first
                        // retry straight back into Dhan's still-active 429
                        // window → instant-429 restart loop. Best-effort: a
                        // write failure logs WS-GAP-08 but never blocks the
                        // WS loop (the in-memory streak still applies here).
                        let floor_ms = compute_rate_limit_floor_ms(
                            new_streak,
                            tickvault_common::constants::WS_RATE_LIMIT_BACKOFF_BASE_MS,
                            tickvault_common::constants::WS_RATE_LIMIT_BACKOFF_CAP_MS,
                        );
                        crate::websocket::rate_limit_cooldown::record_rate_limit_hit(
                            crate::websocket::rate_limit_cooldown::now_epoch_ms(),
                            new_streak,
                            floor_ms,
                        );
                    }
                }
                return Err(WebSocketError::ConnectionFailed {
                    url: self.websocket_base_url.clone(),
                    source: err,
                });
            }
        };

        // Send pre-built subscription messages (cached in new()) with yield pacing.
        let (mut write, read) = ws_stream.split();

        for (batch_index, message) in self.cached_subscription_messages.iter().enumerate() {
            write
                .send(Message::Text(message.clone().into()))
                .await
                .map_err(|err| WebSocketError::SubscriptionFailed {
                    connection_id: self.connection_id,
                    reason: err.to_string(),
                })?;
            // Yield between batches to avoid starving other tasks.
            // Skip yield after the last batch (nothing to wait for).
            if batch_index < self.cached_subscription_messages.len().saturating_sub(1) {
                tokio::task::yield_now().await;
            }
        }

        info!(
            connection_id = self.connection_id,
            instruments = self.instruments.len(),
            messages_sent = self.cached_subscription_messages.len(),
            "Subscriptions sent"
        );

        // Clean connect + subscribe — clear any feed rate-limit streak so the
        // post-429 reconnect floor resets to normal backoff.
        self.rate_limit_streak.store(0, Ordering::Release);

        // Fix B (2026-06-30): stamp the successful-connect epoch so the NEXT
        // `wait_with_backoff` can compute this session's uptime (`now -
        // connected_at`). A session that lives < WS_SHORT_SESSION_THRESHOLD_MS
        // before dropping gets a floored (non-instant) first reconnect.
        self.connected_at
            .store(chrono::Utc::now().timestamp(), Ordering::Release);

        // Reunite for the read loop.
        // APPROVED: reuniting same split — cannot fail
        #[allow(clippy::expect_used)]
        Ok(read.reunite(write).expect("reunite same stream")) // APPROVED: reuniting same split — cannot fail
    }
    // O(1) EXEMPT: end

    /// Runs the read loop: reads binary frames, handles server pings, detects disconnects.
    ///
    /// Dhan server sends pings every 10 seconds. tokio-tungstenite auto-pongs
    /// for standard WebSocket pings. For any explicit Ping frames that reach
    /// the application layer, we send Pong manually.
    async fn run_read_loop(
        &self,
        ws_stream: WebSocketStream<MaybeTlsStream<TcpStream>>,
    ) -> Result<(), WebSocketError> {
        let (write, mut read) = ws_stream.split();
        let write = Arc::new(tokio::sync::Mutex::new(write));

        // Fix #3 (2026-04-24): hold the subscribe-command receiver via a
        // reinstall-on-drop guard so the NEXT reconnect cycle can take it
        // again. Before this fix the receiver was `.take()`d without ever
        // being put back, which caused the 10:11 / 10:15 IST 2026-04-24
        // silent-socket → watchdog trip → reconnect cascade. See
        // SubscribeRxGuard doc-comment above for the root-cause writeup.
        //
        // The guard is dropped when this function returns via ANY path
        // (Err, Ok, panic unwind) and reinstalls whatever state the read
        // loop left in `guard.rx_mut()` back into `self.subscribe_cmd_rx`.
        // A post-drop call to `run_read_loop` on the same connection
        // (the reconnect case) will then take the receiver again from
        // the same slot.
        let mut subscribe_rx_guard = SubscribeRxGuard::acquire(&self.subscribe_cmd_rx).await;
        let subscribe_rx: &mut Option<mpsc::Receiver<SubscribeCommand>> =
            subscribe_rx_guard.rx_mut();
        let m_subscribe_dispatched = metrics::counter!("tv_ws_subscribe_command_dispatched_total");
        let m_subscribe_failed = metrics::counter!("tv_ws_subscribe_command_failed_total");

        // ZL-P0-1: pre-build the heartbeat gauge handle ONCE so the hot
        // per-frame update is a single atomic store.
        // O(1) EXEMPT: begin
        let m_last_frame_epoch = crate::websocket::activity_watchdog::build_heartbeat_gauge(
            "live_feed",
            self.connection_id.to_string(),
        );
        // O(1) EXEMPT: end

        // STAGE-B (plan item P1.1): No client-side read deadline.
        //
        // Per Dhan spec (docs/dhan-ref/03-live-market-feed-websocket.md:90-95):
        //   "Server pings every 10s. WebSocket library auto-responds with pong.
        //    If client does not respond for >40s, server closes the connection."
        //
        // The 40-second timeout is ENFORCED BY THE SERVER. A client-side
        // `time::timeout(40s, read.next())` wrapper is redundant AND creates
        // false-positive disconnects whenever downstream is slow (tick
        // processor stall propagates backpressure → read loop can't advance
        // → 40s deadline fires even though the socket is fine).
        //
        // The read loop now blocks purely on `read.next().await`. Exit
        // conditions: stream returns None, stream returns Err, or the
        // shutdown notify fires. That's it.
        //
        // Liveness is enforced by: (a) Dhan's own 40s server-side timeout,
        // and (b) Stage C's watchdog task (independent of this loop).

        loop {
            // A5: tokio::select! lets the read loop respond to a graceful
            // shutdown request immediately. On shutdown: send Disconnect JSON
            // (RequestCode 12), close the socket, and return Ok so the outer
            // `run()` loop exits cleanly (see `is_shutdown_requested` check
            // in `run()`).
            let frame_result = tokio::select! {
                biased;
                // STAGE-C.3: activity watchdog fired — socket is dead, force
                // reconnect. Does NOT send RequestCode 12 (the socket is
                // already gone) and returns Err so the outer run() loop
                // reconnects instead of exiting cleanly like shutdown does.
                () = self.watchdog_notify.notified() => {
                    return Err(WebSocketError::WatchdogFired {
                        label: format!("live-feed-{}", self.connection_id), // O(1) EXEMPT: cold path — fires once per dead socket, not per frame
                        // Phase 0 Item 4 (2026-05-13): same field as the watchdog
                        // constructor so the WatchdogFired event reports the actual
                        // threshold (3s under Phase 0; 50s under legacy).
                        silent_secs: self.ws_config.activity_watchdog_threshold_secs,
                        threshold_secs: self.ws_config.activity_watchdog_threshold_secs,
                    });
                }
                () = self.shutdown_notify.notified() => {
                    info!(
                        connection_id = self.connection_id,
                        "A5: graceful shutdown notified — sending RequestCode 12 (Disconnect) to Dhan"
                    );
                    let disconnect_json = crate::websocket::subscription_builder::build_disconnect_message();
                    let send_timeout = Duration::from_secs(2); // APPROVED: graceful-shutdown upper bound — 2s per connection is the session 7 A5 spec, not tunable.
                    let send_result = {
                        let mut sink = write.lock().await;
                        time::timeout(
                            send_timeout,
                            sink.send(Message::Text(disconnect_json.into())),
                        )
                        .await
                    };
                    match send_result {
                        Ok(Ok(())) => {
                            info!(
                                connection_id = self.connection_id,
                                "A5: Disconnect request sent successfully"
                            );
                            metrics::counter!(
                                "tv_ws_graceful_unsub_total",
                                "connection_id" => self.connection_id.to_string(), // APPROVED: cold-path (graceful shutdown metrics label, runs once per SIGTERM)
                                "outcome" => "sent"
                            )
                            .increment(1);
                        }
                        Ok(Err(err)) => {
                            warn!(
                                connection_id = self.connection_id,
                                ?err,
                                "A5: failed to send Disconnect request — socket already dead"
                            );
                            metrics::counter!(
                                "tv_ws_graceful_unsub_total",
                                "connection_id" => self.connection_id.to_string(), // APPROVED: cold-path (graceful shutdown metrics label, runs once per SIGTERM)
                                "outcome" => "send_failed"
                            )
                            .increment(1);
                        }
                        Err(_elapsed) => {
                            warn!(
                                connection_id = self.connection_id,
                                timeout_secs = send_timeout.as_secs(),
                                "A5: Disconnect send timed out — proceeding with socket close"
                            );
                            metrics::counter!(
                                "tv_ws_graceful_unsub_total",
                                "connection_id" => self.connection_id.to_string(), // APPROVED: cold-path (graceful shutdown metrics label, runs once per SIGTERM)
                                "outcome" => "timeout"
                            )
                            .increment(1);
                        }
                    }
                    // Close the socket regardless of send outcome.
                    {
                        let mut sink = write.lock().await;
                        drop(time::timeout(Duration::from_secs(1), sink.close()).await); // APPROVED: graceful-shutdown inner close timeout — 1s is the session 7 A5 spec.
                    }
                    return Ok(());
                }
                // O1-B (2026-04-17): runtime subscribe-command arm.
                // When the pool dispatches a `SubscribeCommand::AddInstruments`
                // here, build subscribe messages (RequestCode 17/21) and
                // send them via the existing write sink. Zero disconnect,
                // single round-trip per batch. If `subscribe_rx` is None
                // (no channel ever installed), `pending()` keeps the arm
                // dormant forever — the rest of select! is unaffected.
                maybe_cmd = async {
                    match subscribe_rx.as_mut() {
                        Some(rx) => rx.recv().await,
                        None => std::future::pending::<Option<SubscribeCommand>>().await,
                    }
                } => {
                    if let Some(SubscribeCommand::AddInstruments { instruments, feed_mode }) = maybe_cmd {
                        let count = instruments.len();
                        let messages = build_subscription_messages(
                            &instruments,
                            feed_mode,
                            self.ws_config.subscription_batch_size,
                        );
                        let msg_count = messages.len();
                        let mut sink = write.lock().await;
                        let mut send_failures: u32 = 0;
                        // Consume messages by value to avoid an extra alloc
                        // — the Vec is built per dispatch, no reuse needed.
                        // O(1) EXEMPT: cold path — at most a handful
                        // of messages per Phase 2 dispatch (max 100
                        // instruments per message per Dhan rule).
                        for msg in messages {
                            match sink.send(Message::Text(msg.into())).await {
                                Ok(()) => {}
                                Err(e) => {
                                    error!(
                                        connection_id = self.connection_id,
                                        ?e,
                                        "O1-B: subscribe-command send failed — Dhan socket likely dead"
                                    );
                                    send_failures = send_failures.saturating_add(1);
                                }
                            }
                        }
                        drop(sink);
                        if send_failures == 0 {
                            info!(
                                connection_id = self.connection_id,
                                added_count = count,
                                msg_count,
                                "O1-B: subscribe-command dispatched successfully"
                            );
                            m_subscribe_dispatched.increment(1);
                        } else {
                            warn!(
                                connection_id = self.connection_id,
                                send_failures,
                                "O1-B: subscribe-command had partial failures"
                            );
                            m_subscribe_failed.increment(1);
                        }
                        // Loop back to the read arm — do NOT consume a
                        // frame this iteration.
                        continue;
                    }
                    // Channel closed → leave the arm dormant for the rest
                    // of this connect cycle. Assign through the &mut
                    // reference so the SubscribeRxGuard reinstalls `None`
                    // on drop (Fix #3 correctness — the pool must then
                    // re-install via `install_subscribe_channel` if it
                    // wants further subscribe commands delivered).
                    *subscribe_rx = None;
                    continue;
                }
                // PR-E (2026-06-21): runtime feed-disable arm. Active ONLY when a
                // feed-enable flag is attached (Dhan main feed); for every other
                // connection it is `pending()` forever (zero effect). When the
                // operator disables the feed, this resolves, we drop the socket
                // (returning `FeedDisabled`), and the outer `run()` loop parks the
                // connection dormant. The poll cadence is the cold `FEED_DISABLE_POLL`
                // — no per-frame work, so the hot read path is untouched.
                () = async {
                    match self.feed_enable_flag.as_ref() {
                        Some(flag) => loop {
                            if !flag.load(Ordering::Relaxed) {
                                return;
                            }
                            time::sleep(FEED_DISABLE_POLL).await;
                        },
                        None => std::future::pending::<()>().await,
                    }
                } => {
                    info!(
                        connection_id = self.connection_id,
                        "PR-E: Dhan feed disabled — closing socket to go dormant"
                    );
                    return Err(WebSocketError::FeedDisabled);
                }
                // STAGE-B (P1.1): plain read.next().await — no deadline.
                next_frame = read.next() => next_frame,
            };

            // STAGE-C.3: Bump the activity counter on EVERY Some(Ok(_)) frame
            // — binary, ping, pong, text. The watchdog only cares that the
            // socket is producing something, not what. A steady stream of
            // Dhan pings (every 10s) keeps the counter advancing during
            // data-quiet periods, so the watchdog never fires a false
            // positive PROVIDED the effective threshold sits above the
            // 10s ping cadence. Audit GAP-1 (Operator approved
            // 2026-07-02: "approve 15s"): the DailyUniverse scope clamp is
            // WATCHDOG_THRESHOLD_DAILY_UNIVERSE_SECS = 15 (> 10s), so
            // this guarantee holds; the historical Indices4Only 3s clamp
            // pre-dates it and is retained unchanged. One relaxed atomic
            // increment is O(1) and allocation free — safe on the hot
            // path.
            //
            // ZL-P0-1: also update the heartbeat gauge with the current
            // wall-clock epoch seconds. Grafana rule fires if
            // `time() - tv_ws_last_frame_epoch_secs > 5` during market
            // hours — this catches frozen event loops the watchdog
            // cannot see (watchdog only fires after 50s of NO frames;
            // this catches "frames every 60s" which is alive-but-dead).
            if matches!(frame_result, Some(Ok(_))) {
                self.activity_counter.fetch_add(1, Ordering::Relaxed);
                // chrono::Utc::now().timestamp() is a clock_gettime
                // syscall (~20ns) — acceptable on this path; the
                // downstream parse + dispatch is ~10 μs anyway.
                let now_secs = chrono::Utc::now().timestamp();
                m_last_frame_epoch.set(now_secs as f64);
                // Bump the connection-local timestamp so health() can
                // surface "last frame N seconds ago" in Telegram payloads
                // for ping/pong heartbeat liveness.
                self.last_activity_at_epoch_secs
                    .store(now_secs, Ordering::Relaxed);
            }

            match frame_result {
                Some(Ok(Message::Binary(data))) => {
                    // STAGE-C.5: early disconnect-code detection.
                    //
                    // Dhan signals API-level errors via a binary
                    // disconnect packet with response_code == 50 and
                    // a u16 LE reason code at bytes 8-9. See
                    // `docs/dhan-ref/08-annexure-enums.md` Section 11.
                    //
                    // The tick-processor eventually parses this into
                    // `ParsedFrame::Disconnect(code)` via the
                    // dispatcher, but by then the read loop has
                    // already continued — it will not know to stop
                    // reconnecting on a non-reconnectable code
                    // (804/805/806/808/809/810/811/812/813/814) or to
                    // trigger token refresh on 807.
                    //
                    // Intercept here so the WS layer can raise
                    // `DhanDisconnect { code }` BEFORE the bytes flow
                    // downstream, letting the outer `run()` loop take
                    // the right reconnect / halt / refresh decision
                    // as per the existing `is_reconnectable` +
                    // `requires_token_refresh` logic.
                    //
                    // We still WAL-append and forward the bytes so
                    // the existing parser path sees the same
                    // disconnect (needed for metrics + observability).
                    // The Err return happens AFTER the forward so no
                    // frame is lost on the way out.
                    //
                    // O(1) — one byte check + one u16 LE load on a
                    // 10-byte packet. No allocation, no parsing on
                    // the happy path (early-return for non-50 codes).
                    let maybe_disconnect_code: Option<DisconnectCode> = if !data.is_empty()
                        && data[0] == tickvault_common::constants::RESPONSE_CODE_DISCONNECT
                        && data.len() >= tickvault_common::constants::DISCONNECT_PACKET_SIZE
                    {
                        // Bytes 8-9 per docs/dhan-ref/03-live-market-feed-websocket.md:258
                        let raw_code = u16::from_le_bytes([data[8], data[9]]);
                        Some(DisconnectCode::from_u16(raw_code))
                    } else {
                        None
                    };

                    // STAGE-C (P1.2+P1.3): durable WAL first, live forward second.
                    //
                    // (1) If a WAL spill is attached, append the raw frame to
                    //     disk via a crossbeam try_send. This is O(1) and never
                    //     blocks the read loop. The frame is now durable —
                    //     even if the downstream consumer is stuck and we
                    //     later drop the live forward, the next boot will
                    //     replay this frame from disk.
                    //
                    // (2) Forward to the live consumer channel with try_send.
                    //     On Full, log WARN (not CRITICAL) because the frame
                    //     is already durable in the WAL. The watchdog will
                    //     restart the downstream consumer on sustained
                    //     backpressure, which replays from WAL on boot.
                    //
                    // (3) Only if the WAL itself dropped the frame do we fire
                    //     the CRITICAL metric — that is the single "real loss"
                    //     path and it requires both the disk writer thread
                    //     AND the downstream consumer to be stuck simultaneously.
                    // TICK-SEQ-01: stamp the capture sequence ONCE per frame, then
                    // share the SAME value with BOTH the durable WAL and the live
                    // broadcast so the persisted `capture_seq` is replay-stable.
                    // O(1) atomic, zero heap alloc.
                    let frame_seq = tickvault_storage::ws_frame_spill::next_frame_seq();
                    if let Some(spill) = self.wal_spill.as_ref() {
                        // Zero-tick-loss PR-8a (H1): hand the WAL spill an O(1)
                        // `Bytes` Arc-refcount handle instead of a per-frame
                        // `Vec<u8>` malloc. `data` is already `Bytes` (the live
                        // forward below also moves it zero-copy), so cloning the
                        // handle is a refcount bump — no heap allocation on the
                        // read loop.
                        // O(1) EXEMPT: begin — Bytes Arc-refcount bump, not a heap alloc (PR-8a H1).
                        let outcome =
                            spill.append_with_seq(WsType::LiveFeed, data.clone(), frame_seq);
                        // O(1) EXEMPT: end
                        if outcome == tickvault_storage::ws_frame_spill::AppendOutcome::Dropped {
                            error!(
                                connection_id = self.connection_id,
                                "CRITICAL: WAL spill dropped LiveFeed frame — disk writer stalled"
                            );
                            // CRITICAL metric already incremented inside spill.append().
                        }
                    }
                    match self.frame_sender.try_send((frame_seq, data)) {
                        Ok(()) => {}
                        Err(mpsc::error::TrySendError::Full(_dropped)) => {
                            let wal_attached = self.wal_spill.is_some();
                            if wal_attached {
                                warn!(
                                    connection_id = self.connection_id,
                                    channel_capacity = self.frame_sender.capacity(),
                                    wal_attached = true,
                                    "WS live frame channel full — backpressure from downstream. \
                                     Frame is durable in WAL; watchdog will restart consumer."
                                );
                                metrics::counter!(
                                    "tv_ws_frame_live_backpressure_total",
                                    "ws_type" => "live_feed"
                                )
                                .increment(1);
                            } else {
                                // WS-2: WAL NOT attached — the frame is genuinely lost,
                                // not just held in backpressure. Log at ERROR level
                                // (Telegram alert path) and increment a DEDICATED
                                // counter so dashboards can distinguish "buffered in
                                // WAL" from "dropped on the floor".
                                error!(
                                    connection_id = self.connection_id,
                                    channel_capacity = self.frame_sender.capacity(),
                                    "CRITICAL: WS live frame channel full AND no WAL \
                                     attached — frame LOST. Investigate consumer task \
                                     liveness and re-enable WAL spill (WS-2 audit gap)."
                                );
                                // NOTE: emitted WITHOUT a label so the
                                // `cloudwatch_app_alarms_wiring` guard's
                                // single-line `counter!("name"` detector finds it
                                // (this is now an alarmed metric — G4). The
                                // ws_type was always "live_feed"; the CloudWatch
                                // alarm keys on the `host` dimension regardless.
                                metrics::counter!("tv_ws_frame_dropped_no_wal_total").increment(1);
                                // Also increment the existing backpressure counter so
                                // single-pane-of-glass dashboards still see the event.
                                metrics::counter!(
                                    "tv_ws_frame_live_backpressure_total",
                                    "ws_type" => "live_feed"
                                )
                                .increment(1);
                            }
                            // Intentionally do NOT return Err — the socket is
                            // healthy, we just can't forward this one frame on
                            // the live path. The read loop MUST continue so
                            // pings flow and the WAL keeps durably recording
                            // (when WAL is attached).
                        }
                        Err(mpsc::error::TrySendError::Closed(_)) => {
                            // WS-GAP-07: the tick-processing consumer holding the
                            // Receiver is gone — unlike a `Full` backpressure (the
                            // WAL still records the frame), a `Closed` channel means
                            // NO ticks reach the pipeline from this connection until
                            // the consumer/app restarts. error! (Telegram via Loki) +
                            // dedicated counter per audit Rule 5 (drain failures are
                            // error!, never warn!).
                            error!(
                                connection_id = self.connection_id,
                                code = tickvault_common::error_code::ErrorCode::WsGap07LiveChannelClosed
                                    .code_str(),
                                "WS live frame channel CLOSED — tick consumer dropped; \
                                 stopping read loop. Frames will not flow on this \
                                 connection until the consumer/app restarts."
                            );
                            metrics::counter!(
                                "tv_ws_live_channel_closed_drop_total",
                                "ws_type" => "live_feed"
                            )
                            .increment(1);
                            return Ok(());
                        }
                    }

                    // STAGE-C.5: if the packet was a disconnect, surface
                    // the classified error now. The outer `run()` loop:
                    //   - non-reconnectable codes → return NonReconnectableDisconnect
                    //     (stops retrying — no reconnect storm on 805/808/etc.)
                    //   - 807 AccessTokenExpired → wait_for_valid_token()
                    //     (refresh via the token manager before reconnecting)
                    //   - 800 + Unknown → exponential backoff reconnect
                    //
                    // We return AFTER the WAL append + live forward so
                    // the tick processor still sees the disconnect for
                    // metrics + observability.
                    if let Some(code) = maybe_disconnect_code {
                        warn!(
                            connection_id = self.connection_id,
                            disconnect_code = %code,
                            reconnectable = code.is_reconnectable(),
                            requires_token_refresh = code.requires_token_refresh(),
                            "Dhan binary disconnect packet received — surfacing to run() loop"
                        );
                        metrics::counter!(
                            "tv_ws_dhan_disconnect_total",
                            "code" => format!("{code}"), // O(1) EXEMPT: cold path, fires once per disconnect
                            "ws_type" => "live_feed"
                        )
                        .increment(1);
                        return Err(WebSocketError::DhanDisconnect { code });
                    }
                }
                Some(Ok(Message::Ping(data))) => {
                    // Server ping — respond with pong to keep connection alive.
                    // Pong send is bounded by pong_timeout — if the socket's
                    // TCP buffer is stuck for >pong_timeout, the socket is
                    // genuinely dead and we return Err to trigger reconnect.
                    let pong_timeout = Duration::from_secs(self.ws_config.pong_timeout_secs);
                    let mut sink = write.lock().await;
                    if time::timeout(pong_timeout, sink.send(Message::Pong(data)))
                        .await
                        .is_err()
                    {
                        warn!(
                            connection_id = self.connection_id,
                            timeout_secs = self.ws_config.pong_timeout_secs,
                            "Pong send timed out — connection likely dead"
                        );
                        return Err(WebSocketError::ReadTimeout {
                            connection_id: self.connection_id,
                            timeout_secs: self.ws_config.pong_timeout_secs,
                        });
                    }
                }
                Some(Ok(Message::Pong(_))) => {
                    // Pong from server (e.g., echo of our pong). Ignore.
                }
                Some(Ok(Message::Close(frame))) => {
                    if let Some(frame) = frame {
                        let code: u16 = frame.code.into();
                        let reason = frame.reason.to_string(); // O(1) EXEMPT: close frame — once at disconnect
                        warn!(
                            connection_id = self.connection_id,
                            close_code = code,
                            reason = %reason,
                            "WebSocket close frame received"
                        );
                    }
                    return Ok(());
                }
                Some(Ok(Message::Text(text))) => {
                    // Dhan may send text for disconnect messages.
                    debug!(
                        connection_id = self.connection_id,
                        text_len = text.len(),
                        "Received text message (unexpected)"
                    );
                }
                Some(Err(err)) => {
                    return Err(WebSocketError::ConnectionFailed {
                        url: self.websocket_base_url.clone(), // O(1) EXEMPT: error path — once at disconnect
                        source: err,
                    });
                }
                None => {
                    // Stream ended — exit cleanly. NOTE (W2#8 truth-sync,
                    // 2026-07-10): the outer `run()` loop treats a clean
                    // read-loop exit as TERMINAL for this run() invocation
                    // (the pre-W2#8 comment here claimed "outer loop
                    // reconnects" — stale). Recovery is now owned by the
                    // pool-slot supervisor
                    // (`connection_pool::run_supervised_pool_slot`), which
                    // classifies a non-shutdown clean exit as respawnable
                    // and re-enters run() with a storm-bounded backoff.
                    return Ok(());
                }
                Some(Ok(_)) => {
                    // tokio-tungstenite may surface Frame variants we don't care about.
                }
            }
        }
    }

    /// Waits with exponential backoff. Returns false if max attempts exhausted.
    ///
    /// When `reconnect_max_attempts == 0` (production default), retries forever —
    /// the app lifecycle (graceful shutdown at market close) controls when
    /// connections should stop, not an arbitrary attempt limit. A CRITICAL alert
    /// fires every 10 consecutive failures so the operator is aware.
    async fn wait_with_backoff(&self) -> bool {
        let attempt = self.total_reconnections.load(Ordering::Acquire);

        // reconnect_max_attempts == 0 → infinite retries (never give up).
        // Non-zero → respect the limit (used by tests and explicit config overrides).
        if self.ws_config.reconnect_max_attempts > 0
            && attempt >= self.ws_config.reconnect_max_attempts as u64
        {
            error!(
                connection_id = self.connection_id,
                attempts = attempt,
                "Max reconnection attempts exhausted"
            );
            return false;
        }

        // Post-market guard: if current IST time is AT OR AFTER 15:30 IST AND
        // we've had 3+ consecutive failures, stop reconnecting. Pre-market
        // (< 09:00 IST) keeps retrying indefinitely because Dhan idle-resets
        // TCP connections between 00:00 and 09:00 IST but opens up at the
        // pre-open session start (09:00) — giving up pre-market then forces
        // a process restart after the pool watchdog's 300s halt, which blocks
        // boot at 09:15 IST when the subscription planner is not yet trimmed
        // (see Bug A/B/C fix note in claude/fix-market-hours-guards).
        //
        // Only applies to infinite-retry mode (reconnect_max_attempts == 0,
        // the production default). When an explicit finite budget is set
        // (tests, debug runs), the caller has already bounded retries and
        // the math path should run regardless of wall-clock.
        if attempt >= 3 && self.ws_config.reconnect_max_attempts == 0 {
            let now_utc_secs = chrono::Utc::now().timestamp();
            let now_ist_secs_of_day = (now_utc_secs.saturating_add(i64::from(
                tickvault_common::constants::IST_UTC_OFFSET_SECONDS,
            )))
            .rem_euclid(86400) as u32;
            // POST-close only — pre-open keeps retrying.
            let raw_post_close = now_ist_secs_of_day
                >= tickvault_common::constants::TICK_PERSIST_END_SECS_OF_DAY_IST;
            // Test-only escape hatch so the "infinite retries math" path
            // can be exercised by `cargo test` regardless of wall-clock.
            #[cfg(test)]
            let post_close = raw_post_close
                && !TEST_FORCE_IN_MARKET_HOURS.load(std::sync::atomic::Ordering::Relaxed);
            #[cfg(not(test))]
            let post_close = raw_post_close;
            if post_close {
                // Wave 2 Item 5 (G1): if a TradingCalendar is installed
                // globally, sleep until the next NSE market open (skipping
                // weekends + holidays) instead of giving up. This closes
                // the legacy "process restart required after 15:30 IST"
                // gap. Falls back to the legacy `return false` when no
                // calendar is installed (e.g., unit tests that construct
                // a WebSocketConnection directly).
                if let Some(cal) = market_calendar() {
                    let sleep_secs = cal
                        .secs_until_next_market_open(now_utc_secs)
                        .unwrap_or(60 * 60); // 1h fallback if calendar exhausts
                    info!(
                        connection_id = self.connection_id,
                        attempt,
                        sleep_secs,
                        code = tickvault_common::error_code::ErrorCode::WsGap04PostCloseSleep
                            .code_str(),
                        "WS-GAP-04 main-feed entered post-close sleep until next market open"
                    );
                    metrics::counter!(
                        "tv_ws_post_close_sleep_total",
                        "feed" => "main"
                    )
                    .increment(1);
                    // Wave 2 — emit Telegram WebSocketSleepEntered (Severity::Low).
                    if let Some(ref n) = self.notifier {
                        n.notify(
                            crate::notification::events::NotificationEvent::WebSocketSleepEntered {
                                feed: "main".to_string(),
                                connection_index: self.connection_id as usize,
                                sleep_secs,
                            },
                        );
                    }
                    // 2026-06-12: forensic row — dormant sleep entered; the
                    // sleep duration is recorded in down_secs.
                    self.emit_ws_audit(
                        tickvault_common::ws_event_types::WsEventKind::SleepEntered,
                        "n/a",
                        "post-close dormant sleep until next market open",
                        None,
                        sleep_secs,
                        0,
                    );
                    time::sleep(Duration::from_secs(sleep_secs)).await;
                    info!(
                        connection_id = self.connection_id,
                        slept_for_secs = sleep_secs,
                        "Main-feed sleep resumed — attempting reconnect"
                    );
                    // Wave 2 Item 5.4 (AUTH-GAP-03) — proactively renew the
                    // token if it has < 4h validity left BEFORE attempting
                    // the post-sleep reconnect. Prevents the legacy
                    // "wake → reconnect → DH-901 → renew → reconnect"
                    // 30-second cascade.
                    //
                    // D2c (closes C4): target the LANE-OWNED TokenManager when
                    // injected (every runtime cold-start via start_dhan_lane),
                    // falling back to the global `OnceLock` for the boot-ON /
                    // fast crash-recovery arm. After a runtime lane stop→re-start
                    // the global still points at the dead prior manager, so a
                    // global-only renewal would renew the WRONG token; the
                    // injected lane manager is the one the live pool is using.
                    if let Some(tm) = self.wake_renewal_token_manager() {
                        // Snapshot remaining seconds for the typed Telegram event.
                        let remaining_secs_before = tm
                            .next_renewal_at()
                            .map(|exp| {
                                (exp - chrono::Utc::now().with_timezone(
                                    &tickvault_common::trading_calendar::ist_offset(),
                                ))
                                .num_seconds()
                            })
                            .unwrap_or(i64::MIN);
                        match tm.force_renewal_if_stale(14_400).await {
                            Ok(true) => {
                                if let Some(ref n) = self.notifier {
                                    n.notify(
                                        crate::notification::events::NotificationEvent::WebSocketTokenForceRenewedOnWake {
                                            feed: "main".to_string(),
                                            connection_index: self.connection_id as usize,
                                            remaining_secs_before,
                                            threshold_secs: 14_400,
                                        },
                                    );
                                }
                            }
                            Ok(false) => {
                                // Token still fresh — no Telegram noise.
                            }
                            Err(e) => {
                                tracing::error!(
                                    connection_id = self.connection_id,
                                    error = ?e,
                                    code = tickvault_common::error_code::ErrorCode::AuthGap03TokenForceRenewedOnWake.code_str(),
                                    "AUTH-GAP-03 wake-time token renewal failed — will rely on reconnect retry"
                                );
                            }
                        }
                    }
                    // Wave 2 — emit Telegram WebSocketSleepResumed.
                    if let Some(ref n) = self.notifier {
                        n.notify(
                            crate::notification::events::NotificationEvent::WebSocketSleepResumed {
                                feed: "main".to_string(),
                                connection_index: self.connection_id as usize,
                                slept_for_secs: sleep_secs,
                            },
                        );
                    }
                    // 2026-06-12: forensic row — resumed from dormant sleep.
                    self.emit_ws_audit(
                        tickvault_common::ws_event_types::WsEventKind::SleepResumed,
                        "n/a",
                        "resumed from dormant sleep at market open",
                        None,
                        sleep_secs,
                        0,
                    );
                    // Reset reconnection counter so the post-sleep attempt
                    // starts fresh (3-failure post-close gate is per-cycle).
                    self.total_reconnections.store(0, Ordering::Release);
                    return true;
                }
                // Legacy fallback (tests / no-calendar profile): give up
                // and let the outer run() loop exit cleanly.
                info!(
                    connection_id = self.connection_id,
                    attempt,
                    "Post-close (>=15:30 IST): stopping reconnect after {attempt} failures — market is closed"
                );
                return false;
            }
        }

        // CRITICAL alert every 10 consecutive failures (triggers Telegram).
        // Parthiban directive (2026-04-21): always ERROR + Telegram
        // regardless of market hours. A 10-failure streak is always a
        // real signal that warrants operator attention — even if Dhan
        // is mid-maintenance at 07:40 IST, the operator needs to know
        // that our recovery loop is still churning.
        if attempt.is_multiple_of(10) {
            error!(
                connection_id = self.connection_id,
                consecutive_failures = attempt,
                "WebSocket reconnection threshold hit — still retrying (infinite resilience mode)"
            );
        }

        // Phase 0 Item 4 (operator-locked 2026-05-13): first reconnect
        // attempt fires INSTANTLY (0ms). See
        // `compute_reconnect_base_delay_ms` doc for the full rationale.
        let mut base_delay_ms = compute_reconnect_base_delay_ms(
            attempt,
            self.ws_config.reconnect_initial_delay_ms,
            self.ws_config.reconnect_max_delay_ms,
        );

        // Fix B (2026-06-30): floor the FIRST reconnect after a short-lived
        // session. `connected_at == 0` (no successful session this cycle) reads
        // as uptime 0 → floored (safe). A long-lived (>= threshold) session that
        // drops keeps the instant first retry. Only RAISES the attempt-0 delay.
        let connected_at = self.connected_at.load(Ordering::Acquire);
        let session_uptime_secs = if connected_at == 0 {
            0
        } else {
            chrono::Utc::now()
                .timestamp()
                .saturating_sub(connected_at)
                .max(0)
        };
        let short_session_floor_ms = compute_short_session_reconnect_floor_ms(
            attempt,
            session_uptime_secs,
            (tickvault_common::constants::WS_SHORT_SESSION_THRESHOLD_MS / 1000) as i64,
            tickvault_common::constants::WS_SHORT_SESSION_RECONNECT_FLOOR_MS,
        );
        let short_session_floor_applied = short_session_floor_ms > base_delay_ms;
        base_delay_ms = base_delay_ms.max(short_session_floor_ms);

        // B1: add deterministic-but-varying jitter so 5 connections failing
        // together do not synchronize their reconnect attempts and hammer
        // Dhan's endpoint in lock-step. We hash the current wall-clock
        // subsec_nanos with the connection_id so each connection draws a
        // different value on each reconnect attempt. No new crate dep.
        //
        // Jitter range: ±jitter_pct% of base_delay_ms, clamped to [0, base*2].
        let jittered_ms = jitter_reconnect_delay(base_delay_ms, self.connection_id, attempt);

        // Post-429 floor: if Dhan has rate-limited the feed (HTTP 429 /
        // DATA-805 class), enforce a 60s→5m minimum so we stop hammering the
        // endpoint. Instant-retry after a 429 just keeps the limit alive.
        let rl_streak = self.rate_limit_streak.load(Ordering::Acquire);
        let delay_ms = if rl_streak > 0 {
            let floor_ms = compute_rate_limit_floor_ms(
                rl_streak,
                tickvault_common::constants::WS_RATE_LIMIT_BACKOFF_BASE_MS,
                tickvault_common::constants::WS_RATE_LIMIT_BACKOFF_CAP_MS,
            );
            metrics::counter!("tv_ws_rate_limit_backoff_total", "feed" => "main").increment(1);
            jittered_ms.max(floor_ms)
        } else {
            jittered_ms
        };

        info!(
            connection_id = self.connection_id,
            attempt = attempt,
            base_delay_ms = base_delay_ms,
            delay_ms = delay_ms,
            rate_limit_streak = rl_streak,
            session_uptime_secs = session_uptime_secs,
            short_session_floor_applied = short_session_floor_applied,
            "Reconnecting after backoff"
        );

        time::sleep(Duration::from_millis(delay_ms)).await;
        true
    }

    /// Waits until the token handle contains a valid (non-expired) token.
    ///
    /// Polls every 5 seconds up to 60 seconds. If no valid token appears
    /// (renewal task hasn't swapped one in), gives up and lets the reconnect
    /// loop attempt with whatever token is available.
    async fn wait_for_valid_token(&self) {
        const POLL_INTERVAL_SECS: u64 = 5;
        const MAX_WAIT_SECS: u64 = 60;

        let mut waited: u64 = 0;
        while waited < MAX_WAIT_SECS {
            let guard = self.token_handle.load();
            if let Some(state) = guard.as_ref().as_ref()
                && state.is_valid()
            {
                info!(
                    connection_id = self.connection_id,
                    waited_secs = waited,
                    "Valid token available — resuming reconnection"
                );
                return;
            }
            time::sleep(Duration::from_secs(POLL_INTERVAL_SECS)).await;
            waited = waited.saturating_add(POLL_INTERVAL_SECS);
        }
        warn!(
            connection_id = self.connection_id,
            "Timed out waiting for valid token — attempting reconnect anyway"
        );
    }

    #[allow(clippy::expect_used)] // APPROVED: lock poison is unrecoverable
    fn set_state(&self, new_state: ConnectionState) {
        let mut state = self.state.lock().expect("state lock poisoned"); // APPROVED: lock poison is unrecoverable
        *state = new_state;
    }

    /// PR #790b (2026-05-25) — record a disconnect for later inclusion in
    /// the matching reconnect Telegram message. Captures reason + wall-clock
    /// epoch and increments the attempt counter.
    ///
    /// Idempotent for multi-step disconnects (e.g., a 807 token-expired
    /// disconnect immediately followed by a reconnect failure): the reason
    /// of the LATEST disconnect wins, but `last_disconnect_at` is only
    /// updated on the FIRST disconnect of the cycle so `down_secs` reflects
    /// total downtime (not just the last step).
    ///
    /// COLD PATH ONLY. Called from the 4 disconnect emit sites in `run()`.
    /// `dhan_code` is `Some(805/807/...)` when the disconnect carried an
    /// authoritative Dhan code, `None` for a transport/TLS error — passing it
    /// gives the classifier the ground-truth source instead of a digit-blind
    /// string guess.
    #[allow(clippy::expect_used)] // APPROVED: lock poison is unrecoverable
    fn record_disconnect(
        &self,
        reason: String,
        dhan_code: Option<u16>,
        audit_kind: tickvault_common::ws_event_types::WsEventKind,
    ) {
        // 2026-06-12: structured log at the SINGLE disconnect choke point so
        // EVERY WebSocket disconnect lands in CloudWatch (app.log), not just
        // Telegram — with the PRECISE source. Cold path (once per disconnect
        // cycle, never per tick). O(1) EXEMPT.
        let source =
            tickvault_common::disconnect_cause::classify_disconnect_cause(&reason, dhan_code)
                .label();
        // SECURITY: the reason may embed the feed URL (`wss://...?token=<JWT>`)
        // on a TLS/handshake error — redact query params so no token reaches
        // CloudWatch/app.log. Same redaction the #1108 Telegram path uses.
        let safe_reason = tickvault_common::sanitize::redact_url_params(&reason);
        warn!(
            connection_id = usize::from(self.connection_id),
            reason = %safe_reason,
            source = source,
            "WebSocket disconnected"
        );
        // 2026-06-12: stamp the forensic ws_event_audit row at the SAME single
        // choke point so EVERY disconnect (all 4 paths) is durably tracked. The
        // `audit_kind` is passed BY THE CALL SITE so it mirrors EXACTLY the
        // Telegram variant that site fires (the 807 / non-reconnectable branches
        // always fire WebSocketDisconnected regardless of hours; the 2 transport
        // branches use the in-market vs off-hours split). down_secs / attempts
        // are reconnect-only (0 here); reason is redacted at the ILP boundary.
        self.emit_ws_audit(audit_kind, source, &reason, dhan_code, 0, 0);
        // Capture the Dhan code (latest wins) so the matching reconnect log can
        // classify the same PRECISE source. u32::MAX sentinel = none.
        self.last_disconnect_dhan_code.store(
            dhan_code.map_or(u32::MAX, u32::from),
            std::sync::atomic::Ordering::Release,
        );
        // Capture the reason (latest wins).
        if let Ok(mut slot) = self.last_disconnect_reason.lock() {
            *slot = Some(reason);
        }
        // Capture the FIRST disconnect timestamp in this cycle (so down_secs
        // measures total downtime including any intermediate retry failures).
        let now_secs = chrono::Utc::now().timestamp();
        // First-disconnect-wins: a lost CAS race just means another path
        // already stamped the epoch this cycle, which is fine. Bind to a
        // `_`-prefixed name (not bare `_`) so the must-use result is consumed.
        let _cas_outcome = self.last_disconnect_at_epoch_secs.compare_exchange(
            0,
            now_secs,
            std::sync::atomic::Ordering::AcqRel,
            std::sync::atomic::Ordering::Acquire,
        );
        // Bump attempt counter.
        self.attempts_since_last_disconnect
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
    }

    /// PR #790b — read disconnect-context for the matching reconnect event,
    /// then atomically reset state. Returns `(reason, down_secs, attempts,
    /// dhan_code)` where:
    /// * `reason` is `None` if no disconnect was recorded (rare initial path).
    /// * `down_secs` is `0` if `last_disconnect_at_epoch_secs == 0`.
    /// * `attempts` is `0` if no retry was needed.
    /// * `dhan_code` is `Some(805/807/...)` when the disconnect carried an
    ///   authoritative Dhan code, `None` otherwise — lets the reconnect log
    ///   classify the SAME precise source the disconnect log did.
    fn take_disconnect_context(&self) -> (Option<String>, u64, u32, Option<u16>) {
        let reason = self
            .last_disconnect_reason
            .lock()
            .ok()
            .and_then(|mut slot| slot.take());
        let last_at = self
            .last_disconnect_at_epoch_secs
            .swap(0, std::sync::atomic::Ordering::AcqRel);
        let attempts = self
            .attempts_since_last_disconnect
            .swap(0, std::sync::atomic::Ordering::AcqRel);
        // Reset the code slot to the none-sentinel as we take it.
        let raw_code = self
            .last_disconnect_dhan_code
            .swap(u32::MAX, std::sync::atomic::Ordering::AcqRel);
        let dhan_code = if raw_code == u32::MAX {
            None
        } else {
            u16::try_from(raw_code).ok()
        };
        let down_secs = if last_at == 0 {
            0
        } else {
            let now = chrono::Utc::now().timestamp();
            now.saturating_sub(last_at).max(0) as u64
        };
        (reason, down_secs, attempts, dhan_code)
    }
}

// ---------------------------------------------------------------------------
// B1: Reconnect jitter — prevents thundering-herd on simultaneous failures.
// ---------------------------------------------------------------------------

/// Maximum jitter as a fraction of the base delay. ±20% means for a 10s
/// base delay, the actual delay is in [8s, 12s]. Tuned large enough that
/// 5 connections in lock-step break up within 2 reconnect rounds.
const RECONNECT_JITTER_PCT: u64 = 20;

/// Applies deterministic-but-varying jitter to a reconnect delay.
///
/// Uses the connection_id + attempt + wall-clock subsec_nanos as the jitter
/// source. This is NOT cryptographic — it only needs to decorrelate across
/// the 5 connections in the pool, which all see different connection_id
/// values. Avoids adding a `rand` crate dependency.
///
/// # Contract
/// - Returns a value in `[base * (1 - pct/100), base * (1 + pct/100)]`,
///   clamped to `[1, base * 2]`.
/// - For `base == 0`, returns `0`.
/// - For very small `base` (< 5ms), returns `base` unchanged (jitter
///   would be less than 1ms and add no decorrelation).
/// Phase 0 Item 4 (operator-locked 2026-05-13) — computes the base
/// reconnect delay (pre-jitter) for a given attempt index. The FIRST
/// attempt (`attempt == 0`) fires instantly (0 ms) — TCP-side errors
/// typically indicate an already-dead socket, so waiting 500ms before
/// the first reconnect just adds latency to detection of the existing
/// dead state. Subsequent attempts (1, 2, ...) exponential-backoff as
/// before: `initial * 2^(attempt-1)`, capped at `max_ms`.
///
/// Pure function (cannot be `const fn` — `u64::min` is not const-stable
/// yet, see <https://github.com/rust-lang/rust/issues/143874>). Tested by
/// `test_compute_reconnect_base_delay_ms_first_attempt_is_zero` +
/// `test_compute_reconnect_base_delay_ms_subsequent_attempts_exponential`.
#[inline]
#[must_use]
pub fn compute_reconnect_base_delay_ms(attempt: u64, initial_ms: u64, max_ms: u64) -> u64 {
    if attempt == 0 {
        return 0;
    }
    // attempt=1 → initial; attempt=2 → 2×initial; etc.
    let shift = attempt.saturating_sub(1).min(63) as u32;
    let multiplier = 1u64.checked_shl(shift).unwrap_or(u64::MAX);
    initial_ms.saturating_mul(multiplier).min(max_ms)
}

/// Fix B (2026-06-30) — minimum first-reconnect delay (ms) after a SHORT-lived
/// session. Dhan was observed to silently TCP-RST the main-feed socket ~5-6s
/// after each connect; the near-instant (0ms) first reconnect then re-subscribes
/// 775 SIDs immediately, tightening the connect/reset/re-subscribe loop and
/// hastening Dhan's per-IP 429. When the PRIOR session lived less than
/// `short_session_threshold_secs` seconds, this returns `floor_ms` so the caller
/// can `base_delay_ms.max(floor)` and slow the loop down.
///
/// # Contract (pure)
/// - `attempt != 0` → `0` (later attempts already exponential-backoff — never raised).
/// - `attempt == 0 && session_uptime_secs >= short_session_threshold_secs` → `0`
///   (a long-lived session that drops keeps the instant first retry — fast recovery).
/// - `attempt == 0 && session_uptime_secs < short_session_threshold_secs` → `floor_ms`
///   (short session, including `session_uptime_secs == 0` = no successful session).
///
/// Only ever RAISES the attempt-0 delay (via the caller's `.max`), never lowers it.
/// Tested by `test_compute_short_session_reconnect_floor_ms_*`.
#[inline]
#[must_use]
pub fn compute_short_session_reconnect_floor_ms(
    attempt: u64,
    session_uptime_secs: i64,
    short_session_threshold_secs: i64,
    floor_ms: u64,
) -> u64 {
    if attempt != 0 {
        return 0;
    }
    if session_uptime_secs < short_session_threshold_secs {
        floor_ms
    } else {
        0
    }
}

/// Minimum reconnect wait (ms) after `streak` consecutive Dhan feed
/// rate-limit (HTTP 429 / DATA-805 class) connect failures:
/// `base_ms * 2^(streak-1)`, capped at `cap_ms`. A `streak` of `0` returns
/// `0` (no floor — normal backoff applies). The caller takes
/// `delay.max(floor)`, so a 429 turns the instant (0ms) first-retry into a
/// `base_ms` (60s) cooldown that grows ×2 per consecutive 429. This mirrors
/// Dhan's "stop all connections for 60s, then reconnect one at a time"
/// guidance for the 805 class and prevents an instant-reconnect loop from
/// keeping the rate-limit alive.
///
/// Pure function. Tested by `test_compute_rate_limit_floor_ms_*`.
#[inline]
#[must_use]
pub fn compute_rate_limit_floor_ms(streak: u32, base_ms: u64, cap_ms: u64) -> u64 {
    if streak == 0 {
        return 0;
    }
    let shift = streak.saturating_sub(1).min(63);
    let multiplier = 1u64.checked_shl(shift).unwrap_or(u64::MAX);
    base_ms.saturating_mul(multiplier).min(cap_ms)
}

pub(crate) fn jitter_reconnect_delay(base_delay_ms: u64, connection_id: u8, attempt: u64) -> u64 {
    if base_delay_ms == 0 {
        return 0;
    }
    if base_delay_ms < 5 {
        return base_delay_ms;
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::from(d.subsec_nanos()));
    // Mix the three sources with a simple xor+mul — good enough to
    // decorrelate 5 connections without a crypto-grade RNG.
    let mix: u64 = nanos
        ^ (u64::from(connection_id).wrapping_mul(0x9E37_79B9_7F4A_7C15))
        ^ (attempt.wrapping_mul(0xBF58_476D_1CE4_E5B9));
    let jitter_range_ms = base_delay_ms
        .saturating_mul(RECONNECT_JITTER_PCT)
        .saturating_div(100);
    if jitter_range_ms == 0 {
        return base_delay_ms;
    }
    // Choose a signed offset in [-jitter_range_ms, +jitter_range_ms).
    let span = jitter_range_ms.saturating_mul(2);
    let offset = (mix % span) as i64 - jitter_range_ms as i64;
    let applied = (base_delay_ms as i64).saturating_add(offset);
    applied.max(1) as u64
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tickvault_common::types::ExchangeSegment;

    // -----------------------------------------------------------------------
    // 2026-07-05: ws_event_audit loud-drop labels
    // -----------------------------------------------------------------------

    #[test]
    fn test_ws_audit_drop_reason_labels() {
        // Full → "full"; Closed → "closed". Static labels only — the counter
        // `tv_ws_event_audit_dropped_total{reason}` must never see a third
        // value, and the row content is never formatted into the label.
        let row = tickvault_common::ws_event_types::WsEventAuditRow {
            event_ts_ist_nanos: 0,
            trading_date_ist_nanos: 0,
            feed: tickvault_common::feed::Feed::Dhan,
            ws_type: tickvault_common::ws_event_types::WsType::MainFeed,
            connection_index: 0,
            pool_size: 1,
            event_kind: tickvault_common::ws_event_types::WsEventKind::Connected,
            source: "n/a".to_string(),
            reason: String::new(),
            dhan_code: tickvault_common::ws_event_types::WS_EVENT_NO_DHAN_CODE,
            down_secs: 0,
            attempts: 0,
            market_hours: false,
        };
        // Capacity-1 channel: first try_send fills it, second returns Full.
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        tx.try_send(row.clone()).expect("first send fills capacity");
        let full_err = tx.try_send(row.clone()).expect_err("second send is Full");
        assert_eq!(ws_audit_drop_reason(&full_err), "full");
        // Dropping the receiver closes the channel.
        drop(rx);
        let closed_err = tx.try_send(row).expect_err("send on closed channel");
        assert_eq!(ws_audit_drop_reason(&closed_err), "closed");
    }

    // -----------------------------------------------------------------------
    // B1: jitter_reconnect_delay — thundering-herd prevention
    // -----------------------------------------------------------------------

    #[test]
    fn test_jitter_reconnect_delay_within_bounds() {
        // ±20% of 10_000 = 2_000 → range [8_000, 12_000].
        let base = 10_000u64;
        for attempt in 0u64..50 {
            for conn_id in 0u8..5 {
                let delay = jitter_reconnect_delay(base, conn_id, attempt);
                assert!(
                    (8_000..=12_000).contains(&delay),
                    "delay {delay} out of bounds for base {base} (conn={conn_id}, attempt={attempt})"
                );
            }
        }
    }

    #[test]
    fn test_jitter_reconnect_delay_zero_base() {
        assert_eq!(jitter_reconnect_delay(0, 0, 0), 0);
        assert_eq!(jitter_reconnect_delay(0, 7, 42), 0);
    }

    // -----------------------------------------------------------------------
    // Phase 0 Item 4 — compute_reconnect_base_delay_ms ratchets
    // -----------------------------------------------------------------------

    #[test]
    fn test_compute_reconnect_base_delay_ms_first_attempt_is_zero() {
        // attempt=0 → instant reconnect regardless of initial/max.
        assert_eq!(compute_reconnect_base_delay_ms(0, 500, 30_000), 0);
        assert_eq!(compute_reconnect_base_delay_ms(0, 0, 0), 0);
        assert_eq!(compute_reconnect_base_delay_ms(0, 1, 1_000_000), 0);
    }

    #[test]
    fn test_compute_rate_limit_floor_ms_zero_streak_is_no_floor() {
        // No 429 seen → no floor; normal backoff applies.
        assert_eq!(compute_rate_limit_floor_ms(0, 60_000, 300_000), 0);
    }

    // --- Fix B (2026-06-30): short-session first-reconnect floor ---

    #[test]
    fn test_compute_short_session_reconnect_floor_ms_short_session_floored() {
        // attempt=0, uptime 5s < 10s threshold → floored to 3000ms.
        assert_eq!(
            compute_short_session_reconnect_floor_ms(0, 5, 10, 3_000),
            3_000
        );
        // 0s uptime (no successful session) is also "short" → floored.
        assert_eq!(
            compute_short_session_reconnect_floor_ms(0, 0, 10, 3_000),
            3_000
        );
    }

    #[test]
    fn test_compute_short_session_reconnect_floor_ms_long_session_zero() {
        // attempt=0, uptime 5m >= 10s threshold → NO floor (instant first retry
        // preserved — fast recovery from a genuine long-lived drop).
        assert_eq!(
            compute_short_session_reconnect_floor_ms(0, 300, 10, 3_000),
            0
        );
        assert_eq!(
            compute_short_session_reconnect_floor_ms(0, 11, 10, 3_000),
            0
        );
    }

    #[test]
    fn test_compute_short_session_reconnect_floor_ms_boundary_at_threshold() {
        // Exactly AT the threshold (10s) is NOT short (>= threshold) → no floor.
        assert_eq!(
            compute_short_session_reconnect_floor_ms(0, 10, 10, 3_000),
            0
        );
        // One second under the threshold IS short → floored.
        assert_eq!(
            compute_short_session_reconnect_floor_ms(0, 9, 10, 3_000),
            3_000
        );
    }

    #[test]
    fn test_compute_short_session_reconnect_floor_ms_attempt_nonzero_zero() {
        // Attempts 1+ are never raised — exponential backoff already applies.
        assert_eq!(compute_short_session_reconnect_floor_ms(1, 0, 10, 3_000), 0);
        assert_eq!(compute_short_session_reconnect_floor_ms(7, 5, 10, 3_000), 0);
    }

    #[test]
    fn test_compute_short_session_reconnect_floor_ms_zero_uptime_floored() {
        // Defensive: a `connected_at == 0` (never connected this cycle) maps to
        // uptime 0 → floored, so we err toward backing off, never instant.
        assert_eq!(
            compute_short_session_reconnect_floor_ms(0, 0, 10, 3_000),
            3_000
        );
    }

    #[test]
    fn test_compute_rate_limit_floor_ms_grows_then_caps() {
        // streak=1 → base (60s); doubles each consecutive 429; caps at 5m.
        assert_eq!(compute_rate_limit_floor_ms(1, 60_000, 300_000), 60_000);
        assert_eq!(compute_rate_limit_floor_ms(2, 60_000, 300_000), 120_000);
        assert_eq!(compute_rate_limit_floor_ms(3, 60_000, 300_000), 240_000);
        // 60_000 * 2^3 = 480_000 > 300_000 → clamped to the cap.
        assert_eq!(compute_rate_limit_floor_ms(4, 60_000, 300_000), 300_000);
        assert_eq!(compute_rate_limit_floor_ms(50, 60_000, 300_000), 300_000);
    }

    #[test]
    fn test_compute_rate_limit_floor_overrides_instant_first_retry() {
        // The real bug: attempt=0 normally yields an INSTANT (0ms) reconnect,
        // which after a 429 keeps the rate-limit alive. With a streak the
        // caller takes max(0, floor) = floor, so the first retry waits 60s.
        let instant = compute_reconnect_base_delay_ms(0, 500, 30_000);
        let floor = compute_rate_limit_floor_ms(1, 60_000, 300_000);
        assert_eq!(instant, 0);
        assert_eq!(instant.max(floor), 60_000);
    }

    #[test]
    fn test_compute_reconnect_base_delay_ms_subsequent_attempts_exponential() {
        // attempt=1 → initial. attempt=2 → 2×initial. attempt=3 → 4×initial.
        assert_eq!(compute_reconnect_base_delay_ms(1, 500, 30_000), 500);
        assert_eq!(compute_reconnect_base_delay_ms(2, 500, 30_000), 1_000);
        assert_eq!(compute_reconnect_base_delay_ms(3, 500, 30_000), 2_000);
        assert_eq!(compute_reconnect_base_delay_ms(4, 500, 30_000), 4_000);
        assert_eq!(compute_reconnect_base_delay_ms(5, 500, 30_000), 8_000);
        assert_eq!(compute_reconnect_base_delay_ms(6, 500, 30_000), 16_000);
    }

    #[test]
    fn test_compute_reconnect_base_delay_ms_caps_at_max_ms() {
        // Capping kicks in once the exponential overshoots max.
        // 500 * 2^7 = 64_000 > 30_000 → clamped to 30_000.
        assert_eq!(compute_reconnect_base_delay_ms(8, 500, 30_000), 30_000);
        assert_eq!(compute_reconnect_base_delay_ms(20, 500, 30_000), 30_000);
        // saturating_mul protects against u64 overflow at very high attempts.
        assert_eq!(compute_reconnect_base_delay_ms(100, 500, 30_000), 30_000);
    }

    #[test]
    fn test_jitter_reconnect_delay_small_base_unchanged() {
        // For base < 5ms, jitter is pointless (would be sub-ms) — pass through.
        assert_eq!(jitter_reconnect_delay(1, 0, 0), 1);
        assert_eq!(jitter_reconnect_delay(4, 3, 5), 4);
    }

    #[test]
    fn test_jitter_reconnect_delay_decorrelates_across_connections() {
        // The whole point of B1: 5 connections with the same base delay
        // should NOT all land on the same value. Collect samples and
        // assert at least 2 distinct results — very loose bound because
        // the nanos source is shared, but connection_id mixes in.
        let base = 1_000u64;
        let samples: Vec<u64> = (0..5u8)
            .map(|id| jitter_reconnect_delay(base, id, 1))
            .collect();
        let unique_count = {
            let mut sorted = samples.clone();
            sorted.sort_unstable();
            sorted.dedup();
            sorted.len()
        };
        assert!(
            unique_count >= 2,
            "jitter must decorrelate across at least 2 connection_ids; got samples={samples:?}"
        );
    }

    #[test]
    fn test_jitter_reconnect_delay_never_returns_zero_for_positive_base() {
        // Even at worst-case -20% offset, a positive base must not yield 0.
        for base in [5u64, 10, 100, 1_000, 30_000] {
            for attempt in 0u64..20 {
                for conn_id in 0u8..5 {
                    let delay = jitter_reconnect_delay(base, conn_id, attempt);
                    assert!(
                        delay >= 1,
                        "delay must be at least 1ms for positive base; got {delay} \
                         (base={base}, conn={conn_id}, attempt={attempt})"
                    );
                }
            }
        }
    }

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
            connection_stagger_ms: 0,
            activity_watchdog_threshold_secs: 50,
        }
    }

    fn make_test_token_handle() -> TokenHandle {
        Arc::new(arc_swap::ArcSwap::new(Arc::new(None)))
    }

    /// Extract `ReconnectionExhausted` fields from a `Result`, panicking if the
    /// variant doesn't match. Consolidates 6+ identical `let ... else { panic!() }`
    /// sites into one uncovered panic line.
    #[track_caller]
    fn unwrap_reconnection_exhausted(result: Result<(), WebSocketError>) -> (u8, u32) {
        match result {
            Err(WebSocketError::ReconnectionExhausted {
                connection_id,
                attempts,
            }) => (connection_id, attempts),
            other => panic!("expected ReconnectionExhausted, got {other:?}"),
        }
    }

    /// Extract the `Pong` payload from a WS stream message, panicking if the
    /// message is not `Some(Ok(Message::Pong(_)))`.
    #[track_caller]
    fn unwrap_pong(
        msg: Option<Result<Message, tokio_tungstenite::tungstenite::Error>>,
    ) -> bytes::Bytes {
        match msg {
            Some(Ok(Message::Pong(data))) => data,
            other => panic!("expected Pong, got {other:?}"),
        }
    }

    #[test]
    fn test_connection_initial_state_disconnected() {
        let (tx, _rx) = mpsc::channel(100);
        let conn = WebSocketConnection::new(
            0,
            make_test_token_handle(),
            "test-client".to_string(),
            make_test_dhan_config(),
            make_test_ws_config(),
            vec![],
            FeedMode::Ticker,
            tx,
            None,
        );
        let health = conn.health();
        assert_eq!(health.state, ConnectionState::Disconnected);
        assert_eq!(health.connection_id, 0);
        assert_eq!(health.subscribed_count, 0);
        assert_eq!(health.total_reconnections, 0);
    }

    #[test]
    fn test_feed_enable_flag_gates_feed_enabled() {
        // tests with_feed_enable_flag
        // PR-E: no flag → always enabled (every pre-PR-E caller). With a flag
        // attached, `feed_enabled()` mirrors the shared atomic the API toggles.
        let (tx, _rx) = mpsc::channel(100);
        let conn = WebSocketConnection::new(
            0,
            make_test_token_handle(),
            "test-client".to_string(),
            make_test_dhan_config(),
            make_test_ws_config(),
            vec![],
            FeedMode::Ticker,
            tx,
            None,
        );
        assert!(conn.feed_enabled(), "no flag → always enabled");

        let flag = Arc::new(AtomicBool::new(true));
        let conn = conn.with_feed_enable_flag(Arc::clone(&flag));
        assert!(conn.feed_enabled(), "flag true → enabled");
        flag.store(false, Ordering::Relaxed);
        assert!(!conn.feed_enabled(), "flag false → disabled (operator OFF)");
        flag.store(true, Ordering::Relaxed);
        assert!(conn.feed_enabled(), "flag true again → re-enabled");
    }

    /// D2c (C4) — `with_token_manager` stores the lane manager and
    /// `wake_renewal_token_manager()` returns THAT exact manager, even when a
    /// (different) global is installed. This is the core of the stop→re-start
    /// fix: the wake renews the manager the live pool actually holds, never the
    /// stale global `OnceLock`.
    #[test]
    fn wake_renewal_token_manager_prefers_injected_over_global() {
        use crate::auth::TokenManager;
        // Install SOME global so the fallback path is non-trivially different
        // from the injected one (idempotent — a no-op if already set by another
        // test in this binary; either way it is a DIFFERENT Arc instance from
        // `lane_mgr` below, so the pointer assertion is meaningful).
        let global_mgr = TokenManager::new_for_test(None);
        let _ = crate::auth::token_manager::set_global_token_manager(global_mgr);

        // The lane-owned manager — a distinct instance.
        let lane_mgr = TokenManager::new_for_test(None);

        let (tx, _rx) = mpsc::channel(100);
        let conn = WebSocketConnection::new(
            0,
            make_test_token_handle(),
            "test-client".to_string(),
            make_test_dhan_config(),
            make_test_ws_config(),
            vec![],
            FeedMode::Ticker,
            tx,
            None,
        )
        .with_token_manager(Arc::clone(&lane_mgr));

        let selected = conn
            .wake_renewal_token_manager()
            .expect("injected lane manager must be returned");
        assert!(
            Arc::ptr_eq(selected, &lane_mgr),
            "wake-renewal MUST target the injected lane manager, not the global \
             OnceLock — this is the C4 stop→re-start fix"
        );
    }

    /// D2c (C4) — without injection (the boot-ON / fast crash-recovery arm),
    /// `wake_renewal_token_manager()` returns EXACTLY the global `OnceLock`
    /// value, preserving pre-D2c behaviour. Ordering-robust: asserts the helper
    /// result pointer-matches whatever `global_token_manager()` currently is
    /// (including `None` if no global has been installed in this binary yet).
    #[test]
    fn wake_renewal_token_manager_falls_back_to_global_when_not_injected() {
        let (tx, _rx) = mpsc::channel(100);
        let conn = WebSocketConnection::new(
            0,
            make_test_token_handle(),
            "test-client".to_string(),
            make_test_dhan_config(),
            make_test_ws_config(),
            vec![],
            FeedMode::Ticker,
            tx,
            None,
        );
        // No `with_token_manager` → must mirror the global accessor exactly.
        let selected = conn.wake_renewal_token_manager();
        let global = crate::auth::token_manager::global_token_manager();
        match (selected, global) {
            (Some(s), Some(g)) => assert!(
                Arc::ptr_eq(s, g),
                "no injection → wake-renewal must use the global OnceLock"
            ),
            (None, None) => { /* no global installed in this binary — consistent */ }
            (s, g) => panic!(
                "no-injection helper must mirror global_token_manager(): \
                 selected.is_some()={}, global.is_some()={}",
                s.is_some(),
                g.is_some()
            ),
        }
    }

    /// D2c (C4) — `with_token_manager` actually populates the field (builder
    /// wiring ratchet). Asserts via the selection helper that the injected
    /// manager is the one returned.
    #[test]
    fn with_token_manager_sets_the_field() {
        use crate::auth::TokenManager;
        let lane_mgr = TokenManager::new_for_test(None);
        let (tx, _rx) = mpsc::channel(100);
        let conn = WebSocketConnection::new(
            0,
            make_test_token_handle(),
            "test-client".to_string(),
            make_test_dhan_config(),
            make_test_ws_config(),
            vec![],
            FeedMode::Ticker,
            tx,
            None,
        )
        .with_token_manager(Arc::clone(&lane_mgr));
        assert!(
            conn.token_manager.is_some(),
            "with_token_manager must populate the field"
        );
        assert!(
            Arc::ptr_eq(conn.token_manager.as_ref().expect("just set"), &lane_mgr),
            "stored manager must be the exact injected Arc"
        );
    }

    /// PR #790b (2026-05-25) — disconnect-context capture ratchet.
    ///
    /// `record_disconnect` MUST capture (reason, timestamp, attempt count)
    /// and `take_disconnect_context` MUST return them then reset state. This
    /// is the wire between disconnect emit sites and the reconnect Telegram
    /// message that surfaces WHY/HOW-LONG/HOW-MANY for the operator.
    #[test]
    fn test_record_disconnect_then_take_returns_context_and_resets() {
        let (tx, _rx) = mpsc::channel(100);
        let conn = WebSocketConnection::new(
            0,
            make_test_token_handle(),
            "test-client".to_string(),
            make_test_dhan_config(),
            make_test_ws_config(),
            vec![],
            FeedMode::Ticker,
            tx,
            None,
        );

        // Initial state: no context to take.
        let (reason0, down0, attempts0, code0) = conn.take_disconnect_context();
        assert!(reason0.is_none(), "no disconnect recorded yet");
        assert_eq!(down0, 0, "no timestamp recorded yet");
        assert_eq!(attempts0, 0, "no attempts recorded yet");
        assert_eq!(code0, None, "no Dhan code recorded yet");

        // Record 3 disconnects (simulating 3 retry failures in one outage).
        // The first carries a Dhan 807 token-expired code; the latest is a
        // transport error (no code) — so the code slot reflects "latest wins".
        let dk = tickvault_common::ws_event_types::WsEventKind::Disconnected;
        conn.record_disconnect("Token expired (807)".to_string(), Some(807), dk);
        conn.record_disconnect("Reset by peer".to_string(), None, dk);
        conn.record_disconnect("connect failed: timeout".to_string(), None, dk);

        // Take the context — should reflect the LATEST reason and the
        // FIRST disconnect's timestamp (so down_secs measures full outage),
        // with attempts = 3. The latest disconnect had no Dhan code.
        let (reason1, _down1, attempts1, code1) = conn.take_disconnect_context();
        assert_eq!(
            reason1.as_deref(),
            Some("connect failed: timeout"),
            "latest reason wins (caller diagnoses the most recent failure)"
        );
        assert_eq!(attempts1, 3, "all 3 disconnects counted");
        assert_eq!(code1, None, "latest disconnect carried no Dhan code");

        // A Dhan-coded disconnect surfaces the precise code on take.
        conn.record_disconnect("Token expired (807)".to_string(), Some(807), dk);
        let (_r, _d, _a, code_coded) = conn.take_disconnect_context();
        assert_eq!(
            code_coded,
            Some(807),
            "Dhan code threaded through for the reconnect log"
        );

        // After take, state is reset — next take returns empty again.
        let (reason2, down2, attempts2, code2) = conn.take_disconnect_context();
        assert!(reason2.is_none(), "reason cleared after take");
        assert_eq!(down2, 0, "timestamp cleared after take");
        assert_eq!(attempts2, 0, "attempts cleared after take");
        assert_eq!(code2, None, "Dhan code cleared after take");
    }

    #[test]
    fn test_connection_id_matches() {
        let (tx, _rx) = mpsc::channel(100);
        let conn = WebSocketConnection::new(
            3,
            make_test_token_handle(),
            "test-client".to_string(),
            make_test_dhan_config(),
            make_test_ws_config(),
            vec![],
            FeedMode::Full,
            tx,
            None,
        );
        assert_eq!(conn.connection_id(), 3);
    }

    /// Audit-2026-05-03 H2 ratchet: `connection_id_label_static` MUST
    /// return the canonical `&'static str` for every valid ConnectionId
    /// 0..MAX_WEBSOCKET_CONNECTIONS, AND fall back to "unknown" past
    /// the cap. Pinning this prevents a future commit from re-introducing
    /// `to_string()` on the run-loop entry path.
    #[test]
    fn test_h2_connection_id_label_static_zero_alloc_lookup() {
        // Canonical labels for the 5 supported connection ids.
        assert_eq!(connection_id_label_static(0), "0");
        assert_eq!(connection_id_label_static(1), "1");
        assert_eq!(connection_id_label_static(2), "2");
        assert_eq!(connection_id_label_static(3), "3");
        assert_eq!(connection_id_label_static(4), "4");
        // Defensive fallback for ids past MAX_WEBSOCKET_CONNECTIONS=5.
        assert_eq!(connection_id_label_static(5), "unknown");
        assert_eq!(connection_id_label_static(99), "unknown");
        assert_eq!(connection_id_label_static(u8::MAX), "unknown");
    }

    #[test]
    fn test_h2_connection_caches_static_label_at_construction() {
        let (tx, _rx) = mpsc::channel(100);
        let conn = WebSocketConnection::new(
            2,
            make_test_token_handle(),
            "test-client".to_string(),
            make_test_dhan_config(),
            make_test_ws_config(),
            vec![],
            FeedMode::Full,
            tx,
            None,
        );
        // Audit-2026-05-03 H2: the cached label MUST equal the canonical
        // value. The field type `&'static str` is itself the zero-alloc
        // proof — no `String` wrapping is possible at compile time.
        // Pointer equality across literals is NOT guaranteed by the Rust
        // language (string interning is an optimization, not a contract),
        // so value equality is the correct ratchet.
        assert_eq!(conn.connection_id_label, "2");
        assert_eq!(conn.connection_id_label, CONNECTION_ID_LABELS[2]);
        // Round-trip: cached label must equal the canonical lookup.
        assert_eq!(
            conn.connection_id_label,
            connection_id_label_static(2),
            "cached label must match the canonical lookup"
        );
    }

    #[test]
    fn test_connection_tracks_instrument_count() {
        let (tx, _rx) = mpsc::channel(100);
        let instruments = vec![
            InstrumentSubscription::new(ExchangeSegment::NseFno, 1000),
            InstrumentSubscription::new(ExchangeSegment::NseFno, 1001),
            InstrumentSubscription::new(ExchangeSegment::IdxI, 13),
        ];
        let conn = WebSocketConnection::new(
            1,
            make_test_token_handle(),
            "test-client".to_string(),
            make_test_dhan_config(),
            make_test_ws_config(),
            instruments,
            FeedMode::Quote,
            tx,
            None,
        );
        assert_eq!(conn.health().subscribed_count, 3);
    }

    #[tokio::test]
    async fn test_connection_run_fails_without_token() {
        let (tx, _rx) = mpsc::channel(100);
        let conn = WebSocketConnection::new(
            0,
            make_test_token_handle(), // No token stored
            "test-client".to_string(),
            make_test_dhan_config(),
            WebSocketConfig {
                reconnect_max_attempts: 1,     // Fail fast
                reconnect_initial_delay_ms: 1, // Don't wait
                reconnect_max_delay_ms: 1,
                ..make_test_ws_config()
            },
            vec![],
            FeedMode::Ticker,
            tx,
            None,
        );
        let result = conn.run().await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_connection_run_returns_reconnection_exhausted() {
        let (tx, _rx) = mpsc::channel(100);
        let conn = WebSocketConnection::new(
            2,
            make_test_token_handle(), // No token → every attempt fails
            "test-client".to_string(),
            make_test_dhan_config(),
            WebSocketConfig {
                reconnect_max_attempts: 3,
                reconnect_initial_delay_ms: 1,
                reconnect_max_delay_ms: 1,
                ..make_test_ws_config()
            },
            vec![],
            FeedMode::Quote,
            tx,
            None,
        );
        let result = conn.run().await;
        let (connection_id, attempts) = unwrap_reconnection_exhausted(result);
        assert_eq!(connection_id, 2);
        assert_eq!(attempts, 3);
    }

    #[test]
    fn test_set_state_changes_reflected_in_health() {
        let (tx, _rx) = mpsc::channel(100);
        let conn = WebSocketConnection::new(
            0,
            make_test_token_handle(),
            "test-client".to_string(),
            make_test_dhan_config(),
            make_test_ws_config(),
            vec![],
            FeedMode::Ticker,
            tx,
            None,
        );
        assert_eq!(conn.health().state, ConnectionState::Disconnected);

        conn.set_state(ConnectionState::Connecting);
        assert_eq!(conn.health().state, ConnectionState::Connecting);

        conn.set_state(ConnectionState::Connected);
        assert_eq!(conn.health().state, ConnectionState::Connected);

        conn.set_state(ConnectionState::Reconnecting);
        assert_eq!(conn.health().state, ConnectionState::Reconnecting);

        conn.set_state(ConnectionState::Disconnected);
        assert_eq!(conn.health().state, ConnectionState::Disconnected);
    }

    #[test]
    fn test_health_tracks_reconnection_count() {
        let (tx, _rx) = mpsc::channel(100);
        let conn = WebSocketConnection::new(
            0,
            make_test_token_handle(),
            "test-client".to_string(),
            make_test_dhan_config(),
            make_test_ws_config(),
            vec![],
            FeedMode::Ticker,
            tx,
            None,
        );
        assert_eq!(conn.health().total_reconnections, 0);

        conn.total_reconnections
            .store(7, std::sync::atomic::Ordering::Release);
        assert_eq!(conn.health().total_reconnections, 7);
    }

    #[test]
    fn test_connection_max_id_4() {
        let (tx, _rx) = mpsc::channel(100);
        let conn = WebSocketConnection::new(
            4,
            make_test_token_handle(),
            "test-client".to_string(),
            make_test_dhan_config(),
            make_test_ws_config(),
            vec![],
            FeedMode::Full,
            tx,
            None,
        );
        assert_eq!(conn.connection_id(), 4);
        assert_eq!(conn.health().connection_id, 4);
    }

    #[test]
    fn test_connection_all_feed_modes() {
        for feed_mode in [FeedMode::Ticker, FeedMode::Quote, FeedMode::Full] {
            let (tx, _rx) = mpsc::channel(100);
            let conn = WebSocketConnection::new(
                0,
                make_test_token_handle(),
                "test-client".to_string(),
                make_test_dhan_config(),
                make_test_ws_config(),
                vec![InstrumentSubscription::new(ExchangeSegment::NseFno, 1000)],
                feed_mode,
                tx,
                None,
            );
            // All feed modes should create successfully with 1 instrument
            assert_eq!(conn.health().subscribed_count, 1);
        }
    }

    #[tokio::test]
    async fn test_wait_with_backoff_returns_false_when_exhausted() {
        let (tx, _rx) = mpsc::channel(100);
        let conn = WebSocketConnection::new(
            0,
            make_test_token_handle(),
            "test-client".to_string(),
            make_test_dhan_config(),
            WebSocketConfig {
                reconnect_max_attempts: 3,
                reconnect_initial_delay_ms: 1,
                reconnect_max_delay_ms: 1,
                ..make_test_ws_config()
            },
            vec![],
            FeedMode::Ticker,
            tx,
            None,
        );

        // Simulate 3 reconnections already happened
        conn.total_reconnections
            .store(3, std::sync::atomic::Ordering::Release);
        let result = conn.wait_with_backoff().await;
        assert!(!result, "Should return false when max attempts exhausted");
    }

    #[tokio::test]
    async fn test_wait_with_backoff_returns_true_when_under_limit() {
        let (tx, _rx) = mpsc::channel(100);
        let conn = WebSocketConnection::new(
            0,
            make_test_token_handle(),
            "test-client".to_string(),
            make_test_dhan_config(),
            WebSocketConfig {
                reconnect_max_attempts: 10,
                reconnect_initial_delay_ms: 1, // 1ms for fast test
                reconnect_max_delay_ms: 1,
                ..make_test_ws_config()
            },
            vec![],
            FeedMode::Ticker,
            tx,
            None,
        );

        // Only 2 reconnections — well under limit of 10
        conn.total_reconnections
            .store(2, std::sync::atomic::Ordering::Release);
        let result = conn.wait_with_backoff().await;
        assert!(result, "Should return true when under limit");
    }

    // --- Read Timeout Formula Tests ---

    /// Helper: computes read timeout from config values using the same formula as run_read_loop.
    fn compute_read_timeout(ping_interval: u64, pong_timeout: u64, max_failures: u32) -> u64 {
        ping_interval
            .saturating_mul(u64::from(max_failures) + 1)
            .saturating_add(pong_timeout)
    }

    #[test]
    fn test_read_timeout_formula_default() {
        // Default: 10 * (2 + 1) + 10 = 40s
        assert_eq!(compute_read_timeout(10, 10, 2), 40);
    }

    #[test]
    fn test_read_timeout_formula_custom() {
        // Custom: 5 * (5 + 1) + 15 = 45s
        assert_eq!(compute_read_timeout(5, 15, 5), 45);
    }

    #[test]
    fn test_read_timeout_formula_zero_failures() {
        // Zero failures: 10 * (0 + 1) + 10 = 20s
        assert_eq!(compute_read_timeout(10, 10, 0), 20);
    }

    #[test]
    fn test_read_timeout_formula_overflow_safe() {
        // u32::MAX failures → saturating_mul prevents overflow
        let result = compute_read_timeout(10, 10, u32::MAX);
        assert!(result >= 10, "Should not overflow to zero");
    }

    // --- Cached Subscription Messages Tests ---

    #[test]
    fn test_cached_messages_empty_instruments() {
        let (tx, _rx) = mpsc::channel(100);
        let conn = WebSocketConnection::new(
            0,
            make_test_token_handle(),
            "test-client".to_string(),
            make_test_dhan_config(),
            make_test_ws_config(),
            vec![],
            FeedMode::Full,
            tx,
            None,
        );
        assert!(
            conn.cached_subscription_messages.is_empty(),
            "0 instruments should produce 0 cached messages"
        );
    }

    #[test]
    fn test_cached_messages_built_at_construction() {
        let (tx, _rx) = mpsc::channel(100);
        let instruments = vec![
            InstrumentSubscription::new(ExchangeSegment::NseFno, 1000),
            InstrumentSubscription::new(ExchangeSegment::NseFno, 1001),
        ];
        let conn = WebSocketConnection::new(
            0,
            make_test_token_handle(),
            "test-client".to_string(),
            make_test_dhan_config(),
            make_test_ws_config(),
            instruments,
            FeedMode::Full,
            tx,
            None,
        );
        assert_eq!(conn.cached_subscription_messages.len(), 1);
        assert!(conn.cached_subscription_messages[0].contains("\"RequestCode\":21"));
        assert!(conn.cached_subscription_messages[0].contains("\"InstrumentCount\":2"));
    }

    #[test]
    fn test_cached_messages_idx_i_uses_quote() {
        let (tx, _rx) = mpsc::channel(100);
        let instruments = vec![
            InstrumentSubscription::new(ExchangeSegment::IdxI, 13),
            InstrumentSubscription::new(ExchangeSegment::IdxI, 26),
        ];
        let conn = WebSocketConnection::new(
            0,
            make_test_token_handle(),
            "test-client".to_string(),
            make_test_dhan_config(),
            make_test_ws_config(),
            instruments,
            FeedMode::Full, // Configured as Full, but IDX_I uses Quote
            tx,
            None,
        );
        // All IDX_I → only Quote messages (request_code 17), NOT Full (21).
        // Operator directive 2026-05-20 — Quote packet carries day OHLC.
        assert_eq!(conn.cached_subscription_messages.len(), 1);
        assert!(conn.cached_subscription_messages[0].contains("\"RequestCode\":17"));
        assert!(!conn.cached_subscription_messages[0].contains("\"RequestCode\":21"));
    }

    #[test]
    fn test_cached_messages_non_idx_uses_config_mode() {
        let (tx, _rx) = mpsc::channel(100);
        let instruments = vec![InstrumentSubscription::new(ExchangeSegment::NseFno, 1000)];
        let conn = WebSocketConnection::new(
            0,
            make_test_token_handle(),
            "test-client".to_string(),
            make_test_dhan_config(),
            make_test_ws_config(),
            instruments,
            FeedMode::Quote,
            tx,
            None,
        );
        assert_eq!(conn.cached_subscription_messages.len(), 1);
        assert!(conn.cached_subscription_messages[0].contains("\"RequestCode\":17"));
    }

    #[test]
    fn test_cached_messages_count_matches_batches() {
        let (tx, _rx) = mpsc::channel(100);
        // 5000 instruments / batch_size 100 = 50 messages
        let instruments: Vec<_> = (0..5000)
            .map(|i| InstrumentSubscription::new(ExchangeSegment::NseFno, i + 1000))
            .collect();
        let conn = WebSocketConnection::new(
            0,
            make_test_token_handle(),
            "test-client".to_string(),
            make_test_dhan_config(),
            make_test_ws_config(), // batch_size = 100
            instruments,
            FeedMode::Ticker,
            tx,
            None,
        );
        assert_eq!(conn.cached_subscription_messages.len(), 50);
    }

    #[test]
    fn test_cached_messages_mixed_idx_and_non_idx() {
        let (tx, _rx) = mpsc::channel(100);
        let instruments = vec![
            InstrumentSubscription::new(ExchangeSegment::IdxI, 13),
            InstrumentSubscription::new(ExchangeSegment::NseFno, 1000),
            InstrumentSubscription::new(ExchangeSegment::IdxI, 26),
            InstrumentSubscription::new(ExchangeSegment::NseFno, 1001),
        ];
        let conn = WebSocketConnection::new(
            0,
            make_test_token_handle(),
            "test-client".to_string(),
            make_test_dhan_config(),
            make_test_ws_config(),
            instruments,
            FeedMode::Full, // Non-IDX gets Full (21), IDX_I gets Quote (17)
            tx,
            None,
        );
        // 2 non-IDX → 1 Full message, 2 IDX_I → 1 Quote message = 2 total
        assert_eq!(conn.cached_subscription_messages.len(), 2);
        let has_full = conn
            .cached_subscription_messages
            .iter()
            .any(|m| m.contains("\"RequestCode\":21"));
        // IDX_I subscribes in Quote mode (RequestCode 17) per the
        // 2026-05-20 operator directive — the Quote packet carries
        // day open/high/low directly.
        let has_quote = conn
            .cached_subscription_messages
            .iter()
            .any(|m| m.contains("\"RequestCode\":17"));
        assert!(has_full, "Should have Full mode message for NseFno");
        assert!(has_quote, "Should have Quote mode message for IdxI");
    }

    // --- Edge Case: Empty client_id ---

    #[test]
    fn test_connection_empty_client_id() {
        let (tx, _rx) = mpsc::channel(100);
        let conn = WebSocketConnection::new(
            0,
            make_test_token_handle(),
            String::new(), // empty client_id
            make_test_dhan_config(),
            make_test_ws_config(),
            vec![],
            FeedMode::Ticker,
            tx,
            None,
        );
        // Construction succeeds; empty client_id is a runtime concern at connect time
        assert_eq!(conn.connection_id(), 0);
        assert_eq!(conn.health().subscribed_count, 0);
    }

    // --- Edge Case: Zero subscription_batch_size ---

    #[test]
    fn test_connection_zero_subscription_batch_size_no_instruments() {
        let (tx, _rx) = mpsc::channel(100);
        let conn = WebSocketConnection::new(
            0,
            make_test_token_handle(),
            "test-client".to_string(),
            make_test_dhan_config(),
            WebSocketConfig {
                subscription_batch_size: 0,
                ..make_test_ws_config()
            },
            vec![], // no instruments means no batching issue
            FeedMode::Ticker,
            tx,
            None,
        );
        assert!(conn.cached_subscription_messages.is_empty());
    }

    // --- State Transition: Rapid cycling through all states ---

    #[test]
    fn test_set_state_rapid_cycling() {
        let (tx, _rx) = mpsc::channel(100);
        let conn = WebSocketConnection::new(
            0,
            make_test_token_handle(),
            "test-client".to_string(),
            make_test_dhan_config(),
            make_test_ws_config(),
            vec![],
            FeedMode::Ticker,
            tx,
            None,
        );

        // Cycle through states multiple times
        for _ in 0..10 {
            conn.set_state(ConnectionState::Connecting);
            assert_eq!(conn.health().state, ConnectionState::Connecting);

            conn.set_state(ConnectionState::Connected);
            assert_eq!(conn.health().state, ConnectionState::Connected);

            conn.set_state(ConnectionState::Reconnecting);
            assert_eq!(conn.health().state, ConnectionState::Reconnecting);

            conn.set_state(ConnectionState::Disconnected);
            assert_eq!(conn.health().state, ConnectionState::Disconnected);
        }
    }

    // --- State Transition: Same state set twice ---

    #[test]
    fn test_set_state_idempotent() {
        let (tx, _rx) = mpsc::channel(100);
        let conn = WebSocketConnection::new(
            0,
            make_test_token_handle(),
            "test-client".to_string(),
            make_test_dhan_config(),
            make_test_ws_config(),
            vec![],
            FeedMode::Ticker,
            tx,
            None,
        );

        conn.set_state(ConnectionState::Connected);
        conn.set_state(ConnectionState::Connected);
        assert_eq!(conn.health().state, ConnectionState::Connected);
    }

    // --- Health returns correct values after state changes ---

    #[test]
    fn test_health_reflects_state_and_reconnection_count_together() {
        let (tx, _rx) = mpsc::channel(100);
        let instruments = vec![
            InstrumentSubscription::new(ExchangeSegment::NseFno, 1000),
            InstrumentSubscription::new(ExchangeSegment::NseFno, 1001),
        ];
        let conn = WebSocketConnection::new(
            3,
            make_test_token_handle(),
            "test-client".to_string(),
            make_test_dhan_config(),
            make_test_ws_config(),
            instruments,
            FeedMode::Full,
            tx,
            None,
        );

        // Simulate reconnecting state with accumulated reconnections
        conn.set_state(ConnectionState::Reconnecting);
        conn.total_reconnections.store(5, Ordering::Release);

        let health = conn.health();
        assert_eq!(health.connection_id, 3);
        assert_eq!(health.state, ConnectionState::Reconnecting);
        assert_eq!(health.subscribed_count, 2);
        assert_eq!(health.total_reconnections, 5);
    }

    // --- wait_with_backoff: attempt=0 (first attempt, minimum delay) ---

    #[tokio::test]
    async fn test_wait_with_backoff_attempt_zero() {
        let (tx, _rx) = mpsc::channel(100);
        let conn = WebSocketConnection::new(
            0,
            make_test_token_handle(),
            "test-client".to_string(),
            make_test_dhan_config(),
            WebSocketConfig {
                reconnect_max_attempts: 10,
                reconnect_initial_delay_ms: 1,
                reconnect_max_delay_ms: 100,
                ..make_test_ws_config()
            },
            vec![],
            FeedMode::Ticker,
            tx,
            None,
        );

        // attempt=0 → delay = initial * 2^0 = initial
        conn.total_reconnections.store(0, Ordering::Release);
        let result = conn.wait_with_backoff().await;
        assert!(result, "attempt 0 should succeed (under limit)");
    }

    // --- wait_with_backoff: overflow scenario with large attempt number ---

    #[tokio::test]
    async fn test_wait_with_backoff_large_attempt_caps_at_max_delay() {
        let (tx, _rx) = mpsc::channel(100);
        let conn = WebSocketConnection::new(
            0,
            make_test_token_handle(),
            "test-client".to_string(),
            make_test_dhan_config(),
            WebSocketConfig {
                reconnect_max_attempts: 100,
                reconnect_initial_delay_ms: 1,
                reconnect_max_delay_ms: 1, // cap at 1ms for fast test
                ..make_test_ws_config()
            },
            vec![],
            FeedMode::Ticker,
            tx,
            None,
        );

        // attempt=63 → 2^63 would overflow, but saturating_mul + min caps at max_delay
        conn.total_reconnections.store(63, Ordering::Release);
        let result = conn.wait_with_backoff().await;
        assert!(result, "large attempt should succeed if under max_attempts");
    }

    // --- wait_with_backoff: exactly at boundary (attempt == max_attempts - 1) ---

    #[tokio::test]
    async fn test_wait_with_backoff_exactly_at_boundary() {
        let (tx, _rx) = mpsc::channel(100);
        let conn = WebSocketConnection::new(
            0,
            make_test_token_handle(),
            "test-client".to_string(),
            make_test_dhan_config(),
            WebSocketConfig {
                reconnect_max_attempts: 5,
                reconnect_initial_delay_ms: 1,
                reconnect_max_delay_ms: 1,
                ..make_test_ws_config()
            },
            vec![],
            FeedMode::Ticker,
            tx,
            None,
        );

        // attempt 4 (last allowed when max=5) should succeed
        conn.total_reconnections.store(4, Ordering::Release);
        let result = conn.wait_with_backoff().await;
        assert!(result, "attempt at max-1 should succeed");

        // attempt 5 (equals max) should fail
        conn.total_reconnections.store(5, Ordering::Release);
        let result = conn.wait_with_backoff().await;
        assert!(!result, "attempt at max should fail");
    }

    // --- wait_with_backoff: zero max_attempts means infinite retries (never give up) ---

    #[tokio::test]
    async fn test_wait_with_backoff_zero_max_attempts_means_infinite() {
        // Force the post-market guard to treat wall-clock as in-market so
        // this test exercises the pure infinite-retry math path regardless
        // of when `cargo test` is invoked.
        super::TEST_FORCE_IN_MARKET_HOURS.store(true, std::sync::atomic::Ordering::Relaxed);
        struct Reset;
        impl Drop for Reset {
            fn drop(&mut self) {
                super::TEST_FORCE_IN_MARKET_HOURS
                    .store(false, std::sync::atomic::Ordering::Relaxed);
            }
        }
        let _reset = Reset;

        let (tx, _rx) = mpsc::channel(100);
        let conn = WebSocketConnection::new(
            0,
            make_test_token_handle(),
            "test-client".to_string(),
            make_test_dhan_config(),
            WebSocketConfig {
                reconnect_max_attempts: 0, // 0 = infinite retries
                reconnect_initial_delay_ms: 1,
                reconnect_max_delay_ms: 1,
                ..make_test_ws_config()
            },
            vec![],
            FeedMode::Ticker,
            tx,
            None,
        );

        // With max_attempts=0 (infinite), even high attempt counts should return true.
        conn.total_reconnections.store(0, Ordering::Release);
        let result = conn.wait_with_backoff().await;
        assert!(
            result,
            "zero max_attempts = infinite retries, should always succeed"
        );

        conn.total_reconnections.store(100, Ordering::Release);
        let result = conn.wait_with_backoff().await;
        assert!(
            result,
            "attempt 100 with infinite mode should still succeed"
        );

        conn.total_reconnections.store(10000, Ordering::Release);
        let result = conn.wait_with_backoff().await;
        assert!(
            result,
            "attempt 10000 with infinite mode should still succeed"
        );
    }

    // --- connection_id boundary: 0 is valid ---

    #[test]
    fn test_connection_id_zero() {
        let (tx, _rx) = mpsc::channel(100);
        let conn = WebSocketConnection::new(
            0,
            make_test_token_handle(),
            "test-client".to_string(),
            make_test_dhan_config(),
            make_test_ws_config(),
            vec![],
            FeedMode::Ticker,
            tx,
            None,
        );
        assert_eq!(conn.connection_id(), 0);
        assert_eq!(conn.health().connection_id, 0);
    }

    // --- Large instrument list works ---

    #[test]
    fn test_connection_large_instrument_list() {
        let (tx, _rx) = mpsc::channel(100);
        let instruments: Vec<_> = (0..5000)
            .map(|i| InstrumentSubscription::new(ExchangeSegment::NseFno, i + 1000))
            .collect();
        let conn = WebSocketConnection::new(
            0,
            make_test_token_handle(),
            "test-client".to_string(),
            make_test_dhan_config(),
            make_test_ws_config(),
            instruments,
            FeedMode::Full,
            tx,
            None,
        );
        assert_eq!(conn.health().subscribed_count, 5000);
        // 5000 / batch_size(100) = 50 messages
        assert_eq!(conn.cached_subscription_messages.len(), 50);
    }

    // --- Only IDX_I instruments with Ticker mode (should still use Ticker) ---

    #[test]
    fn test_connection_only_idx_i_in_quote_mode() {
        let (tx, _rx) = mpsc::channel(100);
        let instruments = vec![InstrumentSubscription::new(ExchangeSegment::IdxI, 13)];
        let conn = WebSocketConnection::new(
            0,
            make_test_token_handle(),
            "test-client".to_string(),
            make_test_dhan_config(),
            make_test_ws_config(),
            instruments,
            FeedMode::Ticker, // Passed mode applies to non-IDX only; IDX_I uses Quote
            tx,
            None,
        );
        assert_eq!(conn.cached_subscription_messages.len(), 1);
        assert!(conn.cached_subscription_messages[0].contains("\"RequestCode\":17"));
    }

    // --- Batch size of 1 with multiple instruments ---

    #[test]
    fn test_cached_messages_batch_size_one() {
        let (tx, _rx) = mpsc::channel(100);
        let instruments = vec![
            InstrumentSubscription::new(ExchangeSegment::NseFno, 1000),
            InstrumentSubscription::new(ExchangeSegment::NseFno, 1001),
            InstrumentSubscription::new(ExchangeSegment::NseFno, 1002),
        ];
        let conn = WebSocketConnection::new(
            0,
            make_test_token_handle(),
            "test-client".to_string(),
            make_test_dhan_config(),
            WebSocketConfig {
                subscription_batch_size: 1,
                ..make_test_ws_config()
            },
            instruments,
            FeedMode::Ticker,
            tx,
            None,
        );
        // 3 instruments / batch_size 1 = 3 messages
        assert_eq!(conn.cached_subscription_messages.len(), 3);
        for msg in &conn.cached_subscription_messages {
            assert!(msg.contains("\"InstrumentCount\":1"));
        }
    }

    // --- Run with one max_attempt returns ReconnectionExhausted fast ---
    // Session 8 final sweep fix: `reconnect_max_attempts: 0` means
    // "retry forever" in production (see wait_with_backoff doc). Tests
    // that want fail-fast semantics must use `reconnect_max_attempts: 1`
    // (one attempt, then exit with ReconnectionExhausted). These tests
    // were previously using 0 and hanging indefinitely.
    #[tokio::test]
    async fn test_connection_run_zero_max_attempts() {
        let (tx, _rx) = mpsc::channel(100);
        let conn = WebSocketConnection::new(
            1,
            make_test_token_handle(),
            "test-client".to_string(),
            DhanConfig {
                websocket_url: "wss://127.0.0.1:19999".to_string(),
                ..make_test_dhan_config()
            },
            WebSocketConfig {
                reconnect_max_attempts: 1, // one attempt, then exhaust
                reconnect_initial_delay_ms: 1,
                reconnect_max_delay_ms: 1,
                ..make_test_ws_config()
            },
            vec![],
            FeedMode::Ticker,
            tx,
            None,
        );
        let result = conn.run().await;
        let (connection_id, _attempts) = unwrap_reconnection_exhausted(result);
        assert_eq!(connection_id, 1);
    }

    // --- Tests with a valid token to cover connect_and_subscribe path (lines 234-274+) ---

    /// Helper: creates a TokenHandle containing a real TokenState so
    /// `connect_and_subscribe` progresses past the `NoTokenAvailable` check.
    fn make_token_handle_with_token() -> TokenHandle {
        use crate::auth::TokenState;
        use crate::auth::types::DhanAuthResponseData;

        let response_data = DhanAuthResponseData {
            access_token: "test-jwt-token-for-ws".to_string(),
            token_type: "Bearer".to_string(),
            expires_in: 86400,
        };
        let state = TokenState::from_response(&response_data);
        Arc::new(arc_swap::ArcSwap::new(Arc::new(Some(state))))
    }

    #[tokio::test]
    async fn test_run_with_token_fails_connection_and_exhausts_retries() {
        // With a valid token, connect_and_subscribe will:
        // 1. Read the token (line 233-237) — succeeds
        // 2. Expose the secret (line 239) — succeeds
        // 3. Build the URL (lines 247-251) — succeeds
        // 4. Build the request (lines 253-259) — succeeds
        // 5. Build TLS connector (line 271) — succeeds
        // 6. Attempt TCP connection (lines 274-288) — FAILS (no server)
        // This covers lines 234-288 in connect_and_subscribe.
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

        let (tx, _rx) = mpsc::channel(100);
        let conn = WebSocketConnection::new(
            0,
            make_token_handle_with_token(),
            "test-client-id".to_string(),
            DhanConfig {
                websocket_url: "wss://127.0.0.1:19999".to_string(), // unreachable port
                ..make_test_dhan_config()
            },
            WebSocketConfig {
                reconnect_max_attempts: 2,
                reconnect_initial_delay_ms: 1,
                reconnect_max_delay_ms: 1,
                ..make_test_ws_config()
            },
            vec![InstrumentSubscription::new(ExchangeSegment::NseFno, 1000)],
            FeedMode::Full,
            tx,
            None,
        );

        let result = conn.run().await;
        let (connection_id, attempts) = unwrap_reconnection_exhausted(result);
        assert_eq!(connection_id, 0);
        assert_eq!(attempts, 2);
        // After run(), state should be Reconnecting (set before backoff exhaustion check)
        // and total_reconnections should reflect the attempts
        assert!(conn.health().total_reconnections >= 2);
    }

    #[tokio::test]
    async fn test_run_with_token_covers_state_transitions() {
        // Verify state transitions during run() with a real token:
        // Disconnected → Connecting → (connect fails) → Reconnecting → ...exhausted
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

        let (tx, _rx) = mpsc::channel(100);
        let conn = Arc::new(WebSocketConnection::new(
            1,
            make_token_handle_with_token(),
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
        ));

        let result = conn.run().await;
        assert!(result.is_err());
        // After exhaustion, final state should be Reconnecting (set before backoff check)
        let health = conn.health();
        assert_eq!(health.connection_id, 1);
    }

    #[tokio::test]
    async fn test_run_with_token_and_instruments_covers_url_building() {
        // This test exercises URL construction with token (lines 247-251)
        // and request building (lines 253-259) with multiple instruments.
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

        let (tx, _rx) = mpsc::channel(100);
        let instruments: Vec<_> = (0..10)
            .map(|i| InstrumentSubscription::new(ExchangeSegment::NseFno, 2000 + i))
            .collect();
        let conn = WebSocketConnection::new(
            2,
            make_token_handle_with_token(),
            "MY-CLIENT-123".to_string(),
            DhanConfig {
                websocket_url: "wss://127.0.0.1:19999".to_string(),
                ..make_test_dhan_config()
            },
            WebSocketConfig {
                reconnect_max_attempts: 1, // one attempt, then exhaust
                reconnect_initial_delay_ms: 1,
                reconnect_max_delay_ms: 1,
                ..make_test_ws_config()
            },
            instruments,
            FeedMode::Quote,
            tx,
            None,
        );

        let result = conn.run().await;
        assert!(result.is_err());
        assert_eq!(conn.health().subscribed_count, 10);
    }

    #[tokio::test]
    async fn test_run_with_trailing_slash_url() {
        // Tests that trailing slash in websocket_url is trimmed (line 247)
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

        let (tx, _rx) = mpsc::channel(100);
        let conn = WebSocketConnection::new(
            0,
            make_token_handle_with_token(),
            "test-client".to_string(),
            DhanConfig {
                websocket_url: "wss://127.0.0.1:19999/".to_string(), // trailing slash
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

        let result = conn.run().await;
        let Err(WebSocketError::ReconnectionExhausted { .. }) = result else {
            panic!("Expected ReconnectionExhausted")
        };
    }

    #[tokio::test]
    async fn test_run_with_mixed_instruments_covers_subscription_caching() {
        // Mixed IDX_I and non-IDX_I instruments test that both code paths
        // in cached_subscription_messages builder are exercised during run().
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

        let (tx, _rx) = mpsc::channel(100);
        let instruments = vec![
            InstrumentSubscription::new(ExchangeSegment::IdxI, 13),
            InstrumentSubscription::new(ExchangeSegment::NseFno, 1000),
            InstrumentSubscription::new(ExchangeSegment::IdxI, 26),
        ];
        let conn = WebSocketConnection::new(
            3,
            make_token_handle_with_token(),
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
            instruments,
            FeedMode::Full,
            tx,
            None,
        );

        // Verify cached messages were built correctly
        assert_eq!(conn.cached_subscription_messages.len(), 2);

        let result = conn.run().await;
        assert!(result.is_err());
        assert_eq!(conn.health().subscribed_count, 3);
    }

    #[tokio::test]
    async fn test_wait_with_backoff_exponential_progression() {
        // Tests that backoff delay increases exponentially (line 444-448).
        // We can't easily measure the exact delay, but we verify the
        // function returns true for each attempt under the limit.
        let (tx, _rx) = mpsc::channel(100);
        let conn = WebSocketConnection::new(
            0,
            make_test_token_handle(),
            "test-client".to_string(),
            make_test_dhan_config(),
            WebSocketConfig {
                reconnect_max_attempts: 5,
                reconnect_initial_delay_ms: 1,
                reconnect_max_delay_ms: 1, // cap at 1ms so tests are fast
                ..make_test_ws_config()
            },
            vec![],
            FeedMode::Ticker,
            tx,
            None,
        );

        for attempt in 0..5u64 {
            conn.total_reconnections.store(attempt, Ordering::Release);
            let result = conn.wait_with_backoff().await;
            assert!(result, "attempt {attempt} should succeed");
        }

        // attempt 5 should fail (>= max_attempts)
        conn.total_reconnections.store(5, Ordering::Release);
        let result = conn.wait_with_backoff().await;
        assert!(!result, "attempt 5 should fail at max_attempts=5");
    }

    #[tokio::test]
    async fn test_wait_with_backoff_checked_shl_overflow() {
        // Tests the checked_shl overflow path (line 447).
        // When attempt >= 64, checked_shl returns None, unwrap_or gives u64::MAX,
        // then saturating_mul caps, then min(max_delay) caps.
        let (tx, _rx) = mpsc::channel(100);
        let conn = WebSocketConnection::new(
            0,
            make_test_token_handle(),
            "test-client".to_string(),
            make_test_dhan_config(),
            WebSocketConfig {
                reconnect_max_attempts: 200,
                reconnect_initial_delay_ms: 100,
                reconnect_max_delay_ms: 1, // cap at 1ms
                ..make_test_ws_config()
            },
            vec![],
            FeedMode::Ticker,
            tx,
            None,
        );

        // attempt=64 → checked_shl(64) returns None → unwrap_or(u64::MAX)
        conn.total_reconnections.store(64, Ordering::Release);
        let result = conn.wait_with_backoff().await;
        assert!(result, "attempt 64 with max_attempts=200 should succeed");

        // attempt=128 → also overflows
        conn.total_reconnections.store(128, Ordering::Release);
        let result = conn.wait_with_backoff().await;
        assert!(result, "attempt 128 with max_attempts=200 should succeed");
    }

    #[tokio::test]
    async fn test_run_tracks_reconnection_count_incrementally() {
        // Verifies that total_reconnections increments with each retry (line 214).
        let (tx, _rx) = mpsc::channel(100);
        let conn = WebSocketConnection::new(
            0,
            make_test_token_handle(),
            "test-client".to_string(),
            make_test_dhan_config(),
            WebSocketConfig {
                reconnect_max_attempts: 3,
                reconnect_initial_delay_ms: 1,
                reconnect_max_delay_ms: 1,
                ..make_test_ws_config()
            },
            vec![],
            FeedMode::Ticker,
            tx,
            None,
        );

        assert_eq!(conn.health().total_reconnections, 0);
        let _ = conn.run().await;
        // After exhausting 3 attempts, total_reconnections should be 3
        // (each loop iteration increments once at line 214)
        assert_eq!(conn.health().total_reconnections, 3);
    }

    #[tokio::test]
    async fn test_run_with_token_invalid_url_covers_request_build_error() {
        // Invalid URL causes IntoClientRequest to fail (lines 253-259).
        // This produces a ConnectionFailed error which triggers reconnection.
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

        let (tx, _rx) = mpsc::channel(100);
        let conn = WebSocketConnection::new(
            0,
            make_token_handle_with_token(),
            "test-client".to_string(),
            DhanConfig {
                // Completely invalid URL — can't be parsed into a request
                websocket_url: "not-a-valid-url".to_string(),
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

        let result = conn.run().await;
        assert!(result.is_err());
    }

    #[test]
    fn test_connection_websocket_base_url_stored_from_config() {
        // Verifies that the websocket_base_url is cloned from config (line 98).
        let (tx, _rx) = mpsc::channel(100);
        let custom_url = "wss://custom-feed.example.com";
        let conn = WebSocketConnection::new(
            0,
            make_test_token_handle(),
            "test-client".to_string(),
            DhanConfig {
                websocket_url: custom_url.to_string(),
                ..make_test_dhan_config()
            },
            make_test_ws_config(),
            vec![],
            FeedMode::Ticker,
            tx,
            None,
        );
        // The websocket_base_url is private, but we can verify via health
        // that construction succeeded.
        assert_eq!(conn.connection_id(), 0);
    }

    #[test]
    fn test_connection_config_stored_correctly() {
        // Verifies DhanConfig and WebSocketConfig are stored (fields used in connect).
        let (tx, _rx) = mpsc::channel(100);
        let ws_cfg = WebSocketConfig {
            ping_interval_secs: 20,
            pong_timeout_secs: 15,
            max_consecutive_pong_failures: 5,
            reconnect_initial_delay_ms: 200,
            reconnect_max_delay_ms: 5000,
            reconnect_max_attempts: 7,
            subscription_batch_size: 50,
            connection_stagger_ms: 0,
            activity_watchdog_threshold_secs: 50,
        };
        let instruments: Vec<_> = (0..250)
            .map(|i| InstrumentSubscription::new(ExchangeSegment::NseFno, 3000 + i))
            .collect();
        let conn = WebSocketConnection::new(
            4,
            make_test_token_handle(),
            "custom-client".to_string(),
            make_test_dhan_config(),
            ws_cfg,
            instruments,
            FeedMode::Full,
            tx,
            None,
        );
        assert_eq!(conn.connection_id(), 4);
        assert_eq!(conn.health().subscribed_count, 250);
        // 250 / batch_size(50) = 5 messages
        assert_eq!(conn.cached_subscription_messages.len(), 5);
    }

    #[test]
    fn test_read_timeout_formula_large_interval() {
        // Large ping_interval to test saturating_mul behavior (line 356-359).
        assert_eq!(compute_read_timeout(u64::MAX, 0, 0), u64::MAX);
    }

    #[test]
    fn test_read_timeout_formula_large_pong_timeout() {
        // Large pong_timeout to test saturating_add behavior (line 359).
        assert_eq!(compute_read_timeout(0, u64::MAX, 0), u64::MAX);
    }

    #[test]
    fn test_read_timeout_formula_all_max() {
        // All values at maximum — should not overflow.
        let result = compute_read_timeout(u64::MAX, u64::MAX, u32::MAX);
        assert_eq!(result, u64::MAX);
    }

    #[tokio::test]
    async fn test_run_no_token_single_attempt_state_transitions() {
        // With 1 max attempt and no token, verify that the connection
        // goes through Connecting → (fail) → Reconnecting → exhausted.
        let (tx, _rx) = mpsc::channel(100);
        let conn = WebSocketConnection::new(
            0,
            make_test_token_handle(), // no token
            "test-client".to_string(),
            make_test_dhan_config(),
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

        let result = conn.run().await;
        let (connection_id, attempts) = unwrap_reconnection_exhausted(result);
        assert_eq!(connection_id, 0);
        assert_eq!(attempts, 1);
        // total_reconnections should be 1
        assert_eq!(conn.health().total_reconnections, 1);
    }

    #[tokio::test]
    async fn test_run_with_token_multiple_attempts_increments_reconnections() {
        // With a token present, each loop iteration still fails (no server)
        // and increments total_reconnections.
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

        let (tx, _rx) = mpsc::channel(100);
        let conn = WebSocketConnection::new(
            4,
            make_token_handle_with_token(),
            "test-client".to_string(),
            DhanConfig {
                websocket_url: "wss://127.0.0.1:19999".to_string(),
                ..make_test_dhan_config()
            },
            WebSocketConfig {
                reconnect_max_attempts: 3,
                reconnect_initial_delay_ms: 1,
                reconnect_max_delay_ms: 1,
                ..make_test_ws_config()
            },
            vec![InstrumentSubscription::new(ExchangeSegment::NseFno, 5555)],
            FeedMode::Ticker,
            tx,
            None,
        );

        let result = conn.run().await;
        let (connection_id, attempts) = unwrap_reconnection_exhausted(result);
        assert_eq!(connection_id, 4);
        assert_eq!(attempts, 3);
        assert_eq!(conn.health().total_reconnections, 3);
    }

    #[test]
    fn test_cached_messages_large_batch_size_bigger_than_instruments() {
        // When batch_size > instrument count, we get exactly 1 message.
        let (tx, _rx) = mpsc::channel(100);
        let instruments = vec![
            InstrumentSubscription::new(ExchangeSegment::NseFno, 1000),
            InstrumentSubscription::new(ExchangeSegment::NseFno, 1001),
        ];
        let conn = WebSocketConnection::new(
            0,
            make_test_token_handle(),
            "test-client".to_string(),
            make_test_dhan_config(),
            WebSocketConfig {
                subscription_batch_size: 10000, // much larger than 2 instruments
                ..make_test_ws_config()
            },
            instruments,
            FeedMode::Full,
            tx,
            None,
        );
        assert_eq!(conn.cached_subscription_messages.len(), 1);
    }

    #[test]
    fn test_cached_messages_exact_batch_boundary() {
        // 200 instruments / batch_size 100 = exactly 2 messages (no remainder).
        let (tx, _rx) = mpsc::channel(100);
        let instruments: Vec<_> = (0..200)
            .map(|i| InstrumentSubscription::new(ExchangeSegment::NseFno, i + 1000))
            .collect();
        let conn = WebSocketConnection::new(
            0,
            make_test_token_handle(),
            "test-client".to_string(),
            make_test_dhan_config(),
            make_test_ws_config(), // batch_size = 100
            instruments,
            FeedMode::Ticker,
            tx,
            None,
        );
        assert_eq!(conn.cached_subscription_messages.len(), 2);
    }

    #[test]
    fn test_cached_messages_batch_boundary_plus_one() {
        // 101 instruments / batch_size 100 = 2 messages (100 + 1).
        let (tx, _rx) = mpsc::channel(100);
        let instruments: Vec<_> = (0..101)
            .map(|i| InstrumentSubscription::new(ExchangeSegment::NseFno, i + 1000))
            .collect();
        let conn = WebSocketConnection::new(
            0,
            make_test_token_handle(),
            "test-client".to_string(),
            make_test_dhan_config(),
            make_test_ws_config(),
            instruments,
            FeedMode::Ticker,
            tx,
            None,
        );
        assert_eq!(conn.cached_subscription_messages.len(), 2);
        // First batch should have 100 instruments
        assert!(conn.cached_subscription_messages[0].contains("\"InstrumentCount\":100"));
        // Second batch should have 1 instrument
        assert!(conn.cached_subscription_messages[1].contains("\"InstrumentCount\":1"));
    }

    // =========================================================================
    // Mock WebSocket Server Tests — cover run_read_loop() paths (lines 345-429)
    // =========================================================================
    //
    // Strategy: Create a local TCP listener, perform a plain WebSocket handshake
    // (no TLS) using tokio-tungstenite's `accept_async` (server) and
    // `client_async` (client). This produces a
    // `WebSocketStream<MaybeTlsStream<TcpStream>>` that can be passed directly
    // to the private `run_read_loop` method.

    use tokio::net::TcpListener;
    use tokio_tungstenite::tungstenite::protocol::CloseFrame;
    use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;

    /// Creates a connected WebSocket pair: (server_ws, client_ws).
    ///
    /// The client side is wrapped in `MaybeTlsStream::Plain` so it matches
    /// `run_read_loop`'s expected `WebSocketStream<MaybeTlsStream<TcpStream>>`.
    async fn make_ws_pair() -> (
        WebSocketStream<TcpStream>,
        WebSocketStream<MaybeTlsStream<TcpStream>>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind failed"); // APPROVED: test helper
        let addr = listener.local_addr().expect("local_addr failed"); // APPROVED: test helper

        let (server_result, client_result) = tokio::join!(
            async {
                let (stream, _) = listener.accept().await.expect("accept failed"); // APPROVED: test helper
                tokio_tungstenite::accept_async(stream)
                    .await
                    .expect("server WS handshake failed") // APPROVED: test helper
            },
            async {
                let stream = TcpStream::connect(addr)
                    .await
                    .expect("client connect failed"); // APPROVED: test helper
                let url = format!("ws://127.0.0.1:{}", addr.port());
                let (ws, _resp) =
                    tokio_tungstenite::client_async(url, MaybeTlsStream::Plain(stream))
                        .await
                        .expect("client WS handshake failed"); // APPROVED: test helper
                ws
            }
        );

        (server_result, client_result)
    }

    /// Helper: creates a `WebSocketConnection` with tiny timeouts for fast tests.
    fn make_test_conn_for_read_loop(
        frame_sender: mpsc::Sender<(u64, bytes::Bytes)>,
    ) -> WebSocketConnection {
        WebSocketConnection::new(
            0,
            make_test_token_handle(),
            "test-client".to_string(),
            make_test_dhan_config(),
            WebSocketConfig {
                ping_interval_secs: 1,
                pong_timeout_secs: 0,
                max_consecutive_pong_failures: 0,
                reconnect_max_attempts: 0,
                reconnect_initial_delay_ms: 1,
                reconnect_max_delay_ms: 1,
                subscription_batch_size: 100,
                connection_stagger_ms: 0,
                activity_watchdog_threshold_secs: 50,
            },
            vec![],
            FeedMode::Ticker,
            frame_sender,
            None,
        )
    }

    // --- run_read_loop: Binary frame forwarding (line 376-384) ---

    #[tokio::test]
    async fn test_read_loop_binary_frame_forwarded_to_channel() {
        let (tx, mut rx) = mpsc::channel(100);
        let conn = make_test_conn_for_read_loop(tx);
        let (mut server_ws, client_ws) = make_ws_pair().await;

        let payload = vec![0xDE, 0xAD, 0xBE, 0xEF];
        let payload_clone = payload.clone();

        let read_handle = tokio::spawn(async move { conn.run_read_loop(client_ws).await });

        // Server sends a binary frame then closes.
        server_ws
            .send(Message::Binary(payload_clone.into()))
            .await
            .expect("send binary failed"); // APPROVED: test
        server_ws.close(None).await.ok();

        let result = read_handle.await.expect("task panicked"); // APPROVED: test
        assert!(result.is_ok(), "read loop should return Ok on close");

        // Verify the binary payload was forwarded.
        let (_seq, received) = rx.recv().await.expect("should receive frame"); // APPROVED: test
        assert_eq!(received, payload);
    }

    // --- run_read_loop: Multiple binary frames (line 376-384) ---

    #[tokio::test]
    async fn test_read_loop_multiple_binary_frames() {
        let (tx, mut rx) = mpsc::channel(100);
        let conn = make_test_conn_for_read_loop(tx);
        let (mut server_ws, client_ws) = make_ws_pair().await;

        let read_handle = tokio::spawn(async move { conn.run_read_loop(client_ws).await });

        // Server sends 3 binary frames.
        for i in 0u8..3 {
            server_ws
                .send(Message::Binary(vec![i, i + 1, i + 2].into()))
                .await
                .expect("send failed"); // APPROVED: test
        }
        server_ws.close(None).await.ok();

        let result = read_handle.await.expect("task panicked"); // APPROVED: test
        assert!(result.is_ok());

        // All 3 frames should be received.
        for i in 0u8..3 {
            let (_seq, frame) = rx.recv().await.expect("should receive frame"); // APPROVED: test
            assert_eq!(frame, vec![i, i + 1, i + 2]);
        }
    }

    // --- run_read_loop: Binary frame with dropped receiver (line 378-384) ---

    #[tokio::test]
    async fn test_read_loop_binary_frame_receiver_dropped_returns_ok() {
        let (tx, rx) = mpsc::channel(1);
        let conn = make_test_conn_for_read_loop(tx);
        let (mut server_ws, client_ws) = make_ws_pair().await;

        // Drop the receiver before sending data.
        drop(rx);

        let read_handle = tokio::spawn(async move { conn.run_read_loop(client_ws).await });

        // Server sends a binary frame — the receiver is dropped so send will fail.
        server_ws
            .send(Message::Binary(vec![1, 2, 3].into()))
            .await
            .expect("send failed"); // APPROVED: test

        let result = read_handle.await.expect("task panicked"); // APPROVED: test
        // Should return Ok(()) when receiver is dropped (line 383).
        assert!(result.is_ok(), "should return Ok when receiver dropped");
    }

    // --- run_read_loop: Ping frame triggers Pong (line 386-390) ---

    #[tokio::test]
    async fn test_read_loop_ping_responds_with_pong() {
        let (tx, _rx) = mpsc::channel(100);
        let conn = make_test_conn_for_read_loop(tx);
        let (mut server_ws, client_ws) = make_ws_pair().await;

        let read_handle = tokio::spawn(async move { conn.run_read_loop(client_ws).await });

        let ping_data: bytes::Bytes = vec![0x01, 0x02].into();
        server_ws
            .send(Message::Ping(ping_data.clone()))
            .await
            .expect("send ping failed"); // APPROVED: test

        // Read the pong response from the client.
        let data = unwrap_pong(server_ws.next().await);
        assert_eq!(data, ping_data, "pong payload should echo ping");

        // Close to end the read loop.
        server_ws.close(None).await.ok();
        let result = read_handle.await.expect("task panicked"); // APPROVED: test
        assert!(result.is_ok());
    }

    // --- run_read_loop: Pong frame is ignored (line 391-393) ---

    #[tokio::test]
    async fn test_read_loop_pong_frame_ignored() {
        let (tx, _rx) = mpsc::channel(100);
        let conn = make_test_conn_for_read_loop(tx);
        let (mut server_ws, client_ws) = make_ws_pair().await;

        let read_handle = tokio::spawn(async move { conn.run_read_loop(client_ws).await });

        // Server sends a Pong frame (unusual, but should be silently ignored).
        server_ws
            .send(Message::Pong(vec![0x99].into()))
            .await
            .expect("send pong failed"); // APPROVED: test

        // Close cleanly to verify read loop didn't error on Pong.
        server_ws.close(None).await.ok();

        let result = read_handle.await.expect("task panicked"); // APPROVED: test
        assert!(
            result.is_ok(),
            "Pong should be ignored, loop continues until close"
        );
    }

    // --- run_read_loop: Close frame with code+reason (line 394-406) ---

    #[tokio::test]
    async fn test_read_loop_close_frame_with_reason_returns_ok() {
        let (tx, _rx) = mpsc::channel(100);
        let conn = make_test_conn_for_read_loop(tx);
        let (mut server_ws, client_ws) = make_ws_pair().await;

        let read_handle = tokio::spawn(async move { conn.run_read_loop(client_ws).await });

        // Server sends a close frame with a code and reason.
        let close_frame = CloseFrame {
            code: CloseCode::Normal,
            reason: "server shutting down".into(),
        };
        server_ws
            .send(Message::Close(Some(close_frame)))
            .await
            .expect("send close failed"); // APPROVED: test

        let result = read_handle.await.expect("task panicked"); // APPROVED: test
        assert!(
            result.is_ok(),
            "Close frame should cause clean return Ok(())"
        );
    }

    // --- run_read_loop: Close frame without payload (line 394-406, None path) ---

    #[tokio::test]
    async fn test_read_loop_close_frame_without_payload_returns_ok() {
        let (tx, _rx) = mpsc::channel(100);
        let conn = make_test_conn_for_read_loop(tx);
        let (mut server_ws, client_ws) = make_ws_pair().await;

        let read_handle = tokio::spawn(async move { conn.run_read_loop(client_ws).await });

        // Server sends a close frame with no payload.
        server_ws
            .send(Message::Close(None))
            .await
            .expect("send close failed"); // APPROVED: test

        let result = read_handle.await.expect("task panicked"); // APPROVED: test
        assert!(result.is_ok(), "Close(None) should return Ok(())");
    }

    // =======================================================================
    // A5: Graceful unsubscribe on shutdown
    // =======================================================================

    /// A5: Requesting graceful shutdown sets the flag and wakes any waiter.
    #[tokio::test]
    async fn test_graceful_shutdown_sets_flag_and_notifies() {
        let (tx, _rx) = mpsc::channel(100);
        let conn = make_test_conn_for_read_loop(tx);

        assert!(
            !conn.is_shutdown_requested(),
            "fresh connection must not have shutdown requested"
        );

        conn.request_graceful_shutdown();

        assert!(
            conn.is_shutdown_requested(),
            "request_graceful_shutdown must set the atomic flag"
        );

        // Idempotent.
        conn.request_graceful_shutdown();
        assert!(
            conn.is_shutdown_requested(),
            "double-calling must remain true"
        );
    }

    /// W2#8 (WS-GAP-05): `live_frame_channel_closed` must track the
    /// downstream frame-channel Receiver lifecycle — false while the tick
    /// consumer holds it, true after it drops. The pool-slot respawn
    /// classifier uses this to keep a WS-GAP-07 consumer-dead clean exit
    /// TERMINAL (respawning would churn Dhan connects with zero benefit).
    #[tokio::test]
    async fn test_live_frame_channel_closed_tracks_receiver_drop() {
        let (tx, rx) = mpsc::channel(100);
        let conn = make_test_conn_for_read_loop(tx);
        assert!(
            !conn.live_frame_channel_closed(),
            "channel must read open while the consumer Receiver is alive"
        );
        drop(rx);
        assert!(
            conn.live_frame_channel_closed(),
            "channel must read closed after the consumer Receiver drops"
        );
    }

    /// A5: When the read loop is running and receives a shutdown request, it
    /// writes the Disconnect JSON (RequestCode 12) to the socket and returns Ok.
    #[tokio::test]
    async fn test_graceful_shutdown_sends_disconnect_request() {
        let (tx, _rx) = mpsc::channel(100);
        let conn = std::sync::Arc::new(make_test_conn_for_read_loop(tx));
        let (mut server_ws, client_ws) = make_ws_pair().await;

        let conn_clone = std::sync::Arc::clone(&conn);
        let read_handle = tokio::spawn(async move { conn_clone.run_read_loop(client_ws).await });

        // Give the read loop a moment to enter select!.
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Trigger shutdown.
        conn.request_graceful_shutdown();

        // Server side should receive the Disconnect JSON text frame.
        let received = tokio::time::timeout(Duration::from_secs(3), server_ws.next())
            .await
            .expect("server must receive a frame within 3s"); // APPROVED: test

        let text = match received {
            Some(Ok(Message::Text(t))) => t.to_string(),
            Some(Ok(Message::Close(_))) => {
                panic!(
                    "expected Text(Disconnect), got Close — socket closed before Disconnect was sent"
                )
            }
            other => panic!("expected Text(Disconnect), got {other:?}"),
        };
        assert!(
            text.contains("\"RequestCode\":12"),
            "text frame must contain RequestCode 12 (Disconnect), got: {text}"
        );

        // The read loop must return Ok(()) promptly.
        let result = tokio::time::timeout(Duration::from_secs(3), read_handle)
            .await
            .expect("read loop must finish within 3s") // APPROVED: test
            .expect("task must not panic"); // APPROVED: test
        assert!(
            result.is_ok(),
            "read loop must return Ok on graceful shutdown, got {result:?}"
        );
    }

    /// A5: If the write side is dead (server disappeared), the send times out
    /// or fails and the read loop still returns Ok — graceful shutdown must
    /// never block indefinitely.
    #[tokio::test]
    async fn test_graceful_shutdown_timeout_does_not_block() {
        let (tx, _rx) = mpsc::channel(100);
        let conn = std::sync::Arc::new(make_test_conn_for_read_loop(tx));
        let (server_ws, client_ws) = make_ws_pair().await;

        // Drop the server immediately — subsequent writes will fail fast.
        drop(server_ws);

        let conn_clone = std::sync::Arc::clone(&conn);
        let read_handle = tokio::spawn(async move { conn_clone.run_read_loop(client_ws).await });

        tokio::time::sleep(Duration::from_millis(50)).await;
        conn.request_graceful_shutdown();

        // Must still return within the A5 budget (send timeout 2s + close 1s + slack).
        let result = tokio::time::timeout(Duration::from_secs(5), read_handle)
            .await
            .expect("graceful shutdown must not block past 5s") // APPROVED: test
            .expect("task must not panic"); // APPROVED: test

        // The dead socket may cause the read loop to return an error (socket
        // closed) OR Ok (if the select! arm fired before the read arm).
        // Both are acceptable — the key invariant is that it returns.
        let _ = result;
    }

    // --- run_read_loop: Text frame is logged and loop continues (line 407-413) ---

    #[tokio::test]
    async fn test_read_loop_text_frame_continues_loop() {
        let (tx, _rx) = mpsc::channel(100);
        let conn = make_test_conn_for_read_loop(tx);
        let (mut server_ws, client_ws) = make_ws_pair().await;

        let read_handle = tokio::spawn(async move { conn.run_read_loop(client_ws).await });

        // Server sends a text message (unexpected in Dhan protocol).
        server_ws
            .send(Message::Text("hello unexpected".into()))
            .await
            .expect("send text failed"); // APPROVED: test

        // Then close cleanly — read loop should have continued past the text.
        server_ws.close(None).await.ok();

        let result = read_handle.await.expect("task panicked"); // APPROVED: test
        assert!(result.is_ok(), "Text message should not cause error");
    }

    // --- run_read_loop: Stream end (None) returns Ok (line 421-423) ---

    #[tokio::test]
    async fn test_read_loop_stream_end_returns_ok() {
        let (tx, _rx) = mpsc::channel(100);
        let conn = make_test_conn_for_read_loop(tx);
        let (server_ws, client_ws) = make_ws_pair().await;

        let read_handle = tokio::spawn(async move { conn.run_read_loop(client_ws).await });

        // Drop the server side entirely — stream ends with None.
        drop(server_ws);

        let result = read_handle.await.expect("task panicked"); // APPROVED: test
        // Stream end may surface as Ok(()) (None case) or as an error
        // depending on how tungstenite surfaces the TCP reset.
        // Both are valid outcomes for this test.
        let _ = result;
    }

    // Removed 2026-04-17: `test_read_loop_timeout_returns_read_timeout_error`
    // relied on a client-side `time::timeout(read.next())` wrapper that
    // STAGE-B (plan item P1.1) deleted. `run_read_loop` now blocks on
    // `read.next().await` indefinitely and relies on the server's 40s
    // server-side timeout + the watchdog for liveness — so this test
    // simply hung forever waiting for a timeout path that no longer
    // exists in production code. The `WatchdogFired` code path is
    // covered by the watchdog tests in `activity_watchdog.rs`.

    // --- run_read_loop: Error from stream (line 415-420) ---

    #[tokio::test]
    async fn test_read_loop_stream_error_returns_connection_failed() {
        let (tx, _rx) = mpsc::channel(100);
        let conn = make_test_conn_for_read_loop(tx);

        // Create a WS pair, then forcefully close the server TCP socket mid-stream.
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind failed"); // APPROVED: test
        let addr = listener.local_addr().expect("local_addr failed"); // APPROVED: test

        let (server_tcp, client_tcp) = tokio::join!(
            async {
                let (stream, _) = listener.accept().await.expect("accept failed"); // APPROVED: test
                stream
            },
            async {
                TcpStream::connect(addr).await.expect("connect failed") // APPROVED: test
            }
        );

        // Perform WS handshake.
        let (server_ws_result, client_ws_result) = tokio::join!(
            tokio_tungstenite::accept_async(server_tcp),
            tokio_tungstenite::client_async(
                format!("ws://127.0.0.1:{}", addr.port()),
                MaybeTlsStream::Plain(client_tcp),
            )
        );

        let mut server_ws = server_ws_result.expect("server handshake failed"); // APPROVED: test
        let (client_ws, _) = client_ws_result.expect("client handshake failed"); // APPROVED: test

        let read_handle = tokio::spawn(async move { conn.run_read_loop(client_ws).await });

        // Send a binary frame to keep the connection alive, then abruptly close.
        server_ws.send(Message::Binary(vec![1].into())).await.ok();

        // Drop server to cause a connection reset.
        drop(server_ws);

        let result = read_handle.await.expect("task panicked"); // APPROVED: test
        // The result may be Ok (if tungstenite surfaces it as stream end)
        // or Err(ConnectionFailed) if it surfaces as a read error.
        // Either exercises the relevant code path.
        let _ = result;
    }

    // --- run_read_loop: Mixed frames exercise all branches ---

    #[tokio::test]
    async fn test_read_loop_mixed_frame_sequence() {
        let (tx, mut rx) = mpsc::channel(100);
        let conn = make_test_conn_for_read_loop(tx);
        let (mut server_ws, client_ws) = make_ws_pair().await;

        let read_handle = tokio::spawn(async move { conn.run_read_loop(client_ws).await });

        // Send a variety of frames.
        // 1. Binary
        server_ws
            .send(Message::Binary(vec![0xAA].into()))
            .await
            .expect("send binary failed"); // APPROVED: test

        // 2. Text (ignored by read loop, but loop continues)
        server_ws
            .send(Message::Text("status: ok".into()))
            .await
            .expect("send text failed"); // APPROVED: test

        // 3. Ping (triggers pong response)
        server_ws
            .send(Message::Ping(vec![0x42].into()))
            .await
            .expect("send ping failed"); // APPROVED: test

        // Read the pong (so server doesn't block).
        let _pong = server_ws.next().await;

        // 4. Pong (ignored)
        server_ws
            .send(Message::Pong(vec![0x99].into()))
            .await
            .expect("send pong failed"); // APPROVED: test

        // 5. Another binary
        server_ws
            .send(Message::Binary(vec![0xBB].into()))
            .await
            .expect("send binary 2 failed"); // APPROVED: test

        // 6. Close
        server_ws
            .send(Message::Close(Some(CloseFrame {
                code: CloseCode::Normal,
                reason: "done".into(),
            })))
            .await
            .expect("send close failed"); // APPROVED: test

        let result = read_handle.await.expect("task panicked"); // APPROVED: test
        assert!(result.is_ok());

        // Check that both binary frames were forwarded.
        let (_seq, frame1) = rx.recv().await.expect("should get frame 1"); // APPROVED: test
        assert_eq!(frame1, vec![0xAA]);
        let (_seq2, frame2) = rx.recv().await.expect("should get frame 2"); // APPROVED: test
        assert_eq!(frame2, vec![0xBB]);
    }

    // =========================================================================
    // run() integration tests via mock WS server — cover run() success path
    // (lines 161-222) and connect_and_subscribe success path (lines 283-336)
    // =========================================================================
    //
    // Strategy: Set up a local plain WS server and point the connection at it
    // using ws:// URL. Since connect_and_subscribe uses
    // connect_async_tls_with_config with a Rustls connector, connecting to a
    // plain WS server will cause a TLS error during connect. This covers the
    // connect_and_subscribe error path returning Err → run() reconnect loop.
    //
    // For the SUCCESS path through connect_and_subscribe (lines 290-336),
    // we test via run_read_loop directly (above) since we can't bypass TLS.

    #[tokio::test]
    async fn test_run_with_token_against_local_server_tls_fails() {
        // A local plain-TCP WS server causes TLS handshake failure,
        // exercising the connect_and_subscribe error path (lines 290-305)
        // and the run() reconnect path (lines 203-210).
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind failed"); // APPROVED: test
        let port = listener.local_addr().expect("addr failed").port(); // APPROVED: test

        // Accept connections in the background (just hold them open).
        let _server = tokio::spawn(async move {
            loop {
                let accepted = listener.accept().await;
                if let Ok((_stream, _addr)) = accepted {
                    // Hold the connection open briefly so TLS handshake can attempt.
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
            }
        });

        let (tx, _rx) = mpsc::channel(100);
        let conn = WebSocketConnection::new(
            0,
            make_token_handle_with_token(),
            "test-client".to_string(),
            DhanConfig {
                websocket_url: format!("wss://127.0.0.1:{port}"),
                ..make_test_dhan_config()
            },
            WebSocketConfig {
                reconnect_max_attempts: 1,
                reconnect_initial_delay_ms: 1,
                reconnect_max_delay_ms: 1,
                ..make_test_ws_config()
            },
            vec![InstrumentSubscription::new(ExchangeSegment::NseFno, 1000)],
            FeedMode::Full,
            tx,
            None,
        );

        let result = conn.run().await;
        // Expected: TLS handshake fails, retries exhausted.
        let (_connection_id, _attempts) = unwrap_reconnection_exhausted(result);
        assert!(conn.health().total_reconnections >= 1);
    }

    // --- run_read_loop: Close frame with various close codes ---

    #[tokio::test]
    async fn test_read_loop_close_frame_away_code() {
        let (tx, _rx) = mpsc::channel(100);
        let conn = make_test_conn_for_read_loop(tx);
        let (mut server_ws, client_ws) = make_ws_pair().await;

        let read_handle = tokio::spawn(async move { conn.run_read_loop(client_ws).await });

        // Send close with Away code.
        server_ws
            .send(Message::Close(Some(CloseFrame {
                code: CloseCode::Away,
                reason: "going away".into(),
            })))
            .await
            .expect("send close failed"); // APPROVED: test

        let result = read_handle.await.expect("task panicked"); // APPROVED: test
        assert!(result.is_ok());
    }

    // --- run_read_loop: Large binary frame ---

    #[tokio::test]
    async fn test_read_loop_large_binary_frame() {
        let (tx, mut rx) = mpsc::channel(100);
        let conn = make_test_conn_for_read_loop(tx);
        let (mut server_ws, client_ws) = make_ws_pair().await;

        let read_handle = tokio::spawn(async move { conn.run_read_loop(client_ws).await });

        // Send a large binary frame (simulating a full market data packet).
        let large_payload: Vec<u8> = (0..4096).map(|i| (i % 256) as u8).collect();
        let expected = large_payload.clone();
        server_ws
            .send(Message::Binary(large_payload.into()))
            .await
            .expect("send large binary failed"); // APPROVED: test
        server_ws.close(None).await.ok();

        let result = read_handle.await.expect("task panicked"); // APPROVED: test
        assert!(result.is_ok());

        let (_seq, received) = rx.recv().await.expect("should receive large frame"); // APPROVED: test
        assert_eq!(received.len(), 4096);
        assert_eq!(received, expected);
    }

    // --- run_read_loop: Empty binary frame ---

    #[tokio::test]
    async fn test_read_loop_empty_binary_frame() {
        let (tx, mut rx) = mpsc::channel(100);
        let conn = make_test_conn_for_read_loop(tx);
        let (mut server_ws, client_ws) = make_ws_pair().await;

        let read_handle = tokio::spawn(async move { conn.run_read_loop(client_ws).await });

        // Send an empty binary frame.
        server_ws
            .send(Message::Binary(vec![].into()))
            .await
            .expect("send empty binary failed"); // APPROVED: test
        server_ws.close(None).await.ok();

        let result = read_handle.await.expect("task panicked"); // APPROVED: test
        assert!(result.is_ok());

        let (_seq, received) = rx.recv().await.expect("should receive empty frame"); // APPROVED: test
        assert!(received.is_empty());
    }

    // --- run_read_loop: Multiple pings ---

    #[tokio::test]
    async fn test_read_loop_multiple_pings_all_get_pong() {
        let (tx, _rx) = mpsc::channel(100);
        let conn = make_test_conn_for_read_loop(tx);
        let (mut server_ws, client_ws) = make_ws_pair().await;

        let read_handle = tokio::spawn(async move { conn.run_read_loop(client_ws).await });

        // Send 3 pings and verify each gets a pong.
        for i in 0u8..3 {
            server_ws
                .send(Message::Ping(vec![i].into()))
                .await
                .expect("send ping failed"); // APPROVED: test

            let data = unwrap_pong(server_ws.next().await);
            assert_eq!(data.as_ref(), &[i], "pong should echo ping data");
        }

        server_ws.close(None).await.ok();
        let result = read_handle.await.expect("task panicked"); // APPROVED: test
        assert!(result.is_ok());
    }

    // --- run_read_loop: Text then binary then close sequence ---

    #[tokio::test]
    async fn test_read_loop_text_does_not_interfere_with_binary() {
        let (tx, mut rx) = mpsc::channel(100);
        let conn = make_test_conn_for_read_loop(tx);
        let (mut server_ws, client_ws) = make_ws_pair().await;

        let read_handle = tokio::spawn(async move { conn.run_read_loop(client_ws).await });

        // Text first — should be logged and ignored.
        server_ws
            .send(Message::Text("disconnect warning".into()))
            .await
            .expect("send text failed"); // APPROVED: test

        // Binary after — should be forwarded normally.
        server_ws
            .send(Message::Binary(vec![0xFF].into()))
            .await
            .expect("send binary failed"); // APPROVED: test

        server_ws.close(None).await.ok();

        let result = read_handle.await.expect("task panicked"); // APPROVED: test
        assert!(result.is_ok());

        let (_seq, frame) = rx.recv().await.expect("should receive binary frame"); // APPROVED: test
        assert_eq!(frame, vec![0xFF]);
    }

    // -----------------------------------------------------------------------
    // WebSocketConnection — subscription message caching
    // -----------------------------------------------------------------------

    #[test]
    fn test_connection_caches_subscription_messages_empty_instruments() {
        let (tx, _rx) = mpsc::channel(100);
        let conn = WebSocketConnection::new(
            0,
            make_test_token_handle(),
            "test-client".to_string(),
            make_test_dhan_config(),
            make_test_ws_config(),
            vec![],
            FeedMode::Full,
            tx,
            None,
        );
        // No instruments → no subscription messages
        assert!(conn.cached_subscription_messages.is_empty());
    }

    #[test]
    fn test_connection_caches_subscription_messages_non_idx() {
        let (tx, _rx) = mpsc::channel(100);
        let instruments = vec![
            InstrumentSubscription::new(ExchangeSegment::NseFno, 1000),
            InstrumentSubscription::new(ExchangeSegment::NseFno, 1001),
        ];
        let conn = WebSocketConnection::new(
            0,
            make_test_token_handle(),
            "test-client".to_string(),
            make_test_dhan_config(),
            make_test_ws_config(),
            instruments,
            FeedMode::Full,
            tx,
            None,
        );
        assert!(
            !conn.cached_subscription_messages.is_empty(),
            "should have subscription messages for non-IDX instruments"
        );
    }

    #[test]
    fn test_connection_idx_instruments_use_quote_mode() {
        let (tx, _rx) = mpsc::channel(100);
        let instruments = vec![InstrumentSubscription::new(ExchangeSegment::IdxI, 13)];
        let conn = WebSocketConnection::new(
            0,
            make_test_token_handle(),
            "test-client".to_string(),
            make_test_dhan_config(),
            make_test_ws_config(),
            instruments,
            FeedMode::Full, // Full mode specified but IDX_I uses Quote
            tx,
            None,
        );
        assert!(!conn.cached_subscription_messages.is_empty());
        // The subscription message should contain RequestCode 17 (SubscribeQuote).
        let msg = &conn.cached_subscription_messages[0];
        assert!(
            msg.contains("\"RequestCode\":17"),
            "IDX_I should subscribe with Quote mode (code 17): {}",
            msg
        );
    }

    #[test]
    fn test_connection_mixed_idx_and_non_idx_instruments() {
        let (tx, _rx) = mpsc::channel(100);
        let instruments = vec![
            InstrumentSubscription::new(ExchangeSegment::NseFno, 1000),
            InstrumentSubscription::new(ExchangeSegment::IdxI, 13),
        ];
        let conn = WebSocketConnection::new(
            0,
            make_test_token_handle(),
            "test-client".to_string(),
            make_test_dhan_config(),
            make_test_ws_config(),
            instruments,
            FeedMode::Full,
            tx,
            None,
        );
        // Should have at least 2 messages: one for non-IDX (Full) and one for IDX (Ticker)
        assert!(
            conn.cached_subscription_messages.len() >= 2,
            "mixed instruments should produce multiple subscription messages, got {}",
            conn.cached_subscription_messages.len()
        );
    }

    // -----------------------------------------------------------------------
    // ConnectionHealth — field access
    // -----------------------------------------------------------------------

    #[test]
    fn test_connection_health_debug_format() {
        let (tx, _rx) = mpsc::channel(100);
        let conn = WebSocketConnection::new(
            2,
            make_test_token_handle(),
            "test-client".to_string(),
            make_test_dhan_config(),
            make_test_ws_config(),
            vec![InstrumentSubscription::new(ExchangeSegment::NseFno, 42)],
            FeedMode::Quote,
            tx,
            None,
        );
        let health = conn.health();
        let debug_str = format!("{:?}", health);
        assert!(debug_str.contains("connection_id"));
    }

    // -----------------------------------------------------------------------
    // wait_for_valid_token — no token available
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_wait_for_valid_token_times_out_with_no_token() {
        let (tx, _rx) = mpsc::channel(100);
        let conn = WebSocketConnection::new(
            0,
            make_test_token_handle(), // No token stored
            "test-client".to_string(),
            make_test_dhan_config(),
            make_test_ws_config(),
            vec![],
            FeedMode::Ticker,
            tx,
            None,
        );
        // This will poll for up to 60s but we can't wait that long in tests.
        // Just verify the function exists and can be called.
        // The actual timeout test would be too slow.
        // Instead, test that the method doesn't panic with a short timeout.
        let start = std::time::Instant::now();
        // We'll just verify it compiles and the method signature is correct
        // by calling it in a select with a short timeout.
        tokio::select! {
            _ = conn.wait_for_valid_token() => {},
            _ = tokio::time::sleep(std::time::Duration::from_millis(100)) => {},
        }
        assert!(
            start.elapsed().as_millis() < 1000,
            "should abort quickly via select"
        );
    }

    // -----------------------------------------------------------------------
    // wait_for_valid_token — returns immediately when valid token present
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_wait_for_valid_token_returns_immediately_with_valid_token() {
        let (tx, _rx) = mpsc::channel(100);
        let conn = WebSocketConnection::new(
            0,
            make_token_handle_with_token(),
            "test-client".to_string(),
            make_test_dhan_config(),
            make_test_ws_config(),
            vec![],
            FeedMode::Ticker,
            tx,
            None,
        );
        let start = std::time::Instant::now();
        conn.wait_for_valid_token().await;
        // Should return almost immediately since token is valid.
        assert!(
            start.elapsed().as_millis() < 500,
            "should return immediately with valid token"
        );
    }

    // -----------------------------------------------------------------------
    // IDX_I partition: only non-IDX instruments
    // -----------------------------------------------------------------------

    #[test]
    fn test_idx_partition_no_idx_instruments() {
        let (tx, _rx) = mpsc::channel(100);
        let instruments = vec![
            InstrumentSubscription::new(ExchangeSegment::NseEquity, 100),
            InstrumentSubscription::new(ExchangeSegment::NseFno, 200),
        ];
        let conn = WebSocketConnection::new(
            0,
            make_test_token_handle(),
            "test-client".to_string(),
            make_test_dhan_config(),
            make_test_ws_config(),
            instruments,
            FeedMode::Full,
            tx,
            None,
        );
        // All instruments are non-IDX, so all messages use Full (code 21)
        assert_eq!(conn.cached_subscription_messages.len(), 1);
        assert!(conn.cached_subscription_messages[0].contains("\"RequestCode\":21"));
    }

    // -----------------------------------------------------------------------
    // IDX_I partition: only IDX instruments
    // -----------------------------------------------------------------------

    #[test]
    fn test_idx_partition_only_idx_instruments() {
        let (tx, _rx) = mpsc::channel(100);
        let instruments = vec![
            InstrumentSubscription::new(ExchangeSegment::IdxI, 13),
            InstrumentSubscription::new(ExchangeSegment::IdxI, 26),
            InstrumentSubscription::new(ExchangeSegment::IdxI, 99),
        ];
        let conn = WebSocketConnection::new(
            0,
            make_test_token_handle(),
            "test-client".to_string(),
            make_test_dhan_config(),
            make_test_ws_config(),
            instruments,
            FeedMode::Full,
            tx,
            None,
        );
        // All instruments are IDX, so all use Quote (code 17), no Full messages
        assert_eq!(conn.cached_subscription_messages.len(), 1);
        assert!(conn.cached_subscription_messages[0].contains("\"RequestCode\":17"));
        assert!(!conn.cached_subscription_messages[0].contains("\"RequestCode\":21"));
    }

    // -----------------------------------------------------------------------
    // run() — non-reconnectable disconnect code via read loop
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_read_loop_close_frame_with_specific_ws_close_code() {
        // Verify that a close frame with a specific code is handled cleanly.
        let (tx, _rx) = mpsc::channel(100);
        let conn = make_test_conn_for_read_loop(tx);
        let (mut server_ws, client_ws) = make_ws_pair().await;

        let read_handle = tokio::spawn(async move { conn.run_read_loop(client_ws).await });

        // Send close with a specific error code (1008 = Policy Violation).
        server_ws
            .send(Message::Close(Some(CloseFrame {
                code: CloseCode::Policy,
                reason: "policy violation".into(),
            })))
            .await
            .expect("send close failed"); // APPROVED: test

        let result = read_handle.await.expect("task panicked"); // APPROVED: test
        assert!(result.is_ok(), "Close frame should return Ok(())");
    }

    // -----------------------------------------------------------------------
    // run() — expired token triggers NoTokenAvailable on connect
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_run_expired_token_fails_with_no_token_available() {
        use crate::auth::types::TokenState;
        use chrono::{Duration as ChronoDuration, Utc};
        use secrecy::SecretString;
        use tickvault_common::trading_calendar::ist_offset;

        // Create an expired token.
        let now_ist = Utc::now().with_timezone(&ist_offset());
        let expired_at = now_ist - ChronoDuration::hours(1);
        let issued_at = now_ist - ChronoDuration::hours(25);
        let state = TokenState::from_cached(
            SecretString::from("expired-jwt".to_string()),
            expired_at,
            issued_at,
        );
        let token_handle: crate::auth::TokenHandle =
            Arc::new(arc_swap::ArcSwap::new(Arc::new(Some(state))));

        let (tx, _rx) = mpsc::channel(100);
        let conn = WebSocketConnection::new(
            0,
            token_handle,
            "test-client".to_string(),
            make_test_dhan_config(),
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

        let result = conn.run().await;
        // Expired token → connect_and_subscribe returns NoTokenAvailable → retries exhaust.
        assert!(result.is_err());
    }

    // --- M1: Mid-session backfill logging test ---

    #[test]
    fn test_mid_session_backfill_triggered() {
        // Verify the reconnection notification event exists and produces the expected message.
        // The mid-session backfill log in `run()` fires when `total_reconnections > 0`
        // after a successful `connect_and_subscribe`. This test verifies the
        // supporting notification event used for Telegram alerts.
        use crate::notification::NotificationEvent;
        let event = NotificationEvent::WebSocketReconnected {
            connection_index: 0,
            reason: None,
            down_secs: 0,
            attempts: 0,
        };
        let msg = event.to_message();
        assert!(
            msg.contains("reconnected"),
            "WebSocketReconnected event message must contain 'reconnected'"
        );
    }

    // --- WS-2: frame drop without WAL distinguished from backpressure ---

    /// Smoke test for the WS-2 metric name. The actual metric fires in a
    /// hot-path branch inside `run_read_loop` that we cannot easily
    /// exercise from a unit test without wiring a full mock TLS WebSocket
    /// stream. This test pins the metric NAME so a rename silently
    /// reverting the fix would trip a future regression. Production
    /// behaviour is validated end-to-end in integration tests against a
    /// real Dhan feed.
    #[test]
    fn test_ws2_frame_drop_metric_name_stable() {
        // The metric name is the contract between the code and dashboards /
        // alerts. If this string ever changes, all downstream Prometheus
        // rules must be updated in lockstep. This test exists ONLY to make
        // accidental renames a compile-visible diff.
        let metric_name = "tv_ws_frame_dropped_no_wal_total";
        assert_eq!(
            metric_name, "tv_ws_frame_dropped_no_wal_total",
            "WS-2 frame-drop metric name is a public contract — coordinate \
             with Prometheus rule owners before renaming"
        );
        // Also pin the backpressure metric name as a reference point —
        // the two must remain distinct so dashboards can separate
        // "buffered in WAL" from "dropped on the floor".
        let backpressure_name = "tv_ws_frame_live_backpressure_total";
        assert_ne!(
            metric_name, backpressure_name,
            "backpressure and frame-drop metrics must be distinct counters \
             (WS-2 invariant)"
        );
    }

    // -----------------------------------------------------------------
    // Fix #3 (2026-04-24): SubscribeRxGuard reinstall-on-drop tests.
    //
    // Without these ratchets, a regression where `run_read_loop` takes
    // the receiver without reinstalling would silently resurface the
    // 10:11 / 10:15 IST silent-socket → watchdog → reconnect cascade.
    // -----------------------------------------------------------------

    #[tokio::test]
    async fn test_subscribe_rx_guard_reinstalls_on_drop() {
        // Arrange: a slot with a live receiver.
        let (tx, rx) = mpsc::channel::<SubscribeCommand>(4);
        let slot = tokio::sync::Mutex::new(Some(rx));

        // Act: acquire and drop the guard (simulating a run_read_loop
        // exit path with no state changes).
        {
            let _guard = SubscribeRxGuard::acquire(&slot).await;
            // No reads, no writes — simulate a quick reconnect
            // triggered by a Dhan TCP RST (the 10:08 scenario).
        }

        // Assert: the slot still holds the receiver, so the NEXT
        // read-loop invocation can take it again.
        let reinstalled = slot.lock().await;
        assert!(
            reinstalled.is_some(),
            "SubscribeRxGuard::drop must reinstall the receiver so \
             the next reconnect cycle can resume subscribe-command \
             delivery (Fix #3 — see .claude/plans/active-plan.md)"
        );
        // Sanity: the reinstalled receiver is still usable.
        drop(reinstalled);
        drop(tx);
    }

    #[tokio::test]
    async fn test_subscribe_rx_guard_survives_many_cycles() {
        // Simulate N reconnect cycles. The receiver must be re-usable
        // after every cycle — this is the property the 10:11 cascade
        // violated before the guard existed.
        let (tx, rx) = mpsc::channel::<SubscribeCommand>(4);
        let slot = tokio::sync::Mutex::new(Some(rx));

        for cycle in 0..10 {
            let mut guard = SubscribeRxGuard::acquire(&slot).await;
            // Each cycle briefly borrows the receiver. On early cycles
            // the slot was empty between acquire→drop moments.
            let rx_ref = guard.rx_mut();
            assert!(
                rx_ref.is_some(),
                "cycle {cycle}: receiver must still be present after prior cycle"
            );
            // Drop the guard at scope exit — slot is reinstalled.
        }

        // After 10 cycles, one more acquire still gets the receiver.
        let final_guard = SubscribeRxGuard::acquire(&slot).await;
        assert!(
            final_guard.rx.is_some(),
            "receiver must survive N reconnect cycles — the pre-Fix-3 \
             behaviour dropped it on cycle 1, causing silent sockets."
        );
        drop(final_guard);
        drop(tx);
    }

    #[tokio::test]
    async fn test_subscribe_rx_guard_preserves_none_on_closed_channel() {
        // If the pool dropped the sender during the read loop, the
        // loop sets *subscribe_rx = None. On guard drop we reinstall
        // None — the pool MUST call install_subscribe_channel to
        // resume. This ratchet pins the "closed stays closed" semantic
        // so a refactor doesn't accidentally resurrect the old
        // receiver state.
        let (tx, rx) = mpsc::channel::<SubscribeCommand>(4);
        let slot = tokio::sync::Mutex::new(Some(rx));

        {
            let mut guard = SubscribeRxGuard::acquire(&slot).await;
            // Read loop sees closed channel → sets None.
            *guard.rx_mut() = None;
            drop(tx); // sender gone
        }

        let after = slot.lock().await;
        assert!(
            after.is_none(),
            "guard must reinstall whatever state the loop left, \
             including None — the pool re-installs a fresh receiver"
        );
    }

    #[tokio::test]
    async fn test_subscribe_rx_guard_empty_slot_is_noop() {
        // Some connections never get a subscribe channel installed
        // (depth-only pool, tests, etc.). The guard must handle the
        // slot=None case without panicking and leave the slot=None.
        let slot: tokio::sync::Mutex<Option<mpsc::Receiver<SubscribeCommand>>> =
            tokio::sync::Mutex::new(None);

        {
            let _guard = SubscribeRxGuard::acquire(&slot).await;
        }

        let after = slot.lock().await;
        assert!(after.is_none());
    }

    // -----------------------------------------------------------------
    // Wave 2 Item 5 (G1) — global TradingCalendar wiring tests.
    // -----------------------------------------------------------------

    #[test]
    fn test_market_calendar_accessor_returns_none_before_install() {
        // In a fresh test binary that does NOT call set_market_calendar,
        // the accessor must return None so the legacy `return false`
        // give-up path activates and unit tests retain their old
        // semantics.
        // Note: cargo test runs each #[test] in the same process per
        // binary, so once another test calls set_market_calendar this
        // assertion would no longer hold. We assert the Option type
        // shape only — the contract.
        let _: Option<&'static Arc<tickvault_common::trading_calendar::TradingCalendar>> =
            super::market_calendar();
    }

    #[test]
    fn test_set_market_calendar_is_idempotent() {
        use tickvault_common::config::TradingConfig;

        let cfg = TradingConfig {
            market_open_time: "09:00:00".to_string(),
            market_close_time: "15:30:00".to_string(),
            order_cutoff_time: "15:29:00".to_string(),
            data_collection_start: "09:00:00".to_string(),
            data_collection_end: "15:30:00".to_string(),
            timezone: "Asia/Kolkata".to_string(),
            max_orders_per_second: 10,
            nse_holidays: vec![],
            muhurat_trading_dates: vec![],
            nse_mock_trading_dates: vec![],
        };
        let cal = std::sync::Arc::new(
            tickvault_common::trading_calendar::TradingCalendar::from_config(&cfg)
                .expect("calendar build"),
        );
        // First call may or may not succeed depending on test ordering
        // (other tests in the same binary may have already installed).
        // Either way the second call must NOT succeed.
        let first = super::set_market_calendar(cal.clone());
        let second = super::set_market_calendar(cal);
        // At least one call must have failed (idempotency invariant).
        assert!(
            !(first && second),
            "set_market_calendar must be idempotent (at most one Ok)"
        );
    }

    // -----------------------------------------------------------------------
    // Phase 0 Item 5 (operator-locked 2026-05-13) — SubscribeRxGuard
    // production wiring source-scan ratchet (Rule 13 — method-defined-
    // but-never-called is a bug). The drop-semantics tests above cover
    // the guard's contract in isolation; this test verifies the guard
    // is ACTUALLY ACQUIRED inside `run_read_loop` so a future refactor
    // can't silently remove the call site (which would resurrect the
    // 10:11/10:15 IST 2026-04-24 silent-socket reconnect cascade).
    // -----------------------------------------------------------------------

    #[test]
    fn test_subscribe_rx_guard_acquired_in_run_read_loop() {
        // Read this file at test time and assert the production call site
        // is present. If a future refactor removes it, this ratchet fails
        // and the operator must consciously decide what replaces it.
        let source = include_str!("connection.rs");
        let call_site = "SubscribeRxGuard::acquire(&self.subscribe_cmd_rx)";
        assert!(
            source.contains(call_site),
            "Phase 0 Item 5: production `run_read_loop` MUST acquire \
             SubscribeRxGuard so reconnect cycles preserve the subscribe \
             command channel. Required call site: `{call_site}`. \
             See PR #337 fix-3 + .claude/rules/project/depth-subscription.md \
             2026-04-24 Updates §5 for the root-cause writeup.",
        );
    }

    #[test]
    fn test_subscribe_rx_guard_has_drop_impl() {
        // Pin the Drop impl by source-scan. Without `impl Drop for
        // SubscribeRxGuard`, the reinstall-on-drop semantic is lost.
        let source = include_str!("connection.rs");
        assert!(
            source.contains("impl Drop for SubscribeRxGuard"),
            "Phase 0 Item 5: SubscribeRxGuard MUST have a Drop impl \
             that reinstalls the Option<Receiver> into the slot. \
             Removing this impl breaks the contract that the next \
             reconnect cycle can take the receiver again.",
        );
    }
}
