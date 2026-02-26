//! Single WebSocket connection to Dhan Live Market Feed.
//!
//! Handles: connect → authenticate → subscribe → ping loop → read frames →
//! disconnect handling → reconnect with backoff.
//!
//! Each connection manages up to 5,000 instruments.
//! The connection pool creates up to 5 of these.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use secrecy::ExposeSecret;
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::time;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};
use tracing::{debug, error, info, warn};

use dhan_live_trader_common::config::{DhanConfig, WebSocketConfig};
use dhan_live_trader_common::types::FeedMode;

use crate::auth::TokenHandle;
use crate::websocket::subscription_builder::build_subscription_messages;
use crate::websocket::types::{
    ConnectionHealth, ConnectionId, ConnectionState, InstrumentSubscription, WebSocketError,
};

// ---------------------------------------------------------------------------
// WebSocket Connection
// ---------------------------------------------------------------------------

/// A single WebSocket connection to Dhan's live market feed.
///
/// Manages its own lifecycle: connect, subscribe, ping, read, reconnect.
/// Binary frames are forwarded to `frame_sender` for downstream processing.
pub struct WebSocketConnection {
    /// Connection identifier within the pool (0–4).
    connection_id: ConnectionId,

    /// Atomic token handle — O(1) reads, swapped atomically on renewal.
    token_handle: TokenHandle,

    /// Dhan client ID (from SSM credentials).
    client_id: String,

    /// Dhan WebSocket URL (from config).
    websocket_url: String,

    /// Dhan config for connection limits.
    dhan_config: DhanConfig,

    /// WebSocket keep-alive and reconnection config.
    ws_config: WebSocketConfig,

    /// Instruments assigned to this connection.
    instruments: Vec<InstrumentSubscription>,

    /// Feed mode for subscriptions.
    feed_mode: FeedMode,

    /// Channel sender for forwarding raw binary frames to downstream.
    frame_sender: mpsc::Sender<Vec<u8>>,

    /// Current connection state (tracked for health reporting).
    state: std::sync::Mutex<ConnectionState>,

    /// Consecutive pong failures counter.
    consecutive_pong_failures: AtomicU32,

    /// Total reconnection count since startup.
    total_reconnections: AtomicU64,
}

impl WebSocketConnection {
    /// Creates a new WebSocket connection (not yet connected).
    ///
    /// Call `run()` to start the connection lifecycle.
    #[allow(clippy::too_many_arguments)]
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
        let websocket_url = dhan_config.websocket_url.clone();
        Self {
            connection_id,
            token_handle,
            client_id,
            websocket_url,
            dhan_config,
            ws_config,
            instruments,
            feed_mode,
            frame_sender,
            state: std::sync::Mutex::new(ConnectionState::Disconnected),
            consecutive_pong_failures: AtomicU32::new(0),
            total_reconnections: AtomicU64::new(0),
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
            state: *self.state.lock().expect("state lock poisoned"),
            subscribed_count: self.instruments.len(),
            consecutive_pong_failures: self.consecutive_pong_failures.load(Ordering::Relaxed),
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
                    self.consecutive_pong_failures.store(0, Ordering::Relaxed);

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

    /// Establishes the WebSocket connection with auth headers and sends subscriptions.
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

        // Build HTTP request with auth headers.
        let mut request = self
            .websocket_url
            .as_str()
            .into_client_request()
            .map_err(|err| WebSocketError::ConnectionFailed {
                url: self.websocket_url.clone(),
                source: err,
            })?;

        request.headers_mut().insert(
            "Authorization",
            access_token
                .parse()
                .map_err(|_| WebSocketError::NoTokenAvailable)?,
        );
        request.headers_mut().insert(
            "Client-Id",
            self.client_id
                .parse()
                .map_err(|_| WebSocketError::NoTokenAvailable)?,
        );

        debug!(
            connection_id = self.connection_id,
            url = %self.websocket_url,
            "Connecting to Dhan WebSocket"
        );

        // Connect with timeout.
        let connect_timeout = Duration::from_millis(
            self.dhan_config.max_instruments_per_connection as u64 * 10 + 10000,
        );
        let (ws_stream, _response) = time::timeout(connect_timeout, connect_async(request))
            .await
            .map_err(|_| WebSocketError::ConnectionFailed {
                url: self.websocket_url.clone(),
                source: tokio_tungstenite::tungstenite::Error::Io(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "WebSocket connection timed out",
                )),
            })?
            .map_err(|err| WebSocketError::ConnectionFailed {
                url: self.websocket_url.clone(),
                source: err,
            })?;

        // Send subscription messages (batched, max 100 per message).
        let messages = build_subscription_messages(
            &self.instruments,
            self.feed_mode,
            self.ws_config.subscription_batch_size,
        );

        let (mut write, read) = ws_stream.split();

        for msg in &messages {
            write
                .send(Message::Text(msg.clone().into()))
                .await
                .map_err(|err| WebSocketError::SubscriptionFailed {
                    connection_id: self.connection_id,
                    reason: err.to_string(),
                })?;
        }

        info!(
            connection_id = self.connection_id,
            instruments = self.instruments.len(),
            messages_sent = messages.len(),
            "Subscriptions sent"
        );

        // Reunite for the read loop (ping task gets its own write handle).
        Ok(read.reunite(write).expect("reunite same stream"))
    }

    /// Runs the read loop: reads binary frames, handles pings, detects disconnects.
    ///
    /// Splits the stream: read half processes frames, write half sends pings.
    async fn run_read_loop(
        &self,
        ws_stream: WebSocketStream<MaybeTlsStream<TcpStream>>,
    ) -> Result<(), WebSocketError> {
        let (write, mut read) = ws_stream.split();
        let write = Arc::new(tokio::sync::Mutex::new(write));

        // Spawn ping task.
        let ping_write = Arc::clone(&write);
        let ping_interval = Duration::from_secs(self.ws_config.ping_interval_secs);
        let connection_id = self.connection_id;

        let ping_handle = tokio::spawn(async move {
            let mut interval = time::interval(ping_interval);
            interval.tick().await; // first tick is immediate — skip it
            loop {
                interval.tick().await;
                let mut sink = ping_write.lock().await;
                if let Err(err) = sink.send(Message::Ping(vec![].into())).await {
                    debug!(
                        connection_id = connection_id,
                        error = %err,
                        "Ping send failed"
                    );
                    break;
                }
            }
        });

        // Read loop.
        let result = loop {
            match read.next().await {
                Some(Ok(Message::Binary(data))) => {
                    // Forward raw binary frame to downstream.
                    if self.frame_sender.send(data.to_vec()).await.is_err() {
                        warn!(
                            connection_id = self.connection_id,
                            "Frame receiver dropped — stopping read loop"
                        );
                        break Ok(());
                    }
                }
                Some(Ok(Message::Pong(_))) => {
                    self.consecutive_pong_failures.store(0, Ordering::Relaxed);
                }
                Some(Ok(Message::Ping(data))) => {
                    // Respond to server pings (unusual but handle gracefully).
                    let mut sink = write.lock().await;
                    let _ = sink.send(Message::Pong(data)).await;
                }
                Some(Ok(Message::Close(frame))) => {
                    if let Some(frame) = frame {
                        let code: u16 = frame.code.into();
                        let reason = frame.reason.to_string();
                        warn!(
                            connection_id = self.connection_id,
                            close_code = code,
                            reason = %reason,
                            "WebSocket close frame received"
                        );
                    }
                    break Ok(());
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
                    break Err(WebSocketError::ConnectionFailed {
                        url: self.websocket_url.clone(),
                        source: err,
                    });
                }
                None => {
                    // Stream ended.
                    break Ok(());
                }
                _ => {}
            }
        };

        ping_handle.abort();
        result
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
        let mut state = self.state.lock().expect("state lock poisoned");
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
            websocket_url: "wss://api-feed.dhan.co/v2".to_string(),
            rest_api_base_url: "https://api.dhan.co/v2".to_string(),
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
        assert_eq!(health.consecutive_pong_failures, 0);
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
    fn test_health_tracks_pong_failure_increments() {
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
        assert_eq!(conn.health().consecutive_pong_failures, 0);

        conn.consecutive_pong_failures
            .store(3, std::sync::atomic::Ordering::Relaxed);
        assert_eq!(conn.health().consecutive_pong_failures, 3);
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
}
