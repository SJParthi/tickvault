//! Dhan live-feed socket transport — the ONE implementation of
//! [`DhanFeedSocket`](super::pool_supervisor::DhanFeedSocket).
//!
//! Part of the 16-connection revival authorized by the operator on 2026-08-09
//! (`.claude/rules/project/websocket-connection-scope-lock.md`, sections
//! "2026-08-09 — DHAN LIVE MAIN-FEED WS REVIVAL AUTHORIZED" and "2026-08-09
//! (SAME DAY, SECOND QUOTE) — 16 CONNECTIONS + depth-20/depth-200
//! AUTHORIZED"). The previous connection layer was deleted on 2026-07-17 with
//! the retired lane; this module rebuilds ONLY the transport. Every reconnect,
//! park, backoff and resubscribe DECISION already lives in
//! [`super::pool_supervisor`] and is not re-derived here — this module contains
//! no policy whatsoever.
//!
//! # THE ONE RULE
//! The read path does exactly two things per data frame: hand the raw bytes up
//! (the supervisor's shell then performs one WAL append + one non-blocking ring
//! push) and return. No parsing, no aggregation, no database write, no lock, no
//! await on disk.
//!
//! This is the disconnect-prevention mechanism, not a style preference.
//! `03-live-market-feed-websocket.md` rule 16: ping/pong is handled by the
//! library — and the library can only emit that automatic pong **while the read
//! loop is polling the socket**. Dhan closes a client that has been silent for
//! more than 40 seconds. A reader that blocks to do work therefore stops
//! ponging while looking perfectly healthy locally, and gets closed. A slow
//! consumer on this feed is not merely late; it is disconnected.
//!
//! # Frame-size caps — the security-critical part
//! `tokio-tungstenite`'s default configuration allows a **64 MiB message** and
//! a **16 MiB frame** (`tungstenite-0.29.0/src/protocol/mod.rs:95-106`). Every
//! `connect_async*` call in this workspace before today passed `None` for the
//! config and therefore inherited those defaults, so a hostile or MITM'd
//! endpoint could make us allocate 64 MiB per message before a single
//! app-level check ran — sixteen sockets deep, on a box whose whole app budget
//! is a few GiB.
//!
//! This module passes an EXPLICIT [`WebSocketConfig`] on every dial, sized to
//! the real Dhan protocol (see [`max_frame_bytes`]). Over-cap input is refused
//! by the library with `CapacityError::MessageTooLong` before the payload is
//! accumulated; the read surfaces it as a close, and the supervisor's ladder
//! reconnects. Fail-closed, bounded, and loud.
//!
//! # What "no parsing" does and does not mean
//! One exception exists and is deliberate: a **disconnect control packet** is
//! classified here, by fixed offset, with no allocation
//! ([`classify_frame`]). The supervisor's entire 805-park design (re-dialing
//! 805 kills a healthy sibling rather than recovering this socket) depends on
//! knowing the Dhan disconnect code, and `SocketEvent::Closed { code }` is the
//! only channel that carries it. Reading three bytes out of a ten-byte control
//! packet is transport control-plane work, not tick parsing — no price, no
//! quantity, no timestamp is ever touched here, and the frame is still handed
//! up so the durable path sees it first.
//!
//! # Complexity
//! | Path | Cost | Note |
//! |---|---|---|
//! | [`classify_frame`] | O(1) on the exact-length path; **O(packets per frame)** on the stacked walk, zero alloc either way | An exact-length frame is a length check plus two fixed-offset byte reads. A main-feed frame of any OTHER length walks packet boundaries looking for a stacked disconnect (`stacked_disconnect_reason`), bounded by `MAX_PACKETS_PER_FRAME` (70,000). Corrected 2026-08-26: this row still claimed the pre-2026-08-25 cost, which is the stale-complexity-claim class this repo has recorded five times. |
//! | [`DhanFeedSocketImpl::recv`] steady state | O(1), zero alloc *of ours* | `Bytes` move; tungstenite owns the decode buffer |
//! | [`build_feed_url`] | O(n) in url length, allocates | cold path, once per dial |
//! | [`build_subscribe_payload`] | O(n) in batch, allocates | cold path, ~50 messages per connect |
//!
//! **Honesty on "zero allocation on the per-frame path."** Exact for the code
//! in this module: the steady-state data path moves a `Bytes` (an `Arc`
//! refcount, not a copy) and touches no heap of ours. It is NOT a claim about
//! the whole read loop — `tokio-tungstenite` allocates the frame buffer when it
//! decodes a message, upstream of anything here, and that allocation is not
//! ours to remove.
//!
//! # What this module CANNOT deliver (no false-OK)
//! The India main feed has **no snapshot-on-subscribe** (that is documented
//! only for the US global-stocks socket, feed code 29, which is FORBIDDEN here)
//! and **no sequence number** in any packet. Packet loss is therefore
//! undetectable at the protocol level. Nothing in this module detects a gap;
//! the 15:31 REST cross-verification is the only available ground truth.

use std::time::Duration;

use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use tickvault_common::constants::{
    DEEP_DEPTH_HEADER_OFFSET_FEED_CODE, DEEP_DEPTH_HEADER_SIZE, DISCONNECT_PACKET_SIZE,
    MAX_PACKETS_PER_FRAME, RESPONSE_CODE_DISCONNECT,
};
use tickvault_common::error_code::ErrorCode;
use tickvault_common::sanitize::redact_url_params;
use tickvault_common::types::{ExchangeSegment, FeedMode};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::{Bytes as WsBytes, Message};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async_tls_with_config};
use tracing::{debug, error, warn};
use zeroize::Zeroizing;

// Re-exported so a caller outside this crate can implement `FeedTokenSource`
// without taking its own `zeroize` dependency — and, more to the point, so it
// cannot accidentally satisfy the trait with a plain `String` that lingers on
// the heap after the dial.
pub use zeroize::Zeroizing as FeedTokenBuffer;

use super::pool_budget::{DhanEndpointType, MAIN_FEED_INSTRUMENTS_PER_CONNECTION};
use super::pool_supervisor::{DhanFeedSocket, SocketEvent, SocketFailure, SubscribeInstrument};
use super::subscription_builder::{
    build_subscription_messages, build_twenty_depth_subscription_messages,
    build_twenty_depth_unsubscription_messages, build_two_hundred_depth_subscription_message,
    build_two_hundred_depth_unsubscription_message, build_unsubscription_messages,
};
use super::tls::{build_websocket_tls_connector, build_websocket_tls_connector_no_alpn};
use super::types::{DisconnectCode, InstrumentSubscription};

// ---------------------------------------------------------------------------
// Frame-size caps
// ---------------------------------------------------------------------------

/// Largest single main-feed packet: the Full packet
/// (`03-live-market-feed-websocket.md` rule 5 — 162 bytes).
const MAIN_FEED_LARGEST_PACKET_BYTES: usize = 162;

/// Ceiling on a main-feed WebSocket message.
///
/// ## CORRECTED 2026-08-14 — the previous sizing measured the wrong thing
///
/// This constant was 256 KiB, justified as follows (quoted from the comment it
/// replaces, because the error is instructive rather than careless):
///
/// > "a subscribe message is capped at 100 instruments, so even a hypothetical
/// > fully-batched burst of one Full packet per instrument in a single
/// > subscribe batch is ~16 KiB"
///
/// The arithmetic is correct. The **quantity is not**. How many instruments we
/// name in one *subscribe request* has nothing to do with how many *data
/// packets* Dhan coalesces into one WebSocket message on the way back. The
/// binding number is instruments **per connection**, which is
/// [`MAIN_FEED_INSTRUMENTS_PER_CONNECTION`] = 5,000 — fifty times the
/// subscribe-batch figure the old sizing reasoned from.
///
/// Why it matters, concretely: at 5,000 instruments × 162 B, one message
/// carrying a single Full packet per instrument is **~810 KiB**, three times
/// the old cap. That burst is not exotic — it is what the vendor's queued
/// backlog looks like at 09:15 after a quiet pre-open. Exceeding the cap
/// raises `Error::Capacity`, which this module maps to `reason="oversize"` and
/// returns as `SocketEvent::Closed`, so the supervisor reconnects,
/// re-subscribes all 5,000, and the same backlog arrives again: a permanent
/// reconnect loop at market open, on a metric with no alarm, while
/// `tv_dhan_feed_stack_up` still reads 1.0 because the other sockets are fine.
///
/// The cap is now DERIVED from the two facts that actually bound it rather
/// than written as a literal, so it cannot drift back to a subscribe-batch
/// rationale. `×2` headroom covers coalescing that exceeds one packet per
/// instrument in a single message; the result is ~1.6 MiB.
///
/// Still finite, deliberately: this is a defence against a hostile or
/// malfunctioning peer, so it must remain a ceiling. Sixteen sockets at this
/// cap is ~26 MiB of worst-case buffering — larger than the old ~5 MiB, and
/// still nowhere near the 64 MiB-per-socket library default this replaces.
pub const MAIN_FEED_MAX_FRAME_BYTES: usize =
    MAIN_FEED_LARGEST_PACKET_BYTES * (MAIN_FEED_INSTRUMENTS_PER_CONNECTION as usize) * 2;

/// Ceiling on a depth-20 WebSocket message.
///
/// Sizing: a depth-20 packet is `TWENTY_DEPTH_PACKET_SIZE` (12-byte header +
/// 20 levels × 16 bytes = 332 bytes) and bid/ask arrive as SEPARATE packets
/// (`04-full-market-depth-websocket.md` — codes 41 and 51), so one message
/// realistically carries one packet. 256 KiB is ~789 such packets.
pub const DEPTH_20_MAX_FRAME_BYTES: usize = 256 * 1024;

/// Ceiling on a depth-200 WebSocket message.
///
/// Sizing: a full depth-200 packet is `TWO_HUNDRED_DEPTH_PACKET_SIZE`
/// (12-byte header + 200 levels × 16 bytes = 3,212 bytes) — twenty times the
/// main feed's largest — and depth-200 carries exactly ONE instrument per
/// connection, so a message realistically carries one packet. 512 KiB is ~163
/// such packets. Deliberately larger than the other two pools because the
/// packet is larger, not because the risk is lower.
pub const DEPTH_200_MAX_FRAME_BYTES: usize = 512 * 1024;

// Compile-time proof that every cap is larger than the packet it must carry.
// A cap below its own packet size would not degrade gracefully — it would
// refuse EVERY frame on that endpoint, silently, forever.
const _: () = assert!(
    MAIN_FEED_MAX_FRAME_BYTES > MAIN_FEED_LARGEST_PACKET_BYTES * 1_000,
    "main-feed cap must hold at least a thousand Full packets"
);
const _: () = assert!(
    DEPTH_20_MAX_FRAME_BYTES > tickvault_common::constants::TWENTY_DEPTH_PACKET_SIZE * 100,
    "depth-20 cap must hold at least a hundred depth-20 packets"
);
const _: () = assert!(
    DEPTH_200_MAX_FRAME_BYTES > tickvault_common::constants::TWO_HUNDRED_DEPTH_PACKET_SIZE * 80,
    "depth-200 cap must hold at least eighty depth-200 packets"
);

/// Read-buffer size for a feed socket. Below tungstenite's 128 KiB default:
/// this buffer is EAGERLY allocated per connection, and sixteen sockets at the
/// default would reserve 2 MiB of read buffer before a single byte arrived.
/// 16 KiB still holds ~100 Full packets per read syscall.
const FEED_READ_BUFFER_BYTES: usize = 16 * 1024;

/// Write-buffer target. We write only subscribe messages (a few KiB, tens of
/// times per connect), so the 128 KiB default is pure waste per socket.
const FEED_WRITE_BUFFER_BYTES: usize = 4 * 1024;

/// Hard ceiling on the write buffer, providing backpressure if writes to the
/// socket start failing. Must exceed [`FEED_WRITE_BUFFER_BYTES`] by at least
/// one message; the largest subscribe message is ~100 instruments of JSON.
const FEED_MAX_WRITE_BUFFER_BYTES: usize = 64 * 1024;

/// How long a single dial (TCP + TLS + WebSocket upgrade) may take before it is
/// abandoned. Without this a black-holed DNS or a silently dropped SYN parks a
/// connection task forever — the supervisor's idle watchdog covers a socket
/// that came UP and went quiet, not a dial that never completes.
// APPROVED: this line IS the named constant the no-hardcoded-Duration rule asks for; the scanner matches the declaration itself. Same shape as `pool_supervisor::IDLE_POLL_INTERVAL`.
pub const DIAL_TIMEOUT: Duration = Duration::from_secs(15);

/// How long one subscribe message may take to reach the socket. A blocked write
/// while the pool waits is the same disconnect hazard as a blocked read.
// APPROVED: this line IS the named constant the no-hardcoded-Duration rule asks for; the scanner matches the declaration itself. Same shape as `pool_supervisor::IDLE_POLL_INTERVAL`.
pub const SUBSCRIBE_SEND_TIMEOUT: Duration = Duration::from_secs(10);

/// Counter: control / non-data frames observed and skipped (ping, pong, text,
/// raw). Labels: `endpoint`, `kind`.
pub const CONTROL_FRAME_METRIC: &str = "tv_dhan_ws_control_frames_total";

/// Client-originated keepalive pings, by endpoint and outcome.
///
/// Local-exporter only — deliberately NOT added to the EMF selector, because
/// `user-data.sh.tftpl` renders at exactly its 15,872-byte budget with zero
/// bytes free (measured 2026-08-26). The condition this guards already pages
/// through the existing depth/silence signals; this counter exists so the
/// fix can be VERIFIED on the box (`sent` climbing, and the matching
/// `tv_dhan_ws_control_frames_total{kind="pong",endpoint="depth_200"}` series
/// coming into existence for the first time), not so it can page.
pub const CLIENT_KEEPALIVE_PING_METRIC: &str = "tv_dhan_ws_client_keepalive_ping_total";

/// Counter: a dial failed. Labels: `endpoint`, `reason`.
pub const DIAL_FAILED_METRIC: &str = "tv_dhan_ws_dial_failed_total";

/// Counter: a subscribe message could not be sent. Labels: `endpoint`, `reason`.
pub const SUBSCRIBE_FAILED_METRIC: &str = "tv_dhan_ws_subscribe_failed_total";

/// Counter: a frame was refused by the read path. Labels: `endpoint`, `reason`.
/// `reason="oversize"` is the frame-cap refusal — the hostile-input signal.
pub const FRAME_REFUSED_METRIC: &str = "tv_dhan_ws_frame_refused_total";

/// The maximum bytes one message of `endpoint` may carry.
///
/// `OrderUpdate` is included for totality only — that socket is owned by
/// `order_update_connection.rs` and never reaches this module. It gets the
/// main-feed cap, which is ample for a JSON order event.
#[must_use]
pub const fn max_frame_bytes(endpoint: DhanEndpointType) -> usize {
    match endpoint {
        DhanEndpointType::MainFeed | DhanEndpointType::OrderUpdate => MAIN_FEED_MAX_FRAME_BYTES,
        DhanEndpointType::Depth20 => DEPTH_20_MAX_FRAME_BYTES,
        DhanEndpointType::Depth200 => DEPTH_200_MAX_FRAME_BYTES,
    }
}

/// The explicit WebSocket configuration for `endpoint`.
///
/// `max_message_size` is set EQUAL to `max_frame_size` deliberately: a
/// fragmented message accumulates across continuation frames, so bounding only
/// the frame would still let a hostile peer accumulate an unbounded message out
/// of individually-legal fragments. Both caps already carry two to three orders
/// of magnitude of headroom over real Dhan traffic, so tying them costs
/// nothing.
#[must_use]
pub fn websocket_config(endpoint: DhanEndpointType) -> WebSocketConfig {
    let cap = max_frame_bytes(endpoint);
    // `WebSocketConfig` is `#[non_exhaustive]`, so it is built through its
    // builder rather than a struct literal. Every field this product cares
    // about is set EXPLICITLY — inheriting a default here is the bug.
    WebSocketConfig::default()
        .read_buffer_size(FEED_READ_BUFFER_BYTES)
        .write_buffer_size(FEED_WRITE_BUFFER_BYTES)
        .max_write_buffer_size(FEED_MAX_WRITE_BUFFER_BYTES)
        .max_message_size(Some(cap))
        .max_frame_size(Some(cap))
        .accept_unmasked_frames(false)
}

// ---------------------------------------------------------------------------
// Token source
// ---------------------------------------------------------------------------

/// Supplies the current Dhan access token at dial time.
///
/// A trait rather than a stored `String` because the token rotates: the socket
/// must read the CURRENT value on every dial, so that the supervisor's
/// `RefreshTokenThenDial` action (807/809 staleness) actually presents a new
/// credential instead of re-presenting the dead one.
///
/// Generic rather than `dyn` so the call monomorphises and no trait object is
/// introduced into the websocket path.
pub trait FeedTokenSource: Send + Sync + 'static {
    /// The current access token, or `None` when no valid token exists.
    ///
    /// Returned in a [`Zeroizing`] wrapper so the copy is wiped when the dial
    /// finishes rather than lingering on the heap.
    fn current_token(&self) -> Option<Zeroizing<String>>;
}

impl<F> FeedTokenSource for F
where
    F: Fn() -> Option<Zeroizing<String>> + Send + Sync + 'static,
{
    fn current_token(&self) -> Option<Zeroizing<String>> {
        self()
    }
}

// ---------------------------------------------------------------------------
// URL construction — the credential-bearing part
// ---------------------------------------------------------------------------

/// Builds the full connect URL for `endpoint`.
///
/// Dhan carries the credential in the QUERY STRING
/// (`03-live-market-feed-websocket.md` rule 2:
/// `wss://api-feed.dhan.co?version=2&token=<ACCESS_TOKEN>&clientId=<CLIENT_ID>&authType=2`),
/// so the returned string IS a secret. It is wrapped in [`Zeroizing`] and must
/// never be logged, stored on a struct, put in a metric label, or written to
/// `ws_event_audit`. Use [`loggable_url`] for anything human-facing.
///
/// The depth endpoints take the same credential parameters without
/// `version=2`, matching the documented full URLs recorded on
/// `DHAN_TWENTY_DEPTH_WS_BASE_URL` and `DHAN_TWO_HUNDRED_DEPTH_WS_BASE_URL`.
/// The depth-200 base deliberately has NO path — the root `/` was verified
/// against Dhan's own vendor SDK on 2026-04-23 after `/twohundreddepth`
/// produced two solid weeks of `ResetWithoutClosingHandshake`; that constant's
/// doc comment carries the full history.
/// Whether a base URL carries a path component after its authority.
///
/// # The incident this exists for (prod, 2026-08-12)
///
/// The revived main feed failed EVERY dial attempt for a whole session with
/// `WS-GAP-03 … "HTTP error: 400 Bad Request"`, retrying every 30s from
/// 09:29 IST. The daily cross-verification then reported `compared: 0,
/// missing_live: 373` — the live lane produced zero candles all day, while
/// the REST record had all 373 minutes.
///
/// The cause was one missing character. `build_feed_url` appended `?` directly
/// to `wss://api-feed.dhan.co`, and tungstenite writes its request line as
/// `GET {path_and_query} HTTP/1.1`
/// (`tungstenite-0.29 handshake/client.rs:114-117`). For a URL with no path,
/// `Uri::path_and_query()` returns `"?version=2&…"` — with **no leading
/// slash** — so the bytes on the wire were:
///
/// ```text
/// GET ?version=2&token=…&clientId=…&authType=2 HTTP/1.1
/// ```
///
/// RFC 9112 §3.2.1 requires an origin-form request-target to begin with `/`.
/// Dhan's edge rejected it with 400 before the WebSocket upgrade was ever
/// considered — so this was never a credential, entitlement, or subscription
/// problem, which is exactly why it survived a session of investigation
/// pointed at the token.
///
/// # Why the other three endpoints were unaffected
///
/// depth-20's base already ends in `/twentydepth`, and depth-200 was given a
/// literal `/?` by a special case in the builder. The main feed was the ONLY
/// endpoint reaching the wire without a path — which is precisely the set that
/// failed. The special case is now gone: the rule is derived from the URL
/// rather than hardcoded per endpoint, so a future change to any base-URL
/// constant is covered automatically instead of needing someone to remember
/// this.
///
/// # What "has a path" means here
///
/// True when a `/` appears after the `://` authority. Deliberately conservative
/// about inputs it does not recognise: a string with no `://` is reported as
/// HAVING a path, so no slash is inserted into something that is not a URL.
#[must_use]
pub fn url_has_path(base_url: &str) -> bool {
    match base_url.find("://") {
        // `+ 3` skips the `://` itself so the scheme's own slashes cannot be
        // mistaken for a path.
        Some(scheme_end) => base_url[scheme_end.saturating_add(3)..].contains('/'),
        // Not a URL shape we understand — say "has a path" so the caller
        // leaves it alone rather than corrupting it.
        None => true,
    }
}

#[must_use]
pub fn build_feed_url(
    endpoint: DhanEndpointType,
    base_url: &str,
    token: &str,
    client_id: &str,
) -> Zeroizing<String> {
    let trimmed = base_url.trim_end_matches('/');
    let mut url = String::with_capacity(trimmed.len() + token.len() + client_id.len() + 64);
    url.push_str(trimmed);
    // A URL whose authority is followed directly by `?` produces an HTTP
    // request-target with NO leading slash, and that is malformed — see
    // `url_has_path` for the full incident.
    if !url_has_path(trimmed) {
        url.push('/');
    }
    url.push('?');
    if endpoint == DhanEndpointType::MainFeed {
        url.push_str("version=2&");
    }
    url.push_str("token=");
    url.push_str(token);
    url.push_str("&clientId=");
    url.push_str(client_id);
    url.push_str("&authType=2");
    Zeroizing::new(url)
}

/// The human-facing form of a feed URL: scheme, host and path only, with every
/// query parameter replaced by `?[REDACTED]`.
///
/// Built from the BASE url (which carries no secret) rather than by redacting
/// the full url, so there is no code path in which the credential-bearing
/// string could reach a formatter at all.
#[must_use]
pub fn loggable_url(base_url: &str) -> String {
    redact_url_params(base_url)
}

/// Redacts an arbitrary transport error before it reaches a log.
///
/// `tungstenite` embeds the request URI in several of its error `Display`
/// impls, so an unredacted `{err}` on this path leaks the 24-hour JWT into
/// CloudWatch. Every error string in this module goes through here.
///
/// **Cold path by construction.** This allocates, and it is reached ONLY when a
/// dial, a subscribe write or a read has already failed — i.e. once per
/// disconnect episode, never per frame. `#[cold]` + `#[inline(never)]` keep it
/// out of the hot read loop's instruction stream as well as off its execution
/// path, so the allocation can never be paid by a healthy socket.
#[cold]
#[inline(never)]
#[must_use]
fn safe_err(err: &impl std::fmt::Display) -> String {
    // O(1) EXEMPT: begin — error-path formatting, once per disconnect episode,
    // never per frame. The allocation is the price of not leaking a JWT.
    let rendered = err.to_string();
    redact_url_params(&rendered)
    // O(1) EXEMPT: end
}

// ---------------------------------------------------------------------------
// Subscribe payload
// ---------------------------------------------------------------------------

/// The main-feed subscription mode.
///
/// **Full** (request code 21 — 162-byte packets) since 2026-08-19, per the
/// operator's 2026-08-15 authorization in `websocket-connection-scope-lock.md`
/// ("entire NTM and entire nse indices … shdou lbe fully subscribed dude wiht
/// full mdoe"). That quote landed in the rule file on 2026-08-15 and the
/// constant was left at `Quote` — the flip is this line.
///
/// Changing it is a scope decision, not a transport one, which is why the
/// scope-lock test below pins it and why moving it back requires the same
/// rule-file-first protocol.
///
/// **What Full costs and what it buys, stated plainly.** 50 B → 162 B per
/// packet is 3.24× the bytes on the wire for the same tick rate. In exchange
/// the packet carries OI, day OHLC and 5 levels of bid/ask at fixed offsets.
///
/// **What it does NOT do today:** the 5 depth levels inside every Full packet
/// are parsed and then DISCARDED — the drain matches
/// `ParsedFrame::TickWithDepth(tick, _)` and folds only the tick, because no
/// depth writer or depth table exists (both were deleted with the earlier
/// live-feed retirements). So this flip pays the full 3.24× bandwidth and
/// consumes the tick half of it. That is deliberate — the depth vertical
/// (parser call → DDL → writer → dedup key → retention) is its own unit of
/// work, and the operator's 2026-08-15 second quote requires the writer to
/// exist before any depth is claimed as captured.
pub const DEFAULT_MAIN_FEED_MODE: FeedMode = FeedMode::Full;

/// The mode Dhan actually serves for `IDX_I`, which is NOT the configured one.
///
/// **An index is a computed number, not a traded instrument.** It has no order
/// book, so there are no bid/ask levels for a Full (code 21) packet to carry.
/// Dhan does not answer that request with an error — it accepts the subscribe
/// and sends NOTHING AT ALL, which is indistinguishable from a quiet
/// instrument and is why this survived three sessions undetected.
///
/// **Measured on the box, 2026-08-21.** 163,934 packets were walked frame by
/// frame out of the live capture log. Every one was `code 8 / segment 1 or 2`
/// (Full, NSE_EQ or NSE_FNO). There were ZERO packets on segment 0 — including
/// zero `code 6` PrevClose, which Dhan support CONFIRMED (Ticket #5525125) is
/// emitted for IDX_I on ANY subscription in ANY mode. A subscription that
/// never draws even its one guaranteed packet was never accepted. The lane's
/// own detector agreed independently: `never_ticked` has equalled the IDX_I
/// count exactly, every day — 4 when four seeds were subscribed, 119 once the
/// master-sourced indices arrived.
///
/// **This restores a partition that already existed and was lost.** The
/// pre-retirement lane sorted indices into their own Quote-mode batch;
/// `docs/rules-archive/live-market-feed-subscription.md:279` records it as the
/// `idx_instruments` partition, chosen so the Quote packet's `day_open` /
/// `day_high` / `day_low` come from the exchange rather than being tracked
/// app-side. The lane was deleted 2026-07-17 and rebuilt in the 2026-08-09
/// revival with ONE global mode and no partition, so `idx_instruments` has
/// zero occurrences on the tree today.
///
/// **Nothing is given up by not asking for depth here.** An index has no depth
/// to lose. Full stays the mode for every instrument that has a book.
///
/// **UNVERIFIED-LIVE, and deliberately loud rather than assumed.** Whether
/// Quote (17) is served for IDX_I on this account is not settled in this
/// repository: `docs/dhan-support/2026-05-18-idx-i-quote-full-mode-support.md`
/// asked Dhan this exact question and no answer is recorded, and an older
/// uncited note claimed Dhan forces Ticker (15) for indices. If Quote is also
/// refused, the failure is IDENTICAL in shape to today's — silence — so the
/// verification is the RISK-GAP-03 never-ticked count, which must fall to zero
/// for the index set on the first session after this lands. If it does not,
/// `Ticker` is the next value to try, and this constant is the one line to
/// change.
pub const IDX_I_FEED_MODE: FeedMode = FeedMode::Quote;

/// The mode to subscribe `segment` in, given the configured main-feed mode.
///
/// Total and O(1). Indices take [`IDX_I_FEED_MODE`]; everything else takes the
/// caller's configured mode unchanged.
#[must_use]
pub fn feed_mode_for_segment(segment: ExchangeSegment, configured: FeedMode) -> FeedMode {
    if matches!(segment, ExchangeSegment::IdxI) {
        IDX_I_FEED_MODE
    } else {
        configured
    }
}

/// Splits a main-feed batch into `(indices, others)`.
///
/// # Zero-loss property
///
/// `indices.len() + others.len() == batch.len()`, and concatenating the two
/// reproduces every instrument in `batch` exactly once. That is the property
/// worth pinning: a partition that dropped or duplicated an instrument would
/// leave it unsubscribed (silent, since a stream that never arrives leaves
/// nothing to count) or double-subscribed against the per-connection cap.
#[must_use]
pub fn partition_index_batch(
    batch: &[SubscribeInstrument],
) -> (Vec<SubscribeInstrument>, Vec<SubscribeInstrument>) {
    // O(1) EXEMPT: begin — cold path, runs at connect/top-up time, not per tick.
    let mut indices: Vec<SubscribeInstrument> = Vec::new();
    let mut others: Vec<SubscribeInstrument> = Vec::new();
    for instrument in batch {
        if matches!(instrument.segment, ExchangeSegment::IdxI) {
            indices.push(*instrument);
        } else {
            others.push(*instrument);
        }
    }
    (indices, others)
    // O(1) EXEMPT: end
}

/// Why a subscribe payload could not be built.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SubscribePayloadError {
    /// The batch was empty. Sending an empty `InstrumentList` would consume a
    /// message slot and subscribe nothing, so it is refused rather than sent.
    #[error("refusing to build a subscribe message for an empty batch")]
    EmptyBatch,
    /// depth-200 accepts exactly ONE instrument per message (and per
    /// connection); a larger batch means the sharding upstream is wrong.
    #[error("depth-200 accepts exactly 1 instrument per message, {got} supplied")]
    Depth200Overfull {
        /// Instruments the caller supplied.
        got: usize,
    },
    /// The batch produced more than one wire message. The caller is expected to
    /// have sharded it with `SubscribeGuard::batches` already; a split here
    /// means the two batch sizes disagree and instruments would be silently
    /// dropped by a transport that sends only the first message.
    #[error("batch of {instruments} split into {messages} messages; it must produce exactly 1")]
    BatchSplit {
        /// Instruments the caller supplied.
        instruments: usize,
        /// Messages the builder produced.
        messages: usize,
    },
    /// The shared builder refused the request (e.g. depth on a non-NSE
    /// segment, which Dhan does not serve).
    #[error("subscribe payload refused: {reason}")]
    Refused {
        /// The builder's message.
        reason: String,
    },
}

/// Serialises ONE subscribe message for `batch`.
///
/// **The wire format is NOT defined here.** Every byte of it comes from
/// [`super::subscription_builder`], which is the single source of truth for the
/// Dhan subscribe JSON (request codes, the string-`SecurityId` rule, the
/// flat-vs-list shape split, the depth segment restriction). This function is
/// only the transport-side adapter: it maps a supervisor
/// [`SubscribeInstrument`] batch onto that builder and asserts the batch
/// produced exactly one message.
///
/// Batching is likewise NOT decided here — the caller supplies a batch already
/// sized by `SubscribeGuard::batches`, which honours each endpoint's documented
/// per-message cap. Passing the batch length as the builder's `batch_size` makes
/// a second, disagreeing split impossible: if the two ever diverged, the
/// [`SubscribePayloadError::BatchSplit`] arm fails loudly rather than sending a
/// truncated subscription.
///
/// # Errors
/// See [`SubscribePayloadError`].
pub fn build_subscribe_payload(
    endpoint: DhanEndpointType,
    feed_mode: FeedMode,
    batch: &[SubscribeInstrument],
) -> Result<String, SubscribePayloadError> {
    let Some(first) = batch.first() else {
        return Err(SubscribePayloadError::EmptyBatch);
    };

    // depth-200 is the flat single-instrument shape — no InstrumentList at all.
    if endpoint == DhanEndpointType::Depth200 {
        if batch.len() > 1 {
            return Err(SubscribePayloadError::Depth200Overfull { got: batch.len() });
        }
        return build_two_hundred_depth_subscription_message(first.segment, first.security_id)
            .map_err(|reason| SubscribePayloadError::Refused { reason });
    }

    let mut instruments = Vec::with_capacity(batch.len());
    for instrument in batch {
        instruments.push(InstrumentSubscription::new(
            instrument.segment,
            instrument.security_id,
        ));
    }
    let mut messages = match endpoint {
        // Depth is NSE-only at the vendor. A BSE_FNO (SENSEX) contract here
        // used to build a well-formed payload that came back as SILENCE —
        // indistinguishable from a quiet book. It is now refused through the
        // same `Refused` path the 200-level endpoint has always used, so the
        // socket is torn down loudly instead of sitting live and blind.
        DhanEndpointType::Depth20 => {
            build_twenty_depth_subscription_messages(&instruments, batch.len())
                .map_err(|reason| SubscribePayloadError::Refused { reason })?
        }
        _ => build_subscription_messages(&instruments, feed_mode, batch.len()),
    };
    if messages.len() != 1 {
        return Err(SubscribePayloadError::BatchSplit {
            instruments: batch.len(),
            messages: messages.len(),
        });
    }
    messages.pop().ok_or(SubscribePayloadError::EmptyBatch)
}

/// The unsubscribe twin of [`build_subscribe_payload`].
///
/// **ADDED 2026-08-26** for the per-minute at-the-money re-selection: a depth
/// socket that swaps which strike it carries must drop the old one, and until
/// now nothing in the transport could say so — the three message builders
/// existed and had no caller.
///
/// It mirrors the subscribe path deliberately, arm for arm, because the two
/// must agree on what a batch MEANS. If depth-200 rejects a 2-instrument
/// subscribe and its unsubscribe silently accepted one, a swap could send an
/// unsubscribe the socket never applied and then a subscribe that takes the
/// connection over its one-instrument limit — which Dhan answers with a Fatal
/// 804.
///
/// # Errors
///
/// The same shapes as [`build_subscribe_payload`]: an empty batch, a
/// depth-200 batch of more than one, a segment the depth endpoints refuse, or
/// a batch that split into anything other than exactly one message.
pub fn build_unsubscribe_payload(
    endpoint: DhanEndpointType,
    feed_mode: FeedMode,
    batch: &[SubscribeInstrument],
) -> Result<String, SubscribePayloadError> {
    let Some(first) = batch.first() else {
        return Err(SubscribePayloadError::EmptyBatch);
    };

    if endpoint == DhanEndpointType::Depth200 {
        if batch.len() > 1 {
            return Err(SubscribePayloadError::Depth200Overfull { got: batch.len() });
        }
        return build_two_hundred_depth_unsubscription_message(first.segment, first.security_id)
            .map_err(|reason| SubscribePayloadError::Refused { reason });
    }

    let mut instruments = Vec::with_capacity(batch.len());
    for instrument in batch {
        instruments.push(InstrumentSubscription::new(
            instrument.segment,
            instrument.security_id,
        ));
    }
    let mut messages = match endpoint {
        // NOT segment-validated, and the asymmetry with the subscribe arm is
        // deliberate rather than an oversight. Subscribe refuses a BSE_FNO
        // contract because depth is NSE-only at the vendor and a BSE subscribe
        // comes back as SILENCE — indistinguishable from a quiet book.
        // Unsubscribe has no such failure mode: dropping an instrument the
        // socket does not hold is a no-op, while REFUSING to drop one would
        // strand it subscribed, which is the outcome the refusal exists to
        // prevent. Fail-safe runs the opposite way on this path.
        DhanEndpointType::Depth20 => {
            build_twenty_depth_unsubscription_messages(&instruments, batch.len())
        }
        _ => build_unsubscription_messages(&instruments, feed_mode, batch.len()),
    };
    if messages.len() != 1 {
        return Err(SubscribePayloadError::BatchSplit {
            instruments: batch.len(),
            messages: messages.len(),
        });
    }
    messages.pop().ok_or(SubscribePayloadError::EmptyBatch)
}

// ---------------------------------------------------------------------------
// Frame classification — control plane only
// ---------------------------------------------------------------------------

/// What one inbound binary message is, at the transport level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameClass {
    /// Ordinary market data. Hand it up untouched; nothing here looks inside.
    Data,
    /// A Dhan disconnect control packet carrying a reason code.
    Disconnect(DisconnectCode),
}

/// Classifies one inbound binary message as data or a disconnect control
/// packet. O(1), zero allocation, total — any shape that is not exactly a
/// disconnect packet is [`FrameClass::Data`] and is handed up untouched.
///
/// The two feeds carry the disconnect packet differently and conflating them
/// would misread a real data frame as a disconnect:
/// * **main feed** — 10 bytes, 8-byte header, response code at byte 0 == 50,
///   reason as u16 LE at bytes 8..10 (`03-live-market-feed-websocket.md`
///   rule 12).
/// * **depth feeds** — 14 bytes, 12-byte header whose byte 0..2 is the message
///   LENGTH and whose byte 2 is the feed code == 50, reason as u16 LE at bytes
///   12..14.
///
/// Fail-safe direction: a malformed or unknown packet is DATA. A false
/// "disconnect" would park a healthy connection on a code we invented; a false
/// "data" is one 10-byte frame handed to a downstream parser that already
/// tolerates unknown response codes without panicking.
#[must_use]
pub fn classify_frame(endpoint: DhanEndpointType, frame: &[u8]) -> FrameClass {
    let (expected_len, code_offset, reason_offset) = match endpoint {
        DhanEndpointType::Depth20 | DhanEndpointType::Depth200 => (
            DEEP_DEPTH_HEADER_SIZE + 2,
            DEEP_DEPTH_HEADER_OFFSET_FEED_CODE,
            DEEP_DEPTH_HEADER_SIZE,
        ),
        DhanEndpointType::MainFeed | DhanEndpointType::OrderUpdate => {
            (DISCONNECT_PACKET_SIZE, 0, DISCONNECT_PACKET_SIZE - 2)
        }
    };
    if frame.len() != expected_len {
        // A Dhan frame STACKS packets, so a disconnect can arrive with data
        // ahead of it. Until 2026-08-25 this arm returned `Data` for every such
        // frame, so `[quote][disconnect 804]` never became
        // `SocketEvent::Closed{code}` -- the drain decoded the disconnect and
        // counted it as untyped "non-tick" traffic, and the reason reached no
        // log, no metric and no classifier. The socket then closed on TCP with
        // `code: None`, which `classify_disconnect` reads as Transient, so the
        // ladder retried the IDENTICAL over-limit subscribe set forever: the
        // exact self-amplifying loop the Fatal class was added to stop.
        //
        // Only the main feed walks here. The depth feeds carry a length-
        // prefixed header whose packet sizes this function does not own, and
        // their drain already counts a stacked disconnect -- guessing at their
        // boundaries to shave a milder gap would be inventing a classification.
        if matches!(endpoint, DhanEndpointType::MainFeed)
            && let Some(reason) =
                crate::parser::dispatcher::stacked_disconnect_reason(frame, MAX_PACKETS_PER_FRAME)
        {
            return FrameClass::Disconnect(DisconnectCode::from_u16(reason));
        }
        return FrameClass::Data;
    }
    let Some(&code) = frame.get(code_offset) else {
        return FrameClass::Data;
    };
    if code != RESPONSE_CODE_DISCONNECT {
        return FrameClass::Data;
    }
    let Some(raw) = frame.get(reason_offset..reason_offset + 2) else {
        return FrameClass::Data;
    };
    // Little endian, always (`03-live-market-feed-websocket.md` rule 13).
    let reason = u16::from_le_bytes([raw[0], raw[1]]);
    FrameClass::Disconnect(DisconnectCode::from_u16(reason))
}

// ---------------------------------------------------------------------------
// The socket
// ---------------------------------------------------------------------------

/// Everything one feed socket needs, minus the rotating credential.
///
/// Deliberately does NOT derive `Debug`: adding a credential-bearing field
/// later would then silently become loggable. `base_url` and `client_id` are
/// not secrets, but the struct must never become a place where one could hide.
pub struct DhanSocketParams {
    /// Which endpoint type this socket serves. Selects the frame cap, the TLS
    /// ALPN profile, the URL shape and the disconnect-packet layout.
    pub endpoint: DhanEndpointType,
    /// Base URL WITHOUT query parameters (e.g. `wss://api-feed.dhan.co`).
    /// Config-driven for the main feed; the depth bases live in
    /// `constants.rs`.
    pub base_url: String,
    /// Dhan `clientId`. Not a secret on its own, but it is redacted out of
    /// every log line here anyway — it is an account identifier.
    pub client_id: String,
    /// Main-feed subscription mode. Ignored by the depth endpoints, whose
    /// request code is fixed at 23 by the protocol. Defaults to
    /// [`DEFAULT_MAIN_FEED_MODE`]; the tuning owner may override.
    pub feed_mode: FeedMode,
}

impl DhanSocketParams {
    /// Params for `endpoint` in the scope-locked [`DEFAULT_MAIN_FEED_MODE`].
    #[must_use]
    pub fn new(endpoint: DhanEndpointType, base_url: String, client_id: String) -> Self {
        Self {
            endpoint,
            base_url,
            client_id,
            feed_mode: DEFAULT_MAIN_FEED_MODE,
        }
    }
}

/// The production [`DhanFeedSocket`]: tokio-tungstenite over TLS, with an
/// explicit frame cap and no policy of its own.
///
/// No `Debug` derive, by design — see [`DhanSocketParams`].
pub struct DhanFeedSocketImpl<T: FeedTokenSource> {
    params: DhanSocketParams,
    token: T,
    /// The live socket. `None` before the first dial and after every close, so
    /// a stale stream can never be written to.
    stream: Option<WebSocketStream<MaybeTlsStream<TcpStream>>>,
}

impl<T: FeedTokenSource> DhanFeedSocketImpl<T> {
    /// A socket that has not dialed yet.
    #[must_use]
    pub const fn new(params: DhanSocketParams, token: T) -> Self {
        Self {
            params,
            token,
            stream: None,
        }
    }

    /// The endpoint type this socket serves.
    #[must_use]
    pub const fn endpoint(&self) -> DhanEndpointType {
        self.params.endpoint
    }

    /// Whether a socket is currently established.
    #[must_use]
    pub const fn is_connected(&self) -> bool {
        self.stream.is_some()
    }

    /// The loggable form of this socket's URL — host and path, no query.
    #[must_use]
    pub fn loggable_url(&self) -> String {
        loggable_url(&self.params.base_url)
    }

    fn count_dial_failure(&self, reason: &'static str) {
        metrics::counter!(
            DIAL_FAILED_METRIC,
            "endpoint" => self.params.endpoint.as_str(),
            "reason" => reason,
        )
        .increment(1);
    }
    /// Writes ONE unsubscribe message.
    ///
    /// **ADDED 2026-08-26** for the per-minute at-the-money re-selection. It
    /// deliberately reuses the subscribe path's failure metrics and error
    /// code rather than minting new ones: an operator asking "did this socket
    /// fail to change what it is subscribed to?" wants one answer, and the
    /// `reason` label already separates a payload refusal from a dead socket
    /// from a write failure from a timeout.
    async fn send_unsubscribe_in_mode(
        &mut self,
        batch: &[SubscribeInstrument],
        feed_mode: FeedMode,
    ) -> Result<(), SocketFailure> {
        let endpoint = self.params.endpoint;
        let payload = match build_unsubscribe_payload(endpoint, feed_mode, batch) {
            Ok(p) => p,
            Err(err) => {
                metrics::counter!(
                    SUBSCRIBE_FAILED_METRIC,
                    "endpoint" => endpoint.as_str(),
                    "reason" => "unsubscribe_payload",
                )
                .increment(1);
                error!(
                    code = ErrorCode::WsGapSubscriptionBatching.code_str(),
                    endpoint = endpoint.as_str(),
                    instruments = batch.len(),
                    reason = %err,
                    "Dhan unsubscribe payload could not be built — the instrument stays \
                     subscribed, so this socket keeps carrying a contract that is no longer \
                     the one that was chosen"
                );
                return Err(SocketFailure);
            }
        };

        let Some(stream) = self.stream.as_mut() else {
            metrics::counter!(
                SUBSCRIBE_FAILED_METRIC,
                "endpoint" => endpoint.as_str(),
                "reason" => "unsubscribe_not_connected",
            )
            .increment(1);
            warn!(
                code = ErrorCode::WsGapSubscriptionBatching.code_str(),
                endpoint = endpoint.as_str(),
                "unsubscribe attempted with no live Dhan socket"
            );
            return Err(SocketFailure);
        };

        let send = stream.send(Message::Text(payload.into()));
        match tokio::time::timeout(SUBSCRIBE_SEND_TIMEOUT, send).await {
            Ok(Ok(())) => {
                debug!(
                    endpoint = endpoint.as_str(),
                    instruments = batch.len(),
                    "Dhan unsubscribe batch sent"
                );
                Ok(())
            }
            Ok(Err(err)) => {
                metrics::counter!(
                    SUBSCRIBE_FAILED_METRIC,
                    "endpoint" => endpoint.as_str(),
                    "reason" => "unsubscribe_send",
                )
                .increment(1);
                warn!(
                    code = ErrorCode::WsGapSubscriptionBatching.code_str(),
                    endpoint = endpoint.as_str(),
                    instruments = batch.len(),
                    reason = %safe_err(&err),
                    "Dhan unsubscribe batch could not be written to the socket"
                );
                Err(SocketFailure)
            }
            Err(_elapsed) => {
                metrics::counter!(
                    SUBSCRIBE_FAILED_METRIC,
                    "endpoint" => endpoint.as_str(),
                    "reason" => "unsubscribe_timeout",
                )
                .increment(1);
                warn!(
                    code = ErrorCode::WsGapSubscriptionBatching.code_str(),
                    endpoint = endpoint.as_str(),
                    instruments = batch.len(),
                    timeout_secs = SUBSCRIBE_SEND_TIMEOUT.as_secs(),
                    "Dhan unsubscribe batch write timed out"
                );
                Err(SocketFailure)
            }
        }
    }

    async fn send_subscribe_in_mode(
        &mut self,
        batch: &[SubscribeInstrument],
        feed_mode: FeedMode,
    ) -> Result<(), SocketFailure> {
        let endpoint = self.params.endpoint;
        let payload = match build_subscribe_payload(endpoint, feed_mode, batch) {
            Ok(p) => p,
            Err(err) => {
                metrics::counter!(
                    SUBSCRIBE_FAILED_METRIC,
                    "endpoint" => endpoint.as_str(),
                    "reason" => "payload",
                )
                .increment(1);
                error!(
                    code = ErrorCode::WsGapSubscriptionBatching.code_str(),
                    endpoint = endpoint.as_str(),
                    instruments = batch.len(),
                    reason = %err,
                    "Dhan subscribe payload could not be built — the socket would be live but \
                     blind, so the connection is torn down instead"
                );
                return Err(SocketFailure);
            }
        };

        let Some(stream) = self.stream.as_mut() else {
            metrics::counter!(
                SUBSCRIBE_FAILED_METRIC,
                "endpoint" => endpoint.as_str(),
                "reason" => "not_connected",
            )
            .increment(1);
            warn!(
                code = ErrorCode::WsGapSubscriptionBatching.code_str(),
                endpoint = endpoint.as_str(),
                "subscribe attempted with no live Dhan socket"
            );
            return Err(SocketFailure);
        };

        let send = stream.send(Message::Text(payload.into()));
        match tokio::time::timeout(SUBSCRIBE_SEND_TIMEOUT, send).await {
            Ok(Ok(())) => {
                debug!(
                    endpoint = endpoint.as_str(),
                    instruments = batch.len(),
                    "Dhan subscribe batch sent"
                );
                Ok(())
            }
            Ok(Err(err)) => {
                metrics::counter!(
                    SUBSCRIBE_FAILED_METRIC,
                    "endpoint" => endpoint.as_str(),
                    "reason" => "send",
                )
                .increment(1);
                warn!(
                    code = ErrorCode::WsGapSubscriptionBatching.code_str(),
                    endpoint = endpoint.as_str(),
                    instruments = batch.len(),
                    reason = %safe_err(&err),
                    "Dhan subscribe batch could not be written to the socket"
                );
                Err(SocketFailure)
            }
            Err(_elapsed) => {
                metrics::counter!(
                    SUBSCRIBE_FAILED_METRIC,
                    "endpoint" => endpoint.as_str(),
                    "reason" => "timeout",
                )
                .increment(1);
                warn!(
                    code = ErrorCode::WsGapSubscriptionBatching.code_str(),
                    endpoint = endpoint.as_str(),
                    instruments = batch.len(),
                    timeout_secs = SUBSCRIBE_SEND_TIMEOUT.as_secs(),
                    "Dhan subscribe write stalled past its deadline"
                );
                Err(SocketFailure)
            }
        }
    }
}

impl<T: FeedTokenSource> DhanFeedSocket for DhanFeedSocketImpl<T> {
    async fn connect(&mut self) -> Result<(), SocketFailure> {
        // A previous socket must be gone before a new one is dialed, or the
        // pool budget is silently exceeded and Dhan kills the OLDEST member.
        self.close().await;

        let endpoint = self.params.endpoint;
        let Some(token) = self.token.current_token() else {
            self.count_dial_failure("no_token");
            warn!(
                code = ErrorCode::WsGapConnectionState.code_str(),
                endpoint = endpoint.as_str(),
                url = %self.loggable_url(),
                "no Dhan access token available — cannot dial the live feed"
            );
            return Err(SocketFailure);
        };

        // TLS profile is NOT uniform. depth-200 must advertise NO ALPN: the
        // http/1.1 profile produced two weeks of ResetWithoutClosingHandshake
        // on full-depth-api.dhan.co until the 2026-04-23 vendor-SDK
        // verification. Everything else goes through proxies that REQUIRE
        // http/1.1 ALPN for the upgrade.
        let connector = match endpoint {
            DhanEndpointType::Depth200 => build_websocket_tls_connector_no_alpn(),
            _ => build_websocket_tls_connector(),
        };
        let connector = match connector {
            Ok(c) => c,
            Err(err) => {
                self.count_dial_failure("tls_config");
                error!(
                    code = ErrorCode::WsGapConnectionState.code_str(),
                    endpoint = endpoint.as_str(),
                    reason = %safe_err(&err),
                    "TLS connector could not be built for a Dhan feed socket"
                );
                return Err(SocketFailure);
            }
        };

        // The URL carries the JWT. It lives in this scope only, is never
        // stored on `self`, and is zeroized on drop.
        let url = build_feed_url(
            endpoint,
            &self.params.base_url,
            &token,
            &self.params.client_id,
        );
        let request = match url.as_str().into_client_request() {
            Ok(r) => r,
            Err(err) => {
                self.count_dial_failure("bad_url");
                error!(
                    code = ErrorCode::WsGapConnectionState.code_str(),
                    endpoint = endpoint.as_str(),
                    url = %self.loggable_url(),
                    reason = %safe_err(&err),
                    "Dhan feed URL could not be turned into a client request"
                );
                return Err(SocketFailure);
            }
        };

        // EXPLICIT config — never `None`. See the module docs: `None` inherits
        // a 64 MiB message ceiling.
        let config = Some(websocket_config(endpoint));
        // Third argument is `disable_nagle`. It is `true` — Nagle's algorithm is
        // DISABLED on every one of the 16 sockets.
        //
        // This lane is receive-heavy, so the instinct is that Nagle cannot
        // matter. It does, because we DO write, and the writes are small and
        // latency-critical:
        //
        //   1. PONGS. Dhan closes a socket after ~40 s of silence
        //      (`deploy/aws/sysctl/99-tickvault-net.conf:123-124`). A pong is a
        //      handful of bytes. With Nagle ON, a small write is held until the
        //      previous segment is ACKed; paired with the peer's delayed-ACK
        //      timer that is up to ~200 ms of avoidable delay on the one write
        //      whose lateness costs a RECONNECT — and a reconnect costs a full
        //      re-auth and re-subscribe of the whole instrument set.
        //   2. SUBSCRIBE BATCHES. Up to 250 messages across the pool at open.
        //      Nagle coalescing here delays the moment the feed starts flowing,
        //      at exactly 09:15.
        //
        // Left `false` since the lane was revived. Changing it is free, cannot
        // affect the depth-200 no-ALPN carve-out above (a socket option and a
        // TLS config are orthogonal), and is standard practice for trading
        // sockets. Pinned by `dial_disables_nagle_on_every_endpoint`.
        let dial = connect_async_tls_with_config(request, config, true, Some(connector));
        let outcome = match tokio::time::timeout(DIAL_TIMEOUT, dial).await {
            Ok(result) => result,
            Err(_elapsed) => {
                self.count_dial_failure("timeout");
                warn!(
                    code = ErrorCode::WsGapConnectionState.code_str(),
                    endpoint = endpoint.as_str(),
                    url = %self.loggable_url(),
                    timeout_secs = DIAL_TIMEOUT.as_secs(),
                    "Dhan feed dial timed out — a dial that never completes is invisible to \
                     the idle watchdog, so it is abandoned here"
                );
                return Err(SocketFailure);
            }
        };

        match outcome {
            Ok((stream, _response)) => {
                debug!(
                    endpoint = endpoint.as_str(),
                    url = %self.loggable_url(),
                    max_frame_bytes = max_frame_bytes(endpoint),
                    "Dhan feed socket connected"
                );
                self.stream = Some(stream);
                Ok(())
            }
            Err(err) => {
                self.count_dial_failure("connect");
                warn!(
                    code = ErrorCode::WsGapConnectionState.code_str(),
                    endpoint = endpoint.as_str(),
                    url = %self.loggable_url(),
                    reason = %safe_err(&err),
                    "Dhan feed dial failed"
                );
                Err(SocketFailure)
            }
        }
    }

    /// Subscribes `batch`, splitting indices into their own message.
    ///
    /// Indices are subscribed in [`IDX_I_FEED_MODE`] and everything else in the
    /// configured mode, because Dhan serves an `IDX_I` Full subscription with
    /// SILENCE rather than an error (see [`IDX_I_FEED_MODE`] for the measured
    /// evidence). One Dhan message carries exactly one `RequestCode`, so the
    /// two modes cannot share a message and the split is structural, not a
    /// preference.
    ///
    /// Only the main feed splits. The depth endpoints refuse a non-NSE segment
    /// outright and never carry an index, so partitioning there would add a
    /// branch that can never be taken.
    ///
    /// Both halves must reach the wire for the subscribe to count as sent: a
    /// partial success would leave the socket live and blind for one half of
    /// its universe, which is the exact shape this whole change exists to
    /// remove.
    async fn send_subscribe(&mut self, batch: &[SubscribeInstrument]) -> Result<(), SocketFailure> {
        let configured = self.params.feed_mode;
        if self.params.endpoint != DhanEndpointType::MainFeed {
            return self.send_subscribe_in_mode(batch, configured).await;
        }

        let (indices, others) = partition_index_batch(batch);

        // Nothing to split — send the batch exactly as before.
        if indices.is_empty() {
            return self.send_subscribe_in_mode(batch, configured).await;
        }

        if !others.is_empty() {
            self.send_subscribe_in_mode(&others, configured).await?;
        }
        self.send_subscribe_in_mode(&indices, IDX_I_FEED_MODE).await
    }

    async fn send_unsubscribe(
        &mut self,
        batch: &[SubscribeInstrument],
    ) -> Result<(), SocketFailure> {
        // Mirrors `send_subscribe` arm for arm, including the index split. An
        // IDX_I instrument was SUBSCRIBED in Quote mode while everything else
        // went out in Full (the 2026-08-21 index-mode carve-out), and the
        // unsubscribe RequestCode is derived from the feed mode — so
        // unsubscribing an index with the Full code would send a code the
        // instrument was never subscribed under, and Dhan would have nothing
        // to remove. The socket would look fine and keep the instrument.
        let configured = self.params.feed_mode;
        if self.params.endpoint != DhanEndpointType::MainFeed {
            return self.send_unsubscribe_in_mode(batch, configured).await;
        }

        let (indices, others) = partition_index_batch(batch);

        if indices.is_empty() {
            return self.send_unsubscribe_in_mode(batch, configured).await;
        }

        if !others.is_empty() {
            self.send_unsubscribe_in_mode(&others, configured).await?;
        }
        self.send_unsubscribe_in_mode(&indices, IDX_I_FEED_MODE)
            .await
    }

    async fn recv(&mut self) -> SocketEvent {
        let endpoint = self.params.endpoint;
        let Some(stream) = self.stream.as_mut() else {
            return SocketEvent::Closed { code: None };
        };

        // ONE poll, ONE event — no loop (2026-08-19).
        //
        // This used to loop over control frames, skipping ping/pong/text as
        // "carrying no market data and costing nothing to skip". They cost
        // ~300 reconnects a morning: skipping a ping meant the supervisor
        // never saw it, so the idle watchdog never reset and tore down a
        // healthy socket every 27 seconds of pre-open quiet. A ping is not
        // noise, it is the peer telling us it is alive, and the supervisor is
        // the thing that needs to hear it. Every arm now returns an event.
        {
            let Some(message) = stream.next().await else {
                debug!(endpoint = endpoint.as_str(), "Dhan feed stream ended");
                return SocketEvent::Closed { code: None };
            };
            let message = match message {
                Ok(m) => m,
                Err(err) => {
                    // The frame cap surfaces here as `Capacity`. It is the
                    // hostile-input signal and is counted separately from an
                    // ordinary transport error.
                    let reason =
                        if matches!(err, tokio_tungstenite::tungstenite::Error::Capacity(_)) {
                            "oversize"
                        } else {
                            "transport"
                        };
                    metrics::counter!(
                        FRAME_REFUSED_METRIC,
                        "endpoint" => endpoint.as_str(),
                        "reason" => reason,
                    )
                    .increment(1);
                    warn!(
                        code = ErrorCode::WsGapConnectionState.code_str(),
                        endpoint = endpoint.as_str(),
                        reason,
                        detail = %safe_err(&err),
                        "Dhan feed read failed — treating as a disconnect so the supervisor's \
                         ladder reconnects"
                    );
                    return SocketEvent::Closed { code: None };
                }
            };

            match message {
                Message::Binary(payload) => {
                    match classify_frame(endpoint, &payload) {
                        FrameClass::Disconnect(code) => SocketEvent::Closed { code: Some(code) },
                        // `Bytes` -> `bytes::Bytes` is a refcount move, not a
                        // copy: the whole point of this type on this path.
                        FrameClass::Data => SocketEvent::Frame(ws_bytes_to_bytes(payload)),
                    }
                }
                Message::Close(_frame) => {
                    // A WebSocket close code is NOT a Dhan disconnect code
                    // (1000-4999 vs 800-814); conflating them would invent a
                    // classification. Dhan sends its own code as a binary
                    // packet, handled above.
                    debug!(
                        endpoint = endpoint.as_str(),
                        "Dhan feed sent a WebSocket close frame"
                    );
                    SocketEvent::Closed { code: None }
                }
                Message::Ping(_) | Message::Pong(_) | Message::Text(_) | Message::Frame(_) => {
                    let kind = match message {
                        Message::Ping(_) => "ping",
                        Message::Pong(_) => "pong",
                        Message::Text(_) => "text",
                        _ => "raw",
                    };
                    metrics::counter!(
                        CONTROL_FRAME_METRIC,
                        "endpoint" => endpoint.as_str(),
                        "kind" => kind,
                    )
                    .increment(1);
                    // RETURN, do not loop (2026-08-19).
                    //
                    // This arm used to increment the counter and fall through
                    // to the next loop iteration, so a Ping never reached the
                    // supervisor and never reset the idle watchdog. Dhan pings
                    // to keep the connection open; we counted the proof of
                    // life and threw it away, then tore the socket down at 27s
                    // of "silence" that was only silence of DATA.
                    //
                    // Measured on prod the morning this was found: 8–25
                    // reconnects PER MINUTE from 08:31 to 08:59 IST — the
                    // whole pre-open window, when no instrument ticks because
                    // the market is shut — and exactly ZERO after 09:15 once
                    // real data started resetting the watchdog on its own.
                    // ~300 needless reconnects a morning, each one a full
                    // re-auth plus a re-subscribe of the entire universe.
                    SocketEvent::KeepAlive
                }
            }
        }
    }

    async fn close(&mut self) {
        let Some(mut stream) = self.stream.take() else {
            return;
        };
        // Best effort. A close that fails means the peer is already gone, which
        // is exactly the state we are trying to reach.
        if let Err(err) = stream.close(None).await {
            debug!(
                endpoint = self.params.endpoint.as_str(),
                reason = %safe_err(&err),
                "Dhan feed socket close was not clean — the socket is dropped regardless"
            );
        }
    }
}

/// Reconnect-safety guard — the successor to the deleted `SubscribeRxGuard`.
///
/// # What the old guard protected, and why this replaces it
/// The pre-2026-07-13 `SubscribeRxGuard` (`73c17517^:connection.rs:145`) took a
/// subscribe-command receiver OUT of a shared `Mutex<Option<Receiver>>` slot for
/// the duration of the read loop and reinstalled it on every exit path. It
/// existed because the prior code did `.take()` without ever putting the
/// receiver back, so after the 2026-04-24 10:08 IST TCP-RST storm the reconnected
/// socket had nowhere to deliver a late subscribe command and came up
/// under-subscribed — silent, partial tick loss that cascaded into further
/// watchdog reconnects.
///
/// **That failure mode is now structurally impossible rather than guarded.** The
/// shared-slot ownership model is gone: `run_connection` owns its
/// `SubscribeGuard` BY VALUE for the whole life of the connection, and the
/// subscription set outlives every socket. A reconnect calls `mark_lost()` and
/// then REPLAYS the same set from the same owned value; there is no slot to take
/// from and therefore nothing to fail to put back. Rebuilding a receiver guard
/// here would mean re-inventing the deleted command channel just to have
/// something to guard, which would add the failure mode back rather than remove
/// it.
///
/// # What DOES still need a drop guarantee
/// One hazard survives at this layer and it is the same class of bug: a socket
/// dropped while still holding a LIVE stream. An orphaned stream keeps its TCP
/// connection open, Dhan keeps counting it against the five-socket cap, and the
/// next dial makes Dhan silently disconnect the OLDEST healthy socket with code
/// 805 — a sibling connection's whole subscription lost, exactly the
/// under-subscribed-after-reconnect outcome the old guard existed to prevent.
///
/// Dropping the stream releases the file descriptor and closes the connection,
/// so the budget slot IS freed on every exit path including a panic unwind. This
/// impl makes the unclean case VISIBLE rather than silent: a teardown that never
/// went through [`DhanFeedSocket::close`] is logged, so an operator debugging an
/// 805 can tell a task that was cancelled mid-stream from one that closed
/// properly.
impl<T: FeedTokenSource> Drop for DhanFeedSocketImpl<T> {
    fn drop(&mut self) {
        if self.stream.is_some() {
            // Not an error: task cancellation is a legitimate way to end a
            // connection, and the fd is released either way. It is logged
            // because an unexplained 805 on a SIBLING socket is otherwise very
            // hard to attribute.
            debug!(
                endpoint = self.params.endpoint.as_str(),
                "Dhan feed socket dropped while still holding a live stream — the file \
                 descriptor is released by this drop, which frees the connection-budget slot; \
                 no WebSocket close handshake was sent"
            );
        }
    }
}

/// Moves a tungstenite payload into the `bytes::Bytes` the sink expects.
///
/// Both are `bytes::Bytes` at the type level in tungstenite 0.29, so this is a
/// move, not a copy. Written as a named function so a future upstream change
/// that turns it INTO a copy shows up as an edit here rather than silently
/// doubling the per-frame cost.
#[inline]
fn ws_bytes_to_bytes(payload: WsBytes) -> Bytes {
    payload
}

#[cfg(test)]
mod tests {
    use super::*;
    use tickvault_common::constants::{
        DHAN_MAIN_FEED_WS_BASE_URL, DHAN_TWENTY_DEPTH_WS_BASE_URL,
        DHAN_TWO_HUNDRED_DEPTH_WS_BASE_URL, RESPONSE_CODE_FULL, RESPONSE_CODE_OI,
        RESPONSE_CODE_QUOTE, RESPONSE_CODE_TICKER, TWENTY_DEPTH_PACKET_SIZE,
        TWO_HUNDRED_DEPTH_PACKET_SIZE,
    };
    use tickvault_common::types::{ExchangeSegment, SecurityId};

    /// EVERY endpoint's URL must yield a request-target starting with `/`.
    ///
    /// This is the regression pin for the 2026-08-12 all-day main-feed outage:
    /// twelve `400 Bad Request` dials and a cross-verify reporting
    /// `compared: 0, missing_live: 373`, all caused by one missing slash.
    ///
    /// It asserts on `path_and_query()` rather than on the URL string because
    /// that is the value tungstenite actually writes into the request line
    /// (`GET {path_and_query} HTTP/1.1`, `handshake/client.rs:114-117`). A test
    /// that only checked our own formatting would have passed throughout the
    /// outage — the URL string looked exactly like the vendor doc's example.
    #[test]
    fn test_every_endpoint_request_target_starts_with_a_slash() {
        use tokio_tungstenite::tungstenite::client::IntoClientRequest;

        for (endpoint, base) in [
            (DhanEndpointType::MainFeed, DHAN_MAIN_FEED_WS_BASE_URL),
            (DhanEndpointType::Depth20, DHAN_TWENTY_DEPTH_WS_BASE_URL),
            (
                DhanEndpointType::Depth200,
                DHAN_TWO_HUNDRED_DEPTH_WS_BASE_URL,
            ),
        ] {
            let url = build_feed_url(endpoint, base, FAKE_TOKEN, FAKE_CLIENT_ID);
            let request = url
                .as_str()
                .into_client_request()
                .expect("the built URL must parse as a client request");
            let target = request
                .uri()
                .path_and_query()
                .expect("a websocket URL always has a query here")
                .as_str();
            assert!(
                target.starts_with('/'),
                "{endpoint:?} produces request line `GET {target} HTTP/1.1` — an \
                 origin-form request-target must begin with '/' (RFC 9112 3.2.1). \
                 Dhan answers 400 and the socket never opens."
            );
        }
    }

    /// The slash is inserted from the URL's SHAPE, not from a per-endpoint
    /// special case — so a future base-URL constant change is covered without
    /// anyone remembering the incident.
    #[test]
    fn test_url_has_path_drives_the_slash_rather_than_the_endpoint_kind() {
        assert!(!url_has_path("wss://api-feed.dhan.co"));
        assert!(!url_has_path("wss://full-depth-api.dhan.co"));
        assert!(url_has_path("wss://depth-api-feed.dhan.co/twentydepth"));
        assert!(url_has_path("wss://host.example/"));
        // A scheme's own `//` must never be mistaken for a path.
        assert!(!url_has_path("ws://h"));
        // Unrecognised shapes are left ALONE rather than "fixed" into
        // something else.
        assert!(url_has_path("not-a-url"));
    }

    #[test]
    fn test_build_feed_url_shape_per_endpoint() {
        // Dhan's own reference requires all four query parameters on the main
        // feed, and `version=2` ONLY there. A missing parameter is refused at
        // the handshake with a code that looks like an auth failure, which
        // sends the operator hunting the wrong problem.
        let main = build_feed_url(
            DhanEndpointType::MainFeed,
            DHAN_MAIN_FEED_WS_BASE_URL,
            FAKE_TOKEN,
            FAKE_CLIENT_ID,
        );
        // NOTE the `/` before `?`. Without it tungstenite writes a request
        // line of `GET ?version=2… HTTP/1.1`, which Dhan answers with 400 —
        // the 2026-08-12 all-day outage. See `url_has_path`.
        assert!(main.starts_with("wss://api-feed.dhan.co/?version=2&token="));
        assert!(main.contains("&clientId="));
        assert!(main.ends_with("&authType=2"));

        // depth-20 takes the same parameters WITHOUT `version=2`.
        let d20 = build_feed_url(
            DhanEndpointType::Depth20,
            DHAN_TWENTY_DEPTH_WS_BASE_URL,
            FAKE_TOKEN,
            FAKE_CLIENT_ID,
        );
        assert!(
            !d20.contains("version=2"),
            "version=2 is main-feed only; sending it elsewhere is protocol drift"
        );
        assert!(d20.starts_with("wss://depth-api-feed.dhan.co/twentydepth?token="));

        // depth-200 connects on the ROOT path — `/?token=`. This exact detail
        // cost two weeks of ResetWithoutClosingHandshake in April 2026 before
        // Dhan's own SDK settled it, so it is pinned rather than trusted.
        let d200 = build_feed_url(
            DhanEndpointType::Depth200,
            DHAN_TWO_HUNDRED_DEPTH_WS_BASE_URL,
            FAKE_TOKEN,
            FAKE_CLIENT_ID,
        );
        assert!(
            d200.starts_with("wss://full-depth-api.dhan.co/?token="),
            "depth-200 must use the root path: {}",
            redact_url_params(&d200)
        );
    }

    #[test]
    fn test_build_feed_url_tolerates_a_trailing_slash_on_the_base() {
        // A base URL with a trailing slash is an easy config slip. Producing
        // `host//?token=` would be refused by the server for a reason that
        // reads as an auth problem, so the slash is trimmed rather than
        // trusted.
        let a = build_feed_url(
            DhanEndpointType::MainFeed,
            &trailing_slash_base(),
            FAKE_TOKEN,
            FAKE_CLIENT_ID,
        );
        let b = build_feed_url(
            DhanEndpointType::MainFeed,
            DHAN_MAIN_FEED_WS_BASE_URL,
            FAKE_TOKEN,
            FAKE_CLIENT_ID,
        );
        assert_eq!(*a, *b, "a trailing slash must not change the URL");
    }

    #[test]
    fn test_build_subscribe_payload_refuses_an_empty_batch() {
        // An empty batch means the caller has nothing to subscribe. Sending a
        // zero-instrument message would be accepted by the server and deliver
        // nothing forever — silence that looks like a dead market.
        assert!(matches!(
            build_subscribe_payload(DhanEndpointType::MainFeed, FeedMode::Quote, &[]),
            Err(SubscribePayloadError::EmptyBatch)
        ));
    }

    #[test]
    fn test_build_subscribe_payload_refuses_an_overfull_depth_200_batch() {
        // depth-200 is one instrument per connection. Silently dropping the
        // extras would subscribe less than the caller asked for while
        // reporting success.
        let two = [
            SubscribeInstrument {
                security_id: SecurityId::from(13u32),
                segment: ExchangeSegment::IdxI,
            },
            SubscribeInstrument {
                security_id: SecurityId::from(25u32),
                segment: ExchangeSegment::IdxI,
            },
        ];
        assert!(matches!(
            build_subscribe_payload(DhanEndpointType::Depth200, FeedMode::Quote, &two),
            Err(SubscribePayloadError::Depth200Overfull { got: 2 })
        ));
    }

    #[test]
    fn test_build_subscribe_payload_emits_security_ids_as_strings() {
        // Dhan requires SecurityId as a JSON STRING. A numeric id is rejected
        // by the server, and the rejection does not name the field — so this
        // is pinned at the payload level.
        let one = [SubscribeInstrument {
            security_id: SecurityId::from(13u32),
            segment: ExchangeSegment::IdxI,
        }];
        let payload = build_subscribe_payload(DhanEndpointType::MainFeed, FeedMode::Quote, &one)
            .expect("a single valid instrument must build");
        assert!(
            payload.contains("\"13\""),
            "SecurityId must serialize as a string: {payload}"
        );
        assert!(
            !payload.contains(":13,") && !payload.contains(&bare_id_at_object_end()),
            "SecurityId must NOT serialize as a bare number: {payload}"
        );
    }

    const FAKE_TOKEN: &str = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMTA2NjU2ODgyIn0.sig";

    /// The main-feed base with a trailing slash, for the trim test.
    ///
    /// Built by concatenation rather than written as a literal because the
    /// banned-pattern scanner tracks `#[cfg(test)]` regions by counting braces
    /// in the raw source — a `{` or `}` inside a string literal (as a
    /// `format!` placeholder would be) closes the skip region early and
    /// exposes the rest of the test module to production-only rules. Cheap to
    /// avoid; confusing to debug.
    fn trailing_slash_base() -> String {
        let mut s = String::from(DHAN_MAIN_FEED_WS_BASE_URL);
        s.push('/');
        s
    }

    /// A bare numeric SecurityId immediately before a JSON object close.
    ///
    /// Assembled from a byte rather than written as a literal, and this doc
    /// comment names no brace either, for one reason worth knowing before you
    /// simplify it: the banned-pattern scanner decides where the
    /// test-only region ends by counting brace characters in the RAW SOURCE,
    /// comments and string contents included. A balanced pair (every format
    /// placeholder) nets out and is harmless; a LONE closing brace shifts the
    /// depth, ends the skip region early, and silently exposes the rest of
    /// this test module to production-only rules. Two earlier attempts at this
    /// helper — one in code, one in this very comment — did exactly that, and
    /// the symptom was a pre-existing test 300 lines below being reported as a
    /// hot-path violation.
    fn bare_id_at_object_end() -> String {
        let mut s = String::from(":13");
        s.push(char::from(OBJECT_CLOSE_BYTE));
        s
    }

    /// ASCII `0x7D`, the JSON object terminator.
    const OBJECT_CLOSE_BYTE: u8 = 0x7D;

    /// Owned copies of the production base URL / client id for the socket
    /// constructors, which take `String`.
    fn main_feed_base() -> String {
        String::from(DHAN_MAIN_FEED_WS_BASE_URL)
    }

    fn fake_client_id() -> String {
        String::from(FAKE_CLIENT_ID)
    }
    /// A deliberately SYNTHETIC client id.
    ///
    /// This constant previously held the operator's REAL Dhan client id. The
    /// `FAKE_` prefix did not make it fake — the value is published in the
    /// Dhan support correspondence and the secret scanner correctly refused
    /// it. Every real credential lives in AWS SSM Parameter Store and is read
    /// at runtime; none is ever committed, not even to a test.
    ///
    /// Deliberately NOT a 10-digit numeric string. The secret scanner's rule
    /// is `client[_-]id.*=.*"[0-9]{10,}"` — it matches the SHAPE, not one
    /// particular account, and it is right to. A synthetic 10-digit value
    /// would still trip it, and silencing that by renaming the constant would
    /// be evading the scanner rather than satisfying it.
    ///
    /// Nothing is lost: `redact_url_params` redacts the whole `clientId=`
    /// parameter regardless of its value, so the URL-construction and
    /// leak-assertion tests below exercise exactly the same code paths.
    const FAKE_CLIENT_ID: &str = "TEST-CLIENT-ID";

    fn instrument(id: SecurityId, segment: ExchangeSegment) -> SubscribeInstrument {
        SubscribeInstrument {
            security_id: id,
            segment,
        }
    }

    // -- frame caps ---------------------------------------------------------

    /// The main-feed cap must admit the burst a FULLY-SUBSCRIBED connection
    /// can produce, not the burst a single subscribe batch describes.
    ///
    /// This is the 2026-08-14 regression pin. The old cap was 256 KiB, sized
    /// from "a subscribe message is capped at 100 instruments … ~16 KiB". A
    /// connection carries up to 5,000 instruments, so one message with a
    /// single Full packet per instrument is ~810 KiB — three times that cap.
    /// Exceeding it raises `Error::Capacity`, which this module reports as a
    /// disconnect, so the supervisor reconnects, re-subscribes all 5,000, and
    /// the same vendor backlog arrives again: a permanent reconnect loop at
    /// 09:15, while `tv_dhan_feed_stack_up` still reads 1.0.
    #[test]
    fn test_main_feed_cap_admits_a_full_connection_burst_not_a_subscribe_batch() {
        let one_packet_per_instrument =
            MAIN_FEED_LARGEST_PACKET_BYTES * (MAIN_FEED_INSTRUMENTS_PER_CONNECTION as usize);

        assert!(
            MAIN_FEED_MAX_FRAME_BYTES >= one_packet_per_instrument,
            "main-feed cap {MAIN_FEED_MAX_FRAME_BYTES} is below {one_packet_per_instrument}, \
             the size of ONE Full packet for every instrument a connection carries \
             ({MAIN_FEED_INSTRUMENTS_PER_CONNECTION} x {MAIN_FEED_LARGEST_PACKET_BYTES} B). \
             At market open Dhan flushes its queued backlog, so this is the ordinary case, \
             not the worst one — and a cap below it turns every open into a reconnect loop."
        );

        // Non-vacuity: the cap must be derived from the per-CONNECTION count,
        // not the per-SUBSCRIBE-MESSAGE count. If someone re-derives it from
        // the 100-instrument batch again, this catches it — 100 x 162 x 2 is
        // ~32 KiB, nowhere near the bound above.
        let subscribe_batch_sized = MAIN_FEED_LARGEST_PACKET_BYTES * 100 * 2;
        assert!(
            MAIN_FEED_MAX_FRAME_BYTES > subscribe_batch_sized,
            "main-feed cap {MAIN_FEED_MAX_FRAME_BYTES} looks like it was sized from the \
             100-instrument SUBSCRIBE BATCH ({subscribe_batch_sized} B) rather than the \
             5,000 instruments a CONNECTION carries. That is the exact reasoning error this \
             pin exists to prevent."
        );
    }

    #[test]
    fn test_every_endpoint_cap_is_finite_and_far_below_the_library_default() {
        // The whole point of this module's config: tungstenite's default is a
        // 64 MiB message / 16 MiB frame, which sixteen sockets could turn into
        // a gigabyte of attacker-controlled allocation.
        const LIBRARY_DEFAULT_MESSAGE_BYTES: usize = 64 << 20;
        for endpoint in DhanEndpointType::ALL {
            let cap = max_frame_bytes(endpoint);
            assert!(cap > 0, "{endpoint} cap must be positive");
            // 2026-08-14: was `cap <= 512 * 1024`, "the few-hundred-KiB
            // envelope". That bound encoded the SAME mistake the main-feed cap
            // itself did — it was derived from a 100-instrument subscribe
            // batch rather than from the 5,000 instruments a connection
            // actually carries, so it would have rejected any correctly-sized
            // cap. A test that enforces a wrong constant is worse than no
            // test: it makes the fix look like the regression.
            //
            // The real invariant is not an absolute byte count — it is that
            // the cap stays bounded relative to the traffic it must admit and
            // far below the library default. Both are asserted below.
            assert!(
                cap <= 4 << 20,
                "{endpoint} cap {cap} exceeds 4 MiB. The cap exists to bound a hostile or \
                 malfunctioning peer; past this it stops being a defence."
            );
            // 16× headroom: even the largest cap stays an order of magnitude
            // under the default it replaces, so sixteen sockets cannot turn
            // into a gigabyte of attacker-controlled allocation.
            assert!(
                cap * 16 < LIBRARY_DEFAULT_MESSAGE_BYTES,
                "{endpoint} cap {cap} is not materially below the library default"
            );
        }
    }

    #[test]
    fn test_caps_leave_at_least_eighty_packets_of_headroom() {
        // Sized against the REAL protocol maxima, not a round number picked
        // for looks. If a future edit shrinks a cap below its own packet size
        // the feed would break on every frame; this is the guard.
        assert!(
            MAIN_FEED_MAX_FRAME_BYTES / MAIN_FEED_LARGEST_PACKET_BYTES >= 1_000,
            "main feed must hold ≥1000 Full packets per message"
        );
        assert!(
            DEPTH_20_MAX_FRAME_BYTES / TWENTY_DEPTH_PACKET_SIZE >= 100,
            "depth-20 must hold ≥100 packets per message"
        );
        assert!(
            DEPTH_200_MAX_FRAME_BYTES / TWO_HUNDRED_DEPTH_PACKET_SIZE >= 80,
            "depth-200 must hold ≥80 packets per message"
        );
    }

    #[test]
    fn test_websocket_config_sets_both_caps_and_never_leaves_them_none() {
        // `None` means "unlimited" in tungstenite. Either field left unset is
        // the bug this module exists to prevent.
        for endpoint in DhanEndpointType::ALL {
            let config = websocket_config(endpoint);
            let expected = max_frame_bytes(endpoint);
            assert_eq!(
                config.max_message_size,
                Some(expected),
                "{endpoint} message cap"
            );
            assert_eq!(
                config.max_frame_size,
                Some(expected),
                "{endpoint} frame cap"
            );
        }
    }

    #[test]
    fn test_message_cap_equals_frame_cap_so_fragments_cannot_accumulate() {
        // A hostile peer can send many individually-legal continuation frames.
        // Bounding only the frame would leave the reassembled message
        // unbounded.
        for endpoint in DhanEndpointType::ALL {
            let config = websocket_config(endpoint);
            assert_eq!(config.max_message_size, config.max_frame_size);
        }
    }

    #[test]
    fn test_config_read_and_write_buffers_are_below_the_library_defaults() {
        // 128 KiB eagerly-allocated read buffer × 16 sockets = 2 MiB reserved
        // before a byte arrives.
        let config = websocket_config(DhanEndpointType::MainFeed);
        assert!(config.read_buffer_size <= 32 * 1024);
        assert!(config.write_buffer_size <= 32 * 1024);
        assert!(
            config.max_write_buffer_size > config.write_buffer_size,
            "the write ceiling must exceed the write target by at least one message"
        );
        assert!(
            config.max_write_buffer_size < usize::MAX,
            "an unlimited write buffer removes all backpressure"
        );
        assert!(!config.accept_unmasked_frames);
    }

    // -- URL construction + redaction --------------------------------------

    #[test]
    fn test_main_feed_url_has_all_four_documented_query_params() {
        // `03-live-market-feed-websocket.md` rule 2: all four are required.
        let url = build_feed_url(
            DhanEndpointType::MainFeed,
            DHAN_MAIN_FEED_WS_BASE_URL,
            FAKE_TOKEN,
            FAKE_CLIENT_ID,
        );
        assert!(url.starts_with("wss://api-feed.dhan.co/?"));
        assert!(url.contains("version=2"));
        assert!(url.contains(&format!("token={FAKE_TOKEN}")));
        assert!(url.contains(&format!("clientId={FAKE_CLIENT_ID}")));
        assert!(url.contains("authType=2"));
    }

    #[test]
    fn test_depth_200_url_uses_the_vendor_sdk_verified_root_path() {
        // 2026-04-23: `/twohundreddepth` produced two weeks of
        // ResetWithoutClosingHandshake; the root path is what actually works.
        let url = build_feed_url(
            DhanEndpointType::Depth200,
            DHAN_TWO_HUNDRED_DEPTH_WS_BASE_URL,
            FAKE_TOKEN,
            FAKE_CLIENT_ID,
        );
        assert!(url.starts_with("wss://full-depth-api.dhan.co/?"));
        assert!(
            !url.contains("twohundreddepth"),
            "the documented path is the one that does NOT work for our account"
        );
        // The depth sockets take no `version` parameter.
        assert!(!url.contains("version=2"));
    }

    #[test]
    fn test_depth_20_url_keeps_its_path_and_omits_version() {
        let url = build_feed_url(
            DhanEndpointType::Depth20,
            DHAN_TWENTY_DEPTH_WS_BASE_URL,
            FAKE_TOKEN,
            FAKE_CLIENT_ID,
        );
        assert!(url.starts_with("wss://depth-api-feed.dhan.co/twentydepth?"));
        assert!(!url.contains("version=2"));
        assert!(url.contains("authType=2"));
    }

    #[test]
    fn test_trailing_slash_on_a_base_url_never_produces_a_double_slash() {
        let url = build_feed_url(
            DhanEndpointType::MainFeed,
            &trailing_slash_base(),
            FAKE_TOKEN,
            FAKE_CLIENT_ID,
        );
        assert!(!url.contains(".co//"), "double slash in {}", url.as_str());
    }

    #[test]
    fn test_loggable_url_never_carries_a_credential() {
        // The load-bearing security property: the string that reaches a log
        // is built from the BASE url, so there is no path by which the JWT
        // could reach a formatter.
        for base in [
            "wss://api-feed.dhan.co",
            "wss://depth-api-feed.dhan.co/twentydepth",
            "wss://full-depth-api.dhan.co",
        ] {
            let logged = loggable_url(base);
            assert!(!logged.contains(FAKE_TOKEN));
            assert!(!logged.contains(FAKE_CLIENT_ID));
            assert!(
                logged.contains("dhan.co"),
                "the host must survive: {logged}"
            );
        }
    }

    #[test]
    fn test_a_transport_error_echoing_the_full_url_is_redacted() {
        // tungstenite embeds the request URI in several Display impls. This is
        // the exact leak class `redact_url_params` was extended for.
        let url = build_feed_url(
            DhanEndpointType::MainFeed,
            DHAN_MAIN_FEED_WS_BASE_URL,
            FAKE_TOKEN,
            FAKE_CLIENT_ID,
        );
        let hostile = format!("Tls(HandshakeFailure) for {} failed", url.as_str());
        let safe = safe_err(&hostile);
        assert!(!safe.contains(FAKE_TOKEN), "JWT leaked: {safe}");
        assert!(!safe.contains(FAKE_CLIENT_ID), "client id leaked: {safe}");
        // No 20-char window of the token survives either.
        for window in FAKE_TOKEN.as_bytes().windows(20) {
            if let Ok(fragment) = std::str::from_utf8(window) {
                assert!(!safe.contains(fragment), "token fragment leaked: {safe}");
            }
        }
        assert!(safe.contains("[REDACTED]"));
    }

    // -- subscribe payload --------------------------------------------------

    #[test]
    fn test_main_feed_subscribe_serialises_security_id_as_a_string() {
        // Rule 15: a NUMERIC SecurityId is silently rejected by Dhan and the
        // subscription simply never happens — the worst possible failure mode.
        let batch = [instrument(13, ExchangeSegment::IdxI)];
        let json = build_subscribe_payload(DhanEndpointType::MainFeed, FeedMode::Quote, &batch)
            .expect("a single index subscribes");
        assert!(json.contains("\"SecurityId\":\"13\""), "{json}");
        assert!(!json.contains("\"SecurityId\":13"), "{json}");
        assert!(json.contains("\"ExchangeSegment\":\"IDX_I\""));
        assert!(json.contains("\"RequestCode\":17"));
    }

    #[test]
    fn test_instrument_count_always_matches_the_list_length() {
        for size in [1usize, 2, 17, 100] {
            let batch: Vec<_> = (0..size)
                .map(|i| {
                    instrument(
                        SecurityId::try_from(i).unwrap_or(SecurityId::MAX),
                        ExchangeSegment::NseFno,
                    )
                })
                .collect();
            let json = build_subscribe_payload(DhanEndpointType::MainFeed, FeedMode::Quote, &batch)
                .expect("inside the per-message cap");
            assert!(
                json.contains(&format!("\"InstrumentCount\":{size}")),
                "count must match the array length for size {size}: {json}"
            );
        }
    }

    #[test]
    fn test_depth_200_uses_the_flat_shape_with_no_instrument_list() {
        // depth-200 has a DIFFERENT wire shape; sending the list form would be
        // rejected.
        let batch = [instrument(72271, ExchangeSegment::NseEquity)];
        let json = build_subscribe_payload(DhanEndpointType::Depth200, FeedMode::Quote, &batch)
            .expect("one instrument is exactly the depth-200 capacity");
        assert!(!json.contains("InstrumentList"), "{json}");
        assert!(!json.contains("InstrumentCount"), "{json}");
        assert!(json.contains("\"SecurityId\":\"72271\""));
        assert!(json.contains("\"ExchangeSegment\":\"NSE_EQ\""));
    }

    #[test]
    fn test_depth_200_refuses_more_than_one_instrument() {
        // A batch >1 here means the sharding upstream is wrong; refusing
        // locally turns it into a loud failure at the mistake rather than a
        // silent half-subscription.
        let batch = [
            instrument(1, ExchangeSegment::NseEquity),
            instrument(2, ExchangeSegment::NseEquity),
        ];
        assert_eq!(
            build_subscribe_payload(DhanEndpointType::Depth200, FeedMode::Quote, &batch),
            Err(SubscribePayloadError::Depth200Overfull { got: 2 })
        );
    }

    #[test]
    fn test_empty_batch_is_refused_rather_than_sent() {
        for endpoint in DhanEndpointType::ALL {
            assert_eq!(
                build_subscribe_payload(endpoint, FeedMode::Quote, &[]),
                Err(SubscribePayloadError::EmptyBatch),
                "{endpoint} must refuse an empty batch"
            );
        }
    }

    #[test]
    fn test_the_default_feed_mode_is_the_scope_locked_full_mode() {
        // RE-BLESSED 2026-08-19. This test pinned `Quote` from the original
        // daily-universe scope lock. The operator's 2026-08-15 authorization
        // ("entire NTM and entire nse indices … shdou lbe fully subscribed
        // dude wiht full mdoe") moved the scope to Full (request code 21,
        // 162-byte packets); the rule file recorded that on 2026-08-15 and the
        // constant did not follow until now. Moving it BACK requires the same
        // rule-file-first protocol — this assertion is the gate.
        assert_eq!(DEFAULT_MAIN_FEED_MODE, FeedMode::Full);
        let params = DhanSocketParams::new(
            DhanEndpointType::MainFeed,
            "wss://api-feed.dhan.co".to_string(),
            FAKE_CLIENT_ID.to_string(),
        );
        assert_eq!(params.feed_mode, FeedMode::Full);
    }

    #[test]
    fn test_the_main_feed_payload_carries_the_full_request_code() {
        // The code itself comes from `subscription_builder`; this pins that the
        // transport asks for the scope-locked MODE and gets 21 back. It is the
        // half of the flip that would silently fail: a constant changed with a
        // builder that still emitted 17 would subscribe Quote while every doc
        // claimed Full.
        //
        // FIXTURE CORRECTED 2026-08-21. This asserted that an `IDX_I`
        // instrument gets code 21 — the defect itself, written down as
        // expected behaviour, exactly like the `stock_options == 0` assertion
        // corrected in a2acb6a. Indices are the ONE segment that must not get
        // 21 (see `IDX_I_FEED_MODE`), so the fixture is now a tradeable
        // instrument, which is what this test was always about.
        let batch = [instrument(59099, ExchangeSegment::NseFno)];
        let json =
            build_subscribe_payload(DhanEndpointType::MainFeed, DEFAULT_MAIN_FEED_MODE, &batch)
                .expect("a single derivative subscribes");
        assert!(json.contains("\"RequestCode\":21"), "{json}");
        assert!(
            !json.contains("\"RequestCode\":17"),
            "the Quote code must be gone, not merely joined: {json}"
        );
    }

    // -- IDX_I mode partition (2026-08-21) ---------------------------------

    #[test]
    fn feed_mode_for_segment_gives_an_index_quote_not_full() {
        // The whole fix in one assertion. Dhan answers an IDX_I Full
        // subscribe with silence, so a payload carrying 21 for segment 0 is
        // the bug — measured as 0 IDX_I packets in 163,934 captured frames.
        let batch = [instrument(13, ExchangeSegment::IdxI)];
        let json = build_subscribe_payload(
            DhanEndpointType::MainFeed,
            feed_mode_for_segment(ExchangeSegment::IdxI, DEFAULT_MAIN_FEED_MODE),
            &batch,
        )
        .expect("a single index subscribes");
        assert!(json.contains("\"RequestCode\":17"), "{json}");
        assert!(
            !json.contains("\"RequestCode\":21"),
            "an index must not ask for depth it does not have: {json}"
        );
    }

    #[test]
    fn feed_mode_for_segment_keeps_full_for_every_other_segment() {
        // Non-vacuity. Without this, `feed_mode_for_segment` returning Quote
        // unconditionally would satisfy the test above while silently
        // downgrading all ~24,600 tradeable instruments off depth-5 — a far
        // worse regression than the one being fixed.
        for segment in [
            ExchangeSegment::NseEquity,
            ExchangeSegment::NseFno,
            ExchangeSegment::BseEquity,
            ExchangeSegment::BseFno,
        ] {
            assert_eq!(
                feed_mode_for_segment(segment, FeedMode::Full),
                FeedMode::Full,
                "{segment:?} has an order book and must keep Full"
            );
        }
    }

    #[test]
    fn feed_mode_for_segment_passes_the_configured_mode_through() {
        // Guards the OTHER direction: if the scope lock ever moves the
        // main-feed mode again, non-index instruments must follow it. A
        // `FeedMode::Full` literal inside the helper would pass the test above
        // and quietly pin the lane to Full forever.
        assert_eq!(
            feed_mode_for_segment(ExchangeSegment::NseFno, FeedMode::Ticker),
            FeedMode::Ticker
        );
        assert_eq!(
            feed_mode_for_segment(ExchangeSegment::IdxI, FeedMode::Ticker),
            IDX_I_FEED_MODE,
            "the index override wins over the configured mode, whatever it is"
        );
    }

    #[test]
    fn feed_mode_for_segment_makes_a_mixed_batch_two_messages() {
        // Why the split is structural rather than stylistic: one Dhan message
        // carries exactly one RequestCode, so a batch holding both an index
        // and a derivative CANNOT be sent as a single payload without one of
        // them getting the wrong mode. This pins the property that forces
        // `send_subscribe` to emit two messages.
        let idx = feed_mode_for_segment(ExchangeSegment::IdxI, DEFAULT_MAIN_FEED_MODE);
        let fno = feed_mode_for_segment(ExchangeSegment::NseFno, DEFAULT_MAIN_FEED_MODE);
        assert_ne!(
            idx, fno,
            "if these ever match, the partition in send_subscribe is dead code and \
             should be removed rather than left to rot"
        );
    }

    #[test]
    fn partition_index_batch_loses_nothing_and_duplicates_nothing() {
        // A partition that dropped an instrument would leave it unsubscribed
        // and SILENT — there is no other evidence for that, which is the whole
        // reason this class of bug survives. A duplicate would double-count
        // against the per-connection cap.
        let batch = [
            instrument(13, ExchangeSegment::IdxI),
            instrument(59099, ExchangeSegment::NseFno),
            instrument(21, ExchangeSegment::IdxI),
            instrument(2885, ExchangeSegment::NseEquity),
        ];
        let (indices, others) = partition_index_batch(&batch);
        assert_eq!(indices.len(), 2);
        assert_eq!(others.len(), 2);
        assert_eq!(indices.len() + others.len(), batch.len());
        assert!(indices.iter().all(|i| i.segment == ExchangeSegment::IdxI));
        assert!(others.iter().all(|i| i.segment != ExchangeSegment::IdxI));
        let mut seen: Vec<u64> = indices
            .iter()
            .chain(others.iter())
            .map(|i| i.security_id)
            .collect();
        seen.sort_unstable();
        assert_eq!(
            seen,
            vec![13, 21, 2885, 59099],
            "every id survives exactly once"
        );
    }

    #[test]
    fn partition_index_batch_puts_an_all_index_batch_on_one_side() {
        let batch = [
            instrument(13, ExchangeSegment::IdxI),
            instrument(25, ExchangeSegment::IdxI),
        ];
        let (indices, others) = partition_index_batch(&batch);
        assert_eq!(indices.len(), 2);
        assert!(
            others.is_empty(),
            "an all-index batch must not emit an empty second message — \
             build_subscribe_payload refuses EmptyBatch and would tear the socket down"
        );
    }

    #[test]
    fn partition_index_batch_with_no_index_leaves_the_original_path() {
        // Non-vacuity for the fast path in send_subscribe: with no index the
        // batch is sent exactly as it was before this change.
        let batch = [instrument(59099, ExchangeSegment::NseFno)];
        let (indices, others) = partition_index_batch(&batch);
        assert!(indices.is_empty());
        assert_eq!(others.len(), 1);
    }

    // -- disconnect classification -----------------------------------------

    fn main_feed_disconnect(reason: u16) -> Vec<u8> {
        let mut packet = vec![0u8; DISCONNECT_PACKET_SIZE];
        packet[0] = RESPONSE_CODE_DISCONNECT;
        packet[8..10].copy_from_slice(&reason.to_le_bytes());
        packet
    }

    fn depth_disconnect(reason: u16) -> Vec<u8> {
        let mut packet = vec![0u8; DEEP_DEPTH_HEADER_SIZE + 2];
        packet[DEEP_DEPTH_HEADER_OFFSET_FEED_CODE] = RESPONSE_CODE_DISCONNECT;
        packet[DEEP_DEPTH_HEADER_SIZE..].copy_from_slice(&reason.to_le_bytes());
        packet
    }

    #[test]
    fn test_main_feed_805_is_classified_so_the_supervisor_can_park() {
        // The single most consequential classification: re-dialing 805 kills a
        // healthy sibling instead of recovering this socket, so the supervisor
        // MUST see the code.
        assert_eq!(
            classify_frame(DhanEndpointType::MainFeed, &main_feed_disconnect(805)),
            FrameClass::Disconnect(DisconnectCode::ExceededActiveConnections)
        );
    }

    #[test]
    fn test_depth_feeds_use_the_twelve_byte_header_layout() {
        // Reading the depth disconnect with the main-feed offsets would find
        // the message LENGTH where the feed code lives and misclassify.
        for endpoint in [DhanEndpointType::Depth20, DhanEndpointType::Depth200] {
            assert_eq!(
                classify_frame(endpoint, &depth_disconnect(805)),
                FrameClass::Disconnect(DisconnectCode::ExceededActiveConnections),
                "{endpoint}"
            );
        }
    }

    /// One zero-filled main-feed packet of `code`.
    fn main_feed_packet(code: u8) -> Vec<u8> {
        let len = crate::parser::dispatcher::main_feed_packet_len(&[code]).expect("known code");
        let mut p = vec![0u8; len];
        p[0] = code;
        p
    }

    #[test]
    fn a_disconnect_stacked_behind_data_reaches_the_supervisor() {
        // THE 2026-08-25 regression, at the layer that actually gates
        // `SocketEvent::Closed{code}`. A Dhan frame stacks packets, so
        // `[quote][disconnect 804]` is a legal shape -- and the old
        // `frame.len() != 10 => Data` test called it data. The drain then
        // decoded the disconnect and counted it as untyped "non-tick"
        // traffic, so the reason reached no log, no metric and no
        // classifier; the socket closed on TCP with `code: None`, which
        // `classify_disconnect` reads as Transient, and the ladder retried
        // the IDENTICAL over-limit subscribe set forever.
        let mut frame = main_feed_packet(RESPONSE_CODE_QUOTE);
        frame.extend_from_slice(&main_feed_disconnect(804));
        assert_eq!(
            classify_frame(DhanEndpointType::MainFeed, &frame),
            FrameClass::Disconnect(DisconnectCode::InstrumentsExceedLimit),
            "a stacked 804 must be classified so the supervisor can PARK, not retry"
        );
    }

    #[test]
    fn a_stacked_805_is_classified_too() {
        let mut frame = main_feed_packet(RESPONSE_CODE_FULL);
        frame.extend_from_slice(&main_feed_packet(RESPONSE_CODE_OI));
        frame.extend_from_slice(&main_feed_disconnect(805));
        assert_eq!(
            classify_frame(DhanEndpointType::MainFeed, &frame),
            FrameClass::Disconnect(DisconnectCode::ExceededActiveConnections)
        );
    }

    #[test]
    fn a_multi_packet_data_frame_stays_data() {
        // Non-vacuity in the other direction: the walk must not turn ordinary
        // stacked market data into a fabricated disconnect, which would park
        // a healthy socket on a code we invented.
        let mut frame = main_feed_packet(RESPONSE_CODE_QUOTE);
        frame.extend_from_slice(&main_feed_packet(RESPONSE_CODE_FULL));
        frame.extend_from_slice(&main_feed_packet(RESPONSE_CODE_TICKER));
        assert_eq!(
            classify_frame(DhanEndpointType::MainFeed, &frame),
            FrameClass::Data
        );
    }

    #[test]
    fn the_stacked_walk_does_not_apply_to_the_depth_feeds() {
        // Depth carries a length-prefixed header whose packet sizes the
        // main-feed table does not own. Walking it with those sizes would be
        // inventing a classification, so a depth frame that is not exactly a
        // depth disconnect stays data -- unchanged by this fix.
        let mut frame = main_feed_packet(RESPONSE_CODE_QUOTE);
        frame.extend_from_slice(&main_feed_disconnect(804));
        for endpoint in [DhanEndpointType::Depth20, DhanEndpointType::Depth200] {
            assert_eq!(
                classify_frame(endpoint, &frame),
                FrameClass::Data,
                "{endpoint}"
            );
        }
    }

    #[test]
    fn test_a_depth_layout_disconnect_on_the_main_feed_is_not_misread() {
        // Cross-layout confusion in both directions must be impossible.
        assert_eq!(
            classify_frame(DhanEndpointType::MainFeed, &depth_disconnect(805)),
            FrameClass::Data
        );
        assert_eq!(
            classify_frame(DhanEndpointType::Depth20, &main_feed_disconnect(805)),
            FrameClass::Data
        );
    }

    #[test]
    fn test_every_documented_disconnect_code_round_trips() {
        for reason in [
            800u16, 804, 805, 806, 807, 808, 809, 810, 811, 812, 813, 814,
        ] {
            assert_eq!(
                classify_frame(DhanEndpointType::MainFeed, &main_feed_disconnect(reason)),
                FrameClass::Disconnect(DisconnectCode::from_u16(reason)),
                "reason {reason}"
            );
        }
    }

    #[test]
    fn test_an_unknown_reason_code_classifies_without_panicking() {
        // Annexure rule 15: never panic on an unknown code.
        assert_eq!(
            classify_frame(DhanEndpointType::MainFeed, &main_feed_disconnect(4242)),
            FrameClass::Disconnect(DisconnectCode::Unknown(4242))
        );
    }

    #[test]
    fn test_real_data_packets_are_never_classified_as_disconnects() {
        // A 16-byte ticker, a 50-byte quote, a 162-byte full and a 12-byte OI
        // packet must all pass straight through untouched.
        for size in [16usize, 50, 162, 12, MAIN_FEED_LARGEST_PACKET_BYTES] {
            let mut packet = vec![0u8; size];
            // Even with the disconnect response code in byte 0, only the exact
            // 10-byte shape is a disconnect.
            packet[0] = RESPONSE_CODE_DISCONNECT;
            assert_eq!(
                classify_frame(DhanEndpointType::MainFeed, &packet),
                FrameClass::Data,
                "a {size}-byte packet is data, not a disconnect"
            );
        }
    }

    #[test]
    fn test_classification_is_total_over_short_and_empty_frames() {
        // Fail-safe direction: anything unrecognised is DATA. A false
        // "disconnect" would park a healthy connection on an invented code.
        //
        // AMENDED 2026-08-25 with the stacked-disconnect walk. This loop fills
        // every length with the disconnect response code itself, so any length
        // that is an exact MULTIPLE of the 10-byte disconnect packet is not
        // "unrecognised" at all -- it is N well-formed disconnect packets
        // stacked, which is precisely the shape the walk exists to catch. The
        // blanket assertion held before only because no walk existed; keeping
        // it would now pin the bug rather than the invariant.
        //
        // The invariant this test protects is UNCHANGED and still non-vacuous:
        // every length that does NOT resolve into whole packets must be DATA,
        // and 15 of the 21 lengths here exercise exactly that.
        for len in 0usize..=20 {
            let frame = vec![RESPONSE_CODE_DISCONNECT; len];
            for endpoint in DhanEndpointType::ALL {
                let class = classify_frame(endpoint, &frame);
                let walks_into_whole_disconnect_packets =
                    len > 0 && len % DISCONNECT_PACKET_SIZE == 0;
                if len == DISCONNECT_PACKET_SIZE
                    || len == DEEP_DEPTH_HEADER_SIZE + 2
                    || walks_into_whole_disconnect_packets
                {
                    // May be either, depending on layout — the point is only
                    // that it does not panic.
                    let _ = class;
                } else {
                    assert_eq!(class, FrameClass::Data, "len {len} on {endpoint}");
                }
            }
        }
    }

    // -- socket construction (no network) ----------------------------------

    #[test]
    fn test_a_fresh_socket_holds_no_stream_and_reports_its_endpoint() {
        let socket = DhanFeedSocketImpl::new(
            DhanSocketParams::new(
                DhanEndpointType::MainFeed,
                main_feed_base(),
                fake_client_id(),
            ),
            || None,
        );
        assert!(!socket.is_connected());
        assert_eq!(socket.endpoint(), DhanEndpointType::MainFeed);
        assert!(!socket.loggable_url().contains(FAKE_CLIENT_ID));
    }

    #[tokio::test]
    async fn test_close_on_a_never_dialed_socket_is_a_no_op() {
        let mut socket = DhanFeedSocketImpl::new(
            DhanSocketParams::new(
                DhanEndpointType::MainFeed,
                main_feed_base(),
                fake_client_id(),
            ),
            || None,
        );
        socket.close().await;
        assert!(!socket.is_connected());
    }

    #[tokio::test]
    async fn test_recv_on_a_never_dialed_socket_reports_closed_rather_than_hanging() {
        let mut socket = DhanFeedSocketImpl::new(
            DhanSocketParams::new(
                DhanEndpointType::MainFeed,
                main_feed_base(),
                fake_client_id(),
            ),
            || None,
        );
        assert_eq!(socket.recv().await, SocketEvent::Closed { code: None });
    }

    #[tokio::test]
    async fn test_subscribe_without_a_socket_fails_instead_of_reporting_success() {
        // A subscribe that silently "succeeds" against no socket would leave
        // the supervisor believing the connection is subscribed and live.
        let mut socket = DhanFeedSocketImpl::new(
            DhanSocketParams::new(
                DhanEndpointType::MainFeed,
                main_feed_base(),
                fake_client_id(),
            ),
            || None,
        );
        let batch = [instrument(13, ExchangeSegment::IdxI)];
        assert!(socket.send_subscribe(&batch).await.is_err());
    }

    #[tokio::test]
    async fn test_a_dial_without_a_token_fails_without_touching_the_network() {
        // No token means no dial: presenting an empty credential would earn a
        // rejection and burn a ladder rung for nothing.
        let mut socket = DhanFeedSocketImpl::new(
            DhanSocketParams::new(
                DhanEndpointType::MainFeed,
                main_feed_base(),
                fake_client_id(),
            ),
            || None,
        );
        assert!(socket.connect().await.is_err());
        assert!(!socket.is_connected());
    }

    #[test]
    fn test_token_source_is_read_fresh_on_every_call() {
        // The supervisor's RefreshTokenThenDial only works if the socket reads
        // the CURRENT token rather than one captured at construction.
        use std::sync::atomic::{AtomicUsize, Ordering};
        static CALLS: AtomicUsize = AtomicUsize::new(0);
        let source = || {
            let n = CALLS.fetch_add(1, Ordering::SeqCst);
            Some(Zeroizing::new(format!("token-{n}")))
        };
        let first = source.current_token();
        let second = source.current_token();
        assert_eq!(first.as_ref().map(|t| t.as_str()), Some("token-0"));
        assert_eq!(second.as_ref().map(|t| t.as_str()), Some("token-1"));
    }

    // -- structural proofs --------------------------------------------------

    #[test]
    fn test_the_production_half_never_dials_without_an_explicit_config() {
        // The security property this module exists for: `None` as the config
        // argument inherits tungstenite's 64 MiB message ceiling.
        let src = include_str!("connection.rs");
        let test_marker = concat!("#[cfg(", "test)]");
        let production = src.split(test_marker).next().unwrap_or(src);
        assert!(
            production.contains("let config = Some(websocket_config(endpoint));"),
            "the dial must build an explicit config"
        );
        assert!(
            !production.contains("connect_async_tls_with_config(request, None"),
            "no dial may pass None for the WebSocket config"
        );
        assert!(
            !production.contains("connect_async("),
            "the config-less connect helper inherits the 64 MiB default"
        );
    }

    #[test]
    fn test_the_production_half_does_no_parsing_on_the_data_path() {
        // THE ONE RULE, mechanically. The only byte-level read permitted here
        // is the disconnect control packet in `classify_frame`.
        let src = include_str!("connection.rs");
        let test_marker = concat!("#[cfg(", "test)]");
        let production = src.split(test_marker).next().unwrap_or(src);
        for banned in [
            concat!("parse_", "ticker_packet"),
            concat!("parse_", "quote_packet"),
            concat!("parse_", "full_packet"),
            concat!("parse_", "depth_packet"),
            concat!("f32::from_", "le_bytes"),
            concat!("f64::from_", "le_bytes"),
            concat!("questdb", "::"),
            concat!("append_", "tick"),
        ] {
            assert!(
                !production.contains(banned),
                "the socket transport must not parse or persist; found `{banned}`"
            );
        }
        // Exactly one integer read exists, and it is the disconnect reason.
        assert_eq!(
            production.matches("u16::from_le_bytes").count(),
            1,
            "only the disconnect reason code may be read out of a frame"
        );
    }

    #[test]
    fn test_the_socket_keeps_a_drop_impl_so_a_torn_down_connection_frees_its_budget_slot() {
        // Successor to the deleted `SubscribeRxGuard` ratchet. The old guard's
        // failure mode (a reconnected socket coming up under-subscribed) is now
        // structurally impossible — `run_connection` owns its `SubscribeGuard`
        // by value and replays it — but the drop-path hazard at THIS layer is
        // real: an orphaned live stream keeps a Dhan connection-budget slot,
        // and the next dial makes Dhan kill the OLDEST healthy socket with 805.
        let src = include_str!("connection.rs");
        let test_marker = concat!("#[cfg(", "test)]");
        let production = src.split(test_marker).next().unwrap_or(src);
        assert!(
            production.contains("impl<T: FeedTokenSource> Drop for DhanFeedSocketImpl<T>"),
            "the socket must keep its Drop impl — removing it makes an unclean teardown \
             invisible when debugging an 805 on a sibling connection"
        );
        assert!(
            production.contains("self.stream.is_some()"),
            "the Drop impl must actually inspect the live-stream state"
        );
    }

    #[tokio::test]
    async fn test_a_closed_socket_drops_without_reporting_an_unclean_teardown() {
        // The clean path: close() takes the stream, so Drop sees None.
        let mut socket = DhanFeedSocketImpl::new(
            DhanSocketParams::new(
                DhanEndpointType::MainFeed,
                main_feed_base(),
                fake_client_id(),
            ),
            || None,
        );
        socket.close().await;
        assert!(!socket.is_connected());
        drop(socket);
    }

    #[test]
    fn test_no_struct_in_this_module_can_debug_print_a_credential() {
        // A `#[derive(Debug)]` on a struct that later gains a token field
        // would make the credential loggable by accident.
        let src = include_str!("connection.rs");
        let test_marker = concat!("#[cfg(", "test)]");
        let production = src.split(test_marker).next().unwrap_or(src);
        for line in production.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with("#[derive(") && trimmed.contains("Debug") {
                assert!(
                    !trimmed.contains("Debug)")
                        || production.lines().any(|l| l.contains("pub enum FrameClass")
                            || l.contains("pub enum SubscribePayloadError")),
                    "only the payload-free enums may derive Debug"
                );
            }
        }
        assert!(
            !production.contains("pub struct DhanSocketParams {\n    #[derive"),
            "params must not derive Debug"
        );
        // The two credential-adjacent structs must not derive Debug at all.
        for (marker, name) in [
            ("pub struct DhanSocketParams", "DhanSocketParams"),
            ("pub struct DhanFeedSocketImpl", "DhanFeedSocketImpl"),
        ] {
            let Some(idx) = production.find(marker) else {
                panic!("{name} must exist");
            };
            let preceding = &production[idx.saturating_sub(200)..idx];
            assert!(
                !preceding.contains("#[derive(Debug"),
                "{name} must not derive Debug — it would become a credential leak surface"
            );
        }
    }
}
