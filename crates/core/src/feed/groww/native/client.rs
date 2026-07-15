//! The Groww native shadow CONNECTOR — composes the merged protocol
//! primitives into a live NATS-over-WebSocket client loop.
//!
//! Session lifecycle (mirrors the sidecar's two-tier recovery):
//!
//! 1. Read the ACCESS token from SSM (read-only — shared-minter lock).
//! 2. Generate a FRESH ed25519 session keypair; mint the socket token
//!    ([`super::socket_token`]) — one JWT per session.
//! 3. `connect_async(wss://socket-api.groww.in)` → wait `INFO` → send
//!    `CONNECT{jwt, sig}` → send `SUB` per watch-file subject.
//! 4. Read loop: accumulate WS payload bytes, drain
//!    [`crate::feed::groww::nats::parse_frame`], `PONG` on `PING`, decode
//!    `MSG` payloads ([`crate::feed::groww::proto`]), map subject → identity,
//!    `try_send` a [`ShadowTick`] to the writer.
//! 5. On ANY disconnect: bounded expo backoff and reconnect WITH THE SAME
//!    session JWT (fast path). On an AUTH-class failure (`-ERR Authorization
//!    …` / mint 401): fresh keypair + fresh mint, access token RE-READ from
//!    SSM at the ≥60s floor — NEVER minted.
//!
//! Hot-path discipline: the per-MSG path is `parse_frame` (zero-copy) →
//! `decode_socket_response` (zero-alloc) → one `HashMap` lookup (borrowed
//! `&str` key) → `try_send` of a `Copy` struct. No clone/format/Vec on the
//! per-message path; buffer compaction is one `copy_within` per WS frame
//! batch.

use std::collections::HashMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures_util::{SinkExt, StreamExt};
use secrecy::{ExposeSecret, SecretString};
use tickvault_common::constants::{
    GROWW_NATIVE_AUTH_RETRY_FLOOR_SECS, GROWW_NATIVE_RECONNECT_BASE_SECS,
    GROWW_NATIVE_RECONNECT_MAX_SECS, GROWW_SOCKET_URL,
};
use tickvault_common::error_code::ErrorCode;
use tokio::sync::mpsc;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;
use tracing::{error, info, warn};

use crate::auth::secret_manager::fetch_groww_access_token;
use crate::feed::groww::native::connect::{build_connect_frame, extract_nonce};
use crate::feed::groww::native::keypair::GrowwSessionKeypair;
use crate::feed::groww::native::shadow_writer::ShadowTick;
use crate::feed::groww::native::socket_token::{is_auth_status, mint_client, mint_socket_token};
use crate::feed::groww::nats::{self, NatsServerOp};
use crate::feed::groww::proto::{GrowwSocketMessage, decode_socket_response};
use crate::feed::groww::watch_reader::TickIdentity;

/// SUB lines batched per WebSocket message at subscribe time (cold path;
/// keeps any single WS frame comfortably small).
const SUBS_PER_WS_MESSAGE: usize = 200;

/// Hard cap on the accumulated framing buffer. `parse_frame` already rejects
/// any single payload > 1 MiB; this bounds pathological fragmentation across
/// frames so a hostile stream can never grow memory unboundedly.
const MAX_FRAMING_BUFFER_BYTES: usize = 4 * 1024 * 1024;

/// Bounded exponential reconnect delay: `BASE << attempt`, capped at MAX.
/// Pure + total (shift saturates via the cap; attempt 0 = first retry).
#[must_use]
pub fn reconnect_delay_secs(attempt: u32) -> u64 {
    let shifted = GROWW_NATIVE_RECONNECT_BASE_SECS.saturating_mul(1u64 << attempt.min(6));
    shifted.min(GROWW_NATIVE_RECONNECT_MAX_SECS)
}

/// True when a NATS `-ERR` message is AUTH-class (the session JWT / nkey was
/// rejected) — triggers the fresh-keypair + re-mint slow path. Pure.
#[must_use]
pub fn is_auth_err_message(err_msg: &str) -> bool {
    let lower = err_msg.to_ascii_lowercase();
    lower.contains("authorization") || lower.contains("authentication")
}

/// Why one connect-and-stream session ended. Drives the outer recovery loop.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionEnd {
    /// Transport-class end (socket closed / IO error / protocol violation) —
    /// reconnect with the SAME session JWT after backoff.
    Transport,
    /// Auth-class end (`-ERR authorization …`) — fresh keypair + fresh mint.
    AuthRejected,
    /// The writer channel closed (app shutting down) — exit the client.
    WriterGone,
}

/// UTC epoch nanos capture stamp (the native analogue of the sidecar's
/// per-callback `time.time_ns()`).
#[inline]
fn capture_ns_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_nanos()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

/// Run the native shadow client until the writer channel closes. Owns the
/// full mint → connect → stream → recover ladder. `subjects` is the
/// watch-file subject → identity map (built once per trading day by the
/// caller).
/// The pure primitives this composes (reconnect_delay_secs /
/// is_auth_err_message / parse_frame / decode_socket_response /
/// build_connect_frame / extract_nonce / assemble_shadow_tick) are
/// unit-tested in their modules.
// TEST-EXEMPT: live-network orchestration loop over unit-tested pure primitives.
pub async fn run_native_shadow_client(
    subjects: HashMap<String, TickIdentity>,
    tick_tx: mpsc::Sender<ShadowTick>,
) {
    let mut transport_attempt: u32 = 0;
    // The current session credentials (None = need a fresh mint). The signing
    // keypair travels WITH the JWT — a re-mint always re-keys (sidecar
    // rebuild semantics).
    let mut session: Option<SessionCreds> = None;

    loop {
        if tick_tx.is_closed() {
            info!("groww native shadow: writer gone before connect — exiting");
            return;
        }

        // ---- Session credentials (mint slow path) ----
        if session.is_none() {
            match mint_session_creds().await {
                Ok(creds) => {
                    session = Some(creds);
                }
                Err(auth_class) => {
                    let wait = if auth_class {
                        // Access token possibly stale — SSM re-read pacing
                        // floor (token-minter lock: re-read, NEVER mint).
                        GROWW_NATIVE_AUTH_RETRY_FLOOR_SECS
                    } else {
                        reconnect_delay_secs(transport_attempt)
                    };
                    transport_attempt = transport_attempt.saturating_add(1);
                    metrics::counter!("tv_groww_native_reconnects_total", "reason" => "mint_failed")
                        .increment(1);
                    tokio::time::sleep(Duration::from_secs(wait)).await;
                    continue;
                }
            }
        }

        // `session` is Some by construction here; fail-closed otherwise.
        let Some(creds) = session.as_ref() else {
            continue;
        };

        let (end, streamed) = connect_and_stream(creds, &subjects, &tick_tx).await;
        if streamed {
            // A session that completed its handshake resets the ladder.
            transport_attempt = 0;
        }
        match end {
            SessionEnd::WriterGone => {
                info!("groww native shadow: writer channel closed — exiting client");
                return;
            }
            SessionEnd::AuthRejected => {
                // Slow path: fresh keypair + fresh socket token next turn.
                session = None;
                metrics::counter!("tv_groww_native_reconnects_total", "reason" => "auth")
                    .increment(1);
                error!(
                    code = ErrorCode::GrowwNative02AuthFailed.code_str(),
                    "GROWW-NATIVE-02: Groww rejected the session (auth-class) — \
                     re-keying + re-minting the socket token"
                );
                tokio::time::sleep(Duration::from_secs(GROWW_NATIVE_AUTH_RETRY_FLOOR_SECS)).await;
            }
            SessionEnd::Transport => {
                let wait = reconnect_delay_secs(transport_attempt);
                transport_attempt = transport_attempt.saturating_add(1);
                metrics::counter!("tv_groww_native_reconnects_total", "reason" => "transport")
                    .increment(1);
                // Edge-friendly: first disconnect logs at error!, the storm
                // is visible via the counter + escalating attempt field.
                error!(
                    code = ErrorCode::GrowwNative01ConnectFailed.code_str(),
                    attempt = transport_attempt,
                    wait_secs = wait,
                    "GROWW-NATIVE-01: shadow client disconnected — reconnecting \
                     with backoff (shadow-only; sidecar capture unaffected)"
                );
                tokio::time::sleep(Duration::from_secs(wait)).await;
            }
        }
    }
}

/// The per-session credentials: the socket JWT + the ed25519 keypair that
/// signs each `INFO` nonce for it. Single-owner; replaced as a pair on
/// re-mint (a JWT is bound to its nkey, so they can never be mixed).
struct SessionCreds {
    jwt: SecretString,
    keypair: GrowwSessionKeypair,
}

/// Mint a fresh session (fresh keypair + socket-token REST call).
/// `Err(true)` = auth-class (stale access token — pace SSM re-reads);
/// `Err(false)` = transport-class (backoff + retry).
// TEST-EXEMPT: composes the unit-tested keypair + socket_token surfaces over
// live SSM + HTTPS; no test may hit the real endpoints.
async fn mint_session_creds() -> Result<SessionCreds, bool> {
    let access_token = match fetch_groww_access_token().await {
        Ok(token) => token,
        Err(e) => {
            error!(
                code = ErrorCode::GrowwNative02AuthFailed.code_str(),
                error = %e,
                "GROWW-NATIVE-02: access-token SSM read failed (transport/IAM \
                 class) — retrying with backoff; never minting"
            );
            return Err(false);
        }
    };
    let keypair = match GrowwSessionKeypair::generate() {
        Ok(kp) => kp,
        Err(e) => {
            error!(
                code = ErrorCode::GrowwNative02AuthFailed.code_str(),
                error = %e,
                "GROWW-NATIVE-02: session keypair generation failed"
            );
            return Err(false);
        }
    };
    let client = match mint_client() {
        Ok(c) => c,
        Err(e) => {
            error!(
                code = ErrorCode::GrowwNative02AuthFailed.code_str(),
                error = %e,
                "GROWW-NATIVE-02: socket-token HTTP client build failed"
            );
            return Err(false);
        }
    };
    match mint_socket_token(&client, &access_token, keypair.nkey_public()).await {
        Ok(token) => {
            info!(
                subscription_id_len = token.subscription_id.len(),
                "groww native shadow: socket token minted for a fresh session"
            );
            Ok(SessionCreds {
                jwt: token.jwt,
                keypair,
            })
        }
        Err(e) => {
            let auth = is_auth_status(&e);
            error!(
                code = ErrorCode::GrowwNative02AuthFailed.code_str(),
                error = %e,
                auth_class = auth,
                "GROWW-NATIVE-02: socket-token mint failed"
            );
            Err(auth)
        }
    }
}

/// One connect-and-stream session. Returns how it ended.
// TEST-EXEMPT: live-WebSocket orchestration; the framing/auth/decode arms are
// pure-function tested (nats.rs / connect.rs / proto.rs / assemble_shadow_tick).
async fn connect_and_stream(
    creds: &SessionCreds,
    subjects: &HashMap<String, TickIdentity>,
    tick_tx: &mpsc::Sender<ShadowTick>,
) -> (SessionEnd, bool) {
    let (ws, _resp) = match connect_async(GROWW_SOCKET_URL).await {
        Ok(pair) => pair,
        Err(e) => {
            warn!(error = %e, "groww native shadow: websocket connect failed");
            return (SessionEnd::Transport, false);
        }
    };
    let (mut sink, mut stream) = ws.split();

    let mut buf: Vec<u8> = Vec::with_capacity(64 * 1024);
    let mut handshaken = false;
    let mut dropped_full: u64 = 0;

    while let Some(frame) = stream.next().await {
        let payload: &[u8] = match &frame {
            Ok(Message::Binary(b)) => b.as_ref(),
            Ok(Message::Text(t)) => t.as_bytes(),
            Ok(Message::Ping(_) | Message::Pong(_)) => continue, // WS-level keepalive
            Ok(Message::Close(_)) => {
                warn!("groww native shadow: server closed the websocket");
                return (SessionEnd::Transport, handshaken);
            }
            Ok(Message::Frame(_)) => continue,
            Err(e) => {
                warn!(error = %e, "groww native shadow: websocket read error");
                return (SessionEnd::Transport, handshaken);
            }
        };
        if buf.len() + payload.len() > MAX_FRAMING_BUFFER_BYTES {
            error!(
                code = ErrorCode::GrowwNative03DecodeFailed.code_str(),
                buffered = buf.len(),
                "GROWW-NATIVE-03: framing buffer exceeded its cap — \
                 resetting the connection"
            );
            return (SessionEnd::Transport, handshaken);
        }
        buf.extend_from_slice(payload);

        // Drain complete NATS frames from the front of the buffer.
        let mut cursor = 0usize;
        loop {
            let (op, consumed) = match nats::parse_frame(&buf[cursor..]) {
                Ok(Some(pair)) => pair,
                Ok(None) => break, // incomplete — wait for more bytes
                Err(e) => {
                    error!(
                        code = ErrorCode::GrowwNative03DecodeFailed.code_str(),
                        error = %e,
                        "GROWW-NATIVE-03: NATS framing violation — \
                         resetting the connection"
                    );
                    metrics::counter!("tv_groww_native_decode_errors_total", "kind" => "framing")
                        .increment(1);
                    return (SessionEnd::Transport, handshaken);
                }
            };
            match op {
                NatsServerOp::Info(info_json) => {
                    let Some(nonce) = extract_nonce(info_json) else {
                        error!(
                            code = ErrorCode::GrowwNative02AuthFailed.code_str(),
                            "GROWW-NATIVE-02: server INFO carried no nonce — \
                             cannot sign; treating as auth-class"
                        );
                        return (SessionEnd::AuthRejected, handshaken);
                    };
                    let sig = creds.keypair.sign_nonce_b64url(nonce.as_bytes());
                    let connect_frame = build_connect_frame(&creds.jwt, &sig);
                    if sink
                        .send(Message::Text(
                            connect_frame.expose_secret().to_owned().into(),
                        ))
                        .await
                        .is_err()
                    {
                        return (SessionEnd::Transport, handshaken);
                    }
                    if !handshaken {
                        if send_subscriptions(&mut sink, subjects).await.is_err() {
                            return (SessionEnd::Transport, false);
                        }
                        handshaken = true;
                        info!(
                            subjects = subjects.len(),
                            "groww native shadow: CONNECT sent + subjects subscribed"
                        );
                    }
                }
                NatsServerOp::Ping => {
                    if sink
                        .send(Message::Text(nats::build_pong().into()))
                        .await
                        .is_err()
                    {
                        return (SessionEnd::Transport, handshaken);
                    }
                }
                NatsServerOp::Pong | NatsServerOp::Ok => {}
                NatsServerOp::Err(msg) => {
                    let auth = is_auth_err_message(msg);
                    error!(
                        code = ErrorCode::GrowwNative02AuthFailed.code_str(),
                        server_err = msg,
                        auth_class = auth,
                        "GROWW-NATIVE-02: server -ERR"
                    );
                    return if auth {
                        (SessionEnd::AuthRejected, handshaken)
                    } else {
                        (SessionEnd::Transport, handshaken)
                    };
                }
                NatsServerOp::Msg {
                    subject, payload, ..
                } => {
                    if let Some(identity) = subjects.get(subject) {
                        match decode_socket_response(payload) {
                            Ok(message) => {
                                if let Some(tick) =
                                    assemble_shadow_tick(*identity, &message, capture_ns_now())
                                {
                                    if tick_tx.try_send(tick).is_err() {
                                        if tick_tx.is_closed() {
                                            return (SessionEnd::WriterGone, handshaken);
                                        }
                                        dropped_full += 1;
                                        metrics::counter!(
                                            "tv_groww_native_dropped_total",
                                            "reason" => "writer_full"
                                        )
                                        .increment(1);
                                        // Coalesced visibility: first drop of
                                        // a burst logs; the counter carries
                                        // the magnitude.
                                        if dropped_full == 1 {
                                            error!(
                                                code =
                                                    ErrorCode::GrowwNative04WriterFailed.code_str(),
                                                "GROWW-NATIVE-04: shadow writer channel full — \
                                                 dropping shadow lines (production capture \
                                                 unaffected)"
                                            );
                                        }
                                    } else {
                                        dropped_full = 0;
                                    }
                                }
                            }
                            Err(e) => {
                                metrics::counter!(
                                    "tv_groww_native_decode_errors_total",
                                    "kind" => "proto"
                                )
                                .increment(1);
                                // Skip-and-continue: a malformed payload never
                                // kills the stream. Coalesce to debug to avoid
                                // per-tick log spam; the counter is the signal.
                                tracing::debug!(error = %e, subject, "groww native: proto decode skip");
                            }
                        }
                    } else {
                        metrics::counter!(
                            "tv_groww_native_decode_errors_total",
                            "kind" => "unknown_subject"
                        )
                        .increment(1);
                    }
                }
            }
            cursor += consumed;
            if cursor >= buf.len() {
                break;
            }
        }
        // Compact once per WS frame batch (single memmove).
        if cursor > 0 {
            buf.copy_within(cursor.., 0);
            buf.truncate(buf.len() - cursor);
        }
    }
    warn!("groww native shadow: websocket stream ended");
    (SessionEnd::Transport, handshaken)
}

/// Assemble the writer-bound tick from a decoded payload + subject identity.
/// Pure; `None` for payload kinds we don't capture (depth/info) or a
/// zero-value tick (Groww pads absent proto3 fields with 0 — a zero LTP tick
/// carries no information for the parity comparer).
#[must_use]
pub fn assemble_shadow_tick(
    identity: TickIdentity,
    message: &GrowwSocketMessage,
    capture_ns: i64,
) -> Option<ShadowTick> {
    let (ts_millis, ltp) = match message {
        GrowwSocketMessage::LivePrice(price) => (price.ts_in_millis, price.ltp),
        GrowwSocketMessage::Index(index) => (index.ts_in_millis, index.value),
        GrowwSocketMessage::Other => return None,
    };
    if !(ltp.is_finite() && ltp > 0.0 && ts_millis.is_finite()) || ts_millis <= 0.0 {
        return None;
    }
    #[allow(clippy::cast_possible_truncation)] // APPROVED: epoch-ms ≪ 2^53 — f64 is exact here
    let exchange_ts_millis = ts_millis as i64;
    Some(ShadowTick {
        security_id: identity.security_id,
        segment: identity.segment,
        exchange_ts_millis,
        ltp,
        capture_ns,
    })
}

/// Send `SUB` lines for every subject, batched into WS text messages.
// TEST-EXEMPT: thin sink-send loop over the unit-tested nats::build_sub.
async fn send_subscriptions<S>(
    sink: &mut S,
    subjects: &HashMap<String, TickIdentity>,
) -> Result<(), ()>
where
    S: SinkExt<Message> + Unpin,
{
    let mut batch = String::with_capacity(SUBS_PER_WS_MESSAGE * 40);
    for (sid, subject) in (1u64..).zip(subjects.keys()) {
        let sid_str = sid.to_string(); // cold path: subscribe-time only
        batch.push_str(&nats::build_sub(subject, &sid_str));
        if (sid as usize).is_multiple_of(SUBS_PER_WS_MESSAGE)
            && sink
                .send(Message::Text(std::mem::take(&mut batch).into()))
                .await
                .is_err()
        {
            return Err(());
        }
    }
    if !batch.is_empty() && sink.send(Message::Text(batch.into())).await.is_err() {
        return Err(());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::feed::groww::proto::{GrowwIndexValue, GrowwLivePrice};

    /// The reconnect ladder: 5, 10, 20, 40, 60 (cap), 60, … — bounded forever.
    #[test]
    fn test_reconnect_delay_secs_table() {
        assert_eq!(reconnect_delay_secs(0), 5);
        assert_eq!(reconnect_delay_secs(1), 10);
        assert_eq!(reconnect_delay_secs(2), 20);
        assert_eq!(reconnect_delay_secs(3), 40);
        assert_eq!(reconnect_delay_secs(4), 60);
        assert_eq!(reconnect_delay_secs(10), 60);
        assert_eq!(reconnect_delay_secs(u32::MAX), 60);
    }

    /// Auth-class classification of server `-ERR` bodies (NATS wire values).
    #[test]
    fn test_is_auth_err_message() {
        assert!(is_auth_err_message("Authorization Violation"));
        assert!(is_auth_err_message("authentication expired"));
        assert!(is_auth_err_message("User Authentication Failed"));
        assert!(!is_auth_err_message("Slow Consumer Detected"));
        assert!(!is_auth_err_message("Maximum Payload Violation"));
        assert!(!is_auth_err_message(""));
    }

    const IDENTITY: TickIdentity = TickIdentity {
        security_id: 2885,
        segment: "NSE_EQ",
    };

    /// LivePrice + Index payloads assemble; Other and zero/garbage values
    /// are filtered (no zero-LTP shadow lines).
    #[test]
    fn test_assemble_shadow_tick() {
        let price = GrowwSocketMessage::LivePrice(GrowwLivePrice {
            ts_in_millis: 1_783_052_553_456.0,
            ltp: 2954.5,
            ..Default::default()
        });
        let tick = assemble_shadow_tick(IDENTITY, &price, 42).expect("price assembles");
        assert_eq!(tick.security_id, 2885);
        assert_eq!(tick.segment, "NSE_EQ");
        assert_eq!(tick.exchange_ts_millis, 1_783_052_553_456);
        assert!((tick.ltp - 2954.5).abs() < f64::EPSILON);
        assert_eq!(tick.capture_ns, 42);

        let index = GrowwSocketMessage::Index(GrowwIndexValue {
            ts_in_millis: 1_783_052_553_000.0,
            value: 25_432.15,
        });
        let tick = assemble_shadow_tick(IDENTITY, &index, 1).expect("index assembles");
        assert!((tick.ltp - 25_432.15).abs() < f64::EPSILON);

        assert_eq!(
            assemble_shadow_tick(IDENTITY, &GrowwSocketMessage::Other, 1),
            None
        );
    }

    /// proto3 zero-defaults and garbage never produce a shadow line.
    #[test]
    fn test_assemble_shadow_tick_filters_zero_and_garbage() {
        let zero = GrowwSocketMessage::LivePrice(GrowwLivePrice::default());
        assert_eq!(assemble_shadow_tick(IDENTITY, &zero, 1), None);

        let nan = GrowwSocketMessage::LivePrice(GrowwLivePrice {
            ts_in_millis: 1_783_052_553_456.0,
            ltp: f64::NAN,
            ..Default::default()
        });
        assert_eq!(assemble_shadow_tick(IDENTITY, &nan, 1), None);

        let negative_ts = GrowwSocketMessage::LivePrice(GrowwLivePrice {
            ts_in_millis: -5.0,
            ltp: 100.0,
            ..Default::default()
        });
        assert_eq!(assemble_shadow_tick(IDENTITY, &negative_ts, 1), None);
    }
}
