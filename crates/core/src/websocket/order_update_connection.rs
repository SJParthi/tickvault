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

use dhan_live_trader_common::constants::{
    ORDER_UPDATE_MAX_RECONNECT_ATTEMPTS, ORDER_UPDATE_OFF_HOURS_READ_TIMEOUT_SECS,
    ORDER_UPDATE_READ_TIMEOUT_SECS, ORDER_UPDATE_RECONNECT_INITIAL_DELAY_MS,
    ORDER_UPDATE_RECONNECT_MAX_DELAY_MS,
};
use dhan_live_trader_common::order_types::OrderUpdate;
use dhan_live_trader_common::trading_calendar::{TradingCalendar, ist_offset};

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
                        warn!(
                            ?err,
                            attempt = consecutive_failures,
                            "order update WebSocket error — will reconnect"
                        );
                    }
                }
            }
        }

        // Exponential backoff: delay_ms = initial * 2^(failures-1), capped at max.
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
    let access_token = token_state.access_token().expose_secret().to_string();

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

    let (mut write, mut read) = ws_stream.split();

    // Send login message.
    let login_msg = build_order_update_login(client_id, &access_token);
    write
        .send(Message::Text(login_msg.into()))
        .await
        .map_err(|err| OrderUpdateConnectionError::Send(err.to_string()))?; // O(1) EXEMPT: cold path, once per connect

    debug!("order update WebSocket login sent");

    // Use longer timeout outside market hours to avoid reconnect noise.
    let timeout_secs = select_read_timeout_secs(is_within_market_hours(calendar));
    let read_timeout = Duration::from_secs(timeout_secs);

    // Read loop — receive order updates until disconnect.
    loop {
        let msg = match time::timeout(read_timeout, read.next()).await {
            Ok(Some(Ok(msg))) => msg,
            Ok(Some(Err(err))) => {
                // O(1) EXEMPT: error path, not hot path
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

/// Selects the appropriate read timeout based on market hours.
///
/// During market hours: shorter timeout to detect dead connections quickly.
/// Outside market hours: longer timeout to avoid reconnect noise.
///
/// Pure function — no I/O.
fn select_read_timeout_secs(within_market_hours: bool) -> u64 {
    if within_market_hours {
        ORDER_UPDATE_READ_TIMEOUT_SECS
    } else {
        ORDER_UPDATE_OFF_HOURS_READ_TIMEOUT_SECS
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
/// Pure function — no I/O.
fn decide_reconnect_action(
    error_is_read_timeout: bool,
    within_market_hours: bool,
    consecutive_failures_after_increment: u32,
) -> ReconnectAction {
    if error_is_read_timeout && !within_market_hours {
        ReconnectAction::ResetAndReconnect
    } else if consecutive_failures_after_increment > ORDER_UPDATE_MAX_RECONNECT_ATTEMPTS {
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
    fn test_select_read_timeout_during_market_hours() {
        let secs = select_read_timeout_secs(true);
        assert_eq!(secs, ORDER_UPDATE_READ_TIMEOUT_SECS);
    }

    #[test]
    fn test_select_read_timeout_outside_market_hours() {
        let secs = select_read_timeout_secs(false);
        assert_eq!(secs, ORDER_UPDATE_OFF_HOURS_READ_TIMEOUT_SECS);
    }

    #[test]
    fn test_off_hours_timeout_greater_than_market_hours() {
        assert!(
            select_read_timeout_secs(false) > select_read_timeout_secs(true),
            "off-hours timeout must be longer than market-hours timeout"
        );
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
    fn test_reconnect_action_exhausted() {
        let action = decide_reconnect_action(false, true, ORDER_UPDATE_MAX_RECONNECT_ATTEMPTS + 1);
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
}
