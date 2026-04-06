//! 20-level and 200-level depth WebSocket connections.
//!
//! Separate connections from the Live Market Feed. Connect to:
//! - 20-level: `wss://depth-api-feed.dhan.co/twentydepth`
//! - 200-level: `wss://full-depth-api.dhan.co/twohundreddepth`
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
    DHAN_TWENTY_DEPTH_WS_BASE_URL, FRAME_SEND_TIMEOUT_SECS, WEBSOCKET_AUTH_TYPE,
};

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
            }
            Err(err) => {
                let attempt = reconnect_counter.fetch_add(1, Ordering::Relaxed);

                if attempt.is_multiple_of(10) {
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

        // Reset counter on successful connection (only failures accumulate)
        reconnect_counter.store(0, Ordering::Relaxed);
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

    // Build authenticated URL
    let base = DHAN_TWENTY_DEPTH_WS_BASE_URL.trim_end_matches('/');
    let authenticated_url = zeroize::Zeroizing::new(format!(
        "{base}/?token={access_token}&clientId={client_id}&authType={WEBSOCKET_AUTH_TYPE}"
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
}
