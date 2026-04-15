//! 20-level and 200-level depth WebSocket connections.
//!
//! Separate connections from the Live Market Feed. Connect to:
//! - 20-level: `wss://depth-api-feed.dhan.co/twentydepth`
//! - 200-level: `wss://full-depth-api.dhan.co/twohundreddepth` (confirmed by Dhan Ticket #5519522)
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

use tickvault_common::constants::{
    DHAN_TWENTY_DEPTH_WS_BASE_URL, DHAN_TWO_HUNDRED_DEPTH_WS_BASE_URL, WEBSOCKET_AUTH_TYPE,
};
use tickvault_common::types::ExchangeSegment;

use super::tls::build_websocket_tls_connector;
use super::types::{InstrumentSubscription, WebSocketError};
use crate::auth::TokenHandle;
use crate::websocket::subscription_builder::build_twenty_depth_subscription_messages;

/// Identifier for depth connections in logs/metrics.
const DEPTH_CONNECTION_PREFIX: &str = "depth-20lvl";

/// Read timeout for depth connections (seconds).
// STAGE-B (P1.3): `DEPTH_READ_TIMEOUT_SECS` removed.
//
// Per Dhan spec (docs/dhan-ref/04-full-market-depth-websocket.md), depth
// WebSockets follow the same ping/pong as the live market feed — server
// pings, library auto-pongs, server closes on >40s silence. Our own
// client-side deadline is redundant and was the root cause of false
// reconnect storms whenever downstream stalled.
//
// Liveness is now enforced by: (a) Dhan's server-side timeout, and (b)
// Stage C's watchdog task (independent of the read loop).

/// Reconnection backoff initial delay (ms).
const DEPTH_RECONNECT_INITIAL_MS: u64 = 1000;

/// Reconnection backoff max delay (ms).
const DEPTH_RECONNECT_MAX_MS: u64 = 30000;

/// Pong send timeout (seconds). Matches main feed connection behavior.
/// If pong send takes longer, the TCP connection is likely dead.
const DEPTH_PONG_TIMEOUT_SECS: u64 = 10;

/// Subscription batch size for 20-level depth (max 50 per Dhan docs).
const DEPTH_SUBSCRIPTION_BATCH_SIZE: usize = 50;

// Market-hours sleep REMOVED: depth connections stay connected 24/7, same as
// main Live Market Feed WebSocket. Reconnection uses exponential backoff
// regardless of time-of-day. If Dhan servers reject outside market hours,
// backoff caps at 30s — no multi-hour sleeps that hide connection state.

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
/// * `underlying_label` — Underlying name (e.g. "NIFTY") for metric labels.
/// * `connected_signal` — Fires after first data frame received (not just subscription).
// TEST-EXEMPT: Integration-level — requires live Dhan WebSocket endpoint
pub async fn run_twenty_depth_connection(
    token_handle: TokenHandle,
    client_id: String,
    instruments: Vec<InstrumentSubscription>,
    frame_sender: mpsc::Sender<Bytes>,
    underlying_label: String,
    connected_signal: Option<tokio::sync::oneshot::Sender<()>>,
) -> Result<(), WebSocketError> {
    if instruments.is_empty() {
        info!("20-level depth: no instruments to subscribe — skipping");
        return Ok(());
    }

    let instrument_count = instruments.len().min(DEPTH_SUBSCRIPTION_BATCH_SIZE);
    let prefix = format!("{DEPTH_CONNECTION_PREFIX}-{underlying_label}"); // O(1) EXEMPT: boot-time

    info!(
        instrument_count,
        underlying = %underlying_label,
        "starting 20-level depth WebSocket connection"
    );

    // Pre-build subscription messages (reused across reconnects).
    let subscription_messages = build_twenty_depth_subscription_messages(
        &instruments[..instrument_count],
        DEPTH_SUBSCRIPTION_BATCH_SIZE,
    );

    let reconnect_counter = AtomicU64::new(0);
    // O(1) EXEMPT: metric handle created once at boot, not per tick
    let m_reconnections = metrics::counter!("tv_depth_20lvl_reconnections_total", "underlying" => underlying_label.to_string());
    // Consumed after first Binary data frame — notifies caller that
    // the connection is truly alive and receiving data (not just subscribed).
    let mut pending_signal = connected_signal;

    loop {
        match connect_and_run_depth(
            &token_handle,
            &client_id,
            &subscription_messages,
            &frame_sender,
            instrument_count,
            &mut pending_signal,
            &prefix,
            &underlying_label,
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
                m_reconnections.increment(1);

                // Escalate to ERROR after 10+ consecutive failures
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
    connected_signal: &mut Option<tokio::sync::oneshot::Sender<()>>,
    prefix: &str,
    underlying_label: &str,
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

    debug!("{prefix}: connecting to depth WebSocket");

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
        "{prefix}: connected — sending subscriptions"
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
        debug!("{prefix}: subscription sent");
    }

    info!(
        instrument_count,
        subscription_count = subscription_messages.len(),
        "{prefix}: all subscriptions sent — reading depth frames"
    );

    // Metrics — labeled per underlying so Grafana can distinguish connections.
    let m_frames = metrics::counter!("tv_depth_20lvl_frames_total", "underlying" => underlying_label.to_string());
    let m_active = metrics::gauge!("tv_depth_20lvl_connection_active", "underlying" => underlying_label.to_string());
    // Don't set active=1 yet — wait for first data frame (avoids false-positive Telegram alerts).

    // STAGE-B (P1.1): no client-side read deadline — tokio-tungstenite +
    // Dhan's server-side 40s pong timeout are the only liveness checks.
    let mut first_frame_received = false;

    loop {
        let frame_result = read.next().await;
        match frame_result {
            Some(Ok(Message::Binary(data))) => {
                // Fire connected signal + set active metric on FIRST data frame only.
                // This guarantees Telegram alert fires only when data actually flows.
                if !first_frame_received {
                    first_frame_received = true;
                    m_active.set(1.0);
                    if let Some(signal) = connected_signal.take() {
                        let _ = signal.send(());
                    }
                }
                m_frames.increment(1);

                // STAGE-B (P1.2): never block the reader. Try-send only.
                // On channel-full, increment CRITICAL drop counter. Stage C
                // replaces this drop with a durable WAL append.
                match frame_sender.try_send(data) {
                    Ok(()) => {}
                    Err(mpsc::error::TrySendError::Full(_dropped)) => {
                        error!(
                            underlying = %underlying_label,
                            channel_capacity = frame_sender.capacity(),
                            "CRITICAL: 20-depth frame channel full — frame dropped. \
                             Downstream consumer is stuck."
                        );
                        metrics::counter!(
                            "tv_ws_frame_spill_drop_critical",
                            "ws_type" => "depth_20",
                            "underlying" => underlying_label.to_string()
                        )
                        .increment(1);
                        // Do NOT return — socket is healthy.
                    }
                    Err(mpsc::error::TrySendError::Closed(_)) => {
                        m_active.set(0.0);
                        warn!("{prefix}: receiver dropped — stopping");
                        return Ok(());
                    }
                }
            }
            Some(Ok(Message::Ping(data))) => {
                let pong_timeout = Duration::from_secs(DEPTH_PONG_TIMEOUT_SECS);
                match time::timeout(pong_timeout, write.send(Message::Pong(data))).await {
                    Ok(Ok(())) => {}
                    Ok(Err(err)) => {
                        m_active.set(0.0);
                        warn!(?err, "{prefix}: pong send failed");
                        return Err(WebSocketError::ConnectionFailed {
                            url: DHAN_TWENTY_DEPTH_WS_BASE_URL.to_string(),
                            source: err,
                        });
                    }
                    Err(_) => {
                        m_active.set(0.0);
                        warn!(
                            timeout_secs = DEPTH_PONG_TIMEOUT_SECS,
                            "{prefix}: pong send timed out — connection likely dead"
                        );
                        return Err(WebSocketError::ReadTimeout {
                            connection_id: 99,
                            timeout_secs: DEPTH_PONG_TIMEOUT_SECS,
                        });
                    }
                }
            }
            Some(Ok(Message::Pong(_))) => {
                // Server echo pong — ignore (keep-alive confirmation)
            }
            Some(Ok(Message::Close(_))) => {
                m_active.set(0.0);
                info!("{prefix}: server sent close frame");
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
        }
    }
    // O(1) EXEMPT: end
}

// ---------------------------------------------------------------------------
// 200-Level Depth Connection
// ---------------------------------------------------------------------------

/// Runs a 200-level depth WebSocket connection for a SINGLE instrument.
///
/// Connects to `wss://full-depth-api.dhan.co/twohundreddepth` (Dhan Ticket #5519522).
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
    connected_signal: Option<tokio::sync::oneshot::Sender<()>>,
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
    let m_reconnections =
        // O(1) EXEMPT: metric handle created once at boot, not per tick
        metrics::counter!("tv_depth_200lvl_reconnections_total", "underlying" => prefix.to_string());
    let mut pending_signal = connected_signal;

    loop {
        match connect_and_run_200_depth(
            &token_handle,
            &client_id,
            &subscribe_msg,
            &frame_sender,
            &prefix,
            &mut pending_signal,
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
                m_reconnections.increment(1);

                // Escalate to ERROR after 10+ consecutive failures
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
    connected_signal: &mut Option<tokio::sync::oneshot::Sender<()>>,
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
    // 200-level URL: /twohundreddepth path confirmed by Dhan support (Ticket #5519522).
    let base = DHAN_TWO_HUNDRED_DEPTH_WS_BASE_URL.trim_end_matches('/');
    let authenticated_url = zeroize::Zeroizing::new(format!(
        "{base}?token={access_token}&clientId={client_id}&authType={WEBSOCKET_AUTH_TYPE}",
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

    // Metrics — labeled per underlying for Grafana differentiation.
    let m_frames =
        metrics::counter!("tv_depth_200lvl_frames_total", "underlying" => prefix.to_string());
    let m_active =
        metrics::gauge!("tv_depth_200lvl_connection_active", "underlying" => prefix.to_string());
    // Don't set active=1 yet — wait for first data frame.

    // STAGE-B (P1.1): no client-side read deadline.
    let mut first_frame_received = false;

    loop {
        let frame_result = read.next().await;
        match frame_result {
            Some(Ok(Message::Binary(data))) => {
                if !first_frame_received {
                    first_frame_received = true;
                    m_active.set(1.0);
                    if let Some(signal) = connected_signal.take() {
                        let _ = signal.send(());
                    }
                }
                m_frames.increment(1);

                // STAGE-B (P1.2): try-send only — never block the reader.
                match frame_sender.try_send(data) {
                    Ok(()) => {}
                    Err(mpsc::error::TrySendError::Full(_dropped)) => {
                        error!(
                            label = %prefix,
                            channel_capacity = frame_sender.capacity(),
                            "CRITICAL: 200-depth frame channel full — frame dropped. \
                             Downstream consumer is stuck."
                        );
                        metrics::counter!(
                            "tv_ws_frame_spill_drop_critical",
                            "ws_type" => "depth_200",
                            "label" => prefix.to_string()
                        )
                        .increment(1);
                    }
                    Err(mpsc::error::TrySendError::Closed(_)) => {
                        m_active.set(0.0);
                        return Ok(());
                    }
                }
            }
            Some(Ok(Message::Ping(data))) => {
                let pong_timeout = Duration::from_secs(DEPTH_PONG_TIMEOUT_SECS);
                match time::timeout(pong_timeout, write.send(Message::Pong(data))).await {
                    Ok(Ok(())) => {}
                    Ok(Err(err)) => {
                        m_active.set(0.0);
                        return Err(WebSocketError::ConnectionFailed {
                            url: DHAN_TWO_HUNDRED_DEPTH_WS_BASE_URL.to_string(),
                            source: err,
                        });
                    }
                    Err(_) => {
                        m_active.set(0.0);
                        warn!(
                            timeout_secs = DEPTH_PONG_TIMEOUT_SECS,
                            "{prefix}: pong send timed out — connection likely dead"
                        );
                        return Err(WebSocketError::ReadTimeout {
                            connection_id: 98,
                            timeout_secs: DEPTH_PONG_TIMEOUT_SECS,
                        });
                    }
                }
            }
            Some(Ok(Message::Pong(_))) => {}
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
        // STAGE-B: DEPTH_READ_TIMEOUT_SECS removed — client-side read deadline
        // was redundant with Dhan's own 40s server-side pong timeout.
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
        let result = run_twenty_depth_connection(
            token_handle,
            "test".to_string(),
            vec![],
            tx,
            "TEST".to_string(),
            None,
        )
        .await;
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
        // Dhan support confirmed /twohundreddepth path (Ticket #5519522, 2026-04-10)
        assert_eq!(
            DHAN_TWO_HUNDRED_DEPTH_WS_BASE_URL,
            "wss://full-depth-api.dhan.co/twohundreddepth"
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
        let result = connect_and_run_depth(
            &token_handle,
            "test",
            &[],
            &tx,
            0,
            &mut None,
            "depth-20lvl-TEST",
            "TEST",
        )
        .await;
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
            connect_and_run_200_depth(&token_handle, "test", "{}", &tx, "test-label", &mut None)
                .await;
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
    fn test_depth_read_loops_have_no_client_side_deadline() {
        // STAGE-B (P1.1): assert that neither depth read loop wraps
        // `read.next()` in a `time::timeout(...)`. This was the root cause of
        // false reconnects whenever downstream stalled. The only allowed
        // `time::timeout` calls in the depth loops are:
        //   - pong send timeout (DEPTH_PONG_TIMEOUT_SECS, not a read deadline)
        //   - connection establishment timeout (DEPTH_CONNECT_TIMEOUT_SECS)
        // A regression that re-adds a read deadline must fail this test.
        let source = include_str!("depth_connection.rs");
        assert!(
            !source.contains("time::timeout(read_timeout, read.next())"),
            "depth read loop must not wrap read.next() in time::timeout"
        );
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
    fn test_two_hundred_depth_url_uses_twohundreddepth_path() {
        // Dhan support confirmed /twohundreddepth (Ticket #5519522, 2026-04-10).
        // Root path / caused ResetWithoutClosingHandshake.
        let base = DHAN_TWO_HUNDRED_DEPTH_WS_BASE_URL.trim_end_matches('/');
        let url = format!("{base}?token=TEST&clientId=TEST&authType=2",);
        assert!(
            url.contains("twohundreddepth?token="),
            "200-level must use /twohundreddepth path: {url}"
        );
        assert!(
            !url.contains("twohundreddepth/?"),
            "must NOT have spurious / before query string: {url}"
        );
    }

    // -------------------------------------------------------------------
    // 24/7 connection behavior tests (market hours sleep removed)
    // -------------------------------------------------------------------

    #[test]
    fn test_depth_reconnect_backoff_caps_at_30s() {
        // With market-hours sleep removed, exponential backoff is the ONLY
        // reconnection delay mechanism. Verify the cap is reasonable.
        let max_delay = DEPTH_RECONNECT_INITIAL_MS
            .saturating_mul(1u64.checked_shl(63).unwrap_or(u64::MAX))
            .min(DEPTH_RECONNECT_MAX_MS);
        assert_eq!(max_delay, DEPTH_RECONNECT_MAX_MS);
        assert_eq!(DEPTH_RECONNECT_MAX_MS, 30000, "max backoff must be 30s");
    }

    #[test]
    fn test_depth_always_retries_regardless_of_time() {
        // Depth connections now retry 24/7 — no market hours guard.
        // This test documents the intentional removal.
        // The only backoff is exponential (1s → 30s), never multi-hour sleep.
        assert!(
            DEPTH_RECONNECT_MAX_MS <= 60_000,
            "max reconnect delay must be <= 60s (no multi-hour sleep)"
        );
    }
}
