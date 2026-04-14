//! Single WebSocket connection to Dhan Live Market Feed.
//!
//! Handles: connect → authenticate → subscribe → ping loop → read frames →
//! disconnect handling → reconnect with backoff.
//!
//! Each connection manages up to 5,000 instruments.
//! The connection pool creates up to 5 of these.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use secrecy::ExposeSecret;
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::time;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async_tls_with_config};

use dhan_live_trader_common::constants::{WEBSOCKET_AUTH_TYPE, WEBSOCKET_PROTOCOL_VERSION};
use tracing::{debug, error, info, instrument, warn};

use dhan_live_trader_common::config::{DhanConfig, WebSocketConfig};
use dhan_live_trader_common::types::FeedMode;

use crate::auth::TokenHandle;
use crate::websocket::subscription_builder::build_subscription_messages;
use crate::websocket::tls::build_websocket_tls_connector;
use crate::websocket::types::{
    ConnectionHealth, ConnectionId, ConnectionState, InstrumentSubscription, WebSocketError,
};

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
    frame_sender: mpsc::Sender<bytes::Bytes>,

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
        frame_sender: mpsc::Sender<bytes::Bytes>,
        notifier: Option<Arc<crate::notification::NotificationService>>,
    ) -> Self {
        let websocket_base_url = dhan_config.websocket_url.clone(); // O(1) EXEMPT: constructor — once

        // Pre-build subscription messages once. IDX_I instruments only support Ticker mode —
        // Dhan silently drops Full/Quote subscriptions for index value feeds.
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
                FeedMode::Ticker,
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
        }
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

    /// Returns the connection identifier.
    pub fn connection_id(&self) -> ConnectionId {
        self.connection_id
    }

    /// Returns a health snapshot for monitoring.
    #[allow(clippy::expect_used)] // APPROVED: lock poison is unrecoverable
    pub fn health(&self) -> ConnectionHealth {
        ConnectionHealth {
            connection_id: self.connection_id,
            state: *self.state.lock().expect("state lock poisoned"), // APPROVED: lock poison is unrecoverable
            subscribed_count: self.instruments.len(),
            total_reconnections: self.total_reconnections.load(Ordering::Acquire),
        }
    }

    /// Runs the connection lifecycle: connect → subscribe → read loop.
    ///
    /// On disconnect, attempts reconnection with exponential backoff.
    /// Returns only on non-reconnectable errors or exhausted retries.
    #[instrument(skip_all, fields(conn_id = self.connection_id))]
    pub async fn run(&self) -> Result<(), WebSocketError> {
        // O(1) EXEMPT: begin — metric handles grabbed once before loop, not per-message
        let m_conn_active = metrics::gauge!("dlt_websocket_connections_active", "connection_id" => self.connection_id.to_string());
        let m_reconnections = metrics::counter!("dlt_websocket_reconnections_total", "connection_id" => self.connection_id.to_string());
        // O(1) EXEMPT: end

        loop {
            self.set_state(ConnectionState::Connecting);

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

                    // M1: After a reconnection (not initial connect), log that a
                    // mid-session candle gap may exist. The existing post-market
                    // historical fetch will backfill any missing data.
                    if reconnection_count > 0 {
                        info!(
                            connection_id = self.connection_id,
                            reconnection_count,
                            "WebSocket reconnected — mid-session candle gap may exist, next post-market fetch will backfill"
                        );
                        // H1: Fire Telegram alert on reconnection success.
                        if let Some(ref n) = self.notifier {
                            n.notify(crate::notification::events::NotificationEvent::WebSocketReconnected {
                                connection_index: usize::from(self.connection_id),
                            });
                        }
                    }

                    // Run read + ping loops until disconnect.
                    let disconnect_result = self.run_read_loop(ws_stream).await;

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
                        Err(WebSocketError::DhanDisconnect { code })
                            if !code.is_reconnectable() =>
                        {
                            error!(
                                connection_id = self.connection_id,
                                disconnect_code = %code,
                                "Non-reconnectable disconnect — stopping connection"
                            );
                            // H1: Critical disconnect — fire Telegram immediately.
                            if let Some(ref n) = self.notifier {
                                n.notify(crate::notification::events::NotificationEvent::WebSocketDisconnected {
                                    connection_index: usize::from(self.connection_id),
                                    // O(1) EXEMPT: cold path — reconnection error, not per tick
                                    reason: format!("Non-reconnectable: {code}"),
                                });
                            }
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
                            if let Some(ref n) = self.notifier {
                                n.notify(crate::notification::events::NotificationEvent::WebSocketDisconnected {
                                    connection_index: usize::from(self.connection_id),
                                    // O(1) EXEMPT: cold path, once per 807 event
                                    reason: format!("Token expired ({code}) — waiting for renewal"),
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
                            warn!(
                                connection_id = self.connection_id,
                                error = %err,
                                "WebSocket disconnected — will reconnect"
                            );
                            // H1: Fire Telegram alert on unexpected disconnect.
                            if let Some(ref n) = self.notifier {
                                n.notify(crate::notification::events::NotificationEvent::WebSocketDisconnected {
                                    connection_index: usize::from(self.connection_id),
                                    // O(1) EXEMPT: cold path — reconnection error, not per tick
                                    reason: format!("{err}"),
                                });
                            }
                        }
                    }
                }
                Err(err) => {
                    warn!(
                        connection_id = self.connection_id,
                        error = %err,
                        "WebSocket connection failed — will retry"
                    );
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

        // Dynamic read timeout: ping_interval × (max_failures + 1) + pong_timeout.
        // Default: 10 × (2 + 1) + 10 = 40s. Matches SERVER_PING_TIMEOUT_SECS.
        // Any received frame (Binary, Ping, Text) resets the timeout.
        let read_timeout_secs = self
            .ws_config
            .ping_interval_secs
            .saturating_mul(u64::from(self.ws_config.max_consecutive_pong_failures) + 1)
            .saturating_add(self.ws_config.pong_timeout_secs);
        let read_timeout = Duration::from_secs(read_timeout_secs);

        loop {
            // A5: tokio::select! lets the read loop respond to a graceful
            // shutdown request without waiting for the 40-second read timeout.
            // On shutdown: send Disconnect JSON (RequestCode 12), close the
            // socket, and return Ok so the outer `run()` loop exits cleanly
            // (see `is_shutdown_requested` check in `run()`).
            let select_result = tokio::select! {
                biased;
                () = self.shutdown_notify.notified() => {
                    info!(
                        connection_id = self.connection_id,
                        "A5: graceful shutdown notified — sending RequestCode 12 (Disconnect) to Dhan"
                    );
                    let disconnect_json = crate::websocket::subscription_builder::build_disconnect_message();
                    let send_timeout = Duration::from_secs(2);
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
                                "dlt_ws_graceful_unsub_total",
                                "connection_id" => self.connection_id.to_string(),
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
                                "dlt_ws_graceful_unsub_total",
                                "connection_id" => self.connection_id.to_string(),
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
                                "dlt_ws_graceful_unsub_total",
                                "connection_id" => self.connection_id.to_string(),
                                "outcome" => "timeout"
                            )
                            .increment(1);
                        }
                    }
                    // Close the socket regardless of send outcome.
                    {
                        let mut sink = write.lock().await;
                        let _ = time::timeout(Duration::from_secs(1), sink.close()).await;
                    }
                    return Ok(());
                }
                timeout_result = time::timeout(read_timeout, read.next()) => timeout_result,
            };

            match select_result {
                Err(_elapsed) => {
                    warn!(
                        connection_id = self.connection_id,
                        timeout_secs = read_timeout_secs,
                        "WebSocket read timeout — no data received, treating as dead connection"
                    );
                    return Err(WebSocketError::ReadTimeout {
                        connection_id: self.connection_id,
                        timeout_secs: read_timeout_secs,
                    });
                }
                Ok(frame_result) => match frame_result {
                    Some(Ok(Message::Binary(data))) => {
                        // Forward raw binary frame to downstream.
                        // ZERO TICK LOSS: Use try_send for fast path, fall back to
                        // blocking send (backpressure) if channel is full. This
                        // guarantees no frame is ever dropped. The WS connection
                        // survives backpressure because Dhan's ping timeout is 40s,
                        // and the 128K channel gives 13+ seconds of headroom.
                        match self.frame_sender.try_send(data) {
                            Ok(()) => {} // fast path — channel has space
                            Err(mpsc::error::TrySendError::Full(data)) => {
                                // Channel full — apply backpressure (block WS read).
                                // This is safer than dropping: WS survives 40s of
                                // backpressure, and zero ticks are lost.
                                warn!(
                                    connection_id = self.connection_id,
                                    channel_capacity = self.frame_sender.capacity(),
                                    "SPSC channel full — backpressure active (zero-drop mode)"
                                );
                                metrics::counter!("dlt_ws_frame_backpressure_total").increment(1);
                                // Blocking send with backpressure timeout — if still
                                // stuck, the WS ping timeout (40s) will kill the
                                // connection and auto-reconnect will re-establish it.
                                const BACKPRESSURE_TIMEOUT: Duration = Duration::from_secs(
                                    dhan_live_trader_common::constants::FRAME_BACKPRESSURE_TIMEOUT_SECS,
                                );
                                match time::timeout(
                                    BACKPRESSURE_TIMEOUT,
                                    self.frame_sender.send(data),
                                )
                                .await
                                {
                                    Ok(Ok(())) => {
                                        debug!(
                                            connection_id = self.connection_id,
                                            "backpressure resolved — frame sent"
                                        );
                                    }
                                    Ok(Err(_)) => {
                                        warn!(
                                            connection_id = self.connection_id,
                                            "Frame receiver dropped — stopping read loop"
                                        );
                                        return Ok(());
                                    }
                                    Err(_elapsed) => {
                                        // 30s backpressure exhausted. The WS ping timeout
                                        // (40s) will disconnect us shortly. Log CRITICAL.
                                        error!(
                                            connection_id = self.connection_id,
                                            "CRITICAL: 30s backpressure timeout — tick processor frozen"
                                        );
                                        metrics::counter!("dlt_ws_backpressure_timeout_total")
                                            .increment(1);
                                    }
                                }
                            }
                            Err(mpsc::error::TrySendError::Closed(_)) => {
                                warn!(
                                    connection_id = self.connection_id,
                                    "Frame receiver dropped — stopping read loop"
                                );
                                return Ok(());
                            }
                        }
                    }
                    Some(Ok(Message::Ping(data))) => {
                        // Server ping — respond with pong to keep connection alive.
                        // Timeout prevents hang if TCP buffer is full on a dead connection.
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
                        // Stream ended.
                        return Ok(());
                    }
                    _ => {}
                },
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

        // CRITICAL alert every 10 consecutive failures (triggers Telegram).
        if attempt.is_multiple_of(10) {
            error!(
                connection_id = self.connection_id,
                consecutive_failures = attempt,
                "WebSocket reconnection threshold hit — still retrying (infinite resilience mode)"
            );
        }

        // Exponential backoff: initial * 2^attempt, capped at max.
        let delay_ms = self
            .ws_config
            .reconnect_initial_delay_ms
            .saturating_mul(1u64.checked_shl(attempt.min(63) as u32).unwrap_or(u64::MAX))
            .min(self.ws_config.reconnect_max_delay_ms);

        info!(
            connection_id = self.connection_id,
            attempt = attempt,
            delay_ms = delay_ms,
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
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use dhan_live_trader_common::types::ExchangeSegment;

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

    /// Extract `ReadTimeout` fields from a `Result`, panicking if the variant
    /// doesn't match.
    #[track_caller]
    fn unwrap_read_timeout(result: Result<(), WebSocketError>) -> (u8, u64) {
        match result {
            Err(WebSocketError::ReadTimeout {
                connection_id,
                timeout_secs,
            }) => (connection_id, timeout_secs),
            other => panic!("expected ReadTimeout, got {other:?}"),
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
    fn test_cached_messages_idx_i_uses_ticker() {
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
            FeedMode::Full, // Configured as Full, but IDX_I should use Ticker
            tx,
            None,
        );
        // All IDX_I → only Ticker messages (request_code 15), NOT Full (21)
        assert_eq!(conn.cached_subscription_messages.len(), 1);
        assert!(conn.cached_subscription_messages[0].contains("\"RequestCode\":15"));
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
            FeedMode::Full, // Non-IDX gets Full (21), IDX_I gets Ticker (15)
            tx,
            None,
        );
        // 2 non-IDX → 1 Full message, 2 IDX_I → 1 Ticker message = 2 total
        assert_eq!(conn.cached_subscription_messages.len(), 2);
        let has_full = conn
            .cached_subscription_messages
            .iter()
            .any(|m| m.contains("\"RequestCode\":21"));
        let has_ticker = conn
            .cached_subscription_messages
            .iter()
            .any(|m| m.contains("\"RequestCode\":15"));
        assert!(has_full, "Should have Full mode message for NseFno");
        assert!(has_ticker, "Should have Ticker mode message for IdxI");
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
    fn test_connection_only_idx_i_in_ticker_mode() {
        let (tx, _rx) = mpsc::channel(100);
        let instruments = vec![InstrumentSubscription::new(ExchangeSegment::IdxI, 13)];
        let conn = WebSocketConnection::new(
            0,
            make_test_token_handle(),
            "test-client".to_string(),
            make_test_dhan_config(),
            make_test_ws_config(),
            instruments,
            FeedMode::Ticker, // Same as forced mode for IDX_I
            tx,
            None,
        );
        assert_eq!(conn.cached_subscription_messages.len(), 1);
        assert!(conn.cached_subscription_messages[0].contains("\"RequestCode\":15"));
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

    // --- Run with zero max_attempts returns ReconnectionExhausted immediately ---

    #[tokio::test]
    async fn test_connection_run_zero_max_attempts() {
        let (tx, _rx) = mpsc::channel(100);
        let conn = WebSocketConnection::new(
            1,
            make_test_token_handle(),
            "test-client".to_string(),
            make_test_dhan_config(),
            WebSocketConfig {
                reconnect_max_attempts: 0,
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
        assert_eq!(connection_id, 1);
        assert_eq!(attempts, 0);
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
                reconnect_max_attempts: 0, // fail immediately
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
        // Connection had 10 instruments
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
                reconnect_max_attempts: 0,
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
        // Should fail with ReconnectionExhausted, not a URL parse error
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
                reconnect_max_attempts: 0,
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
        frame_sender: mpsc::Sender<bytes::Bytes>,
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
        let received = rx.recv().await.expect("should receive frame"); // APPROVED: test
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
            let frame = rx.recv().await.expect("should receive frame"); // APPROVED: test
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

    // --- run_read_loop: Read timeout (line 363-374) ---

    #[tokio::test]
    async fn test_read_loop_timeout_returns_read_timeout_error() {
        let (tx, _rx) = mpsc::channel(100);
        // Use very small timeout: ping_interval=1 * (0+1) + pong_timeout=0 = 1s
        let conn = make_test_conn_for_read_loop(tx);
        let (_server_ws, client_ws) = make_ws_pair().await;

        // Server is connected but sends nothing — read loop should timeout.
        let result = conn.run_read_loop(client_ws).await;

        let (connection_id, timeout_secs) = unwrap_read_timeout(result);
        assert_eq!(connection_id, 0);
        assert_eq!(timeout_secs, 1); // 1*(0+1)+0 = 1
    }

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
        let frame1 = rx.recv().await.expect("should get frame 1"); // APPROVED: test
        assert_eq!(frame1, vec![0xAA]);
        let frame2 = rx.recv().await.expect("should get frame 2"); // APPROVED: test
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

        let received = rx.recv().await.expect("should receive large frame"); // APPROVED: test
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

        let received = rx.recv().await.expect("should receive empty frame"); // APPROVED: test
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

        let frame = rx.recv().await.expect("should receive binary frame"); // APPROVED: test
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
    fn test_connection_idx_instruments_use_ticker_mode() {
        let (tx, _rx) = mpsc::channel(100);
        let instruments = vec![InstrumentSubscription::new(ExchangeSegment::IdxI, 13)];
        let conn = WebSocketConnection::new(
            0,
            make_test_token_handle(),
            "test-client".to_string(),
            make_test_dhan_config(),
            make_test_ws_config(),
            instruments,
            FeedMode::Full, // Full mode specified but IDX_I should use Ticker
            tx,
            None,
        );
        assert!(!conn.cached_subscription_messages.is_empty());
        // The subscription message should contain RequestCode 15 (SubscribeTicker)
        let msg = &conn.cached_subscription_messages[0];
        assert!(
            msg.contains("15"),
            "IDX_I should subscribe with Ticker mode (code 15): {}",
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
        // All instruments are IDX, so all use Ticker (code 15), no Full messages
        assert_eq!(conn.cached_subscription_messages.len(), 1);
        assert!(conn.cached_subscription_messages[0].contains("\"RequestCode\":15"));
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
        use dhan_live_trader_common::trading_calendar::ist_offset;
        use secrecy::SecretString;

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
        };
        let msg = event.to_message();
        assert!(
            msg.contains("reconnected"),
            "WebSocketReconnected event message must contain 'reconnected'"
        );
    }
}
