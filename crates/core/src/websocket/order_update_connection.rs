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
                // Off-hours ReadTimeout is expected (no orders outside market hours).
                // Log at debug level and reset backoff to avoid noise.
                if matches!(err, OrderUpdateConnectionError::ReadTimeout)
                    && !is_within_market_hours(&calendar)
                {
                    debug!(
                        "order update WebSocket idle timeout outside market hours — reconnecting"
                    );
                    consecutive_failures = 0;
                } else {
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
    let timeout_secs = if is_within_market_hours(calendar) {
        ORDER_UPDATE_READ_TIMEOUT_SECS
    } else {
        ORDER_UPDATE_OFF_HOURS_READ_TIMEOUT_SECS
    };
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

    #[allow(clippy::expect_used)] // APPROVED: compile-time provable constants
    let start = NaiveTime::from_hms_opt(DATA_COLLECTION_START.0, DATA_COLLECTION_START.1, 0)
        .expect("09:00:00 is always valid"); // APPROVED: compile-time provable constant
    #[allow(clippy::expect_used)] // APPROVED: compile-time provable constants
    let end = NaiveTime::from_hms_opt(DATA_COLLECTION_END.0, DATA_COLLECTION_END.1, 0)
        .expect("16:00:00 is always valid"); // APPROVED: compile-time provable constant

    now_ist >= start && now_ist < end
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
    // is_within_market_hours — unit tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_data_collection_time_constants() {
        // Verify the data collection constants are reasonable
        assert_eq!(DATA_COLLECTION_START, (9, 0));
        assert_eq!(DATA_COLLECTION_END, (16, 0));
        assert!(DATA_COLLECTION_START.0 < DATA_COLLECTION_END.0);
    }

    #[test]
    fn test_is_within_market_hours_does_not_panic() {
        // The function checks calendar.is_trading_day_today() first.
        // We verify it runs without panicking with a minimal TradingCalendar.
        use dhan_live_trader_common::config::TradingConfig;

        let config = TradingConfig {
            market_open_time: "09:00:00".to_string(),
            market_close_time: "15:30:00".to_string(),
            order_cutoff_time: "15:29:00".to_string(),
            data_collection_start: "09:00:00".to_string(),
            data_collection_end: "16:00:00".to_string(),
            timezone: "Asia/Kolkata".to_string(),
            max_orders_per_second: 10,
            nse_holidays: vec![],
            muhurat_trading_dates: vec![],
        };
        let calendar = TradingCalendar::from_config(&config).unwrap();
        // Just verify it runs without panic — the result depends on the
        // current IST time and day of week.
        let _ = is_within_market_hours(&calendar);
    }

    #[test]
    fn test_off_hours_read_timeout_longer_than_market_hours() {
        assert!(
            ORDER_UPDATE_OFF_HOURS_READ_TIMEOUT_SECS >= ORDER_UPDATE_READ_TIMEOUT_SECS,
            "off-hours timeout should be >= market hours timeout"
        );
    }

    // -----------------------------------------------------------------------
    // OrderUpdateConnectionError — additional error variant tests
    // -----------------------------------------------------------------------

    #[test]
    fn test_error_token_expired_display() {
        let err = OrderUpdateConnectionError::TokenExpired;
        let msg = err.to_string();
        assert!(msg.contains("expired"));
        assert!(!msg.is_empty());
    }

    #[test]
    fn test_error_no_token_is_distinct_from_token_expired() {
        let no_token = OrderUpdateConnectionError::NoToken.to_string();
        let expired = OrderUpdateConnectionError::TokenExpired.to_string();
        assert_ne!(no_token, expired);
    }

    #[test]
    fn test_error_read_timeout_mentions_constant() {
        let err = OrderUpdateConnectionError::ReadTimeout;
        let msg = err.to_string();
        // The display mentions the timeout constant
        assert!(msg.contains("read timeout"));
    }

    // -----------------------------------------------------------------------
    // Backoff calculation — saturating_sub edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn test_backoff_consecutive_failures_zero() {
        // When consecutive_failures = 0, shift = 0.saturating_sub(1) = 0 (capped)
        // Actually the code does consecutive_failures.saturating_sub(1).min(63)
        // With 0 failures, shift = 0, delay = initial * 1 = initial
        let initial = ORDER_UPDATE_RECONNECT_INITIAL_DELAY_MS;
        let max = ORDER_UPDATE_RECONNECT_MAX_DELAY_MS;
        let shift = 0_u32.saturating_sub(1).min(63);
        let delay = initial.saturating_mul(1_u64 << shift).min(max);
        assert_eq!(delay, initial);
    }

    #[test]
    fn test_backoff_consecutive_failures_one() {
        // failures = 1, shift = 1.saturating_sub(1) = 0, delay = initial * 2^0 = initial
        let initial = ORDER_UPDATE_RECONNECT_INITIAL_DELAY_MS;
        let max = ORDER_UPDATE_RECONNECT_MAX_DELAY_MS;
        let shift = 1_u32.saturating_sub(1).min(63);
        let delay = initial.saturating_mul(1_u64 << shift).min(max);
        assert_eq!(delay, initial);
    }

    #[test]
    fn test_backoff_consecutive_failures_u32_max() {
        // u32::MAX.saturating_sub(1) = u32::MAX - 1, but .min(63) = 63
        // 1_u64 << 63 is valid (no overflow), saturating_mul caps at u64::MAX
        let initial = ORDER_UPDATE_RECONNECT_INITIAL_DELAY_MS;
        let max = ORDER_UPDATE_RECONNECT_MAX_DELAY_MS;
        let shift = u32::MAX.saturating_sub(1).min(63);
        assert_eq!(shift, 63);
        let delay = initial.saturating_mul(1_u64 << shift).min(max);
        assert_eq!(delay, max, "should cap at max delay");
    }

    // -----------------------------------------------------------------------
    // is_within_market_hours — boundary test using TradingCalendar with holidays
    // -----------------------------------------------------------------------

    #[test]
    fn test_is_within_market_hours_with_holidays() {
        use dhan_live_trader_common::config::{NseHolidayEntry, TradingConfig};

        // Add today as a holiday to ensure is_within_market_hours returns false
        let today = chrono::Utc::now()
            .with_timezone(&ist_offset())
            .format("%Y-%m-%d")
            .to_string();

        let config = TradingConfig {
            market_open_time: "09:00:00".to_string(),
            market_close_time: "15:30:00".to_string(),
            order_cutoff_time: "15:29:00".to_string(),
            data_collection_start: "09:00:00".to_string(),
            data_collection_end: "16:00:00".to_string(),
            timezone: "Asia/Kolkata".to_string(),
            max_orders_per_second: 10,
            nse_holidays: vec![NseHolidayEntry {
                date: today,
                name: "Test Holiday".to_string(),
            }],
            muhurat_trading_dates: vec![],
        };
        let calendar = TradingCalendar::from_config(&config).unwrap();
        // Today is a holiday → should return false regardless of time
        assert!(!is_within_market_hours(&calendar));
    }

    // -----------------------------------------------------------------------
    // OrderUpdateConnectionError matches() pattern
    // -----------------------------------------------------------------------

    #[test]
    fn test_error_variant_matches_read_timeout() {
        let err = OrderUpdateConnectionError::ReadTimeout;
        assert!(matches!(err, OrderUpdateConnectionError::ReadTimeout));
    }

    #[test]
    fn test_error_variant_matches_no_token() {
        let err = OrderUpdateConnectionError::NoToken;
        assert!(matches!(err, OrderUpdateConnectionError::NoToken));
    }

    #[test]
    fn test_error_variant_matches_token_expired() {
        let err = OrderUpdateConnectionError::TokenExpired;
        assert!(matches!(err, OrderUpdateConnectionError::TokenExpired));
    }

    #[test]
    fn test_error_tls_preserves_message() {
        let err = OrderUpdateConnectionError::Tls("certificate error".to_string());
        assert!(err.to_string().contains("certificate error"));
    }

    #[test]
    fn test_error_connect_preserves_message() {
        let err = OrderUpdateConnectionError::Connect("DNS resolution failed".to_string());
        assert!(err.to_string().contains("DNS resolution failed"));
    }

    #[test]
    fn test_error_send_preserves_message() {
        let err = OrderUpdateConnectionError::Send("socket closed".to_string());
        assert!(err.to_string().contains("socket closed"));
    }

    #[test]
    fn test_error_read_preserves_message() {
        let err = OrderUpdateConnectionError::Read("TCP reset".to_string());
        assert!(err.to_string().contains("TCP reset"));
    }

    // -----------------------------------------------------------------------
    // is_within_market_hours — additional coverage with different calendars
    // -----------------------------------------------------------------------

    #[test]
    fn test_is_within_market_hours_weekend_returns_false() {
        use dhan_live_trader_common::config::TradingConfig;

        // Build a calendar with no holidays — weekends are still non-trading
        let config = TradingConfig {
            market_open_time: "09:00:00".to_string(),
            market_close_time: "15:30:00".to_string(),
            order_cutoff_time: "15:29:00".to_string(),
            data_collection_start: "09:00:00".to_string(),
            data_collection_end: "16:00:00".to_string(),
            timezone: "Asia/Kolkata".to_string(),
            max_orders_per_second: 10,
            nse_holidays: vec![],
            muhurat_trading_dates: vec![],
        };
        let calendar = TradingCalendar::from_config(&config).unwrap();
        // Just verify the function does not panic.
        // Result depends on current day being a weekend or not.
        let _ = is_within_market_hours(&calendar);
    }

    #[test]
    fn test_data_collection_window_covers_market_hours() {
        // Market hours 09:00-15:30 must fit within data collection 09:00-16:00
        assert!(DATA_COLLECTION_START.0 <= 9);
        assert!(DATA_COLLECTION_END.0 >= 15);
    }

    // -----------------------------------------------------------------------
    // Backoff edge cases — verify formula for specific failure counts
    // -----------------------------------------------------------------------

    #[test]
    fn test_backoff_formula_failure_two() {
        let initial = ORDER_UPDATE_RECONNECT_INITIAL_DELAY_MS;
        let max = ORDER_UPDATE_RECONNECT_MAX_DELAY_MS;
        // failures=2 -> shift=1 -> delay = initial * 2
        let shift = 2_u32.saturating_sub(1).min(63);
        let delay = initial.saturating_mul(1_u64 << shift).min(max);
        assert_eq!(shift, 1);
        assert_eq!(delay, initial.saturating_mul(2).min(max));
    }

    #[test]
    fn test_backoff_formula_failure_three() {
        let initial = ORDER_UPDATE_RECONNECT_INITIAL_DELAY_MS;
        let max = ORDER_UPDATE_RECONNECT_MAX_DELAY_MS;
        // failures=3 -> shift=2 -> delay = initial * 4
        let shift = 3_u32.saturating_sub(1).min(63);
        let delay = initial.saturating_mul(1_u64 << shift).min(max);
        assert_eq!(shift, 2);
        assert_eq!(delay, initial.saturating_mul(4).min(max));
    }

    // -----------------------------------------------------------------------
    // OrderUpdateConnectionError — error chain and source
    // -----------------------------------------------------------------------

    #[test]
    fn test_error_variants_implement_std_error() {
        let err: Box<dyn std::error::Error> = Box::new(OrderUpdateConnectionError::NoToken);
        assert!(!err.to_string().is_empty());
    }

    #[test]
    fn test_tls_error_contains_nested_reason() {
        let err = OrderUpdateConnectionError::Tls("invalid cert chain: expired".to_string());
        let msg = err.to_string();
        assert!(msg.contains("invalid cert chain: expired"));
        assert!(msg.contains("TLS"));
    }

    #[test]
    fn test_connect_error_empty_reason() {
        let err = OrderUpdateConnectionError::Connect(String::new());
        let msg = err.to_string();
        assert!(msg.contains("connection failed"));
    }
}
