//! 20-level and 200-level depth WebSocket connections.
//!
//! Separate connections from the Live Market Feed. Connect to:
//! - 20-level: `wss://depth-api-feed.dhan.co/twentydepth`
//! - 200-level: `wss://full-depth-api.dhan.co/` (root path; Python SDK verified 2026-04-23 —
//!   reverses Dhan ticket #5519522's earlier `/twohundreddepth` advice)
//!
//! Frames are sent to the same `mpsc::Sender<Bytes>` channel as the main feed,
//! so the tick processor handles dispatch via `dispatch_deep_depth_frame()`.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use secrecy::ExposeSecret;
use tickvault_storage::ws_frame_spill::{AppendOutcome, WsFrameSpill, WsType};
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
use crate::websocket::activity_watchdog::{
    ActivityWatchdog, WATCHDOG_THRESHOLD_LIVE_AND_DEPTH_SECS, build_heartbeat_gauge,
    spawn_with_panic_notify,
};
use crate::websocket::subscription_builder::build_twenty_depth_subscription_messages;

/// Identifier for depth connections in logs/metrics.
const DEPTH_CONNECTION_PREFIX: &str = "depth-20lvl";

/// Command sent to a live depth connection to swap instruments without disconnecting.
/// Unsubscribes old instruments (RequestCode 25) then subscribes new ones (RequestCode 23).
/// Zero disconnect, zero reconnect, zero tick gap.
///
/// Plan item D (2026-04-22): adds `InitialSubscribe20` / `InitialSubscribe200`
/// variants dispatched by the Phase 2 scheduler at 09:12:30 IST. Under the
/// unified 09:12 close flow, depth connections are spawned idle (socket open,
/// authenticated, but no subscribe sent) and only start streaming after
/// Phase 2 dispatches the initial subscription using the 09:12 index close.
#[derive(Debug)]
pub enum DepthCommand {
    /// 09:12:30 IST first subscribe for a 20-level depth connection. No
    /// prior state to unsubscribe — just send the pre-built subscribe
    /// messages. Fires at most once per connection per day.
    InitialSubscribe20 { subscribe_messages: Vec<String> },

    /// 09:12:30 IST first subscribe for a 200-level depth connection.
    /// Single JSON message (200-level = 1 instrument per connection).
    InitialSubscribe200 { subscribe_message: String },

    /// Swap 20-level instruments: unsubscribe old list, subscribe new list.
    /// Both lists are pre-built JSON messages ready to send.
    Swap20 {
        unsubscribe_messages: Vec<String>,
        subscribe_messages: Vec<String>,
    },
    /// Swap 200-level instrument: unsubscribe old, subscribe new.
    /// Single JSON message each (200-level = 1 instrument per connection).
    Swap200 {
        unsubscribe_message: String,
        subscribe_message: String,
    },
}

// Read timeout for depth connections (seconds).
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
///
/// 500ms matches the main feed's `ws_config.reconnect_initial_delay_ms`
/// default so all 4 WebSocket types (main + depth-20 + depth-200 +
/// order-update) share a single first-retry latency. Doubles on each
/// failure up to `DEPTH_RECONNECT_MAX_MS`. Keeps total 8-attempt
/// retry window ≈ 60s, matching Dhan's DH-805 rate-limit guidance.
const DEPTH_RECONNECT_INITIAL_MS: u64 = 500;

/// Reconnection backoff max delay (ms).
const DEPTH_RECONNECT_MAX_MS: u64 = 30000;

/// Maximum consecutive reconnection attempts before giving up.
///
/// **2026-04-21 production evidence**: all 4 depth-200 connections
/// exhausted the previous 20-attempt budget within 10 minutes during
/// market hours, then terminated permanently. Dhan's server repeatedly
/// TCP-reset the socket with `Protocol(ResetWithoutClosingHandshake)`
/// *despite the subscribed security_id being ATM* (Parthiban verified
/// against the Python SDK — same account, same strikes, Python works).
/// Root cause is therefore NOT off-ATM filtering. Investigation is
/// queued as a separate item in `.claude/queues/production-fixes-2026-04-21.md`
/// (I14 — Python-vs-Rust depth-200 protocol diff).
///
/// This 60-attempt cap is a conservative tolerance raise from 20 →
/// ≈30 min of retrying during market hours. It does NOT solve the
/// underlying Rust-side protocol bug; it only gives the process more
/// time to ride through a transient server reset. The
/// `ReconnectionExhausted` terminal error still fires CRITICAL
/// Telegram so the operator is paged if even 30 minutes cannot
/// recover.
const DEPTH_RECONNECT_MAX_ATTEMPTS: u64 = 60;

/// Per-connection initial-connect stagger for 200-level depth (ms).
///
/// **2026-04-24 production evidence** (12:07:54 IST boot): all 4
/// 200-level depth connections (NIFTY CE/PE, BANKNIFTY CE/PE) fired
/// concurrent TLS handshakes against `wss://full-depth-api.dhan.co`
/// within <100ms and all 4 were immediately TCP-reset with
/// `Protocol(ResetWithoutClosingHandshake)`. They stayed in retry loop
/// for 9+ minutes (observed attempt 20 at 12:16:53 IST).
///
/// Staggering each subsequent spawn by 2s gives Dhan's auth endpoint
/// exclusive processing time per handshake — serialising 4 connects
/// across ~8s. This is a speculative mitigation for what MAY be a
/// Dhan-side concurrent-auth rate limit; the variants-matrix test
/// (`cargo run --release --example depth_200_variants`) is the
/// primary investigation for the underlying Rust-vs-Python protocol
/// mismatch and will suggest the real fix (URL/UA/ALPN combo that
/// works without reset).
///
/// Zero cost on the happy path (spawn index 0 gets 0ms stagger).
pub const DEPTH_200_INITIAL_STAGGER_MS: u64 = 2000;

/// Pong send timeout (seconds). Matches main feed connection behavior.
/// If pong send takes longer, the TCP connection is likely dead.
const DEPTH_PONG_TIMEOUT_SECS: u64 = 10;

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
/// * `underlying_label` — Underlying name (e.g. "NIFTY") for metric labels.
/// * `connected_signal` — Fires after first data frame received (not just subscription).
#[allow(clippy::too_many_arguments)] // APPROVED: STAGE-C added wal_spill param + 2026-04-21 notifier
// TEST-EXEMPT: Integration-level — requires live Dhan WebSocket endpoint
pub async fn run_twenty_depth_connection(
    token_handle: TokenHandle,
    client_id: String,
    instruments: Vec<InstrumentSubscription>,
    frame_sender: mpsc::Sender<Bytes>,
    underlying_label: String,
    connected_signal: Option<tokio::sync::oneshot::Sender<()>>,
    wal_spill: Option<Arc<WsFrameSpill>>,
    mut cmd_rx: mpsc::Receiver<DepthCommand>,
    notifier: Option<Arc<crate::notification::NotificationService>>,
) -> Result<(), WebSocketError> {
    // Plan item B (2026-04-23): empty `instruments` is NO LONGER an early-
    // exit path. Under the unified 09:12 close flow, boot callers pass an
    // empty list intentionally; the connection opens idle (socket up,
    // authenticated, no subscribe sent) and waits for a
    // `DepthCommand::InitialSubscribe20` from the 09:13 dispatcher before
    // it starts streaming. `subscription_messages` being empty is the
    // sentinel — see `connect_and_run_depth` which sends only when the
    // slice is non-empty.
    let instrument_count = instruments.len().min(DEPTH_SUBSCRIPTION_BATCH_SIZE);
    let prefix = format!("{DEPTH_CONNECTION_PREFIX}-{underlying_label}"); // O(1) EXEMPT: boot-time

    if instrument_count == 0 {
        info!(
            underlying = %underlying_label,
            "{prefix}: starting 20-level depth in DEFERRED mode — awaiting InitialSubscribe20 from 09:13 dispatcher"
        );
    } else {
        info!(
            instrument_count,
            underlying = %underlying_label,
            "starting 20-level depth WebSocket connection"
        );
    }

    // Pre-build subscription messages (reused across reconnects). Empty
    // slice when deferred — the connect loop treats empty as "skip the
    // initial subscribe step" and relies on InitialSubscribe20 to kick
    // in via the command channel.
    // Deferred-mode empty Vec is allocated once per depth connection at
    // boot (4 connections total across NIFTY/BANKNIFTY/FINNIFTY/MIDCPNIFTY).
    let subscription_messages: Vec<String> = if instrument_count == 0 {
        Vec::new() // O(1) EXEMPT: boot-time deferred-mode empty sentinel
    } else {
        build_twenty_depth_subscription_messages(
            &instruments[..instrument_count],
            DEPTH_SUBSCRIPTION_BATCH_SIZE,
        )
    };

    let reconnect_counter = AtomicU64::new(0);
    // O(1) EXEMPT: metric handle created once at boot, not per tick
    let m_reconnections = metrics::counter!("tv_depth_20lvl_reconnections_total", "underlying" => underlying_label.to_string());
    // Consumed after first Binary data frame — notifies caller that
    // the connection is truly alive and receiving data (not just subscribed).
    let mut pending_signal = connected_signal;

    loop {
        // Parthiban directive (2026-04-21): snapshot the failure count
        // so we can detect "this attempt is a RECOVERY from prior
        // failures" and fire the DepthTwentyReconnected Telegram event.
        let failures_before_attempt = reconnect_counter.load(Ordering::Relaxed);
        match connect_and_run_depth(
            &token_handle,
            &client_id,
            &subscription_messages,
            &frame_sender,
            instrument_count,
            &mut pending_signal,
            &prefix,
            &underlying_label,
            wal_spill.as_ref(),
            &mut cmd_rx,
        )
        .await
        {
            Ok(()) => {
                info!("{prefix}: connection closed normally");
                // If we reached a clean close AFTER at least one prior
                // failure, we successfully recovered (connect + run +
                // clean server-side close). Fire the reconnect event.
                if failures_before_attempt > 0 {
                    if let Some(ref n) = notifier {
                        n.notify(
                            crate::notification::events::NotificationEvent::DepthTwentyReconnected {
                                underlying: underlying_label.clone(), // O(1) EXEMPT: cold path, once per reconnect
                            },
                        );
                    }
                }
                // Reset counter only on successful connection (failures accumulate for backoff)
                reconnect_counter.store(0, Ordering::Relaxed);
            }
            Err(err) => {
                // STAGE-C.5: classify Dhan disconnect errors so we do
                // not retry into a ban window (805) or loop on a
                // permanently-rejected subscription (808/809/810/etc).
                // 807 (access token expired) IS reconnectable — the
                // outer run() loop will re-read the token_handle on
                // the next connect attempt, which picks up whatever
                // the token renewal task has swapped in.
                if let WebSocketError::DhanDisconnect { code } = &err
                    && !code.is_reconnectable()
                {
                    error!(
                        underlying = %underlying_label,
                        disconnect_code = %code,
                        "{prefix}: non-reconnectable Dhan disconnect — halting reconnect loop"
                    );
                    return Err(WebSocketError::NonReconnectableDisconnect { code: *code });
                }

                let attempt = reconnect_counter.fetch_add(1, Ordering::Relaxed);
                m_reconnections.increment(1);

                // Give up after max attempts — prevents infinite retry loops
                // after market close or on permanently unreachable instruments.
                if attempt >= DEPTH_RECONNECT_MAX_ATTEMPTS {
                    error!(
                        attempt,
                        instrument_count,
                        ?err,
                        "{prefix}: exhausted {DEPTH_RECONNECT_MAX_ATTEMPTS} reconnection attempts — giving up"
                    );
                    return Err(WebSocketError::ReconnectionExhausted {
                        connection_id: 0, // depth connections don't use pool IDs
                        attempts: attempt.min(u64::from(u32::MAX)) as u32,
                    });
                }

                // Escalate to ERROR after 10+ consecutive failures
                if attempt > 0 && attempt.is_multiple_of(10) {
                    error!(
                        attempt,
                        instrument_count,
                        ?err,
                        "{prefix}: reconnection threshold — still retrying"
                    );
                } else {
                    warn!(
                        attempt,
                        instrument_count,
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
///
/// Accepts an optional `cmd_rx` for live instrument swaps (unsubscribe old +
/// subscribe new on the SAME connection, zero disconnect, zero gap).
// O(1) EXEMPT: begin — connection lifecycle runs once per connect/reconnect, not per tick
#[allow(clippy::too_many_arguments)] // APPROVED: STAGE-C added wal_spill + cmd_rx params
async fn connect_and_run_depth(
    token_handle: &TokenHandle,
    client_id: &str,
    subscription_messages: &[String],
    frame_sender: &mpsc::Sender<Bytes>,
    instrument_count: usize,
    connected_signal: &mut Option<tokio::sync::oneshot::Sender<()>>,
    prefix: &str,
    underlying_label: &str,
    wal_spill: Option<&Arc<WsFrameSpill>>,
    cmd_rx: &mut mpsc::Receiver<DepthCommand>,
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

    if subscription_messages.is_empty() {
        // Plan item B (2026-04-23): deferred mode — connection is up but no
        // subscribe has been sent. The 09:13 dispatcher will send an
        // `InitialSubscribe20` via the command channel once the 09:12 close
        // is available. Until then, Dhan's 10s server pings keep the
        // socket alive and the watchdog quiet.
        info!(
            "{prefix}: connected in DEFERRED mode — no subscribe sent, awaiting InitialSubscribe20"
        );
    } else {
        info!(
            instrument_count,
            "{prefix}: connected — sending subscriptions"
        );
    }

    let (mut write, mut read) = ws_stream.split();

    // Send subscription messages (empty slice in deferred mode = no-op).
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

    if !subscription_messages.is_empty() {
        info!(
            instrument_count,
            subscription_count = subscription_messages.len(),
            "{prefix}: all subscriptions sent — reading depth frames"
        );
    }

    // Metrics — labeled per underlying so Grafana can distinguish connections.
    let m_frames = metrics::counter!("tv_depth_20lvl_frames_total", "underlying" => underlying_label.to_string());
    let m_active = metrics::gauge!("tv_depth_20lvl_connection_active", "underlying" => underlying_label.to_string());
    // Don't set active=1 yet — wait for first data frame (avoids false-positive Telegram alerts).

    // STAGE-B (P1.1): no client-side read deadline — tokio-tungstenite +
    // Dhan's server-side 40s pong timeout are the only liveness checks.
    let mut first_frame_received = false;

    // STAGE-C.3: per-connection activity watchdog. The counter is bumped on
    // every Some(Ok(_)) frame; the watchdog task fires `watchdog_notify`
    // when it observes no forward progress for 50s (40s server ping timeout
    // + 10s margin), and the select! below returns Err(WatchdogFired) so
    // the outer reconnect loop kicks in.
    let activity_counter = Arc::new(AtomicU64::new(0));
    let watchdog_notify = Arc::new(tokio::sync::Notify::new());
    let watchdog_label = format!("depth-20-{underlying_label}"); // O(1) EXEMPT: one alloc per connect cycle
    let watchdog = ActivityWatchdog::new(
        watchdog_label.clone(),
        Arc::clone(&activity_counter),
        Duration::from_secs(WATCHDOG_THRESHOLD_LIVE_AND_DEPTH_SECS),
        Arc::clone(&watchdog_notify),
    );
    // WS-1: panic-safe watchdog spawn — see activity_watchdog::spawn_with_panic_notify.
    let watchdog_handle = spawn_with_panic_notify(watchdog);

    // ZL-P0-1: heartbeat gauge handle for this depth-20 connection.
    // O(1) EXEMPT: one gauge lookup per reconnect cycle
    // O(1) EXEMPT: one allocation per reconnect cycle
    let m_last_frame_epoch = build_heartbeat_gauge("depth_20", underlying_label.to_string());

    // Helper: construct the WatchdogFired error uniformly from both the
    // select! branch (below) and any other watchdog-triggered return path.
    // O(1) EXEMPT: cold path, one per dead socket.
    let make_watchdog_err = || WebSocketError::WatchdogFired {
        label: watchdog_label.clone(),
        silent_secs: WATCHDOG_THRESHOLD_LIVE_AND_DEPTH_SECS,
        threshold_secs: WATCHDOG_THRESHOLD_LIVE_AND_DEPTH_SECS,
    };

    let loop_result: Result<(), WebSocketError> = loop {
        let frame_result = tokio::select! {
            biased;
            () = watchdog_notify.notified() => {
                m_active.set(0.0);
                break Err(make_watchdog_err());
            }
            // O(1) EXEMPT: cold path — command arrives at most once per 60s rebalance
            Some(cmd) = cmd_rx.recv() => {
                match cmd {
                    DepthCommand::InitialSubscribe20 { subscribe_messages } => {
                        // Plan item D (2026-04-22): 09:12:30 IST first subscribe
                        // for this 20-level connection. No unsubscribe — connection
                        // was spawned idle per item B.
                        info!(
                            "{prefix}: INITIAL SUBSCRIBE @ 09:12:30 — sending {} subscribe message(s) from 09:12 close",
                            subscribe_messages.len()
                        );
                        for msg in &subscribe_messages {
                            if let Err(err) = write.send(Message::Text(msg.clone().into())).await {
                                warn!(?err, "{prefix}: initial subscribe send failed");
                            }
                        }
                        info!("{prefix}: INITIAL SUBSCRIBE complete");
                    }
                    DepthCommand::Swap20 { unsubscribe_messages, subscribe_messages } => {
                        info!("{prefix}: REBALANCE — sending {} unsubscribe + {} subscribe messages (zero disconnect)",
                            unsubscribe_messages.len(), subscribe_messages.len());
                        for msg in &unsubscribe_messages {
                            if let Err(err) = write.send(Message::Text(msg.clone().into())).await {
                                warn!(?err, "{prefix}: rebalance unsubscribe send failed");
                            }
                        }
                        for msg in &subscribe_messages {
                            if let Err(err) = write.send(Message::Text(msg.clone().into())).await {
                                warn!(?err, "{prefix}: rebalance subscribe send failed");
                            }
                        }
                        info!("{prefix}: REBALANCE complete — instrument swap done");
                    }
                    // Wrong-type commands on a 20-level connection are no-ops
                    // — the Phase 2 dispatcher routes by feed type, so these
                    // should never fire; logging a warn! catches regressions.
                    other => {
                        warn!(?other, "{prefix}: ignored mis-routed DepthCommand on 20-level connection");
                    }
                }
                continue;
            }
            next_frame = read.next() => next_frame,
        };
        // STAGE-C.3: bump the activity counter on every Some(Ok(_)) event so
        // the watchdog can distinguish a truly dead socket from a data-quiet
        // period with pings still flowing. One relaxed atomic increment is
        // O(1) and allocation-free.
        // ZL-P0-1: also set the heartbeat gauge (single atomic store).
        if matches!(frame_result, Some(Ok(_))) {
            activity_counter.fetch_add(1, Ordering::Relaxed);
            m_last_frame_epoch.set(chrono::Utc::now().timestamp() as f64);
        }
        match frame_result {
            Some(Ok(Message::Binary(data))) => {
                // STAGE-C.5: Dhan depth-WS disconnect packet is 14 bytes:
                // 12-byte header (bytes 0-1 = message length LE, byte 2 =
                // feed code, byte 3 = segment, bytes 4-7 = security_id LE,
                // bytes 8-11 = sequence/row_count LE) + u16 LE reason code
                // at bytes 12-13. See
                // `docs/dhan-ref/04-full-market-depth-websocket.md:168`.
                //
                // Feed code 50 signals a disconnect frame. Intercept here
                // so the outer reconnect loop can classify the reason via
                // `DisconnectCode::from_u16()` and apply the same
                // is_reconnectable / requires_token_refresh logic as the
                // Live Market Feed connection.
                //
                // O(1) — one byte check + one u16 LE load. Happy path
                // (feed code != 50) is a single byte compare + early fall
                // through.
                let maybe_disconnect_code: Option<crate::websocket::types::DisconnectCode> =
                    if data.len() >= 14
                        && data[2] == tickvault_common::constants::RESPONSE_CODE_DISCONNECT
                    {
                        let raw = u16::from_le_bytes([data[12], data[13]]);
                        Some(crate::websocket::types::DisconnectCode::from_u16(raw))
                    } else {
                        None
                    };

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

                // STAGE-C (P1.2+P1.3): durable WAL first, live forward second.
                // See connection.rs for the full rationale — mirrored here.
                if let Some(spill) = wal_spill {
                    // O(1) EXEMPT: one Vec<u8> copy per frame (≤332 B for 20-depth)
                    // to hand owned memory to the disk writer thread. Bounded allocation.
                    let frame_vec = data.to_vec();
                    let outcome = spill.append(WsType::Depth20, frame_vec);
                    if outcome == AppendOutcome::Dropped {
                        error!(
                            underlying = %underlying_label,
                            "CRITICAL: WAL spill dropped Depth20 frame — disk writer stalled"
                        );
                        // CRITICAL metric already incremented inside spill.append().
                    }
                }
                match frame_sender.try_send(data) {
                    Ok(()) => {}
                    Err(mpsc::error::TrySendError::Full(_dropped)) => {
                        warn!(
                            underlying = %underlying_label,
                            channel_capacity = frame_sender.capacity(),
                            wal_attached = wal_spill.is_some(),
                            "20-depth live frame channel full — backpressure from downstream. \
                             Frame is durable in WAL; watchdog will restart consumer."
                        );
                        metrics::counter!(
                            "tv_ws_frame_live_backpressure_total",
                            "ws_type" => "depth_20",
                            "underlying" => underlying_label.to_string()
                        )
                        .increment(1);
                        // Catalog-aligned metric (tv_depth_frames_dropped_total,
                        // 2026-04-17 audit cleanup): emits the per-type / per-
                        // depth-level drop count that the metrics_catalog.rs
                        // required list expects.
                        metrics::counter!(
                            "tv_depth_frames_dropped_total",
                            "type" => "channel_full",
                            "depth" => "20"
                        )
                        .increment(1);
                        // Do NOT return — socket is healthy.
                    }
                    Err(mpsc::error::TrySendError::Closed(_)) => {
                        m_active.set(0.0);
                        warn!("{prefix}: receiver dropped — stopping");
                        break Ok(());
                    }
                }

                // STAGE-C.5: surface disconnect AFTER WAL + forward so
                // observability still sees the frame. Return Err so the
                // outer reconnect loop either backs off, halts on
                // non-reconnectable, or waits for token refresh on 807.
                if let Some(code) = maybe_disconnect_code {
                    warn!(
                        underlying = %underlying_label,
                        disconnect_code = %code,
                        reconnectable = code.is_reconnectable(),
                        requires_token_refresh = code.requires_token_refresh(),
                        "20-depth: Dhan binary disconnect packet — surfacing to reconnect loop"
                    );
                    metrics::counter!(
                        "tv_ws_dhan_disconnect_total",
                        "code" => format!("{code}"), // O(1) EXEMPT: cold path, once per disconnect
                        "ws_type" => "depth_20"
                    )
                    .increment(1);
                    m_active.set(0.0);
                    break Err(WebSocketError::DhanDisconnect { code });
                }
            }
            Some(Ok(Message::Ping(data))) => {
                let pong_timeout = Duration::from_secs(DEPTH_PONG_TIMEOUT_SECS);
                match time::timeout(pong_timeout, write.send(Message::Pong(data))).await {
                    Ok(Ok(())) => {}
                    Ok(Err(err)) => {
                        m_active.set(0.0);
                        warn!(?err, "{prefix}: pong send failed");
                        break Err(WebSocketError::ConnectionFailed {
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
                        break Err(WebSocketError::ReadTimeout {
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
                break Ok(());
            }
            Some(Err(err)) => {
                m_active.set(0.0);
                break Err(WebSocketError::ConnectionFailed {
                    url: DHAN_TWENTY_DEPTH_WS_BASE_URL.to_string(),
                    source: err,
                });
            }
            None => {
                m_active.set(0.0);
                break Ok(());
            }
            _ => {}
        }
    };

    // STAGE-C.3: Always abort the watchdog task when the read loop exits
    // for any reason. If the watchdog already fired, `abort()` is a no-op.
    watchdog_handle.abort();
    loop_result
    // O(1) EXEMPT: end
}

// ---------------------------------------------------------------------------
// 200-Level Depth Connection
// ---------------------------------------------------------------------------

/// Runs a 200-level depth WebSocket connection for a SINGLE instrument.
///
/// Connects to `wss://full-depth-api.dhan.co/` (root path — Python SDK verified
/// 2026-04-23, reverses Dhan ticket #5519522's earlier `/twohundreddepth` advice).
/// Only 1 instrument per connection (Dhan limitation).
/// Infinite reconnection with exponential backoff.
#[allow(clippy::too_many_arguments)] // APPROVED: STAGE-C added wal_spill param + 2026-04-21 notifier
// TEST-EXEMPT: Integration-level — requires live Dhan WebSocket endpoint
pub async fn run_two_hundred_depth_connection(
    token_handle: TokenHandle,
    client_id: String,
    exchange_segment: ExchangeSegment,
    security_id: Option<u32>,
    label: String,
    frame_sender: mpsc::Sender<Bytes>,
    connected_signal: Option<tokio::sync::oneshot::Sender<()>>,
    wal_spill: Option<Arc<WsFrameSpill>>,
    mut cmd_rx: mpsc::Receiver<DepthCommand>,
    notifier: Option<Arc<crate::notification::NotificationService>>,
    initial_stagger_ms: u64,
) -> Result<(), WebSocketError> {
    if initial_stagger_ms > 0 {
        info!(
            label = %label,
            stagger_ms = initial_stagger_ms,
            "200-level depth: staggering initial connect to avoid concurrent-auth reset"
        );
        tokio::time::sleep(std::time::Duration::from_millis(initial_stagger_ms)).await;
    }

    let segment_str = exchange_segment.as_str();

    // Plan item B (2026-04-23): `security_id == None` = deferred mode.
    // Connect idle, skip sending initial subscribe; wait for
    // `DepthCommand::InitialSubscribe200` from the 09:13 dispatcher.
    // Internal code below carries `u32` with 0 as the "unassigned"
    // sentinel — callers must never pass `Some(0)` because the
    // SubscribeRequest JSON `"SecurityId":"0"` would be rejected by
    // Dhan. The inner `connect_and_run_200_depth` treats an empty
    // `subscribe_msg` as a no-op for the initial subscribe step.
    let deferred = security_id.is_none();
    let security_id: u32 = security_id.unwrap_or(0);
    // Deferred-mode empty String is allocated once per 200-level depth
    // connection at boot (max 4 — NIFTY CE/PE + BANKNIFTY CE/PE).
    let subscribe_msg: String = if deferred {
        String::new() // O(1) EXEMPT: boot-time deferred-mode empty sentinel
    } else {
        let sid_str = security_id.to_string(); // O(1) EXEMPT: boot-time
        // 200-level uses flat JSON (no InstrumentList array)
        serde_json::json!({
            "RequestCode": 23,
            "ExchangeSegment": segment_str,
            "SecurityId": sid_str,
        })
        .to_string() // O(1) EXEMPT: boot-time
    };

    if deferred {
        info!(
            label,
            segment = segment_str,
            "starting 200-level depth WebSocket connection in DEFERRED mode — awaiting InitialSubscribe200"
        );
    } else {
        info!(
            label,
            security_id,
            segment = segment_str,
            "starting 200-level depth WebSocket connection"
        );
    }

    let reconnect_counter = AtomicU64::new(0);
    let prefix = format!("depth-200lvl-{label}"); // O(1) EXEMPT: boot-time
    let m_reconnections =
        // O(1) EXEMPT: metric handle created once at boot, not per tick
        metrics::counter!("tv_depth_200lvl_reconnections_total", "underlying" => prefix.to_string());
    // L7: Consecutive reset gauge — tracks current streak without success.
    // Resets to 0 on successful connection, climbs on consecutive failures.
    // Drives Depth200ResetLoop alert in tickvault-alerts.yml.
    let m_consecutive_resets =
        // O(1) EXEMPT: metric handle created once at boot
        metrics::gauge!("tv_depth_200lvl_consecutive_resets", "label" => label.to_string());
    // 2026-04-24 Path B: runtime variant rotator. Starts at variant A (root
    // path + default UA + http/1.1 ALPN) and rotates through the 8-variant
    // catalog after `RESET_ROTATE_THRESHOLD` consecutive
    // `ResetWithoutClosingHandshake` errors. On first successful frame the
    // variant is locked in for the life of the process.
    let variant_rotator = crate::websocket::depth_200_variants::VariantRotator::new();
    let mut pending_signal = connected_signal;

    loop {
        // Parthiban directive (2026-04-21): snapshot failures before
        // attempt so we can fire DepthTwoHundredReconnected on recovery.
        let failures_before_attempt = reconnect_counter.load(Ordering::Relaxed);
        let current_variant = variant_rotator.current();
        info!(
            security_id,
            label = %label,
            variant = current_variant.label,
            url_path = current_variant.url_path,
            ua = ?current_variant.ua,
            alpn = ?current_variant.alpn,
            locked_in = variant_rotator.is_locked_in(),
            "{prefix}: attempting connect with variant"
        );
        match connect_and_run_200_depth(
            &token_handle,
            &client_id,
            &subscribe_msg,
            &frame_sender,
            &prefix,
            &mut pending_signal,
            security_id,
            &label,
            wal_spill.as_ref(),
            &mut cmd_rx,
            current_variant,
            Some(&variant_rotator),
        )
        .await
        {
            Ok(()) => {
                info!(security_id, label = %label, variant = current_variant.label, "{prefix}: connection closed normally");
                // record_success already called inside the inner fn when the
                // handshake completed — no need to call it again here.
                // Recovery from prior failure streak — fire Telegram.
                if failures_before_attempt > 0 {
                    if let Some(ref n) = notifier {
                        n.notify(
                            crate::notification::events::NotificationEvent::DepthTwoHundredReconnected {
                                contract: label.clone(), // O(1) EXEMPT: cold path, once per reconnect
                                security_id,
                            },
                        );
                    }
                }
                // Reset counter only on successful connection (failures accumulate for backoff)
                reconnect_counter.store(0, Ordering::Relaxed);
                m_consecutive_resets.set(0.0);
            }
            Err(err) => {
                // STAGE-C.5: classify Dhan disconnect errors.
                if let WebSocketError::DhanDisconnect { code } = &err
                    && !code.is_reconnectable()
                {
                    error!(
                        security_id,
                        label = %label,
                        disconnect_code = %code,
                        "{prefix}: non-reconnectable Dhan disconnect — halting reconnect loop"
                    );
                    return Err(WebSocketError::NonReconnectableDisconnect { code: *code });
                }

                // 2026-04-24 Path B: if the failure is the
                // `Protocol(ResetWithoutClosingHandshake)` error that has
                // plagued us for 2 weeks, feed it to the rotator. After
                // RESET_ROTATE_THRESHOLD consecutive resets on the current
                // variant, the next attempt automatically uses the next
                // variant from the catalog.
                let is_reset_handshake = matches!(
                    &err,
                    WebSocketError::ConnectionFailed {
                        source: tokio_tungstenite::tungstenite::Error::Protocol(
                            tokio_tungstenite::tungstenite::error::ProtocolError::ResetWithoutClosingHandshake
                        ),
                        ..
                    }
                );
                if is_reset_handshake && variant_rotator.record_reset() {
                    let new_variant = variant_rotator.current();
                    error!(
                        security_id,
                        label = %label,
                        from_variant = current_variant.label,
                        to_variant = new_variant.label,
                        "{prefix}: rotating to next 200-depth variant after {} consecutive resets",
                        crate::websocket::depth_200_variants::RESET_ROTATE_THRESHOLD
                    );
                }

                let attempt = reconnect_counter.fetch_add(1, Ordering::Relaxed);
                m_reconnections.increment(1);
                m_consecutive_resets.set(attempt.saturating_add(1) as f64);

                // Give up after max attempts — prevents infinite retry loops
                // after market close or on permanently unreachable instruments.
                if attempt >= DEPTH_RECONNECT_MAX_ATTEMPTS {
                    error!(
                        security_id,
                        label = %label,
                        attempt,
                        ?err,
                        "{prefix}: exhausted {DEPTH_RECONNECT_MAX_ATTEMPTS} reconnection attempts — giving up"
                    );
                    return Err(WebSocketError::ReconnectionExhausted {
                        connection_id: 0, // depth connections don't use pool IDs
                        attempts: attempt.min(u64::from(u32::MAX)) as u32,
                    });
                }

                // Log policy — reduces noise during transient Dhan-side
                // resets (e.g. 200-level TCP resets per Ticket #5519522/#5543510,
                // fixed 2026-04-23 by switching from `/twohundreddepth` to root path)
                // while preserving escalation.
                //
                // * attempt == 0          → WARN  (first failure is visible)
                // * attempt == 1..=9      → DEBUG (silent during brief backoff)
                // * attempt % 10 == 0 (≥10) → ERROR (Telegram escalation)
                if attempt > 0 && attempt.is_multiple_of(10) {
                    error!(
                        security_id,
                        label = %label,
                        attempt,
                        ?err,
                        "{prefix}: reconnection threshold — still retrying"
                    );
                } else if attempt == 0 {
                    warn!(
                        security_id,
                        label = %label,
                        attempt,
                        ?err,
                        "{prefix}: connection failed — will reconnect"
                    );
                } else {
                    debug!(
                        security_id,
                        label = %label,
                        attempt,
                        ?err,
                        "{prefix}: connection still failing — retrying"
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
#[allow(clippy::too_many_arguments)] // APPROVED: STAGE-C added security_id/label/wal_spill params; 2026-04-24 added variant
async fn connect_and_run_200_depth(
    token_handle: &TokenHandle,
    client_id: &str,
    subscribe_msg: &str,
    frame_sender: &mpsc::Sender<Bytes>,
    prefix: &str,
    connected_signal: &mut Option<tokio::sync::oneshot::Sender<()>>,
    security_id: u32,
    label: &str,
    wal_spill: Option<&Arc<WsFrameSpill>>,
    cmd_rx: &mut mpsc::Receiver<DepthCommand>,
    variant: crate::websocket::depth_200_variants::DepthVariant,
    variant_rotator: Option<&crate::websocket::depth_200_variants::VariantRotator>,
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
    // 200-level URL path + UA + ALPN come from the variant. Variant A
    // (root + default UA + http/1.1) is the production baseline verified
    // via Dhan Python SDK on 2026-04-23. If it fails with repeated
    // `ResetWithoutClosingHandshake`, the outer run loop rotates to the
    // next variant automatically.
    let base_host = DHAN_TWO_HUNDRED_DEPTH_WS_BASE_URL
        .trim_start_matches("wss://")
        .trim_end_matches('/');

    let request = crate::websocket::depth_200_variants::build_request_for_variant(
        variant,
        base_host,
        &access_token,
        client_id,
    )?;

    let tls_connector =
        crate::websocket::depth_200_variants::build_tls_connector_for_variant(variant)?;
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

    // 2026-04-24 Path B: TLS + WebSocket handshake succeeded — Dhan accepted
    // this variant's URL/UA/ALPN combination. Lock it in immediately, BEFORE
    // we subscribe or read any frames. This is the critical semantic:
    // `ResetWithoutClosingHandshake` that later occurs mid-stream (e.g. Dhan
    // rotating servers after 30 min) must NOT rotate us away from a variant
    // we know works. Only handshake-time resets (which short-circuit this
    // code path via the `?` above) count toward rotation.
    if let Some(rotator) = variant_rotator {
        if !rotator.is_locked_in() {
            rotator.record_success();
            info!(
                label = %label,
                variant = variant.label,
                "{prefix}: handshake succeeded — locking in variant for this process"
            );
            metrics::counter!(
                "tv_depth_200_variant_success_total",
                "variant" => variant.label
            )
            .increment(1);
        }
    }

    if subscribe_msg.is_empty() {
        // Plan item B (2026-04-23): deferred mode — socket authenticated
        // but no subscribe sent. `InitialSubscribe200` via `cmd_rx` will
        // kick in at 09:12:30 IST. Dhan's 10s server pings keep the
        // watchdog quiet in the meantime. `security_id` is the 0 sentinel
        // passed by the outer `run_two_hundred_depth_connection` when the
        // caller supplies `None`.
        info!(
            label = %label,
            "{prefix}: connected in DEFERRED mode — no subscribe sent, awaiting InitialSubscribe200"
        );
    } else {
        info!(
            security_id,
            label = %label,
            "{prefix}: connected — sending subscription"
        );
    }

    let (mut write, mut read) = ws_stream.split();

    if !subscribe_msg.is_empty() {
        write
            .send(Message::Text(subscribe_msg.to_string().into()))
            .await
            .map_err(|err| WebSocketError::ConnectionFailed {
                url: DHAN_TWO_HUNDRED_DEPTH_WS_BASE_URL.to_string(),
                source: err,
            })?;

        info!(
            security_id,
            label = %label,
            "{prefix}: subscription sent — reading 200-level depth frames"
        );
    }

    // Metrics — labeled per underlying for Grafana differentiation.
    let m_frames =
        metrics::counter!("tv_depth_200lvl_frames_total", "underlying" => prefix.to_string());
    let m_active =
        metrics::gauge!("tv_depth_200lvl_connection_active", "underlying" => prefix.to_string());
    // Don't set active=1 yet — wait for first data frame.

    // STAGE-B (P1.1): no client-side read deadline.
    let mut first_frame_received = false;

    // STAGE-C.3: per-connection activity watchdog — same semantics as the
    // 20-level depth path above. 50s threshold (40s Dhan server ping timeout
    // + 10s margin). The label carries the security_id so a fired alert
    // points directly at the dead contract.
    let activity_counter = Arc::new(AtomicU64::new(0));
    let watchdog_notify = Arc::new(tokio::sync::Notify::new());
    // Plan item B (2026-04-23): `security_id == 0` = DEFERRED sentinel
    // (outer caller passed `None`). Label differentiates so a fired
    // watchdog alert is unambiguous about what hasn't been subscribed.
    let watchdog_label = if security_id == 0 {
        format!("depth-200-{label}-deferred") // O(1) EXEMPT: one alloc per connect cycle
    } else {
        format!("depth-200-{label}-sid{security_id}") // O(1) EXEMPT: one alloc per connect cycle
    };
    let watchdog = ActivityWatchdog::new(
        watchdog_label.clone(),
        Arc::clone(&activity_counter),
        Duration::from_secs(WATCHDOG_THRESHOLD_LIVE_AND_DEPTH_SECS),
        Arc::clone(&watchdog_notify),
    );
    // WS-1: panic-safe watchdog spawn — see activity_watchdog::spawn_with_panic_notify.
    let watchdog_handle = spawn_with_panic_notify(watchdog);

    // ZL-P0-1: heartbeat gauge handle for this depth-200 connection.
    // O(1) EXEMPT: one gauge lookup per reconnect cycle
    let m_last_frame_epoch = build_heartbeat_gauge("depth_200", watchdog_label.clone());

    // O(1) EXEMPT: cold path, one per dead socket.
    let make_watchdog_err = || WebSocketError::WatchdogFired {
        label: watchdog_label.clone(),
        silent_secs: WATCHDOG_THRESHOLD_LIVE_AND_DEPTH_SECS,
        threshold_secs: WATCHDOG_THRESHOLD_LIVE_AND_DEPTH_SECS,
    };

    let loop_result: Result<(), WebSocketError> = loop {
        let frame_result = tokio::select! {
            biased;
            () = watchdog_notify.notified() => {
                m_active.set(0.0);
                break Err(make_watchdog_err());
            }
            // O(1) EXEMPT: cold path — rebalance command at most once per 60s
            Some(cmd) = cmd_rx.recv() => {
                match cmd {
                    DepthCommand::InitialSubscribe200 { subscribe_message } => {
                        // Plan item D (2026-04-22): 09:12:30 IST first subscribe.
                        info!("{prefix}: INITIAL SUBSCRIBE @ 09:12:30 — 200-level from 09:12 close");
                        if let Err(err) = write.send(Message::Text(subscribe_message.into())).await {
                            warn!(?err, "{prefix}: initial 200-level subscribe send failed");
                        }
                        info!("{prefix}: INITIAL SUBSCRIBE complete");
                    }
                    DepthCommand::Swap200 { unsubscribe_message, subscribe_message } => {
                        info!("{prefix}: REBALANCE — swapping 200-level ATM instrument (zero disconnect)");
                        if let Err(err) = write.send(Message::Text(unsubscribe_message.into())).await {
                            warn!(?err, "{prefix}: rebalance 200-level unsubscribe failed");
                        }
                        if let Err(err) = write.send(Message::Text(subscribe_message.into())).await {
                            warn!(?err, "{prefix}: rebalance 200-level subscribe failed");
                        }
                        info!("{prefix}: REBALANCE complete — 200-level ATM swap done, zero disconnect");
                    }
                    // Wrong-type commands on a 200-level connection are no-ops
                    // — the Phase 2 dispatcher routes by feed type, so this
                    // should never fire; logging a warn! catches regressions.
                    other => {
                        warn!(?other, "{prefix}: ignored mis-routed DepthCommand on 200-level connection");
                    }
                }
                continue;
            }
            next_frame = read.next() => next_frame,
        };
        // ZL-P0-1: also set the heartbeat gauge on every frame.
        if matches!(frame_result, Some(Ok(_))) {
            activity_counter.fetch_add(1, Ordering::Relaxed);
            m_last_frame_epoch.set(chrono::Utc::now().timestamp() as f64);
        }
        match frame_result {
            Some(Ok(Message::Binary(data))) => {
                // STAGE-C.5: Depth-200 disconnect packet parse.
                // Same 14-byte layout as Depth-20 (12-byte header +
                // u16 LE reason code at bytes 12-13). Feed code 50
                // at byte 2 signals a disconnect frame.
                let maybe_disconnect_code: Option<crate::websocket::types::DisconnectCode> =
                    if data.len() >= 14
                        && data[2] == tickvault_common::constants::RESPONSE_CODE_DISCONNECT
                    {
                        let raw = u16::from_le_bytes([data[12], data[13]]);
                        Some(crate::websocket::types::DisconnectCode::from_u16(raw))
                    } else {
                        None
                    };

                if !first_frame_received {
                    first_frame_received = true;
                    m_active.set(1.0);
                    if let Some(signal) = connected_signal.take() {
                        let _ = signal.send(());
                    }
                }
                m_frames.increment(1);

                // STAGE-C (P1.2+P1.3): durable WAL first, live forward second.
                if let Some(spill) = wal_spill {
                    // O(1) EXEMPT: one Vec<u8> copy per frame (≤3212 B for 200-depth,
                    // bounded by protocol max) to hand owned memory to the disk writer.
                    let frame_vec = data.to_vec();
                    let outcome = spill.append(WsType::Depth200, frame_vec);
                    if outcome == AppendOutcome::Dropped {
                        error!(
                            security_id,
                            label = %label,
                            "CRITICAL: WAL spill dropped Depth200 frame — disk writer stalled"
                        );
                    }
                }
                match frame_sender.try_send(data) {
                    Ok(()) => {}
                    Err(mpsc::error::TrySendError::Full(_dropped)) => {
                        warn!(
                            security_id,
                            label = %label,
                            channel_capacity = frame_sender.capacity(),
                            wal_attached = wal_spill.is_some(),
                            "200-depth live frame channel full — backpressure from downstream. \
                             Frame is durable in WAL; watchdog will restart consumer."
                        );
                        metrics::counter!(
                            "tv_ws_frame_live_backpressure_total",
                            "ws_type" => "depth_200",
                            "label" => label.to_string()
                        )
                        .increment(1);
                        // Catalog-aligned metric (see depth_20 site above).
                        metrics::counter!(
                            "tv_depth_frames_dropped_total",
                            "type" => "channel_full",
                            "depth" => "200"
                        )
                        .increment(1);
                    }
                    Err(mpsc::error::TrySendError::Closed(_)) => {
                        m_active.set(0.0);
                        break Ok(());
                    }
                }

                // STAGE-C.5: surface disconnect after forward.
                if let Some(code) = maybe_disconnect_code {
                    warn!(
                        security_id,
                        label = %label,
                        disconnect_code = %code,
                        reconnectable = code.is_reconnectable(),
                        requires_token_refresh = code.requires_token_refresh(),
                        "200-depth: Dhan binary disconnect packet — surfacing to reconnect loop"
                    );
                    metrics::counter!(
                        "tv_ws_dhan_disconnect_total",
                        "code" => format!("{code}"), // O(1) EXEMPT: cold path, once per disconnect
                        "ws_type" => "depth_200"
                    )
                    .increment(1);
                    m_active.set(0.0);
                    break Err(WebSocketError::DhanDisconnect { code });
                }
            }
            Some(Ok(Message::Ping(data))) => {
                let pong_timeout = Duration::from_secs(DEPTH_PONG_TIMEOUT_SECS);
                match time::timeout(pong_timeout, write.send(Message::Pong(data))).await {
                    Ok(Ok(())) => {}
                    Ok(Err(err)) => {
                        m_active.set(0.0);
                        break Err(WebSocketError::ConnectionFailed {
                            url: DHAN_TWO_HUNDRED_DEPTH_WS_BASE_URL.to_string(),
                            source: err,
                        });
                    }
                    Err(_) => {
                        m_active.set(0.0);
                        warn!(
                            security_id,
                            label = %label,
                            timeout_secs = DEPTH_PONG_TIMEOUT_SECS,
                            "{prefix}: pong send timed out — connection likely dead"
                        );
                        break Err(WebSocketError::ReadTimeout {
                            connection_id: 98,
                            timeout_secs: DEPTH_PONG_TIMEOUT_SECS,
                        });
                    }
                }
            }
            Some(Ok(Message::Pong(_))) => {}
            Some(Ok(Message::Close(_))) => {
                m_active.set(0.0);
                info!(security_id, label = %label, "{prefix}: server sent close frame");
                break Ok(());
            }
            Some(Err(err)) => {
                m_active.set(0.0);
                break Err(WebSocketError::ConnectionFailed {
                    url: DHAN_TWO_HUNDRED_DEPTH_WS_BASE_URL.to_string(),
                    source: err,
                });
            }
            None => {
                m_active.set(0.0);
                break Ok(());
            }
            _ => {}
        }
    };

    // STAGE-C.3: Always abort the watchdog task when the read loop exits.
    watchdog_handle.abort();
    loop_result
    // O(1) EXEMPT: end
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_depth_command_has_initial_subscribe_variants() {
        // Plan item D (2026-04-22): Phase 2 scheduler dispatches the 09:12:30
        // IST initial subscribe via these variants. Using string-only fields
        // means the scheduler can construct them without any WebSocket state.
        let _ = DepthCommand::InitialSubscribe20 {
            subscribe_messages: vec!["{\"RequestCode\":23}".to_string()],
        };
        let _ = DepthCommand::InitialSubscribe200 {
            subscribe_message: "{\"RequestCode\":23}".to_string(),
        };
    }

    #[test]
    fn test_depth_command_initial_subscribe_20_holds_batch() {
        let msgs = vec![
            "{\"RequestCode\":23,\"InstrumentCount\":1}".to_string(),
            "{\"RequestCode\":23,\"InstrumentCount\":2}".to_string(),
        ];
        let cmd = DepthCommand::InitialSubscribe20 {
            subscribe_messages: msgs.clone(),
        };
        if let DepthCommand::InitialSubscribe20 { subscribe_messages } = cmd {
            assert_eq!(subscribe_messages, msgs);
        } else {
            panic!("expected InitialSubscribe20 variant");
        }
    }

    #[test]
    fn test_depth_command_initial_subscribe_200_holds_single_message() {
        let msg = "{\"RequestCode\":23,\"ExchangeSegment\":\"NSE_FNO\",\"SecurityId\":\"1333\"}"
            .to_string();
        let cmd = DepthCommand::InitialSubscribe200 {
            subscribe_message: msg.clone(),
        };
        if let DepthCommand::InitialSubscribe200 { subscribe_message } = cmd {
            assert_eq!(subscribe_message, msg);
        } else {
            panic!("expected InitialSubscribe200 variant");
        }
    }

    #[test]
    fn test_depth_constants() {
        assert_eq!(DEPTH_SUBSCRIPTION_BATCH_SIZE, 50);
        // STAGE-B: DEPTH_READ_TIMEOUT_SECS removed — client-side read deadline
        // was redundant with Dhan's own 40s server-side pong timeout.
        assert!(DEPTH_RECONNECT_MAX_MS <= 60000);
    }

    #[test]
    fn test_backoff_calculation() {
        // Verify backoff caps at max. Initial = 500ms (2026-04-21 uniform
        // fast-retry parity with main feed).
        let delay_0 = DEPTH_RECONNECT_INITIAL_MS
            .saturating_mul(1)
            .min(DEPTH_RECONNECT_MAX_MS);
        assert_eq!(delay_0, 500);

        let delay_6 = DEPTH_RECONNECT_INITIAL_MS
            .saturating_mul(1u64.checked_shl(6).unwrap_or(u64::MAX))
            .min(DEPTH_RECONNECT_MAX_MS);
        assert_eq!(delay_6, 30000); // 500 * 64 = 32000, capped at 30000
    }

    /// Plan item B (2026-04-23): verifies that empty `instruments` is NO
    /// LONGER an early-exit `Ok(())` path — the connection function
    /// attempts to enter the connect loop so it can wait for
    /// `InitialSubscribe20` from the 09:13 dispatcher. We prove the old
    /// early-exit is gone by asserting the future does NOT complete
    /// within a short timeout (the real code will either connect to
    /// Dhan or retry-backoff on NoTokenAvailable — either way it is
    /// NOT an immediate `Ok(())`).
    #[tokio::test]
    async fn test_run_twenty_depth_empty_instruments_enters_deferred_mode_not_early_ok() {
        let (tx, _rx) = mpsc::channel(16);
        let token_handle: TokenHandle =
            std::sync::Arc::new(arc_swap::ArcSwap::new(std::sync::Arc::new(None)));
        let (_cmd_tx, cmd_rx) = mpsc::channel(4);
        let fut = run_twenty_depth_connection(
            token_handle,
            "test".to_string(),
            vec![],
            tx,
            "TEST".to_string(),
            None,
            None,
            cmd_rx,
            None, // no notifier in unit test
        );
        // Old behaviour: empty instruments returned `Ok(())` synchronously
        // before any async yield. If the future completes inside 200ms,
        // the old early-exit is back — fail loudly.
        let outcome = tokio::time::timeout(Duration::from_millis(200), fut).await;
        assert!(
            outcome.is_err(),
            "empty instruments must enter the connect/reconnect loop (deferred mode); \
             early-exit Ok(()) regression detected. Result = {outcome:?}"
        );
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
        // 500ms — uniform fast first-retry parity with the main feed.
        // 2026-04-21: lowered from 1000ms; see ws_telegram_visibility_guard.
        assert_eq!(DEPTH_RECONNECT_INITIAL_MS, 500);
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
        // 2026-04-21: initial lowered from 1000ms -> 500ms for parity
        // with main feed. Exponential curve: 500, 1000, 2000, 4000, ...
        let delay_0 = DEPTH_RECONNECT_INITIAL_MS.min(DEPTH_RECONNECT_MAX_MS);
        let delay_1 = DEPTH_RECONNECT_INITIAL_MS
            .saturating_mul(2)
            .min(DEPTH_RECONNECT_MAX_MS);
        let delay_2 = DEPTH_RECONNECT_INITIAL_MS
            .saturating_mul(4)
            .min(DEPTH_RECONNECT_MAX_MS);
        assert_eq!(delay_0, 500);
        assert_eq!(delay_1, 1000);
        assert_eq!(delay_2, 2000);
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
        // 2026-04-23: Python SDK `dhanhq==2.2.0rc1` verified root path `/` is the
        // working URL for 200-depth. Reverses Dhan ticket #5519522 which had
        // told us to use `/twohundreddepth` (caused ResetWithoutClosingHandshake).
        assert_eq!(
            DHAN_TWO_HUNDRED_DEPTH_WS_BASE_URL,
            "wss://full-depth-api.dhan.co"
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
        let (_cmd_tx, mut cmd_rx) = mpsc::channel(4);
        let result = connect_and_run_depth(
            &token_handle,
            "test",
            &[],
            &tx,
            0,
            &mut None,
            "depth-20lvl-TEST",
            "TEST",
            None,
            &mut cmd_rx,
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
        let (_cmd_tx, mut cmd_rx) = mpsc::channel(4);
        let result = connect_and_run_200_depth(
            &token_handle,
            "test",
            "{}",
            &tx,
            "test-label",
            &mut None,
            12345,
            "TEST-LABEL",
            None,
            &mut cmd_rx,
            crate::websocket::depth_200_variants::VARIANTS[0],
            None,
        )
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
        //
        // STAGE-C.2 fix: the forbidden pattern is assembled at runtime from
        // fragments so the literal string never appears verbatim in this
        // source file — otherwise `include_str!` + `.contains()` would match
        // itself and the test would always fail.
        let source = include_str!("depth_connection.rs");
        let fragment_a = "time::timeout(read_";
        let fragment_b = "timeout, read.next()";
        let forbidden = format!("{fragment_a}{fragment_b})");
        assert!(
            !source.contains(&forbidden),
            "depth read loop must not wrap read.next() in a client-side read timeout"
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

        // Verify backoff grows: attempt 0 = 500ms, attempt 4 = 8s
        // (initial = 500ms post-2026-04-21 fast-retry parity)
        let delay_0 = DEPTH_RECONNECT_INITIAL_MS
            .saturating_mul(1u64.checked_shl(0).unwrap_or(u64::MAX))
            .min(DEPTH_RECONNECT_MAX_MS);
        let delay_4 = DEPTH_RECONNECT_INITIAL_MS
            .saturating_mul(1u64.checked_shl(4).unwrap_or(u64::MAX))
            .min(DEPTH_RECONNECT_MAX_MS);
        assert_eq!(delay_0, 500, "first attempt: 500ms");
        assert_eq!(delay_4, 8000, "fifth attempt: 8s");

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
    fn test_two_hundred_depth_url_uses_root_path() {
        // 2026-04-23: Python SDK `dhanhq==2.2.0rc1` verified root path `/` is the
        // working URL. `/twohundreddepth` kept TCP-resetting our Rust client
        // for 2+ weeks. The URL builder now explicitly emits `/?token=...` so
        // the resulting URL matches the Python SDK output exactly.
        let base = DHAN_TWO_HUNDRED_DEPTH_WS_BASE_URL.trim_end_matches('/');
        let url = format!("{base}/?token=TEST&clientId=TEST&authType=2",);
        assert!(
            url.starts_with("wss://full-depth-api.dhan.co/?token="),
            "200-level must use root path: {url}"
        );
        assert!(
            !url.contains("/twohundreddepth"),
            "must NOT contain the old /twohundreddepth path: {url}"
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
