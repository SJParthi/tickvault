//! Dhan connection-pool supervisor — pure decision core plus a thin async shell.
//!
//! Part of the 16-connection revival authorized by the operator on 2026-08-09
//! (`.claude/rules/project/websocket-connection-scope-lock.md`, section
//! "2026-08-09 (SAME DAY, SECOND QUOTE) — 16 CONNECTIONS + depth-20/depth-200
//! AUTHORIZED"). The connection layer was deleted 2026-07-17 with the retired
//! lane; this module rebuilds the *supervision* half of it on top of the three
//! pure modules that landed first — [`super::reconnect_ladder`],
//! [`super::idle_watchdog`] and [`super::pool_budget`] — which it REUSES rather
//! than re-deriving.
//!
//! # THE ONE RULE
//! The socket read task does exactly two things: append the raw frame to the
//! write-ahead log, and push it into the bounded ring. Nothing else, ever. No
//! parsing, no aggregation, no database write, no lock shared with a writer, no
//! await on network or disk.
//!
//! This is not a style preference, it is the disconnect-prevention mechanism.
//! `15-live-market-feed.md:75`: "An automated pong is sent by websocket
//! library." The library can only emit that pong **while the read loop is
//! polling the socket**, and `:77` gives the server-side death sentence at
//! "more than 40 seconds" of client silence. A reader that blocks to do work
//! therefore stops ponging while looking perfectly healthy locally, and Dhan
//! closes it. A slow consumer on this feed is not merely late — it is
//! disconnected.
//!
//! The rule is expressed in code as [`FrameSink`], whose ONLY implementation on
//! the production path is [`WalRingSink`]: one WAL append, one non-blocking
//! `try_send`, no `.await`. Everything downstream of the ring may stall, crash
//! or fall behind without the reader noticing.
//!
//! # Shape: pure core, thin shell
//! [`ConnectionSupervisor::on_event`] and [`ConnectionSupervisor::poll`] are
//! total, allocation-free, clock-injected functions from
//! `(state, event, now)` to [`SupervisorAction`]. Every reconnect, park and
//! resubscribe decision this product makes is reachable from a unit test
//! without opening a socket. [`run_connection`] is the shell that executes
//! those actions against a [`DhanFeedSocket`]; it contains no policy of its own.
//!
//! # Why the clock is monotonic everywhere
//! The idle watchdog is driven by [`std::time::Instant`] via
//! [`IdleWatchdog`], never by wall time. An NTP step of +30s against wall time
//! would push ALL sixteen sockets past the idle threshold in the same instant —
//! synthesising the exact simultaneous-drop thundering herd that
//! [`super::reconnect_ladder`]'s jitter exists to prevent, on sixteen sockets
//! that were perfectly healthy. This module never reads the wall clock; that is
//! enforced mechanically by `test_pool_supervisor_source_never_reads_wall_clock`.
//!
//! # Complexity
//! | Path | Cost | Note |
//! |---|---|---|
//! | [`ConnectionSupervisor::on_event`] | O(1), zero alloc | enum match + integer arithmetic |
//! | [`ConnectionSupervisor::poll`] | O(1), zero alloc | one `Instant` subtraction |
//! | [`classify_disconnect`] | O(1), zero alloc | enum match |
//! | [`WalRingSink::accept`] | O(1), zero alloc *of ours* | WAL append + `try_send`; see the honesty note below |
//! | [`SubscribeGuard::batches`] | O(1) per batch, zero alloc | slice `chunks`, no copy |
//! | [`SubscribeGuard`] full iteration | **O(n) in instruments — NOT O(1)** | inherent: every instrument must be named on the wire. Cold path: once per connect, ≤5,000 items, ~50 messages |
//! | [`PoolSupervisor::poll_all`] | **O(N) with N ≤ 16 — NOT O(1)** | bounded by the operator-authorized ceiling, so it is a fixed constant, but it is a scan and is labelled as one |
//! | [`PoolSupervisor::admit`] | O(1), zero alloc | delegates to the four-counter [`PoolBudget`] |
//!
//! **Honesty on "zero allocation on the per-frame path."** The claim is exact
//! for the code in this module: [`WalRingSink::accept`] performs no heap
//! allocation, and the `Bytes` hand-off into the WAL is an `Arc` refcount bump,
//! not a copy. It is NOT a claim about the whole read loop — `tokio-tungstenite`
//! allocates the frame buffer when it decodes a message, upstream of anything
//! here, and that allocation is not ours to remove. Per frame this module also
//! performs one `Instant::now()` (a ~20ns vDSO read) to feed the watchdog:
//! 0.03% of the 66.7µs per-packet budget on the busiest socket. Both costs are
//! recorded rather than rounded away.

use std::time::{Duration, Instant};

use bytes::Bytes;
use tickvault_common::error_code::ErrorCode;
use tickvault_common::types::{ExchangeSegment, SecurityId};
use tickvault_storage::ws_frame_spill::{AppendOutcome, WsFrameSpill, WsType, next_frame_seq};
use tracing::{error, info, warn};

use super::idle_watchdog::{IDLE_RECONNECT_TIMEOUT_SECS, IdleWatchdog};
use super::pool_budget::{
    ConnectionSlot, DhanEndpointType, MAX_TOTAL_DHAN_CONNECTIONS, PoolBudget, PoolBudgetRefusal,
};
use super::reconnect_ladder::{
    FLAP_DAMPED_METRIC, FLAP_WINDOW_MS, FlapVerdict, ReconnectDecision, damped_reconnect_delay,
    damped_reconnect_delay_with_jitter, reconnect_jitter_ms,
};
use super::types::{ConnectionId, ConnectionState, DisconnectCode};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Minimum delay before re-dialing after a token-staleness disconnect (807/809).
///
/// Deliberately far above the ladder's instant first rung. A stale token is not
/// a transient TCP event: re-dialing immediately presents the SAME dead token
/// and earns the same rejection, sixteen times over, which is precisely the
/// retry storm the house rules ban. This floor gives the REST stack's renewal
/// loop / mid-session watchdog room to publish a fresh token before we ask for
/// one again.
pub const TOKEN_STALE_REDIAL_FLOOR_MS: u64 = 5_000;

/// How often the shell wakes to ask the supervisor whether a socket has gone
/// idle. One second gives a 27–28s detection window against the 27s threshold,
/// comfortably inside Dhan's documented 40s server-side close, at a cost of one
/// timer wake per connection per second.
// APPROVED: this line IS the named constant the no-hardcoded-Duration rule asks for; the scanner matches the declaration itself.
pub const IDLE_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// How often a client-originated keepalive ping is sent, on the endpoints that
/// need one ([`DhanEndpointType::needs_client_keepalive_ping`]).
///
/// 10 seconds, matching `DHAN_SERVER_PING_INTERVAL_SECS` — the cadence Dhan
/// documents for its OWN server-side ping on the endpoints where it actually
/// sends one. Using their number rather than inventing one keeps a single
/// stated cadence across the pool.
///
/// The relationship that must hold is with the watchdog, and it is asserted
/// below: at 10s we get roughly **2.7 pings inside every 27-second window**,
/// so a single dropped ping or pong cannot expire a healthy socket. It also
/// keeps us well inside Dhan's documented 40-second client-silence close,
/// which on this endpoint we were previously relying on the watchdog to
/// pre-empt rather than avoid.
// APPROVED: this line IS the named constant the no-hardcoded-Duration rule asks for; the scanner matches the declaration itself. Same shape as `IDLE_POLL_INTERVAL` above.
pub const CLIENT_KEEPALIVE_PING_INTERVAL: Duration = Duration::from_secs(10);

const _: () = {
    assert!(
        CLIENT_KEEPALIVE_PING_INTERVAL.as_secs() * 2 < IDLE_RECONNECT_TIMEOUT_SECS,
        "at least two keepalive pings must fit inside the idle window, so one \
         lost ping or pong can never expire a healthy socket"
    );
    assert!(
        CLIENT_KEEPALIVE_PING_INTERVAL.as_secs() >= IDLE_POLL_INTERVAL.as_secs(),
        "the keepalive is driven by the idle ticker, so it can never be sent \
         more often than that ticker fires"
    );
};

/// Counter: connection dropped and is being re-dialed. Labels: `endpoint`, `reason`.
pub const RECONNECT_METRIC: &str = "tv_dhan_ws_reconnect_total";

/// Ring capacity for the per-socket re-dial timestamps the flap damper counts.
///
/// Fixed-size and inline: the damper runs on the disconnect path of a socket
/// that may be flapping thousands of times an hour, so it must not allocate.
/// Eight is comfortably above `FLAP_REDIAL_CEILING` (3) — the count only ever
/// needs to distinguish "below the ceiling" from "at or above it", so the ring
/// saturating at eight loses nothing, while the extra headroom keeps the count
/// honest if the ceiling is ever raised.
pub const FLAP_HISTORY_SLOTS: usize = 8;

/// Counter: connection parked permanently. Labels: `endpoint`, `reason`.
pub const PARK_METRIC: &str = "tv_dhan_ws_park_total";

/// Counter: frame captured but the bounded ring refused it — the frame is
/// durable in the WAL, the downstream consumer is behind. Label: `endpoint`.
pub const RING_FULL_METRIC: &str = "tv_dhan_ws_ring_full_total";

/// Counter: the ring refusal above was the BYTE budget, not the frame count.
///
/// A strict subset of `RING_FULL_METRIC` — every byte-refusal increments both,
/// so the ring-full alarm keeps working unchanged and this one answers the
/// follow-up question the operator will actually have: was the queue long, or
/// were the frames huge? Those have different causes (a stalled fold versus an
/// oversized or hostile peer) and, at the count bound alone, are
/// indistinguishable. Label: `endpoint`.
pub const RING_BYTES_FULL_METRIC: &str = "tv_dhan_ws_ring_bytes_full_total";

/// Counter: the WAL itself refused a frame. This is the only genuine capture
/// loss path. Label: `endpoint`.
pub const WAL_DROP_METRIC: &str = "tv_dhan_ws_wal_dropped_total";

// ---------------------------------------------------------------------------
// Subscribe dispatch pacing (WS-GAP-02)
// ---------------------------------------------------------------------------

/// Delay inserted BETWEEN consecutive subscribe messages on one connection.
///
/// # Why a paced dispatch exists at all
///
/// A throttled or rate-limited subscribe does not come back as an error. Dhan's
/// live-feed protocol has NO per-subscribe acknowledgement and NO sequence
/// number (`docs/dhan-ref/03-live-market-feed-websocket.md`), so a message the
/// vendor silently declines is indistinguishable from an instrument that simply
/// has not traded. Measured 2026-08-25: 21,498 instruments subscribed for depth,
/// 17,241 ever ticked — and nothing in the system can say which part of that gap
/// is illiquidity and which is a lost subscribe. Bursting 50 messages back to
/// back is therefore not merely impolite; it is the one failure mode this
/// codebase cannot observe after the fact.
///
/// # Where the number comes from — and what is NOT documented
///
/// Dhan documents the SIZE limits precisely: 5 connections, 5,000 instruments
/// per connection, and **100 instruments per JSON subscribe message**
/// (`03-live-market-feed-websocket.md` §"Connection Limits" and note 8;
/// depth-20 permits all 50 of its instruments in one message,
/// `04-full-market-depth-websocket.md`). Those are already enforced by
/// [`SubscribeGuard`].
///
/// **Dhan documents NO message RATE for the WebSocket.** The published
/// rate-limit table (`01-introduction-and-rate-limits.md` §3 — Order 10/s,
/// Data 5/s, Quote 1/s, Non-Trading 20/s) is explicitly a REST-API table and
/// says nothing about WS control frames. This interval is therefore CHOSEN, not
/// cited, and is justified below against the only hard constraint that exists —
/// the pre-open deadline. It is deliberately far slower than an unbounded burst
/// and far faster than the deadline requires.
///
/// # The deadline arithmetic (see `subscribe_dispatch_fits_the_preopen_budget`)
///
/// Worst case for the whole authorized pool, if every connection dispatched
/// SERIALLY (they do not — each runs in its own task):
///
/// | Endpoint | Instruments | Per message | Messages |
/// |---|---|---|---|
/// | main feed | 5 × 5,000 | 100 | 250 |
/// | depth-20  | 5 × 50    | 50  | 5   |
/// | depth-200 | 5 × 1     | 1   | 5   |
/// | **total** | | | **260** |
///
/// 260 messages × 25 ms ≈ **6.5 s**, against a pre-open attach budget of
/// **720 s** (09:00 → 09:12 IST). That is under 1 % of the budget in the
/// pessimistic serial case, and ~1.25 s per connection in the real concurrent
/// one. The pacing cannot be what misses the deadline.
// APPROVED: this line IS the named constant the no-hardcoded-Duration rule asks for; the scanner matches the declaration itself.
pub const SUBSCRIBE_BATCH_INTERVAL: Duration = Duration::from_millis(25);

/// Mirror of `tickvault_app::dhan_feed_stack::PREOPEN_READY_DEADLINE_IST_SECS`
/// (09:12:00 IST) — the moment the contract attach must be complete.
///
/// Duplicated rather than imported because the dependency flow is
/// `common ← core ← trading ← storage ← api ← app`: this crate cannot see the
/// app crate. The value is asserted literally in
/// `subscribe_dispatch_fits_the_preopen_budget` so a silent drift in either
/// copy shows up as a failing arithmetic test rather than as a missed deadline.
pub const PREOPEN_READY_DEADLINE_IST_SECS: u32 = 9 * 3_600 + 12 * 60;

/// Earliest IST second the pre-open attach can begin dialing — the 09:00
/// persistence window open. The span between this and
/// [`PREOPEN_READY_DEADLINE_IST_SECS`] is the whole budget the dispatch shares
/// with dialing, the pricing quorum, and contract selection.
pub const PREOPEN_ATTACH_WINDOW_OPEN_IST_SECS: u32 = 9 * 3_600;

/// Counter: one subscribe message was written to the socket. Labels:
/// `endpoint`.
///
/// # What this can and cannot prove
///
/// It counts messages this process WROTE, not messages Dhan ACCEPTED. The
/// protocol offers no acknowledgement, so an accepted-message counter cannot
/// honestly exist and is not invented here. What it does give is the missing
/// half of an existing question: compared against
/// [`SubscribeGuard::batch_count`] for the same connection, a shortfall means
/// instruments that were never even ASKED for — which is a different diagnosis
/// from "subscribed and never ticked", and until now the two were the same
/// number.
pub const SUBSCRIBE_BATCH_METRIC: &str = "tv_dhan_ws_subscribe_batches_total";

/// Counter: instruments covered by the subscribe messages actually written.
/// Labels: `endpoint`. The instrument-level companion to
/// [`SUBSCRIBE_BATCH_METRIC`], and the number that lines up directly against a
/// tick-gap detector's seeded set.
pub const SUBSCRIBE_INSTRUMENTS_METRIC: &str = "tv_dhan_ws_subscribe_instruments_total";

/// Counter: a subscribe dispatch stopped part-way — a message failed to write
/// and every batch after it was abandoned. Labels: `endpoint`.
///
/// Loss-shaped by name, and genuinely loss-shaped in effect: the instruments in
/// the abandoned tail are not subscribed, and without this the only trace was a
/// per-message `warn!` in `connection.rs` that says nothing about how much of
/// the set never went out.
pub const SUBSCRIBE_DISPATCH_FAILED_METRIC: &str = "tv_dhan_ws_subscribe_dispatch_failed_total";

/// Gauge: wall-clock milliseconds the last full subscribe dispatch took on this
/// connection. Labels: `endpoint`. Exists so the 09:12 margin is WATCHED rather
/// than assumed — the arithmetic above is a bound, this is the measurement.
pub const SUBSCRIBE_DISPATCH_MS_METRIC: &str = "tv_dhan_ws_subscribe_dispatch_ms";

/// Counter: instruments the guard REFUSED to put on the wire because the
/// connection would then have carried the same one twice. Labels: `endpoint`,
/// `site` (`new` | `extend` | `swap`).
///
/// This one names an 804. Dhan answers a duplicate subscribe with error code
/// 804, which `classify_disconnect` files as Fatal: the socket closes and this
/// supervisor parks it for the session rather than redialling into the same
/// refusal. So a single repeated instrument in ONE batch does not degrade a
/// connection, it ends it — and every instrument that connection was carrying
/// goes dark with it.
///
/// A non-zero value is therefore a PRODUCER defect that was caught rather than
/// suffered: some caller built a set with a repeat in it. The instrument is
/// named in the log line beside this.
pub const SUBSCRIBE_DUPLICATE_METRIC: &str = "tv_dhan_ws_subscribe_duplicate_total";

// ---------------------------------------------------------------------------
// Disconnect classification (WS-GAP-01)
// ---------------------------------------------------------------------------

/// What a disconnect means for the reconnect decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisconnectClass {
    /// Network reset, server hiccup, or an unrecognised code. Retry on the ladder.
    Transient,
    /// The access token is expired or invalid (807 / 809). Retry, but only
    /// after obtaining a fresh token and only past [`TOKEN_STALE_REDIAL_FLOOR_MS`].
    TokenStale,
    /// 805 — too many connections. **Park.**
    ///
    /// `15-live-market-feed.md:209` and `16-full-market-depth.md:183`, verbatim:
    /// "If more than 5 websockets are established, then the first socket will be
    /// disconnected with `805` with every additional connection." Exceeding the
    /// cap does not reject the NEW socket, it kills the OLDEST one — so a
    /// reconnect loop against 805 does not retry a failure, it executes one
    /// healthy fully-subscribed pool member per attempt. Re-dialing here makes
    /// the situation strictly worse, which is why this is the one transport
    /// error that must stop rather than back off.
    PoolOverflow,
    /// Entitlement or credential errors that never self-heal without operator
    /// action (806 data-API not subscribed, 808 auth failed, 810 client id
    /// invalid). Park; a human must fix the account or the config.
    Fatal,
}

/// Classifies a disconnect code into a reconnect policy. Total: an absent or
/// unknown code is [`DisconnectClass::Transient`], the fail-safe direction
/// (retry politely on the ladder rather than park a healthy pool).
///
/// Runbook: WS-GAP-01 ([`ErrorCode::WsGapDisconnectClassification`]).
#[must_use]
pub fn classify_disconnect(code: Option<DisconnectCode>) -> DisconnectClass {
    match code {
        Some(DisconnectCode::ExceededActiveConnections) => DisconnectClass::PoolOverflow,
        Some(DisconnectCode::AccessTokenExpired | DisconnectCode::AccessTokenInvalid) => {
            DisconnectClass::TokenStale
        }
        Some(
            DisconnectCode::DataApiSubscriptionRequired
            | DisconnectCode::AuthenticationFailed
            | DisconnectCode::ClientIdInvalid
            // 804 — "Requested number of instruments exceeds limit."
            //
            // MOVED here from the `_ => Transient` catch-all on 2026-08-14.
            // Transient means "retry on the ladder", and retrying 804 re-sends
            // the IDENTICAL over-limit subscribe set that was just rejected —
            // forever, every 30s at the ladder's cap. Nothing in that loop can
            // ever succeed, because nothing about the request changes between
            // attempts. It is a request-shaped error wearing a transport-code
            // costume, and the catch-all could not tell the difference.
            //
            // Worse, it is self-amplifying in exactly the direction that hurts
            // most: a permanent connect/subscribe/reject cycle is precisely
            // the traffic pattern 805 describes as "too many requests", whose
            // documented consequence is the USER being blocked — so retrying
            // one account-level rejection can earn another.
            //
            // Fatal parks the socket, and since 2026-08-14 a park is no longer
            // silent: it emits a coded error naming the endpoint and slot, and
            // `tv_dhan_ws_park_total` has an alarm. So this turns an invisible
            // infinite loop into one page that names the real problem —
            // somebody asked for more instruments than the endpoint allows.
            | DisconnectCode::InstrumentsExceedLimit,
        ) => DisconnectClass::Fatal,
        _ => DisconnectClass::Transient,
    }
}

// ---------------------------------------------------------------------------
// State machine vocabulary
// ---------------------------------------------------------------------------

/// Lifecycle phase of one supervised connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnPhase {
    /// Constructed, never dialed.
    Idle,
    /// A dial is in flight.
    Dialing,
    /// Socket is up; subscribe batches are being sent.
    Subscribing,
    /// Socket is up and delivering frames.
    Live,
    /// Waiting out a reconnect delay.
    Backoff,
    /// Stopped permanently. Never dials again this process.
    Parked,
}

impl ConnPhase {
    /// Whether the idle watchdog applies. Only phases where we legitimately
    /// expect traffic are watched: during [`ConnPhase::Backoff`] we are
    /// deliberately silent, and parking is terminal.
    #[must_use]
    pub const fn is_watchdog_eligible(self) -> bool {
        matches!(self, Self::Dialing | Self::Subscribing | Self::Live)
    }

    /// Stable lowercase tag for logs and metric labels.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Dialing => "dialing",
            Self::Subscribing => "subscribing",
            Self::Live => "live",
            Self::Backoff => "backoff",
            Self::Parked => "parked",
        }
    }
}

/// Something that happened to a connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnEvent {
    /// The shell is about to dial. Idempotent from [`ConnPhase::Idle`] or
    /// [`ConnPhase::Backoff`]; ignored once parked.
    BeginDial,
    /// Socket established.
    DialSucceeded,
    /// Dial failed before the socket came up.
    DialFailed,
    /// Every subscribe batch for this connection was accepted by the socket.
    SubscribeAcked,
    /// A subscribe batch could not be sent.
    SubscribeFailed,
    /// One frame arrived.
    FrameReceived,
    /// A control frame arrived proving the peer is alive, carrying no data
    /// (a Ping/Pong). Resets the idle watchdog and NOTHING else — see
    /// [`SocketEvent::KeepAlive`] for why the distinction is load-bearing.
    KeepAliveReceived,
    /// The socket closed, optionally carrying a Dhan disconnect code.
    Disconnected { code: Option<DisconnectCode> },
    /// The watchdog fired: no traffic for the idle threshold.
    IdleElapsed,
    /// Orderly shutdown.
    ShutdownRequested,
}

/// Why a connection stopped permanently.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParkReason {
    /// 805 — re-dialing would kill a sibling. See [`DisconnectClass::PoolOverflow`].
    PoolOverflow,
    /// An entitlement or credential error that needs operator action.
    FatalDisconnect,
    /// Orderly shutdown.
    Shutdown,
}

impl ParkReason {
    /// Every reason, for baseline pre-registration of the park counter.
    pub const ALL: [Self; 3] = [Self::PoolOverflow, Self::FatalDisconnect, Self::Shutdown];

    /// Stable lowercase tag for logs and metric labels.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PoolOverflow => "pool_overflow",
            Self::FatalDisconnect => "fatal_disconnect",
            Self::Shutdown => "shutdown",
        }
    }
}

/// Why a connection is being re-dialed. Metric label only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReconnectReason {
    /// The dial itself failed.
    DialFailed,
    /// A subscribe batch could not be sent.
    SubscribeFailed,
    /// The socket closed on a retryable code.
    Disconnected,
    /// The token was rejected.
    TokenStale,
    /// The watchdog fired.
    IdleSilence,
}

impl ReconnectReason {
    /// Stable lowercase tag for logs and metric labels.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::DialFailed => "dial_failed",
            Self::SubscribeFailed => "subscribe_failed",
            Self::Disconnected => "disconnected",
            Self::TokenStale => "token_stale",
            Self::IdleSilence => "idle_silence",
        }
    }
}

/// What the shell must do next. The supervisor emits exactly one of these per
/// event; the shell contains no policy beyond executing them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SupervisorAction {
    /// Dial now. Any existing socket must be closed first.
    Dial,
    /// Close any existing socket, sleep `delay_ms`, then send
    /// [`ConnEvent::BeginDial`].
    SleepThenDial { delay_ms: u64 },
    /// As [`SupervisorAction::SleepThenDial`], but the caller MUST obtain a
    /// fresh access token before dialing — the previous socket died on a stale
    /// one, so re-presenting it would simply be rejected again.
    RefreshTokenThenDial { delay_ms: u64 },
    /// Send this connection's subscribe batches.
    Subscribe,
    /// Stop permanently; never dial again this process.
    Park { reason: ParkReason },
    /// Nothing to do.
    Continue,
}

// ---------------------------------------------------------------------------
// Per-connection supervisor (the pure core)
// ---------------------------------------------------------------------------

/// The state machine for ONE connection.
///
/// Plain value, single-owner: the connection task owns it outright, so no lock
/// is taken on the per-frame path. That is a load-bearing property, not an
/// implementation detail — a mutex here would be shared with the frame path and
/// would violate THE ONE RULE.
#[derive(Debug, Clone)]
pub struct ConnectionSupervisor {
    slot: ConnectionSlot,
    phase: ConnPhase,
    /// Consecutive failed attempts so far; also the ladder index for the NEXT
    /// delay. Reset to zero when the connection proves healthy.
    attempt: u32,
    watchdog: IdleWatchdog,
    /// Whether a frame has arrived since the current socket came up. The
    /// attempt counter resets on the FIRST FRAME rather than on a successful
    /// dial, because a socket that connects and subscribes but never delivers
    /// anything is not healthy — resetting on dial would let such a socket
    /// re-dial instantly forever.
    proven_healthy: bool,
    /// When the CURRENT socket delivered its FIRST frame. `None` until it does,
    /// and cleared on every dial.
    ///
    /// This is the flap damper's health clock. `proven_healthy` above answers
    /// "did a frame ever arrive on this socket", which a connection that dies
    /// one millisecond after its prev-close packet satisfies — and that
    /// connection then re-dialled instantly, forever. `healthy_since` answers
    /// the question that actually matters: *for how long* did it carry frames.
    healthy_since: Option<Instant>,
    /// Monotonic timestamps of recent re-dials, newest overwriting oldest.
    ///
    /// A fixed inline array, never a `Vec`: this is written on the disconnect
    /// path of a socket that may be flapping, and an allocation there is
    /// exactly what THE ONE RULE forbids.
    redial_history: [Option<Instant>; FLAP_HISTORY_SLOTS],
    /// Next write position in [`Self::redial_history`].
    redial_cursor: usize,
    frames: u64,
    reconnects: u64,
    /// Set exactly once, when the supervisor parks. Retained so the shell can
    /// report WHY without re-deriving it, and so a caller handed an
    /// already-parked supervisor can exit instead of spinning.
    park_reason: Option<ParkReason>,
}

impl ConnectionSupervisor {
    /// A fresh supervisor for `slot`, idle and never dialed.
    #[must_use]
    pub fn new(slot: ConnectionSlot, now: Instant) -> Self {
        Self {
            slot,
            phase: ConnPhase::Idle,
            attempt: 0,
            watchdog: IdleWatchdog::new(now),
            proven_healthy: false,
            healthy_since: None,
            redial_history: [None; FLAP_HISTORY_SLOTS],
            redial_cursor: 0,
            frames: 0,
            reconnects: 0,
            park_reason: None,
        }
    }

    /// The budget-granted slot this supervisor drives.
    #[must_use]
    pub const fn slot(&self) -> ConnectionSlot {
        self.slot
    }

    /// Current lifecycle phase.
    #[must_use]
    pub const fn phase(&self) -> ConnPhase {
        self.phase
    }

    /// Consecutive failed attempts; the ladder index for the next delay.
    #[must_use]
    pub const fn attempt(&self) -> u32 {
        self.attempt
    }

    /// Frames observed on this connection since process start.
    #[must_use]
    pub const fn frames_received(&self) -> u64 {
        self.frames
    }

    /// Reconnect cycles this connection has entered since process start.
    #[must_use]
    pub const fn reconnects(&self) -> u64 {
        self.reconnects
    }

    /// Why this connection parked, if it has. `None` while it is still alive.
    #[must_use]
    pub const fn park_reason(&self) -> Option<ParkReason> {
        self.park_reason
    }

    /// Phase projected onto the shared [`ConnectionState`] vocabulary, for
    /// `ws_event_audit` rows and the `/health` surface.
    #[must_use]
    pub const fn connection_state(&self) -> ConnectionState {
        match self.phase {
            ConnPhase::Idle | ConnPhase::Parked => ConnectionState::Disconnected,
            ConnPhase::Dialing => ConnectionState::Connecting,
            ConnPhase::Subscribing | ConnPhase::Live => ConnectionState::Connected,
            ConnPhase::Backoff => ConnectionState::Reconnecting,
        }
    }

    /// Feeds one event through the state machine and returns the action the
    /// shell must take. Total, allocation-free, O(1).
    pub fn on_event(&mut self, event: ConnEvent, now: Instant) -> SupervisorAction {
        // Parking is terminal and absorbs everything except a repeat shutdown.
        if self.phase == ConnPhase::Parked {
            return SupervisorAction::Continue;
        }

        match event {
            ConnEvent::ShutdownRequested => self.park(ParkReason::Shutdown),

            ConnEvent::BeginDial => {
                self.phase = ConnPhase::Dialing;
                self.proven_healthy = false;
                self.healthy_since = None;
                // Reset here, not on dial completion: the watchdog must also
                // cover a dial that hangs forever without ever completing.
                self.watchdog.record_activity(now);
                SupervisorAction::Dial
            }

            ConnEvent::DialSucceeded => {
                self.phase = ConnPhase::Subscribing;
                self.watchdog.record_activity(now);
                SupervisorAction::Subscribe
            }

            ConnEvent::DialFailed => self.schedule_redial(ReconnectReason::DialFailed, now),

            ConnEvent::SubscribeFailed => {
                // WS-GAP-02: a subscribe that will not go out leaves the socket
                // connected but blind. Tear it down rather than sit on a live
                // socket carrying nothing.
                warn!(
                    code = ErrorCode::WsGapSubscriptionBatching.code_str(),
                    endpoint = self.slot.endpoint.as_str(),
                    pool_index = self.slot.pool_index,
                    "subscribe batch could not be sent — tearing the socket down and re-dialing"
                );
                self.schedule_redial(ReconnectReason::SubscribeFailed, now)
            }

            ConnEvent::SubscribeAcked => {
                self.phase = ConnPhase::Live;
                self.watchdog.record_activity(now);
                SupervisorAction::Continue
            }

            ConnEvent::FrameReceived => {
                self.watchdog.record_activity(now);
                self.frames = self.frames.saturating_add(1);
                if !self.proven_healthy {
                    self.proven_healthy = true;
                    self.attempt = 0;
                    // Start the health clock at the FIRST frame. The attempt
                    // reset above is retained for compatibility with the
                    // ladder's own semantics, but it no longer implies an
                    // instant re-dial on its own: the damper reads this
                    // timestamp and withholds rung 0 until the socket has
                    // actually carried frames for MIN_HEALTHY_SESSION_MS.
                    self.healthy_since = Some(now);
                }
                // A frame can legitimately arrive before our own subscribe ack
                // (the prev-close packet is pushed on subscribe). Treat it as
                // proof the socket is live.
                if self.phase == ConnPhase::Subscribing {
                    self.phase = ConnPhase::Live;
                }
                SupervisorAction::Continue
            }

            // A ping proves the TRANSPORT is alive. It must reset the idle
            // watchdog — and must do nothing else.
            //
            // It deliberately does NOT bump `frames` and does NOT set
            // `proven_healthy`: those mean "this connection delivered market
            // data", and a socket that only pings has delivered none. Letting
            // a keep-alive claim health would hide a silently-failed subscribe
            // behind a heartbeat. That condition has its own detector — the
            // 30s silence scan (RISK-GAP-03), which is market-hours gated so
            // the legitimately silent pre-open never pages.
            ConnEvent::KeepAliveReceived => {
                self.watchdog.record_activity(now);
                SupervisorAction::Continue
            }

            ConnEvent::Disconnected { code } => {
                self.reconnects = self.reconnects.saturating_add(1);
                match classify_disconnect(code) {
                    DisconnectClass::PoolOverflow => {
                        error!(
                            code = ErrorCode::WsGapDisconnectClassification.code_str(),
                            endpoint = self.slot.endpoint.as_str(),
                            pool_index = self.slot.pool_index,
                            "Dhan closed this socket with 805 (too many connections). Dhan kills \
                             the OLDEST socket per extra connection, so re-dialing would destroy \
                             a healthy sibling instead of recovering this one — parking. Check \
                             for a second process holding Dhan sockets on this account."
                        );
                        self.park(ParkReason::PoolOverflow)
                    }
                    DisconnectClass::Fatal => {
                        error!(
                            code = ErrorCode::WsGapDisconnectClassification.code_str(),
                            endpoint = self.slot.endpoint.as_str(),
                            pool_index = self.slot.pool_index,
                            disconnect_code = code.map_or(0, |c| c.as_u16()),
                            "Dhan closed this socket with a credential or entitlement error that \
                             cannot self-heal — parking. Operator action required."
                        );
                        self.park(ParkReason::FatalDisconnect)
                    }
                    DisconnectClass::TokenStale => {
                        // Floor the LADDER, then add this socket's stagger —
                        // never the other way round (2026-08-11).
                        //
                        // This was `next_delay_ms().max(FLOOR)`, which reads
                        // as "at least the floor" and is, but it also silently
                        // discarded the jitter. The ladder's first three rungs
                        // are 0/1000/2000 ms and the whole jitter range is
                        // 0-375 ms, so every jittered value on those rungs is
                        // below the 5,000 ms floor and `max` collapsed all of
                        // them onto exactly 5,000.
                        //
                        // That is precisely the wrong behaviour for the event
                        // this arm exists to handle: a token expiring kills
                        // ALL sixteen sockets at once, so all sixteen slept an
                        // identical 5,000 ms, woke in the same tick, and hit
                        // the token endpoint together — a self-inflicted
                        // thundering herd on the one code path guaranteed to
                        // be entered by every connection simultaneously.
                        //
                        // Flooring the base first keeps the "wait at least
                        // 5 s" intent and restores the fan-out on top of it.
                        //
                        // 2026-08-19: the base is now the DAMPED ladder value
                        // rather than the raw rung, so a socket flapping on a
                        // token that keeps going stale is slowed by the same
                        // ceiling as any other flapper. The floor-then-jitter
                        // ordering is unchanged.
                        let damped = self.damped_decision_without_jitter(now);
                        let base = damped.delay_ms.max(TOKEN_STALE_REDIAL_FLOOR_MS);
                        let delay = base.saturating_add(self.jitter_ms());
                        self.enter_backoff(ReconnectReason::TokenStale, damped.verdict, now);
                        SupervisorAction::RefreshTokenThenDial { delay_ms: delay }
                    }
                    DisconnectClass::Transient => {
                        // THE CASCADE ARM. `connection.rs` reports a bare TCP
                        // reset as `Closed { code: None }`, which classifies
                        // here — so this is the arm an 805-delivered-as-RST
                        // lands in, and before the damper it re-dialled on
                        // ladder rung 0 (`0ms`), evicting a healthy sibling
                        // per Dhan's oldest-socket-dies semantics.
                        self.schedule_redial(ReconnectReason::Disconnected, now)
                    }
                }
            }

            ConnEvent::IdleElapsed => {
                if !self.phase.is_watchdog_eligible() {
                    return SupervisorAction::Continue;
                }
                self.reconnects = self.reconnects.saturating_add(1);
                warn!(
                    code = ErrorCode::WsGapConnectionState.code_str(),
                    endpoint = self.slot.endpoint.as_str(),
                    pool_index = self.slot.pool_index,
                    phase = self.phase.as_str(),
                    idle_secs = self.watchdog.idle_for(now).as_secs(),
                    "socket silent past the idle threshold — reconnecting on our terms before \
                     Dhan closes it at 40s"
                );
                self.schedule_redial(ReconnectReason::IdleSilence, now)
            }
        }
    }

    /// Asks whether the idle watchdog has fired. Returns
    /// [`SupervisorAction::Continue`] when it has not. Call at
    /// [`IDLE_POLL_INTERVAL`]. O(1), no allocation, monotonic clock only.
    pub fn poll(&mut self, now: Instant) -> SupervisorAction {
        if !self.phase.is_watchdog_eligible() || !self.watchdog.is_expired(now) {
            return SupervisorAction::Continue;
        }
        self.on_event(ConnEvent::IdleElapsed, now)
    }

    /// How long the CURRENT socket has been delivering frames, in
    /// milliseconds. Zero if it has delivered none.
    ///
    /// Saturating: a non-monotonic `now` (impossible with `Instant`, but the
    /// function stays total anyway) yields 0, which is the fail-safe direction
    /// — it makes the socket look UNhealthy and earns backoff rather than an
    /// instant re-dial.
    fn healthy_duration_ms(&self, now: Instant) -> u64 {
        match self.healthy_since {
            Some(since) => {
                u64::try_from(now.saturating_duration_since(since).as_millis()).unwrap_or(u64::MAX)
            }
            None => 0,
        }
    }

    /// Re-dials by THIS socket inside [`FLAP_WINDOW_MS`], counting only
    /// re-dials that already happened — the one being decided right now is
    /// recorded afterwards, so it never counts itself.
    ///
    /// O([`FLAP_HISTORY_SLOTS`]) = O(1) with a fixed bound of eight, zero
    /// allocation, monotonic clock only.
    fn recent_redial_count(&self, now: Instant) -> u32 {
        let window = Duration::from_millis(FLAP_WINDOW_MS);
        let mut count: u32 = 0;
        for stamp in &self.redial_history {
            if let Some(at) = stamp
                && now.saturating_duration_since(*at) <= window
            {
                count = count.saturating_add(1);
            }
        }
        count
    }

    /// Records that a re-dial happened at `now`, oldest entry overwritten.
    fn record_redial(&mut self, now: Instant) {
        if let Some(slot) = self.redial_history.get_mut(self.redial_cursor) {
            *slot = Some(now);
        }
        self.redial_cursor = (self.redial_cursor + 1) % FLAP_HISTORY_SLOTS;
    }

    /// The flap-damped delay for the next attempt, including this connection's
    /// fixed per-slot stagger. Does NOT mutate.
    fn damped_decision(&self, now: Instant) -> ReconnectDecision {
        damped_reconnect_delay_with_jitter(
            self.attempt,
            self.healthy_duration_ms(now),
            self.recent_redial_count(now),
            self.slot.global_index,
        )
    }

    /// The same decision WITHOUT this socket's stagger.
    ///
    /// Split out so a caller that needs to raise the floor can floor the
    /// damped ladder and then add the stagger, rather than flooring the sum
    /// and throwing the stagger away — see the `TokenStale` arm.
    fn damped_decision_without_jitter(&self, now: Instant) -> ReconnectDecision {
        damped_reconnect_delay(
            self.attempt,
            self.healthy_duration_ms(now),
            self.recent_redial_count(now),
        )
    }

    /// This socket's fixed fan-out offset. Index 0 always gets zero, so one
    /// connection per pool keeps the exact instant-retry behaviour.
    fn jitter_ms(&self) -> u64 {
        reconnect_jitter_ms(self.slot.global_index)
    }

    /// Common tail for every retryable failure: compute the delay, count it,
    /// advance the ladder, drop into backoff.
    fn schedule_redial(&mut self, reason: ReconnectReason, now: Instant) -> SupervisorAction {
        let decision = self.damped_decision(now);
        self.enter_backoff(reason, decision.verdict, now);
        SupervisorAction::SleepThenDial {
            delay_ms: decision.delay_ms,
        }
    }

    fn enter_backoff(&mut self, reason: ReconnectReason, verdict: FlapVerdict, now: Instant) {
        self.attempt = self.attempt.saturating_add(1);
        self.phase = ConnPhase::Backoff;
        self.proven_healthy = false;
        self.healthy_since = None;
        self.record_redial(now);
        metrics::counter!(
            RECONNECT_METRIC,
            "endpoint" => self.slot.endpoint.as_str(),
            "reason" => reason.as_str(),
        )
        .increment(1);
        // The damper must never act in silence. A socket held at 30s while the
        // operator believes the ladder is running is the false-OK class the
        // house rules forbid, so every re-dial the damper actually slowed down
        // is counted under the reason that provoked it.
        if verdict.is_damped() {
            metrics::counter!(
                FLAP_DAMPED_METRIC,
                "endpoint" => self.slot.endpoint.as_str(),
                "verdict" => verdict.as_str(),
            )
            .increment(1);
        }
    }

    fn park(&mut self, reason: ParkReason) -> SupervisorAction {
        self.phase = ConnPhase::Parked;
        self.park_reason = Some(reason);
        metrics::counter!(
            PARK_METRIC,
            "endpoint" => self.slot.endpoint.as_str(),
            "reason" => reason.as_str(),
        )
        .increment(1);
        // A park is PERMANENT — this socket will never dial again for the rest
        // of the session, by design (re-dialing into a 805 kills a healthy pool
        // member, and re-dialing into a credential rejection just repeats it).
        //
        // The park POLICY is correct and is not changed here. What was wrong,
        // until 2026-08-14, is that it happened in complete silence: the
        // counter above was incremented and nothing else. The counter had no
        // alarm and no log line, so a socket could drop out of a 16-socket pool
        // permanently and the only way to notice was to go looking for a
        // metric nobody was watching. A permanent capacity loss must announce
        // itself.
        //
        // Cold path by construction: a park is terminal for this slot, so this
        // can log at most once per connection per session.
        error!(
            code = ErrorCode::WsGapConnectionState.code_str(),
            endpoint = self.slot.endpoint.as_str(),
            connection_index = self.slot.global_index,
            reason = reason.as_str(),
            "a Dhan live-feed socket has PARKED PERMANENTLY and will not dial again this \
             session — the pool is now carrying fewer connections than it was planned for, \
             and this one only returns on a process restart"
        );
        SupervisorAction::Park { reason }
    }
}

// ---------------------------------------------------------------------------
// Subscribe guard — subscriptions survive reconnect
// ---------------------------------------------------------------------------

/// One instrument in a connection's subscription set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SubscribeInstrument {
    /// Dhan `SecurityId`. Serialised as a STRING on the wire by the transport
    /// (`15-live-market-feed.md` — the subscribe payload uses string ids); the
    /// guard stores the numeric identity.
    ///
    /// The shared 64-bit alias, not a narrower local type: the id space was
    /// widened to `u64` by the §28.1 lift and is namespace-banded per feed, so
    /// a `u32` here would reintroduce the silent truncation that widening
    /// removed.
    pub security_id: SecurityId,
    /// The instrument's segment. Carried alongside the id because
    /// `security_id` ALONE is not unique — the only unique instrument key is
    /// `(security_id, exchange_segment)` per I-P1-11.
    pub segment: ExchangeSegment,
}

/// Why a subscription set was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SubscribeGuardRefusal {
    /// More instruments than this endpoint type permits on ONE connection.
    #[error("{endpoint} accepts at most {max} instruments per connection, {requested} requested")]
    TooManyInstruments {
        /// Endpoint type asked for.
        endpoint: DhanEndpointType,
        /// Instruments the caller supplied.
        requested: usize,
        /// Documented per-connection cap.
        max: u32,
    },
    /// A swap was asked for against an instrument this connection does not
    /// hold.
    ///
    /// Fail-closed rather than fall back to an append: applying it would
    /// subscribe the NEW instrument while unsubscribing something that was
    /// never there, so the retained set would claim an instrument the socket
    /// does not have and every later reconnect would replay it.
    #[error("{endpoint} was asked to swap an instrument it does not hold")]
    NotSubscribed {
        /// Endpoint type asked for.
        endpoint: DhanEndpointType,
    },
    /// A swap was asked for whose NEW instrument this connection already
    /// holds.
    ///
    /// Fail-closed, and this is the sharpest of the three: applying it would
    /// send a subscribe for an instrument already on this socket, and Dhan
    /// answers a duplicate subscribe with an 804 — Fatal, so the connection
    /// closes and parks for the session. Refusing costs one minute's swap;
    /// applying costs every instrument the connection was carrying.
    #[error("{endpoint} was asked to swap in an instrument it already holds")]
    AlreadySubscribed {
        /// Endpoint type asked for.
        endpoint: DhanEndpointType,
    },
}

/// The subscription set for ONE connection, and the fact of whether the live
/// socket has it.
///
/// This is what makes subscriptions survive a reconnect: the set is built once
/// at boot and OUTLIVES every socket. A reconnect does not rebuild it, it
/// replays it — so a resubscribe cannot silently drop instruments because a
/// rebuild path had different inputs.
#[derive(Debug, Clone)]
pub struct SubscribeGuard {
    endpoint: DhanEndpointType,
    instruments: Vec<SubscribeInstrument>,
    /// Bumped on every confirmed subscribe. Lets a consumer discard a frame
    /// attributed to a previous socket incarnation.
    generation: u64,
    confirmed: bool,
}

/// Drops instruments that would put the same one on a connection twice, and
/// says so loudly.
///
/// # Why the guard does this at all
///
/// Every caller today builds a duplicate-free set, and the guard is not
/// relying on that. It is the LAST place a set can be inspected before it
/// reaches the wire, it is shared by all four endpoint types, and the cost of
/// a duplicate reaching Dhan is not a degraded connection but a dead one:
/// error 804 is Fatal, so the socket closes, the supervisor parks it for the
/// session, and every instrument that connection was carrying goes dark.
///
/// # Why DROP rather than refuse the whole set
///
/// Refusing would leave the connection undialed, which is the same outcome as
/// the 804 it prevents — no connection, no data. Dropping keeps the other 249
/// instruments live, and because the guard's own `instruments` records the
/// drop, the retained set still matches the wire exactly: nothing here can
/// create the believed-held-ahead-of-wire divergence this module exists to
/// prevent.
///
/// The drop is never silent: each one increments
/// [`SUBSCRIBE_DUPLICATE_METRIC`] and the first is named in a coded `error!`,
/// because a non-zero count is a producer defect that was caught rather than
/// suffered.
///
/// # Complexity
///
/// O(n) with an O(1)-average set probe per instrument; n is at most 5,000 on
/// a main-feed connection and is walked once, on the cold path, at dial.
fn drop_duplicates(
    endpoint: DhanEndpointType,
    site: &'static str,
    already: &[SubscribeInstrument],
    incoming: Vec<SubscribeInstrument>,
) -> Vec<SubscribeInstrument> {
    // Explicit loops rather than an iterator-collect, and pre-sized. This is
    // the DIAL path, not the drain — it runs when a connection is built and
    // on a live top-up, never per frame — but the file is hot-path-classed
    // and an exemption comment is a weaker thing to leave behind than code
    // that does not need one. Both allocations are bounded by the endpoint
    // cap (5,000 on the main feed, 50 on depth-20) and happen once.
    let mut seen: std::collections::HashSet<(SecurityId, ExchangeSegment)> =
        std::collections::HashSet::with_capacity(already.len().saturating_add(incoming.len()));
    for i in already {
        seen.insert((i.security_id, i.segment));
    }
    let before = incoming.len();
    let mut first_dropped: Option<SubscribeInstrument> = None;
    let mut kept: Vec<SubscribeInstrument> = Vec::with_capacity(before);
    for i in incoming {
        if seen.insert((i.security_id, i.segment)) {
            kept.push(i);
        } else if first_dropped.is_none() {
            first_dropped = Some(i);
        }
    }
    let dropped = before.saturating_sub(kept.len());
    if dropped > 0 {
        metrics::counter!(
            SUBSCRIBE_DUPLICATE_METRIC,
            "endpoint" => endpoint.as_str(),
            "site" => site,
        )
        .increment(dropped as u64);
        if let Some(example) = first_dropped {
            error!(
                code = ErrorCode::WsGapSubscriptionBatching.code_str(),
                endpoint = endpoint.as_str(),
                site,
                dropped,
                security_id = example.security_id,
                segment = example.segment.as_str(),
                "a subscription set carried the same instrument more than once — \
                 dropped before it reached the wire, because Dhan answers a \
                 duplicate subscribe with an 804 (Fatal) and the connection \
                 would have parked for the session. The producer built a set \
                 with a repeat in it."
            );
        }
    }
    kept
}

impl SubscribeGuard {
    /// Builds a guard, refusing a set larger than the endpoint's documented
    /// per-connection cap.
    ///
    /// Fail-closed by design: Dhan answers an over-limit subscribe with 804
    /// rather than silently truncating, so refusing locally turns a live
    /// disconnect into a boot-time error at the place the mistake was made.
    ///
    /// # Errors
    /// [`SubscribeGuardRefusal::TooManyInstruments`] when the set exceeds
    /// [`DhanEndpointType::max_instruments_per_connection`].
    pub fn try_new(
        endpoint: DhanEndpointType,
        instruments: Vec<SubscribeInstrument>,
    ) -> Result<Self, SubscribeGuardRefusal> {
        // Deduped BEFORE the cap check, so a set that only breaches the
        // cap because of its own repeats is dialed rather than refused.
        let instruments = drop_duplicates(endpoint, "new", &[], instruments);
        let max = endpoint.max_instruments_per_connection();
        let requested = instruments.len();
        if u64::try_from(requested).unwrap_or(u64::MAX) > u64::from(max) {
            warn!(
                code = ErrorCode::WsGapSubscriptionBatching.code_str(),
                endpoint = endpoint.as_str(),
                requested,
                max,
                "refusing a subscription set larger than the endpoint's per-connection cap"
            );
            return Err(SubscribeGuardRefusal::TooManyInstruments {
                endpoint,
                requested,
                max,
            });
        }
        Ok(Self {
            endpoint,
            instruments,
            generation: 0,
            confirmed: false,
        })
    }

    /// Endpoint type this set belongs to.
    #[must_use]
    pub const fn endpoint(&self) -> DhanEndpointType {
        self.endpoint
    }

    /// Instruments in the set.
    #[must_use]
    pub fn len(&self) -> usize {
        self.instruments.len()
    }

    /// Whether the set is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.instruments.is_empty()
    }

    /// Incarnation counter — bumped on every confirmed subscribe.
    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    /// Whether the live socket is known to hold this subscription.
    #[must_use]
    pub const fn is_confirmed(&self) -> bool {
        self.confirmed
    }

    /// Whether the set must be (re)sent before the connection is useful.
    #[must_use]
    pub const fn needs_resubscribe(&self) -> bool {
        !self.confirmed
    }

    /// The subscribe messages for this set, honouring the endpoint's documented
    /// per-message cap (100 for the main feed, 50 for depth-20).
    ///
    /// Zero-allocation: each item is a borrowed slice of the set, not a copy.
    /// The iteration itself is O(n) in instruments — inherent, since every
    /// instrument must be named on the wire — and runs on the cold path, once
    /// per connect.
    pub fn batches(&self) -> impl Iterator<Item = &[SubscribeInstrument]> {
        // `max(1)`: a zero cap would panic `chunks`. Only order-update reports
        // zero, and it never carries instruments, so this is a totality guard
        // rather than a live path.
        let per_message = usize::try_from(self.endpoint.max_instruments_per_subscribe_message())
            .unwrap_or(usize::MAX)
            .max(1);
        self.instruments.chunks(per_message)
    }

    /// Number of subscribe messages [`SubscribeGuard::batches`] will yield.
    #[must_use]
    pub fn batch_count(&self) -> usize {
        self.batches().count()
    }

    /// Appends instruments to a set that is already on the wire, returning the
    /// index the new tail starts at.
    ///
    /// # Why a live connection is topped up rather than re-dialed
    ///
    /// Dhan caps a connection at 5,000 instruments and an account at 5
    /// connections — 25,000 total, and both numbers are already the documented
    /// maximum. The spot universe (~850) is dialed at boot and packs onto ONE
    /// connection, so ~4,150 slots sit on a socket that will never use them.
    /// The contract attach, which runs later once option prices exist, can only
    /// claim WHOLE free connections: 4 × 5,000 = 20,000. The authorized
    /// ATM ± 25 window needs ~23,000–23,750, so the window silently shrank to
    /// ± 20 — not because the operator's scope was wrong, but because ~4,150
    /// paid-for slots were unreachable.
    ///
    /// They are reachable by SUBSCRIBING MORE on the socket that already holds
    /// them. Dhan's protocol allows exactly that: subscribe messages are
    /// incremental, capped at 100 instruments each, and a connection accepts
    /// them until it reaches 5,000. Re-dialing the connection would also work
    /// and is strictly worse — it drops the spot feed mid-session, and the
    /// close/reconnect race would briefly leave two tasks driving one slot.
    ///
    /// # Why the caller gets an index rather than the batches
    ///
    /// Only the NEW instruments may be sent. Re-sending the whole set would
    /// re-subscribe several thousand already-live instruments, which is both
    /// wasteful and a real risk: Dhan answers an over-limit subscribe with 804
    /// and drops the socket. Returning the split point lets the caller batch
    /// exactly the tail via [`SubscribeGuard::batches_from`].
    ///
    /// # Errors
    ///
    /// [`SubscribeGuardRefusal::TooManyInstruments`] when the COMBINED set
    /// would exceed the endpoint's per-connection cap. Fail-closed, and the
    /// guard is left untouched — a refused top-up must not half-apply, or the
    /// in-memory set would claim instruments the socket never received and
    /// every later reconnect would replay a subscription Dhan rejects.
    pub fn try_extend(
        &mut self,
        more: Vec<SubscribeInstrument>,
    ) -> Result<usize, SubscribeGuardRefusal> {
        if more.is_empty() {
            return Ok(self.instruments.len());
        }
        // Deduped against what this connection ALREADY holds as well as
        // within the top-up itself. A top-up naming a held instrument is
        // the most likely shape here: it is what a re-selection produces
        // when only part of the desired set changed.
        let more = drop_duplicates(self.endpoint, "extend", &self.instruments, more);
        if more.is_empty() {
            return Ok(self.instruments.len());
        }
        let max = self.endpoint.max_instruments_per_connection();
        let combined = self.instruments.len().saturating_add(more.len());
        if u64::try_from(combined).unwrap_or(u64::MAX) > u64::from(max) {
            warn!(
                code = ErrorCode::WsGapSubscriptionBatching.code_str(),
                endpoint = self.endpoint.as_str(),
                already = self.instruments.len(),
                adding = more.len(),
                max,
                "refusing a top-up that would take a live connection past its \
                 per-connection cap — the guard is left unchanged so the \
                 in-memory set never claims instruments the socket does not hold"
            );
            return Err(SubscribeGuardRefusal::TooManyInstruments {
                endpoint: self.endpoint,
                requested: combined,
                max,
            });
        }
        let start = self.instruments.len();
        self.instruments.extend(more);
        Ok(start)
    }

    /// Subscribe messages covering only the instruments from `start` onward.
    ///
    /// The tail counterpart of [`SubscribeGuard::batches`], used to send a
    /// top-up without re-subscribing what the socket already holds. A `start`
    /// past the end yields nothing rather than panicking: the caller is
    /// reporting "no new instruments", which is a legal no-op.
    pub fn batches_from(&self, start: usize) -> impl Iterator<Item = &[SubscribeInstrument]> {
        let per_message = usize::try_from(self.endpoint.max_instruments_per_subscribe_message())
            .unwrap_or(usize::MAX)
            .max(1);
        let tail = self.instruments.get(start..).unwrap_or(&[]);
        tail.chunks(per_message)
    }

    /// Free slots left on this connection before the endpoint's cap.
    ///
    /// This is the number the contract attach needs in order to stop stranding
    /// capacity: without it, room on a partially-filled connection is invisible
    /// and the attach asks only for whole free connections.
    /// Shrinks the tracked set back to `len`, dropping the tail.
    ///
    /// The ONLY sanctioned use is a top-up that stopped part-way on its wire
    /// budget: the guard must then describe what the socket actually received,
    /// because the guard's set IS the reconnect replay. A guard naming
    /// instruments that never reached the wire makes every later reconnect
    /// replay a subscription the socket never had -- and if that pushes the
    /// replay over the endpoint cap, Dhan answers 804 and drops the socket.
    ///
    /// Growing is not possible here by construction: a `len` at or past the
    /// current length is a no-op, so this can only ever shrink.
    ///
    /// # Complexity
    /// O(dropped) -- truncation of a `Vec`, no allocation.
    pub fn truncate_to(&mut self, len: usize) {
        if len < self.instruments.len() {
            self.instruments.truncate(len);
        }
    }

    #[must_use]
    pub fn spare_capacity(&self) -> usize {
        let max =
            usize::try_from(self.endpoint.max_instruments_per_connection()).unwrap_or(usize::MAX);
        max.saturating_sub(self.instruments.len())
    }

    /// Records that the live socket accepted the whole set.
    pub fn mark_confirmed(&mut self) {
        self.confirmed = true;
        self.generation = self.generation.saturating_add(1);
    }

    /// Records that the socket carrying this subscription is gone. The set is
    /// retained verbatim for replay on the next connect.
    pub fn mark_lost(&mut self) {
        self.confirmed = false;
    }

    /// Replaces one live instrument with another on a socket that is already
    /// up, returning what to send on the wire.
    ///
    /// # Why this exists (2026-08-26)
    ///
    /// The operator's depth-200 rule is that the four sockets carry the
    /// AT-THE-MONEY call and put of NIFTY and BANKNIFTY, re-picked every
    /// minute. An at-the-money strike chosen at 09:10 is not at-the-money at
    /// 14:00, so a set fixed at attach time silently drifts away from the
    /// thing it was chosen to be — and drifts SILENTLY, because a
    /// subscription to a now-far-from-the-money strike is perfectly healthy
    /// and returns real data all day.
    ///
    /// Until now the guard could only GROW ([`SubscribeGuard::try_extend`])
    /// or be replayed whole. Neither expresses a swap:
    ///
    /// - Re-dialing the connection drops depth for the reconnect and is the
    ///   churn this design exists to avoid — one socket redialled **322
    ///   times in a single session** on 2026-08-26 before the ranking fix.
    /// - `try_extend` alone leaves the OLD instrument subscribed. The socket
    ///   would then hold both, the retained set would claim both, and every
    ///   later reconnect would replay a set that grows once per ATM change
    ///   until it hits the per-connection cap.
    ///
    /// # What the caller must do with the result
    ///
    /// Send [`SubscribeSwap::unsubscribe`] FIRST, then
    /// [`SubscribeSwap::subscribe`]. Order matters on a depth-200 socket,
    /// which accepts exactly one instrument: subscribing before unsubscribing
    /// asks for two, and Dhan answers an over-limit subscribe with **804**,
    /// which is Fatal — retrying re-sends the identical over-limit set
    /// forever and can earn an 805 account block.
    ///
    /// # Why the retained set is updated even though the wire has not moved
    ///
    /// The guard is the REPLAY source for a reconnect. If the swap were
    /// recorded only after the socket confirmed, a disconnect in between
    /// would replay the OLD instrument and the sockets would quietly revert
    /// to a strike nobody chose. Recording it here means a reconnect replays
    /// the CURRENT intent, and the cost of the socket never receiving the
    /// swap is one stale minute — recoverable, and visible, because the next
    /// minute's evaluation asks again.
    ///
    /// # Errors
    ///
    /// [`SubscribeGuardRefusal::NotSubscribed`] when `old` is not in the set.
    /// Fail-closed and the guard is left untouched: a swap against an
    /// instrument this connection never held would otherwise ADD `new`
    /// while unsubscribing something that was never there, which is
    /// `try_extend` wearing a swap's name.
    ///
    /// A swap of an instrument for ITSELF is a legal no-op — it returns empty
    /// wire work rather than an error, because the caller asking "make this
    /// socket carry X" when it already carries X is a correct question with a
    /// correct answer of "nothing to do". That is the ordinary case every
    /// minute the market does not move a strike.
    ///
    /// # Complexity
    ///
    /// O(n) in the instruments held by THIS connection — a linear scan for
    /// `old`. On a depth-200 connection n is 1 and on depth-20 it is at most
    /// 50, so this is O(1) in every use it was written for. It is NOT O(1) on
    /// a 5,000-instrument main-feed connection, and is flagged rather than
    /// relabelled: swapping on the main feed is not a use this was built for.
    pub fn try_swap(
        &mut self,
        old: SubscribeInstrument,
        new: SubscribeInstrument,
    ) -> Result<SubscribeSwap, SubscribeGuardRefusal> {
        // A linear scan, and it is NOT relabelled as O(1) — the doc above
        // flags it as O(n) in the instruments held by THIS connection. What
        // makes it acceptable is what n actually is at the only call sites
        // this was written for: 1 on a depth-200 connection and at most 50 on
        // depth-20, once a minute, on the cold path. An index would cost a
        // second structure that must stay in lockstep with the Vec across
        // every swap, extend and reconnect replay — and a structure that can
        // drift is a larger risk than a scan of fifty elements.
        // O(1) EXEMPT: n is 1 (depth-200) or <= 50 (depth-20), cold path, once per minute.
        let Some(at) = self.instruments.iter().position(|held| *held == old) else {
            warn!(
                code = ErrorCode::WsGapSubscriptionBatching.code_str(),
                endpoint = self.endpoint.as_str(),
                held = self.instruments.len(),
                "refusing a swap whose OLD instrument this connection does not \
                 hold — applying it would subscribe the new one while \
                 unsubscribing something that was never there"
            );
            return Err(SubscribeGuardRefusal::NotSubscribed {
                endpoint: self.endpoint,
            });
        };
        if old == new {
            return Ok(SubscribeSwap::NO_OP);
        }
        // The NEW instrument must not already be on this connection, and this
        // is the sharpest of the guard's three checks.
        //
        // `old == new` above catches the no-op. This catches the different
        // case: `new` is held at some OTHER position, so the swap would
        // unsubscribe `old` and then subscribe something the socket already
        // has. Dhan answers that with an 804 — Fatal — and the supervisor
        // parks the connection for the session, taking every instrument it
        // carried with it.
        //
        // Fail-closed. Refusing costs this minute's swap; the caller retries
        // next minute, and the set is left exactly as the wire has it.
        //
        // Same scan as the one above, same exemption for the same reason.
        // O(1) EXEMPT: n is 1 (depth-200) or <= 50 (depth-20), cold path, once per minute.
        if self.instruments.contains(&new) {
            metrics::counter!(
                SUBSCRIBE_DUPLICATE_METRIC,
                "endpoint" => self.endpoint.as_str(),
                "site" => "swap",
            )
            .increment(1);
            error!(
                code = ErrorCode::WsGapSubscriptionBatching.code_str(),
                endpoint = self.endpoint.as_str(),
                held = self.instruments.len(),
                security_id = new.security_id,
                segment = new.segment.as_str(),
                "refusing a swap whose NEW instrument this connection already \
                 holds — subscribing it twice is an 804, which is Fatal and \
                 parks the connection for the session"
            );
            return Err(SubscribeGuardRefusal::AlreadySubscribed {
                endpoint: self.endpoint,
            });
        }
        // Replace in place rather than remove-then-push: position is the only
        // thing that distinguishes one depth-20 slot from another when the
        // caller reasons about the set, and a swap should not reorder the rest.
        if let Some(slot) = self.instruments.get_mut(at) {
            *slot = new;
        }
        Ok(SubscribeSwap {
            unsubscribe: Some(old),
            subscribe: Some(new),
        })
    }
}

/// The wire work one [`SubscribeGuard::try_swap`] implies.
///
/// Both fields are `Option` for the same reason: the no-op swap (an
/// instrument replaced by itself) must be expressible as "send nothing"
/// rather than as an error or as an empty `Vec` the caller might loop over
/// and mistake for work. That case is the COMMON one — every minute the
/// at-the-money strike has not moved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SubscribeSwap {
    /// Send this FIRST. See [`SubscribeGuard::try_swap`] for why the order is
    /// not a preference.
    pub unsubscribe: Option<SubscribeInstrument>,
    /// Send this SECOND, and only after the unsubscribe has gone out.
    pub subscribe: Option<SubscribeInstrument>,
}

impl SubscribeSwap {
    /// Nothing to send — the socket already carries what was asked for.
    pub const NO_OP: Self = Self {
        unsubscribe: None,
        subscribe: None,
    };

    /// `true` when this swap implies no wire traffic at all.
    #[must_use]
    pub const fn is_no_op(&self) -> bool {
        self.unsubscribe.is_none() && self.subscribe.is_none()
    }
}

/// How long ONE wire call of a swap may occupy the drain task.
///
/// **THIS BOUND IS THE POINT, and without it the swap is dangerous.** Both
/// wire calls run ON the drain task — the same task that reads frames off the
/// socket — and the transport's own `SUBSCRIBE_SEND_TIMEOUT` is **10
/// seconds**. An unsubscribe followed by a subscribe would therefore be a
/// worst case of **twenty seconds** of drain occupancy, on the very socket the
/// swap exists to keep useful. During a stall the socket receive buffer fills,
/// and Dhan's published architecture skips a slow consumer forward to "the
/// latest available state" — so the ticks in between are dropped at THEIR
/// side, with no sequence number for us to detect it. That is tick loss caused
/// by the mechanism meant to improve coverage.
///
/// One second each, so a whole swap is bounded at two. A subscribe message is
/// a few hundred bytes; a socket that cannot write one in a second is sick,
/// and the reconnect ladder is the right answer to a sick socket — not
/// waiting nine more seconds on the drain.
///
/// At the ~5,000 packet/sec envelope two seconds is ~10,000 packets, inside
/// the socket receive buffer. Once a minute per socket, and only on a REAL
/// at-the-money change — the edge-triggered tracker upstream means an ordinary
/// minute costs zero wire calls and zero stall.
// APPROVED: this line IS the named constant the no-hardcoded-Duration rule asks for; the scanner matches the declaration itself.
pub const SWAP_WIRE_BUDGET: Duration = Duration::from_secs(1);

/// How long a WHOLE live top-up may occupy the drain task.
///
/// `SWAP_WIRE_BUDGET` above bounds ONE wire call and its doc opens with "THIS
/// BOUND IS THE POINT, and without it the swap is dangerous" -- because two
/// unbounded calls would be twenty seconds of drain occupancy. Every word of
/// that applies to the top-up with far more force, and the top-up shipped with
/// no bound at all.
///
/// The arithmetic it was missing: `try_extend` admits up to the endpoint's
/// 5,000-instrument cap, so a connection holding ~850 spot instruments can be
/// topped up by ~4,150 -- **42 subscribe messages**, each bounded only by the
/// transport's own 10-second `SUBSCRIBE_SEND_TIMEOUT`. That is a worst case of
/// **420 seconds** of not polling `recv()`. Dhan closes a silent socket after
/// [`DHAN_SERVER_CLOSE_AFTER_SILENCE_SECS`] = 40, and the automatic pong is
/// only emitted while `recv()` is polling -- so **four slow sends already
/// exceed the close**, and the mechanism that exists to widen coverage takes
/// the socket down instead.
///
/// Five seconds, because a healthy top-up costs ~1.1: 42 messages spaced by
/// [`SUBSCRIBE_BATCH_INTERVAL`] (25 ms) is 1.05 s of deliberate pacing, and a
/// few-hundred-byte write on a healthy socket is sub-millisecond. So this is
/// ~4.5x the real cost and ~8x under the server close. A top-up that cannot
/// finish inside it is on a sick socket, and the right answer to a sick socket
/// is the reconnect ladder -- not four hundred more seconds on the drain.
///
/// On exhaustion the guard is TRUNCATED to what actually reached the wire, so
/// it never claims instruments the socket does not hold. That differs from the
/// send-failure arm, which deliberately leaves the guard wide because a dying
/// socket is about to replay its whole set anyway. A budget stop is the
/// opposite case: the socket is alive and staying alive, so the guard must
/// stay truthful or a later reconnect replays a subscription Dhan rejects.
// APPROVED: this line IS the named constant the no-hardcoded-Duration rule asks for; the scanner matches the declaration itself.
pub const TOPUP_WIRE_BUDGET: Duration = Duration::from_secs(5);

/// A change to what a LIVE socket is subscribed to, sent while it is up.
///
/// One channel carries both shapes deliberately. Two optional receivers on
/// the connection driver would each need their own null check in the drain
/// loop, and — worse — could deliver an `Extend` and a `Swap` in an order
/// neither sender chose. One channel gives the sequence the sender wrote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LiveSubscriptionCommand {
    /// Add instruments to a set already on the wire.
    ///
    /// The original reason a live command channel exists at all: reaching the
    /// ~4,150 slots stranded on the boot-dialed spot connection, which is
    /// exactly what the authorized ATM window was short of. See
    /// [`SubscribeGuard::try_extend`].
    Extend(Vec<SubscribeInstrument>),
    /// Replace one instrument with another, without re-dialing.
    ///
    /// The per-minute at-the-money re-selection for depth-200 and depth-20.
    /// See [`SubscribeGuard::try_swap`] — in particular why the unsubscribe
    /// must go out before the subscribe.
    Swap {
        /// The instrument to drop. Must be one this connection holds, or the
        /// command is refused fail-closed.
        old: SubscribeInstrument,
        /// The instrument to take its place.
        new: SubscribeInstrument,
    },
}

// ---------------------------------------------------------------------------
// Frame sink — THE ONE RULE, made executable
// ---------------------------------------------------------------------------

/// What happened to one captured frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameSinkOutcome {
    /// QUEUED for durable write, and handed to the ring.
    ///
    /// 2026-08-14 correction: this said "Durable in the WAL". It is not.
    /// `append_with_seq` returns as soon as the record is on the writer
    /// thread's bounded channel — its own doc says "queued for durable write"
    /// — so a `SIGKILL` or host power loss discards everything still on that
    /// channel plus the writer's buffer, ALL of it already reported here as
    /// captured. There is no `fsync` anywhere on this path, deliberately: it
    /// is a throughput trade, not an oversight. Naming the outcome for the
    /// stronger property was the oversight.
    Captured,
    /// Queued for durable write; the bounded ring refused it because the
    /// downstream consumer is behind.
    ///
    /// 2026-08-14 correction: this said "NOT capture loss — replay recovers
    /// it." That was FALSE AS SHIPPED and had been since the live lane was
    /// revived — boot replay DROPS every live-feed frame, because no re-fold
    /// path exists. The drain-side log line has always said so honestly; this
    /// doc did not, and a comment claiming recovery is worse than no comment,
    /// because it tells the next reader not to look.
    ///
    /// Until a re-fold exists, a ring refusal is PERMANENT LOSS and is alarmed
    /// as such (`tv-<env>-ws-ring-full`).
    RingFull,
    /// The WAL refused it outright — the queue itself was full, so the frame
    /// reached neither disk nor the ring. Genuine, immediate capture loss.
    WalDropped,
}

/// The only thing a socket read task is permitted to do with a frame.
///
/// One method, no `async`: an `async fn` here would let a future implementation
/// `.await` inside the read loop, which is exactly the disconnect mechanism
/// described in the module docs. The signature is the enforcement.
pub trait FrameSink: Send + Sync + 'static {
    /// Accepts one raw frame. MUST NOT block, allocate, or await.
    fn accept(&self, frame: Bytes) -> FrameSinkOutcome;

    /// Reports a socket LIFECYCLE transition — connected, lost, parked.
    ///
    /// Default is a no-op, so every existing implementation and every call
    /// site is unchanged by construction.
    ///
    /// # Why this hangs off the sink rather than a new parameter
    ///
    /// `run_connection` is generic over four type parameters and is called
    /// from tests, benches and the live stack. Threading an audit channel
    /// through it would touch every one of those for a concern none of them
    /// has. The sink is ALREADY the one app-owned object the supervisor
    /// holds, and it already knows which socket it serves — so it is the
    /// natural place to hang a per-socket side-channel.
    ///
    /// # Why this may allocate when `accept` may not
    ///
    /// `accept` runs per FRAME, thousands per second, between two `recv()`
    /// calls — the window in which the automatic pong is not being emitted.
    /// This runs per socket LIFECYCLE EVENT: a handful of times a day, and
    /// never while a frame is waiting. It is the same cold-path budget the
    /// order-update socket's audit emit has had since 2026-07-05.
    ///
    /// Implementations must still not block or await. The house pattern is a
    /// `try_send` onto a bounded channel: a full channel costs a forensic
    /// row, never a stalled socket.
    fn on_lifecycle(
        &self,
        _kind: tickvault_common::ws_event_types::WsEventKind,
        _reason: &'static str,
    ) {
    }
}

/// One socket lifecycle transition, WITHOUT a timestamp.
///
/// # Why there is no timestamp on this
///
/// `test_pool_supervisor_source_never_reads_the_wall_clock` bans every
/// wall-clock call in this file, because the supervisor's ladder, its token
/// expiry and its backoff are all monotonic — an NTP step must be unable to
/// expire all sixteen sockets at once. That ban is a blanket one on purpose,
/// so nobody has to re-litigate it per call site, and an audit row's
/// timestamp is not special enough to earn a carve-out.
///
/// So the socket reports WHAT happened and the app crate — which already
/// reads the wall clock for exactly this, and already owns the IST offset
/// convention — stamps WHEN. Every field here is `Copy`, so building one
/// allocates nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WsLifecycleEvent {
    /// Which endpoint's pool the socket belongs to. The row's `ws_type` is
    /// derived from THIS, never from the sink's `ws_type` field — that one is
    /// the WAL record discriminant and reads `LiveFeed` for all fifteen
    /// market-data sockets, so a row built from it would file a depth-200
    /// park under the main feed.
    pub endpoint: DhanEndpointType,
    /// The socket's own index, so one sick connection is not averaged away
    /// by its siblings.
    pub connection_index: u8,
    /// Connected / Disconnected.
    pub kind: tickvault_common::ws_event_types::WsEventKind,
    /// A fixed machine cause slug. `&'static str` deliberately: it keeps this
    /// type `Copy`, and it means no caller can put unbounded vendor text into
    /// a forensic row.
    pub reason: &'static str,
}

/// One captured frame and the sequence it was stamped with at the read
/// instant.
///
/// The sequence travels WITH the bytes rather than being re-derived
/// downstream, and that is the whole design. `next_frame_seq` is minted
/// exactly once per received frame and written into the WAL record; the
/// consumer must stamp `ticks.capture_seq` with the SAME value, because on
/// replay the WAL hands back its stored sequence. A consumer that minted its
/// own would produce a different key for a replayed frame and the DEDUP
/// collapse that makes replay idempotent would never happen — every restart
/// would silently duplicate the session (`data-integrity.md`, TICK-SEQ-01).
#[derive(Debug, Clone)]
pub struct CapturedFrame {
    /// The replay-stable sequence, already persisted in the WAL record.
    pub seq: u64,
    /// Which socket this frame came off.
    ///
    /// Load-bearing, not decoration: the main feed and the depth feeds use
    /// DIFFERENT wire formats — an 8-byte header versus a 12-byte one whose
    /// first two bytes are a message length. A consumer that cannot tell them
    /// apart will feed a depth frame to the main-feed parser, where byte 0 is a
    /// length low-byte matching no response code, and silently discard every
    /// depth packet as unparseable. Carrying the endpoint makes that mistake
    /// impossible to make by accident.
    pub endpoint: DhanEndpointType,
    /// WHICH socket of that endpoint's pool, `0..MAX_TOTAL_DHAN_CONNECTIONS`.
    ///
    /// `endpoint` alone cannot answer "is ONE socket sick?" — it folds all five
    /// main-feed connections into a single identity, so a socket delivering
    /// minutes-late ticks would be averaged away by its four healthy siblings.
    /// A `u8` alongside the existing enum adds no allocation: `Bytes` remains
    /// the only heap member of this struct.
    pub connection_index: u8,
    /// The monotonic instant this frame came off the socket.
    ///
    /// Stamped in the READ TASK, and monotonic on purpose — both halves
    /// matter.
    ///
    /// **Why stamped here:** until 2026-08-18 the drain called the clock
    /// itself and used that as the receive time, which measured
    /// `Dhan's delivery + OUR time queued in the ring`. Those are different
    /// quantities with different owners. Under a fold stall the ring backs up
    /// and the drain falls behind, so every lag sample inflated precisely when
    /// the cause was LOCAL — the lag alarm would fire hardest at our own
    /// backlog while naming the vendor for it.
    ///
    /// **Why `Instant` and not a wall-clock stamp:** this module is forbidden
    /// from reading the wall clock at all, and that ban is load-bearing rather
    /// than stylistic — an NTP step must be unable to expire all sixteen
    /// sockets at once (`test_pool_supervisor_source_never_reads_the_wall_clock`).
    /// A monotonic stamp respects it, and is strictly better anyway: the
    /// consumer derives the wall-clock receipt instant by subtracting
    /// `elapsed()` from its own clock read, so a clock step landing between
    /// receipt and fold cannot corrupt the measured lag.
    ///
    /// Costs one vDSO read per frame on a task that already performs one for
    /// the watchdog. No allocation: `Bytes` remains the only heap member.
    pub received_at: std::time::Instant,
    /// The frame exactly as it arrived. Never parsed on the read task.
    pub bytes: Bytes,
}

/// The ring's SECOND bound: total bytes resident, not just frame count.
///
/// A channel bounded only by count is bounded only if the items are a known
/// size, and these are not. `CapturedFrame` owns a `Bytes` whose length is
/// whatever the peer sent, up to `max_frame_bytes(endpoint)` — 256 KiB on the
/// main feed, 512 KiB on depth-200. At the ring's 65,536-frame capacity that
/// is **16 GiB of resident heap on the main feed alone, 32 GiB with the depth
/// pools open**: the entire machine, held by a queue whose own documentation
/// called it a bounded burst absorber.
///
/// It never bites in normal operation, which is exactly why it is worth
/// bounding. Real Dhan frames are small — a Quote packet is 50 bytes and a
/// frame batches a handful — so the count bound engages first and this budget
/// is inert. It engages only when frames are large AND the fold has stalled,
/// which is the shape of both a hostile peer and a genuine downstream stall,
/// and in that shape the count bound alone permits the process to eat the host.
///
/// Refusal here is the SAME event as a count-full ring, not a new failure mode:
/// the frame is already durable in the WAL by the time this is consulted, so a
/// refusal is a lag signal and the outcome is `RingFull`.
#[derive(Debug)]
pub struct RingByteBudget {
    resident: std::sync::atomic::AtomicUsize,
    cap: usize,
}

impl RingByteBudget {
    /// A budget capped at `cap` resident bytes.
    #[must_use]
    pub const fn new(cap: usize) -> Self {
        Self {
            resident: std::sync::atomic::AtomicUsize::new(0),
            cap,
        }
    }

    /// The configured ceiling.
    #[must_use]
    pub const fn cap(&self) -> usize {
        self.cap
    }

    /// Bytes currently reserved by frames sitting in the ring.
    #[must_use]
    pub fn resident(&self) -> usize {
        self.resident.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Reserves `len` bytes, or refuses.
    ///
    /// CAS loop rather than a `fetch_add`-then-check: `fetch_add` would
    /// momentarily push `resident` past the cap, and with N reader tasks all
    /// adding at once the overshoot is unbounded — the very thing being
    /// bounded. This way `resident` never exceeds `cap`, even transiently.
    ///
    /// A frame LARGER than the whole cap can never be admitted. That is
    /// deliberate: the per-endpoint frame caps are two to three orders of
    /// magnitude above real traffic, so such a frame is already outside the
    /// envelope, and admitting it would mean the budget does not bound.
    pub fn try_reserve(&self, len: usize) -> bool {
        self.resident
            .fetch_update(
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Relaxed,
                |cur| {
                    let next = cur.checked_add(len)?;
                    (next <= self.cap).then_some(next)
                },
            )
            .is_ok()
    }

    /// Returns `len` bytes to the budget as a frame leaves the ring.
    ///
    /// Saturating, never wrapping. An underflow here would be a bookkeeping
    /// bug, and the wrong response to one is to wrap to `usize::MAX` and refuse
    /// every frame forever — that converts a counting error into a total feed
    /// outage. Saturating at zero degrades to "the budget stops bounding",
    /// which is the same state as not having it, and stays visible through
    /// `resident()`.
    pub fn release(&self, len: usize) {
        // An explicit CAS loop rather than `fetch_update`, because the closure
        // form can only ever return `Some` here and its `Result` would then have
        // to be discarded — and `let _ =` on a `#[must_use]` value is exactly
        // what this crate's `let_underscore_must_use` deny exists to stop. The
        // loop has no result to throw away.
        let mut cur = self.resident.load(std::sync::atomic::Ordering::Relaxed);
        loop {
            let next = cur.saturating_sub(len);
            match self.resident.compare_exchange_weak(
                cur,
                next,
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Relaxed,
            ) {
                Ok(_) => return,
                Err(observed) => cur = observed,
            }
        }
    }
}

/// Production [`FrameSink`]: append to the write-ahead log, then push into the
/// bounded ring. In that order, always.
///
/// The order is the durability guarantee. The WAL append happens BEFORE the
/// frame is visible to anything downstream, so a process kill between the two
/// steps loses nothing — replay finds the frame. A ring that is full is
/// therefore a lag signal, not a loss event.
pub struct WalRingSink {
    spill: std::sync::Arc<WsFrameSpill>,
    ring: tokio::sync::mpsc::Sender<CapturedFrame>,
    budget: std::sync::Arc<RingByteBudget>,
    ws_type: WsType,
    endpoint: DhanEndpointType,
    /// Which socket of the pool this sink serves; stamped onto every frame
    /// so a single sick connection cannot be averaged away by its siblings.
    connection_index: u8,
    /// Loss counters resolved ONCE at construction — see the note on
    /// [`WalRingSink::new`] for why the macro form is banned on this path.
    wal_dropped: metrics::Counter,
    ring_full: metrics::Counter,
    ring_bytes_full: metrics::Counter,
    /// Optional forensic side-channel for socket lifecycle events.
    ///
    /// `None` by default and set only by [`WalRingSink::with_audit`], so a
    /// sink built the old way behaves exactly as before — no channel, no
    /// rows, no cost.
    audit_tx: Option<tokio::sync::mpsc::Sender<WsLifecycleEvent>>,
}

impl WalRingSink {
    /// Wires a sink to a WAL, a bounded ring, and the ring's byte budget.
    ///
    /// # Why the counters are resolved here and not at the emit site
    ///
    /// `metrics::counter!(NAME, "label" => value)` builds a `Key` on EVERY
    /// call, and a keyed `Key` owns a `Vec<Label>` — so the macro form heap
    /// allocates once per invocation. Putting it on `accept`'s drop paths puts
    /// an allocation on the one path that only executes when the system is
    /// ALREADY under pressure: the ring is full, the WAL refused, the process
    /// is losing data. Allocating there is both a violation of principle 1
    /// (zero allocation on the hot path) and, practically, the worst possible
    /// moment to ask the allocator for anything.
    ///
    /// The `endpoint` label is fixed for the lifetime of a sink, so a single
    /// handle per sink covers every emit — no `OnceLock`, no per-call lookup.
    /// `Counter::increment` on a resolved handle is a plain atomic add: O(1),
    /// zero allocation. Same shape as `DrainCounters` in the app crate and
    /// `dispatcher.rs`'s pre-resolved handles.
    #[must_use]
    pub fn new(
        spill: std::sync::Arc<WsFrameSpill>,
        ring: tokio::sync::mpsc::Sender<CapturedFrame>,
        budget: std::sync::Arc<RingByteBudget>,
        ws_type: WsType,
        endpoint: DhanEndpointType,
        connection_index: u8,
    ) -> Self {
        let endpoint_label = endpoint.as_str();
        let sink = Self {
            spill,
            ring,
            budget,
            ws_type,
            endpoint,
            connection_index,
            wal_dropped: metrics::counter!(WAL_DROP_METRIC, "endpoint" => endpoint_label),
            ring_full: metrics::counter!(RING_FULL_METRIC, "endpoint" => endpoint_label),
            ring_bytes_full: metrics::counter!(RING_BYTES_FULL_METRIC, "endpoint" => endpoint_label),
            audit_tx: None,
        };
        sink.pre_register();
        sink
    }

    /// Attaches the `ws_event_audit` side-channel to this socket.
    ///
    /// A builder rather than a `new` parameter so every existing construction
    /// — tests, benches, the DHAT gates — is untouched, and opting in is one
    /// visible line at the boot site.
    #[must_use]
    pub fn with_audit(mut self, tx: tokio::sync::mpsc::Sender<WsLifecycleEvent>) -> Self {
        self.audit_tx = Some(tx);
        self
    }

    /// Publishes a zero on every loss series this sink owns.
    ///
    /// # Why a zero has to be published at all
    ///
    /// The CloudWatch agent computes a counter's alarm value as a DELTA
    /// between consecutive samples, and it has no previous sample for a series
    /// that has never been emitted — so it drops the first one. If the first
    /// emission a series ever sees is the outage itself, that outage is the
    /// dropped sample and the alarm does not fire for it. Publishing a zero at
    /// construction makes the harmless zero the dropped sample instead, which
    /// is the whole point.
    ///
    /// Called from `new` so it cannot be forgotten at a call site: a sink that
    /// exists has published its baseline.
    fn pre_register(&self) {
        self.wal_dropped.increment(0);
        self.ring_full.increment(0);
        self.ring_bytes_full.increment(0);
    }
}

impl FrameSink for WalRingSink {
    fn on_lifecycle(
        &self,
        kind: tickvault_common::ws_event_types::WsEventKind,
        reason: &'static str,
    ) {
        let Some(tx) = self.audit_tx.as_ref() else {
            return;
        };
        // No allocation and no clock read: every field is `Copy`. The app
        // crate stamps the time and widens this into the audit row.
        let event = WsLifecycleEvent {
            endpoint: self.endpoint,
            connection_index: self.connection_index,
            kind,
            reason,
        };
        // `try_send`, never `send`: a slow consumer may cost a forensic row,
        // and must never stall a socket. The drop is COUNTED rather than
        // swallowed — a silently lost audit row is the same false-OK the
        // whole table exists to prevent.
        if tx.try_send(event).is_err() {
            metrics::counter!("tv_ws_event_audit_dropped_total", "reason" => "live_feed_lifecycle")
                .increment(1);
        }
    }

    fn accept(&self, frame: Bytes) -> FrameSinkOutcome {
        // Stamped FIRST, before the WAL append and before the budget check.
        // This is the frame's arrival instant; every microsecond of our own
        // work after this line must NOT be charged to the vendor.
        // Monotonic, never wall-clock — see `CapturedFrame::received_at`.
        let received_at = Instant::now();
        // Minted ONCE, here, at the read instant — see `CapturedFrame`.
        let seq = next_frame_seq();
        // Step 1 — durability. `Bytes` into the WAL is an Arc refcount bump.
        //
        // `append_with_seq_at`, never `append_with_seq`: the receipt is the
        // whole point of the TVW3 record and it is DERIVED from `received_at`
        // above, not read from a clock here — this file bans wall-clock reads
        // (`test_pool_supervisor_source_never_reads_the_wall_clock`) and that
        // ban is right, because an NTP step must not be able to expire all
        // sixteen sockets at once.
        //
        // Before this was wired, `append_with_seq` passed the UNKNOWN sentinel
        // and `append_with_seq_at` had zero production callers, so every record
        // on disk carried 0 while the format claimed to carry a receipt — and
        // replay re-stamped `now()`, which is the moment of REPLAY, not of
        // arrival. A shape that advertises a guarantee it never delivers is
        // worse than one that does not advertise it.
        //
        // `append_with_seq`, never `append`: `append` would mint a SECOND
        // sequence internally and the WAL record would then disagree with the
        // one the consumer stamps.
        if self.spill.append_with_seq_at(
            self.ws_type,
            // APPROVED: `Bytes::clone` is an atomic refcount increment, NOT a copy of the frame payload — the whole point of `Bytes` on this path.
            frame.clone(),
            seq,
            tickvault_storage::ws_frame_spill::receipt_nanos_from(received_at),
        ) == AppendOutcome::Dropped
        {
            self.wal_dropped.increment(1);
            return FrameSinkOutcome::WalDropped;
        }
        // Step 2 — byte budget. Consulted BEFORE `try_send` because a reserve
        // taken after a successful send could not be refused, and one taken
        // for a send that then fails would leak. Reserve, then send, then
        // release on failure: the only ordering with no window in which the
        // budget and the ring disagree.
        let len = frame.len();
        if !self.budget.try_reserve(len) {
            self.ring_full.increment(1);
            self.ring_bytes_full.increment(1);
            return FrameSinkOutcome::RingFull;
        }
        // Step 3 — visibility. `try_send` never awaits; a full ring returns
        // immediately so the reader keeps polling (and therefore keeps ponging).
        if self
            .ring
            .try_send(CapturedFrame {
                seq,
                endpoint: self.endpoint,
                connection_index: self.connection_index,
                received_at,
                bytes: frame,
            })
            .is_err()
        {
            // The frame never entered the ring, so nothing downstream will ever
            // release its reservation. Give it back here or the budget ratchets
            // down on every count-full frame until it refuses everything —
            // a slow strangulation that would look like the feed dying for no
            // reason.
            self.budget.release(len);
            self.ring_full.increment(1);
            return FrameSinkOutcome::RingFull;
        }
        FrameSinkOutcome::Captured
    }
}

// ---------------------------------------------------------------------------
// Pool supervisor — N connections of one endpoint type
// ---------------------------------------------------------------------------

/// Owns the connection budget and the per-connection supervisors for a whole
/// pool.
///
/// Plain value with no interior mutability: it lives on the pool's own task.
/// Keeping it lock-free is what lets the per-frame path stay lock-free.
#[derive(Debug)]
pub struct PoolSupervisor {
    budget: PoolBudget,
    connections: Vec<ConnectionSupervisor>,
}

impl PoolSupervisor {
    /// An empty supervisor with a fresh budget.
    #[must_use]
    pub fn new() -> Self {
        // Publish a zero on every (endpoint, reason) series the park counter
        // can ever produce, BEFORE any socket is admitted.
        //
        // The CloudWatch agent computes a counter's alarm value as the DELTA
        // between consecutive samples and has no previous sample for a series
        // it has never seen, so it drops the first one. A park happens at most
        // once per connection per session and is otherwise never emitted — so
        // without this, the FIRST park a series ever sees IS the dropped
        // sample, it publishes no datapoint, and `tv-<env>-dhan-socket-parked`
        // (threshold 1, one 300s period) never fires for it. The alarm would
        // be dead precisely for the single-park case, which is the normal
        // shape of the incident it exists to catch.
        //
        // Both labels must be enumerated, not just one: the agent baselines
        // per Prometheus SERIES, and the EMF processor folds the labels to
        // `{host}` afterwards by summing the per-series deltas. A series left
        // unregistered contributes nothing to that sum on the sample where it
        // is born, so one missing combination is one invisible park.
        //
        // Twelve series at the /metrics endpoint, ONE series in CloudWatch
        // after folding — so this costs nothing on the bill.
        //
        // Done in `new` so it cannot be forgotten at a call site: a supervisor
        // that exists has published its baseline. Same discipline as
        // `WalRingSink::pre_register` and `SpillDropCounters::new`.
        for endpoint in DhanEndpointType::ALL {
            for reason in ParkReason::ALL {
                metrics::counter!(
                    PARK_METRIC,
                    "endpoint" => endpoint.as_str(),
                    "reason" => reason.as_str(),
                )
                .increment(0);
            }
            // Same baseline discipline for the flap damper. Only the verdicts
            // that are actually EMITTED are registered — pre-registering
            // `ladder` would publish a series that can never move, which is a
            // different flavour of the same lie.
            for verdict in FlapVerdict::ALL {
                if verdict.is_damped() {
                    metrics::counter!(
                        FLAP_DAMPED_METRIC,
                        "endpoint" => endpoint.as_str(),
                        "verdict" => verdict.as_str(),
                    )
                    .increment(0);
                }
            }
        }
        Self {
            budget: PoolBudget::new(),
            // Pre-sized to the hard ceiling rather than left unsized: the pool
            // can never exceed MAX_TOTAL_DHAN_CONNECTIONS, so one small
            // allocation here means the connection table never reallocates.
            connections: Vec::with_capacity(MAX_TOTAL_DHAN_CONNECTIONS as usize),
        }
    }

    /// Reserves one connection of `endpoint` and registers a supervisor for it.
    ///
    /// Refusal is the fail-closed direction: better fifteen connections and a
    /// loud refusal than sixteen and a silently murdered pool member (see
    /// [`DisconnectClass::PoolOverflow`]).
    ///
    /// # Errors
    /// Whatever [`PoolBudget::try_open`] refuses with.
    pub fn admit(
        &mut self,
        endpoint: DhanEndpointType,
        now: Instant,
    ) -> Result<ConnectionSlot, PoolBudgetRefusal> {
        let slot = self.budget.try_open(endpoint)?;
        self.connections.push(ConnectionSupervisor::new(slot, now));
        Ok(slot)
    }

    /// Connections currently registered.
    #[must_use]
    pub fn len(&self) -> usize {
        self.connections.len()
    }

    /// Whether nothing is registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.connections.is_empty()
    }

    /// Total connections open across every endpoint type.
    #[must_use]
    pub const fn total_open(&self) -> u16 {
        self.budget.total_open()
    }

    /// Connections open for one endpoint type.
    #[must_use]
    pub const fn open_count(&self, endpoint: DhanEndpointType) -> u8 {
        self.budget.open_count(endpoint)
    }

    /// Mutable access to the supervisor at `global_index`, if registered.
    pub fn connection_mut(
        &mut self,
        global_index: ConnectionId,
    ) -> Option<&mut ConnectionSupervisor> {
        self.connections
            .iter_mut()
            .find(|c| c.slot().global_index == global_index)
    }

    /// Read-only view of every registered supervisor.
    #[must_use]
    pub fn connections(&self) -> &[ConnectionSupervisor] {
        &self.connections
    }

    /// Runs the idle watchdog across every registered connection, returning the
    /// slots that need re-dialing.
    ///
    /// **O(N), not O(1)** — a scan. N is bounded by the operator-authorized
    /// ceiling of 16, so the cost is a fixed constant, but it is a scan and is
    /// labelled as one rather than dressed up. It runs once per
    /// [`IDLE_POLL_INTERVAL`] on the cold path, never per frame. The returned
    /// `Vec` allocates; also cold path, and empty in the overwhelmingly common
    /// case where nothing timed out.
    pub fn poll_all(&mut self, now: Instant) -> Vec<(ConnectionSlot, SupervisorAction)> {
        // Pre-sized to the ceiling: at most one action per connection, and the
        // pool is hard-capped, so this never reallocates mid-sweep.
        let mut due = Vec::with_capacity(MAX_TOTAL_DHAN_CONNECTIONS as usize);
        for conn in &mut self.connections {
            let action = conn.poll(now);
            if action != SupervisorAction::Continue {
                due.push((conn.slot(), action));
            }
        }
        due
    }

    /// Releases one connection of `endpoint` back to the budget and forgets its
    /// supervisor.
    ///
    /// # Deliberately UNCALLED in production, and that is not an oversight
    ///
    /// A park is PERMANENT by design — the park arm of `run_supervised_socket`
    /// says so in as many words, and nothing in the tree re-dials a parked
    /// socket. So retiring one would hand a budget slot back that no caller
    /// ever asks for. A 2026-08-21 sweep reported the missing call as "a
    /// permanent silent loss of one of the 16 authorized sockets per park";
    /// that reading is wrong in the direction that matters. The socket is lost
    /// to the PARK. Releasing its budget entry would not bring it back,
    /// because nothing dials a replacement.
    ///
    /// It is kept rather than deleted because it is the primitive a future
    /// re-dial-after-park feature needs, and it is now safe to call.
    ///
    /// # Idempotent since 2026-08-21
    ///
    /// It previously released the budget UNCONDITIONALLY, even when `retain`
    /// removed nothing — an unknown slot, or the same slot retired twice. Since
    /// `PoolBudget::release` saturates at zero it cannot underflow and report
    /// the problem; it would simply undercount, and the pool would then admit
    /// a connection PAST the 5-per-endpoint cap. That is the one invariant the
    /// 16-connection lock rests on, so a dormant function that breaks it when
    /// called is worse than one that does nothing: it makes wiring it later a
    /// trap.
    ///
    /// Now the budget moves only if a connection was actually removed.
    pub fn retire(&mut self, slot: ConnectionSlot) {
        let before = self.connections.len();
        self.connections
            .retain(|c| c.slot().global_index != slot.global_index);
        if self.connections.len() < before {
            self.budget.release(slot.endpoint);
        }
    }
}

impl Default for PoolSupervisor {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// The thin async shell
// ---------------------------------------------------------------------------

/// What a socket read returned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SocketEvent {
    /// One raw frame, exactly as it arrived. Never parsed here.
    Frame(Bytes),
    /// A control frame proving the PEER IS ALIVE, carrying no market data —
    /// a WebSocket Ping or Pong (and any text/raw control frame Dhan sends).
    ///
    /// This exists because omitting it cost ~300 self-inflicted reconnects
    /// every trading morning (measured on prod, 2026-08-19: 8–25 per minute
    /// from 08:31 to 08:59 IST, then ZERO once real ticks began at 09:15).
    /// The pre-2026-08-19 read loop counted a Ping into a metric and then
    /// LOOPED without returning anything, so the supervisor never learned the
    /// socket was alive, the idle watchdog was never reset, and at
    /// `IDLE_RECONNECT_TIMEOUT_SECS` we tore down a perfectly healthy
    /// connection — re-authenticating and re-subscribing the whole universe
    /// against a broker whose own docs warn that too many requests "may
    /// result in user being blocked".
    ///
    /// Deliberately DISTINCT from [`SocketEvent::Frame`]: a ping proves
    /// TRANSPORT liveness, never DATA liveness. It must reset the watchdog
    /// and must NOT count as a data frame or mark the connection
    /// proven-healthy — a socket that only ever pings has delivered nothing,
    /// and that condition belongs to the silence scan (RISK-GAP-03), which
    /// measures exactly it and is market-hours gated so the legitimately
    /// silent pre-open never pages.
    KeepAlive,
    /// The socket closed, optionally with a Dhan disconnect code.
    Closed { code: Option<DisconnectCode> },
}

/// The transport a supervised connection drives.
///
/// Deliberately a trait rather than a concrete socket: it keeps every branch of
/// [`run_connection`] — dial failure, subscribe failure, 805, token staleness,
/// idle timeout, shutdown — reachable from a unit test with no network. The
/// production implementation (tokio-tungstenite over TLS) is a separate module
/// and is NOT part of this round.
///
/// Returns are written as `impl Future + Send` rather than `async fn` so the
/// `Send` bound is explicit at the definition, which is what lets a supervised
/// connection be `tokio::spawn`ed.
pub trait DhanFeedSocket: Send {
    /// Dial and complete the handshake.
    fn connect(&mut self) -> impl std::future::Future<Output = Result<(), SocketFailure>> + Send;
    /// Send ONE subscribe message for the given batch.
    fn send_subscribe(
        &mut self,
        batch: &[SubscribeInstrument],
    ) -> impl std::future::Future<Output = Result<(), SocketFailure>> + Send;
    /// Send ONE unsubscribe message for the given batch.
    ///
    /// **ADDED 2026-08-26** for the per-minute at-the-money re-selection. It
    /// has a default implementation that REFUSES rather than silently
    /// succeeding: a transport that cannot unsubscribe must fail the swap
    /// loudly, because the alternative — reporting success and leaving the old
    /// instrument subscribed — puts a depth-200 connection over its
    /// one-instrument limit on the very next subscribe, and Dhan answers that
    /// with a Fatal 804.
    fn send_unsubscribe(
        &mut self,
        batch: &[SubscribeInstrument],
    ) -> impl std::future::Future<Output = Result<(), SocketFailure>> + Send {
        let _ = batch;
        async { Err(SocketFailure) }
    }
    /// Send ONE client-originated keepalive Ping.
    ///
    /// Required, not defaulted, deliberately: a default that quietly returned
    /// `Ok(())` would let a transport claim keepalive support it does not have,
    /// and the failure would look exactly like a healthy socket. The compiler
    /// asking every implementor is the point.
    ///
    /// Driven ONLY for endpoints where
    /// [`DhanEndpointType::needs_client_keepalive_ping`] is true. The watchdog
    /// reset stays on the RECEIVED pong, never on this send.
    fn send_ping(&mut self) -> impl std::future::Future<Output = Result<(), SocketFailure>> + Send;
    /// Await the next socket event. This is the call that keeps the automatic
    /// pong flowing — nothing may be done between two of these but
    /// [`FrameSink::accept`].
    fn recv(&mut self) -> impl std::future::Future<Output = SocketEvent> + Send;
    /// Close the socket, best effort.
    fn close(&mut self) -> impl std::future::Future<Output = ()> + Send;
}

/// A transport-level failure. Opaque on purpose: policy lives in the
/// supervisor, not in the transport.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("dhan socket failure")]
pub struct SocketFailure;

/// Why a supervised connection loop returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionExit {
    /// The supervisor parked the connection.
    Parked(ParkReason),
}

/// Drives ONE connection for its whole life: dial, subscribe, drain, reconnect,
/// park.
///
/// Contains no policy — every decision comes from `supervisor`. The inner drain
/// loop does exactly two things per frame: [`FrameSink::accept`], and one
/// `Instant::now()` to feed the watchdog. It never parses, never writes to a
/// database, never takes a lock, and never awaits anything but the socket
/// itself and the one-second idle tick.
///
/// `refresh_token` is invoked before re-dialing after a token-staleness
/// disconnect; supply a no-op when the transport does not carry a token.
// Every decision this executes is unit-tested via `ConnectionSupervisor`, and
// the loop itself is driven end-to-end against a fake transport by the
// `test_run_connection_*` cases below — dial retry, subscribe failure, 805,
// token staleness and the already-parked entry are each covered.
pub async fn run_connection<S, K, F, Fut>(
    socket: S,
    supervisor: ConnectionSupervisor,
    guard: SubscribeGuard,
    sink: std::sync::Arc<K>,
    refresh_token: F,
) -> ConnectionExit
where
    S: DhanFeedSocket,
    K: FrameSink + ?Sized,
    F: FnMut() -> Fut + Send,
    Fut: std::future::Future<Output = ()> + Send,
{
    run_connection_with_commands(socket, supervisor, guard, sink, refresh_token, None).await
}

/// [`run_connection`] plus a channel that can change the LIVE subscription
/// while the socket is up.
///
/// Two commands ride it, and they exist for different reasons.
/// [`LiveSubscriptionCommand::Extend`] is the only way to reach the ~4,150
/// slots stranded on the boot-dialed spot connection — exactly what the
/// authorized ATM ± 25 window was short of. [`LiveSubscriptionCommand::Swap`]
/// is the per-minute at-the-money re-selection: without it, changing which
/// strike a depth socket carries means re-dialing it, and that is the churn
/// that redialled one socket 322 times in a single session.
///
/// **RENAMED 2026-08-26** from `run_connection_with_topup`. The old name was
/// accurate when adding was the only thing the channel could do, and became a
/// name for one of its two jobs — the same class of mistake this file already
/// records against `FrameSinkOutcome::Captured`.
///
/// `commands` is `None` for every connection with nothing to change. A `None`
/// receiver costs a null check on an enum discriminant per drain iteration and
/// nothing else, so such a connection behaves byte-identically to before.
pub async fn run_connection_with_commands<S, K, F, Fut>(
    mut socket: S,
    mut supervisor: ConnectionSupervisor,
    mut guard: SubscribeGuard,
    sink: std::sync::Arc<K>,
    mut refresh_token: F,
    mut commands: Option<tokio::sync::mpsc::Receiver<LiveSubscriptionCommand>>,
) -> ConnectionExit
where
    S: DhanFeedSocket,
    K: FrameSink + ?Sized,
    F: FnMut() -> Fut + Send,
    Fut: std::future::Future<Output = ()> + Send,
{
    let endpoint = supervisor.slot().endpoint.as_str();
    let pool_index = supervisor.slot().pool_index;
    let mut action = supervisor.on_event(ConnEvent::BeginDial, Instant::now());

    loop {
        match action {
            SupervisorAction::Park { reason } => {
                socket.close().await;
                // A park is PERMANENT — nothing re-dials this socket, and its
                // shard of the universe stops delivering for the rest of the
                // session. Until 2026-08-20 that fact reached a log line and a
                // counter and nothing queryable: `ws_event_audit` had exactly
                // one production producer (the order-update socket), so an
                // operator asking "did any feed socket drop today?" got rows
                // back, saw no live-feed rows, and read that as no drops
                // rather than as not recorded.
                sink.on_lifecycle(
                    tickvault_common::ws_event_types::WsEventKind::Disconnected,
                    reason.as_str(),
                );
                info!(
                    endpoint,
                    pool_index,
                    reason = reason.as_str(),
                    frames = supervisor.frames_received(),
                    reconnects = supervisor.reconnects(),
                    "supervised Dhan connection parked"
                );
                return ConnectionExit::Parked(reason);
            }

            SupervisorAction::Continue => {
                // Reachable only if a caller hands in a supervisor that is not
                // freshly constructed. An ALREADY-PARKED supervisor absorbs
                // every event and would otherwise spin here forever, so it is
                // checked first; anything else re-enters the dial cycle.
                if let Some(reason) = supervisor.park_reason() {
                    socket.close().await;
                    return ConnectionExit::Parked(reason);
                }
                action = supervisor.on_event(ConnEvent::BeginDial, Instant::now());
            }

            SupervisorAction::SleepThenDial { delay_ms } => {
                socket.close().await;
                guard.mark_lost();
                // `mark_lost` is the honest edge: the subscription is gone and
                // will have to be re-sent. Recorded BEFORE the sleep so the
                // row's timestamp is the moment we lost the socket, not the
                // moment we got around to re-dialing.
                sink.on_lifecycle(
                    tickvault_common::ws_event_types::WsEventKind::Disconnected,
                    "backoff_redial",
                );
                tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                action = supervisor.on_event(ConnEvent::BeginDial, Instant::now());
            }

            SupervisorAction::RefreshTokenThenDial { delay_ms } => {
                socket.close().await;
                guard.mark_lost();
                sink.on_lifecycle(
                    tickvault_common::ws_event_types::WsEventKind::Disconnected,
                    "token_refresh_redial",
                );
                tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                refresh_token().await;
                action = supervisor.on_event(ConnEvent::BeginDial, Instant::now());
            }

            SupervisorAction::Dial => {
                let event = match socket.connect().await {
                    Ok(()) => ConnEvent::DialSucceeded,
                    Err(_) => ConnEvent::DialFailed,
                };
                action = supervisor.on_event(event, Instant::now());
            }

            SupervisorAction::Subscribe => {
                let slot = supervisor.slot();
                let outcome =
                    dispatch_subscribe(&mut socket, &guard, slot.endpoint, slot.pool_index).await;
                if outcome.stopped_early() {
                    // `mark_confirmed` is deliberately NOT reached: the guard
                    // still reads `needs_resubscribe`, so the retained set is
                    // replayed WHOLE on the next connect rather than resuming
                    // from a remembered offset. Resuming would be wrong twice
                    // over — the socket is about to be torn down, and a partial
                    // in-memory "confirmed" would claim instruments the wire
                    // never carried.
                    action = supervisor.on_event(ConnEvent::SubscribeFailed, Instant::now());
                    continue;
                }
                guard.mark_confirmed();
                // CONNECTED means subscribed-and-acked, not merely dialed.
                // The 2026-08-12 blackout is why: twelve sockets dialed and
                // every one died on the handshake, so a row written at dial
                // time would have recorded twelve connections that never
                // carried a byte. This edge is the first moment the socket can
                // actually deliver.
                sink.on_lifecycle(
                    tickvault_common::ws_event_types::WsEventKind::Connected,
                    "subscribe_acked",
                );
                action = supervisor.on_event(ConnEvent::SubscribeAcked, Instant::now());
                // Drain until something changes.
                action = drain(
                    &mut socket,
                    &mut supervisor,
                    sink.as_ref(),
                    action,
                    &mut guard,
                    commands.as_mut(),
                )
                .await;
            }
        }
    }
}

/// What a paced subscribe dispatch actually got onto the wire.
///
/// # Why an outcome type rather than a `bool`
///
/// The old dispatch returned nothing but "did any send fail". That answer is
/// unusable for the question an operator actually has when instruments are
/// silent — *were they even asked for?* — because a failure part-way through
/// leaves an arbitrary prefix subscribed and an arbitrary tail not, and the
/// boolean erases the split.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SubscribeDispatch {
    /// Subscribe messages successfully written to the socket.
    pub batches_sent: usize,
    /// Subscribe messages the set required.
    pub batches_total: usize,
    /// Instruments covered by the messages written.
    pub instruments_sent: usize,
    /// Instruments the set contained.
    pub instruments_total: usize,
    /// Wall-clock time the dispatch took, pacing included.
    pub elapsed: Duration,
}

impl SubscribeDispatch {
    /// Whether a send failed and the remaining batches were abandoned.
    ///
    /// True only for a genuine short write — an EMPTY set dispatches zero of
    /// zero batches and is a legal no-op, not a failure.
    #[must_use]
    pub const fn stopped_early(&self) -> bool {
        self.batches_sent < self.batches_total
    }

    /// Instruments in the abandoned tail: named on the in-memory set, never
    /// written to the socket, and therefore NOT subscribed.
    ///
    /// This is the number that separates "subscribed and never ticked" from
    /// "not subscribed". It is exact, because it is counted on our own side of
    /// the wire — unlike vendor acceptance, which this protocol never reports.
    #[must_use]
    pub const fn instruments_undispatched(&self) -> usize {
        self.instruments_total.saturating_sub(self.instruments_sent)
    }
}

/// Writes a connection's subscription to the socket, one message per
/// [`SubscribeGuard::batches`] chunk, spaced by [`SUBSCRIBE_BATCH_INTERVAL`].
///
/// # Why the spacing is between messages and not before the first
///
/// The first message is the one the pre-open deadline is waiting on, and there
/// is nothing before it to be polite to. Pacing `n` messages therefore costs
/// `(n - 1)` intervals, which is also what the deadline arithmetic in
/// [`SUBSCRIBE_BATCH_INTERVAL`] assumes.
///
/// # Mid-loop failure
///
/// A failed write ends the dispatch immediately rather than pressing on. Two
/// reasons: the transport is already reporting itself broken, so the remaining
/// messages would be written into a socket that is about to be torn down; and
/// continuing would make the subscribed set a function of WHICH sends happened
/// to fail, which is the un-diagnosable shape this function exists to remove.
/// The caller must not `mark_confirmed` on an early stop — the whole set is
/// replayed on the next connect.
///
/// Zero heap allocation in the loop: batches are borrowed slices, and both
/// counters are resolved once before the first send rather than per message.
async fn dispatch_subscribe<S>(
    socket: &mut S,
    guard: &SubscribeGuard,
    endpoint: DhanEndpointType,
    pool_index: u8,
) -> SubscribeDispatch
where
    S: DhanFeedSocket,
{
    let endpoint_label = endpoint.as_str();
    let batches_metric = metrics::counter!(SUBSCRIBE_BATCH_METRIC, "endpoint" => endpoint_label);
    let instruments_metric =
        metrics::counter!(SUBSCRIBE_INSTRUMENTS_METRIC, "endpoint" => endpoint_label);

    let batches_total = guard.batch_count();
    let instruments_total = guard.len();
    // `tokio::time::Instant`, not `std::time::Instant`: identical in
    // production, but it also advances with the test clock, so the pacing
    // above is asserted as an EXACT duration rather than as a wall-clock
    // approximation that would be flaky under load.
    let started = tokio::time::Instant::now();

    let mut batches_sent = 0_usize;
    let mut instruments_sent = 0_usize;

    for (index, batch) in guard.batches().enumerate() {
        if index > 0 {
            // Paced BETWEEN messages. See `SUBSCRIBE_BATCH_INTERVAL` for why a
            // burst is dangerous rather than merely rude, and for the
            // arithmetic showing this cannot miss the 09:12 deadline.
            tokio::time::sleep(SUBSCRIBE_BATCH_INTERVAL).await;
        }
        if socket.send_subscribe(batch).await.is_err() {
            break;
        }
        batches_sent = batches_sent.saturating_add(1);
        instruments_sent = instruments_sent.saturating_add(batch.len());
        batches_metric.increment(1);
        instruments_metric.increment(batch.len() as u64);
    }

    let outcome = SubscribeDispatch {
        batches_sent,
        batches_total,
        instruments_sent,
        instruments_total,
        elapsed: started.elapsed(),
    };

    // Published on EVERY dispatch, success or not, so the 09:12 margin is a
    // measurement rather than an assumption — and so a dispatch that stalls
    // (a slow socket, a retrying write) is visible even when nothing failed.
    metrics::gauge!(SUBSCRIBE_DISPATCH_MS_METRIC, "endpoint" => endpoint_label)
        .set(outcome.elapsed.as_millis() as f64);

    if outcome.stopped_early() {
        metrics::counter!(SUBSCRIBE_DISPATCH_FAILED_METRIC, "endpoint" => endpoint_label)
            .increment(1);
        error!(
            code = ErrorCode::WsGapSubscriptionBatching.code_str(),
            endpoint = endpoint_label,
            pool_index,
            batches_sent = outcome.batches_sent,
            batches_total = outcome.batches_total,
            instruments_sent = outcome.instruments_sent,
            instruments_undispatched = outcome.instruments_undispatched(),
            "subscribe dispatch stopped part-way — the instruments in the abandoned tail were \
             never written to the socket, so they are NOT subscribed and will never tick. This \
             is deliberately reported as a count rather than as vendor acceptance: the Dhan \
             live-feed protocol acknowledges no subscribe, so what we CAN prove is what we \
             sent. The whole set is replayed on the next connect."
        );
    } else {
        info!(
            endpoint = endpoint_label,
            pool_index,
            batches = outcome.batches_total,
            instruments = outcome.instruments_total,
            dispatch_ms = outcome.elapsed.as_millis(),
            interval_ms = SUBSCRIBE_BATCH_INTERVAL.as_millis(),
            "subscribe dispatch complete — every batch written. Written, not acknowledged: the \
             protocol carries no per-subscribe ack, so an instrument that stays silent after \
             this line is either illiquid or was declined upstream, and only the tick-gap \
             detector can narrow that further."
        );
    }

    outcome
}

/// The drain loop. Returns as soon as the supervisor asks for anything other
/// than [`SupervisorAction::Continue`].
///
/// THE ONE RULE lives here: per frame, one [`FrameSink::accept`] and one
/// watchdog update. Nothing else is permitted between two `recv()` calls,
/// because the automatic pong is only emitted while `recv()` is polling.
async fn drain<S, K>(
    socket: &mut S,
    supervisor: &mut ConnectionSupervisor,
    sink: &K,
    mut action: SupervisorAction,
    guard: &mut SubscribeGuard,
    mut commands: Option<&mut tokio::sync::mpsc::Receiver<LiveSubscriptionCommand>>,
) -> SupervisorAction
where
    S: DhanFeedSocket,
    K: FrameSink + ?Sized,
{
    let mut ticker = tokio::time::interval(IDLE_POLL_INTERVAL);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // Consume the immediate first tick so a fresh socket is not polled for
    // idleness the instant it comes up.
    ticker.tick().await;

    // Per-socket count of frames the ring refused, used only to throttle the
    // log below. Scoped to this drain call, so a reconnect starts the ladder
    // again — deliberate: a fresh connection that immediately backs up is
    // worth hearing about even if the previous one already reported.
    let mut ring_full_seen: u64 = 0;

    // When we last sent a client keepalive ping on this socket.
    //
    // Seeded to NOW rather than to the epoch so a freshly-dialled socket does
    // not ping on its very first tick: the subscribe is still in flight at
    // that moment, and adding a control frame to that window buys nothing.
    // Monotonic `Instant`, never wall time — same reason the watchdog is:
    // an NTP step must not be able to conjure or suppress a keepalive.
    let mut last_client_ping = Instant::now();

    while action == SupervisorAction::Continue {
        // Top-up check, BEFORE the select and deliberately not an arm of it.
        //
        // It was a select arm first. That is wrong, and the fake transport
        // proved it: with `biased`, the socket arm wins whenever it is ready,
        // so a continuously-ready socket starves every later arm and the
        // top-up never runs. A real socket is pending between frames, so it
        // would usually land — "usually" is not a property worth shipping for
        // the one event that must arrive at 09:16 on the dot.
        //
        // THE ONE RULE still holds, and the `Option` is what makes that true.
        // In steady state `topup` is `None` — for every depth socket, every
        // attach-dialed connection, and the spot connection itself once its
        // single top-up has been consumed — so this compiles to a null check
        // on an enum discriminant, not an atomic, not a syscall. The cost is
        // present only in the handful of iterations between the socket coming
        // up and the attach sending, and it disappears permanently after.
        if let Some(rx) = commands.as_mut() {
            match rx.try_recv() {
                Ok(LiveSubscriptionCommand::Extend(more)) => {
                    let added = more.len();
                    match guard.try_extend(more) {
                        Ok(start) => {
                            // BOUNDED and PACED. Both were missing, and the
                            // `SWAP_WIRE_BUDGET` doc two hundred lines up
                            // already explains why that is dangerous: these
                            // sends run ON the drain task, the automatic pong
                            // is only emitted while `recv()` is polling, and
                            // Dhan closes a silent socket after 40 seconds.
                            // Unbounded, this loop's worst case is 42 messages
                            // x the transport's 10-second send timeout = 420
                            // seconds. The swap path one branch below was given
                            // a budget for a TWENTY-second exposure; this one
                            // had twenty times that and no budget.
                            //
                            // Pacing is the same 25 ms the initial dispatch
                            // uses, and for the same reason: 42 messages fired
                            // back to back is a burst against a broker that
                            // answers an over-rate subscribe by dropping the
                            // socket.
                            // `tokio::time::Instant`, not `std::time::Instant`.
                            // Both are monotonic, so this is NOT the NTP
                            // reasoning that governs the watchdog -- it is
                            // that a bound the test harness cannot advance is
                            // a bound nothing can prove. Under
                            // `start_paused` the std clock stands still while
                            // the runtime's advances, so a std deadline here
                            // would pass every test and only ever fire in
                            // production, which is the worst of both.
                            let deadline = tokio::time::Instant::now() + TOPUP_WIRE_BUDGET;
                            let mut sent = 0usize;
                            let mut failed = false;
                            let mut budget_exhausted = false;
                            for batch in guard.batches_from(start) {
                                if tokio::time::Instant::now() >= deadline {
                                    budget_exhausted = true;
                                    break;
                                }
                                match tokio::time::timeout(
                                    SWAP_WIRE_BUDGET,
                                    socket.send_subscribe(batch),
                                )
                                .await
                                {
                                    Ok(Ok(())) => sent += batch.len(),
                                    // A per-message timeout is a SICK SOCKET,
                                    // not a failed send: treating it as the
                                    // budget case keeps the guard truthful and
                                    // lets the reconnect ladder do its job.
                                    Err(_) => {
                                        budget_exhausted = true;
                                        break;
                                    }
                                    Ok(Err(_)) => {
                                        failed = true;
                                        break;
                                    }
                                }
                                tokio::time::sleep(SUBSCRIBE_BATCH_INTERVAL).await;
                            }
                            if budget_exhausted {
                                // Keep the guard honest: it is the reconnect
                                // replay, so it must name only what the socket
                                // actually holds.
                                guard.truncate_to(start + sent);
                                error!(
                                    code = ErrorCode::WsGapSubscriptionBatching.code_str(),
                                    endpoint = supervisor.slot().endpoint.as_str(),
                                    pool_index = supervisor.slot().pool_index,
                                    added,
                                    sent,
                                    budget_secs = TOPUP_WIRE_BUDGET.as_secs(),
                                    "live subscription top-up hit its wire budget and STOPPED - \
                                     the socket stayed up and is carrying what did reach it. \
                                     The remainder is not subscribed this session unless a later \
                                     top-up offers it again."
                                );
                                metrics::counter!("tv_dhan_ws_topup_budget_exhausted_total")
                                    .increment(1);
                            } else if failed {
                                // Do NOT mark the guard lost here — the
                                // supervisor owns that transition and will see
                                // the disconnect itself. The guard now names
                                // instruments the socket may not hold; the
                                // reconnect replay sends the WHOLE set, which
                                // reconciles it.
                                error!(
                                    code = ErrorCode::WsGapSubscriptionBatching.code_str(),
                                    endpoint = supervisor.slot().endpoint.as_str(),
                                    pool_index = supervisor.slot().pool_index,
                                    added,
                                    sent,
                                    "live subscription top-up failed part-way — the reconnect \
                                     replay sends the whole set and reconciles it"
                                );
                                metrics::counter!("tv_dhan_ws_topup_failed_total").increment(1);
                            } else {
                                info!(
                                    endpoint = supervisor.slot().endpoint.as_str(),
                                    pool_index = supervisor.slot().pool_index,
                                    added,
                                    total = guard.len(),
                                    "live subscription topped up — the slots stranded on this \
                                     connection are now carrying contracts"
                                );
                                metrics::counter!("tv_dhan_ws_topup_instruments_total")
                                    .increment(added as u64);
                            }
                        }
                        Err(_) => {
                            // LOUD at the emit site, not only inside
                            // `try_extend`. The loss-counter visibility guard
                            // caught the first version of this: `try_extend`
                            // warns about the CAP ARITHMETIC, but that is a
                            // different function, so the counter reached no
                            // operator surface at all — measured loss,
                            // discarded measurement, green dashboard.
                            //
                            // The two lines say different things and both are
                            // worth having: `try_extend` explains WHY the set
                            // was refused, this says WHAT IT COSTS — the
                            // overflow contracts are not on the wire, so the
                            // ATM window is narrower than the operator
                            // authorized and nothing downstream can tell.
                            error!(
                                code = ErrorCode::WsGapSubscriptionBatching.code_str(),
                                endpoint = supervisor.slot().endpoint.as_str(),
                                pool_index = supervisor.slot().pool_index,
                                added,
                                already = guard.len(),
                                "live subscription top-up REFUSED — those contracts are NOT \
                                 subscribed this session, so the ATM window is narrower than \
                                 authorized. The pool-dialed contracts and depth are unaffected."
                            );
                            metrics::counter!("tv_dhan_ws_topup_refused_total").increment(1);
                        }
                    }
                }
                Ok(LiveSubscriptionCommand::Swap { old, new }) => {
                    match guard.try_swap(old, new) {
                        Ok(swap) if swap.is_no_op() => {
                            // The socket already carries what was asked for.
                            // Not counted and not logged: this is the ORDINARY
                            // minute, and a line per socket per minute would
                            // be ~1,500 lines a session saying nothing
                            // happened. The tracker upstream is edge-triggered
                            // precisely so this case rarely even reaches here.
                        }
                        Ok(swap) => {
                            // UNSUBSCRIBE FIRST. A depth-200 connection holds
                            // exactly one instrument; subscribing before
                            // unsubscribing asks for two, and Dhan answers an
                            // over-limit subscribe with 804 — Fatal, and
                            // retrying re-sends the same over-limit set
                            // forever. The order is the safety property.
                            // Both calls bounded by `SWAP_WIRE_BUDGET`. See
                            // that constant: unbounded, these two would be a
                            // twenty-second drain stall on the socket the
                            // swap exists to keep useful.
                            let mut wire_failed = false;
                            let mut wire_timed_out = false;
                            // Tracked separately from `wire_failed` because the two
                            // halves leave the socket in OPPOSITE states. A failed
                            // unsubscribe means the socket still holds its OLD
                            // instrument and is still delivering; a failed subscribe
                            // AFTER a successful unsubscribe means it holds nothing.
                            // Only the second is worth tearing the socket down for.
                            let mut unsubscribe_succeeded = false;
                            if let Some(drop_this) = swap.unsubscribe {
                                match tokio::time::timeout(
                                    SWAP_WIRE_BUDGET,
                                    socket.send_unsubscribe(&[drop_this]),
                                )
                                .await
                                {
                                    Ok(Ok(())) => unsubscribe_succeeded = true,
                                    Ok(Err(_)) => wire_failed = true,
                                    Err(_elapsed) => {
                                        wire_failed = true;
                                        wire_timed_out = true;
                                    }
                                }
                            }
                            if !wire_failed && let Some(add_this) = swap.subscribe {
                                match tokio::time::timeout(
                                    SWAP_WIRE_BUDGET,
                                    socket.send_subscribe(&[add_this]),
                                )
                                .await
                                {
                                    Ok(Ok(())) => {}
                                    Ok(Err(_)) => wire_failed = true,
                                    Err(_elapsed) => {
                                        wire_failed = true;
                                        wire_timed_out = true;
                                    }
                                }
                            }
                            if wire_failed {
                                // FORCE A REDIAL when the swap left the socket
                                // holding LESS than it should — added
                                // 2026-08-28.
                                //
                                // The order of the two wire calls is a safety
                                // property (see the comment above), and it has
                                // a consequence this arm used to ignore: when
                                // the unsubscribe SUCCEEDS and the subscribe
                                // then fails, a depth-200 socket — which holds
                                // exactly one instrument — is left holding
                                // NOTHING.
                                //
                                // The error line below has always claimed "the
                                // reconnect replay lands it. One stale minute,
                                // not a lost strike." The first half was true
                                // and the second was an assumption: nothing
                                // scheduled a reconnect. Only two things do —
                                // a `Disconnected` event from the read half,
                                // and the idle watchdog. Neither fires here.
                                // The socket is transport-healthy (that is why
                                // the write did not error), and depth-200 is
                                // client-pinged by us with Dhan ponging back,
                                // so `KeepAliveReceived` keeps resetting the
                                // watchdog on a socket carrying no data at all.
                                //
                                // It is not permanent — the next ATM move
                                // issues a fresh swap, and `depth200_atm`
                                // skips only while the strike is UNCHANGED —
                                // and the 600s deaf-socket alarm does page. So
                                // this was detected and simply never
                                // remediated: an empty socket for as long as
                                // the strike holds still, which on a flat
                                // afternoon is the rest of the session.
                                //
                                // `ConnEvent::SubscribeFailed` is the right
                                // event and already exists: the initial
                                // subscribe path fires it for the same shape
                                // (a set the wire did not carry), and it
                                // schedules a redial through the normal
                                // backoff ladder rather than a bare reconnect
                                // — so a socket failing this repeatedly backs
                                // off and parks instead of spinning.
                                //
                                // Gated on `lost_instruments` rather than on
                                // `wire_failed` alone, because a swap whose
                                // UNSUBSCRIBE failed never sent the subscribe:
                                // that socket still holds its old instrument
                                // and is still delivering, so tearing it down
                                // would trade a stale strike for a real gap.
                                // The socket is EMPTY when the unsubscribe left
                                // the wire and the subscribe did not follow it.
                                //
                                // TWO ways that happens, and the second was
                                // missed until 2026-08-28:
                                //
                                // 1. The unsubscribe was ACKNOWLEDGED and the
                                //    subscribe then failed. Unambiguous.
                                // 2. The unsubscribe TIMED OUT. `wire_failed`
                                //    is set, `unsubscribe_succeeded` is not —
                                //    but a budget elapsing does not mean the
                                //    frame was never written. A slow flush that
                                //    lands afterwards leaves Dhan holding
                                //    NOTHING on this socket, which is exactly
                                //    the case this remediation exists for, and
                                //    the old condition read it as "still
                                //    holding its old instrument" and did
                                //    nothing. Worse, the emptied-socket counter
                                //    then read zero while a socket was empty —
                                //    a false-OK on the very signal added to
                                //    make this visible.
                                //
                                // Treated as lost, not as safe, because the two
                                // errors are not symmetric: a redial is
                                // idempotent and costs a backoff, while an
                                // empty socket keeps ponging and delivers
                                // nothing for the rest of the session.
                                //
                                // A failed unsubscribe that ANSWERED (`Ok(Err)`)
                                // is still not lost: the wire returned an
                                // error, the subscribe was never sent, and that
                                // socket is still delivering its old strike.
                                // TWO facts, deliberately kept apart
                                // (RE-NARROWED 2026-08-28 after this widening
                                // hung CI).
                                //
                                // `lost_instruments` is the REMEDIATION
                                // trigger: it returns `SubscribeFailed`, which
                                // makes the outer loop redial. It stays gated
                                // on an unsubscribe that ANSWERED `Ok`.
                                //
                                // `possibly_emptied` is the VISIBILITY fact: a
                                // timed-out unsubscribe may still have reached
                                // the wire, so the socket may be empty even
                                // though nothing here can know it. That is the
                                // blindness worth counting, and counting it
                                // costs nothing.
                                //
                                // Why they are not the same bool: making a
                                // TIMEOUT trigger the redial turned a bounded
                                // failure into an unbounded one. The redial
                                // re-enters `send_subscribe`, and this code
                                // path has no timeout of its own around that
                                // call — so a socket that never answers took
                                // the redial, hung on the replayed subscribe,
                                // and held the drain forever. CI caught it as
                                // a 180-second test timeout on
                                // `a_socket_that_never_answers_costs_the_drain_two_seconds_not_twenty`,
                                // which passes on main and hung here: proof
                                // that the widening, not the test, was wrong.
                                //
                                // The reasoning that produced the widening was
                                // still right — a timed-out unsubscribe really
                                // may have landed — so the FACT is kept and
                                // only the ACTION is withheld. Acting on it
                                // safely needs a bounded redial, which is its
                                // own change.
                                let lost_instruments =
                                    unsubscribe_succeeded && swap.subscribe.is_some();
                                let possibly_emptied = wire_timed_out
                                    && swap.unsubscribe.is_some()
                                    && swap.subscribe.is_some();
                                // The guard already carries the NEW instrument
                                // — see `try_swap` for why it is recorded
                                // before the wire moves. That is what makes
                                // this recoverable rather than a silent
                                // revert: the reconnect replays the CURRENT
                                // intent, so the socket comes back holding the
                                // strike that was chosen, not the one it had.
                                // The `source` field below is the CloudWatch
                                // filter's discriminator, and a string rather
                                // than the `lost_instruments` bool because
                                // filter PATTERN syntax is validated only by
                                // the real PutMetricFilter call at apply time:
                                // a malformed pattern passes every PR check and
                                // breaks the post-merge apply lane. This reuses
                                // the exact three-condition shape already
                                // proven live by the ws-gap-03 filters.
                                // THREE `source` values, not two, and the
                                // reasoning is kept HERE rather than inside the
                                // macro on purpose: `loss_counter_visibility_guard`
                                // requires the counter below to sit near its
                                // log line, and a comment block inside the
                                // macro pushes it out of that window. That is
                                // not a hypothetical — it broke this exact
                                // counter twice today.
                                //
                                // The alarm keys on `swap_emptied_socket` only,
                                // because that is the case a redial is
                                // scheduled for. `swap_maybe_emptied` is the
                                // honest third state: the unsubscribe did not
                                // answer, so whether the socket is empty is
                                // UNKNOWN here. Folding it into either of the
                                // other two would be a claim this code cannot
                                // make — one direction hides an empty socket,
                                // the other pages for one that is fine.
                                error!(
                                    code = ErrorCode::WsGapSubscriptionBatching.code_str(),
                                    source = if lost_instruments {
                                        "swap_emptied_socket"
                                    } else if possibly_emptied {
                                        "swap_maybe_emptied"
                                    } else {
                                        "swap_wire_failed"
                                    },
                                    endpoint = supervisor.slot().endpoint.as_str(),
                                    pool_index = supervisor.slot().pool_index,
                                    lost_instruments,
                                    possibly_emptied,
                                    wire_timed_out,
                                    "live subscription swap failed on the wire. The retained set \
                                     already names the new instrument, so a reconnect replay lands \
                                     the right strike. When the unsubscribe had already succeeded \
                                     the socket is now carrying NOTHING, and a redial is scheduled \
                                     here to recover it — without one, a transport-healthy socket \
                                     keeps ponging while delivering no data. When the unsubscribe \
                                     TIMED OUT instead, whether the socket is empty is unknown \
                                     from here: no redial is scheduled, and this line is the only \
                                     record that the question exists."
                                );
                                metrics::counter!("tv_dhan_ws_swap_failed_total").increment(1);
                                if wire_timed_out {
                                    // Separated from an ordinary write error
                                    // because they mean different things: a
                                    // write error is a socket that answered,
                                    // a timeout is a socket that did not —
                                    // and the second is the one that also
                                    // cost the drain a full second.
                                    metrics::counter!("tv_dhan_ws_swap_timeout_total").increment(1);
                                }
                                if lost_instruments {
                                    metrics::counter!("tv_dhan_ws_swap_emptied_socket_total")
                                        .increment(1);
                                    action = supervisor
                                        .on_event(ConnEvent::SubscribeFailed, Instant::now());
                                    // Leave the drain so the outer loop can act
                                    // on the scheduled redial. Returning the
                                    // action rather than continuing is what
                                    // makes this a remediation instead of
                                    // another log line.
                                    return action;
                                }
                            } else {
                                info!(
                                    endpoint = supervisor.slot().endpoint.as_str(),
                                    pool_index = supervisor.slot().pool_index,
                                    total = guard.len(),
                                    "live subscription swapped — this socket now carries the \
                                     current at-the-money contract without a re-dial"
                                );
                                metrics::counter!("tv_dhan_ws_swap_total").increment(1);
                            }
                        }
                        Err(_) => {
                            // Fail-closed and LOUD at the emit site, for the
                            // same reason the refused top-up is: `try_swap`
                            // explains WHY it refused, this says WHAT IT COSTS
                            // — this socket keeps carrying a strike that is no
                            // longer at-the-money, and it will look perfectly
                            // healthy doing it, because a far-from-the-money
                            // subscription still delivers real depth all day.
                            error!(
                                code = ErrorCode::WsGapSubscriptionBatching.code_str(),
                                endpoint = supervisor.slot().endpoint.as_str(),
                                pool_index = supervisor.slot().pool_index,
                                held = guard.len(),
                                "live subscription swap REFUSED — this connection does not hold \
                                 the instrument it was asked to replace, so it keeps the strike \
                                 it has. That strike is no longer at-the-money and nothing \
                                 downstream can tell."
                            );
                            metrics::counter!("tv_dhan_ws_swap_refused_total").increment(1);
                        }
                    }
                }
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {}
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                    // Every sender is gone: the attach sent its one overflow
                    // and dropped, or the session's re-selection task ended.
                    // Clearing the Option is what makes the steady-state cost
                    // of this whole block exactly zero.
                    commands = None;
                }
            }
        }

        tokio::select! {
            biased;
            event = socket.recv() => {
                match event {
                    // A Ping/Pong: proof the peer is alive, carrying no data.
                    // One watchdog reset, no sink write, no frame count.
                    SocketEvent::KeepAlive => {
                        action = supervisor
                            .on_event(ConnEvent::KeepAliveReceived, Instant::now());
                    }
                    SocketEvent::Frame(frame) => {
                        // Two operations. That is the whole loop body.
                        let outcome = sink.accept(frame);
                        action = supervisor.on_event(ConnEvent::FrameReceived, Instant::now());
                        if outcome == FrameSinkOutcome::WalDropped {
                            // Loud, but the reader does NOT stop draining:
                            // stopping would cost the pong and turn one lost
                            // frame into a disconnect.
                            error!(
                                code = ErrorCode::WsGapConnectionState.code_str(),
                                endpoint = supervisor.slot().endpoint.as_str(),
                                pool_index = supervisor.slot().pool_index,
                                "write-ahead log refused a Dhan frame — that frame is lost; \
                                 the reader keeps draining so the socket is not also lost"
                            );
                        }
                        if outcome == FrameSinkOutcome::RingFull {
                            // 2026-08-11: this outcome used to bump a counter
                            // and produce NO log line at all.
                            //
                            // The counter's documentation calls a full ring
                            // "not capture loss — the consumer is behind",
                            // and for a brief burst that is fair: the frame is
                            // already in the WAL. But nothing re-folds WAL
                            // frames into the database, so in practice a full
                            // ring means those ticks and candles never arrive
                            // — while the lane's health gauge still reads 1.
                            // Silent permanent loss behind a green light is
                            // exactly the class the charter forbids.
                            //
                            // Throttled by powers of two rather than rate-
                            // limited: the first occurrence is always
                            // reported, and a sustained storm degrades to a
                            // handful of lines instead of one per frame. A
                            // slow consumer must not be able to drown the log
                            // it is being reported in.
                            ring_full_seen = ring_full_seen.saturating_add(1);
                            if ring_full_seen.is_power_of_two() {
                                error!(
                                    code = ErrorCode::WsGapConnectionState.code_str(),
                                    endpoint = supervisor.slot().endpoint.as_str(),
                                    pool_index = supervisor.slot().pool_index,
                                    dropped_on_this_socket = ring_full_seen,
                                    "the frame ring is FULL — the fold cannot keep up, so this \
                                     frame is not being turned into ticks or candles NOW. It IS \
                                     in the write-ahead log and the next boot re-folds it, so \
                                     the tick ROWS come back; what does not come back is their \
                                     CANDLE contribution, because by then the tick is outside \
                                     the aggregating session and only the row is written. Treat \
                                     this as lost candles, not lost ticks. Logged at \
                                     1, 2, 4, 8 ... occurrences per socket to bound the noise."
                                );
                            }
                        }
                    }
                    SocketEvent::Closed { code } => {
                        action = supervisor
                            .on_event(ConnEvent::Disconnected { code }, Instant::now());
                    }
                }
            }
            _ = ticker.tick() => {
                // Client-originated keepalive, for the endpoints Dhan does not
                // ping (measured: depth-200 receives zero server control
                // frames — see `needs_client_keepalive_ping`).
                //
                // Sited on the EXISTING 1s ticker rather than a second timer:
                // one more `select!` arm on the drain loop would be a second
                // place that can hold this task away from `recv`, and the
                // whole reason this watchdog exists is to catch a reader that
                // stopped polling. Reusing the tick costs nothing and adds no
                // new way to stall.
                //
                // The send is awaited but bounded by SUBSCRIBE_SEND_TIMEOUT
                // inside the socket, and its failure is deliberately ignored
                // here: if Dhan does not answer pings on this endpoint the
                // watchdog keeps governing the socket exactly as it does
                // today. This path can improve that behaviour, never worsen
                // it.
                if supervisor.slot().endpoint.needs_client_keepalive_ping()
                    && last_client_ping.elapsed() >= CLIENT_KEEPALIVE_PING_INTERVAL
                {
                    last_client_ping = Instant::now();
                    // Spelled as an exhaustive match rather than `let _ =`,
                    // and not only to satisfy `clippy::let_underscore_must_use`
                    // (which is what caught the first draft). `send_ping`
                    // counts EVERY outcome under
                    // `CLIENT_KEEPALIVE_PING_METRIC` and logs the failing ones,
                    // so there is genuinely nothing left for this site to do —
                    // but a discard says that by accident, whereas a match says
                    // it on purpose and forces a decision here if a future
                    // outcome ever appears that the supervisor SHOULD act on.
                    //
                    // Escalating a failure here would be wrong in the fail-safe
                    // direction: it would turn "Dhan does not answer pings on
                    // this endpoint" into a disconnect, which is worse than the
                    // behaviour being fixed.
                    //
                    // The clock advances BEFORE the await, deliberately. A
                    // failing send must not be retried on the next 1 s tick:
                    // `SUBSCRIBE_SEND_TIMEOUT` is 10 s, so a HUNG send holds
                    // this loop — the loop whose job is to keep polling `recv`
                    // — for that long. Once per interval is an accepted
                    // exposure; once per second would be a tenfold increase in
                    // the very stall this watchdog exists to catch.
                    match socket.send_ping().await {
                        Ok(()) | Err(SocketFailure) => {}
                    }
                }
                action = supervisor.poll(Instant::now());
            }
        }
    }
    action
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use std::collections::{BTreeSet, VecDeque};
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::super::reconnect_ladder::{
        FLAP_CEILING_REDIAL_FLOOR_MS, FLAP_REDIAL_CEILING, MIN_HEALTHY_SESSION_MS,
        RECONNECT_DELAY_WITH_JITTER_MAX_MS, RECONNECT_JITTER_STEP_MS,
        SHORT_SESSION_REDIAL_FLOOR_MS, reconnect_delay_ms,
    };

    fn si(id: u64) -> SubscribeInstrument {
        SubscribeInstrument {
            security_id: id,
            segment: ExchangeSegment::NseFno,
        }
    }

    // ------------------------------------------------------------------
    // try_swap — the primitive the per-minute at-the-money re-selection
    // needs (2026-08-26). Without it the only ways to change what a live
    // socket carries are "grow it" or "re-dial it", and neither is a swap.
    // ------------------------------------------------------------------

    #[test]
    fn a_swap_reports_the_unsubscribe_first_and_the_subscribe_second() {
        let mut g = SubscribeGuard::try_new(DhanEndpointType::Depth200, vec![si(1)])
            .expect("one instrument");
        let swap = g.try_swap(si(1), si(2)).expect("holds the old one");
        assert_eq!(swap.unsubscribe, Some(si(1)));
        assert_eq!(swap.subscribe, Some(si(2)));
        assert!(!swap.is_no_op());
    }

    // -- the 804 guard: no connection may carry one instrument twice --------
    //
    // Dhan answers a duplicate subscribe with error code 804, which
    // `classify_disconnect` files as Fatal: the socket closes and the
    // supervisor parks it for the SESSION. So a repeat is not a wasted slot,
    // it is the loss of every instrument that connection was carrying.
    //
    // Every producer today builds a duplicate-free set. These pin that the
    // guard does not depend on that — it is the last place a set can be
    // inspected before the wire, and it is shared by all four endpoint types.

    #[test]
    fn a_set_carrying_the_same_instrument_twice_only_subscribes_it_once() {
        let mut set = instruments(3);
        set.push(set[1]);
        let guard =
            SubscribeGuard::try_new(DhanEndpointType::Depth20, set).expect("inside the cap");
        assert_eq!(guard.len(), 3, "the repeat must not reach the wire");
        let flat: Vec<SubscribeInstrument> = guard.batches().flatten().copied().collect();
        let mut seen = std::collections::HashSet::new();
        for i in &flat {
            assert!(
                seen.insert((i.security_id, i.segment)),
                "duplicate {i:?} in a subscribe batch — that is an 804 (Fatal)"
            );
        }
    }

    #[test]
    fn the_same_id_in_two_segments_is_two_instruments_not_a_duplicate() {
        // I-P1-11: `security_id` ALONE is not an identity. Deduping on the id
        // would silently drop a real contract and leave the connection
        // carrying fewer instruments than the operator authorized.
        let set = vec![
            SubscribeInstrument {
                security_id: 7,
                segment: ExchangeSegment::NseFno,
            },
            SubscribeInstrument {
                security_id: 7,
                segment: ExchangeSegment::BseFno,
            },
        ];
        let guard =
            SubscribeGuard::try_new(DhanEndpointType::Depth20, set).expect("inside the cap");
        assert_eq!(guard.len(), 2, "two segments are two instruments");
    }

    #[test]
    fn a_top_up_naming_something_already_held_adds_nothing() {
        // The likeliest live shape: a re-selection where only part of the
        // desired set changed, so the top-up repeats what is already there.
        let mut guard =
            SubscribeGuard::try_new(DhanEndpointType::Depth20, instruments(4)).expect("cap");
        let start = guard.try_extend(instruments(4)).expect("no cap breach");
        assert_eq!(guard.len(), 4, "nothing new was added");
        assert_eq!(
            guard.batches_from(start).flatten().count(),
            0,
            "a top-up of pure repeats must put NO subscribe on the wire"
        );
    }

    #[test]
    fn a_top_up_keeps_the_new_half_and_drops_the_repeated_half() {
        let mut guard =
            SubscribeGuard::try_new(DhanEndpointType::Depth20, instruments(4)).expect("cap");
        let mut more = instruments(2);
        more.extend(instruments_from(4, 3));
        let start = guard.try_extend(more).expect("no cap breach");
        assert_eq!(guard.len(), 7, "4 held + 3 genuinely new");
        let sent: Vec<SubscribeInstrument> = guard.batches_from(start).flatten().copied().collect();
        assert_eq!(sent.len(), 3, "only the new three go on the wire");
        for i in &sent {
            assert!(
                i.security_id >= 4,
                "a repeated instrument {i:?} reached the wire"
            );
        }
    }

    #[test]
    fn a_swap_whose_new_instrument_is_already_held_is_refused() {
        // Distinct from the `old == new` no-op: here `new` sits at a
        // DIFFERENT position, so the swap would unsubscribe `old` and then
        // subscribe something the socket already has.
        let mut guard =
            SubscribeGuard::try_new(DhanEndpointType::Depth20, instruments(3)).expect("cap");
        let err = guard
            .try_swap(instruments(3)[0], instruments(3)[2])
            .expect_err("subscribing a held instrument is an 804");
        assert!(matches!(
            err,
            SubscribeGuardRefusal::AlreadySubscribed { .. }
        ));
        assert_eq!(
            guard.len(),
            3,
            "a refused swap must leave the set exactly as the wire has it"
        );
        let flat: Vec<SubscribeInstrument> = guard.batches().flatten().copied().collect();
        assert!(
            flat.contains(&instruments(3)[0]),
            "the OLD instrument must still be held — nothing was unsubscribed"
        );
    }

    #[test]
    fn a_swap_onto_itself_is_still_a_no_op_not_a_refusal() {
        // The common minute: the at-the-money strike has not moved. This must
        // stay distinguishable from the duplicate refusal above, or every
        // quiet minute would log an 804 warning.
        let mut guard =
            SubscribeGuard::try_new(DhanEndpointType::Depth200, instruments(1)).expect("cap");
        let swap = guard
            .try_swap(instruments(1)[0], instruments(1)[0])
            .expect("a no-op swap is legal");
        assert!(swap.is_no_op());
    }

    /// A depth-200 connection accepts exactly ONE instrument. Subscribing
    /// before unsubscribing asks for two, and Dhan answers an over-limit
    /// subscribe with 804 — Fatal, and retrying re-sends the same over-limit
    /// set forever. The ORDER is the safety property, so it is pinned by the
    /// field names rather than left to a comment.
    #[test]
    fn the_swap_never_leaves_a_depth_200_connection_holding_two_instruments() {
        let mut g = SubscribeGuard::try_new(DhanEndpointType::Depth200, vec![si(1)])
            .expect("one instrument");
        let _ = g.try_swap(si(1), si(2)).expect("holds the old one");
        assert_eq!(g.len(), 1, "a swap must not grow the set");
    }

    /// The retained set is the REPLAY source. If a swap were recorded only
    /// after the socket confirmed, a disconnect in between would replay the
    /// OLD strike and the sockets would quietly revert to one nobody chose.
    #[test]
    fn a_reconnect_after_a_swap_replays_the_new_instrument_not_the_old() {
        let mut g = SubscribeGuard::try_new(DhanEndpointType::Depth200, vec![si(1)])
            .expect("one instrument");
        let _ = g.try_swap(si(1), si(2)).expect("holds the old one");
        g.mark_lost();
        let replayed: Vec<_> = g.batches().flatten().copied().collect();
        assert_eq!(
            replayed,
            vec![si(2)],
            "the reconnect replayed the OLD strike"
        );
    }

    /// The COMMON case, every minute the market has not moved a strike.
    /// Expressed as "send nothing" rather than as an error, because the
    /// caller asking "make this socket carry X" when it already carries X is
    /// a correct question.
    #[test]
    fn swapping_an_instrument_for_itself_is_a_silent_no_op() {
        let mut g = SubscribeGuard::try_new(DhanEndpointType::Depth200, vec![si(1)])
            .expect("one instrument");
        let swap = g.try_swap(si(1), si(1)).expect("holds it");
        assert!(
            swap.is_no_op(),
            "an unchanged strike must cost no wire traffic"
        );
        assert_eq!(swap.unsubscribe, None);
        assert_eq!(swap.subscribe, None);
        assert_eq!(g.len(), 1);
    }

    /// Fail-closed. Falling back to an append here would subscribe the new
    /// instrument while unsubscribing one that was never there — `try_extend`
    /// wearing a swap's name, and the retained set would then claim something
    /// the socket does not hold.
    #[test]
    fn a_swap_against_an_instrument_the_connection_never_held_is_refused() {
        let mut g = SubscribeGuard::try_new(DhanEndpointType::Depth200, vec![si(1)])
            .expect("one instrument");
        let refusal = g.try_swap(si(99), si(2)).expect_err("does not hold 99");
        assert!(matches!(
            refusal,
            SubscribeGuardRefusal::NotSubscribed { .. }
        ));
        assert_eq!(g.len(), 1, "a refused swap must not half-apply");
        let held: Vec<_> = g.batches().flatten().copied().collect();
        assert_eq!(held, vec![si(1)], "the refused swap mutated the set");
    }

    /// `security_id` ALONE is not unique — the only unique key is
    /// `(security_id, exchange_segment)` per I-P1-11. A swap that matched on
    /// the id alone would unsubscribe the wrong instrument whenever two
    /// segments share a number, which they do: id 13 is NIFTY in `IDX_I` and
    /// an unrelated cash stock in `NSE_EQ`.
    #[test]
    fn a_swap_matches_on_the_composite_key_not_the_bare_security_id() {
        let idx = SubscribeInstrument {
            security_id: 13,
            segment: ExchangeSegment::IdxI,
        };
        let eq = SubscribeInstrument {
            security_id: 13,
            segment: ExchangeSegment::NseEquity,
        };
        let mut g =
            SubscribeGuard::try_new(DhanEndpointType::Depth20, vec![idx]).expect("one instrument");
        let refusal = g
            .try_swap(eq, si(2))
            .expect_err("same id, different segment, is a different instrument");
        assert!(matches!(
            refusal,
            SubscribeGuardRefusal::NotSubscribed { .. }
        ));
    }

    /// Position is what distinguishes one depth-20 slot from another when a
    /// caller reasons about the set. A swap replaces in place; it must not
    /// reorder the instruments around it.
    #[test]
    fn a_swap_replaces_in_place_and_leaves_the_rest_in_order() {
        let mut g = SubscribeGuard::try_new(DhanEndpointType::Depth20, (0..5).map(si).collect())
            .expect("under cap");
        let _ = g.try_swap(si(2), si(42)).expect("holds it");
        let held: Vec<_> = g.batches().flatten().map(|i| i.security_id).collect();
        assert_eq!(held, vec![0, 1, 42, 3, 4]);
    }

    /// A whole session of at-the-money changes must not grow the set. This is
    /// the failure `try_extend` alone would have produced: one instrument
    /// added per change until the connection hits its cap and Dhan answers
    /// 804.
    #[test]
    fn four_hundred_swaps_leave_the_set_exactly_one_instrument_long() {
        let mut g = SubscribeGuard::try_new(DhanEndpointType::Depth200, vec![si(0)])
            .expect("one instrument");
        for step in 0..400u64 {
            let swap = g
                .try_swap(si(step), si(step + 1))
                .expect("holds the old one");
            assert!(!swap.is_no_op());
        }
        assert_eq!(g.len(), 1, "the set grew across a session of swaps");
        let held: Vec<_> = g.batches().flatten().copied().collect();
        assert_eq!(held, vec![si(400)]);
    }

    /// The whole point: reach the slots stranded on a live connection.
    #[test]
    fn try_extend_appends_and_reports_where_the_new_tail_starts() {
        let mut g = SubscribeGuard::try_new(DhanEndpointType::MainFeed, (0..150).map(si).collect())
            .expect("under cap");
        let start = g.try_extend((150..260).map(si).collect()).expect("fits");
        assert_eq!(start, 150, "the tail starts where the old set ended");
        assert_eq!(g.len(), 260);
    }

    /// Only the NEW instruments may go on the wire. Re-sending the whole set
    /// would re-subscribe thousands of live instruments and risk an 804.
    #[test]
    fn batches_from_covers_only_the_new_tail() {
        let mut g = SubscribeGuard::try_new(DhanEndpointType::MainFeed, (0..150).map(si).collect())
            .expect("under cap");
        let start = g.try_extend((150..260).map(si).collect()).expect("fits");
        let tail: Vec<usize> = g
            .batches_from(start)
            .map(<[SubscribeInstrument]>::len)
            .collect();
        assert_eq!(
            tail,
            vec![100, 10],
            "110 new instruments, capped at 100/message"
        );
        assert_eq!(tail.iter().sum::<usize>(), 110);
        // And the full set still batches over everything.
        assert_eq!(g.batch_count(), 3, "260 total => 100+100+60");
    }

    /// Fail-closed AND atomic. A half-applied top-up would leave the guard
    /// naming instruments the socket never got, and every later reconnect
    /// would replay a subscription Dhan rejects with 804.
    #[test]
    fn try_extend_past_the_cap_is_refused_and_leaves_the_guard_untouched() {
        let cap = usize::try_from(DhanEndpointType::MainFeed.max_instruments_per_connection())
            .expect("cap fits");
        let mut g = SubscribeGuard::try_new(
            DhanEndpointType::MainFeed,
            (0..cap as u64 - 10).map(si).collect(),
        )
        .expect("under cap");
        let before = g.len();
        let err = g.try_extend((0..50).map(|i| si(900_000 + i)).collect());
        assert!(err.is_err(), "51st instrument past the cap must be refused");
        assert_eq!(
            g.len(),
            before,
            "the guard must be UNCHANGED after a refusal"
        );
    }

    /// Exactly filling the connection is legal — the cap is inclusive.
    #[test]
    fn try_extend_to_exactly_the_cap_is_allowed() {
        let cap = usize::try_from(DhanEndpointType::MainFeed.max_instruments_per_connection())
            .expect("cap fits");
        let mut g = SubscribeGuard::try_new(
            DhanEndpointType::MainFeed,
            (0..cap as u64 - 10).map(si).collect(),
        )
        .expect("under cap");
        assert!(
            g.try_extend((0..10).map(|i| si(900_000 + i)).collect())
                .is_ok()
        );
        assert_eq!(g.len(), cap);
        assert_eq!(g.spare_capacity(), 0);
    }

    /// The number the contract attach needs so room on a partially-filled
    /// connection stops being invisible.
    #[test]
    fn spare_capacity_reports_the_room_the_attach_was_missing() {
        let g = SubscribeGuard::try_new(DhanEndpointType::MainFeed, (0..850).map(si).collect())
            .expect("under cap");
        let cap = usize::try_from(DhanEndpointType::MainFeed.max_instruments_per_connection())
            .expect("cap fits");
        assert_eq!(g.spare_capacity(), cap - 850);
        assert_eq!(g.spare_capacity(), 4_150, "the stranded slots, by name");
    }

    /// An empty top-up is a legal no-op, not an error — the attach may have
    /// nothing to add, and that must not log or count as a refusal.
    #[test]
    fn an_empty_top_up_is_a_no_op() {
        let mut g = SubscribeGuard::try_new(DhanEndpointType::MainFeed, (0..10).map(si).collect())
            .expect("under cap");
        let start = g.try_extend(Vec::new()).expect("no-op");
        assert_eq!(start, 10);
        assert_eq!(g.len(), 10);
        assert_eq!(g.batches_from(start).count(), 0, "nothing to send");
    }

    #[test]
    fn test_park_reason_all_covers_every_variant_so_no_series_goes_unbaselined() {
        // `ParkReason::ALL` drives the metric pre-registration in
        // `PoolSupervisor::new`, and the CloudWatch agent baselines PER LABEL
        // COMBINATION: a reason missing from ALL has its first park eaten as
        // the delta baseline, so `tv-<env>-dhan-socket-parked` stays silent for
        // exactly that reason. `[Self; 3]` alone does not protect against
        // that — adding a variant forces only `as_str()`'s match to change,
        // and the array compiles untouched.
        //
        // Same shape as `pool_budget::test_endpoint_type_has_exactly_four_...`,
        // which is what makes `DhanEndpointType::ALL` — the other half of the
        // registration loop — genuinely compile-protected.
        assert_eq!(ParkReason::ALL.len(), 3, "exactly three park reasons");

        // Exhaustive match: adding a variant stops this compiling until ALL
        // and this arm list are updated together.
        for reason in ParkReason::ALL {
            match reason {
                ParkReason::PoolOverflow | ParkReason::FatalDisconnect | ParkReason::Shutdown => {}
            }
        }

        // No duplicate standing in for a missing variant — an ALL of
        // `[PoolOverflow, PoolOverflow, Shutdown]` has the right length and
        // matches exhaustively, yet leaves FatalDisconnect unregistered.
        let distinct: BTreeSet<&'static str> = ParkReason::ALL
            .into_iter()
            .map(ParkReason::as_str)
            .collect();
        assert_eq!(
            distinct.len(),
            ParkReason::ALL.len(),
            "every park reason must contribute a DISTINCT metric label"
        );
    }

    fn t0() -> Instant {
        Instant::now()
    }

    fn slot(endpoint: DhanEndpointType, pool_index: u8) -> ConnectionSlot {
        ConnectionSlot {
            endpoint,
            pool_index,
            global_index: endpoint.jitter_base().saturating_add(pool_index),
        }
    }

    fn sup(endpoint: DhanEndpointType, pool_index: u8, now: Instant) -> ConnectionSupervisor {
        ConnectionSupervisor::new(slot(endpoint, pool_index), now)
    }

    fn instruments(n: usize) -> Vec<SubscribeInstrument> {
        (0..n)
            .map(|i| SubscribeInstrument {
                security_id: SecurityId::try_from(i).unwrap_or(SecurityId::MAX),
                segment: ExchangeSegment::NseFno,
            })
            .collect()
    }

    /// `n` instruments whose ids start at `start`, for building a top-up that
    /// is genuinely NEW rather than a repeat of what the connection holds.
    ///
    /// `instruments(n)` always starts at 0, so extending a 250-instrument set
    /// with `instruments(150)` supplies 150 instruments it ALREADY has. On the
    /// wire that is 150 duplicate subscribes, and Dhan answers a duplicate
    /// subscribe with an 804 — Fatal. The guard now drops them, so a test
    /// wanting a real top-up has to ask for one.
    fn instruments_from(start: usize, n: usize) -> Vec<SubscribeInstrument> {
        (start..start.saturating_add(n))
            .map(|i| SubscribeInstrument {
                security_id: SecurityId::try_from(i).unwrap_or(SecurityId::MAX),
                segment: ExchangeSegment::NseFno,
            })
            .collect()
    }

    // -- disconnect classification (WS-GAP-01) ------------------------------

    #[test]
    fn test_classify_disconnect_805_is_pool_overflow_not_a_retry() {
        // The single most consequential classification in this module: Dhan
        // kills the OLDEST socket per extra connection, so retrying 805 kills
        // a healthy sibling per attempt instead of recovering this one.
        assert_eq!(
            classify_disconnect(Some(DisconnectCode::ExceededActiveConnections)),
            DisconnectClass::PoolOverflow
        );
    }

    #[test]
    fn test_classify_disconnect_token_codes_are_token_stale() {
        for code in [
            DisconnectCode::AccessTokenExpired,
            DisconnectCode::AccessTokenInvalid,
        ] {
            assert_eq!(
                classify_disconnect(Some(code)),
                DisconnectClass::TokenStale,
                "{code:?} must demand a fresh token before re-dialing"
            );
        }
    }

    #[test]
    fn test_classify_disconnect_credential_and_entitlement_errors_are_fatal() {
        for code in [
            DisconnectCode::DataApiSubscriptionRequired,
            DisconnectCode::AuthenticationFailed,
            DisconnectCode::ClientIdInvalid,
        ] {
            assert_eq!(
                classify_disconnect(Some(code)),
                DisconnectClass::Fatal,
                "{code:?} never self-heals — retrying it is a storm with no upside"
            );
        }
    }

    #[test]
    fn test_classify_disconnect_absent_and_unknown_codes_are_transient() {
        // Fail-safe direction: retry politely rather than park a healthy pool
        // on a code we simply have not seen before.
        assert_eq!(classify_disconnect(None), DisconnectClass::Transient);
        assert_eq!(
            classify_disconnect(Some(DisconnectCode::Unknown(4242))),
            DisconnectClass::Transient
        );
        assert_eq!(
            classify_disconnect(Some(DisconnectCode::InternalServerError)),
            DisconnectClass::Transient
        );
    }

    /// 804 must NOT ride the reconnect ladder (2026-08-14 regression pin).
    ///
    /// "Requested number of instruments exceeds limit" is a REQUEST error
    /// wearing a transport-code costume. Retrying it re-sends the identical
    /// over-limit subscribe set that was just rejected, forever, every 30s at
    /// the ladder's cap — nothing about the request changes between attempts,
    /// so nothing in that loop can ever succeed.
    ///
    /// It is also self-amplifying in the worst direction: a permanent
    /// connect/subscribe/reject cycle is exactly the traffic 805 calls "too
    /// many requests", whose documented consequence is the USER being blocked.
    /// So retrying one account-level rejection can earn another.
    #[test]
    fn test_classify_disconnect_804_is_fatal_not_an_infinite_retry() {
        assert_eq!(
            classify_disconnect(Some(DisconnectCode::InstrumentsExceedLimit)),
            DisconnectClass::Fatal,
            "804 (instruments exceed limit) must PARK, not retry. Transient here means \
             re-sending the same rejected subscribe set every 30s forever."
        );
    }

    proptest! {
        #[test]
        fn prop_classify_disconnect_is_total_over_every_u16(raw in any::<u16>()) {
            // Annexure rule: never panic on an unknown code.
            let _ = classify_disconnect(Some(DisconnectCode::from_u16(raw)));
        }
    }

    // -- phase + label vocabulary ------------------------------------------

    #[test]
    fn test_conn_phase_watchdog_eligibility_excludes_backoff_and_parked() {
        // Arming the watchdog during a deliberate backoff sleep would make a
        // 30s ladder rung self-cancel at 27s.
        assert!(ConnPhase::Dialing.is_watchdog_eligible());
        assert!(ConnPhase::Subscribing.is_watchdog_eligible());
        assert!(ConnPhase::Live.is_watchdog_eligible());
        assert!(!ConnPhase::Idle.is_watchdog_eligible());
        assert!(!ConnPhase::Backoff.is_watchdog_eligible());
        assert!(!ConnPhase::Parked.is_watchdog_eligible());
    }

    #[test]
    fn test_label_strings_are_unique_within_each_vocabulary() {
        let phases: BTreeSet<&str> = [
            ConnPhase::Idle,
            ConnPhase::Dialing,
            ConnPhase::Subscribing,
            ConnPhase::Live,
            ConnPhase::Backoff,
            ConnPhase::Parked,
        ]
        .iter()
        .map(|p| p.as_str())
        .collect();
        assert_eq!(phases.len(), 6, "phase labels must not collide in metrics");

        let parks: BTreeSet<&str> = [
            ParkReason::PoolOverflow,
            ParkReason::FatalDisconnect,
            ParkReason::Shutdown,
        ]
        .iter()
        .map(|p| p.as_str())
        .collect();
        assert_eq!(parks.len(), 3);

        let reasons: BTreeSet<&str> = [
            ReconnectReason::DialFailed,
            ReconnectReason::SubscribeFailed,
            ReconnectReason::Disconnected,
            ReconnectReason::TokenStale,
            ReconnectReason::IdleSilence,
        ]
        .iter()
        .map(|r| r.as_str())
        .collect();
        assert_eq!(reasons.len(), 5);
    }

    // -- the state machine --------------------------------------------------

    #[test]
    fn test_supervisor_begins_idle_and_first_dial_is_instant_for_slot_zero() {
        let now = t0();
        let mut s = sup(DhanEndpointType::MainFeed, 0, now);
        assert_eq!(s.phase(), ConnPhase::Idle);
        assert_eq!(s.attempt(), 0);
        assert_eq!(s.park_reason(), None);

        assert_eq!(
            s.on_event(ConnEvent::BeginDial, now),
            SupervisorAction::Dial
        );
        assert_eq!(s.phase(), ConnPhase::Dialing);

        // A socket that has NEVER delivered a frame does NOT get the instant
        // rung — the flap damper withholds it and applies
        // SHORT_SESSION_REDIAL_FLOOR_MS instead. Before 2026-08-19 this
        // asserted 0ms, and that 0ms is the cascade: a bare TCP reset arrives
        // as `Closed { code: None }`, classifies Transient, and re-dialled
        // instantly — evicting a healthy sibling under Dhan's 805
        // oldest-socket-dies semantics. The instant rung is not gone; it is
        // earned, and `..._instant_retry_survives_a_genuinely_healthy_session`
        // below proves it still happens for the case it exists for.
        assert_eq!(
            s.on_event(ConnEvent::DialFailed, now),
            SupervisorAction::SleepThenDial {
                delay_ms: SHORT_SESSION_REDIAL_FLOOR_MS
            }
        );
        assert_eq!(s.phase(), ConnPhase::Backoff);
        assert_eq!(s.attempt(), 1);
    }

    #[test]
    fn test_supervisor_ladder_advances_on_repeated_dial_failure() {
        let now = t0();
        let mut s = sup(DhanEndpointType::MainFeed, 0, now);
        let mut seen = Vec::new();
        for _ in 0..7 {
            let _ = s.on_event(ConnEvent::BeginDial, now);
            match s.on_event(ConnEvent::DialFailed, now) {
                SupervisorAction::SleepThenDial { delay_ms } => seen.push(delay_ms),
                other => panic!("expected SleepThenDial, got {other:?}"),
            }
        }
        // Slot 0 has zero jitter, so these are the bare ladder rungs — EXCEPT
        // rung 0, which the flap damper floors because this socket has never
        // delivered a frame (see the test above). Every later rung is the raw
        // ladder, unchanged.
        //
        // That the ladder survives intact here is the whole reason
        // FLAP_REDIAL_CEILING is 6 rather than 3: a ceiling of 3 would have
        // clamped attempts 3+ to the 30s cap, swallowing the 5s and 15s rungs
        // and turning a ten-second Dhan blip into a ~30s blind window on a
        // feed with no snapshot-on-subscribe. The damper must not make the
        // most common failure worse.
        assert_eq!(
            seen,
            vec![
                SHORT_SESSION_REDIAL_FLOOR_MS,
                1_000,
                2_000,
                5_000,
                15_000,
                30_000,
                30_000
            ]
        );
    }

    // -- flap damper (the RST-cascade fix, 2026-08-19) ----------------------

    /// Drives one full connect -> frame -> disconnect cycle and returns the
    /// re-dial delay the supervisor chose.
    ///
    /// `live_for` is how long the socket carries frames before it drops, which
    /// is the input the damper actually judges. Returns `(delay_ms, now)` so
    /// the caller can chain cycles on a single advancing clock.
    fn one_session(
        s: &mut ConnectionSupervisor,
        now: Instant,
        live_for: Duration,
    ) -> (u64, Instant) {
        let _ = s.on_event(ConnEvent::BeginDial, now);
        let _ = s.on_event(ConnEvent::DialSucceeded, now);
        let _ = s.on_event(ConnEvent::SubscribeAcked, now);
        let _ = s.on_event(ConnEvent::FrameReceived, now);
        let dropped_at = now + live_for;
        match s.on_event(ConnEvent::Disconnected { code: None }, dropped_at) {
            SupervisorAction::SleepThenDial { delay_ms } => (delay_ms, dropped_at),
            other => panic!("expected SleepThenDial, got {other:?}"),
        }
    }

    /// The instant rung must SURVIVE for the case it exists for: a clean,
    /// isolated drop after a genuinely healthy session. This is the latency
    /// win the damper is not allowed to cost us.
    #[test]
    fn test_supervisor_instant_retry_survives_a_genuinely_healthy_session() {
        let now = t0();
        // Slot 0 of the main feed carries zero stagger, so any non-zero result
        // here is the damper and nothing else.
        let mut s = sup(DhanEndpointType::MainFeed, 0, now);

        let live_for = Duration::from_millis(MIN_HEALTHY_SESSION_MS + 1_000);
        let (delay, _) = one_session(&mut s, now, live_for);

        assert_eq!(
            delay,
            0,
            "a socket that carried frames for {}ms then took ONE isolated drop must keep the \
             instant first retry — the damper exists to withhold it from sockets that never \
             earned it, not to remove it",
            MIN_HEALTHY_SESSION_MS + 1_000
        );
    }

    /// THE SECOND DEFECT (HIGH). The first `FrameReceived` sets
    /// `proven_healthy` and resets `attempt` to 0, and the `Disconnected` arm
    /// reads the CURRENT attempt before `enter_backoff` increments it — so a
    /// socket that yields exactly one frame then drops re-dialled at 0ms,
    /// forever, with no flap-rate ceiling at all.
    #[test]
    fn test_supervisor_one_frame_connection_never_re_dials_instantly() {
        let now = t0();
        let mut s = sup(DhanEndpointType::MainFeed, 0, now);

        // One frame, then gone 40ms later. Ten cycles in a row.
        let mut clock = now;
        for cycle in 0..10 {
            let (delay, after) = one_session(&mut s, clock, Duration::from_millis(40));
            assert!(
                delay > 0,
                "cycle {cycle}: a one-frame socket re-dialled instantly ({delay}ms) — this is \
                 the unbounded flap the damper exists to stop"
            );
            assert!(
                delay >= SHORT_SESSION_REDIAL_FLOOR_MS,
                "cycle {cycle}: delay {delay}ms is below the short-session floor"
            );
            clock = after + Duration::from_millis(delay);
        }
    }

    /// THE FIRST DEFECT (CRITICAL). A bare TCP reset arrives as
    /// `Closed { code: None }` and classifies `Transient`. On a socket that
    /// never delivered a frame that used to be an instant re-dial — which,
    /// under Dhan's 805 oldest-socket-dies semantics, evicts a healthy sibling
    /// and starts a self-sustaining cascade across the sixteen sockets.
    #[test]
    fn test_supervisor_bare_reset_on_an_unproven_socket_does_not_re_dial_instantly() {
        let now = t0();
        for pool_index in 0..5_u8 {
            let mut s = sup(DhanEndpointType::MainFeed, pool_index, now);
            let _ = s.on_event(ConnEvent::BeginDial, now);
            let _ = s.on_event(ConnEvent::DialSucceeded, now);
            // No frame ever arrives — the socket is RST'd during subscribe.
            match s.on_event(ConnEvent::Disconnected { code: None }, now) {
                SupervisorAction::SleepThenDial { delay_ms } => assert!(
                    delay_ms >= SHORT_SESSION_REDIAL_FLOOR_MS,
                    "pool_index {pool_index}: bare RST re-dialled in {delay_ms}ms"
                ),
                other => panic!("expected SleepThenDial, got {other:?}"),
            }
        }
    }

    /// The flap CEILING: a socket can look healthy on every single session and
    /// still be flapping. Health alone cannot see that; the rate ceiling can.
    #[test]
    fn test_supervisor_flap_ceiling_forces_backoff_on_a_socket_that_looks_healthy() {
        let now = t0();
        let mut s = sup(DhanEndpointType::MainFeed, 0, now);

        // Each session is comfortably "healthy" by the duration test, and the
        // whole run stays inside one flap window.
        let live_for = Duration::from_millis(MIN_HEALTHY_SESSION_MS + 1_000);
        let mut clock = now;
        let mut delays = Vec::new();
        for _ in 0..=FLAP_REDIAL_CEILING {
            let (delay, after) = one_session(&mut s, clock, live_for);
            delays.push(delay);
            clock = after;
        }

        let ceiling = usize::try_from(FLAP_REDIAL_CEILING).unwrap_or(usize::MAX);
        for (i, delay) in delays.iter().take(ceiling).enumerate() {
            assert_eq!(
                *delay, 0,
                "re-dial {i} is below the ceiling and each session looked healthy, so the \
                 instant rung is still correct"
            );
        }
        assert_eq!(
            delays.get(ceiling).copied(),
            Some(FLAP_CEILING_REDIAL_FLOOR_MS),
            "re-dial {ceiling} crossed FLAP_REDIAL_CEILING inside FLAP_WINDOW_MS and MUST be \
             forced onto backoff regardless of how healthy each session looked"
        );
        // And the clamp is sticky while the window still holds the history.
        let (next, _) = one_session(&mut s, clock, live_for);
        assert_eq!(next, FLAP_CEILING_REDIAL_FLOOR_MS);
    }

    /// The ceiling must RELEASE. A socket that recovers should not be punished
    /// for the rest of the trading day — that is why the window is rolling.
    #[test]
    fn test_supervisor_flap_ceiling_releases_once_the_window_has_passed() {
        let now = t0();
        let mut s = sup(DhanEndpointType::MainFeed, 0, now);
        let live_for = Duration::from_millis(MIN_HEALTHY_SESSION_MS + 1_000);

        let mut clock = now;
        for _ in 0..=FLAP_REDIAL_CEILING {
            let (_, after) = one_session(&mut s, clock, live_for);
            clock = after;
        }
        // Confirm we really are clamped before testing the release.
        assert_eq!(s.recent_redial_count(clock), FLAP_REDIAL_CEILING + 1);

        // Walk the clock past the whole window with no further re-dials.
        let released = clock + Duration::from_millis(FLAP_WINDOW_MS + 1_000);
        assert_eq!(
            s.recent_redial_count(released),
            0,
            "every recorded re-dial has aged out of the rolling window"
        );
        let (delay, _) = one_session(&mut s, released, live_for);
        assert_eq!(
            delay, 0,
            "a recovered socket gets its instant retry back once the window has passed"
        );
    }

    /// The damper must never act in silence. `enter_backoff` counts exactly
    /// the verdicts that `is_damped()` reports, so this pins the value that
    /// drives `FLAP_DAMPED_METRIC` against the delay actually chosen — a
    /// damped delay with an undamped verdict would be an uncounted action.
    #[test]
    fn test_supervisor_damped_redials_are_the_ones_reported_as_damped() {
        let now = t0();
        let mut s = sup(DhanEndpointType::MainFeed, 0, now);
        // Slot 0 has zero stagger, so raw == damped whenever the verdict is
        // Ladder and raw < damped exactly when it is not.
        let raw = reconnect_delay_ms(s.attempt());

        // Unproven socket -> damped, and the delay really did move.
        let decision = s.damped_decision(now);
        assert!(
            decision.verdict.is_damped(),
            "an unproven socket must be reported as damped"
        );
        assert!(
            decision.delay_ms > raw,
            "reported damped but the delay did not change"
        );
        assert_eq!(decision.verdict.as_str(), "short_session");

        // Healthy socket -> undamped, and the delay is untouched.
        let _ = s.on_event(ConnEvent::BeginDial, now);
        let _ = s.on_event(ConnEvent::DialSucceeded, now);
        let _ = s.on_event(ConnEvent::FrameReceived, now);
        let healthy_at = now + Duration::from_millis(MIN_HEALTHY_SESSION_MS + 1);
        let decision = s.damped_decision(healthy_at);
        assert!(!decision.verdict.is_damped());
        assert_eq!(decision.delay_ms, reconnect_delay_ms(s.attempt()));
    }

    /// The health clock is per-SOCKET, not per-process: a dial wipes it, so a
    /// previous session's health can never vouch for the next connection.
    #[test]
    fn test_supervisor_health_clock_resets_on_every_dial() {
        let now = t0();
        let mut s = sup(DhanEndpointType::MainFeed, 0, now);
        let _ = s.on_event(ConnEvent::BeginDial, now);
        let _ = s.on_event(ConnEvent::FrameReceived, now);

        let much_later = now + Duration::from_millis(MIN_HEALTHY_SESSION_MS * 10);
        assert!(s.healthy_duration_ms(much_later) >= MIN_HEALTHY_SESSION_MS);

        // A fresh dial starts a NEW socket, which has proved nothing yet.
        let _ = s.on_event(ConnEvent::BeginDial, much_later);
        assert_eq!(
            s.healthy_duration_ms(much_later + Duration::from_secs(1)),
            0,
            "the new socket must not inherit the old socket's health"
        );
    }

    /// The re-dial history is a fixed inline ring — it must never allocate and
    /// must stay accurate across wraparound.
    #[test]
    fn test_supervisor_recent_redial_count_wraps_without_losing_the_window() {
        let now = t0();
        let mut s = sup(DhanEndpointType::MainFeed, 0, now);
        assert_eq!(s.recent_redial_count(now), 0);

        // Overfill the ring; the count saturates at its capacity, which is all
        // it ever needs to distinguish (it is only compared to the ceiling).
        for i in 0..(FLAP_HISTORY_SLOTS * 3) {
            s.record_redial(now + Duration::from_millis(i as u64));
        }
        let count = s.recent_redial_count(now + Duration::from_secs(1));
        assert_eq!(count as usize, FLAP_HISTORY_SLOTS);
        assert!(
            count >= FLAP_REDIAL_CEILING,
            "the ring must hold enough history to reach the ceiling"
        );

        // Everything ages out together once the window passes.
        assert_eq!(
            s.recent_redial_count(now + Duration::from_millis(FLAP_WINDOW_MS + 1_000)),
            0
        );
    }

    #[test]
    fn test_supervisor_attempt_resets_on_first_frame_not_on_a_successful_dial() {
        // A socket that connects and subscribes but never delivers anything is
        // NOT healthy. Resetting on dial would let it re-dial instantly for
        // ever; resetting on the first frame makes the ladder bite.
        let now = t0();
        let mut s = sup(DhanEndpointType::MainFeed, 0, now);
        for _ in 0..3 {
            let _ = s.on_event(ConnEvent::BeginDial, now);
            let _ = s.on_event(ConnEvent::DialFailed, now);
        }
        assert_eq!(s.attempt(), 3);

        let _ = s.on_event(ConnEvent::BeginDial, now);
        assert_eq!(
            s.on_event(ConnEvent::DialSucceeded, now),
            SupervisorAction::Subscribe
        );
        assert_eq!(s.attempt(), 3, "a successful dial alone proves nothing");

        let _ = s.on_event(ConnEvent::SubscribeAcked, now);
        assert_eq!(
            s.attempt(),
            3,
            "a successful subscribe alone proves nothing"
        );

        let _ = s.on_event(ConnEvent::FrameReceived, now);
        assert_eq!(s.attempt(), 0, "the first frame is the health proof");
        assert_eq!(s.frames_received(), 1);
    }

    #[test]
    fn test_supervisor_805_parks_permanently_and_never_dials_again() {
        let now = t0();
        let mut s = sup(DhanEndpointType::MainFeed, 2, now);
        let _ = s.on_event(ConnEvent::BeginDial, now);
        let _ = s.on_event(ConnEvent::DialSucceeded, now);
        let _ = s.on_event(ConnEvent::SubscribeAcked, now);

        assert_eq!(
            s.on_event(
                ConnEvent::Disconnected {
                    code: Some(DisconnectCode::ExceededActiveConnections)
                },
                now
            ),
            SupervisorAction::Park {
                reason: ParkReason::PoolOverflow
            }
        );
        assert_eq!(s.phase(), ConnPhase::Parked);
        assert_eq!(s.park_reason(), Some(ParkReason::PoolOverflow));

        // The critical property: nothing can talk it back into dialing.
        for ev in [
            ConnEvent::BeginDial,
            ConnEvent::DialSucceeded,
            ConnEvent::FrameReceived,
            ConnEvent::IdleElapsed,
            ConnEvent::Disconnected { code: None },
        ] {
            assert_eq!(
                s.on_event(ev, now),
                SupervisorAction::Continue,
                "a parked 805 connection must never be re-dialed by {ev:?}"
            );
            assert_eq!(s.phase(), ConnPhase::Parked);
        }
    }

    #[test]
    fn test_supervisor_fatal_disconnect_parks() {
        let now = t0();
        for code in [
            DisconnectCode::AuthenticationFailed,
            DisconnectCode::ClientIdInvalid,
            DisconnectCode::DataApiSubscriptionRequired,
        ] {
            let mut s = sup(DhanEndpointType::Depth20, 1, now);
            let _ = s.on_event(ConnEvent::BeginDial, now);
            assert_eq!(
                s.on_event(ConnEvent::Disconnected { code: Some(code) }, now),
                SupervisorAction::Park {
                    reason: ParkReason::FatalDisconnect
                },
                "{code:?}"
            );
        }
    }

    #[test]
    fn test_supervisor_token_stale_demands_a_refresh_and_floors_the_delay() {
        let now = t0();
        let mut s = sup(DhanEndpointType::MainFeed, 0, now);
        let _ = s.on_event(ConnEvent::BeginDial, now);
        let _ = s.on_event(ConnEvent::DialSucceeded, now);

        // Attempt 0's bare ladder rung is 0ms; a stale token must NOT be
        // re-presented instantly — that is a 16-way rejection storm.
        match s.on_event(
            ConnEvent::Disconnected {
                code: Some(DisconnectCode::AccessTokenExpired),
            },
            now,
        ) {
            SupervisorAction::RefreshTokenThenDial { delay_ms } => {
                assert_eq!(delay_ms, TOKEN_STALE_REDIAL_FLOOR_MS);
                assert!(delay_ms > reconnect_delay_ms(0));
            }
            other => panic!("expected RefreshTokenThenDial, got {other:?}"),
        }
    }

    #[test]
    fn test_supervisor_token_stale_floor_never_shortens_a_longer_ladder_rung() {
        let now = t0();
        let mut s = sup(DhanEndpointType::MainFeed, 0, now);
        for _ in 0..6 {
            let _ = s.on_event(ConnEvent::BeginDial, now);
            let _ = s.on_event(ConnEvent::DialFailed, now);
        }
        let _ = s.on_event(ConnEvent::BeginDial, now);
        match s.on_event(
            ConnEvent::Disconnected {
                code: Some(DisconnectCode::AccessTokenInvalid),
            },
            now,
        ) {
            SupervisorAction::RefreshTokenThenDial { delay_ms } => {
                assert_eq!(delay_ms, 30_000, "the floor is a floor, not a clamp");
            }
            other => panic!("expected RefreshTokenThenDial, got {other:?}"),
        }
    }

    /// The pre-open reconnect-storm regression.
    ///
    /// Measured on prod 2026-08-19: 8–25 reconnects PER MINUTE from 08:31 to
    /// 08:59 IST, then exactly zero after the 09:15 open. The market is shut
    /// pre-open, so no instrument ticks; Dhan pinged to hold the socket open;
    /// the read loop counted the ping and looped without telling the
    /// supervisor, so the watchdog expired on DATA silence and tore down a
    /// healthy connection roughly 300 times a morning — each one a full
    /// re-auth plus a re-subscribe of the whole universe.
    ///
    /// Pre-fix this test fails at the FIRST assertion: without the keep-alive
    /// the poll at the timeout returns `SleepThenDial`.
    #[test]
    fn test_supervisor_keepalive_prevents_the_pre_open_reconnect_storm() {
        use super::super::idle_watchdog::IDLE_RECONNECT_TIMEOUT_SECS;
        let now = t0();
        let mut s = sup(DhanEndpointType::MainFeed, 0, now);
        let _ = s.on_event(ConnEvent::BeginDial, now);
        let _ = s.on_event(ConnEvent::DialSucceeded, now);
        let _ = s.on_event(ConnEvent::SubscribeAcked, now);

        // Walk 10 minutes of pre-open silence, pinged every 20s the way Dhan
        // holds a connection open. Not one of them may reconnect.
        let mut at = now;
        for _ in 0..30 {
            at += Duration::from_secs(20);
            assert_eq!(
                s.on_event(ConnEvent::KeepAliveReceived, at),
                SupervisorAction::Continue,
                "a ping is proof of life, never a reason to redial"
            );
            assert_eq!(
                s.poll(at + Duration::from_secs(IDLE_RECONNECT_TIMEOUT_SECS - 1)),
                SupervisorAction::Continue,
                "the watchdog must have been reset by the ping"
            );
        }
        assert_eq!(
            s.reconnects(),
            0,
            "ten minutes of pinged pre-open silence must cost ZERO reconnects \
             (prod was paying ~300 a morning)"
        );

        // The other half of the contract: a ping proves the TRANSPORT is
        // alive, never that DATA flows. It must not fake frames or health —
        // a silently-failed subscribe belongs to the RISK-GAP-03 silence
        // scan, and letting a heartbeat claim health would hide it.
        assert_eq!(s.frames_received(), 0, "a ping is not a data frame");

        // And the watchdog still bites when even the pings stop: that is a
        // genuinely dead transport.
        at += Duration::from_secs(20);
        let _ = s.on_event(ConnEvent::KeepAliveReceived, at);
        assert!(
            matches!(
                s.poll(at + Duration::from_secs(IDLE_RECONNECT_TIMEOUT_SECS)),
                SupervisorAction::SleepThenDial { .. }
            ),
            "silence with NO pings at all is a dead socket and must still redial"
        );
    }

    #[test]
    fn test_supervisor_idle_timeout_reconnects_before_dhan_would_close() {
        use super::super::idle_watchdog::{
            DHAN_SERVER_CLOSE_AFTER_SILENCE_SECS, IDLE_RECONNECT_TIMEOUT_SECS,
        };
        let now = t0();
        let mut s = sup(DhanEndpointType::MainFeed, 0, now);
        let _ = s.on_event(ConnEvent::BeginDial, now);
        let _ = s.on_event(ConnEvent::DialSucceeded, now);
        let _ = s.on_event(ConnEvent::SubscribeAcked, now);
        let _ = s.on_event(ConnEvent::FrameReceived, now);

        // One second before the threshold: still quiet, still fine.
        assert_eq!(
            s.poll(now + Duration::from_secs(IDLE_RECONNECT_TIMEOUT_SECS - 1)),
            SupervisorAction::Continue
        );
        // At the threshold: we tear down on OUR terms.
        let at = now + Duration::from_secs(IDLE_RECONNECT_TIMEOUT_SECS);
        assert!(matches!(s.poll(at), SupervisorAction::SleepThenDial { .. }));
        assert_eq!(s.phase(), ConnPhase::Backoff);
        assert!(
            IDLE_RECONNECT_TIMEOUT_SECS < DHAN_SERVER_CLOSE_AFTER_SILENCE_SECS,
            "we must always act before Dhan's documented server-side close"
        );
    }

    #[test]
    fn test_supervisor_idle_watchdog_is_disarmed_during_backoff() {
        // Otherwise a 30s ladder rung would be cancelled by the 27s watchdog
        // and the ladder could never reach its cap.
        let now = t0();
        let mut s = sup(DhanEndpointType::MainFeed, 0, now);
        let _ = s.on_event(ConnEvent::BeginDial, now);
        let _ = s.on_event(ConnEvent::DialFailed, now);
        assert_eq!(s.phase(), ConnPhase::Backoff);
        assert_eq!(
            s.poll(now + Duration::from_secs(600)),
            SupervisorAction::Continue
        );
        assert_eq!(s.phase(), ConnPhase::Backoff);
    }

    #[test]
    fn test_supervisor_frame_arriving_before_our_subscribe_ack_promotes_to_live() {
        // Dhan pushes the prev-close packet on subscribe, so a frame can beat
        // our own batch-completion bookkeeping.
        let now = t0();
        let mut s = sup(DhanEndpointType::MainFeed, 0, now);
        let _ = s.on_event(ConnEvent::BeginDial, now);
        let _ = s.on_event(ConnEvent::DialSucceeded, now);
        assert_eq!(s.phase(), ConnPhase::Subscribing);
        let _ = s.on_event(ConnEvent::FrameReceived, now);
        assert_eq!(s.phase(), ConnPhase::Live);
    }

    #[test]
    fn test_supervisor_subscribe_failure_tears_the_socket_down() {
        // A connected-but-blind socket is worse than no socket: it consumes a
        // pool slot and delivers nothing.
        let now = t0();
        let mut s = sup(DhanEndpointType::MainFeed, 0, now);
        let _ = s.on_event(ConnEvent::BeginDial, now);
        let _ = s.on_event(ConnEvent::DialSucceeded, now);
        assert!(matches!(
            s.on_event(ConnEvent::SubscribeFailed, now),
            SupervisorAction::SleepThenDial { .. }
        ));
        assert_eq!(s.phase(), ConnPhase::Backoff);
    }

    #[test]
    fn test_supervisor_shutdown_parks_with_the_shutdown_reason() {
        let now = t0();
        let mut s = sup(DhanEndpointType::Depth200, 3, now);
        assert_eq!(
            s.on_event(ConnEvent::ShutdownRequested, now),
            SupervisorAction::Park {
                reason: ParkReason::Shutdown
            }
        );
        assert_eq!(s.park_reason(), Some(ParkReason::Shutdown));
    }

    #[test]
    fn test_supervisor_connection_state_projection_covers_every_phase() {
        let now = t0();
        let mut s = sup(DhanEndpointType::MainFeed, 0, now);
        assert_eq!(s.connection_state(), ConnectionState::Disconnected);
        let _ = s.on_event(ConnEvent::BeginDial, now);
        assert_eq!(s.connection_state(), ConnectionState::Connecting);
        let _ = s.on_event(ConnEvent::DialSucceeded, now);
        assert_eq!(s.connection_state(), ConnectionState::Connected);
        let _ = s.on_event(ConnEvent::SubscribeAcked, now);
        assert_eq!(s.connection_state(), ConnectionState::Connected);
        let _ = s.on_event(ConnEvent::Disconnected { code: None }, now);
        assert_eq!(s.connection_state(), ConnectionState::Reconnecting);
        assert_eq!(s.reconnects(), 1);
        let _ = s.on_event(ConnEvent::ShutdownRequested, now);
        assert_eq!(s.connection_state(), ConnectionState::Disconnected);
    }

    #[test]
    fn test_supervisor_slot_is_reported_verbatim() {
        let now = t0();
        let s = sup(DhanEndpointType::Depth20, 4, now);
        assert_eq!(s.slot().endpoint, DhanEndpointType::Depth20);
        assert_eq!(s.slot().pool_index, 4);
        assert_eq!(
            s.slot().global_index,
            DhanEndpointType::Depth20.jitter_base() + 4
        );
    }

    // -- thundering-herd guard ---------------------------------------------

    #[test]
    fn test_all_sixteen_simultaneous_drops_receive_distinct_delays() {
        // The scenario the design calls out: every socket drops in the same
        // instant. If they all redialled together we would hand Dhan sixteen
        // handshakes plus sixteen subscribe bursts inside one scheduler tick.
        let now = t0();
        let mut delays = Vec::new();
        for endpoint in DhanEndpointType::ALL {
            for pool_index in 0..endpoint.max_connections() {
                let mut s = sup(endpoint, pool_index, now);
                let _ = s.on_event(ConnEvent::BeginDial, now);
                let _ = s.on_event(ConnEvent::DialSucceeded, now);
                let _ = s.on_event(ConnEvent::SubscribeAcked, now);
                match s.on_event(ConnEvent::Disconnected { code: None }, now) {
                    SupervisorAction::SleepThenDial { delay_ms } => delays.push(delay_ms),
                    other => panic!("expected SleepThenDial, got {other:?}"),
                }
            }
        }
        assert_eq!(delays.len(), 16, "the authorized ceiling is 16 connections");
        let unique: BTreeSet<u64> = delays.iter().copied().collect();
        assert_eq!(
            unique.len(),
            16,
            "every one of the sixteen must get its own delay: {delays:?}"
        );
        // And the spread stays negligible against the reconnect path itself.
        let spread =
            delays.iter().max().copied().unwrap_or(0) - delays.iter().min().copied().unwrap_or(0);
        assert_eq!(spread, 15 * RECONNECT_JITTER_STEP_MS);
    }

    #[test]
    fn test_token_expiry_drops_all_sixteen_and_they_still_receive_distinct_delays() {
        // The test above uses `code: None` — a Transient disconnect, which
        // never touches the token-stale floor. So it proved the jitter works
        // on the ONE path where sixteen sockets do NOT reliably drop together,
        // and proved nothing about the path where they always do.
        //
        // Token expiry (807) is the real simultaneous-drop event: one JWT
        // backs all sixteen sockets, so when it dies Dhan closes all sixteen
        // inside the same second. That arm floors the delay at 5,000 ms, and
        // until 2026-08-11 it floored the ALREADY-JITTERED value — every
        // jittered delay on ladder rungs 0/1/2 is below 5,000, so `max`
        // flattened all sixteen onto exactly 5,000 ms. Sixteen simultaneous
        // wakeups, sixteen token renewals, sixteen handshakes, in one tick.
        //
        // This test fails on that code and passes on the fix.
        let now = t0();
        let mut delays = Vec::new();
        for endpoint in DhanEndpointType::ALL {
            for pool_index in 0..endpoint.max_connections() {
                let mut s = sup(endpoint, pool_index, now);
                let _ = s.on_event(ConnEvent::BeginDial, now);
                let _ = s.on_event(ConnEvent::DialSucceeded, now);
                let _ = s.on_event(ConnEvent::SubscribeAcked, now);
                match s.on_event(
                    ConnEvent::Disconnected {
                        code: Some(DisconnectCode::AccessTokenExpired),
                    },
                    now,
                ) {
                    SupervisorAction::RefreshTokenThenDial { delay_ms } => delays.push(delay_ms),
                    other => panic!("807 must refresh the token then dial, got {other:?}"),
                }
            }
        }

        assert_eq!(delays.len(), 16, "the authorized ceiling is 16 connections");
        let unique: BTreeSet<u64> = delays.iter().copied().collect();
        assert_eq!(
            unique.len(),
            16,
            "a token expiry drops ALL sixteen at once — each must still wake at its own \
             instant, or we hand Dhan sixteen simultaneous renewals: {delays:?}"
        );

        // Every one still respects the floor: the point of the fix is to keep
        // the floor AND the fan-out, not to trade one for the other.
        for d in &delays {
            assert!(
                *d >= TOKEN_STALE_REDIAL_FLOOR_MS,
                "a token-stale redial must never be shorter than the floor: {d}"
            );
        }
        // Index 0 gets zero stagger, so the minimum is exactly the floor.
        assert_eq!(
            delays.iter().min().copied().unwrap_or(0),
            TOKEN_STALE_REDIAL_FLOOR_MS
        );
        assert_eq!(
            delays.iter().max().copied().unwrap_or(0),
            TOKEN_STALE_REDIAL_FLOOR_MS + 15 * RECONNECT_JITTER_STEP_MS
        );
    }

    proptest! {
        #[test]
        fn prop_on_event_is_total_and_delays_stay_bounded(
            endpoint_idx in 0usize..4,
            pool_index in 0u8..5,
            events in prop::collection::vec(0u8..9, 1..60),
        ) {
            let now = Instant::now();
            let endpoint = DhanEndpointType::ALL[endpoint_idx];
            let mut s = ConnectionSupervisor::new(
                ConnectionSlot {
                    endpoint,
                    pool_index,
                    global_index: endpoint.jitter_base().saturating_add(pool_index),
                },
                now,
            );
            for e in events {
                let event = match e {
                    0 => ConnEvent::BeginDial,
                    1 => ConnEvent::DialSucceeded,
                    2 => ConnEvent::DialFailed,
                    3 => ConnEvent::SubscribeAcked,
                    4 => ConnEvent::SubscribeFailed,
                    5 => ConnEvent::FrameReceived,
                    6 => ConnEvent::Disconnected { code: None },
                    7 => ConnEvent::IdleElapsed,
                    _ => ConnEvent::Disconnected {
                        code: Some(DisconnectCode::InternalServerError),
                    },
                };
                match s.on_event(event, now) {
                    SupervisorAction::SleepThenDial { delay_ms } => {
                        prop_assert!(delay_ms <= RECONNECT_DELAY_WITH_JITTER_MAX_MS);
                    }
                    SupervisorAction::RefreshTokenThenDial { delay_ms } => {
                        prop_assert!(
                            delay_ms
                                <= RECONNECT_DELAY_WITH_JITTER_MAX_MS
                                    .max(TOKEN_STALE_REDIAL_FLOOR_MS)
                        );
                    }
                    _ => {}
                }
            }
        }
    }

    // -- subscribe guard ----------------------------------------------------

    #[test]
    fn test_subscribe_guard_try_new_refuses_a_set_above_the_per_connection_cap() {
        let over = usize::try_from(DhanEndpointType::Depth20.max_instruments_per_connection() + 1)
            .unwrap_or(usize::MAX);
        let err = SubscribeGuard::try_new(DhanEndpointType::Depth20, instruments(over))
            .expect_err("51 instruments on a 50-cap depth-20 connection must be refused");
        assert!(matches!(
            err,
            SubscribeGuardRefusal::TooManyInstruments { max: 50, .. }
        ));
    }

    #[test]
    fn test_subscribe_guard_accepts_exactly_the_cap() {
        let at_cap = usize::try_from(DhanEndpointType::MainFeed.max_instruments_per_connection())
            .unwrap_or(usize::MAX);
        let g = SubscribeGuard::try_new(DhanEndpointType::MainFeed, instruments(at_cap))
            .expect("5,000 instruments is exactly the documented main-feed cap");
        assert_eq!(g.len(), 5_000);
        assert!(!g.is_empty());
        assert_eq!(g.endpoint(), DhanEndpointType::MainFeed);
    }

    #[test]
    fn test_subscribe_guard_batches_and_batch_count_respect_the_per_message_cap() {
        // Main feed: 100 per message (15-live-market-feed.md:50).
        let g = SubscribeGuard::try_new(DhanEndpointType::MainFeed, instruments(250))
            .expect("250 is inside the 5,000 cap");
        let batches: Vec<usize> = g.batches().map(<[SubscribeInstrument]>::len).collect();
        assert_eq!(batches, vec![100, 100, 50]);
        assert_eq!(g.batch_count(), 3);

        // Depth-20 explicitly permits all 50 in one message.
        let d = SubscribeGuard::try_new(DhanEndpointType::Depth20, instruments(50))
            .expect("50 is the depth-20 cap");
        assert_eq!(d.batch_count(), 1);
    }

    #[test]
    fn test_subscribe_guard_batches_cover_every_instrument_exactly_once() {
        let g = SubscribeGuard::try_new(DhanEndpointType::MainFeed, instruments(1_003))
            .expect("inside cap");
        let seen: Vec<SecurityId> = g
            .batches()
            .flat_map(|b| b.iter().map(|i| i.security_id))
            .collect();
        assert_eq!(
            seen.len(),
            1_003,
            "no instrument may be dropped by batching"
        );
        let unique: BTreeSet<SecurityId> = seen.iter().copied().collect();
        assert_eq!(unique.len(), 1_003, "no instrument may be duplicated");
    }

    #[test]
    fn test_subscribe_guard_mark_lost_then_mark_confirmed_survives_reconnect() {
        // The whole point: a reconnect REPLAYS the set, it does not rebuild it.
        let g0 = instruments(120);
        let mut g =
            SubscribeGuard::try_new(DhanEndpointType::MainFeed, g0.clone()).expect("inside cap");
        assert!(g.needs_resubscribe());
        g.mark_confirmed();
        assert!(g.is_confirmed());
        assert_eq!(g.generation(), 1);

        g.mark_lost();
        assert!(
            g.needs_resubscribe(),
            "a lost socket must force a resubscribe"
        );
        let replayed: Vec<SubscribeInstrument> =
            g.batches().flat_map(|b| b.iter().copied()).collect();
        assert_eq!(
            replayed, g0,
            "the set must survive the socket byte for byte"
        );

        g.mark_confirmed();
        assert_eq!(
            g.generation(),
            2,
            "each incarnation gets its own generation"
        );
    }

    #[test]
    fn test_subscribe_guard_depth_200_is_one_instrument_per_connection() {
        assert!(
            SubscribeGuard::try_new(DhanEndpointType::Depth200, instruments(1)).is_ok(),
            "200-level depth is one instrument per connection"
        );
        assert!(
            SubscribeGuard::try_new(DhanEndpointType::Depth200, instruments(2)).is_err(),
            "two instruments on a depth-200 connection must be refused locally"
        );
    }

    #[test]
    fn test_subscribe_guard_zero_cap_endpoint_does_not_panic_on_batching() {
        // order-update reports a zero per-message cap and carries no
        // instruments; `chunks(0)` would panic, so the batcher floors at 1.
        let g = SubscribeGuard::try_new(DhanEndpointType::OrderUpdate, Vec::new())
            .expect("an empty set is always admissible");
        assert!(g.is_empty());
        assert_eq!(g.batch_count(), 0);
    }

    // -- frame sink: THE ONE RULE ------------------------------------------

    /// Recording sink used to prove ordering and to prove the drain loop does
    /// nothing else per frame.
    #[derive(Default)]
    struct RecordingSink {
        accepted: Mutex<Vec<Bytes>>,
    }

    impl FrameSink for RecordingSink {
        fn accept(&self, frame: Bytes) -> FrameSinkOutcome {
            if let Ok(mut g) = self.accepted.lock() {
                g.push(frame);
            }
            FrameSinkOutcome::Captured
        }
    }

    fn wal_dir(name: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!("tv-poolsup-{name}-{}-{nanos}", std::process::id()))
    }

    #[test]
    fn test_wal_ring_sink_captures_durably_then_publishes() {
        let dir = wal_dir("capture");
        let spill = std::sync::Arc::new(
            WsFrameSpill::new(&dir).expect("WAL must open under a fresh temp dir"),
        );
        let (tx, mut rx) = tokio::sync::mpsc::channel::<CapturedFrame>(4);
        let sink = WalRingSink::new(
            std::sync::Arc::clone(&spill),
            tx,
            std::sync::Arc::new(RingByteBudget::new(usize::MAX)),
            WsType::LiveFeed,
            DhanEndpointType::MainFeed,
            0,
        );

        let frame = Bytes::from_static(&[2u8, 16, 0, 0, 0, 0, 0, 0]);
        assert_eq!(sink.accept(frame.clone()), FrameSinkOutcome::Captured);
        let published = rx.try_recv().expect("a captured frame must be published");
        assert_eq!(published.bytes, frame);
        assert!(
            published.seq > 0,
            "the published frame must carry the sequence minted at the read \
             instant — a zero would mean nothing stamped it"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn with_audit_is_silent_until_opted_in_and_names_its_own_endpoint() {
        // Two properties in one test because they are the same property from
        // both sides: a sink built the old way must emit NOTHING (so every
        // existing caller, test and bench is unchanged by construction), and
        // an opted-in sink must report the endpoint it actually serves.
        //
        // The endpoint half is the one with teeth. The sink's `ws_type` field
        // is `LiveFeed` for ALL fifteen market-data sockets — it is the WAL
        // record discriminant, not the socket's identity — so a consumer that
        // built the audit row from it would file a depth-200 park under the
        // main feed and leave the table unable to answer which pool went dark,
        // which is the only question it exists to answer. Carrying the
        // ENDPOINT on the event is what makes that mistake unavailable
        // downstream.
        use tickvault_common::ws_event_types::WsEventKind;

        let dir = wal_dir("lifecycle");
        let spill = std::sync::Arc::new(
            WsFrameSpill::new(&dir).expect("WAL must open under a fresh temp dir"),
        );
        let budget = std::sync::Arc::new(RingByteBudget::new(usize::MAX));
        let (frames_tx, _frames_rx) = tokio::sync::mpsc::channel::<CapturedFrame>(4);

        let plain = WalRingSink::new(
            std::sync::Arc::clone(&spill),
            frames_tx.clone(),
            std::sync::Arc::clone(&budget),
            WsType::LiveFeed,
            DhanEndpointType::MainFeed,
            0,
        );
        // No channel attached: the default path must not panic and must not
        // need one.
        plain.on_lifecycle(WsEventKind::Disconnected, "no_channel_attached");

        for endpoint in [
            DhanEndpointType::MainFeed,
            DhanEndpointType::Depth20,
            DhanEndpointType::Depth200,
        ] {
            let (audit_tx, mut audit_rx) = tokio::sync::mpsc::channel::<WsLifecycleEvent>(4);
            let sink = WalRingSink::new(
                std::sync::Arc::clone(&spill),
                frames_tx.clone(),
                std::sync::Arc::clone(&budget),
                // Deliberately the SAME ws_type for every endpoint — that is
                // exactly the real shape, and the reason the row must not be
                // built from this field.
                WsType::LiveFeed,
                endpoint,
                3,
            )
            .with_audit(audit_tx);

            sink.on_lifecycle(WsEventKind::Disconnected, "park_fatal");
            let event = audit_rx
                .try_recv()
                .expect("an opted-in sink must emit the lifecycle event");
            assert_eq!(
                event.endpoint, endpoint,
                "the event must name the ENDPOINT, not the sink's WAL \
                 discriminant — otherwise every socket files under one label \
                 and the audit cannot say which pool went dark"
            );
            assert_eq!(event.kind, WsEventKind::Disconnected);
            assert_eq!(event.connection_index, 3, "the socket's own index");
            assert_eq!(event.reason, "park_fatal");
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_supervisor_reports_connect_and_loss_to_the_sink() {
        // A source pin, because the emit points are inside an async loop whose
        // arms need a live socket and a real supervisor to reach. What must
        // never silently disappear is the CALL — the sink's own emit is unit
        // tested above, and a sink nobody tells about a park records nothing
        // while looking wired.
        //
        // CONNECTED is anchored on the subscribe-ack arm on purpose. The
        // 2026-08-12 blackout is the reason: twelve sockets dialed and every
        // one died on the handshake, so a row written at dial time would have
        // recorded twelve connections that never carried a byte.
        let src = include_str!("pool_supervisor.rs");
        let marker = concat!("#[cfg(", "test)]");
        let production = src.split(marker).next().unwrap_or(src);

        let park = production
            // The match ARM, not the enum construction at `on_event`'s return
            // — anchoring on the bare pattern matched that one first and
            // captured the wrong span. A guard that fails for a reason
            // unrelated to what it protects teaches the next reader to delete
            // it, so the anchor has to be the thing itself.
            .split("SupervisorAction::Park { reason } => {")
            .nth(1)
            .and_then(|s| s.split("return ConnectionExit::Parked").next())
            .expect("the park arm must exist");
        assert!(
            park.contains("sink.on_lifecycle"),
            "a park is PERMANENT — nothing re-dials that socket. It must reach \
             the audit, not only a log line and a counter:\n{park}"
        );

        let subscribe = production
            .split("guard.mark_confirmed();")
            .nth(1)
            .and_then(|s| s.split("action = supervisor.on_event").next())
            .expect("the subscribe-ack arm must exist");
        assert!(
            subscribe.contains("sink.on_lifecycle"),
            "CONNECTED must be recorded at subscribe-ack — the first instant \
             the socket can actually deliver:\n{subscribe}"
        );

        let redial_arms = production.matches("guard.mark_lost();").count();
        let audited_redials = production
            .split("guard.mark_lost();")
            .skip(1)
            .filter(|s| {
                s.split("action = supervisor.on_event")
                    .next()
                    .unwrap_or("")
                    .contains("sink.on_lifecycle")
            })
            .count();
        assert_eq!(
            audited_redials, redial_arms,
            "every arm that marks the subscription lost is a real disconnect \
             and must be audited — {audited_redials} of {redial_arms} are"
        );
    }

    #[test]
    fn test_wal_ring_sink_stamps_receipt_time_in_the_read_task() {
        // The published frame must carry a receipt stamp taken INSIDE
        // `accept` — i.e. on the read task — not left for the drain to
        // invent later. Bracketing the call proves the stamp belongs to the
        // arrival instant and not to whenever a consumer got around to it.
        //
        // Why this matters enough to test: the drain used to call
        // `Utc::now()` itself, so every lag sample measured
        // `Dhan's delivery + our own time queued in the ring`. Under a fold
        // stall the ring backs up and that number inflates — the lag alarm
        // would fire hardest when the fault was LOCAL and name the vendor
        // for it.
        let dir = wal_dir("recvstamp");
        let spill = std::sync::Arc::new(
            WsFrameSpill::new(&dir).expect("WAL must open under a fresh temp dir"),
        );
        let (tx, mut rx) = tokio::sync::mpsc::channel::<CapturedFrame>(4);
        let sink = WalRingSink::new(
            std::sync::Arc::clone(&spill),
            tx,
            std::sync::Arc::new(RingByteBudget::new(usize::MAX)),
            WsType::LiveFeed,
            DhanEndpointType::MainFeed,
            0,
        );

        let before = std::time::Instant::now();
        assert_eq!(
            sink.accept(Bytes::from_static(&[2u8, 16, 0, 0, 0, 0, 0, 0])),
            FrameSinkOutcome::Captured
        );
        let after = std::time::Instant::now();

        let published = rx.try_recv().expect("a captured frame must be published");
        assert!(
            published.received_at >= before && published.received_at <= after,
            "the receipt stamp must be taken inside accept() — it fell outside the \
             bracket taken around the call, so it was not stamped at receipt"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
    #[test]
    fn test_wal_ring_sink_stamps_a_distinct_ascending_seq_per_frame() {
        // Two arrivals of BYTE-IDENTICAL content must still receive distinct
        // sequences, or the DEDUP key would collapse them and the second tick
        // would be silently lost. This is the live index-loss class recorded in
        // data-integrity.md (23,146.45 → .75 → .45 on a volume-0 index).
        let dir = wal_dir("seqascend");
        let spill = std::sync::Arc::new(
            WsFrameSpill::new(&dir).expect("WAL must open under a fresh temp dir"),
        );
        let (tx, mut rx) = tokio::sync::mpsc::channel::<CapturedFrame>(4);
        let sink = WalRingSink::new(
            spill,
            tx,
            std::sync::Arc::new(RingByteBudget::new(usize::MAX)),
            WsType::LiveFeed,
            DhanEndpointType::MainFeed,
            0,
        );

        let frame = Bytes::from_static(&[2u8, 16, 0, 0, 0, 0, 0, 0]);
        assert_eq!(sink.accept(frame.clone()), FrameSinkOutcome::Captured);
        assert_eq!(sink.accept(frame), FrameSinkOutcome::Captured);

        let first = rx.try_recv().expect("first frame published");
        let second = rx.try_recv().expect("second frame published");
        assert!(
            second.seq > first.seq,
            "identical content must still get a strictly greater sequence: \
             {} then {}",
            first.seq,
            second.seq
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_wal_ring_sink_full_ring_is_lag_not_capture_loss() {
        // A full ring must NOT stop the reader: stopping costs the pong and
        // turns downstream lag into a disconnect. The frame is already durable.
        let dir = wal_dir("ringfull");
        let spill = std::sync::Arc::new(
            WsFrameSpill::new(&dir).expect("WAL must open under a fresh temp dir"),
        );
        let (tx, _rx) = tokio::sync::mpsc::channel::<CapturedFrame>(1);
        let sink = WalRingSink::new(
            spill,
            tx,
            std::sync::Arc::new(RingByteBudget::new(usize::MAX)),
            WsType::LiveFeed,
            DhanEndpointType::MainFeed,
            0,
        );

        assert_eq!(
            sink.accept(Bytes::from_static(b"first")),
            FrameSinkOutcome::Captured
        );
        assert_eq!(
            sink.accept(Bytes::from_static(b"second")),
            FrameSinkOutcome::RingFull,
            "a full ring is a lag signal, never silent capture loss"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    // -- ring byte budget ---------------------------------------------------

    #[test]
    fn test_ring_byte_budget_cap_reports_the_configured_ceiling() {
        // `cap()` is what the lane's boot line reports as `ring_max_bytes`.
        // It exists so the operator sees the queue's size in the unit that
        // actually runs out — reporting only the 65,536 frame count is how a
        // 16 GiB ceiling hid behind a number that looks modest.
        let b = RingByteBudget::new(256 * 1024 * 1024);
        assert_eq!(b.cap(), 256 * 1024 * 1024);
        assert_eq!(b.resident(), 0, "a fresh budget holds nothing");
        assert!(b.try_reserve(1_000));
        assert_eq!(
            b.cap(),
            256 * 1024 * 1024,
            "cap is the CEILING and must not move as frames come and go — only \
             resident() tracks occupancy"
        );
    }

    #[test]
    fn test_try_reserve_never_exceeds_the_cap() {
        let b = RingByteBudget::new(100);
        assert!(b.try_reserve(60));
        assert_eq!(b.resident(), 60);
        assert!(b.try_reserve(40), "exactly filling the cap must be allowed");
        assert_eq!(b.resident(), 100);
        assert!(!b.try_reserve(1), "one byte past the cap must be refused");
        assert_eq!(
            b.resident(),
            100,
            "a REFUSED reserve must not move the counter — a fetch_add-then-check \
             would have left the overshoot behind, which is the bound failing to bound"
        );
    }

    #[test]
    fn test_try_reserve_refuses_a_frame_larger_than_the_whole_cap() {
        let b = RingByteBudget::new(1_024);
        assert!(
            !b.try_reserve(4_096),
            "a frame bigger than the entire budget cannot be admitted — admitting it \
             would mean the budget does not bound"
        );
        assert_eq!(b.resident(), 0);
        // And the budget is still usable afterwards: one oversized frame must
        // not poison it for the frames that follow.
        assert!(b.try_reserve(512));
        assert_eq!(b.resident(), 512);
    }

    #[test]
    fn test_ring_byte_budget_release_saturates_and_never_wraps() {
        let b = RingByteBudget::new(1_000);
        assert!(b.try_reserve(100));
        // Over-release: a bookkeeping bug, deliberately made harmless. Wrapping
        // to usize::MAX here would refuse every subsequent frame forever —
        // turning a counting error into a total feed outage.
        b.release(10_000);
        assert_eq!(b.resident(), 0, "release must saturate at zero, not wrap");
        assert!(
            b.try_reserve(1_000),
            "the budget must still admit frames after an over-release"
        );
    }

    #[test]
    fn test_wal_ring_sink_returns_the_reservation_when_the_count_bound_refuses() {
        // The leak this pins: the byte reserve is taken BEFORE `try_send`, so a
        // frame refused by the COUNT bound has a reservation nothing downstream
        // will ever release. Without the release on that path the budget
        // ratchets down on every count-full frame until it refuses everything —
        // a slow strangulation that would present as the feed dying for no
        // visible reason, long after the burst that caused it.
        let dir = std::env::temp_dir().join(format!(
            "tv-ring-budget-leak-{}-{}",
            std::process::id(),
            next_frame_seq()
        ));
        let spill = std::sync::Arc::new(
            WsFrameSpill::new(&dir).expect("WAL must open under a fresh temp dir"),
        );
        // Capacity 1: the second frame is refused by COUNT, not by bytes.
        let (tx, _rx) = tokio::sync::mpsc::channel::<CapturedFrame>(1);
        let budget = std::sync::Arc::new(RingByteBudget::new(1_000_000));
        let sink = WalRingSink::new(
            std::sync::Arc::clone(&spill),
            tx,
            std::sync::Arc::clone(&budget),
            WsType::LiveFeed,
            DhanEndpointType::MainFeed,
            0,
        );

        assert_eq!(
            sink.accept(Bytes::from_static(b"0123456789")),
            FrameSinkOutcome::Captured
        );
        assert_eq!(budget.resident(), 10);

        for _ in 0..50 {
            assert_eq!(
                sink.accept(Bytes::from_static(b"0123456789")),
                FrameSinkOutcome::RingFull
            );
        }
        assert_eq!(
            budget.resident(),
            10,
            "only the ONE frame actually sitting in the ring may hold a reservation; \
             50 count-refused frames must each have given theirs back"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_wal_ring_sink_refuses_on_the_byte_bound_before_the_count_bound() {
        // The whole point of the second bound: a ring with plenty of SLOTS free
        // still refuses when those slots would hold too many BYTES.
        let dir = std::env::temp_dir().join(format!(
            "tv-ring-budget-bytes-{}-{}",
            std::process::id(),
            next_frame_seq()
        ));
        let spill = std::sync::Arc::new(
            WsFrameSpill::new(&dir).expect("WAL must open under a fresh temp dir"),
        );
        // 64 slots, but only 25 bytes of budget: the count bound cannot bind.
        let (tx, mut rx) = tokio::sync::mpsc::channel::<CapturedFrame>(64);
        let budget = std::sync::Arc::new(RingByteBudget::new(25));
        let sink = WalRingSink::new(
            std::sync::Arc::clone(&spill),
            tx,
            std::sync::Arc::clone(&budget),
            WsType::LiveFeed,
            DhanEndpointType::MainFeed,
            0,
        );

        for _ in 0..2 {
            assert_eq!(
                sink.accept(Bytes::from_static(b"0123456789")),
                FrameSinkOutcome::Captured
            );
        }
        assert_eq!(
            sink.accept(Bytes::from_static(b"0123456789")),
            FrameSinkOutcome::RingFull,
            "20 of 25 bytes are resident and 62 slots are free — the BYTE bound must \
             refuse the third frame, which is the bound the count alone never gave"
        );

        // Draining releases, and the sink accepts again — proving this is
        // backpressure, not a latch.
        let taken = rx.try_recv().expect("a captured frame must be published");
        budget.release(taken.bytes.len());
        assert_eq!(
            sink.accept(Bytes::from_static(b"0123456789")),
            FrameSinkOutcome::Captured,
            "releasing on drain must re-open the budget"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    // -- pool supervisor ----------------------------------------------------

    #[test]
    fn test_pool_supervisor_admits_exactly_the_authorized_sixteen() {
        let now = t0();
        let mut pool = PoolSupervisor::new();
        assert!(pool.is_empty());
        let mut admitted = 0usize;
        for endpoint in DhanEndpointType::ALL {
            for _ in 0..endpoint.max_connections() {
                assert!(pool.admit(endpoint, now).is_ok());
                admitted += 1;
            }
            // The sixth of any five-cap type is refused.
            if endpoint.max_connections() == 5 {
                assert!(pool.admit(endpoint, now).is_err());
            }
        }
        assert_eq!(admitted, 16);
        assert_eq!(pool.len(), 16);
        assert_eq!(pool.total_open(), 16);
        assert_eq!(pool.open_count(DhanEndpointType::MainFeed), 5);
        assert_eq!(pool.open_count(DhanEndpointType::OrderUpdate), 1);
    }

    #[test]
    fn test_pool_supervisor_refuses_the_sixth_main_feed_before_dialing() {
        // Fail-closed: fifteen connections and a loud refusal beats sixteen
        // and a silently murdered pool member.
        let now = t0();
        let mut pool = PoolSupervisor::new();
        for _ in 0..5 {
            assert!(pool.admit(DhanEndpointType::MainFeed, now).is_ok());
        }
        let refusal = pool
            .admit(DhanEndpointType::MainFeed, now)
            .expect_err("the sixth main-feed socket must be refused locally");
        assert_eq!(refusal.endpoint(), DhanEndpointType::MainFeed);
        assert_eq!(pool.len(), 5, "a refusal must register nothing");
    }

    #[test]
    fn test_pool_supervisor_poll_all_returns_only_the_expired_connections() {
        let now = t0();
        let mut pool = PoolSupervisor::new();
        for _ in 0..3 {
            let _ = pool.admit(DhanEndpointType::MainFeed, now);
        }
        // Drive two of them live; leave the third idle (never dialed).
        for gi in [0u8, 1] {
            if let Some(c) = pool.connection_mut(gi) {
                let _ = c.on_event(ConnEvent::BeginDial, now);
                let _ = c.on_event(ConnEvent::DialSucceeded, now);
                let _ = c.on_event(ConnEvent::SubscribeAcked, now);
            }
        }
        assert!(pool.poll_all(now).is_empty(), "nothing is idle yet");

        let later = now + Duration::from_secs(60);
        let due = pool.poll_all(later);
        assert_eq!(due.len(), 2, "only the two live connections time out");
        for (s, a) in &due {
            assert_eq!(s.endpoint, DhanEndpointType::MainFeed);
            assert!(matches!(a, SupervisorAction::SleepThenDial { .. }));
        }
        assert_eq!(pool.connections().len(), 3);
    }

    #[test]
    fn test_pool_supervisor_retire_frees_the_budget_slot() {
        let now = t0();
        let mut pool = PoolSupervisor::default();
        let mut slots = Vec::new();
        for _ in 0..5 {
            if let Ok(s) = pool.admit(DhanEndpointType::MainFeed, now) {
                slots.push(s);
            }
        }
        assert!(pool.admit(DhanEndpointType::MainFeed, now).is_err());
        if let Some(&s) = slots.first() {
            pool.retire(s);
        }
        assert_eq!(pool.len(), 4);
        assert!(
            pool.admit(DhanEndpointType::MainFeed, now).is_ok(),
            "retiring a parked connection must return its slot to the budget"
        );
    }

    /// Retiring the same slot twice must NOT hand back two budget entries.
    ///
    /// `retire` used to call `budget.release` unconditionally, even when
    /// `retain` removed nothing. `PoolBudget::release` saturates at zero, so it
    /// cannot underflow and report the problem — it simply undercounts, and the
    /// pool then admits a connection PAST the 5-per-endpoint cap. That cap is
    /// the entire basis of the 16-connection lock.
    ///
    /// It was unreachable only because `retire` has no production caller. A
    /// dormant function that breaks the system's core invariant when called is
    /// worse than one that does nothing: it makes wiring it later a trap.
    #[test]
    fn retiring_the_same_slot_twice_does_not_over_admit_past_the_cap() {
        let now = t0();
        let mut pool = PoolSupervisor::new();
        let mut slots = Vec::new();
        for _ in 0..5 {
            if let Ok(s) = pool.admit(DhanEndpointType::MainFeed, now) {
                slots.push(s);
            }
        }
        assert_eq!(slots.len(), 5, "the cap is 5 main-feed connections");
        assert!(pool.admit(DhanEndpointType::MainFeed, now).is_err());

        let first = *slots.first().expect("five slots were admitted");
        pool.retire(first);
        // The SAME slot again — already gone.
        pool.retire(first);
        assert_eq!(pool.len(), 4, "the second retire removes nothing");

        assert!(
            pool.admit(DhanEndpointType::MainFeed, now).is_ok(),
            "the one genuinely retired slot comes back"
        );
        assert!(
            pool.admit(DhanEndpointType::MainFeed, now).is_err(),
            "and ONLY one — a second admit here means the double-retire handed \
             back a slot that was never occupied, and the 16-connection lock is \
             arithmetic, not a hope"
        );
    }

    /// The same guarantee for a slot the pool never admitted at all.
    #[test]
    fn retiring_an_unknown_slot_does_not_move_the_budget() {
        let now = t0();
        let mut pool = PoolSupervisor::new();
        for _ in 0..5 {
            let _ = pool.admit(DhanEndpointType::MainFeed, now);
        }
        assert!(pool.admit(DhanEndpointType::MainFeed, now).is_err());

        // A slot with a global index the pool has never issued.
        let bogus = ConnectionSlot {
            endpoint: DhanEndpointType::MainFeed,
            global_index: u8::MAX,
            pool_index: u8::MAX,
        };
        pool.retire(bogus);
        assert_eq!(pool.len(), 5, "nothing was removed");
        assert!(
            pool.admit(DhanEndpointType::MainFeed, now).is_err(),
            "and nothing was released — an unknown slot must never create capacity"
        );
    }
    #[test]
    fn test_pool_supervisor_connection_mut_addresses_by_global_index() {
        let now = t0();
        let mut pool = PoolSupervisor::new();
        let _ = pool.admit(DhanEndpointType::MainFeed, now);
        let _ = pool.admit(DhanEndpointType::Depth200, now);
        let depth_gi = DhanEndpointType::Depth200.jitter_base();
        assert_eq!(
            pool.connection_mut(depth_gi).map(|c| c.slot().endpoint),
            Some(DhanEndpointType::Depth200)
        );
        assert!(pool.connection_mut(200).is_none());
    }

    // -- the async shell ----------------------------------------------------

    #[derive(Default)]
    struct FakeState {
        connect_results: VecDeque<bool>,
        subscribe_results: VecDeque<bool>,
        recv_events: VecDeque<SocketEvent>,
        connects: usize,
        subscribes: usize,
        closes: usize,
        unsubscribe_results: VecDeque<bool>,
        unsubscribes: usize,
        /// Every wire call in order, so a test can assert that the
        /// unsubscribe went out BEFORE the subscribe. On a depth-200 socket
        /// the reverse order asks for two instruments and earns a Fatal 804,
        /// so the ORDER is the property under test, not the counts.
        wire_calls: Vec<&'static str>,
        pings: usize,
        /// How long each `send_subscribe` takes on the wire.
        ///
        /// Exists so a test can reach the top-up's wire budget. Under
        /// `start_paused = true` tokio auto-advances, so a "slow" socket costs
        /// no real wall time -- which is the only reason a 40-message top-up
        /// against a 900 ms socket is testable at all.
        subscribe_delay: Option<Duration>,
    }

    struct FakeSocket {
        state: std::sync::Arc<Mutex<FakeState>>,
    }

    impl DhanFeedSocket for FakeSocket {
        fn connect(
            &mut self,
        ) -> impl std::future::Future<Output = Result<(), SocketFailure>> + Send {
            let state = std::sync::Arc::clone(&self.state);
            async move {
                let ok = match state.lock() {
                    Ok(mut s) => {
                        s.connects += 1;
                        s.connect_results.pop_front().unwrap_or(true)
                    }
                    Err(_) => true,
                };
                if ok { Ok(()) } else { Err(SocketFailure) }
            }
        }

        fn send_unsubscribe(
            &mut self,
            _batch: &[SubscribeInstrument],
        ) -> impl std::future::Future<Output = Result<(), SocketFailure>> + Send {
            let state = std::sync::Arc::clone(&self.state);
            async move {
                let ok = match state.lock() {
                    Ok(mut s) => {
                        s.unsubscribes += 1;
                        s.wire_calls.push("unsubscribe");
                        s.unsubscribe_results.pop_front().unwrap_or(true)
                    }
                    Err(_) => true,
                };
                if ok { Ok(()) } else { Err(SocketFailure) }
            }
        }

        fn send_ping(
            &mut self,
        ) -> impl std::future::Future<Output = Result<(), SocketFailure>> + Send {
            let state = std::sync::Arc::clone(&self.state);
            async move {
                if let Ok(mut s) = state.lock() {
                    s.pings += 1;
                }
                Ok(())
            }
        }

        fn send_subscribe(
            &mut self,
            _batch: &[SubscribeInstrument],
        ) -> impl std::future::Future<Output = Result<(), SocketFailure>> + Send {
            let state = std::sync::Arc::clone(&self.state);
            async move {
                let (ok, delay) = match state.lock() {
                    Ok(mut s) => {
                        s.subscribes += 1;
                        s.wire_calls.push("subscribe");
                        (
                            s.subscribe_results.pop_front().unwrap_or(true),
                            s.subscribe_delay,
                        )
                    }
                    Err(_) => (true, None),
                };
                if let Some(d) = delay {
                    tokio::time::sleep(d).await;
                }
                if ok { Ok(()) } else { Err(SocketFailure) }
            }
        }

        fn recv(&mut self) -> impl std::future::Future<Output = SocketEvent> + Send {
            let state = std::sync::Arc::clone(&self.state);
            async move {
                match state.lock() {
                    Ok(mut s) => s.recv_events.pop_front().unwrap_or(
                        // Terminator: a fatal code parks the loop, so an
                        // exhausted script ends the test instead of hanging.
                        SocketEvent::Closed {
                            code: Some(DisconnectCode::AuthenticationFailed),
                        },
                    ),
                    Err(_) => SocketEvent::Closed {
                        code: Some(DisconnectCode::AuthenticationFailed),
                    },
                }
            }
        }

        fn close(&mut self) -> impl std::future::Future<Output = ()> + Send {
            let state = std::sync::Arc::clone(&self.state);
            async move {
                if let Ok(mut s) = state.lock() {
                    s.closes += 1;
                }
            }
        }
    }

    fn fake(state: &std::sync::Arc<Mutex<FakeState>>) -> FakeSocket {
        FakeSocket {
            state: std::sync::Arc::clone(state),
        }
    }
    // -- subscribe dispatch pacing (WS-GAP-02) ------------------------------

    /// The defect: batches went out back to back with no spacing, and a
    /// throttled subscribe is INVISIBLE — no error, no ack, no sequence
    /// number, just an instrument that never ticks.
    ///
    /// Pacing is between messages, so `n` batches cost `n - 1` intervals.
    #[tokio::test(start_paused = true)]
    async fn subscribe_batches_are_paced_by_the_named_interval() {
        let st = std::sync::Arc::new(Mutex::new(FakeState::default()));
        let mut sock = fake(&st);
        // 250 main-feed instruments = 3 messages at the documented 100/message.
        let guard = SubscribeGuard::try_new(DhanEndpointType::MainFeed, (0..250).map(si).collect())
            .unwrap();
        assert_eq!(guard.batch_count(), 3, "documented 100 instruments/message");

        let out = dispatch_subscribe(&mut sock, &guard, DhanEndpointType::MainFeed, 0).await;

        assert_eq!(out.batches_sent, 3);
        assert_eq!(out.instruments_sent, 250);
        assert!(!out.stopped_early());
        assert_eq!(out.instruments_undispatched(), 0);
        // The bite: before pacing this was ~0. Two gaps for three messages.
        assert_eq!(
            out.elapsed,
            SUBSCRIBE_BATCH_INTERVAL * 2,
            "three batches must be spaced by exactly two intervals"
        );
        assert_eq!(st.lock().unwrap().subscribes, 3);
    }

    /// A one-batch set is not paced at all: there is nothing before the first
    /// message to be polite to, and the deadline is waiting on it.
    #[tokio::test(start_paused = true)]
    async fn a_single_batch_dispatch_pays_no_pacing_cost() {
        let st = std::sync::Arc::new(Mutex::new(FakeState::default()));
        let mut sock = fake(&st);
        let guard =
            SubscribeGuard::try_new(DhanEndpointType::MainFeed, (0..40).map(si).collect()).unwrap();

        let out = dispatch_subscribe(&mut sock, &guard, DhanEndpointType::MainFeed, 0).await;

        assert_eq!(out.batches_sent, 1);
        assert_eq!(out.elapsed, Duration::ZERO);
    }

    /// An empty set dispatches zero of zero and is a legal no-op, NOT a
    /// failure — otherwise the order-update socket, which carries no
    /// instruments, would park itself on every connect.
    #[tokio::test(start_paused = true)]
    async fn an_empty_set_is_a_no_op_not_an_early_stop() {
        let st = std::sync::Arc::new(Mutex::new(FakeState::default()));
        let mut sock = fake(&st);
        let guard = SubscribeGuard::try_new(DhanEndpointType::MainFeed, Vec::new()).unwrap();

        let out = dispatch_subscribe(&mut sock, &guard, DhanEndpointType::MainFeed, 0).await;

        assert!(!out.stopped_early());
        assert_eq!(out.batches_total, 0);
        assert_eq!(st.lock().unwrap().subscribes, 0);
    }

    /// The silent half of the defect: a send that fails part-way leaves a tail
    /// that was never written. Before this, the loop returned a bare `bool`
    /// and the size of that tail was unrecorded — indistinguishable from
    /// instruments that were subscribed and merely quiet.
    #[tokio::test(start_paused = true)]
    async fn a_mid_loop_send_failure_counts_the_undispatched_tail() {
        let st = std::sync::Arc::new(Mutex::new(FakeState {
            // batch 1 ok, batch 2 fails; batch 3 must never be attempted.
            subscribe_results: VecDeque::from(vec![true, false]),
            ..FakeState::default()
        }));
        let mut sock = fake(&st);
        let guard = SubscribeGuard::try_new(DhanEndpointType::MainFeed, (0..250).map(si).collect())
            .unwrap();

        let out = dispatch_subscribe(&mut sock, &guard, DhanEndpointType::MainFeed, 0).await;

        assert!(out.stopped_early(), "a short write must be reported");
        assert_eq!(out.batches_sent, 1);
        assert_eq!(out.batches_total, 3);
        assert_eq!(out.instruments_sent, 100);
        assert_eq!(
            out.instruments_undispatched(),
            150,
            "150 instruments were named on the set and never written to the socket"
        );
        assert_eq!(
            st.lock().unwrap().subscribes,
            2,
            "the dispatch must STOP at the failure, not press on into a broken socket"
        );
    }

    /// A failed dispatch must not leave the guard claiming a subscription the
    /// wire never carried — the next connect replays the WHOLE set.
    #[tokio::test(start_paused = true)]
    async fn an_early_stop_leaves_the_guard_unconfirmed_for_a_whole_replay() {
        let st = std::sync::Arc::new(Mutex::new(FakeState {
            subscribe_results: VecDeque::from(vec![false]),
            ..FakeState::default()
        }));
        let mut sock = fake(&st);
        let mut guard =
            SubscribeGuard::try_new(DhanEndpointType::MainFeed, (0..250).map(si).collect())
                .unwrap();

        let out = dispatch_subscribe(&mut sock, &guard, DhanEndpointType::MainFeed, 0).await;
        // Mirrors the caller: `mark_confirmed` is reached only on a full send.
        if !out.stopped_early() {
            guard.mark_confirmed();
        }

        assert!(guard.needs_resubscribe());
        assert_eq!(guard.generation(), 0);
        assert_eq!(
            guard.batch_count(),
            3,
            "the retained set is replayed whole, never resumed from an offset"
        );
    }

    /// The deadline this pacing must not threaten, proven arithmetically
    /// against the named constants rather than by wall clock.
    #[test]
    fn subscribe_dispatch_fits_the_preopen_budget() {
        // Pin the mirrored deadline so a drift from the app crate's own
        // `PREOPEN_READY_DEADLINE_IST_SECS` fails HERE.
        assert_eq!(PREOPEN_READY_DEADLINE_IST_SECS, 9 * 3_600 + 12 * 60);
        assert_eq!(PREOPEN_ATTACH_WINDOW_OPEN_IST_SECS, 9 * 3_600);
        let budget_ms =
            u128::from(PREOPEN_READY_DEADLINE_IST_SECS - PREOPEN_ATTACH_WINDOW_OPEN_IST_SECS)
                * 1_000;
        assert_eq!(budget_ms, 720_000, "09:00 -> 09:12 IST");

        // Worst case per endpoint: a FULL pool, every connection at its
        // documented instrument cap, batched at its documented per-message cap.
        let mut worst_case_messages = 0_u128;
        for endpoint in [
            DhanEndpointType::MainFeed,
            DhanEndpointType::Depth20,
            DhanEndpointType::Depth200,
        ] {
            let per_conn = u128::from(endpoint.max_instruments_per_connection());
            let per_msg = u128::from(endpoint.max_instruments_per_subscribe_message()).max(1);
            let msgs_per_conn = per_conn.div_ceil(per_msg);
            worst_case_messages += msgs_per_conn * u128::from(endpoint.max_connections());
        }
        assert_eq!(
            worst_case_messages, 260,
            "5x50 main-feed + 5x1 depth-20 + 5x1 depth-200 messages"
        );

        // Pacing is BETWEEN messages, so n messages cost n-1 intervals. Costed
        // as if every connection dispatched serially — they do not, each runs
        // in its own task, so this is a ceiling on the real cost.
        let interval_ms = SUBSCRIBE_BATCH_INTERVAL.as_millis();
        let worst_case_ms = (worst_case_messages - 1) * interval_ms;
        assert_eq!(worst_case_ms, 6_475, "260 messages, 25 ms apart");

        // The pacing may not consume more than 5% of the pre-open budget: it
        // shares that window with dialing, the pricing quorum and contract
        // selection, so "merely under the deadline" is not the bar.
        assert!(
            worst_case_ms * 20 < budget_ms,
            "paced dispatch {worst_case_ms} ms must stay under 5% of the {budget_ms} ms \
             pre-open budget"
        );
    }

    /// One connection's worst case — the number that actually matters, since
    /// connections dispatch concurrently.
    #[test]
    fn one_full_main_feed_connection_dispatches_in_about_a_second() {
        let e = DhanEndpointType::MainFeed;
        let msgs = u128::from(e.max_instruments_per_connection())
            .div_ceil(u128::from(e.max_instruments_per_subscribe_message()));
        assert_eq!(msgs, 50, "5,000 instruments at 100 per message");
        let ms = (msgs - 1) * SUBSCRIBE_BATCH_INTERVAL.as_millis();
        assert_eq!(ms, 1_225);
        assert!(ms < 2_000);
    }

    #[tokio::test(start_paused = true)]
    async fn test_run_connection_dials_subscribes_and_drains_every_frame() {
        let st = std::sync::Arc::new(Mutex::new(FakeState {
            recv_events: VecDeque::from(vec![
                SocketEvent::Frame(Bytes::from_static(b"aaaaaaaa")),
                SocketEvent::Frame(Bytes::from_static(b"bbbbbbbb")),
                SocketEvent::Frame(Bytes::from_static(b"cccccccc")),
            ]),
            ..FakeState::default()
        }));
        let sink = std::sync::Arc::new(RecordingSink::default());
        let guard = SubscribeGuard::try_new(DhanEndpointType::MainFeed, instruments(250))
            .expect("inside cap");

        let exit = run_connection(
            fake(&st),
            sup(DhanEndpointType::MainFeed, 0, t0()),
            guard,
            std::sync::Arc::clone(&sink),
            || async {},
        )
        .await;

        assert_eq!(exit, ConnectionExit::Parked(ParkReason::FatalDisconnect));
        let seen = sink.accepted.lock().map(|g| g.len()).unwrap_or(0);
        assert_eq!(seen, 3, "every frame must reach the sink exactly once");
        let s = st.lock().expect("fake state");
        assert_eq!(s.connects, 1);
        assert_eq!(s.subscribes, 3, "250 instruments = three 100-cap messages");
    }

    /// The whole ATM +/- 25 mechanism, end to end through the real drain loop.
    ///
    /// A live socket carrying 250 instruments is topped up with 150 more, and
    /// the assertion that matters is the SUBSCRIBE COUNT: 3 messages for the
    /// initial 250, then exactly 2 for the 150 added — never 5, which is what
    /// re-sending the whole set would produce and what Dhan answers with 804.
    // ------------------------------------------------------------------
    // Swap over the live command channel (2026-08-26). The pure guard logic
    // is tested above; these pin the WIRE behaviour, which is where the
    // 804-shaped mistakes live.
    // ------------------------------------------------------------------

    fn one_scripted_frame() -> VecDeque<SocketEvent> {
        VecDeque::from(vec![
            SocketEvent::Frame(Bytes::from_static(b"aaaaaaaa")),
            SocketEvent::Frame(Bytes::from_static(b"bbbbbbbb")),
        ])
    }

    /// THE safety property. A depth-200 connection holds exactly one
    /// instrument; subscribing before unsubscribing asks for two, and Dhan
    /// answers an over-limit subscribe with 804 — Fatal, and retrying
    /// re-sends the identical over-limit set forever.
    #[tokio::test(start_paused = true)]
    async fn a_swap_unsubscribes_before_it_subscribes() {
        let st = std::sync::Arc::new(Mutex::new(FakeState {
            recv_events: one_scripted_frame(),
            ..FakeState::default()
        }));
        let sink = std::sync::Arc::new(RecordingSink::default());
        let guard = SubscribeGuard::try_new(DhanEndpointType::Depth200, vec![si(1)])
            .expect("one instrument");
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        tx.send(LiveSubscriptionCommand::Swap {
            old: si(1),
            new: si(2),
        })
        .await
        .expect("channel open");
        drop(tx);

        let _ = run_connection_with_commands(
            fake(&st),
            sup(DhanEndpointType::Depth200, 0, t0()),
            guard,
            std::sync::Arc::clone(&sink),
            || async {},
            Some(rx),
        )
        .await;

        let s = st.lock().expect("fake state");
        // The initial subscribe, then the swap's unsubscribe, then its
        // subscribe. The ORDER of the last two is the assertion.
        assert_eq!(
            s.wire_calls,
            vec!["subscribe", "unsubscribe", "subscribe"],
            "a swap that subscribes before unsubscribing puts a depth-200 \
             connection over its one-instrument limit"
        );
        assert_eq!(s.connects, 1, "a swap must never re-dial");
    }

    /// The ordinary minute. An edge-triggered tracker upstream means this
    /// rarely reaches the socket at all, but when it does it must cost
    /// nothing — a re-subscribe per socket per minute is ~1,500 needless wire
    /// messages a session on four sockets.
    #[tokio::test(start_paused = true)]
    async fn a_swap_to_the_same_instrument_touches_the_wire_not_at_all() {
        let st = std::sync::Arc::new(Mutex::new(FakeState {
            recv_events: one_scripted_frame(),
            ..FakeState::default()
        }));
        let sink = std::sync::Arc::new(RecordingSink::default());
        let guard = SubscribeGuard::try_new(DhanEndpointType::Depth200, vec![si(1)])
            .expect("one instrument");
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        tx.send(LiveSubscriptionCommand::Swap {
            old: si(1),
            new: si(1),
        })
        .await
        .expect("channel open");
        drop(tx);

        let _ = run_connection_with_commands(
            fake(&st),
            sup(DhanEndpointType::Depth200, 0, t0()),
            guard,
            std::sync::Arc::clone(&sink),
            || async {},
            Some(rx),
        )
        .await;

        let s = st.lock().expect("fake state");
        assert_eq!(s.unsubscribes, 0, "an unchanged strike sent an unsubscribe");
        assert_eq!(
            s.wire_calls,
            vec!["subscribe"],
            "only the INITIAL subscribe should be on the wire"
        );
    }

    /// A refused swap must leave the socket alone. Sending an unsubscribe for
    /// an instrument this connection never held would drop nothing and then a
    /// subscribe would ADD one — turning a refused swap into a silent
    /// `try_extend`, on a socket whose cap is one.
    #[tokio::test(start_paused = true)]
    async fn a_refused_swap_sends_nothing_at_all() {
        let st = std::sync::Arc::new(Mutex::new(FakeState {
            recv_events: one_scripted_frame(),
            ..FakeState::default()
        }));
        let sink = std::sync::Arc::new(RecordingSink::default());
        let guard = SubscribeGuard::try_new(DhanEndpointType::Depth200, vec![si(1)])
            .expect("one instrument");
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        tx.send(LiveSubscriptionCommand::Swap {
            old: si(99),
            new: si(2),
        })
        .await
        .expect("channel open");
        drop(tx);

        let _ = run_connection_with_commands(
            fake(&st),
            sup(DhanEndpointType::Depth200, 0, t0()),
            guard,
            std::sync::Arc::clone(&sink),
            || async {},
            Some(rx),
        )
        .await;

        let s = st.lock().expect("fake state");
        assert_eq!(
            s.wire_calls,
            vec!["subscribe"],
            "a refused swap put traffic on the wire"
        );
    }

    /// A failed unsubscribe must NOT be followed by the subscribe. Sending it
    /// anyway is the exact shape that takes a depth-200 connection to two
    /// instruments and earns the Fatal 804.
    #[tokio::test(start_paused = true)]
    async fn a_failed_unsubscribe_stops_the_swap_rather_than_subscribing_anyway() {
        let st = std::sync::Arc::new(Mutex::new(FakeState {
            recv_events: one_scripted_frame(),
            unsubscribe_results: VecDeque::from(vec![false]),
            ..FakeState::default()
        }));
        let sink = std::sync::Arc::new(RecordingSink::default());
        let guard = SubscribeGuard::try_new(DhanEndpointType::Depth200, vec![si(1)])
            .expect("one instrument");
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        tx.send(LiveSubscriptionCommand::Swap {
            old: si(1),
            new: si(2),
        })
        .await
        .expect("channel open");
        drop(tx);

        let _ = run_connection_with_commands(
            fake(&st),
            sup(DhanEndpointType::Depth200, 0, t0()),
            guard,
            std::sync::Arc::clone(&sink),
            || async {},
            Some(rx),
        )
        .await;

        let s = st.lock().expect("fake state");
        assert_eq!(
            s.wire_calls,
            vec!["subscribe", "unsubscribe"],
            "the subscribe went out after the unsubscribe FAILED — that is the \
             over-limit shape Dhan answers with a Fatal 804"
        );
    }

    /// A swap whose unsubscribe LANDS and whose subscribe FAILS leaves the
    /// socket carrying nothing — and must force a redial.
    ///
    /// The order of the two wire calls is a safety property (subscribing first
    /// on a one-instrument depth socket asks for two and earns a Fatal 804), and
    /// this is its consequence: when the first call succeeds and the second does
    /// not, a depth-200 connection holds ZERO instruments.
    ///
    /// Until 2026-08-28 that arm only logged, under a line claiming "the
    /// reconnect replay lands it. One stale minute, not a lost strike." The
    /// first half was true — the guard already names the new instrument, so any
    /// reconnect restores the right strike — and the second half was an
    /// assumption. Nothing scheduled a reconnect. Only a `Disconnected` event
    /// and the idle watchdog do, and neither fires here: the socket is
    /// transport-healthy, and depth-200 is client-pinged with Dhan ponging back,
    /// so the watchdog keeps being reset on a socket carrying no data at all.
    ///
    /// It was not permanent — the next ATM move issues a fresh swap — but on a
    /// flat afternoon the strike does not move, and the socket stays empty for
    /// the rest of the session.
    #[tokio::test]
    async fn a_swap_that_empties_the_socket_forces_a_redial() {
        let st = std::sync::Arc::new(Mutex::new(FakeState {
            recv_events: one_scripted_frame(),
            // The initial subscribe succeeds; the swap's subscribe fails.
            subscribe_results: VecDeque::from(vec![true, false]),
            // The swap's unsubscribe LANDS — this is what empties the socket.
            unsubscribe_results: VecDeque::from(vec![true]),
            ..FakeState::default()
        }));
        let sink = std::sync::Arc::new(RecordingSink::default());
        let guard = SubscribeGuard::try_new(DhanEndpointType::Depth200, vec![si(1)])
            .expect("one instrument");
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        tx.send(LiveSubscriptionCommand::Swap {
            old: si(1),
            new: si(2),
        })
        .await
        .expect("channel open");
        drop(tx);

        let _ = run_connection_with_commands(
            fake(&st),
            sup(DhanEndpointType::Depth200, 0, t0()),
            guard,
            std::sync::Arc::clone(&sink),
            || async {},
            Some(rx),
        )
        .await;

        let s = st.lock().expect("fake state");
        assert_eq!(
            &s.wire_calls[..3],
            &["subscribe", "unsubscribe", "subscribe"],
            "the swap must unsubscribe before it subscribes — that ordering is \
             what makes the empty-socket case possible in the first place"
        );
        // The FOURTH call is the point: it is the redial re-subscribing the
        // guard's retained set, which already names the NEW instrument. Before
        // this fix the sequence stopped at three and the socket kept draining
        // while holding nothing.
        assert_eq!(
            s.wire_calls.len(),
            4,
            "the redial must replay the retained set; got {:?}",
            s.wire_calls
        );
        // The redial is the point. Without it the drain keeps running on a
        // socket that holds nothing, answers pings, and delivers no data.
        assert!(
            s.connects > 1,
            "a swap that emptied the socket must force a redial so the guard's \
             retained set is replayed; got {} dial(s), meaning the drain stayed \
             on a connection carrying zero instruments",
            s.connects
        );
    }

    /// A socket that never answers must not hold the drain. Unbounded, the
    /// transport's own 10-second timeout would make one swap a TWENTY-second
    /// stall on the socket the swap exists to keep useful — and during a
    /// stall Dhan skips a slow consumer forward, dropping the intervening
    /// ticks at their side where no counter of ours can see them.
    #[tokio::test(start_paused = true)]
    async fn a_socket_that_never_answers_costs_the_drain_two_seconds_not_twenty() {
        struct HangingSocket {
            state: std::sync::Arc<Mutex<FakeState>>,
        }
        impl DhanFeedSocket for HangingSocket {
            fn connect(
                &mut self,
            ) -> impl std::future::Future<Output = Result<(), SocketFailure>> + Send {
                async { Ok(()) }
            }
            fn send_subscribe(
                &mut self,
                _batch: &[SubscribeInstrument],
            ) -> impl std::future::Future<Output = Result<(), SocketFailure>> + Send {
                let state = std::sync::Arc::clone(&self.state);
                async move {
                    let first = match state.lock() {
                        Ok(mut s) => {
                            s.subscribes += 1;
                            s.wire_calls.push("subscribe");
                            s.subscribes == 1
                        }
                        Err(_) => true,
                    };
                    // The INITIAL subscribe answers; the swap's does not.
                    if first {
                        Ok(())
                    } else {
                        std::future::pending::<()>().await;
                        unreachable!()
                    }
                }
            }
            fn send_unsubscribe(
                &mut self,
                _batch: &[SubscribeInstrument],
            ) -> impl std::future::Future<Output = Result<(), SocketFailure>> + Send {
                let state = std::sync::Arc::clone(&self.state);
                async move {
                    if let Ok(mut s) = state.lock() {
                        s.unsubscribes += 1;
                        s.wire_calls.push("unsubscribe");
                    }
                    std::future::pending::<()>().await;
                    unreachable!()
                }
            }
            /// This fake exists to HANG a swap, not to exercise keepalive.
            /// Answering immediately keeps it that way: a pending ping here
            /// would stall the supervisor for a reason the test is not about,
            /// and the resulting failure would point at the wrong thing.
            fn send_ping(
                &mut self,
            ) -> impl std::future::Future<Output = Result<(), SocketFailure>> + Send {
                async { Ok(()) }
            }
            fn recv(&mut self) -> impl std::future::Future<Output = SocketEvent> + Send {
                let state = std::sync::Arc::clone(&self.state);
                async move {
                    let next = match state.lock() {
                        Ok(mut s) => s.recv_events.pop_front(),
                        Err(_) => None,
                    };
                    next.unwrap_or(SocketEvent::Closed {
                        code: Some(DisconnectCode::AuthenticationFailed),
                    })
                }
            }
            fn close(&mut self) -> impl std::future::Future<Output = ()> + Send {
                async {}
            }
        }

        let st = std::sync::Arc::new(Mutex::new(FakeState {
            recv_events: one_scripted_frame(),
            ..FakeState::default()
        }));
        let sink = std::sync::Arc::new(RecordingSink::default());
        let guard = SubscribeGuard::try_new(DhanEndpointType::Depth200, vec![si(1)])
            .expect("one instrument");
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        tx.send(LiveSubscriptionCommand::Swap {
            old: si(1),
            new: si(2),
        })
        .await
        .expect("channel open");
        drop(tx);

        let started = tokio::time::Instant::now();
        let _ = run_connection_with_commands(
            HangingSocket {
                state: std::sync::Arc::clone(&st),
            },
            sup(DhanEndpointType::Depth200, 0, t0()),
            guard,
            std::sync::Arc::clone(&sink),
            || async {},
            Some(rx),
        )
        .await;
        let spent = started.elapsed();

        // Paused clock, so this is the virtual time the swap actually
        // occupied — not wall-clock jitter.
        assert!(
            spent < Duration::from_secs(3),
            "a hanging socket held the drain for {spent:?} — the swap budget \
             is not bounding it"
        );
        let s = st.lock().expect("fake state");
        assert_eq!(
            s.wire_calls,
            vec!["subscribe", "unsubscribe"],
            "the subscribe went out after the unsubscribe TIMED OUT — that is \
             the over-limit shape Dhan answers with a Fatal 804"
        );
    }

    /// Frames must keep flowing while a swap is processed. The command block
    /// runs before the select rather than as an arm of it precisely so it
    /// cannot be starved — but it must not starve the socket either.
    #[tokio::test(start_paused = true)]
    async fn a_swap_does_not_eat_the_frames_arriving_around_it() {
        let st = std::sync::Arc::new(Mutex::new(FakeState {
            recv_events: one_scripted_frame(),
            ..FakeState::default()
        }));
        let sink = std::sync::Arc::new(RecordingSink::default());
        let guard = SubscribeGuard::try_new(DhanEndpointType::Depth200, vec![si(1)])
            .expect("one instrument");
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        tx.send(LiveSubscriptionCommand::Swap {
            old: si(1),
            new: si(2),
        })
        .await
        .expect("channel open");
        drop(tx);

        let _ = run_connection_with_commands(
            fake(&st),
            sup(DhanEndpointType::Depth200, 0, t0()),
            guard,
            std::sync::Arc::clone(&sink),
            || async {},
            Some(rx),
        )
        .await;

        assert_eq!(
            sink.accepted.lock().map(|g| g.len()).unwrap_or(0),
            2,
            "the swap arm ate frames that should have reached the sink"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_live_extend_command_grows_the_subscription_without_a_redial() {
        let st = std::sync::Arc::new(Mutex::new(FakeState {
            recv_events: VecDeque::from(vec![
                SocketEvent::Frame(Bytes::from_static(b"aaaaaaaa")),
                SocketEvent::Frame(Bytes::from_static(b"bbbbbbbb")),
            ]),
            ..FakeState::default()
        }));
        let sink = std::sync::Arc::new(RecordingSink::default());
        let guard = SubscribeGuard::try_new(DhanEndpointType::MainFeed, instruments(250))
            .expect("inside cap");
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        // Queued before the loop starts, so the arm is ready as soon as the
        // scripted frames are drained.
        // A top-up of instruments the connection does NOT already hold. Ids
        // 250..400, so nothing here repeats the initial set — see
        // `instruments_from`.
        tx.send(LiveSubscriptionCommand::Extend(instruments_from(250, 150)))
            .await
            .expect("channel open");
        drop(tx);

        let exit = run_connection_with_commands(
            fake(&st),
            sup(DhanEndpointType::MainFeed, 0, t0()),
            guard,
            std::sync::Arc::clone(&sink),
            || async {},
            Some(rx),
        )
        .await;

        assert_eq!(exit, ConnectionExit::Parked(ParkReason::FatalDisconnect));
        let s = st.lock().expect("fake state");
        assert_eq!(
            s.subscribes, 5,
            "3 messages for the initial 250 + 2 for the 150 added — NOT 5 more \
             for a full re-send"
        );
        assert_eq!(s.connects, 1, "a top-up must never re-dial");
        assert_eq!(
            sink.accepted.lock().map(|g| g.len()).unwrap_or(0),
            2,
            "frames still reach the sink — the top-up arm does not eat them"
        );
    }

    /// The top-up must STOP at its wire budget instead of stalling the reader.
    ///
    /// # The defect
    ///
    /// These sends run ON the drain task, and the automatic pong is only
    /// emitted while `recv()` is polling. `try_extend` admits up to the
    /// endpoint's 5,000 cap, so a top-up can be ~42 messages, each bounded
    /// only by the transport's 10-second `SUBSCRIBE_SEND_TIMEOUT` -- a worst
    /// case of 420 seconds of silence against Dhan's 40-second close. Four
    /// slow sends already exceed it. The mechanism that exists to widen
    /// coverage was able to take the socket down.
    ///
    /// The swap arm one branch below was given `SWAP_WIRE_BUDGET` for a
    /// TWENTY-second exposure, with a doc opening "THIS BOUND IS THE POINT".
    /// The top-up had twenty times that exposure and no bound at all.
    ///
    /// # What this asserts
    ///
    /// A 4,000-instrument top-up is 40 messages. On a 900 ms socket that is
    /// ~37 seconds unbounded -- past the close. Bounded at 5 seconds it must
    /// send only a handful and stop. The number is deliberately asserted as a
    /// RANGE, not a constant: pinning an exact count would break on any
    /// reasonable retiming and teach the next reader to delete the test.
    #[tokio::test(start_paused = true)]
    async fn a_top_up_stops_at_its_wire_budget_instead_of_stalling_the_reader() {
        let st = std::sync::Arc::new(Mutex::new(FakeState {
            recv_events: VecDeque::from(vec![SocketEvent::Frame(Bytes::from_static(b"aaaaaaaa"))]),
            // Slow but NOT slow enough to trip the per-message timeout, so
            // what bites is the whole-top-up budget rather than one bad send.
            subscribe_delay: Some(Duration::from_millis(900)),
            ..FakeState::default()
        }));
        let sink = std::sync::Arc::new(RecordingSink::default());
        let guard = SubscribeGuard::try_new(DhanEndpointType::MainFeed, instruments(250))
            .expect("inside cap");
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        tx.send(LiveSubscriptionCommand::Extend(instruments_from(
            250, 4_000,
        )))
        .await
        .expect("channel open");
        drop(tx);

        let exit = run_connection_with_commands(
            fake(&st),
            sup(DhanEndpointType::MainFeed, 0, t0()),
            guard,
            std::sync::Arc::clone(&sink),
            || async {},
            Some(rx),
        )
        .await;

        assert_eq!(exit, ConnectionExit::Parked(ParkReason::FatalDisconnect));
        let s = st.lock().expect("fake state");
        let initial = 3; // 250 instruments / 100 per message
        let topup_sent = s.subscribes - initial;
        assert!(
            topup_sent >= 1,
            "the budget must still permit forward progress -- a top-up that \
             sends nothing is a permanent hole, not a bound"
        );
        assert!(
            topup_sent < 40,
            "the top-up must STOP at its budget; it sent {topup_sent} of 40 \
             messages, which means the bound is not being applied and the \
             drain can be held past Dhan's 40-second close"
        );
        assert_eq!(s.connects, 1, "a bounded top-up must never re-dial");
    }

    /// A guard may only ever SHRINK, and shrinking is what keeps a
    /// budget-stopped top-up honest.
    ///
    /// The guard's set IS the reconnect replay. If it names instruments the
    /// socket never received, every later reconnect replays a subscription the
    /// socket never had — and if that pushes the replay past the endpoint cap,
    /// Dhan answers 804 and drops the socket. So the budget path truncates and
    /// the send-FAILURE path deliberately does not: a dying socket is about to
    /// replay everything anyway.
    #[test]
    fn truncate_to_shrinks_the_guard_and_can_never_grow_it() {
        let mut g = SubscribeGuard::try_new(DhanEndpointType::MainFeed, instruments(250))
            .expect("inside cap");
        assert_eq!(g.len(), 250);

        g.truncate_to(1_000);
        assert_eq!(g.len(), 250, "a length past the end must be a no-op");

        g.truncate_to(100);
        assert_eq!(g.len(), 100, "and must otherwise shrink to exactly that");

        g.truncate_to(100);
        assert_eq!(g.len(), 100, "truncating twice is a no-op");

        g.truncate_to(0);
        assert!(g.is_empty(), "shrinking to nothing is legal");
    }

    /// A connection given NO channel must behave byte-identically to before.
    #[tokio::test(start_paused = true)]
    async fn no_command_channel_is_byte_identical_to_before() {
        let st = std::sync::Arc::new(Mutex::new(FakeState {
            recv_events: VecDeque::from(vec![SocketEvent::Frame(Bytes::from_static(b"aaaaaaaa"))]),
            ..FakeState::default()
        }));
        let sink = std::sync::Arc::new(RecordingSink::default());
        let guard = SubscribeGuard::try_new(DhanEndpointType::MainFeed, instruments(250))
            .expect("inside cap");

        let exit = run_connection_with_commands(
            fake(&st),
            sup(DhanEndpointType::MainFeed, 0, t0()),
            guard,
            std::sync::Arc::clone(&sink),
            || async {},
            None,
        )
        .await;

        assert_eq!(exit, ConnectionExit::Parked(ParkReason::FatalDisconnect));
        let s = st.lock().expect("fake state");
        assert_eq!(s.subscribes, 3, "only the initial set");
        assert_eq!(s.connects, 1);
    }

    #[tokio::test(start_paused = true)]
    async fn test_run_connection_parks_on_805_without_ever_redialing() {
        // The safety property that protects sibling sockets.
        let st = std::sync::Arc::new(Mutex::new(FakeState {
            recv_events: VecDeque::from(vec![SocketEvent::Closed {
                code: Some(DisconnectCode::ExceededActiveConnections),
            }]),
            ..FakeState::default()
        }));
        let sink = std::sync::Arc::new(RecordingSink::default());
        let guard = SubscribeGuard::try_new(DhanEndpointType::MainFeed, instruments(10))
            .expect("inside cap");

        let exit = run_connection(
            fake(&st),
            sup(DhanEndpointType::MainFeed, 0, t0()),
            guard,
            sink,
            || async {},
        )
        .await;

        assert_eq!(exit, ConnectionExit::Parked(ParkReason::PoolOverflow));
        let s = st.lock().expect("fake state");
        assert_eq!(
            s.connects, 1,
            "805 must never be retried — a retry kills a healthy sibling"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn test_run_connection_retries_a_failed_dial_then_succeeds() {
        let st = std::sync::Arc::new(Mutex::new(FakeState {
            connect_results: VecDeque::from(vec![false, false, true]),
            recv_events: VecDeque::from(vec![SocketEvent::Frame(Bytes::from_static(b"tick"))]),
            ..FakeState::default()
        }));
        let sink = std::sync::Arc::new(RecordingSink::default());
        let guard = SubscribeGuard::try_new(DhanEndpointType::MainFeed, instruments(5))
            .expect("inside cap");

        let exit = run_connection(
            fake(&st),
            sup(DhanEndpointType::MainFeed, 0, t0()),
            guard,
            std::sync::Arc::clone(&sink),
            || async {},
        )
        .await;

        assert_eq!(exit, ConnectionExit::Parked(ParkReason::FatalDisconnect));
        let s = st.lock().expect("fake state");
        assert_eq!(s.connects, 3, "two failures then a success");
        assert_eq!(sink.accepted.lock().map(|g| g.len()).unwrap_or(0), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn test_run_connection_refreshes_the_token_before_redialing_on_807() {
        let st = std::sync::Arc::new(Mutex::new(FakeState {
            recv_events: VecDeque::from(vec![SocketEvent::Closed {
                code: Some(DisconnectCode::AccessTokenExpired),
            }]),
            ..FakeState::default()
        }));
        let refreshes = std::sync::Arc::new(AtomicUsize::new(0));
        let r = std::sync::Arc::clone(&refreshes);
        let sink = std::sync::Arc::new(RecordingSink::default());
        let guard = SubscribeGuard::try_new(DhanEndpointType::MainFeed, instruments(5))
            .expect("inside cap");

        let exit = run_connection(
            fake(&st),
            sup(DhanEndpointType::MainFeed, 0, t0()),
            guard,
            sink,
            move || {
                let r = std::sync::Arc::clone(&r);
                async move {
                    r.fetch_add(1, Ordering::SeqCst);
                }
            },
        )
        .await;

        assert_eq!(exit, ConnectionExit::Parked(ParkReason::FatalDisconnect));
        assert_eq!(
            refreshes.load(Ordering::SeqCst),
            1,
            "a stale token must be replaced before the socket is re-dialed"
        );
        let s = st.lock().expect("fake state");
        assert_eq!(
            s.connects, 2,
            "one original dial plus one post-refresh dial"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn test_run_connection_tears_down_when_a_subscribe_batch_fails() {
        let st = std::sync::Arc::new(Mutex::new(FakeState {
            subscribe_results: VecDeque::from(vec![true, false]),
            ..FakeState::default()
        }));
        let sink = std::sync::Arc::new(RecordingSink::default());
        let guard = SubscribeGuard::try_new(DhanEndpointType::MainFeed, instruments(150))
            .expect("inside cap");

        let exit = run_connection(
            fake(&st),
            sup(DhanEndpointType::MainFeed, 0, t0()),
            guard,
            sink,
            || async {},
        )
        .await;

        assert_eq!(exit, ConnectionExit::Parked(ParkReason::FatalDisconnect));
        let s = st.lock().expect("fake state");
        assert!(s.closes >= 1, "a blind socket must be closed, not kept");
        assert!(s.connects >= 2, "and re-dialed");
    }

    #[tokio::test(start_paused = true)]
    async fn test_run_connection_exits_immediately_on_an_already_parked_supervisor() {
        // Regression guard: the `Continue` arm must not spin on a supervisor
        // that absorbs every event.
        let st = std::sync::Arc::new(Mutex::new(FakeState::default()));
        let sink = std::sync::Arc::new(RecordingSink::default());
        let guard = SubscribeGuard::try_new(DhanEndpointType::MainFeed, instruments(1))
            .expect("inside cap");
        let mut s = sup(DhanEndpointType::MainFeed, 0, t0());
        let _ = s.on_event(ConnEvent::ShutdownRequested, t0());

        let exit = run_connection(fake(&st), s, guard, sink, || async {}).await;
        assert_eq!(exit, ConnectionExit::Parked(ParkReason::Shutdown));
        assert_eq!(
            st.lock().map(|g| g.connects).unwrap_or(usize::MAX),
            0,
            "a parked supervisor must never dial"
        );
    }

    // -- structural proofs --------------------------------------------------

    #[test]
    fn test_pool_supervisor_source_never_reads_the_wall_clock() {
        // An NTP step must be unable to expire all sixteen sockets at once.
        let src = include_str!("pool_supervisor.rs");
        let test_marker = concat!("#[cfg(", "test)]");
        let production_half = src.split(test_marker).next().unwrap_or(src);
        assert!(production_half.contains("Instant"), "sanity");
        for needle in [
            concat!("System", "Time"),
            concat!("Utc", "::now"),
            concat!("chr", "ono"),
            concat!("UNIX_", "EPOCH"),
            concat!("Local", "::now"),
        ] {
            assert!(
                !production_half.contains(needle),
                "pool supervisor production code must never touch the wall clock, \
                 found `{needle}`"
            );
        }
    }

    #[test]
    fn test_drain_loop_body_does_nothing_but_capture_and_watchdog() {
        // THE ONE RULE, as a build-failing structural assertion. If a future
        // change adds parsing, a database write, or a lock to the per-frame
        // path, this fails before it can cost us the pong.
        let src = include_str!("pool_supervisor.rs");
        let test_marker = concat!("#[cfg(", "test)]");
        let production_half = src.split(test_marker).next().unwrap_or(src);
        let drain_body = production_half
            .split("async fn drain<S, K>")
            .nth(1)
            .unwrap_or("");
        assert!(
            drain_body.contains("sink.accept(frame)"),
            "sanity: the drain body must still be the real one"
        );
        for banned in [
            concat!("dispatch_", "frame"),
            concat!("parse_", "depth"),
            concat!("consume_", "tick"),
            concat!(".lock", "()"),
            concat!("questdb", ""),
            concat!("write_", "row"),
        ] {
            assert!(
                !drain_body.contains(banned),
                "the drain loop must do nothing but append+push per frame; found `{banned}`. \
                 Dhan's automatic pong is only emitted while the read loop is polling, so \
                 work here does not merely lag — it gets the socket disconnected"
            );
        }
    }

    #[test]
    fn test_frame_sink_trait_is_not_async_so_a_future_impl_cannot_await() {
        // The signature IS the enforcement: an `async fn accept` would let a
        // future implementation await inside the read loop.
        let src = include_str!("pool_supervisor.rs");
        let test_marker = concat!("#[cfg(", "test)]");
        let production_half = src.split(test_marker).next().unwrap_or(src);
        let trait_body = production_half
            .split("pub trait FrameSink")
            .nth(1)
            .unwrap_or("")
            .split("}\n")
            .next()
            .unwrap_or("");
        assert!(trait_body.contains("fn accept"), "sanity");
        assert!(
            !trait_body.contains("async") && !trait_body.contains("Future"),
            "FrameSink::accept must stay synchronous"
        );
    }

    /// The WAL receipt must be WRITTEN, not merely representable.
    ///
    /// # The defect this pins
    ///
    /// TVW3 added an 8-byte `received_at_nanos` to the WAL record on
    /// 2026-08-28, and `append_with_seq_at` was written to persist it. It then
    /// had ZERO production callers: both real append sites went through
    /// `append_with_seq`, which passes `WAL_RECEIPT_UNKNOWN_NANOS`. So every
    /// record on the box carried the sentinel while the format claimed to carry
    /// a receipt, and boot replay re-stamped `now()` — the moment of REPLAY,
    /// not of arrival. Since `received_at` is the candle BUCKETING clock, that
    /// silently re-buckets every recovered frame into the minute the process
    /// happened to restart in.
    ///
    /// A format that advertises a field nothing fills is worse than one that
    /// does not advertise it, because the reader has no way to tell.
    #[test]
    fn the_capture_path_persists_the_frame_receipt() {
        let source = include_str!("pool_supervisor.rs");
        let production_half = source
            .split_once("#[cfg(test)]")
            .map_or(source, |(prod, _)| prod);
        assert!(
            production_half.contains("append_with_seq_at("),
            "FrameSink::accept must call append_with_seq_at, not append_with_seq. \
             append_with_seq records the UNKNOWN sentinel, so the TVW3 receipt field \
             would be written as 0 on every frame and replay would re-stamp now() — the \
             moment of REPLAY rather than of arrival, on the clock that buckets candles."
        );
        assert!(
            production_half.contains("receipt_nanos_from(received_at)"),
            "the receipt must be DERIVED from the `received_at` instant already stamped \
             at the top of accept, never read from a clock here: this file bans \
             wall-clock reads so an NTP step cannot expire all sixteen sockets at once, \
             and that ban is right."
        );
        assert!(
            !production_half.contains("append_with_seq(self.ws_type"),
            "no production append may go through append_with_seq — it hardcodes the \
             unknown-receipt sentinel"
        );
    }

    /// The anchor the receipt is derived from must be RE-TAKEN, not boot-only.
    ///
    /// `Instant` is CLOCK_MONOTONIC and `SystemTime` is CLOCK_REALTIME; NTP
    /// SLEWS the latter at up to 500 ppm, so a single boot anchor drifts ~1.8 s
    /// per hour — ~16 s over a session. On the candle bucketing clock that
    /// files bars in the wrong second by a margin that GROWS all day, which is
    /// the error shape hardest to notice and hardest to reconstruct afterwards.
    #[test]
    fn the_receipt_anchor_is_refreshed_off_the_hot_path() {
        let lane = include_str!("../../../app/src/dhan_feed_stack.rs");
        let production_half = lane
            .split_once("#[cfg(test)]")
            .map_or(lane, |(prod, _)| prod);
        assert!(
            production_half.contains("refresh_receipt_anchor()"),
            "some off-hot-path timer must re-take the receipt anchor; a boot-only anchor \
             drifts against the wall clock under NTP slew for the whole session"
        );
        let refresh = production_half
            .rfind("refresh_receipt_anchor()")
            .expect("checked above");
        let silence_arm = production_half
            .rfind("silence_timer.tick()")
            .expect("the lane must still have its 30s silence timer");
        assert!(
            silence_arm < refresh,
            "the refresh must sit on the 30s silence arm, never the 500 ms flush arm: it \
             is a wall-clock syscall plus an allocation, and 30 s already bounds the drift \
             to ~15 ms at the worst permitted slew — three orders of magnitude inside a \
             one-second bucket."
        );
    }
}
