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

use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use secrecy::ExposeSecret;
use tokio::sync::broadcast;
use tokio::time;
use tokio_tungstenite::connect_async_tls_with_config;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tracing::{debug, error, info, warn};

use chrono::{NaiveTime, Utc};

use tickvault_common::constants::{
    ORDER_UPDATE_AUTH_TIMEOUT_SECS, ORDER_UPDATE_MAX_RECONNECT_ATTEMPTS,
    ORDER_UPDATE_OFF_HOURS_READ_TIMEOUT_SECS, ORDER_UPDATE_READ_TIMEOUT_SECS,
    ORDER_UPDATE_RECONNECT_INITIAL_DELAY_MS, ORDER_UPDATE_RECONNECT_MAX_DELAY_MS,
};
use tickvault_common::order_types::OrderUpdate;
use tickvault_common::trading_calendar::{TradingCalendar, ist_offset};

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
    calendar: Arc<TradingCalendar>,
) {
    let mut consecutive_failures: u32 = 0;
    let m_reconnections = metrics::counter!("tv_order_update_reconnections_total");

    info!("order update WebSocket starting");

    loop {
        match connect_and_listen(
            &order_update_url,
            &client_id,
            &token_handle,
            &order_sender,
            &calendar,
        )
        .await
        {
            Ok(()) => {
                // Clean disconnect — reset backoff and reconnect.
                consecutive_failures = 0;
                info!("order update WebSocket disconnected cleanly — reconnecting");
            }
            Err(err) => {
                let is_timeout = matches!(err, OrderUpdateConnectionError::ReadTimeout);
                let within_hours = is_within_market_hours(&calendar);
                let tentative_failures = consecutive_failures.saturating_add(1);

                match decide_reconnect_action(is_timeout, within_hours, tentative_failures) {
                    ReconnectAction::ResetAndReconnect => {
                        debug!(
                            "order update WebSocket idle timeout outside market hours — reconnecting"
                        );
                        consecutive_failures = 0;
                    }
                    ReconnectAction::Exhausted => {
                        consecutive_failures = tentative_failures;
                        error!(
                            attempts = consecutive_failures,
                            "order update WebSocket exhausted reconnection attempts"
                        );
                        return;
                    }
                    ReconnectAction::IncrementAndRetry => {
                        consecutive_failures = tentative_failures;
                        // CRITICAL alert every 10 consecutive failures (triggers Telegram).
                        if consecutive_failures.is_multiple_of(10) {
                            error!(
                                consecutive_failures,
                                "order update WebSocket reconnection threshold hit — still retrying"
                            );
                        } else {
                            warn!(
                                ?err,
                                attempt = consecutive_failures,
                                "order update WebSocket error — will reconnect"
                            );
                        }
                    }
                }
            }
        }

        // Exponential backoff: delay_ms = initial * 2^(failures-1), capped at max.
        m_reconnections.increment(1);
        let delay_ms = compute_reconnect_backoff_ms(consecutive_failures);
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
    calendar: &TradingCalendar,
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

    // O(1) EXEMPT: begin — cold path, runs once per WebSocket connect
    // SEC-3: Zeroize the access token after use to prevent heap residency.
    let access_token =
        zeroize::Zeroizing::new(token_state.access_token().expose_secret().to_string());

    // Build TLS connector.
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
    // O(1) EXEMPT: end

    info!("order update WebSocket connected");

    let m_active = metrics::gauge!("tv_order_update_ws_active");
    let m_messages = metrics::counter!("tv_order_update_messages_total");
    m_active.set(1.0);

    let (mut write, mut read) = ws_stream.split();

    // Send login message.
    let login_msg = build_order_update_login(client_id, &access_token);
    write
        .send(Message::Text(login_msg.into()))
        .await
        .map_err(|err| OrderUpdateConnectionError::Send(err.to_string()))?; // O(1) EXEMPT: cold path, once per connect

    debug!("order update WebSocket login sent");

    // Dhan's order update WS does NOT send an explicit auth response.
    // After accepting the login, it silently starts streaming order updates
    // when new ones arrive. If auth fails, the server sends a Close frame
    // which the read loop below detects. No need to wait for a response
    // that may never come (causes false AuthTimeout reconnect loops).
    info!("order update WebSocket login sent — entering read loop");

    // Use longer timeout outside market hours to avoid reconnect noise.
    let timeout_secs = select_read_timeout_secs(is_within_market_hours(calendar));
    let read_timeout = Duration::from_secs(timeout_secs);

    // Read loop — receive order updates until disconnect.
    loop {
        let msg = match time::timeout(read_timeout, read.next()).await {
            Ok(Some(Ok(msg))) => msg,
            Ok(Some(Err(err))) => {
                m_active.set(0.0);
                // O(1) EXEMPT: error path, not hot path — .to_string() on cold disconnect
                return Err(OrderUpdateConnectionError::Read(err.to_string()));
            }
            Ok(None) => {
                // Stream ended (server closed connection).
                m_active.set(0.0);
                return Ok(());
            }
            Err(_) => {
                // Read timeout — server may have stopped sending pings.
                m_active.set(0.0);
                return Err(OrderUpdateConnectionError::ReadTimeout);
            }
        };

        match msg {
            Message::Text(text) => {
                match parse_order_update(&text) {
                    Ok(update) => {
                        m_messages.increment(1);
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
                        // Not a valid order update — check if it's an auth error.
                        match classify_auth_response(&text) {
                            AuthResponseKind::Failed(reason) => {
                                error!(
                                    reason = %reason,
                                    "order update WebSocket auth/API error from server"
                                );
                                m_active.set(0.0);
                                return Err(OrderUpdateConnectionError::AuthFailed(reason));
                            }
                            AuthResponseKind::Success => {
                                // Login ack or heartbeat — not all messages are order updates.
                                metrics::counter!("tv_order_update_non_order_messages_total")
                                    .increment(1);
                                debug!(
                                    ?err,
                                    text_preview = &text[..text.len().min(200)],
                                    "non-order JSON message"
                                );
                            }
                        }
                    }
                }
            }
            Message::Ping(_) => {
                // tokio-tungstenite auto-pongs.
                debug!("order update WebSocket ping received");
            }
            Message::Close(frame) => {
                info!(?frame, "order update WebSocket close frame received");
                m_active.set(0.0);
                return Ok(());
            }
            _ => {
                // Binary, Pong, Frame — ignore.
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Market hours check
// ---------------------------------------------------------------------------

/// Data collection window boundaries (IST).
/// Orders can only arrive during market hours (09:00 – 16:00 IST).
const DATA_COLLECTION_START: (u32, u32) = (9, 0);
const DATA_COLLECTION_END: (u32, u32) = (16, 0);

/// Returns `true` if the current IST time is within data collection hours
/// AND today is a trading day (not weekend, not NSE holiday).
///
/// Cold path — called once per connection lifecycle. Allocation-free.
fn is_within_market_hours(calendar: &TradingCalendar) -> bool {
    // Check trading day first (weekend + holiday check via calendar).
    if !calendar.is_trading_day_today() {
        return false;
    }

    let now_ist = Utc::now().with_timezone(&ist_offset()).time();
    is_time_within_data_collection_window(now_ist)
}

/// Pure function: checks if a given IST time falls within [09:00, 16:00).
///
/// Extracted from `is_within_market_hours` for testability.
fn is_time_within_data_collection_window(ist_time: NaiveTime) -> bool {
    #[allow(clippy::expect_used)] // APPROVED: compile-time provable constants
    let start = NaiveTime::from_hms_opt(DATA_COLLECTION_START.0, DATA_COLLECTION_START.1, 0)
        .expect("09:00:00 is always valid"); // APPROVED: compile-time provable constant
    #[allow(clippy::expect_used)] // APPROVED: compile-time provable constants
    let end = NaiveTime::from_hms_opt(DATA_COLLECTION_END.0, DATA_COLLECTION_END.1, 0)
        .expect("16:00:00 is always valid"); // APPROVED: compile-time provable constant

    ist_time >= start && ist_time < end
}

/// Computes exponential backoff delay in milliseconds for order update reconnection.
///
/// Formula: `initial * 2^(failures-1)`, capped at `max_delay_ms`.
/// Pure function — no I/O.
fn compute_reconnect_backoff_ms(consecutive_failures: u32) -> u64 {
    let shift = consecutive_failures.saturating_sub(1).min(63);
    ORDER_UPDATE_RECONNECT_INITIAL_DELAY_MS
        .saturating_mul(1_u64 << shift)
        .min(ORDER_UPDATE_RECONNECT_MAX_DELAY_MS)
}

/// Selects the appropriate read timeout for the order update WebSocket.
///
/// Always uses the longer timeout (600s) to avoid false reconnects when the
/// trader is idle — no orders for 2 minutes does NOT mean Dhan is down.
/// The order update WS only sends messages when orders arrive; idle silence
/// is normal and expected even during market hours.
///
/// Pure function — no I/O.
fn select_read_timeout_secs(_within_market_hours: bool) -> u64 {
    // Always use the longer timeout. The shorter 120s timeout caused
    // unnecessary reconnects every 2 minutes when no orders were placed.
    ORDER_UPDATE_OFF_HOURS_READ_TIMEOUT_SECS
}

// ---------------------------------------------------------------------------
// Auth response classification
// ---------------------------------------------------------------------------

/// Result of classifying the server's auth response.
#[derive(Debug, PartialEq, Eq)]
enum AuthResponseKind {
    /// Auth handshake succeeded (no error indicators found).
    Success,
    /// Auth handshake failed with the given reason.
    Failed(String),
}

/// Classifies the server's first text response after sending the LoginReq.
///
/// The Dhan order update WebSocket sends JSON responses. An auth failure
/// typically contains `"error"` or `"errorCode"` fields. If none are found,
/// the response is treated as a success (could be an ack or the first order update).
///
/// Pure function — no I/O, no allocation on success path when response is valid.
fn classify_auth_response(response_text: &str) -> AuthResponseKind {
    // Try to parse as JSON to inspect for error fields.
    match serde_json::from_str::<serde_json::Value>(response_text) {
        Ok(value) => classify_auth_json(&value),
        Err(_) => {
            // Non-JSON text response — check for error keywords.
            let lower = response_text.to_lowercase(); // O(1) EXEMPT: cold auth path, runs once per connect
            if lower.contains("error") || lower.contains("unauthorized") || lower.contains("denied")
            {
                AuthResponseKind::Failed(
                    truncate_for_log(response_text, 200).to_string(), // O(1) EXEMPT: cold error path
                )
            } else {
                // Non-JSON, non-error text — treat as success.
                AuthResponseKind::Success
            }
        }
    }
}

/// Classifies a parsed JSON auth response.
///
/// Checks for common Dhan error patterns:
/// - Top-level `errorCode` or `errorType` fields (Dhan REST error format)
/// - Nested `error` field
/// - `status` field containing failure indicators
///
/// Pure function — no I/O.
fn classify_auth_json(value: &serde_json::Value) -> AuthResponseKind {
    // Check for Dhan REST-style error response: {"errorType": "...", "errorCode": "...", "errorMessage": "..."}
    if let Some(error_code) = value.get("errorCode").and_then(|v| v.as_str()) {
        let error_msg = value
            .get("errorMessage")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown"); // O(1) EXEMPT: cold error path
        return AuthResponseKind::Failed(format!("{error_code}: {error_msg}")); // O(1) EXEMPT: cold error path
    }

    // Check for a generic "error" field.
    if let Some(error_val) = value.get("error") {
        let reason = if let Some(s) = error_val.as_str() {
            s.to_string() // O(1) EXEMPT: cold error path
        } else {
            error_val.to_string() // O(1) EXEMPT: cold error path
        };
        return AuthResponseKind::Failed(reason);
    }

    // Check for status field with failure indication.
    if let Some(status) = value.get("status").and_then(|v| v.as_str()) {
        let lower = status.to_lowercase(); // O(1) EXEMPT: cold auth path
        if lower.contains("error") || lower.contains("fail") || lower.contains("denied") {
            return AuthResponseKind::Failed(format!("status: {status}")); // O(1) EXEMPT: cold error path
        }
    }

    // No error indicators found — treat as success.
    AuthResponseKind::Success
}

/// Truncates a string for safe logging (avoids unbounded log messages).
///
/// Pure function — returns a slice, no allocation.
fn truncate_for_log(s: &str, max_len: usize) -> &str {
    if s.len() <= max_len {
        s
    } else {
        // Find a valid UTF-8 boundary at or before max_len.
        let mut end = max_len;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        &s[..end]
    }
}

/// Result of processing a connection attempt error in the reconnect loop.
#[derive(Debug, PartialEq, Eq)]
enum ReconnectAction {
    /// Reset backoff and reconnect (expected idle timeout outside hours).
    ResetAndReconnect,
    /// Increment failure counter and retry.
    IncrementAndRetry,
    /// Max attempts exhausted — stop reconnecting.
    Exhausted,
}

/// Decides the reconnect action based on error type and market hours.
///
/// During market hours, NEVER returns `Exhausted` — the app lifecycle
/// (graceful shutdown at market close) controls when connections should stop.
/// A CRITICAL alert fires at the threshold, but retrying continues.
///
/// Pure function — no I/O.
fn decide_reconnect_action(
    error_is_read_timeout: bool,
    within_market_hours: bool,
    consecutive_failures_after_increment: u32,
) -> ReconnectAction {
    if error_is_read_timeout && !within_market_hours {
        ReconnectAction::ResetAndReconnect
    } else if !within_market_hours
        && consecutive_failures_after_increment > ORDER_UPDATE_MAX_RECONNECT_ATTEMPTS
    {
        // Only give up OUTSIDE market hours. During market hours, never stop retrying.
        ReconnectAction::Exhausted
    } else {
        ReconnectAction::IncrementAndRetry
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

    #[error("auth response indicates failure: {0}")]
    AuthFailed(String),

    #[error("no auth response within {ORDER_UPDATE_AUTH_TIMEOUT_SECS}s")]
    #[allow(dead_code)] // APPROVED: kept for test coverage + future auth timeout detection
    AuthTimeout,

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

        let err = OrderUpdateConnectionError::AuthFailed("bad token".to_string());
        assert!(err.to_string().contains("auth response indicates failure"));
        assert!(err.to_string().contains("bad token"));

        let err = OrderUpdateConnectionError::AuthTimeout;
        assert!(err.to_string().contains("no auth response"));

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
        const _: () = assert!(ORDER_UPDATE_MAX_RECONNECT_ATTEMPTS >= 5);
        const _: () = assert!(ORDER_UPDATE_MAX_RECONNECT_ATTEMPTS <= 20);
    }

    #[test]
    fn test_read_timeout_is_reasonable() {
        // Must be long enough for idle periods but short enough to detect dead connections
        const _: () = assert!(ORDER_UPDATE_READ_TIMEOUT_SECS >= 30);
        const _: () = assert!(ORDER_UPDATE_READ_TIMEOUT_SECS <= 600);
    }

    #[test]
    fn test_reconnect_initial_delay_positive() {
        const _: () = assert!(ORDER_UPDATE_RECONNECT_INITIAL_DELAY_MS > 0);
        const _: () = assert!(ORDER_UPDATE_RECONNECT_INITIAL_DELAY_MS <= 10000);
    }

    #[test]
    fn test_reconnect_max_delay_greater_than_initial() {
        const _: () =
            assert!(ORDER_UPDATE_RECONNECT_MAX_DELAY_MS > ORDER_UPDATE_RECONNECT_INITIAL_DELAY_MS);
    }

    #[test]
    fn test_backoff_does_not_overflow() {
        // Ensure backoff calculation handles large failure counts
        let initial = ORDER_UPDATE_RECONNECT_INITIAL_DELAY_MS;
        let max = ORDER_UPDATE_RECONNECT_MAX_DELAY_MS;

        // Simulate many failures — must not panic or overflow
        for failures in 0..100_u32 {
            let shift = failures.min(63);
            let delay = initial.saturating_mul(1_u64 << shift).min(max);
            assert!(delay <= max, "delay exceeded max for failures={failures}");
            assert!(delay > 0, "delay must be positive for failures={failures}");
        }
    }

    #[test]
    fn test_error_variants_are_distinct() {
        let errors = [
            OrderUpdateConnectionError::NoToken.to_string(),
            OrderUpdateConnectionError::TokenExpired.to_string(),
            OrderUpdateConnectionError::Tls("x".to_string()).to_string(),
            OrderUpdateConnectionError::Connect("x".to_string()).to_string(),
            OrderUpdateConnectionError::Send("x".to_string()).to_string(),
            OrderUpdateConnectionError::AuthFailed("x".to_string()).to_string(),
            OrderUpdateConnectionError::AuthTimeout.to_string(),
            OrderUpdateConnectionError::Read("x".to_string()).to_string(),
            OrderUpdateConnectionError::ReadTimeout.to_string(),
        ];

        // All error messages should be unique
        for (i, a) in errors.iter().enumerate() {
            for (j, b) in errors.iter().enumerate() {
                if i != j {
                    assert_ne!(a, b, "error variants {i} and {j} have identical messages");
                }
            }
        }
    }

    #[test]
    fn test_error_debug_formatting() {
        let err = OrderUpdateConnectionError::Connect("connection refused".to_string());
        let debug_str = format!("{err:?}");
        assert!(debug_str.contains("Connect"));
        assert!(debug_str.contains("connection refused"));
    }

    #[test]
    fn test_backoff_doubling_pattern() {
        let initial = ORDER_UPDATE_RECONNECT_INITIAL_DELAY_MS;
        let max = ORDER_UPDATE_RECONNECT_MAX_DELAY_MS;

        let mut prev_delay = 0_u64;
        for failures in 1..=5_u32 {
            let shift = failures.saturating_sub(1).min(63);
            let delay = initial.saturating_mul(1_u64 << shift).min(max);

            if delay < max && prev_delay > 0 && prev_delay < max {
                assert_eq!(delay, prev_delay * 2, "backoff should double each attempt");
            }
            prev_delay = delay;
        }
    }

    // -----------------------------------------------------------------------
    // compute_reconnect_backoff_ms — extracted pure function
    // -----------------------------------------------------------------------

    #[test]
    fn test_compute_reconnect_backoff_zero_failures() {
        let delay = compute_reconnect_backoff_ms(0);
        assert_eq!(delay, ORDER_UPDATE_RECONNECT_INITIAL_DELAY_MS);
    }

    #[test]
    fn test_compute_reconnect_backoff_one_failure() {
        let delay = compute_reconnect_backoff_ms(1);
        assert_eq!(delay, ORDER_UPDATE_RECONNECT_INITIAL_DELAY_MS);
    }

    #[test]
    fn test_compute_reconnect_backoff_two_failures() {
        let delay = compute_reconnect_backoff_ms(2);
        assert_eq!(delay, ORDER_UPDATE_RECONNECT_INITIAL_DELAY_MS * 2);
    }

    #[test]
    fn test_compute_reconnect_backoff_three_failures() {
        let delay = compute_reconnect_backoff_ms(3);
        assert_eq!(delay, ORDER_UPDATE_RECONNECT_INITIAL_DELAY_MS * 4);
    }

    #[test]
    fn test_compute_reconnect_backoff_capped_at_max() {
        let delay = compute_reconnect_backoff_ms(50);
        assert_eq!(delay, ORDER_UPDATE_RECONNECT_MAX_DELAY_MS);
    }

    #[test]
    fn test_compute_reconnect_backoff_u32_max() {
        let delay = compute_reconnect_backoff_ms(u32::MAX);
        assert_eq!(delay, ORDER_UPDATE_RECONNECT_MAX_DELAY_MS);
    }

    // -----------------------------------------------------------------------
    // select_read_timeout_secs — extracted pure function
    // -----------------------------------------------------------------------

    #[test]
    fn test_select_read_timeout_always_uses_long_timeout() {
        // Always uses 600s to avoid false reconnects when trader is idle.
        // No market-hours distinction — order WS only sends when orders arrive.
        let during_hours = select_read_timeout_secs(true);
        let off_hours = select_read_timeout_secs(false);
        assert_eq!(during_hours, ORDER_UPDATE_OFF_HOURS_READ_TIMEOUT_SECS);
        assert_eq!(off_hours, ORDER_UPDATE_OFF_HOURS_READ_TIMEOUT_SECS);
        assert_eq!(during_hours, off_hours);
    }

    // -----------------------------------------------------------------------
    // is_time_within_data_collection_window — extracted pure function
    // -----------------------------------------------------------------------

    #[test]
    fn test_time_before_market_open() {
        let time = NaiveTime::from_hms_opt(8, 59, 59).unwrap();
        assert!(!is_time_within_data_collection_window(time));
    }

    #[test]
    fn test_time_at_market_open() {
        let time = NaiveTime::from_hms_opt(9, 0, 0).unwrap();
        assert!(is_time_within_data_collection_window(time));
    }

    #[test]
    fn test_time_during_market_hours() {
        let time = NaiveTime::from_hms_opt(12, 30, 0).unwrap();
        assert!(is_time_within_data_collection_window(time));
    }

    #[test]
    fn test_time_just_before_market_close() {
        let time = NaiveTime::from_hms_opt(15, 59, 59).unwrap();
        assert!(is_time_within_data_collection_window(time));
    }

    #[test]
    fn test_time_at_market_close() {
        // 16:00 is exclusive — should return false
        let time = NaiveTime::from_hms_opt(16, 0, 0).unwrap();
        assert!(!is_time_within_data_collection_window(time));
    }

    #[test]
    fn test_time_after_market_close() {
        let time = NaiveTime::from_hms_opt(18, 0, 0).unwrap();
        assert!(!is_time_within_data_collection_window(time));
    }

    #[test]
    fn test_time_midnight() {
        let time = NaiveTime::from_hms_opt(0, 0, 0).unwrap();
        assert!(!is_time_within_data_collection_window(time));
    }

    // -----------------------------------------------------------------------
    // decide_reconnect_action — extracted pure function
    // -----------------------------------------------------------------------

    #[test]
    fn test_reconnect_action_timeout_outside_hours_resets() {
        let action = decide_reconnect_action(true, false, 5);
        assert_eq!(action, ReconnectAction::ResetAndReconnect);
    }

    #[test]
    fn test_reconnect_action_timeout_during_hours_increments() {
        let action = decide_reconnect_action(true, true, 3);
        assert_eq!(action, ReconnectAction::IncrementAndRetry);
    }

    #[test]
    fn test_reconnect_action_non_timeout_during_hours_increments() {
        let action = decide_reconnect_action(false, true, 3);
        assert_eq!(action, ReconnectAction::IncrementAndRetry);
    }

    #[test]
    fn test_reconnect_action_non_timeout_outside_hours_increments() {
        let action = decide_reconnect_action(false, false, 3);
        assert_eq!(action, ReconnectAction::IncrementAndRetry);
    }

    #[test]
    fn test_reconnect_action_never_exhausted_during_market_hours() {
        // During market hours, NEVER give up — infinite resilience mode.
        let action = decide_reconnect_action(false, true, ORDER_UPDATE_MAX_RECONNECT_ATTEMPTS + 1);
        assert_eq!(action, ReconnectAction::IncrementAndRetry);
    }

    #[test]
    fn test_reconnect_action_exhausted_outside_market_hours() {
        // Outside market hours, respect the max attempts limit.
        let action = decide_reconnect_action(false, false, ORDER_UPDATE_MAX_RECONNECT_ATTEMPTS + 1);
        assert_eq!(action, ReconnectAction::Exhausted);
    }

    #[test]
    fn test_reconnect_action_at_max_not_exhausted() {
        let action = decide_reconnect_action(false, true, ORDER_UPDATE_MAX_RECONNECT_ATTEMPTS);
        assert_eq!(action, ReconnectAction::IncrementAndRetry);
    }

    #[test]
    fn test_reconnect_action_timeout_outside_hours_even_at_max() {
        // Off-hours timeout always resets, even if we're at max attempts
        let action = decide_reconnect_action(true, false, ORDER_UPDATE_MAX_RECONNECT_ATTEMPTS + 1);
        assert_eq!(action, ReconnectAction::ResetAndReconnect);
    }

    // -----------------------------------------------------------------------
    // OrderUpdateConnectionError — additional coverage
    // -----------------------------------------------------------------------

    #[test]
    fn test_error_is_read_timeout_check() {
        // Verify the pattern matching used in reconnect logic
        let timeout = OrderUpdateConnectionError::ReadTimeout;
        assert!(matches!(timeout, OrderUpdateConnectionError::ReadTimeout));

        let connect = OrderUpdateConnectionError::Connect("test".to_string());
        assert!(!matches!(connect, OrderUpdateConnectionError::ReadTimeout));
    }

    #[test]
    fn test_no_token_error_display() {
        let err = OrderUpdateConnectionError::NoToken;
        assert_eq!(err.to_string(), "no authentication token available");
    }

    #[test]
    fn test_token_expired_error_display() {
        let err = OrderUpdateConnectionError::TokenExpired;
        assert_eq!(err.to_string(), "authentication token expired");
    }

    #[test]
    fn test_read_timeout_error_display_contains_timeout_value() {
        let err = OrderUpdateConnectionError::ReadTimeout;
        let msg = err.to_string();
        assert!(
            msg.contains(&ORDER_UPDATE_READ_TIMEOUT_SECS.to_string()),
            "ReadTimeout message should contain the timeout value"
        );
    }

    #[test]
    fn test_tls_error_display() {
        let err = OrderUpdateConnectionError::Tls("cert expired".to_string());
        let msg = err.to_string();
        assert!(msg.contains("TLS"));
        assert!(msg.contains("cert expired"));
    }

    #[test]
    fn test_send_error_display() {
        let err = OrderUpdateConnectionError::Send("broken pipe".to_string());
        let msg = err.to_string();
        assert!(msg.contains("login message"));
        assert!(msg.contains("broken pipe"));
    }

    // -----------------------------------------------------------------------
    // compute_reconnect_backoff_ms — additional edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn test_compute_reconnect_backoff_monotonically_increases() {
        let mut prev = 0_u64;
        for failures in 0..=10 {
            let delay = compute_reconnect_backoff_ms(failures);
            assert!(
                delay >= prev,
                "backoff must not decrease: {prev} -> {delay} at failures={failures}"
            );
            prev = delay;
        }
    }

    #[test]
    fn test_compute_reconnect_backoff_always_positive() {
        for failures in 0..=100 {
            let delay = compute_reconnect_backoff_ms(failures);
            assert!(delay > 0, "delay must be positive for failures={failures}");
        }
    }

    #[test]
    fn test_compute_reconnect_backoff_capped_consistently() {
        // Once capped, all higher failure counts return the same max
        let max_delay = ORDER_UPDATE_RECONNECT_MAX_DELAY_MS;
        let high = compute_reconnect_backoff_ms(30);
        let higher = compute_reconnect_backoff_ms(50);
        let highest = compute_reconnect_backoff_ms(u32::MAX);
        assert_eq!(high, max_delay);
        assert_eq!(higher, max_delay);
        assert_eq!(highest, max_delay);
    }

    // -----------------------------------------------------------------------
    // select_read_timeout_secs — additional coverage
    // -----------------------------------------------------------------------

    #[test]
    fn test_select_read_timeout_returns_600s_regardless_of_input() {
        // Both true and false must return the same long timeout (600s).
        // This prevents false reconnects when no orders are placed.
        assert_eq!(
            select_read_timeout_secs(true),
            ORDER_UPDATE_OFF_HOURS_READ_TIMEOUT_SECS
        );
        assert_eq!(
            select_read_timeout_secs(false),
            ORDER_UPDATE_OFF_HOURS_READ_TIMEOUT_SECS
        );
    }

    // -----------------------------------------------------------------------
    // is_time_within_data_collection_window — boundary saturation
    // -----------------------------------------------------------------------

    #[test]
    fn test_time_at_0915_is_within_window() {
        // Pre-open ends at 09:15 IST, continuous trading starts — well within window
        let time = NaiveTime::from_hms_opt(9, 15, 0).unwrap();
        assert!(is_time_within_data_collection_window(time));
    }

    #[test]
    fn test_time_at_1530_is_within_window() {
        // 15:30 IST is still within [09:00, 16:00) order data collection window
        let time = NaiveTime::from_hms_opt(15, 30, 0).unwrap();
        assert!(is_time_within_data_collection_window(time));
    }

    #[test]
    fn test_time_at_2359_is_outside_window() {
        let time = NaiveTime::from_hms_opt(23, 59, 59).unwrap();
        assert!(!is_time_within_data_collection_window(time));
    }

    // -----------------------------------------------------------------------
    // decide_reconnect_action — exhaustive matrix
    // -----------------------------------------------------------------------

    #[test]
    fn test_reconnect_action_exhausted_outside_hours_non_timeout() {
        // Non-timeout outside hours at max+1 → exhausted (timeout check is first)
        let action = decide_reconnect_action(false, false, ORDER_UPDATE_MAX_RECONNECT_ATTEMPTS + 1);
        assert_eq!(action, ReconnectAction::Exhausted);
    }

    #[test]
    fn test_reconnect_action_zero_failures_increments() {
        // First failure (tentative_failures=1) should increment
        let action = decide_reconnect_action(false, true, 1);
        assert_eq!(action, ReconnectAction::IncrementAndRetry);
    }

    #[test]
    fn test_reconnect_action_timeout_outside_hours_at_zero_failures() {
        let action = decide_reconnect_action(true, false, 0);
        assert_eq!(action, ReconnectAction::ResetAndReconnect);
    }

    // -----------------------------------------------------------------------
    // DATA_COLLECTION constants
    // -----------------------------------------------------------------------

    #[test]
    fn test_data_collection_window_constants() {
        assert_eq!(DATA_COLLECTION_START, (9, 0));
        assert_eq!(DATA_COLLECTION_END, (16, 0));
    }

    // -----------------------------------------------------------------------
    // ReconnectAction derive coverage
    // -----------------------------------------------------------------------

    #[test]
    fn test_reconnect_action_debug() {
        let action = ReconnectAction::ResetAndReconnect;
        let debug = format!("{action:?}");
        assert!(debug.contains("ResetAndReconnect"));
    }

    #[test]
    fn test_reconnect_action_all_variants_distinct() {
        let variants = [
            ReconnectAction::ResetAndReconnect,
            ReconnectAction::IncrementAndRetry,
            ReconnectAction::Exhausted,
        ];
        for (i, a) in variants.iter().enumerate() {
            for (j, b) in variants.iter().enumerate() {
                if i != j {
                    assert_ne!(a, b, "variants {i} and {j} must be distinct");
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // classify_auth_response — auth validation (H3)
    // -----------------------------------------------------------------------

    /// Tests auth validation logic for the order update WebSocket.
    ///
    /// After sending LoginReq (MsgCode 42), the server responds with JSON.
    /// `classify_auth_response` classifies the response as success or failure.
    #[test]
    fn test_order_update_ws_auth_validation() {
        // Dhan REST-style error response — auth failure with errorCode.
        let dhan_error = r#"{"errorType":"AUTHENTICATION_ERROR","errorCode":"DH-901","errorMessage":"Invalid access token"}"#;
        let result = classify_auth_response(dhan_error);
        assert_eq!(
            result,
            AuthResponseKind::Failed("DH-901: Invalid access token".to_string())
        );

        // Valid order update message — should be treated as success (auth passed).
        let order_update = r#"{"Data":{"OrderNo":"123","Status":"TRADED"},"Type":"order_alert"}"#;
        let result = classify_auth_response(order_update);
        assert_eq!(result, AuthResponseKind::Success);

        // Empty JSON object — no error fields, treat as success (ack).
        let empty_json = "{}";
        let result = classify_auth_response(empty_json);
        assert_eq!(result, AuthResponseKind::Success);

        // Non-JSON error text from server.
        let text_error = "Unauthorized: invalid token";
        let result = classify_auth_response(text_error);
        assert!(matches!(result, AuthResponseKind::Failed(_)));

        // Non-JSON non-error text — treat as success.
        let text_ok = "connected";
        let result = classify_auth_response(text_ok);
        assert_eq!(result, AuthResponseKind::Success);
    }

    #[test]
    fn test_classify_auth_response_dhan_error_code() {
        // Dhan REST error format: {"errorType": "...", "errorCode": "...", "errorMessage": "..."}
        let json = r#"{"errorType":"DATA_API_ERROR","errorCode":"DH-904","errorMessage":"Rate limit exceeded"}"#;
        let result = classify_auth_response(json);
        assert_eq!(
            result,
            AuthResponseKind::Failed("DH-904: Rate limit exceeded".to_string())
        );
    }

    #[test]
    fn test_classify_auth_response_error_code_without_message() {
        let json = r#"{"errorCode":"DH-901"}"#;
        let result = classify_auth_response(json);
        assert_eq!(
            result,
            AuthResponseKind::Failed("DH-901: unknown".to_string())
        );
    }

    #[test]
    fn test_classify_auth_response_generic_error_field_string() {
        let json = r#"{"error":"authentication failed"}"#;
        let result = classify_auth_response(json);
        assert_eq!(
            result,
            AuthResponseKind::Failed("authentication failed".to_string())
        );
    }

    #[test]
    fn test_classify_auth_response_generic_error_field_object() {
        let json = r#"{"error":{"code":401,"message":"unauthorized"}}"#;
        let result = classify_auth_response(json);
        match result {
            AuthResponseKind::Failed(reason) => {
                assert!(reason.contains("401"));
                assert!(reason.contains("unauthorized"));
            }
            AuthResponseKind::Success => panic!("expected failure"),
        }
    }

    #[test]
    fn test_classify_auth_response_status_field_failure() {
        let json = r#"{"status":"error","message":"token expired"}"#;
        let result = classify_auth_response(json);
        assert_eq!(
            result,
            AuthResponseKind::Failed("status: error".to_string())
        );
    }

    #[test]
    fn test_classify_auth_response_status_field_failed() {
        let json = r#"{"status":"failed"}"#;
        let result = classify_auth_response(json);
        assert_eq!(
            result,
            AuthResponseKind::Failed("status: failed".to_string())
        );
    }

    #[test]
    fn test_classify_auth_response_status_field_denied() {
        let json = r#"{"status":"access denied"}"#;
        let result = classify_auth_response(json);
        assert_eq!(
            result,
            AuthResponseKind::Failed("status: access denied".to_string())
        );
    }

    #[test]
    fn test_classify_auth_response_status_field_success() {
        // Status that does NOT indicate failure should be treated as success.
        let json = r#"{"status":"ok"}"#;
        let result = classify_auth_response(json);
        assert_eq!(result, AuthResponseKind::Success);
    }

    #[test]
    fn test_classify_auth_response_non_json_error_keywords() {
        let inputs_and_expected = [
            ("Error: connection refused", true),
            ("unauthorized access", true),
            ("access denied by server", true),
            ("welcome to the server", false),
            ("pong", false),
            ("", false),
        ];

        for (input, should_fail) in inputs_and_expected {
            let result = classify_auth_response(input);
            if should_fail {
                assert!(
                    matches!(result, AuthResponseKind::Failed(_)),
                    "expected failure for: {input}"
                );
            } else {
                assert_eq!(
                    result,
                    AuthResponseKind::Success,
                    "expected success for: {input}"
                );
            }
        }
    }

    #[test]
    fn test_classify_auth_response_valid_order_update_is_success() {
        // If the first message after login is already an order update, auth succeeded.
        let json = r#"{
            "Data": {
                "OrderNo": "999",
                "Status": "PENDING",
                "Symbol": "NIFTY"
            },
            "Type": "order_alert"
        }"#;
        let result = classify_auth_response(json);
        assert_eq!(result, AuthResponseKind::Success);
    }

    #[test]
    fn test_classify_auth_response_error_code_takes_precedence_over_status() {
        // errorCode should be detected before status field.
        let json = r#"{"errorCode":"DH-901","errorMessage":"bad token","status":"ok"}"#;
        let result = classify_auth_response(json);
        assert_eq!(
            result,
            AuthResponseKind::Failed("DH-901: bad token".to_string())
        );
    }

    // -----------------------------------------------------------------------
    // truncate_for_log — pure function
    // -----------------------------------------------------------------------

    #[test]
    fn test_truncate_for_log_short_string() {
        let s = "hello";
        assert_eq!(truncate_for_log(s, 200), "hello");
    }

    #[test]
    fn test_truncate_for_log_exact_length() {
        let s = "abcde";
        assert_eq!(truncate_for_log(s, 5), "abcde");
    }

    #[test]
    fn test_truncate_for_log_long_string() {
        let s = "a".repeat(300);
        let truncated = truncate_for_log(&s, 200);
        assert_eq!(truncated.len(), 200);
    }

    #[test]
    fn test_truncate_for_log_empty_string() {
        assert_eq!(truncate_for_log("", 200), "");
    }

    #[test]
    fn test_truncate_for_log_multibyte_boundary() {
        // Multi-byte character that would be split at max_len.
        let s = "abc\u{00E9}def"; // 'e' with accent = 2 bytes in UTF-8
        // "abc" = 3 bytes, "\u{00E9}" = 2 bytes (bytes 3-4), "def" = 3 bytes
        // Truncate at 4 would split the multi-byte char — should back up to 3.
        let truncated = truncate_for_log(s, 4);
        assert!(truncated.len() <= 4);
        assert!(truncated.is_char_boundary(truncated.len()));
    }

    // -----------------------------------------------------------------------
    // AuthResponseKind — derive coverage
    // -----------------------------------------------------------------------

    #[test]
    fn test_auth_response_kind_debug() {
        let success = AuthResponseKind::Success;
        assert!(format!("{success:?}").contains("Success"));

        let failed = AuthResponseKind::Failed("reason".to_string());
        assert!(format!("{failed:?}").contains("Failed"));
        assert!(format!("{failed:?}").contains("reason"));
    }

    #[test]
    fn test_auth_response_kind_eq() {
        assert_eq!(AuthResponseKind::Success, AuthResponseKind::Success);
        assert_eq!(
            AuthResponseKind::Failed("a".to_string()),
            AuthResponseKind::Failed("a".to_string())
        );
        assert_ne!(
            AuthResponseKind::Success,
            AuthResponseKind::Failed("a".to_string())
        );
        assert_ne!(
            AuthResponseKind::Failed("a".to_string()),
            AuthResponseKind::Failed("b".to_string())
        );
    }

    // -----------------------------------------------------------------------
    // AuthFailed / AuthTimeout error variants — display coverage
    // -----------------------------------------------------------------------

    #[test]
    fn test_auth_failed_error_display() {
        let err = OrderUpdateConnectionError::AuthFailed("DH-901: bad token".to_string());
        let msg = err.to_string();
        assert!(msg.contains("auth response indicates failure"));
        assert!(msg.contains("DH-901: bad token"));
    }

    #[test]
    fn test_auth_timeout_error_display() {
        let err = OrderUpdateConnectionError::AuthTimeout;
        let msg = err.to_string();
        assert!(msg.contains("no auth response"));
        assert!(
            msg.contains(&ORDER_UPDATE_AUTH_TIMEOUT_SECS.to_string()),
            "AuthTimeout message should contain the timeout value"
        );
    }

    #[test]
    fn test_auth_failed_debug_formatting() {
        let err = OrderUpdateConnectionError::AuthFailed("test reason".to_string());
        let debug = format!("{err:?}");
        assert!(debug.contains("AuthFailed"));
        assert!(debug.contains("test reason"));
    }

    #[test]
    fn test_auth_timeout_debug_formatting() {
        let err = OrderUpdateConnectionError::AuthTimeout;
        let debug = format!("{err:?}");
        assert!(debug.contains("AuthTimeout"));
    }
}
