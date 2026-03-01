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
use tracing::{debug, error, info, warn};

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
    frame_sender: mpsc::Sender<Vec<u8>>,

    /// Current connection state (tracked for health reporting).
    state: std::sync::Mutex<ConnectionState>,

    /// Total reconnection count since startup.
    total_reconnections: AtomicU64,

    /// Pre-built subscription messages — built once in `new()`, reused on every reconnect.
    /// IDX_I instruments use Ticker mode; all others use the configured feed mode.
    cached_subscription_messages: Vec<String>,
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
        frame_sender: mpsc::Sender<Vec<u8>>,
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
        }
    }

    /// Returns the connection identifier.
    pub fn connection_id(&self) -> ConnectionId {
        self.connection_id
    }

    /// Returns a health snapshot for monitoring.
    pub fn health(&self) -> ConnectionHealth {
        ConnectionHealth {
            connection_id: self.connection_id,
            state: *self.state.lock().expect("state lock poisoned"), // APPROVED: lock poison is unrecoverable
            subscribed_count: self.instruments.len(),
            total_reconnections: self.total_reconnections.load(Ordering::Relaxed),
        }
    }

    /// Runs the connection lifecycle: connect → subscribe → read loop.
    ///
    /// On disconnect, attempts reconnection with exponential backoff.
    /// Returns only on non-reconnectable errors or exhausted retries.
    pub async fn run(&self) -> Result<(), WebSocketError> {
        loop {
            self.set_state(ConnectionState::Connecting);

            match self.connect_and_subscribe().await {
                Ok(ws_stream) => {
                    self.set_state(ConnectionState::Connected);

                    info!(
                        connection_id = self.connection_id,
                        instruments = self.instruments.len(),
                        "WebSocket connected and subscribed"
                    );

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
                            self.set_state(ConnectionState::Disconnected);
                            return Err(WebSocketError::NonReconnectableDisconnect { code });
                        }
                        Err(err) => {
                            warn!(
                                connection_id = self.connection_id,
                                error = %err,
                                "WebSocket disconnected — will reconnect"
                            );
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
            self.total_reconnections.fetch_add(1, Ordering::Relaxed);

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

        let access_token = token_state.access_token().expose_secret().to_string();

        // Build URL with auth query parameters (Dhan V2 protocol).
        // CRITICAL: The base URL must have an explicit "/" path before the query
        // string. Without it, http::Uri parses the path as empty, and tungstenite
        // writes "GET ?version=2&... HTTP/1.1" which is invalid HTTP (RFC 7230
        // requires the request-target to start with "/"). Proxies reject this
        // with 400 Bad Request.
        let base = self.websocket_base_url.trim_end_matches('/');
        let authenticated_url = format!(
            "{base}/?version={}&token={}&clientId={}&authType={}",
            WEBSOCKET_PROTOCOL_VERSION, access_token, self.client_id, WEBSOCKET_AUTH_TYPE,
        );

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
            self.dhan_config.max_instruments_per_connection as u64 * 10 + 10000,
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
            Err(tokio_tungstenite::tungstenite::Error::Http(ref response)) => {
                error!(
                    connection_id = self.connection_id,
                    status = %response.status(),
                    body = ?response.body().as_ref().map(|b| String::from_utf8_lossy(b).to_string()),
                    "WebSocket HTTP error"
                );
                return Err(WebSocketError::ConnectionFailed {
                    url: self.websocket_base_url.clone(),
                    source: connect_result.unwrap_err(),
                });
            }
            Err(err) => {
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
            if batch_index < self.cached_subscription_messages.len() - 1 {
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
            match time::timeout(read_timeout, read.next()).await {
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
                        if self.frame_sender.send(data.to_vec()).await.is_err() {
                            warn!(
                                connection_id = self.connection_id,
                                "Frame receiver dropped — stopping read loop"
                            );
                            return Ok(());
                        }
                    }
                    Some(Ok(Message::Ping(data))) => {
                        // Server ping — respond with pong to keep connection alive.
                        let mut sink = write.lock().await;
                        let _ = sink.send(Message::Pong(data)).await;
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
    async fn wait_with_backoff(&self) -> bool {
        let attempt = self.total_reconnections.load(Ordering::Relaxed);
        if attempt >= self.ws_config.reconnect_max_attempts as u64 {
            error!(
                connection_id = self.connection_id,
                attempts = attempt,
                "Max reconnection attempts exhausted"
            );
            return false;
        }

        // Exponential backoff: initial * 2^attempt, capped at max.
        let delay_ms = self
            .ws_config
            .reconnect_initial_delay_ms
            .saturating_mul(1u64.checked_shl(attempt as u32).unwrap_or(u64::MAX))
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
            rest_api_base_url: "https://api.dhan.co/v2".to_string(),
            auth_base_url: "https://auth.dhan.co".to_string(),
            instrument_csv_url: "https://example.com/csv".to_string(),
            instrument_csv_fallback_url: "https://example.com/csv-fallback".to_string(),
            max_instruments_per_connection: 5000,
            max_websocket_connections: 5,
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
        }
    }

    fn make_test_token_handle() -> TokenHandle {
        Arc::new(arc_swap::ArcSwap::new(Arc::new(None)))
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
        );
        let result = conn.run().await;
        match result {
            Err(WebSocketError::ReconnectionExhausted {
                connection_id,
                attempts,
            }) => {
                assert_eq!(connection_id, 2);
                assert_eq!(attempts, 3);
            }
            other => panic!("Expected ReconnectionExhausted, got {other:?}"),
        }
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
        );
        assert_eq!(conn.health().total_reconnections, 0);

        conn.total_reconnections
            .store(7, std::sync::atomic::Ordering::Relaxed);
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
        );

        // Simulate 3 reconnections already happened
        conn.total_reconnections
            .store(3, std::sync::atomic::Ordering::Relaxed);
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
        );

        // Only 2 reconnections — well under limit of 10
        conn.total_reconnections
            .store(2, std::sync::atomic::Ordering::Relaxed);
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
}
