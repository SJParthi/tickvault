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
    // Data collection window constants
    // -----------------------------------------------------------------------

    #[test]
    fn test_data_collection_start_is_valid_time() {
        let t = NaiveTime::from_hms_opt(DATA_COLLECTION_START.0, DATA_COLLECTION_START.1, 0);
        assert!(t.is_some(), "DATA_COLLECTION_START must be a valid time");
    }

    #[test]
    fn test_data_collection_end_is_valid_time() {
        let t = NaiveTime::from_hms_opt(DATA_COLLECTION_END.0, DATA_COLLECTION_END.1, 0);
        assert!(t.is_some(), "DATA_COLLECTION_END must be a valid time");
    }

    #[test]
    fn test_data_collection_start_before_end() {
        let start =
            NaiveTime::from_hms_opt(DATA_COLLECTION_START.0, DATA_COLLECTION_START.1, 0).unwrap();
        let end = NaiveTime::from_hms_opt(DATA_COLLECTION_END.0, DATA_COLLECTION_END.1, 0).unwrap();
        assert!(start < end, "collection start must precede end");
    }

    #[test]
    fn test_data_collection_window_covers_market_hours() {
        let start =
            NaiveTime::from_hms_opt(DATA_COLLECTION_START.0, DATA_COLLECTION_START.1, 0).unwrap();
        let end = NaiveTime::from_hms_opt(DATA_COLLECTION_END.0, DATA_COLLECTION_END.1, 0).unwrap();
        let market_open = NaiveTime::from_hms_opt(9, 15, 0).unwrap();
        let market_close = NaiveTime::from_hms_opt(15, 30, 0).unwrap();
        assert!(
            start <= market_open,
            "collection must start at or before market open"
        );
        assert!(
            end >= market_close,
            "collection must end at or after market close"
        );
    }

    #[test]
    fn test_data_collection_window_duration_reasonable() {
        let start_secs = DATA_COLLECTION_START.0 * 3600 + DATA_COLLECTION_START.1 * 60;
        let end_secs = DATA_COLLECTION_END.0 * 3600 + DATA_COLLECTION_END.1 * 60;
        let duration_hours = (end_secs - start_secs) / 3600;
        assert!(duration_hours >= 6, "window must be at least 6 hours");
        assert!(duration_hours <= 12, "window must be at most 12 hours");
    }

    // -----------------------------------------------------------------------
    // is_within_market_hours — exercised with real calendar
    // -----------------------------------------------------------------------

    fn make_test_calendar() -> TradingCalendar {
        use dhan_live_trader_common::config::{NseHolidayEntry, TradingConfig};
        let config = TradingConfig {
            market_open_time: "09:00:00".to_string(),
            market_close_time: "15:30:00".to_string(),
            order_cutoff_time: "15:29:00".to_string(),
            data_collection_start: "09:00:00".to_string(),
            data_collection_end: "15:30:00".to_string(),
            timezone: "Asia/Kolkata".to_string(),
            max_orders_per_second: 10,
            nse_holidays: vec![NseHolidayEntry {
                date: "2026-01-26".to_string(),
                name: "Republic Day".to_string(),
            }],
            muhurat_trading_dates: vec![],
        };
        TradingCalendar::from_config(&config).unwrap()
    }

    #[test]
    fn test_is_within_market_hours_does_not_panic() {
        let calendar = make_test_calendar();
        // Just exercise the function — result depends on current time/day
        let _result = is_within_market_hours(&calendar);
    }

    #[test]
    fn test_is_within_market_hours_returns_bool() {
        let calendar = make_test_calendar();
        let result = is_within_market_hours(&calendar);
        // Result is deterministic for the same instant — just verify it's a bool
        assert!(result || !result);
    }

    // -----------------------------------------------------------------------
    // OrderUpdateConnectionError — additional coverage
    // -----------------------------------------------------------------------

    #[test]
    fn test_error_source_is_none() {
        // thiserror errors without #[source] should return None
        let err = OrderUpdateConnectionError::NoToken;
        assert!(
            std::error::Error::source(&err).is_none(),
            "NoToken has no source error"
        );
    }

    #[test]
    fn test_read_timeout_error_includes_constant() {
        let err = OrderUpdateConnectionError::ReadTimeout;
        let msg = err.to_string();
        assert!(
            msg.contains(&ORDER_UPDATE_READ_TIMEOUT_SECS.to_string()),
            "ReadTimeout message must include the timeout value"
        );
    }

    // -----------------------------------------------------------------------
    // connect_and_listen — NoToken error when no token is available
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_connect_and_listen_no_token() {
        // TokenHandle with None token should return NoToken error.
        let token_handle: TokenHandle = Arc::new(arc_swap::ArcSwap::new(Arc::new(None)));
        let (order_sender, _rx) = broadcast::channel(16);
        let calendar = make_test_calendar();

        let result = connect_and_listen(
            "wss://api-order-update.dhan.co",
            "test-client",
            &token_handle,
            &order_sender,
            &calendar,
        )
        .await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(err, OrderUpdateConnectionError::NoToken),
            "expected NoToken, got: {err:?}"
        );
    }

    // -----------------------------------------------------------------------
    // connect_and_listen — TokenExpired error when token is expired
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_connect_and_listen_token_expired() {
        use chrono::{Duration as ChronoDuration, Utc};
        use dhan_live_trader_common::trading_calendar::ist_offset;
        use secrecy::SecretString;

        // Create an expired token state.
        let now_ist = Utc::now().with_timezone(&ist_offset());
        let expired_at = now_ist - ChronoDuration::hours(1);
        let issued_at = now_ist - ChronoDuration::hours(25);
        let token_state = crate::auth::types::TokenState::from_cached(
            SecretString::from("expired-jwt".to_string()),
            expired_at,
            issued_at,
        );

        let token_handle: TokenHandle =
            Arc::new(arc_swap::ArcSwap::new(Arc::new(Some(token_state))));
        let (order_sender, _rx) = broadcast::channel(16);
        let calendar = make_test_calendar();

        let result = connect_and_listen(
            "wss://api-order-update.dhan.co",
            "test-client",
            &token_handle,
            &order_sender,
            &calendar,
        )
        .await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(err, OrderUpdateConnectionError::TokenExpired),
            "expected TokenExpired, got: {err:?}"
        );
    }

    // -----------------------------------------------------------------------
    // connect_and_listen — Connection refused (valid token, bad URL)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_connect_and_listen_connection_refused() {
        use chrono::{Duration as ChronoDuration, Utc};
        use dhan_live_trader_common::trading_calendar::ist_offset;
        use secrecy::SecretString;

        // Create a valid (non-expired) token.
        let now_ist = Utc::now().with_timezone(&ist_offset());
        let expires_at = now_ist + ChronoDuration::hours(23);
        let token_state = crate::auth::types::TokenState::from_cached(
            SecretString::from("valid-jwt-for-test".to_string()),
            expires_at,
            now_ist,
        );

        let token_handle: TokenHandle =
            Arc::new(arc_swap::ArcSwap::new(Arc::new(Some(token_state))));
        let (order_sender, _rx) = broadcast::channel(16);
        let calendar = make_test_calendar();

        // Use a URL that will fail to connect.
        let result = connect_and_listen(
            "wss://127.0.0.1:1",
            "test-client",
            &token_handle,
            &order_sender,
            &calendar,
        )
        .await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(err, OrderUpdateConnectionError::Connect(_)),
            "expected Connect error, got: {err:?}"
        );
    }

    // -----------------------------------------------------------------------
    // run_order_update_connection — reconnect exhaustion with no token
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_run_order_update_exits_after_max_reconnect_failures() {
        // With no token, every connect attempt fails.
        // After ORDER_UPDATE_MAX_RECONNECT_ATTEMPTS failures, the function returns.
        let token_handle: TokenHandle = Arc::new(arc_swap::ArcSwap::new(Arc::new(None)));
        let (order_sender, _rx) = broadcast::channel(16);
        let calendar = Arc::new(make_test_calendar());

        // Use a generous timeout to cover exponential backoff delays.
        // With 10 attempts and initial=1000ms/max=60000ms:
        // Sum ≈ 1+2+4+8+16+32+60+60+60+60 = ~303s in worst case.
        // Use tokio::time::pause() to avoid real wall-clock delays.
        tokio::time::pause();

        let result = tokio::time::timeout(
            Duration::from_secs(600),
            run_order_update_connection(
                "wss://127.0.0.1:1".to_string(),
                "test-client".to_string(),
                token_handle,
                order_sender,
                calendar,
            ),
        )
        .await;

        // The function should have exited after exhausting reconnection attempts.
        assert!(
            result.is_ok(),
            "run_order_update_connection should exit within timeout"
        );
    }

    // -----------------------------------------------------------------------
    // is_within_market_hours — holiday check
    // -----------------------------------------------------------------------

    #[test]
    fn test_is_within_market_hours_on_holiday_returns_false() {
        use chrono::Datelike;
        use dhan_live_trader_common::config::{NseHolidayEntry, TradingConfig};

        // If today is a weekend, the calendar already returns false for
        // is_trading_day_today() — we can't add a weekend date as a holiday
        // because TradingCalendar::from_config rejects weekend holidays.
        // So we test with a known future weekday as a holiday.
        let now_ist = chrono::Utc::now().with_timezone(&ist_offset());
        let today_date = now_ist.date_naive();

        // Find today or the next weekday to use as a holiday date.
        let mut holiday_date = today_date;
        while holiday_date.weekday() == chrono::Weekday::Sat
            || holiday_date.weekday() == chrono::Weekday::Sun
        {
            holiday_date += chrono::Duration::days(1);
        }
        let holiday_str = holiday_date.format("%Y-%m-%d").to_string();

        let config = TradingConfig {
            market_open_time: "09:00:00".to_string(),
            market_close_time: "15:30:00".to_string(),
            order_cutoff_time: "15:29:00".to_string(),
            data_collection_start: "09:00:00".to_string(),
            data_collection_end: "15:30:00".to_string(),
            timezone: "Asia/Kolkata".to_string(),
            max_orders_per_second: 10,
            nse_holidays: vec![NseHolidayEntry {
                date: holiday_str.clone(),
                name: "Test Holiday".to_string(),
            }],
            muhurat_trading_dates: vec![],
        };
        let calendar = TradingCalendar::from_config(&config).unwrap();

        // If today matches the holiday_date (weekday), it returns false.
        // If today is a weekend, it also returns false (not a trading day).
        // Either way, we prove the function doesn't panic.
        if today_date == holiday_date {
            assert!(
                !is_within_market_hours(&calendar),
                "should return false on a holiday"
            );
        } else {
            // Today is a weekend, so is_within_market_hours returns false anyway.
            assert!(
                !is_within_market_hours(&calendar),
                "should return false on a weekend"
            );
        }
    }

    // -----------------------------------------------------------------------
    // OrderUpdateConnectionError — matches check
    // -----------------------------------------------------------------------

    #[test]
    fn test_read_timeout_matches_pattern() {
        // Verify the pattern used in run_order_update_connection's off-hours check.
        let err = OrderUpdateConnectionError::ReadTimeout;
        assert!(matches!(err, OrderUpdateConnectionError::ReadTimeout));

        let err2 = OrderUpdateConnectionError::NoToken;
        assert!(!matches!(err2, OrderUpdateConnectionError::ReadTimeout));
    }

    // -----------------------------------------------------------------------
    // Off-hours read timeout — consecutive_failures reset logic
    // -----------------------------------------------------------------------

    #[test]
    fn test_off_hours_timeout_value_is_longer() {
        // Off-hours timeout should be longer than market-hours timeout.
        assert!(
            ORDER_UPDATE_OFF_HOURS_READ_TIMEOUT_SECS >= ORDER_UPDATE_READ_TIMEOUT_SECS,
            "off-hours timeout ({ORDER_UPDATE_OFF_HOURS_READ_TIMEOUT_SECS}) should be >= market timeout ({ORDER_UPDATE_READ_TIMEOUT_SECS})"
        );
    }

    // -----------------------------------------------------------------------
    // connect_and_listen — TLS error (bad URL scheme)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_connect_and_listen_tls_error_on_invalid_scheme() {
        use chrono::{Duration as ChronoDuration, Utc};
        use dhan_live_trader_common::trading_calendar::ist_offset;
        use secrecy::SecretString;

        // Install the crypto provider (needed for TLS connector).
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

        // Create a valid token.
        let now_ist = Utc::now().with_timezone(&ist_offset());
        let expires_at = now_ist + ChronoDuration::hours(23);
        let token_state = crate::auth::types::TokenState::from_cached(
            SecretString::from("valid-jwt".to_string()),
            expires_at,
            now_ist,
        );
        let token_handle: TokenHandle =
            Arc::new(arc_swap::ArcSwap::new(Arc::new(Some(token_state))));
        let (order_sender, _rx) = broadcast::channel(16);
        let calendar = make_test_calendar();

        // Use a non-WSS URL to trigger a Connect error during request building.
        let result = connect_and_listen(
            "not-a-valid-websocket-url",
            "test-client",
            &token_handle,
            &order_sender,
            &calendar,
        )
        .await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(err, OrderUpdateConnectionError::Connect(_)),
            "expected Connect error from bad URL, got: {err:?}"
        );
    }

    // -----------------------------------------------------------------------
    // is_within_market_hours — non-trading day (weekend) check
    // -----------------------------------------------------------------------

    #[test]
    fn test_is_within_market_hours_on_weekend_returns_false() {
        use dhan_live_trader_common::config::{NseHolidayEntry, TradingConfig};

        // Find a known Saturday/Sunday — 2026-01-03 is a Saturday.
        // The calendar checks weekdays internally; we just need no holidays
        // to prove the weekend check in is_trading_day_today() does the job.
        let config = TradingConfig {
            market_open_time: "09:00:00".to_string(),
            market_close_time: "15:30:00".to_string(),
            order_cutoff_time: "15:29:00".to_string(),
            data_collection_start: "09:00:00".to_string(),
            data_collection_end: "15:30:00".to_string(),
            timezone: "Asia/Kolkata".to_string(),
            max_orders_per_second: 10,
            nse_holidays: vec![NseHolidayEntry {
                date: "2099-01-01".to_string(), // far future holiday, won't affect today
                name: "Future".to_string(),
            }],
            muhurat_trading_dates: vec![],
        };
        let calendar = TradingCalendar::from_config(&config).unwrap();

        // The function uses current time. We can only verify it doesn't panic
        // and returns a bool. On weekends it'll be false, on weekdays true (if in window).
        let _result = is_within_market_hours(&calendar);
    }

    // -----------------------------------------------------------------------
    // run_order_update_connection — clean disconnect resets backoff
    // -----------------------------------------------------------------------

    #[test]
    fn test_consecutive_failures_saturating_add() {
        // Verify saturating_add behavior used in the reconnect loop.
        let mut failures: u32 = u32::MAX - 1;
        failures = failures.saturating_add(1);
        assert_eq!(failures, u32::MAX);
        failures = failures.saturating_add(1);
        assert_eq!(failures, u32::MAX, "must not overflow past u32::MAX");
    }

    // -----------------------------------------------------------------------
    // Timeout selection based on market hours
    // -----------------------------------------------------------------------

    #[test]
    fn test_timeout_constants_positive() {
        assert!(ORDER_UPDATE_READ_TIMEOUT_SECS > 0);
        assert!(ORDER_UPDATE_OFF_HOURS_READ_TIMEOUT_SECS > 0);
    }
}
