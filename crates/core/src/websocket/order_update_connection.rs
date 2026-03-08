//! WebSocket connection for Dhan live order updates.
//!
//! Connects to `wss://api-order-update.dhan.co`, sends a login message,
//! then receives JSON order updates and broadcasts them via `broadcast::Sender`.
//!
//! # Protocol
//! - JSON-based (NOT binary like the market feed)
//! - Login message: `{"LoginReq": {"MsgCode": 42, "ClientId": "...", "Token": "..."}, "UserType": "SELF"}`
//! - Order updates arrive automatically after login (no subscription needed)
//!
//! # Performance
//! Cold path — orders are infrequent (~1-100/day). Allocations are acceptable here.

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use secrecy::ExposeSecret;
use tokio::sync::broadcast;
use tokio::time;
use tokio_tungstenite::connect_async_tls_with_config;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tracing::{debug, error, info, warn};

use dhan_live_trader_common::constants::{
    ORDER_UPDATE_MAX_RECONNECT_ATTEMPTS, ORDER_UPDATE_READ_TIMEOUT_SECS,
    ORDER_UPDATE_RECONNECT_INITIAL_DELAY_MS, ORDER_UPDATE_RECONNECT_MAX_DELAY_MS,
};
use dhan_live_trader_common::order_types::OrderUpdate;

use crate::auth::TokenHandle;
use crate::parser::order_update::{build_order_update_login, parse_order_update};
use crate::websocket::tls::build_websocket_tls_connector;

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Runs the order update WebSocket connection lifecycle.
///
/// Connects, logs in, reads order updates, and broadcasts them.
/// Reconnects with exponential backoff on failure.
///
/// # Arguments
/// * `order_update_url` — Dhan order update WebSocket URL.
/// * `client_id` — Dhan client identifier.
/// * `token_handle` — Atomic token handle for O(1) reads.
/// * `order_sender` — Broadcast channel for order updates.
///
/// Runs until the task is aborted or reconnection attempts are exhausted.
pub async fn run_order_update_connection(
    order_update_url: String,
    client_id: String,
    token_handle: TokenHandle,
    order_sender: broadcast::Sender<OrderUpdate>,
) {
    let mut consecutive_failures: u32 = 0;

    info!("order update WebSocket starting");

    loop {
        match connect_and_listen(&order_update_url, &client_id, &token_handle, &order_sender).await
        {
            Ok(()) => {
                // Clean disconnect — reset backoff and reconnect.
                consecutive_failures = 0;
                info!("order update WebSocket disconnected cleanly — reconnecting");
            }
            Err(err) => {
                consecutive_failures = consecutive_failures.saturating_add(1);
                if consecutive_failures > ORDER_UPDATE_MAX_RECONNECT_ATTEMPTS {
                    error!(
                        attempts = consecutive_failures,
                        "order update WebSocket exhausted reconnection attempts"
                    );
                    return;
                }
                warn!(
                    ?err,
                    attempt = consecutive_failures,
                    "order update WebSocket error — will reconnect"
                );
            }
        }

        // Exponential backoff: delay_ms = initial * 2^(failures-1), capped at max.
        let shift = consecutive_failures.saturating_sub(1).min(63);
        let delay_ms = ORDER_UPDATE_RECONNECT_INITIAL_DELAY_MS
            .saturating_mul(1_u64 << shift)
            .min(ORDER_UPDATE_RECONNECT_MAX_DELAY_MS);
        debug!(delay_ms, "order update WebSocket reconnect backoff");
        time::sleep(Duration::from_millis(delay_ms)).await;
    }
}

// ---------------------------------------------------------------------------
// Internal
// ---------------------------------------------------------------------------

/// Single connection lifecycle: connect → login → read loop.
async fn connect_and_listen(
    url: &str,
    client_id: &str,
    token_handle: &TokenHandle,
    order_sender: &broadcast::Sender<OrderUpdate>,
) -> Result<(), OrderUpdateConnectionError> {
    // Read current token.
    let token_guard = token_handle.load();
    let token_state = token_guard
        .as_ref()
        .as_ref()
        .ok_or(OrderUpdateConnectionError::NoToken)?;

    if !token_state.is_valid() {
        return Err(OrderUpdateConnectionError::TokenExpired);
    }

    let access_token = token_state.access_token().expose_secret().to_string();

    // Build TLS connector (O(1) EXEMPT: cold path, runs once per connect).
    let tls_connector = build_websocket_tls_connector()
        .map_err(|err| OrderUpdateConnectionError::Tls(err.to_string()))?;

    // Connect to order update WebSocket.
    let request = url
        .into_client_request()
        .map_err(|err| OrderUpdateConnectionError::Connect(err.to_string()))?;

    let (ws_stream, _response) =
        connect_async_tls_with_config(request, None, false, Some(tls_connector))
            .await
            .map_err(|err| OrderUpdateConnectionError::Connect(err.to_string()))?;

    info!("order update WebSocket connected");

    let (mut write, mut read) = ws_stream.split();

    // Send login message.
    let login_msg = build_order_update_login(client_id, &access_token);
    write
        .send(Message::Text(login_msg.into()))
        .await
        .map_err(|err| OrderUpdateConnectionError::Send(err.to_string()))?;

    debug!("order update WebSocket login sent");

    let read_timeout = Duration::from_secs(ORDER_UPDATE_READ_TIMEOUT_SECS);

    // Read loop — receive order updates until disconnect.
    loop {
        let msg = match time::timeout(read_timeout, read.next()).await {
            Ok(Some(Ok(msg))) => msg,
            Ok(Some(Err(err))) => {
                return Err(OrderUpdateConnectionError::Read(err.to_string()));
            }
            Ok(None) => {
                // Stream ended (server closed connection).
                return Ok(());
            }
            Err(_) => {
                // Read timeout — server may have stopped sending pings.
                return Err(OrderUpdateConnectionError::ReadTimeout);
            }
        };

        match msg {
            Message::Text(text) => {
                match parse_order_update(&text) {
                    Ok(update) => {
                        debug!(
                            order_no = %update.order_no,
                            status = %update.status,
                            symbol = %update.symbol,
                            "order update received"
                        );
                        // Broadcast to subscribers (OMS, notification, etc.).
                        // If no receivers, send returns Err — that's fine, just discard.
                        let _ = order_sender.send(update);
                    }
                    Err(err) => {
                        // Could be a login response or heartbeat — not all messages are order updates.
                        debug!(
                            ?err,
                            text_preview = &text[..text.len().min(200)],
                            "non-order JSON message"
                        );
                    }
                }
            }
            Message::Ping(_) => {
                // tokio-tungstenite auto-pongs.
                debug!("order update WebSocket ping received");
            }
            Message::Close(frame) => {
                info!(?frame, "order update WebSocket close frame received");
                return Ok(());
            }
            _ => {
                // Binary, Pong, Frame — ignore.
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

/// Errors from the order update WebSocket connection.
#[derive(Debug, thiserror::Error)]
enum OrderUpdateConnectionError {
    #[error("no authentication token available")]
    NoToken,

    #[error("authentication token expired")]
    TokenExpired,

    #[error("TLS configuration failed: {0}")]
    Tls(String),

    #[error("connection failed: {0}")]
    Connect(String),

    #[error("failed to send login message: {0}")]
    Send(String),

    #[error("read error: {0}")]
    Read(String),

    #[error("read timeout — no messages for {ORDER_UPDATE_READ_TIMEOUT_SECS}s")]
    ReadTimeout,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_order_update_connection_error_display_variants() {
        let err = OrderUpdateConnectionError::NoToken;
        assert!(err.to_string().contains("no authentication token"));

        let err = OrderUpdateConnectionError::TokenExpired;
        assert!(err.to_string().contains("token expired"));

        let err = OrderUpdateConnectionError::Tls("test".to_string());
        assert!(err.to_string().contains("TLS"));

        let err = OrderUpdateConnectionError::Connect("refused".to_string());
        assert!(err.to_string().contains("connection failed"));

        let err = OrderUpdateConnectionError::Send("broken pipe".to_string());
        assert!(err.to_string().contains("login message"));

        let err = OrderUpdateConnectionError::Read("reset".to_string());
        assert!(err.to_string().contains("read error"));

        let err = OrderUpdateConnectionError::ReadTimeout;
        assert!(err.to_string().contains("read timeout"));
    }

    #[test]
    fn test_backoff_calculation() {
        // Verify the exponential backoff formula used in the connection loop.
        let initial = ORDER_UPDATE_RECONNECT_INITIAL_DELAY_MS;
        let max = ORDER_UPDATE_RECONNECT_MAX_DELAY_MS;

        // Attempt 1: initial * 2^0 = initial
        let delay = initial.saturating_mul(1_u64 << 0);
        assert_eq!(delay.min(max), initial);

        // Attempt 2: initial * 2^1 = 2 * initial
        let delay = initial.saturating_mul(1_u64 << 1);
        assert_eq!(delay.min(max), initial.saturating_mul(2));

        // Attempt 10: capped at max
        let delay = initial.saturating_mul(1_u64 << 9);
        assert_eq!(delay.min(max), max);
    }

    #[test]
    fn test_max_reconnect_attempts_constant() {
        assert!(ORDER_UPDATE_MAX_RECONNECT_ATTEMPTS >= 5);
        assert!(ORDER_UPDATE_MAX_RECONNECT_ATTEMPTS <= 20);
    }
}
