//! Supervised order/position PUSH runner (order-push Stage C, 2026-07-16) —
//! bootstrap + read loop over the Stage B NATS-over-WS transport primitives.
//!
//! # Session shape (bootstrap → read loop)
//! 1. Access token via the caller-supplied [`GrowwAccessTokenProvider`] —
//!    a READ-ONLY consumer of the shared minter's SSM parameter upstream
//!    (`groww-shared-token-minter-2026-07-02.md`); this runner NEVER mints an
//!    access token, in any environment, ever.
//! 2. Fresh ephemeral ed25519 session keypair ([`GrowwSessionKeypair`]) —
//!    regenerated EVERY session, so every auth-class retry automatically gets
//!    a fresh keypair + a fresh socket-token mint.
//! 3. Per-session socket-token mint ([`mint_socket_token`]).
//! 4. WebSocket connect to [`GROWW_SOCKET_URL`] (tokio-tungstenite; the boot
//!    Step-1 process-wide rustls CryptoProvider install covers the TLS leg).
//! 5. NATS `INFO` → nonce → sign → `CONNECT` → `SUB` the 3 push subjects.
//! 6. Read loop: NATS framing → route MSG by subject class → proto decode →
//!    order/trade frames through the [`super::order_mapper`] into the
//!    full-fidelity fan-out; position frames through [`super::position`];
//!    NATS `PING` → `PONG`.
//!
//! # Failure taxonomy (GROWW-PUSH-01..04)
//! - Connect/transport failures (WS connect, socket EOF, framing violations,
//!   non-auth mint failures) → `GROWW-PUSH-01` + bounded expo reconnect
//!   (5s → 60s cap; the ladder resets after a session that stayed connected
//!   ≥ 60s).
//! - Auth-class failures (token-provider read failure, mint 401/403, missing
//!   `INFO` nonce, auth-class `-ERR`) → `GROWW-PUSH-02` + a fresh keypair +
//!   fresh mint on retry, the access token RE-READ via the provider (never
//!   minted), paced at ≥ 60s between auth-class attempts.
//! - Protobuf decode failures → `GROWW-PUSH-03`, counted, frame skipped, the
//!   session survives.
//! - Runner task death → `GROWW-PUSH-04` + supervised 5s-backoff respawn
//!   (the house WS-GAP-05 pattern).
//!
//! # Panic honesty (the TICK-FLUSH-01 wording)
//! The workspace release profile sets `panic = "abort"`, so in the PRODUCTION
//! binary a panicked runner task ABORTS the process at the panic site — the
//! `GROWW-PUSH-04` panic-respawn arm is an unwind (dev/test) build self-heal
//! only; recovery for a release-build panic is process restart. No claim of
//! in-process panic self-healing is made for release builds.
//!
//! # Bounds
//! Everything is bounded: the read reassembly buffer is capped at
//! [`MAX_READ_BUFFER_BYTES`] (the framing layer separately rejects any single
//! MSG payload over 1 MiB), the fan-out sinks are bounded `try_send` queues,
//! backoffs are capped, and the handshake carries a hard timeout. There are
//! no unbounded channels anywhere on this path.

use std::sync::Arc;
use std::time::Duration;

use futures_util::future::BoxFuture;
use futures_util::{SinkExt, StreamExt};
use secrecy::{ExposeSecret, SecretString};
use tickvault_common::constants::GROWW_SOCKET_URL;
use tickvault_common::error_code::ErrorCode;
use tokio::sync::watch;
use tokio::time::Instant;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, error, info};

use super::super::events::GrowwPushFanOut;
use super::connect::{build_connect_frame, extract_nonce};
use super::nats::{self, NatsParseError, NatsServerOp};
use super::nkey::GrowwSessionKeypair;
use super::order_events::{GrowwPushCapture, build_groww_order_event_record, now_ist_epoch_nanos};
use super::order_mapper::map_order_broadcast;
use super::position;
use super::proto::decode_order_details_broadcast;
use super::socket_token::{SocketTokenError, is_auth_status, mint_client, mint_socket_token};
use super::subjects::{
    PushSubjectKind, classify_push_subject, equity_order_updates_subject,
    fno_order_updates_subject, fno_position_updates_subject,
};

/// First reconnect wait after a failure (seconds).
const RECONNECT_BASE_SECS: u64 = 5;
/// Reconnect wait ceiling (seconds).
const RECONNECT_CAP_SECS: u64 = 60;
/// A session that stayed CONNECTED at least this long resets the backoff
/// ladder (the WS-GAP-05 stability-window precedent).
const STABLE_SESSION_SECS: u64 = 60;
/// Minimum wait between auth-class retries — every retry re-reads the access
/// token via the provider, so this floor is the ≥60s SSM re-read pacing
/// (the sidecar `AUTH_RETRY_FLOOR_SECS` precedent; never a mint).
const AUTH_RETRY_FLOOR_SECS: u64 = 60;
/// Supervisor respawn backoff after a runner-task death (seconds).
const SUPERVISOR_RESPAWN_BACKOFF_SECS: u64 = 5;
/// Hard bound on WS connect + the INFO→CONNECT→SUB handshake (seconds).
const HANDSHAKE_TIMEOUT_SECS: u64 = 30;
/// Read reassembly buffer hard cap: one max NATS payload (1 MiB) plus header
/// headroom. A buffer past this with no parseable frame is a protocol
/// violation → close + reconnect (never unbounded buffering).
const MAX_READ_BUFFER_BYTES: usize = nats::MAX_MSG_PAYLOAD_BYTES + 8 * 1024;
/// Bounded length for a sanitized server `-ERR` text in logs.
const ERR_TEXT_MAX_CHARS: usize = 160;

/// Caller-supplied access-token source. The upstream MUST be the read-only
/// SSM parameter written by the shared minter Lambda
/// (`groww-shared-token-minter-2026-07-02.md`) — implementations NEVER mint
/// and NEVER embed the token in the error string.
pub trait GrowwAccessTokenProvider: Send + Sync {
    /// Read (never mint) the current shared Groww access token. The `Err`
    /// string is a secret-free failure description.
    fn access_token(&self) -> BoxFuture<'_, Result<SecretString, String>>;
}

/// Why one session ended in failure. Never carries the access token, the
/// socket JWT, or an unsanitized server body.
#[derive(Debug)]
enum SessionError {
    /// The token provider's read failed (auth class — SSM re-read next try).
    TokenRead(String),
    /// Local ed25519 keypair generation failed (connect class).
    Keypair(String),
    /// The socket-token mint failed ([`is_auth_status`] splits auth vs
    /// transport class).
    Mint(SocketTokenError),
    /// The WebSocket connect failed or timed out (connect class).
    WsConnect(String),
    /// The INFO→CONNECT→SUB handshake exceeded its hard timeout.
    HandshakeTimeout,
    /// The server `INFO` carried no nonce — Groww's feed always nonces, so
    /// this is auth-path breakage (auth class).
    MissingNonce,
    /// A WS read/write failed mid-session (connect class).
    Transport(String),
    /// The WS stream ended (server close / EOF — connect class).
    StreamClosed,
    /// A NATS framing protocol violation (connect class — close + reconnect
    /// per the framing module's contract).
    Framing(NatsParseError),
    /// The reassembly buffer exceeded [`MAX_READ_BUFFER_BYTES`] without a
    /// parseable frame (connect class).
    BufferOverflow,
    /// The server sent `-ERR`; `auth` is the pre-classified auth-text
    /// verdict, `sanitized` a bounded control-char-stripped copy.
    ServerErr { auth: bool, sanitized: String },
}

impl core::fmt::Display for SessionError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::TokenRead(e) => write!(f, "access-token read failed: {e}"),
            Self::Keypair(e) => write!(f, "session keypair generation failed: {e}"),
            Self::Mint(e) => write!(f, "socket-token mint failed: {e}"),
            Self::WsConnect(e) => write!(f, "websocket connect failed: {e}"),
            Self::HandshakeTimeout => f.write_str("handshake timed out"),
            Self::MissingNonce => f.write_str("server INFO carried no auth nonce"),
            Self::Transport(e) => write!(f, "websocket transport failed: {e}"),
            Self::StreamClosed => f.write_str("websocket stream closed by server"),
            Self::Framing(e) => write!(f, "nats framing violation: {e}"),
            Self::BufferOverflow => f.write_str("read buffer overflow without a parseable frame"),
            Self::ServerErr { auth, sanitized } => {
                write!(f, "server -ERR (auth={auth}): {sanitized}")
            }
        }
    }
}

/// The two failure classes of the GROWW-PUSH-01/02 taxonomy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FailureClass {
    /// Connect/transport class → `GROWW-PUSH-01`, expo backoff.
    Connect,
    /// Auth class → `GROWW-PUSH-02`, fresh keypair + fresh mint + token
    /// re-read at the ≥60s floor.
    Auth,
}

/// Total classification of a session failure (pure).
fn classify_session_error(err: &SessionError) -> FailureClass {
    match err {
        SessionError::TokenRead(_) | SessionError::MissingNonce => FailureClass::Auth,
        SessionError::Mint(e) if is_auth_status(e) => FailureClass::Auth,
        SessionError::ServerErr { auth: true, .. } => FailureClass::Auth,
        _ => FailureClass::Connect,
    }
}

/// Bounded exponential reconnect ladder: 5s, 10s, 20s, 40s, 60s cap (pure).
fn reconnect_backoff_secs(consecutive_failures: u32) -> u64 {
    let exp = consecutive_failures.saturating_sub(1).min(6);
    RECONNECT_BASE_SECS
        .saturating_mul(1_u64 << exp)
        .min(RECONNECT_CAP_SECS)
}

/// Auth-class retries never run faster than the ≥60s token re-read floor
/// (pure).
fn auth_retry_wait_secs(base_wait_secs: u64) -> u64 {
    base_wait_secs.max(AUTH_RETRY_FLOOR_SECS)
}

/// Next consecutive-failure streak: a session that was CONNECTED and lived
/// ≥ [`STABLE_SESSION_SECS`] resets the ladder (pure).
fn next_failure_streak(previous: u32, connected: bool, session_secs: u64) -> u32 {
    if connected && session_secs >= STABLE_SESSION_SECS {
        1
    } else {
        previous.saturating_add(1)
    }
}

/// Auth-text classifier for server `-ERR` messages (pure; case-insensitive).
fn is_auth_err_text(msg: &str) -> bool {
    let lower = msg.to_ascii_lowercase();
    ["authorization", "authentication", "permissions", "user jwt"]
        .iter()
        .any(|needle| lower.contains(needle))
}

/// Bounded, control-char-stripped copy of a server `-ERR` text for logging
/// (server-controlled input — never echoed raw or unbounded; pure).
fn sanitize_err_text(msg: &str) -> String {
    msg.chars()
        .filter(|c| !c.is_control())
        .take(ERR_TEXT_MAX_CHARS)
        .collect()
}

/// Which push stream a delivered MSG subject routes to (pure).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PushStream {
    /// F&O or equity ORDER/trade updates → the order mapper + fan-out.
    Order,
    /// F&O POSITION updates → decode + count + log.
    Position,
}

/// Route a delivered MSG subject; `None` = not one of the 3 known push
/// prefixes (a foreign payload can never reach the decoders) (pure).
fn route_subject(subject: &str) -> Option<PushStream> {
    match classify_push_subject(subject) {
        Some((PushSubjectKind::FnoOrder | PushSubjectKind::EquityOrder, _)) => {
            Some(PushStream::Order)
        }
        Some((PushSubjectKind::FnoPosition, _)) => Some(PushStream::Position),
        None => None,
    }
}

/// Epoch milliseconds wall clock — the seam's `received_at_ms` anchor.
fn now_epoch_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

/// How one session ended.
enum SessionEnd {
    /// The stop/disable signal flipped — clean exit, no respawn.
    Shutdown,
    /// The session failed; `connected` = whether the CONNECT+SUB handshake
    /// had completed (drives connect-failure accounting + ladder reset).
    Failed {
        error: SessionError,
        connected: bool,
    },
}

/// Actions the caller must perform after a frame drain.
struct DrainActions {
    /// Number of NATS `PING`s observed — one `PONG` owed each.
    pongs: u32,
}

/// Handle one routed MSG frame: order frames decode → map → fan-out (+ the
/// ADDITIVE full-fidelity capture record, best-effort); position frames
/// decode + count + log + capture; unknown subjects are counted and
/// skipped. A decode failure NEVER ends the session (GROWW-PUSH-03).
fn handle_msg(
    subject: &str,
    payload: &[u8],
    fan_out: &GrowwPushFanOut,
    capture: &GrowwPushCapture,
) {
    match route_subject(subject) {
        Some(PushStream::Order) => match decode_order_details_broadcast(payload) {
            Ok(dto) => {
                if let Some(event) = map_order_broadcast(&dto, now_epoch_ms()) {
                    metrics::counter!("tv_groww_push_order_events_total").increment(1);
                    fan_out.deliver(&event);
                } else {
                    debug!(
                        "groww push: order frame carried no order detail (stage-only) — skipped"
                    );
                }
                // ADDITIVE full-fidelity capture lane (ORDER-EVT-01): every
                // decoded field → the order_update_events forensic table.
                // Best-effort try_send; never blocks or fails this loop.
                if let Some(record) = build_groww_order_event_record(&dto, now_ist_epoch_nanos()) {
                    capture.publish_order(record);
                }
            }
            Err(e) => {
                error!(
                    code = ErrorCode::GrowwPush03DecodeFailed.code_str(),
                    error = %e,
                    payload_len = payload.len(),
                    "groww push: order payload failed protobuf decode — frame skipped, session continues"
                );
                metrics::counter!("tv_groww_push_decode_errors_total", "kind" => "order")
                    .increment(1);
            }
        },
        Some(PushStream::Position) => {
            // Decode + count + log + best-effort full-fidelity capture
            // (position_update_events — ORDER-EVT-01 subsystem).
            let _outcome =
                position::handle_position_payload(payload, capture, now_ist_epoch_nanos());
        }
        None => {
            metrics::counter!("tv_groww_push_decode_errors_total", "kind" => "unknown_subject")
                .increment(1);
            debug!("groww push: MSG on unknown subject — skipped");
        }
    }
}

/// Drain every complete NATS frame from the front of `buf`, routing MSGs and
/// collecting owed `PONG`s. Consumed bytes are removed from `buf`; a partial
/// trailing frame stays for the next read. A framing violation or server
/// `-ERR` ends the session via `Err`.
fn drain_frames(
    buf: &mut Vec<u8>,
    fan_out: &GrowwPushFanOut,
    capture: &GrowwPushCapture,
) -> Result<DrainActions, SessionError> {
    let mut pos: usize = 0;
    let mut pongs: u32 = 0;
    let result = loop {
        match nats::parse_frame(&buf[pos..]) {
            Ok(None) => break Ok(()),
            Ok(Some((op, consumed))) => {
                match op {
                    NatsServerOp::Ping => pongs = pongs.saturating_add(1),
                    NatsServerOp::Pong | NatsServerOp::Ok | NatsServerOp::Info(_) => {}
                    NatsServerOp::Err(msg) => {
                        break Err(SessionError::ServerErr {
                            auth: is_auth_err_text(msg),
                            sanitized: sanitize_err_text(msg),
                        });
                    }
                    NatsServerOp::Msg {
                        subject, payload, ..
                    } => handle_msg(subject, payload, fan_out, capture),
                }
                pos += consumed;
            }
            Err(e) => {
                metrics::counter!("tv_groww_push_decode_errors_total", "kind" => "framing")
                    .increment(1);
                break Err(SessionError::Framing(e));
            }
        }
    };
    buf.drain(..pos);
    result.map(|()| DrainActions { pongs })
}

/// The text/binary payload of a WS message, if it carries NATS bytes.
fn ws_payload(msg: &Message) -> Option<&[u8]> {
    match msg {
        Message::Binary(b) => Some(b.as_ref()),
        Message::Text(t) => Some(t.as_str().as_bytes()),
        _ => None,
    }
}

/// True when the stop watch says stop (a dropped sender is fail-safe stop).
fn stop_requested(stop_rx: &watch::Receiver<bool>) -> bool {
    *stop_rx.borrow()
}

/// Sleep `secs` unless the stop signal fires first. Returns `true` when the
/// caller must stop.
async fn wait_or_stop(stop_rx: &mut watch::Receiver<bool>, secs: u64) -> bool {
    if stop_requested(stop_rx) {
        return true;
    }
    tokio::select! {
        () = tokio::time::sleep(Duration::from_secs(secs)) => stop_requested(stop_rx),
        changed = stop_rx.changed() => changed.is_err() || stop_requested(stop_rx),
    }
}

/// Run exactly one push session: bootstrap, handshake, subscribe, read loop.
async fn run_one_session(
    provider: &dyn GrowwAccessTokenProvider,
    fan_out: &GrowwPushFanOut,
    capture: &GrowwPushCapture,
    stop_rx: &mut watch::Receiver<bool>,
) -> SessionEnd {
    let failed = |error: SessionError, connected: bool| SessionEnd::Failed { error, connected };

    // 1. Access token — READ-ONLY via the provider (never minted here).
    let access_token = match provider.access_token().await {
        Ok(t) => t,
        Err(e) => return failed(SessionError::TokenRead(e), false),
    };

    // 2. Fresh ephemeral session keypair (regenerated every session).
    let keypair = match GrowwSessionKeypair::generate() {
        Ok(k) => k,
        Err(e) => return failed(SessionError::Keypair(e.to_string()), false),
    };

    // 3. Per-session socket-token mint.
    let client = match mint_client() {
        Ok(c) => c,
        Err(e) => return failed(SessionError::Mint(e), false),
    };
    let socket_token = match mint_socket_token(&client, &access_token, keypair.nkey_public()).await
    {
        Ok(t) => t,
        Err(e) => return failed(SessionError::Mint(e), false),
    };

    // 4. WebSocket connect (bounded).
    let ws = match tokio::time::timeout(
        Duration::from_secs(HANDSHAKE_TIMEOUT_SECS),
        connect_async(GROWW_SOCKET_URL),
    )
    .await
    {
        Ok(Ok((ws, _response))) => ws,
        Ok(Err(e)) => return failed(SessionError::WsConnect(e.to_string()), false),
        Err(_elapsed) => {
            return failed(
                SessionError::WsConnect("connect timed out".to_owned()),
                false,
            );
        }
    };
    let (mut write, mut read) = ws.split();
    let mut buf: Vec<u8> = Vec::with_capacity(8 * 1024);

    // 5. Handshake: wait for INFO → sign nonce → CONNECT → SUB ×3 (bounded).
    let handshake = tokio::time::timeout(Duration::from_secs(HANDSHAKE_TIMEOUT_SECS), async {
        loop {
            match read.next().await {
                Some(Ok(msg)) => {
                    if let Some(bytes) = ws_payload(&msg) {
                        buf.extend_from_slice(bytes);
                        if buf.len() > MAX_READ_BUFFER_BYTES {
                            return Err(SessionError::BufferOverflow);
                        }
                        if let Some(nonce) = try_extract_handshake_nonce(&mut buf)? {
                            return Ok(nonce);
                        }
                    }
                }
                Some(Err(e)) => return Err(SessionError::Transport(e.to_string())),
                None => return Err(SessionError::StreamClosed),
            }
        }
    })
    .await;
    let nonce = match handshake {
        Ok(Ok(nonce)) => nonce,
        Ok(Err(e)) => return failed(e, false),
        Err(_elapsed) => return failed(SessionError::HandshakeTimeout, false),
    };

    let signature = keypair.sign_nonce_b64url(nonce.as_bytes());
    let connect_frame = build_connect_frame(&socket_token.jwt, &signature);
    if let Err(e) = write
        .send(Message::text(connect_frame.expose_secret()))
        .await
    {
        return failed(SessionError::Transport(e.to_string()), false);
    }
    let subscription_id = socket_token.subscription_id.as_str();
    let subjects = [
        fno_order_updates_subject(subscription_id),
        fno_position_updates_subject(subscription_id),
        equity_order_updates_subject(subscription_id),
    ];
    for (index, subject) in subjects.iter().enumerate() {
        // NATS sids "1".."3" — stable per session.
        let sid = match index {
            0 => "1",
            1 => "2",
            _ => "3",
        };
        if let Err(e) = write
            .send(Message::text(nats::build_sub(subject, sid)))
            .await
        {
            return failed(SessionError::Transport(e.to_string()), false);
        }
    }
    if let Err(e) = write.flush().await {
        return failed(SessionError::Transport(e.to_string()), false);
    }
    metrics::counter!("tv_groww_push_connects_total", "outcome" => "ok").increment(1);
    info!(
        subjects = subjects.len(),
        subscription_id_len = subscription_id.len(),
        "groww push: connected + subscribed — awaiting order/position events"
    );

    // 6. Read loop (connected from here on).
    loop {
        tokio::select! {
            changed = stop_rx.changed() => {
                if changed.is_err() || stop_requested(stop_rx) {
                    return SessionEnd::Shutdown;
                }
            }
            next = read.next() => match next {
                Some(Ok(Message::Ping(p))) => {
                    // WS-protocol ping — answer explicitly (belt + braces over
                    // tungstenite's queued auto-pong).
                    if let Err(e) = write.send(Message::Pong(p)).await {
                        return failed(SessionError::Transport(e.to_string()), true);
                    }
                }
                Some(Ok(Message::Close(_))) => {
                    return failed(SessionError::StreamClosed, true);
                }
                Some(Ok(msg)) => {
                    if let Some(bytes) = ws_payload(&msg) {
                        buf.extend_from_slice(bytes);
                        if buf.len() > MAX_READ_BUFFER_BYTES {
                            return failed(SessionError::BufferOverflow, true);
                        }
                        let actions = match drain_frames(&mut buf, fan_out, capture) {
                            Ok(a) => a,
                            Err(e) => return failed(e, true),
                        };
                        for _ in 0..actions.pongs {
                            // NATS-level PING → PONG per the framing contract.
                            if let Err(e) = write.send(Message::text(nats::build_pong())).await {
                                return failed(SessionError::Transport(e.to_string()), true);
                            }
                        }
                    }
                }
                Some(Err(e)) => return failed(SessionError::Transport(e.to_string()), true),
                None => return failed(SessionError::StreamClosed, true),
            }
        }
    }
}

/// Scan `buf` for a complete `INFO` frame; on the first one, extract the
/// nonce (consuming every scanned frame). `Ok(None)` = no complete INFO yet.
fn try_extract_handshake_nonce(buf: &mut Vec<u8>) -> Result<Option<String>, SessionError> {
    let mut pos: usize = 0;
    let result = loop {
        match nats::parse_frame(&buf[pos..]) {
            Ok(None) => break Ok(None),
            Ok(Some((op, consumed))) => {
                if let NatsServerOp::Info(json) = op {
                    let nonce = extract_nonce(json);
                    pos += consumed;
                    break match nonce {
                        Some(n) => Ok(Some(n)),
                        None => Err(SessionError::MissingNonce),
                    };
                }
                // Pre-CONNECT frames other than INFO are skipped (no PONG owed
                // pre-auth; the handshake completes within the timeout).
                pos += consumed;
            }
            Err(e) => break Err(SessionError::Framing(e)),
        }
    };
    buf.drain(..pos);
    result
}

/// The inner reconnect loop: one session at a time, classified failures,
/// bounded backoff, ≥60s auth pacing, ladder reset after a stable session.
/// Returns ONLY on the stop signal.
async fn run_groww_push_loop(
    provider: Arc<dyn GrowwAccessTokenProvider>,
    fan_out: Arc<GrowwPushFanOut>,
    capture: GrowwPushCapture,
    mut stop_rx: watch::Receiver<bool>,
) {
    let mut consecutive_failures: u32 = 0;
    loop {
        if stop_requested(&stop_rx) {
            return;
        }
        let session_start = Instant::now();
        match run_one_session(provider.as_ref(), &fan_out, &capture, &mut stop_rx).await {
            SessionEnd::Shutdown => return,
            SessionEnd::Failed { error, connected } => {
                if !connected {
                    metrics::counter!("tv_groww_push_connects_total", "outcome" => "failed")
                        .increment(1);
                }
                let session_secs = session_start.elapsed().as_secs();
                consecutive_failures =
                    next_failure_streak(consecutive_failures, connected, session_secs);
                let base_wait = reconnect_backoff_secs(consecutive_failures);
                let wait_secs = match classify_session_error(&error) {
                    FailureClass::Connect => {
                        error!(
                            code = ErrorCode::GrowwPush01ConnectFailed.code_str(),
                            error = %error,
                            consecutive_failures,
                            wait_secs = base_wait,
                            "groww push: connect/transport failure — reconnecting with bounded backoff"
                        );
                        metrics::counter!("tv_groww_push_reconnects_total", "reason" => "transport")
                            .increment(1);
                        base_wait
                    }
                    FailureClass::Auth => {
                        let paced = auth_retry_wait_secs(base_wait);
                        error!(
                            code = ErrorCode::GrowwPush02AuthFailed.code_str(),
                            error = %error,
                            consecutive_failures,
                            wait_secs = paced,
                            "groww push: auth-class failure — access token will be RE-READ (never minted) + fresh keypair/mint on retry"
                        );
                        metrics::counter!("tv_groww_push_reconnects_total", "reason" => "auth")
                            .increment(1);
                        paced
                    }
                };
                if wait_or_stop(&mut stop_rx, wait_secs).await {
                    return;
                }
            }
        }
    }
}

/// The supervised push runner (house WS-GAP-05 pattern): spawns the inner
/// reconnect loop and respawns it on ANY unexpected death — `GROWW-PUSH-04`
/// coded + counted, 5s backoff. Returns only when the stop/disable signal
/// flips (config-disabled or process shutdown), so a Stage D consumer stops
/// it cleanly by flipping the watch to `true`.
///
/// Panic honesty: release builds abort on panic (`panic = "abort"`), so the
/// `reason = "panic"` respawn arm self-heals in unwind (dev/test) builds only
/// — see the module docs.
// TEST-EXEMPT: live-socket supervision orchestration over the unit-tested pure helpers (backoff / classification / routing / drain) — the Stage D app consumer wires the spawn; no test may open a real Groww socket.
pub async fn run_groww_push_supervised(
    provider: Arc<dyn GrowwAccessTokenProvider>,
    fan_out: Arc<GrowwPushFanOut>,
    capture: GrowwPushCapture,
    stop_rx: watch::Receiver<bool>,
) {
    let mut outer_stop = stop_rx.clone();
    loop {
        if stop_requested(&outer_stop) {
            info!("groww push: runner stopped (shutdown/disable signal)");
            return;
        }
        let handle = tokio::spawn(run_groww_push_loop(
            Arc::clone(&provider),
            Arc::clone(&fan_out),
            capture.clone(),
            stop_rx.clone(),
        ));
        let reason: &'static str = match handle.await {
            Ok(()) => {
                if stop_requested(&outer_stop) {
                    info!("groww push: runner stopped (shutdown/disable signal)");
                    return;
                }
                // The inner loop returns only on stop — a clean exit without
                // the stop flag is an invariant break; respawn loudly.
                "clean_exit"
            }
            Err(join_err) if join_err.is_panic() => "panic",
            Err(_cancelled) => "cancelled",
        };
        error!(
            code = ErrorCode::GrowwPush04SupervisorRespawned.code_str(),
            reason,
            backoff_secs = SUPERVISOR_RESPAWN_BACKOFF_SECS,
            "groww push: runner task died — respawning after backoff (unwind-build self-heal; release panics abort the process)"
        );
        metrics::counter!("tv_groww_push_supervisor_respawn_total", "reason" => reason)
            .increment(1);
        if wait_or_stop(&mut outer_stop, SUPERVISOR_RESPAWN_BACKOFF_SECS).await {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::super::events::BoundedPushEventSink;
    use super::*;

    // --- backoff ladder ---

    #[test]
    fn backoff_ladder_is_5s_doubling_to_a_60s_cap() {
        assert_eq!(reconnect_backoff_secs(0), 5);
        assert_eq!(reconnect_backoff_secs(1), 5);
        assert_eq!(reconnect_backoff_secs(2), 10);
        assert_eq!(reconnect_backoff_secs(3), 20);
        assert_eq!(reconnect_backoff_secs(4), 40);
        assert_eq!(reconnect_backoff_secs(5), 60);
        assert_eq!(reconnect_backoff_secs(6), 60);
        assert_eq!(reconnect_backoff_secs(u32::MAX), 60);
    }

    #[test]
    fn stable_connected_session_resets_the_ladder() {
        assert_eq!(next_failure_streak(7, true, STABLE_SESSION_SECS), 1);
        assert_eq!(next_failure_streak(7, true, STABLE_SESSION_SECS + 100), 1);
    }

    #[test]
    fn short_or_unconnected_sessions_increment_the_ladder() {
        assert_eq!(next_failure_streak(2, true, STABLE_SESSION_SECS - 1), 3);
        // A long-but-never-connected bootstrap (mint timeouts) never resets.
        assert_eq!(next_failure_streak(2, false, 10_000), 3);
        assert_eq!(next_failure_streak(u32::MAX, false, 0), u32::MAX);
    }

    #[test]
    fn auth_retries_are_paced_at_the_60s_floor() {
        assert_eq!(auth_retry_wait_secs(5), 60);
        assert_eq!(auth_retry_wait_secs(60), 60);
        assert_eq!(auth_retry_wait_secs(120), 120);
    }

    // --- failure classification ---

    #[test]
    fn auth_class_failures_classify_as_auth() {
        for err in [
            SessionError::TokenRead("ssm read failed".to_owned()),
            SessionError::MissingNonce,
            SessionError::Mint(SocketTokenError::Status(401)),
            SessionError::Mint(SocketTokenError::Status(403)),
            SessionError::ServerErr {
                auth: true,
                sanitized: "Authorization Violation".to_owned(),
            },
        ] {
            assert_eq!(classify_session_error(&err), FailureClass::Auth, "{err}");
        }
    }

    #[test]
    fn connect_class_failures_classify_as_connect() {
        for err in [
            SessionError::Keypair("rng".to_owned()),
            SessionError::Mint(SocketTokenError::Status(500)),
            SessionError::Mint(SocketTokenError::Transport("dns".to_owned())),
            SessionError::Mint(SocketTokenError::MalformedResponse),
            SessionError::WsConnect("refused".to_owned()),
            SessionError::HandshakeTimeout,
            SessionError::Transport("reset".to_owned()),
            SessionError::StreamClosed,
            SessionError::Framing(NatsParseError::UnknownVerb),
            SessionError::BufferOverflow,
            SessionError::ServerErr {
                auth: false,
                sanitized: "Slow Consumer".to_owned(),
            },
        ] {
            assert_eq!(classify_session_error(&err), FailureClass::Connect, "{err}");
        }
    }

    #[test]
    fn auth_err_text_classifier_matches_the_nats_auth_vocabulary() {
        assert!(is_auth_err_text("Authorization Violation"));
        assert!(is_auth_err_text("authentication timeout"));
        assert!(is_auth_err_text("Permissions Violation for Subscription"));
        assert!(is_auth_err_text("bad user JWT"));
        assert!(!is_auth_err_text("Slow Consumer"));
        assert!(!is_auth_err_text("Maximum Payload Violation"));
    }

    #[test]
    fn err_text_sanitizer_strips_control_chars_and_bounds_length() {
        let hostile = format!("bad\r\nthing\x1b[31m{}", "x".repeat(500));
        let sanitized = sanitize_err_text(&hostile);
        assert!(!sanitized.contains('\r'));
        assert!(!sanitized.contains('\n'));
        assert!(!sanitized.contains('\x1b'));
        assert!(sanitized.chars().count() <= ERR_TEXT_MAX_CHARS);
    }

    // --- subject routing ---

    #[test]
    fn order_subjects_route_to_the_order_stream() {
        assert_eq!(
            route_subject("stocks_fo/order/updates.apex.sub-1"),
            Some(PushStream::Order)
        );
        assert_eq!(
            route_subject("stocks/order/updates.apex.sub-1"),
            Some(PushStream::Order)
        );
    }

    #[test]
    fn position_subjects_route_to_the_position_stream() {
        assert_eq!(
            route_subject("stocks_fo/position/updates.apex.sub-1"),
            Some(PushStream::Position)
        );
    }

    #[test]
    fn foreign_subjects_never_route() {
        assert_eq!(route_subject("/ld/eq/nse/price.RELIANCE"), None);
        assert_eq!(route_subject("stocks_fo/order/updates.apex."), None);
        assert_eq!(route_subject(""), None);
        assert_eq!(route_subject("garbage"), None);
    }

    // --- frame drain ---

    /// Minimal valid `OrderDetailsBroadCastDto` wire bytes: field 2
    /// (order_detail, len-delim) containing field 7 (groww_order_id = "AB").
    fn order_payload_bytes() -> Vec<u8> {
        let inner = [0x3A, 0x02, b'A', b'B']; // (7<<3)|2, len 2, "AB"
        let mut out = vec![0x12, 0x04]; // (2<<3)|2, len 4
        out.extend_from_slice(&inner);
        out
    }

    fn msg_frame(subject: &str, payload: &[u8]) -> Vec<u8> {
        let mut frame = format!("MSG {subject} 1 {}\r\n", payload.len()).into_bytes();
        frame.extend_from_slice(payload);
        frame.extend_from_slice(b"\r\n");
        frame
    }

    #[test]
    fn drain_counts_pings_and_removes_consumed_bytes() {
        let fan_out = GrowwPushFanOut::new(Vec::new());
        let mut buf = b"PING\r\nPING\r\n+OK\r\nPI".to_vec();
        let actions = drain_frames(&mut buf, &fan_out, &GrowwPushCapture::disabled())
            .unwrap_or_else(|e| panic!("drain: {e}"));
        assert_eq!(actions.pongs, 2);
        // The partial trailing frame stays for the next read.
        assert_eq!(buf, b"PI");
    }

    #[test]
    fn drain_routes_an_order_msg_into_the_full_fidelity_fan_out() {
        let (sink, mut rx) = BoundedPushEventSink::channel("test_runner");
        let fan_out = GrowwPushFanOut::new(vec![std::sync::Arc::new(sink)]);
        let mut buf = msg_frame("stocks_fo/order/updates.apex.sub-1", &order_payload_bytes());
        let actions = drain_frames(&mut buf, &fan_out, &GrowwPushCapture::disabled())
            .unwrap_or_else(|e| panic!("drain: {e}"));
        assert_eq!(actions.pongs, 0);
        assert!(buf.is_empty());
        let event = rx
            .try_recv()
            .unwrap_or_else(|_| panic!("one event expected"));
        assert_eq!(event.broker_order_id, "AB");
    }

    #[test]
    fn drain_publishes_an_order_capture_record_into_the_capture_sink() {
        let fan_out = GrowwPushFanOut::new(Vec::new());
        let (order_tx, mut order_rx) = tokio::sync::mpsc::channel(4);
        let (pos_tx, _pos_rx) = tokio::sync::mpsc::channel(4);
        let capture = GrowwPushCapture::new(order_tx, pos_tx);
        let mut buf = msg_frame("stocks_fo/order/updates.apex.sub-1", &order_payload_bytes());
        let actions =
            drain_frames(&mut buf, &fan_out, &capture).unwrap_or_else(|e| panic!("drain: {e}"));
        assert_eq!(actions.pongs, 0);
        let record = order_rx
            .try_recv()
            .unwrap_or_else(|_| panic!("one capture record expected"));
        assert_eq!(record.order_id, "AB");
        assert_eq!(record.source, "push");
        assert!(record.ts_ist_nanos > 0);
    }

    #[test]
    fn drain_skips_a_corrupt_order_payload_and_survives() {
        let fan_out = GrowwPushFanOut::new(Vec::new());
        // Length-delimited field claiming 100 bytes with 1 present.
        let mut buf = msg_frame("stocks_fo/order/updates.apex.sub-1", &[0x12, 0x64, 0x00]);
        buf.extend_from_slice(b"PING\r\n");
        let actions = drain_frames(&mut buf, &fan_out, &GrowwPushCapture::disabled())
            .unwrap_or_else(|e| panic!("drain: {e}"));
        // The session survived the decode failure and still saw the PING.
        assert_eq!(actions.pongs, 1);
        assert!(buf.is_empty());
    }

    #[test]
    fn drain_ends_the_session_on_a_server_err() {
        let fan_out = GrowwPushFanOut::new(Vec::new());
        let mut buf = b"-ERR 'Authorization Violation'\r\n".to_vec();
        match drain_frames(&mut buf, &fan_out, &GrowwPushCapture::disabled()) {
            Err(SessionError::ServerErr { auth, sanitized }) => {
                assert!(auth);
                assert!(sanitized.contains("Authorization"));
            }
            other => panic!("expected ServerErr, got {:?}", other.map(|a| a.pongs)),
        }
    }

    #[test]
    fn drain_ends_the_session_on_a_framing_violation() {
        let fan_out = GrowwPushFanOut::new(Vec::new());
        let mut buf = b"BOGUS\r\n".to_vec();
        match drain_frames(&mut buf, &fan_out, &GrowwPushCapture::disabled()) {
            Err(SessionError::Framing(NatsParseError::UnknownVerb)) => {}
            other => panic!("expected framing error, got {:?}", other.map(|a| a.pongs)),
        }
    }

    // --- handshake nonce extraction ---

    #[test]
    fn handshake_extracts_the_info_nonce_and_consumes_the_frame() {
        let mut buf = b"INFO {\"server_id\":\"x\",\"nonce\":\"N0nCe\"}\r\nPING\r\n".to_vec();
        let nonce = try_extract_handshake_nonce(&mut buf)
            .unwrap_or_else(|e| panic!("handshake: {e}"))
            .unwrap_or_else(|| panic!("nonce expected"));
        assert_eq!(nonce, "N0nCe");
        // Only the INFO frame is consumed; the rest stays for the read loop.
        assert_eq!(buf, b"PING\r\n");
    }

    #[test]
    fn handshake_reports_a_nonce_less_info_as_auth_path_breakage() {
        let mut buf = b"INFO {\"server_id\":\"x\"}\r\n".to_vec();
        match try_extract_handshake_nonce(&mut buf) {
            Err(SessionError::MissingNonce) => {}
            other => panic!("expected MissingNonce, got {other:?}"),
        }
    }

    #[test]
    fn handshake_waits_on_a_partial_info_frame() {
        let mut buf = b"INFO {\"nonce\":".to_vec();
        let got =
            try_extract_handshake_nonce(&mut buf).unwrap_or_else(|e| panic!("handshake: {e}"));
        assert_eq!(got, None);
        assert_eq!(buf, b"INFO {\"nonce\":");
    }
}
