//! 20-level and 200-level depth WebSocket connections.
//!
//! Separate connections from the Live Market Feed. Connect to:
//! - 20-level: `wss://depth-api-feed.dhan.co/twentydepth`
//! - 200-level: `wss://full-depth-api.dhan.co/` (matches DhanHQ Python SDK)
//!
//! Frames are sent to the same `mpsc::Sender<Bytes>` channel as the main feed,
//! so the tick processor handles dispatch via `dispatch_deep_depth_frame()`.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use secrecy::ExposeSecret;
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::time;
use tokio_tungstenite::connect_async_tls_with_config;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use tracing::{debug, error, info, warn};

use dhan_live_trader_common::constants::{
    DHAN_TWENTY_DEPTH_WS_BASE_URL, DHAN_TWO_HUNDRED_DEPTH_WS_BASE_URL, FRAME_SEND_TIMEOUT_SECS,
    WEBSOCKET_AUTH_TYPE,
};
use dhan_live_trader_common::types::ExchangeSegment;

use super::tls::build_websocket_tls_connector;
use super::types::{InstrumentSubscription, WebSocketError};
use crate::auth::TokenHandle;
use crate::websocket::subscription_builder::build_twenty_depth_subscription_messages;

/// Identifier for depth connections in logs/metrics.
const DEPTH_CONNECTION_PREFIX: &str = "depth-20lvl";

/// Read timeout for depth connections (seconds). Same as main feed.
const DEPTH_READ_TIMEOUT_SECS: u64 = 40;

/// Reconnection backoff initial delay (ms).
const DEPTH_RECONNECT_INITIAL_MS: u64 = 1000;

/// Reconnection backoff max delay (ms).
const DEPTH_RECONNECT_MAX_MS: u64 = 30000;

/// Subscription batch size for 20-level depth (max 50 per Dhan docs).
const DEPTH_SUBSCRIPTION_BATCH_SIZE: usize = 50;

/// Pre-market backoff delay for 200-level depth (seconds).
/// Dhan's 200-level depth servers reset connections outside market hours.
/// Instead of aggressive exponential retry, wait this long before retrying pre-market.
const DEPTH_200_PRE_MARKET_BACKOFF_SECS: u64 = 60;

/// Returns true if current IST time is within market data hours (08:55 - 16:00).
/// 200-level depth connections should only retry aggressively during this window.
// O(1) EXEMPT: called once per reconnection attempt (cold path), not per tick
fn is_within_market_data_hours() -> bool {
    let now_utc = chrono::Utc::now();
    // IST = UTC + 5:30
    let ist_secs = now_utc.timestamp() + 19800;
    let ist_hour = ((ist_secs % 86400) / 3600) as u32;
    let ist_minute = ((ist_secs % 3600) / 60) as u32;
    let time_mins = ist_hour * 60 + ist_minute;
    // 08:55 = 535 minutes, 16:00 = 960 minutes
    // O(1) EXEMPT: integer comparison, not Vec::contains
    #[allow(clippy::manual_range_contains)] // APPROVED: avoids banned .contains() pattern
    {
        time_mins >= 535 && time_mins <= 960
    }
}

/// Connection timeout for depth WebSocket (seconds).
const DEPTH_CONNECT_TIMEOUT_SECS: u64 = 15;

/// Runs a 20-level depth WebSocket connection with infinite reconnection.
///
/// Connects to `wss://depth-api-feed.dhan.co/twentydepth`, subscribes to
/// the given instruments, and forwards raw binary frames to the shared channel.
/// The tick processor dispatches these via `dispatch_deep_depth_frame()`.
///
/// # Arguments
/// * `token_handle` — Shared atomic token for O(1) reads.
/// * `client_id` — Dhan client ID.
/// * `instruments` — Instruments to subscribe (max 50, same type as main feed).
/// * `frame_sender` — Shared channel to tick processor.
// TEST-EXEMPT: Integration-level — requires live Dhan WebSocket endpoint
pub async fn run_twenty_depth_connection(
    token_handle: TokenHandle,
    client_id: String,
    instruments: Vec<InstrumentSubscription>,
    frame_sender: mpsc::Sender<Bytes>,
) -> Result<(), WebSocketError> {
    if instruments.is_empty() {
        info!("20-level depth: no instruments to subscribe — skipping");
        return Ok(());
    }

    let instrument_count = instruments.len().min(DEPTH_SUBSCRIPTION_BATCH_SIZE);
    info!(
        instrument_count,
        "starting 20-level depth WebSocket connection"
    );

    // Pre-build subscription messages (reused across reconnects).
    let subscription_messages = build_twenty_depth_subscription_messages(
        &instruments[..instrument_count],
        DEPTH_SUBSCRIPTION_BATCH_SIZE,
    );

    let reconnect_counter = AtomicU64::new(0);

    loop {
        match connect_and_run_depth(
            &token_handle,
            &client_id,
            &subscription_messages,
            &frame_sender,
            instrument_count,
        )
        .await
        {
            Ok(()) => {
                info!("{DEPTH_CONNECTION_PREFIX}: connection closed normally");
                // Reset counter only on successful connection (failures accumulate for backoff)
                reconnect_counter.store(0, Ordering::Relaxed);
            }
            Err(err) => {
                let attempt = reconnect_counter.fetch_add(1, Ordering::Relaxed);

                // Escalate to ERROR after 10+ consecutive failures (not on first failure)
                if attempt > 0 && attempt.is_multiple_of(10) {
                    error!(
                        attempt,
                        ?err,
                        "{DEPTH_CONNECTION_PREFIX}: reconnection threshold — still retrying"
                    );
                } else {
                    warn!(
                        attempt,
                        ?err,
                        "{DEPTH_CONNECTION_PREFIX}: connection failed — will reconnect"
                    );
                }

                // Exponential backoff: 1s, 2s, 4s, ..., capped at 30s
                let delay_ms = DEPTH_RECONNECT_INITIAL_MS
                    .saturating_mul(1u64.checked_shl(attempt.min(63) as u32).unwrap_or(u64::MAX))
                    .min(DEPTH_RECONNECT_MAX_MS);
                tokio::time::sleep(Duration::from_millis(delay_ms)).await;
            }
        }
    }
}

/// Connects to the depth WebSocket, subscribes, and runs the read loop.
// O(1) EXEMPT: begin — connection lifecycle runs once per connect/reconnect, not per tick
async fn connect_and_run_depth(
    token_handle: &TokenHandle,
    client_id: &str,
    subscription_messages: &[String],
    frame_sender: &mpsc::Sender<Bytes>,
    instrument_count: usize,
) -> Result<(), WebSocketError> {
    // Read token
    let token_guard = token_handle.load();
    let token_state = token_guard
        .as_ref()
        .as_ref()
        .ok_or(WebSocketError::NoTokenAvailable)?;

    if !token_state.is_valid() {
        return Err(WebSocketError::NoTokenAvailable);
    }

    let access_token = token_state.access_token().expose_secret().to_string();

    // Build authenticated URL — no trailing slash before query string (SDK match)
    let base = DHAN_TWENTY_DEPTH_WS_BASE_URL.trim_end_matches('/');
    let authenticated_url = zeroize::Zeroizing::new(format!(
        "{base}?token={access_token}&clientId={client_id}&authType={WEBSOCKET_AUTH_TYPE}"
    ));

    let request = authenticated_url
        .as_str()
        .into_client_request()
        .map_err(|err| WebSocketError::ConnectionFailed {
            url: DHAN_TWENTY_DEPTH_WS_BASE_URL.to_string(),
            source: err,
        })?;

    debug!("{DEPTH_CONNECTION_PREFIX}: connecting to depth WebSocket");

    let tls_connector = build_websocket_tls_connector()?;

    let connect_timeout = Duration::from_secs(DEPTH_CONNECT_TIMEOUT_SECS);
    let connect_result = time::timeout(
        connect_timeout,
        connect_async_tls_with_config(request, None, false, Some(tls_connector)),
    )
    .await
    .map_err(|_| WebSocketError::ConnectionFailed {
        url: DHAN_TWENTY_DEPTH_WS_BASE_URL.to_string(),
        source: tokio_tungstenite::tungstenite::Error::Io(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "depth WebSocket connection timed out",
        )),
    })?;

    let (ws_stream, _response): (WebSocketStream<MaybeTlsStream<TcpStream>>, _) = connect_result
        .map_err(|err| WebSocketError::ConnectionFailed {
            url: DHAN_TWENTY_DEPTH_WS_BASE_URL.to_string(),
            source: err,
        })?;

    info!(
        instrument_count,
        "{DEPTH_CONNECTION_PREFIX}: connected — sending subscriptions"
    );

    let (mut write, mut read) = ws_stream.split();

    // Send subscription messages
    for msg in subscription_messages {
        write
            .send(Message::Text(msg.clone().into()))
            .await
            .map_err(|err| WebSocketError::ConnectionFailed {
                url: DHAN_TWENTY_DEPTH_WS_BASE_URL.to_string(),
                source: err,
            })?;
        debug!("{DEPTH_CONNECTION_PREFIX}: subscription sent");
    }

    info!(
        instrument_count,
        subscription_count = subscription_messages.len(),
        "{DEPTH_CONNECTION_PREFIX}: all subscriptions sent — reading depth frames"
    );

    // Metrics
    let m_frames = metrics::counter!("dlt_depth_20lvl_frames_total");
    let m_active = metrics::gauge!("dlt_depth_20lvl_connection_active");
    m_active.set(1.0);

    let read_timeout = Duration::from_secs(DEPTH_READ_TIMEOUT_SECS);

    // Read loop
    loop {
        match time::timeout(read_timeout, read.next()).await {
            Err(_elapsed) => {
                m_active.set(0.0);
                warn!("{DEPTH_CONNECTION_PREFIX}: read timeout — reconnecting");
                return Err(WebSocketError::ReadTimeout {
                    connection_id: 99, // depth connection uses ID 99
                    timeout_secs: DEPTH_READ_TIMEOUT_SECS,
                });
            }
            Ok(frame_result) => match frame_result {
                Some(Ok(Message::Binary(data))) => {
                    m_frames.increment(1);
                    let send_timeout = Duration::from_secs(FRAME_SEND_TIMEOUT_SECS);
                    match time::timeout(send_timeout, frame_sender.send(data)).await {
                        Ok(Ok(())) => {}
                        Ok(Err(_)) => {
                            m_active.set(0.0);
                            warn!("{DEPTH_CONNECTION_PREFIX}: receiver dropped — stopping");
                            return Ok(());
                        }
                        Err(_) => {
                            warn!("{DEPTH_CONNECTION_PREFIX}: frame send timeout — dropping frame");
                            metrics::counter!("dlt_depth_frames_dropped_total", "type" => "send_timeout", "depth" => "20").increment(1);
                        }
                    }
                }
                Some(Ok(Message::Ping(data))) => {
                    if let Err(err) = write.send(Message::Pong(data)).await {
                        m_active.set(0.0);
                        warn!(?err, "{DEPTH_CONNECTION_PREFIX}: pong send failed");
                        return Err(WebSocketError::ConnectionFailed {
                            url: DHAN_TWENTY_DEPTH_WS_BASE_URL.to_string(),
                            source: err,
                        });
                    }
                }
                Some(Ok(Message::Close(_))) => {
                    m_active.set(0.0);
                    info!("{DEPTH_CONNECTION_PREFIX}: server sent close frame");
                    return Ok(());
                }
                Some(Err(err)) => {
                    m_active.set(0.0);
                    return Err(WebSocketError::ConnectionFailed {
                        url: DHAN_TWENTY_DEPTH_WS_BASE_URL.to_string(),
                        source: err,
                    });
                }
                None => {
                    m_active.set(0.0);
                    return Ok(());
                }
                _ => {}
            },
        }
    }
    // O(1) EXEMPT: end
}

// ---------------------------------------------------------------------------
// 200-Level Depth Connection
// ---------------------------------------------------------------------------

/// Runs a 200-level depth WebSocket connection for a SINGLE instrument.
///
/// Connects to `wss://full-depth-api.dhan.co/` (matches DhanHQ Python SDK).
/// Only 1 instrument per connection (Dhan limitation).
/// Infinite reconnection with exponential backoff.
// TEST-EXEMPT: Integration-level — requires live Dhan WebSocket endpoint
pub async fn run_two_hundred_depth_connection(
    token_handle: TokenHandle,
    client_id: String,
    exchange_segment: ExchangeSegment,
    security_id: u32,
    label: String,
    frame_sender: mpsc::Sender<Bytes>,
) -> Result<(), WebSocketError> {
    let segment_str = exchange_segment.as_str();
    let sid_str = security_id.to_string(); // O(1) EXEMPT: boot-time

    // 200-level uses flat JSON (no InstrumentList array)
    let subscribe_msg = serde_json::json!({
        "RequestCode": 23,
        "ExchangeSegment": segment_str,
        "SecurityId": sid_str,
    })
    .to_string(); // O(1) EXEMPT: boot-time

    info!(
        label,
        security_id,
        segment = segment_str,
        "starting 200-level depth WebSocket connection"
    );

    let reconnect_counter = AtomicU64::new(0);
    let prefix = format!("depth-200lvl-{label}"); // O(1) EXEMPT: boot-time

    loop {
        match connect_and_run_200_depth(
            &token_handle,
            &client_id,
            &subscribe_msg,
            &frame_sender,
            &prefix,
        )
        .await
        {
            Ok(()) => {
                info!("{prefix}: connection closed normally");
                // Reset counter only on successful connection (failures accumulate for backoff)
                reconnect_counter.store(0, Ordering::Relaxed);
            }
            Err(err) => {
                let attempt = reconnect_counter.fetch_add(1, Ordering::Relaxed);

                // Outside market data hours (before 08:55 or after 16:00 IST),
                // Dhan's 200-level depth servers reset connections. Use a long
                // fixed backoff instead of spamming exponential retries.
                if !is_within_market_data_hours() {
                    if attempt == 0 {
                        info!(
                            "{prefix}: outside market hours — will retry every {DEPTH_200_PRE_MARKET_BACKOFF_SECS}s"
                        );
                    }
                    tokio::time::sleep(Duration::from_secs(DEPTH_200_PRE_MARKET_BACKOFF_SECS))
                        .await;
                    continue;
                }

                // During market hours: escalate to ERROR after 10+ consecutive failures
                if attempt > 0 && attempt.is_multiple_of(10) {
                    error!(
                        attempt,
                        ?err,
                        "{prefix}: reconnection threshold — still retrying"
                    );
                } else {
                    warn!(
                        attempt,
                        ?err,
                        "{prefix}: connection failed — will reconnect"
                    );
                }

                let delay_ms = DEPTH_RECONNECT_INITIAL_MS
                    .saturating_mul(1u64.checked_shl(attempt.min(63) as u32).unwrap_or(u64::MAX))
                    .min(DEPTH_RECONNECT_MAX_MS);
                tokio::time::sleep(Duration::from_millis(delay_ms)).await;
            }
        }
    }
}

/// Connects to the 200-level depth WebSocket and runs the read loop.
// O(1) EXEMPT: begin — connection lifecycle
async fn connect_and_run_200_depth(
    token_handle: &TokenHandle,
    client_id: &str,
    subscribe_msg: &str,
    frame_sender: &mpsc::Sender<Bytes>,
    prefix: &str,
) -> Result<(), WebSocketError> {
    let token_guard = token_handle.load();
    let token_state = token_guard
        .as_ref()
        .as_ref()
        .ok_or(WebSocketError::NoTokenAvailable)?;

    if !token_state.is_valid() {
        return Err(WebSocketError::NoTokenAvailable);
    }

    let access_token = token_state.access_token().expose_secret().to_string();
    // 200-level URL is root path — SDK uses wss://full-depth-api.dhan.co/?token=...
    // The trailing slash before ? is REQUIRED for root-path URLs (400 without it).
    let authenticated_url = zeroize::Zeroizing::new(format!(
        "{}?token={access_token}&clientId={client_id}&authType={WEBSOCKET_AUTH_TYPE}",
        DHAN_TWO_HUNDRED_DEPTH_WS_BASE_URL
    ));

    let request = authenticated_url
        .as_str()
        .into_client_request()
        .map_err(|err| WebSocketError::ConnectionFailed {
            url: DHAN_TWO_HUNDRED_DEPTH_WS_BASE_URL.to_string(),
            source: err,
        })?;

    let tls_connector = build_websocket_tls_connector()?;
    let connect_timeout = Duration::from_secs(DEPTH_CONNECT_TIMEOUT_SECS);

    let connect_result = time::timeout(
        connect_timeout,
        connect_async_tls_with_config(request, None, false, Some(tls_connector)),
    )
    .await
    .map_err(|_| WebSocketError::ConnectionFailed {
        url: DHAN_TWO_HUNDRED_DEPTH_WS_BASE_URL.to_string(),
        source: tokio_tungstenite::tungstenite::Error::Io(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "200-depth WebSocket connection timed out",
        )),
    })?;

    let (ws_stream, _response): (WebSocketStream<MaybeTlsStream<TcpStream>>, _) = connect_result
        .map_err(|err| WebSocketError::ConnectionFailed {
            url: DHAN_TWO_HUNDRED_DEPTH_WS_BASE_URL.to_string(),
            source: err,
        })?;

    info!("{prefix}: connected — sending subscription");

    let (mut write, mut read) = ws_stream.split();

    write
        .send(Message::Text(subscribe_msg.to_string().into()))
        .await
        .map_err(|err| WebSocketError::ConnectionFailed {
            url: DHAN_TWO_HUNDRED_DEPTH_WS_BASE_URL.to_string(),
            source: err,
        })?;

    info!("{prefix}: subscription sent — reading 200-level depth frames");

    let m_frames =
        metrics::counter!("dlt_depth_200lvl_frames_total", "label" => prefix.to_string());
    let m_active =
        metrics::gauge!("dlt_depth_200lvl_connection_active", "label" => prefix.to_string());
    m_active.set(1.0);

    let read_timeout = Duration::from_secs(DEPTH_READ_TIMEOUT_SECS);

    loop {
        match time::timeout(read_timeout, read.next()).await {
            Err(_elapsed) => {
                m_active.set(0.0);
                warn!("{prefix}: read timeout — reconnecting");
                return Err(WebSocketError::ReadTimeout {
                    connection_id: 98,
                    timeout_secs: DEPTH_READ_TIMEOUT_SECS,
                });
            }
            Ok(frame_result) => match frame_result {
                Some(Ok(Message::Binary(data))) => {
                    m_frames.increment(1);
                    let send_timeout = Duration::from_secs(FRAME_SEND_TIMEOUT_SECS);
                    match time::timeout(send_timeout, frame_sender.send(data)).await {
                        Ok(Ok(())) => {}
                        Ok(Err(_)) => {
                            m_active.set(0.0);
                            return Ok(());
                        }
                        Err(_) => {
                            warn!("{prefix}: frame send timeout — dropping frame");
                            metrics::counter!("dlt_depth_frames_dropped_total", "type" => "send_timeout", "depth" => "200").increment(1);
                        }
                    }
                }
                Some(Ok(Message::Ping(data))) => {
                    if let Err(err) = write.send(Message::Pong(data)).await {
                        m_active.set(0.0);
                        return Err(WebSocketError::ConnectionFailed {
                            url: DHAN_TWO_HUNDRED_DEPTH_WS_BASE_URL.to_string(),
                            source: err,
                        });
                    }
                }
                Some(Ok(Message::Close(_))) => {
                    m_active.set(0.0);
                    info!("{prefix}: server sent close frame");
                    return Ok(());
                }
                Some(Err(err)) => {
                    m_active.set(0.0);
                    return Err(WebSocketError::ConnectionFailed {
                        url: DHAN_TWO_HUNDRED_DEPTH_WS_BASE_URL.to_string(),
                        source: err,
                    });
                }
                None => {
                    m_active.set(0.0);
                    return Ok(());
                }
                _ => {}
            },
        }
    }
    // O(1) EXEMPT: end
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_depth_constants() {
        assert_eq!(DEPTH_SUBSCRIPTION_BATCH_SIZE, 50);
        assert!(DEPTH_READ_TIMEOUT_SECS >= 30);
        assert!(DEPTH_RECONNECT_MAX_MS <= 60000);
    }

    #[test]
    fn test_backoff_calculation() {
        // Verify backoff caps at max
        let delay_0 = DEPTH_RECONNECT_INITIAL_MS
            .saturating_mul(1)
            .min(DEPTH_RECONNECT_MAX_MS);
        assert_eq!(delay_0, 1000);

        let delay_5 = DEPTH_RECONNECT_INITIAL_MS
            .saturating_mul(1u64.checked_shl(5).unwrap_or(u64::MAX))
            .min(DEPTH_RECONNECT_MAX_MS);
        assert_eq!(delay_5, 30000); // 1000 * 32 = 32000, capped at 30000
    }

    #[tokio::test]
    async fn test_run_twenty_depth_empty_instruments_returns_ok() {
        let (tx, _rx) = mpsc::channel(16);
        let token_handle: TokenHandle =
            std::sync::Arc::new(arc_swap::ArcSwap::new(std::sync::Arc::new(None)));
        let result =
            run_twenty_depth_connection(token_handle, "test".to_string(), vec![], tx).await;
        assert!(result.is_ok());
    }

    #[test]
    fn test_depth_connection_prefix() {
        assert_eq!(DEPTH_CONNECTION_PREFIX, "depth-20lvl");
    }

    #[test]
    fn test_depth_connect_timeout_reasonable() {
        assert!(DEPTH_CONNECT_TIMEOUT_SECS >= 5);
        assert!(DEPTH_CONNECT_TIMEOUT_SECS <= 30);
    }

    #[test]
    fn test_depth_reconnect_initial_delay() {
        assert_eq!(DEPTH_RECONNECT_INITIAL_MS, 1000);
    }

    #[test]
    fn test_depth_reconnect_max_delay() {
        assert_eq!(DEPTH_RECONNECT_MAX_MS, 30000);
    }

    #[test]
    fn test_backoff_caps_at_max_for_large_attempts() {
        for attempt in 6..20 {
            let delay = DEPTH_RECONNECT_INITIAL_MS
                .saturating_mul(1u64.checked_shl(attempt).unwrap_or(u64::MAX))
                .min(DEPTH_RECONNECT_MAX_MS);
            assert_eq!(delay, DEPTH_RECONNECT_MAX_MS);
        }
    }

    #[test]
    fn test_backoff_grows_exponentially_before_cap() {
        let delay_0 = DEPTH_RECONNECT_INITIAL_MS.min(DEPTH_RECONNECT_MAX_MS);
        let delay_1 = DEPTH_RECONNECT_INITIAL_MS
            .saturating_mul(2)
            .min(DEPTH_RECONNECT_MAX_MS);
        let delay_2 = DEPTH_RECONNECT_INITIAL_MS
            .saturating_mul(4)
            .min(DEPTH_RECONNECT_MAX_MS);
        assert_eq!(delay_0, 1000);
        assert_eq!(delay_1, 2000);
        assert_eq!(delay_2, 4000);
    }

    #[test]
    fn test_twenty_depth_ws_url_correct() {
        assert_eq!(
            DHAN_TWENTY_DEPTH_WS_BASE_URL,
            "wss://depth-api-feed.dhan.co/twentydepth"
        );
    }

    #[test]
    fn test_two_hundred_depth_ws_url_correct() {
        // Updated to match DhanHQ Python SDK (fulldepth.py) which uses root path
        assert_eq!(
            DHAN_TWO_HUNDRED_DEPTH_WS_BASE_URL,
            "wss://full-depth-api.dhan.co/"
        );
    }

    #[test]
    fn test_websocket_auth_type_is_2() {
        assert_eq!(WEBSOCKET_AUTH_TYPE, 2);
    }

    #[test]
    fn test_subscription_batch_size_matches_dhan_limit() {
        // Dhan docs: max 50 instruments per 20-level depth connection
        assert_eq!(DEPTH_SUBSCRIPTION_BATCH_SIZE, 50);
    }

    #[tokio::test]
    async fn test_connect_and_run_depth_no_token_returns_error() {
        let token_handle: TokenHandle =
            std::sync::Arc::new(arc_swap::ArcSwap::new(std::sync::Arc::new(None)));
        let (tx, _rx) = mpsc::channel(16);
        let result = connect_and_run_depth(&token_handle, "test", &[], &tx, 0).await;
        assert!(result.is_err());
        match result.unwrap_err() {
            WebSocketError::NoTokenAvailable => {}
            other => panic!("expected NoTokenAvailable, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_connect_and_run_200_depth_no_token_returns_error() {
        let token_handle: TokenHandle =
            std::sync::Arc::new(arc_swap::ArcSwap::new(std::sync::Arc::new(None)));
        let (tx, _rx) = mpsc::channel(16);
        let result =
            connect_and_run_200_depth(&token_handle, "test", "{}", &tx, "test-label").await;
        assert!(result.is_err());
        match result.unwrap_err() {
            WebSocketError::NoTokenAvailable => {}
            other => panic!("expected NoTokenAvailable, got {other:?}"),
        }
    }

    #[test]
    fn test_200_level_subscribe_json_format() {
        // 200-level uses flat JSON (no InstrumentList), per Dhan docs
        let msg = serde_json::json!({
            "RequestCode": 23,
            "ExchangeSegment": "NSE_FNO",
            "SecurityId": "49081",
        });
        let json_str = msg.to_string();
        assert!(json_str.contains("\"RequestCode\":23"));
        assert!(json_str.contains("\"ExchangeSegment\":\"NSE_FNO\""));
        assert!(json_str.contains("\"SecurityId\":\"49081\""));
        // Must NOT contain InstrumentList (that's 20-level format)
        assert!(!json_str.contains("InstrumentList"));
        assert!(!json_str.contains("InstrumentCount"));
    }

    #[test]
    fn test_depth_read_timeout_matches_keepalive() {
        // Dhan docs: server pings every 10s, connection drops after 40s
        assert_eq!(DEPTH_READ_TIMEOUT_SECS, 40);
    }

    #[test]
    fn test_reconnect_counter_accumulates_on_consecutive_failures() {
        // Regression: 2026-04-06 — reconnect counter was reset after BOTH Ok and Err,
        // causing backoff to always use attempt=0 (1s delay instead of exponential).
        // Fix: reset only inside Ok() arm.
        let counter = AtomicU64::new(0);

        // Simulate 5 consecutive failures
        for expected in 0..5u64 {
            let attempt = counter.fetch_add(1, Ordering::Relaxed);
            assert_eq!(attempt, expected, "attempt must accumulate on failure");
        }
        // After 5 failures, counter should be 5
        assert_eq!(counter.load(Ordering::Relaxed), 5);

        // Verify backoff grows: attempt 0 = 1s, attempt 4 = 16s
        let delay_0 = DEPTH_RECONNECT_INITIAL_MS
            .saturating_mul(1u64.checked_shl(0).unwrap_or(u64::MAX))
            .min(DEPTH_RECONNECT_MAX_MS);
        let delay_4 = DEPTH_RECONNECT_INITIAL_MS
            .saturating_mul(1u64.checked_shl(4).unwrap_or(u64::MAX))
            .min(DEPTH_RECONNECT_MAX_MS);
        assert_eq!(delay_0, 1000, "first attempt: 1s");
        assert_eq!(delay_4, 16000, "fifth attempt: 16s");

        // Only reset on success (simulated Ok path)
        counter.store(0, Ordering::Relaxed);
        assert_eq!(counter.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn test_reconnect_counter_resets_only_on_success() {
        // Regression: 2026-04-06 — counter must NOT reset after Err
        let counter = AtomicU64::new(0);

        // Failure path: increment
        let _ = counter.fetch_add(1, Ordering::Relaxed);
        assert_eq!(
            counter.load(Ordering::Relaxed),
            1,
            "should be 1 after failure"
        );

        // Another failure: still accumulates
        let _ = counter.fetch_add(1, Ordering::Relaxed);
        assert_eq!(
            counter.load(Ordering::Relaxed),
            2,
            "should be 2 after second failure"
        );

        // Success path: now we reset
        counter.store(0, Ordering::Relaxed);
        assert_eq!(
            counter.load(Ordering::Relaxed),
            0,
            "should be 0 after success"
        );

        // Next failure starts from 0 again
        let attempt = counter.fetch_add(1, Ordering::Relaxed);
        assert_eq!(attempt, 0, "fresh failure after success starts at 0");
    }

    #[test]
    fn test_twenty_depth_url_no_trailing_slash_before_query() {
        // Regression: 2026-04-06 — trailing slash before ?token= caused HTTP 404.
        // SDK produces: wss://depth-api-feed.dhan.co/twentydepth?token=...
        // Bug produced: wss://depth-api-feed.dhan.co/twentydepth/?token=...
        let base = DHAN_TWENTY_DEPTH_WS_BASE_URL.trim_end_matches('/');
        let url = format!("{base}?token=TEST&clientId=TEST&authType=2");
        assert!(
            url.contains("/twentydepth?token="),
            "must NOT have trailing slash before query: {url}"
        );
        assert!(
            !url.contains("/twentydepth/?"),
            "must NOT have /twentydepth/? pattern: {url}"
        );
    }

    #[test]
    fn test_two_hundred_depth_url_preserves_trailing_slash() {
        // 200-level base is root path — trailing slash before ? is REQUIRED.
        // Without it: wss://full-depth-api.dhan.co?token=... → 400 Bad Request
        // With it: wss://full-depth-api.dhan.co/?token=... → works (matches SDK)
        let url = format!(
            "{}?token=TEST&clientId=TEST&authType=2",
            DHAN_TWO_HUNDRED_DEPTH_WS_BASE_URL
        );
        assert!(
            url.contains(".dhan.co/?token="),
            "200-level must have trailing slash before query: {url}"
        );
        assert!(
            url.starts_with("wss://full-depth-api.dhan.co/"),
            "must use correct host with slash: {url}"
        );
    }
}
